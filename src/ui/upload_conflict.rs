//! 上传冲突弹窗：远端已有同名文件、或残留了没传完的 `.filepart` 时问用户怎么办。
//!
//! 这是 WinSCP 那套交互的对应物。三个选择各自对应 `UploadDecision` 的一个值：
//!
//! | 按钮 | 含义 | worker 侧动作 |
//! | --- | --- | --- |
//! | 断点续传 | 从 `.filepart` 已有的字节后面接着传 | `offset = filepart 长度` |
//! | 安全覆盖 | 丢弃 `.filepart`，从头写，写完原子替换 | `offset = 0`，`create` 截断 |
//! | 跳过 | 什么都不做 | 不发传输任务，直接 `TransferOutcome::Skipped` |
//!
//! 弹窗是**强制选择**的：没有右上角叉、点遮罩不关、Esc 也不关。因为任务正挂在
//! worker 的等待表里，随便关掉会让它永远悬在那儿（用户还能在队列里点取消兜底）。

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    v_flex,
};
use gpui_kit::prelude::*;
use gpui_kit::*;

use crate::sftp::session::format_size;
use crate::sftp::types::{SftpCommand, UploadConflict, UploadDecision};

pub struct UploadConflictPrompt {
    conflict: UploadConflict,
    sender: flume::Sender<SftpCommand>,
    /// 「对后续冲突执行相同操作」。
    apply_to_all: bool,
}

impl UploadConflictPrompt {
    pub fn new(conflict: UploadConflict, sender: flume::Sender<SftpCommand>) -> Self {
        Self {
            conflict,
            sender,
            apply_to_all: false,
        }
    }

    /// 把选择送回 worker 并关掉弹窗。
    fn resolve(&mut self, decision: UploadDecision, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.sender.send(SftpCommand::ResolveUpload {
            id: self.conflict.id,
            decision,
            apply_to_all: self.apply_to_all,
        });

        // 「对后续冲突执行相同操作」时，worker 会一次性把**所有**在等决定的上传
        // 都处理掉（同一批拖进来的文件可能已经叠了好几个弹窗），这里要一起关。
        if self.apply_to_all {
            window.close_all_dialogs(cx);
        } else {
            window.close_dialog(cx);
        }
    }
}

impl Render for UploadConflictPrompt {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();

        // 先把要用的值取出来，后面 `cx.listener(...)` 就不用再借 `self`。
        let file_name = self.conflict.file_name();
        let remote_path = self.conflict.remote_path.clone();
        let local_size = self.conflict.local_size;
        let existing_size = self.conflict.existing_size;
        let filepart_size = self.conflict.filepart_size;
        let can_resume = self.conflict.can_resume();
        let stale_filepart = self.conflict.filepart_is_stale();
        let headline = if self.conflict.has_existing() {
            format!("远端已存在 {file_name}，请选择处理方式。")
        } else {
            format!("远端有没传完的 {file_name}，请选择处理方式。")
        };

        let fact = |label: &'static str, value: String, highlight: bool| -> AnyElement {
            h_flex()
                .w_full()
                .justify_between()
                .gap_3()
                .text_xs()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .truncate()
                        .text_color(if highlight {
                            theme.warning
                        } else {
                            theme.foreground
                        })
                        .child(SharedString::from(value)),
                )
                .into_any_element()
        };

        v_flex()
            .w_full()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_sm()
                            .child(SharedString::from(headline)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(remote_path)),
                    ),
            )
            .child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .p_2()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.muted.opacity(0.3))
                    .child(fact("本地文件", format_size(local_size), false))
                    .child(fact(
                        "远端现有",
                        match existing_size {
                            Some(size) => format_size(size),
                            None => "不存在".to_string(),
                        },
                        false,
                    ))
                    .child(fact(
                        "未完成临时文件",
                        match filepart_size {
                            Some(size) => format_size(size),
                            None => "无".to_string(),
                        },
                        can_resume,
                    )),
            )
            .when(can_resume, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(SharedString::from(format!(
                            "续传只补剩下的 {}，已传部分不会重来。",
                            format_size(local_size.saturating_sub(filepart_size.unwrap_or(0)))
                        ))),
                )
            })
            .when(stale_filepart, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child("临时文件不小于本地文件，内容可能已经过期 —— 只能从头覆盖。"),
                )
            })
            .child(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("conflict-skip")
                            .outline()
                            .small()
                            .label("跳过")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.resolve(UploadDecision::Skip, window, cx)
                            })),
                    )
                    .child(
                        Button::new("conflict-overwrite")
                            .outline()
                            .small()
                            .label("安全覆盖")
                            .tooltip("先写临时文件，写完再原子替换，不会让正在运行的程序崩溃")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.resolve(UploadDecision::Overwrite, window, cx)
                            })),
                    )
                    .child(
                        Button::new("conflict-resume")
                            .primary()
                            .small()
                            .label("断点续传")
                            .disabled(!can_resume)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.resolve(UploadDecision::Resume, window, cx)
                            })),
                    ),
            )
            .child(
                Checkbox::new("conflict-apply-all")
                    .label(SharedString::from("对后续冲突执行相同操作"))
                    .checked(self.apply_to_all)
                    .on_click(cx.listener(|this, checked, _, cx| {
                        this.apply_to_all = *checked;
                        cx.notify();
                    })),
            )
    }
}
