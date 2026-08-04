use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 支持的 provider kind（format）。TUI 表单的 kind 字段在此循环；
/// `provider_factory` 接受前三种（openai/anthropic/ollama），
/// `openai-compatible` 复用 openai 的 SSE 解析路径。
pub const PROVIDER_KINDS: &[&str] = &["openai", "anthropic", "ollama", "openai-compatible"];

/// 对应 `~/.cyber/providers.toml`。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProvidersConfig {
    pub default_provider: String,
    pub providers: HashMap<String, ProviderConfig>,
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
}

/// token 单价配置（每百万 token 美元）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PriceConfig {
    /// 每百万输入 token（缓存未命中）价格。
    pub input_per_m: Option<f64>,
    /// 每百万输出 token 价格。
    pub output_per_m: Option<f64>,
    /// 每百万输入 token（缓存命中）价格。缺省时回退到 input_per_m。
    pub cache_hit_per_m: Option<f64>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            kind: "openai".into(),
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            max_tokens: 4096,
            temperature: 0.7,
            price: None,
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

#[cfg(test)]
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
        assert_eq!(resolve_api_key("${CYBER_TEST_KEY_RESOLVE}"), "secret-value-42");
        std::env::remove_var("CYBER_TEST_KEY_RESOLVE");
    }

    #[test]
    fn resolve_unset_env_var_returns_empty() {
        // 极不可能存在的变量名
        assert_eq!(resolve_api_key("${CYBER_TEST_KEY_DEFINITELY_UNSET_XYZ}"), "");
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
        assert_eq!(PROVIDER_KINDS.len(), 4);
        assert!(PROVIDER_KINDS.contains(&"openai"));
        assert!(PROVIDER_KINDS.contains(&"openai-compatible"));
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
}
