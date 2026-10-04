//! 版本更新检查与缓存机制。
//!
//! 1. `check_for_updates(force)`：检测 GitHub 最新 Release 或 main 分支版本；
//! 2. 缓存结果于 `~/.cyber/update_check.json`（默认 1 小时内不重复请求，避免限频与启动延迟）；
//! 3. 提供语义化版本比较 `is_newer(current, remote)`。

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::paths::Paths;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const GITHUB_REPO: &str = "chuzouX/cyber-master";
pub const GITHUB_API_URL: &str =
    "https://api.github.com/repos/chuzouX/cyber-master/releases/latest";
pub const GITHUB_RAW_CARGO: &str =
    "https://raw.githubusercontent.com/chuzouX/cyber-master/main/Cargo.toml";
pub const CNB_REPO: &str = "funxlink/cyber-master";
pub const CNB_RAW_CARGO: &str = "https://cnb.cool/funxlink/cyber-master/-/git/raw/main/Cargo.toml";
pub const CNB_RELEASES_URL: &str = "https://cnb.cool/funxlink/cyber-master/-/releases";

/// 缓存过期时间：1 小时。
const CACHE_TTL_SECS: u64 = 3600;

/// 网络请求超时时间：3 秒（不阻塞常规操作）。
const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReleaseInfo {
    pub version: String,
    pub html_url: String,
    pub release_notes: Option<String>,
    pub published_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdateCache {
    checked_at: u64,
    release: Option<ReleaseInfo>,
}

/// 解析语义化版本号：支持 `0.4.2`、`v0.4.2`、`V0.4.2` 等格式。
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let s = v.trim().trim_start_matches(['v', 'V']);
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() >= 3 {
        let major = parts[0].parse().ok()?;
        let minor = parts[1].parse().ok()?;
        let patch_part = parts[2].split(|c: char| !c.is_ascii_digit()).next()?;
        let patch = patch_part.parse().ok()?;
        Some((major, minor, patch))
    } else if parts.len() == 2 {
        let major = parts[0].parse().ok()?;
        let minor = parts[1].parse().ok()?;
        Some((major, minor, 0))
    } else {
        None
    }
}

/// 比较 `remote` 是否严格高于 `current`。
pub fn is_newer(current: &str, remote: &str) -> bool {
    match (parse_version(current), parse_version(remote)) {
        (Some(c), Some(r)) => r > c,
        _ => false,
    }
}

fn cache_path() -> Option<PathBuf> {
    Paths::detect()
        .ok()
        .map(|p| p.cyber_home.join("update_check.json"))
}

/// 读取本地有效缓存中的版本信息（若存在且未过期返回 Some(ReleaseInfo)）。
pub fn cached_latest_version() -> Option<ReleaseInfo> {
    let path = cache_path()?;
    if !path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&path).ok()?;
    let cache: UpdateCache = serde_json::from_str(&data).ok()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if now.saturating_sub(cache.checked_at) < CACHE_TTL_SECS {
        cache.release
    } else {
        None
    }
}

fn save_cache(release: Option<&ReleaseInfo>) {
    let Some(path) = cache_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let cache = UpdateCache {
        checked_at: now,
        release: release.cloned(),
    };
    if let Ok(json) = serde_json::to_string(&cache) {
        let _ = std::fs::write(path, json);
    }
}

/// 检查最新版本：
/// 1. `force = false` 时优先读取 1 小时内有效缓存；
/// 2. 否则通过 GitHub API / Raw Cargo.toml 查询，并写回缓存。
pub async fn check_for_updates(force: bool) -> Option<ReleaseInfo> {
    if !force {
        if let Some(cached) = cached_latest_version() {
            debug!(version = %cached.version, "使用本地更新检查缓存");
            return Some(cached);
        }
    }

    let client = reqwest::Client::builder()
        .timeout(NETWORK_TIMEOUT)
        .user_agent(format!("cyber-updater/{CURRENT_VERSION}"))
        .build()
        .ok()?;

    // 优先尝试 GitHub Releases API
    let release_result = client.get(GITHUB_API_URL).send().await;
    let release = match release_result {
        Ok(resp) if resp.status().is_success() => {
            #[derive(Deserialize)]
            struct GhRelease {
                tag_name: String,
                html_url: String,
                body: Option<String>,
                published_at: Option<String>,
            }
            if let Ok(gh) = resp.json::<GhRelease>().await {
                let ver = gh
                    .tag_name
                    .trim()
                    .trim_start_matches(['v', 'V'])
                    .to_string();
                Some(ReleaseInfo {
                    version: ver,
                    html_url: gh.html_url,
                    release_notes: gh.body,
                    published_at: gh.published_at,
                })
            } else {
                None
            }
        }
        _ => None,
    };

    // 备用：若 GitHub Release 为空或被限频/不可达，优先尝试 CNB 国内极速源 Raw Cargo.toml
    let release = match release {
        Some(rel) => Some(rel),
        None => {
            debug!("GitHub Releases 查询未成功，尝试从 CNB 国内源读取 Raw Cargo.toml");
            match client.get(CNB_RAW_CARGO).send().await {
                Ok(resp) if resp.status().is_success() => {
                    let text = resp.text().await.unwrap_or_default();
                    parse_cargo_toml_version(&text).map(|ver| ReleaseInfo {
                        version: ver,
                        html_url: CNB_RELEASES_URL.to_string(),
                        release_notes: None,
                        published_at: None,
                    })
                }
                _ => {
                    debug!("CNB 查询未成功，降级尝试 GitHub Raw Cargo.toml");
                    match client.get(GITHUB_RAW_CARGO).send().await {
                        Ok(resp) if resp.status().is_success() => {
                            let text = resp.text().await.unwrap_or_default();
                            let ver = parse_cargo_toml_version(&text)?;
                            Some(ReleaseInfo {
                                version: ver,
                                html_url: format!("https://github.com/{GITHUB_REPO}"),
                                release_notes: None,
                                published_at: None,
                            })
                        }
                        _ => None,
                    }
                }
            }
        }
    };

    save_cache(release.as_ref());
    release
}

fn parse_cargo_toml_version(content: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(content).ok()?;
    value
        .get("workspace")?
        .get("package")?
        .get("version")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_handles_standard_and_prefixed() {
        assert_eq!(parse_version("0.4.2"), Some((0, 4, 2)));
        assert_eq!(parse_version("v0.4.2"), Some((0, 4, 2)));
        assert_eq!(parse_version("V1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.5.0-alpha"), Some((0, 5, 0)));
        assert_eq!(parse_version("invalid"), None);
    }

    #[test]
    fn is_newer_correctly_compares() {
        assert!(is_newer("0.4.2", "0.4.3"));
        assert!(is_newer("0.4.2", "v0.4.3"));
        assert!(is_newer("0.4.2", "0.5.0"));
        assert!(is_newer("0.4.2", "1.0.0"));
        assert!(!is_newer("0.4.2", "0.4.2"));
        assert!(!is_newer("0.4.2", "v0.4.2"));
        assert!(!is_newer("0.4.2", "0.4.1"));
        assert!(!is_newer("0.4.2", "0.3.9"));
    }

    #[test]
    fn parse_cargo_toml_version_extracts_workspace_version() {
        let toml_str = r#"
[workspace.package]
version = "0.4.9"
edition = "2021"
"#;
        assert_eq!(
            parse_cargo_toml_version(toml_str),
            Some("0.4.9".to_string())
        );
    }

    #[test]
    fn cnb_constants_and_cargo_toml_parsing() {
        assert_eq!(CNB_REPO, "funxlink/cyber-master");
        assert!(CNB_RAW_CARGO.starts_with("https://cnb.cool/"));
        assert!(CNB_RELEASES_URL.starts_with("https://cnb.cool/"));
        let toml_sample = r#"
[workspace.package]
version = "0.5.0"
"#;
        assert_eq!(
            parse_cargo_toml_version(toml_sample),
            Some("0.5.0".to_string())
        );
    }
}
