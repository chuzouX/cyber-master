//! read_file 工具：读取文本文件内容（分页读取，每页 64KB）。

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::resolve_under_cwd;

/// 每页最大字节数（64KB）。
const PAGE_BYTES: usize = 64 * 1024;

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "读取文本文件内容（相对工作目录或绝对路径，分页读取每页 64KB。offset 为字节偏移量，默认 0。文件超过一页时返回值会提示总大小和下一页 offset）".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径" },
                    "offset": {
                        "type": "integer",
                        "description": "字节偏移量（默认 0，从文件开头读取）。上次读取若被截断，返回值会给出 next_offset，传入即可读下一页",
                        "minimum": 0
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("read_file 缺少 path 参数".into()))?;
            let offset = input
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(0);
            let resolved = resolve_under_cwd(std::path::Path::new(path), &ctx.cwd)
                .map_err(AgentError::Provider)?;
            let bytes = std::fs::read(&resolved).map_err(|e| {
                AgentError::Provider(format!("读取 {} 失败: {e}", resolved.display()))
            })?;
            let total = bytes.len();
            // offset 超过文件末尾 → 空内容 + 提示
            if offset >= total {
                return Ok(ToolOutput {
                    content: format!("（offset {offset} 已超过文件末尾，文件总大小 {total} 字节）"),
                    is_error: false,
                });
            }
            let end = (offset + PAGE_BYTES).min(total);
            let slice = &bytes[offset..end];
            let mut content = String::from_utf8_lossy(slice).into_owned();
            let has_more = end < total;
            if has_more {
                content.push_str(&format!(
                    "\n\n[已截断：本页 offset={offset}-{end}，总大小 {total} 字节。读取下一页请传 offset={end}]"
                ));
            } else if offset > 0 {
                content.push_str(&format!(
                    "\n\n[最后一页：offset={offset}-{end}，总大小 {total} 字节]"
                ));
            }
            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(dir: &std::path::Path) -> ToolCtx {
        ToolCtx {
            cwd: dir.to_path_buf(),
            rules: vec![],
            scope: None,
        }
    }

    #[tokio::test]
    async fn reads_existing_file() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_basic");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("hello.txt");
        std::fs::write(&f, "你好世界").unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "hello.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "你好世界");
        assert!(!out.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let ctx = ctx_with(&std::env::temp_dir());
        let out = ReadFileTool
            .run(json!({"path": "nonexistent_xyz.txt"}), &ctx)
            .await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn small_file_no_truncation_hint() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_small");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("small.txt");
        std::fs::write(&f, "short content").unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "small.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "short content");
        assert!(!out.content.contains("截断"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn large_file_first_page_has_next_hint() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_large");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("large.bin");
        // 写 128KB（两页）
        let data = vec![b'A'; 128 * 1024];
        std::fs::write(&f, &data).unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "large.bin"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("截断"), "大文件第一页应提示截断");
        assert!(out.content.contains("offset=65536"), "应提示下一页 offset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn large_file_second_page_reads_rest() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_page2");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("large.bin");
        // 写 128KB（两页）
        let data = vec![b'B'; 128 * 1024];
        std::fs::write(&f, &data).unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "large.bin", "offset": 65536}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("最后一页"), "第二页应提示是最后一页");
        assert!(!out.content.contains("offset=131072"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offset_beyond_end_returns_empty() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_beyond");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("tiny.txt");
        std::fs::write(&f, "hi").unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "tiny.txt", "offset": 9999}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("超过文件末尾"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
