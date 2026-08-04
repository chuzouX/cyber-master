//! read_file 工具：读取文本文件内容（受 64KB 上限）。

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::resolve_under_cwd;

const MAX_BYTES: usize = 64 * 1024;

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "读取文本文件内容（相对工作目录或绝对路径，上限 64KB）".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径" }
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
            let resolved = resolve_under_cwd(std::path::Path::new(path), &ctx.cwd)
                .map_err(AgentError::Provider)?;
            let bytes = std::fs::read(&resolved).map_err(|e| {
                AgentError::Provider(format!("读取 {} 失败: {e}", resolved.display()))
            })?;
            let truncated = bytes.len() > MAX_BYTES;
            let slice = if truncated { &bytes[..MAX_BYTES] } else { &bytes };
            let mut content = String::from_utf8_lossy(slice).into_owned();
            if truncated {
                content.push_str("\n…（已截断，超过 64KB）");
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

    #[tokio::test]
    async fn reads_existing_file() {
        let dir = std::env::temp_dir().join("cyber_read_file_test");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("hello.txt");
        std::fs::write(&f, "你好世界").unwrap();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            rules: vec![],
            scope: None,
        };
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
        let ctx = ToolCtx {
            cwd: std::env::temp_dir(),
            rules: vec![],
            scope: None,
        };
        let out = ReadFileTool
            .run(json!({"path": "nonexistent_xyz.txt"}), &ctx)
            .await;
        assert!(out.is_err());
    }
}
