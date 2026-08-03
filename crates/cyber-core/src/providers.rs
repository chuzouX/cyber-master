use std::collections::HashMap;

use serde::{Deserialize, Serialize};

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
}

impl ProviderConfig {
    /// 解析自身 `api_key`：`${ENV_VAR}` 引用展开为环境变量值，明文原样返回。
    pub fn resolved_api_key(&self) -> String {
        resolve_api_key(&self.api_key)
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
}
