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
pub fn load_custom_tools(dir: &Path) -> (Vec<LoadedCustomTool>, Vec<(PathBuf, CoreError)>) {
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
                && path.extension().is_some_and(|ext| {
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
        return Err(CoreError::Config(
            "custom tool name must not be empty".into(),
        ));
    }
    if config.command.trim().is_empty() {
        return Err(CoreError::Config(
            "custom tool command must not be empty".into(),
        ));
    }
    Ok(config)
}

/// 将自定义工具配置保存到指定目录下的 `<name>.toml`。
pub fn save_custom_tool(dir: &Path, tool: &CustomToolConfig) -> Result<PathBuf, CoreError> {
    let name = tool.name.trim();
    if name.is_empty() {
        return Err(CoreError::Config(
            "custom tool name must not be empty".into(),
        ));
    }
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err(CoreError::Config(format!(
            "invalid custom tool name: '{name}'"
        )));
    }
    if tool.command.trim().is_empty() {
        return Err(CoreError::Config(
            "custom tool command must not be empty".into(),
        ));
    }
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
    }
    let file_path = dir.join(format!("{name}.toml"));
    let toml_str = toml::to_string_pretty(tool)
        .map_err(|e| CoreError::Config(format!("failed to serialize custom tool: {e}")))?;
    crate::atomic_write(&file_path, toml_str.as_bytes())?;
    Ok(file_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir_unique(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_tool_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[test]
    fn test_save_and_load_custom_tool() {
        let dir = temp_dir_unique("save_load");
        let tool = CustomToolConfig {
            name: "sqlmap".into(),
            description: "sqlmap injection tool".into(),
            command: "python sqlmap.py -u {url}".into(),
            tags: vec!["sqli".into()],
            parameters: vec![CustomToolParam {
                name: "url".into(),
                description: "target url".into(),
                required: true,
                default: None,
            }],
        };
        let saved_path = save_custom_tool(&dir, &tool).unwrap();
        assert!(saved_path.exists());
        assert_eq!(saved_path.file_name().unwrap(), "sqlmap.toml");

        let (loaded, errors) = load_custom_tools(&dir);
        assert!(errors.is_empty());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].config.name, "sqlmap");
        assert_eq!(loaded[0].config.parameters.len(), 1);
        assert_eq!(loaded[0].config.parameters[0].name, "url");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_save_custom_tool_validation() {
        let dir = temp_dir_unique("val");
        let mut invalid_tool = CustomToolConfig::default();
        assert!(save_custom_tool(&dir, &invalid_tool).is_err());

        invalid_tool.name = "../evil".into();
        invalid_tool.command = "whoami".into();
        assert!(save_custom_tool(&dir, &invalid_tool).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
