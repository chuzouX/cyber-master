//! list_dir 工具：列出目录条目。

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::resolve_under_cwd;

pub struct ListDirTool;

impl Tool for ListDirTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_dir".into(),
            description: "列出目录条目（相对工作目录或绝对路径）".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "目录路径（默认工作目录）" }
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
            let path_str = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let resolved = resolve_under_cwd(std::path::Path::new(path_str), &ctx.cwd)
                .map_err(AgentError::Provider)?;
            let rd = std::fs::read_dir(&resolved).map_err(|e| {
                AgentError::Provider(format!("读取目录 {} 失败: {e}", resolved.display()))
            })?;
            let mut entries: Vec<String> = Vec::new();
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let suffix = if entry
                    .file_type()
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
                    "/"
                } else {
                    ""
                };
                entries.push(format!("{name}{suffix}"));
            }
            entries.sort();
            if entries.is_empty() {
                return Ok(ToolOutput {
                    content: format!("{}（空目录）", resolved.display()),
                    is_error: false,
                });
            }
            Ok(ToolOutput {
                content: entries.join("\n"),
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_directory() {
        let dir = std::env::temp_dir().join("cyber_list_dir_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            rules: vec![],
            scope: None,
            env: Vec::new(),
        };
        let out = ListDirTool
            .run(json!({"path": "."}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("a.txt"));
        assert!(out.content.contains("sub/"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn default_path_is_cwd() {
        let dir = std::env::temp_dir().join("cyber_list_default");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        let ctx = ToolCtx {
            cwd: dir.clone(),
            rules: vec![],
            scope: None,
            env: Vec::new(),
        };
        let out = ListDirTool.run(json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("marker.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
