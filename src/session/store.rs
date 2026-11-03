//! 会话持久化：密码进系统凭据库，连接配置进系统配置目录下的 JSON。

use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "macos")]
use apple_native_keyring_store::protected::{AccessPolicy, Cred as ProtectedCredential};
use serde::{Deserialize, Serialize};

use crate::sftp::types::{AuthMethod, SessionConfig};

/// 系统凭据库里的 service 名，所有条目共享。
const KEYRING_SERVICE: &str = "com.amov.Blip.credentials";
const LEGACY_KEYRING_SERVICE: &str = "com.macscp.credentials";
const CONFIG_DIR_NAME: &str = "blip";
const LEGACY_CONFIG_DIR_NAME: &str = "macscp";
const LAN_SFTP_PASSWORD_ACCOUNT: &str = "lan-sftp-server-password";
#[cfg(target_os = "macos")]
const ERR_SEC_MISSING_ENTITLEMENT: i32 = -34018;
const PROFILES_FILE: &str = "profiles.json";
const SETTINGS_FILE: &str = "settings.json";
const DIRECTORIES_FILE: &str = "directories.json";
const PROFILES_SCHEMA_VERSION: u32 = 1;
const PROFILE_LIMIT: usize = 50;

static PROFILES_SAVE_BLOCKED: AtomicBool = AtomicBool::new(false);
static PROFILES_WARNING: Mutex<Option<String>> = Mutex::new(None);
static TEMP_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 最近成功打开的目录。本地目录全局共享，远端目录按 `user@host`
/// 隔离，避免连接到另一台服务器时尝试打开不相关的路径。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectoryHistory {
    local: Option<PathBuf>,
    remote: Vec<RemoteDirectory>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct RemoteDirectory {
    host: String,
    username: String,
    path: String,
}

impl DirectoryHistory {
    /// 保存的本地目录仍存在时才恢复，否则使用主目录等默认值。
    pub fn local_or(&self, default: PathBuf) -> PathBuf {
        self.local
            .as_ref()
            .filter(|path| path.is_dir())
            .cloned()
            .unwrap_or(default)
    }

    pub fn remote_for(&self, host: &str, username: &str) -> Option<&str> {
        self.remote
            .iter()
            .find(|item| item.host == host && item.username == username)
            .map(|item| item.path.as_str())
    }

    pub fn remember_local(&mut self, path: PathBuf) -> bool {
        if self.local.as_ref() == Some(&path) {
            return false;
        }
        self.local = Some(path);
        true
    }

    pub fn remember_remote(&mut self, host: &str, username: &str, path: String) -> bool {
        if self.remote_for(host, username) == Some(path.as_str()) {
            return false;
        }
        self.remote
            .retain(|item| !(item.host == host && item.username == username));
        self.remote.insert(
            0,
            RemoteDirectory {
                host: host.to_string(),
                username: username.to_string(),
                path,
            },
        );
        self.remote.truncate(50);
        true
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteOpenBehavior {
    /// 下载到编辑缓存后，用系统为该文件选择的默认应用打开。
    #[serde(alias = "download")]
    #[default]
    SystemDefault,
    /// 下载到编辑缓存后，始终用用户指定的应用打开。
    CustomEditor,
    /// 忽略扩展名和内容类型，直接用内置文本编辑器打开。
    #[serde(alias = "text_editor")]
    BuiltInTextEditor,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanSftpPasswordMode {
    #[default]
    #[serde(alias = "random")]
    Default,
    Manual,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub remote_open_behavior: RemoteOpenBehavior,
    /// 自定义编辑器应用或可执行文件路径；系统默认和内置模式下忽略。
    pub editor_path: Option<PathBuf>,
    /// 内置局域网 SFTP 的共享根目录；`None` 表示当前用户主目录。
    pub lan_sftp_root: Option<PathBuf>,
    pub lan_sftp_password_mode: LanSftpPasswordMode,
}

impl Default for AppSettings {
    fn default() -> Self {
        preferred_settings(find_notepad_editor())
    }
}

fn preferred_settings(notepad: Option<PathBuf>) -> AppSettings {
    match notepad {
        Some(editor_path) => AppSettings {
            remote_open_behavior: RemoteOpenBehavior::CustomEditor,
            editor_path: Some(editor_path),
            lan_sftp_root: None,
            lan_sftp_password_mode: LanSftpPasswordMode::Default,
        },
        None => AppSettings {
            remote_open_behavior: RemoteOpenBehavior::SystemDefault,
            editor_path: None,
            lan_sftp_root: None,
            lan_sftp_password_mode: LanSftpPasswordMode::Default,
        },
    }
}

fn find_notepad_editor() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    let candidates = {
        let mut candidates = vec![
            PathBuf::from("/Applications/Notepad--.app"),
            PathBuf::from("/System/Applications/Notepad--.app"),
        ];
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join("Applications/Notepad--.app"));
        }
        candidates
    };
    #[cfg(target_os = "windows")]
    let candidates: Vec<PathBuf> = dirs::data_local_dir()
        .map(|directory| directory.join("Programs/Notepad--/Notepad--.exe"))
        .into_iter()
        .collect();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let candidates: Vec<PathBuf> = Vec::new();
    if let Some(path) = candidates.into_iter().find(|path| path.exists()) {
        return Some(path);
    }

    let executable = if cfg!(target_os = "windows") {
        "notepad--.exe"
    } else {
        "notepad--"
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join(executable))
        .find(|path| path.is_file())
}

fn migrate_legacy_editor(
    raw: &str,
    settings: AppSettings,
    notepad: Option<PathBuf>,
) -> AppSettings {
    let was_download_default = serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("remote_open_behavior")?
                .as_str()
                .map(str::to_owned)
        })
        .is_some_and(|behavior| behavior == "download");
    if was_download_default {
        preferred_settings(notepad)
    } else {
        settings
    }
}

/// 保存到磁盘的连接配置。**不含任何凭证**。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SavedAuth,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_connected: Option<i64>,
}

impl SavedProfile {
    pub fn endpoint_label(&self) -> String {
        format!("{}@{}:{}", self.username, self.host, self.port)
    }

    pub fn title(&self) -> String {
        self.name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{}@{}", self.username, self.host))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedAuth {
    /// 密码存在系统凭据库里，这里只是一个标记。
    Password,
    Key {
        path: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
struct ProfilesDocument {
    version: u32,
    profiles: Vec<SavedProfile>,
}

#[derive(Debug)]
pub struct ProfileUpsert {
    pub previous: Option<SavedProfile>,
    pub evicted: Option<SavedProfile>,
}

pub(crate) fn config_dir() -> PathBuf {
    static CONFIG_DIR: OnceLock<PathBuf> = OnceLock::new();
    CONFIG_DIR
        .get_or_init(|| {
            let base = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
            let current = base.join(CONFIG_DIR_NAME);
            let legacy = base.join(LEGACY_CONFIG_DIR_NAME);
            if let Err(err) = copy_missing_config(&legacy, &current) {
                log::warn!(
                    "旧版配置迁移失败（仍会尝试直接读取旧文件） {} → {}：{err}",
                    legacy.display(),
                    current.display()
                );
            }
            current
        })
        .clone()
}

fn legacy_config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(LEGACY_CONFIG_DIR_NAME)
}

/// 只复制新目录里还不存在的内容，迁移失败或用户降级时旧版数据仍完整可用。
fn copy_missing_config(legacy: &Path, current: &Path) -> std::io::Result<()> {
    if !legacy.is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(current)?;
    for item in std::fs::read_dir(legacy)? {
        let item = item?;
        let source = item.path();
        let target = current.join(item.file_name());
        match std::fs::symlink_metadata(&target) {
            Ok(_) => continue,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
        let metadata = std::fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            log::warn!("跳过旧配置中的符号链接：{}", source.display());
        } else if metadata.is_dir() {
            copy_missing_config(&source, &target)?;
        } else if metadata.is_file() {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

fn read_config_file(name: &str) -> Option<(PathBuf, String)> {
    let current = config_dir().join(name);
    match std::fs::read_to_string(&current) {
        Ok(raw) => Some((current, raw)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let legacy = legacy_config_dir().join(name);
            std::fs::read_to_string(&legacy)
                .ok()
                .map(|raw| (legacy, raw))
        }
        Err(_) => None,
    }
}

fn profiles_path() -> PathBuf {
    config_dir().join(PROFILES_FILE)
}

fn settings_path() -> PathBuf {
    config_dir().join(SETTINGS_FILE)
}

fn directories_path() -> PathBuf {
    config_dir().join(DIRECTORIES_FILE)
}

fn normalize_profiles(profiles: Vec<SavedProfile>) -> Vec<SavedProfile> {
    let mut normalized = Vec::new();
    for profile in profiles {
        if normalized
            .iter()
            .any(|item: &SavedProfile| same_profile(item, &profile))
        {
            continue;
        }
        normalized.push(profile);
    }
    while normalized.len() > PROFILE_LIMIT {
        trim_profiles(&mut normalized);
    }
    normalized
}

fn parse_profiles(raw: &str) -> Result<Vec<SavedProfile>, String> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|err| format!("JSON 解析失败：{err}"))?;
    if value.is_array() {
        return serde_json::from_value(value).map_err(|err| format!("旧格式解析失败：{err}"));
    }

    let document: ProfilesDocument =
        serde_json::from_value(value).map_err(|err| format!("收藏格式解析失败：{err}"))?;
    if document.version != PROFILES_SCHEMA_VERSION {
        return Err(format!(
            "不支持的收藏版本 {}（当前支持 {}）",
            document.version, PROFILES_SCHEMA_VERSION
        ));
    }
    Ok(document.profiles)
}

fn corrupt_backup_path(path: &Path) -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(PROFILES_FILE);
    let mut candidate = path.with_file_name(format!("{file_name}.corrupt-{timestamp}"));
    let mut suffix = 1;
    while candidate.exists() {
        candidate = path.with_file_name(format!("{file_name}.corrupt-{timestamp}-{suffix}"));
        suffix += 1;
    }
    candidate
}

fn preserve_corrupt_profiles(path: &Path, parse_error: &str) -> String {
    let backup = corrupt_backup_path(path);
    match std::fs::rename(path, &backup) {
        Ok(()) => format!(
            "收藏文件损坏，已备份为 {}。为避免覆盖原数据，本次运行不会再写入收藏。错误：{parse_error}",
            backup.display()
        ),
        Err(backup_error) => format!(
            "收藏文件损坏且备份失败（{}）。为避免覆盖原数据，本次运行不会再写入收藏。错误：{parse_error}",
            backup_error
        ),
    }
}

fn load_profiles_from(path: &Path, raw: &str) -> Result<Vec<SavedProfile>, String> {
    parse_profiles(raw)
        .map(normalize_profiles)
        .map_err(|err| preserve_corrupt_profiles(path, &err))
}

pub fn load_profiles() -> Vec<SavedProfile> {
    let Some((path, raw)) = read_config_file(PROFILES_FILE) else {
        return Vec::new();
    };
    match load_profiles_from(&path, &raw) {
        Ok(profiles) => profiles,
        Err(message) => {
            PROFILES_SAVE_BLOCKED.store(true, Ordering::SeqCst);
            if let Ok(mut warning) = PROFILES_WARNING.lock() {
                *warning = Some(message.clone());
            }
            log::warn!("{message}");
            Vec::new()
        }
    }
}

pub fn take_profiles_warning() -> Option<String> {
    PROFILES_WARNING
        .lock()
        .ok()
        .and_then(|mut warning| warning.take())
}

pub fn save_profiles(profiles: &[SavedProfile]) -> Result<(), String> {
    save_profiles_to(
        &profiles_path(),
        profiles,
        PROFILES_SAVE_BLOCKED.load(Ordering::SeqCst),
    )
}

fn save_profiles_to(
    path: &Path,
    profiles: &[SavedProfile],
    save_blocked: bool,
) -> Result<(), String> {
    if save_blocked {
        return Err("收藏文件已损坏；本次运行已停止写入，请重启 Blip 后再保存".to_string());
    }
    let json = serde_json::to_vec_pretty(&ProfilesDocument {
        version: PROFILES_SCHEMA_VERSION,
        profiles: profiles.to_vec(),
    })
    .map_err(|err| format!("序列化连接配置失败：{err}"))?;
    write_atomic(path, &json).map_err(|err| format!("写入连接配置失败：{err}"))
}

pub fn load_settings() -> AppSettings {
    let Some((path, raw)) = read_config_file(SETTINGS_FILE) else {
        return AppSettings::default();
    };
    match serde_json::from_str(&raw) {
        Ok(settings) => migrate_legacy_editor(&raw, settings, find_notepad_editor()),
        Err(err) => {
            log::warn!("忽略损坏的设置文件 {}：{err}", path.display());
            AppSettings::default()
        }
    }
}

pub fn save_settings(settings: &AppSettings) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建配置目录失败：{err}"))?;
    let json =
        serde_json::to_vec_pretty(settings).map_err(|err| format!("序列化设置失败：{err}"))?;
    write_atomic(&settings_path(), &json).map_err(|err| format!("写入设置失败：{err}"))
}

pub fn load_directory_history() -> DirectoryHistory {
    let Some((path, raw)) = read_config_file(DIRECTORIES_FILE) else {
        return DirectoryHistory::default();
    };
    match serde_json::from_str(&raw) {
        Ok(history) => history,
        Err(err) => {
            log::warn!("忽略损坏的目录历史 {}：{err}", path.display());
            DirectoryHistory::default()
        }
    }
}

pub fn save_directory_history(history: &DirectoryHistory) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建配置目录失败：{err}"))?;
    let json =
        serde_json::to_vec_pretty(history).map_err(|err| format!("序列化目录历史失败：{err}"))?;
    write_atomic(&directories_path(), &json).map_err(|err| format!("写入目录历史失败：{err}"))
}

fn temp_path(path: &Path) -> PathBuf {
    let sequence = TEMP_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.json");
    path.with_file_name(format!(
        ".{file_name}.tmp-{}-{timestamp}-{sequence}",
        std::process::id(),
    ))
}

#[cfg(not(target_os = "windows"))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

#[cfg(target_os = "windows")]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: 两个 UTF-16 缓冲区都以 NUL 结尾，并在调用期间保持有效。
    if unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_atomic_with(
    path: &Path,
    bytes: &[u8],
    publish: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = temp_path(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        publish(&temporary, path)?;
        #[cfg(not(target_os = "windows"))]
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_atomic_with(path, bytes, replace_file)
}

#[cfg(target_os = "macos")]
fn store_protected_password(service: &str, account: &str, password: &str) -> keyring::Result<()> {
    let entry = ProtectedCredential::build(
        service,
        account,
        AccessPolicy::RequireUserPresence,
        None,
        false,
    )?;
    entry.set_password(password)
}

#[cfg(target_os = "macos")]
fn load_protected_password(service: &str, account: &str) -> keyring::Result<String> {
    let entry = ProtectedCredential::build(
        service,
        account,
        AccessPolicy::RequireUserPresence,
        None,
        false,
    )?;
    entry.get_password()
}

#[cfg(target_os = "macos")]
fn delete_protected_password(service: &str, account: &str) -> keyring::Result<()> {
    let entry = ProtectedCredential::build(
        service,
        account,
        AccessPolicy::RequireUserPresence,
        None,
        false,
    )?;
    entry.delete_credential()
}

#[cfg(target_os = "macos")]
fn is_missing_protected_keychain_entitlement(err: &keyring::Error) -> bool {
    let source = match err {
        keyring::Error::PlatformFailure(source) | keyring::Error::NoStorageAccess(source) => source,
        _ => return false,
    };
    source
        .downcast_ref::<security_framework::base::Error>()
        .is_some_and(|err| err.code() == ERR_SEC_MISSING_ENTITLEMENT)
}

fn plain_entry(service: &str, account: &str) -> keyring::Result<keyring::Entry> {
    keyring::Entry::new(service, account)
}

fn store_plain_password(service: &str, account: &str, password: &str) -> keyring::Result<()> {
    plain_entry(service, account)?.set_password(password)
}

fn load_plain_password(service: &str, account: &str) -> keyring::Result<Option<String>> {
    match plain_entry(service, account)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(err) => Err(err),
    }
}

fn delete_plain_password(service: &str, account: &str) {
    if let Ok(entry) = plain_entry(service, account) {
        let _ = entry.delete_credential();
    }
}

pub fn store_lan_sftp_password(password: &str) -> Result<(), String> {
    store_plain_password(KEYRING_SERVICE, LAN_SFTP_PASSWORD_ACCOUNT, password)
        .map_err(|err| format!("保存局域网 SFTP 密码失败：{err}"))?;
    delete_plain_password(LEGACY_KEYRING_SERVICE, LAN_SFTP_PASSWORD_ACCOUNT);
    Ok(())
}

pub fn load_lan_sftp_password() -> Result<Option<String>, String> {
    let current = load_plain_password(KEYRING_SERVICE, LAN_SFTP_PASSWORD_ACCOUNT)
        .map_err(|err| format!("读取局域网 SFTP 密码失败：{err}"))?;
    if current.is_some() {
        return Ok(current);
    }
    let legacy = load_plain_password(LEGACY_KEYRING_SERVICE, LAN_SFTP_PASSWORD_ACCOUNT)
        .map_err(|err| format!("读取旧版局域网 SFTP 密码失败：{err}"))?;
    if let Some(password) = legacy.as_deref()
        && let Err(err) = store_lan_sftp_password(password)
    {
        log::warn!("旧版局域网 SFTP 密码迁移失败：{err}");
    }
    Ok(legacy)
}

/// 把密码写进 Keychain。macOS 上优先写入要求用户在场的受保护条目：系统会先
/// 尝试 Touch ID，并允许用 Mac 登录密码回退。未签名的开发二进制没有受保护
/// Keychain 权限，此时保留旧 Keychain 存储，避免 `cargo run` 无法记住密码。
pub fn store_password(config: &SessionConfig, password: &str) -> Result<(), String> {
    let account = config.keyring_account();
    #[cfg(target_os = "macos")]
    {
        match store_protected_password(KEYRING_SERVICE, &account, password) {
            Ok(()) => {
                delete_plain_password(KEYRING_SERVICE, &account);
                delete_plain_password(LEGACY_KEYRING_SERVICE, &account);
                let _ = delete_protected_password(LEGACY_KEYRING_SERVICE, &account);
                return Ok(());
            }
            Err(err) if is_missing_protected_keychain_entitlement(&err) => {
                log::warn!(
                    "当前构建未获得受保护 Keychain 权限，密码暂存普通 Keychain；\
                     使用带 provisioning profile 的签名应用后会自动迁移"
                );
            }
            Err(err) => return Err(format!("写入 Touch ID 保护的钥匙串失败：{err}")),
        }
    }

    store_plain_password(KEYRING_SERVICE, &account, password)
        .map_err(|err| format!("写入系统凭据库失败：{err}"))?;
    delete_plain_password(LEGACY_KEYRING_SERVICE, &account);
    #[cfg(target_os = "macos")]
    {
        let _ = delete_protected_password(LEGACY_KEYRING_SERVICE, &account);
    }
    Ok(())
}

/// 从 Keychain 取密码；受保护条目由系统执行 Touch ID 优先、登录密码回退。
/// 旧版普通 Keychain 条目在首次成功读取后会自动迁移。
pub fn load_password(config: &SessionConfig) -> Result<Option<String>, String> {
    #[cfg(target_os = "macos")]
    {
        let account = config.keyring_account();
        match load_protected_password(KEYRING_SERVICE, &account) {
            Ok(password) => return Ok(Some(password)),
            Err(keyring::Error::NoEntry) => {}
            Err(err) if is_missing_protected_keychain_entitlement(&err) => {
                log::debug!("当前构建无受保护 Keychain 权限，改读普通 Keychain");
            }
            Err(err) => return Err(format!("Touch ID 验证或钥匙串读取失败：{err}")),
        }

        let current_plain = load_plain_password(KEYRING_SERVICE, &account)
            .map_err(|err| format!("读取系统钥匙串失败：{err}"))?;
        if let Some(password) = current_plain {
            match store_protected_password(KEYRING_SERVICE, &account, &password) {
                Ok(()) => delete_plain_password(KEYRING_SERVICE, &account),
                Err(err) if is_missing_protected_keychain_entitlement(&err) => {}
                Err(err) => log::warn!("密码迁移到 Touch ID 钥匙串失败：{err}"),
            }
            return Ok(Some(password));
        }

        let legacy_protected = match load_protected_password(LEGACY_KEYRING_SERVICE, &account) {
            Ok(password) => Some(password),
            Err(keyring::Error::NoEntry) => None,
            Err(err) if is_missing_protected_keychain_entitlement(&err) => None,
            Err(err) => return Err(format!("读取旧版 Touch ID 钥匙串失败：{err}")),
        };
        let legacy = match legacy_protected {
            Some(password) => Some(password),
            None => load_plain_password(LEGACY_KEYRING_SERVICE, &account)
                .map_err(|err| format!("读取旧版钥匙串失败：{err}"))?,
        };
        if let Some(password) = legacy {
            if let Err(err) = store_password(config, &password) {
                log::warn!("旧版密码迁移到 Blip 凭据项失败：{err}");
            }
            return Ok(Some(password));
        }
        Ok(None)
    }

    #[cfg(not(target_os = "macos"))]
    {
        let account = config.keyring_account();
        let current = load_plain_password(KEYRING_SERVICE, &account)
            .map_err(|err| format!("读取系统凭据库失败：{err}"))?;
        if current.is_some() {
            return Ok(current);
        }
        let legacy = load_plain_password(LEGACY_KEYRING_SERVICE, &account)
            .map_err(|err| format!("读取旧版系统凭据库失败：{err}"))?;
        if let Some(password) = legacy.as_deref()
            && let Err(err) = store_password(config, password)
        {
            log::warn!("旧版密码迁移到 Blip 凭据项失败：{err}");
        }
        Ok(legacy)
    }
}

pub fn forget_password(config: &SessionConfig) {
    let account = config.keyring_account();
    #[cfg(target_os = "macos")]
    {
        let _ = delete_protected_password(KEYRING_SERVICE, &account);
        let _ = delete_protected_password(LEGACY_KEYRING_SERVICE, &account);
    }
    delete_plain_password(KEYRING_SERVICE, &account);
    delete_plain_password(LEGACY_KEYRING_SERVICE, &account);
}

fn same_profile(left: &SavedProfile, right: &SavedProfile) -> bool {
    left.host == right.host && left.username == right.username
}

fn oldest_profile_index(profiles: &[SavedProfile], unpinned_only: bool) -> Option<usize> {
    profiles
        .iter()
        .enumerate()
        .filter(|(_, profile)| !unpinned_only || !profile.pinned)
        .min_by(|(left_index, left), (right_index, right)| {
            left.last_connected
                .unwrap_or(i64::MIN)
                .cmp(&right.last_connected.unwrap_or(i64::MIN))
                .then_with(|| right_index.cmp(left_index))
        })
        .map(|(index, _)| index)
}

fn trim_profiles(profiles: &mut Vec<SavedProfile>) -> Option<SavedProfile> {
    if profiles.len() <= PROFILE_LIMIT {
        return None;
    }
    let index = oldest_profile_index(profiles, true)
        .or_else(|| oldest_profile_index(profiles, false))
        .expect("超出收藏上限时至少有一条收藏");
    Some(profiles.remove(index))
}

/// 记录/更新一条收藏（按 `user@host` 去重，端口变化视为同一收藏）。
///
/// 已有条目保留原写入位置；新条目写到最前。超过上限时优先淘汰未置顶且最久未连接的条目。
pub fn upsert_profile(profiles: &mut Vec<SavedProfile>, profile: SavedProfile) -> ProfileUpsert {
    let (previous, index) = profiles
        .iter()
        .position(|item| same_profile(item, &profile))
        .map(|index| (Some(profiles.remove(index)), index))
        .unwrap_or((None, 0));
    profiles.insert(index.min(profiles.len()), profile);
    ProfileUpsert {
        previous,
        evicted: trim_profiles(profiles),
    }
}

pub fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// 菜单排序：置顶优先，其次最近连接时间降序；其余依赖稳定排序保留写入顺序。
pub fn sort_profiles_for_display(profiles: &mut [SavedProfile]) {
    profiles.sort_by(|left, right| {
        right
            .pinned
            .cmp(&left.pinned)
            .then_with(|| right.last_connected.cmp(&left.last_connected))
    });
}

fn config_for_profile(profile: &SavedProfile) -> SessionConfig {
    SessionConfig {
        host: profile.host.clone(),
        port: profile.port,
        username: profile.username.clone(),
        auth: AuthMethod::Password(String::new()),
    }
}

fn migrate_password_with(
    old_account: &str,
    new_account: &str,
    load: impl FnOnce(&str) -> Result<Option<String>, String>,
    store: impl FnOnce(&str, &str) -> Result<(), String>,
    delete: impl FnOnce(&str),
) -> Result<bool, String> {
    if old_account == new_account {
        return Ok(false);
    }
    let Some(password) = load(old_account)? else {
        return Ok(false);
    };
    store(new_account, &password)?;
    delete(old_account);
    Ok(true)
}

/// 编辑收藏导致端口变化时，先复制密码到新 account，成功后再删除旧 account。
pub fn migrate_profile_password(
    previous: &SavedProfile,
    current: &SavedProfile,
) -> Result<bool, String> {
    if !matches!(previous.auth, SavedAuth::Password) || !matches!(current.auth, SavedAuth::Password)
    {
        return Ok(false);
    }
    let old_config = config_for_profile(previous);
    let new_config = config_for_profile(current);
    migrate_password_with(
        &old_config.keyring_account(),
        &new_config.keyring_account(),
        |_| load_password(&old_config),
        |_, password| store_password(&new_config, password),
        |_| forget_password(&old_config),
    )
}

/// 删除一条收藏（唯一键与写入时一致：user@host）。
pub fn remove_profile(
    profiles: &mut Vec<SavedProfile>,
    username: &str,
    host: &str,
) -> Option<SavedProfile> {
    let index = profiles
        .iter()
        .position(|item| item.host == host && item.username == username)?;
    Some(profiles.remove(index))
}

#[cfg(test)]
mod tests {
    use ::core::prelude::v1::test;
    use std::{cell::RefCell, rc::Rc};

    use super::*;

    fn profile(host: &str, port: u16, username: &str) -> SavedProfile {
        SavedProfile {
            name: None,
            host: host.to_string(),
            port,
            username: username.to_string(),
            auth: SavedAuth::Password,
            pinned: false,
            last_connected: None,
        }
    }

    fn test_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "blip-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("系统时间")
                .as_nanos()
        ))
    }

    fn probe_config() -> SessionConfig {
        SessionConfig {
            host: "blip-keychain-probe.invalid".to_string(),
            port: 22,
            username: "tester".to_string(),
            auth: AuthMethod::Password(String::new()),
        }
    }

    /// account 键必须带上端口，否则同一台机器的 22 和 2222 会互相覆盖。
    #[test]
    fn keyring_account_is_unique_per_endpoint() {
        let mut config = probe_config();
        let a = config.keyring_account();
        config.port = 2222;
        let b = config.keyring_account();

        assert_ne!(a, b);
        assert!(a.contains("tester@"));
        assert!(a.ends_with(":22"));
    }

    /// 只有开发构建缺少 entitlement 时才允许回退普通 Keychain。用户取消
    /// Touch ID/密码验证必须原样失败，否则一次“取消”就会绕过用户在场保护。
    #[cfg(target_os = "macos")]
    #[test]
    fn protected_keychain_fallback_never_bypasses_cancelled_authentication() {
        let missing_entitlement = keyring::Error::PlatformFailure(Box::new(
            security_framework::base::Error::from_code(ERR_SEC_MISSING_ENTITLEMENT),
        ));
        assert!(is_missing_protected_keychain_entitlement(
            &missing_entitlement
        ));

        let user_cancelled = keyring::Error::PlatformFailure(Box::new(
            security_framework::base::Error::from_code(-128),
        ));
        assert!(!is_missing_protected_keychain_entitlement(&user_cancelled));
        assert!(!is_missing_protected_keychain_entitlement(
            &keyring::Error::NoEntry
        ));
    }

    #[test]
    fn reconnect_replaces_existing_favorite_in_place() {
        let mut profiles = vec![
            profile("first", 22, "root"),
            profile("second", 22, "root"),
            profile("second", 22, "admin"),
        ];

        let result = upsert_profile(&mut profiles, profile("second", 2022, "root"));

        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].host, "first");
        assert_eq!(profiles[1].port, 2022);
        assert_eq!(profiles[2].username, "admin");
        assert_eq!(result.previous.expect("旧收藏").port, 22);
    }

    #[test]
    fn loading_keeps_the_newest_favorite_per_user_and_host() {
        let profiles = normalize_profiles(vec![
            profile("server", 2022, "root"),
            profile("server", 22, "root"),
        ]);

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].port, 2022);
    }

    #[test]
    fn removing_a_favorite_keeps_other_users_on_the_same_host() {
        let mut profiles = vec![
            profile("server", 22, "root"),
            profile("server", 22, "admin"),
        ];

        let removed = remove_profile(&mut profiles, "root", "server").unwrap();

        assert_eq!(removed.username, "root");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].username, "admin");
    }

    #[test]
    fn atomic_write_failure_keeps_original_file() {
        let root = test_directory("atomic-write");
        std::fs::create_dir_all(&root).expect("创建临时目录");
        let path = root.join("settings.json");
        std::fs::write(&path, b"original").expect("写原文件");

        let error = write_atomic_with(&path, b"replacement", |_, _| {
            Err(io::Error::other("模拟发布失败"))
        })
        .expect_err("原子替换应失败");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(std::fs::read(&path).expect("读取原文件"), b"original");
        assert_eq!(std::fs::read_dir(&root).expect("读取临时目录").count(), 1);
        std::fs::remove_dir_all(root).expect("清理测试目录");
    }

    #[test]
    fn corrupt_profiles_are_backed_up_and_not_overwritten() {
        let root = test_directory("corrupt-profiles");
        std::fs::create_dir_all(&root).expect("创建临时目录");
        let path = root.join(PROFILES_FILE);
        std::fs::write(&path, b"{not-json").expect("写损坏收藏");

        let message = load_profiles_from(&path, "{not-json").expect_err("应拒绝损坏收藏");

        assert!(message.contains("本次运行不会再写入收藏"));
        assert!(!path.exists());
        let backup = std::fs::read_dir(&root)
            .expect("读取备份目录")
            .map(|entry| entry.expect("备份项").path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("profiles.json.corrupt-"))
            })
            .expect("损坏文件备份");
        assert_eq!(std::fs::read(backup).expect("读取备份"), b"{not-json");
        let save_result = save_profiles_to(&path, &[profile("replacement", 22, "root")], true);
        assert!(save_result.is_err());
        assert!(!path.exists(), "本次会话不得重新生成 profiles.json");
        std::fs::remove_dir_all(root).expect("清理测试目录");
    }

    #[test]
    fn legacy_profile_array_is_loaded_with_field_defaults() {
        let raw = r#"[{"host":"server","port":22,"username":"root","auth":{"kind":"password"}}]"#;

        let profiles = parse_profiles(raw).expect("读取旧格式");

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0], profile("server", 22, "root"));
    }

    #[test]
    fn saved_profiles_use_the_versioned_document_shape() {
        let root = test_directory("versioned-profiles");
        let path = root.join(PROFILES_FILE);

        save_profiles_to(&path, &[profile("server", 22, "root")], false).expect("保存收藏");

        let raw = std::fs::read_to_string(&path).expect("读取收藏");
        let value: serde_json::Value = serde_json::from_str(&raw).expect("解析收藏 JSON");
        assert_eq!(value["version"], PROFILES_SCHEMA_VERSION);
        assert_eq!(value["profiles"].as_array().map(Vec::len), Some(1));
        assert_eq!(parse_profiles(&raw).expect("读取新版收藏").len(), 1);
        std::fs::remove_dir_all(root).expect("清理测试目录");
    }

    #[test]
    fn profile_sort_is_pinned_then_recent_and_stable() {
        let mut first = profile("first", 22, "root");
        first.last_connected = Some(10);
        let mut same_time = profile("same-time", 22, "root");
        same_time.last_connected = Some(10);
        let mut recent = profile("recent", 22, "root");
        recent.last_connected = Some(20);
        let mut pinned = profile("pinned", 22, "root");
        pinned.pinned = true;
        pinned.last_connected = Some(1);
        let mut profiles = vec![first, same_time, recent, pinned];

        sort_profiles_for_display(&mut profiles);

        assert_eq!(
            profiles
                .iter()
                .map(|profile| profile.host.as_str())
                .collect::<Vec<_>>(),
            ["pinned", "recent", "first", "same-time"]
        );
    }

    #[test]
    fn profile_limit_evicts_the_oldest_unpinned_favorite() {
        let mut profiles: Vec<_> = (0..PROFILE_LIMIT)
            .map(|index| {
                let mut item = profile(&format!("server-{index}"), 22, "root");
                item.last_connected = Some(index as i64 + 10);
                item
            })
            .collect();
        profiles[0].pinned = true;
        profiles[1].last_connected = Some(1);

        let mut new_profile = profile("new-server", 22, "root");
        new_profile.last_connected = Some(100);
        let result = upsert_profile(&mut profiles, new_profile);

        assert_eq!(profiles.len(), PROFILE_LIMIT);
        assert_eq!(result.evicted.expect("应淘汰收藏").host, "server-1");
        assert!(profiles.iter().any(|item| item.host == "server-0"));
    }

    #[test]
    fn changing_port_migrates_password_before_deleting_old_account() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let load_calls = calls.clone();
        let store_calls = calls.clone();
        let delete_calls = calls.clone();

        let migrated = migrate_password_with(
            "root@server:22",
            "root@server:2022",
            move |account| {
                load_calls.borrow_mut().push(format!("load:{account}"));
                Ok(Some("secret".to_string()))
            },
            move |account, password| {
                store_calls
                    .borrow_mut()
                    .push(format!("store:{account}:{password}"));
                Ok(())
            },
            move |account| delete_calls.borrow_mut().push(format!("delete:{account}")),
        )
        .expect("迁移密码");

        assert!(migrated);
        assert_eq!(
            calls.borrow().as_slice(),
            [
                "load:root@server:22",
                "store:root@server:2022:secret",
                "delete:root@server:22",
            ]
        );
    }

    #[test]
    fn directory_history_keeps_remote_paths_per_user_and_host() {
        let mut history = DirectoryHistory::default();

        assert!(history.remember_remote("server", "root", "/srv/root".to_string()));
        assert!(history.remember_remote("server", "admin", "/srv/admin".to_string()));
        assert!(history.remember_remote("other", "root", "/opt".to_string()));
        assert!(!history.remember_remote("server", "root", "/srv/root".to_string()));
        assert!(history.remember_remote("server", "root", "/srv/new".to_string()));

        assert_eq!(history.remote_for("server", "root"), Some("/srv/new"));
        assert_eq!(history.remote_for("server", "admin"), Some("/srv/admin"));
        assert_eq!(history.remote_for("other", "root"), Some("/opt"));
    }

    #[test]
    fn local_history_uses_default_only_when_saved_directory_is_missing() {
        let unique = format!(
            "blip-directory-history-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间")
                .as_nanos()
        );
        let existing = std::env::temp_dir().join(unique);
        std::fs::create_dir_all(&existing).expect("创建临时目录");
        let default = PathBuf::from("/default");

        let mut history = DirectoryHistory::default();
        history.remember_local(existing.clone());
        assert_eq!(history.local_or(default.clone()), existing);

        std::fs::remove_dir_all(&existing).expect("删除临时目录");
        assert_eq!(history.local_or(default.clone()), default);
    }

    #[test]
    fn config_migration_copies_missing_files_without_overwriting_new_values() {
        let root = std::env::temp_dir().join(format!(
            "blip-config-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("系统时间")
                .as_nanos()
        ));
        let legacy = root.join("macscp");
        let current = root.join("blip");
        std::fs::create_dir_all(legacy.join("nested")).expect("创建旧配置目录");
        std::fs::create_dir_all(&current).expect("创建新配置目录");
        std::fs::write(legacy.join("profiles.json"), b"legacy").expect("写旧配置");
        std::fs::write(legacy.join("nested/device_id"), b"device").expect("写旧设备 ID");
        std::fs::write(current.join("profiles.json"), b"current").expect("写新配置");

        copy_missing_config(&legacy, &current).expect("迁移配置");

        assert_eq!(
            std::fs::read(current.join("profiles.json")).expect("读取新配置"),
            b"current"
        );
        assert_eq!(
            std::fs::read(current.join("nested/device_id")).expect("读取迁移设备 ID"),
            b"device"
        );
        assert!(legacy.join("profiles.json").exists(), "旧配置必须保留");
        std::fs::remove_dir_all(root).expect("清理测试目录");
    }

    #[test]
    fn settings_prefers_notepad_and_migrates_previous_values() {
        let notepad = PathBuf::from("/Applications/Notepad--.app");
        let defaults = preferred_settings(Some(notepad.clone()));
        assert_eq!(
            defaults.remote_open_behavior,
            RemoteOpenBehavior::CustomEditor
        );
        assert_eq!(defaults.editor_path, Some(notepad));
        assert_eq!(
            defaults.lan_sftp_password_mode,
            LanSftpPasswordMode::Default
        );

        let fallback = preferred_settings(None);
        assert_eq!(
            fallback.remote_open_behavior,
            RemoteOpenBehavior::SystemDefault
        );
        assert_eq!(fallback.editor_path, None);
        assert_eq!(
            fallback.lan_sftp_password_mode,
            LanSftpPasswordMode::Default
        );

        let settings = AppSettings {
            remote_open_behavior: RemoteOpenBehavior::CustomEditor,
            editor_path: Some(PathBuf::from("/Applications/Visual Studio Code.app")),
            lan_sftp_root: Some(PathBuf::from("/Users/tester/Shared")),
            lan_sftp_password_mode: LanSftpPasswordMode::Manual,
        };
        let json = serde_json::to_string(&settings).expect("序列化设置");
        let decoded: AppSettings = serde_json::from_str(&json).expect("读回设置");
        assert_eq!(decoded, settings);

        let previous_random: AppSettings =
            serde_json::from_str(r#"{"lan_sftp_password_mode":"random"}"#)
                .expect("迁移原随机密码模式");
        assert_eq!(
            previous_random.lan_sftp_password_mode,
            LanSftpPasswordMode::Default
        );

        let previous_download: AppSettings =
            serde_json::from_str(r#"{"remote_open_behavior":"download"}"#).expect("迁移原下载设置");
        assert_eq!(
            previous_download.remote_open_behavior,
            RemoteOpenBehavior::SystemDefault
        );
        let migrated = migrate_legacy_editor(
            r#"{"remote_open_behavior":"download"}"#,
            previous_download,
            Some(PathBuf::from("/Applications/Notepad--.app")),
        );
        assert_eq!(
            migrated.remote_open_behavior,
            RemoteOpenBehavior::CustomEditor
        );
        assert_eq!(
            migrated.editor_path,
            Some(PathBuf::from("/Applications/Notepad--.app"))
        );
        let previous_text: AppSettings =
            serde_json::from_str(r#"{"remote_open_behavior":"text_editor"}"#)
                .expect("迁移原内置编辑器设置");
        assert_eq!(
            previous_text.remote_open_behavior,
            RemoteOpenBehavior::BuiltInTextEditor
        );
        assert_eq!(
            previous_text.lan_sftp_password_mode,
            LanSftpPasswordMode::Default
        );
    }

    /// 写-读-删一整圈。默认 `#[ignore]`：无人值守环境可能卡在系统凭据库授权弹窗上，
    /// 需要本机手动跑 `cargo test keychain_round_trip -- --ignored`。
    #[test]
    #[ignore = "会真的读写系统凭据库，需要人工授权"]
    fn keychain_round_trip() {
        let config = probe_config();
        forget_password(&config);

        store_password(&config, "s3cret-token").expect("写入钥匙串");
        assert_eq!(
            load_password(&config).expect("读取钥匙串").as_deref(),
            Some("s3cret-token")
        );

        forget_password(&config);
        assert_eq!(
            load_password(&config).expect("读取已删除凭证"),
            None,
            "删除后不应再读到"
        );
    }
}
