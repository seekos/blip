//! 会话持久化：收藏、设置与凭据统一写入主密码保护的 `~/.ssh/blip.toml`。

use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use age::secrecy::{ExposeSecret as _, SecretString};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::sftp::types::{AuthMethod, SessionConfig};

const CONFIG_FILE: &str = "blip.toml";
const LOCAL_UNLOCK_KEY_FILE: &str = ".blip-unlock-key";
const CONFIG_DIR_NAME: &str = "blip";
const LEGACY_CONFIG_DIR_NAME: &str = "macscp";
const LAN_SFTP_PASSWORD_ACCOUNT: &str = "lan-sftp-server-password";
const PROFILES_FILE: &str = "profiles.json";
const SETTINGS_FILE: &str = "settings.json";
const DIRECTORIES_FILE: &str = "directories.json";
const CONFIG_SCHEMA_VERSION: u32 = 1;
const PROFILES_SCHEMA_VERSION: u32 = 1;
// 本机解锁密钥是随机生成的 256 位密钥，不需要像人类密码一样用高成本 scrypt
// 抵抗字典攻击。主密码保护的配置正文仍使用 age 的自适应工作因子。
const LOCAL_UNLOCK_WORK_FACTOR: u8 = 2;
pub(crate) const PROFILE_LIMIT: usize = 50;
pub const DEFAULT_DISCOVERY_PORT: u16 = 1985;
pub const DEFAULT_LAN_HTTP_PORT: u16 = 1986;
pub const DEFAULT_LAN_SFTP_USERNAME: &str = "zy";

static UNLOCKED_CONFIG: OnceLock<Mutex<Option<UnlockedConfig>>> = OnceLock::new();
static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());
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
    /// 局域网发现列表中代替计算机名的可选用户名。
    pub discovery_username: Option<String>,
    /// UDP 局域网发现监听与广播端口。
    pub discovery_port: u16,
    /// 内置局域网 SFTP 的共享根目录；`None` 表示当前用户主目录。
    pub lan_sftp_root: Option<PathBuf>,
    /// 内置局域网 SFTP 与 HTTP 文件共享共用的登录用户名。
    pub lan_sftp_username: String,
    pub lan_sftp_password_mode: LanSftpPasswordMode,
    /// 浏览器文件共享服务的监听端口。
    pub lan_http_port: u16,
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
            discovery_username: None,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            lan_sftp_root: None,
            lan_sftp_username: DEFAULT_LAN_SFTP_USERNAME.to_string(),
            lan_sftp_password_mode: LanSftpPasswordMode::Default,
            lan_http_port: DEFAULT_LAN_HTTP_PORT,
        },
        None => AppSettings {
            remote_open_behavior: RemoteOpenBehavior::SystemDefault,
            editor_path: None,
            discovery_username: None,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            lan_sftp_root: None,
            lan_sftp_username: DEFAULT_LAN_SFTP_USERNAME.to_string(),
            lan_sftp_password_mode: LanSftpPasswordMode::Default,
            lan_http_port: DEFAULT_LAN_HTTP_PORT,
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
        let endpoint = self.endpoint_label();
        if let Some(name) = self
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            format!("{endpoint}-{name}")
        } else {
            endpoint
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedAuth {
    /// 密码存在加密配置的凭据表里，这里只是一个标记。
    Password,
    Key {
        path: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
struct ConfigDocument {
    version: u32,
    settings: AppSettings,
    profiles: Vec<SavedProfile>,
    credentials: BTreeMap<String, String>,
}

impl Default for ConfigDocument {
    fn default() -> Self {
        Self {
            version: CONFIG_SCHEMA_VERSION,
            settings: AppSettings::default(),
            profiles: Vec::new(),
            credentials: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct EncryptedConfigFile {
    version: u32,
    ciphertext: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local_unlock: Option<String>,
}

struct UnlockedConfig {
    passphrase: SecretString,
    document: ConfigDocument,
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

fn unlocked_config() -> &'static Mutex<Option<UnlockedConfig>> {
    UNLOCKED_CONFIG.get_or_init(|| Mutex::new(None))
}

fn encrypted_config_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".ssh").join(CONFIG_FILE))
        .ok_or_else(|| "无法确定用户主目录，不能定位 ~/.ssh/blip.toml".to_string())
}

fn local_unlock_key_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".ssh").join(LOCAL_UNLOCK_KEY_FILE))
        .ok_or_else(|| "无法确定用户主目录，不能定位本机解锁密钥".to_string())
}

pub fn is_config_initialized() -> bool {
    encrypted_config_path().is_ok_and(|path| path.is_file())
}

pub fn initialize_config(password: &str) -> Result<(), String> {
    if password.is_empty() {
        return Err("主密码不能为空".to_string());
    }
    let path = encrypted_config_path()?;
    if path.exists() {
        return Err(format!("配置文件已存在：{}", path.display()));
    }

    let passphrase = SecretString::from(password.to_owned());
    let document = ConfigDocument {
        profiles: load_legacy_profiles(),
        settings: load_legacy_settings(),
        ..ConfigDocument::default()
    };
    write_encrypted_config(&path, &passphrase, &document)?;
    *unlocked_config()
        .lock()
        .map_err(|_| "配置状态已损坏，请重启 Blip".to_string())? = Some(UnlockedConfig {
        passphrase,
        document,
    });
    Ok(())
}

pub fn try_auto_unlock_config() -> Result<bool, String> {
    let path = encrypted_config_path()?;
    let Some(outer) = read_encrypted_config(&path)? else {
        return Ok(false);
    };
    let Some(local_unlock) = outer.local_unlock else {
        return Ok(false);
    };
    let Some(local_key) = load_local_unlock_key()? else {
        return Ok(false);
    };
    let passphrase = match decrypt_master_password(&local_unlock, local_key) {
        Ok(passphrase) => passphrase,
        Err(message) => {
            return Err(format!("本机自动解锁失效：{message}"));
        }
    };
    let document = decrypt_document(&outer.ciphertext, passphrase.clone())?;
    *unlocked_config()
        .lock()
        .map_err(|_| "配置状态已损坏，请重启 Blip".to_string())? = Some(UnlockedConfig {
        passphrase,
        document,
    });
    Ok(true)
}

pub fn unlock_config(password: &str) -> Result<(), String> {
    let path = encrypted_config_path()?;
    let outer = read_encrypted_config(&path)?
        .ok_or_else(|| format!("配置文件不存在：{}", path.display()))?;

    let passphrase = SecretString::from(password.to_owned());
    let document = decrypt_document(&outer.ciphertext, passphrase.clone())?;
    // 旧配置首次手动解锁后补上本机自动解锁材料，以后启动不再询问。
    write_encrypted_config(&path, &passphrase, &document)?;
    *unlocked_config()
        .lock()
        .map_err(|_| "配置状态已损坏，请重启 Blip".to_string())? = Some(UnlockedConfig {
        passphrase,
        document,
    });
    Ok(())
}

fn read_encrypted_config(path: &Path) -> Result<Option<EncryptedConfigFile>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("读取配置文件 {} 失败：{err}", path.display())),
    };
    let outer: EncryptedConfigFile =
        toml::from_str(&raw).map_err(|err| format!("配置文件格式损坏：{err}"))?;
    if outer.version != CONFIG_SCHEMA_VERSION {
        return Err(format!(
            "不支持的配置版本 {}（当前支持 {}）",
            outer.version, CONFIG_SCHEMA_VERSION
        ));
    }
    Ok(Some(outer))
}

fn encrypt_document(document: &ConfigDocument, passphrase: SecretString) -> Result<String, String> {
    let plaintext =
        toml::to_string_pretty(document).map_err(|err| format!("序列化配置失败：{err}"))?;
    let recipient = age::scrypt::Recipient::new(passphrase);
    let ciphertext = age::encrypt(&recipient, plaintext.as_bytes())
        .map_err(|err| format!("加密配置失败：{err}"))?;
    Ok(BASE64.encode(ciphertext))
}

fn decrypt_document(ciphertext: &str, passphrase: SecretString) -> Result<ConfigDocument, String> {
    let ciphertext = BASE64
        .decode(ciphertext.trim())
        .map_err(|err| format!("配置密文的 Base64 无效：{err}"))?;
    let identity = age::scrypt::Identity::new(passphrase);
    let plaintext = age::decrypt(&identity, &ciphertext)
        .map_err(|_| "主密码错误，或配置密文已经损坏".to_string())?;
    let plaintext =
        String::from_utf8(plaintext).map_err(|_| "解密后的配置不是有效 UTF-8".to_string())?;
    let mut document: ConfigDocument =
        toml::from_str(&plaintext).map_err(|err| format!("解密后的配置格式损坏：{err}"))?;
    if document.version != CONFIG_SCHEMA_VERSION {
        return Err(format!(
            "不支持的配置内容版本 {}（当前支持 {}）",
            document.version, CONFIG_SCHEMA_VERSION
        ));
    }
    document.profiles = normalize_profiles(document.profiles);
    Ok(document)
}

fn encrypt_master_password(
    passphrase: &SecretString,
    local_key: SecretString,
) -> Result<String, String> {
    let mut recipient = age::scrypt::Recipient::new(local_key);
    recipient.set_work_factor(LOCAL_UNLOCK_WORK_FACTOR);
    let ciphertext = age::encrypt(&recipient, passphrase.expose_secret().as_bytes())
        .map_err(|err| format!("创建本机自动解锁数据失败：{err}"))?;
    Ok(BASE64.encode(ciphertext))
}

fn decrypt_master_password(
    ciphertext: &str,
    local_key: SecretString,
) -> Result<SecretString, String> {
    let ciphertext = BASE64
        .decode(ciphertext.trim())
        .map_err(|err| format!("本机自动解锁数据的 Base64 无效：{err}"))?;
    let identity = age::scrypt::Identity::new(local_key);
    let plaintext =
        age::decrypt(&identity, &ciphertext).map_err(|_| "本机解锁密钥与配置不匹配".to_string())?;
    String::from_utf8(plaintext)
        .map(SecretString::from)
        .map_err(|_| "本机自动解锁数据不是有效 UTF-8".to_string())
}

fn ensure_secure_ssh_directory(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "配置文件路径缺少父目录".to_string())?;
    std::fs::create_dir_all(parent).map_err(|err| format!("创建 ~/.ssh 目录失败：{err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            .map_err(|err| format!("设置 ~/.ssh 目录权限失败：{err}"))?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn machine_fingerprint() -> Result<String, String> {
    let output = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-rd1", "-c", "IOPlatformExpertDevice"])
        .output()
        .map_err(|err| format!("读取 Mac 机器标识失败：{err}"))?;
    if !output.status.success() {
        return Err("读取 Mac 机器标识失败".to_string());
    }
    let output = String::from_utf8_lossy(&output.stdout);
    output
        .lines()
        .find(|line| line.contains("\"IOPlatformUUID\""))
        .and_then(|line| line.split('"').nth(3))
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Mac 机器标识为空".to_string())
}

#[cfg(target_os = "linux")]
fn machine_fingerprint() -> Result<String, String> {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(value) = std::fs::read_to_string(path) {
            let value = value.trim();
            if !value.is_empty() {
                return Ok(value.to_string());
            }
        }
    }
    Err("无法读取 Linux machine-id".to_string())
}

#[cfg(target_os = "windows")]
fn machine_fingerprint() -> Result<String, String> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt as _};
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};

    fn wide(value: &str) -> Vec<u16> {
        OsStr::new(value).encode_wide().chain(Some(0)).collect()
    }

    let subkey = wide(r"SOFTWARE\Microsoft\Cryptography");
    let value = wide("MachineGuid");
    let mut byte_len = 0_u32;
    // SAFETY: 两个 UTF-16 字符串均以 NUL 结尾；首次调用只查询缓冲区长度。
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut byte_len,
        )
    };
    if result != 0 || byte_len < 2 {
        return Err(format!("读取 Windows MachineGuid 失败：{result}"));
    }
    let mut buffer = vec![0_u16; byte_len.div_ceil(2) as usize];
    // SAFETY: 缓冲区按上次返回的字节数分配，指针在调用期间有效。
    let result = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut byte_len,
        )
    };
    if result != 0 {
        return Err(format!("读取 Windows MachineGuid 失败：{result}"));
    }
    let length = (byte_len as usize / 2).min(buffer.len());
    let value = String::from_utf16_lossy(&buffer[..length])
        .trim_end_matches('\0')
        .trim()
        .to_string();
    if value.is_empty() {
        Err("Windows MachineGuid 为空".to_string())
    } else {
        Ok(value)
    }
}

fn bind_local_unlock_key(raw_key: &str, machine: &str, user_home: &str) -> SecretString {
    let mut digest = Sha256::new();
    digest.update(b"blip-local-unlock-v1\0");
    digest.update(machine.as_bytes());
    digest.update(b"\0");
    digest.update(user_home.as_bytes());
    digest.update(b"\0");
    digest.update(raw_key.as_bytes());
    SecretString::from(BASE64.encode(digest.finalize()))
}

fn bound_local_unlock_key(raw_key: &str) -> Result<SecretString, String> {
    let machine = machine_fingerprint()?;
    let home = dirs::home_dir()
        .ok_or_else(|| "无法确定当前用户主目录".to_string())?
        .to_string_lossy()
        .to_string();
    Ok(bind_local_unlock_key(raw_key, &machine, &home))
}

fn load_local_unlock_key() -> Result<Option<SecretString>, String> {
    let path = local_unlock_key_path()?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("读取本机解锁密钥失败：{err}")),
    };
    let encoded = raw.trim();
    let bytes = BASE64
        .decode(encoded)
        .map_err(|err| format!("本机解锁密钥格式损坏：{err}"))?;
    if bytes.len() != 32 {
        return Err("本机解锁密钥长度无效".to_string());
    }
    bound_local_unlock_key(encoded).map(Some)
}

fn load_or_create_local_unlock_key() -> Result<SecretString, String> {
    match load_local_unlock_key() {
        Ok(Some(key)) => return Ok(key),
        Ok(None) => {}
        Err(message) => {
            // 主密码已经在内存中时，本机解锁密钥只是可重建的缓存。
            log::warn!("{message}；将重新生成本机解锁密钥");
        }
    }
    let path = local_unlock_key_path()?;
    ensure_secure_ssh_directory(&path)?;
    let mut bytes = [0_u8; 32];
    rand::fill(&mut bytes);
    let encoded = BASE64.encode(bytes);
    write_atomic(&path, encoded.as_bytes())
        .map_err(|err| format!("写入本机解锁密钥失败：{err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("设置本机解锁密钥权限失败：{err}"))?;
    }
    bound_local_unlock_key(&encoded)
}

fn write_encrypted_config(
    path: &Path,
    passphrase: &SecretString,
    document: &ConfigDocument,
) -> Result<(), String> {
    ensure_secure_ssh_directory(path)?;
    let local_unlock = encrypt_master_password(passphrase, load_or_create_local_unlock_key()?)?;
    let outer = EncryptedConfigFile {
        version: CONFIG_SCHEMA_VERSION,
        ciphertext: encrypt_document(document, passphrase.clone())?,
        local_unlock: Some(local_unlock),
    };
    let raw =
        toml::to_string_pretty(&outer).map_err(|err| format!("序列化加密配置外层失败：{err}"))?;
    write_atomic(path, raw.as_bytes()).map_err(|err| format!("写入加密配置失败：{err}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|err| format!("设置配置文件权限失败：{err}"))?;
    }
    Ok(())
}

fn update_config(update: impl FnOnce(&mut ConfigDocument)) -> Result<(), String> {
    // 加密和 fsync 可能持续一秒以上。单独串行化写入，并且不要在这期间占着
    // 内存配置锁；这样前台读取密码或设置不会跟着后台保存一起卡住。
    let _write = CONFIG_WRITE_LOCK
        .lock()
        .map_err(|_| "配置写入状态已损坏，请重启 Blip".to_string())?;
    let path = encrypted_config_path()?;
    let (passphrase, mut document) = {
        let unlocked = unlocked_config()
            .lock()
            .map_err(|_| "配置状态已损坏，请重启 Blip".to_string())?;
        let unlocked = unlocked
            .as_ref()
            .ok_or_else(|| "配置尚未解锁".to_string())?;
        (unlocked.passphrase.clone(), unlocked.document.clone())
    };
    update(&mut document);
    write_encrypted_config(&path, &passphrase, &document)?;
    unlocked_config()
        .lock()
        .map_err(|_| "配置状态已损坏，请重启 Blip".to_string())?
        .as_mut()
        .ok_or_else(|| "配置尚未解锁".to_string())?
        .document = document;
    Ok(())
}

fn read_config<T>(read: impl FnOnce(&ConfigDocument) -> T) -> Result<T, String> {
    let unlocked = unlocked_config()
        .lock()
        .map_err(|_| "配置状态已损坏，请重启 Blip".to_string())?;
    unlocked
        .as_ref()
        .map(|unlocked| read(&unlocked.document))
        .ok_or_else(|| "配置尚未解锁".to_string())
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

fn load_legacy_profiles() -> Vec<SavedProfile> {
    let Some((path, raw)) = read_config_file(PROFILES_FILE) else {
        return Vec::new();
    };
    match load_profiles_from(&path, &raw) {
        Ok(profiles) => profiles,
        Err(message) => {
            log::warn!("{message}");
            Vec::new()
        }
    }
}

pub fn load_profiles() -> Vec<SavedProfile> {
    read_config(|document| document.profiles.clone()).unwrap_or_default()
}

pub fn save_profiles(profiles: &[SavedProfile]) -> Result<(), String> {
    let profiles = normalize_profiles(profiles.to_vec());
    update_config(move |document| document.profiles = profiles)
}

#[cfg(test)]
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

fn load_legacy_settings() -> AppSettings {
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

pub fn load_settings() -> AppSettings {
    read_config(|document| document.settings.clone()).unwrap_or_default()
}

pub fn save_settings_with_lan_password(
    settings: &AppSettings,
    manual_password: Option<&str>,
) -> Result<(), String> {
    let settings = settings.clone();
    let manual_password = manual_password.map(str::to_owned);
    update_config(move |document| {
        document.settings = settings;
        if let Some(password) = manual_password {
            document
                .credentials
                .insert(LAN_SFTP_PASSWORD_ACCOUNT.to_string(), password);
        }
    })
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
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
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

pub fn load_lan_sftp_password() -> Result<Option<String>, String> {
    load_credential(LAN_SFTP_PASSWORD_ACCOUNT)
}

fn store_credential(account: String, secret: String) -> Result<(), String> {
    update_config(move |document| {
        document.credentials.insert(account, secret);
    })
}

fn load_credential(account: &str) -> Result<Option<String>, String> {
    read_config(|document| document.credentials.get(account).cloned())
}

fn forget_credential(account: String) {
    if let Err(message) = update_config(move |document| {
        document.credentials.remove(&account);
    }) {
        log::warn!("删除加密配置中的凭据失败：{message}");
    }
}

fn password_account(config: &SessionConfig) -> String {
    format!("password:{}", config.credential_account())
}

pub fn store_password(config: &SessionConfig, password: &str) -> Result<(), String> {
    store_credential(password_account(config), password.to_string())
}

pub fn load_password(config: &SessionConfig) -> Result<Option<String>, String> {
    load_credential(&password_account(config))
}

pub fn forget_password(config: &SessionConfig) {
    forget_credential(password_account(config));
}

fn key_passphrase_account(config: &SessionConfig) -> String {
    format!("key-passphrase:{}", config.credential_account())
}

pub fn store_key_passphrase(config: &SessionConfig, passphrase: &str) -> Result<(), String> {
    store_credential(key_passphrase_account(config), passphrase.to_string())
}

pub fn load_key_passphrase(config: &SessionConfig) -> Result<Option<String>, String> {
    load_credential(&key_passphrase_account(config))
}

pub fn forget_key_passphrase(config: &SessionConfig) {
    forget_credential(key_passphrase_account(config));
}

pub fn forget_profile_credential(profile: &SavedProfile) {
    let config = config_for_profile(profile);
    match profile.auth {
        SavedAuth::Password => forget_password(&config),
        SavedAuth::Key { .. } => forget_key_passphrase(&config),
    }
}

pub fn save_profile_removal(
    profiles: &[SavedProfile],
    removed: &SavedProfile,
) -> Result<(), String> {
    let profiles = normalize_profiles(profiles.to_vec());
    let removed = removed.clone();
    update_config(move |document| apply_profile_removal(document, profiles, &removed))
}

fn apply_profile_removal(
    document: &mut ConfigDocument,
    profiles: Vec<SavedProfile>,
    removed: &SavedProfile,
) {
    document.profiles = profiles;
    remove_profile_credential(document, removed);
}

fn remove_profile_credential(document: &mut ConfigDocument, profile: &SavedProfile) {
    let config = config_for_profile(profile);
    let account = match profile.auth {
        SavedAuth::Password => password_account(&config),
        SavedAuth::Key { .. } => key_passphrase_account(&config),
    };
    document.credentials.remove(&account);
}

/// 连接成功后的收藏、密码与旧凭据清理一次性落盘，避免一次连接连续做多轮
/// 主密码派生，同时保证配置正文始终是一个一致快照。
pub fn save_connection(
    profiles: &[SavedProfile],
    config: &SessionConfig,
    remember_password: bool,
    previous: Option<&SavedProfile>,
    evicted: Option<&SavedProfile>,
) -> Result<(), String> {
    let profiles = normalize_profiles(profiles.to_vec());
    let config = config.clone();
    let previous = previous.cloned();
    let evicted = evicted.cloned();
    update_config(move |document| {
        apply_connection_update(
            document,
            profiles,
            &config,
            remember_password,
            previous.as_ref(),
            evicted.as_ref(),
        )
    })
}

fn apply_connection_update(
    document: &mut ConfigDocument,
    profiles: Vec<SavedProfile>,
    config: &SessionConfig,
    remember_password: bool,
    previous: Option<&SavedProfile>,
    evicted: Option<&SavedProfile>,
) {
    document.profiles = profiles;
    if let Some(evicted) = evicted {
        remove_profile_credential(document, evicted);
    }

    let current_password = password_account(config);
    let current_key_passphrase = key_passphrase_account(config);
    match &config.auth {
        AuthMethod::Password(password) => {
            document.credentials.remove(&current_key_passphrase);
            if remember_password {
                document
                    .credentials
                    .insert(current_password.clone(), password.clone());
            } else {
                document.credentials.remove(&current_password);
            }

            if let Some(previous) = previous {
                let old_config = config_for_profile(previous);
                match previous.auth {
                    SavedAuth::Password => {
                        let old = password_account(&old_config);
                        if old != current_password {
                            document.credentials.remove(&old);
                        }
                    }
                    SavedAuth::Key { .. } => {
                        document
                            .credentials
                            .remove(&key_passphrase_account(&old_config));
                    }
                }
            }
        }
        AuthMethod::PrivateKey { passphrase, .. } => {
            document.credentials.remove(&current_password);
            if remember_password {
                if let Some(passphrase) = passphrase.as_deref().filter(|value| !value.is_empty()) {
                    document
                        .credentials
                        .insert(current_key_passphrase.clone(), passphrase.to_string());
                } else {
                    document.credentials.remove(&current_key_passphrase);
                }
            } else {
                document.credentials.remove(&current_key_passphrase);
            }
            if let Some(previous) = previous {
                let old_config = config_for_profile(previous);
                match previous.auth {
                    SavedAuth::Password => {
                        document.credentials.remove(&password_account(&old_config));
                    }
                    SavedAuth::Key { .. } => {
                        let old = key_passphrase_account(&old_config);
                        if old != current_key_passphrase {
                            document.credentials.remove(&old);
                        }
                    }
                }
            }
        }
    }
}

fn same_profile(left: &SavedProfile, right: &SavedProfile) -> bool {
    left.host == right.host && left.port == right.port && left.username == right.username
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

/// 记录/更新一条收藏（按 `user@host:port` 去重）。
///
/// 已有条目保留原写入位置；新条目写到最前。超过上限时优先淘汰未置顶且最久未连接的条目。
pub fn upsert_profile(profiles: &mut Vec<SavedProfile>, profile: SavedProfile) -> ProfileUpsert {
    let index = profiles
        .iter()
        .position(|item| same_profile(item, &profile))
        .unwrap_or(0);
    let previous = profiles
        .iter()
        .find(|item| same_profile(item, &profile))
        .cloned();
    profiles.retain(|item| !same_profile(item, &profile));
    profiles.insert(index.min(profiles.len()), profile);
    ProfileUpsert {
        previous,
        evicted: trim_profiles(profiles),
    }
}

/// 编辑收藏时允许修改唯一键，但不能覆盖另一条已有收藏。
pub fn replace_profile(
    profiles: &mut Vec<SavedProfile>,
    previous: &SavedProfile,
    profile: SavedProfile,
) -> Result<ProfileUpsert, String> {
    if same_profile(previous, &profile) {
        return Ok(upsert_profile(profiles, profile));
    }
    if profiles.iter().any(|item| same_profile(item, &profile)) {
        return Err("该主机、端口和用户名已在收藏中".to_string());
    }
    let previous = remove_profile(profiles, &previous.username, &previous.host, previous.port)
        .ok_or_else(|| "原收藏已不存在，请重新打开设置后再试".to_string())?;
    let mut result = upsert_profile(profiles, profile);
    result.previous = Some(previous);
    Ok(result)
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

/// 编辑收藏导致连接标识变化时，先复制密码到新 account，成功后再删除旧 account。
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
        &old_config.credential_account(),
        &new_config.credential_account(),
        |_| load_password(&old_config),
        |_, password| store_password(&new_config, password),
        |_| forget_password(&old_config),
    )
}

/// 编辑私钥收藏导致连接标识变化时，把密钥口令迁移到新的独立 account。
pub fn migrate_profile_key_passphrase(
    previous: &SavedProfile,
    current: &SavedProfile,
) -> Result<bool, String> {
    if !matches!(previous.auth, SavedAuth::Key { .. })
        || !matches!(current.auth, SavedAuth::Key { .. })
    {
        return Ok(false);
    }
    let old_config = config_for_profile(previous);
    let new_config = config_for_profile(current);
    migrate_password_with(
        &key_passphrase_account(&old_config),
        &key_passphrase_account(&new_config),
        |_| load_key_passphrase(&old_config),
        |_, passphrase| store_key_passphrase(&new_config, passphrase),
        |_| forget_key_passphrase(&old_config),
    )
}

/// 删除一条收藏（唯一键与写入时一致：user@host:port）。
pub fn remove_profile(
    profiles: &mut Vec<SavedProfile>,
    username: &str,
    host: &str,
    port: u16,
) -> Option<SavedProfile> {
    let index = profiles
        .iter()
        .position(|item| item.host == host && item.port == port && item.username == username)?;
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
            host: "blip-config-probe.invalid".to_string(),
            port: 22,
            username: "tester".to_string(),
            auth: AuthMethod::Password(String::new()),
        }
    }

    #[test]
    fn favorite_title_keeps_the_login_endpoint_when_named() {
        let mut favorite = profile("192.168.1.8", 22, "root");
        assert_eq!(favorite.title(), "root@192.168.1.8:22");

        favorite.name = Some("NAS".to_string());
        assert_eq!(favorite.title(), "root@192.168.1.8:22-NAS");
    }

    /// account 键必须带上端口，否则同一台机器的 22 和 2222 会互相覆盖。
    #[test]
    fn credential_account_is_unique_per_endpoint() {
        let mut config = probe_config();
        let a = config.credential_account();
        config.port = 2222;
        let b = config.credential_account();

        assert_ne!(a, b);
        assert!(a.contains("tester@"));
        assert!(a.ends_with(":22"));
    }

    #[test]
    fn reconnect_replaces_the_same_endpoint_in_place() {
        let mut profiles = vec![
            profile("first", 22, "root"),
            profile("second", 22, "root"),
            profile("second", 22, "admin"),
        ];

        let mut updated = profile("second", 22, "root");
        updated.last_connected = Some(42);
        let result = upsert_profile(&mut profiles, updated);

        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].host, "first");
        assert_eq!(profiles[1].last_connected, Some(42));
        assert_eq!(profiles[2].username, "admin");
        assert_eq!(result.previous.expect("旧收藏").port, 22);
    }

    #[test]
    fn reconnect_removes_all_duplicate_endpoint_rows() {
        let duplicate = profile("server", 22, "root");
        let mut profiles = vec![
            profile("first", 22, "root"),
            duplicate.clone(),
            duplicate,
            profile("last", 22, "root"),
        ];

        let mut updated = profile("server", 22, "root");
        updated.last_connected = Some(42);
        upsert_profile(&mut profiles, updated);

        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[1].host, "server");
        assert_eq!(profiles[1].last_connected, Some(42));
    }

    #[test]
    fn different_ports_are_distinct_favorites() {
        let mut profiles = vec![profile("server", 22, "root")];

        let result = upsert_profile(&mut profiles, profile("server", 2022, "root"));

        assert_eq!(profiles.len(), 2);
        assert!(result.previous.is_none());
        assert!(profiles.iter().any(|profile| profile.port == 22));
        assert!(profiles.iter().any(|profile| profile.port == 2022));
    }

    #[test]
    fn editing_identity_replaces_only_the_selected_favorite() {
        let original = profile("server", 22, "root");
        let mut profiles = vec![original.clone(), profile("server", 2022, "admin")];

        let result = replace_profile(
            &mut profiles,
            &original,
            profile("renamed.example", 2222, "deploy"),
        )
        .unwrap();

        assert_eq!(profiles.len(), 2);
        assert!(profiles.iter().any(|profile| {
            profile.host == "renamed.example"
                && profile.port == 2222
                && profile.username == "deploy"
        }));
        assert_eq!(result.previous.unwrap(), original);
    }

    #[test]
    fn editing_cannot_overwrite_another_favorite() {
        let original = profile("server", 22, "root");
        let mut profiles = vec![original.clone(), profile("server", 2022, "root")];

        let error =
            replace_profile(&mut profiles, &original, profile("server", 2022, "root")).unwrap_err();

        assert!(error.contains("已在收藏中"));
        assert_eq!(profiles.len(), 2);
    }

    #[test]
    fn loading_keeps_each_user_host_and_port_combination() {
        let profiles = normalize_profiles(vec![
            profile("server", 2022, "root"),
            profile("server", 22, "root"),
        ]);

        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].port, 2022);
        assert_eq!(profiles[1].port, 22);
    }

    #[test]
    fn removing_a_favorite_keeps_other_ports_and_users_on_the_same_host() {
        let mut profiles = vec![
            profile("server", 22, "root"),
            profile("server", 2022, "root"),
            profile("server", 22, "admin"),
        ];

        let removed = remove_profile(&mut profiles, "root", "server", 22).unwrap();

        assert_eq!(removed.username, "root");
        assert_eq!(removed.port, 22);
        assert_eq!(profiles.len(), 2);
        assert!(profiles.iter().any(|profile| profile.port == 2022));
        assert!(profiles.iter().any(|profile| profile.username == "admin"));
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
    fn changing_identity_migrates_password_before_deleting_old_account() {
        let calls = Rc::new(RefCell::new(Vec::new()));
        let load_calls = calls.clone();
        let store_calls = calls.clone();
        let delete_calls = calls.clone();

        let migrated = migrate_password_with(
            "root@server:22",
            "deploy@renamed.example:2022",
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
                "store:deploy@renamed.example:2022:secret",
                "delete:root@server:22",
            ]
        );
    }

    #[test]
    fn key_passphrase_uses_a_separate_credential_account() {
        let config = probe_config();

        assert_eq!(
            config.credential_account(),
            "tester@blip-config-probe.invalid:22"
        );
        assert_eq!(
            key_passphrase_account(&config),
            "key-passphrase:tester@blip-config-probe.invalid:22"
        );
    }

    #[test]
    fn profile_removal_updates_profiles_and_credentials_together() {
        let mut document = ConfigDocument::default();
        let removed = profile("removed", 22, "root");
        let kept = profile("kept", 22, "root");
        document.profiles = vec![removed.clone(), kept.clone()];
        document
            .credentials
            .insert("password:root@removed:22".into(), "removed-secret".into());
        document
            .credentials
            .insert("password:root@kept:22".into(), "kept-secret".into());

        apply_profile_removal(&mut document, vec![kept.clone()], &removed);

        assert_eq!(document.profiles, vec![kept]);
        assert!(
            !document
                .credentials
                .contains_key("password:root@removed:22")
        );
        assert!(document.credentials.contains_key("password:root@kept:22"));
    }

    #[test]
    fn connection_update_saves_password_and_cleans_replaced_credentials_once() {
        let mut document = ConfigDocument::default();
        let mut previous = profile("server", 22, "root");
        previous.auth = SavedAuth::Key {
            path: "/tmp/id".into(),
        };
        let evicted = profile("old-server", 22, "admin");
        document.credentials.insert(
            "key-passphrase:root@server:22".into(),
            "old-key-secret".into(),
        );
        document.credentials.insert(
            "password:admin@old-server:22".into(),
            "evicted-secret".into(),
        );
        let config = SessionConfig {
            host: "server".into(),
            port: 2022,
            username: "root".into(),
            auth: AuthMethod::Password("new-password".into()),
        };
        let current = profile("server", 2022, "root");

        apply_connection_update(
            &mut document,
            vec![current.clone()],
            &config,
            true,
            Some(&previous),
            Some(&evicted),
        );

        assert_eq!(document.profiles, vec![current]);
        assert_eq!(
            document.credentials.get("password:root@server:2022"),
            Some(&"new-password".to_string())
        );
        assert!(
            !document
                .credentials
                .contains_key("key-passphrase:root@server:22")
        );
        assert!(
            !document
                .credentials
                .contains_key("password:admin@old-server:22")
        );
    }

    #[test]
    fn connection_update_forgets_key_passphrase_when_not_requested() {
        let mut document = ConfigDocument::default();
        let mut previous = profile("server", 22, "root");
        previous.auth = SavedAuth::Key {
            path: "/tmp/old-key".into(),
        };
        document
            .credentials
            .insert("key-passphrase:root@server:22".into(), "key-secret".into());
        let config = SessionConfig {
            host: "server".into(),
            port: 2022,
            username: "root".into(),
            auth: AuthMethod::PrivateKey {
                path: "/tmp/new-key".into(),
                passphrase: None,
            },
        };

        apply_connection_update(
            &mut document,
            Vec::new(),
            &config,
            false,
            Some(&previous),
            None,
        );

        assert!(
            !document
                .credentials
                .contains_key("key-passphrase:root@server:22")
        );
        assert!(
            !document
                .credentials
                .contains_key("key-passphrase:root@server:2022")
        );
    }

    #[test]
    fn connection_update_saves_key_passphrase_when_requested() {
        let mut document = ConfigDocument::default();
        let config = SessionConfig {
            host: "server".into(),
            port: 2022,
            username: "root".into(),
            auth: AuthMethod::PrivateKey {
                path: "/tmp/id".into(),
                passphrase: Some("key-secret".into()),
            },
        };

        apply_connection_update(&mut document, Vec::new(), &config, true, None, None);

        assert_eq!(
            document.credentials.get("key-passphrase:root@server:2022"),
            Some(&"key-secret".to_string())
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
            discovery_username: Some("小明".to_string()),
            discovery_port: 21985,
            lan_sftp_root: Some(PathBuf::from("/Users/tester/Shared")),
            lan_sftp_username: "phone".to_string(),
            lan_sftp_password_mode: LanSftpPasswordMode::Manual,
            lan_http_port: 2986,
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
        assert_eq!(previous_random.discovery_username, None);
        assert_eq!(previous_random.discovery_port, DEFAULT_DISCOVERY_PORT);
        assert_eq!(previous_random.lan_sftp_username, DEFAULT_LAN_SFTP_USERNAME);
        assert_eq!(previous_random.lan_http_port, DEFAULT_LAN_HTTP_PORT);

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

    #[test]
    fn encrypted_document_round_trips_and_rejects_the_wrong_password() {
        let mut document = ConfigDocument::default();
        document.profiles.push(profile("server", 22, "root"));
        document
            .credentials
            .insert("password:root@server:22".into(), "s3cret-token".into());
        let password = SecretString::from("correct horse battery staple".to_string());

        let encrypted = encrypt_document(&document, password.clone()).expect("加密配置");
        assert!(!encrypted.contains("s3cret-token"));
        let restored = decrypt_document(&encrypted, password).expect("解密配置");

        assert_eq!(restored.profiles, document.profiles);
        assert_eq!(restored.credentials, document.credentials);
        assert!(
            decrypt_document(&encrypted, SecretString::from("wrong password".to_string())).is_err()
        );
    }

    #[test]
    fn local_unlock_wraps_the_master_password_and_keeps_old_files_compatible() {
        let password = SecretString::from("master password".to_string());
        let local_key = SecretString::from(BASE64.encode([7_u8; 32]));

        let wrapped = encrypt_master_password(&password, local_key.clone()).expect("包裹主密码");
        let restored = decrypt_master_password(&wrapped, local_key).expect("自动解锁主密码");

        assert_eq!(restored.expose_secret(), password.expose_secret());
        assert!(
            decrypt_master_password(&wrapped, SecretString::from(BASE64.encode([8_u8; 32])))
                .is_err()
        );

        let previous: EncryptedConfigFile =
            toml::from_str("version = 1\nciphertext = 'old'").expect("读取旧配置外层");
        assert!(previous.local_unlock.is_none());

        let first_machine = bind_local_unlock_key("key", "machine-a", "/home/user");
        let other_machine = bind_local_unlock_key("key", "machine-b", "/home/user");
        let other_user = bind_local_unlock_key("key", "machine-a", "/home/other");
        assert_ne!(first_machine.expose_secret(), other_machine.expose_secret());
        assert_ne!(first_machine.expose_secret(), other_user.expose_secret());
    }
}
