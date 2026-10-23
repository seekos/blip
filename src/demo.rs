//! 本地演示服务器 —— 只在 `--features demo-server` 时编译。
//!
//! 为什么需要它：这个项目最难验证的部分是「真的连上去之后」的行为 ——
//! 进度条动不动、速率是不是 MB/s、取消之后半成品还在不在、从 Finder 拖文件
//! 能不能落到当前远程目录。没有真实的 SFTP 主机时这些全靠猜。
//!
//! 这里把 [`crate::sftp::testserver`] 里那个进程内 russh 服务器编进二进制：
//! 监听 127.0.0.1 的随机端口，账号 `tester` / `s3cret`，家目录 `/home/tester`，
//! 文件树在内存里（传完就没了，不碰磁盘）。
//!
//! ```text
//! cargo run --features demo-server
//! ```
//!
//! 启动后表单会自动填好并直接连接，右侧面板立刻就是真实的 `read_dir` 结果。

use std::net::SocketAddr;
use std::sync::OnceLock;
use std::time::Duration;

use russh_sftp::protocol::FileAttributes;

use crate::sftp::testserver;

/// 演示服务器的登录信息。
pub struct DemoLogin {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// 大文件尺寸：本地回环传完要一小会儿，够看清楚进度/速率，也来得及点「取消」。
const LARGE_FILE: &str = "demo-large.bin";
const LARGE_FILE_LEN: usize = 64 * 1024 * 1024;

/// `MACSCP_DEMO_PKG=1` 时预置的远端目录树，用来演示「整目录下载」。
///
/// 形状和本地那棵演示树（`workspace::demo_tree`）保持一致：
/// 两个文件 + 两层子目录 + 一个空目录。
pub fn pkg_path() -> String {
    format!("{}/demo-pkg", testserver::HOME)
}

static LOGIN: OnceLock<Option<DemoLogin>> = OnceLock::new();

/// 取演示服务器地址；没启动或启动失败时为 `None`。
pub fn login() -> Option<&'static DemoLogin> {
    LOGIN.get().and_then(Option::as_ref)
}

/// 启动演示服务器（幂等，最多真的启动一次）。
pub fn start() -> Option<&'static DemoLogin> {
    LOGIN
        .get_or_init(|| match boot() {
            Ok(addr) => {
                log::info!(
                    "演示服务器就绪：{}@{}:{}，家目录 {}",
                    testserver::USER,
                    addr.ip(),
                    addr.port(),
                    testserver::HOME
                );
                Some(DemoLogin {
                    host: addr.ip().to_string(),
                    port: addr.port(),
                    username: testserver::USER.to_string(),
                    password: testserver::PASSWORD.to_string(),
                })
            }
            Err(message) => {
                log::error!("演示服务器启动失败：{message}");
                None
            }
        })
        .as_ref()
}

/// 在独立线程上建 runtime 跑服务器，主线程只等端口号。
///
/// 注意：服务器任务是 spawn 在这个 runtime 上的，所以 `block_on` 必须一直挂着，
/// 不能像 `worker` 那样把 runtime 放进线程里就返回 —— 那会让 listener 立刻被丢弃。
fn boot() -> Result<SocketAddr, String> {
    let (tx, rx) = flume::bounded::<Result<SocketAddr, String>>(1);

    std::thread::Builder::new()
        .name("macscp-demo".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("macscp-demo-rt")
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = tx.send(Err(format!("创建演示 runtime 失败：{err}")));
                    return;
                }
            };

            runtime.block_on(async move {
                seed().await;
                let addr = testserver::spawn().await;
                let _ = tx.send(Ok(addr));
                std::future::pending::<()>().await;
            });
        })
        .map_err(|err| format!("创建演示线程失败：{err}"))?;

    rx.recv_timeout(Duration::from_secs(10))
        .map_err(|err| format!("演示服务器 10 秒内没有就绪：{err}"))?
}

/// 预置虚拟文件树。
async fn seed() {
    testserver::vfs_reset().await;
    // 打开「上传过/预置的文件也出现在列表里」这条通道，否则传完看不见。
    testserver::enable_extra_entries();

    let large: Vec<u8> = (0..LARGE_FILE_LEN).map(|ix| (ix % 251) as u8).collect();
    testserver::vfs_write(&format!("{}/{}", testserver::HOME, LARGE_FILE), &large).await;
    testserver::add_extra_entry(LARGE_FILE, file_attrs(LARGE_FILE_LEN as u64));

    // 再补两个小的：一个文本一个「图片」，让列表看起来像真的目录。
    for (name, size) in [("notes.md", 1_284_u64), ("screenshot.png", 3_456_789)] {
        let data: Vec<u8> = (0..size).map(|ix| (ix % 97) as u8).collect();
        testserver::vfs_write(&format!("{}/{}", testserver::HOME, name), &data).await;
        testserver::add_extra_entry(name, file_attrs(size));
    }

    // `MACSCP_DEMO_RESUME=1`：给 notes.md 留一个「上次没传完」的临时文件，
    // 这样冲突弹窗会走「可以续传」的那一支（高亮 + 断点续传按钮可用）。
    // 本地演示文件是 14 字节，这里留 6 字节，正好够触发 `can_resume`。
    if std::env::var_os("MACSCP_DEMO_RESUME").is_some() {
        let partial = format!("{}/notes.md.filepart", testserver::HOME);
        testserver::vfs_write(&partial, b"# demo").await;
        log::info!("演示模式：预置了半成品 {partial}（6 字节）");
    }

    // `MACSCP_DEMO_PKG=1`：预置一棵真正的远端目录树，用来手工验证整目录下载。
    // 目录必须登记成「额外目录项」才会出现在家目录列表里（`readdir` 只列
    // `fixture()` + 额外项，不会把 VFS 里所有键倒出来）。
    if std::env::var_os("MACSCP_DEMO_PKG").is_some() {
        seed_pkg().await;
    }
}

/// 在虚拟磁盘上造一棵 `demo-pkg/`，内容可复现、大小适中（含一个 256 KiB 的文件）。
async fn seed_pkg() {
    let root = pkg_path();
    for suffix in ["", "/sub", "/sub/deep", "/empty"] {
        testserver::vfs_mkdir(&format!("{root}{suffix}")).await;
    }

    for (relative, data) in [
        ("a.txt", b"# demo folder\n".to_vec()),
        ("sub/b.txt", b"second file\n".to_vec()),
        (
            "sub/deep/c.bin",
            (0..256 * 1024u32).map(|ix| (ix % 251) as u8).collect(),
        ),
    ] {
        testserver::vfs_write(&format!("{root}/{relative}"), &data).await;
    }

    testserver::add_extra_entry(
        "demo-pkg",
        FileAttributes {
            size: Some(4096),
            permissions: Some(0o040_755),
            mtime: Some(1_700_000_700),
            ..Default::default()
        },
    );
    log::info!("演示模式：预置了远端目录树 {root}（3 个文件 / 4 个目录）");
}

fn file_attrs(size: u64) -> FileAttributes {
    FileAttributes {
        size: Some(size),
        permissions: Some(0o100_644),
        mtime: Some(1_700_000_600),
        ..Default::default()
    }
}
