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

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    notification::Notification,
    resizable::{h_resizable, resizable_panel, v_resizable},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::sftp::session::{join_remote, remote_parent};
use crate::sftp::types::{
    ErrorContext, ExistingPolicy, FileEntry, SessionConfig, TransferDirection, TransferOutcome,
    UploadConflict,
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
    _subscriptions: Vec<Subscription>,
    _pump: Task<()>,
}

/// 攒着待刷新的面板。
#[derive(Clone, Copy, Default)]
struct PendingRefresh {
    local: bool,
    remote: bool,
}

impl PendingRefresh {
    fn is_empty(self) -> bool {
        !self.local && !self.remote
    }
}

/// 传输完成到刷新列表之间的合并窗口。
const REFRESH_DEBOUNCE: Duration = Duration::from_millis(300);

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let worker = SftpWorker::spawn();

        let home = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .to_string_lossy()
            .to_string();

        // 左侧从一开始就读真实磁盘：本地浏览不需要连服务器。
        // 右侧在连接成功前仍是 Mock，由 `go_live` 换成服务器数据。
        let local = cx.new(|cx| FilePane::new(PaneSide::Local, home, window, cx));
        let remote = cx.new(|cx| {
            FilePane::new_mock(PaneSide::Remote, "/srv/app".to_string(), window, cx)
        });
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
                FilePaneEvent::Activate(entry) => {
                    workspace.download_entry(entry.clone(), window, cx)
                }
                FilePaneEvent::Action { entry, action } => {
                    workspace.handle_action(entry.clone(), *action, window, cx);
                }
                FilePaneEvent::PaneAction(action) => {
                    workspace.handle_pane_action(*action, window, cx);
                }
                // Finder 拖进来的东西：文件直接上传，文件夹整棵上传。
                FilePaneEvent::DropFiles(paths) => {
                    workspace.upload_paths(paths.clone(), window, cx);
                }
            },
        ));

        // 本地面板：双击文件 = 上传；右键菜单同理。目录导航它自己处理。
        subscriptions.push(cx.subscribe_in(
            &local,
            window,
            |workspace, _pane, event: &FilePaneEvent, window, cx| match event {
                FilePaneEvent::Navigate(_) => {}
                FilePaneEvent::Activate(entry) => workspace.upload_entry(entry.clone(), window, cx),
                FilePaneEvent::Action { entry, action } => {
                    workspace.handle_action(entry.clone(), *action, window, cx);
                }
                FilePaneEvent::PaneAction(action) => {
                    workspace.handle_pane_action(*action, window, cx);
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
            home: None,
            last_error: None,
            request_seq: 0,
            next_transfer_id: 1,
            pending_refresh: PendingRefresh::default(),
            refresh_timer: None,
            _subscriptions: subscriptions,
            _pump: pump,
        }
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
        self.state = SessionState::Connecting;
        self.last_error = None;
        self.worker.send(SftpCommand::Connect(config));
        cx.notify();
    }

    /// 由工具栏的「断开」触发，理由同上：不 emit。
    pub fn disconnect(&mut self, cx: &mut Context<Self>) {
        self.worker.send(SftpCommand::Disconnect);
        self.state = SessionState::Disconnected;
        self.home = None;
        self.set_online(false, cx);
        cx.notify();
    }

    /// 连接成功后把两个面板切到真实数据源。
    ///
    /// 远端面板只切模式、不在这里触发 `Navigate` —— 本实体此刻正被借用，
    /// 同步回调会撞上独占租约。目录由下面主动请求。
    fn go_live(&mut self, home: String, cx: &mut Context<Self>) {
        self.local.update(cx, |pane, cx| {
            pane.use_live_data(cx);
            pane.set_online(true, cx);
        });
        self.remote.update(cx, |pane, cx| {
            pane.leave_mock(cx);
            pane.set_online(true, cx);
            pane.begin_loading(home.clone(), cx);
        });
    }

    fn set_online(&mut self, online: bool, cx: &mut Context<Self>) {
        self.local.update(cx, |pane, cx| pane.set_online(online, cx));
        self.remote.update(cx, |pane, cx| pane.set_online(online, cx));
    }

    fn emit_state(&mut self, cx: &mut Context<Self>) {
        cx.emit(WorkspaceEvent::SessionStateChanged(self.state));
    }

    // -----------------------------------------------------------------------
    // 事件处理
    // -----------------------------------------------------------------------

    fn handle_event(&mut self, event: SftpEvent, window: &mut Window, cx: &mut Context<Self>) {
        match event {
            SftpEvent::Connected { home } => {
                self.state = SessionState::Connected;
                self.home = Some(home.clone());
                self.last_error = None;

                self.go_live(home.clone(), cx);
                self.request_remote_listing(home, cx);

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
                self.state = SessionState::Disconnected;
                self.home = None;
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
                    self.remote
                        .update(cx, |pane, cx| pane.set_entries(path, entries, cx));
                }
            }

            SftpEvent::TransferStarted {
                id,
                resumed_from,
                total,
            } => {
                self.transfers.update(cx, |queue, cx| {
                    queue.started(id, resumed_from, total, cx)
                });
            }

            SftpEvent::TransferProgress {
                id,
                transferred,
                total,
                speed,
            } => {
                self.transfers
                    .update(cx, |queue, cx| queue.progress(id, transferred, total, speed, cx));
            }

            SftpEvent::TransferCompleted { id, outcome } => {
                let failed = matches!(outcome, TransferOutcome::Failed(_));
                // 传输结束会改动某一侧的目录：上传多一个文件、下载多一个文件，
                // 取消则会留下 `.filepart` / 半成品。两种都值得刷一下列表。
                let touched = matches!(
                    outcome,
                    TransferOutcome::Completed | TransferOutcome::Cancelled
                );
                let direction = self.transfers.read(cx).direction_of(id);

                self.transfers
                    .update(cx, |queue, cx| queue.finished(id, outcome, cx));

                if touched && let Some(direction) = direction {
                    self.schedule_refresh(direction, cx);
                }
                if failed {
                    self.toast(window, cx, Notification::error("传输失败"));
                }
            }

            SftpEvent::UploadConflict(conflict) => {
                // 队列里那一行先切成「等待确认」，用户才知道任务没丢。
                self.transfers
                    .update(cx, |queue, cx| queue.awaiting_decision(conflict.id, cx));
                self.ask_upload_conflict(conflict, window, cx);
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
                        self.remote.update(cx, |pane, cx| pane.set_error(message, cx));
                    }
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
        if self.state != SessionState::Connected {
            return;
        }
        self.request_seq += 1;
        let request_id = self.request_seq;
        self.worker.send(SftpCommand::ListDir { request_id, path });
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

    /// 刷新两侧面板。
    pub fn refresh_all(&mut self, cx: &mut Context<Self>) {
        self.local.update(cx, |pane, cx| pane.reload(cx));
        self.reload_remote(cx);
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
            "打开上传冲突弹窗：id={} existing={:?} filepart={:?} can_resume={}",
            conflict.id,
            conflict.existing_size,
            conflict.filepart_size,
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

    fn upload_entry(&mut self, entry: FileEntry, window: &mut Window, cx: &mut Context<Self>) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }
        if entry.is_dir() {
            // 目录整棵传。远端同名文件怎么办必须先问一句 —— 文件夹上传不会
            // 中途弹冲突框（那样一个目录能弹几百次），策略只能一次定下来。
            self.confirm_folder_transfer(
                FolderDirection::Upload,
                vec![FolderTarget::new(&entry.path)],
                window,
                cx,
            );
            return;
        }

        let remote_dir = self.remote.read(cx).path().to_string();
        let id = self.next_transfer_id;
        self.next_transfer_id += 1;

        self.transfers.update(cx, |queue, cx| {
            queue.enqueue(
                id,
                entry.name.clone(),
                TransferDirection::Upload,
                entry.size,
                cx,
            )
        });
        self.worker.send(SftpCommand::Upload {
            id,
            local: PathBuf::from(&entry.path),
            remote: join_remote(&remote_dir, &entry.name),
            total: entry.size,
            resume: true,
        });
    }

    fn download_entry(&mut self, entry: FileEntry, window: &mut Window, cx: &mut Context<Self>) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }
        if entry.is_dir() {
            // 目录整棵取回，同样先问一次「本地同名文件怎么办」。
            self.confirm_folder_transfer(
                FolderDirection::Download,
                vec![FolderTarget::new(&entry.path)],
                window,
                cx,
            );
            return;
        }

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
            let targets = dirs.iter().map(|dir| FolderTarget::new(&*dir.to_string_lossy())).collect();
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
            dialog.title(title).w(px(520.)).child(
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
                                    workspace
                                        .start_folder_transfer(
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

    fn handle_action(
        &mut self,
        entry: FileEntry,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            FileAction::Upload => self.upload_entry(entry, window, cx),
            FileAction::Download => self.download_entry(entry, window, cx),
            FileAction::Rename => self.open_rename_dialog(entry, window, cx),
            FileAction::Delete => self.confirm_delete(entry, window, cx),
            FileAction::NewFolder => self.new_folder(window, cx),
            // 刷新通常由面板自己就地处理（`FilePane::request`），走到这里
            // 只可能是将来别的入口复用这个动作：那就两侧都刷。
            FileAction::Refresh => self.refresh_all(cx),
        }
    }

    /// 不针对某个条目的菜单动作。目前只有「新建目录」。
    fn handle_pane_action(
        &mut self,
        action: FileAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match action {
            FileAction::NewFolder => self.new_folder(window, cx),
            other => log::warn!("忽略面板级动作：{other:?}"),
        }
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
                                if !name.is_empty() {
                                    let _ = sender.send(SftpCommand::Rename {
                                        from: from.clone(),
                                        to: join_remote(&dir, &name),
                                    });
                                }
                                window.close_dialog(cx);
                            },
                        )),
                )
        });
    }

    fn confirm_delete(&mut self, entry: FileEntry, window: &mut Window, cx: &mut Context<Self>) {
        if self.state != SessionState::Connected {
            self.warn("请先连接服务器", cx);
            return;
        }

        let sender = self.worker.sender();
        let path = entry.path.clone();
        let name = entry.name.clone();
        let is_dir = entry.is_dir();

        window.open_alert_dialog(cx, move |alert, _window, _cx| {
            let sender = sender.clone();
            let path = path.clone();
            alert
                .confirm()
                .title("确认删除")
                .description(SharedString::from(format!(
                    "将永久删除 {name}，此操作不可撤销。"
                )))
                .on_ok(move |_, _, _| {
                    let _ = sender.send(SftpCommand::Remove {
                        path: path.clone(),
                        is_dir,
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
                        .child(Button::new("new-folder-ok").primary().label("创建").on_click(
                            move |_, window, cx| {
                                let name = input.read(cx).value().trim().to_string();
                                if name.is_empty() {
                                    return;
                                }
                                let _ = sender.send(SftpCommand::Mkdir {
                                    path: join_remote(&dir, &name),
                                });
                                window.close_dialog(cx);
                            },
                        )),
                )
        });
    }

    // -----------------------------------------------------------------------
    // 渲染
    // -----------------------------------------------------------------------

    fn render_split(&self) -> AnyElement {
        h_resizable("workspace-horizontal")
            .child(
                resizable_panel()
                    .size(px(560.))
                    .size_range(px(300.)..px(2000.))
                    .child(self.local.clone()),
            )
            .child(resizable_panel().child(self.remote.clone()))
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
                    .on_click(cx.listener(|workspace, _, _, cx| workspace.set_collapsed(false, cx))),
            )
            .into_any_element()
    }
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
                .child(div().flex_1().min_h_0().child(
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
                ))
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
            (FolderDirection::Download, [one]) => {
                PathBuf::from(destination).join(&one.name).display().to_string()
            }
            (_, _) => destination.to_string(),
        }
    }

    fn note(self) -> &'static str {
        match self {
            FolderDirection::Upload => {
                "整棵目录会递归上传（符号链接跳过）。远端已存在的同名文件："
            }
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
            FolderDirection::Download.note().contains("本地已存在的同名文件"),
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
            FolderDirection::Upload.note().contains("远端已存在的同名文件"),
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
}

