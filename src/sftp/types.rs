//! 领域模型：GPUI 主线程与 Tokio SFTP worker 之间共享的类型。
//!
//! 这里的类型都必须 `Send + 'static` —— 它们通过 `flume` channel 跨运行时传递，
//! 一边是 GPUI 的 executor，另一边是独立线程里的 Tokio runtime。

use std::fmt;
use std::path::PathBuf;
use std::time::SystemTime;

pub const DEFAULT_SSH_PORT: u16 = 22;

// ---------------------------------------------------------------------------
// 连接配置
// ---------------------------------------------------------------------------

/// 一次连接的完整配置。
#[derive(Clone)]
pub struct SessionConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
}

impl SessionConfig {
    /// `user@host:port`，用于界面展示。
    pub fn endpoint(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }

    /// 系统凭据库中的 account 键，保证不同端口的不同主机不会互相覆盖。
    pub fn keyring_account(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }
}

impl fmt::Debug for SessionConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("auth", &self.auth)
            .finish()
    }
}

#[derive(Clone)]
pub enum AuthMethod {
    Password(String),
    /// `path` 为空表示“自动查找”，由 `session::discover_private_key` 依次尝试
    /// `~/.ssh/id_ed25519`、`id_ecdsa`、`id_rsa`。
    PrivateKey {
        path: PathBuf,
        passphrase: Option<String>,
    },
}

// 手写 Debug：密码/口令永远不进日志。
impl fmt::Debug for AuthMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthMethod::Password(_) => f.write_str("Password(<redacted>)"),
            AuthMethod::PrivateKey { path, passphrase } => f
                .debug_struct("PrivateKey")
                .field("path", path)
                .field("passphrase", &passphrase.as_ref().map(|_| "<redacted>"))
                .finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// 文件条目
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
}

#[derive(Clone, Debug)]
pub struct FileEntry {
    /// 文件/目录名（不含路径）。
    pub name: String,
    /// 完整路径：本地为绝对文件系统路径，远端为 POSIX 路径。
    pub path: String,
    pub kind: EntryKind,
    /// 本地使用平台隐藏属性，远端使用点文件命名规则。
    pub hidden: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// POSIX mode 位，含文件类型位（例如目录是 `0o040_755`）。
    pub mode: Option<u32>,
}

impl FileEntry {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, EntryKind::Directory)
    }
}

// ---------------------------------------------------------------------------
// 传输
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferDirection {
    Upload,
    Download,
}

pub type TransferId = u64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferOutcome {
    Completed,
    /// 主体传输已完成，但有条目因为链接、权限或并发变化未被处理。
    Partial(String),
    Failed(String),
    Cancelled,
    /// 用户在冲突弹窗里选了「跳过」，一个字节都没传。
    Skipped,
}

// ---------------------------------------------------------------------------
// 上传冲突
// ---------------------------------------------------------------------------

/// 上传前的远端现状：远端已有同名文件，或存在没传完的 `.filepart`。
///
/// 对应 WinSCP 发现同名文件时弹的那个确认框。worker 在**写第一个字节之前**
/// 把远端探明，把决定权交给 UI —— 这样「覆盖还是跳过」永远不会是隐式的。
#[derive(Clone, Debug)]
pub struct UploadConflict {
    pub id: TransferId,
    /// 本地源文件。
    pub local_path: PathBuf,
    /// 远端目标文件（不带 `.filepart` 后缀）。
    pub remote_path: String,
    /// 本地文件总大小。
    pub local_size: u64,
    /// 远端同名文件的当前大小；`None` 表示不存在。
    pub existing_size: Option<u64>,
    /// 远端 `<remote>.filepart` 的大小；`None` 表示没有半成品。
    pub filepart_size: Option<u64>,
    /// 半成品是否带有 macscp 写入的内容身份凭据。
    pub checkpoint_present: bool,
}

impl UploadConflict {
    /// 远端已有同名文件（覆盖会替换掉它）。
    pub fn has_existing(&self) -> bool {
        self.existing_size.is_some()
    }

    /// 能不能续传：临时文件存在、非空、且**比本地短**，同时必须带身份凭据。
    ///
    /// 比本地长说明本地文件换过了，前半段内容对不上，只能从头传。
    pub fn can_resume(&self) -> bool {
        self.checkpoint_present
            && matches!(self.filepart_size, Some(size) if size > 0 && size < self.local_size)
    }

    /// 临时文件已经不小于本地文件：残留的是一份过期数据。
    pub fn filepart_is_stale(&self) -> bool {
        matches!(self.filepart_size, Some(size) if size >= self.local_size)
    }

    /// 文件名（用于标题与提示）。
    pub fn file_name(&self) -> String {
        self.local_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| self.local_path.to_string_lossy().to_string())
    }
}

/// 用户在冲突弹窗里的选择。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UploadDecision {
    /// 从 `.filepart` 已有的字节后面接着传。
    Resume,
    /// 丢弃 `.filepart`，从头传；传完再原子替换原文件。
    Overwrite,
    /// 什么都不做，跳过这个文件。
    Skip,
}

/// 首次见到 SSH 主机密钥或已记录的密钥发生变化时，用户的处理决定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostKeyDecision {
    Reject,
    TrustOnce,
    TrustAndSave,
}

/// 展示给用户确认的 SSH 主机身份。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostKeyPrompt {
    pub endpoint: String,
    pub algorithm: String,
    pub fingerprint: String,
    /// `Some(line)` 表示 known_hosts 的这一行记录了不同密钥。
    pub changed_line: Option<usize>,
}

impl UploadDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            UploadDecision::Resume => "断点续传",
            UploadDecision::Overwrite => "安全覆盖",
            UploadDecision::Skip => "跳过",
        }
    }

    /// 是否要真的写数据（`Skip` 不写）。
    pub fn is_transfer(self) -> bool {
        !matches!(self, UploadDecision::Skip)
    }
}

/// 整目录上传时，远端已有同名文件的处理方式。
///
/// 文件夹里可能有几百个文件，逐个弹「覆盖还是跳过」的确认框没法用 ——
/// 所以这个决定在**上传开始前问一次**，之后整批照办。
/// 没得选的情况（远端干干净净）不需要问，直接按 [`ExistingPolicy::Overwrite`] 走。
///
/// 和 [`UploadDecision`] 的区别：那个是「单个文件怎么办」，这个是「一整批怎么办」。
/// 整批不会自动信任残留 `.filepart`：没有逐文件确认时，安全覆盖优先于按长度续传。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExistingPolicy {
    /// 覆盖同名文件（依旧先写 `.filepart` 再原子替换，目标文件不会被截断）。
    Overwrite,
    /// 远端已有同名文件就跳过它，一个字节都不动。
    Skip,
}

impl ExistingPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ExistingPolicy::Overwrite => "覆盖同名文件",
            ExistingPolicy::Skip => "跳过同名文件",
        }
    }
}

// ---------------------------------------------------------------------------
// 指令：UI → Worker
// ---------------------------------------------------------------------------

/// UI 发给 worker 的指令。
///
/// 业务指令（连目录、传文件、传整个文件夹、建/删/改名）之外还有几条控制指令：
/// `Disconnect`（断开但线程继续跑）、`Cancel`（按 id 取消传输）、
/// `ResolveUpload`（回答上传冲突）、`Shutdown`（让 worker 线程退出）。
#[derive(Debug)]
pub enum SftpCommand {
    /// 建立 SSH 会话并打开 SFTP 子系统。
    Connect(SessionConfig),
    /// 断开当前会话，worker 继续等待下一条指令。
    Disconnect,
    /// 读取远端目录。`request_id` 由 UI 自增，用于丢弃迟到的响应。
    /// `fallback` 只用于恢复上次目录：目标明确不存在时改读登录目录。
    ListDir {
        request_id: u64,
        path: String,
        fallback: Option<String>,
    },
    /// 远端 → 本地。
    Download {
        id: TransferId,
        remote: String,
        local: PathBuf,
        /// 预期总字节数，仅用于进度百分比；真实长度以实际读取为准。
        total: u64,
        /// 允许断点续传：本地已有不完整文件时从它的末尾接着写。
        resume: bool,
    },
    /// 本地 → 远端。
    ///
    /// 永远先写 `<remote>.filepart`，写完再原子替换 `<remote>`：
    /// 中途取消/断线不会留下一个被截断的目标文件，也不会因为
    /// 「目标正在被运行」而 ETXTBSY。
    Upload {
        id: TransferId,
        local: PathBuf,
        remote: String,
        total: u64,
        /// 允许断点续传：远端 `.filepart` 比本地短时从它的大小接着传。
        resume: bool,
    },
    /// 外部编辑器保存后的本地缓存 → 原远端路径。
    ///
    /// 这是用户正在编辑的同一份文件，直接安全覆盖，不再弹上传冲突确认。
    UploadOverwrite {
        id: TransferId,
        local: PathBuf,
        remote: String,
        total: u64,
    },
    /// 回答一次 [`SftpEvent::UploadConflict`]。
    ResolveUpload {
        id: TransferId,
        decision: UploadDecision,
        /// 本次会话内对后续所有冲突都用这个选择（弹窗里的「对后续冲突执行相同操作」）。
        apply_to_all: bool,
    },
    /// 回答首次连接 / 主机密钥变化确认框。
    ResolveHostKey(HostKeyDecision),
    /// 把本机公钥追加到当前账号的 `~/.ssh/authorized_keys`。
    InstallPublicKey {
        public_key: PathBuf,
        /// 部署成功后，UI 用对应私钥作为下次登录方式。
        private_key: PathBuf,
    },
    /// 整目录下载：在 `local` 建目录，然后把 `remote` 里的内容递归取回来。
    ///
    /// 和 [`SftpCommand::UploadDir`] 完全对称，只是方向反过来。
    DownloadDir {
        id: TransferId,
        /// 远端目录根。
        remote: String,
        /// 本地目标目录（含文件夹名）。
        local: PathBuf,
        /// 本地已有同名文件时怎么办。整批问一次，不在中途弹窗。
        on_existing: ExistingPolicy,
    },
    /// 整目录上传：在 `remote` 建目录，然后把 `local` 里的内容递归传上去。
    ///
    /// 一条指令对应队列里的**一行**：worker 自己扫本地目录、建远端目录、
    /// 逐个文件走「探针 → `.filepart` → 原子替换」，进度按整批字节汇总上报。
    /// 这样一次拖进一个 500 个文件的目录，队列里也只多一行，而不是 500 行。
    UploadDir {
        id: TransferId,
        /// 本地目录根。
        local: PathBuf,
        /// 远端目标目录（含文件夹名），例如 `/srv/app/dist`。
        remote: String,
        /// 远端已有同名文件时怎么办。整批问一次，不在中途弹窗。
        on_existing: ExistingPolicy,
    },
    /// 取消一个进行中的传输。
    Cancel(TransferId),
    /// 读取一个小型远端文件，强制作为文本供应用内编辑器使用。
    ReadText { path: String },
    /// 用应用内编辑器的内容安全替换远端文件。
    WriteText { path: String, content: String },
    /// 新建目录。
    Mkdir { path: String },
    /// 批量删除文件或目录。元组依次为远端路径、是否为目录；worker 会逐项处理并汇总结果。
    Remove { entries: Vec<(String, bool)> },
    /// 重命名 / 移动。worker 侧已实现，UI 入口留待右键菜单接入。
    #[allow(dead_code)]
    Rename { from: String, to: String },
    /// 让 worker 线程退出（`SftpWorker::drop` 时自动发送）。
    Shutdown,
}

// ---------------------------------------------------------------------------
// 事件：Worker → UI
// ---------------------------------------------------------------------------

/// 错误发生在哪一步。UI 据此决定显示位置：
/// `Connect` 回到工具栏内联错误，`List` 显示在面板顶部，其余弹 Toast。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorContext {
    Connect,
    List,
    Mkdir,
    Remove,
    Rename,
    ReadText,
    WriteText,
    InstallPublicKey,
    Runtime,
}

impl ErrorContext {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorContext::Connect => "connect",
            ErrorContext::List => "list",
            ErrorContext::Mkdir => "mkdir",
            ErrorContext::Remove => "remove",
            ErrorContext::Rename => "rename",
            ErrorContext::ReadText => "read_text",
            ErrorContext::WriteText => "write_text",
            ErrorContext::InstallPublicKey => "install_public_key",
            ErrorContext::Runtime => "runtime",
        }
    }
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Worker 发给 UI 的事件。
#[derive(Debug)]
pub enum SftpEvent {
    /// 连接 + 认证 + SFTP 握手全部成功。`home` 是服务器给出的登录目录。
    Connected {
        home: String,
        upload_sources: Vec<String>,
        upload_concurrency: usize,
    },
    /// 已建立的连接意外关闭；worker 会保留连接意图并持续尝试恢复。
    Reconnecting,
    Disconnected,
    /// 后台附加网卡连接建立或失效后，刷新状态栏里的上传链路信息。
    TransferPoolChanged {
        upload_sources: Vec<String>,
        upload_concurrency: usize,
    },
    /// 目录读取完成，`entries` 已按「文件夹优先、名称字母序」排好。
    DirListed {
        request_id: u64,
        path: String,
        entries: Vec<FileEntry>,
    },
    TransferStarted {
        id: TransferId,
        /// 传输开始前就已经存在的字节数（断点续传的偏移）。
        ///
        /// 为 0 表示从零开始；大于 0 时 UI 应该显示「续传」而不是「等待中」。
        resumed_from: u64,
        /// 本次要传输的总字节数（续传时 = 全文件大小）。
        total: u64,
    },
    TransferProgress {
        id: TransferId,
        /// 全文件的已传输字节数（含续传的那一段）。
        transferred: u64,
        total: u64,
        /// worker 侧测出来的实时速率，字节/秒；`f64::NAN` 表示「暂时算不出来」。
        speed: f64,
    },
    /// 整目录并发上传时，当前实际占用传输槽的文件。
    ///
    /// 最多只有目录上传并发数那么多项；空列表表示正在扫描目录或已处理完当前一批。
    TransferActiveFiles {
        id: TransferId,
        files: Vec<String>,
    },
    TransferCompleted {
        id: TransferId,
        outcome: TransferOutcome,
    },
    /// 上传前发现远端有同名文件或残留的 `.filepart`，等 UI 回答
    /// [`SftpCommand::ResolveUpload`]。在此之前不会写任何字节。
    UploadConflict(UploadConflict),
    /// SSH 主机身份尚未被信任，等 UI 用模态框让用户确认。
    HostKeyPrompt(HostKeyPrompt),
    /// 远端文本文件已读取完成。
    TextLoaded {
        path: String,
        content: String,
    },
    /// 远端文本文件已安全替换完成。
    TextSaved {
        path: String,
    },
    /// 公钥已经存在也算成功；`already_present` 只用于给用户更准确的反馈。
    PublicKeyInstalled {
        private_key: PathBuf,
        already_present: bool,
    },
    /// 操作成功后的轻提示（建目录、删除、重命名）。
    Notice(String),
    /// 任何网络/协议异常最终都落到这里，绝不 panic、绝不静默吞掉。
    ///
    /// 比裸 `Error(String)` 多带一个 `context`：UI 需要区分「连接失败」和
    /// 「读目录失败」，前者回到登录表单，后者显示在文件面板里。
    Error {
        context: ErrorContext,
        message: String,
    },
}

impl SftpEvent {
    pub fn error(context: ErrorContext, message: impl Into<String>) -> Self {
        SftpEvent::Error {
            context,
            message: message.into(),
        }
    }
}
