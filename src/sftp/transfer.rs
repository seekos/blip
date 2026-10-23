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
//! * **续传**：允许时，先看两端已有的字节数，从小的那一段后面接着传。
//!   取消时**保留**半成品（不再删掉），否则下次就没法续了。
//! * **上传永远走临时文件**：数据先写 `<remote>.filepart`，写完整再替换目标。
//!   这样中途取消不会留下一个被截断的目标文件，替换时也不必打开目标文件，
//!   于是「覆盖正在运行的可执行文件」不会撞上 `ETXTBSY`（详见 [`replace_remote`]）。
//! * **整目录上传**（[`run_upload_dir`]）：一条指令传一整棵树，队列里只占一行。
//!   进度按整批字节汇总，取消一次就停掉整批。

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::FileAttributes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::local_tree::{self, LocalTree};
use super::remote_tree;
use super::session::join_remote;
use super::types::{
    ExistingPolicy, SftpEvent, TransferId, TransferOutcome, UploadDecision,
};

/// 一次搬运的块大小。64 KiB 是「进度条平滑」和「系统调用次数」之间的折中。
const CHUNK_SIZE: usize = 64 * 1024;

/// 进度上报的最小间隔。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

/// 速率平滑系数：越接近 1 越稳，越接近 0 越灵敏。
const SPEED_SMOOTHING: f64 = 0.7;

/// 两次采样之间的最小间隔（秒）。低于它的样本直接丢弃 —— 除以接近 0 的时间
/// 会把速率算成天文数字。
const MIN_SAMPLE_INTERVAL: f64 = 0.05;

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
    let outcome = match download_once(
        &sftp, id, &remote, &local, total, resume, &cancel, &events,
    )
    .await
    {
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
    let plan = plan_download(sftp, remote, local, total, resume).await?;
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
) -> Result<Plan> {
    // 远端大小优先用 stat，拿不到就退回调用方给的值。
    let remote_len = match sftp.metadata(remote).await {
        Ok(meta) => meta.size.unwrap_or(total),
        Err(_) => total,
    };
    let total = remote_len.max(total);

    let local_len = tokio::fs::metadata(local)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0);

    Ok(Plan {
        offset: resume_offset(resume, local_len, total),
        total,
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
    plan: &Plan,
    cancel: &AtomicBool,
    sink: &mut dyn UploadSink,
) -> Result<()> {
    sink.started(plan.total, plan.offset);

    // 本地已经和远端一样长：不用传，但进度要报满（队列那一行得走到 100%）。
    if plan.total > 0 && plan.offset >= plan.total {
        sink.progress(plan.total, plan.total, 0.);
        return Ok(());
    }

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

    if let Some(parent) = local.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|err| anyhow!("创建本地目录 {} 失败：{err}", parent.display()))?;
    }

    // 从头传才需要 truncate；续传必须用 append，否则前半段会被抹掉。
    let mut target = if plan.offset > 0 {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(local)
            .await
            .map_err(|err| anyhow!("打开本地文件 {} 失败：{err}", local.display()))?
    } else {
        tokio::fs::File::create(local)
            .await
            .map_err(|err| anyhow!("创建本地文件 {} 失败：{err}", local.display()))?
    };

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

        transferred += read as u64;
        if throttle.ready() {
            sink.progress(transferred, plan.total, speedometer.sample(transferred));
        }
    }

    target
        .flush()
        .await
        .map_err(|err| anyhow!("刷新本地文件失败：{err}"))?;

    // 终值：即使最后一次节流没触发，进度条也要走到头。
    sink.progress(transferred, plan.total.max(transferred), speedometer.current());

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
    let local_size = tokio::fs::metadata(local)
        .await
        .map(|meta| meta.len())
        .map_err(|err| anyhow!("读取本地文件 {} 失败：{err}", local.display()))?;

    // 目标不存在是常态（第一次上传），stat 失败不代表出错。
    let (existing_size, preserved_mode, target_is_dir) = match sftp.metadata(remote).await {
        Ok(meta) => (
            Some(meta.size.unwrap_or(0)),
            meta.permissions.map(|mode| mode & 0o7777),
            is_dir_mode(meta.permissions),
        ),
        Err(_) => (None, None, false),
    };

    let filepart_size = sftp
        .metadata(filepart_path(remote))
        .await
        .ok()
        .and_then(|meta| meta.size);

    Ok(UploadProbe {
        local_size,
        existing_size,
        filepart_size,
        preserved_mode,
        target_is_dir,
    })
}

/// POSIX mode 的类型位是不是「目录」。
fn is_dir_mode(mode: Option<u32>) -> bool {
    mode.is_some_and(mode_is_dir)
}

/// 同上，但直接吃一个裸 mode。
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
    fn new(
        id: TransferId,
        events: &'a flume::Sender<SftpEvent>,
        base: u64,
        total: u64,
    ) -> Self {
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
    let outcome = match upload_once(
        &sftp, id, &local, &remote, total, decision, &cancel, &events,
    )
    .await
    {
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
    upload_file(sftp, local, remote, &probe, total, decision, cancel, &mut sink).await
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

    let total = if probe.local_size > 0 {
        probe.local_size
    } else {
        total_hint
    };
    let temp = filepart_path(remote);

    // 续传：只有「选了续传 + 临时文件非空 + 比本地短」才接着写。
    let offset = upload_resume_offset(decision, probe.filepart_size.unwrap_or(0), total);

    sink.started(total, offset);

    upload_stream(
        sftp,
        local,
        &temp,
        &Plan { offset, total },
        cancel,
        sink,
    )
    .await?;

    // 1) 继承原文件的权限位 —— 可执行位丢了最难查。
    //    失败只警告、不中断：相比「传完的数据不要了」，丢一个权限位是小事。
    if let Some(mode) = probe.preserved_mode
        && let Err(err) = apply_mode(sftp, &temp, mode).await
    {
        log::warn!("回填权限 {mode:o} 到 {temp} 失败：{err:#}");
    }

    // 2) 原子替换。到这一步临时文件已经完整落盘，替换失败也不会丢数据。
    replace_remote(sftp, &temp, remote).await
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
/// 2. 有些服务器严格照 SFTP v3 规范，目标存在就返回失败。这时退化成
///    「先 `remove_file` 解除旧链接，再 `rename`」：`unlink` 只摘掉目录项，
///    进程手里的 inode 还活着，所以同样安全。
///
/// 第 2 步只在**确认目标确实存在**时才删 —— rename 也可能因为权限、跨文件系统
/// 之类的原因失败，那种情况下删掉用户的原文件是纯粹的破坏。
async fn replace_remote(sftp: &SftpSession, temp: &str, target: &str) -> Result<()> {
    let Err(rename_err) = sftp.rename(temp, target).await else {
        return Ok(());
    };
    let reason = rename_err.to_string();

    // 临时文件都不在了，后面动目标文件毫无意义。
    if !sftp.try_exists(temp).await.unwrap_or(false) {
        return Err(anyhow!(
            "替换失败：临时文件 {temp} 已不存在（rename 报错：{reason}）"
        ));
    }

    match sftp.metadata(target).await {
        Ok(meta) if is_dir_mode(meta.permissions) => {
            Err(anyhow!("替换失败：远端同名条目是目录：{target}"))
        }
        Ok(_) => {
            sftp.remove_file(target)
                .await
                .map_err(|err| anyhow!("解除旧文件链接失败：{err}（rename 报错：{reason}）"))?;
            sftp.rename(temp, target)
                .await
                .map_err(|err| anyhow!("替换 {target} 失败：{err}"))?;
            Ok(())
        }
        Err(_) => Err(anyhow!("替换 {target} 失败：{reason}")),
    }
}

/// 真正的搬运循环：本地 → 远端临时文件。
async fn upload_stream(
    sftp: &SftpSession,
    local: &Path,
    temp_remote: &str,
    plan: &Plan,
    cancel: &AtomicBool,
    sink: &mut dyn UploadSink,
) -> Result<()> {
    let mut source = tokio::fs::File::open(local)
        .await
        .map_err(|err| anyhow!("打开本地文件 {} 失败：{err}", local.display()))?;

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
    sink.progress(transferred, plan.total.max(transferred), speedometer.current());

    Ok(())
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
    let outcome =
        match upload_dir_inner(&sftp, id, &local, &remote, on_existing, &cancel, &events).await {
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

        // 上次传到一半留下的 `.filepart` 自动接着传 —— 那不是覆盖，是续传。
        let decision = if upload_resume_offset(
            UploadDecision::Resume,
            probe.filepart_size.unwrap_or(0),
            probe.local_size,
        ) > 0
        {
            UploadDecision::Resume
        } else {
            UploadDecision::Overwrite
        };

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

    Ok(batch_outcome(uploaded, skipped, &failures))
}

/// 整批的最终结论：有失败就报失败（带首个错误），全跳过就是跳过。
fn batch_outcome(transferred: usize, skipped: usize, failures: &[String]) -> TransferOutcome {
    if let Some(first) = failures.first() {
        let suffix = if failures.len() > 1 {
            format!("（共 {} 处失败）", failures.len())
        } else {
            String::new()
        };
        return TransferOutcome::Failed(format!("{first}{suffix}"));
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
        Ok(_) => Err(anyhow!(
            "远端同名条目不是目录，无法上传文件夹进去：{path}"
        )),
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
    let outcome =
        match download_dir_inner(&sftp, id, &remote, &local, on_existing, &cancel, &events).await {
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

    // 本地目标目录本身建不出来就整批没戏（多半是没权限或路径写错）。
    tokio::fs::create_dir_all(local)
        .await
        .map_err(|err| anyhow!("创建本地目录 {} 失败：{err}", local.display()))?;

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
        let path = local.join(relative);
        if let Err(err) = tokio::fs::create_dir_all(&path).await {
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

        // 允许续传：本地那个文件比远端短就从它末尾接着写（取消过的下载能续）。
        let plan = match plan_download(sftp, &file.remote, &target, file.size, true).await {
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

    Ok(batch_outcome(finished, skipped, &failures))
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

    /// 上传续传只认「临时文件比本地短」；其余一律重传。
    #[test]
    fn upload_resume_needs_a_shorter_filepart() {
        use UploadDecision::{Overwrite, Resume, Skip};

        assert_eq!(upload_resume_offset(Resume, 50 * 1024, 300 * 1024), 50 * 1024);
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
