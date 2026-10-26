//! 流式上传 / 下载：64 KiB 分块 + 实时速率 + 100ms 节流 + 断点续传。
//!
//! 关键点：
//!
//! * **分块**：一次只搬 64 KiB，全程不把文件读进内存 —— GB 级文件和几百 KB 的
//!   小文件走同一条路径。
//! * **节流**：进度事件不是每读一个 chunk 就发一次。64 KiB 的 chunk 在千兆网下
//!   每秒会产生上千条消息，GPUI 主线程会被重绘请求淹没。这里按 100ms 节流，
//!   结束前再补一条终值，保证进度条一定能到 100%。
//! * **速率**：worker 侧测（`Speedometer`），UI 直接显示，不再自己按时间戳反推。
//! * **续传**：下载只续写身份匹配的 `<local>.filepart`，绝不把已有目标文件当半成品；
//!   上传续写远端 `.filepart`。取消时保留半成品，完整后才原子替换目标。
//! * **上传永远走临时文件**：数据先写 `<remote>.filepart`，写完整再替换目标。
//!   这样中途取消不会留下一个被截断的目标文件，替换时也不必打开目标文件，
//!   于是「覆盖正在运行的可执行文件」不会撞上 `ETXTBSY`（详见 [`replace_remote`]）。
//! * **整目录上传**（[`run_upload_dir`]）：一条指令传一整棵树，队列里只占一行。
//!   进度按整批字节汇总，取消一次就停掉整批。

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpClientError;
use russh_sftp::client::fs::Metadata as RemoteMetadata;
use russh_sftp::protocol::{FileAttributes, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::local_tree::{self, LocalTree};
use super::remote_tree;
use super::session::join_remote;
use super::types::{ExistingPolicy, SftpEvent, TransferId, TransferOutcome, UploadDecision};

/// 一次搬运的块大小。64 KiB 是「进度条平滑」和「系统调用次数」之间的折中。
const CHUNK_SIZE: usize = 64 * 1024;

/// 进度上报的最小间隔。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// 速率平滑系数：越接近 1 越稳，越接近 0 越灵敏。
const SPEED_SMOOTHING: f64 = 0.7;

/// 两次采样之间的最小间隔（秒）。低于它的样本直接丢弃 —— 除以接近 0 的时间
/// 会把速率算成天文数字。
const MIN_SAMPLE_INTERVAL: f64 = 0.05;

/// 应用内文本编辑器可打开的最大文件。
///
/// 编辑器需要把全部内容放进内存和文本布局引擎，不适合无上限地打开日志或二进制文件。
pub const MAX_EDITABLE_TEXT_SIZE: u64 = 2 * 1024 * 1024;

static NEXT_EDIT_TEMP: AtomicU64 = AtomicU64::new(1);
static NEXT_REMOTE_TEMP: AtomicU64 = AtomicU64::new(1);

fn edit_temp_path(path: &str) -> String {
    let sequence = NEXT_EDIT_TEMP.fetch_add(1, Ordering::Relaxed);
    format!("{path}.macscp-editpart-{}-{sequence}", std::process::id())
}

pub(crate) fn decode_text_for_editor(bytes: &[u8]) -> String {
    // 「以文本打开」是用户的显式选择：非 UTF-8 字节和 NUL 以替换字符显示。
    String::from_utf8_lossy(bytes).replace('\0', "\u{FFFD}")
}

/// 读取一个远端文件供文本编辑器打开，不判断扩展名或文件类型。
pub async fn read_remote_text(sftp: &SftpSession, path: &str) -> Result<String> {
    let metadata = sftp
        .metadata(path)
        .await
        .map_err(|err| anyhow!("读取 {path} 的信息失败：{err}"))?;
    if metadata.size.unwrap_or(0) > MAX_EDITABLE_TEXT_SIZE {
        return Err(anyhow!("文件超过 2 MiB，请下载后使用本地编辑器"));
    }

    let bytes = sftp
        .read(path)
        .await
        .map_err(|err| anyhow!("读取 {path} 失败：{err}"))?;
    if bytes.len() as u64 > MAX_EDITABLE_TEXT_SIZE {
        return Err(anyhow!("文件超过 2 MiB，请下载后使用本地编辑器"));
    }
    Ok(decode_text_for_editor(&bytes))
}

/// 把文本先写到同目录临时文件，完整落盘后再替换目标。
///
/// 这样在断线、超时或写入失败时，原文件仍保持完整。
pub async fn write_remote_text(sftp: &SftpSession, path: &str, content: &str) -> Result<()> {
    if content.len() as u64 > MAX_EDITABLE_TEXT_SIZE {
        return Err(anyhow!("编辑后的内容超过 2 MiB，无法保存"));
    }

    let preserved_mode = sftp
        .metadata(path)
        .await
        .map_err(|err| anyhow!("读取 {path} 的信息失败：{err}"))?
        .permissions;
    let temp = edit_temp_path(path);
    let mut file = sftp
        .create(&temp)
        .await
        .map_err(|err| anyhow!("创建临时文件 {temp} 失败：{err}"))?;

    file.write_all(content.as_bytes())
        .await
        .map_err(|err| anyhow!("写入临时文件 {temp} 失败：{err}"))?;
    file.flush()
        .await
        .map_err(|err| anyhow!("刷新临时文件 {temp} 失败：{err}"))?;
    file.sync_all()
        .await
        .map_err(|err| anyhow!("同步临时文件 {temp} 失败：{err}"))?;
    file.shutdown()
        .await
        .map_err(|err| anyhow!("关闭临时文件 {temp} 失败：{err}"))?;

    let metadata = optional_remote_metadata(sftp, &temp)
        .await?
        .ok_or_else(|| anyhow!("保存校验失败：远端临时文件 {temp} 已消失"))?;
    if metadata.is_dir() || metadata.is_symlink() || metadata.size != Some(content.len() as u64) {
        return Err(anyhow!("保存校验失败：远端临时文件长度或类型异常：{temp}"));
    }
    let expected_hash = format_digest(Sha256::digest(content.as_bytes()));
    let cancel = AtomicBool::new(false);
    if hash_remote_file(sftp, &temp, &cancel).await? != expected_hash {
        return Err(anyhow!(
            "保存校验失败：远端临时文件内容不一致，原文件未被替换"
        ));
    }

    if let Some(mode) = preserved_mode {
        apply_mode(sftp, &temp, mode).await?;
    }
    replace_remote(sftp, &temp, path).await
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 简单的时间节流器：只有距上次放行超过 `interval` 才返回 true。
struct Throttle {
    last: Instant,
    interval: Duration,
}

impl Throttle {
    fn new(interval: Duration) -> Self {
        Self {
            last: Instant::now(),
            interval,
        }
    }

    fn ready(&mut self) -> bool {
        if self.last.elapsed() >= self.interval {
            self.last = Instant::now();
            true
        } else {
            false
        }
    }
}

/// 实时速率表：指数滑动平均，避免数字在个位数和十位数之间跳。
///
/// `sample` 必须传单调递增的「全文件已传输字节数」。
pub struct Speedometer {
    last_at: Instant,
    last_bytes: u64,
    smoothed: f64,
}

impl Speedometer {
    pub fn new(start_bytes: u64) -> Self {
        Self {
            last_at: Instant::now(),
            last_bytes: start_bytes,
            smoothed: 0.,
        }
    }

    /// 记录一个新样本，返回平滑后的速率（字节/秒）。
    ///
    /// 间隔太短（< 50ms）时直接沿用上一次的值：除以一个接近 0 的时间会算出
    /// 天文数字，进度条上的「MB/s」会瞬间飙到 TB 级。
    pub fn sample(&mut self, transferred: u64) -> f64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_at).as_secs_f64();
        if elapsed < MIN_SAMPLE_INTERVAL {
            return self.smoothed;
        }
        self.last_at = now;

        let instant = transferred.saturating_sub(self.last_bytes) as f64 / elapsed;
        self.last_bytes = transferred;

        self.smoothed = if self.smoothed <= 0. {
            instant
        } else {
            self.smoothed * SPEED_SMOOTHING + instant * (1. - SPEED_SMOOTHING)
        };
        self.smoothed
    }

    /// 当前平滑速率（还没采样过就是 0）。
    pub fn current(&self) -> f64 {
        self.smoothed
    }
}

/// 断点续传的偏移决策。
///
/// 规则（纯函数，方便单测）：
///
/// * 不允许续传、或者目标比源还长（内容对不上）→ 0，重头来，多出来的部分会被截断；
/// * 否则 → 目标已有的长度。这个值等于源长度时表示「其实已经传完了」，
///   调用方据此直接判定完成。
pub fn resume_offset(resume: bool, target_len: u64, source_len: u64) -> u64 {
    if !resume || target_len > source_len {
        return 0;
    }
    target_len
}

/// 一次传输的前置检查结果。
pub struct Plan {
    /// 从这里开始传（0 = 从头）。
    pub offset: u64,
    /// 全文件大小。
    pub total: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RemoteIdentity {
    remote: String,
    size: u64,
    modified_secs: Option<u64>,
}

struct DownloadPlan {
    offset: u64,
    total: u64,
    temp: PathBuf,
    marker: PathBuf,
    identity: RemoteIdentity,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct UploadCheckpoint {
    version: u8,
    size: u64,
    sha256: String,
}

struct PreparedUpload {
    file: tokio::fs::File,
    checkpoint: UploadCheckpoint,
    metadata: std::fs::Metadata,
}

// ---------------------------------------------------------------------------
// 下载：远端 → 本地
// ---------------------------------------------------------------------------

/// actor 侧的入口：参数和 `SftpCommand::Download` 的字段一一对应，
/// 打包成结构体只会让 worker 和测试各多写一层解包，因此保持平铺。
#[allow(clippy::too_many_arguments)]
pub async fn run_download(
    sftp: Arc<SftpSession>,
    id: TransferId,
    remote: String,
    local: PathBuf,
    total: u64,
    resume: bool,
    cancel: Arc<AtomicBool>,
    events: flume::Sender<SftpEvent>,
) {
    let result = if cancel.load(Ordering::Relaxed) {
        Err(anyhow!("已取消"))
    } else {
        download_once(&sftp, id, &remote, &local, total, resume, &cancel, &events).await
    };
    let outcome = match result {
        Ok(()) => TransferOutcome::Completed,
        Err(err) => {
            if cancel.load(Ordering::Relaxed) {
                TransferOutcome::Cancelled
            } else {
                TransferOutcome::Failed(err.to_string())
            }
        }
    };

    let _ = events.send(SftpEvent::TransferCompleted { id, outcome });
}

// 同上：只比公开入口少一个 `Arc`，参数照样平铺。
#[allow(clippy::too_many_arguments)]
async fn download_once(
    sftp: &SftpSession,
    id: TransferId,
    remote: &str,
    local: &Path,
    total: u64,
    resume: bool,
    cancel: &AtomicBool,
    events: &flume::Sender<SftpEvent>,
) -> Result<()> {
    let plan = plan_download(sftp, remote, local, total, resume, cancel).await?;
    let mut sink = RowSink { id, events };
    download_file(sftp, remote, local, &plan, cancel, &mut sink).await
}

/// 决定从哪个字节开始写本地文件。
async fn plan_download(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    total: u64,
    resume: bool,
    cancel: &AtomicBool,
) -> Result<DownloadPlan> {
    if let Some(parent) = local.parent() {
        let meta = tokio::fs::symlink_metadata(parent)
            .await
            .map_err(|err| anyhow!("本地目标目录 {} 不可用：{err}", parent.display()))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(anyhow!(
                "本地目标目录不安全或不是目录：{}",
                parent.display()
            ));
        }
    }

    let identity = read_remote_identity(sftp, remote, total).await?;
    let total = identity.size;
    let temp = local_filepart_path(local);
    let marker = local_marker_path(&temp);

    let recorded = read_download_marker(&marker).await;
    let temp_len = tokio::fs::symlink_metadata(&temp)
        .await
        .ok()
        .filter(|meta| !meta.file_type().is_symlink() && meta.is_file())
        .map(|meta| meta.len());
    let mut reusable = resume && recorded.as_ref() == Some(&identity) && temp_len.is_some();
    let mut offset = if reusable {
        resume_offset(true, temp_len.unwrap_or(0), total)
    } else {
        0
    };

    // 元数据相同仍不足以证明半成品属于当前远端内容（SFTP v3 的 mtime 只有秒级）。
    // 真正续传前逐字节校验已有前缀；不匹配就安全地从头开始。
    if reusable
        && offset > 0
        && !download_prefix_matches(sftp, remote, &temp, offset, cancel).await?
    {
        log::warn!("下载断点内容与远端前缀不一致，放弃续传：{remote}");
        reusable = false;
        offset = 0;
    }

    if !reusable {
        // 先截断旧数据，再把标记指向新身份。反过来做时，如果此处被取消或打开远端
        // 失败，下一次会把“旧内容 + 新标记”误认成可续传断点。
        drop(
            open_local_no_follow(&temp, false)
                .await
                .map_err(|err| anyhow!("重置本地半成品 {} 失败：{err}", temp.display()))?,
        );
        write_download_marker(&marker, &identity).await?;
    }
    Ok(DownloadPlan {
        offset,
        total,
        temp,
        marker,
        identity,
    })
}

/// 下载一个文件的核心流程：`plan` 已经由调用方算好。
///
/// 和上传侧的 `upload_file` 对称：整目录下载也走这里，只是换一个 `UploadSink`
/// 实现，把「本文件已写字节」折算成整批进度。
async fn download_file(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    plan: &DownloadPlan,
    cancel: &AtomicBool,
    sink: &mut dyn UploadSink,
) -> Result<()> {
    sink.started(plan.total, plan.offset);

    let mut source = sftp
        .open(remote)
        .await
        .map_err(|err| anyhow!("打开远端文件失败：{err}"))?;

    // 续传：把远端读指针跳到本地已有的长度。
    if plan.offset > 0 {
        source
            .seek(SeekFrom::Start(plan.offset))
            .await
            .map_err(|err| anyhow!("定位远端文件偏移失败：{err}"))?;
    }

    // 从头传才需要 truncate；续传必须用 append，否则前半段会被抹掉。
    let mut target = if plan.offset > 0 {
        open_local_no_follow(&plan.temp, true)
            .await
            .map_err(|err| anyhow!("打开本地半成品 {} 失败：{err}", plan.temp.display()))?
    } else {
        open_local_no_follow(&plan.temp, false)
            .await
            .map_err(|err| anyhow!("创建本地半成品 {} 失败：{err}", plan.temp.display()))?
    };

    let mut digest = Sha256::new();
    if plan.offset > 0 {
        hash_local_prefix(&plan.temp, plan.offset, cancel, &mut digest).await?;
    }

    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut transferred = plan.offset;
    let mut speedometer = Speedometer::new(transferred);
    let mut throttle = Throttle::new(PROGRESS_INTERVAL);

    loop {
        if cancel.load(Ordering::Relaxed) {
            // 半成品**不删**：它正是下次续传的起点。
            drop(target);
            return Err(anyhow!("已取消"));
        }

        let read = source
            .read(&mut buffer)
            .await
            .map_err(|err| anyhow!("读取远端数据失败：{err}"))?;
        if read == 0 {
            break;
        }

        target
            .write_all(&buffer[..read])
            .await
            .map_err(|err| anyhow!("写入本地文件失败：{err}"))?;

        digest.update(&buffer[..read]);
        transferred += read as u64;
        if throttle.ready() {
            sink.progress(transferred, plan.total, speedometer.sample(transferred));
        }
    }

    target
        .flush()
        .await
        .map_err(|err| anyhow!("刷新本地文件失败：{err}"))?;
    target
        .sync_all()
        .await
        .map_err(|err| anyhow!("同步本地文件失败：{err}"))?;
    drop(target);

    if transferred != plan.total {
        return Err(anyhow!(
            "远端文件长度在下载过程中发生变化：预期 {} 字节，实际读取 {transferred} 字节；已保留半成品",
            plan.total
        ));
    }

    let current = read_remote_identity(sftp, remote, plan.total).await?;
    if current != plan.identity {
        return Err(anyhow!(
            "远端文件在下载过程中发生变化，已保留半成品而未覆盖本地文件"
        ));
    }

    // 元数据可能没有 mtime，或者同一秒内发生等长改写。再完整读取一次远端内容，
    // 与本次落盘数据的 SHA-256 对比，避免把撕裂快照发布为本地目标文件。
    let downloaded_hash = format_digest(digest.finalize());
    let remote_hash = hash_remote_file(sftp, remote, cancel).await?;
    if downloaded_hash != remote_hash {
        return Err(anyhow!(
            "远端文件在下载或校验期间发生变化，校验值不一致；已保留半成品"
        ));
    }
    replace_local(&plan.temp, local).await?;
    let _ = tokio::fs::remove_file(&plan.marker).await;

    // 终值：即使最后一次节流没触发，进度条也要走到头。
    sink.progress(
        transferred,
        plan.total.max(transferred),
        speedometer.current(),
    );

    Ok(())
}

pub(crate) fn local_filepart_path(local: &Path) -> PathBuf {
    let mut name = local.as_os_str().to_os_string();
    name.push(FILEPART_SUFFIX);
    PathBuf::from(name)
}

fn local_marker_path(temp: &Path) -> PathBuf {
    let mut name = temp.as_os_str().to_os_string();
    name.push(".json");
    PathBuf::from(name)
}

#[cfg(test)]
pub(crate) async fn seed_download_checkpoint_for_test(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    prefix: &[u8],
) -> Result<()> {
    let identity = read_remote_identity(sftp, remote, prefix.len() as u64).await?;
    let temp = local_filepart_path(local);
    tokio::fs::write(&temp, prefix)
        .await
        .map_err(|err| anyhow!("写测试下载断点失败：{err}"))?;
    write_download_marker(&local_marker_path(&temp), &identity).await
}

async fn download_prefix_matches(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    len: u64,
    cancel: &AtomicBool,
) -> Result<bool> {
    let mut local_file = open_local_read_no_follow(local)
        .await
        .map_err(|err| anyhow!("读取本地下载断点 {} 失败：{err}", local.display()))?;
    let mut remote_file = sftp
        .open(remote)
        .await
        .map_err(|err| anyhow!("打开远端文件校验断点失败：{err}"))?;
    let mut local_buffer = vec![0u8; CHUNK_SIZE];
    let mut remote_buffer = vec![0u8; CHUNK_SIZE];
    let mut checked = 0u64;

    while checked < len {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("已取消"));
        }
        let wanted = usize::try_from((len - checked).min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);
        let local_read = local_file
            .read(&mut local_buffer[..wanted])
            .await
            .map_err(|err| anyhow!("读取本地下载断点失败：{err}"))?;
        let remote_read = remote_file
            .read(&mut remote_buffer[..wanted])
            .await
            .map_err(|err| anyhow!("读取远端断点前缀失败：{err}"))?;
        if local_read != remote_read
            || local_read == 0
            || local_buffer[..local_read] != remote_buffer[..remote_read]
        {
            return Ok(false);
        }
        checked += local_read as u64;
    }
    Ok(true)
}

async fn hash_local_prefix(
    path: &Path,
    len: u64,
    cancel: &AtomicBool,
    digest: &mut Sha256,
) -> Result<()> {
    let mut file = open_local_read_no_follow(path)
        .await
        .map_err(|err| anyhow!("读取本地下载断点 {} 失败：{err}", path.display()))?;
    let mut remaining = len;
    let mut buffer = vec![0u8; CHUNK_SIZE];
    while remaining > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("已取消"));
        }
        let wanted = usize::try_from(remaining.min(CHUNK_SIZE as u64)).unwrap_or(CHUNK_SIZE);
        let read = file
            .read(&mut buffer[..wanted])
            .await
            .map_err(|err| anyhow!("读取本地下载断点失败：{err}"))?;
        if read == 0 {
            return Err(anyhow!("本地下载断点短于记录长度"));
        }
        digest.update(&buffer[..read]);
        remaining -= read as u64;
    }
    Ok(())
}

async fn hash_remote_file(sftp: &SftpSession, path: &str, cancel: &AtomicBool) -> Result<String> {
    let mut file = sftp
        .open(path)
        .await
        .map_err(|err| anyhow!("打开远端文件做完整性校验失败：{err}"))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_SIZE];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("已取消"));
        }
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|err| anyhow!("读取远端文件做完整性校验失败：{err}"))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format_digest(digest.finalize()))
}

fn format_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

async fn read_remote_identity(
    sftp: &SftpSession,
    remote: &str,
    total_hint: u64,
) -> Result<RemoteIdentity> {
    let meta = sftp
        .metadata(remote)
        .await
        .map_err(|err| anyhow!("读取远端文件信息失败：{err}"))?;
    let modified_secs = meta.modified().ok().and_then(|time| {
        time.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_secs())
    });
    Ok(RemoteIdentity {
        remote: remote.to_string(),
        size: meta.size.unwrap_or(total_hint),
        modified_secs,
    })
}

async fn read_download_marker(path: &Path) -> Option<RemoteIdentity> {
    let mut file = open_local_read_no_follow(path).await.ok()?;
    let metadata = file.metadata().await.ok()?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 {
        return None;
    }
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut raw).await.ok()?;
    serde_json::from_slice(&raw).ok()
}

async fn write_download_marker(path: &Path, identity: &RemoteIdentity) -> Result<()> {
    let raw = serde_json::to_vec(identity).map_err(|err| anyhow!("生成下载断点信息失败：{err}"))?;
    let mut file = open_local_no_follow(path, false)
        .await
        .map_err(|err| anyhow!("创建下载断点信息 {} 失败：{err}", path.display()))?;
    file.write_all(&raw)
        .await
        .map_err(|err| anyhow!("写入下载断点信息失败：{err}"))?;
    file.flush()
        .await
        .map_err(|err| anyhow!("刷新下载断点信息失败：{err}"))?;
    file.sync_all()
        .await
        .map_err(|err| anyhow!("同步下载断点信息失败：{err}"))
}

async fn open_local_no_follow(path: &Path, append: bool) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true);
    if append {
        options.append(true);
    } else {
        options.truncate(true);
    }
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    options.open(path).await
}

async fn open_local_read_no_follow(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW);
    options.open(path).await
}

async fn replace_local(temp: &Path, target: &Path) -> Result<()> {
    if tokio::fs::symlink_metadata(target)
        .await
        .is_ok_and(|meta| meta.is_dir())
    {
        return Err(anyhow!(
            "本地同名条目是目录，无法覆盖：{}",
            target.display()
        ));
    }
    tokio::fs::rename(temp, target)
        .await
        .map_err(|err| anyhow!("替换本地文件 {} 失败：{err}", target.display()))?;

    // 文件本身已经 fsync；再同步目录项，降低系统崩溃后 rename 丢失的概率。
    // 某些文件系统不允许打开目录做 fsync，此时目标已经完整发布，只记录警告。
    if let Some(parent) = target.parent() {
        match tokio::fs::File::open(parent).await {
            Ok(directory) => {
                if let Err(err) = directory.sync_all().await {
                    log::warn!(
                        "本地文件已替换，但同步目录 {} 失败：{err}",
                        parent.display()
                    );
                }
            }
            Err(err) => {
                log::warn!(
                    "本地文件已替换，但打开目录 {} 做同步失败：{err}",
                    parent.display()
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 上传：本地 → 远端（.filepart + 原子替换）
// ---------------------------------------------------------------------------

/// 上传期间使用的临时文件后缀。
pub const FILEPART_SUFFIX: &str = ".filepart";

/// `<remote>` 对应的临时文件：同目录、同名 + `.filepart`。
///
/// 必须同目录 —— 跨目录 rename 会退化成「复制 + 删除」，既不原子也慢一倍。
pub fn filepart_path(remote: &str) -> String {
    let mut path = String::with_capacity(remote.len() + FILEPART_SUFFIX.len());
    path.push_str(remote);
    path.push_str(FILEPART_SUFFIX);
    path
}

/// 上传前的远端探针：一次把「要不要问用户」需要的信息全拿到。
#[derive(Clone, Debug)]
pub struct UploadProbe {
    /// 本地文件大小（真实长度；调用方给的 `total` 只作兜底）。
    pub local_size: u64,
    /// 远端同名文件的大小；`None` 表示不存在。
    pub existing_size: Option<u64>,
    /// 远端 `.filepart` 的大小；`None` 表示没有半成品。
    pub filepart_size: Option<u64>,
    /// 远端断点身份凭据是否存在且是普通文件；续传时还会校验其具体内容。
    pub checkpoint_present: bool,
    /// 原文件的权限位（已去掉类型位），传完回填给临时文件。
    pub preserved_mode: Option<u32>,
    /// 远端同名条目是目录：不能上传。
    pub target_is_dir: bool,
}

impl UploadProbe {
    /// 远端已有同名文件或半成品 → 需要用户拍板。
    pub fn has_conflict(&self) -> bool {
        self.existing_size.is_some() || self.filepart_size.is_some()
    }
}

/// 读本地大小 + 两次远端 `stat`。
pub async fn probe_upload(sftp: &SftpSession, local: &Path, remote: &str) -> Result<UploadProbe> {
    let local_meta = tokio::fs::symlink_metadata(local)
        .await
        .map_err(|err| anyhow!("读取本地文件 {} 失败：{err}", local.display()))?;
    if local_meta.file_type().is_symlink() || !local_meta.is_file() {
        return Err(anyhow!(
            "本地源必须是普通文件且不能是符号链接：{}",
            local.display()
        ));
    }
    let local_size = local_meta.len();

    // 只有明确的 NoSuchFile 才表示不存在；权限、超时和协议错误必须中止上传，
    // 否则会绕过覆盖确认。
    let (existing_size, preserved_mode, target_is_dir) =
        match optional_remote_metadata(sftp, remote).await? {
            Some(meta) => (
                Some(meta.size.unwrap_or(0)),
                (!meta.is_symlink())
                    .then(|| meta.permissions.map(|mode| mode & 0o7777))
                    .flatten(),
                meta.is_dir(),
            ),
            None => (None, None, false),
        };

    let temp = filepart_path(remote);
    let filepart_size = match optional_remote_metadata(sftp, &temp).await? {
        Some(meta) if meta.is_dir() || meta.is_symlink() => {
            return Err(anyhow!("远端临时路径不是普通文件，拒绝写入：{temp}"));
        }
        Some(meta) => Some(meta.size.unwrap_or(0)),
        None => None,
    };
    let checkpoint = upload_checkpoint_path(remote);
    let checkpoint_present = match optional_remote_metadata(sftp, &checkpoint).await? {
        Some(meta) if meta.is_dir() || meta.is_symlink() => {
            return Err(anyhow!("远端断点身份路径不是普通文件：{checkpoint}"));
        }
        Some(meta) if meta.size.unwrap_or(u64::MAX) > 16 * 1024 => {
            return Err(anyhow!("远端断点身份文件过大：{checkpoint}"));
        }
        Some(_) => true,
        None => false,
    };

    Ok(UploadProbe {
        local_size,
        existing_size,
        filepart_size,
        checkpoint_present,
        preserved_mode,
        target_is_dir,
    })
}

async fn optional_remote_metadata(
    sftp: &SftpSession,
    path: &str,
) -> Result<Option<RemoteMetadata>> {
    match sftp.symlink_metadata(path).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(SftpClientError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
            Ok(None)
        }
        Err(err) => Err(anyhow!("读取远端路径 {path} 的信息失败：{err}")),
    }
}

fn upload_checkpoint_path(remote: &str) -> String {
    format!("{}.macscp.json", filepart_path(remote))
}

async fn prepare_upload_source(path: &Path, cancel: &AtomicBool) -> Result<PreparedUpload> {
    let mut file = open_local_read_no_follow(path)
        .await
        .map_err(|err| anyhow!("打开本地文件 {} 失败：{err}", path.display()))?;
    let before = file
        .metadata()
        .await
        .map_err(|err| anyhow!("读取本地文件 {} 失败：{err}", path.display()))?;
    if !before.is_file() {
        return Err(anyhow!("本地源不是普通文件：{}", path.display()));
    }

    // 先为源文件生成稳定身份。断点信息绑定内容散列，而不只绑定长度；代价是上传前
    // 多读一遍本地磁盘，但不会额外占用远端带宽，也杜绝旧 `.filepart` 被错误拼接。
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut hashed = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(anyhow!("已取消"));
        }
        let read = file
            .read(&mut buffer)
            .await
            .map_err(|err| anyhow!("读取本地文件 {} 失败：{err}", path.display()))?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        hashed += read as u64;
    }
    let after_hash = file
        .metadata()
        .await
        .map_err(|err| anyhow!("重新读取本地文件 {} 失败：{err}", path.display()))?;
    if hashed != before.len() || !same_local_file(&before, &after_hash) {
        return Err(anyhow!(
            "本地文件在准备上传时发生变化，请保存完成后重试：{}",
            path.display()
        ));
    }
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(|err| anyhow!("重置本地文件读取位置失败：{err}"))?;

    Ok(PreparedUpload {
        file,
        checkpoint: UploadCheckpoint {
            version: 1,
            size: before.len(),
            sha256: format_digest(digest.finalize()),
        },
        metadata: before,
    })
}

fn same_local_file(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.mtime() == after.mtime()
            && before.mtime_nsec() == after.mtime_nsec()
    }
    #[cfg(not(unix))]
    {
        true
    }
}

async fn read_upload_checkpoint(
    sftp: &SftpSession,
    path: &str,
) -> Result<Option<UploadCheckpoint>> {
    let Some(metadata) = optional_remote_metadata(sftp, path).await? else {
        return Ok(None);
    };
    if metadata.is_dir() || metadata.is_symlink() || metadata.size.unwrap_or(u64::MAX) > 16 * 1024 {
        return Err(anyhow!("远端断点身份文件不安全或过大：{path}"));
    }
    let raw = sftp
        .read(path)
        .await
        .map_err(|err| anyhow!("读取远端断点身份文件失败：{err}"))?;
    let checkpoint: UploadCheckpoint =
        serde_json::from_slice(&raw).map_err(|err| anyhow!("远端断点身份文件损坏：{err}"))?;
    if checkpoint.version != 1 {
        return Err(anyhow!("远端断点身份版本不受支持：{}", checkpoint.version));
    }
    Ok(Some(checkpoint))
}

async fn write_upload_checkpoint(
    sftp: &SftpSession,
    path: &str,
    checkpoint: &UploadCheckpoint,
) -> Result<()> {
    let raw =
        serde_json::to_vec(checkpoint).map_err(|err| anyhow!("生成上传断点身份失败：{err}"))?;
    let sequence = NEXT_REMOTE_TEMP.fetch_add(1, Ordering::Relaxed);
    let temp = format!("{path}.tmp-{}-{sequence}", std::process::id());
    let mut file = sftp
        .create(&temp)
        .await
        .map_err(|err| anyhow!("创建远端断点身份临时文件失败：{err}"))?;
    file.write_all(&raw)
        .await
        .map_err(|err| anyhow!("写入远端断点身份失败：{err}"))?;
    file.flush()
        .await
        .map_err(|err| anyhow!("刷新远端断点身份失败：{err}"))?;
    file.sync_all()
        .await
        .map_err(|err| anyhow!("同步远端断点身份失败：{err}"))?;
    file.shutdown()
        .await
        .map_err(|err| anyhow!("关闭远端断点身份失败：{err}"))?;
    replace_remote(sftp, &temp, path).await
}

async fn remove_remote_if_exists(sftp: &SftpSession, path: &str) -> Result<()> {
    match optional_remote_metadata(sftp, path).await? {
        None => Ok(()),
        Some(meta) if meta.is_dir() => Err(anyhow!("远端清理目标是目录：{path}")),
        Some(_) => sftp
            .remove_file(path)
            .await
            .map_err(|err| anyhow!("删除远端文件 {path} 失败：{err}")),
    }
}

#[cfg(test)]
pub(crate) async fn seed_upload_checkpoint_for_test(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
) -> Result<()> {
    let cancel = AtomicBool::new(false);
    let prepared = prepare_upload_source(local, &cancel).await?;
    write_upload_checkpoint(sftp, &upload_checkpoint_path(remote), &prepared.checkpoint).await
}

/// POSIX mode 的类型位是不是「目录」。
fn mode_is_dir(mode: u32) -> bool {
    mode & 0o170_000 == 0o040_000
}

/// 上传的续传偏移决策（纯函数，方便单测）。
///
/// 和下载的 [`resume_offset`] 不同：这里只认「临时文件比本地短」这一种情况。
/// 临时文件比本地长，说明本地文件换过了，前半段内容对不上，只能重传。
pub fn upload_resume_offset(decision: UploadDecision, filepart_len: u64, local_len: u64) -> u64 {
    if decision != UploadDecision::Resume {
        return 0;
    }
    if filepart_len == 0 || filepart_len >= local_len {
        return 0;
    }
    filepart_len
}

/// 上传进度的出口。
///
/// 单文件上传把每个事件原样映射到队列里的一行；整目录上传则把每个文件的进度
/// 折算成「整批已传字节」，所有文件共用一行。搬运逻辑（[`upload_stream`]）
/// 只管往这里喂字节数，不必知道自己在为谁干活。
pub trait UploadSink: Send {
    /// 一个文件即将开始写数据。`resumed_from > 0` 表示这个文件是续传。
    fn started(&mut self, total: u64, resumed_from: u64);
    /// 本文件已写入 `transferred` 字节（含续传偏移），`speed` 是字节/秒。
    fn progress(&mut self, transferred: u64, total: u64, speed: f64);
}

/// 单文件上传：一条 `TransferId` 对应一条队列行。
struct RowSink<'a> {
    id: TransferId,
    events: &'a flume::Sender<SftpEvent>,
}

impl UploadSink for RowSink<'_> {
    fn started(&mut self, total: u64, resumed_from: u64) {
        let _ = self.events.send(SftpEvent::TransferStarted {
            id: self.id,
            resumed_from,
            total,
        });
    }

    fn progress(&mut self, transferred: u64, total: u64, speed: f64) {
        let _ = self.events.send(SftpEvent::TransferProgress {
            id: self.id,
            transferred,
            total,
            speed,
        });
    }
}

/// 整目录上传：所有文件折算成同一行，`base` 是前面那些文件已经传完的字节。
struct BatchSink<'a> {
    id: TransferId,
    events: &'a flume::Sender<SftpEvent>,
    /// 本文件开始前，整批已完成的字节数。
    base: u64,
    /// 整批总字节（扫描时算好，全程不变）。
    total: u64,
    /// 最近一次上报的「整批已传字节」。
    ///
    /// 单个文件失败时，调用方要靠它知道已经推进到哪儿了 —— 否则下一个文件
    /// 会从 `base` 重新开始，进度条当场倒退。
    last: u64,
}

impl<'a> BatchSink<'a> {
    fn new(id: TransferId, events: &'a flume::Sender<SftpEvent>, base: u64, total: u64) -> Self {
        Self {
            id,
            events,
            base,
            total,
            last: base,
        }
    }
}

impl UploadSink for BatchSink<'_> {
    /// 整批只在开始时发一条 `TransferStarted`，单个文件不再单独报。
    fn started(&mut self, _total: u64, _resumed_from: u64) {}

    fn progress(&mut self, transferred: u64, _total: u64, speed: f64) {
        self.last = self.base + transferred;
        let _ = self.events.send(SftpEvent::TransferProgress {
            id: self.id,
            transferred: self.last,
            total: self.total,
            speed,
        });
    }
}

/// 上传一个文件。
///
/// `decision` 由 worker（或冲突弹窗）定好，这里只负责执行：
/// 写 `.filepart` → 回填权限 → 原子替换目标。
/// 和 [`run_download`] 对称：参数与 `SftpCommand::Upload` 的字段一一对应。
#[allow(clippy::too_many_arguments)]
pub async fn run_upload(
    sftp: Arc<SftpSession>,
    id: TransferId,
    local: PathBuf,
    remote: String,
    total: u64,
    decision: UploadDecision,
    cancel: Arc<AtomicBool>,
    events: flume::Sender<SftpEvent>,
) {
    let result = if cancel.load(Ordering::Relaxed) {
        Err(anyhow!("已取消"))
    } else {
        upload_once(
            &sftp, id, &local, &remote, total, decision, &cancel, &events,
        )
        .await
    };
    let outcome = match result {
        Ok(()) => TransferOutcome::Completed,
        Err(err) => {
            if cancel.load(Ordering::Relaxed) {
                TransferOutcome::Cancelled
            } else {
                TransferOutcome::Failed(err.to_string())
            }
        }
    };

    let _ = events.send(SftpEvent::TransferCompleted { id, outcome });
}

// 同上：只比公开入口少一个 `Arc`，参数照样平铺。
#[allow(clippy::too_many_arguments)]
async fn upload_once(
    sftp: &SftpSession,
    id: TransferId,
    local: &Path,
    remote: &str,
    total: u64,
    decision: UploadDecision,
    cancel: &AtomicBool,
    events: &flume::Sender<SftpEvent>,
) -> Result<()> {
    // `Skip` 正常走不到这里（worker 直接判定完成），防御性返回。
    if !decision.is_transfer() {
        return Ok(());
    }

    let probe = probe_upload(sftp, local, remote).await?;
    let mut sink = RowSink { id, events };
    upload_file(
        sftp, local, remote, &probe, total, decision, cancel, &mut sink,
    )
    .await
}

/// 上传一个文件的核心流程：探针已经由调用方跑过。
///
/// 分开是因为整目录上传要先用探针结果判断「这个文件该不该跳过」，
/// 再决定传不传 —— 两次 `stat` 没必要跑两遍。
#[allow(clippy::too_many_arguments)]
async fn upload_file(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    probe: &UploadProbe,
    total_hint: u64,
    decision: UploadDecision,
    cancel: &AtomicBool,
    sink: &mut dyn UploadSink,
) -> Result<()> {
    if probe.target_is_dir {
        return Err(anyhow!("远端同名条目是目录，无法覆盖：{remote}"));
    }

    let mut prepared = prepare_upload_source(local, cancel).await?;
    let total = prepared.checkpoint.size;
    if total != probe.local_size {
        log::debug!(
            "上传排队后源文件大小由 {} 变为 {total}：{}（以已打开文件为准）",
            probe.local_size,
            local.display()
        );
    }
    let _ = total_hint;
    let temp = filepart_path(remote);
    let checkpoint_path = upload_checkpoint_path(remote);

    // 续传：除了长度合理，还必须有身份凭据。批量“对后续冲突应用续传”可能碰到
    // 旧版本留下的裸 `.filepart`；这种情况安全地从头覆盖，而不是让整批无谓失败。
    let offset = if probe.checkpoint_present {
        upload_resume_offset(decision, probe.filepart_size.unwrap_or(0), total)
    } else {
        if decision == UploadDecision::Resume && probe.filepart_size.is_some() {
            log::warn!("远端断点缺少身份凭据，改为从头安全覆盖：{remote}");
        }
        0
    };

    if offset > 0 {
        let recorded = read_upload_checkpoint(sftp, &checkpoint_path)
            .await?
            .ok_or_else(|| anyhow!("远端断点缺少身份信息，不能安全续传；请改用安全覆盖"))?;
        if recorded != prepared.checkpoint {
            return Err(anyhow!(
                "远端断点属于另一版本的本地文件，不能安全续传；请改用安全覆盖"
            ));
        }
    } else {
        write_upload_checkpoint(sftp, &checkpoint_path, &prepared.checkpoint).await?;
    }

    sink.started(total, offset);

    let transferred = upload_stream(
        sftp,
        local,
        &mut prepared,
        &temp,
        &Plan { offset, total },
        cancel,
        sink,
    )
    .await?;
    if transferred != total {
        return Err(anyhow!(
            "本地文件长度在上传过程中发生变化：预期 {total} 字节，实际读取 {transferred} 字节"
        ));
    }

    let remote_temp = optional_remote_metadata(sftp, &temp)
        .await?
        .ok_or_else(|| anyhow!("远端临时文件在发布前消失：{temp}"))?;
    if remote_temp.is_dir() || remote_temp.is_symlink() || remote_temp.size != Some(total) {
        return Err(anyhow!(
            "远端临时文件校验失败：预期 {total} 字节，实际 {} 字节",
            remote_temp.size.unwrap_or(0)
        ));
    }
    let remote_hash = hash_remote_file(sftp, &temp, cancel).await?;
    if remote_hash != prepared.checkpoint.sha256 {
        return Err(anyhow!(
            "远端临时文件内容校验失败，本地源可能在上传期间发生变化；原文件未被替换"
        ));
    }

    // 1) 继承原文件的权限位 —— 可执行位丢了最难查。
    //    失败只警告、不中断：相比「传完的数据不要了」，丢一个权限位是小事。
    if let Some(mode) = probe.preserved_mode
        && let Err(err) = apply_mode(sftp, &temp, mode).await
    {
        log::warn!("回填权限 {mode:o} 到 {temp} 失败：{err:#}");
    }

    // 2) 原子替换。到这一步临时文件已经完整落盘，替换失败也不会丢数据。
    replace_remote(sftp, &temp, remote).await?;
    if let Err(err) = remove_remote_if_exists(sftp, &checkpoint_path).await {
        log::warn!("上传完成但清理断点身份文件失败：{err:#}");
    }
    Ok(())
}

/// 把 `mode` 贴到远端文件上（只发 permissions，不动大小/时间）。
async fn apply_mode(sftp: &SftpSession, path: &str, mode: u32) -> Result<()> {
    let attrs = FileAttributes {
        permissions: Some(mode),
        ..Default::default()
    };
    sftp.set_metadata(path, attrs)
        .await
        .map_err(|err| anyhow!("设置 {path} 的权限失败：{err}"))
}

/// 用临时文件替换目标文件，**绝不写目标文件本身的 inode**。
///
/// 1. 先直接 `rename`。OpenSSH 的 sftp-server 用 `rename(2)` 实现它：目标存在时
///    原子覆盖，而且 `rename(2)` 只改目录项、不打开目标文件 —— 正在运行的可执行
///    文件因此不会被 `ETXTBSY` 拦住。
/// 2. 有些服务器严格照 SFTP v3 规范，目标存在就返回失败。这时先把旧目标原子改名
///    到唯一备份，再把临时文件发布；发布失败则把备份改回。任何失败路径都至少保留
///    一份可恢复的完整原文件，不会出现「旧文件已删、新文件又没发布」的窗口。
async fn replace_remote(sftp: &SftpSession, temp: &str, target: &str) -> Result<()> {
    let Err(rename_err) = sftp.rename(temp, target).await else {
        return Ok(());
    };
    let reason = rename_err.to_string();

    let Some(temp_meta) = optional_remote_metadata(sftp, temp).await? else {
        return Err(anyhow!(
            "替换失败：临时文件 {temp} 已不存在（rename 报错：{reason}）"
        ));
    };
    if temp_meta.is_dir() || temp_meta.is_symlink() {
        return Err(anyhow!("替换失败：临时路径不是普通文件：{temp}"));
    }

    let Some(target_meta) = optional_remote_metadata(sftp, target).await? else {
        return Err(anyhow!("替换 {target} 失败：{reason}"));
    };
    if target_meta.is_dir() {
        return Err(anyhow!("替换失败：远端同名条目是目录：{target}"));
    }

    let backup = unique_remote_backup_path(sftp, target).await?;
    sftp.rename(target, &backup)
        .await
        .map_err(|err| anyhow!("保护原文件失败：无法把 {target} 移到备份 {backup}：{err}"))?;

    match sftp.rename(temp, target).await {
        Ok(()) => {
            if let Err(err) = sftp.remove_file(&backup).await {
                log::warn!("新文件已发布，但清理远端备份 {backup} 失败：{err}");
            }
            Ok(())
        }
        Err(publish_err) => match optional_remote_metadata(sftp, target).await {
            Ok(None) => match sftp.rename(&backup, target).await {
                Ok(()) => Err(anyhow!("发布 {target} 失败：{publish_err}；原文件已恢复")),
                Err(rollback_err) => Err(anyhow!(
                    "发布 {target} 失败：{publish_err}；恢复原文件也失败：{rollback_err}；完整原文件保留在 {backup}"
                )),
            },
            Ok(Some(_)) => Err(anyhow!(
                "发布 {target} 返回失败：{publish_err}；目标路径仍存在，原文件备份保留在 {backup}"
            )),
            Err(check_err) => Err(anyhow!(
                "发布 {target} 失败：{publish_err}；无法确认目标状态：{check_err:#}；原文件备份保留在 {backup}"
            )),
        },
    }
}

async fn unique_remote_backup_path(sftp: &SftpSession, target: &str) -> Result<String> {
    for _ in 0..32 {
        let sequence = NEXT_REMOTE_TEMP.fetch_add(1, Ordering::Relaxed);
        let candidate = format!("{target}.macscp-backup-{}-{sequence}", std::process::id());
        if optional_remote_metadata(sftp, &candidate).await?.is_none() {
            return Ok(candidate);
        }
    }
    Err(anyhow!("无法为 {target} 分配唯一的远端备份路径"))
}

/// 真正的搬运循环：本地 → 远端临时文件。
async fn upload_stream(
    sftp: &SftpSession,
    local: &Path,
    prepared: &mut PreparedUpload,
    temp_remote: &str,
    plan: &Plan,
    cancel: &AtomicBool,
    sink: &mut dyn UploadSink,
) -> Result<u64> {
    let source = &mut prepared.file;

    if plan.offset > 0 {
        source
            .seek(SeekFrom::Start(plan.offset))
            .await
            .map_err(|err| anyhow!("定位本地文件偏移失败：{err}"))?;
    }

    // 从头传用 create（CREATE|TRUNCATE），续传用 WRITE|CREATE 保住已有的字节。
    let mut target = if plan.offset > 0 {
        let mut file = sftp
            .open_with_flags(temp_remote, WRITE_OR_CREATE)
            .await
            .map_err(|err| anyhow!("打开临时文件 {temp_remote} 失败：{err}"))?;
        // 远端写指针也要跳过去：SFTP 的写入偏移来自 `File::pos`，不 seek 就会
        // 从 0 开始覆盖，把前半段写坏。
        file.seek(SeekFrom::Start(plan.offset))
            .await
            .map_err(|err| anyhow!("定位远端临时文件偏移失败：{err}"))?;
        file
    } else {
        sftp.create(temp_remote)
            .await
            .map_err(|err| anyhow!("创建临时文件 {temp_remote} 失败：{err}"))?
    };

    let mut buffer = vec![0u8; CHUNK_SIZE];
    let mut transferred = plan.offset;
    let mut speedometer = Speedometer::new(transferred);
    let mut throttle = Throttle::new(PROGRESS_INTERVAL);

    loop {
        if cancel.load(Ordering::Relaxed) {
            // 临时文件保留：它就是下次续传的起点，目标文件则完全没被碰过。
            // 用 shutdown 而不是直接 drop —— CLOSE 发出去，服务器才会落盘。
            let _ = target.shutdown().await;
            return Err(anyhow!("已取消"));
        }

        let read = source
            .read(&mut buffer)
            .await
            .map_err(|err| anyhow!("读取本地文件失败：{err}"))?;
        if read == 0 {
            break;
        }

        target
            .write_all(&buffer[..read])
            .await
            .map_err(|err| anyhow!("写入远端临时文件失败：{err}"))?;

        transferred += read as u64;
        if throttle.ready() {
            sink.progress(transferred, plan.total, speedometer.sample(transferred));
        }
    }

    target
        .flush()
        .await
        .map_err(|err| anyhow!("刷新远端临时文件失败：{err}"))?;
    // 服务器支持 fsync@openssh.com 时把数据刷到磁盘；不支持时是空操作。
    target
        .sync_all()
        .await
        .map_err(|err| anyhow!("同步远端临时文件失败：{err}"))?;
    // 先关句柄再 rename：有的服务器在还有写句柄时 rename 会报 busy。
    target
        .shutdown()
        .await
        .map_err(|err| anyhow!("关闭远端临时文件失败：{err}"))?;

    // 终值：即使最后一次节流没触发，进度条也要走到头。
    sink.progress(
        transferred,
        plan.total.max(transferred),
        speedometer.current(),
    );

    let after = source
        .metadata()
        .await
        .map_err(|err| anyhow!("重新读取本地文件 {} 失败：{err}", local.display()))?;
    if !same_local_file(&prepared.metadata, &after) {
        return Err(anyhow!(
            "本地文件在上传过程中发生变化，远端原文件未被替换：{}",
            local.display()
        ));
    }

    Ok(transferred)
}

/// 续传上传时用的打开标志：写 + 不存在则创建，**不** truncate。
const WRITE_OR_CREATE: russh_sftp::protocol::OpenFlags =
    russh_sftp::protocol::OpenFlags::CREATE.union(russh_sftp::protocol::OpenFlags::WRITE);

// ---------------------------------------------------------------------------
// 整目录上传：一条指令传一棵树
// ---------------------------------------------------------------------------

/// 上传一整个本地目录。
///
/// 三步：扫本地树 → 在远端建目录（`mkdir -p` 语义）→ 逐个文件走单文件那套流程
/// （探针 / `.filepart` / 原子替换）。队列里只占一行，进度按整批字节汇总。
///
/// 单个文件失败**不中断**整批：剩下的照传，最后把首个错误连同失败个数一起报回去。
/// 传 500 个文件时因为第 3 个没权限就全放弃，比失败本身更让人恼火。
pub async fn run_upload_dir(
    sftp: Arc<SftpSession>,
    id: TransferId,
    local: PathBuf,
    remote: String,
    on_existing: ExistingPolicy,
    cancel: Arc<AtomicBool>,
    events: flume::Sender<SftpEvent>,
) {
    let result = if cancel.load(Ordering::Relaxed) {
        Ok(TransferOutcome::Cancelled)
    } else {
        upload_dir_inner(&sftp, id, &local, &remote, on_existing, &cancel, &events).await
    };
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            if cancel.load(Ordering::Relaxed) {
                TransferOutcome::Cancelled
            } else {
                TransferOutcome::Failed(err.to_string())
            }
        }
    };

    let _ = events.send(SftpEvent::TransferCompleted { id, outcome });
}

async fn upload_dir_inner(
    sftp: &SftpSession,
    id: TransferId,
    local: &Path,
    remote: &str,
    on_existing: ExistingPolicy,
    cancel: &AtomicBool,
    events: &flume::Sender<SftpEvent>,
) -> Result<TransferOutcome> {
    // 扫目录是纯本地 I/O，可能读几千个目录。丢到 blocking 池里，
    // 别占住 runtime 的工作线程（另一条线程还在跑别的传输）。
    let root = local.to_path_buf();
    let tree: LocalTree = tokio::task::spawn_blocking(move || local_tree::scan(&root))
        .await
        .map_err(|err| anyhow!("扫描本地目录失败：{err}"))?
        .map_err(|err| anyhow!("{err}"))?;

    if tree.skipped > 0 {
        log::warn!(
            "上传 {}：跳过 {} 个符号链接 / 特殊文件",
            local.display(),
            tree.skipped
        );
    }
    log::info!(
        "上传目录 {} → {}：{} 个文件 / {} 个子目录，共 {} 字节",
        local.display(),
        remote,
        tree.files.len(),
        tree.dirs.len(),
        tree.total_bytes
    );

    let total = tree.total_bytes;
    let _ = events.send(SftpEvent::TransferStarted {
        id,
        resumed_from: 0,
        total,
    });

    // 目标目录本身 + 树里每个子目录。扫描时已按深度排好，父目录一定在前面。
    // 根目录建不出来就整批没戏，直接失败。
    ensure_dir(sftp, remote).await?;

    let mut done = 0u64;
    let mut uploaded = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();
    // 建不出来的子目录（带结尾 `/`）：它下面的文件不再逐个尝试，
    // 错误已经在目录那一条记过了，否则一个没权限的子目录会刷出几百条失败。
    let mut broken_dirs: Vec<String> = Vec::new();

    for relative in &tree.dirs {
        if cancel.load(Ordering::Relaxed) {
            return Ok(TransferOutcome::Cancelled);
        }
        let name = local_tree::relative_to_posix(relative);
        if let Err(err) = ensure_dir(sftp, &join_remote(remote, &name)).await {
            // 一个没权限的子目录不该毁掉整棵树：记下来，继续建别的。
            failures.push(format!("{name}/：{err:#}"));
            broken_dirs.push(format!("{name}/"));
        }
    }

    for file in &tree.files {
        if cancel.load(Ordering::Relaxed) {
            return Ok(TransferOutcome::Cancelled);
        }

        let name = local_tree::relative_to_posix(&file.relative);
        let target = join_remote(remote, &name);

        // 父目录根本没建出来：跳过，别为它再刷一条失败。
        if broken_dirs.iter().any(|dir| name.starts_with(dir.as_str())) {
            continue;
        }

        let probe = match probe_upload(sftp, &file.local, &target).await {
            Ok(probe) => probe,
            Err(err) => {
                failures.push(format!("{name}：{err}"));
                continue;
            }
        };

        // 「跳过同名」且远端确实有这个文件：一个字节都不动。
        // 但进度要往前走 —— 否则用户看着进度条卡住，不知道是在干活还是卡死了。
        if on_existing == ExistingPolicy::Skip && probe.existing_size.is_some() {
            skipped += 1;
            done += probe.local_size;
            let _ = events.send(SftpEvent::TransferProgress {
                id,
                transferred: done,
                total,
                speed: 0.,
            });
            continue;
        }

        // 整批没有逐文件的“确认这份断点可信”步骤，因此不能只凭长度自动续传；
        // 残留 `.filepart` 可能来自旧版本文件，混接会悄悄产出损坏内容。
        let decision = UploadDecision::Overwrite;

        let mut sink = BatchSink::new(id, events, done, total);
        match upload_file(
            sftp,
            &file.local,
            &target,
            &probe,
            file.size,
            decision,
            cancel,
            &mut sink,
        )
        .await
        {
            Ok(()) => uploaded += 1,
            Err(err) => {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(TransferOutcome::Cancelled);
                }
                failures.push(format!("{name}：{err}"));
            }
        }

        // 成功或半途失败，都以「最后一次上报的位置」为准：进度绝不倒退。
        done = sink.last.max(done);
    }

    let _ = events.send(SftpEvent::TransferProgress {
        id,
        transferred: done,
        total: total.max(done),
        speed: 0.,
    });

    Ok(batch_outcome(uploaded, skipped, &failures, tree.skipped))
}

/// 整批的最终结论：成功传了一部分但另有遗漏时明确显示“部分完成”，不能静默报成功。
fn batch_outcome(
    transferred: usize,
    skipped: usize,
    failures: &[String],
    scan_skipped: usize,
) -> TransferOutcome {
    if let Some(first) = failures.first() {
        let suffix = if failures.len() > 1 {
            format!("（共 {} 处失败）", failures.len())
        } else {
            String::new()
        };
        let message = format!("{first}{suffix}");
        return if transferred > 0 || skipped > 0 {
            TransferOutcome::Partial(message)
        } else {
            TransferOutcome::Failed(message)
        };
    }
    if scan_skipped > 0 {
        return TransferOutcome::Partial(format!(
            "主体内容已完成，但扫描时跳过了 {scan_skipped} 个符号链接、特殊文件或不可读目录"
        ));
    }
    if transferred == 0 && skipped > 0 {
        TransferOutcome::Skipped
    } else {
        TransferOutcome::Completed
    }
}

/// 建远端目录，已经存在算成功（`mkdir -p` 语义）。
///
/// 不猜错误码：`create_dir` 失败后 `stat` 一次，确认那个路径确实是个目录就放过。
/// 各家服务器在「目录已存在」上返回的状态码并不统一（`Failure` /
/// `PermissionDenied` / 自定义 message），匹配状态码或字符串太脆。
async fn ensure_dir(sftp: &SftpSession, path: &str) -> Result<()> {
    let Err(create_err) = sftp.create_dir(path).await else {
        return Ok(());
    };

    match sftp.metadata(path).await {
        // 已经是个目录：这次是合并上传，不是错误。
        Ok(meta) if meta.permissions.is_none_or(mode_is_dir) => Ok(()),
        Ok(_) => Err(anyhow!("远端同名条目不是目录，无法上传文件夹进去：{path}")),
        Err(_) => Err(anyhow!("创建远端目录 {path} 失败：{create_err}")),
    }
}

// ---------------------------------------------------------------------------
// 整目录下载：一条指令取回一棵树
// ---------------------------------------------------------------------------

/// 下载一整个远端目录。
///
/// 和 [`run_upload_dir`] 对称：扫远端树 → 在本地建目录 → 逐个文件走单文件那套流程。
/// 队列里只占一行，进度按整批字节汇总。
///
/// 单个文件失败不中断整批，最后把首个错误连同失败个数一起报回去。
pub async fn run_download_dir(
    sftp: Arc<SftpSession>,
    id: TransferId,
    remote: String,
    local: PathBuf,
    on_existing: ExistingPolicy,
    cancel: Arc<AtomicBool>,
    events: flume::Sender<SftpEvent>,
) {
    let result = if cancel.load(Ordering::Relaxed) {
        Ok(TransferOutcome::Cancelled)
    } else {
        download_dir_inner(&sftp, id, &remote, &local, on_existing, &cancel, &events).await
    };
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(err) => {
            if cancel.load(Ordering::Relaxed) {
                TransferOutcome::Cancelled
            } else {
                TransferOutcome::Failed(err.to_string())
            }
        }
    };

    let _ = events.send(SftpEvent::TransferCompleted { id, outcome });
}

async fn download_dir_inner(
    sftp: &SftpSession,
    id: TransferId,
    remote: &str,
    local: &Path,
    on_existing: ExistingPolicy,
    cancel: &AtomicBool,
    events: &flume::Sender<SftpEvent>,
) -> Result<TransferOutcome> {
    // 扫远端要一路 `read_dir`，全是网络往返 —— 在 runtime 上 await 就好，
    // 千万别搬到 blocking 池里（那里没有 reactor）。
    let tree = remote_tree::scan(sftp, remote)
        .await
        .map_err(|err| anyhow!("{err}"))?;

    if tree.skipped > 0 {
        log::warn!(
            "下载 {remote}：跳过 {} 个符号链接 / 读不到的目录",
            tree.skipped
        );
    }
    log::info!(
        "下载目录 {remote} → {}：{} 个文件 / {} 个子目录，共 {} 字节",
        local.display(),
        tree.files.len(),
        tree.dirs.len(),
        tree.total_bytes
    );

    let total = tree.total_bytes;
    let _ = events.send(SftpEvent::TransferStarted {
        id,
        resumed_from: 0,
        total,
    });

    // 每一级都拒绝符号链接，恶意服务器文件名或本地预置链接都不能把下载写出根目录。
    ensure_safe_download_root(local).await?;

    let mut done = 0u64;
    let mut finished = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<String> = Vec::new();
    // 建不出来的本地子目录：它下面的文件不再逐个尝试（错误已经记在目录那一条）。
    let mut broken_dirs: Vec<PathBuf> = Vec::new();

    for relative in &tree.dirs {
        if cancel.load(Ordering::Relaxed) {
            return Ok(TransferOutcome::Cancelled);
        }
        if let Err(err) = ensure_safe_download_subdir(local, relative).await {
            failures.push(format!("{}/：创建本地目录失败：{err}", relative.display()));
            broken_dirs.push(relative.clone());
        }
    }

    for file in &tree.files {
        if cancel.load(Ordering::Relaxed) {
            return Ok(TransferOutcome::Cancelled);
        }
        if broken_dirs.iter().any(|dir| file.relative.starts_with(dir)) {
            continue;
        }

        let name = local_tree::relative_to_posix(&file.relative);
        let parent = file.relative.parent().unwrap_or_else(|| Path::new(""));
        if let Err(err) = ensure_safe_download_subdir(local, parent).await {
            failures.push(format!("{name}：本地父目录不安全：{err}"));
            continue;
        }
        let target = local.join(&file.relative);

        // 「跳过同名」且本地已经有这个文件：一个字节都不动，进度照走。
        if on_existing == ExistingPolicy::Skip && tokio::fs::metadata(&target).await.is_ok() {
            skipped += 1;
            done += file.size;
            let _ = events.send(SftpEvent::TransferProgress {
                id,
                transferred: done,
                total,
                speed: 0.,
            });
            continue;
        }

        // 只续写带匹配身份标记的 `.filepart`；已有目标文件始终保持不动直到下载完成。
        let plan = match plan_download(sftp, &file.remote, &target, file.size, true, cancel).await {
            Ok(plan) => plan,
            Err(err) => {
                failures.push(format!("{name}：{err}"));
                continue;
            }
        };

        let mut sink = BatchSink::new(id, events, done, total);
        match download_file(sftp, &file.remote, &target, &plan, cancel, &mut sink).await {
            Ok(()) => finished += 1,
            Err(err) => {
                if cancel.load(Ordering::Relaxed) {
                    return Ok(TransferOutcome::Cancelled);
                }
                failures.push(format!("{name}：{err}"));
            }
        }

        // 成功或半途失败，都以「最后一次上报的位置」为准：进度绝不倒退。
        done = sink.last.max(done);
    }

    let _ = events.send(SftpEvent::TransferProgress {
        id,
        transferred: done,
        total: total.max(done),
        speed: 0.,
    });

    Ok(batch_outcome(finished, skipped, &failures, tree.skipped))
}

/// 准备递归下载的根目录。只创建最后一级，避免 `create_dir_all` 沿途跟随符号链接。
async fn ensure_safe_download_root(root: &Path) -> Result<()> {
    match tokio::fs::symlink_metadata(root).await {
        Ok(meta) if meta.file_type().is_symlink() => {
            Err(anyhow!("本地下载目录不能是符号链接：{}", root.display()))
        }
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(anyhow!("本地下载目标不是目录：{}", root.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let parent = root
                .parent()
                .ok_or_else(|| anyhow!("本地下载路径没有父目录"))?;
            let parent_meta = tokio::fs::symlink_metadata(parent)
                .await
                .map_err(|err| anyhow!("读取父目录 {} 失败：{err}", parent.display()))?;
            if parent_meta.file_type().is_symlink() || !parent_meta.is_dir() {
                return Err(anyhow!("本地下载父目录不安全：{}", parent.display()));
            }
            tokio::fs::create_dir(root)
                .await
                .map_err(|err| anyhow!("创建本地目录 {} 失败：{err}", root.display()))
        }
        Err(err) => Err(anyhow!("读取本地目录 {} 失败：{err}", root.display())),
    }
}

/// 在已验证的根目录内逐级创建子目录；任何非普通分量或符号链接都会终止该分支。
async fn ensure_safe_download_subdir(root: &Path, relative: &Path) -> Result<PathBuf> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(anyhow!("远端路径包含非法分量：{}", relative.display()));
        };
        current.push(name);
        match tokio::fs::symlink_metadata(&current).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(anyhow!("拒绝跟随符号链接：{}", current.display()));
            }
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(anyhow!("路径不是目录：{}", current.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tokio::fs::create_dir(&current)
                    .await
                    .map_err(|err| anyhow!("创建 {} 失败：{err}", current.display()))?;
            }
            Err(err) => return Err(anyhow!("读取 {} 失败：{err}", current.display())),
        }
    }
    Ok(current)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 速率表：等间隔采样时应收敛到真实速率附近。
    #[test]
    fn speedometer_converges_to_real_rate() {
        let mut meter = Speedometer::new(0);
        // 第一条样本还没走时间，得到 0 或瞬时值都行 —— 只要求不发散。
        let _ = meter.sample(1024);
        std::thread::sleep(Duration::from_millis(120));
        meter.sample(1024 + 512 * 1024);

        let speed = meter.current();
        assert!(speed > 0., "应有速率");
        assert!(speed < 1024. * 1024. * 1024., "速率不应离谱");
    }

    /// 节流：100ms 内最多放行一次，否则进度事件会淹没 UI。
    #[test]
    fn throttle_allows_one_event_per_interval() {
        let mut throttle = Throttle::new(Duration::from_millis(80));
        // 第一次调用时 `last` 就是刚刚，所以不会立刻放行。
        assert!(!throttle.ready());

        std::thread::sleep(Duration::from_millis(100));
        assert!(throttle.ready());
        // 紧接着的第二次调用必须被压掉。
        assert!(!throttle.ready());
    }

    #[test]
    fn resume_offset_is_zero_without_resume() {
        assert_eq!(resume_offset(false, 100, 200), 0);
        assert_eq!(resume_offset(false, 0, 200), 0);
    }

    #[test]
    fn resume_offset_follows_target_length() {
        // 目标比源短：从目标长度接着传。
        assert_eq!(resume_offset(true, 100, 200), 100);
        assert_eq!(resume_offset(true, 0, 200), 0);
        // 目标比源长（内容变了）：重头传，多的部分会被截断。
        assert_eq!(resume_offset(true, 300, 200), 0);
        // 一样长：偏移正好等于源长度，调用方会判定为「已完成」。
        assert_eq!(resume_offset(true, 200, 200), 200);
    }

    /// 临时文件必须和目标同目录 —— 跨目录 rename 会退化成复制。
    #[test]
    fn filepart_sits_next_to_the_target() {
        assert_eq!(
            filepart_path("/srv/app/release.bin"),
            "/srv/app/release.bin.filepart"
        );
        assert_eq!(filepart_path("rel.txt"), "rel.txt.filepart");
        // 已经带后缀也不会重复加：调用方传的是目标路径。
        assert_eq!(filepart_path("/a/b"), "/a/b.filepart");
    }

    #[test]
    fn text_edit_temp_paths_are_unique_and_stay_next_to_the_target() {
        let first = edit_temp_path("/srv/app/config.toml");
        let second = edit_temp_path("/srv/app/config.toml");
        assert_ne!(first, second);
        assert!(first.starts_with("/srv/app/config.toml.macscp-editpart-"));
        assert!(!first.contains("/../"));
    }

    #[test]
    fn text_editor_force_opens_non_utf8_and_nul_bytes() {
        let decoded = decode_text_for_editor(&[b'A', 0xff, 0, b'B']);
        assert_eq!(decoded, "A\u{FFFD}\u{FFFD}B");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn download_marker_does_not_follow_a_symlink() {
        let nonce = NEXT_EDIT_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("macscp-marker-link-{}-{nonce}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建测试目录");
        let actual = dir.join("actual.json");
        let marker = dir.join("marker.json");
        let identity = RemoteIdentity {
            remote: "/srv/data.bin".to_string(),
            size: 123,
            modified_secs: Some(456),
        };
        std::fs::write(&actual, serde_json::to_vec(&identity).expect("编码身份")).expect("写身份");
        std::os::unix::fs::symlink(&actual, &marker).expect("建标记链接");

        assert_eq!(read_download_marker(&marker).await, None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 上传续传只认「临时文件比本地短」；其余一律重传。
    #[test]
    fn upload_resume_needs_a_shorter_filepart() {
        use UploadDecision::{Overwrite, Resume, Skip};

        assert_eq!(
            upload_resume_offset(Resume, 50 * 1024, 300 * 1024),
            50 * 1024
        );
        // 临时文件是空的 / 不存在：没有可续的东西。
        assert_eq!(upload_resume_offset(Resume, 0, 300 * 1024), 0);
        // 一样长：内容可能对也可能不对，按规范重传。
        assert_eq!(upload_resume_offset(Resume, 300 * 1024, 300 * 1024), 0);
        // 比本地还长：本地文件换过了，必须重传。
        assert_eq!(upload_resume_offset(Resume, 400 * 1024, 300 * 1024), 0);
        // 没选续传就别碰已有的字节。
        assert_eq!(upload_resume_offset(Overwrite, 50 * 1024, 300 * 1024), 0);
        assert_eq!(upload_resume_offset(Skip, 50 * 1024, 300 * 1024), 0);
    }
}
