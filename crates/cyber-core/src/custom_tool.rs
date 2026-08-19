//! 用户自定义工具配置与目录加载器。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::CoreError;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomToolParam {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomToolConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub parameters: Vec<CustomToolParam>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedCustomTool {
    pub path: PathBuf,
    pub config: CustomToolConfig,
}

/// 扫描目录顶层 TOML 文件。单文件失败不会阻断其余工具。
pub fn load_custom_tools(
    dir: &Path,
) -> (Vec<LoadedCustomTool>, Vec<(PathBuf, CoreError)>) {
    if !dir.exists() {
        return (Vec::new(), Vec::new());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) => {
            let error = CoreError::FileRead {
                path: dir.display().to_string(),
                source,
            };
            return (Vec::new(), vec![(dir.to_path_buf(), error)]);
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .is_some_and(|ext| {
                        ext.eq_ignore_ascii_case(['t', 'o', 'm', 'l'].iter().collect::<String>())
                    })
        })
        .collect();
    paths.sort();

    let mut tools = Vec::new();
    let mut errors = Vec::new();
    for path in paths {
        match load_one(&path) {
            Ok(config) => tools.push(LoadedCustomTool {
                path: path.clone(),
                config,
            }),
            Err(error) => {
                warn!(path = %path.display(), error = %error);
                errors.push((path, error));
            }
        }
    }
    debug!(count = tools.len(), errors = errors.len());
    (tools, errors)
}

fn load_one(path: &Path) -> Result<CustomToolConfig, CoreError> {
    let bytes = std::fs::read(path).map_err(|source| CoreError::FileRead {
        path: path.display().to_string(),
        source,
    })?;
    let raw = String::from_utf8(bytes).map_err(|source| CoreError::FileEncoding {
        path: path.display().to_string(),
        source,
    })?;
    let config: CustomToolConfig = toml::from_str(&raw)?;
    if config.name.trim().is_empty() {
        return Err(CoreError::Config("custom tool name must not be empty".into()));
    }
    if config.command.trim().is_empty() {
        return Err(CoreError::Config("custom tool command must not be empty".into()));
    }
    Ok(config)
}
