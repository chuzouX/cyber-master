use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 支持的 provider kind（format）。TUI 表单的 kind 字段在此循环；
/// `provider_factory` 接受前四种（openai/anthropic/ollama/responses），
/// `openai-compatible` 复用 openai 的 SSE 解析路径。
pub const PROVIDER_KINDS: &[&str] = &[
    "openai",
    "anthropic",
    "ollama",
    "openai-compatible",
    "responses",
];

/// 上下文窗口（context_length）常用预设：(显示标签, 数值字符串)。
/// 空字符串表示「未设置」，对应 `context_length = None`。
pub const CONTEXT_LENGTH_PRESETS: &[(&str, &str)] = &[
    ("默认(留空)", ""),
    ("128K", "131072"),
    ("256K", "262144"),
    ("512K", "524288"),
    ("1M", "1048576"),
];

/// 新建 / 缺省 provider 的默认最大输出 token 数。
pub const DEFAULT_MAX_TOKENS: u32 = 384_000;

/// 模型未声明 `context_length` 时的输出上限兜底：避免把超大 `max_tokens` 发给上限未知的端点。
pub const DEFAULT_OUTPUT_TOKEN_CAP: u32 = 128_000;

/// 对应 `~/.cyber/providers.toml`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub default_provider: String,
    pub providers: HashMap<String, ProviderConfig>,
}

/// Provider 级思考（思维链）配置。
///
/// 两个字段互相独立：`type` 控制是否开启 think，`effort` 控制思考强度（OpenAI 风格
/// `reasoning_effort`）。都为 `None` 时**不下发任何思考参数**（与旧行为完全一致）。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ThinkingConfig {
    /// `thinking.type`：`"enabled"` 或 `"disabled"`。`None` = 不下发 `thinking` 参数。
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    /// 思考强度：`"low"` / `"medium"` / `"high"`。`None` = 不下发 `reasoning_effort`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// openai | anthropic | ollama | openai-compatible
    pub kind: String,
    pub base_url: String,
    /// 可为空（ollama），或 `${ENV_VAR}` 引用环境变量。
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    /// 每百万 token 价格（美元），用于 TUI 显示成本。可选，缺省时不显示成本。
    pub price: Option<PriceConfig>,
    /// 每个 model 的专属配置（覆盖 provider 级默认值）。key = model id。
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
    /// 自定义流式对话端点（高级选项）。为空时使用默认 `{base_url}/chat/completions`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_endpoint: Option<String>,
    /// 自定义模型列表端点（高级选项）。为空时使用默认逻辑（`{base_url}/models`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_endpoint: Option<String>,
    /// Provider 级思考配置（高级选项）。`None` = 不下发任何思考参数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
}

/// 单个 model 的专属配置（覆盖 provider 级默认值）。
///
/// 存于 `ProviderConfig::models` map，key 为 model id。所有字段可选：
/// 缺省时回退到 provider 级的 `max_tokens` / `temperature` / `price`。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelConfig {
    /// 显示别名（空则用 model id）。/model 面板和 chat 标题显示用，不影响 API 调用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// 上下文长度（token 数，如 128000）。空则未知。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_length: Option<u32>,
    /// 最大输出 token 数（覆盖 provider.max_tokens）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// 采样温度（覆盖 provider.temperature）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// 价格配置（覆盖 provider.price）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<PriceConfig>,
    /// 备注（自由文本）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// 视觉/识图多模态能力。Some(true) = 支持, Some(false) = 不支持, None = 未知/未探测。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    /// 思考/推理能力。Some(true)=支持, Some(false)=不支持, None=未知/未探测。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
}

/// token 单价配置（每百万 token）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PriceConfig {
    /// 每百万输入 token（缓存未命中）价格。
    pub input_per_m: Option<f64>,
    /// 每百万输出 token 价格。
    pub output_per_m: Option<f64>,
    /// 每百万输入 token（缓存命中）价格。缺省时回退到 input_per_m。
    pub cache_hit_per_m: Option<f64>,
    /// 价格货币："usd"（美元）或 "cny"（人民币）。缺省 "usd"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: "openai".into(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: 0.7,
            price: None,
            models: HashMap::new(),
            chat_endpoint: None,
            models_endpoint: None,
            thinking: None,
        }
    }
}

impl ProvidersConfig {
    /// 三家并存默认模板（OpenAI / Anthropic / Ollama）。
    pub fn default_template() -> Self {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".into(),
            ProviderConfig {
                kind: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                api_key: "${OPENAI_API_KEY}".into(),
                model: "gpt-4o".into(),
                ..Default::default()
            },
        );
        providers.insert(
            "anthropic".into(),
            ProviderConfig {
                kind: "anthropic".into(),
                base_url: "https://api.anthropic.com".into(),
                api_key: "${ANTHROPIC_API_KEY}".into(),
                model: "claude-sonnet-4-5".into(),
                ..Default::default()
            },
        );
        providers.insert(
            "ollama".into(),
            ProviderConfig {
                kind: "ollama".into(),
                base_url: "http://localhost:11434".into(),
                api_key: String::new(),
                model: "qwen2.5:32b".into(),
                ..Default::default()
            },
        );
        Self {
            default_provider: "openai".into(),
            providers,
        }
    }

    /// 排序后的 provider 名列表（供 TUI 渲染与 cursor 索引复用，保证顺序稳定）。
    pub fn sorted_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.providers.keys().cloned().collect();
        names.sort();
        names
    }

    /// 新增或覆盖（按 name 作 key）。同时规范化 `ProviderConfig`（去 base_url 尾斜杠）。
    pub fn upsert(&mut self, name: &str, mut cfg: ProviderConfig) {
        cfg.normalize();
        self.providers.insert(name.to_string(), cfg);
    }

    /// 按 name 删除，返回被删的旧配置（不存在则 None）。
    pub fn remove(&mut self, name: &str) -> Option<ProviderConfig> {
        self.providers.remove(name)
    }
}

impl ProviderConfig {
    /// 解析自身 `api_key`：`${ENV_VAR}` 引用展开为环境变量值，明文原样返回。
    pub fn resolved_api_key(&self) -> String {
        resolve_api_key(&self.api_key)
    }

    /// 就地规范化：trim `base_url` 并去尾部 `/`（参考 wepclaude `normalizeBaseUrl`）。
    pub fn normalize(&mut self) {
        self.base_url = self.base_url.trim().trim_end_matches('/').to_string();
        self.api_key = self.api_key.trim().to_string();
        self.model = self.model.trim().to_string();
        if let Some(t) = self.thinking.as_mut() {
            t.r#type = t
                .r#type
                .as_deref()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty());
            t.effort = t
                .effort
                .as_deref()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty());
            if t.r#type.is_none() && t.effort.is_none() {
                self.thinking = None;
            }
        }
    }

    /// 当前 model 的专属配置（若存在）。
    pub fn current_model_config(&self) -> Option<&ModelConfig> {
        self.models.get(&self.model)
    }

    /// 当前 model 的有效 max_tokens：per-model 优先，回退到 provider 级；
    /// 再按模型声明的 `context_length` 钳制（未声明时用 `DEFAULT_OUTPUT_TOKEN_CAP` 兜底）。
    pub fn effective_max_tokens(&self) -> u32 {
        let configured = self
            .current_model_config()
            .and_then(|m| m.max_tokens)
            .unwrap_or(self.max_tokens);
        configured.min(
            self.effective_context_length()
                .unwrap_or(DEFAULT_OUTPUT_TOKEN_CAP),
        )
    }

    /// 当前 model 的有效 temperature：per-model 优先，回退到 provider 级。
    pub fn effective_temperature(&self) -> f32 {
        self.current_model_config()
            .and_then(|m| m.temperature)
            .unwrap_or(self.temperature)
    }

    /// 当前 model 的有效价格：per-model 优先，回退到 provider 级。
    pub fn effective_price(&self) -> Option<&PriceConfig> {
        self.current_model_config()
            .and_then(|m| m.price.as_ref())
            .or(self.price.as_ref())
    }

    /// 当前 model 的有效货币："usd" 或 "cny"。缺省 "usd"。
    pub fn effective_currency(&self) -> &str {
        self.effective_price()
            .and_then(|p| p.currency.as_deref())
            .filter(|s| !s.is_empty())
            .unwrap_or("usd")
    }

    /// 当前 model 的显示名：per-model alias 非空则用 alias，否则用 model id。
    /// 用于 /model 面板、chat 标题等 UI 展示；API 调用始终用 `self.model`。
    pub fn model_display_name(&self) -> &str {
        self.current_model_config()
            .and_then(|m| m.alias.as_deref())
            .filter(|s| !s.is_empty())
            .unwrap_or(&self.model)
    }

    /// 当前 model 的有效上下文长度（token 数）。仅 per-model 配置生效；
    /// 未配置时返回 None（调用方应回退到默认值，如 128_000）。
    ///
    /// 用于自动上下文压缩阈值计算与 TUI 状态栏剩余百分比显示。
    pub fn effective_context_length(&self) -> Option<u32> {
        self.current_model_config()
            .and_then(|m| m.context_length)
            .filter(|&n| n > 0)
    }

    /// 有效的流式对话端点。优先使用 `chat_endpoint`，为空则回退到默认（ollama 为 `{base_url}/api/chat`，其余为 `{base_url}/chat/completions`）。
    pub fn chat_endpoint(&self) -> String {
        self.chat_endpoint
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|| {
                let base = self.base_url.trim().trim_end_matches('/');
                if self.kind == "ollama" {
                    format!("{base}/api/chat")
                } else {
                    format!("{base}/chat/completions")
                }
            })
    }

    /// 有效的模型列表端点。优先使用 `models_endpoint`，为空则回退到默认逻辑（由 models.rs 处理）。
    pub fn models_endpoint(&self) -> Option<String> {
        self.models_endpoint
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map(|s| s.trim().to_string())
    }
}

/// 判定 base_url 的 path 中是否已含 API 版本段（`/v1`、`/v1beta`、`/v2`、`/api-v1` …）。
///
/// 只检查 path，忽略 `scheme://host`，避免把 `v1.example.com` 这类主机名误判为版本段。
fn has_api_version_path(base_url: &str) -> bool {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let path = after_scheme
        .split_once('/')
        .map(|(_, p)| p)
        .unwrap_or_default();
    let is_version_segment = |s: &str| {
        let b = s.as_bytes();
        b.len() >= 2 && b[0] == b'v' && b[1].is_ascii_digit()
    };
    path.split('/').any(|seg| {
        let seg = seg.trim().to_ascii_lowercase();
        // `v1` / `v1beta` / `v2…`，或 `api-v1` 这类以 `-v<数字>` 结尾的版本段。
        is_version_segment(&seg)
            || seg
                .rsplit_once('-')
                .is_some_and(|(_, tail)| is_version_segment(tail))
    })
}

/// 把 API 版本段拼进 base_url：已含版本段（`/v1`、`/v1beta`、`/v2`…）则原样返回（仅去尾 `/`），
/// 否则追加 `/v1`。
///
/// 用于 anthropic `/v1/messages`、`/v1/models` 等固定带版本段的路径拼接：
/// 用户把 base_url 配成 `https://api.anthropic.com/v1` 时不得再拼成 `/v1/v1/messages`。
pub fn with_api_version(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if has_api_version_path(base) {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

/// 展开 `${ENV_VAR}` 引用；无 `${}` 包裹的明文原样返回。
///
/// - `${OPENAI_API_KEY}` → `std::env::var("OPENAI_API_KEY")`，未设置则返回空串
///   （调用方据此报 Provider 错误，而非 panic）
/// - `sk-xxxx`（明文）→ 原样返回
/// - 前后空白被 trim
///
/// 放在 cyber-core 而非 cyber-agent：纯字符串→env 映射，无 HTTP 依赖，
/// 且 `ProviderConfig::resolved_api_key` 与配置层同处更自然。
pub fn resolve_api_key(s: &str) -> String {
    let s = s.trim();
    if let Some(var) = s.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
        std::env::var(var).unwrap_or_default()
    } else {
        s.to_string()
    }
}

/// 判定模型是否为 DeepSeek 多模态视觉模型（或变体）。
pub fn is_deepseek_vision_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("deepseek-flash")
        || m.contains("flash-vision")
        || m.contains("deepseek-vl")
        || m.contains("deepseek-v4-flash")
}

/// 依据模型 id 判定是否具备思考/推理能力（规则表，非实测；仅用于列表打标）。
pub fn is_reasoning_model(model: &str) -> bool {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return false;
    }
    const SUBSTR: &[&str] = &[
        "reasoner",
        "reasoning",
        "thinking",
        "-think",
        "think-",
        "qwq",
        "-r1",
        "r1-",
        "gpt-5",
        "glm-5",
        "glm-4.5",
        "qwen3",
        "magistral",
        "deepseek-v4",
        "kimi-k2",
        "grok-4",
        "claude-sonnet-4",
        "claude-opus-4",
    ];
    if SUBSTR.iter().any(|p| m.contains(p)) {
        return true;
    }
    // OpenAI o 系列：必须出现在开头或路径/连字符边界，避免误判随机 id。
    ["o1", "o3", "o4"].iter().any(|p| {
        m.starts_with(p)
            || m.contains(&format!("/{p}"))
            || m.contains(&format!("-{p}"))
            || m.contains(&format!("{p}-"))
    })
}

/// 依据模型 id 判定是否具备视觉能力（规则表，非实测；仅用于列表打标）。
pub fn is_vision_model_by_name(model: &str) -> bool {
    const SUBSTR: &[&str] = &[
        "vision",
        "-vl",
        "vl-",
        "vl2",
        "multimodal",
        "gpt-4o",
        "gpt-4.1",
        "gpt-5",
        "claude-3",
        "claude-4",
        "claude-sonnet",
        "claude-opus",
        "gemini",
        "qwen-vl",
        "qwen2-vl",
        "qwen2.5-vl",
        "qwen3-vl",
        "llava",
        "pixtral",
        "internvl",
        "glm-4v",
        "deepseek-vl",
        "deepseek-flash",
        "flash-vision",
        "deepseek-v4-flash",
    ];
    let m = model.trim().to_ascii_lowercase();
    !m.is_empty() && SUBSTR.iter().any(|p| m.contains(p))
}

/// 判定服务商配置是否为 DeepSeek 服务商（官方或包含 DeepSeek 模型的第三方服务）。
pub fn is_deepseek_provider(cfg: &ProviderConfig) -> bool {
    cfg.base_url.contains("deepseek.com") || cfg.model.to_ascii_lowercase().contains("deepseek")
}

/// 大模型厂商与服务商热门预设。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: &'static str,
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub suggested_models: &'static [&'static str],
    pub env_var_suggestion: &'static str,
    pub description: &'static str,
}

impl ProviderPreset {
    /// 转换为初始的 `ProviderConfig`。
    pub fn to_provider_config(&self) -> ProviderConfig {
        ProviderConfig {
            kind: self.kind.to_string(),
            base_url: self.base_url.to_string(),
            api_key: if self.env_var_suggestion.is_empty() {
                String::new()
            } else {
                format!("${{{}}}", self.env_var_suggestion)
            },
            model: self.default_model.to_string(),
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: 0.7,
            ..Default::default()
        }
    }
}

/// 模型视觉/识图多模态支持状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum VisionCapability {
    #[default]
    Unknown,
    Supported,
    Unsupported,
}

impl VisionCapability {
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }

    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    pub fn from_bool(val: bool) -> Self {
        if val {
            Self::Supported
        } else {
            Self::Unsupported
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Supported => Some(true),
            Self::Unsupported => Some(false),
            Self::Unknown => None,
        }
    }

    /// 用于 UI 或 CLI 展示的状态文本。
    pub fn badge_text(&self) -> &'static str {
        match self {
            Self::Supported => "◈ 视觉",
            Self::Unsupported => "",
            Self::Unknown => "",
        }
    }
}

/// 模型思考/推理支持状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningCapability {
    #[default]
    Unknown,
    Supported,
    Unsupported,
}

impl ReasoningCapability {
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported)
    }

    pub fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }

    pub fn from_bool(val: bool) -> Self {
        if val {
            Self::Supported
        } else {
            Self::Unsupported
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Supported => Some(true),
            Self::Unsupported => Some(false),
            Self::Unknown => None,
        }
    }

    /// 用于 UI 或 CLI 展示的状态文本。
    pub fn badge_text(&self) -> &'static str {
        match self {
            Self::Supported => "◈ 推理",
            Self::Unsupported => "",
            Self::Unknown => "",
        }
    }
}

/// 模型能力本地持久化缓存（存放在 `~/.cyber/cache/capabilities.json`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CapabilityStore {
    #[serde(default)]
    pub capabilities: HashMap<String, VisionCapability>,
    #[serde(default)]
    pub reasoning: HashMap<String, ReasoningCapability>,
}

impl CapabilityStore {
    pub fn new() -> Self {
        Self {
            capabilities: HashMap::new(),
            reasoning: HashMap::new(),
        }
    }

    /// 规范化缓存 key：`{provider}:{model}`（若 provider 为空则直接使用 model）。
    pub fn cache_key(provider: &str, model: &str) -> String {
        let p = provider.trim().to_ascii_lowercase();
        let m = model.trim();
        if p.is_empty() {
            m.to_string()
        } else {
            format!("{p}:{m}")
        }
    }

    /// 获取模型视觉能力。支持以 `{provider}:{model}` 或 `{model}` 查询。
    pub fn get(&self, provider: &str, model: &str) -> VisionCapability {
        let key = Self::cache_key(provider, model);
        if let Some(&cap) = self.capabilities.get(&key) {
            return cap;
        }
        let m = model.trim();
        if let Some(&cap) = self.capabilities.get(m) {
            return cap;
        }
        VisionCapability::Unknown
    }

    /// 设置模型视觉能力。
    pub fn set(&mut self, provider: &str, model: &str, cap: VisionCapability) {
        let key = Self::cache_key(provider, model);
        self.capabilities.insert(key, cap);
        let m = model.trim().to_string();
        if !provider.trim().is_empty() {
            self.capabilities.entry(m).or_insert(cap);
        }
    }

    /// 获取模型思考/推理能力。支持以 `{provider}:{model}` 或 `{model}` 查询。
    pub fn get_reasoning(&self, provider: &str, model: &str) -> ReasoningCapability {
        let key = Self::cache_key(provider, model);
        if let Some(&cap) = self.reasoning.get(&key) {
            return cap;
        }
        let m = model.trim();
        if let Some(&cap) = self.reasoning.get(m) {
            return cap;
        }
        ReasoningCapability::Unknown
    }

    /// 设置模型思考/推理能力。
    pub fn set_reasoning(&mut self, provider: &str, model: &str, cap: ReasoningCapability) {
        let key = Self::cache_key(provider, model);
        self.reasoning.insert(key, cap);
        let m = model.trim().to_string();
        if !provider.trim().is_empty() {
            self.reasoning.entry(m).or_insert(cap);
        }
    }

    /// 从默认路径加载持久化缓存文件。
    pub fn load() -> Self {
        if let Some(path) = Self::default_cache_path() {
            Self::load_from_path(&path).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    /// 从指定路径读取缓存。
    pub fn load_from_path(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path)?;
        let store: Self = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(store)
    }

    /// 保存当前缓存到默认路径。
    pub fn save(&self) -> std::io::Result<()> {
        if let Some(path) = Self::default_cache_path() {
            self.save_to_path(&path)?;
        }
        Ok(())
    }

    /// 原子保存当前缓存到指定路径。
    pub fn save_to_path(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// 默认缓存文件路径：`~/.cyber/cache/capabilities.json`。
    pub fn default_cache_path() -> Option<PathBuf> {
        crate::paths::Paths::detect()
            .ok()
            .map(|p| p.cyber_home.join("cache").join("capabilities.json"))
    }
}

/// 查询指定模型是否具备视觉能力。
///
/// 优先级：
/// 1. `ProviderConfig::models` 中该模型显式配置的 `vision` 字段（Some(true) / Some(false)）；
/// 2. 传入的或从本地持久化加载的 `CapabilityStore`；
/// 3. 若均未命中，返回 `VisionCapability::Unknown`。
pub fn get_model_vision_capability(
    cfg: &ProviderConfig,
    provider_name: &str,
    model: &str,
    cache: Option<&CapabilityStore>,
) -> VisionCapability {
    if let Some(mc) = cfg.models.get(model) {
        if let Some(v) = mc.vision {
            return VisionCapability::from_bool(v);
        }
    }
    if let Some(store) = cache {
        let cap = store.get(provider_name, model);
        if !cap.is_unknown() {
            return cap;
        }
    } else {
        let store = CapabilityStore::load();
        let cap = store.get(provider_name, model);
        if !cap.is_unknown() {
            return cap;
        }
    }
    VisionCapability::Unknown
}

/// 保存模型视觉能力实测结果到持久化缓存，并同步回写至 `ProviderConfig::models`。
pub fn save_model_vision_capability(
    provider_name: &str,
    model: &str,
    capability: VisionCapability,
    provider_cfg: Option<&mut ProviderConfig>,
) -> std::io::Result<()> {
    let mut store = CapabilityStore::load();
    store.set(provider_name, model, capability);
    let _ = store.save();

    if let Some(cfg) = provider_cfg {
        let mc = cfg.models.entry(model.to_string()).or_default();
        mc.vision = capability.as_bool();
    }
    Ok(())
}

/// 保存模型思考/推理能力实测结果到持久化缓存，并同步回写至 `ProviderConfig::models`。
pub fn save_model_reasoning_capability(
    provider_name: &str,
    model: &str,
    capability: ReasoningCapability,
    provider_cfg: Option<&mut ProviderConfig>,
) -> std::io::Result<()> {
    let mut store = CapabilityStore::load();
    store.set_reasoning(provider_name, model, capability);
    let _ = store.save();

    if let Some(cfg) = provider_cfg {
        let mc = cfg.models.entry(model.to_string()).or_default();
        mc.reasoning = capability.as_bool();
    }
    Ok(())
}

/// 视觉能力：显式 `models` 配置 → 持久化实测缓存 → 名称规则表 → `Unknown`。
pub fn resolve_vision_capability(
    models: Option<&HashMap<String, ModelConfig>>,
    provider: &str,
    model: &str,
    store: &CapabilityStore,
) -> VisionCapability {
    if let Some(v) = models.and_then(|m| m.get(model)).and_then(|mc| mc.vision) {
        return VisionCapability::from_bool(v);
    }
    let cached = store.get(provider, model);
    if !cached.is_unknown() {
        return cached;
    }
    if is_vision_model_by_name(model) {
        return VisionCapability::Supported;
    }
    VisionCapability::Unknown
}

/// 推理能力：显式 `models` 配置 → 持久化实测缓存 → 名称规则表 → `Unknown`。
pub fn resolve_reasoning_capability(
    models: Option<&HashMap<String, ModelConfig>>,
    provider: &str,
    model: &str,
    store: &CapabilityStore,
) -> ReasoningCapability {
    if let Some(v) = models
        .and_then(|m| m.get(model))
        .and_then(|mc| mc.reasoning)
    {
        return ReasoningCapability::from_bool(v);
    }
    let cached = store.get_reasoning(provider, model);
    if !cached.is_unknown() {
        return cached;
    }
    if is_reasoning_model(model) {
        return ReasoningCapability::Supported;
    }
    ReasoningCapability::Unknown
}

/// 常见的大模型厂商预设列表。
pub const PROVIDER_PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "deepseek",
        name: "DeepSeek 官方",
        kind: "openai",
        base_url: "https://api.deepseek.com",
        default_model: "deepseek-chat",
        suggested_models: &["deepseek-chat", "deepseek-reasoner", "deepseek-flash"],
        env_var_suggestion: "DEEPSEEK_API_KEY",
        description:
            "国内顶尖推理与通用大模型，支持 deepseek-chat、R1 深度思考及 deepseek-flash 视觉多模态",
    },
    ProviderPreset {
        id: "siliconflow",
        name: "硅基流动 (SiliconFlow)",
        kind: "openai",
        base_url: "https://api.siliconflow.cn/v1",
        default_model: "deepseek-ai/DeepSeek-V3",
        suggested_models: &[
            "deepseek-ai/DeepSeek-V3",
            "deepseek-ai/DeepSeek-R1",
            "Qwen/Qwen2.5-72B-Instruct",
        ],
        env_var_suggestion: "SILICONFLOW_API_KEY",
        description: "高并发模型托管云，极速响应 DeepSeek-V3 / R1 与开源生态",
    },
    ProviderPreset {
        id: "dashscope",
        name: "阿里百炼 (DashScope)",
        kind: "openai",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        default_model: "qwen-plus",
        suggested_models: &["qwen-plus", "qwen-max", "deepseek-v3", "deepseek-r1"],
        env_var_suggestion: "DASHSCOPE_API_KEY",
        description: "阿里云千问通义系列，企业级高可用模型服务",
    },
    ProviderPreset {
        id: "zhipu",
        name: "智谱 AI (GLM)",
        kind: "openai",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        default_model: "glm-4-plus",
        suggested_models: &["glm-4-plus", "glm-4-flash"],
        env_var_suggestion: "ZHIPU_API_KEY",
        description: "智谱开放平台，中文理解与代码能力强劲",
    },
    ProviderPreset {
        id: "moonshot",
        name: "月之暗面 (Kimi)",
        kind: "openai",
        base_url: "https://api.moonshot.cn/v1",
        default_model: "moonshot-v1-32k",
        suggested_models: &["moonshot-v1-8k", "moonshot-v1-32k", "moonshot-v1-128k"],
        env_var_suggestion: "MOONSHOT_API_KEY",
        description: "长上下文模型，适合长审计报告与海量日志分析",
    },
    ProviderPreset {
        id: "openai",
        name: "OpenAI 官方",
        kind: "openai",
        base_url: "https://api.openai.com/v1",
        default_model: "gpt-4o",
        suggested_models: &["gpt-4o", "gpt-4o-mini", "o3-mini"],
        env_var_suggestion: "OPENAI_API_KEY",
        description: "OpenAI 官方服务，支持 GPT-4o 及推理模型",
    },
    ProviderPreset {
        id: "anthropic",
        name: "Anthropic 官方",
        kind: "anthropic",
        base_url: "https://api.anthropic.com",
        default_model: "claude-3-7-sonnet-20250219",
        suggested_models: &[
            "claude-3-7-sonnet-20250219",
            "claude-3-5-sonnet-20241022",
            "claude-3-5-haiku-20241022",
        ],
        env_var_suggestion: "ANTHROPIC_API_KEY",
        description: "顶尖代码与推理能力，Claude 3.7 Sonnet 混合思考模型",
    },
    ProviderPreset {
        id: "openrouter",
        name: "OpenRouter 聚合",
        kind: "openai",
        base_url: "https://openrouter.ai/api/v1",
        default_model: "deepseek/deepseek-chat",
        suggested_models: &[
            "deepseek/deepseek-chat",
            "anthropic/claude-3.7-sonnet",
            "openai/gpt-4o",
        ],
        env_var_suggestion: "OPENROUTER_API_KEY",
        description: "全球模型聚合网关，统一 API 访问数百款模型",
    },
    ProviderPreset {
        id: "ollama",
        name: "本地 Ollama",
        kind: "ollama",
        base_url: "http://localhost:11434",
        default_model: "qwen2.5:32b",
        suggested_models: &["qwen2.5:32b", "deepseek-r1:14b", "llama3.3:70b"],
        env_var_suggestion: "",
        description: "完全本地离线运行，数据绝不上云，适合敏感审计与内网测试",
    },
    ProviderPreset {
        id: "custom",
        name: "自定义 OpenAI 兼容接口",
        kind: "openai",
        base_url: "http://localhost:8000/v1",
        default_model: "custom",
        suggested_models: &[],
        env_var_suggestion: "CUSTOM_API_KEY",
        description: "兼容 OpenAI 规范的自建模型、OneAPI、vLLM 或本地中转代理",
    },
];

#[cfg(test)]
#[allow(clippy::field_reassign_with_default)]
mod tests {
    use super::*;

    #[test]
    fn resolve_plaintext_passthrough() {
        assert_eq!(resolve_api_key("sk-abc123"), "sk-abc123");
        assert_eq!(resolve_api_key(""), "");
    }

    #[test]
    fn resolve_env_var_reference() {
        std::env::set_var("CYBER_TEST_KEY_RESOLVE", "secret-value-42");
        assert_eq!(
            resolve_api_key("${CYBER_TEST_KEY_RESOLVE}"),
            "secret-value-42"
        );
        std::env::remove_var("CYBER_TEST_KEY_RESOLVE");
    }

    #[test]
    fn resolve_unset_env_var_returns_empty() {
        // 极不可能存在的变量名
        assert_eq!(
            resolve_api_key("${CYBER_TEST_KEY_DEFINITELY_UNSET_XYZ}"),
            ""
        );
    }

    #[test]
    fn resolve_trims_whitespace() {
        std::env::set_var("CYBER_TEST_KEY_TRIM", "v");
        assert_eq!(resolve_api_key("  ${CYBER_TEST_KEY_TRIM}  "), "v");
        std::env::remove_var("CYBER_TEST_KEY_TRIM");
        assert_eq!(resolve_api_key("  sk-plain  "), "sk-plain");
    }

    #[test]
    fn resolve_no_suffix_treated_as_plaintext() {
        // 缺少 `}` 不视作 env 引用，原样返回（避免误吞用户输入）
        assert_eq!(resolve_api_key("${OPENAI_API_KEY"), "${OPENAI_API_KEY");
    }

    #[test]
    fn deepseek_detection_tests() {
        assert!(is_deepseek_vision_model("deepseek-flash"));
        assert!(is_deepseek_vision_model("DeepSeek-Flash-Vision"));
        assert!(is_deepseek_vision_model("deepseek-vl-7b"));
        assert!(is_deepseek_vision_model("deepseek-v4-flash-vision-exp"));
        assert!(!is_deepseek_vision_model("deepseek-chat"));
        assert!(!is_deepseek_vision_model("gpt-4o"));

        let p1 = ProviderConfig {
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-chat".into(),
            ..Default::default()
        };
        assert!(is_deepseek_provider(&p1));

        let p2 = ProviderConfig {
            base_url: "https://api.siliconflow.cn/v1".into(),
            model: "deepseek-ai/DeepSeek-V3".into(),
            ..Default::default()
        };
        assert!(is_deepseek_provider(&p2));

        let p3 = ProviderConfig {
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-4o".into(),
            ..Default::default()
        };
        assert!(!is_deepseek_provider(&p3));
    }
    #[test]
    fn provider_config_resolved_api_key() {
        std::env::set_var("CYBER_TEST_KEY_CFG", "cfg-value");
        let p = ProviderConfig {
            api_key: "${CYBER_TEST_KEY_CFG}".into(),
            ..Default::default()
        };
        assert_eq!(p.resolved_api_key(), "cfg-value");
        std::env::remove_var("CYBER_TEST_KEY_CFG");

        let p2 = ProviderConfig {
            api_key: "sk-plain".into(),
            ..Default::default()
        };
        assert_eq!(p2.resolved_api_key(), "sk-plain");
    }

    #[test]
    fn provider_kinds_has_four_entries() {
        assert_eq!(PROVIDER_KINDS.len(), 5);
        assert!(PROVIDER_KINDS.contains(&"openai"));
        assert!(PROVIDER_KINDS.contains(&"openai-compatible"));
        assert!(PROVIDER_KINDS.contains(&"responses"));
    }

    #[test]
    fn normalize_strips_trailing_slash_and_trims() {
        let mut p = ProviderConfig {
            base_url: "  https://api.openai.com/v1/  ".into(),
            api_key: "  sk-x  ".into(),
            model: "  gpt-4o  ".into(),
            ..Default::default()
        };
        p.normalize();
        assert_eq!(p.base_url, "https://api.openai.com/v1");
        assert_eq!(p.api_key, "sk-x");
        assert_eq!(p.model, "gpt-4o");
    }

    #[test]
    fn sorted_names_returns_sorted() {
        let cfg = ProvidersConfig::default_template();
        let names = cfg.sorted_names();
        assert_eq!(names, vec!["anthropic", "ollama", "openai"]);
    }

    #[test]
    fn upsert_inserts_and_overrides() {
        let mut cfg = ProvidersConfig::default();
        cfg.upsert(
            "foo",
            ProviderConfig {
                kind: "openai".into(),
                base_url: "https://x/".into(),
                ..Default::default()
            },
        );
        assert_eq!(cfg.providers.len(), 1);
        assert_eq!(cfg.providers["foo"].base_url, "https://x"); // normalize 去尾 /

        cfg.upsert(
            "foo",
            ProviderConfig {
                kind: "anthropic".into(),
                base_url: "https://y".into(),
                ..Default::default()
            },
        );
        assert_eq!(cfg.providers.len(), 1, "同名应覆盖");
        assert_eq!(cfg.providers["foo"].kind, "anthropic");
    }

    #[test]
    fn remove_returns_old_or_none() {
        let mut cfg = ProvidersConfig::default_template();
        let removed = cfg.remove("openai");
        assert!(removed.is_some());
        assert!(!cfg.providers.contains_key("openai"));
        assert!(cfg.remove("nope").is_none());
    }

    // ── ModelConfig / effective_* 测试 ──

    #[test]
    fn default_max_tokens_is_384k_and_clamped_by_context_length() {
        let mut p = ProviderConfig::default();
        p.model = "m".into();
        assert_eq!(p.max_tokens, DEFAULT_MAX_TOKENS);
        // 未声明 context_length → 兜底钳制到 DEFAULT_OUTPUT_TOKEN_CAP
        assert_eq!(p.effective_max_tokens(), DEFAULT_OUTPUT_TOKEN_CAP);
        // 声明 context_length → 钳制到该值
        p.models.insert(
            "m".into(),
            ModelConfig {
                context_length: Some(64_000),
                ..Default::default()
            },
        );
        assert_eq!(p.effective_max_tokens(), 64_000);
        // 声明值大于配置值时不放大
        p.max_tokens = 2048;
        assert_eq!(p.effective_max_tokens(), 2048);
    }

    #[test]
    fn effective_params_fallback_to_provider_level() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        p.max_tokens = 4096;
        p.temperature = 0.5;
        // 无 per-model 配置 → 回退到 provider 级
        assert_eq!(p.effective_max_tokens(), 4096);
        assert!((p.effective_temperature() - 0.5).abs() < 1e-6);
        assert_eq!(p.model_display_name(), "gpt-4o");
    }

    #[test]
    fn effective_params_per_model_overrides() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        p.max_tokens = 4096;
        p.temperature = 0.5;
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                alias: Some("我的GPT".into()),
                max_tokens: Some(8192),
                temperature: Some(0.1),
                ..Default::default()
            },
        );
        assert_eq!(p.effective_max_tokens(), 8192);
        assert!((p.effective_temperature() - 0.1).abs() < 1e-6);
        assert_eq!(p.model_display_name(), "我的GPT");
    }

    #[test]
    fn effective_price_per_model_overrides() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        p.price = Some(PriceConfig {
            input_per_m: Some(2.5),
            ..Default::default()
        });
        // per-model price 覆盖
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                price: Some(PriceConfig {
                    input_per_m: Some(5.0),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let eff = p.effective_price().unwrap();
        assert!((eff.input_per_m.unwrap() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn effective_price_falls_back_to_provider() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        p.price = Some(PriceConfig {
            input_per_m: Some(2.5),
            ..Default::default()
        });
        // per-model 有配置但 price=None → 回退到 provider 级
        p.models.insert("gpt-4o".into(), ModelConfig::default());
        let eff = p.effective_price().unwrap();
        assert!((eff.input_per_m.unwrap() - 2.5).abs() < 1e-9);
    }

    #[test]
    fn model_display_name_empty_alias_falls_back() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                alias: Some(String::new()), // 空字符串
                ..Default::default()
            },
        );
        assert_eq!(p.model_display_name(), "gpt-4o");
    }

    #[test]
    fn model_display_name_no_per_model_config() {
        let mut p = ProviderConfig::default();
        p.model = "claude-3".into();
        // 无 per-model 配置
        assert_eq!(p.model_display_name(), "claude-3");
    }

    #[test]
    fn effective_params_model_not_in_models_map() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o-mini".into();
        p.max_tokens = 2048;
        // models 有 gpt-4o 但当前 model 是 gpt-4o-mini
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                max_tokens: Some(8192),
                ..Default::default()
            },
        );
        // gpt-4o-mini 不在 models → 回退到 provider 级
        assert_eq!(p.effective_max_tokens(), 2048);
    }

    #[test]
    fn effective_context_length_per_model() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                context_length: Some(128_000),
                ..Default::default()
            },
        );
        assert_eq!(p.effective_context_length(), Some(128_000));
    }

    #[test]
    fn effective_context_length_none_when_not_configured() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        // 无 per-model 配置
        assert_eq!(p.effective_context_length(), None);
        // 即便配置了，0 也视作未配置
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                context_length: Some(0),
                ..Default::default()
            },
        );
        assert_eq!(p.effective_context_length(), None);
    }

    #[test]
    fn effective_context_length_falls_back_when_model_not_in_map() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o-mini".into();
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                context_length: Some(128_000),
                ..Default::default()
            },
        );
        // 当前 model 不在 models → None（无 provider 级回退）
        assert_eq!(p.effective_context_length(), None);
    }

    #[test]
    fn effective_currency_defaults_to_usd() {
        let p = ProviderConfig::default();
        assert_eq!(p.effective_currency(), "usd");
    }

    #[test]
    fn effective_currency_from_provider_price() {
        let mut p = ProviderConfig::default();
        p.price = Some(PriceConfig {
            input_per_m: Some(2.5),
            currency: Some("cny".into()),
            ..Default::default()
        });
        assert_eq!(p.effective_currency(), "cny");
    }

    #[test]
    fn effective_currency_per_model_overrides_provider() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        p.price = Some(PriceConfig {
            input_per_m: Some(2.5),
            currency: Some("usd".into()),
            ..Default::default()
        });
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                price: Some(PriceConfig {
                    input_per_m: Some(2.5),
                    currency: Some("cny".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert_eq!(p.effective_currency(), "cny");
    }

    #[test]
    fn model_config_serde_roundtrip() {
        let mc = ModelConfig {
            alias: Some("别名".into()),
            context_length: Some(128000),
            max_tokens: Some(4096),
            temperature: Some(0.7),
            price: Some(PriceConfig {
                input_per_m: Some(2.5),
                output_per_m: Some(10.0),
                cache_hit_per_m: None,
                ..Default::default()
            }),
            notes: Some("测试备注".into()),
            vision: Some(true),
            reasoning: Some(true),
        };
        let json = serde_json::to_string(&mc).unwrap();
        let mc2: ModelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(mc2.alias, Some("别名".into()));
        assert_eq!(mc2.context_length, Some(128000));
        assert_eq!(mc2.notes, Some("测试备注".into()));
        assert_eq!(mc2.vision, Some(true));
        assert_eq!(mc2.reasoning, Some(true));
    }

    #[test]
    fn provider_config_with_models_serde_roundtrip() {
        let mut p = ProviderConfig::default();
        p.model = "gpt-4o".into();
        p.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                alias: Some("GPT4o".into()),
                context_length: Some(128000),
                ..Default::default()
            },
        );
        let toml_str = toml::to_string(&p).unwrap();
        let p2: ProviderConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(p2.model, "gpt-4o");
        assert!(p2.models.contains_key("gpt-4o"));
        assert_eq!(p2.models["gpt-4o"].alias, Some("GPT4o".into()));
    }

    #[test]
    fn provider_config_without_models_backwards_compat() {
        // 旧配置无 models 字段，serde(default) 应正常解析
        let toml_str = r#"
kind = "openai"
base_url = "https://api.openai.com/v1"
api_key = "sk-x"
model = "gpt-4o"
max_tokens = 4096
temperature = 0.7
"#;
        let p: ProviderConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(p.model, "gpt-4o");
        assert!(p.models.is_empty());
        assert_eq!(p.effective_max_tokens(), 4096);
    }

    #[test]
    fn chat_endpoint_defaults_correctly() {
        let openai_cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            ..Default::default()
        };
        assert_eq!(
            openai_cfg.chat_endpoint(),
            "https://api.openai.com/v1/chat/completions"
        );

        let ollama_cfg = ProviderConfig {
            kind: "ollama".into(),
            base_url: "http://localhost:11434".into(),
            ..Default::default()
        };
        assert_eq!(
            ollama_cfg.chat_endpoint(),
            "http://localhost:11434/api/chat"
        );

        let custom_cfg = ProviderConfig {
            kind: "ollama".into(),
            base_url: "http://localhost:11434".into(),
            chat_endpoint: Some("http://localhost:11434/v1/chat/completions".into()),
            ..Default::default()
        };
        assert_eq!(
            custom_cfg.chat_endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );
    }

    #[test]
    fn provider_presets_are_valid() {
        assert!(!PROVIDER_PRESETS.is_empty());
        for preset in PROVIDER_PRESETS {
            assert!(!preset.id.is_empty());
            assert!(!preset.name.is_empty());
            let cfg = preset.to_provider_config();
            assert_eq!(cfg.kind, preset.kind);
            assert_eq!(cfg.base_url, preset.base_url);
            assert_eq!(cfg.model, preset.default_model);
        }
    }

    #[test]
    fn vision_capability_store_roundtrip() {
        let dir = std::env::temp_dir().join(format!("cyber_cap_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("capabilities.json");

        let mut store = CapabilityStore::new();
        store.set("openai", "gpt-4o", VisionCapability::Supported);
        store.set("deepseek", "deepseek-chat", VisionCapability::Unsupported);

        assert_eq!(store.get("openai", "gpt-4o"), VisionCapability::Supported);
        assert_eq!(
            store.get("deepseek", "deepseek-chat"),
            VisionCapability::Unsupported
        );
        assert_eq!(
            store.get("custom", "unknown-model"),
            VisionCapability::Unknown
        );

        store.save_to_path(&path).unwrap();
        let loaded = CapabilityStore::load_from_path(&path).unwrap();
        assert_eq!(loaded.get("openai", "gpt-4o"), VisionCapability::Supported);
        assert_eq!(
            loaded.get("deepseek", "deepseek-chat"),
            VisionCapability::Unsupported
        );
        assert_eq!(
            loaded.get("deepseek", "deepseek-chat"),
            VisionCapability::Unsupported
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn get_model_vision_capability_precedence() {
        let mut cfg = ProviderConfig::default();
        cfg.models.insert(
            "explicit-model".into(),
            ModelConfig {
                vision: Some(true),
                ..Default::default()
            },
        );

        let mut store = CapabilityStore::new();
        store.set("test-p", "cached-model", VisionCapability::Supported);
        store.set("test-p", "explicit-model", VisionCapability::Unsupported);

        // 1. 显式配置优先于缓存
        assert_eq!(
            get_model_vision_capability(&cfg, "test-p", "explicit-model", Some(&store)),
            VisionCapability::Supported
        );

        // 2. 缓存命中
        assert_eq!(
            get_model_vision_capability(&cfg, "test-p", "cached-model", Some(&store)),
            VisionCapability::Supported
        );

        // 3. 未知模型
        assert_eq!(
            get_model_vision_capability(&cfg, "test-p", "non-existent", Some(&store)),
            VisionCapability::Unknown
        );
    }

    #[test]
    fn vision_capability_badge_text() {
        assert_eq!(VisionCapability::Supported.badge_text(), "◈ 视觉");
        assert_eq!(VisionCapability::Unsupported.badge_text(), "");
        assert_eq!(VisionCapability::Unknown.badge_text(), "");
    }

    #[test]
    fn thinking_config_serde_roundtrip() {
        let cfg: ProvidersConfig = toml::from_str(
            r#"
default_provider = "x"
[providers.x]
kind = "openai"
[providers.x.thinking]
type = "enabled"
effort = "high"
"#,
        )
        .unwrap();
        assert_eq!(
            cfg.providers["x"].thinking,
            Some(ThinkingConfig {
                r#type: Some("enabled".into()),
                effort: Some("high".into()),
            })
        );
        let text = toml::to_string(&cfg.providers["x"]).unwrap();
        assert!(text.contains("type = \"enabled\""), "{text}");
        assert!(text.contains("effort = \"high\""), "{text}");
        // 两个字段均未设置时不落盘任何键。
        assert!(toml::to_string(&ThinkingConfig::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn thinking_normalize_trims_and_drops_empty() {
        let mut cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            thinking: Some(ThinkingConfig {
                r#type: Some("  ENABLED ".into()),
                effort: Some(" High ".into()),
            }),
            ..Default::default()
        };
        cfg.normalize();
        assert_eq!(
            cfg.thinking,
            Some(ThinkingConfig {
                r#type: Some("enabled".into()),
                effort: Some("high".into()),
            })
        );

        cfg.thinking = Some(ThinkingConfig {
            r#type: Some("  ".into()),
            effort: Some(String::new()),
        });
        cfg.normalize();
        assert!(cfg.thinking.is_none(), "全空 thinking 应归一为 None");
    }

    #[test]
    fn is_reasoning_model_matches_known_families() {
        for m in [
            "cline-pass/glm-5.3-flash",
            "deepseek-v4.1-flash",
            "qwq-32b",
            "o3-mini",
            "deepseek-reasoner",
            "glm-4.5-air",
        ] {
            assert!(is_reasoning_model(m), "{m} 应判为推理模型");
        }
        for m in ["gpt-4o", "qwen2.5:32b", "", "llama3.3:70b"] {
            assert!(!is_reasoning_model(m), "{m} 不应判为推理模型");
        }
    }

    #[test]
    fn vision_rule_table_matches_known_families() {
        for m in [
            "qwen3-vl-32b",
            "gpt-4o",
            "deepseek-vl2",
            "deepseek-v4-flash-free",
            "claude-3-5-sonnet-20241022",
        ] {
            assert!(is_vision_model_by_name(m), "{m} 应判为视觉模型");
        }
        assert!(!is_vision_model_by_name("plain-text-model"));
    }

    #[test]
    fn resolve_reasoning_capability_precedence() {
        let mut store = CapabilityStore::new();
        store.set_reasoning("p", "glm-5", ReasoningCapability::Supported);
        store.set_reasoning("p", "explicit-model", ReasoningCapability::Supported);

        let mut cfg = ProviderConfig::default();
        cfg.models.insert(
            "explicit-model".into(),
            ModelConfig {
                reasoning: Some(false),
                ..Default::default()
            },
        );

        // 1. 显式配置压过缓存与规则表
        assert_eq!(
            resolve_reasoning_capability(Some(&cfg.models), "p", "explicit-model", &store),
            ReasoningCapability::Unsupported
        );
        // 2. 缓存压过规则表
        assert_eq!(
            resolve_reasoning_capability(Some(&cfg.models), "p", "glm-5", &store),
            ReasoningCapability::Supported
        );
        // 3. 仅规则表命中
        assert_eq!(
            resolve_reasoning_capability(Some(&cfg.models), "p", "deepseek-v4-flash", &store),
            ReasoningCapability::Supported
        );
        // 4. 三者皆无 → Unknown
        assert_eq!(
            resolve_reasoning_capability(Some(&cfg.models), "p", "llama3.3:70b", &store),
            ReasoningCapability::Unknown
        );
    }

    #[test]
    fn resolve_vision_capability_falls_back_to_rule_table() {
        let store = CapabilityStore::new();
        assert_eq!(
            resolve_vision_capability(None, "p", "qwen3-vl-32b", &store),
            VisionCapability::Supported
        );
        assert_eq!(
            resolve_vision_capability(None, "p", "plain-text-model", &store),
            VisionCapability::Unknown
        );
    }

    #[test]
    fn with_api_version_appends_only_when_missing() {
        // 未含版本段 → 补 /v1
        assert_eq!(
            with_api_version("https://api.anthropic.com"),
            "https://api.anthropic.com/v1"
        );
        assert_eq!(
            with_api_version("  http://localhost:11434/  "),
            "http://localhost:11434/v1"
        );
        assert_eq!(
            with_api_version("https://gw.test/openai"),
            "https://gw.test/openai/v1"
        );
        // 已含版本段 → 原样（仅去尾 /），绝不重复
        assert_eq!(
            with_api_version("https://api.anthropic.com/v1"),
            "https://api.anthropic.com/v1"
        );
        assert_eq!(
            with_api_version("https://api.anthropic.com/v1/"),
            "https://api.anthropic.com/v1"
        );
        assert_eq!(
            with_api_version("https://api.cline.bot/api/v1"),
            "https://api.cline.bot/api/v1"
        );
        assert_eq!(
            with_api_version("https://gw.test/api/v1beta"),
            "https://gw.test/api/v1beta"
        );
        assert_eq!(
            with_api_version("https://gw.test/api/v2"),
            "https://gw.test/api/v2"
        );
        assert_eq!(
            with_api_version("https://gw.test/api-v1"),
            "https://gw.test/api-v1"
        );
        // 主机名里的 v1 不算版本段（只看 path）
        assert_eq!(
            with_api_version("https://v1.example.com"),
            "https://v1.example.com/v1"
        );
    }

    #[test]
    fn reasoning_cache_serde_backwards_compatible() {
        // 旧 capabilities.json（无 reasoning 字段）必须可读。
        let store: CapabilityStore =
            serde_json::from_str(r#"{"capabilities":{"p:m":"supported"}}"#).unwrap();
        assert_eq!(store.get("p", "m"), VisionCapability::Supported);
        assert_eq!(store.get_reasoning("p", "m"), ReasoningCapability::Unknown);
    }
}
