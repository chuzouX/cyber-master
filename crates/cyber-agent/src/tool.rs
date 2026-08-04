//! Tool trait + ToolRegistry + 工具执行上下文。
//!
//! Tool 对象安全（不用 async-trait，与 `Provider` 一致）：`run` 返回 boxed future。
//! 内置工具放 `tools/` 子模块；P3/P6 的 MCP/Skill/security 工具将实现本 trait 注入
//! 统一工具表（ToolRegistry），cyber-agent 不反向依赖那些 crate。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use serde_json::Value;

use crate::error::{AgentError, Result};

/// 工具的 JSON Schema 描述（发给 LLM 的 `tools` 字段）。
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// 参数的 JSON Schema（`parameters`）。
    pub parameters: Value,
}

/// 工具执行结果。`content` 回灌给 LLM（作为 tool 结果消息）。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// 工具执行上下文：工作目录 + 安全护栏（rules / scope）。
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub rules: Vec<String>,
    pub scope: Option<String>,
}

/// 工具抽象。`Send + Sync` 以便 `Box<dyn Tool>` 跨 tokio task。
pub trait Tool: Send + Sync {
    fn schema(&self) -> ToolSchema;
    /// 执行工具。`input` 为参数 JSON（LLM 提供），`ctx` 借用（lifetime 绑定 future）。
    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>>;
}

/// 统一工具表：持有 `Box<dyn Tool>`，按名查找、批量导出 schema、统一执行。
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self { tools: Vec::new() }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|t| t.schema()).collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.schema().name == name)
            .map(|t| t.as_ref())
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// 执行工具。未知工具名 → `AgentError::Provider`（回灌给 LLM 让其修正）。
    pub fn execute<'a>(
        &'a self,
        name: &'a str,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        match self.get(name) {
            Some(tool) => tool.run(input, ctx),
            None => Box::pin(async move {
                Err(AgentError::Provider(format!("未知工具: {name}")))
            }),
        }
    }

    /// 注册内置工具（read_file / write_file / list_dir / shell）。
    pub fn with_builtins() -> Self {
        let mut reg = Self::new();
        crate::tools::register_builtins(&mut reg);
        reg
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    impl Tool for EchoTool {
        fn schema(&self) -> ToolSchema {
            ToolSchema {
                name: "echo".into(),
                description: "echo input".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        fn run<'a>(
            &'a self,
            input: Value,
            _ctx: &'a ToolCtx,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
            Box::pin(async move {
                let content = input.to_string();
                Ok(ToolOutput {
                    content,
                    is_error: false,
                })
            })
        }
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            rules: vec![],
            scope: None,
        }
    }

    #[tokio::test]
    async fn registry_lookup_and_execute() {
        let reg = ToolRegistry {
            tools: vec![Box::new(EchoTool)],
        };
        assert!(!reg.is_empty());
        assert_eq!(reg.schemas().len(), 1);
        assert_eq!(reg.schemas()[0].name, "echo");
        let out = reg
            .execute("echo", serde_json::json!({"x": 1}), &ctx())
            .await
            .unwrap();
        assert!(out.content.contains("\"x\":1"));
        assert!(!out.is_error);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error() {
        let reg = ToolRegistry::new();
        let out = reg.execute("nope", serde_json::Value::Null, &ctx()).await;
        assert!(out.is_err(), "未知工具应返回 Err");
    }

    #[test]
    fn with_builtins_registers_four() {
        let reg = ToolRegistry::with_builtins();
        let schemas = reg.schemas();
        let names: Vec<&str> = schemas.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"write_file"));
        assert!(names.contains(&"list_dir"));
        assert!(names.contains(&"shell"));
    }
}
