//! 双栏工作区：左「本地」右「远程」，下方是可折叠的传输队列。
//!
//! 这一层同时是 **SFTP 会话的持有者**：
//!
//! * [`Workspace::new`] 里启动 [`SftpWorker`]，并用 `cx.spawn` 起一个事件泵，
//!   在 GPUI 执行器上 `recv_async().await` 循环消费 `flume::Receiver<SftpEvent>`；
//! * 工具栏只把「用户点了连接」抛给 `ScpApp`，再由 `ScpApp` 转成 [`Workspace::connect`]；
//! * 两个面板只发 [`FilePaneEvent`]，网络指令全部在这里翻译成 [`SftpCommand`]。
//!
//! 布局本身很简单：`v_resizable`（上双栏 / 下队列），收起时换成普通 `v_flex`。

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState, Textarea, TextareaState},
    notification::Notification,
    resizable::{h_resizable, resizable_panel, v_resizable},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::session::store::{self, AppSettings, DirectoryHistory, RemoteOpenBehavior};
use crate::sftp::session::{join_remote, remote_parent};
use crate::sftp::types::{
    ErrorContext, ExistingPolicy, FileEntry, HostKeyDecision, HostKeyPrompt, SessionConfig,
    TransferDirection, TransferOutcome, UploadConflict,
};
use crate::sftp::{SftpCommand, SftpEvent, SftpWorker};
use crate::ui::SessionState;
use crate::ui::file_pane::{FileAction, FilePane, FilePaneEvent, PaneSide};
use crate::ui::transfer_panel::{TransferQueue, TransferQueueEvent};
use crate::ui::upload_conflict::UploadConflictPrompt;

/// 工作区向上抛的事件。
#[derive(Clone, Debug)]
pub enum WorkspaceEvent {
    /// 会话状态变化。**只从事件泵方向发出**：工具栏自己发起的
    /// 「连接中 / 已断开」不会再回传给它（那时它正被更新，重入会 panic）。
    SessionStateChanged(SessionState),
}

impl EventEmitter<WorkspaceEvent> for Workspace {}

pub struct Workspace {
    worker: SftpWorker,
    local: Entity<FilePane>,
    remote: Entity<FilePane>,
    transfers: Entity<TransferQueue>,
    /// 传输队列是否收起：收起后只留底部一条标题栏。
    collapsed: bool,
    state: SessionState,
    settings: AppSettings,
    directory_history: DirectoryHistory,
    /// 当前连接用来查找/更新远端目录历史的 `user@host`。
    remote_endpoint: Option<(String, String)>,
    /// 每次连接/断开都会推进；旧连接打开的编辑缓存绝不能写进新连接。
    connection_generation: u64,
    /// 服务器给出的登录目录。
    home: Option<String>,
    last_error: Option<SharedString>,
    /// 目录列表请求序号，用于丢弃迟到的响应。
    request_seq: u64,
    next_transfer_id: u64,
    /// 传输完成后的自动刷新：待刷新的面板 + 合并用的定时器。
    ///
    /// 一次拖 20 个文件会连着来 20 条 `TransferCompleted`，逐个刷新就是 20 次
    /// 目录往返；攒 [`REFRESH_DEBOUNCE`] 之后只刷一次。
    pending_refresh: PendingRefresh,
    refresh_timer: Option<Task<()>>,
    /// 已发出读取指令、正在等待远端文本的条目。
    pending_text_edit: Option<FileEntry>,
    /// 当前文本编辑器的草稿。保存失败时保留，让用户可直接重试。
    active_text_edit: Option<ActiveTextEdit>,
    pending_external_edits: HashMap<u64, PendingExternalEdit>,
    external_edits: HashMap<PathBuf, ActiveExternalEdit>,
    pending_external_syncs: HashMap<u64, PendingExternalSync>,
    _subscriptions: Vec<Subscription>,
    _pump: Task<()>,
}

/// 攒着待刷新的面板。
#[derive(Clone, Copy, Default)]
struct PendingRefresh {
    local: bool,
    remote: bool,
}

struct ActiveTextEdit {
    target: TextEditTarget,
    saving: bool,
    error: Option<SharedString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum TextEditTarget {
    Remote(String),
    Local(PathBuf),
}

impl TextEditTarget {
    fn display_path(&self) -> String {
        match self {
            Self::Remote(path) => path.clone(),
            Self::Local(path) => path.to_string_lossy().into_owned(),
        }
    }
}

struct PendingExternalEdit {
    remote: String,
    local: PathBuf,
    editor_path: Option<PathBuf>,
    connection_generation: u64,
}

struct ActiveExternalEdit {
    remote: String,
    stamp: FileStamp,
    revision: u64,
    last_attempted_revision: u64,
    uploading: bool,
    pending_since: Option<Instant>,
    connection_generation: u64,
}

struct PendingExternalSync {
    local: PathBuf,
    revision: u64,
    connection_generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl PendingRefresh {
    fn is_empty(self) -> bool {
        !self.local && !self.remote
    }
}

/// 传输完成到刷新列表之间的合并窗口。
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(300);
const EXTERNAL_EDIT_POLL: Duration = Duration::from_millis(500);
const EXTERNAL_EDIT_DEBOUNCE: Duration = Duration::from_millis(700);

impl Workspace {
    pub fn new(settings: AppSettings, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let worker = SftpWorker::spawn();

        let directory_history = store::load_directory_history();
        let home = directory_history
            .local_or(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")))
            .to_string_lossy()
            .to_string();

        // 左侧从一开始就读真实磁盘；右侧等连接成功后再显示服务器数据。
        let local = cx.new(|cx| FilePane::new(PaneSide::Local, home, window, cx));
        let remote = cx.new(|cx| FilePane::new(PaneSide::Remote, "/".to_string(), window, cx));
        // 队列一开始是空的：传输面板只显示真实任务。
        // （早期骨架阶段在这里塞过三条演示行，结果每次启动都「复活」，
        //  还会被误认为正在传输、点取消也没反应 —— 已经删掉。）
        let transfers = cx.new(|_| TransferQueue::new());

        let mut subscriptions = Vec::new();

        // 远端面板：切目录 / 双击文件 / 右键菜单。
        subscriptions.push(cx.subscribe_in(
            &remote,
            window,
            |workspace, _pane, event: &FilePaneEvent, window, cx| match event {
                FilePaneEvent::Navigate(path) => {
                    // 面板 emit 之前已经进了加载态，这里只负责发指令。
                    workspace.request_remote_listing(path.clone(), cx);
                }
                FilePaneEvent::DirectoryLoaded(_) => {}
                FilePaneEvent::Activate(entry) => {
                    workspace.open_entry(PaneSide::Remote, entry.clone(), window, cx)
                }
                FilePaneEvent::Action { entries, action } => {
                    workspace.handle_action(PaneSide::Remote, entries.clone(), *action, window, cx);
                }
                FilePaneEvent::PaneAction(action) => {
                    workspace.handle_pane_action(PaneSide::Remote, *action, window, cx);
                }
                // Finder 拖进来的东西：文件直接上传，文件夹整棵上传。
                FilePaneEvent::DropFiles(paths) => {
                    workspace.upload_paths(paths.clone(), window, cx);
                }
            },
        ));

        // 本地面板：双击文件用设置中的编辑方式打开；上传仍只走右键菜单/拖放。
        subscriptions.push(cx.subscribe_in(
            &local,
            window,
            |workspace, _pane, event: &FilePaneEvent, window, cx| match event {
                FilePaneEvent::Navigate(_) => {}
                FilePaneEvent::DirectoryLoaded(path) => workspace.remember_local_directory(path),
                FilePaneEvent::Activate(entry) => {
                    workspace.open_entry(PaneSide::Local, entry.clone(), window, cx)
                }
                FilePaneEvent::Action { entries, action } => {
                    workspace.handle_action(PaneSide::Local, entries.clone(), *action, window, cx);
                }
                FilePaneEvent::PaneAction(action) => {
                    workspace.handle_pane_action(PaneSide::Local, *action, window, cx);
                }
                // 本地面板不是拖放目标。
                FilePaneEvent::DropFiles(_) => {}
            },
        ));

        subscriptions.push(cx.subscribe(
            &transfers,
            |workspace, _queue, event: &TransferQueueEvent, cx| match event {
                TransferQueueEvent::Cancel(id) => workspace.worker.send(SftpCommand::Cancel(*id)),
                TransferQueueEvent::ToggleCollapsed => {
                    workspace.collapsed = !workspace.collapsed;
                    cx.notify();
                }
            },
        ));

        // 事件泵：GPUI 执行器上等 flume，收到事件后回到实体上下文
        // （这样才能拿到 `&mut Window` 弹 Toast）。
        let events = worker.events().clone();
        let pump = cx.spawn(async move |this, cx| {
            while let Ok(event) = events.recv_async().await {
                let alive = this
                    .update_in(cx, |workspace, window, cx| {
                        workspace.handle_event(event, window, cx)
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        });

        Self {
            worker,
            local,
            remote,
            transfers,
            collapsed: false,
            state: SessionState::Disconnected,
            settings,
            directory_history,
            remote_endpoint: None,
            connection_generation: 0,
            home: None,
            last_error: None,
            request_seq: 0,
            next_transfer_id: 1,
            pending_refresh: PendingRefresh::default(),
            refresh_timer: None,
            pending_text_edit: None,
            active_text_edit: None,
            pending_external_edits: HashMap::new(),
            external_edits: HashMap::new(),
            pending_external_syncs: HashMap::new(),
            _subscriptions: subscriptions,
            _pump: pump,
        }
    }

    pub fn set_settings(&mut self, settings: AppSettings, cx: &mut Context<Self>) {
        self.settings = settings;
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 给根实体用的访问器
    // -----------------------------------------------------------------------

    pub fn local(&self) -> Entity<FilePane> {
        self.local.clone()
    }

    pub fn remote(&self) -> Entity<FilePane> {
        self.remote.clone()
    }

    pub fn transfers(&self) -> Entity<TransferQueue> {
        self.transfers.clone()
    }

    pub fn last_error(&self) -> Option<SharedString> {
        self.last_error.clone()
    }

    pub fn set_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        if self.collapsed != collapsed {
            self.collapsed = collapsed;
            cx.notify();
        }
    }

    // -----------------------------------------------------------------------
    // 连接
    // -----------------------------------------------------------------------

    /// 由工具栏的「连接」触发。
    ///
    /// 这里**不 emit** `SessionStateChanged`：调用链来自工具栏的点击回调，
    /// 那一刻 `Toolbar` 正被更新，回头再 `update` 它会撞上 GPUI 的独占租约。
    /// 工具栏已经自己把按钮切成「连接中」了。
    pub fn connect(&mut self, config: SessionConfig, cx: &mut Context<Self>) {
        if self.state == SessionState::Connecting {
            return;
        }
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.state = SessionState::Connecting;
        self.last_error = None;
        self.remote_endpoint = Some((config.host.clone(), config.username.clone()));
        self.worker.send(SftpCommand::Connect(config));
        cx.notify();
    }

    /// 由工具栏的「断开」触发，理由同上：不 emit。
    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.connection_generation = self.connection_generation.wrapping_add(1);
        self.worker.send(SftpCommand::Disconnect);
        self.state = SessionState::Disconnected;
        self.home = None;
        self.remote_endpoint = None;
        self.set_online(false, cx);
        cx.notify();
    }

    /// 连接成功后启用两个面板，并让远端面板进入恢复目录的加载态。
    fn go_live(&mut self, path: String, cx: &mut Context<Self>) {
        self.local.update(cx, |pane, cx| {
            pane.set_online(true, cx);
        });
        self.remote.update(cx, |pane, cx| {
            pane.set_online(true, cx);
            pane.begin_loading(path.clone(), cx);
        });
    }

    fn set_online(&mut self, online: bool, cx: &mut Context<Self>) {
        self.local
            .update(cx, |pane, cx| pane.set_online(online, cx));
        self.remote.update(cx, |pane, cx| {
            pane.set_online(online, cx);
            if !online {
                pane.set_entries("/".to_string(), Vec::new(), cx);
            }
        });
    }

    fn remember_local_directory(&mut self, path: &str) {
        if self.directory_history.remember_local(PathBuf::from(path)) {
            self.save_directory_history();
        }
    }

    fn remember_remote_directory(&mut self, path: String) {
        let Some((host, username)) = self.remote_endpoint.as_ref() else {
            return;
        };
        if self.directory_history.remember_remote(host, username, path) {
            self.save_directory_history();
        }
    }

    fn save_directory_history(&self) {
        if let Err(err) = store::save_directory_history(&self.directory_history) {
            log::warn!("保存目录历史失败：{err}");
        }
    }

    fn emit_state(&mut self, cx: &mut Context<Self>) {
        cx.emit(WorkspaceEvent::SessionStateChanged(self.state));
    }

    // -----------------------------------------------------------------------
    // 事件处理
    // -----------------------------------------------------------------------

    fn handle_event(&mut self, event: SftpEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            SftpEvent::HostKeyPrompt(prompt) => {
                self.ask_host_key(prompt, window, cx);
            }

            SftpEvent::Connected { home } => {
                self.state = SessionState::Connected;
                self.home = Some(home.clone());
                self.last_error = None;

                let remembered = self.remote_endpoint.as_ref().and_then(|(host, username)| {
                    self.directory_history
                        .remote_for(host, username)
                        .map(str::to_owned)
                });
                let initial_path = remembered.unwrap_or_else(|| home.clone());
                let fallback = (initial_path != home).then_some(home);
                self.go_live(initial_path.clone(), cx);
                self.request_remote_listing_with_fallback(initial_path, fallback, cx);

                self.emit_state(cx);
                self.toast(window, cx, Notification::success("已连接到服务器"));

                // 演示模式：`MACSCP_DEMO_UPLOAD=1` 时自动上传一个与远端同名的文件，
                // 用来手工验证「上传冲突」弹窗 —— 那个弹窗平时点不出来。
                #[cfg(feature = "demo-server")]
                if std::env::var_os("MACSCP_DEMO_UPLOAD").is_some() {
                    let path = std::env::temp_dir().join("notes.md");
                    if std::fs::write(&path, b"# demo upload\n").is_ok() {
                        self.upload_paths(vec![path], window, cx);
                    }
                }

                // `MACSCP_DEMO_FOLDER=1` 时自动传一棵小目录树：不用点右键也能
                // 看到「整目录上传」在队列里只有一行、远端多出一棵完整的树。
                #[cfg(feature = "demo-server")]
                if std::env::var_os("MACSCP_DEMO_FOLDER").is_some() {
                    match demo_tree() {
                        Some(root) => self.start_folder_transfer(
                            FolderDirection::Upload,
                            vec![FolderTarget::new(&*root.to_string_lossy())],
                            ExistingPolicy::Overwrite,
                            cx,
                        ),
                        None => log::warn!("演示目录准备失败，跳过整目录上传"),
                    }
                }

                // `MACSCP_DEMO_DOWNLOAD_FOLDER=1`：直接弹出「整目录下载」确认框。
                // 远端那棵树由演示服务器预置（配 `MACSCP_DEMO_PKG=1`），
                // 点「覆盖同名文件」就能看到整棵树落到本地。
                #[cfg(feature = "demo-server")]
                if std::env::var_os("MACSCP_DEMO_DOWNLOAD_FOLDER").is_some() {
                    self.demo_local_dir(cx);
                    self.confirm_folder_transfer(
                        FolderDirection::Download,
                        vec![FolderTarget::new(crate::demo::pkg_path())],
                        window,
                        cx,
                    );
                }

                // `MACSCP_DEMO_DOWNLOAD_GO=1`：跳过确认框直接开传，用来截「队列里
                // 只有一行、本地多出一棵完整的树」—— 那种状态没法靠点鼠标复现。
                #[cfg(feature = "demo-server")]
                if std::env::var_os("MACSCP_DEMO_DOWNLOAD_GO").is_some() {
                    self.demo_local_dir(cx);
                    self.start_folder_transfer(
                        FolderDirection::Download,
                        vec![FolderTarget::new(crate::demo::pkg_path())],
                        ExistingPolicy::Overwrite,
                        cx,
                    );
                }
            }

            SftpEvent::Disconnected => {
                self.connection_generation = self.connection_generation.wrapping_add(1);
                self.state = SessionState::Disconnected;
                self.home = None;
                self.remote_endpoint = None;
                self.set_online(false, cx);
                self.emit_state(cx);
            }

            SftpEvent::DirListed {
                request_id,
                path,
                entries,
            } => {
                // 迟到的响应直接丢弃：用户可能已经又点进了别的目录。
                if request_id == self.request_seq {
                    self.remember_remote_directory(path.clone());
                    self.remote
                        .update(cx, |pane, cx| pane.set_entries(path, entries, cx));
                }
            }

            SftpEvent::TransferStarted {
                id,
                resumed_from,
                total,
            } => {
                self.transfers
                    .update(cx, |queue, cx| queue.started(id, resumed_from, total, cx));
            }

            SftpEvent::TransferProgress {
                id,
                transferred,
                total,
                speed,
            } => {
                self.transfers.update(cx, |queue, cx| {
                    queue.progress(id, transferred, total, speed, cx)
                });
            }

            SftpEvent::TransferCompleted { id, outcome } => {
                let pending_external_edit = self.pending_external_edits.remove(&id);
                let pending_external_sync = self.pending_external_syncs.remove(&id);
                let failed = matches!(outcome, TransferOutcome::Failed(_));
                let partial_message = match &outcome {
                    TransferOutcome::Partial(message) => Some(message.clone()),
                    _ => None,
                };
                // 传输结束会改动某一侧的目录：成功会新增目标；取消或失败也可能留下
                // `.filepart` / 半成品。只有明确“跳过”保证什么都没改。
                let touched = matches!(
                    outcome,
                    TransferOutcome::Completed
                        | TransferOutcome::Partial(_)
                        | TransferOutcome::Cancelled
                        | TransferOutcome::Failed(_)
                );
                let direction = self.transfers.read(cx).direction_of(id);

                self.transfers
                    .update(cx, |queue, cx| queue.finished(id, outcome.clone(), cx));

                if touched
                    && pending_external_edit.is_none()
                    && let Some(direction) = direction
                {
                    self.schedule_refresh(direction, cx);
                }

                if let Some(edit) = pending_external_edit {
                    if matches!(outcome, TransferOutcome::Completed)
                        && edit.connection_generation == self.connection_generation
                        && self.state == SessionState::Connected
                    {
                        self.launch_external_editor(edit, window, cx);
                    } else if let Some(parent) = edit.local.parent() {
                        let _ = std::fs::remove_dir_all(parent);
                    }
                }

                if let Some(sync) = pending_external_sync {
                    let current_connection =
                        sync.connection_generation == self.connection_generation;
                    if current_connection
                        && let Some(edit) = self.external_edits.get_mut(&sync.local)
                    {
                        edit.uploading = false;
                        if matches!(outcome, TransferOutcome::Completed) {
                            edit.pending_since = (edit.revision > sync.revision).then(Instant::now);
                        } else {
                            // 这次失败后不反复重试并刷屏；再次保存会产生新 revision。
                            edit.pending_since = None;
                        }
                    }
                    if current_connection && matches!(outcome, TransferOutcome::Completed) {
                        self.toast(window, cx, Notification::success("修改已同步到远程"));
                    }
                }
                if failed {
                    self.toast(window, cx, Notification::error("传输失败"));
                } else if let Some(message) = partial_message {
                    self.toast(
                        window,
                        cx,
                        Notification::info(format!("传输部分完成：{message}")),
                    );
                }
            }

            SftpEvent::UploadConflict(conflict) => {
                // 队列里那一行先切成「等待确认」，用户才知道任务没丢。
                self.transfers
                    .update(cx, |queue, cx| queue.awaiting_decision(conflict.id, cx));
                self.ask_upload_conflict(conflict, window, cx);
            }

            SftpEvent::TextLoaded { path, content } => {
                let Some(entry) = self
                    .pending_text_edit
                    .as_ref()
                    .filter(|entry| entry.path == path)
                    .cloned()
                else {
                    return;
                };
                self.pending_text_edit = None;
                self.open_text_editor(
                    TextEditTarget::Remote(entry.path),
                    entry.name,
                    content,
                    window,
                    cx,
                );
            }

            SftpEvent::TextSaved { path } => {
                if self
                    .active_text_edit
                    .as_ref()
                    .is_some_and(|edit| edit.target == TextEditTarget::Remote(path.clone()))
                {
                    self.active_text_edit = None;
                    window.close_dialog(cx);
                    self.toast(window, cx, Notification::success("文件已保存"));
                    self.reload_remote(cx);
                }
            }

            SftpEvent::Notice(message) => {
                self.toast(window, cx, Notification::success(message));
                // 建目录/删除/重命名之后刷新当前远端目录。
                self.reload_remote(cx);
            }

            SftpEvent::Error { context, message } => {
                match context {
                    ErrorContext::Connect => {
                        self.state = SessionState::Disconnected;
                        self.last_error = Some(SharedString::from(message.clone()));
                        self.emit_state(cx);
                    }
                    ErrorContext::List => {
                        // 目录读取失败时，面板要退出加载态并把错误显示在自己的列表上方。
                        let message = SharedString::from(message.clone());
                        self.remote
                            .update(cx, |pane, cx| pane.set_error(message, cx));
                    }
                    ErrorContext::ReadText => {
                        self.pending_text_edit = None;
                    }
                    ErrorContext::WriteText => {
                        if let Some(edit) = self.active_text_edit.as_mut()
                            && matches!(edit.target, TextEditTarget::Remote(_))
                        {
                            edit.saving = false;
                            edit.error = Some(SharedString::from(message.clone()));
                        }
                    }
                    // 批量删除可能部分成功；即使最终事件是错误，也要反映已删除的条目。
                    ErrorContext::Remove => self.reload_remote(cx),
                    _ => {}
                }
                self.toast(window, cx, Notification::error(message));
            }
        }

        cx.notify();
    }

    fn toast(&self, window: &mut Window, cx: &mut App, note: Notification) {
        window.push_notification(note, cx);
    }

    // -----------------------------------------------------------------------
    // 目录
    // -----------------------------------------------------------------------

    /// 只发 `ListDir` 指令，不动面板状态（调用方通常已经在面板的更新上下文里）。
    fn request_remote_listing(&mut self, path: String, cx: &mut Context<Self>) {
        self.request_remote_listing_with_fallback(path, None, cx);
    }

    /// 恢复上次目录时携带登录目录作为后备。worker 只在服务器
    /// 明确返回 `NoSuchFile` 时才读后备目录，权限错误不会被掩盖。
    fn request_remote_listing_with_fallback(
        &mut self,
        path: String,
        fallback: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Connected {
            return;
        }
        self.request_seq += 1;
        let request_id = self.request_seq;
        self.worker.send(SftpCommand::ListDir {
            request_id,
            path,
            fallback,
        });
        cx.notify();
    }

    /// 刷新远端：先把面板切到加载态，再发请求。
    fn reload_remote(&mut self, cx: &mut Context<Self>) {
        if self.state != SessionState::Connected {
            return;
        }
        let path = self.remote.read(cx).path().to_string();
        self.remote
            .update(cx, |pane, cx| pane.begin_loading(path.clone(), cx));
        self.request_remote_listing(path, cx);
    }

    /// 传输结束后把「该刷新的面板」记下来，攒够一个合并窗口再真的去刷。
    ///
    /// 上传改的是远端目录，下载改的是本地目录 —— 只刷受影响的那一侧，
    /// 免得一次下载把远端列表也重新拉一遍（那是要过网络的）。
    fn schedule_refresh(&mut self, direction: TransferDirection, cx: &mut Context<Self>) {
        match direction {
            TransferDirection::Upload => self.pending_refresh.remote = true,
            TransferDirection::Download => self.pending_refresh.local = true,
        }

        // 已经有一个定时器在等：到点会把这些一起刷掉。
        if self.refresh_timer.is_some() {
            return;
        }

        self.refresh_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REFRESH_DEBOUNCE).await;
            let _ = this.update(cx, |workspace, cx| {
                workspace.refresh_timer = None;
                workspace.flush_refresh(cx);
            });
        }));
    }

    /// 真的去刷新。断开连接后不该再发 `ListDir`，所以远端那侧会自己判掉。
    fn flush_refresh(&mut self, cx: &mut Context<Self>) {
        let pending = std::mem::take(&mut self.pending_refresh);
        if pending.is_empty() {
            return;
        }

        if pending.local {
            self.local.update(cx, |pane, cx| pane.reload(cx));
        }
        if pending.remote {
            self.reload_remote(cx);
        }
        log::debug!(
            "传输结束后刷新列表：本地={} 远端={}",
            pending.local,
            pending.remote
        );
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 传输
    // -----------------------------------------------------------------------

    /// 弹「上传冲突」确认框。
    ///
    /// 强制选择：没有右上角关闭按钮、点遮罩不关、Esc 也不关 —— 任务正挂在
    /// worker 的等待表里，随便关掉会让它悬在那儿（用户还能在队列里点取消兜底）。
    fn ask_upload_conflict(
        &mut self,
        conflict: UploadConflict,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sender = self.worker.sender();
        log::info!(
            "打开上传冲突弹窗：id={} existing={:?} filepart={:?} checkpoint={} can_resume={}",
            conflict.id,
            conflict.existing_size,
            conflict.filepart_size,
            conflict.checkpoint_present,
            conflict.can_resume()
        );
        let prompt = cx.new(|_| UploadConflictPrompt::new(conflict, sender));

        window.open_dialog(cx, move |dialog, _window, _cx| {
            dialog
                .title("上传冲突")
                .overlay_closable(false)
                .close_button(false)
                .keyboard(false)
                .child(prompt.clone())
        });
    }

    /// 首次连接或服务器密钥变化时，必须由用户核对指纹后明确选择。
    /// 这是应用内模态框，不使用浏览器式 `alert/confirm/prompt`。
    fn ask_host_key(&mut self, prompt: HostKeyPrompt, window: &mut Window, cx: &mut Context<Self>) {
        let sender = self.worker.sender();
        let theme = cx.theme().clone();
        let changed = prompt.changed_line.is_some();
        let title = if changed {
            "服务器身份已变化"
        } else {
            "确认服务器身份"
        };
        let explanation = if let Some(line) = prompt.changed_line {
            format!(
                "安全警告：此服务器的主机密钥与 known_hosts 第 {line} 行不一致。除非你确认服务器刚更换过密钥，否则应拒绝连接。"
            )
        } else {
            "这是首次连接到该服务器。请通过可信渠道核对下面的 SHA-256 指纹。".to_string()
        };

        window.open_dialog(cx, move |dialog, _window, _cx| {
            let reject_sender = sender.clone();
            let once_sender = sender.clone();
            let save_sender = sender.clone();
            let theme = theme.clone();

            let reject =
                Button::new("host-key-reject")
                    .label("拒绝连接")
                    .on_click(move |_, window, cx| {
                        let _ = reject_sender
                            .send(SftpCommand::ResolveHostKey(HostKeyDecision::Reject));
                        window.close_dialog(cx);
                    });
            let reject = if changed {
                reject.primary()
            } else {
                reject.outline()
            };

            let save =
                Button::new("host-key-save")
                    .label("信任并保存")
                    .on_click(move |_, window, cx| {
                        let _ = save_sender
                            .send(SftpCommand::ResolveHostKey(HostKeyDecision::TrustAndSave));
                        window.close_dialog(cx);
                    });
            let save = if changed {
                save.danger()
            } else {
                save.primary()
            };

            dialog
                .title(title)
                .w(px(560.))
                .overlay_closable(false)
                .close_button(false)
                .keyboard(false)
                .child(
                    v_flex()
                        .w_full()
                        .gap_3()
                        .child(
                            div()
                                .text_sm()
                                .text_color(if changed {
                                    theme.danger
                                } else {
                                    theme.foreground
                                })
                                .child(SharedString::from(explanation.clone())),
                        )
                        .child(
                            v_flex()
                                .w_full()
                                .gap_1()
                                .p_3()
                                .rounded_md()
                                .border_1()
                                .border_color(if changed { theme.danger } else { theme.border })
                                .bg(theme.muted.opacity(0.3))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child("服务器"),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .child(SharedString::from(prompt.endpoint.clone())),
                                )
                                .child(
                                    div()
                                        .mt_2()
                                        .text_xs()
                                        .text_color(theme.muted_foreground)
                                        .child(SharedString::from(format!(
                                            "算法：{} · SHA-256 指纹",
                                            prompt.algorithm
                                        ))),
                                )
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme.primary)
                                        .child(SharedString::from(prompt.fingerprint.clone())),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("“仅本次信任”不会修改 ~/.ssh/known_hosts。"),
                        ),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(reject)
                        .child(
                            Button::new("host-key-once")
                                .outline()
                                .label("仅本次信任")
                                .on_click(move |_, window, cx| {
                                    let _ = once_sender.send(SftpCommand::ResolveHostKey(
                                        HostKeyDecision::TrustOnce,
                                    ));
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(save),
                )
        });
    }

    fn upload_entries(
        &mut self,
        entries: Vec<FileEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.upload_paths(
            entries
                .into_iter()
                .map(|entry| PathBuf::from(entry.path))
                .collect(),
            window,
            cx,
        );
    }

    fn download_entries(
        &mut self,
        entries: Vec<FileEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }

        let mut directories = Vec::new();
        for entry in entries {
            if entry.is_dir() {
                directories.push(FolderTarget::new(&entry.path));
            } else {
                self.queue_download(entry, cx);
            }
        }

        if !directories.is_empty() {
            // 一次选中多个文件夹也只问一次，整批共用同名文件处理策略。
            self.confirm_folder_transfer(FolderDirection::Download, directories, window, cx);
        }
    }

    fn queue_download(&mut self, entry: FileEntry, cx: &mut Context<Self>) {
        let local_dir = self.local.read(cx).path().to_string();
        let id = self.next_transfer_id;
        self.next_transfer_id += 1;

        self.transfers.update(cx, |queue, cx| {
            queue.enqueue(
                id,
                entry.name.clone(),
                TransferDirection::Download,
                entry.size,
                cx,
            )
        });
        self.worker.send(SftpCommand::Download {
            id,
            remote: entry.path,
            local: PathBuf::from(&local_dir).join(&entry.name),
            total: entry.size,
            resume: true,
        });
    }

    /// Finder 拖进来的条目 → 上传到当前远端目录。
    ///
    /// 文件各自入队、并发上传（worker 里每个任务一个 `tokio::spawn`）；
    /// 文件夹走整目录上传，一条队列行传一整棵树。
    pub fn upload_paths(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }
        if paths.is_empty() {
            return;
        }

        let (files, dirs) = split_dropped(paths);
        for path in files {
            self.queue_upload(path, cx);
        }

        if !dirs.is_empty() {
            // 一次拖进多个文件夹也只问一次：策略是整批共用的。
            let targets = dirs
                .iter()
                .map(|dir| FolderTarget::new(&*dir.to_string_lossy()))
                .collect();
            self.confirm_folder_transfer(FolderDirection::Upload, targets, window, cx);
        }
    }

    /// 演示下载前把本地面板切到临时目录。
    ///
    /// 本地面板默认停在 `$HOME` —— 演示时整棵 `demo-pkg/` 会落进去，那是在
    /// 用户的真实家目录里写东西。切到 `$TMPDIR/macscp-demo-download` 后
    /// 随便点、随便覆盖，退出后系统自己会清。
    #[cfg(feature = "demo-server")]
    fn demo_local_dir(&mut self, cx: &mut Context<Self>) {
        let dir = std::env::temp_dir().join("macscp-demo-download");
        if let Err(err) = std::fs::create_dir_all(&dir) {
            log::warn!("演示下载目录创建失败：{err}");
            return;
        }
        let dir = dir.to_string_lossy().to_string();
        self.local.update(cx, |pane, cx| pane.navigate(dir, cx));
    }

    /// 文件夹传输前的确认框：整批只问一次「同名文件怎么办」。
    ///
    /// 上传和下载共用同一个框，差别只在文案和目标路径（`FolderDirection`）。
    /// 两个按钮都是「确定动作」而不是单选 —— 选哪个就直接开传，省一次点击。
    /// 取消就什么都不做（此时队列里还没有行，不会留下悬着的任务）。
    fn confirm_folder_transfer(
        &mut self,
        direction: FolderDirection,
        targets: Vec<FolderTarget>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }
        if targets.is_empty() {
            return;
        }

        // 目标所在的目录：上传看远端当前目录，下载看本地当前目录。
        let destination = match direction {
            FolderDirection::Upload => self.remote.read(cx).path().to_string(),
            FolderDirection::Download => self.local.read(cx).path().to_string(),
        };
        let theme = cx.theme().clone();
        let headline = direction.headline(&targets);
        let target = direction.target(&targets, &destination);
        let names = folder_names(&targets);
        let note = direction.note();
        let bullets = direction.bullets();
        let title = direction.title();
        let weak = cx.entity().downgrade();

        window.open_dialog(cx, move |dialog, _window, _cx| {
            let skip = (weak.clone(), targets.clone());
            let overwrite = (weak.clone(), targets.clone());
            let theme = theme.clone();

            // 默认 448px 装不下 `…/T/macscp-demo-download/demo-pkg` 这种长路径，
            // 会折成三四段；加宽一点，路径基本能一行放下。
            dialog
                .title(title)
                .w(px(520.))
                .child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .child(div().text_sm().child(SharedString::from(headline.clone())))
                        // 目标路径单独一行：长路径折行时也不会把标题那句挤成两截。
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.primary)
                                .child(SharedString::from(target.clone())),
                        )
                        .when(!names.is_empty(), |this| {
                            this.child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(SharedString::from(names.clone())),
                            )
                        })
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(SharedString::from(note)),
                        )
                        // 两条路各自说明后果，别让用户猜「跳过」是跳过整个文件夹还是跳过文件。
                        .child(
                            v_flex()
                                .gap_1()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(bullets[0])
                                .child(bullets[1]),
                        ),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("folder-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("folder-skip")
                                .outline()
                                .label("跳过同名文件")
                                .on_click(move |_, window, cx| {
                                    let (weak, targets) = skip.clone();
                                    let _ = weak.update(cx, |workspace, cx| {
                                        workspace.start_folder_transfer(
                                            direction,
                                            targets,
                                            ExistingPolicy::Skip,
                                            cx,
                                        )
                                    });
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("folder-overwrite")
                                .primary()
                                .label("覆盖同名文件")
                                .on_click(move |_, window, cx| {
                                    let (weak, targets) = overwrite.clone();
                                    let _ = weak.update(cx, |workspace, cx| {
                                        workspace.start_folder_transfer(
                                            direction,
                                            targets,
                                            ExistingPolicy::Overwrite,
                                            cx,
                                        )
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    /// 真的开始传：每个文件夹一条队列行 + 一条 `UploadDir` / `DownloadDir` 指令。
    ///
    /// 总字节数要等 worker 扫完那棵树才知道，所以先按 0 入队，
    /// 随后的 `TransferStarted` 会把真实总量补上。
    fn start_folder_transfer(
        &mut self,
        direction: FolderDirection,
        targets: Vec<FolderTarget>,
        on_existing: ExistingPolicy,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }

        let remote_dir = self.remote.read(cx).path().to_string();
        let local_dir = self.local.read(cx).path().to_string();

        for target in targets {
            let id = self.next_transfer_id;
            self.next_transfer_id += 1;

            // 名字带一个结尾的 `/`，一眼能看出这是文件夹而不是同名文件。
            let row_direction = match direction {
                FolderDirection::Upload => TransferDirection::Upload,
                FolderDirection::Download => TransferDirection::Download,
            };
            self.transfers.update(cx, |queue, cx| {
                queue.enqueue(id, format!("{}/", target.name), row_direction, 0, cx)
            });

            match direction {
                FolderDirection::Upload => self.worker.send(SftpCommand::UploadDir {
                    id,
                    local: PathBuf::from(&target.path),
                    remote: join_remote(&remote_dir, &target.name),
                    on_existing,
                }),
                FolderDirection::Download => self.worker.send(SftpCommand::DownloadDir {
                    id,
                    remote: target.path.clone(),
                    local: PathBuf::from(&local_dir).join(&target.name),
                    on_existing,
                }),
            }
        }
        cx.notify();
    }

    /// 把一个本地文件排进队列并投递上传指令。
    fn queue_upload(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let size = std::fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
        let remote_dir = self.remote.read(cx).path().to_string();

        let id = self.next_transfer_id;
        self.next_transfer_id += 1;

        self.transfers.update(cx, |queue, cx| {
            queue.enqueue(id, name.clone(), TransferDirection::Upload, size, cx)
        });
        self.worker.send(SftpCommand::Upload {
            id,
            local: path,
            remote: join_remote(&remote_dir, &name),
            total: size,
            resume: true,
        });
        cx.notify();
    }

    fn warn(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.last_error = Some(message.into());
        cx.notify();
    }

    // -----------------------------------------------------------------------
    // 右键菜单动作
    // -----------------------------------------------------------------------

    /// 双击两侧文件都表示“打开”，传输只由明确的上传/下载动作触发。
    fn open_entry(
        &mut self,
        side: PaneSide,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match side {
            PaneSide::Local => self.edit_local_entry(entry, window, cx),
            PaneSide::Remote => match self.settings.remote_open_behavior {
                RemoteOpenBehavior::BuiltInTextEditor => self.request_text_edit(entry, window, cx),
                RemoteOpenBehavior::SystemDefault | RemoteOpenBehavior::CustomEditor => {
                    self.request_external_edit(entry, window, cx)
                }
            },
        }
    }

    fn handle_action(
        &mut self,
        side: PaneSide,
        entries: Vec<FileEntry>,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            FileAction::Upload => self.upload_entries(entries, window, cx),
            FileAction::Download => self.download_entries(entries, window, cx),
            FileAction::Edit => {
                let Some(entry) = entries.into_iter().next() else {
                    return;
                };
                self.open_entry(side, entry, window, cx);
            }
            FileAction::CopyPath => {
                let paths: Vec<_> = entries.into_iter().map(|entry| entry.path).collect();
                if paths.is_empty() {
                    return;
                }
                cx.write_to_clipboard(ClipboardItem::new_string(paths.join("\n")));
                self.toast(
                    window,
                    cx,
                    Notification::success(if paths.len() == 1 {
                        "路径已复制".to_string()
                    } else {
                        format!("已复制 {} 个路径", paths.len())
                    }),
                );
            }
            FileAction::RevealInFinder => {
                if side == PaneSide::Local
                    && let Some(entry) = entries.into_iter().next()
                {
                    cx.reveal_path(Path::new(&entry.path));
                    self.toast(window, cx, Notification::info("已在访达中显示"));
                }
            }
            FileAction::Rename => {
                if let Some(entry) = entries.into_iter().next() {
                    match side {
                        PaneSide::Local => self.open_local_rename_dialog(entry, window, cx),
                        PaneSide::Remote => self.open_rename_dialog(entry, window, cx),
                    }
                }
            }
            FileAction::Delete => {
                if entries.is_empty() {
                    return;
                }
                match side {
                    PaneSide::Local => self.confirm_local_trash(entries, window, cx),
                    PaneSide::Remote => self.confirm_delete(entries, window, cx),
                }
            }
            FileAction::NewFolder => match side {
                PaneSide::Local => self.new_local_folder(window, cx),
                PaneSide::Remote => self.new_folder(window, cx),
            },
            // 刷新通常由面板自己就地处理（`FilePane::request`），走到这里
            // 只可能是将来别的入口复用这个动作。
            FileAction::Refresh => match side {
                PaneSide::Local => self.local.update(cx, |pane, cx| pane.reload(cx)),
                PaneSide::Remote => self.reload_remote(cx),
            },
        }
    }

    fn edit_local_entry(&mut self, entry: FileEntry, window: &mut Window, cx: &mut Context<Self>) {
        if entry.is_dir() {
            return;
        }
        let path = PathBuf::from(&entry.path);
        match self.settings.remote_open_behavior {
            RemoteOpenBehavior::BuiltInTextEditor => {
                if entry.size > crate::sftp::transfer::MAX_EDITABLE_TEXT_SIZE {
                    self.toast(
                        window,
                        cx,
                        Notification::error("文件超过 2 MiB，请使用外部编辑器"),
                    );
                    return;
                }
                match std::fs::read(&path) {
                    Ok(bytes)
                        if bytes.len() as u64 <= crate::sftp::transfer::MAX_EDITABLE_TEXT_SIZE =>
                    {
                        self.open_text_editor(
                            TextEditTarget::Local(path),
                            entry.name,
                            crate::sftp::transfer::decode_text_for_editor(&bytes),
                            window,
                            cx,
                        );
                    }
                    Ok(_) => self.toast(
                        window,
                        cx,
                        Notification::error("文件超过 2 MiB，请使用外部编辑器"),
                    ),
                    Err(err) => self.toast(
                        window,
                        cx,
                        Notification::error(format!("读取 {} 失败：{err}", path.display())),
                    ),
                }
            }
            RemoteOpenBehavior::SystemDefault | RemoteOpenBehavior::CustomEditor => {
                let editor_path = match self.settings.remote_open_behavior {
                    RemoteOpenBehavior::CustomEditor => {
                        let Some(editor) = self.settings.editor_path.as_deref() else {
                            self.toast(window, cx, Notification::error("请先在设置中选择编辑器"));
                            return;
                        };
                        Some(editor)
                    }
                    _ => None,
                };
                match open_external_editor(&path, editor_path) {
                    Ok(()) => {
                        self.toast(window, cx, Notification::success("已用编辑器打开本地文件"))
                    }
                    Err(message) => self.toast(window, cx, Notification::error(message)),
                }
            }
        }
    }

    fn request_external_edit(
        &mut self,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Connected {
            self.toast(window, cx, Notification::error("请先连接服务器"));
            return;
        }
        if entry.is_dir() {
            return;
        }

        let editor_path = match self.settings.remote_open_behavior {
            RemoteOpenBehavior::SystemDefault => None,
            RemoteOpenBehavior::CustomEditor => {
                let Some(path) = self.settings.editor_path.clone() else {
                    self.toast(window, cx, Notification::error("请先在设置中选择编辑器"));
                    return;
                };
                Some(path)
            }
            RemoteOpenBehavior::BuiltInTextEditor => {
                self.request_text_edit(entry, window, cx);
                return;
            }
        };

        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        let local = match external_edit_cache_path(id, &entry.name) {
            Ok(path) => path,
            Err(message) => {
                self.toast(window, cx, Notification::error(message));
                return;
            }
        };

        self.transfers.update(cx, |queue, cx| {
            queue.enqueue(
                id,
                format!("编辑 {}", entry.name),
                TransferDirection::Download,
                entry.size,
                cx,
            )
        });
        self.pending_external_edits.insert(
            id,
            PendingExternalEdit {
                remote: entry.path.clone(),
                local: local.clone(),
                editor_path,
                connection_generation: self.connection_generation,
            },
        );
        self.worker.send(SftpCommand::Download {
            id,
            remote: entry.path,
            local,
            total: entry.size,
            resume: false,
        });
        cx.notify();
    }

    fn launch_external_editor(
        &mut self,
        edit: PendingExternalEdit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stamp = match file_stamp(&edit.local) {
            Ok(stamp) => stamp,
            Err(message) => {
                self.toast(window, cx, Notification::error(message));
                return;
            }
        };
        if let Err(message) = open_external_editor(&edit.local, edit.editor_path.as_deref()) {
            self.toast(window, cx, Notification::error(message));
            return;
        }

        let local = edit.local.clone();
        self.external_edits.insert(
            local.clone(),
            ActiveExternalEdit {
                remote: edit.remote,
                stamp,
                revision: 0,
                last_attempted_revision: 0,
                uploading: false,
                pending_since: None,
                connection_generation: edit.connection_generation,
            },
        );

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(EXTERNAL_EDIT_POLL).await;
                if this
                    .update(cx, |workspace, cx| workspace.poll_external_edit(&local, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        self.toast(
            window,
            cx,
            Notification::info("已用外部编辑器打开，保存后将自动同步"),
        );
    }

    fn poll_external_edit(&mut self, local: &Path, cx: &mut Context<Self>) {
        let Ok(stamp) = file_stamp(local) else {
            return;
        };
        let now = Instant::now();
        let dispatch = {
            let Some(edit) = self.external_edits.get_mut(local) else {
                return;
            };
            if stamp != edit.stamp {
                edit.stamp = stamp;
                edit.revision += 1;
                edit.pending_since = Some(now);
                return;
            }
            if self.state != SessionState::Connected
                || edit.connection_generation != self.connection_generation
                || edit.uploading
                || edit.revision == edit.last_attempted_revision
                || edit
                    .pending_since
                    .is_none_or(|changed| now.duration_since(changed) < EXTERNAL_EDIT_DEBOUNCE)
            {
                return;
            }

            edit.uploading = true;
            edit.last_attempted_revision = edit.revision;
            edit.pending_since = None;
            Some((edit.remote.clone(), edit.revision, stamp.len))
        };

        let Some((remote, revision, total)) = dispatch else {
            return;
        };
        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        let name = local
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "远程文件".to_string());
        self.transfers.update(cx, |queue, cx| {
            queue.enqueue(
                id,
                format!("同步 {name}"),
                TransferDirection::Upload,
                total,
                cx,
            )
        });
        self.pending_external_syncs.insert(
            id,
            PendingExternalSync {
                local: local.to_path_buf(),
                revision,
                connection_generation: self.connection_generation,
            },
        );
        self.worker.send(SftpCommand::UploadOverwrite {
            id,
            local: local.to_path_buf(),
            remote,
            total,
        });
        cx.notify();
    }

    fn request_text_edit(&mut self, entry: FileEntry, window: &mut Window, cx: &mut Context<Self>) {
        if self.state != SessionState::Connected {
            self.toast(window, cx, Notification::error("请先连接服务器"));
            return;
        }
        if entry.is_dir() {
            return;
        }
        if self.pending_text_edit.is_some() {
            self.toast(window, cx, Notification::info("正在读取文件，请稍候"));
            return;
        }
        if entry.size > crate::sftp::transfer::MAX_EDITABLE_TEXT_SIZE {
            self.toast(
                window,
                cx,
                Notification::error("文件超过 2 MiB，请下载后使用本地编辑器"),
            );
            return;
        }

        self.pending_text_edit = Some(entry.clone());
        self.worker.send(SftpCommand::ReadText { path: entry.path });
        cx.notify();
    }

    fn open_text_editor(
        &mut self,
        target: TextEditTarget,
        name: String,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| TextareaState::new(window, cx).default_value(content));
        self.active_text_edit = Some(ActiveTextEdit {
            target: target.clone(),
            saving: false,
            error: None,
        });

        let workspace = cx.entity().downgrade();
        let display_path = target.display_path();
        let focus_on_mount = Rc::new(Cell::new(true));

        window.open_dialog(cx, move |dialog, window, cx| {
            let editor = editor.clone();
            let target = target.clone();
            let display_path = display_path.clone();
            let save_owner = workspace.clone();
            let cancel_owner = workspace.clone();
            let close_owner = workspace.clone();
            let close_target = target.clone();
            let focus_on_mount = focus_on_mount.clone();
            let theme = cx.theme().clone();
            let (saving, error) = workspace
                .read_with(cx, |workspace, _| {
                    workspace
                        .active_text_edit
                        .as_ref()
                        .filter(|edit| edit.target == target)
                        .map(|edit| (edit.saving, edit.error.clone()))
                })
                .ok()
                .flatten()
                .unwrap_or((false, None));

            if focus_on_mount.replace(false) {
                let editor = editor.clone();
                window.defer(cx, move |window, cx| {
                    editor.focus_handle(cx).focus(window, cx);
                });
            }

            dialog
                .title(SharedString::from(format!("编辑 {name}")))
                .w(px(760.))
                .max_w(px(960.))
                .close_button(false)
                .overlay_closable(false)
                .keyboard(!saving)
                .on_close(move |_, _, cx| {
                    let _ = close_owner.update(cx, |workspace, cx| {
                        if workspace
                            .active_text_edit
                            .as_ref()
                            .is_some_and(|edit| edit.target == close_target)
                        {
                            workspace.active_text_edit = None;
                            cx.notify();
                        }
                    });
                })
                .child(
                    v_flex()
                        .w_full()
                        .gap_2()
                        .child(
                            div()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(SharedString::from(display_path.clone())),
                        )
                        .child(
                            Textarea::new(&editor)
                                .aria_label("文件内容")
                                .w_full()
                                .h(px(420.))
                                .disabled(saving),
                        )
                        .when_some(error, |this, error| {
                            this.child(div().text_xs().text_color(theme.danger).child(error))
                        }),
                )
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("text-editor-cancel")
                                .label("取消")
                                .disabled(saving)
                                .on_click(move |_, window, cx| {
                                    let _ = cancel_owner.update(cx, |workspace, cx| {
                                        workspace.active_text_edit = None;
                                        cx.notify();
                                    });
                                    window.close_dialog(cx);
                                }),
                        )
                        .child(
                            Button::new("text-editor-save")
                                .primary()
                                .label(if saving { "保存中…" } else { "保存" })
                                .loading(saving)
                                .on_click(move |_, window, cx| {
                                    let content = editor.read(cx).value().to_string();
                                    let target = target.clone();
                                    if content.len() as u64
                                        > crate::sftp::transfer::MAX_EDITABLE_TEXT_SIZE
                                    {
                                        let _ = save_owner.update(cx, |workspace, cx| {
                                            if let Some(edit) = workspace.active_text_edit.as_mut()
                                                && edit.target == target
                                            {
                                                edit.error = Some(SharedString::from(
                                                    "编辑后的内容超过 2 MiB，无法保存",
                                                ));
                                                cx.notify();
                                            }
                                        });
                                        return;
                                    }

                                    match target {
                                        TextEditTarget::Remote(path) => {
                                            let _ = save_owner.update(cx, |workspace, cx| {
                                                let Some(edit) =
                                                    workspace.active_text_edit.as_mut()
                                                else {
                                                    return;
                                                };
                                                if edit.target
                                                    != TextEditTarget::Remote(path.clone())
                                                    || edit.saving
                                                {
                                                    return;
                                                }

                                                edit.saving = true;
                                                edit.error = None;
                                                workspace
                                                    .worker
                                                    .send(SftpCommand::WriteText { path, content });
                                                cx.notify();
                                            });
                                        }
                                        TextEditTarget::Local(path) => {
                                            match std::fs::write(&path, content) {
                                                Ok(()) => {
                                                    let _ =
                                                        save_owner.update(cx, |workspace, cx| {
                                                            if workspace
                                                                .active_text_edit
                                                                .as_ref()
                                                                .is_some_and(|edit| {
                                                                    edit.target
                                                                        == TextEditTarget::Local(
                                                                            path.clone(),
                                                                        )
                                                                })
                                                            {
                                                                workspace.active_text_edit = None;
                                                                workspace
                                                                    .local
                                                                    .update(cx, |pane, cx| {
                                                                        pane.reload(cx)
                                                                    });
                                                                cx.notify();
                                                            }
                                                        });
                                                    window.close_dialog(cx);
                                                    window.push_notification(
                                                        Notification::success("本地文件已保存"),
                                                        cx,
                                                    );
                                                }
                                                Err(err) => {
                                                    let message = SharedString::from(format!(
                                                        "保存 {} 失败：{err}",
                                                        path.display()
                                                    ));
                                                    let _ =
                                                        save_owner.update(cx, |workspace, cx| {
                                                            if let Some(edit) =
                                                                workspace.active_text_edit.as_mut()
                                                                && edit.target
                                                                    == TextEditTarget::Local(
                                                                        path.clone(),
                                                                    )
                                                            {
                                                                edit.error = Some(message.clone());
                                                                cx.notify();
                                                            }
                                                        });
                                                }
                                            }
                                        }
                                    }
                                }),
                        ),
                )
        });
    }

    /// 不针对某个条目的菜单动作。目前只有「新建目录」。
    fn handle_pane_action(
        &mut self,
        side: PaneSide,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            FileAction::NewFolder => match side {
                PaneSide::Local => self.new_local_folder(window, cx),
                PaneSide::Remote => self.new_folder(window, cx),
            },
            other => log::warn!("忽略面板级动作：{other:?}"),
        }
    }

    fn open_local_rename_dialog(
        &mut self,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let from = PathBuf::from(&entry.path);
        let Some(parent) = from.parent().map(Path::to_path_buf) else {
            self.toast(window, cx, Notification::error("无法重命名该路径"));
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(entry.name));
        let local = self.local.clone();

        window.open_dialog(cx, move |dialog, _window, _cx| {
            let input = input.clone();
            let from = from.clone();
            let parent = parent.clone();
            let local = local.clone();

            dialog
                .title("重命名本地条目")
                .child(Input::new(&input).w_full())
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("local-rename-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("local-rename-ok")
                                .primary()
                                .label("确定")
                                .on_click(move |_, window, cx| {
                                    let name = input.read(cx).value().trim().to_string();
                                    if !crate::sftp::session::is_safe_entry_name(&name) {
                                        window.push_notification(
                                            Notification::error(
                                                "名称不能为空，也不能是 .、..，且不能包含 /",
                                            ),
                                            cx,
                                        );
                                        return;
                                    }
                                    let to = parent.join(name);
                                    if to == from {
                                        window.close_dialog(cx);
                                        return;
                                    }
                                    if to.exists() {
                                        window.push_notification(
                                            Notification::error("目标名称已经存在"),
                                            cx,
                                        );
                                        return;
                                    }
                                    match std::fs::rename(&from, &to) {
                                        Ok(()) => {
                                            local.update(cx, |pane, cx| pane.reload(cx));
                                            window.push_notification(
                                                Notification::success("重命名成功"),
                                                cx,
                                            );
                                            window.close_dialog(cx);
                                        }
                                        Err(err) => window.push_notification(
                                            Notification::error(format!("重命名失败：{err}")),
                                            cx,
                                        ),
                                    }
                                }),
                        ),
                )
        });
    }

    fn confirm_local_trash(
        &mut self,
        entries: Vec<FileEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let local = self.local.clone();
        let total = entries.len();
        let targets: Vec<_> = entries
            .into_iter()
            .map(|entry| (entry.name, PathBuf::from(entry.path)))
            .collect();
        let description = if total == 1 {
            format!("将 {} 移到废纸篓，可在访达中恢复。", targets[0].0)
        } else {
            format!("将选中的 {total} 项移到废纸篓，可在访达中恢复。")
        };

        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let local = local.clone();
            let targets = targets.clone();
            let description = description.clone();
            alert
                .confirm()
                .title("移到废纸篓")
                .description(SharedString::from(description))
                .on_ok(move |_, window, cx| {
                    let mut failures = Vec::new();
                    for (_, path) in &targets {
                        if let Err(message) = move_local_to_trash(path) {
                            failures.push(format!("{}：{message}", path.display()));
                        }
                    }

                    let moved = targets.len() - failures.len();
                    if moved > 0 {
                        local.update(cx, |pane, cx| pane.reload(cx));
                    }

                    if failures.is_empty() {
                        let message = if moved == 1 {
                            "已移到废纸篓".to_string()
                        } else {
                            format!("已将 {moved} 项移到废纸篓")
                        };
                        window.push_notification(Notification::success(message), cx);
                        return true;
                    }

                    window.push_notification(
                        Notification::error(batch_delete_error("移到废纸篓", moved, &failures)),
                        cx,
                    );
                    moved > 0
                })
        });
    }

    fn new_local_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dir = PathBuf::from(self.local.read(cx).path());
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("新目录名称"));
        let local = self.local.clone();

        window.open_dialog(cx, move |dialog, _window, _cx| {
            let input = input.clone();
            let dir = dir.clone();
            let local = local.clone();

            dialog
                .title("新建本地目录")
                .child(Input::new(&input).w_full())
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("local-new-folder-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("local-new-folder-ok")
                                .primary()
                                .label("创建")
                                .on_click(move |_, window, cx| {
                                    let name = input.read(cx).value().trim().to_string();
                                    if !crate::sftp::session::is_safe_entry_name(&name) {
                                        window.push_notification(
                                            Notification::error(
                                                "名称不能为空，也不能是 .、..，且不能包含 /",
                                            ),
                                            cx,
                                        );
                                        return;
                                    }
                                    match std::fs::create_dir(dir.join(name)) {
                                        Ok(()) => {
                                            local.update(cx, |pane, cx| pane.reload(cx));
                                            window.push_notification(
                                                Notification::success("目录已创建"),
                                                cx,
                                            );
                                            window.close_dialog(cx);
                                        }
                                        Err(err) => window.push_notification(
                                            Notification::error(format!("创建目录失败：{err}")),
                                            cx,
                                        ),
                                    }
                                }),
                        ),
                )
        });
    }

    fn open_rename_dialog(
        &mut self,
        entry: FileEntry,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }

        let input = cx.new(|cx| InputState::new(window, cx).default_value(entry.name.clone()));
        let sender = self.worker.sender();
        let from = entry.path.clone();
        let dir = remote_parent(&from);

        window.open_dialog(cx, move |dialog, _window, _cx| {
            let input = input.clone();
            let sender = sender.clone();
            let from = from.clone();
            let dir = dir.clone();

            dialog
                .title("重命名")
                .child(Input::new(&input).w_full())
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("rename-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(Button::new("rename-ok").primary().label("确定").on_click(
                            move |_, window, cx| {
                                let name = input.read(cx).value().trim().to_string();
                                if !crate::sftp::session::is_safe_entry_name(&name) {
                                    window.push_notification(
                                        Notification::error(
                                            "名称不能为空，也不能是 .、..，且不能包含 /",
                                        ),
                                        cx,
                                    );
                                    return;
                                }
                                let _ = sender.send(SftpCommand::Rename {
                                    from: from.clone(),
                                    to: join_remote(&dir, &name),
                                });
                                window.close_dialog(cx);
                            },
                        )),
                )
        });
    }

    fn confirm_delete(
        &mut self,
        entries: Vec<FileEntry>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }

        let sender = self.worker.sender();
        let total = entries.len();
        let contains_directory = entries.iter().any(FileEntry::is_dir);
        let description = if total == 1 {
            format!("将永久删除 {}，此操作不可撤销。", entries[0].name)
        } else if contains_directory {
            format!("将永久删除选中的 {total} 项（含文件夹及其内容），此操作不可撤销。")
        } else {
            format!("将永久删除选中的 {total} 个文件，此操作不可撤销。")
        };
        let entries: Vec<_> = entries
            .into_iter()
            .map(|entry| {
                let is_dir = entry.is_dir();
                (entry.path, is_dir)
            })
            .collect();

        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let sender = sender.clone();
            let entries = entries.clone();
            let description = description.clone();
            alert
                .confirm()
                .title("确认删除")
                .description(SharedString::from(description))
                .on_ok(move |_, _, _| {
                    let _ = sender.send(SftpCommand::Remove {
                        entries: entries.clone(),
                    });
                    true
                })
        });
    }

    /// 远端右键菜单「新建目录」。
    pub fn new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }

        let dir = self.remote.read(cx).path().to_string();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("新目录名称"));
        let sender = self.worker.sender();

        window.open_dialog(cx, move |dialog, _window, _cx| {
            let input = input.clone();
            let sender = sender.clone();
            let dir = dir.clone();

            dialog
                .title("新建目录")
                .child(Input::new(&input).w_full())
                .footer(
                    h_flex()
                        .justify_end()
                        .gap_2()
                        .child(
                            Button::new("new-folder-cancel")
                                .label("取消")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("new-folder-ok")
                                .primary()
                                .label("创建")
                                .on_click(move |_, window, cx| {
                                    let name = input.read(cx).value().trim().to_string();
                                    if !crate::sftp::session::is_safe_entry_name(&name) {
                                        window.push_notification(
                                            Notification::error(
                                                "名称不能为空，也不能是 .、..，且不能包含 /",
                                            ),
                                            cx,
                                        );
                                        return;
                                    }
                                    let _ = sender.send(SftpCommand::Mkdir {
                                        path: join_remote(&dir, &name),
                                    });
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    // -----------------------------------------------------------------------
    // 渲染
    // -----------------------------------------------------------------------

    fn render_split(&self) -> AnyElement {
        let remote = if self.state == SessionState::Connected {
            self.remote.clone().into_any_element()
        } else {
            div().size_full().into_any_element()
        };

        h_resizable("workspace-horizontal")
            .child(
                resizable_panel()
                    .size(px(560.))
                    .size_range(px(300.)..px(2000.))
                    .child(self.local.clone()),
            )
            .child(resizable_panel().child(remote))
            .into_any_element()
    }

    fn render_collapsed_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (active, total) = self.transfers.read(cx).summary();

        let label = if total == 0 {
            "传输队列 · 空".to_string()
        } else if active > 0 {
            format!("传输队列 · {active} 个进行中 / 共 {total}")
        } else {
            format!("传输队列 · {total} 个任务（已结束）")
        };

        h_flex()
            .w_full()
            .flex_shrink_0()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_t_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.35))
            .child(
                h_flex()
                    .gap_1p5()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(IconName::ListChecks)
                    .child(SharedString::from(label)),
            )
            .child(
                Button::new("expand-queue")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronUp)
                    .tooltip("展开队列")
                    .on_click(
                        cx.listener(|workspace, _, _, cx| workspace.set_collapsed(false, cx)),
                    ),
            )
            .into_any_element()
    }
}

fn external_edit_cache_path(id: u64, name: &str) -> Result<PathBuf, String> {
    let file_name = Path::new(name)
        .file_name()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| std::ffi::OsStr::new("remote-file"));
    let dir = dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("macscp")
        .join("external-edits")
        .join(format!("{}-{id}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("创建编辑缓存 {} 失败：{err}", dir.display()))?;
    Ok(dir.join(file_name))
}

fn file_stamp(path: &Path) -> Result<FileStamp, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|err| format!("读取编辑缓存 {} 失败：{err}", path.display()))?;
    Ok(FileStamp {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(target_os = "macos")]
fn move_local_to_trash(path: &Path) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSURL};

    let url = NSURL::from_file_path(path)
        .ok_or_else(|| format!("无法识别本地路径：{}", path.display()))?;
    NSFileManager::defaultManager()
        .trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|err| format!("移到废纸篓失败：{err}"))
}

#[cfg(not(target_os = "macos"))]
fn move_local_to_trash(_path: &Path) -> Result<(), String> {
    Err("当前系统不支持移到废纸篓".to_string())
}

fn batch_delete_error(action: &str, completed: usize, failures: &[String]) -> String {
    let preview = failures
        .iter()
        .take(2)
        .cloned()
        .collect::<Vec<_>>()
        .join("；");
    let omitted = failures.len().saturating_sub(2);
    let omitted = if omitted > 0 {
        format!("；另有 {omitted} 项失败")
    } else {
        String::new()
    };
    format!(
        "{action}完成 {completed} 项，失败 {} 项：{preview}{omitted}",
        failures.len()
    )
}

fn open_external_editor(local: &Path, editor_path: Option<&Path>) -> Result<(), String> {
    let mut command = match editor_path {
        Some(editor_path)
            if editor_path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("app")) =>
        {
            let mut command = Command::new("/usr/bin/open");
            command.arg("-a").arg(editor_path);
            command
        }
        Some(editor_path) => Command::new(editor_path),
        None => Command::new("/usr/bin/open"),
    };
    command.arg(local);
    let mut child = command
        .spawn()
        .map_err(|err| format!("启动编辑器失败：{err}"))?;
    // `open` 很快就会退出；后台回收它，避免主线程等待或留下僵尸进程。
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let split = self.render_split();

        if self.collapsed {
            let bar = self.render_collapsed_bar(cx);
            v_flex()
                .size_full()
                .bg(theme.background)
                .child(div().flex_1().min_h_0().child(split))
                .child(bar)
                .into_any_element()
        } else {
            let queue = self.transfers.clone();
            v_flex()
                .size_full()
                .bg(theme.background)
                .child(
                    div().flex_1().min_h_0().child(
                        v_resizable("workspace-vertical")
                            .child(
                                resizable_panel()
                                    .size(px(520.))
                                    .size_range(px(200.)..px(1600.))
                                    .child(split),
                            )
                            .child(
                                resizable_panel()
                                    .size(px(180.))
                                    .size_range(px(96.)..px(420.))
                                    .child(queue),
                            ),
                    ),
                )
                .into_any_element()
        }
    }
}

// ---------------------------------------------------------------------------
// 拖放
// ---------------------------------------------------------------------------

/// 演示模式用的本地目录树：`dist/{a.txt, sub/b.txt, sub/deep/c.bin, empty/}`。
///
/// 只给 `MACSCP_DEMO_FOLDER` 用，返回 None 表示准备失败（不 panic）。
#[cfg(feature = "demo-server")]
fn demo_tree() -> Option<PathBuf> {
    let root = std::env::temp_dir().join("macscp-demo-dist");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("sub/deep")).ok()?;
    std::fs::create_dir_all(root.join("empty")).ok()?;
    std::fs::write(root.join("a.txt"), b"# demo folder\n").ok()?;
    std::fs::write(root.join("sub/b.txt"), b"second file\n").ok()?;
    std::fs::write(root.join("sub/deep/c.bin"), vec![7u8; 256 * 1024]).ok()?;
    Some(root)
}

/// 把拖进来的路径拆成「文件」和「目录」：文件直接入队，目录走整目录上传。
///
/// 已经不存在 / 读不到的条目直接忽略 —— 拖拽过程中源文件被删掉是常事，
/// 不该让整个拖放失败。
fn split_dropped(paths: Vec<PathBuf>) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();

    for path in paths {
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => dirs.push(path),
            Ok(_) => files.push(path),
            Err(err) => log::warn!("忽略拖入的 {}：{err}", path.display()),
        }
    }

    (files, dirs)
}

/// 整目录传输的方向。上传和下载共用一套确认框与派发逻辑，只有文案和
/// 「目标目录从哪来」不同 —— 用一个枚举把差异集中在一处，比复制两份强。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FolderDirection {
    Upload,
    Download,
}

impl FolderDirection {
    fn title(self) -> &'static str {
        match self {
            FolderDirection::Upload => "上传文件夹",
            FolderDirection::Download => "下载文件夹",
        }
    }

    /// 标题句：单个点名文件夹，多个只报个数。
    fn headline(self, targets: &[FolderTarget]) -> String {
        let verb = match self {
            FolderDirection::Upload => "上传到远端",
            FolderDirection::Download => "下载到本地",
        };
        match targets {
            [] => String::new(),
            [one] => format!("将「{}」{verb}", one.name),
            many => format!("将 {} 个文件夹{verb}", many.len()),
        }
    }

    /// 目标路径：单个给完整路径，多个给所在的目标目录。
    fn target(self, targets: &[FolderTarget], destination: &str) -> String {
        match (self, targets) {
            (FolderDirection::Upload, [one]) => join_remote(destination, &one.name),
            (FolderDirection::Download, [one]) => PathBuf::from(destination)
                .join(&one.name)
                .display()
                .to_string(),
            (_, _) => destination.to_string(),
        }
    }

    fn note(self) -> &'static str {
        match self {
            FolderDirection::Upload => "整棵目录会递归上传（符号链接跳过）。远端已存在的同名文件：",
            FolderDirection::Download => {
                "整棵目录会递归下载（符号链接跳过）。本地已存在的同名文件："
            }
        }
    }

    fn bullets(self) -> [&'static str; 2] {
        match self {
            FolderDirection::Upload => [
                "· 跳过同名：保留远端文件不动",
                "· 覆盖同名：先写临时文件，写完再原子替换",
            ],
            FolderDirection::Download => [
                "· 跳过同名：保留本地文件不动",
                "· 覆盖同名：本地那份不完整时从断点接着下",
            ],
        }
    }
}

/// 一次整目录传输的目标：完整路径 + 末段名字。
///
/// 上传时 `path` 是本地绝对路径，下载时是远端 POSIX 路径 —— 两种都是字符串，
/// 本地那边再 `PathBuf::from` 回去。这样确认框和派发逻辑只用写一份。
#[derive(Clone, Debug)]
struct FolderTarget {
    path: String,
    name: String,
}

impl FolderTarget {
    fn new(path: impl AsRef<str>) -> Self {
        let path = path.as_ref().to_string();
        let name = Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        Self { path, name }
    }
}

/// 多个文件夹时才需要列名字（单个的话标题已经点名了）；最多列三个。
fn folder_names(targets: &[FolderTarget]) -> String {
    if targets.len() < 2 {
        return String::new();
    }

    let names: Vec<&str> = targets.iter().map(|target| target.name.as_str()).collect();
    const MAX: usize = 3;
    if names.len() <= MAX {
        names.join("、")
    } else {
        format!("{} 等 {} 个", names[..MAX].join("、"), names.len())
    }
}

#[cfg(test)]
mod tests {
    // 见 `file_pane.rs` 里的同名说明：`use super::*` 会把 gpui 的 `test` 宏
    // 一起带进来，必须显式导入标准库的那个。
    use ::core::prelude::v1::test;

    use super::*;

    #[test]
    fn split_dropped_separates_files_and_directories() {
        let root = std::env::temp_dir().join(format!("macscp-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).expect("建临时目录");
        let file = root.join("a.txt");
        std::fs::write(&file, b"hi").expect("写临时文件");

        let (files, dirs) =
            split_dropped(vec![file.clone(), root.join("sub"), root.join("gone.txt")]);

        assert_eq!(files, vec![file]);
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("sub"));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 下载方向的确认框文案：点名文件夹，目标路径落在本地目录下。
    #[test]
    fn download_folder_prompt_points_at_the_local_dir() {
        let targets = vec![FolderTarget::new("/home/tester/demo-pkg")];

        assert_eq!(FolderDirection::Download.title(), "下载文件夹");
        assert_eq!(
            FolderDirection::Download.headline(&targets),
            "将「demo-pkg」下载到本地"
        );
        assert_eq!(
            FolderDirection::Download.target(&targets, "/Users/me/Downloads"),
            "/Users/me/Downloads/demo-pkg"
        );
        assert!(
            FolderDirection::Download
                .note()
                .contains("本地已存在的同名文件"),
            "下载问的是本地那份要不要覆盖"
        );
        assert_eq!(folder_names(&targets), "", "单个文件夹标题已点名，不用再列");
    }

    /// 上传方向同理，只是目标路径拼在远端。
    #[test]
    fn upload_folder_prompt_points_at_the_remote_dir() {
        let targets = vec![FolderTarget::new("/Users/me/dist")];

        assert_eq!(FolderDirection::Upload.title(), "上传文件夹");
        assert_eq!(
            FolderDirection::Upload.headline(&targets),
            "将「dist」上传到远端"
        );
        assert_eq!(
            FolderDirection::Upload.target(&targets, "/home/tester"),
            "/home/tester/dist"
        );
        assert!(
            FolderDirection::Upload
                .note()
                .contains("远端已存在的同名文件"),
            "上传问的是远端那份要不要覆盖"
        );
    }

    /// 多个文件夹：标题只报个数，目标只给所在目录，名字最多列三个。
    #[test]
    fn multiple_folders_are_summarized() {
        let targets: Vec<FolderTarget> = ["/a/one", "/a/two", "/a/three", "/a/four"]
            .iter()
            .map(FolderTarget::new)
            .collect();

        assert_eq!(
            FolderDirection::Download.headline(&targets),
            "将 4 个文件夹下载到本地"
        );
        assert_eq!(
            FolderDirection::Download.target(&targets, "/tmp"),
            "/tmp",
            "多个目标只给所在目录"
        );
        assert_eq!(folder_names(&targets), "one、two、three 等 4 个");
    }

    #[test]
    fn batch_delete_error_limits_long_failure_details() {
        let failures = vec![
            "one：权限不足".to_string(),
            "two：文件不存在".to_string(),
            "three：目录非空".to_string(),
        ];

        assert_eq!(
            batch_delete_error("移到废纸篓", 2, &failures),
            "移到废纸篓完成 2 项，失败 3 项：one：权限不足；two：文件不存在；另有 1 项失败"
        );
    }
}
