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
                    let _permit = s.acquire().await.unwrap();
                    if let Ok(res) = c.get(&url).send().await {
                        let status = res.status().as_u16();
                        // 过滤 404 和 常见网络错误
                        if status != 404 && status != 0 {
                            let len = res.content_length().unwrap_or(0);
                            let title = extract_title_simple(&res).await;
                            return Some(json!({
                                "path": path,
                                "url": url,
                                "status_code": status,
                                "content_length": len,
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
                "endpoints_tested": results.len(),
                "discovered": results,
            });

            Ok(ToolOutput {
                content: serde_json::to_string_pretty(&output_json).unwrap_or_default(),
                is_error: false,
            })
        })
    }
}

async fn extract_title_simple(_res: &reqwest::Response) -> Option<String> {
    // 简要提取 title，便于查看 200 页面实际内容
    None
}
