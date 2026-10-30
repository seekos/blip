//! SSH 建连、认证与 SFTP 子系统握手。
//!
//! 这个文件里的所有函数都是 `async`，且只能在 [`super::worker`] 的 Tokio runtime 上调用。

use std::fmt;
use std::future::Future;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow};
use log::warn;
use russh::client::{self, Handle};
use russh::keys::{HashAlg, PrivateKey, PrivateKeyWithHashAlg, PublicKey, decode_secret_key};
use russh_sftp::{
    client::{SftpSession, error::Error as SftpClientError},
    protocol::StatusCode,
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::time::timeout;

use super::types::{AuthMethod, EntryKind, FileEntry, SessionConfig};

/// 建连超时：DNS + TCP + 密钥交换通常远快于此，超时意味着网络不可达。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 认证超时：避免用户在密码错误时界面无限转圈。
const AUTH_TIMEOUT: Duration = Duration::from_secs(20);
/// 空闲连接的 SSH 协议级保活间隔；持续有业务数据时 russh 会自动延后发送。
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
/// 最长无响应时间。超过该时间说明连接已经半断，交给界面报告实际操作失败。
const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(60);
/// 连续无响应的保活次数上限。
const KEEPALIVE_MAX: usize = 3;

/// 一次连接允许采用的主机密钥信任方式。
#[derive(Clone)]
pub enum HostKeyPolicy {
    /// 正常连接：严格按 `~/.ssh/known_hosts` 校验。
    KnownHosts,
    /// 用户刚在指纹确认框里选择了信任；重连时只接受这把精确密钥。
    Exact(PublicKey),
}

/// 需要 UI 让用户确认的主机身份。
#[derive(Clone, Debug)]
pub struct HostKeyChallenge {
    pub key: PublicKey,
    pub endpoint: String,
    pub algorithm: String,
    pub fingerprint: String,
    pub changed_line: Option<usize>,
}

#[derive(Debug)]
pub enum ConnectError {
    HostKey(Box<HostKeyChallenge>),
    Other(anyhow::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HostKey(challenge) => write!(
                f,
                "需要确认 SSH 主机密钥：{} {}",
                challenge.endpoint, challenge.fingerprint
            ),
            Self::Other(err) => fmt::Display::fmt(err, f),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<anyhow::Error> for ConnectError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

/// 统一生成 SSH 客户端策略，避免后续重构建连流程时意外丢掉空闲保活。
fn ssh_client_config() -> client::Config {
    client::Config {
        inactivity_timeout: Some(INACTIVITY_TIMEOUT),
        keepalive_interval: Some(KEEPALIVE_INTERVAL),
        keepalive_max: KEEPALIVE_MAX,
        nodelay: true,
        ..Default::default()
    }
}

#[derive(Default)]
struct VerificationState {
    challenge: Option<HostKeyChallenge>,
    error: Option<String>,
}

pub struct ClientHandler {
    host: String,
    port: u16,
    policy: HostKeyPolicy,
    verification: Arc<Mutex<VerificationState>>,
}

impl client::Handler for ClientHandler {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &russh::keys::PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let key = server_public_key.public_key();
        match &self.policy {
            HostKeyPolicy::Exact(expected) => {
                // OpenSSH 注释不属于密钥身份；测试私钥带注释，而握手报文不会带。
                if expected.key_data() == key.key_data() {
                    return Ok(true);
                }
                if let Ok(mut state) = self.verification.lock() {
                    state.error = Some("服务器主机密钥在确认期间再次变化，已拒绝连接".to_string());
                }
                Ok(false)
            }
            HostKeyPolicy::KnownHosts => {
                let changed_line = match russh::keys::check_known_hosts(&self.host, self.port, &key)
                {
                    Ok(true) => return Ok(true),
                    Ok(false) => None,
                    Err(russh::keys::Error::KeyChanged { line }) => Some(line),
                    Err(err) => {
                        if let Ok(mut state) = self.verification.lock() {
                            state.error = Some(format!("读取 ~/.ssh/known_hosts 失败：{err}"));
                        }
                        return Ok(false);
                    }
                };

                let challenge = HostKeyChallenge {
                    endpoint: if self.port == 22 {
                        self.host.clone()
                    } else {
                        format!("[{}]:{}", self.host, self.port)
                    },
                    algorithm: key.algorithm().as_str().to_string(),
                    fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                    key,
                    changed_line,
                };
                if let Ok(mut state) = self.verification.lock() {
                    state.challenge = Some(challenge);
                }
                Ok(false)
            }
        }
    }
}

/// 把用户确认过的主机密钥写入标准 `known_hosts`。
///
/// 新主机走 russh 的追加逻辑；密钥变化时必须替换冲突行，否则旧行仍会让下一次连接
/// 报 `KeyChanged`，用户刚保存的决定完全不起作用。
pub fn remember_host_key(
    config: &SessionConfig,
    key: &PublicKey,
    changed_line: Option<usize>,
) -> Result<(), String> {
    if changed_line.is_some() {
        // 用户核对指纹期间文件可能被 ssh 或别的进程改过；重新定位冲突行，绝不按过期
        // 行号覆盖一个无关主机。若别人已经写好则直接成功，条目被移除则转为追加。
        match russh::keys::check_known_hosts(&config.host, config.port, key) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(russh::keys::Error::KeyChanged { line }) => {
                let path = dirs::home_dir()
                    .ok_or_else(|| "找不到用户主目录，无法更新 known_hosts".to_string())?
                    .join(".ssh/known_hosts");
                return replace_known_host_line(&path, line, key);
            }
            Err(err) => return Err(format!("重新检查 ~/.ssh/known_hosts 失败：{err}")),
        }
    }
    russh::keys::known_hosts::learn_known_hosts(&config.host, config.port, key)
        .map_err(|err| format!("写入 ~/.ssh/known_hosts 失败：{err}"))
}

fn replace_known_host_line(path: &Path, line: usize, key: &PublicKey) -> Result<(), String> {
    let original = std::fs::read_to_string(path)
        .map_err(|err| format!("读取 {} 失败：{err}", path.display()))?;
    let had_final_newline = original.ends_with('\n');
    let mut lines: Vec<String> = original.lines().map(str::to_string).collect();
    let index = line
        .checked_sub(1)
        .filter(|index| *index < lines.len())
        .ok_or_else(|| format!("known_hosts 第 {line} 行已不存在，请重新连接后再确认"))?;
    let host_field = lines[index]
        .split_whitespace()
        .next()
        .ok_or_else(|| format!("known_hosts 第 {line} 行格式无效"))?
        .to_string();
    let encoded = key
        .to_openssh()
        .map_err(|err| format!("编码服务器主机密钥失败：{err}"))?;
    lines[index] = format!("{host_field} {encoded}");

    let mut updated = lines.join("\n");
    if had_final_newline {
        updated.push('\n');
    }

    let parent = path
        .parent()
        .ok_or_else(|| "known_hosts 路径没有父目录".to_string())?;
    let nonce = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = parent.join(format!(
        ".known_hosts.macscp-{}-{nonce}",
        std::process::id()
    ));
    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|err| format!("创建 known_hosts 临时文件失败：{err}"))?;
        if let Ok(metadata) = std::fs::metadata(path) {
            std::fs::set_permissions(&temp, metadata.permissions())
                .map_err(|err| format!("保留 known_hosts 权限失败：{err}"))?;
        }
        file.write_all(updated.as_bytes())
            .and_then(|_| file.sync_all())
            .map_err(|err| format!("写入 known_hosts 临时文件失败：{err}"))?;
        std::fs::rename(&temp, path).map_err(|err| format!("替换 {} 失败：{err}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

struct TransferLane {
    handle: Option<Arc<Handle<ClientHandler>>>,
    sftp: Arc<SftpSession>,
    source: Option<Ipv4Addr>,
    in_flight: AtomicUsize,
    slots: Arc<Semaphore>,
}

const MAX_TRANSFERS_PER_LANE: usize = 8;

impl TransferLane {
    fn is_healthy(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(|handle| !handle.is_closed())
    }
}

struct TransferPoolInner {
    lanes: Mutex<Vec<Arc<TransferLane>>>,
    cursor: AtomicUsize,
    closed: AtomicBool,
    available: Notify,
}

/// 上传文件使用的 SFTP 连接池。每条连接固定绑定一张网卡的本地 IPv4。
#[derive(Clone)]
pub struct TransferPool {
    inner: Arc<TransferPoolInner>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransferPoolStatus {
    pub sources: Vec<String>,
    pub max_concurrency: usize,
}

impl TransferPool {
    fn with_primary(
        handle: Arc<Handle<ClientHandler>>,
        sftp: Arc<SftpSession>,
        source: Option<Ipv4Addr>,
    ) -> Self {
        Self {
            inner: Arc::new(TransferPoolInner {
                lanes: Mutex::new(vec![Arc::new(TransferLane {
                    handle: Some(handle),
                    sftp,
                    source,
                    in_flight: AtomicUsize::new(0),
                    slots: Arc::new(Semaphore::new(MAX_TRANSFERS_PER_LANE)),
                })]),
                cursor: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                available: Notify::new(),
            }),
        }
    }

    /// 测试和单会话调用方仍可沿用原来的 SFTP 会话。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn single(sftp: Arc<SftpSession>) -> Self {
        Self {
            inner: Arc::new(TransferPoolInner {
                lanes: Mutex::new(vec![Arc::new(TransferLane {
                    handle: None,
                    sftp,
                    source: None,
                    in_flight: AtomicUsize::new(0),
                    slots: Arc::new(Semaphore::new(MAX_TRANSFERS_PER_LANE)),
                })]),
                cursor: AtomicUsize::new(0),
                closed: AtomicBool::new(false),
                available: Notify::new(),
            }),
        }
    }

    pub(crate) fn add_lane(
        &self,
        handle: Arc<Handle<ClientHandler>>,
        sftp: Arc<SftpSession>,
        source: Ipv4Addr,
    ) {
        if self.inner.closed.load(Ordering::Relaxed) {
            return;
        }
        let Ok(mut lanes) = self.inner.lanes.lock() else {
            warn!("传输连接池状态异常，忽略本机地址 {source}");
            return;
        };
        if lanes.iter().any(|lane| lane.source == Some(source)) {
            return;
        }
        lanes.push(Arc::new(TransferLane {
            handle: Some(handle),
            sftp,
            source: Some(source),
            in_flight: AtomicUsize::new(0),
            slots: Arc::new(Semaphore::new(MAX_TRANSFERS_PER_LANE)),
        }));
        log::info!(
            "传输连接池已启用本机地址 {source}，当前 {} 条连接",
            lanes.len()
        );
        self.inner.available.notify_waiters();
    }

    pub(crate) async fn acquire(&self) -> TransferLease {
        loop {
            let notified = self.inner.available.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            let candidate = {
                let lanes = self
                    .inner
                    .lanes
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let start = self.inner.cursor.fetch_add(1, Ordering::Relaxed);
                let loads = lanes
                    .iter()
                    .map(|lane| {
                        (lane.is_healthy() && lane.slots.available_permits() > 0)
                            .then(|| lane.in_flight.load(Ordering::Relaxed))
                    })
                    .collect::<Vec<_>>();
                let selected = select_lane_index(&loads, start).or_else(|| {
                    (!lanes.iter().any(|lane| lane.is_healthy())).then(|| {
                        lanes
                            .iter()
                            .position(|lane| lane.slots.available_permits() > 0)
                    })?
                });
                selected.map(|index| lanes[index].clone())
            };

            if let Some(lane) = candidate
                && let Ok(permit) = lane.slots.clone().try_acquire_owned()
            {
                lane.in_flight.fetch_add(1, Ordering::Relaxed);
                return TransferLease {
                    lane,
                    inner: self.inner.clone(),
                    _permit: permit,
                };
            }
            notified.await;
        }
    }

    pub(crate) fn status(&self) -> TransferPoolStatus {
        let lanes = self
            .inner
            .lanes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let sources = lanes
            .iter()
            .filter(|lane| lane.is_healthy())
            .map(|lane| {
                lane.source
                    .map(|source| source.to_string())
                    .unwrap_or_else(|| "系统默认路由".to_string())
            })
            .collect::<Vec<_>>();
        TransferPoolStatus {
            max_concurrency: sources.len() * MAX_TRANSFERS_PER_LANE,
            sources,
        }
    }

    fn close(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.inner.available.notify_waiters();
    }
}

pub(crate) struct TransferLease {
    lane: Arc<TransferLane>,
    inner: Arc<TransferPoolInner>,
    _permit: OwnedSemaphorePermit,
}

impl TransferLease {
    pub(crate) fn sftp(&self) -> Arc<SftpSession> {
        self.lane.sftp.clone()
    }

    #[cfg(test)]
    pub(crate) fn source(&self) -> Option<Ipv4Addr> {
        self.lane.source
    }
}

impl Drop for TransferLease {
    fn drop(&mut self) {
        self.lane.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.inner.available.notify_one();
    }
}

fn select_lane_index(loads: &[Option<usize>], start: usize) -> Option<usize> {
    let len = loads.len();
    (0..len)
        .filter_map(|offset| {
            let index = (start + offset) % len;
            loads[index].map(|load| (index, load, offset))
        })
        .min_by_key(|(_, load, offset)| (*load, *offset))
        .map(|(index, _, _)| index)
}

/// 一条已建立的连接。主会话负责目录与控制操作，传输池负责分配上传文件。
pub struct ActiveSession {
    #[allow(dead_code)]
    pub handle: Arc<Handle<ClientHandler>>,
    pub sftp: Arc<SftpSession>,
    pub home: String,
    pub transfer_pool: TransferPool,
    primary_source: Option<Ipv4Addr>,
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.transfer_pool.close();
    }
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
/// 常见容错，都来自真实踩过的坑：
/// * 填的是目录（`~/.ssh`）→ 在里面按 [`DEFAULT_KEY_NAMES`] 挑一把；
/// * 误选 `.pub` 公钥 → 使用同目录同名私钥；
/// * 文件不存在 → 说清楚是路径问题，而不是甩一句 IO 错误；
/// * 没有读权限 → 给出对应平台能执行的处理方式。
fn resolve_existing_key(path: &Path) -> Result<PathBuf> {
    let mut resolved = resolve_key_path(path)?;

    if resolved.is_dir() {
        return discover_private_key_in(&resolved).ok_or_else(|| {
            anyhow!(
                "{} 是目录，里面没有 id_ed25519 / id_ecdsa / id_rsa / id_dsa",
                resolved.display()
            )
        });
    }

    // Windows 默认隐藏扩展名，id_ed25519 与 id_ed25519.pub 很容易选反。
    if resolved
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pub"))
    {
        let private_key = resolved.with_extension("");
        if private_key.is_file() {
            resolved = private_key;
        }
    }

    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_file() => Ok(resolved),
        Ok(_) => Err(anyhow!("{} 不是普通文件，无法当作私钥", resolved.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Err(anyhow!(
            "找不到私钥 {}：路径写错了？留空则自动查找 ~/.ssh/id_ed25519",
            resolved.display()
        )),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            #[cfg(target_os = "windows")]
            let message = format!(
                "没有权限读取私钥 {}：请在文件属性的“安全”中允许当前用户读取",
                resolved.display()
            );
            #[cfg(not(target_os = "windows"))]
            let message = format!(
                "没有权限读取私钥 {}：试试 chmod 600 {}",
                resolved.display(),
                resolved.display()
            );
            Err(anyhow!(message))
        }
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
    let encoded = read_private_key_text(&key_path)?;
    match decode_secret_key(&encoded, passphrase) {
        Ok(key) => Ok(key),
        Err(err) => Err(explain_key_error(&key_path, passphrase, err)),
    }
}

fn read_private_key_text(path: &Path) -> Result<String> {
    let bytes =
        std::fs::read(path).map_err(|err| anyhow!("无法读取私钥 {}：{err}", path.display()))?;
    let decoded: std::result::Result<String, ()> =
        if let Some(bytes) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
            String::from_utf8(bytes.to_vec()).map_err(|_| ())
        } else if let Some(bytes) = bytes.strip_prefix(&[0xFF, 0xFE]) {
            decode_utf16(bytes, u16::from_le_bytes)
        } else if let Some(bytes) = bytes.strip_prefix(&[0xFE, 0xFF]) {
            decode_utf16(bytes, u16::from_be_bytes)
        } else {
            String::from_utf8(bytes).map_err(|_| ())
        };
    decoded.map_err(|_| {
        anyhow!(
            "私钥 {} 不是有效的 UTF-8/UTF-16 文本，请选择 OpenSSH、PEM 或 PuTTY PPK 私钥",
            path.display()
        )
    })
}

fn decode_utf16(bytes: &[u8], decode: impl Fn([u8; 2]) -> u16) -> std::result::Result<String, ()> {
    if !bytes.len().is_multiple_of(2) {
        return Err(());
    }
    let units = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| decode(*chunk))
        .collect::<Vec<_>>();
    String::from_utf16(&units).map_err(|_| ())
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
const CONNECT_ATTEMPTS: usize = 4;

/// 每次重试前等多久：400ms、1200ms。
///
/// 刻意很短（这类失败通常只需要「再发一个 SYN」），但也留够 ARP 重新解析的
/// 时间 —— 只在已经失败的路径上多花这 1.6 秒，成功路径零成本。
/// 数组长度和 [`CONNECT_ATTEMPTS`] 绑在一起：改次数忘了改等待会直接编译不过。
const CONNECT_RETRY_DELAYS: [Duration; CONNECT_ATTEMPTS - 1] = [
    Duration::from_millis(400),
    Duration::from_millis(1200),
    Duration::from_millis(2000),
];

/// 查询系统访问目标 IPv4 时会选择的本地出口地址，不发送任何数据。
fn default_ipv4_source(target: Ipv4Addr) -> Option<Ipv4Addr> {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            socket.connect((target, 9))?;
            socket.local_addr()
        })
        .ok()
        .and_then(|address| match address.ip() {
            std::net::IpAddr::V4(address) => Some(address),
            std::net::IpAddr::V6(_) => None,
        })
}

/// 为同网段重复地址准备真正不同的 TCP 出口。
///
/// 第一次仍交给系统路由；后续优先绑定同一子网里的其它本机 IPv4。仅仅重复
/// `TcpStream::connect` 不会换路由，在两张网卡同时接入同一网段时会连续走错接口。
fn connection_sources(host: &str) -> [Option<Ipv4Addr>; CONNECT_ATTEMPTS] {
    // shortcut: 只处理设备列表使用的字面量 IPv4；主机名和 IPv6 仍使用系统路由。
    let Ok(target) = host.parse::<Ipv4Addr>() else {
        return [None; CONNECT_ATTEMPTS];
    };
    let default_source = default_ipv4_source(target);
    let networks = netdev::get_interfaces()
        .into_iter()
        .filter(|interface| {
            interface.is_up() && !interface.is_loopback() && !interface.is_point_to_point()
        })
        .flat_map(|interface| {
            interface
                .ipv4
                .into_iter()
                .map(|network| (network.addr(), network.prefix_len()))
        });
    connection_sources_from(target, default_source, networks)
}

/// 附加传输连接可以走不同网段，只要该物理网卡有到服务器的有效路由。
fn transfer_sources(host: &str) -> Vec<Ipv4Addr> {
    let Ok(target) = host.parse::<Ipv4Addr>() else {
        return Vec::new();
    };
    if target.is_loopback() {
        return Vec::new();
    }
    let default_source = default_ipv4_source(target);
    let mut candidates = netdev::get_interfaces()
        .into_iter()
        .filter(|interface| interface.is_up() && interface.is_physical())
        .flat_map(|interface| {
            let speed = interface.transmit_speed.unwrap_or(0);
            interface.ipv4.into_iter().map(move |network| {
                let source = network.addr();
                let direct = same_ipv4_subnet(source, target, network.prefix_len());
                (source, direct, speed)
            })
        })
        .filter(|(source, _, _)| {
            Some(*source) != default_source && !source.is_unspecified() && !source.is_loopback()
        })
        .collect::<Vec<_>>();
    candidates
        .sort_by_key(|(_, direct, speed)| (std::cmp::Reverse(*direct), std::cmp::Reverse(*speed)));
    candidates.dedup_by_key(|(source, _, _)| *source);
    candidates
        .into_iter()
        .map(|(source, _, _)| source)
        .take(CONNECT_ATTEMPTS - 1)
        .collect()
}

fn connection_sources_from(
    target: Ipv4Addr,
    default_source: Option<Ipv4Addr>,
    networks: impl IntoIterator<Item = (Ipv4Addr, u8)>,
) -> [Option<Ipv4Addr>; CONNECT_ATTEMPTS] {
    let mut sources = [None; CONNECT_ATTEMPTS];
    let mut next = 1;
    for (source, prefix_len) in networks {
        if next == CONNECT_ATTEMPTS {
            break;
        }
        if Some(source) == default_source
            || source.is_unspecified()
            || source.is_loopback()
            || !same_ipv4_subnet(source, target, prefix_len)
            || sources[..next].contains(&Some(source))
        {
            continue;
        }
        sources[next] = Some(source);
        next += 1;
    }
    sources
}

fn same_ipv4_subnet(left: Ipv4Addr, right: Ipv4Addr, prefix_len: u8) -> bool {
    if prefix_len > 32 {
        return false;
    }
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - prefix_len)
    };
    u32::from(left) & mask == u32::from(right) & mask
}

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
/// 要能说清楚「已经自动重试过多次还是不行」，否则用户会以为程序没努力过。
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
pub async fn connect(
    config: &SessionConfig,
    host_key_policy: HostKeyPolicy,
) -> std::result::Result<ActiveSession, ConnectError> {
    // russh 会在没有业务数据时发送 SSH keepalive；服务器的响应也会重置空闲计时。
    let ssh_config = Arc::new(ssh_client_config());
    let active =
        connect_with_ssh_config(config, host_key_policy.clone(), ssh_config.clone()).await?;
    spawn_transfer_lanes(
        config.clone(),
        host_key_policy,
        ssh_config,
        active.primary_source,
        active.transfer_pool.clone(),
    );
    Ok(active)
}

async fn connect_with_ssh_config(
    config: &SessionConfig,
    host_key_policy: HostKeyPolicy,
    ssh_config: Arc<client::Config>,
) -> std::result::Result<ActiveSession, ConnectError> {
    connect_with_sources(
        config,
        host_key_policy,
        ssh_config,
        connection_sources(&config.host),
    )
    .await
}

async fn connect_with_sources(
    config: &SessionConfig,
    host_key_policy: HostKeyPolicy,
    ssh_config: Arc<client::Config>,
    sources: [Option<Ipv4Addr>; CONNECT_ATTEMPTS],
) -> std::result::Result<ActiveSession, ConnectError> {
    let verification = Arc::new(Mutex::new(VerificationState::default()));

    let addr = (config.host.as_str(), config.port);
    let target = config.host.parse::<Ipv4Addr>().ok();
    // TCP + 版本交换 + 密钥交换这一段带重试（见 `is_transient_connect_error`）；
    // 后面的认证与 SFTP 握手不重试 —— 那两步失败重试也没用。
    let handle_result = connect_with_retries(|round| {
        let ssh_config = ssh_config.clone();
        let verification = verification.clone();
        let host_key_policy = host_key_policy.clone();
        let host = config.host.clone();
        let port = config.port;
        let source = sources[round - 1];
        async move {
            let transport = timeout(CONNECT_TIMEOUT, async {
                let handler = ClientHandler {
                    host,
                    port,
                    policy: host_key_policy,
                    verification,
                };
                let Some((source, target)) = source.zip(target) else {
                    return client::connect(ssh_config, addr, handler)
                        .await
                        .map(|handle| (handle, None));
                };

                warn!("建连第 {round}/{CONNECT_ATTEMPTS} 次改用本机地址 {source}");
                let socket = tokio::net::TcpSocket::new_v4().map_err(russh::Error::IO)?;
                socket
                    .bind(SocketAddr::from((source, 0)))
                    .map_err(russh::Error::IO)?;
                let stream = socket
                    .connect(SocketAddr::from((target, port)))
                    .await
                    .map_err(russh::Error::IO)?;
                if ssh_config.nodelay
                    && let Err(err) = stream.set_nodelay(true)
                {
                    warn!("设置 TCP_NODELAY 失败：{err}");
                }
                client::connect_stream(ssh_config, stream, handler)
                    .await
                    .map(|handle| (handle, Some(source)))
            })
            .await;
            match transport {
                Ok(result) => result,
                // 超时用 russh 自己的错误表示，`explain_connect_error` 会认它。
                Err(_) => Err(russh::Error::ConnectionTimeout),
            }
        }
    })
    .await;

    let (mut handle, primary_source) = match handle_result {
        Ok(connected) => connected,
        Err((err, rounds)) => {
            let mut state = verification
                .lock()
                .map_err(|_| ConnectError::Other(anyhow!("主机密钥校验状态异常，已拒绝连接")))?;
            if let Some(challenge) = state.challenge.take() {
                return Err(ConnectError::HostKey(Box::new(challenge)));
            }
            if let Some(message) = state.error.take() {
                return Err(ConnectError::Other(anyhow!(message)));
            }
            return Err(ConnectError::Other(explain_connect_error(
                config, &err, rounds,
            )));
        }
    };
    // 默认路由成功时 russh 不返回底层 socket；用同一目标的路由选择补出实际
    // 本地 IPv4，既用于状态栏展示，也避免附加连接重复使用这张网卡。
    let primary_source = primary_source.or_else(|| target.and_then(default_ipv4_source));

    let auth_result = match &config.auth {
        AuthMethod::Password(password) => timeout(
            AUTH_TIMEOUT,
            handle.authenticate_password(config.username.clone(), password.clone()),
        )
        .await
        .map_err(|_| anyhow!("认证超时"))?
        .map_err(|err| anyhow!("认证失败：{err}"))?,
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
        )
        .into());
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

    let handle = Arc::new(handle);
    let transfer_pool = TransferPool::with_primary(handle.clone(), sftp.clone(), primary_source);
    Ok(ActiveSession {
        handle,
        sftp,
        home,
        transfer_pool,
        primary_source,
    })
}

fn spawn_transfer_lanes(
    config: SessionConfig,
    host_key_policy: HostKeyPolicy,
    ssh_config: Arc<client::Config>,
    primary_source: Option<Ipv4Addr>,
    pool: TransferPool,
) {
    for source in transfer_sources(&config.host) {
        if Some(source) == primary_source {
            continue;
        }
        let config = config.clone();
        let host_key_policy = host_key_policy.clone();
        let ssh_config = ssh_config.clone();
        let pool = pool.clone();
        tokio::spawn(async move {
            let sources = [Some(source); CONNECT_ATTEMPTS];
            match connect_with_sources(&config, host_key_policy, ssh_config, sources).await {
                Ok(extra) => pool.add_lane(extra.handle.clone(), extra.sftp.clone(), source),
                Err(err) => warn!("本机地址 {source} 的附加 SFTP 连接不可用：{err}"),
            }
        });
    }
}

/// 测试时缩短保活周期，验证真实 TCP/SSH/SFTP 空闲连接，而不用让测试等待 15 秒。
#[cfg(test)]
pub(crate) async fn connect_with_keepalive_for_test(
    config: &SessionConfig,
    host_key_policy: HostKeyPolicy,
    interval: Duration,
    inactivity_timeout: Duration,
) -> std::result::Result<ActiveSession, ConnectError> {
    let ssh_config = client::Config {
        keepalive_interval: Some(interval),
        keepalive_max: KEEPALIVE_MAX,
        inactivity_timeout: Some(inactivity_timeout),
        nodelay: true,
        ..Default::default()
    };
    connect_with_ssh_config(config, host_key_policy, Arc::new(ssh_config)).await
}

/// 列出远端目录。
///
/// 每条 [`FileEntry`] 都会带上大小、修改时间与 POSIX mode（含文件类型位，
/// 例如目录是 `0o040_755`），最后按「文件夹优先、名称字母序」排序返回。
/// 返回 `Err(String)` 而不是 `anyhow::Error`：这条路径上的错误会直接展示给用户，
/// 不需要保留错误链。
pub async fn list_dir(sftp: &SftpSession, path: &str) -> Result<Vec<FileEntry>, String> {
    read_dir(sftp, path).await.map_err(|err| err.message)
}

/// 读取上次的远端目录；只有 SFTP 明确返回 `NoSuchFile` 时才改读
/// `fallback`。权限不足、连接中断和超时仍保留原错误，不伪装成默认目录。
pub async fn list_dir_or_fallback(
    sftp: &SftpSession,
    path: &str,
    fallback: Option<&str>,
) -> Result<(String, Vec<FileEntry>), String> {
    match read_dir(sftp, path).await {
        Ok(entries) => Ok((path.to_string(), entries)),
        Err(err)
            if err.not_found
                && fallback.is_some_and(|fallback| !fallback.is_empty() && fallback != path) =>
        {
            let fallback = fallback.expect("上面已检查 fallback 存在");
            let entries = read_dir(sftp, fallback)
                .await
                .map_err(|fallback_err| fallback_err.message)?;
            Ok((fallback.to_string(), entries))
        }
        Err(err) => Err(err.message),
    }
}

struct DirectoryReadError {
    message: String,
    not_found: bool,
}

async fn read_dir(sftp: &SftpSession, path: &str) -> Result<Vec<FileEntry>, DirectoryReadError> {
    let read_dir = sftp
        .read_dir(path)
        .await
        .map_err(|err| DirectoryReadError {
            not_found: matches!(
                err,
                SftpClientError::Status(ref status)
                    if status.status_code == StatusCode::NoSuchFile
            ),
            message: format!("读取目录 {path} 失败：{err}"),
        })?;

    let mut entries = Vec::new();
    for entry in read_dir {
        let name = entry.file_name();
        if name == "." || name == ".." {
            continue;
        }
        if !is_safe_entry_name(&name) {
            warn!("跳过服务器返回的危险文件名：{name:?}");
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
            hidden: name.starts_with('.'),
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

/// SFTP `readdir` 的文件名必须只是一个普通路径分量。
///
/// 正常 Unix 文件名不会含 `/`，但恶意服务器可以伪造 `../x` 或绝对路径；如果直接
/// 拼成本地下载目标，就会逃出用户选择的目录。
pub fn is_safe_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

pub async fn create_dir(sftp: &SftpSession, path: &str) -> Result<(), String> {
    sftp.create_dir(path)
        .await
        .map_err(|err| format!("创建目录 {path} 失败：{err}"))
}

#[derive(Debug)]
pub struct RemoveError {
    pub message: String,
    /// 出错前已经永久删除的内部条目数。
    pub removed: usize,
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.removed == 0 {
            f.write_str(&self.message)
        } else {
            write!(
                f,
                "{}（此前已删除 {} 个内部条目）",
                self.message, self.removed
            )
        }
    }
}

const MAX_REMOVE_DEPTH: usize = 64;

/// 删除一个远端条目，返回实际删除的文件/目录总数。
///
/// 目录先完整预检再开始删除，避免读到一半才因权限失败；每个待遍历路径均用 lstat
/// 重新确认，绝不跟随符号链接。真正删除阶段仍可能遇到并发变化，此时错误会准确带上
/// 已永久删除的条目数。
pub async fn remove(
    sftp: &SftpSession,
    path: &str,
    expected_dir: bool,
    cancel: &AtomicBool,
) -> std::result::Result<usize, RemoveError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(RemoveError {
            message: "删除已取消".to_string(),
            removed: 0,
        });
    }
    let root = sftp
        .symlink_metadata(path)
        .await
        .map_err(|err| RemoveError {
            message: format!("读取待删除路径 {path} 失败：{err}"),
            removed: 0,
        })?;
    let actual_dir = root.is_dir() && !root.is_symlink();
    if actual_dir != expected_dir {
        return Err(RemoveError {
            message: format!("{path} 的类型在确认删除后发生变化，已拒绝操作"),
            removed: 0,
        });
    }
    if !actual_dir {
        sftp.remove_file(path).await.map_err(|err| RemoveError {
            message: format!("删除 {path} 失败：{err}"),
            removed: 0,
        })?;
        return Ok(1);
    }

    // 预检阶段只读不删：先确认整棵树可枚举且没有目录链接/异常深度。
    let mut directories = vec![path.to_string()];
    let mut files = Vec::new();
    let mut pending = vec![(path.to_string(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        if cancel.load(Ordering::Relaxed) {
            return Err(RemoveError {
                message: "删除已取消".to_string(),
                removed: 0,
            });
        }
        if depth >= MAX_REMOVE_DEPTH {
            return Err(RemoveError {
                message: format!("目录层级超过 {MAX_REMOVE_DEPTH}，拒绝递归删除：{directory}"),
                removed: 0,
            });
        }
        let entries = list_dir(sftp, &directory)
            .await
            .map_err(|message| RemoveError {
                message,
                removed: 0,
            })?;
        for entry in entries {
            let metadata = sftp
                .symlink_metadata(&entry.path)
                .await
                .map_err(|err| RemoveError {
                    message: format!("检查待删除条目 {} 失败：{err}", entry.path),
                    removed: 0,
                })?;
            if metadata.is_dir() && !metadata.is_symlink() {
                directories.push(entry.path.clone());
                pending.push((entry.path, depth + 1));
            } else {
                files.push(entry.path);
            }
        }
    }

    let mut removed = 0usize;
    for file in files {
        if cancel.load(Ordering::Relaxed) {
            return Err(RemoveError {
                message: "删除已取消".to_string(),
                removed,
            });
        }
        let metadata = sftp
            .symlink_metadata(&file)
            .await
            .map_err(|err| RemoveError {
                message: format!("删除前重新检查 {file} 失败：{err}"),
                removed,
            })?;
        if metadata.is_dir() && !metadata.is_symlink() {
            return Err(RemoveError {
                message: format!("{file} 在删除期间变成了目录，已停止"),
                removed,
            });
        }
        sftp.remove_file(&file).await.map_err(|err| RemoveError {
            message: format!("删除 {file} 失败：{err}"),
            removed,
        })?;
        removed += 1;
    }

    for directory in directories.into_iter().rev() {
        if cancel.load(Ordering::Relaxed) {
            return Err(RemoveError {
                message: "删除已取消".to_string(),
                removed,
            });
        }
        let metadata = sftp
            .symlink_metadata(&directory)
            .await
            .map_err(|err| RemoveError {
                message: format!("删除前重新检查 {directory} 失败：{err}"),
                removed,
            })?;
        if !metadata.is_dir() || metadata.is_symlink() {
            return Err(RemoveError {
                message: format!("{directory} 在删除期间不再是原目录，已停止"),
                removed,
            });
        }
        sftp.remove_dir(&directory)
            .await
            .map_err(|err| RemoveError {
                message: format!("删除 {directory} 失败：{err}"),
                removed,
            })?;
        removed += 1;
    }
    Ok(removed)
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
    fn ssh_client_config_keeps_idle_connections_alive() {
        let config = ssh_client_config();

        assert_eq!(config.keepalive_interval, Some(Duration::from_secs(15)));
        assert_eq!(config.keepalive_max, 3);
        assert_eq!(config.inactivity_timeout, Some(Duration::from_secs(60)));
        assert!(config.nodelay);
    }

    #[test]
    fn join_remote_never_double_slashes() {
        assert_eq!(join_remote("/", "etc"), "/etc");
        assert_eq!(join_remote("/home/zy", "docs"), "/home/zy/docs");
        assert_eq!(join_remote("/home/zy/", "docs"), "/home/zy/docs");
    }

    #[test]
    fn entry_names_must_be_single_safe_components() {
        assert!(is_safe_entry_name("report.txt"));
        assert!(is_safe_entry_name("a\\b"), "反斜杠在 Unix 上是普通字符");
        for unsafe_name in ["", ".", "..", "../secret", "a/b", "a/", "/tmp/owned"] {
            assert!(!is_safe_entry_name(unsafe_name), "应拒绝 {unsafe_name:?}");
        }
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
            hidden: name.starts_with('.'),
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
        let result = connect(&config, HostKeyPolicy::KnownHosts).await;
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

    #[test]
    fn retries_lan_ip_through_an_alternate_interface() {
        let target = Ipv4Addr::new(192, 168, 146, 7);
        let sources = connection_sources_from(
            target,
            Some(Ipv4Addr::new(192, 168, 146, 2)),
            [
                (Ipv4Addr::new(10, 0, 0, 5), 24),
                (Ipv4Addr::new(192, 168, 146, 2), 24),
                (Ipv4Addr::new(192, 168, 146, 4), 24),
            ],
        );

        assert_eq!(
            sources,
            [None, Some(Ipv4Addr::new(192, 168, 146, 4)), None, None]
        );
    }

    #[test]
    fn transfer_pool_prefers_the_least_loaded_healthy_lane() {
        let loads = [Some(3), None, Some(1), Some(1)];
        assert_eq!(select_lane_index(&loads, 0), Some(2));
        assert_eq!(select_lane_index(&loads, 3), Some(3));
        assert_eq!(select_lane_index(&[None, None], 0), None);
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

    /// 一直不可达 → 试满全部次数就放弃，并把次数一起报上来。
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
    fn selecting_a_public_key_uses_its_private_key_pair() {
        let dir = std::env::temp_dir().join(format!(
            "macscp-public-key-selection-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时密钥目录");
        let private_key = dir.join("id_ed25519");
        let public_key = dir.join("id_ed25519.pub");
        std::fs::write(&private_key, crate::sftp::testserver::CLIENT_KEY).expect("写私钥");
        std::fs::write(&public_key, crate::sftp::testserver::CLIENT_KEY_PUBLIC).expect("写公钥");

        assert_eq!(
            resolve_existing_key(&public_key).expect("应找到配套私钥"),
            private_key
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_private_keys_saved_with_windows_byte_order_marks() {
        let dir = std::env::temp_dir().join(format!(
            "macscp-windows-key-encoding-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时密钥目录");

        let utf8_path = dir.join("utf8-bom-key");
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend_from_slice(crate::sftp::testserver::CLIENT_KEY.as_bytes());
        std::fs::write(&utf8_path, utf8).expect("写 UTF-8 BOM 私钥");
        load_private_key(&utf8_path, None).expect("应读取 UTF-8 BOM 私钥");

        let utf16_path = dir.join("utf16-key");
        let mut utf16 = vec![0xFF, 0xFE];
        for unit in crate::sftp::testserver::CLIENT_KEY.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&utf16_path, utf16).expect("写 UTF-16 私钥");
        load_private_key(&utf16_path, None).expect("应读取 UTF-16 私钥");

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

    #[test]
    fn changed_known_host_line_is_replaced_atomically() {
        let dir = std::env::temp_dir().join(format!(
            "macscp-known-hosts-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        let path = dir.join("known_hosts");
        let old =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICI3sgKh1PV+Banw9IaXhAEMKHTyAhBmRyx8MEhL3rdo";
        let new =
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICaEpv0wklfEupBq0hro0QrkwY2Mci/5DVHF4JVqTOjQ";
        std::fs::write(
            &path,
            format!("other.example {old}\nexample.com,alias.example {old}\n"),
        )
        .expect("写 known_hosts");
        let key = PublicKey::from_openssh(new).expect("解析新主机密钥");

        replace_known_host_line(&path, 2, &key).expect("替换冲突行");

        let updated = std::fs::read_to_string(&path).expect("读回 known_hosts");
        assert!(updated.starts_with(&format!("other.example {old}\n")));
        assert!(updated.contains(&format!("example.com,alias.example {new}\n")));
        assert!(!updated.contains(".known_hosts.macscp"));
        assert!(
            russh::keys::check_known_hosts_path("example.com", 22, &key, &path)
                .expect("校验 known_hosts"),
            "保存后的下一次连接应直接通过"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
