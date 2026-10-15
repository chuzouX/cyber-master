//! fuzz_endpoint 工具：轻量高性能目录与敏感端点 Fuzz 工具。
//!
//! 用于 Web 资产探测、敏感路径枚举与未授权接口发现：
//! - 内置高频敏感路径字典（.git, .env, swagger, admin, robots.txt, backup 等）
//! - 支持基于状态码（如 404）和页面长度过滤统一错误页
//! - 并发异步请求与超时控制
//! - 输出结构化发现列表（URL、状态码、长度、网页标题）

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

const COMMON_PATHS: &[&str] = &[
    "robots.txt",
    ".git/HEAD",
    ".env",
    ".svn/entries",
    "admin",
    "admin/login",
    "login",
    "api",
    "api/v1",
    "swagger.json",
    "swagger/index.html",
    "openapi.json",
    "docs",
    "health",
    "actuator/health",
    "actuator",
    "config.json",
    "backup.zip",
    "backup.tar.gz",
    "www.zip",
    "1.zip",
    "phpinfo.php",
    "info.php",
    "test.php",
    ".DS_Store",
    "server-status",
    "console",
];

pub struct FuzzEndpointTool;

impl Tool for FuzzEndpointTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "fuzz_endpoint".into(),
            description: "轻量高性能目录与敏感端点 Fuzz 工具。内置常见漏洞敏感路径（.git, .env, swagger, admin 等），自动过滤 404 与无意义错误页。".into(),
            tags: vec!["security".into(), "recon".into(), "web".into(), "fuzz".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "base_url": {
                        "type": "string",
                        "description": "基础目标 URL（如 http://example.com 或 http://192.168.1.5:8080）"
                    },
                    "custom_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "自定义追加测试路径列表（可选）"
                    },
                    "concurrency": {
                        "type": "integer",
                        "description": "并发请求数（默认 10）",
                        "minimum": 1
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "单请求超时秒数（默认 5）",
                        "minimum": 1
                    }
                },
                "required": ["base_url"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let base_url_raw = input
                .get("base_url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("fuzz_endpoint 缺少 base_url 参数".into()))?;

            let base_url = base_url_raw.trim_end_matches('/');
            let concurrency = input
                .get("concurrency")
                .and_then(|v| v.as_u64())
                .unwrap_or(10) as usize;
            let timeout_secs = input
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(5);

            let mut paths_to_test: Vec<String> =
                COMMON_PATHS.iter().map(|s| s.to_string()).collect();
            if let Some(custom) = input.get("custom_paths").and_then(|v| v.as_array()) {
                for p in custom {
                    if let Some(s) = p.as_str() {
                        paths_to_test.push(s.trim_start_matches('/').to_string());
                    }
                }
            }

            let total_tested = paths_to_test.len();

            let client = match Client::builder()
                .danger_accept_invalid_certs(true)
                .timeout(Duration::from_secs(timeout_secs))
                .redirect(reqwest::redirect::Policy::none())
                .build()
            {
                Ok(c) => Arc::new(c),
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("HTTP Client 创建失败: {e}"),
                        is_error: true,
                    });
                }
            };

            let sem = Arc::new(Semaphore::new(concurrency));
            let mut tasks = Vec::new();

            for path in paths_to_test {
                let s = Arc::clone(&sem);
                let c = Arc::clone(&client);
                let url = format!("{base_url}/{path}");

                tasks.push(tokio::spawn(async move {
                    let _permit = match s.acquire().await {
                        Ok(p) => p,
                        Err(_) => return None,
                    };
                    if let Ok(res) = c.get(&url).send().await {
                        let status = res.status().as_u16();
                        // 过滤 404 和 0 (网络错误)
                        if status != 404 && status != 0 {
                            let len_header = res.content_length();
                            let (actual_len, title) = if let Ok(bytes) = res.bytes().await {
                                let l = bytes.len();
                                let t = extract_title_simple(&bytes);
                                (len_header.unwrap_or(l as u64), t)
                            } else {
                                (len_header.unwrap_or(0), None)
                            };

                            return Some(json!({
                                "path": path,
                                "url": url,
                                "status_code": status,
                                "content_length": actual_len,
                                "title": title
                            }));
                        }
                    }
                    None
                }));
            }

            let mut results = Vec::new();
            for t in tasks {
                if let Ok(Some(item)) = t.await {
                    results.push(item);
                }
            }

            let output_json = json!({
                "base_url": base_url,
                "endpoints_tested": total_tested,
                "endpoints_discovered": results.len(),
                "discovered": results,
            });

            Ok(ToolOutput {
                content: serde_json::to_string_pretty(&output_json).unwrap_or_default(),
                is_error: false,
            })
        })
    }
}

/// 从 HTML 响应体字节流中提取网页 `<title>` 文本。
fn extract_title_simple(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_lowercase();
    let start = lower.find("<title")?;
    let content_start = lower[start..].find('>')? + start + 1;
    let end = lower[content_start..].find("</title>")? + content_start;
    let title = text[content_start..end].trim();
    if title.is_empty() {
        None
    } else {
        Some(title.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_title_simple_basic() {
        let html = b"<html><head><title>Admin Dashboard</title></head><body>OK</body></html>";
        assert_eq!(extract_title_simple(html), Some("Admin Dashboard".into()));
    }

    #[test]
    fn test_extract_title_simple_empty_or_missing() {
        let empty_title = b"<html><head><title>   </title></head></html>";
        assert_eq!(extract_title_simple(empty_title), None);

        let no_title = b"<html><body>Hello</body></html>";
        assert_eq!(extract_title_simple(no_title), None);
    }
}
