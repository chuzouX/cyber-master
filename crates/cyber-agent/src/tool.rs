//! Tool trait + ToolRegistry + 工具执行上下文。
//!
//! Tool 对象安全（不用 async-trait，与 `Provider` 一致）：`run` 返回 boxed future。
//! 内置工具放 `tools/` 子模块；P3/P6 的 MCP/Skill/security 工具将实现本 trait 注入
//! 统一工具表（ToolRegistry），cyber-agent 不反向依赖那些 crate。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::{AgentError, Result};

/// 工具的 JSON Schema 描述（发给 LLM 的 `tools` 字段）。
#[derive(Debug, Clone)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// 参数的 JSON Schema。
    pub parameters: Value,
    /// 仅供本地工具发现使用，不发送给 provider。
    pub tags: Vec<String>,
}

pub type ToolCatalog = Arc<RwLock<Vec<ToolSchema>>>;

/// 工具执行结果。`content` 回灌给 LLM（作为 tool 结果消息）。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// 工具执行上下文：工作目录 + 安全护栏（rules / scope）+ 用户自定义环境变量。
#[derive(Debug, Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub rules: Vec<String>,
    pub scope: Option<String>,
    /// 用户在 Settings → Env 配置的环境变量，注入 shell 子进程。
    pub env: Vec<(String, String)>,
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

    /// 流式执行：与 `run` 相同语义，但可通过 `progress` 通道向前端推送增量输出
    ///（如 shell 逐行 stdout/stderr）。默认实现忽略 `progress` 直接调 `run`，
    /// 需要流式的工具（shell）覆写。`progress` 为 `None` 时表示调用方不消费流
    ///（如单测直接调 `run`），实现应跳过推送避免 channel 缓冲无限增长。
    fn run_streaming<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
        progress: Option<UnboundedSender<String>>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        let _ = progress;
        self.run(input, ctx)
    }
}

/// 统一工具表：持有 `Box<dyn Tool>`，按名查找、批量导出 schema、统一执行。
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    catalog: ToolCatalog,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: Vec::new(),
            catalog: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        let schema = tool.schema();
        let mut catalog = self.catalog.write().unwrap_or_else(|e| e.into_inner());
        if let Some(index) = catalog.iter().position(|item| item.name == schema.name) {
            self.tools[index] = tool;
            catalog[index] = schema;
        } else {
            self.tools.push(tool);
            catalog.push(schema);
        }
    }

    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.catalog.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn catalog(&self) -> ToolCatalog {
        Arc::clone(&self.catalog)
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

    /// 流式执行工具：经 `progress` 通道推送增量输出（shell 逐行 stdout/stderr）。
    /// 不支持流式的工具走 trait 默认实现（忽略 progress，直接 `run`）。
    /// `progress` 为 `None` 时表示调用方不消费流（如单测），工具应跳过推送。
    pub fn execute_streaming<'a>(
        &'a self,
        name: &'a str,
        input: Value,
        ctx: &'a ToolCtx,
        progress: Option<UnboundedSender<String>>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        match self.get(name) {
            Some(tool) => tool.run_streaming(input, ctx, progress),
            None => Box::pin(async move {
                let _ = progress;
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
                tags: vec![],
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
            env: Vec::new(),
        }
    }

    #[tokio::test]
    async fn registry_lookup_and_execute() {
        let mut reg = ToolRegistry::new();
        reg.register(Box::new(EchoTool));
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
