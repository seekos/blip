//! SSH 建连、认证与 SFTP 子系统握手。
//!
//! 这个文件里的所有函数都是 `async`，且只能在 [`super::worker`] 的 Tokio runtime 上调用。

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Result};
use log::warn;
use russh::client::{self, Handle};
use russh::keys::{load_secret_key, HashAlg, PrivateKey, PrivateKeyWithHashAlg};
use russh_sftp::client::SftpSession;
use tokio::time::timeout;

use super::types::{AuthMethod, EntryKind, FileEntry, SessionConfig};

/// 建连超时：DNS + TCP + 密钥交换通常远快于此，超时意味着网络不可达。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 认证超时：避免用户在密码错误时界面无限转圈。
const AUTH_TIMEOUT: Duration = Duration::from_secs(20);

pub struct ClientHandler;

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        _server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // 说明：这里接受任意主机公钥（首次连接即信任）。
        // 生产环境应改为与 `~/.ssh/known_hosts` 比对，或在指纹变化时向 UI 抛出确认事件。
        Ok(true)
    }
}

/// 一条已建立的连接。`handle` 只用来“持有”SSH 会话，读不读无所谓，但必须保活。
pub struct ActiveSession {
    #[allow(dead_code)]
    pub handle: Handle<ClientHandler>,
    pub sftp: Arc<SftpSession>,
    pub home: String,
}

/// 自动查找私钥时的候选文件名，按优先级排列。
pub const DEFAULT_KEY_NAMES: [&str; 4] = ["id_ed25519", "id_ecdsa", "id_rsa", "id_dsa"];

/// `~/.ssh` 下的私钥自动发现。
pub fn discover_private_key() -> Option<PathBuf> {
    discover_private_key_in(&dirs::home_dir()?.join(".ssh"))
}

/// 在指定目录里按 [`DEFAULT_KEY_NAMES`] 的顺序找第一个存在的文件。
pub fn discover_private_key_in(ssh_dir: &Path) -> Option<PathBuf> {
    DEFAULT_KEY_NAMES
        .iter()
        .map(|name| ssh_dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// 显式路径原样返回；空路径表示“自动查找”。
fn resolve_key_path(path: &Path) -> Result<PathBuf> {
    if !path.as_os_str().is_empty() {
        return Ok(path.to_path_buf());
    }
    discover_private_key().ok_or_else(|| {
        anyhow!("没有找到可用的私钥：请指定路径，或把密钥放到 ~/.ssh/id_ed25519（或 id_rsa）")
    })
}

/// 把用户填的路径落到一个**真实存在的私钥文件**上。
///
/// 三种容错，都来自真实踩过的坑：
/// * 填的是目录（`~/.ssh`）→ 在里面按 [`DEFAULT_KEY_NAMES`] 挑一把；
/// * 文件不存在 → 说清楚是路径问题，而不是甩一句 IO 错误；
/// * 没有读权限 → 直接提示 `chmod 600`（OpenSSH 也会拒绝 0644 的私钥）。
fn resolve_existing_key(path: &Path) -> Result<PathBuf> {
    let resolved = resolve_key_path(path)?;

    if resolved.is_dir() {
        return discover_private_key_in(&resolved).ok_or_else(|| {
            anyhow!(
                "{} 是目录，里面没有 id_ed25519 / id_ecdsa / id_rsa / id_dsa",
                resolved.display()
            )
        });
    }

    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_file() => Ok(resolved),
        Ok(_) => Err(anyhow!("{} 不是普通文件，无法当作私钥", resolved.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "找不到私钥 {}：路径写错了？留空则自动查找 ~/.ssh/id_ed25519",
            resolved.display()
        )),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Err(anyhow!(
            "没有权限读取私钥 {}：试试 chmod 600 {}",
            resolved.display(),
            resolved.display()
        )),
        Err(err) => Err(anyhow!("无法访问私钥 {}：{err}", resolved.display())),
    }
}

/// 读私钥，并把失败翻译成一句「用户知道下一步该干什么」的中文。
///
/// 最常见的两种失败是**没给口令**和**口令给错**，russh 原文分别是
/// `The key is encrypted` 和 `SshKey: cryptographic error` —— 直接抛给用户
/// 等于什么都没说。
fn load_private_key(path: &Path, passphrase: Option<&str>) -> Result<PrivateKey> {
    let key_path = resolve_existing_key(path)?;
    match load_secret_key(&key_path, passphrase) {
        Ok(key) => Ok(key),
        Err(err) => Err(explain_key_error(&key_path, passphrase, err)),
    }
}

fn explain_key_error(
    path: &Path,
    passphrase: Option<&str>,
    err: russh::keys::Error,
) -> anyhow::Error {
    use russh::keys::Error as KeyError;

    match err {
        // 加密私钥 + 没给口令：明确要求口令，并给出「去掉口令」的备选。
        KeyError::KeyIsEncrypted => anyhow!(
            "私钥 {} 已加密，需要口令：在工具栏的「口令」框里填一次；\
             或者运行 ssh-keygen -p -N \"\" -f {} 去掉口令",
            path.display(),
            path.display()
        ),
        // 给了口令但解不开：口令错了（也可能是文件被截断，但九成是口令）。
        KeyError::SshKey(_) | KeyError::Unpad(_) | KeyError::Pad(_) if passphrase.is_some() => {
            anyhow!("私钥口令不正确（{}），请重新输入", path.display())
        }
        other => anyhow!("无法读取私钥 {}：{other}", path.display()),
    }
}

/// 公钥认证该按什么顺序试签名算法。
///
/// 只有 RSA 需要这个：ed25519 / ECDSA 的算法由密钥本身定死，
/// `PrivateKeyWithHashAlg` 会忽略传进去的 `hash_alg`。
async fn rsa_hash_candidates(
    handle: &Handle<ClientHandler>,
    key: &PrivateKey,
) -> Vec<Option<HashAlg>> {
    if !matches!(key.algorithm(), russh::keys::Algorithm::Rsa { .. }) {
        return vec![None];
    }

    match handle.best_supported_rsa_hash().await {
        // 服务器明确报了 rsa-sha2-*，照它说的签。
        Ok(Some(Some(alg))) => vec![Some(alg)],
        // 服务器明说只有老的 ssh-rsa（SHA-1）。
        Ok(Some(None)) => vec![None],
        // 没收到 server-sig-algs 扩展（不少服务器不发 EXT_INFO）：
        // 先按现代默认的 sha2-256 试，被拒再退到 SHA-1。
        _ => vec![Some(HashAlg::Sha256), None],
    }
}

/// 建连尝试次数（含首次）。
const CONNECT_ATTEMPTS: usize = 3;

/// 每次重试前等多久：400ms、1200ms。
///
/// 刻意很短（这类失败通常只需要「再发一个 SYN」），但也留够 ARP 重新解析的
/// 时间 —— 只在已经失败的路径上多花这 1.6 秒，成功路径零成本。
/// 数组长度和 [`CONNECT_ATTEMPTS`] 绑在一起：改次数忘了改等待会直接编译不过。
const CONNECT_RETRY_DELAYS: [Duration; CONNECT_ATTEMPTS - 1] = [
    Duration::from_millis(400),
    Duration::from_millis(1200),
];

/// 建连失败的原始错误 + 已经试过几次。
type ConnectFailure = (russh::Error, usize);

/// 这个错误值得再发一次 SYN 吗？
///
/// **典型场景（这个函数存在的理由）**：一台 Mac 上有多张网卡共用同一网段
/// （例如 Wi-Fi `en0` 192.168.146.4 + 虚拟机桥接 `en6` 192.168.146.2），
/// 路由表里同一个 `/24` 有两条 link 路由。ARP 缓存过期之后，内核可能把第一个
/// SYN 发到一张 ARP 已经失效的网卡上，于是 `connect(2)` 直接返回
/// `EHOSTUNREACH`（"No route to host"）；而正是这次失败的 SYN 触发了 ARP
/// 重新解析，紧接着重试就通了 —— 用户看到的就是「第一次点连接报错，第二次才连上」。
///
/// **哪些不重试**：超时（用户已经等了 20 秒，再叠加两次只会让结论来得更晚）、
/// 端口没开（`ConnectionRefused`）、认证被拒 —— 这些重试多少次结果都一样。
fn is_transient_connect_error(err: &russh::Error) -> bool {
    use std::io::ErrorKind;

    match err {
        russh::Error::IO(io) => matches!(
            io.kind(),
            ErrorKind::HostUnreachable
                | ErrorKind::NetworkUnreachable
                | ErrorKind::ConnectionAborted
                | ErrorKind::ConnectionReset
        ),
        // 对端在握手途中把连接关掉（负载均衡踢掉空闲连接、sshd 正在重启…）。
        russh::Error::HUP | russh::Error::Disconnect => true,
        _ => false,
    }
}

/// 反复调用 `attempt`，直到成功、碰到不可重试的错误、或次数用尽。
///
/// 失败时把「最后一次的原始错误」和「总共试了几次」一起返回 —— 最终文案里
/// 要能说清楚「已经自动重试过 3 次还是不行」，否则用户会以为程序没努力过。
async fn connect_with_retries<T, F, Fut>(mut attempt: F) -> std::result::Result<T, ConnectFailure>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = std::result::Result<T, russh::Error>>,
{
    for round in 1..CONNECT_ATTEMPTS {
        match attempt(round).await {
            Ok(value) => return Ok(value),
            Err(err) if is_transient_connect_error(&err) => {
                let delay = CONNECT_RETRY_DELAYS[round - 1];
                warn!(
                    "建连第 {round}/{CONNECT_ATTEMPTS} 次失败（{err}），{} ms 后重试",
                    delay.as_millis()
                );
                tokio::time::sleep(delay).await;
            }
            Err(err) => return Err((err, round)),
        }
    }

    attempt(CONNECT_ATTEMPTS)
        .await
        .map_err(|err| (err, CONNECT_ATTEMPTS))
}

/// 把建连错误翻译成一句「用户知道下一步该干什么」的中文。
///
/// russh 的原文是 `No route to host (os error 65)` 这种，直接抛给用户等于没说。
fn explain_connect_error(
    config: &SessionConfig,
    err: &russh::Error,
    rounds: usize,
) -> anyhow::Error {
    use std::io::ErrorKind;

    let endpoint = format!("{}:{}", config.host, config.port);
    let detail = match err {
        russh::Error::ConnectionTimeout => format!(
            "连接超时：{endpoint} 在 {} 秒内没有响应",
            CONNECT_TIMEOUT.as_secs()
        ),
        russh::Error::IO(io) => match io.kind() {
            ErrorKind::HostUnreachable => format!(
                "主机不可达（No route to host）：{endpoint}。\
                 检查主机是否在线、与本机是否在同一网段"
            ),
            ErrorKind::NetworkUnreachable => format!("网络不可达：本机没有到 {endpoint} 的路由"),
            ErrorKind::ConnectionRefused => {
                format!("连接被拒绝：{endpoint} 上没有 SSH 服务，或被防火墙挡住")
            }
            ErrorKind::TimedOut => format!("连接超时：{endpoint} 没有响应"),
            ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted => {
                format!("连接被重置：{endpoint}")
            }
            _ => format!("无法建立 SSH 连接：{io}"),
        },
        russh::Error::HUP | russh::Error::Disconnect => format!("连接被对端关闭：{endpoint}"),
        other => format!("无法建立 SSH 连接：{other}"),
    };

    if rounds > 1 {
        anyhow!("{detail}（已自动重试 {rounds} 次）")
    } else {
        anyhow!("{detail}")
    }
}

/// 建立 SSH 连接并打开 SFTP 子系统。
///
/// 任何一步失败都会返回人类可读的错误，由 worker 转成 [`super::types::SftpEvent::Error`]。
pub async fn connect(config: &SessionConfig) -> Result<ActiveSession> {
    let ssh_config = Arc::new(client::Config {
        // 传输大文件时长时间没有通道数据是正常的，交给 TCP keepalive。
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    });

    let addr = (config.host.as_str(), config.port);
    // TCP + 版本交换 + 密钥交换这一段带重试（见 `is_transient_connect_error`）；
    // 后面的认证与 SFTP 握手不重试 —— 那两步失败重试也没用。
    let mut handle = connect_with_retries(|_round| {
        let ssh_config = ssh_config.clone();
        async move {
            let transport = timeout(
                CONNECT_TIMEOUT,
                client::connect(ssh_config, addr, ClientHandler),
            )
            .await;
            match transport {
                Ok(result) => result,
                // 超时用 russh 自己的错误表示，`explain_connect_error` 会认它。
                Err(_) => Err(russh::Error::ConnectionTimeout),
            }
        }
    })
    .await
    .map_err(|(err, rounds)| explain_connect_error(config, &err, rounds))?;

    let auth_result = match &config.auth {
        AuthMethod::Password(password) => {
            timeout(
                AUTH_TIMEOUT,
                handle.authenticate_password(config.username.clone(), password.clone()),
            )
            .await
            .map_err(|_| anyhow!("认证超时"))?
            .map_err(|err| anyhow!("认证失败：{err}"))?
        }
        AuthMethod::PrivateKey { path, passphrase } => {
            // `load_private_key` 内部会做路径解析（空路径 = 自动查找）、
            // 存在性检查和「加密/口令错」的翻译。
            let key = Arc::new(load_private_key(path, passphrase.as_deref())?);

            // RSA 的签名算法要跟服务器商量：老服务器只认 ssh-rsa（SHA-1），
            // 新服务器（OpenSSH ≥ 8.8）默认**拒绝** SHA-1。
            // 拿不到 server-sig-algs 时先按 sha2-256 试，被拒再退到 SHA-1。
            let candidates = rsa_hash_candidates(&handle, &key).await;
            let mut result = None;
            for hash_alg in candidates {
                let attempt = timeout(
                    AUTH_TIMEOUT,
                    handle.authenticate_publickey(
                        config.username.clone(),
                        PrivateKeyWithHashAlg::new(key.clone(), hash_alg),
                    ),
                )
                .await
                .map_err(|_| anyhow!("认证超时"))?
                .map_err(|err| anyhow!("认证失败：{err}"))?;

                if attempt.success() {
                    result = Some(attempt);
                    break;
                }
                result = Some(attempt);
            }
            result.expect("至少尝试一种签名算法")
        }
    };

    if !auth_result.success() {
        return Err(anyhow!(
            "认证被拒绝：请检查用户名与{}",
            match config.auth {
                AuthMethod::Password(_) => "密码".to_string(),
                AuthMethod::PrivateKey { .. } => {
                    "私钥（公钥要加到服务器的 ~/.ssh/authorized_keys 里）".to_string()
                }
            }
        ));
    }

    let channel = timeout(CONNECT_TIMEOUT, handle.channel_open_session())
        .await
        .map_err(|_| anyhow!("打开 SSH 会话通道超时"))?
        .map_err(|err| anyhow!("打开 SSH 会话通道失败：{err}"))?;

    timeout(CONNECT_TIMEOUT, channel.request_subsystem(true, "sftp"))
        .await
        .map_err(|_| anyhow!("启动 SFTP 子系统超时"))?
        .map_err(|err| anyhow!("服务器拒绝 SFTP 子系统：{err}"))?;

    let sftp = timeout(CONNECT_TIMEOUT, SftpSession::new(channel.into_stream()))
        .await
        .map_err(|_| anyhow!("SFTP 握手超时"))?
        .map_err(|err| anyhow!("SFTP 握手失败：{err}"))?;

    let sftp = Arc::new(sftp);

    // 登录目录：优先用服务器给出的规范路径，失败则退回根目录。
    let home = sftp
        .canonicalize(".")
        .await
        .unwrap_or_else(|_| "/".to_string());

    Ok(ActiveSession {
        handle,
        sftp,
        home,
    })
}

/// 列出远端目录。
///
/// 每条 [`FileEntry`] 都会带上大小、修改时间与 POSIX mode（含文件类型位，
/// 例如目录是 `0o040_755`），最后按「文件夹优先、名称字母序」排序返回。
/// 返回 `Err(String)` 而不是 `anyhow::Error`：这条路径上的错误会直接展示给用户，
/// 不需要保留错误链。
pub async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<FileEntry>, String> {
    let read_dir = sftp
        .read_dir(path)
        .await
        .map_err(|err| format!("读取目录 {path} 失败：{err}"))?;

    let mut entries = Vec::new();
    for entry in read_dir {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        let meta = entry.metadata();
        let file_type = entry.file_type();
        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };

        entries.push(FileEntry {
            path: join_remote(path, &name),
            name,
            kind,
            size: meta.size.unwrap_or(0),
            modified: meta.modified().ok(),
            mode: meta.permissions,
        });
    }

    sort_entries(&mut entries);
    Ok(entries)
}

pub async fn create_dir(sftp: &SftpSession, path: &str) -> Result<(), String> {
    sftp.create_dir(path)
        .await
        .map_err(|err| format!("创建目录 {path} 失败：{err}"))
}

pub async fn remove(sftp: &SftpSession, path: &str, is_dir: bool) -> Result<(), String> {
    let result = if is_dir {
        sftp.remove_dir(path).await
    } else {
        sftp.remove_file(path).await
    };
    result.map_err(|err| format!("删除 {path} 失败：{err}"))
}

pub async fn rename(sftp: &SftpSession, from: &str, to: &str) -> Result<(), String> {
    sftp.rename(from, to)
        .await
        .map_err(|err| format!("重命名 {from} → {to} 失败：{err}"))
}

/// 目录在前、同类按名称排序（大小写不敏感），和 Finder 的习惯一致。
pub fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        let a_dir = a.is_dir();
        let b_dir = b.is_dir();
        b_dir
            .cmp(&a_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
}

/// 拼接 POSIX 路径，避免出现 `//`。
pub fn join_remote(parent: &str, name: &str) -> String {
    if parent == "/" {
        format!("/{name}")
    } else if parent.ends_with('/') {
        format!("{parent}{name}")
    } else {
        format!("{parent}/{name}")
    }
}

/// 远端父目录；根目录的父目录仍是根目录。
pub fn remote_parent(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(ix) => trimmed[..ix].to_string(),
    }
}

/// 把 `SystemTime` 渲染成 `2026-10-23 11:09`，失败时给占位符。
pub fn format_time(time: Option<SystemTime>) -> String {
    use chrono::{DateTime, Local};
    match time {
        Some(time) => {
            let dt: DateTime<Local> = time.into();
            dt.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "—".to_string(),
    }
}

/// 文件大小的人类可读形式。
pub fn format_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// 本地路径的父目录。
pub fn local_parent(path: &std::path::Path) -> Option<PathBuf> {
    path.parent().map(|p| p.to_path_buf())
}

/// 把 POSIX mode 位渲染成 `drwxr-xr-x` 这样的字符串。
pub fn format_mode(mode: Option<u32>) -> String {
    let Some(mode) = mode else {
        return "—".to_string();
    };

    let mut out = String::with_capacity(10);
    // S_IFMT 掩码，否则 0o100_644（普通文件）会被误判成符号链接。
    out.push(match mode & 0o170_000 {
        0o040_000 => 'd',
        0o120_000 => 'l',
        _ => '-',
    });

    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 0o7;
        out.push(if bits & 0o4 != 0 { 'r' } else { '-' });
        out.push(if bits & 0o2 != 0 { 'w' } else { '-' });
        out.push(if bits & 0o1 != 0 { 'x' } else { '-' });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::types::SessionConfig;

    #[test]
    fn join_remote_never_double_slashes() {
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(join_remote("/home/zy", "docs"), "/home/zy/docs");
        assert_eq!(join_remote("/home/zy/", "docs"), "/home/zy/docs");
    }

    #[test]
    fn remote_parent_walks_up_and_stops_at_root() {
        assert_eq!(remote_parent("/home/zy/docs"), "/home/zy");
        assert_eq!(remote_parent("/home/zy/docs/"), "/home/zy");
        assert_eq!(remote_parent("/home"), "/");
        assert_eq!(remote_parent("/"), "/");
    }

    #[test]
    fn format_size_is_human_readable() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(84_213_760), "80.3 MB");
    }

    #[test]
    fn format_mode_renders_posix_bits() {
        assert_eq!(format_mode(Some(0o040_755)), "drwxr-xr-x");
        assert_eq!(format_mode(Some(0o100_644)), "-rw-r--r--");
        assert_eq!(format_mode(None), "—");
    }

    #[test]
    fn sort_entries_puts_directories_first() {
        let mk = |name: &str, kind: EntryKind| FileEntry {
            name: name.to_string(),
            path: name.to_string(),
            kind,
            size: 0,
            modified: None,
            mode: None,
        };
        let mut entries = vec![
            mk("zeta.txt", EntryKind::File),
            mk("Beta", EntryKind::Directory),
            mk("alpha", EntryKind::Directory),
        ];
        sort_entries(&mut entries);
        let names: Vec<_> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "Beta", "zeta.txt"]);
    }

    /// 连接一个必然拒绝的端口：必须返回错误而不是 panic 或挂死。
    ///
    /// 顺带守住「拒绝不重试」：端口没开是确定性失败，重试只会拖慢报错。
    #[tokio::test]
    async fn connect_to_closed_port_returns_error() {
        let config = SessionConfig {
            host: "127.0.0.1".to_string(),
            port: 1,
            username: "nobody".to_string(),
            auth: AuthMethod::Password("secret".to_string()),
        };

        let started = std::time::Instant::now();
        let result = connect(&config).await;
        let elapsed = started.elapsed();

        let err = result.err().expect("连接关闭端口必须失败");
        assert!(
            err.to_string().contains("连接被拒绝"),
            "unexpected error: {err}"
        );
        // 重试至少会等 400 + 1200 ms，落在 1 秒以上。
        assert!(
            elapsed < Duration::from_secs(1),
            "拒绝不该重试，却花了 {elapsed:?}"
        );
    }

    /// 「No route to host」这类错误必须被认成瞬时失败 —— 否则「点两次才能连上」
    /// 那个 bug 会原样回来。
    #[test]
    fn only_unreachable_kinds_are_retried() {
        use std::io::ErrorKind;

        for kind in [
            ErrorKind::HostUnreachable,
            ErrorKind::NetworkUnreachable,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
        ] {
            assert!(
                is_transient_connect_error(&russh::Error::IO(std::io::Error::from(kind))),
                "{kind:?} 应该重试"
            );
        }
        assert!(is_transient_connect_error(&russh::Error::HUP));
        assert!(is_transient_connect_error(&russh::Error::Disconnect));

        // 确定性失败：重试没有意义。
        for kind in [
            ErrorKind::ConnectionRefused,
            ErrorKind::PermissionDenied,
            ErrorKind::AddrNotAvailable,
        ] {
            assert!(
                !is_transient_connect_error(&russh::Error::IO(std::io::Error::from(kind))),
                "{kind:?} 不该重试"
            );
        }
        assert!(!is_transient_connect_error(
            &russh::Error::ConnectionTimeout
        ));
        assert!(!is_transient_connect_error(&russh::Error::Version));
    }

    /// 前两次「主机不可达」、第三次成功 → 整体成功，且确实试了三次。
    #[tokio::test]
    async fn transient_failure_is_retried_until_success() {
        use std::cell::Cell;
        use std::io::ErrorKind;
        use std::rc::Rc;

        let calls = Rc::new(Cell::new(0usize));
        let result = connect_with_retries(|_round| {
            let calls = calls.clone();
            async move {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err(russh::Error::IO(std::io::Error::from(
                        ErrorKind::HostUnreachable,
                    )))
                } else {
                    Ok(calls.get())
                }
            }
        })
        .await;

        assert_eq!(result.expect("第三次应该成功"), 3);
        assert_eq!(calls.get(), 3);
    }

    /// 一直不可达 → 试满三次就放弃，并把次数一起报上来（文案里要写「已重试 3 次」）。
    #[tokio::test]
    async fn persistent_failure_gives_up_after_all_attempts() {
        use std::io::ErrorKind;

        let result: std::result::Result<(), _> = connect_with_retries(|_round| async {
            Err(russh::Error::IO(std::io::Error::from(
                ErrorKind::HostUnreachable,
            )))
        })
        .await;

        let (err, rounds) = result.expect_err("一直不可达必须失败");
        assert_eq!(rounds, CONNECT_ATTEMPTS);
        assert!(matches!(err, russh::Error::IO(_)));
    }

    /// 不可重试的错误只试一次就返回。
    #[tokio::test]
    async fn deterministic_failure_is_not_retried() {
        use std::cell::Cell;
        use std::io::ErrorKind;
        use std::rc::Rc;

        let calls = Rc::new(Cell::new(0usize));
        let result: std::result::Result<(), _> = connect_with_retries(|_round| {
            let calls = calls.clone();
            async move {
                calls.set(calls.get() + 1);
                Err(russh::Error::IO(std::io::Error::from(
                    ErrorKind::ConnectionRefused,
                )))
            }
        })
        .await;

        assert_eq!(result.expect_err("必须失败").1, 1);
        assert_eq!(calls.get(), 1);
    }

    /// 文案要落到「主机不可达」上，而不是把 russh 的英文原文甩给用户。
    #[test]
    fn unreachable_host_error_is_explained() {
        let config = SessionConfig {
            host: "192.168.146.7".to_string(),
            port: 22,
            username: "root".to_string(),
            auth: AuthMethod::Password(String::new()),
        };
        let err = explain_connect_error(
            &config,
            &russh::Error::IO(std::io::Error::from(std::io::ErrorKind::HostUnreachable)),
            3,
        );
        let message = err.to_string();

        assert!(message.contains("主机不可达"), "{message}");
        assert!(message.contains("192.168.146.7:22"), "{message}");
        assert!(message.contains("已自动重试 3 次"), "{message}");
    }

    #[test]
    fn discover_private_key_prefers_ed25519() {
        let dir = std::env::temp_dir().join(format!("macscp-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");

        // 空目录 → 找不到。
        assert!(discover_private_key_in(&dir).is_none());

        // 只有 id_rsa → 选中 id_rsa。
        std::fs::write(dir.join("id_rsa"), b"x").expect("写 id_rsa");
        assert_eq!(discover_private_key_in(&dir), Some(dir.join("id_rsa")));

        // 同时存在时 ed25519 优先。
        std::fs::write(dir.join("id_ed25519"), b"x").expect("写 id_ed25519");
        assert_eq!(discover_private_key_in(&dir), Some(dir.join("id_ed25519")));

        // 目录同名不算私钥。
        std::fs::remove_file(dir.join("id_ed25519")).expect("删 id_ed25519");
        std::fs::create_dir_all(dir.join("id_ed25519")).expect("建同名目录");
        assert_eq!(discover_private_key_in(&dir), Some(dir.join("id_rsa")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_key_path_keeps_explicit_path() {
        let explicit = PathBuf::from("/tmp/definitely-not-a-key");
        assert_eq!(
            resolve_key_path(&explicit).expect("显式路径不该被改写"),
            explicit
        );
    }
}
