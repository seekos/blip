//! 普通 FTP（被动模式）后端。
//!
//! UI 仍使用同一套 `SftpCommand` / `SftpEvent`，这里只把远端操作翻译成 FTP。
//! 一个 FTP 控制连接一次只能有一个数据流，因此任务通过异步互斥锁串行使用连接；
//! 命令循环本身不会被传输阻塞，取消和断开仍能及时处理。

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use suppaftp::list::{File as FtpFile, ListParser};
use suppaftp::tokio::AsyncFtpStream;
use suppaftp::types::FileType as TransferType;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _, AsyncWriteExt as _};
use tokio::sync::Mutex as AsyncMutex;

use super::local_tree;
use super::session::{is_safe_entry_name, join_remote, remote_parent, sort_entries};
use super::transfer::{MAX_EDITABLE_TEXT_SIZE, Speedometer, decode_text_for_editor};
use super::types::{
    AuthMethod, EntryKind, ErrorContext, ExistingPolicy, FileEntry, RemoteProtocol, SessionConfig,
    SftpCommand, SftpEvent, TransferId, TransferOutcome, UploadConflict, UploadDecision,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const LIST_TIMEOUT: Duration = Duration::from_secs(10);
const CHUNK_SIZE: usize = 64 * 1024;
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

type Client = Arc<AsyncMutex<AsyncFtpStream>>;
type CancelFlag = Arc<AtomicBool>;
type CancelRegistry = Arc<Mutex<HashMap<TransferId, CancelFlag>>>;
type PendingRegistry = Arc<Mutex<HashMap<TransferId, PendingUpload>>>;

struct PendingUpload {
    local: PathBuf,
    remote: String,
    total: u64,
    cancel: CancelFlag,
}

/// 运行一个 FTP 会话，直到断开或整个 worker 关闭。返回 `true` 表示收到 Shutdown。
pub(super) async fn run_session(
    config: SessionConfig,
    commands: &flume::Receiver<SftpCommand>,
    events: &flume::Sender<SftpEvent>,
) -> bool {
    let client = match connect(&config).await {
        Ok(client) => client,
        Err(message) => {
            let _ = events.send(SftpEvent::error(ErrorContext::Connect, message));
            return false;
        }
    };
    let home = {
        let mut ftp = client.lock().await;
        ftp.pwd().await.unwrap_or_else(|_| "/".to_string())
    };
    let _ = events.send(SftpEvent::Connected {
        home,
        upload_sources: vec!["FTP 被动模式".to_string()],
        upload_concurrency: 1,
    });

    let cancels: CancelRegistry = Arc::new(Mutex::new(HashMap::new()));
    let pending: PendingRegistry = Arc::new(Mutex::new(HashMap::new()));
    let mut upload_policy: Option<UploadDecision> = None;

    while let Ok(command) = commands.recv_async().await {
        match command {
            SftpCommand::Connect(_) => {
                let _ = events.send(SftpEvent::error(
                    ErrorContext::Connect,
                    "请先断开当前 FTP 连接",
                ));
            }
            SftpCommand::Disconnect => {
                cancel_all(&cancels);
                cancel_pending(&pending, &cancels, events);
                let _ = events.send(SftpEvent::Disconnected);
                return false;
            }
            SftpCommand::Shutdown => {
                cancel_all(&cancels);
                cancel_pending(&pending, &cancels, events);
                return true;
            }
            SftpCommand::ResolveHostKey(_) => {}
            SftpCommand::InstallPublicKey { .. } => {
                let _ = events.send(SftpEvent::error(
                    ErrorContext::InstallPublicKey,
                    "FTP 不支持 SSH 公钥认证",
                ));
            }
            SftpCommand::ListDir {
                request_id,
                path,
                fallback,
            } => {
                let client = client.clone();
                let config = config.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let mut ftp = client.lock().await;
                    let listed = list_dir_for_navigation(&mut ftp, &config, &path).await;
                    let result = match (listed, fallback) {
                        (Err(_), Some(fallback)) if fallback != path => {
                            list_dir_for_navigation(&mut ftp, &config, &fallback)
                                .await
                                .map(|entries| (fallback, entries))
                        }
                        (Ok(entries), _) => Ok((path, entries)),
                        (Err(message), _) => Err(message),
                    };
                    match result {
                        Ok((path, entries)) => {
                            let _ = events.send(SftpEvent::DirListed {
                                request_id,
                                path,
                                entries,
                            });
                        }
                        Err(message) => {
                            let _ = events.send(SftpEvent::error(ErrorContext::List, message));
                        }
                    }
                });
            }
            SftpCommand::Download {
                id,
                remote,
                local,
                total,
                resume,
            } => {
                let cancel = register_cancel(&cancels, id);
                spawn_download(
                    client.clone(),
                    id,
                    remote,
                    local,
                    total,
                    resume,
                    cancel,
                    cancels.clone(),
                    events.clone(),
                );
            }
            SftpCommand::Upload {
                id,
                local,
                remote,
                total,
                ..
            } => {
                let cancel = register_cancel(&cancels, id);
                let job = PendingUpload {
                    local,
                    remote,
                    total,
                    cancel,
                };
                probe_or_upload(
                    client.clone(),
                    id,
                    job,
                    upload_policy,
                    pending.clone(),
                    cancels.clone(),
                    events.clone(),
                );
            }
            SftpCommand::UploadOverwrite {
                id,
                local,
                remote,
                total,
            } => {
                let cancel = register_cancel(&cancels, id);
                spawn_upload(
                    client.clone(),
                    id,
                    PendingUpload {
                        local,
                        remote,
                        total,
                        cancel,
                    },
                    UploadDecision::Overwrite,
                    cancels.clone(),
                    events.clone(),
                );
            }
            SftpCommand::ResolveUpload {
                id,
                decision,
                apply_to_all,
            } => {
                if apply_to_all {
                    upload_policy = Some(decision);
                }
                let job = pending.lock().ok().and_then(|mut jobs| jobs.remove(&id));
                let Some(job) = job else {
                    continue;
                };
                if decision == UploadDecision::Skip {
                    unregister_cancel(&cancels, id);
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Skipped,
                    });
                } else {
                    spawn_upload(
                        client.clone(),
                        id,
                        job,
                        decision,
                        cancels.clone(),
                        events.clone(),
                    );
                }
            }
            SftpCommand::DownloadDir {
                id,
                remote,
                local,
                on_existing,
            } => {
                let cancel = register_cancel(&cancels, id);
                spawn_download_dir(
                    client.clone(),
                    id,
                    remote,
                    local,
                    on_existing,
                    cancel,
                    cancels.clone(),
                    events.clone(),
                );
            }
            SftpCommand::UploadDir {
                id,
                local,
                remote,
                on_existing,
            } => {
                let cancel = register_cancel(&cancels, id);
                spawn_upload_dir(
                    client.clone(),
                    id,
                    local,
                    remote,
                    on_existing,
                    cancel,
                    cancels.clone(),
                    events.clone(),
                );
            }
            SftpCommand::Cancel(id) => {
                if let Ok(map) = cancels.lock()
                    && let Some(cancel) = map.get(&id)
                {
                    cancel.store(true, Ordering::Relaxed);
                }
                if let Ok(mut jobs) = pending.lock()
                    && jobs.remove(&id).is_some()
                {
                    unregister_cancel(&cancels, id);
                    let _ = events.send(SftpEvent::TransferCompleted {
                        id,
                        outcome: TransferOutcome::Cancelled,
                    });
                }
            }
            SftpCommand::ReadText { path } => {
                let client = client.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let mut ftp = client.lock().await;
                    match read_text(&mut ftp, &path).await {
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
                let client = client.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let mut ftp = client.lock().await;
                    match write_text(&mut ftp, &path, &content).await {
                        Ok(()) => {
                            let _ = events.send(SftpEvent::TextSaved { path });
                        }
                        Err(message) => {
                            let _ = events.send(SftpEvent::error(ErrorContext::WriteText, message));
                        }
                    }
                });
            }
            SftpCommand::Mkdir { path } => {
                let client = client.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let result = client
                        .lock()
                        .await
                        .mkdir(&path)
                        .await
                        .map_err(|error| format!("创建目录 {path} 失败：{error}"));
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
                let client = client.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let mut ftp = client.lock().await;
                    let mut removed = 0usize;
                    let mut failures = Vec::new();
                    for (path, is_dir) in &entries {
                        match remove_path(&mut ftp, path, *is_dir).await {
                            Ok(count) => removed += count,
                            Err(message) => failures.push(message),
                        }
                    }
                    if failures.is_empty() {
                        let _ = events.send(SftpEvent::Notice(format!(
                            "已删除 {} 项（共 {removed} 个文件/目录）",
                            entries.len()
                        )));
                    } else {
                        let _ = events.send(SftpEvent::error(
                            ErrorContext::Remove,
                            format!(
                                "删除完成 {}/{} 项；失败：{}",
                                entries.len() - failures.len(),
                                entries.len(),
                                failures.into_iter().take(2).collect::<Vec<_>>().join("；")
                            ),
                        ));
                    }
                });
            }
            SftpCommand::Rename { from, to } => {
                let client = client.clone();
                let events = events.clone();
                tokio::spawn(async move {
                    let result = client
                        .lock()
                        .await
                        .rename(&from, &to)
                        .await
                        .map_err(|error| format!("重命名 {from} 失败：{error}"));
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
        }
    }
    true
}

async fn connect(config: &SessionConfig) -> Result<Client, String> {
    connect_stream(config)
        .await
        .map(|ftp| Arc::new(AsyncMutex::new(ftp)))
}

async fn connect_stream(config: &SessionConfig) -> Result<AsyncFtpStream, String> {
    if config.protocol != RemoteProtocol::Ftp {
        return Err("内部错误：FTP 后端收到非 FTP 配置".to_string());
    }
    let AuthMethod::Password(password) = &config.auth else {
        return Err("FTP 仅支持用户名和密码认证".to_string());
    };
    let address = format!("{}:{}", config.host, config.port);
    let mut ftp = tokio::time::timeout(CONNECT_TIMEOUT, AsyncFtpStream::connect(&address))
        .await
        .map_err(|_| format!("连接 FTP 服务器超时：{address}"))?
        .map_err(|error| format!("连接 FTP 服务器失败：{error}"))?;
    ftp.set_passive_nat_workaround(true);
    ftp.login(&config.username, password)
        .await
        .map_err(|error| format!("FTP 登录失败：{error}"))?;
    ftp.transfer_type(TransferType::Binary)
        .await
        .map_err(|error| format!("切换 FTP 二进制传输模式失败：{error}"))?;
    Ok(ftp)
}

async fn list_dir(ftp: &mut AsyncFtpStream, path: &str) -> Result<Vec<FileEntry>, String> {
    let original = match ftp.pwd().await {
        Ok(path) => path,
        Err(_) => return list_with_commands(ftp, path, Some(path)).await,
    };
    if original != path && ftp.cwd(path).await.is_err() {
        return list_with_commands(ftp, path, Some(path)).await;
    }

    let result = list_with_commands(ftp, path, None).await;
    if original != path
        && let Err(error) = ftp.cwd(&original).await
    {
        return Err(format!("读取 FTP 目录后无法返回 {original}：{error}"));
    }
    result
}

/// FTP 服务器对带路径参数的 MLSD/LIST 支持并不一致。优先 CWD 到目标目录并传入 None；
/// 不支持 PWD/CWD 的旧服务器则仍可使用 Some(path) 后备。
async fn list_with_commands(
    ftp: &mut AsyncFtpStream,
    path: &str,
    command_path: Option<&str>,
) -> Result<Vec<FileEntry>, String> {
    let mut errors = Vec::new();
    let mut successful_empty_listing = false;

    match ftp.list(command_path).await {
        Ok(lines) => {
            successful_empty_listing |= lines.is_empty();
            if let Some(entries) = usable_listing(path, lines, false) {
                return Ok(entries);
            }
        }
        Err(error) => errors.push(format!("LIST: {error}")),
    }

    match ftp.mlsd(command_path).await {
        Ok(lines) => {
            successful_empty_listing |= lines.is_empty();
            if let Some(entries) = usable_listing(path, lines, true) {
                return Ok(entries);
            }
        }
        Err(error) => errors.push(format!("MLSD: {error}")),
    }

    match ftp.nlst(command_path).await {
        Ok(names) => entries_from_names(ftp, path, names).await,
        Err(_) if successful_empty_listing => Ok(Vec::new()),
        Err(error) => {
            errors.push(format!("NLST: {error}"));
            Err(format!("读取 FTP 目录 {path} 失败：{}", errors.join("；")))
        }
    }
}

/// 数据连接偶尔会在刚切换目录时尚未就绪。UI 导航若第一次得到空结果或错误，就在
/// 同一连接内短暂等待并重试，避免用户重新连接或重复进入目录。
async fn list_dir_for_navigation(
    ftp: &mut AsyncFtpStream,
    config: &SessionConfig,
    path: &str,
) -> Result<Vec<FileEntry>, String> {
    let first = match tokio::time::timeout(LIST_TIMEOUT, list_dir(ftp, path)).await {
        Ok(result) => result,
        Err(_) => {
            *ftp = connect_stream(config).await?;
            tokio::time::timeout(LIST_TIMEOUT, list_dir(ftp, path))
                .await
                .map_err(|_| format!("读取 FTP 目录 {path} 超时"))?
        }
    };
    if first.as_ref().is_ok_and(|entries| !entries.is_empty()) {
        return first;
    }

    tokio::time::sleep(Duration::from_millis(200)).await;
    match tokio::time::timeout(LIST_TIMEOUT, list_dir(ftp, path)).await {
        Err(_) if first.is_ok() => first,
        Err(_) => Err(format!("读取 FTP 目录 {path} 超时")),
        Ok(result) => result,
    }
}

fn usable_listing(path: &str, lines: Vec<String>, machine: bool) -> Option<Vec<FileEntry>> {
    let entries = entries_from_files(path, parse_listing(lines, machine));
    (!entries.is_empty()).then_some(entries)
}

fn entries_from_files(path: &str, files: Vec<FtpFile>) -> Vec<FileEntry> {
    let mut entries = Vec::new();
    for file in files {
        let name = file.name().to_string();
        if !is_safe_ftp_entry_name(&name) {
            continue;
        }
        entries.push(entry_from_file(path, name, &file));
    }
    sort_entries(&mut entries);
    entries
}

fn entry_from_file(path: &str, name: String, file: &FtpFile) -> FileEntry {
    let kind = if file.is_directory() {
        EntryKind::Directory
    } else if file.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::File
    };
    let modified = (file.modified() != SystemTime::UNIX_EPOCH).then(|| file.modified());
    FileEntry {
        path: join_remote(path, &name),
        hidden: name.starts_with('.'),
        name,
        kind,
        size: file.size() as u64,
        modified,
        mode: None,
    }
}

async fn entries_from_names(
    ftp: &mut AsyncFtpStream,
    path: &str,
    names: Vec<String>,
) -> Result<Vec<FileEntry>, String> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for raw_name in names {
        let Some(name) = nlst_entry_name(&raw_name) else {
            continue;
        };
        if !seen.insert(name.clone()) {
            continue;
        }

        let full_path = join_remote(path, &name);
        if let Ok(line) = ftp.mlst(Some(&full_path)).await
            && let Ok(file) = ListParser::parse_mlst(&line)
        {
            entries.push(entry_from_file(path, name, &file));
            continue;
        }

        let size = ftp.size(&full_path).await.ok().map(|size| size as u64);
        let kind = if size.is_some() {
            EntryKind::File
        } else {
            probe_directory(ftp, &full_path).await?
        };
        entries.push(FileEntry {
            path: full_path,
            hidden: name.starts_with('.'),
            name,
            kind,
            size: size.unwrap_or(0),
            modified: None,
            mode: None,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

async fn probe_directory(ftp: &mut AsyncFtpStream, path: &str) -> Result<EntryKind, String> {
    let Ok(original) = ftp.pwd().await else {
        return Ok(EntryKind::File);
    };
    if ftp.cwd(path).await.is_err() {
        return Ok(EntryKind::File);
    }
    ftp.cwd(&original)
        .await
        .map_err(|error| format!("探测 FTP 目录后无法返回 {original}：{error}"))?;
    Ok(EntryKind::Directory)
}

fn nlst_entry_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next()?;
    is_safe_ftp_entry_name(name).then(|| name.to_string())
}

fn is_safe_ftp_entry_name(name: &str) -> bool {
    is_safe_entry_name(name) && !name.contains('\\')
}

fn parse_listing(lines: Vec<String>, machine: bool) -> Vec<FtpFile> {
    lines
        .into_iter()
        .filter_map(|line| {
            if machine {
                ListParser::parse_mlsd(&line).ok()
            } else {
                FtpFile::try_from(line).ok()
            }
        })
        .collect()
}

async fn remote_entry(ftp: &mut AsyncFtpStream, path: &str) -> Result<Option<FileEntry>, String> {
    let name = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    if name.is_empty() {
        return Ok(Some(FileEntry {
            name: "/".to_string(),
            path: "/".to_string(),
            kind: super::types::EntryKind::Directory,
            hidden: false,
            size: 0,
            modified: None,
            mode: None,
        }));
    }
    Ok(list_dir(ftp, &remote_parent(path))
        .await?
        .into_iter()
        .find(|entry| entry.name == name))
}

fn probe_or_upload(
    client: Client,
    id: TransferId,
    job: PendingUpload,
    policy: Option<UploadDecision>,
    pending: PendingRegistry,
    cancels: CancelRegistry,
    events: flume::Sender<SftpEvent>,
) {
    tokio::spawn(async move {
        let mut ftp = client.lock().await;
        if job.cancel.load(Ordering::Relaxed) {
            unregister_cancel(&cancels, id);
            return;
        }
        let existing = remote_entry(&mut ftp, &job.remote).await;
        let filepart_path = format!("{}.filepart", job.remote);
        let filepart = remote_entry(&mut ftp, &filepart_path).await;
        let (existing, filepart) = match (existing, filepart) {
            (Ok(existing), Ok(filepart)) => (existing, filepart),
            (Err(message), _) | (_, Err(message)) => {
                let _ = events.send(SftpEvent::TransferCompleted {
                    id,
                    outcome: TransferOutcome::Failed(message),
                });
                unregister_cancel(&cancels, id);
                return;
            }
        };
        drop(ftp);
        let has_conflict = existing.is_some() || filepart.is_some();
        let decision = policy.filter(|_| has_conflict);
        if has_conflict && decision.is_none() {
            let local_path = job.local.clone();
            let remote_path = job.remote.clone();
            let local_size = job.total;
            let Ok(mut jobs) = pending.lock() else {
                let _ = events.send(SftpEvent::TransferCompleted {
                    id,
                    outcome: TransferOutcome::Failed("FTP 冲突队列状态异常".to_string()),
                });
                unregister_cancel(&cancels, id);
                return;
            };
            jobs.insert(id, job);
            drop(jobs);
            let _ = events.send(SftpEvent::UploadConflict(UploadConflict {
                id,
                local_path,
                remote_path,
                local_size,
                existing_size: existing.map(|entry| entry.size),
                filepart_size: filepart.map(|entry| entry.size),
                // FTP 没有可信的远端身份标记；不向用户提供不安全的续传。
                checkpoint_present: false,
            }));
            return;
        }
        spawn_upload(
            client,
            id,
            job,
            decision.unwrap_or(UploadDecision::Overwrite),
            cancels,
            events,
        );
    });
}

fn spawn_upload(
    client: Client,
    id: TransferId,
    job: PendingUpload,
    decision: UploadDecision,
    cancels: CancelRegistry,
    events: flume::Sender<SftpEvent>,
) {
    tokio::spawn(async move {
        let outcome = if decision == UploadDecision::Skip {
            TransferOutcome::Skipped
        } else {
            let mut ftp = client.lock().await;
            match upload_file(
                &mut ftp,
                id,
                &job.local,
                &job.remote,
                job.total,
                0,
                job.total,
                &job.cancel,
                &events,
            )
            .await
            {
                Ok(()) => TransferOutcome::Completed,
                Err(_) if job.cancel.load(Ordering::Relaxed) => TransferOutcome::Cancelled,
                Err(message) => TransferOutcome::Failed(message),
            }
        };
        let _ = events.send(SftpEvent::TransferCompleted { id, outcome });
        unregister_cancel(&cancels, id);
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_download(
    client: Client,
    id: TransferId,
    remote: String,
    local: PathBuf,
    total: u64,
    resume: bool,
    cancel: CancelFlag,
    cancels: CancelRegistry,
    events: flume::Sender<SftpEvent>,
) {
    tokio::spawn(async move {
        let mut ftp = client.lock().await;
        let result = download_file(
            &mut ftp, id, &remote, &local, total, resume, 0, total, &cancel, &events,
        )
        .await;
        let outcome = match result {
            Ok(()) => TransferOutcome::Completed,
            Err(_) if cancel.load(Ordering::Relaxed) => TransferOutcome::Cancelled,
            Err(message) => TransferOutcome::Failed(message),
        };
        let _ = events.send(SftpEvent::TransferCompleted { id, outcome });
        unregister_cancel(&cancels, id);
    });
}

#[allow(clippy::too_many_arguments)]
async fn upload_file(
    ftp: &mut AsyncFtpStream,
    id: TransferId,
    local: &Path,
    remote: &str,
    file_total: u64,
    base: u64,
    batch_total: u64,
    cancel: &AtomicBool,
    events: &flume::Sender<SftpEvent>,
) -> Result<(), String> {
    let metadata = tokio::fs::symlink_metadata(local)
        .await
        .map_err(|error| format!("读取 {} 失败：{error}", local.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(format!("{} 不是普通文件", local.display()));
    }
    let temp = format!("{remote}.filepart");
    if let Ok(Some(entry)) = remote_entry(ftp, &temp).await {
        if entry.is_dir() {
            return Err(format!("远端临时路径 {temp} 是目录"));
        }
        let _ = ftp.rm(&temp).await;
    }
    let mut source = tokio::fs::File::open(local)
        .await
        .map_err(|error| format!("打开 {} 失败：{error}", local.display()))?;
    let mut stream = ftp
        .put_with_stream(&temp)
        .await
        .map_err(|error| format!("创建 FTP 临时文件 {temp} 失败：{error}"))?;
    let _ = events.send(SftpEvent::TransferStarted {
        id,
        resumed_from: base,
        total: batch_total,
    });
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut copied = 0u64;
    let mut last_progress = Instant::now();
    let mut speed = Speedometer::new(base);
    loop {
        if cancel.load(Ordering::Relaxed) {
            ftp.abort(stream)
                .await
                .map_err(|error| format!("取消 FTP 上传失败：{error}"))?;
            return Err("已取消".to_string());
        }
        let read = source
            .read(&mut buffer)
            .await
            .map_err(|error| format!("读取 {} 失败：{error}", local.display()))?;
        if read == 0 {
            break;
        }
        stream
            .write_all(&buffer[..read])
            .await
            .map_err(|error| format!("写入 FTP 临时文件 {temp} 失败：{error}"))?;
        copied += read as u64;
        let transferred = base.saturating_add(copied);
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            let _ = events.send(SftpEvent::TransferProgress {
                id,
                transferred,
                total: batch_total,
                speed: speed.sample(transferred),
            });
            last_progress = Instant::now();
        }
    }
    stream
        .finish()
        .await
        .map_err(|error| format!("完成 FTP 上传失败：{error}"))?;
    if copied != file_total {
        return Err(format!(
            "本地文件传输期间发生变化：预计 {file_total} 字节，实际读取 {copied} 字节"
        ));
    }
    replace_remote(ftp, &temp, remote).await?;
    let transferred = base.saturating_add(copied);
    let _ = events.send(SftpEvent::TransferProgress {
        id,
        transferred,
        total: batch_total,
        speed: speed.sample(transferred),
    });
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct DownloadIdentity {
    remote: String,
    size: u64,
    modified_secs: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
async fn download_file(
    ftp: &mut AsyncFtpStream,
    id: TransferId,
    remote: &str,
    local: &Path,
    expected_total: u64,
    resume: bool,
    base: u64,
    batch_total: u64,
    cancel: &AtomicBool,
    events: &flume::Sender<SftpEvent>,
) -> Result<(), String> {
    let remote_meta = remote_entry(ftp, remote)
        .await?
        .ok_or_else(|| format!("远端文件不存在：{remote}"))?;
    if remote_meta.is_dir() {
        return Err(format!("远端路径是目录：{remote}"));
    }
    let total = if remote_meta.size > 0 {
        remote_meta.size
    } else {
        expected_total
    };
    let temp = filepart_path(local);
    let marker = marker_path(&temp);
    let identity = DownloadIdentity {
        remote: remote.to_string(),
        size: total,
        modified_secs: remote_meta
            .modified
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs()),
    };
    let offset = if resume
        && read_marker(&marker).await.as_ref() == Some(&identity)
        && is_regular_file(&temp).await
    {
        tokio::fs::metadata(&temp)
            .await
            .ok()
            .map(|metadata| metadata.len())
            .filter(|length| *length <= total)
            .unwrap_or(0)
    } else {
        0
    };
    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|error| format!("创建本地目录 {} 失败：{error}", parent.display()))?;
    }
    reject_symlink(&temp).await?;
    reject_symlink(&marker).await?;
    tokio::fs::write(
        &marker,
        serde_json::to_vec(&identity).map_err(|error| format!("生成断点信息失败：{error}"))?,
    )
    .await
    .map_err(|error| format!("写入断点信息失败：{error}"))?;
    let progress_total = if batch_total == 0 { total } else { batch_total };
    if total > 0 && offset == total {
        replace_local(&temp, local).await?;
        let _ = tokio::fs::remove_file(marker).await;
        let _ = events.send(SftpEvent::TransferStarted {
            id,
            resumed_from: base + offset,
            total: progress_total,
        });
        let _ = events.send(SftpEvent::TransferProgress {
            id,
            transferred: base + offset,
            total: progress_total,
            speed: 0.0,
        });
        return Ok(());
    }
    let mut target = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(offset == 0)
        .open(&temp)
        .await
        .map_err(|error| format!("打开本地临时文件 {} 失败：{error}", temp.display()))?;
    if offset > 0 {
        target
            .seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|error| format!("定位本地断点失败：{error}"))?;
        ftp.resume_transfer(offset as usize)
            .await
            .map_err(|error| format!("设置 FTP 下载断点失败：{error}"))?;
    }
    let mut stream = ftp
        .retr_as_stream(remote)
        .await
        .map_err(|error| format!("下载 {remote} 失败：{error}"))?;
    let _ = events.send(SftpEvent::TransferStarted {
        id,
        resumed_from: base + offset,
        total: progress_total,
    });
    let mut copied = offset;
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut last_progress = Instant::now();
    let mut speed = Speedometer::new(base + offset);
    loop {
        if cancel.load(Ordering::Relaxed) {
            ftp.abort(stream)
                .await
                .map_err(|error| format!("取消 FTP 下载失败：{error}"))?;
            return Err("已取消".to_string());
        }
        let read = stream
            .read(&mut buffer)
            .await
            .map_err(|error| format!("读取 FTP 数据流失败：{error}"))?;
        if read == 0 {
            break;
        }
        target
            .write_all(&buffer[..read])
            .await
            .map_err(|error| format!("写入本地临时文件失败：{error}"))?;
        copied += read as u64;
        let transferred = base + copied;
        if last_progress.elapsed() >= PROGRESS_INTERVAL {
            let _ = events.send(SftpEvent::TransferProgress {
                id,
                transferred,
                total: progress_total,
                speed: speed.sample(transferred),
            });
            last_progress = Instant::now();
        }
    }
    stream
        .finish()
        .await
        .map_err(|error| format!("完成 FTP 下载失败：{error}"))?;
    target
        .flush()
        .await
        .map_err(|error| format!("刷新本地临时文件失败：{error}"))?;
    if total > 0 && copied != total {
        return Err(format!(
            "下载长度不一致：预计 {total} 字节，实际 {copied} 字节"
        ));
    }
    replace_local(&temp, local).await?;
    let _ = tokio::fs::remove_file(marker).await;
    let transferred = base + copied;
    let _ = events.send(SftpEvent::TransferProgress {
        id,
        transferred,
        total: progress_total,
        speed: speed.sample(transferred),
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_upload_dir(
    client: Client,
    id: TransferId,
    local: PathBuf,
    remote: String,
    policy: ExistingPolicy,
    cancel: CancelFlag,
    cancels: CancelRegistry,
    events: flume::Sender<SftpEvent>,
) {
    tokio::spawn(async move {
        let result = async {
            let tree = local_tree::scan(&local)?;
            let mut ftp = client.lock().await;
            ensure_remote_dir(&mut ftp, &remote).await?;
            for dir in &tree.dirs {
                let path = join_remote(&remote, &local_tree::relative_to_posix(dir));
                ensure_remote_dir(&mut ftp, &path).await?;
            }
            let _ = events.send(SftpEvent::TransferStarted {
                id,
                resumed_from: 0,
                total: tree.total_bytes,
            });
            let _ = events.send(SftpEvent::TransferFileProgress {
                id,
                completed: 0,
                total: tree.files.len(),
            });
            let mut base = 0u64;
            let mut skipped = tree.skipped + tree.symlinks.len();
            for (index, file) in tree.files.iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    return Err("已取消".to_string());
                }
                let target = join_remote(&remote, &local_tree::relative_to_posix(&file.relative));
                if policy == ExistingPolicy::Skip
                    && remote_entry(&mut ftp, &target).await?.is_some()
                {
                    skipped += 1;
                    base += file.size;
                } else {
                    let _ = events.send(SftpEvent::TransferActiveFiles {
                        id,
                        files: vec![file.relative.to_string_lossy().to_string()],
                    });
                    upload_file(
                        &mut ftp,
                        id,
                        &file.local,
                        &target,
                        file.size,
                        base,
                        tree.total_bytes,
                        &cancel,
                        &events,
                    )
                    .await?;
                    base += file.size;
                }
                let _ = events.send(SftpEvent::TransferFileProgress {
                    id,
                    completed: index + 1,
                    total: tree.files.len(),
                });
            }
            let _ = events.send(SftpEvent::TransferActiveFiles { id, files: vec![] });
            Ok::<_, String>(skipped)
        }
        .await;
        let outcome = match result {
            Ok(0) => TransferOutcome::Completed,
            Ok(skipped) => TransferOutcome::Partial(format!("跳过 {skipped} 项")),
            Err(_) if cancel.load(Ordering::Relaxed) => TransferOutcome::Cancelled,
            Err(message) => TransferOutcome::Failed(message),
        };
        let _ = events.send(SftpEvent::TransferCompleted { id, outcome });
        unregister_cancel(&cancels, id);
    });
}

#[derive(Default)]
struct RemoteTree {
    dirs: Vec<PathBuf>,
    files: Vec<(String, PathBuf, u64)>,
    total_bytes: u64,
    skipped: usize,
}

async fn scan_remote(ftp: &mut AsyncFtpStream, root: &str) -> Result<RemoteTree, String> {
    let mut tree = RemoteTree::default();
    let mut pending = vec![(root.to_string(), PathBuf::new(), 0usize)];
    while let Some((path, relative, depth)) = pending.pop() {
        let entries = match list_dir(ftp, &path).await {
            Ok(entries) => entries,
            Err(error) if depth > 0 => {
                log::warn!("跳过 FTP 目录 {path}：{error}");
                tree.skipped += 1;
                continue;
            }
            Err(error) => return Err(error),
        };
        for entry in entries {
            let child = relative.join(&entry.name);
            match entry.kind {
                super::types::EntryKind::Directory if depth < 63 => {
                    tree.dirs.push(child.clone());
                    pending.push((entry.path, child, depth + 1));
                }
                super::types::EntryKind::Directory => tree.skipped += 1,
                super::types::EntryKind::File => {
                    tree.total_bytes += entry.size;
                    tree.files.push((entry.path, child, entry.size));
                }
                super::types::EntryKind::Symlink => tree.skipped += 1,
            }
        }
    }
    tree.dirs.sort_by_key(|path| path.components().count());
    Ok(tree)
}

#[allow(clippy::too_many_arguments)]
fn spawn_download_dir(
    client: Client,
    id: TransferId,
    remote: String,
    local: PathBuf,
    policy: ExistingPolicy,
    cancel: CancelFlag,
    cancels: CancelRegistry,
    events: flume::Sender<SftpEvent>,
) {
    tokio::spawn(async move {
        let result = async {
            let mut ftp = client.lock().await;
            let tree = scan_remote(&mut ftp, &remote).await?;
            ensure_local_dir(&local).await?;
            for dir in &tree.dirs {
                ensure_safe_relative(dir)?;
                ensure_local_dir(&local.join(dir)).await?;
            }
            let _ = events.send(SftpEvent::TransferStarted {
                id,
                resumed_from: 0,
                total: tree.total_bytes,
            });
            let _ = events.send(SftpEvent::TransferFileProgress {
                id,
                completed: 0,
                total: tree.files.len(),
            });
            let mut base = 0u64;
            let mut skipped = tree.skipped;
            for (index, (source, relative, size)) in tree.files.iter().enumerate() {
                ensure_safe_relative(relative)?;
                let target = local.join(relative);
                if policy == ExistingPolicy::Skip && target.exists() {
                    skipped += 1;
                    base += size;
                } else {
                    let _ = events.send(SftpEvent::TransferActiveFiles {
                        id,
                        files: vec![relative.to_string_lossy().to_string()],
                    });
                    download_file(
                        &mut ftp,
                        id,
                        source,
                        &target,
                        *size,
                        true,
                        base,
                        tree.total_bytes,
                        &cancel,
                        &events,
                    )
                    .await?;
                    base += size;
                }
                let _ = events.send(SftpEvent::TransferFileProgress {
                    id,
                    completed: index + 1,
                    total: tree.files.len(),
                });
            }
            let _ = events.send(SftpEvent::TransferActiveFiles { id, files: vec![] });
            Ok::<_, String>(skipped)
        }
        .await;
        let outcome = match result {
            Ok(0) => TransferOutcome::Completed,
            Ok(skipped) => TransferOutcome::Partial(format!("跳过 {skipped} 项")),
            Err(_) if cancel.load(Ordering::Relaxed) => TransferOutcome::Cancelled,
            Err(message) => TransferOutcome::Failed(message),
        };
        let _ = events.send(SftpEvent::TransferCompleted { id, outcome });
        unregister_cancel(&cancels, id);
    });
}

async fn ensure_remote_dir(ftp: &mut AsyncFtpStream, path: &str) -> Result<(), String> {
    if remote_entry(ftp, path)
        .await?
        .is_some_and(|entry| entry.is_dir())
    {
        return Ok(());
    }
    ftp.mkdir(path)
        .await
        .map_err(|error| format!("创建 FTP 目录 {path} 失败：{error}"))
}

async fn read_text(ftp: &mut AsyncFtpStream, path: &str) -> Result<String, String> {
    let entry = remote_entry(ftp, path)
        .await?
        .ok_or_else(|| format!("远端文件不存在：{path}"))?;
    if entry.size > MAX_EDITABLE_TEXT_SIZE {
        return Err("文件超过 2 MiB，请下载后使用本地编辑器".to_string());
    }
    let mut stream = ftp
        .retr_as_stream(path)
        .await
        .map_err(|error| format!("读取 {path} 失败：{error}"))?;
    let mut bytes = Vec::new();
    stream
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("读取 {path} 失败：{error}"))?;
    stream
        .finish()
        .await
        .map_err(|error| format!("完成读取 {path} 失败：{error}"))?;
    if bytes.len() as u64 > MAX_EDITABLE_TEXT_SIZE {
        return Err("文件超过 2 MiB，请下载后使用本地编辑器".to_string());
    }
    Ok(decode_text_for_editor(&bytes))
}

async fn write_text(ftp: &mut AsyncFtpStream, path: &str, content: &str) -> Result<(), String> {
    if content.len() as u64 > MAX_EDITABLE_TEXT_SIZE {
        return Err("编辑后的内容超过 2 MiB，无法保存".to_string());
    }
    let temp = format!("{path}.blip-editpart");
    let _ = ftp.rm(&temp).await;
    let mut stream = ftp
        .put_with_stream(&temp)
        .await
        .map_err(|error| format!("创建 FTP 临时文件失败：{error}"))?;
    stream
        .write_all(content.as_bytes())
        .await
        .map_err(|error| format!("写入 FTP 临时文件失败：{error}"))?;
    stream
        .finish()
        .await
        .map_err(|error| format!("完成 FTP 文本上传失败：{error}"))?;
    replace_remote(ftp, &temp, path).await
}

async fn replace_remote(ftp: &mut AsyncFtpStream, temp: &str, target: &str) -> Result<(), String> {
    if ftp.rename(temp, target).await.is_ok() {
        return Ok(());
    }
    let existing = remote_entry(ftp, target).await?;
    if existing.is_none() {
        return ftp
            .rename(temp, target)
            .await
            .map_err(|error| format!("完成 FTP 文件替换失败：{error}"));
    }
    let backup = format!("{target}.blip-backup-{}", std::process::id());
    let _ = ftp.rm(&backup).await;
    ftp.rename(target, &backup)
        .await
        .map_err(|error| format!("保护原 FTP 文件失败：{error}"))?;
    if let Err(error) = ftp.rename(temp, target).await {
        let _ = ftp.rename(backup.as_str(), target).await;
        return Err(format!("替换 FTP 文件失败，已尝试恢复原文件：{error}"));
    }
    let _ = ftp.rm(&backup).await;
    Ok(())
}

async fn remove_path(ftp: &mut AsyncFtpStream, path: &str, is_dir: bool) -> Result<usize, String> {
    if !is_dir {
        ftp.rm(path)
            .await
            .map_err(|error| format!("删除 {path} 失败：{error}"))?;
        return Ok(1);
    }
    let mut total = 1usize;
    let mut stack = vec![(path.to_string(), false)];
    while let Some((current, visited)) = stack.pop() {
        if visited {
            ftp.rmdir(&current)
                .await
                .map_err(|error| format!("删除目录 {current} 失败：{error}"))?;
            continue;
        }
        stack.push((current.clone(), true));
        for entry in list_dir(ftp, &current).await? {
            total += 1;
            if entry.is_dir() {
                stack.push((entry.path, false));
            } else {
                ftp.rm(&entry.path)
                    .await
                    .map_err(|error| format!("删除 {} 失败：{error}", entry.path))?;
            }
        }
    }
    Ok(total)
}

fn register_cancel(registry: &CancelRegistry, id: TransferId) -> CancelFlag {
    let flag = Arc::new(AtomicBool::new(false));
    if let Ok(mut map) = registry.lock() {
        map.insert(id, flag.clone());
    }
    flag
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

fn cancel_pending(
    pending: &PendingRegistry,
    cancels: &CancelRegistry,
    events: &flume::Sender<SftpEvent>,
) {
    if let Ok(mut jobs) = pending.lock() {
        for (id, job) in jobs.drain() {
            job.cancel.store(true, Ordering::Relaxed);
            unregister_cancel(cancels, id);
            let _ = events.send(SftpEvent::TransferCompleted {
                id,
                outcome: TransferOutcome::Cancelled,
            });
        }
    }
}

fn filepart_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".filepart");
    PathBuf::from(value)
}

fn marker_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".blip-meta");
    PathBuf::from(value)
}

async fn read_marker(path: &Path) -> Option<DownloadIdentity> {
    serde_json::from_slice(&tokio::fs::read(path).await.ok()?).ok()
}

async fn is_regular_file(path: &Path) -> bool {
    tokio::fs::symlink_metadata(path)
        .await
        .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
}

async fn reject_symlink(path: &Path) -> Result<(), String> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("拒绝写入符号链接：{}", path.display()))
        }
        Ok(_) | Err(_) => Ok(()),
    }
}

async fn replace_local(temp: &Path, target: &Path) -> Result<(), String> {
    reject_symlink(target).await?;
    if tokio::fs::rename(temp, target).await.is_ok() {
        return Ok(());
    }
    let backup = marker_path(target);
    let _ = tokio::fs::remove_file(&backup).await;
    if target.exists() {
        tokio::fs::rename(target, &backup)
            .await
            .map_err(|error| format!("保护原文件 {} 失败：{error}", target.display()))?;
    }
    if let Err(error) = tokio::fs::rename(temp, target).await {
        if backup.exists() {
            let _ = tokio::fs::rename(&backup, target).await;
        }
        return Err(format!("替换本地文件 {} 失败：{error}", target.display()));
    }
    let _ = tokio::fs::remove_file(backup).await;
    Ok(())
}

fn ensure_safe_relative(path: &Path) -> Result<(), String> {
    if path
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
    {
        Ok(())
    } else {
        Err(format!("服务器返回了危险路径：{}", path.display()))
    }
}

async fn ensure_local_dir(path: &Path) -> Result<(), String> {
    reject_symlink(path).await?;
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|error| format!("创建本地目录 {} 失败：{error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_machine_listing_and_rejects_escape_names() {
        let files = parse_listing(
            vec![
                "type=dir;modify=20261119010000; folder".to_string(),
                "type=file;size=12;modify=20261119010000; report.txt".to_string(),
                "type=file;size=1; ../escape".to_string(),
            ],
            true,
        );
        assert_eq!(files.len(), 3);
        assert!(files[0].is_directory());
        assert_eq!(files[1].size(), 12);
        assert!(!is_safe_ftp_entry_name(files[2].name()));
    }

    #[test]
    fn unusable_human_listing_falls_through_to_machine_listing() {
        assert!(usable_listing("/", vec!["type=unsupported; broken".into()], false).is_none());

        let entries = usable_listing(
            "/",
            vec![
                "type=dir;modify=20261119010000; folder".into(),
                "type=file;size=12;modify=20261119010000; report.txt".into(),
            ],
            true,
        )
        .expect("MLSD 格式应该能作为 LIST 的后备");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "folder");
        assert_eq!(entries[1].name, "report.txt");
    }

    #[test]
    fn nlst_accepts_full_paths_but_keeps_only_safe_names() {
        assert_eq!(
            nlst_entry_name("/home/user/report.txt"),
            Some("report.txt".into())
        );
        assert_eq!(nlst_entry_name(r"C:\share\folder\"), Some("folder".into()));
        assert_eq!(nlst_entry_name("."), None);
        assert_eq!(nlst_entry_name(".."), None);
    }

    #[test]
    fn download_paths_must_stay_relative() {
        assert!(ensure_safe_relative(Path::new("a/b.txt")).is_ok());
        assert!(ensure_safe_relative(Path::new("../outside")).is_err());
        assert!(ensure_safe_relative(Path::new("/absolute")).is_err());
    }
}
