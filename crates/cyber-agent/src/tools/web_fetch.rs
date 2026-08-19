//! web_fetch 工具：抓取 URL 内容并转为纯文本（带 SSRF 保护）。
//!
//! 安全措施：
//! - 只允许 http/https 协议
//! - SSRF 保护：禁止私有 IP / loopback / link-local（DNS 解析后检查所有 IP）
//! - 30s 超时 + 5MB 响应体限制 + 最多 5 次重定向
//! - HTML→纯文本转换（html2text）
//! - 内容截断（默认 50000 字符，UTF-8 安全）

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::check_ssrf;
const DEFAULT_MAX_LENGTH: usize = 50_000;
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024; // 5MB
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 5;

pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "web_fetch".into(),
            description: "抓取指定 URL 的网页内容并转为纯文本返回（带 SSRF 保护，禁止访问内网地址）。适合读取文档、API 响应、漏洞说明等网页内容。".into(),
            tags: vec![],
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "要抓取的网页 URL（仅支持 http/https）"
                    },
                    "max_length": {
                        "type": "integer",
                        "description": "返回内容的最大字符数（默认 50000）"
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
                .ok_or_else(|| AgentError::Provider("缺少 url 参数".into()))?;

            let max_length = input
                .get("max_length")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_MAX_LENGTH);

            // SSRF 检查（解析 URL + DNS + 私有 IP 判断）
            check_ssrf(url)?;

            // 构建客户端：限制超时 + 重定向次数
            let client = reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .user_agent("CyberMaster/0.1")
                .build()?;

            let response = client.get(url).send().await?;

            if !response.status().is_success() {
                return Ok(ToolOutput {
                    content: format!("HTTP {} - 请求失败: {}", response.status(), url),
                    is_error: true,
                });
            }

            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("text/html")
                .to_string();

            // 读取响应体（限制大小）
            let body = response.bytes().await?;
            if body.len() > MAX_RESPONSE_BYTES {
                return Ok(ToolOutput {
                    content: format!(
                        "响应体过大（{} bytes > {} bytes 上限）",
                        body.len(),
                        MAX_RESPONSE_BYTES
                    ),
                    is_error: true,
                });
            }

            // 提取 <title>
            let title = extract_title(&body);

            // HTML → 纯文本（非 HTML 直接返回原文）
            let text = if content_type.contains("text/html") {
                html2text::from_read(&body[..], 120)
                    .unwrap_or_else(|_| String::from_utf8_lossy(&body).to_string())
            } else if content_type.contains("application/json") || content_type.contains("text/") {
                String::from_utf8_lossy(&body).to_string()
            } else {
                format!("（非文本内容类型: {}，{} bytes）", content_type, body.len())
            };

            // 安全截断（按字符，不截断 UTF-8 中间）
            let char_count = text.chars().count();
            let truncated = char_count > max_length;
            let content = if truncated {
                let mut s: String = text.chars().take(max_length).collect();
                s.push_str(&format!("\n\n[内容已截断，原始长度 {} 字符]", char_count));
                s
            } else {
                text
            };

            // 组装输出
            let mut output = String::new();
            output.push_str(&format!("URL: {}\n", url));
            if let Some(t) = &title {
                output.push_str(&format!("标题: {}\n", t));
            }
            output.push_str(&format!("内容类型: {}\n", content_type));
            output.push_str(&format!(
                "内容长度: {} 字符{}\n\n",
                content.chars().count(),
                if truncated { " (已截断)" } else { "" }
            ));
            output.push_str(&content);

            Ok(ToolOutput {
                content: output,
                is_error: false,
            })
        })
    }
}

/// 从 HTML 中提取 `<title>` 标签内容。
fn extract_title(body: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(body);
    let lower = text.to_lowercase();
    let start = lower.find("<title")?;
    let content_start = lower[start..].find('>')? + start + 1;
    let end = lower[content_start..].find("</title>")? + content_start;
    Some(text[content_start..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_title_basic() {
        let html = b"<html><head><title>Test Page</title></head><body>Hello</body></html>";
        assert_eq!(extract_title(html), Some("Test Page".into()));
    }

    #[test]
    fn extract_title_with_attributes() {
        let html = b"<html><head><title id=\"1\">My Title</title></head></html>";
        assert_eq!(extract_title(html), Some("My Title".into()));
    }

    #[test]
    fn extract_title_missing() {
        let html = b"<html><body>No title</body></html>";
        assert_eq!(extract_title(html), None);
    }

    #[test]
    fn extract_title_chinese() {
        let html = "<html><head><title>中文标题</title></head></html>".as_bytes();
        assert_eq!(extract_title(html), Some("中文标题".into()));
    }
}
