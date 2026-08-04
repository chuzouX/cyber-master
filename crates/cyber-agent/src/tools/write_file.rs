//! write_file 工具：写文本文件（护栏检查路径不越界）。

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::{check_write_path, resolve_under_cwd};

pub struct WriteFileTool;

impl Tool for WriteFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "write_file".into(),
            description: "写文本文件（相对工作目录或绝对路径，禁止逃逸出工作目录）".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件路径" },
                    "content": { "type": "string", "description": "写入内容" }
                },
                "required": ["path", "content"]
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
                .ok_or_else(|| AgentError::Provider("write_file 缺少 path 参数".into()))?;
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("write_file 缺少 content 参数".into()))?;
            let path_obj = std::path::Path::new(path);
            check_write_path(path_obj, ctx).map_err(AgentError::Provider)?;
            let resolved = resolve_under_cwd(path_obj, &ctx.cwd).map_err(AgentError::Provider)?;
            if let Some(parent) = resolved.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    AgentError::Provider(format!("创建目录 {} 失败: {e}", parent.display()))
                })?;
            }
            std::fs::write(&resolved, content).map_err(|e| {
                AgentError::Provider(format!("写入 {} 失败: {e}", resolved.display()))
            })?;
            Ok(ToolOutput {
                content: format!("已写入 {}（{} 字节）", resolved.display(), content.len()),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_file_within_cwd() {
        let dir = std::env::temp_dir().join("cyber_write_file_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            rules: vec![],
            scope: None,
            env: Vec::new(),
        };
        let out = WriteFileTool
            .run(json!({"path": "out.txt", "content": "hello"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn rejects_traversal() {
        let ctx = ToolCtx {
            cwd: std::env::temp_dir().join("cyber_write_guard"),
            rules: vec![],
            scope: None,
            env: Vec::new(),
        };
        let out = WriteFileTool
            .run(json!({"path": "../../etc/evil", "content": "x"}), &ctx)
            .await;
        assert!(out.is_err(), "路径逃逸应被拒绝");
    }
}
