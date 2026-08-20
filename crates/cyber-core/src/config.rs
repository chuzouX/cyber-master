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
    pub env: EnvConfig,
    pub memory: MemoryConfig,
}

/// 环境变量配置：存储用户自定义 env vars，供 shell/agent 子进程注入。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub rules: Vec<MemoryRule>,
}

impl Default for MemoryConfig {
    fn default() -> Self { Self { rules: vec![MemoryRule { enabled: true, scope: "both".into(), prompt: "只记录用户长期偏好、身份和项目约定。".into() }] } }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryRule {
    pub enabled: bool,
    pub scope: String,
    pub prompt: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EnvConfig {
    /// 环境变量列表（有序，保留用户输入顺序）。
    pub vars: Vec<EnvVar>,
}

/// 单条环境变量。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EnvVar {
    /// 变量名（如 `OPENAI_API_KEY`）。
    pub key: String,
    /// 变量值。
    pub value: String,
    /// 是否为敏感内容：true 时 UI 脱敏展示（如 `sk-****key`）。
    pub sensitive: bool,
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
    pub thinking_intensity: ThinkingIntensity,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            default_provider: "openai".into(),
            auto_tool_call: true,
            max_steps: 500,
            thinking_intensity: ThinkingIntensity::default(),
        }
    }
}

/// 思考强度档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingIntensity {
    /// 不输出思考过程，直接执行。
    Low,
    /// 3-5 行思考限制（默认）。
    #[default]
    Middle,
    /// 10-15 行思考，允许深入分析。
    High,
    /// 无限制，充分思考。
    Max,
    /// 自动：CTF 模式=High，否则=Middle。
    Auto,
}

impl ThinkingIntensity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Middle => "middle",
            Self::High => "high",
            Self::Max => "max",
            Self::Auto => "auto",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "不输出思考过程，直接执行",
            Self::Middle => "3-5 行思考限制（默认）",
            Self::High => "10-15 行思考，允许深入分析",
            Self::Max => "无限制，充分思考",
            Self::Auto => "自动（CTF=High，否则=Middle）",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "low" => Some(Self::Low),
            "middle" | "mid" => Some(Self::Middle),
            "high" => Some(Self::High),
            "max" => Some(Self::Max),
            "auto" => Some(Self::Auto),
            _ => None,
        }
    }

    /// Auto 模式根据 CTF 状态解析为实际档位。
    pub fn resolve(self, ctf_enabled: bool) -> Self {
        match self {
            Self::Auto if ctf_enabled => Self::High,
            Self::Auto => Self::Middle,
            other => other,
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
