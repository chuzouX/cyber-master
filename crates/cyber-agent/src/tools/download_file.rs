//! download_file 工具：下载文件到本地（支持单文件与批量下载 / 代理 / 自定义 headers / 跳过 SSL 验证）。
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
//!
//! 批量模式支持：
//! - `urls: [...]` 搭配 `output: "dir/"`（自动按 URL 文件名保存）
//! - `items: [{ url: "...", output: "..." }]`（支持独立指定目标文件名）
//! - 并发下载与可控并发数 `concurrency`

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::join_all;
use serde_json::{json, Value};
use tokio::sync::Semaphore;

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::{check_ssrf, check_write_path, resolve_under_cwd};

const DEFAULT_TIMEOUT: u64 = 300;
const MAX_REDIRECTS: usize = 10;
const DEFAULT_CONCURRENCY: usize = 5;

#[derive(Debug, Clone)]
struct DownloadTask {
    url: String,
    output: Option<String>,
}

#[derive(Debug, Clone)]
struct ResolvedDownloadTarget {
    url: String,
    output_path: PathBuf,
}

#[derive(Debug)]
struct DownloadSingleSuccess {
    url: String,
    output_path: PathBuf,
    size: usize,
    expected_size: Option<u64>,
    elapsed: Duration,
    already_existed: bool,
}

#[derive(Debug)]
enum DownloadSingleOutcome {
    Success(DownloadSingleSuccess),
    Failure {
        url: String,
        output_path: Option<PathBuf>,
        error: String,
    },
}

pub struct DownloadFileTool;

impl Tool for DownloadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "download_file".into(),
            description: "下载文件到本地，支持单文件及批量并发下载（支持 HTTP/SOCKS5 代理、自定义 headers、跳过 SSL 验证、并发控制）。默认带 SSRF 保护，下载内网资源时设 no_ssrf_check=true。".into(),
            tags: vec![],
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "单文件下载 URL（仅支持 http/https）"
                    },
                    "urls": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "批量下载 URL 列表。搭配 output 目录使用（文件名自动从各 URL 中提取）"
                    },
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "url": { "type": "string", "description": "下载 URL" },
                                "output": { "type": "string", "description": "保存路径（可选，未指定时保存到全局 output 目录）" }
                            },
                            "required": ["url"]
                        },
                        "description": "批量下载任务项列表（支持为每个文件单独指定 output 保存路径）"
                    },
                    "output": {
                        "type": "string",
                        "description": "保存路径（相对工作目录或绝对路径）。单文件时为文件名或目录；批量下载时为目标保存目录"
                    },
                    "concurrency": {
                        "type": "integer",
                        "description": "批量下载并发数（默认 5，范围 1-20）",
                        "minimum": 1,
                        "maximum": 20
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
                }
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let tasks = parse_download_tasks(&input)?;
            let top_output = input.get("output").and_then(Value::as_str).map(str::trim);
            let targets = resolve_targets(tasks, top_output, &ctx.cwd)?;

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
            let concurrency = input
                .get("concurrency")
                .and_then(|v| v.as_u64())
                .unwrap_or(DEFAULT_CONCURRENCY as u64)
                .clamp(1, 20) as usize;

            let header_map = if let Some(headers) = input.get("headers").and_then(|v| v.as_object())
            {
                let mut map = reqwest::header::HeaderMap::new();
                for (k, v) in headers {
                    if let Some(val_str) = v.as_str() {
                        let header_name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                            .map_err(|e| AgentError::Provider(format!("无效 header 名: {e}")))?;
                        let header_val = reqwest::header::HeaderValue::from_str(val_str)
                            .map_err(|e| AgentError::Provider(format!("无效 header 值: {e}")))?;
                        map.insert(header_name, header_val);
                    }
                }
                Some(map)
            } else {
                None
            };

            let mut client_builder = reqwest::Client::builder()
                .timeout(Duration::from_secs(timeout_secs))
                .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                .user_agent("CyberMaster/0.1");

            if no_ssl_verify {
                client_builder = client_builder.danger_accept_invalid_certs(true);
            }

            if let Some(proxy_url) = proxy {
                let proxy = reqwest::Proxy::all(proxy_url)
                    .map_err(|e| AgentError::Provider(format!("代理配置失败: {e}")))?;
                client_builder = client_builder.proxy(proxy);
            }

            let client = client_builder.build()?;

            if targets.len() == 1 {
                let target = &targets[0];
                let outcome = download_one_file(
                    &client,
                    &target.url,
                    target.output_path.clone(),
                    header_map.as_ref(),
                    no_ssrf_check,
                    overwrite,
                    ctx,
                )
                .await;

                match outcome {
                    DownloadSingleOutcome::Success(s) => {
                        if s.already_existed {
                            return Ok(ToolOutput {
                                content: format!(
                                    "文件已存在且 overwrite=false：{}",
                                    s.output_path.display()
                                ),
                                is_error: false,
                            });
                        }
                        let size_str = format_size(s.size);
                        let mut content = format!(
                            "已下载: {}\n大小: {} ({} bytes)\n耗时: {:.1}s\n来源: {}",
                            s.output_path.display(),
                            size_str,
                            s.size,
                            s.elapsed.as_secs_f64(),
                            s.url
                        );
                        if let Some(expected) = s.expected_size {
                            if expected != s.size as u64 {
                                content.push_str(&format!(
                                    "\n警告: 期望 {} bytes，实际 {} bytes",
                                    expected, s.size
                                ));
                            }
                        }
                        Ok(ToolOutput {
                            content,
                            is_error: false,
                        })
                    }
                    DownloadSingleOutcome::Failure { error, .. } => Ok(ToolOutput {
                        content: error,
                        is_error: true,
                    }),
                }
            } else {
                let batch_start = Instant::now();
                let semaphore = Arc::new(Semaphore::new(concurrency));
                let mut futs = Vec::with_capacity(targets.len());

                for target in targets {
                    let sem = semaphore.clone();
                    let client_ref = client.clone();
                    let headers_ref = header_map.clone();
                    let ctx_clone = ctx.clone();

                    futs.push(async move {
                        let _permit = match sem.acquire().await {
                            Ok(p) => p,
                            Err(e) => {
                                return DownloadSingleOutcome::Failure {
                                    url: target.url,
                                    output_path: Some(target.output_path),
                                    error: format!("信号量获取失败: {e}"),
                                };
                            }
                        };
                        download_one_file(
                            &client_ref,
                            &target.url,
                            target.output_path,
                            headers_ref.as_ref(),
                            no_ssrf_check,
                            overwrite,
                            &ctx_clone,
                        )
                        .await
                    });
                }

                let outcomes = join_all(futs).await;
                let batch_elapsed = batch_start.elapsed();

                let total_count = outcomes.len();
                let mut success_count = 0;
                let mut fail_count = 0;
                let mut total_bytes = 0usize;
                let mut lines = Vec::new();

                for outcome in &outcomes {
                    match outcome {
                        DownloadSingleOutcome::Success(s) => {
                            success_count += 1;
                            total_bytes += s.size;
                            let size_str = format_size(s.size);
                            if s.already_existed {
                                lines.push(format!(
                                    "  [已存在] {} (跳过) <- {}",
                                    s.output_path.display(),
                                    s.url
                                ));
                            } else {
                                let mut line = format!(
                                    "  [成功] {} ({}, {:.1}s) <- {}",
                                    s.output_path.display(),
                                    size_str,
                                    s.elapsed.as_secs_f64(),
                                    s.url
                                );
                                if let Some(exp) = s.expected_size {
                                    if exp != s.size as u64 {
                                        line.push_str(&format!(
                                            " (期望 {} 字节，实际 {} 字节)",
                                            exp, s.size
                                        ));
                                    }
                                }
                                lines.push(line);
                            }
                        }
                        DownloadSingleOutcome::Failure {
                            url,
                            output_path,
                            error,
                        } => {
                            fail_count += 1;
                            let target_desc = output_path
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_else(|| "-".to_string());
                            lines.push(format!("  [失败] {target_desc} <- {url} (原因: {error})"));
                        }
                    }
                }

                let total_size_str = format_size(total_bytes);
                let summary = format!(
                    "批量下载完成：共 {total_count} 个任务，成功 {success_count} 个，失败 {fail_count} 个，总计 {total_size_str}，总耗时 {:.1}s",
                    batch_elapsed.as_secs_f64()
                );

                let content = format!("{}\n\n下载详情：\n{}", summary, lines.join("\n"));
                let is_error = success_count == 0 && total_count > 0;

                Ok(ToolOutput { content, is_error })
            }
        })
    }
}

/// 解析单文件或批量任务参数。
fn parse_download_tasks(input: &Value) -> Result<Vec<DownloadTask>> {
    let mut tasks = Vec::new();

    if let Some(items) = input.get("items").and_then(Value::as_array) {
        if items.is_empty() {
            return Err(AgentError::Provider(
                "download_file items 参数不能为空数组".into(),
            ));
        }
        for (idx, item) in items.iter().enumerate() {
            let url = item
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AgentError::Provider(format!("download_file items[{idx}] 缺少 url 参数"))
                })?;
            let output = item
                .get("output")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string);
            tasks.push(DownloadTask {
                url: url.to_string(),
                output,
            });
        }
    } else if let Some(urls) = input.get("urls").and_then(Value::as_array) {
        if urls.is_empty() {
            return Err(AgentError::Provider(
                "download_file urls 参数不能为空数组".into(),
            ));
        }
        for (idx, u) in urls.iter().enumerate() {
            let url = u
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    AgentError::Provider(format!("download_file urls[{idx}] 必须为非空字符串 URL"))
                })?;
            tasks.push(DownloadTask {
                url: url.to_string(),
                output: None,
            });
        }
    } else if let Some(url) = input
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        tasks.push(DownloadTask {
            url: url.to_string(),
            output: input
                .get("output")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(ToString::to_string),
        });
    } else {
        return Err(AgentError::Provider(
            "download_file 缺少下载目标：请提供 url（单文件）、urls（批量 URL 列表）或 items（批量任务项）".into(),
        ));
    }

    Ok(tasks)
}

/// 解析全部任务的目标路径并处理批量情况下的重名冲突。
fn resolve_targets(
    tasks: Vec<DownloadTask>,
    top_output: Option<&str>,
    cwd: &Path,
) -> Result<Vec<ResolvedDownloadTarget>> {
    let is_batch = tasks.len() > 1;
    let mut targets = Vec::with_capacity(tasks.len());

    for task in tasks {
        let output_path = match &task.output {
            Some(o) => resolve_output_path(o, &task.url, cwd)?,
            None => {
                let base = top_output.ok_or_else(|| {
                    if is_batch {
                        AgentError::Provider(
                            "批量下载时，必须在全局提供 output 目录，或在每个 item 中指定 output 路径"
                                .into(),
                        )
                    } else {
                        AgentError::Provider("download_file 缺少 output 参数".into())
                    }
                })?;
                if is_batch {
                    resolve_batch_output_path(base, &task.url, cwd)?
                } else {
                    resolve_output_path(base, &task.url, cwd)?
                }
            }
        };

        targets.push(ResolvedDownloadTarget {
            url: task.url,
            output_path,
        });
    }

    // 在批量模式下，避免多个同名文件冲突覆盖（自动追加 _1, _2 后缀）
    if is_batch {
        let mut seen: std::collections::HashMap<PathBuf, usize> = std::collections::HashMap::new();
        for target in &mut targets {
            let count = seen.entry(target.output_path.clone()).or_insert(0);
            if *count > 0 {
                let stem = target
                    .output_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("file");
                let ext = target
                    .output_path
                    .extension()
                    .and_then(|s| s.to_str())
                    .map(|e| format!(".{e}"))
                    .unwrap_or_default();
                let new_name = format!("{stem}_{count}{ext}");
                if let Some(parent) = target.output_path.parent() {
                    target.output_path = parent.join(new_name);
                }
            }
            *count += 1;
        }
    }

    Ok(targets)
}

/// 格式化文件大小。
fn format_size(size: usize) -> String {
    if size >= 1024 * 1024 {
        format!("{:.2} MB", size as f64 / (1024.0 * 1024.0))
    } else if size >= 1024 {
        format!("{:.2} KB", size as f64 / 1024.0)
    } else {
        format!("{} B", size)
    }
}

/// 下载单条文件任务并捕获执行结果。
async fn download_one_file(
    client: &reqwest::Client,
    url: &str,
    output_path: PathBuf,
    header_map: Option<&reqwest::header::HeaderMap>,
    no_ssrf_check: bool,
    overwrite: bool,
    ctx: &ToolCtx,
) -> DownloadSingleOutcome {
    if !no_ssrf_check {
        if let Err(e) = check_ssrf(url) {
            return DownloadSingleOutcome::Failure {
                url: url.to_string(),
                output_path: Some(output_path),
                error: format!("SSRF 拦截: {e}"),
            };
        }
    }

    if let Err(e) = check_write_path(&output_path, ctx) {
        return DownloadSingleOutcome::Failure {
            url: url.to_string(),
            output_path: Some(output_path),
            error: format!("写路径安全检查失败: {e}"),
        };
    }

    if output_path.exists() && !overwrite {
        return DownloadSingleOutcome::Success(DownloadSingleSuccess {
            url: url.to_string(),
            output_path,
            size: 0,
            expected_size: None,
            elapsed: Duration::from_secs(0),
            already_existed: true,
        });
    }

    if let Some(parent) = output_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return DownloadSingleOutcome::Failure {
                url: url.to_string(),
                output_path: Some(output_path),
                error: format!("创建目录失败: {e}"),
            };
        }
    }

    let mut req = client.get(url);
    if let Some(map) = header_map {
        req = req.headers(map.clone());
    }

    let start = Instant::now();
    let response = match req.send().await {
        Ok(res) => res,
        Err(e) => {
            return DownloadSingleOutcome::Failure {
                url: url.to_string(),
                output_path: Some(output_path),
                error: format!("请求失败: {e}"),
            };
        }
    };

    if !response.status().is_success() {
        return DownloadSingleOutcome::Failure {
            url: url.to_string(),
            output_path: Some(output_path),
            error: format!("HTTP {} - 下载失败: {}", response.status(), url),
        };
    }

    let total_size = response.content_length();
    let body = match response.bytes().await {
        Ok(b) => b,
        Err(e) => {
            return DownloadSingleOutcome::Failure {
                url: url.to_string(),
                output_path: Some(output_path),
                error: format!("读取响应流失败: {e}"),
            };
        }
    };
    let elapsed = start.elapsed();

    if let Err(e) = std::fs::write(&output_path, &body) {
        return DownloadSingleOutcome::Failure {
            url: url.to_string(),
            output_path: Some(output_path),
            error: format!("写入文件失败: {e}"),
        };
    }

    DownloadSingleOutcome::Success(DownloadSingleSuccess {
        url: url.to_string(),
        output_path,
        size: body.len(),
        expected_size: total_size,
        elapsed,
        already_existed: false,
    })
}

/// 解析单文件输出路径：如果是目录，从 URL 提取文件名。
fn resolve_output_path(output: &str, url: &str, cwd: &Path) -> Result<PathBuf> {
    let resolved = resolve_under_cwd(Path::new(output), cwd).map_err(AgentError::Provider)?;

    // 如果路径以 / 或 \ 结尾，或者已存在且是目录，从 URL 提取文件名
    let needs_filename =
        output.ends_with('/') || output.ends_with('\\') || (resolved.exists() && resolved.is_dir());

    if needs_filename {
        let filename = extract_filename_from_url(url)
            .ok_or_else(|| AgentError::Provider(format!("无法从 URL 提取文件名: {url}")))?;
        Ok(resolved.join(filename))
    } else {
        Ok(resolved)
    }
}

/// 批量下载时解析输出路径：将 output 视为目标目录，并在其下加入从 URL 提取的文件名。
fn resolve_batch_output_path(output: &str, url: &str, cwd: &Path) -> Result<PathBuf> {
    let resolved = resolve_under_cwd(Path::new(output), cwd).map_err(AgentError::Provider)?;
    let filename = extract_filename_from_url(url)
        .ok_or_else(|| AgentError::Provider(format!("无法从 URL 提取文件名: {url}")))?;
    Ok(resolved.join(filename))
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
        assert_eq!(extract_filename_from_url("https://example.com/dir/"), None);
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
        let path = resolve_output_path("downloads/", "https://x.com/archive.tar.gz", cwd).unwrap();
        assert_eq!(
            path,
            std::path::PathBuf::from("/tmp/downloads/archive.tar.gz")
        );
    }

    #[test]
    fn urlencoding_decode_basic() {
        assert_eq!(urlencoding_decode("hello%20world"), "hello world");
        assert_eq!(urlencoding_decode("100%25"), "100%");
        assert_eq!(urlencoding_decode("no_encoding"), "no_encoding");
    }

    #[test]
    fn parse_download_tasks_modes() {
        // 单文件
        let single = parse_download_tasks(&json!({
            "url": "https://example.com/1.txt",
            "output": "1.txt"
        }))
        .unwrap();
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].url, "https://example.com/1.txt");
        assert_eq!(single[0].output.as_deref(), Some("1.txt"));

        // urls 数组
        let urls = parse_download_tasks(&json!({
            "urls": ["https://example.com/1.txt", "https://example.com/2.txt"],
            "output": "dir"
        }))
        .unwrap();
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].url, "https://example.com/1.txt");
        assert_eq!(urls[1].url, "https://example.com/2.txt");

        // items 数组
        let items = parse_download_tasks(&json!({
            "items": [
                {"url": "https://example.com/1.txt", "output": "a.txt"},
                {"url": "https://example.com/2.txt"}
            ]
        }))
        .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].output.as_deref(), Some("a.txt"));
        assert_eq!(items[1].output, None);

        // 缺参数报错
        assert!(parse_download_tasks(&json!({})).is_err());
        assert!(parse_download_tasks(&json!({"urls": []})).is_err());
    }

    #[test]
    fn resolve_targets_dedupes_same_filename_in_batch() {
        let tasks = vec![
            DownloadTask {
                url: "https://site-a.com/file.zip".into(),
                output: None,
            },
            DownloadTask {
                url: "https://site-b.com/file.zip".into(),
                output: None,
            },
        ];
        let cwd = std::path::Path::new("/tmp");
        let targets = resolve_targets(tasks, Some("dist"), cwd).unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(
            targets[0].output_path,
            std::path::PathBuf::from("/tmp/dist/file.zip")
        );
        assert_eq!(
            targets[1].output_path,
            std::path::PathBuf::from("/tmp/dist/file_1.zip")
        );
    }

    #[tokio::test]
    async fn test_batch_download_mock_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // 简易测试 HTTP 响应服务
        tokio::spawn(async move {
            for _ in 0..2 {
                if let Ok((mut socket, _)) = listener.accept().await {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let body = "hello world payload";
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                }
            }
        });

        let temp_dir = std::env::temp_dir().join(format!("cyber_dl_test_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&temp_dir);
        let ctx = ToolCtx::new(temp_dir.clone(), Vec::new(), None, Vec::new());

        let tool = DownloadFileTool;
        let res = tool
            .run(
                json!({
                    "urls": [
                        format!("http://127.0.0.1:{port}/file1.txt"),
                        format!("http://127.0.0.1:{port}/file2.txt")
                    ],
                    "output": "batch_dir",
                    "no_ssrf_check": true,
                    "concurrency": 2
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        assert!(res.content.contains("批量下载完成：共 2 个任务，成功 2 个"));
        assert!(temp_dir.join("batch_dir").join("file1.txt").exists());
        assert!(temp_dir.join("batch_dir").join("file2.txt").exists());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
