//! Tokio worker：把 SFTP 网络 I/O 完全隔离在 GPUI 之外。
//!
//! 结构是经典的 actor 模型：
//!
//! ```text
//!   GPUI 主线程                       专用线程 macscp-sftp
//!  ┌──────────┐   SftpCommand    ┌────────────────────────────────┐
//!  │ ScpApp   │ ───────────────▶ │ tokio Runtime (multi_thread)   │
//!  │          │ ◀─────────────── │  ├─ 命令循环（串行）            │
//!  └──────────┘   SftpEvent      │  └─ 传输/读目录任务（并发）      │
//!                                └────────────────────────────────┘
//! ```
//!
//! 为什么必须这样：GPUI 主线程没有 Tokio reactor，`tokio::spawn` 在 `render` 或
//! `Context` 里调用会 panic；反过来，Tokio 线程也不能碰 GPUI 的 `Entity`。
//! 两个世界只通过 `flume` channel 通信。

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::{debug, error, info, warn};
use russh_sftp::client::SftpSession;

use super::path_lock::{PathLocks, local_key, remote_key};
use super::session::{self, ActiveSession, ConnectError, HostKeyChallenge, HostKeyPolicy};
use super::transfer;
use super::types::{
    ErrorContext, HostKeyDecision, HostKeyPrompt, SftpCommand, SftpEvent, TransferId,
    TransferOutcome, UploadConflict, UploadDecision,
};

/// 传输任务的取消句柄。
type CancelFlag = Arc<AtomicBool>;

/// 进行中的传输：`TransferId -> 取消标志`。
///
/// 用 `Arc<Mutex<..>>` 是因为传输任务跑在 `tokio::spawn` 出来的独立 future 里，
/// 结束时需要自己把条目摘掉 —— 否则长时间运行的客户端会把这张表越撑越大。
type CancelRegistry = Arc<Mutex<HashMap<TransferId, CancelFlag>>>;

/// 等 UI 决定「续传 / 覆盖 / 跳过」的上传任务。
///
/// 探针已经跑完、决定还没来，所以还没 `spawn` 传输任务；但**取消标志已经登记**，
/// 用户在弹窗还没弹出来（或已经弹出来）时点取消，都能立刻生效。
struct PendingUpload {
    local: PathBuf,
    remote: String,
    total: u64,
    /// `SftpCommand::Upload.resume`：关掉时「断点续传」等价于「从头覆盖」。
    resume: bool,
    /// 收到 `Upload` 指令时就创建好的取消标志，spawn 时直接沿用。
    cancel: CancelFlag,
}

/// 等待用户拍板的上传：`TransferId -> 任务`。
type PendingRegistry = Arc<Mutex<HashMap<TransferId, PendingUpload>>>;

struct PendingHostTrust {
    config: super::types::SessionConfig,
    challenge: HostKeyChallenge,
}

/// 单条指令的最长等待时间。超时后按失败处理，避免界面无限转圈。
const OP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SftpWorker {
    commands: flume::Sender<SftpCommand>,
    events: flume::Receiver<SftpEvent>,
    _thread: Option<thread::JoinHandle<()>>,
}

impl SftpWorker {
    /// 起一个具名线程，在里面建 Tokio runtime 并跑事件循环。
    ///
    /// 用 `thread::Builder` 而不是裸 `std::thread::spawn`，只是为了给线程起名
    /// （调试器和 Activity Monitor 里能认出来）；两者本质是同一个 API。
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = flume::unbounded::<SftpCommand>();
        let (event_tx, event_rx) = flume::unbounded::<SftpEvent>();

        // 线程起不来时还要用这个 sender 报错，所以先克隆一份。
        let failure_tx = event_tx.clone();
        let spawned = thread::Builder::new()
            .name("macscp-sftp".to_string())
            .spawn(move || worker_main(command_rx, event_tx));

        let thread = match spawned {
            Ok(handle) => Some(handle),
            Err(err) => {
                error!("无法启动 SFTP 线程：{err}");
                let _ = failure_tx.send(SftpEvent::error(
                    ErrorContext::Runtime,
                    format!("无法启动网络线程：{err}"),
                ));
                None
            }
        };

        Self {
            commands: command_tx,
            events: event_rx,
            _thread: thread,
        }
    }

    /// 投递指令。worker 已退出时只记日志 —— UI 不该因为网络层消失而 panic。
    pub fn send(&self, command: SftpCommand) {
        if let Err(err) = self.commands.send(command) {
            warn!("SFTP worker 已退出，指令被丢弃：{err}");
        }
    }

    pub fn events(&self) -> &flume::Receiver<SftpEvent> {
        &self.events
    }

    /// 供 UI 里的回调（例如对话框按钮）直接投递指令，而不必持有 `ScpApp`。
    pub fn sender(&self) -> flume::Sender<SftpCommand> {
        self.commands.clone()
    }
}

impl Drop for SftpWorker {
    fn drop(&mut self) {
        // 通知命令循环退出，让 runtime 与 SSH 会话有序关闭。
        let _ = self.commands.send(SftpCommand::Shutdown);
    }
}

// ---------------------------------------------------------------------------
// 线程入口
// ---------------------------------------------------------------------------

fn worker_main(commands: flume::Receiver<SftpCommand>, events: flume::Sender<SftpEvent>) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        // 两条线程足够：一条跑命令循环，一条给并发的传输任务。
        .worker_threads(2)
        .thread_name("macscp-tokio")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            error!("无法创建 Tokio runtime：{err}");
            let _ = events.send(SftpEvent::error(
                ErrorContext::Runtime,
                format!("无法创建网络运行时：{err}"),
            ));
            return;
        }
    };

    runtime.block_on(run_loop(commands, events));
}

/// 命令循环。串行处理控制类指令，把耗时操作 `tokio::spawn` 出去并发执行。
async fn run_loop(commands: flume::Receiver<SftpCommand>, events: flume::Sender<SftpEvent>) {
    let mut session: Option<ActiveSession> = None;
    let cancels: CancelRegistry = Arc::new(Mutex::new(HashMap::new()));
    let pending: PendingRegistry = Arc::new(Mutex::new(HashMap::new()));
    let path_locks = PathLocks::new();
    // 短文件操作没有 TransferId；连接切换时用这一枚共享标志终止尚未开始/递归中的任务。
    let mut connection_cancel = Arc::new(AtomicBool::new(false));
    let mut pending_host: Option<PendingHostTrust> = None;
    // 「对后续冲突执行相同操作」的选择，本次连接内有效。
    let mut upload_policy: Option<UploadDecision> = None;

    while let Ok(command) = commands.recv_async().await {
        match command {
            // ---------------------------------------------------------------
            // 连接
            // ---------------------------------------------------------------
            SftpCommand::Connect(config) => {
                info!("正在连接 {}", config.endpoint());
                // 旧连接先释放，避免两条 SSH 会话同时挂在服务器上。
                connection_cancel.store(true, Ordering::Relaxed);
                connection_cancel = Arc::new(AtomicBool::new(false));
                cancel_all(&cancels);
                cancel_pending_uploads(&pending, &cancels, &events);
                session = None;
                pending_host = None;
                upload_policy = None;

                match session::connect(&config, HostKeyPolicy::KnownHosts).await {
                    Ok(active) => {
                        let home = active.home.clone();
                        info!("已连接 {}，登录目录 {home}", config.endpoint());
                        session = Some(active);
                        let _ = events.send(SftpEvent::Connected { home });
                    }
                    Err(ConnectError::HostKey(challenge)) => {
                        let prompt = HostKeyPrompt {
                            endpoint: challenge.endpoint.clone(),
                            algorithm: challenge.algorithm.clone(),
                            fingerprint: challenge.fingerprint.clone(),
                            changed_line: challenge.changed_line,
                        };
                        pending_host = Some(PendingHostTrust {
                            config,
                            challenge: *challenge,
                        });
                        let _ = events.send(SftpEvent::HostKeyPrompt(prompt));
                    }
                    Err(err) => {
                        error!("连接失败：{err:#}");
                        let _ = events
                            .send(SftpEvent::error(ErrorContext::Connect, format!("{err:#}")));
                    }
                }
            }

            SftpCommand::Disconnect => {
                connection_cancel.store(true, Ordering::Relaxed);
                cancel_all(&cancels);
                cancel_pending_uploads(&pending, &cancels, &events);
                session = None;
                pending_host = None;
                upload_policy = None;
                let _ = events.send(SftpEvent::Disconnected);
            }

            SftpCommand::ResolveHostKey(decision) => {
                let Some(waiting) = pending_host.take() else {
                    debug!("主机密钥决定来晚了，忽略");
                    continue;
                };

                if decision == HostKeyDecision::Reject {
                    let _ = events.send(SftpEvent::error(
                        ErrorContext::Connect,
                        "已取消连接：未信任服务器主机密钥",
                    ));
                    continue;
                }

                if decision == HostKeyDecision::TrustAndSave
                    && let Err(message) = session::remember_host_key(
                        &waiting.config,
                        &waiting.challenge.key,
                        waiting.challenge.changed_line,
                    )
                {
                    let _ = events.send(SftpEvent::error(ErrorContext::Connect, message));
                    continue;
                }

                match session::connect(&waiting.config, HostKeyPolicy::Exact(waiting.challenge.key))
                    .await
                {
                    Ok(active) => {
                        let home = active.home.clone();
                        info!("已确认主机密钥，登录目录 {home}");
                        session = Some(active);
                        let _ = events.send(SftpEvent::Connected { home });
                    }
                    Err(err) => {
                        let _ = events
                            .send(SftpEvent::error(ErrorContext::Connect, format!("{err:#}")));
                    }
                }
            }

            SftpCommand::InstallPublicKey {
                public_key,
                private_key,
            } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::error(
                        ErrorContext::InstallPublicKey,
                        "尚未连接到服务器",
                    ));
                    continue;
                };

                let sftp = active.sftp.clone();
                let home = active.home.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let path_key = remote_key(&session::join_remote(&home, ".ssh/authorized_keys"));
                let operation_cancel = connection_cancel.clone();
                tokio::spawn(async move {
                    let _path_guard = locks.acquire([path_key]).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = guard("部署公钥", async {
                        transfer::install_public_key(&sftp, &home, &public_key)
                            .await
                            .map_err(|err| format!("{err:#}"))
                    })
                    .await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match result {
                        Ok(already_present) => {
                            let _ = events.send(SftpEvent::PublicKeyInstalled {
                                private_key,
                                already_present,
                            });
                        }
                        Err(message) => {
                            let _ = events
                                .send(SftpEvent::error(ErrorContext::InstallPublicKey, message));
                        }
                    }
                });
            }

            // ---------------------------------------------------------------
            // 目录
            // ---------------------------------------------------------------
            SftpCommand::ListDir {
                request_id,
                path,
                fallback,
            } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::error(ErrorContext::List, "尚未连接到服务器"));
                    continue;
                };

                let sftp = active.sftp.clone();
                let events = events.clone();
                let operation_cancel = connection_cancel.clone();
                // 读目录可能慢（大目录、高延迟链路），不能卡住命令循环。
                tokio::spawn(async move {
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = guard(
                        "读取目录",
                        session::list_dir_or_fallback(&sftp, &path, fallback.as_deref()),
                    )
                    .await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match result {
                        Ok((listed_path, entries)) => {
                            let _ = events.send(SftpEvent::DirListed {
                                request_id,
                                path: listed_path,
                                entries,
                            });
                        }
                        Err(message) => {
                            let _ = events.send(SftpEvent::error(ErrorContext::List, message));
                        }
                    }
                });
            }

            // ---------------------------------------------------------------
            // 传输
            // ---------------------------------------------------------------
            SftpCommand::Download {
                id,
                remote,
                local,
                total,
                resume,
            } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Failed("尚未连接到服务器".to_string()),
                    });
                    continue;
                };

                let sftp = active.sftp.clone();
                let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
                register_cancel(&cancels, id, cancel.clone());

                let registry = cancels.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let path_key = local_key(&local);
                tokio::spawn(async move {
                    let _path_guard = locks.acquire([path_key]).await;
                    transfer::run_download(sftp, id, remote, local, total, resume, cancel, events)
                        .await;
                    unregister_cancel(&registry, id);
                });
            }

            SftpCommand::Upload {
                id,
                local,
                remote,
                total,
                resume,
            } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Failed("尚未连接到服务器".to_string()),
                    });
                    continue;
                };

                // 一收到指令就登记取消标志：探远端、等用户选「续传 / 覆盖」
                // 这两段也可能耗很久（高延迟链路 + 用户走神），期间点取消
                // 必须立刻生效，不能等 spawn 之后。
                let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
                register_cancel(&cancels, id, cancel.clone());

                // 先探远端现状：要不要打扰用户，取决于这里的结果。
                // 两次 stat 都很快（同一个连接上的往返），串行做比让 UI 等更简单。
                let probe = match guard("检查远端文件", async {
                    transfer::probe_upload(&active.sftp, &local, &remote)
                        .await
                        .map_err(|err| format!("{err:#}"))
                })
                .await
                {
                    Ok(probe) => probe,
                    Err(message) => {
                        unregister_cancel(&cancels, id);
                        let _ = events.send(SftpEvent::TransferCompleted {
                            id,
                            outcome: TransferOutcome::Failed(message),
                        });
                        continue;
                    }
                };

                // 探测期间用户可能已经点了取消：既不该弹冲突框，也不该写字节。
                if cancel.load(Ordering::Relaxed) {
                    debug!("上传 {id} 在探测阶段被取消");
                    unregister_cancel(&cancels, id);
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Cancelled,
                    });
                    continue;
                }

                if probe.target_is_dir {
                    unregister_cancel(&cancels, id);
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Failed(format!(
                            "远端同名条目是目录，无法覆盖：{remote}"
                        )),
                    });
                    continue;
                }

                // 远端干干净净 → 直接写，不用打扰用户。
                // 有冲突时先看「对后续冲突」的策略，最后才落到弹窗。
                let decision = if !probe.has_conflict() {
                    UploadDecision::Overwrite
                } else if let Some(policy) = upload_policy {
                    debug!(
                        "上传 {remote} 有冲突，按既定策略「{}」处理",
                        policy.as_str()
                    );
                    policy
                } else {
                    debug!("上传 {remote} 有冲突，等待用户确认");
                    let _ = events.send(SftpEvent::UploadConflict(UploadConflict {
                        id,
                        local_path: local.clone(),
                        remote_path: remote.clone(),
                        local_size: probe.local_size,
                        existing_size: probe.existing_size,
                        filepart_size: probe.filepart_size,
                        checkpoint_present: probe.checkpoint_present,
                    }));
                    match pending.lock() {
                        Ok(mut map) => {
                            map.insert(
                                id,
                                PendingUpload {
                                    local,
                                    remote,
                                    total,
                                    resume,
                                    cancel,
                                },
                            );
                        }
                        Err(_) => {
                            // 等待表坏了就没法接住这次上传：连取消标志一起摘掉，
                            // 免得留一条永远「等待确认」的行。
                            unregister_cancel(&cancels, id);
                            let _ = events.send(SftpEvent::TransferCompleted {
                                id,
                                outcome: TransferOutcome::Failed(
                                    "内部状态异常，无法等待上传确认".to_string(),
                                ),
                            });
                            warn!("等待表被污染，上传 {id} 已放弃");
                        }
                    }
                    continue;
                };

                spawn_upload(
                    id,
                    PendingUpload {
                        local,
                        remote,
                        total,
                        resume,
                        cancel,
                    },
                    decision,
                    active.sftp.clone(),
                    &cancels,
                    &events,
                    &path_locks,
                );
            }

            SftpCommand::UploadOverwrite {
                id,
                local,
                remote,
                total,
            } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Failed("尚未连接到服务器".to_string()),
                    });
                    continue;
                };

                let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
                register_cancel(&cancels, id, cancel.clone());
                spawn_upload(
                    id,
                    PendingUpload {
                        local,
                        remote,
                        total,
                        resume: false,
                        cancel,
                    },
                    UploadDecision::Overwrite,
                    active.sftp.clone(),
                    &cancels,
                    &events,
                    &path_locks,
                );
            }

            SftpCommand::DownloadDir {
                id,
                remote,
                local,
                on_existing,
            } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Failed("尚未连接到服务器".to_string()),
                    });
                    continue;
                };

                debug!(
                    "整目录下载 {remote} → {local:?}，同名文件：{}",
                    on_existing.as_str()
                );

                let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
                register_cancel(&cancels, id, cancel.clone());

                let sftp = active.sftp.clone();
                let registry = cancels.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let path_key = local_key(&local);
                tokio::spawn(async move {
                    let _path_guard = locks.acquire([path_key]).await;
                    transfer::run_download_dir(
                        sftp,
                        id,
                        remote,
                        local,
                        on_existing,
                        cancel,
                        events,
                    )
                    .await;
                    unregister_cancel(&registry, id);
                });
            }

            SftpCommand::UploadDir {
                id,
                local,
                remote,
                on_existing,
            } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Failed("尚未连接到服务器".to_string()),
                    });
                    continue;
                };

                // 整批一个取消标志：队列里那一行点 ✕ 就停掉整棵树。
                // 也不用等用户确认冲突 —— 策略在指令里就已经定了。
                debug!(
                    "整目录上传 {local:?} → {remote}，同名文件：{}",
                    on_existing.as_str()
                );
                let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
                register_cancel(&cancels, id, cancel.clone());

                let sftp = active.sftp.clone();
                let registry = cancels.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let path_key = remote_key(&remote);
                tokio::spawn(async move {
                    let _path_guard = locks.acquire([path_key]).await;
                    transfer::run_upload_dir(sftp, id, local, remote, on_existing, cancel, events)
                        .await;
                    unregister_cancel(&registry, id);
                });
            }

            SftpCommand::ResolveUpload {
                id,
                decision,
                apply_to_all,
            } => {
                if apply_to_all {
                    upload_policy = Some(decision);
                    debug!("后续上传冲突统一按「{}」处理", decision.as_str());
                }
                info!(
                    "上传 {id} 的冲突处理：{}（apply_to_all={apply_to_all}）",
                    decision.as_str()
                );

                // 同一批拖进来的多个文件可能都在等决定：apply_to_all 时一起处理。
                let waiting: Vec<(TransferId, PendingUpload)> = match pending.lock() {
                    Ok(mut map) => {
                        if apply_to_all {
                            let mut all: Vec<_> = map.drain().collect();
                            // 按 id 排序，日志和入队顺序才是稳定的。
                            all.sort_by_key(|(task_id, _)| *task_id);
                            all
                        } else {
                            map.remove(&id)
                                .map(|job| vec![(id, job)])
                                .unwrap_or_default()
                        }
                    }
                    Err(_) => {
                        warn!("等待表被污染，忽略上传 {id} 的决定");
                        Vec::new()
                    }
                };

                if waiting.is_empty() {
                    debug!("上传 {id} 的决定来晚了（任务已取消或已处理），忽略");
                    continue;
                }

                let Some(active) = session.as_ref() else {
                    for (task_id, _) in waiting {
                        unregister_cancel(&cancels, task_id);
                        let _ = events.send(SftpEvent::TransferCompleted {
                            id: task_id,
                            outcome: TransferOutcome::Failed("尚未连接到服务器".to_string()),
                        });
                    }
                    continue;
                };

                for (task_id, job) in waiting {
                    if job.cancel.load(Ordering::Relaxed) {
                        // 弹窗还开着的时候用户点了行尾的取消。
                        debug!("上传 {task_id} 在确认阶段被取消");
                        unregister_cancel(&cancels, task_id);
                        let _ = events.send(SftpEvent::TransferCompleted {
                            id: task_id,
                            outcome: TransferOutcome::Cancelled,
                        });
                    } else if decision.is_transfer() {
                        spawn_upload(
                            task_id,
                            job,
                            decision,
                            active.sftp.clone(),
                            &cancels,
                            &events,
                            &path_locks,
                        );
                    } else {
                        debug!("上传 {task_id} 被用户跳过");
                        unregister_cancel(&cancels, task_id);
                        let _ = events.send(SftpEvent::TransferCompleted {
                            id: task_id,
                            outcome: TransferOutcome::Skipped,
                        });
                    }
                }
            }

            SftpCommand::Cancel(id) => {
                // 还在等用户确认的上传：直接从等待表里摘掉，顺便注销取消标志。
                let parked = pending.lock().ok().and_then(|mut map| map.remove(&id));
                if parked.is_some() {
                    debug!("取消等待确认的上传 {id}");
                    unregister_cancel(&cancels, id);
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Cancelled,
                    });
                    continue;
                }

                match cancels.lock() {
                    Ok(map) => match map.get(&id) {
                        Some(flag) => {
                            debug!("取消传输 {id}");
                            flag.store(true, Ordering::Relaxed);
                        }
                        None => debug!("取消一个已结束的传输 {id}，忽略"),
                    },
                    Err(_) => warn!("取消表被污染，忽略取消请求"),
                }
            }

            // ---------------------------------------------------------------
            // 文件操作：短、快、需要保序，直接在当前循环里 await
            // ---------------------------------------------------------------
            SftpCommand::ReadText { path } => {
                let Some(active) = session.as_ref() else {
                    let _ =
                        events.send(SftpEvent::error(ErrorContext::ReadText, "尚未连接到服务器"));
                    continue;
                };
                let sftp = active.sftp.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let path_key = remote_key(&path);
                let operation_cancel = connection_cancel.clone();
                tokio::spawn(async move {
                    let _path_guard = locks.acquire([path_key]).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let operation = async {
                        transfer::read_remote_text(&sftp, &path)
                            .await
                            .map_err(|err| format!("{err:#}"))
                    };
                    let result = guard("读取文本文件", operation).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match result {
                        Ok(content) => {
                            let _ = events.send(SftpEvent::TextLoaded { path, content });
                        }
                        Err(message) => {
                            let _ = events.send(SftpEvent::error(ErrorContext::ReadText, message));
                        }
                    }
                });
            }

            SftpCommand::WriteText { path, content } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::error(
                        ErrorContext::WriteText,
                        "尚未连接到服务器",
                    ));
                    continue;
                };

                let sftp = active.sftp.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let path_key = remote_key(&path);
                let operation_cancel = connection_cancel.clone();
                tokio::spawn(async move {
                    let _path_guard = locks.acquire([path_key]).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = transfer::write_remote_text(&sftp, &path, &content).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match result {
                        Ok(()) => {
                            let _ = events.send(SftpEvent::TextSaved { path });
                        }
                        Err(err) => {
                            let _ = events.send(SftpEvent::error(
                                ErrorContext::WriteText,
                                format!("{err:#}"),
                            ));
                        }
                    }
                });
            }

            SftpCommand::Mkdir { path } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::error(ErrorContext::Mkdir, "尚未连接到服务器"));
                    continue;
                };

                let sftp = active.sftp.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let path_key = remote_key(&path);
                let operation_cancel = connection_cancel.clone();
                tokio::spawn(async move {
                    let _path_guard = locks.acquire([path_key]).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = guard("创建目录", session::create_dir(&sftp, &path)).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match result {
                        Ok(()) => {
                            let _ = events.send(SftpEvent::Notice(format!("已创建目录 {path}")));
                        }
                        Err(message) => {
                            let _ = events.send(SftpEvent::error(ErrorContext::Mkdir, message));
                        }
                    }
                });
            }

            SftpCommand::Remove { entries } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::error(ErrorContext::Remove, "尚未连接到服务器"));
                    continue;
                };

                let sftp = active.sftp.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let path_keys: Vec<_> = entries.iter().map(|(path, _)| remote_key(path)).collect();
                let operation_cancel = connection_cancel.clone();
                tokio::spawn(async move {
                    let _path_guard = locks.acquire(path_keys).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let total = entries.len();
                    let mut deleted = 0;
                    let mut removed_entries = 0usize;
                    let mut failures = Vec::new();
                    for (path, is_dir) in entries {
                        match session::remove(&sftp, &path, is_dir, &operation_cancel).await {
                            Ok(removed) => {
                                deleted += 1;
                                removed_entries += removed;
                            }
                            Err(err) => {
                                removed_entries += err.removed;
                                failures.push(err.to_string());
                            }
                        }
                    }

                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }

                    if failures.is_empty() {
                        let message = if deleted == 1 {
                            format!("已删除 1 项（共 {removed_entries} 个文件/目录）")
                        } else {
                            format!("已删除 {deleted} 项（共 {removed_entries} 个文件/目录）")
                        };
                        let _ = events.send(SftpEvent::Notice(message));
                    } else {
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
                        let _ = events.send(SftpEvent::error(
                            ErrorContext::Remove,
                            format!(
                                "删除完成 {deleted}/{total} 项，实际已永久删除 {removed_entries} 个文件/目录；失败 {} 项：{preview}{omitted}",
                                failures.len()
                            ),
                        ));
                    }
                });
            }

            SftpCommand::Rename { from, to } => {
                let Some(active) = session.as_ref() else {
                    let _ = events.send(SftpEvent::error(ErrorContext::Rename, "尚未连接到服务器"));
                    continue;
                };

                let sftp = active.sftp.clone();
                let events = events.clone();
                let locks = path_locks.clone();
                let keys = [remote_key(&from), remote_key(&to)];
                let operation_cancel = connection_cancel.clone();
                tokio::spawn(async move {
                    let _path_guard = locks.acquire(keys).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    let result = guard("重命名", session::rename(&sftp, &from, &to)).await;
                    if operation_cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    match result {
                        Ok(()) => {
                            let _ =
                                events.send(SftpEvent::Notice(format!("已重命名 {from} → {to}")));
                        }
                        Err(message) => {
                            let _ = events.send(SftpEvent::error(ErrorContext::Rename, message));
                        }
                    }
                });
            }

            SftpCommand::Shutdown => {
                connection_cancel.store(true, Ordering::Relaxed);
                cancel_all(&cancels);
                cancel_pending_uploads(&pending, &cancels, &events);
                break;
            }
        }
    }

    debug!("SFTP worker 退出");
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 给一次网络操作套上超时：卡住的连接不应该让界面永远转圈。
async fn guard<T>(
    label: &str,
    operation: impl Future<Output = Result<T, String>>,
) -> Result<T, String> {
    match tokio::time::timeout(OP_TIMEOUT, operation).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "{label}超时：{} 秒内没有响应",
            OP_TIMEOUT.as_secs()
        )),
    }
}

fn register_cancel(registry: &CancelRegistry, id: TransferId, flag: CancelFlag) {
    match registry.lock() {
        Ok(mut map) => {
            map.insert(id, flag);
        }
        Err(_) => warn!("取消表被污染，传输 {id} 将无法取消"),
    }
}

/// 真正把上传任务丢到 runtime 上。
///
/// 取消标志在收到 `Upload` 指令时就已经登记（探测和等确认期间也要能取消），
/// 这里沿用它，**不新建** —— 否则用户在弹窗阶段点的取消会落在旧标志上，
/// 新标志永远是 false，任务照跑。
fn spawn_upload(
    id: TransferId,
    job: PendingUpload,
    decision: UploadDecision,
    sftp: Arc<SftpSession>,
    cancels: &CancelRegistry,
    events: &flume::Sender<SftpEvent>,
    path_locks: &Arc<PathLocks>,
) {
    // 关掉续传时，「断点续传」等价于「从头覆盖」。
    let decision = if job.resume || decision != UploadDecision::Resume {
        decision
    } else {
        UploadDecision::Overwrite
    };

    let cancel = job.cancel;
    let registry = cancels.clone();
    let events = events.clone();
    let locks = path_locks.clone();
    let path_key = remote_key(&job.remote);
    tokio::spawn(async move {
        let _path_guard = locks.acquire([path_key]).await;
        transfer::run_upload(
            sftp, id, job.local, job.remote, job.total, decision, cancel, events,
        )
        .await;
        unregister_cancel(&registry, id);
    });
}

fn unregister_cancel(registry: &CancelRegistry, id: TransferId) {
    if let Ok(mut map) = registry.lock() {
        map.remove(&id);
    }
}

fn cancel_all(registry: &CancelRegistry) {
    if let Ok(map) = registry.lock() {
        for flag in map.values() {
            flag.store(true, Ordering::Relaxed);
        }
    }
}

fn cancel_pending_uploads(
    registry: &PendingRegistry,
    cancels: &CancelRegistry,
    events: &flume::Sender<SftpEvent>,
) {
    if let Ok(mut map) = registry.lock() {
        for (id, job) in map.drain() {
            job.cancel.store(true, Ordering::Relaxed);
            unregister_cancel(cancels, id);
            let _ = events.send(SftpEvent::TransferCompleted {
                id,
                outcome: TransferOutcome::Cancelled,
            });
        }
    }
}
