//! 单个文件面板：本地或远端，使用虚拟化的 `Table` 渲染。
//!
//! 面板本身不碰网络、也不碰另一端 —— 它只负责：
//! * 把当前目录渲染成四列表格（名称 / 大小 / 权限 / 修改时间）；
//! * 顶部面包屑 + 「上级」/「显示隐藏文件」/「刷新」按钮；
//! * 双击进入目录、双击文件抛 [`FilePaneEvent::Activate`]；
//! * 右键弹出 [`gpui_kit::component::menu::PopupMenu`]，把用户选的动作包成
//!   [`FilePaneEvent`] 交给上层（上传/下载/编辑/复制路径/新建目录/重命名/删除，刷新则就地处理）。
//!
//! 列表顶部还有一行 `..`，等价于「返回上一级」。

use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    rc::Rc,
};

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, InteractiveElementExt as _, Selectable as _, Sizable as _,
    breadcrumb::{Breadcrumb, BreadcrumbItem},
    button::{Button, ButtonVariants as _},
    h_flex,
    menu::PopupMenu,
    menu::PopupMenuItem,
    scroll::ScrollableMask,
    table::{Column, DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::sftp::session::{format_mode, format_size, format_time, remote_parent, sort_entries};
use crate::sftp::types::{EntryKind, FileEntry};

/// 列表顶部「返回上一级」那一行的名字。POSIX 不允许真实文件叫这个名字。
pub const PARENT_ROW_NAME: &str = "..";

const FILE_TABLE_COLUMNS: usize = 4;

fn default_column_widths() -> Vec<Pixels> {
    vec![px(190.), px(76.), px(86.), px(136.)]
}

fn remember_column_widths(saved: &RefCell<Vec<Pixels>>, widths: &[Pixels]) {
    // 只接受一整套宽度，避免组件版本或异常事件把列定义保存成残缺状态。
    if widths.len() == FILE_TABLE_COLUMNS {
        saved.replace(widths.to_vec());
    }
}

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
    /// 使用设置中的编辑方式打开文件；远端文件保存后自动同步。
    Edit,
    /// 把一个或多个绝对路径复制到系统剪贴板。
    CopyPath,
    /// 在系统文件管理器中显示本地条目。
    RevealInFileManager,
    Rename,
    Delete,
    /// 在当前面板目录下建目录。与右键落在哪一行无关。
    NewFolder,
    /// 重新读当前目录。纯面板本地动作，不惊动上层。
    Refresh,
}

#[derive(Clone, Debug)]
pub enum FilePaneEvent {
    /// 远端面板请求切换目录（本地面板自己处理，不发这个事件）。
    Navigate(String),
    /// 本地目录已经成功读取，上层可以安全地把它记为下次启动目录。
    DirectoryLoaded(String),
    /// 双击了一个文件：由上层按编辑器设置打开。
    Activate(FileEntry),
    /// 右键菜单选了条目动作；批量动作会一次携带当前多选集合。
    Action {
        entries: Vec<FileEntry>,
        action: FileAction,
    },
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
    local_reading: bool,
    local_request_seq: u64,
    error: Option<SharedString>,
    /// 是否已连接。决定右键菜单里的动作是否可点。
    online: bool,
    /// 是否显示以 `.` 开头的隐藏文件。只影响渲染，不重新读盘。
    show_hidden: bool,
    /// 正在被拖到本面板上的外部文件。`None` 表示没有拖拽悬停。
    drop_hover: Option<Vec<std::path::PathBuf>>,
    /// 当前帧里可见行的窗口坐标。框选时直接做矩形相交，不猜表头/滚动偏移。
    row_bounds: Rc<RefCell<BTreeMap<usize, Bounds<Pixels>>>>,
    /// 表格区域的窗口坐标，用于把全局鼠标坐标换成局部框选矩形。
    selection_surface: Rc<Cell<Option<Bounds<Pixels>>>>,
    marquee: Option<MarqueeSelection>,
    /// macOS 上 Control 不是 GPUI 的 secondary modifier，表格会额外合成一次普通单击。
    /// 这个标记让该次行事件只清理表格自身高亮，不覆盖我们维护的多选集合。
    preserve_selection_on_table_click: bool,
    /// 长路径地址栏的水平滚动状态。
    address_scroll: ScrollHandle,
    /// 切换目录后在下一帧将地址栏定位到末端，确保当前目录可见。
    reveal_address_end: bool,
    table: Entity<TableState<FileTable>>,
    _subscription: Subscription,
}

#[derive(Clone)]
struct MarqueeSelection {
    start: Point<Pixels>,
    current: Point<Pixels>,
    initial_rows: BTreeSet<usize>,
    start_row: Option<usize>,
    active: bool,
}

impl FilePane {
    /// 真实数据源：本地面板立刻读磁盘，远端面板等待上层喂 `set_entries`。
    pub fn new(side: PaneSide, path: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self::build(side, path, window, cx)
    }

    fn build(side: PaneSide, path: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // 表格的右键菜单要回调面板，只能拿弱引用 —— 强引用会和 `table` 形成环。
        let weak = cx.weak_entity();
        // 面板和 delegate 各存一份开关，构造时从这里取，避免两处默认值走偏。
        let show_hidden = false;
        let row_bounds = Rc::new(RefCell::new(BTreeMap::new()));
        let selection_surface = Rc::new(Cell::new(None));
        let column_widths = Rc::new(RefCell::new(default_column_widths()));

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
                    selected_rows: BTreeSet::new(),
                    selection_anchor: None,
                    row_bounds: row_bounds.clone(),
                    column_widths: column_widths.clone(),
                },
                window,
                cx,
            )
            .row_selectable(true)
            // Control / Command / Shift 由 delegate 的多选状态处理，表格自己的单选不能覆盖它。
            .select_on_modifier_click(false)
            .col_selectable(false)
            .cell_selectable(false)
        });

        let remembered_widths = column_widths.clone();
        let subscription = cx.subscribe_in(
            &table,
            window,
            move |pane, _table, event: &TableEvent, _window, cx| {
                match event {
                    TableEvent::DoubleClickedRow(row) => pane.activate_row(*row, cx),
                    // 键盘上下移动仍然能进入我们的多选状态；Control / Command / Shift
                    // 点击则在面板的 MouseDown 阶段接管。
                    TableEvent::SelectRow(row) => pane.select_single_row(*row, cx),
                    TableEvent::ColumnWidthsChanged(widths) => {
                        remember_column_widths(&remembered_widths, widths);
                    }
                    _ => {}
                }
            },
        );

        let mut pane = Self {
            side,
            path,
            loading: false,
            local_reading: false,
            local_request_seq: 0,
            error: None,
            online: false,
            show_hidden,
            drop_hover: None,
            row_bounds,
            selection_surface,
            marquee: None,
            preserve_selection_on_table_click: false,
            address_scroll: ScrollHandle::new(),
            reveal_address_end: true,
            table,
            _subscription: subscription,
        };

        if side == PaneSide::Local {
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

    pub fn is_loading(&self) -> bool {
        self.loading || self.local_reading
    }

    fn selected_count(&self, cx: &App) -> usize {
        self.table.read(cx).delegate().selected_rows.len()
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
            let delegate = state.delegate_mut();
            delegate.set_show_hidden(show_hidden);
            delegate.clear_selection();
            // 行号会整体平移，留着旧选中会指到另一个文件上。
            state.clear_selection(cx);
            state.refresh(cx);
        });
        self.row_bounds.borrow_mut().clear();
        cx.notify();
    }

    /// 上层把远端目录内容塞进来。
    pub fn set_entries(&mut self, path: String, entries: Vec<FileEntry>, cx: &mut Context<Self>) {
        let keep_selection = self.path == path;
        self.set_path(path);
        self.loading = false;
        self.error = None;
        let parent = self.parent_entry();
        self.table.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            let selected_paths = keep_selection.then(|| delegate.selected_paths());
            delegate.set_entries(entries);
            delegate.parent = parent;
            delegate.loading = false;
            match selected_paths {
                Some(paths) => delegate.restore_selection(&paths),
                None => delegate.clear_selection(),
            }
            state.clear_selection(cx);
            state.refresh(cx);
        });
        self.row_bounds.borrow_mut().clear();
        cx.notify();
    }

    pub fn begin_loading(&mut self, path: String, cx: &mut Context<Self>) {
        self.set_path(path);
        self.set_loading(true, cx);
    }

    fn set_path(&mut self, path: String) {
        if self.path != path {
            self.path = path;
            self.reveal_address_end = true;
        }
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

    /// 本地面板读磁盘；远端面板交给上层发指令。
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        match self.side {
            PaneSide::Remote => {
                cx.emit(FilePaneEvent::Navigate(self.path.clone()));
            }
            PaneSide::Local => self.reload_local(true, cx),
        }
    }

    pub fn refresh_silently(&mut self, cx: &mut Context<Self>) {
        if self.side == PaneSide::Local && !self.is_loading() {
            self.reload_local(false, cx);
        }
    }

    fn reload_local(&mut self, show_loading: bool, cx: &mut Context<Self>) {
        let path = PathBuf::from(&self.path);
        let requested_path = path.clone();
        self.local_request_seq = self.local_request_seq.wrapping_add(1);
        let request_id = self.local_request_seq;
        self.local_reading = true;
        if show_loading {
            self.set_loading(true, cx);
        }

        let task = cx
            .background_executor()
            .spawn(async move { read_local_dir(&path) });

        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |pane, cx| {
                // 期间可能已经导航到别处，迟到结果不能覆盖新目录。
                if pane.local_request_seq != request_id || Path::new(&pane.path) != requested_path {
                    return;
                }
                pane.loading = false;
                pane.local_reading = false;
                let parent = pane.parent_entry();
                pane.table.update(cx, |state, cx| {
                    let delegate = state.delegate_mut();
                    let selected_paths = delegate.selected_paths();
                    delegate.loading = false;
                    delegate.parent = parent;
                    if let Ok(entries) = &result {
                        delegate.set_entries(entries.clone());
                        delegate.restore_selection(&selected_paths);
                    }
                    state.clear_selection(cx);
                    state.refresh(cx);
                });
                pane.row_bounds.borrow_mut().clear();
                let loaded = result.is_ok();
                match result {
                    Ok(_) => pane.error = None,
                    Err(message) => pane.error = Some(message.into()),
                }
                if loaded {
                    cx.emit(FilePaneEvent::DirectoryLoaded(pane.path.clone()));
                }
                cx.notify();
            });
        })
        .detach();
    }

    pub fn navigate(&mut self, path: String, cx: &mut Context<Self>) {
        match self.side {
            PaneSide::Local => {
                self.set_path(path);
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

    fn select_single_row(&mut self, row: usize, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.preserve_selection_on_table_click) {
            // macOS 的 Control 点击不属于 TableState 认识的 modifier click，MouseUp
            // 会再发一个普通 SelectRow。多选已在 MouseDown 完成，这里只清原生高亮。
            self.marquee = None;
            self.table.update(cx, |state, cx| {
                state.clear_selection(cx);
                state.refresh(cx);
            });
            cx.notify();
            return;
        }
        if self.marquee.take().is_some_and(|marquee| marquee.active) {
            // 按下和松开落在同一行时，TableState 即使经过拖动仍会合成 click。
            // 保留框选集合，并清掉它刚加上的单选高亮。
            self.table.update(cx, |state, cx| {
                state.clear_selection(cx);
                state.refresh(cx);
            });
            cx.notify();
            return;
        }
        // 同一行内 MouseUp 会被 TableState 的 click 截住；它最终选中行时顺手
        // 清掉 MouseDown 建立的候选框选状态。
        self.marquee = None;
        self.table.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            delegate.selected_rows.clear();
            if delegate
                .entry_at(row)
                .is_some_and(|entry| !is_parent_row(&entry))
            {
                delegate.selected_rows.insert(row);
                delegate.selection_anchor = Some(row);
            } else {
                delegate.selection_anchor = None;
            }
            state.refresh(cx);
        });
        cx.notify();
    }

    fn click_row(&mut self, row: usize, toggle: bool, shift: bool, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            if delegate
                .entry_at(row)
                .is_none_or(|entry| is_parent_row(&entry))
            {
                delegate.clear_selection();
            } else if shift {
                let anchor = delegate.selection_anchor.unwrap_or(row);
                delegate.selected_rows = selectable_row_range(
                    anchor,
                    row,
                    delegate.rows_len(),
                    delegate.parent.is_some(),
                );
            } else if toggle {
                if !delegate.selected_rows.remove(&row) {
                    delegate.selected_rows.insert(row);
                }
                delegate.selection_anchor = Some(row);
            } else {
                delegate.selected_rows.clear();
                delegate.selected_rows.insert(row);
                delegate.selection_anchor = Some(row);
            }
            // 鼠标维护的是多选集合，旧的键盘单选高亮不能继续留在另一行。
            state.clear_selection(cx);
            state.refresh(cx);
        });
        cx.notify();
    }

    /// DataTable 的右键菜单只认识真实行；在捕获阶段把表格正文空白标记为一个
    /// 越界行，让它现有的菜单容器继续负责定位、聚焦与关闭。
    fn prepare_blank_context_menu(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(surface) = self.selection_surface.get() else {
            return;
        };
        if !surface.contains(&position) {
            return;
        }

        let visible = self.table.read(cx).visible_range().rows().clone();
        let bounds = self.row_bounds.borrow();
        if bounds
            .iter()
            .any(|(row, bounds)| visible.contains(row) && bounds.contains(&position))
        {
            return;
        }
        // 表头不是目录空白区；没有任何条目时使用组件默认的 32px 表头高度。
        let body_top = bounds
            .iter()
            .filter(|(row, _)| visible.contains(row))
            .map(|(_, bounds)| bounds.top())
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(surface.top() + px(32.));
        drop(bounds);
        if position.y < body_top {
            return;
        }

        let blank_row = self.table.read(cx).delegate().rows_len();
        self.table.update(cx, |state, cx| {
            state.set_right_clicked_row(Some(blank_row), cx);
        });
    }

    fn begin_marquee(
        &mut self,
        position: Point<Pixels>,
        modifiers: Modifiers,
        cx: &mut Context<Self>,
    ) {
        self.marquee = None;
        self.preserve_selection_on_table_click = false;
        let Some(surface) = self.selection_surface.get() else {
            return;
        };
        if !surface.contains(&position) {
            return;
        }

        // 表头不参与框选。第一条可见行的 top 最可靠；空目录则用组件默认 32px 表头。
        let body_top = self
            .row_bounds
            .borrow()
            .values()
            .map(Bounds::top)
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(surface.top() + px(32.));
        if position.y < body_top {
            return;
        }

        let visible = self.table.read(cx).visible_range().rows().clone();
        let start_row = self
            .row_bounds
            .borrow()
            .iter()
            .find(|(row, bounds)| visible.contains(row) && bounds.contains(&position))
            .map(|(row, _)| *row);
        let secondary = modifiers.secondary();
        let toggle = secondary || modifiers.control;
        if (toggle || modifiers.shift)
            && let Some(row) = start_row
        {
            // TableState 会吞掉带修饰键的 click；在 MouseDown 就完成多选，
            // 同时不进入框选状态，避免 MouseUp 被行事件截住后留下悬空手势。
            self.preserve_selection_on_table_click = toggle && !secondary && !modifiers.shift;
            self.click_row(row, toggle, modifiers.shift, cx);
            return;
        }
        let initial_rows = if toggle {
            self.table.read(cx).delegate().selected_rows.clone()
        } else {
            BTreeSet::new()
        };
        self.marquee = Some(MarqueeSelection {
            start: position,
            current: position,
            initial_rows,
            start_row,
            active: false,
        });
    }

    fn update_marquee(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(surface) = self.selection_surface.get() else {
            return;
        };
        let Some(marquee) = self.marquee.as_mut() else {
            return;
        };

        marquee.current = point(
            clamp_pixel(position.x, surface.left(), surface.right()),
            clamp_pixel(position.y, surface.top(), surface.bottom()),
        );
        let dx = (marquee.current.x - marquee.start.x).as_f32();
        let dy = (marquee.current.y - marquee.start.y).as_f32();
        let became_active = !marquee.active && dx * dx + dy * dy >= 16.0;
        if !marquee.active && !became_active {
            return;
        }
        marquee.active = true;

        let rect = marquee_bounds(marquee.start, marquee.current);
        let visible = self.table.read(cx).visible_range().rows().clone();
        let hit_rows: BTreeSet<usize> = self
            .row_bounds
            .borrow()
            .iter()
            .filter(|(row, bounds)| visible.contains(row) && rect.intersects(bounds))
            .map(|(row, _)| *row)
            .collect();
        let initial_rows = marquee.initial_rows.clone();

        self.table.update(cx, |state, cx| {
            let delegate = state.delegate_mut();
            delegate.selected_rows = initial_rows
                .into_iter()
                .chain(hit_rows)
                .filter(|row| {
                    delegate
                        .entry_at(*row)
                        .is_some_and(|entry| !is_parent_row(&entry))
                })
                .collect();
            if became_active {
                state.clear_selection(cx);
            }
            state.refresh(cx);
        });
        cx.notify();
    }

    fn end_marquee(&mut self, cx: &mut Context<Self>) {
        let Some(marquee) = self.marquee.take() else {
            return;
        };

        if !marquee.active && marquee.start_row.is_none() {
            self.table.update(cx, |state, cx| {
                state.delegate_mut().clear_selection();
                state.clear_selection(cx);
                state.refresh(cx);
            });
        }
        cx.notify();
    }

    /// 右键菜单选中后由表格回调进来。
    ///
    /// 刷新是本面板自己的事（本地面板重读磁盘、远端面板重发一次 `Navigate`），
    /// 所以在这里就地处理；其余动作都交给上层翻译成网络指令。
    fn request(&mut self, action: FileAction, entry: FileEntry, cx: &mut Context<Self>) {
        match action {
            FileAction::Refresh | FileAction::NewFolder => self.request_pane(action, cx),
            FileAction::Upload
            | FileAction::Download
            | FileAction::Edit
            | FileAction::CopyPath
            | FileAction::RevealInFileManager
            | FileAction::Rename
            | FileAction::Delete => {
                let entries = if matches!(
                    action,
                    FileAction::Upload
                        | FileAction::Download
                        | FileAction::CopyPath
                        | FileAction::Delete
                ) {
                    let selected = self.table.read(cx).delegate().selected_entries();
                    if selected.iter().any(|selected| selected.path == entry.path) {
                        selected
                    } else {
                        vec![entry]
                    }
                } else {
                    vec![entry]
                };
                cx.emit(FilePaneEvent::Action { entries, action });
            }
        }
    }

    fn request_pane(&mut self, action: FileAction, cx: &mut Context<Self>) {
        match action {
            FileAction::Refresh => self.reload(cx),
            FileAction::NewFolder => cx.emit(FilePaneEvent::PaneAction(action)),
            _ => {}
        }
    }

    /// 构造 `..` 行；已经在根目录时返回 `None`。
    fn parent_entry(&self) -> Option<FileEntry> {
        let parent = match self.side {
            PaneSide::Local => crate::sftp::session::local_parent(Path::new(&self.path))
                .map(|p| p.to_string_lossy().to_string()),
            PaneSide::Remote => Some(remote_parent(&self.path)),
        }?;

        if parent == self.path {
            return None;
        }

        Some(FileEntry {
            name: PARENT_ROW_NAME.to_string(),
            path: parent,
            kind: EntryKind::Directory,
            hidden: false,
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

    fn render_header(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let weak = cx.entity().downgrade();
        let crumb_count = self.crumbs().len();
        let pane_ix = match self.side {
            PaneSide::Local => 0usize,
            PaneSide::Remote => 1,
        };

        if std::mem::take(&mut self.reveal_address_end) {
            let scroll = self.address_scroll.clone();
            let weak = weak.clone();
            window.on_next_frame(move |_, cx| {
                scroll.set_offset(point(-scroll.max_offset().x, px(0.)));
                let _ = weak.update(cx, |_, cx| cx.notify());
            });
        }

        let mut breadcrumb = Breadcrumb::new();
        for (ix, (label, target)) in self.crumbs().into_iter().enumerate() {
            let weak = weak.clone();
            let is_last = ix + 1 == crumb_count;
            breadcrumb = breadcrumb.child(
                BreadcrumbItem::new(label)
                    .flex_none()
                    .whitespace_nowrap()
                    .disabled(is_last)
                    .on_click(move |_, _, cx| {
                        let _ = weak.update(cx, |pane, cx| pane.navigate(target.clone(), cx));
                    }),
            );
        }
        let breadcrumb = breadcrumb.flex_none().w_auto().min_w_full();

        let count = self.entries_count(cx);
        let selected = self.selected_count(cx);
        let count_label = if self.loading {
            "加载中…".to_string()
        } else if selected > 0 {
            format!("{count} 项 · 已选 {selected} 项")
        } else {
            format!("{count} 项")
        };
        let current_path = self.path.clone();
        let choose_directory = weak.clone();

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
                    .child(
                        div()
                            .id(("pane-address", pane_ix))
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .id(("pane-address-scroll", pane_ix))
                                    .size_full()
                                    .flex()
                                    .flex_row()
                                    .overflow_x_scroll()
                                    .lock_scroll_axis()
                                    .track_scroll(&self.address_scroll)
                                    .child(breadcrumb),
                            )
                            // 鼠标普通滚轮也可以横向查看长路径；触控板仍保留原生横向手势。
                            .child(ScrollableMask::vertical_wheel(&self.address_scroll)),
                    ),
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
                    .when(self.side == PaneSide::Local, |this| {
                        this.child(
                            Button::new("choose-local-directory")
                                .ghost()
                                .xsmall()
                                .icon(IconName::FolderOpen)
                                .tooltip("选择本地目录")
                                .on_click(move |_, _, cx| {
                                    let receiver = cx.prompt_for_paths(PathPromptOptions {
                                        files: false,
                                        directories: true,
                                        multiple: false,
                                        prompt: Some("选择本地目录".into()),
                                    });
                                    let pane = choose_directory.clone();
                                    cx.spawn(async move |cx| {
                                        if let Ok(Ok(Some(paths))) = receiver.await
                                            && let Some(path) = paths.into_iter().next()
                                        {
                                            let path = path.to_string_lossy().into_owned();
                                            let _ =
                                                pane.update(cx, |pane, cx| pane.navigate(path, cx));
                                        }
                                    })
                                    .detach();
                                }),
                        )
                    })
                    .child(
                        Button::new(("pane-copy-path", pane_ix))
                            .ghost()
                            .xsmall()
                            .icon(IconName::Copy)
                            .tooltip("复制当前路径")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    current_path.clone(),
                                ));
                            }),
                    )
                    .child(
                        Button::new((
                            "pane-hidden",
                            if self.side == PaneSide::Remote {
                                1usize
                            } else {
                                0
                            },
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
                            if self.side == PaneSide::Remote {
                                1usize
                            } else {
                                0
                            },
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
                            if self.side == PaneSide::Remote {
                                1usize
                            } else {
                                0
                            },
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // 拖拽结束（Esc 取消、拖到别处）后 `active_drag` 会被清空，高亮跟着消失。
        let dropping = self.drop_hover.is_some() && cx.has_active_drag();
        let pane_ix = match self.side {
            PaneSide::Local => 0usize,
            PaneSide::Remote => 1,
        };
        let weak = cx.entity().downgrade();
        let surface_bounds = self.selection_surface.clone();
        let marquee = self.marquee.clone();

        let selection_surface = {
            let down_weak = weak.clone();
            let move_weak = weak.clone();
            let up_weak = weak.clone();
            let context_weak = weak.clone();
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(DataTable::new(&self.table))
                .child(
                    canvas(
                        move |bounds, _, _| surface_bounds.set(Some(bounds)),
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
                .when_some(
                    marquee
                        .filter(|marquee| marquee.active)
                        .and_then(|marquee| self.selection_surface.get().map(|s| (marquee, s))),
                    |this, (marquee, surface)| {
                        let rect = marquee_bounds(marquee.start, marquee.current);
                        this.child(
                            div()
                                .absolute()
                                .left(rect.left() - surface.left())
                                .top(rect.top() - surface.top())
                                .w(rect.size.width)
                                .h(rect.size.height)
                                .border_1()
                                .border_color(theme.primary)
                                .bg(theme.primary.opacity(0.12)),
                        )
                    },
                )
                .on_mouse_down(MouseButton::Left, move |event, _, cx| {
                    let modifiers = event.modifiers;
                    let _ = down_weak.update(cx, |pane, cx| {
                        pane.begin_marquee(event.position, modifiers, cx)
                    });
                })
                .on_mouse_move(move |event, _, cx| {
                    if event.dragging() {
                        let _ = move_weak
                            .update(cx, |pane, cx| pane.update_marquee(event.position, cx));
                    }
                })
                .on_mouse_up(MouseButton::Left, move |_, _, cx| {
                    let _ = up_weak.update(cx, |pane, cx| pane.end_marquee(cx));
                })
                // 子级 DataTable 会在冒泡阶段截住右键；捕获阶段先辨认正文空白，
                // 这样有条目和完全空目录时都能稳定打开面板菜单。
                .capture_any_mouse_down(move |event, _, cx| {
                    if event.button == MouseButton::Right {
                        let _ = context_weak.update(cx, |pane, cx| {
                            pane.prepare_blank_context_menu(event.position, cx)
                        });
                    }
                })
        };

        v_flex()
            .size_full()
            .min_w_0()
            .bg(theme.background)
            .id(("pane-root", pane_ix))
            .child(self.render_header(window, cx))
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
                    .child(selection_surface),
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
                    this.border_2().border_color(theme.primary).child(
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
                                    .child(SharedString::from(format!("松手上传到 {}", self.path))),
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
    /// 当前可见的条目：`show_hidden` 为假时滤掉平台判定的隐藏项。
    entries: Vec<FileEntry>,
    /// 是否把隐藏文件也列出来。
    show_hidden: bool,
    /// `..` 行；根目录时为 `None`。
    parent: Option<FileEntry>,
    loading: bool,
    online: bool,
    side: PaneSide,
    pane: WeakEntity<FilePane>,
    selected_rows: BTreeSet<usize>,
    selection_anchor: Option<usize>,
    row_bounds: Rc<RefCell<BTreeMap<usize, Bounds<Pixels>>>>,
    /// 表格 `refresh()` 会重新读取 delegate 的列定义；这里保存用户拖拽后的宽度。
    column_widths: Rc<RefCell<Vec<Pixels>>>,
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

    fn rows_len(&self) -> usize {
        self.entries.len() + usize::from(self.parent.is_some())
    }

    fn clear_selection(&mut self) {
        self.selected_rows.clear();
        self.selection_anchor = None;
    }

    fn selected_paths(&self) -> BTreeSet<String> {
        self.selected_entries()
            .into_iter()
            .map(|entry| entry.path)
            .collect()
    }

    fn restore_selection(&mut self, paths: &BTreeSet<String>) {
        self.selected_rows = selected_rows_for_paths(&self.entries, self.parent.is_some(), paths);
        self.selection_anchor = self.selected_rows.iter().next().copied();
    }

    fn selected_entries(&self) -> Vec<FileEntry> {
        self.selected_rows
            .iter()
            .filter_map(|row| self.entry_at(*row))
            .filter(|entry| !is_parent_row(entry))
            .collect()
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

    /// 空白区域没有条目，只提供作用于当前目录的动作。
    fn pane_action_item(pane: &WeakEntity<FilePane>, spec: MenuSpec) -> PopupMenuItem {
        let pane = pane.clone();
        let action = spec.action;
        PopupMenuItem::new(spec.label)
            .icon(spec.icon)
            .disabled(spec.disabled)
            .on_click(move |_, _, cx| {
                let _ = pane.update(cx, |pane, cx| pane.request_pane(action, cx));
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

/// 面板空白区域的右键菜单：动作只针对当前目录，不借用任何文件条目。
pub fn pane_menu_spec(side: PaneSide, online: bool) -> Vec<MenuSpec> {
    vec![
        MenuSpec {
            label: "新建目录…",
            icon: IconName::FolderPlus,
            action: FileAction::NewFolder,
            disabled: side == PaneSide::Remote && !online,
            separator_before: false,
        },
        MenuSpec {
            label: "刷新列表",
            icon: IconName::RefreshCw,
            action: FileAction::Refresh,
            disabled: false,
            separator_before: true,
        },
    ]
}

/// 某一侧、某个条目该有哪些右键菜单项。
///
/// 这里承接了原先工具栏上那一排按钮（上传 / 下载 / 新建目录 / 删除 / 刷新）：
///
/// * 本地：上传 / 编辑 / 在文件管理器中显示 / 复制路径 / 新建目录 / 重命名 /
///   移到系统回收站 / 刷新；
/// * 远端：`下载到当前本地目录`（目录则是 `下载文件夹到当前本地目录`）/
///   编辑 / 复制路径 / 新建目录 / 重命名 / 永久删除 / 刷新。
///
/// 两侧的目录都能整棵递归传：worker 侧一边递归建目录 + 上传，另一边递归
/// 建本地目录 + 下载。未连接时网络动作置灰。
/// `..` 行不给条目级动作，但仍给面板级动作（新建目录 / 刷新）——
/// 空目录里右键就只剩这一行可点，不能让它变成死路。
pub fn menu_spec(side: PaneSide, online: bool, entry: &FileEntry) -> Vec<MenuSpec> {
    let offline = !online;
    let is_dir = entry.is_dir();
    let is_entry = !is_parent_row(entry);

    if !is_entry {
        return pane_menu_spec(side, online);
    }

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
                if !is_dir {
                    items.push(MenuSpec {
                        label: "编辑…",
                        icon: IconName::Pencil,
                        action: FileAction::Edit,
                        disabled: false,
                        separator_before: false,
                    });
                }
                items.push(MenuSpec {
                    label: "在文件管理器中显示",
                    icon: IconName::FolderOpen,
                    action: FileAction::RevealInFileManager,
                    disabled: false,
                    separator_before: true,
                });
                items.push(MenuSpec {
                    label: "复制路径",
                    icon: IconName::Copy,
                    action: FileAction::CopyPath,
                    disabled: false,
                    separator_before: false,
                });
            }
            items.push(MenuSpec {
                label: "新建目录…",
                icon: IconName::FolderPlus,
                action: FileAction::NewFolder,
                disabled: false,
                separator_before: is_entry,
            });
            if is_entry {
                items.push(MenuSpec {
                    label: "重命名…",
                    icon: IconName::FilePen,
                    action: FileAction::Rename,
                    disabled: false,
                    separator_before: false,
                });
                items.push(MenuSpec {
                    label: "移到系统回收站",
                    icon: IconName::Trash,
                    action: FileAction::Delete,
                    disabled: false,
                    separator_before: true,
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
                if !is_dir {
                    items.push(MenuSpec {
                        label: "编辑…",
                        icon: IconName::Pencil,
                        action: FileAction::Edit,
                        disabled: offline,
                        separator_before: false,
                    });
                }
                items.push(MenuSpec {
                    label: "复制路径",
                    icon: IconName::Copy,
                    action: FileAction::CopyPath,
                    disabled: false,
                    separator_before: true,
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
                    icon: IconName::FilePen,
                    action: FileAction::Rename,
                    disabled: offline,
                    separator_before: false,
                });
                items.push(MenuSpec {
                    label: "永久删除",
                    icon: IconName::Trash,
                    action: FileAction::Delete,
                    disabled: offline,
                    separator_before: true,
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
        .filter(|entry| !entry.hidden)
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
        let width = self
            .column_widths
            .borrow()
            .get(col_ix)
            .copied()
            .unwrap_or_else(|| default_column_widths()[col_ix.min(FILE_TABLE_COLUMNS - 1)]);
        match col_ix {
            0 => Column::new("name", "名称").width(width).min_width(px(120.)),
            1 => Column::new("size", "大小")
                .width(width)
                .text_right()
                .resizable(false),
            2 => Column::new("mode", "权限").width(width).resizable(false),
            _ => Column::new("modified", "修改时间")
                .width(width)
                .resizable(false),
        }
    }

    fn loading(&self, _: &App) -> bool {
        self.loading
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let row_bounds = self.row_bounds.clone();
        div()
            .id(("row", row_ix))
            .relative()
            .child(
                canvas(
                    move |bounds, _, _| {
                        row_bounds.borrow_mut().insert(row_ix, bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .when(self.selected_rows.contains(&row_ix), |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .bg(cx.theme().tokens.table_active),
                )
            })
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
                            .when(parent_row, |this| this.text_color(theme.muted_foreground))
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
        cx: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(entry) = self.entry_at(row_ix) else {
            self.clear_selection();
            cx.notify();
            let mut menu = menu;
            for spec in pane_menu_spec(self.side, self.online) {
                if spec.separator_before {
                    menu = menu.separator();
                }
                menu = menu.item(Self::pane_action_item(&self.pane, spec));
            }
            return menu;
        };

        // Finder 风格：右键已选条目保留整批；右键未选条目则只选它。
        if !self.selected_rows.contains(&row_ix) {
            self.clear_selection();
            if !is_parent_row(&entry) {
                self.selected_rows.insert(row_ix);
                self.selection_anchor = Some(row_ix);
            }
            cx.notify();
        }

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

fn clamp_pixel(value: Pixels, min: Pixels, max: Pixels) -> Pixels {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

fn marquee_bounds(a: Point<Pixels>, b: Point<Pixels>) -> Bounds<Pixels> {
    let left = if a.x < b.x { a.x } else { b.x };
    let top = if a.y < b.y { a.y } else { b.y };
    let right = if a.x > b.x { a.x } else { b.x };
    let bottom = if a.y > b.y { a.y } else { b.y };
    Bounds::new(point(left, top), size(right - left, bottom - top))
}

fn selectable_row_range(
    anchor: usize,
    row: usize,
    rows_len: usize,
    has_parent: bool,
) -> BTreeSet<usize> {
    let start = anchor.min(row);
    let end = anchor.max(row).min(rows_len.saturating_sub(1));
    (start..=end)
        .filter(|row| !(has_parent && *row == 0))
        .collect()
}

fn selected_rows_for_paths(
    entries: &[FileEntry],
    has_parent: bool,
    paths: &BTreeSet<String>,
) -> BTreeSet<usize> {
    let offset = usize::from(has_parent);
    entries
        .iter()
        .enumerate()
        .filter_map(|(row, entry)| paths.contains(&entry.path).then_some(row + offset))
        .collect()
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
            hidden: is_hidden_name(&name) || local_file_hidden(meta.as_ref()),
            name,
            kind,
            size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
            modified: meta.as_ref().and_then(|m| m.modified().ok()),
            mode: meta.as_ref().and_then(local_file_mode),
        });
    }

    sort_entries(&mut entries);
    Ok(entries)
}

#[cfg(unix)]
fn local_file_mode(metadata: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;

    Some(metadata.permissions().mode())
}

#[cfg(not(unix))]
fn local_file_mode(_metadata: &std::fs::Metadata) -> Option<u32> {
    None
}

#[cfg(target_os = "windows")]
fn local_file_hidden(metadata: Option<&std::fs::Metadata>) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    metadata.is_some_and(|metadata| windows_file_attributes_hidden(metadata.file_attributes()))
}

#[cfg(not(target_os = "windows"))]
fn local_file_hidden(_metadata: Option<&std::fs::Metadata>) -> bool {
    false
}

#[cfg(any(test, target_os = "windows"))]
fn windows_file_attributes_hidden(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    attributes & FILE_ATTRIBUTE_HIDDEN != 0
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
            hidden: name.starts_with('.'),
            size: 0,
            modified: None,
            mode: None,
        }
    }

    /// 行号映射：有 `..` 时第 0 行是上级目录，数据整体后移一位。
    #[test]
    fn row_entry_offsets_rows_when_parent_row_present() {
        let entries = vec![
            entry("a", EntryKind::File),
            entry("b", EntryKind::Directory),
        ];
        let parent = entry(PARENT_ROW_NAME, EntryKind::Directory);

        assert!(is_parent_row(
            &row_entry(Some(&parent), &entries, 0).expect("第 0 行")
        ));
        assert_eq!(
            row_entry(Some(&parent), &entries, 1).expect("第 1 行").name,
            "a"
        );
        assert_eq!(
            row_entry(Some(&parent), &entries, 2).expect("第 2 行").name,
            "b"
        );
        assert!(row_entry(Some(&parent), &entries, 3).is_none());
    }

    /// 根目录没有 `..` 行，行号与数据一一对应。
    #[test]
    fn row_entry_without_parent_is_identity() {
        let entries = vec![entry("a", EntryKind::File)];
        assert_eq!(row_entry(None, &entries, 0).expect("第 0 行").name, "a");
        assert!(row_entry(None, &entries, 1).is_none());
    }

    #[test]
    fn shift_range_never_selects_the_parent_row() {
        assert_eq!(
            selectable_row_range(3, 0, 5, true),
            BTreeSet::from([1, 2, 3])
        );
        assert_eq!(
            selectable_row_range(0, 2, 3, false),
            BTreeSet::from([0, 1, 2])
        );
    }

    #[test]
    fn refresh_restores_selection_by_path() {
        let entries = vec![
            entry("new.txt", EntryKind::File),
            entry("kept.txt", EntryKind::File),
        ];
        let selected = BTreeSet::from(["/x/kept.txt".to_string(), "/x/deleted.txt".to_string()]);

        assert_eq!(
            selected_rows_for_paths(&entries, true, &selected),
            BTreeSet::from([2])
        );
    }

    #[test]
    fn marquee_bounds_are_normalized_in_every_drag_direction() {
        let forward = marquee_bounds(point(px(10.), px(20.)), point(px(40.), px(70.)));
        let reverse = marquee_bounds(point(px(40.), px(70.)), point(px(10.), px(20.)));

        assert_eq!(forward, reverse);
        assert_eq!(forward.origin, point(px(10.), px(20.)));
        assert_eq!(forward.size, size(px(30.), px(50.)));
    }

    #[::gpui::test]
    fn resized_column_width_survives_table_refresh(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_kit::init);
        let (pane, cx) = cx.add_window_view(|window, cx| {
            let mut pane = FilePane::new(PaneSide::Remote, "/home".to_string(), window, cx);
            pane.set_entries(
                "/home".to_string(),
                vec![entry("one.txt", EntryKind::File)],
                cx,
            );
            pane
        });

        let table = pane.read_with(cx, |pane, _| pane.table.clone());
        table.update(cx, |_, cx| {
            // 等价于用户松开列宽拖拽柄时表格发出的事件。
            cx.emit(TableEvent::ColumnWidthsChanged(vec![
                px(360.),
                px(76.),
                px(86.),
                px(136.),
            ]));
        });
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));

        let blank = pane.read_with(cx, |pane, _| {
            let surface = pane.selection_surface.get().expect("表格区域已布局");
            let rows_bottom = pane
                .row_bounds
                .borrow()
                .values()
                .map(Bounds::bottom)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .expect("应有上级目录行");
            let position = point(surface.left() + px(20.), rows_bottom + px(20.));
            assert!(surface.contains(&position));
            position
        });
        cx.simulate_click(blank, Modifiers::default());

        table.read_with(cx, |state, _| {
            // 点击正文空白会清空选择并 refresh；拖拽后的宽度不能被默认值覆盖。
            assert_eq!(state.col_width(0), Some(px(360.)));
            assert_eq!(state.col_width(1), Some(px(76.)));
        });
    }

    #[::gpui::test]
    fn long_address_bar_scrolls_to_the_current_directory(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_kit::init);
        let path = format!(
            "/{}",
            (0..24)
                .map(|ix| format!("very-long-directory-{ix:02}"))
                .collect::<Vec<_>>()
                .join("/")
        );
        let window = cx.open_window(size(px(480.), px(320.)), |window, cx| {
            FilePane::new(PaneSide::Remote, path, window, cx)
        });
        let mut cx = gpui::VisualTestContext::from_window(window.into(), cx);
        let pane = window.root(&mut cx).expect("文件面板根视图");

        cx.update(|window, cx| window.draw(cx).clear(cx));
        cx.update(|window, cx| {
            window.simulate_next_frame(cx);
            window.draw(cx).clear(cx);
        });

        pane.read_with(&cx, |pane, _| {
            let max = pane.address_scroll.max_offset().x;
            assert!(max > px(0.), "长路径应产生水平滚动范围");
            assert_eq!(pane.address_scroll.offset().x, -max);
        });
    }

    #[::gpui::test]
    fn right_clicking_table_whitespace_opens_the_pane_menu(cx: &mut gpui::TestAppContext) {
        let new_folder_requested = Rc::new(Cell::new(false));
        cx.update(gpui_kit::init);
        let (pane, cx) = cx.add_window_view(|window, cx| {
            let mut pane = FilePane::new(PaneSide::Remote, "/home".to_string(), window, cx);
            pane.set_online(true, cx);
            pane.set_entries(
                "/home".to_string(),
                vec![entry("one.txt", EntryKind::File)],
                cx,
            );
            pane
        });
        cx.update(|_, cx| {
            let requested = new_folder_requested.clone();
            cx.subscribe(&pane, move |_, event: &FilePaneEvent, _| {
                if matches!(event, FilePaneEvent::PaneAction(FileAction::NewFolder)) {
                    requested.set(true);
                }
            })
            .detach();
        });
        cx.update(|window, cx| window.draw(cx).clear(cx));

        let press = pane.read_with(cx, |pane, _| {
            let surface = pane.selection_surface.get().expect("表格区域已布局");
            let rows = pane.row_bounds.borrow();
            let rows_bottom = rows
                .values()
                .map(Bounds::bottom)
                .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
                .expect("至少有上级目录和一个文件行");
            let position = point(surface.left() + px(20.), rows_bottom + px(20.));
            assert!(surface.contains(&position), "测试点必须落在表格空白区域");
            assert!(rows.values().all(|bounds| !bounds.contains(&position)));
            position
        });

        cx.simulate_mouse_down(press, MouseButton::Right, Modifiers::default());
        cx.simulate_mouse_up(press, MouseButton::Right, Modifiers::default());
        cx.run_until_parked();
        cx.update(|window, cx| window.draw(cx).clear(cx));

        pane.read_with(cx, |pane, cx| {
            let table = pane.table.read(cx);
            assert_eq!(
                table.right_clicked_row(),
                Some(table.delegate().rows_len()),
                "空白区应被标记为面板菜单，而不是丢掉右键"
            );
        });

        let first_item = point(press.x + px(30.), press.y + px(17.));
        cx.simulate_mouse_move(first_item, None, Modifiers::default());
        cx.simulate_click(first_item, Modifiers::default());
        cx.run_until_parked();
        assert!(new_folder_requested.get(), "面板菜单第一项应可点击");
    }

    #[::gpui::test]
    fn dragging_across_rows_selects_every_intersected_entry(cx: &mut gpui::TestAppContext) {
        let emitted = Rc::new(RefCell::new(Vec::<String>::new()));
        cx.update(gpui_kit::init);
        let (pane, cx) = cx.add_window_view(|window, cx| {
            let mut pane = FilePane::new(PaneSide::Remote, "/home".to_string(), window, cx);
            pane.set_entries(
                "/home".to_string(),
                vec![
                    entry("one", EntryKind::File),
                    entry("two", EntryKind::File),
                    entry("three", EntryKind::Directory),
                    entry("four", EntryKind::File),
                ],
                cx,
            );
            pane
        });
        cx.update(|_, cx| {
            let emitted = emitted.clone();
            cx.subscribe(&pane, move |_, event: &FilePaneEvent, _| {
                if let FilePaneEvent::Action {
                    entries,
                    action: FileAction::Download,
                } = event
                {
                    emitted
                        .borrow_mut()
                        .extend(entries.iter().map(|entry| entry.name.clone()));
                }
            })
            .detach();
        });

        let (first, third) = pane.read_with(cx, |pane, _| {
            let bounds = pane.row_bounds.borrow();
            let first = bounds.get(&1).copied().expect("第一个文件行已渲染");
            let third = bounds.get(&3).copied().expect("第三个文件行已渲染");
            (first, third)
        });
        let start = point(first.right() - px(8.), first.top() + px(8.));
        let end = point(third.left() + px(8.), third.bottom() - px(8.));

        cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(end, MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());

        pane.read_with(cx, |pane, cx| {
            let selected: Vec<_> = pane
                .table
                .read(cx)
                .delegate()
                .selected_entries()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            assert_eq!(selected, ["one", "two", "three"]);
            assert_eq!(pane.selected_count(cx), 3);
            assert!(pane.marquee.is_none(), "松手后框选矩形应消失");
        });

        let clicked = pane.read_with(cx, |pane, cx| {
            pane.table
                .read(cx)
                .delegate()
                .entry_at(2)
                .expect("框选内的条目")
        });
        pane.update(cx, |pane, cx| {
            pane.request(FileAction::Download, clicked, cx)
        });
        assert_eq!(*emitted.borrow(), ["one", "two", "three"]);
    }

    #[::gpui::test]
    fn delete_action_keeps_the_current_multi_selection(cx: &mut gpui::TestAppContext) {
        let emitted = Rc::new(RefCell::new(Vec::<String>::new()));
        cx.update(gpui_kit::init);
        let (pane, cx) = cx.add_window_view(|window, cx| {
            let mut pane = FilePane::new(PaneSide::Remote, "/home".to_string(), window, cx);
            pane.set_entries(
                "/home".to_string(),
                vec![
                    entry("one", EntryKind::File),
                    entry("two", EntryKind::File),
                    entry("three", EntryKind::Directory),
                ],
                cx,
            );
            pane
        });
        cx.update(|_, cx| {
            let emitted = emitted.clone();
            cx.subscribe(&pane, move |_, event: &FilePaneEvent, _| {
                if let FilePaneEvent::Action {
                    entries,
                    action: FileAction::Delete,
                } = event
                {
                    emitted
                        .borrow_mut()
                        .extend(entries.iter().map(|entry| entry.name.clone()));
                }
            })
            .detach();
        });

        let clicked = pane.update(cx, |pane, cx| {
            pane.table.update(cx, |state, cx| {
                state.delegate_mut().selected_rows = BTreeSet::from([1, 2, 3]);
                state.refresh(cx);
            });
            pane.table
                .read(cx)
                .delegate()
                .entry_at(2)
                .expect("多选内的条目")
        });
        pane.update(cx, |pane, cx| pane.request(FileAction::Delete, clicked, cx));

        assert_eq!(*emitted.borrow(), ["one", "two", "three"]);
    }

    #[::gpui::test]
    fn control_click_adds_and_removes_files_from_selection(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_kit::init);
        let (pane, cx) = cx.add_window_view(|window, cx| {
            let mut pane = FilePane::new(PaneSide::Remote, "/home".to_string(), window, cx);
            pane.set_entries(
                "/home".to_string(),
                vec![
                    entry("one", EntryKind::File),
                    entry("two", EntryKind::File),
                    entry("three", EntryKind::File),
                ],
                cx,
            );
            pane
        });

        let (first, third) = pane.read_with(cx, |pane, _| {
            let bounds = pane.row_bounds.borrow();
            (
                bounds.get(&1).copied().expect("第一个文件行已渲染"),
                bounds.get(&3).copied().expect("第三个文件行已渲染"),
            )
        });
        cx.simulate_click(first.center(), Modifiers::default());
        cx.simulate_mouse_down(third.center(), MouseButton::Left, Modifiers::control());
        pane.read_with(cx, |pane, _| {
            assert!(pane.marquee.is_none(), "Control 单击不应进入框选状态");
        });
        cx.simulate_mouse_up(third.center(), MouseButton::Left, Modifiers::control());
        pane.read_with(cx, |pane, cx| {
            let selected: Vec<_> = pane
                .table
                .read(cx)
                .delegate()
                .selected_entries()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            assert_eq!(selected, ["one", "three"]);
        });

        cx.simulate_click(first.center(), Modifiers::control());
        pane.read_with(cx, |pane, cx| {
            let selected: Vec<_> = pane
                .table
                .read(cx)
                .delegate()
                .selected_entries()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            assert_eq!(selected, ["three"], "Control 点击已选项应减选");
        });
    }

    #[::gpui::test]
    fn shift_click_selects_the_contiguous_row_range(cx: &mut gpui::TestAppContext) {
        cx.update(gpui_kit::init);
        let (pane, cx) = cx.add_window_view(|window, cx| {
            let mut pane = FilePane::new(PaneSide::Remote, "/home".to_string(), window, cx);
            pane.set_entries(
                "/home".to_string(),
                vec![
                    entry("one", EntryKind::File),
                    entry("two", EntryKind::File),
                    entry("three", EntryKind::File),
                ],
                cx,
            );
            pane
        });

        let (first, third) = pane.read_with(cx, |pane, _| {
            let bounds = pane.row_bounds.borrow();
            (
                bounds.get(&1).copied().expect("第一个文件行已渲染"),
                bounds.get(&3).copied().expect("第三个文件行已渲染"),
            )
        });
        cx.simulate_click(first.center(), Modifiers::default());
        cx.simulate_click(third.center(), Modifiers::shift());

        pane.read_with(cx, |pane, cx| {
            let selected: Vec<_> = pane
                .table
                .read(cx)
                .delegate()
                .selected_entries()
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            assert_eq!(selected, ["one", "two", "three"]);
        });
    }

    /// 本地文件具备完整的文件管理菜单；只有上传依赖远端连接。
    #[test]
    fn local_menu_offers_edit_reveal_copy_and_file_management() {
        let file = entry("a.txt", EntryKind::File);

        let online = menu_spec(PaneSide::Local, true, &file);
        let labels: Vec<_> = online.iter().map(|item| item.label).collect();
        assert_eq!(
            labels,
            [
                "上传到当前远程目录",
                "编辑…",
                "在文件管理器中显示",
                "复制路径",
                "新建目录…",
                "重命名…",
                "移到系统回收站",
                "刷新列表"
            ]
        );
        let actions: Vec<_> = online.iter().map(|item| item.action).collect();
        assert_eq!(
            actions,
            [
                FileAction::Upload,
                FileAction::Edit,
                FileAction::RevealInFileManager,
                FileAction::CopyPath,
                FileAction::NewFolder,
                FileAction::Rename,
                FileAction::Delete,
                FileAction::Refresh,
            ]
        );
        assert!(online.iter().all(|item| !item.disabled));

        let offline = menu_spec(PaneSide::Local, false, &file);
        assert!(offline[0].disabled, "未连接时上传应置灰");
        assert!(
            offline.iter().skip(1).all(|item| !item.disabled),
            "本地编辑和文件管理不应依赖远端连接"
        );
    }

    /// 远端文件：下载 / 编辑 / 复制路径 / 新建目录 / 重命名 / 永久删除 / 刷新。
    #[test]
    fn remote_menu_lists_copy_path_and_file_management_actions() {
        let file = entry("a.txt", EntryKind::File);
        let spec = menu_spec(PaneSide::Remote, true, &file);

        let labels: Vec<_> = spec.iter().map(|item| item.label).collect();
        assert_eq!(
            labels,
            [
                "下载到当前本地目录",
                "编辑…",
                "复制路径",
                "新建目录…",
                "重命名…",
                "永久删除",
                "刷新列表"
            ]
        );

        let actions: Vec<_> = spec.iter().map(|item| item.action).collect();
        assert_eq!(
            actions,
            [
                FileAction::Download,
                FileAction::Edit,
                FileAction::CopyPath,
                FileAction::NewFolder,
                FileAction::Rename,
                FileAction::Delete,
                FileAction::Refresh,
            ]
        );

        assert!(spec.iter().all(|item| !item.disabled));
        assert_eq!(
            spec.iter().filter(|item| item.separator_before).count(),
            4,
            "复制路径、新建目录、永久删除和刷新各自按用途分组"
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
        assert!(
            remote.iter().all(|item| item.action != FileAction::Edit),
            "目录不能用文本编辑器打开"
        );
        assert!(
            remote
                .iter()
                .find(|item| item.action == FileAction::NewFolder)
                .is_some_and(|item| !item.disabled),
            "新建目录与选中项无关"
        );
        assert!(
            remote
                .iter()
                .find(|item| item.action == FileAction::Rename)
                .is_some_and(|item| !item.disabled),
            "目录可以重命名"
        );
        assert!(
            remote
                .iter()
                .find(|item| item.action == FileAction::Delete)
                .is_some_and(|item| !item.disabled),
            "目录可以删除"
        );

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
    fn dot_prefix_is_hidden_on_every_platform() {
        assert!(is_hidden_name(".zshrc"));
        assert!(is_hidden_name("."));
        assert!(!is_hidden_name("zshrc"));
        assert!(!is_hidden_name("a.b"));
    }

    #[test]
    fn windows_hidden_attribute_is_respected() {
        assert!(windows_file_attributes_hidden(0x2));
        assert!(windows_file_attributes_hidden(0x2 | 0x4));
        assert!(!windows_file_attributes_hidden(0x1));

        let mut hidden = entry("desktop.ini", EntryKind::File);
        hidden.hidden = true;
        assert!(filter_entries(&[hidden.clone()], false).is_empty());
        assert_eq!(filter_entries(&[hidden], true).len(), 1);
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
        assert_eq!(labels, ["新建目录…", "刷新列表"]);
        assert!(!local[0].separator_before, "第一项前不该有分隔线");
        assert!(local[1].separator_before);
    }
}
