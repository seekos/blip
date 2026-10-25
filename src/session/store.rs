//! 会话持久化：密码进 macOS Keychain，连接配置进 Application Support 下的 JSON。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::sftp::types::SessionConfig;

/// Keychain 里的 service 名，所有条目共享。
const KEYRING_SERVICE: &str = "com.macscp.credentials";
const PROFILES_FILE: &str = "profiles.json";
const SETTINGS_FILE: &str = "settings.json";
const DIRECTORIES_FILE: &str = "directories.json";

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
    /// 下载到编辑缓存后，用 macOS 为该文件选择的默认应用打开。
    #[serde(alias = "download")]
    #[default]
    SystemDefault,
    /// 下载到编辑缓存后，始终用用户指定的应用打开。
    CustomEditor,
    /// 忽略扩展名和内容类型，直接用内置文本编辑器打开。
    #[serde(alias = "text_editor")]
    BuiltInTextEditor,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub remote_open_behavior: RemoteOpenBehavior,
    /// 自定义编辑器的 `.app` 或可执行文件路径；系统默认和内置模式下忽略。
    pub editor_path: Option<PathBuf>,
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
        },
        None => AppSettings {
            remote_open_behavior: RemoteOpenBehavior::SystemDefault,
            editor_path: None,
        },
    }
}

fn find_notepad_editor() -> Option<PathBuf> {
    let mut candidates = vec![
        PathBuf::from("/Applications/Notepad--.app"),
        PathBuf::from("/System/Applications/Notepad--.app"),
    ];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join("Applications/Notepad--.app"));
    }
    if let Some(path) = candidates.into_iter().find(|path| path.is_dir()) {
        return Some(path);
    }

    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|dir| dir.join("notepad--"))
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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedProfile {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SavedAuth,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SavedAuth {
    /// 密码存在 Keychain 里，这里只是一个标记。
    Password,
    Key {
        path: String,
    },
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("macscp")
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
    for profile in profiles.into_iter().rev() {
        upsert_profile(&mut normalized, profile);
    }
    normalized
}

pub fn load_profiles() -> Vec<SavedProfile> {
    let path = profiles_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str(&raw) {
        Ok(profiles) => normalize_profiles(profiles),
        Err(err) => {
            log::warn!("忽略损坏的配置文件 {}：{err}", path.display());
            Vec::new()
        }
    }
}

pub fn save_profiles(profiles: &[SavedProfile]) -> Result<(), String> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir).map_err(|err| format!("创建配置目录失败：{err}"))?;
    let json = serde_json::to_string_pretty(profiles)
        .map_err(|err| format!("序列化连接配置失败：{err}"))?;
    std::fs::write(profiles_path(), json).map_err(|err| format!("写入连接配置失败：{err}"))
}

pub fn load_settings() -> AppSettings {
    let path = settings_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
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
        serde_json::to_string_pretty(settings).map_err(|err| format!("序列化设置失败：{err}"))?;
    std::fs::write(settings_path(), json).map_err(|err| format!("写入设置失败：{err}"))
}

pub fn load_directory_history() -> DirectoryHistory {
    let path = directories_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
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
    let json = serde_json::to_string_pretty(history)
        .map_err(|err| format!("序列化目录历史失败：{err}"))?;
    std::fs::write(directories_path(), json).map_err(|err| format!("写入目录历史失败：{err}"))
}

/// 把密码写进 Keychain。
pub fn store_password(config: &SessionConfig, password: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &config.keyring_account())
        .map_err(|err| format!("无法访问钥匙串：{err}"))?;
    entry
        .set_password(password)
        .map_err(|err| format!("写入钥匙串失败：{err}"))
}

/// 从 Keychain 取密码；没有记录时返回 `None`（不是错误）。
pub fn load_password(config: &SessionConfig) -> Option<String> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, &config.keyring_account()).ok()?;
    entry.get_password().ok()
}

pub fn forget_password(config: &SessionConfig) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &config.keyring_account()) {
        let _ = entry.delete_credential();
    }
}

/// 记录/更新一条连接配置（按 user@host 去重，端口变化视为同一收藏）。
pub fn upsert_profile(profiles: &mut Vec<SavedProfile>, profile: SavedProfile) {
    profiles.retain(|item| !(item.host == profile.host && item.username == profile.username));
    profiles.insert(0, profile);
    profiles.truncate(20);
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

    use super::*;
    use crate::sftp::types::AuthMethod;

    fn probe_config() -> SessionConfig {
        SessionConfig {
            host: "macscp-keychain-probe.invalid".to_string(),
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

    #[test]
    fn reconnect_moves_existing_favorite_to_front() {
        let profile = |host: &str, port: u16, username: &str| SavedProfile {
            host: host.to_string(),
            port,
            username: username.to_string(),
            auth: SavedAuth::Password,
        };
        let mut profiles = vec![
            profile("first", 22, "root"),
            profile("second", 22, "root"),
            profile("second", 22, "admin"),
        ];

        upsert_profile(&mut profiles, profile("second", 2022, "root"));

        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].host, "second");
        assert_eq!(profiles[0].port, 2022);
        assert_eq!(profiles[1].host, "first");
        assert_eq!(profiles[2].username, "admin");
    }

    #[test]
    fn loading_keeps_the_newest_favorite_per_user_and_host() {
        let profile = |port| SavedProfile {
            host: "server".to_string(),
            port,
            username: "root".to_string(),
            auth: SavedAuth::Password,
        };

        let profiles = normalize_profiles(vec![profile(2022), profile(22)]);

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].port, 2022);
    }

    #[test]
    fn removing_a_favorite_keeps_other_users_on_the_same_host() {
        let profile = |username: &str| SavedProfile {
            host: "server".to_string(),
            port: 22,
            username: username.to_string(),
            auth: SavedAuth::Password,
        };
        let mut profiles = vec![profile("root"), profile("admin")];

        let removed = remove_profile(&mut profiles, "root", "server").unwrap();

        assert_eq!(removed.username, "root");
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].username, "admin");
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
            "macscp-directory-history-{}-{}",
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
    fn settings_prefers_notepad_and_migrates_previous_values() {
        let notepad = PathBuf::from("/Applications/Notepad--.app");
        let defaults = preferred_settings(Some(notepad.clone()));
        assert_eq!(
            defaults.remote_open_behavior,
            RemoteOpenBehavior::CustomEditor
        );
        assert_eq!(defaults.editor_path, Some(notepad));

        let fallback = preferred_settings(None);
        assert_eq!(
            fallback.remote_open_behavior,
            RemoteOpenBehavior::SystemDefault
        );
        assert_eq!(fallback.editor_path, None);

        let settings = AppSettings {
            remote_open_behavior: RemoteOpenBehavior::CustomEditor,
            editor_path: Some(PathBuf::from("/Applications/Visual Studio Code.app")),
        };
        let json = serde_json::to_string(&settings).expect("序列化设置");
        let decoded: AppSettings = serde_json::from_str(&json).expect("读回设置");
        assert_eq!(decoded, settings);

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
    }

    /// 写-读-删一整圈。默认 `#[ignore]`：无人值守环境会卡在系统钥匙串授权弹窗上，
    /// 需要本机手动跑 `cargo test keychain_round_trip -- --ignored`。
    #[test]
    #[ignore = "会真的读写 macOS 钥匙串，需要人工授权"]
    fn keychain_round_trip() {
        let config = probe_config();
        forget_password(&config);

        store_password(&config, "s3cret-token").expect("写入钥匙串");
        assert_eq!(load_password(&config).as_deref(), Some("s3cret-token"));

        forget_password(&config);
        assert_eq!(load_password(&config), None, "删除后不应再读到");
    }
}
