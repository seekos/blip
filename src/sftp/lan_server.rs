//! 内置局域网 SFTP 服务端。
//!
//! 服务随应用启动，优先监听 `0.0.0.0:2022`；端口被占用时让系统分配空闲端口。
//! 虚拟根目录默认映射到当前用户主目录，也可在设置中切换；任何越界路径与指向共享
//! 目录外的符号链接都会被拒绝。登录密码默认为 `88888888`，也可在设置中修改；
//! 主机密钥会持久化，避免
//! 客户端反复提示变更。

use std::collections::HashMap;
use std::io::{ErrorKind, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use std::time::Duration;

use russh::keys::{Algorithm, PrivateKey, ssh_key::LineEnding};
use russh::server::{Auth, ChannelOpenHandle, Msg, Response, Server as _, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode, Version,
};
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::sync::Mutex;

const DEFAULT_PORT: u16 = 2022;
const USERNAME: &str = "zy";
pub(crate) const DEFAULT_PASSWORD: &str = "88888888";
const READ_DIR_BATCH: usize = 128;

fn authentication_methods() -> MethodSet {
    MethodSet::from(&[MethodKind::Password, MethodKind::KeyboardInteractive][..])
}

#[derive(Clone)]
pub struct TransferActivity {
    state: Arc<StdMutex<TransferActivityState>>,
}

struct TransferActivityState {
    active: usize,
    events: flume::Sender<bool>,
}

struct TransferActivityGuard {
    activity: TransferActivity,
}

/// 创建内置服务端传输活动监视器；`true`/`false` 分别表示首项开始和末项结束。
pub fn transfer_activity() -> (TransferActivity, flume::Receiver<bool>) {
    let (events, receiver) = flume::unbounded();
    (
        TransferActivity {
            state: Arc::new(StdMutex::new(TransferActivityState { active: 0, events })),
        },
        receiver,
    )
}

impl TransferActivity {
    fn begin(&self) -> TransferActivityGuard {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active += 1;
        if state.active == 1 {
            let _ = state.events.send(true);
        }
        TransferActivityGuard {
            activity: self.clone(),
        }
    }
}

impl Drop for TransferActivityGuard {
    fn drop(&mut self) {
        let mut state = self
            .activity
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            let _ = state.events.send(false);
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServerInfo {
    pub host: String,
    pub port: u16,
    pub username: String,
    password: Arc<RwLock<String>>,
    root: Arc<RwLock<PathBuf>>,
}

impl ServerInfo {
    pub fn endpoint(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    pub fn command(&self) -> String {
        format!("sftp -P {} {}@{}", self.port, self.username, self.host)
    }

    pub fn password(&self) -> String {
        self.password
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn set_password(&self, manual_password: Option<String>) -> Result<(), String> {
        let password = selected_password(manual_password)?;
        *self
            .password
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = password;
        Ok(())
    }

    pub fn root(&self) -> PathBuf {
        self.root
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn set_root(&self, root: PathBuf) -> Result<(), String> {
        let root = canonical_root(Some(root))?;
        *self
            .root
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = root;
        Ok(())
    }
}

/// 启动内置服务。返回后监听端口已经占好，不存在 UI 先显示、随后端口被抢走的竞态。
pub fn start(
    configured_root: Option<PathBuf>,
    manual_password: Option<String>,
    transfer_activity: TransferActivity,
) -> Result<ServerInfo, String> {
    let root = canonical_root(configured_root)?;
    let username = USERNAME.to_string();
    let password = selected_password(manual_password)?;
    let listener = bind_listener(DEFAULT_PORT)?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("无法读取 SFTP 监听端口：{err}"))?
        .port();
    let key = load_or_create_host_key()?;

    let password = Arc::new(RwLock::new(password));
    let credentials = Arc::new(Credentials {
        username: username.clone(),
        password: password.clone(),
    });
    let server_root = Arc::new(RwLock::new(root.clone()));
    let info_root = server_root.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("macscp-lan-sftp-rt")
        .enable_all()
        .build()
        .map_err(|err| format!("创建局域网 SFTP runtime 失败：{err}"))?;

    std::thread::Builder::new()
        .name("macscp-lan-sftp".to_string())
        .spawn(move || {
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(err) => {
                        log::error!("接管局域网 SFTP 监听端口失败：{err}");
                        return;
                    }
                };
                let config = Arc::new(russh::server::Config {
                    keys: vec![key],
                    methods: authentication_methods(),
                    auth_rejection_time: Duration::from_millis(300),
                    auth_rejection_time_initial: Some(Duration::ZERO),
                    ..Default::default()
                });
                let mut server = LanServer {
                    credentials,
                    root: server_root,
                    transfer_activity,
                };
                if let Err(err) = server.run_on_socket(config, &listener).await {
                    log::error!("局域网 SFTP 服务意外停止：{err}");
                }
            });
        })
        .map_err(|err| format!("无法创建局域网 SFTP 线程：{err}"))?;

    Ok(ServerInfo {
        host: local_ipv4().to_string(),
        port,
        username,
        password,
        root: info_root,
    })
}

fn canonical_root(configured_root: Option<PathBuf>) -> Result<PathBuf, String> {
    let root = configured_root
        .or_else(dirs::home_dir)
        .ok_or_else(|| "无法确定用户主目录".to_string())?;
    let root = root
        .canonicalize()
        .map_err(|err| format!("无法访问共享目录 {}：{err}", root.display()))?;
    if !root.is_dir() {
        return Err(format!("共享目录不是文件夹：{}", root.display()));
    }
    Ok(root)
}

fn bind_listener(port: u16) -> Result<TcpListener, String> {
    bind_listener_on(Ipv4Addr::UNSPECIFIED, port)
}

fn bind_listener_on(host: Ipv4Addr, port: u16) -> Result<TcpListener, String> {
    let listener = match TcpListener::bind((host, port)) {
        Ok(listener) => listener,
        Err(err) if err.kind() == ErrorKind::AddrInUse && port != 0 => TcpListener::bind((host, 0))
            .map_err(|fallback| format!("端口 {port} 已占用，分配随机端口也失败：{fallback}"))?,
        Err(err) => return Err(format!("无法监听端口 {port}：{err}")),
    };
    listener
        .set_nonblocking(true)
        .map_err(|err| format!("无法设置 SFTP 监听端口：{err}"))?;
    Ok(listener)
}

fn local_ipv4() -> IpAddr {
    UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .and_then(|socket| {
            // UDP connect 不会发送数据，只让系统选择当前默认路由对应的本机地址。
            socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?;
            socket.local_addr()
        })
        .map(|address| address.ip())
        .ok()
        .filter(|address| !address.is_loopback())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
}

fn selected_password(manual_password: Option<String>) -> Result<String, String> {
    match manual_password {
        Some(password) if password.is_empty() => Err("手动密码不能为空".to_string()),
        Some(password) => Ok(password),
        None => Ok(DEFAULT_PASSWORD.to_string()),
    }
}

fn load_or_create_host_key() -> Result<PrivateKey, String> {
    let directory = dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("macscp");
    let path = directory.join("lan_sftp_host_key");
    if let Ok(contents) = std::fs::read_to_string(&path) {
        return PrivateKey::from_openssh(&contents)
            .map_err(|err| format!("无法读取局域网 SFTP 主机密钥：{err}"));
    }

    std::fs::create_dir_all(&directory).map_err(|err| format!("无法创建配置目录：{err}"))?;
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .map_err(|err| format!("无法生成局域网 SFTP 主机密钥：{err}"))?;
    let encoded = key
        .to_openssh(LineEnding::LF)
        .map_err(|err| format!("无法编码局域网 SFTP 主机密钥：{err}"))?;
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .or_else(|err| {
            if err.kind() == ErrorKind::AlreadyExists {
                std::fs::OpenOptions::new().read(true).open(&path)
            } else {
                Err(err)
            }
        })
        .map_err(|err| format!("无法保存局域网 SFTP 主机密钥：{err}"))?;

    if file.metadata().is_ok_and(|metadata| metadata.len() == 0) {
        file.write_all(encoded.as_bytes())
            .map_err(|err| format!("无法写入局域网 SFTP 主机密钥：{err}"))?;
        Ok(key)
    } else {
        let contents = std::fs::read_to_string(&path)
            .map_err(|err| format!("无法读取局域网 SFTP 主机密钥：{err}"))?;
        PrivateKey::from_openssh(&contents)
            .map_err(|err| format!("无法解析局域网 SFTP 主机密钥：{err}"))
    }
}

struct Credentials {
    username: String,
    password: Arc<RwLock<String>>,
}

impl Credentials {
    fn accepts(&self, user: &str, password: &[u8]) -> bool {
        let expected = self
            .password
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        user == self.username && password == expected.as_bytes()
    }
}

#[derive(Clone)]
struct LanServer {
    credentials: Arc<Credentials>,
    root: Arc<RwLock<PathBuf>>,
    transfer_activity: TransferActivity,
}

impl russh::server::Server for LanServer {
    type Handler = SshSession;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Self::Handler {
        SshSession {
            credentials: self.credentials.clone(),
            root: self.root.clone(),
            transfer_activity: self.transfer_activity.clone(),
            channels: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

struct SshSession {
    credentials: Arc<Credentials>,
    root: Arc<RwLock<PathBuf>>,
    transfer_activity: TransferActivity,
    channels: Arc<Mutex<HashMap<ChannelId, Channel<Msg>>>>,
}

impl russh::server::Handler for SshSession {
    type Error = russh::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if self.credentials.accepts(user, password.as_bytes()) {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_keyboard_interactive(
        &mut self,
        user: &str,
        _submethods: &str,
        response: Option<Response<'_>>,
    ) -> Result<Auth, Self::Error> {
        let Some(mut response) = response else {
            return Ok(Auth::Partial {
                name: "Blip".into(),
                instructions: "".into(),
                prompts: vec![("Password: ".into(), false)].into(),
            });
        };
        let accepted = response
            .next()
            .is_some_and(|password| self.credentials.accepts(user, &password));
        if accepted && response.next().is_none() {
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
        russh_sftp::server::run(
            channel.into_stream(),
            LocalSftp::new(self.root.clone(), self.transfer_activity.clone()),
        )
        .await;
        Ok(())
    }
}

struct DirectoryCursor {
    files: Vec<File>,
    offset: usize,
}

struct LocalSftp {
    root: Arc<RwLock<PathBuf>>,
    transfer_activity: TransferActivity,
    files: HashMap<String, OpenFile>,
    directories: HashMap<String, DirectoryCursor>,
    next_handle: u64,
}

struct OpenFile {
    file: Arc<Mutex<tokio::fs::File>>,
    _activity: TransferActivityGuard,
}

impl LocalSftp {
    fn new(root: Arc<RwLock<PathBuf>>, transfer_activity: TransferActivity) -> Self {
        Self {
            root,
            transfer_activity,
            files: HashMap::new(),
            directories: HashMap::new(),
            next_handle: 1,
        }
    }

    fn next_handle(&mut self, prefix: &str) -> String {
        let handle = format!("{prefix}-{}", self.next_handle);
        self.next_handle += 1;
        handle
    }

    fn root(&self) -> PathBuf {
        self.root
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn relative_path(path: &str) -> Result<PathBuf, StatusCode> {
        let mut relative = PathBuf::new();
        for component in Path::new(path).components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => relative.push(name),
                Component::ParentDir => {
                    if !relative.pop() {
                        return Err(StatusCode::PermissionDenied);
                    }
                }
                Component::Prefix(_) => return Err(StatusCode::PermissionDenied),
            }
        }
        Ok(relative)
    }

    fn virtual_path(path: &str) -> Result<String, StatusCode> {
        let relative = Self::relative_path(path)?;
        if relative.as_os_str().is_empty() {
            Ok("/".to_string())
        } else {
            Ok(format!("/{}", relative.to_string_lossy()))
        }
    }

    fn resolve_existing(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let root = self.root();
        let candidate = root.join(Self::relative_path(path)?);
        let canonical = candidate.canonicalize().map_err(io_status)?;
        if canonical.starts_with(&root) {
            Ok(canonical)
        } else {
            Err(StatusCode::PermissionDenied)
        }
    }

    /// 返回条目自身而不是跟随最后一个符号链接；父目录仍会 canonicalize，阻止越界。
    fn resolve_entry(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let root = self.root();
        let relative = Self::relative_path(path)?;
        let Some(name) = relative.file_name() else {
            return Ok(root);
        };
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let canonical_parent = root.join(parent).canonicalize().map_err(io_status)?;
        if !canonical_parent.starts_with(&root) {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(canonical_parent.join(name))
    }

    /// 校验相对链接按「链接所在目录 + target」解析后仍在共享根目录内。
    fn resolve_link_target(&self, link: &Path, target: &str) -> Result<PathBuf, StatusCode> {
        let root = self.root();
        let parent = link.parent().ok_or(StatusCode::PermissionDenied)?;
        let mut relative = parent
            .strip_prefix(&root)
            .map_err(|_| StatusCode::PermissionDenied)?
            .to_path_buf();
        for component in Path::new(target).components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => relative.push(name),
                Component::ParentDir => {
                    if !relative.pop() {
                        return Err(StatusCode::PermissionDenied);
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(StatusCode::PermissionDenied);
                }
            }
        }
        let resolved = root.join(relative);
        if let Ok(canonical) = resolved.canonicalize()
            && !canonical.starts_with(&root)
        {
            return Err(StatusCode::PermissionDenied);
        }
        Ok(resolved)
    }

    fn resolve_for_open(&self, path: &str) -> Result<PathBuf, StatusCode> {
        let entry = self.resolve_entry(path)?;
        // `Path::exists` 会把悬空符号链接当作不存在；CREATE 随后会跟随它写出共享目录。
        if entry.symlink_metadata().is_ok() {
            self.resolve_existing(path)
        } else {
            Ok(entry)
        }
    }

    async fn set_path_attributes(path: &Path, attrs: FileAttributes) -> Result<(), StatusCode> {
        if let Some(size) = attrs.size {
            tokio::fs::OpenOptions::new()
                .write(true)
                .open(path)
                .await
                .map_err(io_status)?
                .set_len(size)
                .await
                .map_err(io_status)?;
        }
        if let Some(mode) = attrs.permissions {
            set_path_permissions(path, mode).await?;
        }
        Ok(())
    }
}

#[cfg(unix)]
async fn create_local_symlink(
    target: &str,
    link: &Path,
    _resolved_target: &Path,
) -> Result<(), StatusCode> {
    tokio::fs::symlink(Path::new(target), link)
        .await
        .map_err(io_status)
}

#[cfg(windows)]
async fn create_local_symlink(
    target: &str,
    link: &Path,
    resolved_target: &Path,
) -> Result<(), StatusCode> {
    let metadata = tokio::fs::metadata(resolved_target)
        .await
        .map_err(io_status)?;
    if metadata.is_dir() {
        tokio::fs::symlink_dir(Path::new(target), link)
            .await
            .map_err(io_status)
    } else {
        tokio::fs::symlink_file(Path::new(target), link)
            .await
            .map_err(io_status)
    }
}

#[cfg(not(any(unix, windows)))]
async fn create_local_symlink(
    _target: &str,
    _link: &Path,
    _resolved_target: &Path,
) -> Result<(), StatusCode> {
    Err(StatusCode::OpUnsupported)
}

#[cfg(unix)]
async fn set_path_permissions(path: &Path, mode: u32) -> Result<(), StatusCode> {
    tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
        .await
        .map_err(io_status)
}

#[cfg(not(unix))]
async fn set_path_permissions(_path: &Path, _mode: u32) -> Result<(), StatusCode> {
    // Windows 没有 POSIX 权限位；文件访问仍由共享根目录与 ACL 约束。
    Ok(())
}

fn io_status(error: std::io::Error) -> StatusCode {
    match error.kind() {
        ErrorKind::NotFound => StatusCode::NoSuchFile,
        ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

fn ok(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "zh-CN".to_string(),
    }
}

impl russh_sftp::server::Handler for LocalSftp {
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

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        Ok(Name {
            id,
            files: vec![File::dummy(&Self::virtual_path(&path)?)],
        })
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        let path = self.resolve_existing(&path)?;
        let mut entries = tokio::fs::read_dir(path).await.map_err(io_status)?;
        let mut files = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(io_status)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let metadata = tokio::fs::symlink_metadata(entry.path())
                .await
                .map_err(io_status)?;
            files.push(File::new(&name, FileAttributes::from(&metadata)));
        }
        let handle = self.next_handle("dir");
        self.directories
            .insert(handle.clone(), DirectoryCursor { files, offset: 0 });
        Ok(Handle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        let cursor = self
            .directories
            .get_mut(&handle)
            .ok_or(StatusCode::Failure)?;
        if cursor.offset >= cursor.files.len() {
            return Err(StatusCode::Eof);
        }
        let end = (cursor.offset + READ_DIR_BATCH).min(cursor.files.len());
        let files = cursor.files[cursor.offset..end].to_vec();
        cursor.offset = end;
        Ok(Name { id, files })
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = self.resolve_for_open(&filename)?;
        let mut options = tokio::fs::OpenOptions::new();
        options
            .read(pflags.contains(OpenFlags::READ))
            .write(pflags.contains(OpenFlags::WRITE))
            .append(pflags.contains(OpenFlags::APPEND))
            .truncate(pflags.contains(OpenFlags::TRUNCATE));
        if pflags.contains(OpenFlags::CREATE) {
            if pflags.contains(OpenFlags::EXCLUDE) {
                options.create_new(true);
            } else {
                options.create(true);
            }
        }
        let file = options.open(path).await.map_err(io_status)?;
        let handle = self.next_handle("file");
        self.files.insert(
            handle.clone(),
            OpenFile {
                file: Arc::new(Mutex::new(file)),
                _activity: self.transfer_activity.begin(),
            },
        );
        Ok(Handle { id, handle })
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, Self::Error> {
        if self.files.remove(&handle).is_none() && self.directories.remove(&handle).is_none() {
            return Err(StatusCode::Failure);
        }
        Ok(ok(id))
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        let file = self.files.get(&handle).ok_or(StatusCode::Failure)?;
        let mut file = file.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(io_status)?;
        let mut data = vec![0; len as usize];
        let read = file.read(&mut data).await.map_err(io_status)?;
        if read == 0 {
            return Err(StatusCode::Eof);
        }
        data.truncate(read);
        Ok(Data { id, data })
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        let file = self.files.get(&handle).ok_or(StatusCode::Failure)?;
        let mut file = file.file.lock().await;
        file.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(io_status)?;
        file.write_all(&data).await.map_err(io_status)?;
        Ok(ok(id))
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        let file = self.files.get(&handle).ok_or(StatusCode::Failure)?;
        let metadata = file.file.lock().await.metadata().await.map_err(io_status)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let path = self.resolve_existing(&path)?;
        let metadata = tokio::fs::metadata(path).await.map_err(io_status)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let path = self.resolve_entry(&path)?;
        let metadata = tokio::fs::symlink_metadata(path).await.map_err(io_status)?;
        Ok(Attrs {
            id,
            attrs: FileAttributes::from(&metadata),
        })
    }

    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = self.resolve_existing(&path)?;
        Self::set_path_attributes(&path, attrs).await?;
        Ok(ok(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let path = self.resolve_entry(&filename)?;
        tokio::fs::remove_file(path).await.map_err(io_status)?;
        Ok(ok(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let path = self.resolve_entry(&path)?;
        tokio::fs::create_dir(&path).await.map_err(io_status)?;
        if let Some(mode) = attrs.permissions {
            set_path_permissions(&path, mode).await?;
        }
        Ok(ok(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let path = self.resolve_entry(&path)?;
        tokio::fs::remove_dir(path).await.map_err(io_status)?;
        Ok(ok(id))
    }

    async fn symlink(
        &mut self,
        id: u32,
        linkpath: String,
        targetpath: String,
    ) -> Result<Status, Self::Error> {
        let link = self.resolve_entry(&linkpath)?;
        let resolved_target = self.resolve_link_target(&link, &targetpath)?;
        create_local_symlink(&targetpath, &link, &resolved_target).await?;
        Ok(ok(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, Self::Error> {
        let oldpath = self.resolve_entry(&oldpath)?;
        let newpath = self.resolve_entry(&newpath)?;
        tokio::fs::rename(oldpath, newpath)
            .await
            .map_err(io_status)?;
        Ok(ok(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sftp::session::{self, HostKeyPolicy};
    use crate::sftp::types::{AuthMethod, SessionConfig};
    use russh::client;
    use russh::keys::PublicKeyOrCertificate;
    use russh_sftp::server::Handler as _;

    struct AcceptServerKey;

    impl client::Handler for AcceptServerKey {
        type Error = russh::Error;

        async fn check_server_key(
            &mut self,
            _server_public_key: &PublicKeyOrCertificate,
        ) -> Result<bool, Self::Error> {
            Ok(true)
        }
    }

    fn temp_root(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("macscp-lan-server-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("创建临时目录");
        root.canonicalize().expect("规范化临时目录")
    }

    fn test_activity() -> TransferActivity {
        transfer_activity().0
    }

    #[test]
    fn occupied_port_falls_back_to_a_random_port() {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("占用测试端口");
        let port = occupied.local_addr().expect("读取端口").port();
        let fallback = bind_listener_on(Ipv4Addr::LOCALHOST, port).expect("随机端口回退");
        assert_ne!(fallback.local_addr().expect("读取回退端口").port(), port);
    }

    #[test]
    fn default_password_is_88888888() {
        assert_eq!(selected_password(None).as_deref(), Ok(DEFAULT_PASSWORD));
    }

    #[test]
    fn manual_password_is_used_verbatim() {
        let info = ServerInfo {
            host: "127.0.0.1".into(),
            port: DEFAULT_PORT,
            username: USERNAME.into(),
            password: Arc::new(RwLock::new("123456".into())),
            root: Arc::new(RwLock::new(temp_root("manual-password"))),
        };
        info.set_password(Some("my custom password".into()))
            .expect("设置手动密码");
        assert_eq!(info.password(), "my custom password");
        info.set_password(None).expect("恢复默认密码");
        assert_eq!(info.password(), DEFAULT_PASSWORD);
        assert!(selected_password(Some(String::new())).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn paths_cannot_escape_the_shared_root() {
        let root = temp_root("sandbox");
        let outside = root.with_file_name("macscp-lan-server-outside");
        let _ = std::fs::remove_file(root.join("escape"));
        std::os::unix::fs::symlink(&outside, root.join("escape")).expect("创建悬空符号链接");
        let sftp = LocalSftp::new(Arc::new(RwLock::new(root)), test_activity());
        assert_eq!(
            sftp.resolve_existing("/../../etc"),
            Err(StatusCode::PermissionDenied)
        );
        assert!(sftp.resolve_for_open("/escape").is_err());
    }

    #[test]
    fn changing_the_shared_root_takes_effect_immediately() {
        let first = temp_root("root-first");
        let second = temp_root("root-second");
        std::fs::write(first.join("first.txt"), b"first").expect("写第一个目录");
        std::fs::write(second.join("second.txt"), b"second").expect("写第二个目录");
        let shared_root = Arc::new(RwLock::new(first));
        let sftp = LocalSftp::new(shared_root.clone(), test_activity());
        assert!(sftp.resolve_existing("/first.txt").is_ok());

        *shared_root.write().expect("切换共享目录") = second;
        assert!(sftp.resolve_existing("/first.txt").is_err());
        assert!(sftp.resolve_existing("/second.txt").is_ok());
    }

    #[tokio::test]
    async fn reads_and_writes_real_files() {
        let root = temp_root("io");
        let mut sftp = LocalSftp::new(Arc::new(RwLock::new(root.clone())), test_activity());
        let handle = sftp
            .open(
                1,
                "/hello.txt".into(),
                OpenFlags::WRITE | OpenFlags::CREATE | OpenFlags::TRUNCATE,
                FileAttributes::default(),
            )
            .await
            .expect("打开写入文件")
            .handle;
        sftp.write(2, handle.clone(), 0, b"hello sftp".to_vec())
            .await
            .expect("写入文件");
        sftp.close(3, handle).await.expect("关闭写入文件");

        let handle = sftp
            .open(
                4,
                "/hello.txt".into(),
                OpenFlags::READ,
                FileAttributes::default(),
            )
            .await
            .expect("打开读取文件")
            .handle;
        let data = sftp.read(5, handle, 0, 64).await.expect("读取文件");
        assert_eq!(data.data, b"hello sftp");
        assert_eq!(
            std::fs::read(root.join("hello.txt")).expect("本地读取"),
            b"hello sftp"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_keeps_relative_target_inside_shared_root() {
        let root = temp_root("symlink");
        std::fs::create_dir(root.join("bin")).expect("创建链接目录");
        std::fs::write(root.join("tool.js"), b"tool").expect("创建链接目标");
        let mut sftp = LocalSftp::new(Arc::new(RwLock::new(root.clone())), test_activity());

        sftp.symlink(1, "/bin/tool".into(), "../tool.js".into())
            .await
            .expect("创建安全相对链接");
        assert_eq!(
            std::fs::read_link(root.join("bin/tool")).expect("读取链接"),
            PathBuf::from("../tool.js")
        );
        assert!(matches!(
            sftp.symlink(2, "/bin/escape".into(), "../../outside".into())
                .await,
            Err(StatusCode::PermissionDenied)
        ));
    }

    #[tokio::test]
    async fn open_files_report_one_activity_until_the_session_ends() {
        let root = temp_root("activity");
        let (activity, events) = transfer_activity();
        let mut sftp = LocalSftp::new(Arc::new(RwLock::new(root)), activity);

        let first = sftp
            .open(
                1,
                "/first.bin".into(),
                OpenFlags::WRITE | OpenFlags::CREATE,
                FileAttributes::default(),
            )
            .await
            .expect("打开第一个文件")
            .handle;
        assert_eq!(events.recv(), Ok(true));

        sftp.open(
            2,
            "/second.bin".into(),
            OpenFlags::WRITE | OpenFlags::CREATE,
            FileAttributes::default(),
        )
        .await
        .expect("打开第二个文件");
        assert_eq!(events.try_recv(), Err(flume::TryRecvError::Empty));

        sftp.close(3, first).await.expect("关闭第一个文件");
        assert_eq!(events.try_recv(), Err(flume::TryRecvError::Empty));

        // 客户端异常断开时 LocalSftp 会整体释放，最后一个活动句柄也必须结束。
        drop(sftp);
        assert_eq!(events.recv(), Ok(false));
    }

    #[tokio::test]
    async fn accepts_password_and_lists_the_shared_root_over_ssh() {
        let root = temp_root("ssh");
        std::fs::write(root.join("visible.txt"), b"visible").expect("写测试文件");
        let key =
            PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).expect("生成测试主机密钥");
        let host_key = key.public_key().clone();
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("监听测试端口");
        let address = listener.local_addr().expect("读取测试端口");
        let credentials = Arc::new(Credentials {
            username: "lan-user".into(),
            password: Arc::new(RwLock::new("123456".into())),
        });
        let mut server = LanServer {
            credentials,
            root: Arc::new(RwLock::new(root)),
            transfer_activity: test_activity(),
        };
        let config = Arc::new(russh::server::Config {
            keys: vec![key],
            methods: authentication_methods(),
            auth_rejection_time: Duration::from_millis(10),
            auth_rejection_time_initial: Some(Duration::ZERO),
            ..Default::default()
        });
        let server_task = tokio::spawn(async move {
            server
                .run_on_socket(config, &listener)
                .await
                .expect("运行测试服务")
        });

        let connected = session::connect(
            &SessionConfig {
                host: address.ip().to_string(),
                port: address.port(),
                username: "lan-user".into(),
                auth: AuthMethod::Password("123456".into()),
            },
            HostKeyPolicy::Exact(host_key),
        )
        .await
        .expect("连接内置 SFTP");
        assert_eq!(connected.home, "/");
        let entries = session::list_dir(&connected.sftp, "/")
            .await
            .expect("列共享根目录");
        assert!(entries.iter().any(|entry| entry.name == "visible.txt"));

        let mut interactive = client::connect(
            Arc::new(client::Config::default()),
            address,
            AcceptServerKey,
        )
        .await
        .expect("建立 keyboard-interactive 测试连接");
        let challenge = interactive
            .authenticate_keyboard_interactive_start("lan-user", None::<String>)
            .await
            .expect("请求密码挑战");
        assert!(matches!(
            challenge,
            client::KeyboardInteractiveAuthResponse::InfoRequest { .. }
        ));
        let authenticated = interactive
            .authenticate_keyboard_interactive_respond(vec!["123456".into()])
            .await
            .expect("提交 keyboard-interactive 密码");
        assert!(matches!(
            authenticated,
            client::KeyboardInteractiveAuthResponse::Success
        ));

        server_task.abort();
    }
}
