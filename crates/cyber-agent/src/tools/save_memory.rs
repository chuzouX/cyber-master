//! save_memory 工具：agent 自动保存长期记忆（类似 ChatGPT 的 memory）。
//!
//! 当用户在对话中表达偏好、身份、约定或重要信息时，agent 调用本工具把内容
//! 持久化到记忆文件（全局或项目级），下次对话自动注入系统提示词。
//! 记忆文件为纯 markdown，每行 `- 内容`，可直接手动编辑。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use cyber_core::{MemoryScope, MemoryStore};
use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};

/// 记忆保存工具。
///
/// 持有全局记忆文件路径；项目级记忆路径从 `ctx.cwd` 推导（`<cwd>/.cyber/memory.md`）。
pub struct SaveMemoryTool {
    global_memory_file: PathBuf,
}

impl SaveMemoryTool {
    pub fn new(global_memory_file: PathBuf) -> Self {
        Self { global_memory_file }
    }

    /// 根据 `ctx.cwd` 和全局文件路径构建两层记忆存储。
    fn store(&self, ctx: &ToolCtx) -> MemoryStore {
        let project_file = ctx.cwd.join(".cyber").join("memory.md");
        MemoryStore::new(self.global_memory_file.clone(), project_file)
    }
}

impl Tool for SaveMemoryTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "save_memory".into(),
            description: "把重要信息保存到长期记忆，供后续对话自动引用。当用户表达偏好、身份、约定、项目背景等需要记住的内容时调用。scope=global（默认，跨项目）或 project（仅当前项目）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "要记住的内容（一句话或一段话）"
                    },
                    "scope": {
                        "type": "string",
                        "enum": ["global", "project"],
                        "description": "记忆作用域：global 跨项目共享（默认），project 仅当前项目"
                    }
                },
                "required": ["content"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let content = input
                .get("content")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AgentError::Provider("save_memory 缺少非空 content 参数".into()))?;

            let scope = input
                .get("scope")
                .and_then(|v| v.as_str())
                .map(MemoryScope::parse)
                .unwrap_or(MemoryScope::Global);

            let store = self.store(ctx);
            store.append(scope, content).map_err(|e| {
                AgentError::Provider(format!("保存记忆失败: {e}"))
            })?;

            let scope_name = match scope {
                MemoryScope::Global => "全局",
                MemoryScope::Project => "项目",
            };
            Ok(ToolOutput {
                content: format!("已保存到{scope_name}记忆：{content}"),
                is_error: false,
            })
        })
    }
}
