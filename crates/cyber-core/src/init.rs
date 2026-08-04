use tracing::{debug, info, warn};

use crate::error::{CoreError, Result};
use crate::paths::Paths;

/// 默认配置文件内容（构建时嵌入）。
pub const DEFAULT_CONFIG_TOML: &str = include_str!("../assets/default_config.toml");
pub const DEFAULT_PROVIDERS_TOML: &str = include_str!("../assets/default_providers.toml");
pub const DEFAULT_MCP_SERVERS_TOML: &str = include_str!("../assets/default_mcp_servers.toml");

/// 若 `~/.cyber/config.toml` 不存在，执行首次初始化。
///
/// 返回 `true` 表示执行了初始化（首次启动），`false` 表示已存在。
pub fn ensure_global_init(paths: &Paths) -> Result<bool> {
    if paths.config_file.exists() {
        debug!(config_file = %paths.config_file.display(), "配置已存在，跳过首次初始化");
        return Ok(false);
    }
    info!(cyber_home = %paths.cyber_home.display(), "首次启动：初始化 ~/.cyber 目录结构与默认配置");
    create_global_layout(paths)?;
    write_default_files(paths)?;
    info!("~/.cyber 初始化完成");
    Ok(true)
}

fn create_global_layout(paths: &Paths) -> Result<()> {
    for dir in [
        paths.cyber_home.as_path(),
        paths.skills_dir.as_path(),
        paths.mcp_dir.as_path(),
        paths.workflows_dir.as_path(),
        paths.sessions_dir.as_path(),
        paths.logs_dir.as_path(),
        paths.reports_dir.as_path(),
        paths.reports_templates_dir.as_path(),
        paths.history_dir.as_path(),
    ] {
        debug!(dir = %dir.display(), "创建目录");
        std::fs::create_dir_all(dir).map_err(|e| {
            warn!(dir = %dir.display(), error = %e, "目录创建失败（可能权限不足或路径无效）");
            CoreError::Init {
                stage: "create layout",
                path: dir.display().to_string(),
                source: e,
            }
        })?;
    }
    Ok(())
}

fn write_default_files(paths: &Paths) -> Result<()> {
    let items: [(&std::path::Path, &str); 3] = [
        (paths.config_file.as_path(), DEFAULT_CONFIG_TOML),
        (paths.providers_file.as_path(), DEFAULT_PROVIDERS_TOML),
        (paths.mcp_servers_file.as_path(), DEFAULT_MCP_SERVERS_TOML),
    ];
    for (path, content) in items {
        debug!(path = %path.display(), "写入默认配置文件");
        std::fs::write(path, content).map_err(|e| {
            warn!(path = %path.display(), error = %e, "写入默认配置文件失败（可能权限不足或磁盘满）");
            CoreError::Init {
                stage: "write default config",
                path: path.display().to_string(),
                source: e,
            }
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;

    #[test]
    fn ensure_init_creates_layout_and_default_files() {
        let dir = std::env::temp_dir().join(format!(
            "cyber_init_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = Paths::at(dir.clone()).unwrap();

        let first = ensure_global_init(&paths).unwrap();
        assert!(first, "首次应执行初始化");
        assert!(paths.config_file.exists());
        assert!(paths.providers_file.exists());
        assert!(paths.mcp_servers_file.exists());
        assert!(paths.skills_dir.exists());
        assert!(paths.workflows_dir.exists());
        assert!(paths.logs_dir.exists());

        let second = ensure_global_init(&paths).unwrap();
        assert!(!second, "config.toml 已存在时不应重复初始化");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
