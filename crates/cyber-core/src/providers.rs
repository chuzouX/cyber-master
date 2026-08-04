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
    /// 每个 model 的专属配置（覆盖 provider 级默认值）。key = model id。
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
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
            max_tokens: 4096,
            temperature: 0.7,
            price: None,
            models: HashMap::new(),
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

    /// 当前 model 的专属配置（若存在）。
    pub fn current_model_config(&self) -> Option<&ModelConfig> {
        self.models.get(&self.model)
    }

    /// 当前 model 的有效 max_tokens：per-model 优先，回退到 provider 级。
    pub fn effective_max_tokens(&self) -> u32 {
        self.current_model_config()
            .and_then(|m| m.max_tokens)
            .unwrap_or(self.max_tokens)
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

    // ── ModelConfig / effective_* 测试 ──

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
        };
        let json = serde_json::to_string(&mc).unwrap();
        let mc2: ModelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(mc2.alias, Some("别名".into()));
        assert_eq!(mc2.context_length, Some(128000));
        assert_eq!(mc2.notes, Some("测试备注".into()));
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
}
