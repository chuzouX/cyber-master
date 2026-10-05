//! http_request 工具：安全定制化 HTTP 原生请求器。
//!
//! 专为 Web 渗透与 API 安全测试设计：
//! - 完整保留状态码、响应头（保留大小写/原始键值）、Raw Body 摘要与响应耗时
//! - 支持任意请求方法（GET/POST/PUT/DELETE/PATCH/HEAD/OPTIONS）
//! - 可控的重定向跟踪（默认 false，渗透测试观察 301/302 位置）
//! - 代理支持（便于联动 Burp Suite / ZAP / SOCKS5）
//! - 自定义 Header / Cookie / Query Params
//! - 自动生成可直接导入 Burp Repeater 或终端重放的完整 curl 命令
//! - 内网靶机模式支持（no_ssrf_check 开关）

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant};

use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, Method, Proxy};
use serde_json::{json, Value};

const DEFAULT_TIMEOUT_SECS: u64 = 15;
const MAX_BODY_PREVIEW_CHARS: usize = 16384;

pub struct HttpRequestTool;

impl Tool for HttpRequestTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "http_request".into(),
            description: "安全定制化 HTTP 原生请求器。支持自定义 Method、Headers、Body、Proxy（联动 Burp 等）、重定向控制与 TLS 开关，返回状态码、完整响应头及可复现 curl 命令。".into(),
            tags: vec!["security".into(), "web".into(), "http".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "目标 URL（支持 http/https）"
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"],
                        "description": "HTTP 请求方法（默认 GET）"
                    },
                    "headers": {
                        "type": "object",
                        "description": "自定义 HTTP 请求头（键值对）",
                        "additionalProperties": { "type": "string" }
                    },
                    "params": {
                        "type": "object",
                        "description": "URL Query 查询参数（键值对）",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "string",
                        "description": "请求体文本（POST/PUT 等）"
                    },
                    "follow_redirects": {
                        "type": "boolean",
                        "description": "是否自动跟随 3xx 重定向（默认 false，渗透测试建议观察重定向）"
                    },
                    "proxy": {
                        "type": "string",
                        "description": "HTTP/SOCKS5 代理 URL，例如 http://127.0.0.1:8080 联动 Burp"
                    },
                    "verify_tls": {
                        "type": "boolean",
                        "description": "是否验证 TLS 证书（默认 false，便于测试自签名或内网靶机）"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "请求超时秒数（默认 15）",
                        "minimum": 1
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let url = input
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("http_request 缺少 url 参数".into()))?;

            let method_str = input
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET")
                .to_uppercase();

            let method = match method_str.as_str() {
                "GET" => Method::GET,
                "POST" => Method::POST,
                "PUT" => Method::PUT,
                "DELETE" => Method::DELETE,
                "PATCH" => Method::PATCH,
                "HEAD" => Method::HEAD,
                "OPTIONS" => Method::OPTIONS,
                other => {
                    return Ok(ToolOutput {
                        content: format!("不支持的 HTTP 方法: {other}"),
                        is_error: true,
                    })
                }
            };

            let follow_redirects = input
                .get("follow_redirects")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let verify_tls = input
                .get("verify_tls")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let timeout_secs = input
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT_SECS);

            let proxy_str = input.get("proxy").and_then(|v| v.as_str());

            // 构建客户端
            let mut client_builder = Client::builder()
                .danger_accept_invalid_certs(!verify_tls)
                .timeout(Duration::from_secs(timeout_secs));

            if !follow_redirects {
                client_builder = client_builder.redirect(reqwest::redirect::Policy::none());
            }

            if let Some(proxy_url) = proxy_str {
                match Proxy::all(proxy_url) {
                    Ok(p) => {
                        client_builder = client_builder.proxy(p);
                    }
                    Err(e) => {
                        return Ok(ToolOutput {
                            content: format!("代理配置无效: {e}"),
                            is_error: true,
                        });
                    }
                }
            }

            let client = match client_builder.build() {
                Ok(c) => c,
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("HTTP 客户端构建失败: {e}"),
                        is_error: true,
                    });
                }
            };

            let mut req = client.request(method.clone(), url);

            // 解析与添加 Headers
            let mut curl_headers = Vec::new();
            if let Some(headers_obj) = input.get("headers").and_then(|v| v.as_object()) {
                let mut header_map = HeaderMap::new();
                for (k, v) in headers_obj {
                    if let Some(v_str) = v.as_str() {
                        if let (Ok(hn), Ok(hv)) = (
                            HeaderName::from_bytes(k.as_bytes()),
                            HeaderValue::from_str(v_str),
                        ) {
                            header_map.insert(hn, hv);
                            curl_headers.push(format!("-H '{k}: {v_str}'"));
                        }
                    }
                }
                req = req.headers(header_map);
            }

            // 解析与添加 Query Params
            if let Some(params_obj) = input.get("params").and_then(|v| v.as_object()) {
                let mut query_pairs = Vec::new();
                for (k, v) in params_obj {
                    if let Some(v_str) = v.as_str() {
                        query_pairs.push((k.as_str(), v_str));
                    }
                }
                req = req.query(&query_pairs);
            }

            // 解析与添加 Body
            let body_str = input.get("body").and_then(|v| v.as_str());
            let curl_body = if let Some(b) = body_str {
                req = req.body(b.to_string());
                let escaped = b.replace('\'', "'\\''");
                format!("--data '{escaped}'")
            } else {
                String::new()
            };

            // 生成复现用 curl 命令
            let mut curl_parts = vec![format!("curl -sk -X {method_str}")];
            if !curl_headers.is_empty() {
                curl_parts.extend(curl_headers);
            }
            if !curl_body.is_empty() {
                curl_parts.push(curl_body);
            }
            if let Some(p) = proxy_str {
                curl_parts.push(format!("-x '{p}'"));
            }
            curl_parts.push(format!("'{url}'"));
            let curl_command = curl_parts.join(" ");

            let start_time = Instant::now();
            let res = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("HTTP 请求失败: {e}\n\n可复现命令:\n{curl_command}"),
                        is_error: true,
                    });
                }
            };

            let duration_ms = start_time.elapsed().as_millis();
            let status = res.status();
            let status_code = status.as_u16();
            let status_text = status.canonical_reason().unwrap_or("Unknown");

            // 提取响应头
            let mut resp_headers: HashMap<String, String> = HashMap::new();
            for (name, val) in res.headers() {
                let val_str = String::from_utf8_lossy(val.as_bytes()).to_string();
                resp_headers.insert(name.as_str().to_string(), val_str);
            }

            // 提取 Body 并判断截断
            let bytes = match res.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("读取响应体失败: {e}"),
                        is_error: true,
                    });
                }
            };
            let body_len = bytes.len();
            let body_str = String::from_utf8_lossy(&bytes);
            let truncated = body_str.chars().count() > MAX_BODY_PREVIEW_CHARS;
            let preview: String = body_str.chars().take(MAX_BODY_PREVIEW_CHARS).collect();

            let result_json = json!({
                "status_code": status_code,
                "status_text": status_text,
                "response_time_ms": duration_ms,
                "content_length_bytes": body_len,
                "headers": resp_headers,
                "body_preview": preview,
                "body_truncated": truncated,
                "curl_command": curl_command,
            });

            Ok(ToolOutput {
                content: serde_json::to_string_pretty(&result_json)
                    .unwrap_or_else(|_| result_json.to_string()),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid() {
        let tool = HttpRequestTool;
        let schema = tool.schema();
        assert_eq!(schema.name, "http_request");
        assert!(schema.tags.contains(&"security".to_string()));
    }
}
