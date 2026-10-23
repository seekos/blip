//! UI 层：工具栏、双栏工作区、文件面板、传输队列。

pub mod app_icon;
pub mod file_pane;
pub mod mock;
pub mod toolbar;
pub mod transfer_panel;
pub mod upload_conflict;
pub mod workspace;

/// 连接状态机，工具栏与根实体共享。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionState {
    Disconnected,
    Connecting,
    Connected,
}
