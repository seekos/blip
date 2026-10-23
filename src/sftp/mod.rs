//! SFTP 网络层：完全跑在独立线程的 Tokio runtime 上，与 GPUI 零耦合。
//!
//! 对外只有两个入口：
//!
//! * [`SftpWorker`] —— 持有线程与 runtime 的句柄，`send` 指令、`events` 收事件；
//! * [`SftpCommand`] / [`SftpEvent`] —— 唯一的跨线程协议。
//!
//! 内部分工：
//!
//! | 模块 | 职责 |
//! | --- | --- |
//! | [`types`] | 协议类型，全部 `Send + 'static` |
//! | [`worker`] | 线程 + runtime + 命令循环 + 取消表 |
//! | [`session`] | SSH 建连、密码/私钥认证、SFTP 子系统、目录与文件操作 |
//! | [`transfer`] | 流式上传下载，进度按 100ms 节流 |
//! | [`local_tree`] | 上传文件夹前扫本地目录树（纯 `std::fs`，可单测） |
//! | [`remote_tree`] | 下载文件夹前扫远端目录树（异步 `read_dir`，符号链接跳过） |
//!
//! 一条硬约束：这个模块里**不允许**出现任何 GPUI 类型，也**不允许**在 GPUI 的
//! `render` / `Context` 里调用这里的 `async` 函数 —— 只能通过 `flume` 通信。

pub mod local_tree;
pub mod remote_tree;
pub mod session;
pub mod transfer;
pub mod types;
pub mod worker;

#[cfg(any(test, feature = "demo-server"))]
pub mod testserver;

// 对外只需要两样东西：一个句柄，一套协议。其余类型留在 `types` 里按需引入。
pub use types::{SftpCommand, SftpEvent};
pub use worker::SftpWorker;
