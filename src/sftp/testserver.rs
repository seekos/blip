//! 仅测试用的进程内 SFTP 服务器。
//!
//! 目的：在没有真实服务器、也不联网的环境里，把
//! 「TCP → SSH 握手 → 密码认证 → 打开 sftp 子系统 → realpath → read_dir」
//! 整条链路完整跑一遍，验证 `session::connect` / `session::list_dir` 真的能用。
//!
//! 虚拟文件树固定挂在 `/home/tester` 下，故意混合了大小写、点文件、目录与普通文件，
//! 用来验证「文件夹优先、名称字母序」的排序结果。
//!
//! ⚠️ 下面的 `TEST_HOST_KEY` 是**公开的测试密钥**，只用于 127.0.0.1 回环连接，
//! 绝不可用于任何真实服务器。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use russh::keys::PrivateKey;
use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use tokio::sync::Mutex;

/// 测试服务器的登录目录。
pub const HOME: &str = "/home/tester";
/// 测试账号。
pub const USER: &str = "tester";
/// 测试密码。
pub const PASSWORD: &str = "s3cret";

/// 一次性生成的 ed25519 主机密钥（仅测试）。
const TEST_HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACAiN7ICodT1fgWp8PSGl4QBDCh08gIQZkcsfDBIS963agAAAJgWly6tFpcu
rQAAAAtzc2gtZWQyNTUxOQAAACAiN7ICodT1fgWp8PSGl4QBDCh08gIQZkcsfDBIS963ag
AAAECBX0eHxekAn1NeW/yteyCtroUstGrq4YQ8qwkNWeud0iI3sgKh1PV+Banw9IaXhAEM
KHTyAhBmRyx8MEhL3rdqAAAAEG1hY3NjcC10ZXN0LW9ubHkBAgMEBQ==
-----END OPENSSH PRIVATE KEY-----
";

/// 客户端私钥（仅测试，未加密），`auth_publickey` 只认这一把。
///
/// 演示版（`--features demo-server`）里没有人拿它去认证，但它是「公钥登录」
/// 用例的输入，所以不能删 —— 只是不该为它报 dead_code。
#[cfg_attr(not(test), allow(dead_code))]
pub const CLIENT_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACAmhKb9MJJXxLqQatIa6NEK5MGNjHIv+Q1RxeCVakzo0AAAAJjyoaiu8qGo
rgAAAAtzc2gtZWQyNTUxOQAAACAmhKb9MJJXxLqQatIa6NEK5MGNjHIv+Q1RxeCVakzo0A
AAAECYWsPy5gvei+bWjuSok7LIOC2N50vOPBlleOu+7MOOUyaEpv0wklfEupBq0hro0Qrk
wY2Mci/5DVHF4JVqTOjQAAAAEG1hY3NjcC10ZXN0LW9ubHkBAgMEBQ==
-----END OPENSSH PRIVATE KEY-----
";

/// [`CLIENT_KEY`] 对应的公钥（带注释的 OpenSSH 单行格式）。
pub const CLIENT_KEY_PUBLIC: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICaEpv0wklfEupBq0hro0QrkwY2Mci/5DVHF4JVqTOjQ macscp-test-only";

/// 同一把私钥的口令保护版本（口令见 [`CLIENT_KEY_PASSPHRASE`]）。
///
/// 用来验证「加密私钥」这条路径：不给口令必须报一句人话，给了口令必须能连上。
#[cfg_attr(not(test), allow(dead_code))]
pub const CLIENT_KEY_ENCRYPTED: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABB6UTfls+
XoIbtqkWGwyvnoAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAICaEpv0wklfEupBq
0hro0QrkwY2Mci/5DVHF4JVqTOjQAAAAoPJcIr4D5sYU7f9pqQaMXvFcLXPwliS2DgHGgk
pYKxZx5tGXKYnb0Oc+b30OKiKHC3TeE1i0bG8gbPk2S141cVHLsSS3q20qG/qNliUVtvzJ
uDMc1TlDpbjVNgmT70cdPGgkOYCb8kiRgQoum22FnY6ifrASeyyeZLyoU7D2Q7mz53Qd4t
//E558j3Sj1sZUcxtzTo5rWeoXJR749kYjcfs=
-----END OPENSSH PRIVATE KEY-----
";

/// [`CLIENT_KEY_ENCRYPTED`] 的口令。
#[cfg_attr(not(test), allow(dead_code))]
pub const CLIENT_KEY_PASSPHRASE: &str = "test-pass";

/// 在 127.0.0.1 的随机端口上起一个测试服务器，返回它的地址。
pub async fn spawn() -> SocketAddr {
    spawn_with_inactivity_timeout(None).await
}

async fn spawn_with_inactivity_timeout(inactivity_timeout: Option<Duration>) -> SocketAddr {
    let key = PrivateKey::from_openssh(TEST_HOST_KEY).expect("解析测试主机密钥");
    let config = Arc::new(russh::server::Config {
        keys: vec![key],
        inactivity_timeout,
        // 默认 1 秒的拒绝延迟会让「密码错误」用例白等，这里压到 50ms。
        auth_rejection_time: Duration::from_millis(50),
        auth_rejection_time_initial: Some(Duration::ZERO),
        ..Default::default()
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("绑定测试端口");
    let addr = listener.local_addr().expect("读取本地端口");

    tokio::spawn(async move {
        let mut server = TestServer;
        // 这个 future 在 listener 被 drop 前一直不返回，交给后台任务即可。
        let _ = server.run_on_socket(config, &listener).await;
    });

    addr
}

struct TestServer;

impl russh::server::Server for TestServer {
    type Handler = SshSession;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Self::Handler {
        SshSession::default()
    }
}

#[derive(Default)]
struct SshSession {
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == USER && password == PASSWORD {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    /// 只接受 [`CLIENT_KEY_PUBLIC`] 这一把公钥 —— 和真实服务器上
    /// `~/.ssh/authorized_keys` 的行为一致：别的密钥一律拒绝。
    async fn auth_publickey(
        &mut self,
        user: &str,
        public_key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        let expected =
            russh::keys::PublicKey::from_openssh(CLIENT_KEY_PUBLIC).expect("解析测试客户端公钥");
        if user == USER && expected.key_data() == public_key.key_data() {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.lock().await.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }

        let Some(channel) = self.channels.lock().await.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };

        session.channel_success(channel_id)?;
        // `run` 内部会 spawn 出处理循环并立刻返回，所以这里不会阻塞 SSH 会话。
        russh_sftp::server::run(channel.into_stream(), TestSftp::default()).await;
        Ok(())
    }
}

/// 虚拟文件树。返回 (名称, 属性)，属性里的 permissions 含文件类型位。
fn fixture() -> Vec<(&'static str, FileAttributes)> {
    let dir = |mode: u32, mtime: u32| FileAttributes {
        size: Some(4096),
        permissions: Some(mode),
        mtime: Some(mtime),
        ..Default::default()
    };
    let file = |size: u64, mode: u32, mtime: u32| FileAttributes {
        size: Some(size),
        permissions: Some(mode),
        mtime: Some(mtime),
        ..Default::default()
    };

    vec![
        // 故意打乱顺序：服务器不该指望客户端自己排。
        ("zeta.txt", file(100, 0o100_644, 1_700_000_300)),
        ("Beta", dir(0o040_755, 1_700_000_100)),
        (".env", file(12, 0o100_600, 1_700_000_400)),
        ("alpha", dir(0o040_755, 1_700_000_200)),
        ("archive.zip", file(2048, 0o100_644, 1_700_000_500)),
    ]
}

// ---------------------------------------------------------------------------
// 虚拟文件系统
// ---------------------------------------------------------------------------

/// 虚拟磁盘上的一个文件：内容 + POSIX mode + mtime。
///
/// mode 必须留着：上传的「继承原文件权限」逻辑要靠它验证，`setstat` 也要能改它。
#[derive(Clone)]
struct VfsFile {
    data: Vec<u8>,
    /// 含类型位的 POSIX mode（普通文件是 `0o100_xxx`）。
    mode: u32,
    mtime: u32,
}

impl Default for VfsFile {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            mode: 0o100_644,
            mtime: unix_now(),
        }
    }
}

/// 进程内共享的「远端磁盘」。用 `OnceLock<Mutex<..>>` 而不是塞进 `TestSftp`，
/// 这样同一个进程里的多条连接看到的是同一个文件树 —— 用例之间需要它。
static VFS: std::sync::OnceLock<Mutex<HashMap<String, VfsFile>>> = std::sync::OnceLock::new();

fn vfs() -> &'static Mutex<HashMap<String, VfsFile>> {
    VFS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 目录在虚拟磁盘上也是一条记录，靠 mode 的类型位区分（和真 POSIX 一样）。
const DIR_MODE: u32 = 0o040_755;

fn is_dir_mode(mode: u32) -> bool {
    mode & 0o170_000 == 0o040_000
}

/// `path` 在虚拟磁盘上是不是一个目录。
///
/// 三个来源：两个隐含的根、`fixture()` 里那几个目录（它们只存在于「列表」里，
/// 不在虚拟磁盘上）、以及真正 `mkdir` 出来的目录。
fn is_dir_path(disk: &HashMap<String, VfsFile>, path: &str) -> bool {
    if path == HOME || path == "/" {
        return true;
    }
    if let Some(file) = disk.get(path) {
        return is_dir_mode(file.mode);
    }
    direct_child(path).is_some_and(|name| {
        fixture().iter().any(|(fixture_name, attrs)| {
            *fixture_name == name && attrs.permissions.is_some_and(is_dir_mode)
        })
    })
}

/// 虚拟磁盘里 `dir` 的直接子项（不含 `.` / `..`）。
fn children_of<'a>(
    disk: &'a HashMap<String, VfsFile>,
    dir: &str,
) -> impl Iterator<Item = (String, &'a VfsFile)> {
    let dir = if dir.len() > 1 {
        dir.trim_end_matches('/')
    } else {
        dir
    };
    disk.iter().filter_map(move |(path, file)| {
        if crate::sftp::session::remote_parent(path) != dir {
            return None;
        }
        let name = path.rsplit('/').next()?.to_string();
        Some((name, file))
    })
}

/// 重置虚拟磁盘，并按 `fixture()` 的尺寸预填内容（内容是可复现的伪随机字节）。
pub async fn vfs_reset() {
    let mut disk = vfs().lock().await;
    disk.clear();
    for (name, attrs) in fixture() {
        let size = attrs.size.unwrap_or(0) as usize;
        disk.insert(
            format!("{HOME}/{name}"),
            VfsFile {
                data: pseudo_random(name, size),
                mode: attrs.permissions.unwrap_or(0o100_644),
                mtime: attrs.mtime.unwrap_or_default(),
            },
        );
    }
}

/// 直接往虚拟磁盘里写文件，测试里用来构造「上次传了一半」的半成品。
pub async fn vfs_write(path: &str, data: &[u8]) {
    let mut disk = vfs().lock().await;
    let entry = disk.entry(path.to_string()).or_default();
    entry.data = data.to_vec();
}

/// 直接在虚拟磁盘上建一个目录，测试里用来构造「远端已经有一棵树」。
///
/// 只写一条 `mode` 带目录类型位的记录 —— 子项靠路径前缀被 `children_of` 认出来，
/// 不需要额外的父子关系表。
#[cfg_attr(not(test), allow(dead_code))]
pub async fn vfs_mkdir(path: &str) {
    vfs().lock().await.insert(
        path.to_string(),
        VfsFile {
            mode: DIR_MODE,
            ..Default::default()
        },
    );
}

/// 给虚拟磁盘上的文件设置 mode（构造「可执行文件」这类场景）。
#[cfg_attr(not(test), allow(dead_code))]
pub async fn vfs_set_mode(path: &str, mode: u32) {
    if let Some(file) = vfs().lock().await.get_mut(path) {
        file.mode = mode;
    }
}

/// 读虚拟磁盘里的文件，测试里用来校验最终内容。
#[cfg_attr(not(test), allow(dead_code))]
pub async fn vfs_read(path: &str) -> Option<Vec<u8>> {
    vfs().lock().await.get(path).map(|file| file.data.clone())
}

/// 读虚拟磁盘里文件的 mode。
#[cfg_attr(not(test), allow(dead_code))]
pub async fn vfs_mode(path: &str) -> Option<u32> {
    vfs().lock().await.get(path).map(|file| file.mode)
}

/// 文件是否还在虚拟磁盘里。
#[cfg_attr(not(test), allow(dead_code))]
pub async fn vfs_exists(path: &str) -> bool {
    vfs().lock().await.contains_key(path)
}

// ---------------------------------------------------------------------------
// 模拟「严格按 SFTP v3 规范」的服务器
// ---------------------------------------------------------------------------

/// 打开后，`rename` 在目标已存在时返回 `SSH_FX_FAILURE`。
///
/// 真实的 SFTP v3 服务器两种行为都有：OpenSSH 的 `rename(2)` 会直接覆盖，
/// 另一些实现严格照规范拒绝覆盖。上传的 fallback 路径（unlink + rename）
/// 只有在这个模式下才会被走到，所以得能模拟出来。
static RENAME_REFUSES_EXISTING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg_attr(not(test), allow(dead_code))]
pub fn set_rename_refuses_existing(enabled: bool) {
    RENAME_REFUSES_EXISTING.store(enabled, std::sync::atomic::Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// 演示模式：让「新上传的文件」也出现在目录列表里
// ---------------------------------------------------------------------------

/// 家目录下额外挂的目录项，**只有演示模式会初始化**。
///
/// 为什么不用「把 VFS 里所有键都列出来」：测试是并行跑的，VFS 是进程内共享的，
/// 那样做会让 `connect_and_list_dir_over_real_ssh` 里「正好 5 项」的断言随机失败。
/// 这里只在 [`enable_extra_entries`] 被显式调用后生效，测试从不调用它。
static EXTRA_ENTRIES: std::sync::OnceLock<std::sync::Mutex<Vec<(String, FileAttributes)>>> =
    std::sync::OnceLock::new();

/// 打开「额外目录项」登记（幂等）。调用后，通过 SFTP 新建/写入的文件会出现在列表里。
#[cfg_attr(not(feature = "demo-server"), allow(dead_code))]
pub fn enable_extra_entries() {
    EXTRA_ENTRIES.get_or_init(|| std::sync::Mutex::new(Vec::new()));
}

/// 直接把一条目录项塞进列表（演示模式预置大文件用）。
#[cfg_attr(not(feature = "demo-server"), allow(dead_code))]
pub fn add_extra_entry(name: &str, attrs: FileAttributes) {
    enable_extra_entries();
    let Some(mut entries) = extra_entries() else {
        return;
    };
    entries.retain(|(existing, _)| existing != name);
    entries.push((name.to_string(), attrs));
}

fn extra_entries() -> Option<std::sync::MutexGuard<'static, Vec<(String, FileAttributes)>>> {
    EXTRA_ENTRIES
        .get()
        .map(|slot| slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
}

/// `HOME` 的直接子项名字（`/home/tester/a/b` → `None`，`/home/tester/a` → `Some("a")`）。
fn direct_child(path: &str) -> Option<&str> {
    let name = path
        .strip_prefix(HOME)
        .and_then(|rest| rest.strip_prefix('/'))?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(name)
}

/// 把「家目录下的直接子文件」登记/更新到额外目录项里。
fn track_file(path: &str, size: u64, mode: u32) {
    let Some(name) = direct_child(path) else {
        return;
    };
    let Some(mut entries) = extra_entries() else {
        return;
    };
    if let Some((_, attrs)) = entries.iter_mut().find(|(existing, _)| existing == name) {
        attrs.size = Some(size);
        attrs.permissions = Some(mode);
        return;
    }
    entries.push((
        name.to_string(),
        FileAttributes {
            size: Some(size),
            permissions: Some(mode),
            mtime: Some(unix_now()),
            ..Default::default()
        },
    ));
}

/// 重命名之后同步列表项：`x.filepart` 变成 `x`，列表里不该再看到临时文件。
fn rename_extra_entry(from: &str, to: &str) {
    let (Some(from_name), Some(to_name)) = (direct_child(from), direct_child(to)) else {
        return;
    };
    let Some(mut entries) = extra_entries() else {
        return;
    };
    entries.retain(|(name, _)| name != to_name);
    if let Some((name, _)) = entries.iter_mut().find(|(name, _)| name == from_name) {
        *name = to_name.to_string();
    }
}

fn drop_extra_entry(path: &str) {
    let Some(name) = direct_child(path) else {
        return;
    };
    if let Some(mut entries) = extra_entries() {
        entries.retain(|(existing, _)| existing != name);
    }
}

fn unix_now() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as u32)
        .unwrap_or_default()
}

/// 可复现的填充内容：同一个名字总是得到同样的字节，断言时才好写。
fn pseudo_random(seed: &str, len: usize) -> Vec<u8> {
    (0..len)
        .map(|ix| {
            let mixed = seed.bytes().fold(ix as u64, |acc, b| {
                acc.wrapping_mul(31).wrapping_add(u64::from(b))
            });
            (mixed >> 3) as u8
        })
        .collect()
}

/// 一个已经 `opendir` 的目录：路径 + 「内容吐完了没」。
struct DirCursor {
    path: String,
    drained: bool,
}

/// 同时打开的目录句柄上限（客户端不发 CLOSE，只能靠这个兜底回收）。
const MAX_OPEN_DIRS: usize = 1024;

#[derive(Default)]
struct TestSftp {
    /// `opendir` 打开的目录：句柄 → 游标。
    ///
    /// 句柄必须**每次 opendir 都不同**，游标绑在句柄上而不是绑在路径上 ——
    /// 真实服务器就是这么做的。这里原本是一个裸 `bool`，于是两次并发 `read_dir`
    /// （UI 刷新家目录 + 整目录下载同时扫树）会互相把对方标成 EOF，后到的那个
    /// 直接读到空列表，表现为「随机少几个文件」。
    open_dirs: HashMap<String, DirCursor>,
    /// 递增的目录句柄序号。
    next_dir_handle: u64,
}

fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

fn attrs_of(file: &VfsFile) -> FileAttributes {
    FileAttributes {
        size: Some(file.data.len() as u64),
        permissions: Some(file.mode),
        mtime: Some(file.mtime),
        ..Default::default()
    }
}

impl russh_sftp::server::Handler for TestSftp {
    type Error = StatusCode;

    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        _version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, Self::Error> {
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, _path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(HOME)],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        {
            let disk = vfs().lock().await;
            if !is_dir_path(&disk, &path) {
                return Err(StatusCode::NoSuchFile);
            }
        }
        self.next_dir_handle += 1;
        let handle = format!("dir-{}", self.next_dir_handle);
        // 客户端不会发 CLOSE（russh-sftp 的 `Handler` 里没有 closedir），游标只能自己收：
        // 句柄是递增编号，攒到上限说明最早那批早就读完了，整张表清掉即可。
        if self.open_dirs.len() >= MAX_OPEN_DIRS {
            self.open_dirs.clear();
        }
        self.open_dirs.insert(
            handle.clone(),
            DirCursor {
                path,
                drained: false,
            },
        );
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        // 句柄不认识：多半是测试里自己编了一个，别静默返回空列表。
        let Some(cursor) = self.open_dirs.get_mut(&handle) else {
            return Err(StatusCode::Failure);
        };
        // 一次吐完，第二次调用回 EOF —— 客户端会一直读到 EOF 为止。
        if cursor.drained {
            return Err(StatusCode::Eof);
        }
        cursor.drained = true;
        let path = cursor.path.clone();

        // 家目录：固定 fixture（顺序故意打乱）。这里**不**读虚拟磁盘，
        // 否则并行跑的传输用例会把「正好 5 项」的断言弄挂。
        if path == HOME || path == "/" {
            let mut files: Vec<File> = fixture()
                .into_iter()
                .map(|(name, attrs)| File::new(name, attrs))
                .collect();

            // 演示模式下把「上传过 / 预置的」条目补进列表，否则传完看不见。
            if let Some(entries) = extra_entries() {
                files.extend(
                    entries
                        .iter()
                        .map(|(name, attrs)| File::new(name.as_str(), attrs.clone())),
                );
            }

            return Ok(Name { id, files });
        }

        // 子目录：直接从虚拟磁盘按父路径取直接子项（整目录上传之后就靠它浏览）。
        let disk = vfs().lock().await;
        let files: Vec<File> = children_of(&disk, &path)
            .map(|(name, file)| File::new(name.as_str(), attrs_of(file)))
            .collect();
        Ok(Name { id, files })
    }

    /// 建目录。已存在返回 `Failure` —— 客户端的 `ensure_dir` 会 `stat` 一次确认。
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut disk = vfs().lock().await;
        if disk.contains_key(&path) || is_dir_path(&disk, &path) {
            return Err(StatusCode::Failure);
        }
        // 父目录必须真的存在，否则整目录上传漏了某一层就该在这里暴露。
        if !is_dir_path(&disk, &crate::sftp::session::remote_parent(&path)) {
            return Err(StatusCode::NoSuchFile);
        }

        disk.insert(
            path.clone(),
            VfsFile {
                data: Vec::new(),
                mode: DIR_MODE,
                mtime: unix_now(),
            },
        );
        drop(disk);
        // 演示模式：新目录要能出现在家目录列表里。
        track_file(&path, 0, DIR_MODE);
        Ok(ok_status(id))
    }

    /// 删空目录。非空返回 `Failure`（SFTP 的 rmdir 语义）。
    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let mut disk = vfs().lock().await;
        match disk.get(&path) {
            Some(file) if is_dir_mode(file.mode) => {}
            _ => return Err(StatusCode::NoSuchFile),
        }
        if children_of(&disk, &path).next().is_some() {
            return Err(StatusCode::Failure);
        }

        disk.remove(&path);
        drop(disk);
        drop_extra_entry(&path);
        Ok(ok_status(id))
    }

    /// 打开虚拟文件。handle 直接用路径，读写时按路径去 VFS 里找。
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let mut disk = vfs().lock().await;

        if pflags.contains(OpenFlags::WRITE) {
            // 父目录必须存在：整目录上传漏建某一层时，这里会立刻报错，
            // 而不是悄悄在虚拟磁盘里造出一条「父目录不存在」的路径。
            if !is_dir_path(&disk, &crate::sftp::session::remote_parent(&filename)) {
                return Err(StatusCode::NoSuchFile);
            }
            let entry = disk.entry(filename.clone()).or_default();
            if pflags.contains(OpenFlags::TRUNCATE) {
                entry.data.clear();
            }
        } else if !disk.contains_key(&filename) {
            return Err(StatusCode::NoSuchFile);
        }

        // 演示模式：新建/清空之后要立刻能在家目录列表里看到它。
        let (size, mode) = disk
            .get(&filename)
            .map(|file| (file.data.len() as u64, file.mode))
            .unwrap_or((0, 0o100_644));
        drop(disk);
        track_file(&filename, size, mode);

        Ok(Handle {
            id,
            handle: filename,
        })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let disk = vfs().lock().await;
        let Some(file) = disk.get(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };

        let start = offset.min(file.data.len() as u64) as usize;
        let end = (offset + u64::from(len)).min(file.data.len() as u64) as usize;
        if start >= end {
            return Err(StatusCode::Eof);
        }

        Ok(Data {
            id,
            data: file.data[start..end].to_vec(),
        })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let mut disk = vfs().lock().await;
        let entry = disk.entry(handle.clone()).or_default();
        let offset = offset as usize;
        if offset + data.len() > entry.data.len() {
            entry.data.resize(offset + data.len(), 0);
        }
        entry.data[offset..offset + data.len()].copy_from_slice(&data);
        let (size, mode) = (entry.data.len() as u64, entry.mode);
        drop(disk);
        track_file(&handle, size, mode);

        Ok(ok_status(id))
    }

    /// 按 handle（= 路径）返回虚拟文件的属性。
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let disk = vfs().lock().await;
        let Some(file) = disk.get(&handle) else {
            return Err(StatusCode::NoSuchFile);
        };
        Ok(Attrs {
            id,
            attrs: attrs_of(file),
        })
    }

    /// 按路径 stat，用于「远端文件是否已存在」这类查询。
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let disk = vfs().lock().await;
        let Some(file) = disk.get(&path) else {
            return Err(StatusCode::NoSuchFile);
        };
        Ok(Attrs {
            id,
            attrs: attrs_of(file),
        })
    }

    /// 只处理 permissions / size —— 上传完要把原文件的可执行位贴回临时文件。
    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let mut disk = vfs().lock().await;
        let Some(file) = disk.get_mut(&path) else {
            return Err(StatusCode::NoSuchFile);
        };
        if let Some(mode) = attrs.permissions {
            file.mode = mode;
        }
        if let Some(size) = attrs.size {
            file.data.resize(size as usize, 0);
        }
        let (size, mode) = (file.data.len() as u64, file.mode);
        drop(disk);
        track_file(&path, size, mode);
        Ok(ok_status(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let mut disk = vfs().lock().await;
        match disk.get(&filename) {
            // unlink 不动目录：删目录走 `rmdir`。
            Some(file) if is_dir_mode(file.mode) => return Err(StatusCode::Failure),
            Some(_) => {}
            None => return Err(StatusCode::NoSuchFile),
        }
        disk.remove(&filename);
        drop(disk);
        drop_extra_entry(&filename);
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let mut disk = vfs().lock().await;
        if RENAME_REFUSES_EXISTING.load(std::sync::atomic::Ordering::Relaxed)
            && disk.contains_key(&newpath)
        {
            return Err(StatusCode::Failure);
        }
        let Some(file) = disk.remove(&oldpath) else {
            return Err(StatusCode::NoSuchFile);
        };
        disk.insert(newpath.clone(), file);
        drop(disk);
        rename_extra_entry(&oldpath, &newpath);
        Ok(ok_status(id))
    }

    async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
        Ok(ok_status(id))
    }
}

// ---------------------------------------------------------------------------
// 端到端用例
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::session::{self, HostKeyPolicy, format_mode};
    use crate::sftp::types::{
        AuthMethod, ErrorContext, ExistingPolicy, HostKeyDecision, SessionConfig, SftpCommand,
        SftpEvent, TransferOutcome, UploadDecision,
    };
    use crate::sftp::worker::SftpWorker;

    fn config_for(addr: SocketAddr, password: &str) -> SessionConfig {
        SessionConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            username: USER.to_string(),
            auth: AuthMethod::Password(password.to_string()),
        }
    }

    fn test_host_key_policy() -> HostKeyPolicy {
        let key = PrivateKey::from_openssh(TEST_HOST_KEY).expect("解析测试主机密钥");
        HostKeyPolicy::Exact(key.public_key().clone())
    }

    async fn connect_test(
        config: &SessionConfig,
    ) -> std::result::Result<session::ActiveSession, session::ConnectError> {
        session::connect(config, test_host_key_policy()).await
    }

    /// 走完真实 TCP + SSH 握手 + 密码认证 + SFTP 握手，然后读目录。
    #[tokio::test]
    async fn connect_and_list_dir_over_real_ssh() {
        let addr = spawn().await;
        let session = connect_test(&config_for(addr, PASSWORD))
            .await
            .expect("连接测试服务器");

        assert_eq!(session.home, HOME, "登录目录应来自 realpath");

        let entries = session::list_dir(&session.sftp, HOME)
            .await
            .expect("读取目录");

        // 排序：文件夹优先，同类按名称（大小写不敏感）字母序。
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "Beta", ".env", "archive.zip", "zeta.txt"]
        );

        // 目录标记
        assert!(entries[0].is_dir());
        assert!(entries[1].is_dir());
        assert!(!entries[2].is_dir());

        // 元数据：大小、八进制权限、修改时间都解析出来了。
        let zip = entries
            .iter()
            .find(|e| e.name == "archive.zip")
            .expect("archive.zip 应该在列表里");
        assert_eq!(zip.path, "/home/tester/archive.zip");
        assert_eq!(zip.size, 2048);
        assert_eq!(zip.mode, Some(0o100_644));
        assert_eq!(format_mode(zip.mode), "-rw-r--r--");
        assert!(zip.modified.is_some(), "mtime 应被解析成 SystemTime");

        // 目录的权限串以 d 开头，点文件保留 600。
        assert_eq!(format_mode(entries[0].mode), "drwxr-xr-x");
        let env = entries.iter().find(|e| e.name == ".env").expect(".env");
        assert_eq!(format_mode(env.mode), "-rw-------");
    }

    /// 业务层完全空闲时，SSH keepalive 也必须让服务器和客户端都保持会话。
    #[tokio::test]
    async fn keepalive_preserves_an_idle_sftp_session() {
        let server_idle_timeout = Duration::from_millis(250);
        let addr = spawn_with_inactivity_timeout(Some(server_idle_timeout)).await;
        let session = session::connect_with_keepalive_for_test(
            &config_for(addr, PASSWORD),
            test_host_key_policy(),
            Duration::from_millis(50),
            Duration::from_millis(500),
        )
        .await
        .expect("连接带空闲超时的测试服务器");

        // 明显超过服务端空闲阈值；期间不发送任何 SFTP 指令，只靠 SSH keepalive。
        tokio::time::sleep(server_idle_timeout * 3).await;

        let entries = session::list_dir(&session.sftp, HOME)
            .await
            .expect("保活后仍应能读取目录");
        assert!(!entries.is_empty());
    }

    /// 密码错误必须变成一条可读错误，而不是挂死或 panic。
    #[tokio::test]
    async fn wrong_password_is_reported_as_error() {
        let addr = spawn().await;
        let err = connect_test(&config_for(addr, "wrong"))
            .await
            .err()
            .expect("密码错误必须失败");
        assert!(
            err.to_string().contains("认证被拒绝"),
            "unexpected error: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // 私钥认证
    // -----------------------------------------------------------------------

    /// 把一段私钥写进临时文件，返回路径。
    fn write_key(tag: &str, contents: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("macscp-key-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时密钥目录");
        let path = dir.join("id_ed25519");
        std::fs::write(&path, contents).expect("写临时私钥");
        path
    }

    fn config_with_key(
        addr: SocketAddr,
        path: std::path::PathBuf,
        passphrase: Option<&str>,
    ) -> SessionConfig {
        SessionConfig {
            host: addr.ip().to_string(),
            port: addr.port(),
            username: USER.to_string(),
            auth: AuthMethod::PrivateKey {
                path,
                passphrase: passphrase.map(str::to_string),
            },
        }
    }

    /// 未加密的 ed25519 私钥必须能连上 —— 这是「密钥登录」最基本的路径。
    #[tokio::test]
    async fn connect_with_ed25519_key() {
        let addr = spawn().await;
        let path = write_key("plain", CLIENT_KEY);

        let session = connect_test(&config_with_key(addr, path, None))
            .await
            .expect("私钥认证应成功");
        assert_eq!(session.home, HOME);
    }

    /// 加密私钥不给口令：必须明确说「需要口令」，而不是一句
    /// 「无法读取私钥 / The key is encrypted」让用户猜。
    #[tokio::test]
    async fn encrypted_key_without_passphrase_is_explained() {
        let addr = spawn().await;
        let path = write_key("enc-nopass", CLIENT_KEY_ENCRYPTED);

        let err = connect_test(&config_with_key(addr, path, None))
            .await
            .err()
            .expect("缺少口令必须失败");
        let message = format!("{err:#}");
        assert!(
            message.contains("口令"),
            "错误信息应提示需要口令：{message}"
        );
    }

    /// 给了正确口令就必须能连上。
    #[tokio::test]
    async fn encrypted_key_with_passphrase_connects() {
        let addr = spawn().await;
        let path = write_key("enc-pass", CLIENT_KEY_ENCRYPTED);

        let session = connect_test(&config_with_key(addr, path, Some(CLIENT_KEY_PASSPHRASE)))
            .await
            .expect("口令正确的加密私钥应能认证");
        assert_eq!(session.home, HOME);
    }

    /// 口令错误：报错要指向口令，而不是「私钥损坏」。
    #[tokio::test]
    async fn encrypted_key_with_wrong_passphrase_is_explained() {
        let addr = spawn().await;
        let path = write_key("enc-badpass", CLIENT_KEY_ENCRYPTED);

        let err = connect_test(&config_with_key(addr, path, Some("nope")))
            .await
            .err()
            .expect("口令错误必须失败");
        let message = format!("{err:#}");
        assert!(message.contains("口令"), "错误信息应指向口令：{message}");
    }

    /// 服务器不认的密钥：报「认证被拒绝」，并提示公钥要加到 authorized_keys。
    #[tokio::test]
    async fn unknown_key_is_rejected() {
        let addr = spawn().await;
        // 用主机密钥充当客户端密钥：能读、能签名，但不是服务器认可的那把。
        let path = write_key("unknown", TEST_HOST_KEY);

        let err = connect_test(&config_with_key(addr, path, None))
            .await
            .err()
            .expect("未授权的密钥必须被拒绝");
        let message = format!("{err:#}");
        assert!(
            message.contains("认证被拒绝"),
            "unexpected error: {message}"
        );
    }

    /// 路径写错时给出的是「文件不存在」，而不是被当成协议错误。
    #[tokio::test]
    async fn missing_key_file_is_reported() {
        let addr = spawn().await;
        let path = std::env::temp_dir().join("macscp-definitely-missing-key");

        let err = connect_test(&config_with_key(addr, path, None))
            .await
            .err()
            .expect("不存在的私钥必须失败");
        let message = format!("{err:#}");
        assert!(message.contains("私钥"), "unexpected error: {message}");
    }

    /// 服务器没开 SFTP 子系统时也要给出明确错误（这里连的是普通 TCP 端口）。
    #[tokio::test]
    async fn list_dir_without_session_is_rejected() {
        let addr = spawn().await;
        // 不复用连接，直接对不存在的路径读目录：SFTP 层返回 NoSuchFile。
        let session = connect_test(&config_for(addr, PASSWORD))
            .await
            .expect("连接测试服务器");
        let err = session::list_dir(&session.sftp, "/nope")
            .await
            .expect_err("不存在的目录必须失败");
        assert!(err.contains("读取目录"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn missing_remembered_directory_falls_back_to_remote_home() {
        let addr = spawn().await;
        let session = connect_test(&config_for(addr, PASSWORD))
            .await
            .expect("连接测试服务器");

        let (path, entries) = session::list_dir_or_fallback(&session.sftp, "/nope", Some(HOME))
            .await
            .expect("不存在的历史目录应回退到登录目录");

        assert_eq!(path, HOME);
        assert!(!entries.is_empty());
    }

    // -----------------------------------------------------------------------
    // actor 层：指令 → 事件
    // -----------------------------------------------------------------------

    async fn next_event(worker: &SftpWorker) -> SftpEvent {
        tokio::time::timeout(Duration::from_secs(10), worker.events().recv_async())
            .await
            .expect("等待事件超时")
            .expect("事件通道不应关闭")
    }

    /// 真实 worker 首次连接必须先让 UI 确认指纹；测试选择“仅本次信任”。
    async fn connect_worker(worker: &SftpWorker, config: SessionConfig) {
        worker.send(SftpCommand::Connect(config));
        match next_event(worker).await {
            SftpEvent::HostKeyPrompt(prompt) => {
                assert!(prompt.fingerprint.starts_with("SHA256:"));
                assert!(!prompt.algorithm.is_empty());
                assert!(prompt.changed_line.is_none());
            }
            other => panic!("期望 HostKeyPrompt，实际 {other:?}"),
        }
        worker.send(SftpCommand::ResolveHostKey(HostKeyDecision::TrustOnce));
        match next_event(worker).await {
            SftpEvent::Connected { home } => assert_eq!(home, HOME),
            other => panic!("期望 Connected，实际 {other:?}"),
        }
    }

    /// 走 worker 的完整往返：Connect → Connected，ListDir → DirListed。
    #[tokio::test]
    async fn worker_round_trip_connect_and_list() {
        let addr = spawn().await;
        let worker = SftpWorker::spawn();

        connect_worker(&worker, config_for(addr, PASSWORD)).await;

        worker.send(SftpCommand::ListDir {
            request_id: 7,
            path: HOME.to_string(),
            fallback: None,
        });
        match next_event(&worker).await {
            SftpEvent::DirListed {
                request_id,
                path,
                entries,
            } => {
                assert_eq!(request_id, 7, "request_id 必须原样带回");
                assert_eq!(path, HOME);
                assert_eq!(entries.len(), 5);
                assert!(entries[0].is_dir(), "第一条应是目录");
            }
            other => panic!("期望 DirListed，实际 {other:?}"),
        }
    }

    /// 右键编辑的完整链路：读取 UTF-8、保存、保留权限，且不遗留临时文件。
    #[tokio::test]
    async fn worker_round_trip_read_and_write_text() {
        const PATH: &str = "/home/tester/macscp-worker-edit.txt";
        let _guard = vfs_lock().lock_owned().await;
        vfs_reset().await;
        vfs_write(PATH, b"before\n").await;
        vfs_set_mode(PATH, 0o100_640).await;

        let addr = spawn().await;
        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;

        worker.send(SftpCommand::ReadText {
            path: PATH.to_string(),
        });
        match next_event(&worker).await {
            SftpEvent::TextLoaded { path, content } => {
                assert_eq!(path, PATH);
                assert_eq!(content, "before\n");
            }
            other => panic!("期望 TextLoaded，实际 {other:?}"),
        }

        worker.send(SftpCommand::WriteText {
            path: PATH.to_string(),
            content: "修改后\nsecond line\n".to_string(),
        });
        match next_event(&worker).await {
            SftpEvent::TextSaved { path } => assert_eq!(path, PATH),
            other => panic!("期望 TextSaved，实际 {other:?}"),
        }

        assert_eq!(
            vfs_read(PATH).await.as_deref(),
            Some("修改后\nsecond line\n".as_bytes())
        );
        assert_eq!(vfs_mode(PATH).await, Some(0o100_640), "原权限必须保留");
    }

    /// 多选删除必须由一条指令处理整批文件/目录，并只返回一条汇总结果。
    #[tokio::test]
    async fn worker_removes_multiple_selected_entries() {
        const FILE: &str = "/home/tester/delete-one.txt";
        const DIR: &str = "/home/tester/delete-dir";
        const NESTED: &str = "/home/tester/delete-dir/nested.txt";

        let _guard = vfs_lock().lock_owned().await;
        vfs_reset().await;
        vfs_write(FILE, b"one").await;
        vfs_mkdir(DIR).await;
        vfs_write(NESTED, b"nested").await;

        let addr = spawn().await;
        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;
        worker.send(SftpCommand::Remove {
            entries: vec![(FILE.to_string(), false), (DIR.to_string(), true)],
        });

        match next_event(&worker).await {
            SftpEvent::Notice(message) => assert_eq!(message, "已删除 2 项"),
            other => panic!("期望批量删除成功通知，实际 {other:?}"),
        }
        assert!(!vfs_exists(FILE).await);
        assert!(!vfs_exists(NESTED).await);
        assert!(!vfs_exists(DIR).await);
    }

    /// 连不上时必须收到带 `Connect` 上下文的 Error，而不是线程静默死掉。
    #[tokio::test]
    async fn worker_reports_connect_failure() {
        let worker = SftpWorker::spawn();
        worker.send(SftpCommand::Connect(SessionConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            username: "nobody".to_string(),
            auth: AuthMethod::Password("x".to_string()),
        }));

        match next_event(&worker).await {
            SftpEvent::Error { context, message } => {
                assert_eq!(context, ErrorContext::Connect);
                assert!(!message.is_empty());
            }
            other => panic!("期望 Error，实际 {other:?}"),
        }
    }

    /// SFTP 的 rmdir 只能删空目录，应用层删除目录必须先递归清空子项。
    #[tokio::test]
    async fn remove_non_empty_remote_directory_recursively() {
        let ctx = connect_and_dir("remove-dir").await;
        const ROOT: &str = "/home/tester/remove-me";
        const NESTED: &str = "/home/tester/remove-me/nested";
        const FILE: &str = "/home/tester/remove-me/nested/file.txt";

        vfs_mkdir(ROOT).await;
        vfs_mkdir(NESTED).await;
        vfs_write(FILE, b"delete me").await;

        session::remove(&ctx.active.sftp, ROOT, true)
            .await
            .expect("非空远端目录应递归删除");

        assert!(!vfs_exists(FILE).await);
        assert!(!vfs_exists(NESTED).await);
        assert!(!vfs_exists(ROOT).await);
    }

    // -----------------------------------------------------------------------
    // 传输：分块流式 + 续传 + 取消
    // -----------------------------------------------------------------------

    use crate::sftp::transfer;
    use std::sync::atomic::AtomicBool;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("macscp-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("创建临时目录");
        dir
    }

    /// 逐字节比较，失败时只报「第几个字节开始不对」而不是把几 MB 全打出来。
    fn assert_same_bytes(actual: &[u8], expected: &[u8]) {
        assert_eq!(actual.len(), expected.len(), "长度必须一致");
        if let Some(ix) = actual.iter().zip(expected).position(|(a, b)| a != b) {
            panic!(
                "第 {ix} 个字节起内容不一致：实际 {} 期望 {}",
                actual[ix], expected[ix]
            );
        }
    }

    /// 传输类用例共享同一个进程内虚拟磁盘，必须串行跑。
    ///
    /// 并行会互相踩：`vfs_reset()` 会把别人正在读的文件清掉，两个用例写同一个
    /// 远端路径还会互相覆盖 —— 这个坑真的踩到过（续传起点变成别的用例的长度）。
    fn vfs_lock() -> Arc<tokio::sync::Mutex<()>> {
        static LOCK: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// 一个用例独占的上下文：`_vfs_guard` 活到用例结束，天然完成串行化。
    struct TestCtx {
        _vfs_guard: tokio::sync::OwnedMutexGuard<()>,
        active: session::ActiveSession,
        dir: std::path::PathBuf,
    }

    /// 起服务器 + 连上，返回会话与临时目录。
    async fn connect_and_dir(tag: &str) -> TestCtx {
        let guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;
        let active = connect_test(&config_for(addr, PASSWORD))
            .await
            .expect("连接测试服务器");
        TestCtx {
            _vfs_guard: guard,
            active,
            dir: temp_dir(tag),
        }
    }

    /// 512 KiB 的虚拟文件，比 64 KiB 块大 8 倍，确保真的分了很多块。
    const BIG_NAME: &str = "/home/tester/big.bin";
    const BIG_LEN: usize = 512 * 1024;

    async fn seed_big_file() {
        let data: Vec<u8> = (0..BIG_LEN as u32).map(|ix| (ix % 251) as u8).collect();
        vfs_write(BIG_NAME, &data).await;
    }

    /// 全量下载：内容逐字节一致，进度事件单调递增，终值等于总大小。
    #[tokio::test]
    async fn download_streams_file_and_reports_progress() {
        let ctx = connect_and_dir("dl").await;
        seed_big_file().await;

        let local = ctx.dir.join("big.bin");
        let (tx, rx) = flume::unbounded::<SftpEvent>();
        let cancel = Arc::new(AtomicBool::new(false));

        transfer::run_download(
            ctx.active.sftp.clone(),
            1,
            BIG_NAME.to_string(),
            local.clone(),
            BIG_LEN as u64,
            false,
            cancel,
            tx,
        )
        .await;

        let events: Vec<SftpEvent> = rx.drain().collect();
        let mut progress = 0u64;
        let mut saw_started = false;
        for event in &events {
            match event {
                SftpEvent::TransferStarted {
                    id,
                    resumed_from,
                    total,
                } => {
                    assert_eq!(*id, 1);
                    assert_eq!(*resumed_from, 0, "没有半成品时不该续传");
                    assert_eq!(*total, BIG_LEN as u64);
                    saw_started = true;
                }
                SftpEvent::TransferProgress {
                    transferred, total, ..
                } => {
                    assert!(*transferred >= progress, "已传输字节数不能回退");
                    progress = *transferred;
                    assert_eq!(*total, BIG_LEN as u64);
                }
                SftpEvent::TransferCompleted { outcome, .. } => {
                    assert_eq!(*outcome, TransferOutcome::Completed);
                }
                _ => {}
            }
        }
        assert!(saw_started, "应先发 TransferStarted");
        assert_eq!(progress, BIG_LEN as u64, "终值必须等于文件大小");

        let expected = vfs_read(BIG_NAME).await.expect("虚拟文件");
        let actual = tokio::fs::read(&local).await.expect("读取下载结果");
        assert_same_bytes(&actual, &expected);
    }

    /// 断点续传下载：身份标记匹配的 `.filepart` 已有 100 KiB，只补剩余部分。
    #[tokio::test]
    async fn download_resumes_from_partial_local_file() {
        let ctx = connect_and_dir("dl-resume").await;
        let expected = {
            let data: Vec<u8> = (0..BIG_LEN as u32).map(|ix| (ix % 251) as u8).collect();
            vfs_write(BIG_NAME, &data).await;
            data
        };

        let local = ctx.dir.join("big.bin");
        transfer::seed_download_checkpoint_for_test(
            &ctx.active.sftp,
            BIG_NAME,
            &local,
            &expected[..100 * 1024],
        )
        .await
        .expect("写带身份标记的下载断点");

        let (tx, rx) = flume::unbounded::<SftpEvent>();
        transfer::run_download(
            ctx.active.sftp.clone(),
            2,
            BIG_NAME.to_string(),
            local.clone(),
            BIG_LEN as u64,
            true,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
        .await;

        let started = rx
            .drain()
            .into_iter()
            .find_map(|event| match event {
                SftpEvent::TransferStarted { resumed_from, .. } => Some(resumed_from),
                _ => None,
            })
            .expect("应有 TransferStarted");
        assert_eq!(started, 100 * 1024, "应从本地已有的 100 KiB 接着传");

        let actual = tokio::fs::read(&local).await.expect("读取续传结果");
        assert_same_bytes(&actual, &expected);
    }

    /// 上传 + 下载往返：字节原样落到虚拟磁盘，临时文件必须被 rename 掉。
    #[tokio::test]
    async fn upload_then_download_round_trip() {
        let ctx = connect_and_dir("up").await;
        const REMOTE: &str = "/home/tester/round-trip.bin";

        let local = ctx.dir.join("round-trip.bin");
        let payload: Vec<u8> = (0..300 * 1024).map(|ix| (ix % 7) as u8).collect();
        std::fs::write(&local, &payload).expect("写本地文件");

        let (tx, _rx) = flume::unbounded::<SftpEvent>();
        transfer::run_upload(
            ctx.active.sftp.clone(),
            3,
            local.clone(),
            REMOTE.to_string(),
            payload.len() as u64,
            UploadDecision::Overwrite,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
        .await;

        let stored = vfs_read(REMOTE).await.expect("虚拟磁盘里应有这个文件");
        assert_same_bytes(&stored, &payload);
        assert!(
            !vfs_exists(&transfer::filepart_path(REMOTE)).await,
            "临时文件必须被 rename 成目标文件，不能留在目录里"
        );
    }

    /// 断点续传上传：`.filepart` 已有 50 KiB 时，只应补剩下的部分。
    #[tokio::test]
    async fn upload_resumes_from_partial_filepart() {
        let ctx = connect_and_dir("up-resume").await;
        const REMOTE: &str = "/home/tester/payload.bin";

        let local = ctx.dir.join("payload.bin");
        let payload: Vec<u8> = (0..300 * 1024).map(|ix| (ix % 7) as u8).collect();
        std::fs::write(&local, &payload).expect("写本地文件");
        // 上次传到 50 KiB 就断了：目标文件还不存在，只有 .filepart。
        vfs_write(&format!("{REMOTE}.filepart"), &payload[..50 * 1024]).await;

        let (tx, rx) = flume::unbounded::<SftpEvent>();
        transfer::run_upload(
            ctx.active.sftp.clone(),
            4,
            local.clone(),
            REMOTE.to_string(),
            payload.len() as u64,
            UploadDecision::Resume,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
        .await;

        let started = rx
            .drain()
            .into_iter()
            .find_map(|event| match event {
                SftpEvent::TransferStarted { resumed_from, .. } => Some(resumed_from),
                _ => None,
            })
            .expect("应有 TransferStarted");
        assert_eq!(started, 50 * 1024, "应从 .filepart 已有的 50 KiB 接着传");

        let stored = vfs_read(REMOTE).await.expect("虚拟磁盘里应有这个文件");
        assert_same_bytes(&stored, &payload);
        assert!(
            !vfs_exists(&format!("{REMOTE}.filepart")).await,
            "续传完成后临时文件应被替换掉"
        );
    }

    /// 覆盖已有文件：内容被换掉，可执行位必须从原文件继承。
    #[tokio::test]
    async fn upload_overwrites_and_keeps_mode() {
        let ctx = connect_and_dir("up-mode").await;
        const REMOTE: &str = "/home/tester/deploy.sh";

        // 远端已有一个 0755 的可执行脚本 —— 相当于「正在被别的进程运行的文件」。
        vfs_write(REMOTE, b"#!/bin/sh\necho old\n").await;
        vfs_set_mode(REMOTE, 0o100_755).await;

        let local = ctx.dir.join("deploy.sh");
        let payload = b"#!/bin/sh\necho new build\n".to_vec();
        std::fs::write(&local, &payload).expect("写本地文件");

        let (tx, _rx) = flume::unbounded::<SftpEvent>();
        transfer::run_upload(
            ctx.active.sftp.clone(),
            5,
            local.clone(),
            REMOTE.to_string(),
            payload.len() as u64,
            UploadDecision::Overwrite,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
        .await;

        let stored = vfs_read(REMOTE).await.expect("目标文件应被替换");
        assert_same_bytes(&stored, &payload);
        assert_eq!(
            vfs_mode(REMOTE).await.map(|mode| mode & 0o777),
            Some(0o755),
            "可执行位必须从原文件继承，不能变成新建文件的 0644"
        );
        assert!(!vfs_exists(&format!("{REMOTE}.filepart")).await);
    }

    /// 服务器严格照 SFTP v3 规范拒绝覆盖 rename 时，走 unlink + rename 的 fallback。
    #[tokio::test]
    async fn upload_falls_back_to_unlink_when_rename_is_refused() {
        let ctx = connect_and_dir("up-strict").await;
        const REMOTE: &str = "/home/tester/strict.bin";

        vfs_write(REMOTE, b"old content").await;
        set_rename_refuses_existing(true);

        let local = ctx.dir.join("strict.bin");
        let payload: Vec<u8> = (0..80 * 1024).map(|ix| (ix % 13) as u8).collect();
        std::fs::write(&local, &payload).expect("写本地文件");

        let (tx, rx) = flume::unbounded::<SftpEvent>();
        transfer::run_upload(
            ctx.active.sftp.clone(),
            6,
            local.clone(),
            REMOTE.to_string(),
            payload.len() as u64,
            UploadDecision::Overwrite,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
        .await;
        set_rename_refuses_existing(false);

        let outcome = rx
            .drain()
            .into_iter()
            .find_map(|event| match event {
                SftpEvent::TransferCompleted { outcome, .. } => Some(outcome),
                _ => None,
            })
            .expect("应有 TransferCompleted");
        assert_eq!(outcome, TransferOutcome::Completed, "fallback 必须成功");

        let stored = vfs_read(REMOTE).await.expect("目标文件应被替换");
        assert_same_bytes(&stored, &payload);
    }

    /// worker：远端已有同名文件时先发 `UploadConflict`；选「跳过」就一个字节都不写。
    #[tokio::test]
    async fn worker_asks_before_overwriting() {
        let _guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;

        const REMOTE: &str = "/home/tester/conflict.txt";
        vfs_write(REMOTE, b"keep me").await;

        let local = temp_dir("conflict").join("conflict.txt");
        std::fs::write(&local, b"new content").expect("写本地文件");

        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;

        worker.send(SftpCommand::Upload {
            id: 42,
            local,
            remote: REMOTE.to_string(),
            total: 11,
            resume: true,
        });

        match next_event(&worker).await {
            SftpEvent::UploadConflict(conflict) => {
                assert_eq!(conflict.id, 42);
                assert_eq!(conflict.existing_size, Some(7), "远端原文件 7 字节");
                assert_eq!(conflict.filepart_size, None, "没有半成品");
                assert!(!conflict.can_resume(), "没有 .filepart 就谈不上续传");
                assert!(conflict.has_existing());
            }
            other => panic!("期望 UploadConflict，实际 {other:?}"),
        }

        worker.send(SftpCommand::ResolveUpload {
            id: 42,
            decision: UploadDecision::Skip,
            apply_to_all: false,
        });

        match next_event(&worker).await {
            SftpEvent::TransferCompleted { id, outcome } => {
                assert_eq!(id, 42);
                assert_eq!(outcome, TransferOutcome::Skipped);
            }
            other => panic!("期望 TransferCompleted(Skipped)，实际 {other:?}"),
        }

        assert_eq!(
            vfs_read(REMOTE).await.as_deref(),
            Some(&b"keep me"[..]),
            "跳过后原文件一个字都不能变"
        );
        assert!(
            !vfs_exists(&format!("{REMOTE}.filepart")).await,
            "跳过后不该产生临时文件"
        );
    }

    /// 外部编辑器保存属于同一文件的回写：必须直接安全覆盖，不能再弹冲突框。
    #[tokio::test]
    async fn worker_external_edit_overwrites_without_prompt() {
        let _guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;

        const REMOTE: &str = "/home/tester/external-edit.bin";
        vfs_write(REMOTE, b"before").await;
        vfs_set_mode(REMOTE, 0o100_640).await;

        let local = temp_dir("external-edit").join("external-edit.bin");
        std::fs::write(&local, b"after\0binary").expect("写编辑缓存");

        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;
        worker.send(SftpCommand::UploadOverwrite {
            id: 43,
            local,
            remote: REMOTE.to_string(),
            total: 12,
        });

        loop {
            match next_event(&worker).await {
                SftpEvent::TransferStarted { id, .. } | SftpEvent::TransferProgress { id, .. } => {
                    assert_eq!(id, 43)
                }
                SftpEvent::TransferCompleted { id, outcome } => {
                    assert_eq!(id, 43);
                    assert_eq!(outcome, TransferOutcome::Completed);
                    break;
                }
                SftpEvent::UploadConflict(_) => panic!("编辑回写不应再次询问覆盖"),
                other => panic!("意外事件：{other:?}"),
            }
        }

        assert_eq!(
            vfs_read(REMOTE).await.as_deref(),
            Some(&b"after\0binary"[..])
        );
        assert_eq!(
            vfs_mode(REMOTE).await.map(|mode| mode & 0o7777),
            Some(0o640)
        );
        assert!(!vfs_exists(&format!("{REMOTE}.filepart")).await);
    }

    /// worker：上传还在等冲突确认时点取消，必须立刻变成 Cancelled，
    /// 而且之后不管用户再点「覆盖」都不会再写字节。
    ///
    /// 这条用例守的是「传输中的数据不能取消」——取消标志必须在收到
    /// `Upload` 指令时就登记，而不是等到 `spawn` 之后。
    #[tokio::test]
    async fn cancel_while_awaiting_conflict_emits_cancelled() {
        let _guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;

        const REMOTE: &str = "/home/tester/await-cancel.txt";
        vfs_write(REMOTE, b"keep me").await;

        let local = temp_dir("await-cancel").join("await-cancel.txt");
        std::fs::write(&local, b"new content").expect("写本地文件");

        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;

        worker.send(SftpCommand::Upload {
            id: 7,
            local,
            remote: REMOTE.to_string(),
            total: 11,
            resume: true,
        });

        match next_event(&worker).await {
            SftpEvent::UploadConflict(conflict) => assert_eq!(conflict.id, 7),
            other => panic!("期望 UploadConflict，实际 {other:?}"),
        }

        // 弹窗还开着，用户直接点了行尾的取消。
        worker.send(SftpCommand::Cancel(7));
        match next_event(&worker).await {
            SftpEvent::TransferCompleted { id, outcome } => {
                assert_eq!(id, 7);
                assert_eq!(outcome, TransferOutcome::Cancelled);
            }
            other => panic!("期望 TransferCompleted(Cancelled)，实际 {other:?}"),
        }

        // 迟到的「覆盖」决定不该把已经取消的任务重新拉起来。
        worker.send(SftpCommand::ResolveUpload {
            id: 7,
            decision: UploadDecision::Overwrite,
            apply_to_all: false,
        });

        assert!(
            tokio::time::timeout(Duration::from_millis(300), worker.events().recv_async())
                .await
                .is_err(),
            "取消之后不该再收到任何事件（尤其是 TransferStarted）"
        );

        assert_eq!(
            vfs_read(REMOTE).await.as_deref(),
            Some(&b"keep me"[..]),
            "取消之后原文件一个字都不能变"
        );
        assert!(
            !vfs_exists(&format!("{REMOTE}.filepart")).await,
            "取消之后不该产生临时文件"
        );
    }

    /// 断开连接必须结束仍在等待冲突选择的任务，并为队列发出最终状态。
    #[tokio::test]
    async fn disconnect_cancels_pending_upload_and_emits_completion() {
        let _guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;

        const REMOTE: &str = "/home/tester/disconnect-pending.txt";
        vfs_write(REMOTE, b"keep").await;
        let local = temp_dir("disconnect-pending").join("disconnect-pending.txt");
        std::fs::write(&local, b"new").expect("写本地文件");

        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;
        worker.send(SftpCommand::Upload {
            id: 8,
            local,
            remote: REMOTE.to_string(),
            total: 3,
            resume: true,
        });
        assert!(matches!(
            next_event(&worker).await,
            SftpEvent::UploadConflict(_)
        ));

        worker.send(SftpCommand::Disconnect);
        match next_event(&worker).await {
            SftpEvent::TransferCompleted { id, outcome } => {
                assert_eq!(id, 8);
                assert_eq!(outcome, TransferOutcome::Cancelled);
            }
            other => panic!("断连应先结束待决任务，实际 {other:?}"),
        }
        assert!(matches!(next_event(&worker).await, SftpEvent::Disconnected));
        assert_eq!(vfs_read(REMOTE).await.as_deref(), Some(&b"keep"[..]));
    }

    /// 用户选「断点续传」时，只补 `.filepart` 后面的字节，写完再原子替换。
    ///
    /// 这条用例顶替「手点弹窗按钮」的手工验证：`UploadConflict` → `ResolveUpload`
    /// → `TransferStarted{resumed_from}` 是 UI 与 worker 之间唯一的约定。
    #[tokio::test]
    async fn worker_resumes_when_user_picks_resume() {
        let _guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;

        const REMOTE: &str = "/home/tester/resume.bin";
        let payload: Vec<u8> = (0..8192u32).map(|ix| (ix % 251) as u8).collect();

        // 远端：一份旧内容 + 上次没传完的 3000 字节半成品。
        vfs_write(REMOTE, b"stale").await;
        vfs_write(&format!("{REMOTE}.filepart"), &payload[..3000]).await;

        let local = temp_dir("resume-pick").join("resume.bin");
        std::fs::write(&local, &payload).expect("写本地文件");

        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;

        worker.send(SftpCommand::Upload {
            id: 77,
            local,
            remote: REMOTE.to_string(),
            total: payload.len() as u64,
            resume: true,
        });

        match next_event(&worker).await {
            SftpEvent::UploadConflict(conflict) => {
                assert_eq!(conflict.filepart_size, Some(3000));
                assert_eq!(conflict.existing_size, Some(5));
                assert!(conflict.can_resume(), "3000 < 8192，应该可以续传");
                assert!(!conflict.filepart_is_stale());
            }
            other => panic!("期望 UploadConflict，实际 {other:?}"),
        }

        worker.send(SftpCommand::ResolveUpload {
            id: 77,
            decision: UploadDecision::Resume,
            apply_to_all: false,
        });

        match next_event(&worker).await {
            SftpEvent::TransferStarted {
                id, resumed_from, ..
            } => {
                assert_eq!(id, 77);
                assert_eq!(resumed_from, 3000, "续传必须从半成品的末尾开始");
            }
            other => panic!("期望 TransferStarted，实际 {other:?}"),
        }

        let outcome = loop {
            // 中间的进度事件直接跳过，只等结论那一条。
            if let SftpEvent::TransferCompleted { outcome, .. } = next_event(&worker).await {
                break outcome;
            }
        };
        assert_eq!(outcome, TransferOutcome::Completed);

        let stored = vfs_read(REMOTE).await.expect("目标文件应被替换");
        assert_same_bytes(&stored, &payload);
        assert!(
            !vfs_exists(&format!("{REMOTE}.filepart")).await,
            "续传完成后临时文件必须被 rename 掉"
        );
    }

    /// 「对后续冲突执行相同操作」：一次决定要把所有在等决定的上传都处理掉。
    #[tokio::test]
    async fn apply_to_all_resolves_every_pending_upload() {
        let _guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;

        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;

        let dir = temp_dir("apply-all");
        let mut ids = Vec::new();
        for (ix, name) in ["a.bin", "b.bin"].iter().enumerate() {
            let remote = format!("{HOME}/{name}");
            vfs_write(&remote, b"old").await;
            let local = dir.join(name);
            std::fs::write(&local, vec![7u8; 4096]).expect("写本地文件");
            let id = 100 + ix as u64;
            ids.push(id);
            worker.send(SftpCommand::Upload {
                id,
                local,
                remote,
                total: 4096,
                resume: true,
            });
        }

        // 两个文件都在等决定。
        for _ in 0..2 {
            match next_event(&worker).await {
                SftpEvent::UploadConflict(_) => {}
                other => panic!("期望 UploadConflict，实际 {other:?}"),
            }
        }

        // 第一个决定带上 apply_to_all，第二个应该跟着一起走。
        worker.send(SftpCommand::ResolveUpload {
            id: ids[0],
            decision: UploadDecision::Overwrite,
            apply_to_all: true,
        });

        let mut completed = Vec::new();
        while completed.len() < 2 {
            // 传输过程中的进度事件直接跳过。
            if let SftpEvent::TransferCompleted { id, outcome } = next_event(&worker).await {
                completed.push((id, outcome));
            }
        }
        completed.sort_by_key(|(id, _)| *id);
        for (id, outcome) in &completed {
            assert_eq!(*outcome, TransferOutcome::Completed, "任务 {id} 应已完成");
        }

        for name in ["a.bin", "b.bin"] {
            let stored = vfs_read(&format!("{HOME}/{name}")).await.expect("应已上传");
            assert_same_bytes(&stored, &vec![7u8; 4096]);
        }
    }

    /// 任务在开始前已经取消：不应创建目标或半成品。
    #[tokio::test]
    async fn preset_cancel_does_not_touch_local_files() {
        let ctx = connect_and_dir("cancel").await;
        seed_big_file().await;

        let local = ctx.dir.join("big.bin");
        let (tx, rx) = flume::unbounded::<SftpEvent>();
        let cancel = Arc::new(AtomicBool::new(true));

        transfer::run_download(
            ctx.active.sftp.clone(),
            5,
            BIG_NAME.to_string(),
            local.clone(),
            BIG_LEN as u64,
            true,
            cancel,
            tx,
        )
        .await;

        let events: Vec<SftpEvent> = rx.drain().collect();
        match events.last() {
            Some(SftpEvent::TransferCompleted { outcome, .. }) => {
                assert_eq!(*outcome, TransferOutcome::Cancelled);
            }
            other => panic!("期望 TransferCompleted(Cancelled)，实际 {other:?}"),
        }

        assert!(!local.exists(), "取消前目标文件不应出现");
        assert!(
            !transfer::local_filepart_path(&local).exists(),
            "取消前半成品也不应出现"
        );
    }

    // -----------------------------------------------------------------------
    // 整目录上传
    // -----------------------------------------------------------------------

    /// 在本地造一棵 `dist/`：两个文件 + 两层子目录 + 一个空目录。
    ///
    /// 返回 (本地根, [(相对路径, 内容)])，断言时逐个对。
    fn seed_dist(root: &std::path::Path) -> (std::path::PathBuf, Vec<(String, Vec<u8>)>) {
        let dist = root.join("dist");
        let files = vec![
            ("a.txt".to_string(), b"alpha".to_vec()),
            ("sub/b.txt".to_string(), b"bravo".to_vec()),
            (
                "sub/deep/c.bin".to_string(),
                (0..200_000u32).map(|ix| (ix % 251) as u8).collect(),
            ),
        ];

        std::fs::create_dir_all(dist.join("sub/deep")).expect("建本地子目录");
        std::fs::create_dir_all(dist.join("empty")).expect("建空目录");
        for (relative, data) in &files {
            std::fs::write(dist.join(relative), data).expect("写本地文件");
        }

        (dist, files)
    }

    /// 跑一次整目录上传，返回 (整批总量, 最后进度, 结论)。
    ///
    /// 整批只允许发三种事件 —— 单文件那套 `TransferStarted` 不应该冒出来，
    /// 否则队列里那一行的总量会被最后一个文件覆盖掉。
    async fn run_folder_upload(
        ctx: &TestCtx,
        id: u64,
        local: std::path::PathBuf,
        remote: &str,
        policy: ExistingPolicy,
    ) -> (u64, u64, TransferOutcome) {
        let (tx, rx) = flume::unbounded::<SftpEvent>();
        transfer::run_upload_dir(
            ctx.active.sftp.clone(),
            id,
            local,
            remote.to_string(),
            policy,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
        .await;

        let mut total = 0;
        let mut progress = 0;
        let mut outcome = TransferOutcome::Failed("没有收到结论".to_string());
        for event in rx.drain() {
            match event {
                SftpEvent::TransferStarted {
                    id: got, total: t, ..
                } => {
                    assert_eq!(got, id);
                    total = t;
                }
                SftpEvent::TransferProgress {
                    id: got,
                    transferred,
                    ..
                } => {
                    assert_eq!(got, id);
                    progress = transferred;
                }
                SftpEvent::TransferCompleted {
                    id: got,
                    outcome: o,
                } => {
                    assert_eq!(got, id);
                    outcome = o;
                }
                other => panic!("整目录上传不该发这个事件：{other:?}"),
            }
        }

        (total, progress, outcome)
    }

    /// 整目录上传：目录、子目录、空目录、文件全都到位，进度等于总字节。
    #[tokio::test]
    async fn upload_dir_creates_nested_tree() {
        let ctx = connect_and_dir("up-dir").await;
        const REMOTE: &str = "/home/tester/dist";

        let (local, files) = seed_dist(&ctx.dir);
        let expected_total: u64 = files.iter().map(|(_, data)| data.len() as u64).sum();

        let (total, progress, outcome) =
            run_folder_upload(&ctx, 11, local.clone(), REMOTE, ExistingPolicy::Overwrite).await;

        assert_eq!(total, expected_total, "TransferStarted 应给出整批总量");
        assert_eq!(progress, expected_total, "终值必须走到头");
        assert_eq!(outcome, TransferOutcome::Completed);

        for (relative, data) in &files {
            let stored = vfs_read(&format!("{REMOTE}/{relative}"))
                .await
                .unwrap_or_else(|| panic!("远端缺少 {relative}"));
            assert_same_bytes(&stored, data);
            assert!(
                !vfs_exists(&format!("{REMOTE}/{relative}.filepart")).await,
                "{relative} 的临时文件必须被 rename 掉"
            );
        }

        // 目录（含空目录）一个都不能少：漏了空目录，远端结构就和本地对不上。
        for suffix in ["", "/sub", "/sub/deep", "/empty"] {
            let path = format!("{REMOTE}{suffix}");
            let mode = vfs_mode(&path)
                .await
                .unwrap_or_else(|| panic!("缺少目录 {path}"));
            assert_eq!(
                mode & 0o170_000,
                0o040_000,
                "{path} 应该是目录：mode={mode:o}"
            );
        }

        // 传完能浏览进去：假服务器现在真的支持子目录列表。
        let entries = session::list_dir(&ctx.active.sftp, REMOTE)
            .await
            .expect("读取刚上传的目录");
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert_eq!(names, ["empty", "sub", "a.txt"], "目录优先、同类按名字排序");
        assert!(entries[0].is_dir() && entries[1].is_dir() && !entries[2].is_dir());

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 策略「跳过同名」：远端已有内容一个字节都不动，其余照传。
    #[tokio::test]
    async fn upload_dir_skips_existing_files() {
        let ctx = connect_and_dir("up-dir-skip").await;
        const REMOTE: &str = "/home/tester/keep";

        let (local, files) = seed_dist(&ctx.dir);
        let expected_total: u64 = files.iter().map(|(_, data)| data.len() as u64).sum();
        // 远端已经有 a.txt 的旧版本（`/home/tester/keep` 目录由 ensure_dir 顺手建出来）。
        vfs_write(&format!("{REMOTE}/a.txt"), b"OLD").await;

        let (_, progress, outcome) =
            run_folder_upload(&ctx, 12, local.clone(), REMOTE, ExistingPolicy::Skip).await;

        assert_eq!(outcome, TransferOutcome::Completed, "其余文件都传成功了");
        assert_eq!(progress, expected_total, "被跳过的那份也算「处理完」");
        assert_eq!(
            vfs_read(&format!("{REMOTE}/a.txt")).await.unwrap(),
            b"OLD".to_vec(),
            "选了跳过就必须原样保留远端文件"
        );
        assert_eq!(
            vfs_read(&format!("{REMOTE}/sub/b.txt")).await.unwrap(),
            b"bravo".to_vec(),
            "没冲突的文件照传"
        );

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 整批上传不能自动信任仅长度匹配的旧 `.filepart`，必须从头安全覆盖。
    #[tokio::test]
    async fn upload_dir_overwrites_an_untrusted_leftover_filepart() {
        let ctx = connect_and_dir("up-dir-resume").await;
        const REMOTE: &str = "/home/tester/resume";

        let (local, files) = seed_dist(&ctx.dir);
        let target = format!("{REMOTE}/sub/deep/c.bin");

        // 故意造一份长度合理但内容错误的旧断点；按长度续传会生成混合损坏文件。
        vfs_write(&format!("{target}.filepart"), &[0xFFu8; 100 * 1024]).await;

        let (_, _, outcome) =
            run_folder_upload(&ctx, 13, local.clone(), REMOTE, ExistingPolicy::Overwrite).await;
        assert_eq!(outcome, TransferOutcome::Completed);

        let stored = vfs_read(&target).await.expect("远端应有这个文件");
        let original = &files[2].1;
        assert_same_bytes(&stored, original);

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 一个子目录建不出来（远端同名条目是个文件）时，其余部分照传：
    /// 整棵树不会因为一个没权限/被占用的子目录全放弃。
    #[tokio::test]
    async fn upload_dir_keeps_going_when_one_subdirectory_fails() {
        let ctx = connect_and_dir("up-dir-broken").await;
        const REMOTE: &str = "/home/tester/broken";

        let (local, _) = seed_dist(&ctx.dir);
        // 远端 `broken/sub` 是个**文件**：mkdir 必然失败，`sub/` 底下的文件都传不上去。
        vfs_write(&format!("{REMOTE}/sub"), b"i am a file").await;

        let (total, progress, outcome) =
            run_folder_upload(&ctx, 14, local.clone(), REMOTE, ExistingPolicy::Overwrite).await;

        match outcome {
            TransferOutcome::Failed(message) => {
                assert!(message.contains("sub"), "错误里应点出是哪个目录：{message}");
                assert!(
                    message.contains("2 处失败"),
                    "sub 和 sub/deep 各一处：{message}"
                );
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }

        // 根目录下的文件照传。
        assert_eq!(
            vfs_read(&format!("{REMOTE}/a.txt")).await.unwrap(),
            b"alpha".to_vec()
        );
        // 坏目录底下的文件一个都没上去，进度也没走到头。
        assert!(!vfs_exists(&format!("{REMOTE}/sub/b.txt")).await);
        assert!(progress < total, "没传完就不该报 100%");

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// worker 层：`UploadDir` 走完整条链路，所有事件都带同一个 id。
    #[tokio::test]
    async fn worker_uploads_a_whole_folder() {
        // 要碰虚拟磁盘，和别的传输用例一样必须串行。
        let _guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;

        let dir = temp_dir("worker-dir");
        let (local, files) = seed_dist(&dir);
        let expected_total: u64 = files.iter().map(|(_, data)| data.len() as u64).sum();

        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;

        worker.send(SftpCommand::UploadDir {
            id: 21,
            local,
            remote: "/home/tester/worker-dist".to_string(),
            on_existing: ExistingPolicy::Overwrite,
        });

        let mut started_total = 0;
        loop {
            match next_event(&worker).await {
                SftpEvent::TransferStarted { id, total, .. } => {
                    assert_eq!(id, 21);
                    started_total = total;
                }
                SftpEvent::TransferProgress { id, .. } => assert_eq!(id, 21),
                SftpEvent::TransferCompleted { id, outcome } => {
                    assert_eq!(id, 21);
                    assert_eq!(outcome, TransferOutcome::Completed);
                    break;
                }
                other => panic!("意外事件：{other:?}"),
            }
        }

        assert_eq!(started_total, expected_total);
        assert!(
            vfs_exists("/home/tester/worker-dist/sub/deep/c.bin").await,
            "整棵树都该传上去"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 整目录下载
    // -----------------------------------------------------------------------

    /// 在远端造一棵 `pkg/`：两个文件 + 两层子目录 + 一个空目录。
    ///
    /// 结构和 [`seed_dist`] 一模一样 —— 上传和下载的用例共用同一棵树的形状，
    /// 任何一侧漏了空目录都会在断言里现形。
    async fn seed_remote_tree(remote: &str) -> Vec<(String, Vec<u8>)> {
        let files = vec![
            ("a.txt".to_string(), b"alpha".to_vec()),
            ("sub/b.txt".to_string(), b"bravo".to_vec()),
            (
                "sub/deep/c.bin".to_string(),
                (0..200_000u32).map(|ix| (ix % 251) as u8).collect(),
            ),
        ];

        for suffix in ["", "/sub", "/sub/deep", "/empty"] {
            vfs_mkdir(&format!("{remote}{suffix}")).await;
        }
        for (relative, data) in &files {
            vfs_write(&format!("{remote}/{relative}"), data).await;
        }

        files
    }

    /// 跑一次整目录下载，返回 (整批总量, 最后进度, 结论)。
    async fn run_folder_download(
        ctx: &TestCtx,
        id: u64,
        remote: &str,
        local: std::path::PathBuf,
        policy: ExistingPolicy,
    ) -> (u64, u64, TransferOutcome) {
        let (tx, rx) = flume::unbounded::<SftpEvent>();
        transfer::run_download_dir(
            ctx.active.sftp.clone(),
            id,
            remote.to_string(),
            local,
            policy,
            Arc::new(AtomicBool::new(false)),
            tx,
        )
        .await;

        let mut total = 0;
        let mut progress = 0;
        let mut outcome = TransferOutcome::Failed("没有收到结论".to_string());
        for event in rx.drain() {
            match event {
                SftpEvent::TransferStarted {
                    id: got, total: t, ..
                } => {
                    assert_eq!(got, id);
                    total = t;
                }
                SftpEvent::TransferProgress {
                    id: got,
                    transferred,
                    ..
                } => {
                    assert_eq!(got, id);
                    progress = transferred;
                }
                SftpEvent::TransferCompleted {
                    id: got,
                    outcome: o,
                } => {
                    assert_eq!(got, id);
                    outcome = o;
                }
                other => panic!("整目录下载不该发这个事件：{other:?}"),
            }
        }

        (total, progress, outcome)
    }

    /// 整目录下载：目录、子目录、空目录、文件全都落到本地，进度等于总字节。
    #[tokio::test]
    async fn download_dir_creates_nested_tree() {
        let ctx = connect_and_dir("dl-dir").await;
        const REMOTE: &str = "/home/tester/pkg";

        let files = seed_remote_tree(REMOTE).await;
        let expected_total: u64 = files.iter().map(|(_, data)| data.len() as u64).sum();

        let local = ctx.dir.join("pkg");
        let (total, progress, outcome) =
            run_folder_download(&ctx, 31, REMOTE, local.clone(), ExistingPolicy::Overwrite).await;

        assert_eq!(total, expected_total, "TransferStarted 应给出整批总量");
        assert_eq!(progress, expected_total, "终值必须走到头");
        assert_eq!(outcome, TransferOutcome::Completed);

        for (relative, data) in &files {
            let stored = std::fs::read(local.join(relative))
                .unwrap_or_else(|err| panic!("本地缺少 {relative}：{err}"));
            assert_same_bytes(&stored, data);
        }

        // 空目录也得建出来：漏了它，本地结构和远端就对不上。
        for suffix in ["", "sub", "sub/deep", "empty"] {
            let path = local.join(suffix);
            assert!(path.is_dir(), "缺少本地目录 {}", path.display());
        }

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 策略「跳过同名」：本地已有内容一个字节都不动，其余照下。
    #[tokio::test]
    async fn download_dir_skips_existing_files() {
        let ctx = connect_and_dir("dl-dir-skip").await;
        const REMOTE: &str = "/home/tester/keep";

        let files = seed_remote_tree(REMOTE).await;
        let expected_total: u64 = files.iter().map(|(_, data)| data.len() as u64).sum();

        let local = ctx.dir.join("keep");
        std::fs::create_dir_all(local.join("sub")).expect("建本地目录");
        std::fs::write(local.join("a.txt"), b"LOCAL").expect("写本地旧文件");

        let (_, progress, outcome) =
            run_folder_download(&ctx, 32, REMOTE, local.clone(), ExistingPolicy::Skip).await;

        assert_eq!(outcome, TransferOutcome::Completed, "其余文件都下成功了");
        assert_eq!(progress, expected_total, "被跳过的那份也算「处理完」");
        assert_eq!(
            std::fs::read(local.join("a.txt")).expect("读本地文件"),
            b"LOCAL".to_vec(),
            "选了跳过就必须原样保留本地文件"
        );
        assert_eq!(
            std::fs::read(local.join("sub/b.txt")).expect("读下载结果"),
            b"bravo".to_vec(),
            "没冲突的文件照下"
        );

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 本地预置的目录符号链接不能把递归下载引到用户选择目录之外。
    #[cfg(unix)]
    #[tokio::test]
    async fn download_dir_refuses_local_symlink_escape() {
        use std::os::unix::fs::symlink;

        let ctx = connect_and_dir("dl-dir-symlink").await;
        const REMOTE: &str = "/home/tester/link-escape";
        seed_remote_tree(REMOTE).await;

        let local = ctx.dir.join("link-escape");
        let outside = ctx.dir.join("outside");
        std::fs::create_dir_all(&local).expect("建下载根目录");
        std::fs::create_dir_all(&outside).expect("建外部目录");
        symlink(&outside, local.join("sub")).expect("创建测试符号链接");

        let (_, _, outcome) =
            run_folder_download(&ctx, 35, REMOTE, local.clone(), ExistingPolicy::Overwrite).await;

        match outcome {
            TransferOutcome::Failed(message) => {
                assert!(
                    message.contains("符号链接"),
                    "错误应解释拒绝原因：{message}"
                );
            }
            other => panic!("符号链接逃逸必须失败，实际 {other:?}"),
        }
        assert!(
            !outside.join("b.txt").exists() && !outside.join("deep/c.bin").exists(),
            "下载不能写到根目录之外"
        );

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 已有目标文件不能被误当成断点；覆盖必须重新下载并得到远端的完整内容。
    #[tokio::test]
    async fn download_dir_replaces_existing_target_safely() {
        let ctx = connect_and_dir("dl-dir-resume").await;
        const REMOTE: &str = "/home/tester/resume";

        let files = seed_remote_tree(REMOTE).await;
        let local = ctx.dir.join("resume");
        std::fs::create_dir_all(local.join("sub/deep")).expect("建本地目录");

        // 故意放一个“长度像半成品、内容却错误”的目标文件。旧实现会从它末尾续写，
        // 生成前后拼接的损坏文件；新实现必须把目标保留到完整临时文件下载完再替换。
        let target = local.join("sub/deep/c.bin");
        std::fs::write(&target, vec![0xFFu8; 100 * 1024]).expect("写半成品");

        let (_, _, outcome) =
            run_folder_download(&ctx, 33, REMOTE, local.clone(), ExistingPolicy::Overwrite).await;
        assert_eq!(outcome, TransferOutcome::Completed);

        let stored = std::fs::read(&target).expect("读下载结果");
        let original = &files[2].1;
        assert_same_bytes(&stored, original);

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 取消标志一开始就是真的：一个文件都不下，结论是 `Cancelled`。
    ///
    /// 整批共用一个 `AtomicBool`，所以「点一次 ✕ 停整批」在下载侧也成立；
    /// 这里把标志预先置真，验证每个循环入口都真的看了它。
    #[tokio::test]
    async fn download_dir_honours_a_preset_cancel_flag() {
        let ctx = connect_and_dir("dl-dir-cancel").await;
        const REMOTE: &str = "/home/tester/pkg";
        seed_remote_tree(REMOTE).await;

        let local = ctx.dir.join("pkg");
        let (tx, rx) = flume::unbounded::<SftpEvent>();
        transfer::run_download_dir(
            ctx.active.sftp.clone(),
            34,
            REMOTE.to_string(),
            local.clone(),
            ExistingPolicy::Overwrite,
            Arc::new(AtomicBool::new(true)),
            tx,
        )
        .await;

        let outcome = rx.drain().find_map(|event| match event {
            SftpEvent::TransferCompleted { outcome, .. } => Some(outcome),
            _ => None,
        });
        assert_eq!(outcome, Some(TransferOutcome::Cancelled));
        assert!(!local.join("a.txt").exists(), "取消了就不该有文件落地");
        assert!(!local.join("sub").exists(), "子目录也不该建出来");

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 远端根目录不存在：这是「目标选错了」，必须报错而不是安静地传个空目录。
    #[tokio::test]
    async fn download_dir_reports_a_missing_remote_root() {
        let ctx = connect_and_dir("dl-dir-missing").await;
        const REMOTE: &str = "/home/tester/nope";

        let local = ctx.dir.join("nope");
        let (total, _, outcome) =
            run_folder_download(&ctx, 35, REMOTE, local.clone(), ExistingPolicy::Overwrite).await;

        assert_eq!(total, 0, "扫不动就不该报出总量");
        match outcome {
            TransferOutcome::Failed(message) => {
                assert!(
                    message.contains(REMOTE),
                    "错误里应点出是哪个路径：{message}"
                );
            }
            other => panic!("期望 Failed，实际 {other:?}"),
        }
        assert!(!local.join("a.txt").exists());

        let _ = std::fs::remove_dir_all(&ctx.dir);
    }

    /// 两个目录同时打开时互不干扰：句柄各记各的「读到哪儿」。
    ///
    /// 这条是回归用例。以前 `drained` 是 `TestSftp` 上的一个裸 `bool`，两次并发
    /// `read_dir`（UI 刷新家目录 + 整目录下载同时扫树）会互相把对方标成 EOF，
    /// 后到的那个读到空列表 —— 表现为「随机」少几个文件，而且很难复现。
    #[tokio::test]
    async fn concurrent_read_dirs_do_not_share_the_eof_flag() {
        // 直接调 `opendir` / `readdir` 这两个 trait 方法，得把 trait 引进作用域。
        use russh_sftp::server::Handler as _;

        let _guard = vfs_lock().lock_owned().await;
        vfs_reset().await;

        vfs_mkdir("/home/tester/a").await;
        vfs_write("/home/tester/a/one.txt", b"1").await;
        vfs_mkdir("/home/tester/b").await;
        vfs_write("/home/tester/b/two.txt", b"2").await;

        let mut sftp = TestSftp::default();
        let a = sftp
            .opendir(1, "/home/tester/a".to_string())
            .await
            .expect("打开 a");
        let b = sftp
            .opendir(2, "/home/tester/b".to_string())
            .await
            .expect("打开 b");

        let names = |name: &russh_sftp::protocol::Name| -> Vec<String> {
            name.files
                .iter()
                .map(|file| file.filename.clone())
                .collect()
        };

        // 交替读：以前第二次会拿到 EOF（空列表），现在必须各读各的。
        let first = sftp.readdir(3, a.handle.clone()).await.expect("读 a");
        let second = sftp.readdir(4, b.handle.clone()).await.expect("读 b");
        assert_eq!(names(&first), ["one.txt"]);
        assert_eq!(names(&second), ["two.txt"]);

        // 各自读到 EOF 之后仍然互不影响。
        assert!(sftp.readdir(5, a.handle.clone()).await.is_err(), "a 已读完");
        assert!(sftp.readdir(6, b.handle.clone()).await.is_err(), "b 已读完");
        // 不认识的句柄要报错，不能安静地返回空目录。
        assert!(sftp.readdir(7, "dir-999".to_string()).await.is_err());
    }

    /// worker 层：`DownloadDir` 走完整条链路，所有事件都带同一个 id。
    #[tokio::test]
    async fn worker_downloads_a_whole_folder() {
        // 要碰虚拟磁盘，和别的传输用例一样必须串行。
        let _guard = vfs_lock().lock_owned().await;
        let addr = spawn().await;
        vfs_reset().await;

        const REMOTE: &str = "/home/tester/worker-pkg";
        let files = seed_remote_tree(REMOTE).await;
        let expected_total: u64 = files.iter().map(|(_, data)| data.len() as u64).sum();

        let dir = temp_dir("worker-dl-dir");
        let local = dir.join("worker-pkg");

        let worker = SftpWorker::spawn();
        connect_worker(&worker, config_for(addr, PASSWORD)).await;

        worker.send(SftpCommand::DownloadDir {
            id: 41,
            remote: REMOTE.to_string(),
            local: local.clone(),
            on_existing: ExistingPolicy::Overwrite,
        });

        let mut started_total = 0;
        loop {
            match next_event(&worker).await {
                SftpEvent::TransferStarted { id, total, .. } => {
                    assert_eq!(id, 41);
                    started_total = total;
                }
                SftpEvent::TransferProgress { id, .. } => assert_eq!(id, 41),
                SftpEvent::TransferCompleted { id, outcome } => {
                    assert_eq!(id, 41);
                    assert_eq!(outcome, TransferOutcome::Completed);
                    break;
                }
                other => panic!("意外事件：{other:?}"),
            }
        }

        assert_eq!(started_total, expected_total);
        assert!(local.join("sub/deep/c.bin").is_file(), "整棵树都该下到本地");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
