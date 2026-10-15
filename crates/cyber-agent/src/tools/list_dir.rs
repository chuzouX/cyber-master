//! list_dir 工具：列出目录条目（带截断保护与可选详细文件信息）。

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::resolve_under_cwd;

const DEFAULT_LIMIT: usize = 300;

pub struct ListDirTool;

impl Tool for ListDirTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "list_dir".into(),
            description: "列出目录条目（相对工作目录或绝对路径）。支持海量目录数量截断保护及可选详细文件体积展示。".into(),
            tags: vec![],
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "目录路径（默认当前工作目录）"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "最大展示条目数（默认 300，范围 1-1000，防止海量文件撑爆上下文）",
                        "minimum": 1,
                        "maximum": 1000
                    },
                    "detailed": {
                        "type": "boolean",
                        "description": "是否展示文件详细大小与类型信息（默认 false）"
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
            let path_str = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|l| (l as usize).clamp(1, 1000))
                .unwrap_or(DEFAULT_LIMIT);
            let detailed = input
                .get("detailed")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let resolved =
                resolve_under_cwd(Path::new(path_str), &ctx.cwd).map_err(AgentError::Provider)?;
            let rd = std::fs::read_dir(&resolved).map_err(|e| {
                AgentError::Provider(format!("读取目录 {} 失败: {e}", resolved.display()))
            })?;

            struct DirEntryItem {
                name: String,
                is_dir: bool,
                size_bytes: u64,
            }

            let mut raw_items = Vec::new();
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let (is_dir, size_bytes) = if let Ok(meta) = entry.metadata() {
                    (meta.is_dir(), meta.len())
                } else {
                    (false, 0)
                };
                raw_items.push(DirEntryItem {
                    name,
                    is_dir,
                    size_bytes,
                });
            }

            raw_items.sort_by(|a, b| {
                // 目录优先排在前面，同类按名称字母排序
                b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name))
            });

            let total = raw_items.len();
            if total == 0 {
                return Ok(ToolOutput {
                    content: format!("{}（空目录）", resolved.display()),
                    is_error: false,
                });
            }

            let is_truncated = total > limit;
            let display_items = &raw_items[..total.min(limit)];

            let lines: Vec<String> = display_items
                .iter()
                .map(|item| {
                    if detailed {
                        if item.is_dir {
                            format!("{:<32} <DIR>", format!("{}/", item.name))
                        } else {
                            format!("{:<32} {}", item.name, format_size(item.size_bytes))
                        }
                    } else {
                        let suffix = if item.is_dir { "/" } else { "" };
                        format!("{}{suffix}", item.name)
                    }
                })
                .collect();

            let mut content = lines.join("\n");
            if is_truncated {
                content.push_str(&format!(
                    "\n\n[提示：目录条目较多，已截断显示前 {limit} 项（共 {total} 项）。可通过 limit 参数调整展示条目数量]"
                ));
            }

            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

fn format_size(size: u64) -> String {
    if size >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", size as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if size >= 1024 * 1024 {
        format!("{:.2} MB", size as f64 / (1024.0 * 1024.0))
    } else if size >= 1024 {
        format!("{:.2} KB", size as f64 / 1024.0)
    } else {
        format!("{size} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_directory() {
        let dir = std::env::temp_dir().join(format!("cyber_list_dir_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let ctx = ToolCtx::new(dir.clone(), vec![], None, Vec::new());
        let out = ListDirTool.run(json!({"path": "."}), &ctx).await.unwrap();
        assert!(out.content.contains("a.txt"));
        assert!(out.content.contains("sub/"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn default_path_is_cwd() {
        let dir = std::env::temp_dir().join(format!("cyber_list_default_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), "x").unwrap();
        let ctx = ToolCtx::new(dir.clone(), vec![], None, Vec::new());
        let out = ListDirTool.run(json!({}), &ctx).await.unwrap();
        assert!(out.content.contains("marker.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn limits_entries_with_truncation_notice() {
        let dir = std::env::temp_dir().join(format!("cyber_list_limit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for i in 1..=10 {
            std::fs::write(dir.join(format!("file_{i}.txt")), "123").unwrap();
        }
        let ctx = ToolCtx::new(dir.clone(), vec![], None, Vec::new());
        let out = ListDirTool
            .run(json!({"path": ".", "limit": 4}), &ctx)
            .await
            .unwrap();

        assert!(out.content.contains("已截断显示前 4 项（共 10 项）"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn detailed_view_includes_file_sizes() {
        let dir = std::env::temp_dir().join(format!("cyber_list_detailed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.txt"), "hello world").unwrap();
        std::fs::create_dir_all(dir.join("my_dir")).unwrap();
        let ctx = ToolCtx::new(dir.clone(), vec![], None, Vec::new());
        let out = ListDirTool
            .run(json!({"path": ".", "detailed": true}), &ctx)
            .await
            .unwrap();

        assert!(out.content.contains("<DIR>"));
        assert!(out.content.contains("11 B"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
