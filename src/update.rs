use anyhow::{Context as _, Result};
use serde::Deserialize;
use std::process::Command;

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/seekos/macscp/releases/latest";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailableUpdate {
    pub current_version: String,
    pub version: String,
    pub release_url: String,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
}

pub fn check_latest_release(current_version: &str) -> Result<Option<AvailableUpdate>> {
    let output = Command::new("/usr/bin/curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--max-time",
            "8",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            "--user-agent",
            concat!("Blip/", env!("CARGO_PKG_VERSION")),
            LATEST_RELEASE_URL,
        ])
        .output()
        .context("无法启动版本检查")?;
    if !output.status.success() {
        anyhow::bail!("无法读取最新版本");
    }

    let release: GithubRelease =
        serde_json::from_slice(&output.stdout).context("无法解析最新版本信息")?;
    let version = release.tag_name.trim_start_matches(['v', 'V']).to_string();
    if !is_newer_version(&version, current_version) {
        return Ok(None);
    }

    Ok(Some(AvailableUpdate {
        current_version: current_version.to_string(),
        version,
        release_url: release.html_url,
    }))
}

pub fn open_release_page(url: &str) -> Result<()> {
    let status = Command::new("/usr/bin/open")
        .arg(url)
        .status()
        .context("无法打开更新页面")?;
    if !status.success() {
        anyhow::bail!("系统无法打开更新页面");
    }
    Ok(())
}

pub fn application_version() -> String {
    let bundled_version = std::env::current_exe()
        .ok()
        .and_then(|executable| executable.parent().map(std::path::Path::to_path_buf))
        .filter(|directory| directory.file_name().and_then(|name| name.to_str()) == Some("MacOS"))
        .and_then(|macos_directory| macos_directory.parent().map(std::path::Path::to_path_buf))
        .filter(|directory| {
            directory.file_name().and_then(|name| name.to_str()) == Some("Contents")
        })
        .map(|contents_directory| contents_directory.join("Info.plist"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|plist| short_version_from_plist(&plist));

    bundled_version.unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

fn short_version_from_plist(plist: &str) -> Option<String> {
    let (_, remainder) = plist.split_once("<key>CFBundleShortVersionString</key>")?;
    let (_, remainder) = remainder.split_once("<string>")?;
    let (version, _) = remainder.split_once("</string>")?;
    let version = version.trim();
    (!version.is_empty()).then(|| version.to_string())
}

fn is_newer_version(candidate: &str, current: &str) -> bool {
    let parse = |version: &str| {
        let version = version.trim_start_matches(['v', 'V']);
        let (core, is_prerelease) = version
            .split_once('-')
            .map(|(core, _)| (core, true))
            .unwrap_or((version, false));
        let components = core
            .split('.')
            .map(|component| component.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()?;
        Some((components, is_prerelease))
    };
    let (candidate_core, candidate_prerelease) = match parse(candidate) {
        Some(version) => version,
        None => return false,
    };
    let (current_core, current_prerelease) = match parse(current) {
        Some(version) => version,
        None => return false,
    };

    let component_count = candidate_core.len().max(current_core.len());
    for index in 0..component_count {
        let candidate_component = candidate_core.get(index).copied().unwrap_or(0);
        let current_component = current_core.get(index).copied().unwrap_or(0);
        if candidate_component != current_component {
            return candidate_component > current_component;
        }
    }
    !candidate_prerelease && current_prerelease
}

#[cfg(test)]
mod tests {
    use super::{is_newer_version, short_version_from_plist};

    #[test]
    fn compares_release_versions() {
        assert!(is_newer_version("0.2.0", "0.1.9"));
        assert!(is_newer_version("v1.0.0", "1.0.0-beta.1"));
        assert!(!is_newer_version("0.1.0", "0.1.0"));
        assert!(!is_newer_version("0.1.0-beta.1", "0.1.0"));
        assert!(!is_newer_version("not-a-version", "0.1.0"));
    }

    #[test]
    fn reads_bundle_short_version() {
        let plist = r#"<key>CFBundleShortVersionString</key><string>1.2.3</string>"#;
        assert_eq!(short_version_from_plist(plist).as_deref(), Some("1.2.3"));
    }
}
