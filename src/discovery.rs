//! 局域网设备发现：逐网卡 UDP 定向广播 + mDNS/DNS-SD。

use std::collections::HashMap;
use std::io::{BufRead as _, BufReader, ErrorKind, Write as _};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicU16, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mdns_sd::{DaemonEvent, ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DISCOVERY_PORT: u16 = 1985;
// shortcut: 固定 loopback 端口若被其它程序占用会阻止选主，发生冲突时改为文件锁 + 动态端口。
const COORDINATOR_PORT: u16 = 41985;
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(3);
const OFFLINE_TIMEOUT: Duration = Duration::from_secs(10);
const SERVICE_TYPE: &str = "_blip-sftp._tcp.local.";
const IPC_PROTOCOL: &str = "blip-discovery-v1";
const MAX_PACKET_SIZE: usize = 2048;
const MAX_IPC_MESSAGE_SIZE: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredDevice {
    pub id: String,
    pub hostname: String,
    pub address: Ipv4Addr,
    pub port: u16,
}

#[derive(Serialize, Deserialize)]
struct SharedSnapshot {
    protocol: String,
    devices: Vec<DiscoveredDevice>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiscoveryPacket {
    device_id: Uuid,
    hostname: String,
    tcp_port: u16,
    timestamp: u64,
}

#[derive(Clone, Copy)]
enum DiscoverySource {
    Udp,
    Mdns,
}

enum DiscoveryUpdate {
    Seen {
        device: DiscoveredDevice,
        source: DiscoverySource,
    },
    MdnsRemoved(String),
}

struct DeviceState {
    device: DiscoveredDevice,
    udp_seen: Option<Instant>,
    mdns_present: bool,
}

#[derive(Default)]
struct DeviceRegistry {
    devices: HashMap<String, DeviceState>,
}

impl DeviceRegistry {
    fn apply(&mut self, update: DiscoveryUpdate, now: Instant) {
        match update {
            DiscoveryUpdate::Seen { device, source } => {
                let state = self
                    .devices
                    .entry(device.id.clone())
                    .or_insert_with(|| DeviceState {
                        device: device.clone(),
                        udp_seen: None,
                        mdns_present: false,
                    });

                match source {
                    DiscoverySource::Udp => {
                        state.device = device;
                        state.udp_seen = Some(now);
                    }
                    DiscoverySource::Mdns => {
                        // UDP 的源地址是操作系统实际收到报文的路径，优先于 mDNS
                        // 解析结果里可能混入的虚拟网卡地址。
                        if state
                            .udp_seen
                            .is_none_or(|seen| now.duration_since(seen) > OFFLINE_TIMEOUT)
                        {
                            state.device = device;
                        }
                        state.mdns_present = true;
                    }
                }
            }
            DiscoveryUpdate::MdnsRemoved(id) => {
                if let Some(state) = self.devices.get_mut(&id) {
                    state.mdns_present = false;
                }
            }
        }
    }

    fn prune(&mut self, now: Instant) {
        self.devices.retain(|_, state| {
            state.mdns_present
                || state
                    .udp_seen
                    .is_some_and(|seen| now.duration_since(seen) <= OFFLINE_TIMEOUT)
        });
    }

    fn snapshot(&self) -> Vec<DiscoveredDevice> {
        let mut devices = self
            .devices
            .values()
            .map(|state| state.device.clone())
            .collect::<Vec<_>>();
        devices.sort_by(|left, right| {
            left.hostname
                .to_lowercase()
                .cmp(&right.hostname.to_lowercase())
                .then_with(|| left.address.cmp(&right.address))
        });
        devices
    }
}

/// 每个窗口各持有一个订阅；只有抢到协调端口的进程会运行 UDP/mDNS。
pub struct DiscoveryService {
    events: flume::Receiver<Vec<DiscoveredDevice>>,
    port: Arc<AtomicU16>,
}

impl DiscoveryService {
    pub fn events(&self) -> &flume::Receiver<Vec<DiscoveredDevice>> {
        &self.events
    }

    /// 内置 SFTP 从启动失败恢复时，立即开始广播新端口并更新 mDNS 注册。
    pub fn set_port(&self, port: Option<u16>) -> Result<(), String> {
        self.port.store(port.unwrap_or(0), Ordering::Relaxed);
        Ok(())
    }
}

pub fn start(server_port: Option<u16>) -> Result<DiscoveryService, String> {
    let device_id = load_or_create_device_id()?;
    let hostname = local_hostname();
    let port = Arc::new(AtomicU16::new(server_port.unwrap_or(0)));
    let (events_tx, events_rx) = flume::unbounded();
    let coordinator_port = port.clone();
    std::thread::Builder::new()
        .name("blip-discovery-coordinator".to_string())
        .spawn(move || discovery_coordinator(device_id, hostname, coordinator_port, events_tx))
        .map_err(|err| format!("无法启动设备发现协调线程：{err}"))?;

    Ok(DiscoveryService {
        events: events_rx,
        port,
    })
}

fn discovery_coordinator(
    device_id: Uuid,
    hostname: String,
    port: Arc<AtomicU16>,
    events: flume::Sender<Vec<DiscoveredDevice>>,
) {
    loop {
        match TcpListener::bind((Ipv4Addr::LOCALHOST, COORDINATOR_PORT)) {
            Ok(listener) => {
                log::info!("本进程已接管局域网设备发现");
                run_discovery_owner(listener, device_id, hostname, port, events);
                return;
            }
            Err(err) if err.kind() == ErrorKind::AddrInUse => {
                log::debug!("设备发现由另一个 Blip 进程提供，订阅其在线设备列表");
                if let Err(err) = subscribe_to_discovery_owner(&events) {
                    log::debug!("设备发现协调连接已断开：{err}");
                    let _ = events.send(Vec::new());
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
            Err(err) => {
                log::warn!("无法竞争设备发现协调端口：{err}");
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

fn run_discovery_owner(
    listener: TcpListener,
    device_id: Uuid,
    hostname: String,
    port: Arc<AtomicU16>,
    events: flume::Sender<Vec<DiscoveredDevice>>,
) {
    let shared = Arc::new(RwLock::new(Vec::new()));
    let (updates_tx, updates_rx) = flume::unbounded();
    start_registry(updates_rx, events, shared.clone());
    start_udp_listener(device_id, updates_tx.clone());
    start_udp_sender(device_id, hostname.clone(), port.clone());

    let mdns = start_mdns(device_id, updates_tx).map_err(|err| {
        log::warn!("mDNS 设备发现不可用，将继续使用 UDP 定向广播：{err}");
        err
    });
    if let Ok(mdns) = &mdns {
        start_mdns_publisher(mdns.clone(), device_id, hostname, port);
    }

    if let Err(err) = listener.set_nonblocking(true) {
        log::error!("无法设置设备发现协调端口：{err}");
        return;
    }
    loop {
        match listener.accept() {
            Ok((stream, _)) => start_snapshot_client(stream, shared.clone()),
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => {
                log::warn!("接受设备发现订阅失败：{err}");
                std::thread::sleep(Duration::from_millis(300));
            }
        }
    }
}

fn subscribe_to_discovery_owner(
    events: &flume::Sender<Vec<DiscoveredDevice>>,
) -> Result<(), String> {
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, COORDINATOR_PORT))
        .map_err(|err| format!("连接本机发现协调器失败：{err}"))?;
    receive_shared_snapshots(stream, events)
}

fn receive_shared_snapshots(
    stream: TcpStream,
    events: &flume::Sender<Vec<DiscoveredDevice>>,
) -> Result<(), String> {
    let mut reader = BufReader::new(stream);
    let mut line = Vec::new();
    loop {
        line.clear();
        let length = reader
            .read_until(b'\n', &mut line)
            .map_err(|err| format!("读取本机发现快照失败：{err}"))?;
        if length == 0 {
            return Err("发现协调器已退出".to_string());
        }
        if line.len() > MAX_IPC_MESSAGE_SIZE {
            return Err("本机发现快照过大".to_string());
        }
        let snapshot = serde_json::from_slice::<SharedSnapshot>(&line)
            .map_err(|err| format!("解析本机发现快照失败：{err}"))?;
        if snapshot.protocol != IPC_PROTOCOL || !valid_shared_devices(&snapshot.devices) {
            return Err("本机发现协调协议不匹配".to_string());
        }
        if events.send(snapshot.devices).is_err() {
            return Ok(());
        }
    }
}

fn valid_shared_devices(devices: &[DiscoveredDevice]) -> bool {
    devices.len() <= 1024
        && devices.iter().all(|device| {
            Uuid::parse_str(&device.id).is_ok()
                && !device.hostname.trim().is_empty()
                && device.hostname.len() <= 255
                && !device.address.is_unspecified()
                && !device.address.is_loopback()
                && device.port != 0
        })
}

fn start_snapshot_client(stream: TcpStream, shared: Arc<RwLock<Vec<DiscoveredDevice>>>) {
    if let Err(err) = std::thread::Builder::new()
        .name("blip-discovery-subscriber".to_string())
        .spawn(move || serve_snapshot_client(stream, shared))
    {
        log::warn!("无法创建设备发现订阅线程：{err}");
    }
}

fn serve_snapshot_client(mut stream: TcpStream, shared: Arc<RwLock<Vec<DiscoveredDevice>>>) {
    if stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .is_err()
    {
        return;
    }
    let mut previous = None;
    loop {
        let snapshot = shared
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        if previous.as_ref() != Some(&snapshot) {
            let message = SharedSnapshot {
                protocol: IPC_PROTOCOL.to_string(),
                devices: snapshot.clone(),
            };
            if serde_json::to_writer(&mut stream, &message).is_err()
                || stream.write_all(b"\n").is_err()
                || stream.flush().is_err()
            {
                break;
            }
            previous = Some(snapshot);
        }

        let mut byte = [0_u8; 1];
        match stream.peek(&mut byte) {
            Ok(0) => break,
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) => {}
            Err(_) => break,
        }
    }
}

fn start_registry(
    updates: flume::Receiver<DiscoveryUpdate>,
    events: flume::Sender<Vec<DiscoveredDevice>>,
    shared: Arc<RwLock<Vec<DiscoveredDevice>>>,
) {
    if let Err(err) = std::thread::Builder::new()
        .name("blip-discovery-state".to_string())
        .spawn(move || {
            let mut registry = DeviceRegistry::default();
            let mut previous = Vec::new();
            loop {
                match updates.recv_timeout(Duration::from_secs(1)) {
                    Ok(update) => registry.apply(update, Instant::now()),
                    Err(flume::RecvTimeoutError::Timeout) => {}
                    Err(flume::RecvTimeoutError::Disconnected) => break,
                }
                registry.prune(Instant::now());
                let snapshot = registry.snapshot();
                if snapshot != previous {
                    *shared
                        .write()
                        .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot.clone();
                    if events.send(snapshot.clone()).is_err() {
                        break;
                    }
                    previous = snapshot;
                }
            }
        })
    {
        log::error!("无法启动设备发现状态线程：{err}");
    }
}

fn start_udp_listener(device_id: Uuid, updates: flume::Sender<DiscoveryUpdate>) {
    if let Err(err) = std::thread::Builder::new()
        .name("blip-discovery-udp-listener".to_string())
        .spawn(move || {
            let socket = match UdpSocket::bind((Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)) {
                Ok(socket) => socket,
                Err(err) => {
                    log::error!("无法监听 UDP 设备发现端口 {DISCOVERY_PORT}：{err}");
                    return;
                }
            };
            let mut buffer = [0_u8; MAX_PACKET_SIZE];
            loop {
                match socket.recv_from(&mut buffer) {
                    Ok((length, source)) => {
                        if let Some(update) = parse_udp_packet(&buffer[..length], source, device_id)
                            && updates.send(update).is_err()
                        {
                            break;
                        }
                    }
                    Err(err) if err.kind() == ErrorKind::Interrupted => {}
                    Err(err) => {
                        log::error!("UDP 设备发现接收失败：{err}");
                        break;
                    }
                }
            }
        })
    {
        log::error!("无法启动 UDP 设备发现监听线程：{err}");
    }
}

fn start_udp_sender(device_id: Uuid, hostname: String, port: Arc<AtomicU16>) {
    if let Err(err) = std::thread::Builder::new()
        .name("blip-discovery-udp-sender".to_string())
        .spawn(move || {
            loop {
                let tcp_port = port.load(Ordering::Relaxed);
                if tcp_port != 0 {
                    let packet = DiscoveryPacket {
                        device_id,
                        hostname: hostname.clone(),
                        tcp_port,
                        timestamp: unix_timestamp_ms(),
                    };
                    if let Ok(payload) = serde_json::to_vec(&packet) {
                        for target in broadcast_targets() {
                            match UdpSocket::bind((target.source, 0)) {
                                Ok(socket) => {
                                    if socket.set_broadcast(true).is_ok()
                                        && let Err(err) = socket
                                            .send_to(&payload, (target.broadcast, DISCOVERY_PORT))
                                    {
                                        log::debug!(
                                            "从 {} 向 {} 发送发现心跳失败：{err}",
                                            target.source,
                                            target.broadcast
                                        );
                                    }
                                }
                                Err(err) => {
                                    log::debug!("无法绑定设备发现源地址 {}：{err}", target.source)
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(HEARTBEAT_INTERVAL);
            }
        })
    {
        log::error!("无法启动 UDP 设备发现广播线程：{err}");
    }
}

fn start_mdns(
    device_id: Uuid,
    updates: flume::Sender<DiscoveryUpdate>,
) -> Result<ServiceDaemon, String> {
    let mdns = ServiceDaemon::new().map_err(|err| format!("启动 mDNS daemon 失败：{err}"))?;
    let browser = mdns
        .browse(SERVICE_TYPE)
        .map_err(|err| format!("浏览 mDNS 服务失败：{err}"))?;
    let monitor = mdns
        .monitor()
        .map_err(|err| format!("监听 mDNS daemon 状态失败：{err}"))?;

    std::thread::Builder::new()
        .name("blip-discovery-mdns-browser".to_string())
        .spawn(move || {
            let mut ids_by_fullname = HashMap::new();
            while let Ok(event) = browser.recv() {
                match event {
                    ServiceEvent::ServiceResolved(service) => {
                        if let Some((id, update)) = mdns_update(&service, device_id) {
                            ids_by_fullname.insert(service.get_fullname().to_string(), id);
                            if updates.send(update).is_err() {
                                break;
                            }
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        if let Some(id) = ids_by_fullname.remove(&fullname)
                            && updates.send(DiscoveryUpdate::MdnsRemoved(id)).is_err()
                        {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        })
        .map_err(|err| format!("无法启动 mDNS 浏览线程：{err}"))?;

    std::thread::Builder::new()
        .name("blip-discovery-mdns-monitor".to_string())
        .spawn(move || {
            while let Ok(event) = monitor.recv() {
                if let DaemonEvent::Error(err) = event {
                    log::warn!("mDNS daemon 错误：{err}");
                }
            }
        })
        .map_err(|err| format!("无法启动 mDNS 监控线程：{err}"))?;

    Ok(mdns)
}

fn start_mdns_publisher(
    mdns: ServiceDaemon,
    device_id: Uuid,
    hostname: String,
    port: Arc<AtomicU16>,
) {
    if let Err(err) = std::thread::Builder::new()
        .name("blip-discovery-mdns-publisher".to_string())
        .spawn(move || {
            let mut published_port = 0;
            loop {
                let current_port = port.load(Ordering::Relaxed);
                if current_port != published_port {
                    let result = if current_port == 0 {
                        mdns.unregister(&service_fullname(device_id))
                            .map(|_| ())
                            .map_err(|err| err.to_string())
                    } else {
                        mdns_service(device_id, &hostname, current_port).and_then(|service| {
                            mdns.register(service)
                                .map_err(|err| format!("注册 mDNS 服务失败：{err}"))
                        })
                    };
                    match result {
                        Ok(()) => published_port = current_port,
                        Err(err) => log::warn!("更新 mDNS 服务端口失败：{err}"),
                    }
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        })
    {
        log::error!("无法启动 mDNS 发布线程：{err}");
    }
}

fn mdns_update(service: &ResolvedService, own_id: Uuid) -> Option<(String, DiscoveryUpdate)> {
    let id = Uuid::parse_str(service.get_property_val_str("id")?).ok()?;
    if id == own_id || service.get_port() == 0 {
        return None;
    }
    let hostname = service.get_property_val_str("hostname")?.trim();
    if hostname.is_empty() || hostname.len() > 255 {
        return None;
    }
    let address = service.get_addresses_v4().into_iter().next()?;
    let id = id.to_string();
    Some((
        id.clone(),
        DiscoveryUpdate::Seen {
            device: DiscoveredDevice {
                id,
                hostname: hostname.to_string(),
                address,
                port: service.get_port(),
            },
            source: DiscoverySource::Mdns,
        },
    ))
}

fn mdns_service(device_id: Uuid, hostname: &str, port: u16) -> Result<ServiceInfo, String> {
    let instance = service_instance(device_id);
    let host = format!("{instance}.local.");
    let id = device_id.to_string();
    let properties = [
        ("id", id.as_str()),
        ("hostname", hostname),
        ("version", "1"),
    ];
    ServiceInfo::new(SERVICE_TYPE, &instance, &host, (), port, &properties[..])
        .map(ServiceInfo::enable_addr_auto)
        .map_err(|err| format!("创建 mDNS 服务信息失败：{err}"))
}

fn parse_udp_packet(payload: &[u8], source: SocketAddr, own_id: Uuid) -> Option<DiscoveryUpdate> {
    let source = match source.ip() {
        IpAddr::V4(address) if !address.is_unspecified() && !address.is_loopback() => address,
        _ => return None,
    };
    let packet = serde_json::from_slice::<DiscoveryPacket>(payload).ok()?;
    if packet.device_id == own_id
        || packet.tcp_port == 0
        || packet.hostname.trim().is_empty()
        || packet.hostname.len() > 255
    {
        return None;
    }
    Some(DiscoveryUpdate::Seen {
        device: DiscoveredDevice {
            id: packet.device_id.to_string(),
            hostname: packet.hostname,
            address: source,
            port: packet.tcp_port,
        },
        source: DiscoverySource::Udp,
    })
}

struct BroadcastTarget {
    source: Ipv4Addr,
    broadcast: Ipv4Addr,
}

fn broadcast_targets() -> Vec<BroadcastTarget> {
    netdev::get_interfaces()
        .into_iter()
        .filter(|interface| {
            interface.is_up()
                && !interface.is_loopback()
                && interface.is_broadcast()
                && !interface.is_point_to_point()
        })
        .flat_map(|interface| interface.ipv4)
        .filter_map(|network| {
            directed_broadcast(network.addr(), network.prefix_len()).map(|broadcast| {
                BroadcastTarget {
                    source: network.addr(),
                    broadcast,
                }
            })
        })
        .collect()
}

fn directed_broadcast(address: Ipv4Addr, prefix_len: u8) -> Option<Ipv4Addr> {
    if address.is_unspecified() || address.is_loopback() || prefix_len >= 31 {
        return None;
    }
    let mask = u32::MAX << (32 - prefix_len);
    Some(Ipv4Addr::from(u32::from(address) | !mask))
}

fn local_hostname() -> String {
    let hostname = gethostname::gethostname()
        .to_string_lossy()
        .trim()
        .to_string();
    if hostname.is_empty() {
        "Blip device".to_string()
    } else {
        hostname
    }
}

fn unix_timestamp_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn service_instance(device_id: Uuid) -> String {
    format!("blip-{}", &device_id.simple().to_string()[..8])
}

fn service_fullname(device_id: Uuid) -> String {
    format!("{}.{}", service_instance(device_id), SERVICE_TYPE)
}

fn device_id_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("macscp")
        .join("device_id")
}

fn load_or_create_device_id() -> Result<Uuid, String> {
    let path = device_id_path();
    let replace_existing = match std::fs::read_to_string(&path) {
        Ok(raw) => {
            if let Ok(id) = Uuid::parse_str(raw.trim()) {
                return Ok(id);
            }
            log::warn!("设备 ID 文件损坏，将重新生成：{}", path.display());
            true
        }
        Err(err) if err.kind() == ErrorKind::NotFound => false,
        Err(err) => return Err(format!("读取设备 ID 失败：{err}")),
    };

    let directory = path
        .parent()
        .ok_or_else(|| "无法确定设备 ID 配置目录".to_string())?;
    std::fs::create_dir_all(directory).map_err(|err| format!("创建设备 ID 目录失败：{err}"))?;
    let id = Uuid::new_v4();
    if replace_existing {
        std::fs::write(&path, id.hyphenated().to_string())
            .map_err(|err| format!("修复设备 ID 文件失败：{err}"))?;
        return Ok(id);
    }
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    match options.open(&path) {
        Ok(mut file) => file
            .write_all(id.hyphenated().to_string().as_bytes())
            .map_err(|err| format!("保存设备 ID 失败：{err}"))?,
        Err(err) if err.kind() == ErrorKind::AlreadyExists => {
            let raw = std::fs::read_to_string(&path)
                .map_err(|read_err| format!("读取设备 ID 失败：{read_err}"))?;
            return Uuid::parse_str(raw.trim())
                .map_err(|parse_err| format!("解析设备 ID 失败：{parse_err}"));
        }
        Err(err) => return Err(format!("创建设备 ID 文件失败：{err}")),
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: Uuid, address: Ipv4Addr) -> DiscoveredDevice {
        DiscoveredDevice {
            id: id.to_string(),
            hostname: "office-mac".to_string(),
            address,
            port: 2022,
        }
    }

    #[test]
    fn calculates_directed_broadcast_from_prefix() {
        assert_eq!(
            directed_broadcast(Ipv4Addr::new(192, 168, 1, 42), 24),
            Some(Ipv4Addr::new(192, 168, 1, 255))
        );
        assert_eq!(
            directed_broadcast(Ipv4Addr::new(10, 23, 45, 67), 16),
            Some(Ipv4Addr::new(10, 23, 255, 255))
        );
        assert_eq!(directed_broadcast(Ipv4Addr::new(10, 0, 0, 1), 31), None);
    }

    #[test]
    fn udp_packet_filters_self_and_uses_source_address() {
        let own_id = Uuid::new_v4();
        let peer_id = Uuid::new_v4();
        let packet = DiscoveryPacket {
            device_id: peer_id,
            hostname: "peer".to_string(),
            tcp_port: 2022,
            timestamp: 123,
        };
        let payload = serde_json::to_vec(&packet).unwrap();
        let source = SocketAddr::from((Ipv4Addr::new(192, 168, 1, 8), 51000));
        let Some(DiscoveryUpdate::Seen { device, .. }) = parse_udp_packet(&payload, source, own_id)
        else {
            panic!("应解析出设备");
        };
        assert_eq!(device.id, peer_id.to_string());
        assert_eq!(device.address, Ipv4Addr::new(192, 168, 1, 8));

        let own_payload = serde_json::to_vec(&DiscoveryPacket {
            device_id: own_id,
            ..packet
        })
        .unwrap();
        assert!(parse_udp_packet(&own_payload, source, own_id).is_none());
    }

    #[test]
    fn udp_packet_has_the_documented_json_fields() {
        let packet = DiscoveryPacket {
            device_id: Uuid::nil(),
            hostname: "peer".to_string(),
            tcp_port: 2022,
            timestamp: 123,
        };
        let value = serde_json::to_value(packet).unwrap();
        assert_eq!(value["device_id"], Uuid::nil().to_string());
        assert_eq!(value["hostname"], "peer");
        assert_eq!(value["tcp_port"], 2022);
        assert_eq!(value["timestamp"], 123);
    }

    #[test]
    fn udp_device_expires_after_ten_seconds_without_a_heartbeat() {
        let id = Uuid::new_v4();
        let now = Instant::now();
        let mut registry = DeviceRegistry::default();
        registry.apply(
            DiscoveryUpdate::Seen {
                device: device(id, Ipv4Addr::new(192, 168, 1, 8)),
                source: DiscoverySource::Udp,
            },
            now,
        );

        registry.prune(now + OFFLINE_TIMEOUT);
        assert_eq!(registry.snapshot().len(), 1);
        registry.prune(now + OFFLINE_TIMEOUT + Duration::from_millis(1));
        assert!(registry.snapshot().is_empty());
    }

    #[test]
    fn secondary_instance_receives_the_owner_snapshot() {
        let expected = vec![device(Uuid::new_v4(), Ipv4Addr::new(192, 168, 1, 8))];
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let sent = expected.clone();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            serde_json::to_writer(
                &mut stream,
                &SharedSnapshot {
                    protocol: IPC_PROTOCOL.to_string(),
                    devices: sent,
                },
            )
            .unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let stream = TcpStream::connect(address).unwrap();
        let (events_tx, events_rx) = flume::unbounded();

        assert!(receive_shared_snapshots(stream, &events_tx).is_err());
        assert_eq!(events_rx.recv().unwrap(), expected);
        server.join().unwrap();
    }

    #[test]
    fn mdns_and_udp_merge_by_device_id() {
        let id = Uuid::new_v4();
        let now = Instant::now();
        let mut registry = DeviceRegistry::default();
        registry.apply(
            DiscoveryUpdate::Seen {
                device: device(id, Ipv4Addr::new(10, 0, 0, 8)),
                source: DiscoverySource::Mdns,
            },
            now,
        );
        registry.apply(
            DiscoveryUpdate::Seen {
                device: device(id, Ipv4Addr::new(192, 168, 1, 8)),
                source: DiscoverySource::Udp,
            },
            now,
        );
        assert_eq!(registry.snapshot().len(), 1);
        assert_eq!(
            registry.snapshot()[0].address,
            Ipv4Addr::new(192, 168, 1, 8)
        );

        registry.prune(now + OFFLINE_TIMEOUT + Duration::from_secs(1));
        assert_eq!(registry.snapshot().len(), 1, "mDNS 仍在线时保留设备");
        registry.apply(DiscoveryUpdate::MdnsRemoved(id.to_string()), now);
        registry.prune(now + OFFLINE_TIMEOUT + Duration::from_secs(1));
        assert!(registry.snapshot().is_empty());
    }
}
