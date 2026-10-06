//! 版本更新检查与缓存机制。
//!
//! 1. `check_for_updates(force)`：检测 GitHub 最新 Release 或 main 分支版本；
//! 2. 缓存结果于 `~/.cyber/update_check.json`（默认 1 小时内不重复请求，避免限频与启动延迟）；
//! 3. 提供语义化版本比较 `is_newer(current, remote)`。

use std::path::{Path, PathBuf};
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

/// install.ps1 的调用片段（`CYBER_FORCE=1` 让脚本跳过自检与确认，直接覆盖安装；
/// `version = None` 时由脚本自行解析最新版本）。
#[cfg(windows)]
fn powershell_invocation(version: Option<&str>) -> String {
    let version_env = version
        .map(|v| format!("$env:CYBER_VERSION='v{v}'; "))
        .unwrap_or_default();
    format!(
        "{version_env}$env:CYBER_USE_CNB='1'; $env:CYBER_FORCE='1'; irm https://cnb.cool/{CNB_REPO}/-/git/raw/main/install.ps1 | iex"
    )
}

/// install.sh 的调用片段（`--force` 让脚本跳过自检与确认，直接覆盖安装；
/// `version = None` 时由脚本自行解析最新版本）。
#[cfg(not(windows))]
fn shell_invocation(version: Option<&str>) -> String {
    let version_arg = version
        .map(|v| format!(" --version v{v}"))
        .unwrap_or_default();
    format!("curl -fsSL https://cnb.cool/{CNB_REPO}/-/git/raw/main/install.sh | sh -s -- --cnb --force{version_arg}")
}

/// 前台执行的安装脚本命令 `(program, args)`：`cyber update`（阻塞、继承 stdio）用。
/// `version = None`（即 `cyber update --force`）交由安装脚本自行解析最新版本并覆盖安装。
pub fn install_script_command(version: Option<&str>) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        (
            "powershell".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-ExecutionPolicy".to_string(),
                "Bypass".to_string(),
                "-Command".to_string(),
                powershell_invocation(version),
            ],
        )
    }
    #[cfg(not(windows))]
    {
        (
            "sh".to_string(),
            vec!["-c".to_string(), shell_invocation(version)],
        )
    }
}

/// 面向用户的手动升级命令（单行，与 install 脚本 README 用法一致）；`version = None` 装最新。
pub fn install_script_hint(version: Option<&str>) -> String {
    #[cfg(windows)]
    {
        // install.ps1 无版本参数形式，版本经 CYBER_VERSION 传入。
        let _ = version;
        format!("irm https://cnb.cool/{CNB_REPO}/-/git/raw/main/install.ps1 | iex")
    }
    #[cfg(not(windows))]
    {
        match version {
            Some(v) => format!(
                "curl -fsSL https://cnb.cool/{CNB_REPO}/-/git/raw/main/install.sh | sh -s -- --cnb --version v{v}"
            ),
            None => format!(
                "curl -fsSL https://cnb.cool/{CNB_REPO}/-/git/raw/main/install.sh | sh"
            ),
        }
    }
}

/// 安装目标路径的纯计算部分（`CYBER_INSTALL_DIR` 优先，否则 `<home>/.local/bin`；
/// 目录为空白串视为未设置）。与 install.ps1 / install.sh 的默认目录逐字一致。
fn install_target_path(install_dir: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
    let file = if cfg!(windows) { "cyber.exe" } else { "cyber" };
    if let Some(dir) = install_dir.map(str::trim).filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join(file));
    }
    let home = home.map(str::trim).filter(|h| !h.is_empty())?;
    Some(PathBuf::from(home).join(".local").join("bin").join(file))
}

/// 安装脚本的目标二进制路径（读环境变量）。
pub fn installed_binary_path() -> Option<PathBuf> {
    let install_dir = std::env::var("CYBER_INSTALL_DIR").ok();
    let home = if cfg!(windows) {
        std::env::var("USERPROFILE").ok()
    } else {
        std::env::var("HOME").ok()
    };
    install_target_path(install_dir.as_deref(), home.as_deref())
}

/// 两个路径是否指向同一文件：先规范化（失败则原样比较），Windows 不区分大小写。
fn same_binary(a: &Path, b: &Path) -> bool {
    let a = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let b = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    if cfg!(windows) {
        a.to_string_lossy()
            .eq_ignore_ascii_case(&b.to_string_lossy())
    } else {
        a == b
    }
}

/// 当前进程是否就是安装脚本的目标二进制。
pub fn is_self_install_target() -> bool {
    let (Some(target), Ok(current)) = (installed_binary_path(), std::env::current_exe()) else {
        return false;
    };
    same_binary(&current, &target)
}

/// 是否需要「先退出再安装」：Windows 无法覆盖运行中的 exe（install.ps1 的 `Copy-Item -Force`
/// 会以「请关闭正在运行的 cyber」失败），Unix 的 `mv -f` 可原地替换运行中的文件。
pub fn needs_exit_before_install() -> bool {
    cfg!(windows) && is_self_install_target()
}

/// 分离执行的安装脚本内容（Windows PowerShell / 其它 sh）。
pub fn detached_script(version: &str, wait_for_exit: bool) -> String {
    let pid = std::process::id();
    #[cfg(windows)]
    {
        let wait = if wait_for_exit {
            format!("while (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ Start-Sleep -Milliseconds 400 }}\n")
        } else {
            String::new()
        };
        format!(
            "$ErrorActionPreference = 'Stop'\n{wait}{}\n",
            powershell_invocation(Some(version))
        )
    }
    #[cfg(not(windows))]
    {
        let wait = if wait_for_exit {
            format!("while kill -0 {pid} 2>/dev/null; do sleep 0.4; done\n")
        } else {
            String::new()
        };
        format!(
            "#!/bin/sh\nset -u\n{wait}{}\n",
            shell_invocation(Some(version))
        )
    }
}

/// 写脚本到 `~/.cyber/logs/update-v<version>.ps1|sh` 并分离启动，返回脚本路径。
///
/// Windows 走 `cmd /c start "" powershell … -File <脚本>`：新控制台窗口，安装进度可见，
/// 且不阻塞本进程；Unix 直接 `sh <脚本>`，stdout/stderr 追加到同目录 `update.log`。
pub fn launch_detached_install(version: &str, wait_for_exit: bool) -> std::io::Result<PathBuf> {
    use std::process::Stdio;
    let paths = Paths::detect().map_err(|e| std::io::Error::other(e.to_string()))?;
    std::fs::create_dir_all(&paths.logs_dir)?;
    #[cfg(windows)]
    let script = paths.logs_dir.join(format!("update-v{version}.ps1"));
    #[cfg(not(windows))]
    let script = paths.logs_dir.join(format!("update-v{version}.sh"));
    std::fs::write(&script, detached_script(version, wait_for_exit))?;
    #[cfg(windows)]
    {
        std::process::Command::new("cmd")
            .args([
                "/c",
                "start",
                "",
                "powershell",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(&script)
            .stdin(Stdio::null())
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.logs_dir.join("update.log"))?;
        let err = out.try_clone()?;
        std::process::Command::new("sh")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .spawn()?;
    }
    Ok(script)
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

    /// `install.ps1` / `install.sh` 会被 `irm ... | iex`（Windows）与 `curl ... | sh` 直接管道执行，
    /// 二者都不容忍文件开头的 UTF-8 BOM：PowerShell 5.1 的 `iex` 会把 BOM 并进首个标记（实测报
    /// `CommandNotFoundException: 无法将“#”项识别为 cmdlet…`），`sh` 也会把它当未知命令。
    /// 这里锁定「无 BOM + 合法 UTF-8 + 首行是注释」，防止编辑器保存时又把 BOM 加回来。
    #[test]
    fn install_scripts_are_bom_free_utf8() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("cyber-core 位于 <repo>/crates/cyber-core");
        for name in ["install.ps1", "install.sh"] {
            let path = root.join(name);
            let bytes =
                std::fs::read(&path).unwrap_or_else(|e| panic!("无法读取 {}: {e}", path.display()));
            assert!(
                !bytes.starts_with(&[0xEF, 0xBB, 0xBF]),
                "{name} 不得带 UTF-8 BOM：`irm ... | iex` / `curl ... | sh` 会把它当命令执行，\
                 管道安装与 `cyber update` 全部失败"
            );
            let text = std::str::from_utf8(&bytes)
                .unwrap_or_else(|e| panic!("{name} 必须是合法 UTF-8: {e}"));
            assert!(text.starts_with('#'), "{name} 首行必须是注释（# 开头）");
        }
    }

    /// `install.ps1` 会在用户**自己的** PowerShell 会话里执行（`irm ... | iex`），此时 `exit`
    /// 直接结束宿主进程 —— 现象是「脚本跑完终端窗口自己关了」（命中「已是最新版本」或
    /// 「已取消更新」分支时尤其明显）。正常/取消退出用 `return`（退出码 0），致命错误用
    /// `throw`（退出码 1，与旧的 `exit 1` 一致）。注释里提到 `exit` 不算违规。
    #[test]
    fn install_ps1_never_exits_the_host_session() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("cyber-core 位于 <repo>/crates/cyber-core")
            .join("install.ps1");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("无法读取 {}: {e}", path.display()));
        for (idx, line) in text.lines().enumerate() {
            let code = line.split('#').next().unwrap_or("");
            for token in
                code.split(|c: char| c.is_whitespace() || matches!(c, ';' | '(' | ')' | '{' | '}'))
            {
                assert_ne!(
                    token,
                    "exit",
                    "install.ps1:{} 出现 exit：脚本经 `irm ... | iex` 在用户会话内执行，\
                     exit 会直接关掉宿主终端；正常退出请用 return，致命错误请用 throw",
                    idx + 1
                );
            }
        }
    }

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
    fn install_script_command_forces_and_targets_requested_version() {
        let (program, args) = install_script_command(Some("0.9.9"));
        let command = args.last().unwrap();
        #[cfg(windows)]
        {
            assert_eq!(program, "powershell");
            assert!(command.contains("$env:CYBER_FORCE='1'"), "{command}");
            assert!(command.contains("$env:CYBER_VERSION='v0.9.9'"), "{command}");
            assert!(command.contains("install.ps1 | iex"), "{command}");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(program, "sh");
            assert!(command.contains("--force"), "{command}");
            assert!(command.contains("--version v0.9.9"), "{command}");
            assert!(command.contains("install.sh | sh -s"), "{command}");
        }
    }

    #[test]
    fn install_script_command_without_version_lets_installer_resolve_latest() {
        let (program, args) = install_script_command(None);
        let command = args.last().unwrap();
        #[cfg(windows)]
        {
            assert_eq!(program, "powershell");
            assert!(command.contains("$env:CYBER_FORCE='1'"), "{command}");
            assert!(!command.contains("CYBER_VERSION"), "{command}");
        }
        #[cfg(not(windows))]
        {
            assert_eq!(program, "sh");
            assert!(command.contains("--force"), "{command}");
            assert!(!command.contains("--version"), "{command}");
        }
    }

    #[test]
    fn install_target_path_prefers_env_dir_then_home_default() {
        let file = if cfg!(windows) { "cyber.exe" } else { "cyber" };
        assert_eq!(
            install_target_path(Some(" D:/b "), None),
            Some(PathBuf::from("D:/b").join(file))
        );
        assert_eq!(
            install_target_path(None, Some("/home/x")),
            Some(
                PathBuf::from("/home/x")
                    .join(".local")
                    .join("bin")
                    .join(file)
            )
        );
        assert_eq!(install_target_path(Some("   "), None), None);
        assert_eq!(install_target_path(None, Some(" ")), None);
        assert_eq!(install_target_path(None, None), None);
    }

    #[test]
    fn same_binary_compares_canonical_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a");
        std::fs::write(&file, b"x").unwrap();
        assert!(same_binary(&file, &dir.path().join(".").join("a")));
        let other = dir.path().join("b");
        std::fs::write(&other, b"y").unwrap();
        assert!(!same_binary(&file, &other));
        #[cfg(windows)]
        assert!(same_binary(
            Path::new("C:/X/Cyber.exe"),
            Path::new("c:/x/cyber.exe")
        ));
    }

    #[test]
    fn detached_script_includes_wait_when_requested() {
        let waiting = detached_script("0.9.9", true);
        assert!(waiting.contains("0.9.9"), "{waiting}");
        let pid = std::process::id().to_string();
        #[cfg(windows)]
        {
            assert!(
                waiting.contains(&format!("Get-Process -Id {pid}")),
                "{waiting}"
            );
            assert!(
                waiting.starts_with("$ErrorActionPreference = 'Stop'"),
                "{waiting}"
            );
            assert!(waiting.contains("CYBER_FORCE='1'"), "{waiting}");
        }
        #[cfg(not(windows))]
        {
            assert!(waiting.contains(&format!("kill -0 {pid}")), "{waiting}");
            assert!(waiting.starts_with("#!/bin/sh"), "{waiting}");
            assert!(waiting.contains("--force"), "{waiting}");
        }

        let immediate = detached_script("0.9.9", false);
        assert!(immediate.contains("0.9.9"), "{immediate}");
        #[cfg(windows)]
        assert!(!immediate.contains("Get-Process"), "{immediate}");
        #[cfg(not(windows))]
        assert!(!immediate.contains("kill -0"), "{immediate}");
    }

    #[test]
    fn install_script_hint_matches_installer_docs() {
        #[cfg(windows)]
        {
            assert!(install_script_hint(None).contains("install.ps1 | iex"));
            assert!(install_script_hint(Some("0.9.9")).contains("install.ps1 | iex"));
        }
        #[cfg(not(windows))]
        {
            assert!(install_script_hint(None).contains("install.sh | sh"));
            assert!(!install_script_hint(None).contains("--version"));
            assert!(install_script_hint(Some("0.9.9")).contains("--version v0.9.9"));
        }
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
