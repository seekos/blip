//! 底部传输队列：每个任务一行，带进度条、速率与取消按钮。

use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    progress::Progress,
    tooltip::Tooltip,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::sftp::session::format_size;
use crate::sftp::types::{TransferDirection, TransferId, TransferOutcome};

/// 人类可读的速率：`4.6 MB/s`。低于 1 B/s 时显示 `—`。
fn format_speed(bytes_per_second: f64) -> String {
    if !bytes_per_second.is_finite() || bytes_per_second < 1. {
        return "—".to_string();
    }
    format!("{}/s", format_size(bytes_per_second as u64))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransferStatus {
    Queued,
    /// 上传前发现冲突，正在等用户在弹窗里选「续传 / 覆盖 / 跳过」。
    Awaiting,
    Running,
    Done,
    Partial(String),
    /// 用户在冲突弹窗里选了跳过，一个字节都没传。
    Skipped,
    Failed(String),
    Cancelled,
}

pub struct TransferItem {
    pub id: TransferId,
    pub name: String,
    pub direction: TransferDirection,
    pub total: u64,
    pub transferred: u64,
    pub status: TransferStatus,
    /// worker 上报的实时速率（字节/秒），仅用于展示。
    speed: f64,
    /// 本次传输从哪个字节开始（0 = 从头，>0 = 断点续传）。
    resumed_from: u64,
    /// 流式目录上传在扫描完成前没有稳定总量，进度条应显示为不定进度。
    streaming_total: bool,
    /// 整目录并发上传时，当前真正写入远端的文件（最多为并发数）。
    active_files: Vec<String>,
}

impl TransferItem {
    fn ratio(&self) -> f32 {
        if self.total == 0 {
            return if matches!(self.status, TransferStatus::Done) {
                100.
            } else {
                0.
            };
        }
        (self.transferred as f64 / self.total as f64 * 100.).clamp(0., 100.) as f32
    }

    fn is_active(&self) -> bool {
        matches!(
            self.status,
            TransferStatus::Queued | TransferStatus::Awaiting | TransferStatus::Running
        )
    }
}

#[derive(Clone, Debug)]
pub enum TransferQueueEvent {
    Cancel(TransferId),
    /// 请求折叠/展开整个队列面板（由 `Workspace` 决定怎么变布局）。
    ToggleCollapsed,
}

impl EventEmitter<TransferQueueEvent> for TransferQueue {}

pub struct TransferQueue {
    items: Vec<TransferItem>,
    active: usize,
    scroll: UniformListScrollHandle,
    idle_sleep_guard: Option<ActivityGuard>,
    idle_sleep_request_pending: bool,
}

impl TransferQueue {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            active: 0,
            scroll: UniformListScrollHandle::new(),
            idle_sleep_guard: None,
            idle_sleep_request_pending: false,
        }
    }

    /// 第一项传输入队时申请系统休眠抑制，最后一项结束时自动释放。
    fn update_idle_sleep_prevention(&mut self, cx: &mut Context<Self>) {
        if self.active == 0 {
            self.idle_sleep_guard = None;
            return;
        }
        if self.idle_sleep_guard.is_some() || self.idle_sleep_request_pending {
            return;
        }

        self.idle_sleep_request_pending = true;
        let request = cx.prevent_idle_sleep("Blip 正在传输文件");
        cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |queue, _cx| {
                queue.idle_sleep_request_pending = false;
                match result {
                    Ok(guard) if queue.active > 0 => queue.idle_sleep_guard = Some(guard),
                    Ok(_) => {}
                    Err(err) => log::warn!("无法阻止系统在传输时休眠：{err:#}"),
                }
            });
        })
        .detach();
    }

    pub fn enqueue(
        &mut self,
        id: TransferId,
        name: String,
        direction: TransferDirection,
        total: u64,
        cx: &mut Context<Self>,
    ) {
        self.items.push(TransferItem {
            id,
            name,
            direction,
            total,
            transferred: 0,
            status: TransferStatus::Queued,
            speed: 0.,
            resumed_from: 0,
            streaming_total: false,
            active_files: Vec::new(),
        });
        self.active += 1;
        self.update_idle_sleep_prevention(cx);
        cx.notify();
    }

    /// 传输真正开始。`resumed_from > 0` 表示这次是断点续传。
    pub fn started(
        &mut self,
        id: TransferId,
        resumed_from: u64,
        total: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.status = TransferStatus::Running;
            item.resumed_from = resumed_from;
            item.streaming_total = total == 0;
            item.transferred = item.transferred.max(resumed_from);
            if total > 0 {
                item.total = total;
            }
            cx.notify();
        }
    }

    pub fn active_files(&mut self, id: TransferId, files: Vec<String>, cx: &mut Context<Self>) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.active_files = files;
            cx.notify();
        }
    }

    /// 进度更新。速率直接采用 worker 测出来的值，不在 UI 里反推。
    pub fn progress(
        &mut self,
        id: TransferId,
        transferred: u64,
        total: u64,
        speed: f64,
        cx: &mut Context<Self>,
    ) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.transferred = transferred;
            if total > 0 {
                item.total = total;
            }
            if speed.is_finite() && speed >= 0. {
                item.speed = speed;
            }
            cx.notify();
        }
    }

    /// 上传前发现冲突，等用户在弹窗里拍板。
    pub fn awaiting_decision(&mut self, id: TransferId, cx: &mut Context<Self>) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.status = TransferStatus::Awaiting;
            item.speed = 0.;
            item.streaming_total = false;
            item.active_files.clear();
            cx.notify();
        }
    }

    pub fn finished(&mut self, id: TransferId, outcome: TransferOutcome, cx: &mut Context<Self>) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            let was_active = item.is_active();
            item.status = match outcome {
                TransferOutcome::Completed => {
                    item.transferred = item.total.max(item.transferred);
                    TransferStatus::Done
                }
                TransferOutcome::Partial(message) => TransferStatus::Partial(message),
                TransferOutcome::Cancelled => TransferStatus::Cancelled,
                TransferOutcome::Skipped => {
                    item.transferred = 0;
                    TransferStatus::Skipped
                }
                TransferOutcome::Failed(message) => TransferStatus::Failed(message),
            };
            item.speed = 0.;
            item.streaming_total = false;
            item.active_files.clear();
            if was_active {
                self.active = self.active.saturating_sub(1);
                self.update_idle_sleep_prevention(cx);
            }
            cx.notify();
        }
    }

    pub fn clear_finished(&mut self, cx: &mut Context<Self>) {
        self.items.retain(|item| item.is_active());
        cx.notify();
    }

    pub fn clear_all(&mut self, cx: &mut Context<Self>) {
        self.items.clear();
        self.active = 0;
        self.update_idle_sleep_prevention(cx);
        cx.notify();
    }

    /// 移除单条任务（演示行也能删）。
    ///
    /// 真实进行中的任务不在这里处理：它归 worker 管，得先取消，
    /// 否则队列里没了行、传输还在后台跑。
    pub fn dismiss(&mut self, id: TransferId, cx: &mut Context<Self>) {
        let before = self.items.len();
        self.items.retain(|item| item.id != id || item.is_active());
        if self.items.len() != before {
            cx.notify();
        }
    }

    pub fn active_count(&self) -> usize {
        self.active
    }

    /// 某个传输的方向。行已经被用户删掉时返回 `None`。
    ///
    /// 传输完成后要刷哪个面板（上传刷远端、下载刷本地）就看它。
    pub fn direction_of(&self, id: TransferId) -> Option<TransferDirection> {
        self.items
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.direction)
    }

    /// `(进行中, 总数)`，给折叠状态下的标题栏用。
    pub fn summary(&self) -> (usize, usize) {
        (self.active_count(), self.items.len())
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let active = self.active_count();

        h_flex()
            .w_full()
            .justify_between()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted.opacity(0.35))
            .child(
                h_flex()
                    .gap_1p5()
                    .text_sm()
                    .child(IconName::ListChecks)
                    .child(SharedString::from(if active > 0 {
                        format!("传输队列 · {active} 个进行中")
                    } else {
                        "传输队列".to_string()
                    })),
            )
            .child(
                h_flex()
                    .gap_1()
                    .flex_shrink_0()
                    .child(
                        Button::new("clear-finished")
                            .ghost()
                            .xsmall()
                            .label("清理已完成")
                            .disabled(self.items.iter().all(|item| item.is_active()))
                            .on_click(cx.listener(|queue, _, _, cx| queue.clear_finished(cx))),
                    )
                    .child(
                        Button::new("collapse-queue")
                            .ghost()
                            .xsmall()
                            .icon(IconName::ChevronDown)
                            .tooltip("收起队列")
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(TransferQueueEvent::ToggleCollapsed)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_row(&self, item: &TransferItem, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let id = item.id;
        let resuming = item.resumed_from > 0;
        // 进行中的任务点了先取消（要通知 worker）；已结束的直接从列表移除。
        let cancellable = item.is_active();

        let active_summary = item.active_files.first().map(|path| {
            let file_name = path.rsplit('/').next().unwrap_or(path);
            if item.active_files.len() == 1 {
                format!("正在传输 {file_name}")
            } else {
                format!("正在传输 {file_name} 等 {} 个文件", item.active_files.len())
            }
        });
        let (mut status_text, status_color) = match &item.status {
            TransferStatus::Queued => ("等待中".to_string(), theme.muted_foreground),
            TransferStatus::Awaiting => ("等待确认".to_string(), theme.warning),
            TransferStatus::Running => (
                if resuming {
                    format!(
                        "续传 {} / {} · {}/s",
                        format_size(item.transferred),
                        format_size(item.total),
                        format_speed(item.speed)
                    )
                } else {
                    format!(
                        "{} / {} · {}/s",
                        format_size(item.transferred),
                        format_size(item.total),
                        format_speed(item.speed)
                    )
                },
                theme.muted_foreground,
            ),
            TransferStatus::Done => ("已完成".to_string(), theme.success),
            TransferStatus::Partial(message) => (format!("部分完成：{message}"), theme.warning),
            TransferStatus::Skipped => ("已跳过".to_string(), theme.muted_foreground),
            TransferStatus::Cancelled => (
                if resuming {
                    "已取消（保留已传部分）".to_string()
                } else {
                    "已取消".to_string()
                },
                theme.muted_foreground,
            ),
            TransferStatus::Failed(message) => (format!("失败：{message}"), theme.danger),
        };
        if let Some(active_summary) = active_summary {
            status_text = format!("{active_summary} · {status_text}");
        }
        let status_tooltip = if item.active_files.is_empty() {
            status_text.clone()
        } else {
            format!("{status_text}\n\n{}", item.active_files.join("\n"))
        };

        h_flex()
            .w_full()
            .h(px(52.))
            .flex_shrink_0()
            .gap_3()
            .px_3()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .child(div().flex_shrink_0().child(match item.direction {
                TransferDirection::Upload => IconName::Upload,
                TransferDirection::Download => IconName::Download,
            }))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .truncate()
                                            .text_xs()
                                            .child(SharedString::from(item.name.clone())),
                                    )
                                    .when(resuming, |this| {
                                        this.child(
                                            div()
                                                .flex_shrink_0()
                                                .px_1()
                                                .rounded_md()
                                                .border_1()
                                                .border_color(theme.border)
                                                .text_xs()
                                                .text_color(theme.muted_foreground)
                                                .child(SharedString::from(format!(
                                                    "续传 · 已有 {}",
                                                    format_size(item.resumed_from)
                                                ))),
                                        )
                                    }),
                            )
                            .child(
                                div()
                                    .id(("transfer-status", id))
                                    .flex_shrink_0()
                                    .max_w(px(360.))
                                    .truncate()
                                    .text_xs()
                                    .text_color(status_color)
                                    .child(SharedString::from(status_text))
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(status_tooltip.clone()).build(window, cx)
                                    }),
                            ),
                    )
                    .child(
                        Progress::new(("transfer-progress", item.id))
                            .with_size(px(2.))
                            .value(item.ratio())
                            .loading(
                                item.streaming_total
                                    && matches!(item.status, TransferStatus::Running),
                            )
                            .accessibility_label(format!(
                                "{} 传输进度 {:.0}%",
                                item.name,
                                item.ratio()
                            ))
                            .color(match item.status {
                                TransferStatus::Failed(_) => theme.danger,
                                TransferStatus::Done => theme.success,
                                _ => theme.primary,
                            }),
                    ),
            )
            .child(
                div().flex_shrink_0().child(
                    Button::new(("cancel", id))
                        .ghost()
                        .xsmall()
                        .icon(IconName::X)
                        .tooltip(if cancellable {
                            "取消传输（保留已传输部分）"
                        } else {
                            "从列表移除这一条"
                        })
                        .on_click(cx.listener(move |queue, _, _, cx| {
                            if cancellable {
                                cx.emit(TransferQueueEvent::Cancel(id));
                            } else {
                                queue.dismiss(id, cx);
                            }
                        })),
                ),
            )
            .into_any_element()
    }
}

impl Render for TransferQueue {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let header = self.render_header(cx);
        let item_count = self.items.len();
        let scroll = self.scroll.clone();

        v_flex()
            .size_full()
            .bg(theme.background)
            .child(header)
            .child(
                div()
                    .id("transfer-rows")
                    .flex_1()
                    .min_h_0()
                    .when(item_count == 0, |this| {
                        this.child(
                            h_flex()
                                .size_full()
                                .justify_center()
                                .items_center()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("还没有传输任务"),
                        )
                    })
                    .when(item_count > 0, |this| {
                        this.child(
                            uniform_list(
                                "transfer-row-list",
                                item_count,
                                cx.processor(
                                    |queue, range: std::ops::Range<usize>, _window, cx| {
                                        range
                                            .map(|index| queue.render_row(&queue.items[index], cx))
                                            .collect()
                                    },
                                ),
                            )
                            .size_full()
                            .track_scroll(&scroll),
                        )
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    // `use super::*` 会把 gpui 的 `test` 宏一起带进来，必须显式导入标准库那个。
    use ::core::prelude::v1::test;

    use super::*;

    /// 启动时队列必须是空的。
    ///
    /// 曾经在这里塞过三条「演示行」（一条假的「传输中」），删掉重启又回来，
    /// 还会被当成真实任务去点取消 —— 这条用例防止它再被加回来。
    #[test]
    fn new_queue_starts_empty() {
        let queue = TransferQueue::new();
        assert_eq!(queue.summary(), (0, 0));
        assert!(queue.items.is_empty(), "不该有任何预置的假任务");
    }

    #[::gpui::test]
    fn clear_all_removes_active_and_finished_items(cx: &mut gpui::TestAppContext) {
        let queue = cx.update(|cx| cx.new(|_| TransferQueue::new()));
        cx.update(|cx| {
            queue.update(cx, |queue, cx| {
                queue.enqueue(1, "active".into(), TransferDirection::Upload, 10, cx);
                queue.enqueue(2, "finished".into(), TransferDirection::Download, 20, cx);
                queue.finished(2, TransferOutcome::Completed, cx);
                queue.clear_all(cx);
            });
        });

        assert_eq!(cx.update(|cx| queue.read(cx).summary()), (0, 0));
    }

    #[::gpui::test]
    fn active_transfers_prevent_idle_sleep(cx: &mut gpui::TestAppContext) {
        let queue = cx.update(|cx| cx.new(|_| TransferQueue::new()));
        cx.update(|cx| {
            queue.update(cx, |queue, cx| {
                queue.enqueue(1, "first".into(), TransferDirection::Upload, 10, cx);
                queue.enqueue(2, "second".into(), TransferDirection::Download, 20, cx);
            });
        });
        cx.run_until_parked();
        assert_eq!(cx.active_idle_sleep_preventions(), 1);

        cx.update(|cx| {
            queue.update(cx, |queue, cx| {
                queue.finished(1, TransferOutcome::Completed, cx);
            });
        });
        assert_eq!(cx.active_idle_sleep_preventions(), 1);

        cx.update(|cx| {
            queue.update(cx, |queue, cx| {
                queue.finished(2, TransferOutcome::Completed, cx);
            });
        });
        assert_eq!(cx.active_idle_sleep_preventions(), 0);
    }

    #[::gpui::test]
    fn streaming_folder_tracks_current_files_until_completion(cx: &mut gpui::TestAppContext) {
        let queue = cx.update(|cx| cx.new(|_| TransferQueue::new()));
        cx.update(|cx| {
            queue.update(cx, |queue, cx| {
                queue.enqueue(9, "node_modules/".into(), TransferDirection::Upload, 0, cx);
                queue.started(9, 0, 0, cx);
                queue.active_files(
                    9,
                    vec!["pkg/index.js".into(), "pkg/package.json".into()],
                    cx,
                );
            });
        });

        cx.update(|cx| {
            let item = &queue.read(cx).items[0];
            assert!(item.streaming_total, "动态总量必须使用不定进度条");
            assert_eq!(item.active_files.len(), 2);
            assert_eq!(item.active_files[0], "pkg/index.js");
        });

        cx.update(|cx| {
            queue.update(cx, |queue, cx| {
                queue.finished(9, TransferOutcome::Completed, cx);
            });
        });
        cx.update(|cx| {
            let item = &queue.read(cx).items[0];
            assert!(!item.streaming_total);
            assert!(item.active_files.is_empty());
            assert_eq!(item.ratio(), 100.);
        });
    }
}
