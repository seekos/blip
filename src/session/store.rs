//! 会话持久化：密码进 macOS Keychain，连接配置进 Application Support 下的 JSON。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::sftp::types::SessionConfig;

/// Keychain 里的 service 名，所有条目共享。
const KEYRING_SERVICE: &str = "com.macscp.credentials";
const PROFILES_FILE: &str = "profiles.json";

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
    Key { path: String },
}

fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("macscp")
}

fn profiles_path() -> PathBuf {
    config_dir().join(PROFILES_FILE)
}

pub fn load_profiles() -> Vec<SavedProfile> {
    let path = profiles_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    match serde_json::from_str(&raw) {
        Ok(profiles) => profiles,
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

/// 记录/更新一条连接配置（按 user@host:port 去重）。
pub fn upsert_profile(profiles: &mut Vec<SavedProfile>, profile: SavedProfile) {
    profiles.retain(|item| {
        !(item.host == profile.host
            && item.port == profile.port
            && item.username == profile.username)
    });
    profiles.insert(0, profile);
    profiles.truncate(20);
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
