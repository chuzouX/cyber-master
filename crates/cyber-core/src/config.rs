use serde::{Deserialize, Serialize};

/// 顶层配置，对应 `~/.cyber/config.toml`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub ui: UiConfig,
    pub agent: AgentConfig,
    pub workflow: WorkflowConfig,
    pub tools: ToolsConfig,
    pub storage: StorageConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub theme: String,
    pub default_mode: String,
    pub animations: bool,
    pub mouse: bool,
    pub frame_rate: u32,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "cyberpunk".into(),
            default_mode: "chat".into(),
            animations: true,
            mouse: true,
            frame_rate: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub default_provider: String,
    pub auto_tool_call: bool,
    pub max_steps: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".into(),
            auto_tool_call: true,
            max_steps: 25,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowConfig {
    pub max_parallel_nodes: u32,
    pub default_timeout_secs: u64,
    pub checkpoint: bool,
}

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            max_parallel_nodes: 8,
            default_timeout_secs: 1800,
            checkpoint: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub prefer_docker: bool,
    pub extra_path: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    pub history_retention_days: u32,
    pub log_level: String,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            history_retention_days: 90,
            log_level: "info".into(),
        }
    }
}
