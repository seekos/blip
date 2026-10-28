use anyhow::{Context as _, Result};
use futures::AsyncReadExt as _;
use gpui_kit::http_client::{AsyncBody, HttpClient, HttpRequestExt as _, RedirectPolicy, Request};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

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

pub async fn check_latest_release(
    client: Arc<dyn HttpClient>,
    current_version: &str,
) -> Result<Option<AvailableUpdate>> {
    let request = Request::builder()
        .uri(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", concat!("Blip/", env!("CARGO_PKG_VERSION")))
        .follow_redirects(RedirectPolicy::FollowLimit(5))
        .timeout(Duration::from_secs(8))
        .body(AsyncBody::empty())?;
    let mut response = client.send(request).await.context("无法读取最新版本")?;
    if !response.status().is_success() {
        anyhow::bail!("版本检查返回 {}", response.status());
    }
    let mut body = Vec::new();
    response
        .body_mut()
        .read_to_end(&mut body)
        .await
        .context("无法读取版本响应")?;

    available_update_from_json(&body, current_version)
}

fn available_update_from_json(
    body: &[u8],
    current_version: &str,
) -> Result<Option<AvailableUpdate>> {
    let release: GithubRelease = serde_json::from_slice(body).context("无法解析最新版本信息")?;
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
    use super::{available_update_from_json, is_newer_version, short_version_from_plist};

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

    #[test]
    fn parses_github_release_response() {
        let update = available_update_from_json(
            br#"{"tag_name":"v1.2.0","html_url":"https://example.com/release"}"#,
            "1.1.0",
        )
        .expect("解析版本")
        .expect("应有更新");

        assert_eq!(update.version, "1.2.0");
        assert_eq!(update.current_version, "1.1.0");
        assert_eq!(update.release_url, "https://example.com/release");
    }
}
