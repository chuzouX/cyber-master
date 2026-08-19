//! download_file 工具：下载文件到本地（支持代理 / 自定义 headers / 跳过 SSL 验证）。
//!
//! 安全措施：
//! - 只允许 http/https 协议
//! - SSRF 保护（默认开启，可通过 no_ssrf_check 跳过）
//! - 写路径限制在工作目录内
//! - 超时控制（默认 300s，大文件可调大）
//! - 最多 10 次重定向
//!
//! 代理支持：
//! - HTTP 代理：`http://proxy-host:port`
//! - HTTPS 代理：`https://proxy-host:port`
//! - SOCKS5 代理：`socks5://proxy-host:port` 或 `socks5h://proxy-host:port`

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::{check_ssrf, check_write_path, resolve_under_cwd};

const DEFAULT_TIMEOUT: u64 = 300;
const MAX_REDIRECTS: usize = 10;

pub struct DownloadFileTool;

impl Tool for DownloadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "download_file".into(),
            description: "下载文件到本地（支持 HTTP/SOCKS5 代理、自定义 headers、跳过 SSL 验证）。默认带 SSRF 保护，下载内网资源时设 no_ssrf_check=true。".into(),
            tags: vec![],
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "下载 URL（仅支持 http/https）"
                    },
                    "output": {
                        "type": "string",
                        "description": "保存路径（相对工作目录或绝对路径）。如果是目录，从 URL 自动提取文件名"
                    },
                    "proxy": {
                        "type": "string",
                        "description": "代理 URL，如 http://127.0.0.1:8080 或 socks5://127.0.0.1:1080"
                    },
                    "headers": {
                        "type": "object",
                        "description": "自定义 HTTP 请求头（键值对）",
                        "additionalProperties": { "type": "string" }
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "超时秒数（默认 300）",
                        "minimum": 1
                    },
                    "no_ssl_verify": {
                        "type": "boolean",
                        "description": "跳过 SSL 证书验证（默认 false）"
                    },
                    "no_ssrf_check": {
                        "type": "boolean",
                        "description": "跳过 SSRF 检查（默认 false）。下载内网资源时设 true"
                    },
                    "overwrite": {
                        "type": "boolean",
                        "description": "文件已存在时是否覆盖（默认 true）"
                    }
                },
                "required": ["url", "output"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let url = input
                .get("url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("download_file 缺少 url 参数".into()))?;
            let output = input
                .get("output")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("download_file 缺少 output 参数".into()))?;
            let proxy = input.get("proxy").and_then(|v| v.as_str());
            let timeout_secs = input
                .get("timeout_secs")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_TIMEOUT);
            let no_ssl_verify = input
                .get("no_ssl_verify")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let no_ssrf_check = input
                .get("no_ssrf_check")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let overwrite = input
                .get("overwrite")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            // SSRF 检查（除非显式跳过）
            if !no_ssrf_check {
                check_ssrf(url)?;
            }

            // 解析输出路径
            let output_path = resolve_output_path(output, url, &ctx.cwd)?;

            // 写路径安全检查（不允许写到 cwd 之外）
            check_write_path(&output_path, ctx).map_err(AgentError::Provider)?;

            // 检查文件是否已存在
            if output_path.exists() && !overwrite {
                return Ok(ToolOutput {
                    content: format!(
                        "文件已存在且 overwrite=false：{}",
                        output_path.display()
                    ),
                    is_error: false,
                });
            }

            // 创建父目录
            if let Some(parent) = output_path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    AgentError::Provider(format!("创建目录失败: {e}"))
                })?;
            }

            // 构建 HTTP 客户端
            let mut client_builder = reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .user_agent("CyberMaster/0.1");

            if no_ssl_verify {
                client_builder = client_builder.danger_accept_invalid_certs(true);
            }

            if let Some(proxy_url) = proxy {
                let proxy = reqwest::Proxy::all(proxy_url).map_err(|e| {
                    AgentError::Provider(format!("代理配置失败: {e}"))
                })?;
                client_builder = client_builder.proxy(proxy);
            }

            let client = client_builder.build()?;

            // 构建请求
            let mut req = client.get(url);
            if let Some(headers) = input.get("headers").and_then(|v| v.as_object()) {
                let mut header_map = reqwest::header::HeaderMap::new();
                for (k, v) in headers {
                    if let Some(val_str) = v.as_str() {
                        let header_name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                            .map_err(|e| AgentError::Provider(format!("无效 header 名: {e}")))?;
                        let header_val = reqwest::header::HeaderValue::from_str(val_str)
                            .map_err(|e| AgentError::Provider(format!("无效 header 值: {e}")))?;
                        header_map.insert(header_name, header_val);
                    }
                }
                req = req.headers(header_map);
            }

            // 发送请求
            let start = Instant::now();
            let response = req.send().await?;

            if !response.status().is_success() {
                return Ok(ToolOutput {
                    content: format!("HTTP {} - 下载失败: {}", response.status(), url),
                    is_error: true,
                });
            }

            let total_size = response.content_length();
            let body = response.bytes().await?;
            let elapsed = start.elapsed();

            // 写入文件
            std::fs::write(&output_path, &body).map_err(|e| {
                AgentError::Provider(format!("写入文件失败: {e}"))
            })?;

            let size = body.len();
            let size_str = if size >= 1024 * 1024 {
                format!("{:.2} MB", size as f64 / (1024.0 * 1024.0))
            } else if size >= 1024 {
                format!("{:.2} KB", size as f64 / 1024.0)
            } else {
                format!("{} B", size)
            };

            let mut content = format!(
                "已下载: {}\n大小: {} ({} bytes)\n耗时: {:.1}s\n来源: {}",
                output_path.display(),
                size_str,
                size,
                elapsed.as_secs_f64(),
                url
            );
            if let Some(expected) = total_size {
                if expected != size as u64 {
                    content.push_str(&format!(
                        "\n警告: 期望 {} bytes，实际 {} bytes",
                        expected, size
                    ));
                }
            }

            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

/// 解析输出路径：如果是目录，从 URL 提取文件名。
fn resolve_output_path(output: &str, url: &str, cwd: &std::path::Path) -> Result<PathBuf> {
    let resolved = resolve_under_cwd(std::path::Path::new(output), cwd)
        .map_err(AgentError::Provider)?;

    // 如果路径以 / 或 \ 结尾，或者已存在且是目录，从 URL 提取文件名
    let needs_filename = output.ends_with('/')
        || output.ends_with('\\')
        || (resolved.exists() && resolved.is_dir());

    if needs_filename {
        let filename = extract_filename_from_url(url)
            .ok_or_else(|| AgentError::Provider(format!("无法从 URL 提取文件名: {url}")))?;
        Ok(resolved.join(filename))
    } else {
        Ok(resolved)
    }
}

/// 从 URL 提取文件名（取路径最后一段，去掉 query string 和 fragment）。
fn extract_filename_from_url(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url).ok()?;
    let path = parsed.path();
    let last_segment = path.rsplit('/').next()?;
    if last_segment.is_empty() {
        None
    } else {
        // URL decode
        let decoded = urlencoding_decode(last_segment);
        Some(decoded)
    }
}

/// 简单 URL 解码（处理 %XX，正确处理多字节 UTF-8）。
fn urlencoding_decode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            let h1 = chars.next();
            let h2 = chars.next();
            if let (Some(h1), Some(h2)) = (h1, h2) {
                if let Ok(byte) = u8::from_str_radix(&format!("{h1}{h2}"), 16) {
                    bytes.push(byte);
                    continue;
                }
                bytes.extend_from_slice(format!("%{h1}{h2}").as_bytes());
            } else {
                bytes.push(b'%');
            }
        } else {
            // 把字符按 UTF-8 字节追加
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_filename_simple() {
        assert_eq!(
            extract_filename_from_url("https://example.com/file.txt"),
            Some("file.txt".into())
        );
    }

    #[test]
    fn extract_filename_with_query() {
        assert_eq!(
            extract_filename_from_url("https://example.com/path/to/archive.zip?token=abc"),
            Some("archive.zip".into())
        );
    }

    #[test]
    fn extract_filename_url_encoded() {
        assert_eq!(
            extract_filename_from_url("https://example.com/%E4%B8%AD%E6%96%87.txt"),
            Some("中文.txt".into())
        );
    }

    #[test]
    fn extract_filename_no_path() {
        assert_eq!(extract_filename_from_url("https://example.com"), None);
    }

    #[test]
    fn extract_filename_trailing_slash() {
        assert_eq!(
            extract_filename_from_url("https://example.com/dir/"),
            None
        );
    }

    #[test]
    fn resolve_output_path_explicit_file() {
        let cwd = std::path::Path::new("/tmp");
        let path = resolve_output_path("download.zip", "https://x.com/a.zip", cwd).unwrap();
        assert_eq!(path, std::path::PathBuf::from("/tmp/download.zip"));
    }

    #[test]
    fn resolve_output_path_directory_appends_filename() {
        let cwd = std::path::Path::new("/tmp");
        let path =
            resolve_output_path("downloads/", "https://x.com/archive.tar.gz", cwd).unwrap();
        assert_eq!(path, std::path::PathBuf::from("/tmp/downloads/archive.tar.gz"));
    }

    #[test]
    fn urlencoding_decode_basic() {
        assert_eq!(urlencoding_decode("hello%20world"), "hello world");
        assert_eq!(urlencoding_decode("100%25"), "100%");
        assert_eq!(urlencoding_decode("no_encoding"), "no_encoding");
    }
}
