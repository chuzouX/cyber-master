//! 模型列表拉取：`GET {base}/models`（或 `/v1/models`）。
//!
//! 参考 wepclaude `utils/model/customProviders.js` 的 `fetchInferenceProviderModels` +
//! `extractModelIds`。TUI Provider 表单的「拉取模型」按钮异步调用 `fetch_models`，
//! 结果经 mpsc 通道回传主循环（见 `cyber-tui::app` 的第 4 路 `select!`）。

use std::time::Duration;

use serde_json::Value;

use cyber_core::{resolve_api_key, ProviderConfig};

use crate::error::{AgentError, Result};

/// Anthropic API 版本头（与 `anthropic.rs` 流式实现一致）。
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// 拉取超时（秒）。base_url 不可达时避免表单长时间卡在「拉取中」。
const FETCH_TIMEOUT_SECS: u64 = 15;

/// 按 `cfg` 拉取可用模型 id 列表。
///
/// 依次尝试 `fetch_endpoints` 返回的端点（kind 决定顺序），
/// 首个返回非空模型列表的端点即成功；全部失败返回最后一个错误。
pub async fn fetch_models(cfg: &ProviderConfig) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(FETCH_TIMEOUT_SECS))
        .build()?;
    let headers = fetch_headers(cfg);
    let endpoints = if let Some(custom) = cfg.models_endpoint() {
        vec![custom]
    } else {
        fetch_endpoints(&cfg.kind, &cfg.base_url)
    };

    let mut last_error = AgentError::Provider("无模型端点响应成功".into());
    for endpoint in endpoints {
        match client.get(&endpoint).headers(headers.clone()).send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    last_error = AgentError::Provider(format!(
                        "GET {endpoint} 失败 {status}{}",
                        if text.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", &text.chars().take(200).collect::<String>())
                        }
                    ));
                    continue;
                }
                let payload: Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        last_error = AgentError::Provider(format!("GET {endpoint} 解析 JSON 失败: {e}"));
                        continue;
                    }
                };
                let models = extract_model_ids(&payload);
                if models.is_empty() {
                    last_error = AgentError::Provider(format!(
                        "GET {endpoint} 成功但未返回模型 id"
                    ));
                    continue;
                }
                return Ok(models);
            }
            Err(e) => {
                last_error = AgentError::Provider(format!("GET {endpoint} 失败: {e}"));
            }
        }
    }
    Err(last_error)
}

/// 按 kind 返回候选端点（已规范化 base_url 去尾 `/`）。
///
/// - anthropic：先 `/v1/models`（Anthropic 标准），后 `/models`
/// - 其余（openai / openai-compatible / ollama）：先 `/models`，后 `/v1/models`（ollama
///   的 OpenAI 兼容端点）
pub fn fetch_endpoints(kind: &str, base_url: &str) -> Vec<String> {
    let normalized = base_url.trim().trim_end_matches('/');
    if kind == "anthropic" {
        vec![
            format!("{normalized}/v1/models"),
            format!("{normalized}/models"),
        ]
    } else {
        vec![
            format!("{normalized}/models"),
            format!("{normalized}/v1/models"),
        ]
    }
}

/// 按 kind 构造请求头（与各家流式实现一致）。
///
/// - anthropic：`x-api-key: <resolved>` + `anthropic-version`
/// - openai / openai-compatible：`Authorization: Bearer <resolved>`（key 为空则跳过）
/// - ollama：无 auth
#[allow(clippy::collapsible_match)]
pub fn fetch_headers(cfg: &ProviderConfig) -> reqwest::header::HeaderMap {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
    let mut h = HeaderMap::new();
    h.insert(HeaderName::from_static("accept"), HeaderValue::from_static("application/json"));
    let key = resolve_api_key(&cfg.api_key);
    match cfg.kind.as_str() {
        "anthropic" => {
            if !key.is_empty() {
                if let Ok(v) = HeaderValue::from_str(&key) {
                    h.insert(HeaderName::from_static("x-api-key"), v);
                }
            }
            h.insert(
                HeaderName::from_static("anthropic-version"),
                HeaderValue::from_static(ANTHROPIC_VERSION),
            );
        }
        "openai" | "openai-compatible" => {
            if !key.is_empty() {
                if let Ok(v) = HeaderValue::from_str(&format!("Bearer {key}")) {
                    h.insert(HeaderName::from_static("authorization"), v);
                }
            }
        }
        // ollama：无 auth header
        _ => {}
    }
    h
}

/// 从 `/models` 响应 payload 提取模型 id 列表（去重 + trim + 去空）。
///
/// 端口 wepclaude `extractModelIds`，并额外兼容 ollama `/api/tags` 的 `{models:[{name}]}`
/// 形态（对象条目优先取 `id`，回退 `name`）。支持：
/// - 顶层数组：`["m1", {id:"m2"}, {name:"m3"}]`
/// - `{data:[{id}]}`（OpenAI / Anthropic / ollama OpenAI 兼容）
/// - `{models:[{id}|{name}|String]}`（ollama /api/tags 等）
/// - 顶层 `{id:"m"}`
pub fn extract_model_ids(payload: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    collect_ids(payload, &mut out);
    dedupe_preserve_order(&mut out);
    out
}

fn collect_ids(payload: &Value, out: &mut Vec<String>) {
    match payload {
        Value::Array(arr) => {
            for entry in arr {
                match entry {
                    Value::String(s) => out.push(s.clone()),
                    Value::Object(obj) => {
                        if let Some(Value::String(s)) = obj.get("id") {
                            out.push(s.clone());
                        } else if let Some(Value::String(s)) = obj.get("name") {
                            out.push(s.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        Value::Object(obj) => {
            if let Some(data) = obj.get("data") {
                if data.is_array() {
                    return collect_ids(data, out);
                }
            }
            if let Some(models) = obj.get("models") {
                if models.is_array() {
                    return collect_ids(models, out);
                }
            }
            if let Some(Value::String(s)) = obj.get("id") {
                out.push(s.clone());
            }
        }
        _ => {}
    }
}

/// 去重（保留首次出现顺序）+ trim + 去空串。
fn dedupe_preserve_order(v: &mut Vec<String>) {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut i = 0;
    while i < v.len() {
        let s = v[i].trim().to_string();
        if s.is_empty() || !seen.insert(s.clone()) {
            v.remove(i);
        } else {
            v[i] = s;
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_openai_data_array() {
        let p = serde_json::json!({
            "data": [{"id": "gpt-4o"}, {"id": "gpt-4o-mini"}]
        });
        assert_eq!(extract_model_ids(&p), vec!["gpt-4o", "gpt-4o-mini"]);
    }

    #[test]
    fn extract_top_level_string_array() {
        let p = serde_json::json!(["m1", "m2", "m1"]);
        assert_eq!(extract_model_ids(&p), vec!["m1", "m2"]);
    }

    #[test]
    fn extract_ollama_tags_with_name_field() {
        let p = serde_json::json!({
            "models": [{"name": "qwen2.5:32b"}, {"name": "llama3:8b"}]
        });
        assert_eq!(extract_model_ids(&p), vec!["qwen2.5:32b", "llama3:8b"]);
    }

    #[test]
    fn extract_object_with_id_preferred_over_name() {
        let p = serde_json::json!([{"id": "a", "name": "b"}]);
        assert_eq!(extract_model_ids(&p), vec!["a"]);
    }

    #[test]
    fn extract_single_object_with_id() {
        let p = serde_json::json!({"id": "solo"});
        assert_eq!(extract_model_ids(&p), vec!["solo"]);
    }

    #[test]
    fn extract_empty_or_non_object_returns_empty() {
        assert!(extract_model_ids(&serde_json::json!({})).is_empty());
        assert!(extract_model_ids(&serde_json::Value::Null).is_empty());
        assert!(extract_model_ids(&serde_json::json!(42)).is_empty());
        assert!(extract_model_ids(&serde_json::json!({"data": []})).is_empty());
    }

    #[test]
    fn extract_dedupes_and_trims() {
        let p = serde_json::json!(["  a  ", "a", "b", ""]);
        assert_eq!(extract_model_ids(&p), vec!["a", "b"]);
    }

    #[test]
    fn fetch_endpoints_anthropic_v1_first() {
        let eps = fetch_endpoints("anthropic", "https://api.anthropic.com/");
        assert_eq!(eps[0], "https://api.anthropic.com/v1/models");
        assert_eq!(eps[1], "https://api.anthropic.com/models");
    }

    #[test]
    fn fetch_endpoints_openai_models_first() {
        let eps = fetch_endpoints("openai", "https://api.openai.com/v1");
        assert_eq!(eps[0], "https://api.openai.com/v1/models");
        assert_eq!(eps[1], "https://api.openai.com/v1/v1/models");
    }

    #[test]
    fn fetch_endpoints_ollama_models_first() {
        let eps = fetch_endpoints("ollama", "http://localhost:11434/");
        assert_eq!(eps[0], "http://localhost:11434/models");
        assert_eq!(eps[1], "http://localhost:11434/v1/models");
    }

    #[test]
    fn fetch_headers_anthropic_includes_x_api_key_and_version() {
        let cfg = ProviderConfig {
            kind: "anthropic".into(),
            api_key: "sk-ant".into(),
            ..Default::default()
        };
        let h = fetch_headers(&cfg);
        assert_eq!(h.get("x-api-key").unwrap(), "sk-ant");
        assert_eq!(h.get("anthropic-version").unwrap(), ANTHROPIC_VERSION);
    }

    #[test]
    fn fetch_headers_openai_bearer_auth() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            api_key: "sk-x".into(),
            ..Default::default()
        };
        let h = fetch_headers(&cfg);
        assert_eq!(h.get("authorization").unwrap(), "Bearer sk-x");
    }

    #[test]
    fn fetch_headers_ollama_no_auth() {
        let cfg = ProviderConfig {
            kind: "ollama".into(),
            ..Default::default()
        };
        let h = fetch_headers(&cfg);
        assert!(h.get("authorization").is_none());
        assert!(h.get("x-api-key").is_none());
    }

    #[test]
    fn fetch_headers_openai_empty_key_skips_auth() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            api_key: String::new(),
            ..Default::default()
        };
        let h = fetch_headers(&cfg);
        assert!(h.get("authorization").is_none());
    }
}
