//! 单个文件面板：本地或远端，使用虚拟化的 `Table` 渲染。
//!
//! 面板本身不碰网络、也不碰另一端 —— 它只负责：
//! * 把当前目录渲染成四列表格（名称 / 大小 / 权限 / 修改时间）；
//! * 顶部面包屑 + 「上级」/「显示隐藏文件」/「刷新」按钮；
//! * 双击进入目录、双击文件抛 [`FilePaneEvent::Activate`]；
//! * 右键弹出 [`gpui_kit::component::menu::PopupMenu`]，把用户选的动作包成
//!   [`FilePaneEvent`] 交给上层（上传/下载/新建目录/重命名/删除，刷新则就地处理）。
//!
//! 列表顶部还有一行 `..`，等价于「返回上一级」。

use std::path::{Path, PathBuf};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Selectable as _, Sizable as _,
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::PopupMenuItem,
    menu::PopupMenu,
    table::{Column, DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::sftp::session::{format_mode, format_size, format_time, remote_parent, sort_entries};
use crate::sftp::types::{EntryKind, FileEntry};

/// 列表顶部「返回上一级」那一行的名字。POSIX 不允许真实文件叫这个名字。
pub const PARENT_ROW_NAME: &str = "..";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PaneSide {
    Local,
    Remote,
}

/// 右键菜单里能被选中的动作。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileAction {
    /// 本地条目 → 当前远程目录。落在目录上时整棵递归上传。
    Upload,
    /// 远程条目 → 当前本地目录。落在目录上时整棵递归下载。
    Download,
    Rename,
    Delete,
    /// 在远端当前目录下建目录。与右键落在哪一行无关。
    NewFolder,
    /// 重新读当前目录。纯面板本地动作，不惊动上层。
    Refresh,
}

#[derive(Clone, Debug)]
pub enum FilePaneEvent {
    /// 远端面板请求切换目录（本地面板自己处理，不发这个事件）。
    Navigate(String),
    /// 双击了一个文件：由上层决定是上传还是下载。
    Activate(FileEntry),
    /// 右键菜单选了针对某个条目的动作。
    Action { entry: FileEntry, action: FileAction },
    /// 右键菜单选了不针对条目的动作（目前只有「新建目录」）。
    PaneAction(FileAction),
    /// 外部文件被拖到面板上（Finder 拖拽释放）。只可能来自远端面板。
    DropFiles(Vec<std::path::PathBuf>),
}

impl EventEmitter<FilePaneEvent> for FilePane {}

pub struct FilePane {
    side: PaneSide,
    path: String,
    loading: bool,
    error: Option<SharedString>,
    /// 是否处于 Mock 模式：true 时 `reload` 重新生成假数据，不碰磁盘也不发指令。
    mock: bool,
    /// 是否已连接。决定右键菜单里的动作是否可点。
    online: bool,
    /// 是否显示以 `.` 开头的隐藏文件。只影响渲染，不重新读盘。
    show_hidden: bool,
    /// 正在被拖到本面板上的外部文件。`None` 表示没有拖拽悬停。
    drop_hover: Option<Vec<std::path::PathBuf>>,
    table: Entity<TableState<FileTable>>,
    _subscription: Subscription,
}

impl FilePane {
    /// 真实数据源：本地面板立刻读磁盘，远端面板等待上层喂 `set_entries`。
    pub fn new(side: PaneSide, path: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(side, path, false, window, cx)
    }

    /// Mock 数据源：骨架阶段用来把布局填满，连接成功后由 `use_live_data` 切换。
    pub fn new_mock(
        side: PaneSide,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(side, path, true, window, cx)
    }

    fn build(
        side: PaneSide,
        path: String,
        mock: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // 表格的右键菜单要回调面板，只能拿弱引用 —— 强引用会和 `table` 形成环。
        let weak = cx.weak_entity();
        // 面板和 delegate 各存一份开关，构造时从这里取，避免两处默认值走偏。
        let show_hidden = false;

        let table = cx.new(|cx| {
            TableState::new(
                FileTable {
                    all_entries: Vec::new(),
                    entries: Vec::new(),
                    show_hidden,
                    parent: None,
                    loading: false,
                    online: false,
                    side,
                    pane: weak,
                },
                window,
                cx,
            )
            .row_selectable(true)
            .col_selectable(false)
            .cell_selectable(false)
        });

        let subscription = cx.subscribe_in(&table, window, |pane, _table, event: &TableEvent, _window, cx| {
            if let TableEvent::DoubleClickedRow(row) = event {
                pane.activate_row(*row, cx);
            }
        });

        let mut pane = Self {
            side,
            path,
            loading: false,
            error: None,
            mock,
            online: false,
            show_hidden,
            drop_hover: None,
            table,
            _subscription: subscription,
        };

        if mock {
            pane.apply_mock(cx);
        } else if side == PaneSide::Local {
            pane.reload(cx);
        }

        pane
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    /// 当前可见的条目数（隐藏文件是否计入取决于 [`Self::show_hidden`]）。
    pub fn entries_count(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().entries.len()
    }

    /// 连接状态变化时由上层调用，用于启用/禁用右键菜单里的动作。
    pub fn set_online(&mut self, online: bool, cx: &mut Context<Self>) {
        if self.online == online {
            return;
        }
        self.online = online;
        self.table.update(cx, |state, _| {
            state.delegate_mut().online = online;
        });
        cx.notify();
    }

    /// 显示 / 隐藏以 `.` 开头的条目。
    ///
    /// 只重算可见列表，不重新读盘也不重新请求目录：隐藏文件一直躺在
    /// `all_entries` 里，切换是纯本地操作，远端面板也不用等一次往返。
    pub fn toggle_hidden(&mut self, cx: &mut Context<Self>) {
        let show_hidden = !self.show_hidden;
        self.show_hidden = show_hidden;
        self.table.update(cx, |state, cx| {
            state.delegate_mut().set_show_hidden(show_hidden);
            // 行号会整体平移，留着旧选中会指到另一个文件上。
            state.clear_selection(cx);
            state.refresh(cx);
        });
        cx.notify();
    }

    /// 上层把远端目录内容塞进来。
    pub fn set_entries(&mut self, path: String, entries: Vec<FileEntry>, cx: &mut Context<Self>) {
        self.path = path;
        self.loading = false;
        self.error = None;
        let parent = self.parent_entry();
        self.table.update(cx, |state, cx| {
            state.delegate_mut().set_entries(entries);
            state.delegate_mut().parent = parent;
            state.delegate_mut().loading = false;
            state.refresh(cx);
        });
        cx.notify();
    }

    pub fn begin_loading(&mut self, path: String, cx: &mut Context<Self>) {
        self.path = path;
        self.set_loading(true, cx);
    }

    pub fn set_error(&mut self, message: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.loading = false;
        self.error = Some(message.into());
        cx.notify();
    }

    fn set_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        self.loading = loading;
        self.table.update(cx, |state, cx| {
            state.delegate_mut().loading = loading;
            state.refresh(cx);
        });
        cx.notify();
    }

    /// 从 Mock 切换到真实数据源（连接成功后调用）。
    pub fn use_live_data(&mut self, cx: &mut Context<Self>) {
        self.mock = false;
        self.reload(cx);
    }

    /// 只切模式、不重新加载。远端面板的目录由上层主动请求，
    /// 这样调用方（比如 `Workspace::go_live`）在持有父实体借用时不会被同步回调打断。
    pub fn leave_mock(&mut self, cx: &mut Context<Self>) {
        if self.mock {
            self.mock = false;
            cx.notify();
        }
    }

    /// 用假数据填充表格，路径保持不变。
    fn apply_mock(&mut self, cx: &mut Context<Self>) {
        let entries = match self.side {
            PaneSide::Local => crate::ui::mock::local_entries(&self.path),
            PaneSide::Remote => crate::ui::mock::remote_entries(&self.path),
        };
        self.loading = false;
        self.error = None;
        let parent = self.parent_entry();
        self.table.update(cx, |state, cx| {
            state.delegate_mut().set_entries(entries);
            state.delegate_mut().parent = parent;
            state.delegate_mut().loading = false;
            state.refresh(cx);
        });
        cx.notify();
    }

    /// 本地面板读磁盘；远端面板交给上层发指令。Mock 模式下重新生成假数据。
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.mock {
            self.apply_mock(cx);
            return;
        }

        match self.side {
            PaneSide::Remote => {
                cx.emit(FilePaneEvent::Navigate(self.path.clone()));
            }
            PaneSide::Local => {
                let path = PathBuf::from(&self.path);
                self.set_loading(true, cx);

                let task = cx
                    .background_executor()
                    .spawn(async move { read_local_dir(&path) });

                cx.spawn(async move |this, cx| {
                    let result = task.await;
                    let _ = this.update(cx, |pane, cx| {
                        // 期间可能已经切到 Mock 模式，迟到的磁盘结果直接丢弃。
                        if pane.mock {
                            return;
                        }
                        pane.loading = false;
                        let parent = pane.parent_entry();
                        pane.table.update(cx, |state, cx| {
                            state.delegate_mut().loading = false;
                            state.delegate_mut().parent = parent;
                            if let Ok(entries) = &result {
                                state.delegate_mut().set_entries(entries.clone());
                            }
                            state.refresh(cx);
                        });
                        if let Err(message) = result {
                            pane.error = Some(message.into());
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
        }
    }

    pub fn navigate(&mut self, path: String, cx: &mut Context<Self>) {
        if self.mock {
            self.path = path;
            self.reload(cx);
            return;
        }

        match self.side {
            PaneSide::Local => {
                self.path = path;
                self.reload(cx);
            }
            PaneSide::Remote => {
                // 先进加载态再发事件：上层收到 `Navigate` 时只发指令，
                // 不再回头改本实体（那会撞上重入借用）。
                self.begin_loading(path.clone(), cx);
                cx.emit(FilePaneEvent::Navigate(path));
            }
        }
    }

    pub fn go_up(&mut self, cx: &mut Context<Self>) {
        let parent = match self.side {
            PaneSide::Local => crate::sftp::session::local_parent(Path::new(&self.path))
                .map(|path| path.to_string_lossy().to_string()),
            PaneSide::Remote => Some(remote_parent(&self.path)),
        };
        if let Some(parent) = parent
            && parent != self.path
        {
            self.navigate(parent, cx);
        }
    }

    fn activate_row(&mut self, row: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.table.read(cx).delegate().entry_at(row) else {
            return;
        };

        if is_parent_row(&entry) {
            self.go_up(cx);
        } else if entry.is_dir() {
            self.navigate(entry.path.clone(), cx);
        } else {
            cx.emit(FilePaneEvent::Activate(entry));
        }
    }

    /// 右键菜单选中后由表格回调进来。
    ///
    /// 刷新是本面板自己的事（本地面板重读磁盘、远端面板重发一次 `Navigate`），
    /// 所以在这里就地处理；其余动作都交给上层翻译成网络指令。
    fn request(&mut self, action: FileAction, entry: FileEntry, cx: &mut Context<Self>) {
        match action {
            FileAction::Refresh => self.reload(cx),
            FileAction::NewFolder => cx.emit(FilePaneEvent::PaneAction(action)),
            FileAction::Upload | FileAction::Download | FileAction::Rename | FileAction::Delete => {
                cx.emit(FilePaneEvent::Action { entry, action });
            }
        }
    }

    /// 构造 `..` 行；已经在根目录时返回 `None`。
    fn parent_entry(&self) -> Option<FileEntry> {
        let parent = match self.side {
            PaneSide::Local => {
                crate::sftp::session::local_parent(Path::new(&self.path)).map(|p| p.to_string_lossy().to_string())
            }
            PaneSide::Remote => Some(remote_parent(&self.path)),
        }?;

        if parent == self.path {
            return None;
        }

        Some(FileEntry {
            name: PARENT_ROW_NAME.to_string(),
            path: parent,
            kind: EntryKind::Directory,
            size: 0,
            modified: None,
            mode: None,
        })
    }

    /// 面包屑：(显示名, 目标路径)。
    fn crumbs(&self) -> Vec<(String, String)> {
        let mut items: Vec<(String, String)> = Vec::new();

        match self.side {
            PaneSide::Local => {
                let mut acc = PathBuf::new();
                for component in Path::new(&self.path).components() {
                    acc.push(component);
                    let label = match component {
                        std::path::Component::RootDir => "/".to_string(),
                        other => other.as_os_str().to_string_lossy().to_string(),
                    };
                    items.push((label, acc.to_string_lossy().to_string()));
                }
            }
            PaneSide::Remote => {
                items.push(("/".to_string(), "/".to_string()));
                let mut acc = String::new();
                for segment in self.path.split('/').filter(|s| !s.is_empty()) {
                    acc.push('/');
                    acc.push_str(segment);
                    items.push((segment.to_string(), acc.clone()));
                }
            }
        }

        items
    }

    /// 外部文件拖到面板上时：记住路径并刷新（让高亮边框亮起来）。
    ///
    /// `set_drop_hover` 是幂等的，拖动过程中会来很多次。
    pub fn set_drop_hover(&mut self, paths: Vec<std::path::PathBuf>, cx: &mut Context<Self>) {
        self.drop_hover = Some(paths);
        cx.notify();
    }

    /// 松开鼠标：真正抛 `DropFiles` 给上层。
    ///
    /// 只有「先看到文件拖进来、再看到鼠标抬起」才算一次拖放。另外还要确认此刻
    /// 确实有拖拽在进行（`active_drag` 只在抬起之后才被清空）：否则上一次拖拽
    /// 移开留下的悬停状态，会让随后的一次普通点击误触发上传。
    pub fn commit_drop(&mut self, cx: &mut Context<Self>) {
        let paths = self.drop_hover.take();
        if cx.has_active_drag()
            && let Some(paths) = paths
        {
            cx.emit(FilePaneEvent::DropFiles(paths));
        }
        cx.notify();
    }

    /// 拖拽移开（没松手）：丢掉悬停状态。
    ///
    /// 目前不需要外部调用 —— 渲染时会用 `active_drag` 再确认一次，
    /// 拖拽结束高亮自然消失。留着这个入口是为了将来做「拖到面板外」的动画。
    #[allow(dead_code)]
    pub fn clear_drop_hover(&mut self, cx: &mut Context<Self>) {
        if self.drop_hover.take().is_some() {
            cx.notify();
        }
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let weak = cx.entity().downgrade();
        let crumb_count = self.crumbs().len();

        let mut breadcrumb = Breadcrumb::new();
        for (ix, (label, target)) in self.crumbs().into_iter().enumerate() {
            let weak = weak.clone();
            let is_last = ix + 1 == crumb_count;
            breadcrumb = breadcrumb.child(
                BreadcrumbItem::new(label)
                    .disabled(is_last)
                    .on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |pane, cx| pane.navigate(target.clone(), cx));
                    }),
            );
        }

        let count = self.entries_count(cx);
        let count_label = if self.loading {
            "加载中…".to_string()
        } else {
            format!("{count} 项")
        };

        h_flex()
            .w_full()
            .justify_between()
            .gap_2()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.35))
            .child(
                h_flex()
                    .gap_1p5()
                    .flex_1()
                    .min_w_0()
                    .child(match self.side {
                        PaneSide::Local => IconName::HardDrive,
                        PaneSide::Remote => IconName::Server,
                    })
                    .child(div().flex_1().min_w_0().overflow_hidden().child(breadcrumb)),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_shrink_0()
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(count_label)),
                    )
                    .when(self.mock, |this| {
                        this.child(
                            div()
                                .px_1p5()
                                .py_0p5()
                                .rounded_md()
                                .border_1()
                                .border_color(theme.border)
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child("示例数据"),
                        )
                    })
                    .child(
                        Button::new((
                            "pane-hidden",
                            if self.side == PaneSide::Remote { 1usize } else { 0 },
                        ))
                        .ghost()
                        .xsmall()
                        .icon(if self.show_hidden {
                            IconName::Eye
                        } else {
                            IconName::EyeOff
                        })
                        .selected(self.show_hidden)
                        .toggled(self.show_hidden)
                        .tooltip(if self.show_hidden {
                            "不显示隐藏文件"
                        } else {
                            "显示隐藏文件"
                        })
                        .on_click({
                            let weak = weak.clone();
                            move |_, _, cx| {
                                let _ = weak.update(cx, |pane, cx| pane.toggle_hidden(cx));
                            }
                        }),
                    )
                    .child(
                        Button::new((
                            "pane-up",
                            if self.side == PaneSide::Remote { 1usize } else { 0 },
                        ))
                        .ghost()
                            .xsmall()
                            .icon(IconName::ArrowUp)
                            .tooltip("上级目录")
                            .on_click({
                                let weak = weak.clone();
                                move |_, _, cx| {
                                    let _ = weak.update(cx, |pane, cx| pane.go_up(cx));
                                }
                            }),
                    )
                    .child(
                        Button::new((
                            "pane-refresh",
                            if self.side == PaneSide::Remote { 1usize } else { 0 },
                        ))
                        .ghost()
                            .xsmall()
                            .icon(IconName::RefreshCw)
                            .tooltip("刷新")
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |pane, cx| pane.reload(cx));
                            }),
                    ),
            )
    }
}

impl Render for FilePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // 拖拽结束（Esc 取消、拖到别处）后 `active_drag` 会被清空，高亮跟着消失。
        let dropping = self.drop_hover.is_some() && cx.has_active_drag();
        let pane_ix = match self.side {
            PaneSide::Local => 0usize,
            PaneSide::Remote => 1,
        };
        let weak = cx.entity().downgrade();

        v_flex()
            .size_full()
            .min_w_0()
            .bg(theme.background)
            .id(("pane-root", pane_ix))
            .child(self.render_header(cx))
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .when_some(self.error.clone(), |this, error| {
                        this.child(
                            h_flex()
                                .gap_1p5()
                                .px_3()
                                .py_2()
                                .text_sm()
                                .text_color(theme.danger)
                                .child(IconName::CircleAlert)
                                .child(error),
                        )
                    })
                    .child(div().flex_1().min_h_0().child(DataTable::new(&self.table))),
            )
            // 拖放目标：只有远端面板接收外部文件。
            .when(self.side == PaneSide::Remote, |this| {
                let drag_weak = weak.clone();
                let drop_weak = weak.clone();
                this.on_drag_move::<ExternalPaths>(move |event, _window, cx| {
                    let paths = event.drag(cx).paths().to_vec();
                    let _ = drag_weak.update(cx, |pane, cx| pane.set_drop_hover(paths, cx));
                })
                .on_mouse_up(MouseButton::Left, move |_, _window, cx| {
                    let _ = drop_weak.update(cx, |pane, cx| pane.commit_drop(cx));
                })
                    // 悬停时给一个明显的高亮 + 提示，让用户知道松手会上传到哪里。
                    .when(dropping, |this| {
                        this.border_2()
                            .border_color(theme.primary)
                            .child(
                                div()
                                    .absolute()
                                    .inset_0()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .bg(theme.primary.opacity(0.06))
                                    .child(
                                        h_flex()
                                            .gap_2()
                                            .px_3()
                                            .py_1p5()
                                            .rounded_md()
                                            .bg(theme.background)
                                            .border_1()
                                            .border_color(theme.primary)
                                            .text_sm()
                                            .child(IconName::Upload)
                                            .child(SharedString::from(format!(
                                                "松手上传到 {}",
                                                self.path
                                            ))),
                                    ),
                            )
                    })
            })
    }
}

// ---------------------------------------------------------------------------
// Table delegate
// ---------------------------------------------------------------------------

pub struct FileTable {
    /// 目录里的全部条目（含隐藏文件）；行号索引的是下面那个可见列表。
    all_entries: Vec<FileEntry>,
    /// 当前可见的条目：`show_hidden` 为假时滤掉 `.` 开头的名字。
    entries: Vec<FileEntry>,
    /// 是否把隐藏文件也列出来。
    show_hidden: bool,
    /// `..` 行；根目录时为 `None`。
    parent: Option<FileEntry>,
    loading: bool,
    online: bool,
    side: PaneSide,
    pane: WeakEntity<FilePane>,
}

impl FileTable {
    fn icon_for(entry: &FileEntry) -> IconName {
        if is_parent_row(entry) {
            return IconName::CornerLeftUp;
        }
        match entry.kind {
            EntryKind::Directory => IconName::Folder,
            EntryKind::Symlink => IconName::Link,
            EntryKind::File => IconName::File,
        }
    }

    /// 行号 → 条目。第 0 行可能是 `..`。
    fn entry_at(&self, row_ix: usize) -> Option<FileEntry> {
        row_entry(self.parent.as_ref(), &self.entries, row_ix)
    }

    /// 换一批条目：先记住全部，再按当前的开关算出可见列表。
    fn set_entries(&mut self, entries: Vec<FileEntry>) {
        self.all_entries = entries;
        self.apply_visibility();
    }

    /// 按 `show_hidden` 重算可见列表，顺序沿用 `sort_entries` 的结果。
    fn apply_visibility(&mut self) {
        self.entries = filter_entries(&self.all_entries, self.show_hidden);
    }

    /// 打开 / 关闭隐藏文件，并立刻重算可见列表。
    fn set_show_hidden(&mut self, show_hidden: bool) {
        self.show_hidden = show_hidden;
        self.apply_visibility();
    }

    /// 右键菜单项。`entry` 由调用方克隆进来，点击时闭包自己再克隆一份。
    fn action_item(pane: &WeakEntity<FilePane>, spec: MenuSpec, entry: FileEntry) -> PopupMenuItem {
        let pane = pane.clone();
        let action = spec.action;
        PopupMenuItem::new(spec.label)
            .icon(spec.icon)
            .disabled(spec.disabled)
            .on_click(move |_, _, cx| {
                let entry = entry.clone();
                let _ = pane.update(cx, |pane, cx| pane.request(action, entry, cx));
            })
    }
}

/// 右键菜单里的一项，纯数据描述。
///
/// 把「菜单长什么样」和「`PopupMenu` 怎么拼」拆开，菜单内容就能直接单测 ——
/// 不必起窗口、也不必模拟右键点击。
#[derive(Clone, Debug)]
pub struct MenuSpec {
    pub label: &'static str,
    pub icon: IconName,
    pub action: FileAction,
    pub disabled: bool,
    /// 在这一项之前画一条分隔线。
    pub separator_before: bool,
}

/// 某一侧、某个条目该有哪些右键菜单项。
///
/// 这里承接了原先工具栏上那一排按钮（上传 / 下载 / 新建目录 / 删除 / 刷新）：
///
/// * 本地：`上传到当前远程目录`（目录则是 `上传文件夹到当前远程目录`）+ `刷新列表`；
/// * 远端：`下载到当前本地目录`（目录则是 `下载文件夹到当前本地目录`）/
///   `新建目录…` / `重命名…` / `删除` / `刷新列表`。
///
/// 两侧的目录都能整棵递归传：worker 侧一边递归建目录 + 上传，另一边递归
/// 建本地目录 + 下载。未连接时网络动作置灰。
/// `..` 行不给条目级动作，但仍给面板级动作（新建目录 / 刷新）——
/// 空目录里右键就只剩这一行可点，不能让它变成死路。
pub fn menu_spec(side: PaneSide, online: bool, entry: &FileEntry) -> Vec<MenuSpec> {
    let offline = !online;
    let is_dir = entry.is_dir();
    let is_entry = !is_parent_row(entry);

    let refresh = MenuSpec {
        label: "刷新列表",
        icon: IconName::RefreshCw,
        action: FileAction::Refresh,
        disabled: false,
        separator_before: true,
    };

    match side {
        PaneSide::Local => {
            let mut items = Vec::new();
            if is_entry {
                items.push(MenuSpec {
                    // 目录的文案点明「文件夹」，免得以为只是「进目录再传」。
                    label: if is_dir {
                        "上传文件夹到当前远程目录"
                    } else {
                        "上传到当前远程目录"
                    },
                    icon: IconName::Upload,
                    action: FileAction::Upload,
                    disabled: offline,
                    separator_before: false,
                });
            }
            items.push(refresh);
            items
        }
        PaneSide::Remote => {
            let mut items = Vec::new();
            if is_entry {
                items.push(MenuSpec {
                    // 和本地那侧对称：目录的文案点明「文件夹」。
                    label: if is_dir {
                        "下载文件夹到当前本地目录"
                    } else {
                        "下载到当前本地目录"
                    },
                    icon: IconName::Download,
                    action: FileAction::Download,
                    disabled: offline,
                    separator_before: false,
                });
            }
            items.push(MenuSpec {
                label: "新建目录…",
                icon: IconName::FolderPlus,
                action: FileAction::NewFolder,
                disabled: offline,
                separator_before: is_entry,
            });
            if is_entry {
                items.push(MenuSpec {
                    label: "重命名…",
                    icon: IconName::Pencil,
                    action: FileAction::Rename,
                    disabled: offline,
                    separator_before: true,
                });
                items.push(MenuSpec {
                    label: "删除",
                    icon: IconName::Trash,
                    action: FileAction::Delete,
                    disabled: offline,
                    separator_before: false,
                });
            }
            items.push(refresh);
            items
        }
    }
}

fn is_parent_row(entry: &FileEntry) -> bool {
    entry.name == PARENT_ROW_NAME
}

/// 以 `.` 开头就是隐藏文件。列表里本来就没有 `.` / `..`，
/// 所以不必再给这两个名字开例外。
fn is_hidden_name(name: &str) -> bool {
    name.starts_with('.')
}

/// 按「是否显示隐藏文件」筛出一份可见列表。
///
/// 纯函数，方便单测 —— `FileTable` 里带着 `WeakEntity`，起不来。
fn filter_entries(entries: &[FileEntry], show_hidden: bool) -> Vec<FileEntry> {
    if show_hidden {
        return entries.to_vec();
    }
    entries
        .iter()
        .filter(|entry| !is_hidden_name(&entry.name))
        .cloned()
        .collect()
}

/// 行号 → 条目。有 `..` 行时数据整体后移一位。
fn row_entry(
    parent: Option<&FileEntry>,
    entries: &[FileEntry],
    row_ix: usize,
) -> Option<FileEntry> {
    if parent.is_some() {
        if row_ix == 0 {
            return parent.cloned();
        }
        return entries.get(row_ix - 1).cloned();
    }
    entries.get(row_ix).cloned()
}

impl TableDelegate for FileTable {
    fn columns_count(&self, _: &App) -> usize {
        4
    }

    fn rows_count(&self, _: &App) -> usize {
        self.entries.len() + usize::from(self.parent.is_some())
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("name", "名称").width(px(190.)).min_width(px(120.)),
            1 => Column::new("size", "大小")
                .width(px(76.))
                .text_right()
                .resizable(false),
            2 => Column::new("mode", "权限").width(px(86.)).resizable(false),
            _ => Column::new("modified", "修改时间")
                .width(px(136.))
                .resizable(false),
        }
    }

    fn loading(&self, _: &App) -> bool {
        self.loading
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let entry = self.entry_at(row_ix);

        let mut cell = h_flex().w_full().items_center().gap_2();

        if let Some(entry) = entry {
            let parent_row = is_parent_row(&entry);
            match col_ix {
                0 => {
                    cell = cell.child(Self::icon_for(&entry)).child(
                        div()
                            .truncate()
                            .when(parent_row, |this| {
                                this.text_color(theme.muted_foreground)
                            })
                            .child(SharedString::from(entry.name.clone())),
                    );
                }
                1 => {
                    let label = if entry.is_dir() {
                        String::new()
                    } else {
                        format_size(entry.size)
                    };
                    cell = cell
                        .justify_end()
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(label));
                }
                2 => {
                    cell = cell
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(if parent_row {
                            String::new()
                        } else {
                            format_mode(entry.mode)
                        }));
                }
                _ => {
                    cell = cell
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(if parent_row {
                            String::new()
                        } else {
                            format_time(entry.modified)
                        }));
                }
            }
        }

        cell.into_any_element()
    }

    /// 右键菜单：本地给「上传 / 刷新」，远端给「下载 / 新建目录 / 重命名 / 删除 / 刷新」。
    ///
    /// 内容由 [`menu_spec`] 决定（那部分是纯函数，可以单测），这里只负责拼装。
    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(entry) = self.entry_at(row_ix) else {
            return menu;
        };

        let mut menu = menu;
        for spec in menu_spec(self.side, self.online, &entry) {
            if spec.separator_before {
                menu = menu.separator();
            }
            menu = menu.item(Self::action_item(&self.pane, spec, entry.clone()));
        }
        menu
    }
}

// ---------------------------------------------------------------------------
// 本地目录读取
// ---------------------------------------------------------------------------

fn read_local_dir(dir: &Path) -> Result<Vec<FileEntry>, String> {
    let reader =
        std::fs::read_dir(dir).map_err(|err| format!("读取 {} 失败：{err}", dir.display()))?;

    let mut entries = Vec::new();
    for item in reader.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        let Ok(file_type) = item.file_type() else {
            continue;
        };
        // symlink 用 lstat，其余用普通 metadata。
        let meta = if file_type.is_symlink() {
            std::fs::symlink_metadata(item.path())
        } else {
            item.metadata()
        }
        .ok();

        let kind = if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_symlink() {
            EntryKind::Symlink
        } else {
            EntryKind::File
        };

        entries.push(FileEntry {
            path: item.path().to_string_lossy().to_string(),
            name,
            kind,
            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            mode: meta
                .as_ref()
                .map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions())),
        });
    }

    sort_entries(&mut entries);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    // `use super::*` 会把 `gpui_kit::*` 里的 `gpui_macros::test` 一起带进来，
    // 盖掉标准库的 `#[test]`，于是 `#[test]` 变成 gpui 的 UI 测试宏（需要
    // 异步上下文，且会重新解析函数体）。显式导入真正的 `test` 宏压过 glob。
    use ::core::prelude::v1::test;

    use super::*;

    fn entry(name: &str, kind: EntryKind) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            path: format!("/x/{name}"),
            kind,
            size: 0,
            modified: None,
            mode: None,
        }
    }

    /// 行号映射：有 `..` 时第 0 行是上级目录，数据整体后移一位。
    #[test]
    fn row_entry_offsets_rows_when_parent_row_present() {
        let entries = vec![entry("a", EntryKind::File), entry("b", EntryKind::Directory)];
        let parent = entry(PARENT_ROW_NAME, EntryKind::Directory);

        assert!(is_parent_row(
            &row_entry(Some(&parent), &entries, 0).expect("第 0 行")
        ));
        assert_eq!(row_entry(Some(&parent), &entries, 1).expect("第 1 行").name, "a");
        assert_eq!(row_entry(Some(&parent), &entries, 2).expect("第 2 行").name, "b");
        assert!(row_entry(Some(&parent), &entries, 3).is_none());
    }

    /// 根目录没有 `..` 行，行号与数据一一对应。
    #[test]
    fn row_entry_without_parent_is_identity() {
        let entries = vec![entry("a", EntryKind::File)];
        assert_eq!(row_entry(None, &entries, 0).expect("第 0 行").name, "a");
        assert!(row_entry(None, &entries, 1).is_none());
    }

    /// 本地文件：上传 + 刷新列表，连接后才可上传。
    #[test]
    fn local_menu_offers_upload_and_refresh() {
        let file = entry("a.txt", EntryKind::File);

        let online = menu_spec(PaneSide::Local, true, &file);
        let labels: Vec<_> = online.iter().map(|item| item.label).collect();
        assert_eq!(labels, ["上传到当前远程目录", "刷新列表"]);
        assert_eq!(online[0].action, FileAction::Upload);
        assert!(!online[0].disabled);
        assert!(!online[1].disabled, "刷新不需要连接");

        let offline = menu_spec(PaneSide::Local, false, &file);
        assert!(offline[0].disabled, "未连接时上传应置灰");
        assert!(!offline[1].disabled, "未连接也能刷新本地列表");
    }

    /// 远端文件：下载 / 新建目录 / 重命名 / 删除 / 刷新，分段清晰。
    #[test]
    fn remote_menu_lists_download_new_folder_rename_delete_refresh() {
        let file = entry("a.txt", EntryKind::File);
        let spec = menu_spec(PaneSide::Remote, true, &file);

        let labels: Vec<_> = spec.iter().map(|item| item.label).collect();
        assert_eq!(
            labels,
            ["下载到当前本地目录", "新建目录…", "重命名…", "删除", "刷新列表"]
        );

        let actions: Vec<_> = spec.iter().map(|item| item.action).collect();
        assert_eq!(
            actions,
            [
                FileAction::Download,
                FileAction::NewFolder,
                FileAction::Rename,
                FileAction::Delete,
                FileAction::Refresh,
            ]
        );

        assert!(spec.iter().all(|item| !item.disabled));
        assert_eq!(
            spec.iter().filter(|item| item.separator_before).count(),
            3,
            "新建目录前、重命名前、刷新前各一条分隔线"
        );
    }

    /// 两侧的目录都能整棵传：远端可整目录下载，本地可整目录上传。
    #[test]
    fn directories_can_be_transferred_whole_on_both_sides() {
        let dir = entry("sub", EntryKind::Directory);

        let remote = menu_spec(PaneSide::Remote, true, &dir);
        assert!(!remote[0].disabled, "目录可以整棵下载");
        assert_eq!(remote[0].action, FileAction::Download);
        assert_eq!(remote[0].label, "下载文件夹到当前本地目录");
        assert!(!remote[1].disabled, "新建目录与选中项无关");
        assert!(!remote[2].disabled, "目录可以重命名");
        assert!(!remote[3].disabled, "目录可以删除");

        let local = menu_spec(PaneSide::Local, true, &dir);
        assert!(!local[0].disabled, "目录可以整棵上传");
        assert_eq!(local[0].action, FileAction::Upload);
        assert_eq!(local[0].label, "上传文件夹到当前远程目录");

        // 未连接时依然要置灰：整目录传输也要过网络。
        assert!(menu_spec(PaneSide::Local, false, &dir)[0].disabled);
        assert!(menu_spec(PaneSide::Remote, false, &dir)[0].disabled);
    }

    /// 默认隐藏 `.` 开头的条目，打开开关后全部回来，且顺序不变。
    #[test]
    fn hidden_entries_follow_the_toggle() {
        let entries = vec![
            entry(".env", EntryKind::File),
            entry("app.log", EntryKind::File),
            entry("src", EntryKind::Directory),
        ];

        let hidden_off = filter_entries(&entries, false);
        let names: Vec<_> = hidden_off.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["app.log", "src"], "`.env` 应被滤掉");

        let hidden_on = filter_entries(&entries, true);
        assert_eq!(hidden_on.len(), 3, "打开开关后一条都不能少");
        assert_eq!(hidden_on[0].name, ".env", "顺序保持原样");
    }

    #[test]
    fn dot_prefix_is_the_only_hidden_rule() {
        assert!(is_hidden_name(".zshrc"));
        assert!(is_hidden_name("."));
        assert!(!is_hidden_name("zshrc"));
        assert!(!is_hidden_name("a.b"));
    }

    /// `..` 行不给条目级动作，但保留面板级动作（新建目录 / 刷新）——
    /// 空目录里只有这一行可右键，不能是死路。
    #[test]
    fn parent_row_only_offers_pane_actions() {
        let parent = entry(PARENT_ROW_NAME, EntryKind::Directory);

        let remote = menu_spec(PaneSide::Remote, true, &parent);
        let labels: Vec<_> = remote.iter().map(|item| item.label).collect();
        assert_eq!(labels, ["新建目录…", "刷新列表"]);
        assert!(!remote[0].separator_before, "第一项前不该有分隔线");
        assert!(remote[1].separator_before);

        let local = menu_spec(PaneSide::Local, true, &parent);
        let labels: Vec<_> = local.iter().map(|item| item.label).collect();
        assert_eq!(labels, ["刷新列表"]);
    }
}
