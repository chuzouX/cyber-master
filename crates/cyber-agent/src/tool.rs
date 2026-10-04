//! Tool trait + ToolRegistry + 工具执行上下文。
//!
//! Tool 对象安全（不用 async-trait，与 `Provider` 一致）：`run` 返回 boxed future。
//! 内置工具放 `tools/` 子模块；P3/P6 的 MCP/Skill/security 工具将实现本 trait 注入
//! 统一工具表（ToolRegistry），cyber-agent 不反向依赖那些 crate。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};

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

/// 确保工具名符合标准 LLM 命名约束（`^[a-zA-Z0-9_-]{1,64}$`）。
/// 包含点号、空格或特殊字符会被替换为 `_`，超出 64 字符会被截断。
pub fn sanitize_tool_name(name: &str) -> String {
    let sanitized: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() > 64 {
        sanitized[..64].to_string()
    } else {
        sanitized
    }
}

/// 工具执行结果。`content` 回灌给 LLM（作为 tool 结果消息）。
#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// 工具执行上下文：工作目录 + 安全护栏（rules / scope）+ 用户自定义环境变量。
#[derive(Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    pub rules: Vec<String>,
    pub scope: Option<String>,
    /// 用户在 Settings → Env 配置的环境变量，注入 shell 子进程。
    pub env: Vec<(String, String)>,
    pub(crate) subagent_runtime: Option<Arc<crate::agent::SubagentRuntime>>,
    pub(crate) subagent_archive: Option<Arc<crate::subagent::SubagentArchive>>,
    pub(crate) background: Option<Arc<crate::background::BackgroundRegistry>>,
    pub(crate) provider_config: Option<cyber_core::ProviderConfig>,
    pub(crate) mock: bool,
}

impl ToolCtx {
    pub fn new(
        cwd: PathBuf,
        rules: Vec<String>,
        scope: Option<String>,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            cwd,
            rules,
            scope,
            env,
            subagent_runtime: None,
            subagent_archive: None,
            background: None,
            provider_config: None,
            mock: false,
        }
    }

    pub(crate) fn with_subagent_runtime(
        mut self,
        runtime: Arc<crate::agent::SubagentRuntime>,
    ) -> Self {
        self.subagent_runtime = Some(runtime);
        self
    }

    pub fn with_provider_config(mut self, cfg: Option<cyber_core::ProviderConfig>) -> Self {
        self.provider_config = cfg;
        self
    }

    pub fn with_mock(mut self, mock: bool) -> Self {
        self.mock = mock;
        self
    }

    pub fn provider_config(&self) -> Option<&cyber_core::ProviderConfig> {
        self.provider_config.as_ref()
    }

    pub fn is_mock(&self) -> bool {
        self.mock
    }

    pub(crate) fn subagent_runtime(&self) -> Option<&Arc<crate::agent::SubagentRuntime>> {
        self.subagent_runtime.as_ref()
    }

    pub(crate) fn with_subagent_archive(
        mut self,
        archive: Option<Arc<crate::subagent::SubagentArchive>>,
    ) -> Self {
        self.subagent_archive = archive;
        self
    }

    pub(crate) fn subagent_archive(&self) -> Option<&Arc<crate::subagent::SubagentArchive>> {
        self.subagent_archive.as_ref()
    }

    pub(crate) fn with_background(
        mut self,
        background: Option<Arc<crate::background::BackgroundRegistry>>,
    ) -> Self {
        self.background = background;
        self
    }

    pub(crate) fn background(&self) -> Option<&Arc<crate::background::BackgroundRegistry>> {
        self.background.as_ref()
    }
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

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

/// 统一工具表：持有 `Box<dyn Tool>`，按名查找、批量导出 schema、统一执行。
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
    catalog: ToolCatalog,
}

/// Wrap the Tool itself so even get(...).run(...) cannot bypass approval.
struct PermissionTool {
    schema: ToolSchema,
    inner: Arc<ToolRegistry>,
    broker: Arc<crate::permission::PermissionBroker>,
}

impl Tool for PermissionTool {
    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        self.run_streaming(input, ctx, None)
    }

    fn run_streaming<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
        progress: Option<UnboundedSender<String>>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let name = &self.schema.name;
            if !self.broker.authorize(name, &input).await {
                return Ok(ToolOutput {
                    content: format!("Permission denied: {name}; tool was not executed"),
                    is_error: true,
                });
            }
            self.inner
                .execute_streaming(name, input, ctx, progress)
                .await
        })
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.get(&self.schema.name).and_then(|t| t.as_any())
    }
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
        if let Some(index) = self
            .tools
            .iter()
            .position(|item| item.schema().name == schema.name)
        {
            self.tools[index] = tool;
        } else {
            self.tools.push(tool);
        }
        if let Some(index) = catalog.iter().position(|item| item.name == schema.name) {
            catalog[index] = schema;
        } else {
            catalog.push(schema);
        }
    }

    /// 仅注册为可执行工具，但不将 schema 导出到 catalog（不出现在发给 LLM 的 tools 列表中）。
    /// 适合海量工具（如海量 Skills）场景，避免超出模型 tools 数量限制。
    pub fn register_hidden(&mut self, tool: Box<dyn Tool>) {
        let schema = tool.schema();
        let mut catalog = self.catalog.write().unwrap_or_else(|e| e.into_inner());
        catalog.retain(|item| item.name != schema.name);
        if let Some(index) = self
            .tools
            .iter()
            .position(|item| item.schema().name == schema.name)
        {
            self.tools[index] = tool;
        } else {
            self.tools.push(tool);
        }
    }

    /// 导出当前在 catalog 中的工具 Schema（发给 LLM 的 tools 列表）。
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.catalog
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// 导出全部已注册工具的 Schema（包含 register_hidden 的工具，供系统提示词等索引提取）。
    pub fn all_schemas(&self) -> Vec<ToolSchema> {
        self.tools.iter().map(|t| t.schema()).collect()
    }

    pub fn catalog(&self) -> ToolCatalog {
        Arc::clone(&self.catalog)
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| {
                let schema_name = &t.schema().name;
                schema_name == name
                    || (schema_name.starts_with("custom_")
                        && schema_name.strip_prefix("custom_") == Some(name))
                    || (name.starts_with("custom_")
                        && name.strip_prefix("custom_") == Some(schema_name.as_str()))
                    || (schema_name.starts_with("skill_")
                        && schema_name.replace('.', "_") == name.replace('.', "_"))
            })
            .map(|t| t.as_ref())
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn todo_state(&self) -> Option<Arc<Mutex<Vec<cyber_core::TodoItem>>>> {
        self.get("todo")
            .and_then(|t| t.as_any())
            .and_then(|a| a.downcast_ref::<crate::tools::TodoTool>())
            .map(|t| t.todos())
    }

    /// All execution entry points on this view require explicit approval.
    pub fn with_permissions(
        inner: Arc<Self>,
        broker: Arc<crate::permission::PermissionBroker>,
    ) -> Self {
        let tools = inner
            .all_schemas()
            .into_iter()
            .map(|schema| {
                Box::new(PermissionTool {
                    schema,
                    inner: inner.clone(),
                    broker: broker.clone(),
                }) as Box<dyn Tool>
            })
            .collect();
        Self {
            tools,
            catalog: Arc::new(RwLock::new(inner.schemas())),
        }
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
            None => Box::pin(async move { Err(AgentError::Provider(format!("未知工具: {name}"))) }),
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
        ToolCtx::new(std::env::temp_dir(), vec![], None, Vec::new())
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

    #[test]
    fn sanitize_tool_name_replaces_illegal_characters() {
        assert_eq!(sanitize_tool_name("valid_name-123"), "valid_name-123");
        assert_eq!(
            sanitize_tool_name("skill_ctf-crypto-1.0.0"),
            "skill_ctf-crypto-1_0_0"
        );
        assert_eq!(sanitize_tool_name("tool with space!"), "tool_with_space_");
        let long_name = "a".repeat(100);
        assert_eq!(sanitize_tool_name(&long_name).len(), 64);
    }

    #[tokio::test]
    async fn register_hidden_executes_but_omits_from_schemas() {
        let mut reg = ToolRegistry::new();
        reg.register_hidden(Box::new(EchoTool));
        // schemas() 为空（不发给 LLM）
        assert_eq!(reg.schemas().len(), 0);
        // all_schemas() 包含该工具（供提示词索引提取）
        assert_eq!(reg.all_schemas().len(), 1);
        // 可被正常查找和执行
        let out = reg
            .execute("echo", serde_json::json!({"test": 123}), &ctx())
            .await
            .unwrap();
        assert!(out.content.contains("123"));
    }

    struct CountingTool(Arc<std::sync::atomic::AtomicUsize>);

    impl Tool for CountingTool {
        fn schema(&self) -> ToolSchema {
            EchoTool.schema()
        }

        fn run<'a>(
            &'a self,
            input: Value,
            ctx: &'a ToolCtx,
        ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            EchoTool.run(input, ctx)
        }
    }

    #[tokio::test]
    async fn permission_denial_blocks_both_execution_paths_including_hidden_tools() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut inner = ToolRegistry::new();
        inner.register_hidden(Box::new(CountingTool(count.clone())));
        let registry = ToolRegistry::with_permissions(
            Arc::new(inner),
            Arc::new(crate::PermissionBroker::deny_all()),
        );
        assert!(registry.schemas().is_empty());
        assert_eq!(registry.all_schemas().len(), 1);
        let output = registry
            .execute("echo", serde_json::json!({}), &ctx())
            .await
            .unwrap();
        assert!(output.is_error);
        let output = registry
            .execute_streaming("echo", serde_json::json!({}), &ctx(), None)
            .await
            .unwrap();
        assert!(output.is_error);
        let output = registry
            .get("echo")
            .unwrap()
            .run(serde_json::json!({}), &ctx())
            .await
            .unwrap();
        assert!(output.is_error);
        let output = registry
            .get("echo")
            .unwrap()
            .run_streaming(serde_json::json!({}), &ctx(), None)
            .await
            .unwrap();
        assert!(output.is_error);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn approval_precedes_execution_and_closed_ui_denies() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut inner = ToolRegistry::new();
        inner.register(Box::new(CountingTool(count.clone())));
        let (broker, mut requests) = crate::PermissionBroker::interactive();
        broker.set_mode(crate::PermissionMode::Manual);
        let registry = Arc::new(ToolRegistry::with_permissions(
            Arc::new(inner),
            Arc::new(broker),
        ));
        let worker_registry = registry.clone();
        let worker = tokio::spawn(async move {
            worker_registry
                .execute("echo", serde_json::json!({"x": 1}), &ctx())
                .await
                .unwrap()
        });
        let request = requests.recv().await.unwrap();
        assert_eq!(request.tool, "echo");
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        request
            .reply
            .send(crate::PermissionDecision::AllowOnce)
            .unwrap();
        assert!(!worker.await.unwrap().is_error);
        drop(requests);
        assert!(
            registry
                .execute("echo", serde_json::json!({"x": 1}), &ctx())
                .await
                .unwrap()
                .is_error
        );
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn visible_registration_replaces_actual_tool_after_hidden_registration() {
        let mut registry = ToolRegistry::new();
        registry.register_hidden(Box::new(EchoTool));
        crate::tools::register_builtins(&mut registry);
        crate::tools::register_builtins(&mut registry);
        assert_eq!(registry.all_schemas().len(), 14);
        assert!(registry.get("echo").is_some());
        assert!(registry.get("list_dir").is_some());
        assert!(registry.get("bg_shell").is_some());
        assert!(registry.get("inspect_image").is_some());
        // Promoting a hidden tool must replace it, rather than duplicate it.
        registry.register(Box::new(EchoTool));
        assert_eq!(registry.all_schemas().len(), 14);
        assert_eq!(registry.schemas().len(), 14);
    }

    #[test]
    fn permission_view_catalog_changes_do_not_modify_source_catalog() {
        let mut inner = ToolRegistry::new();
        inner.register(Box::new(EchoTool));
        let inner = Arc::new(inner);
        let mut protected = ToolRegistry::with_permissions(
            inner.clone(),
            Arc::new(crate::PermissionBroker::deny_all()),
        );
        protected.register_hidden(Box::new(EchoTool));
        assert!(protected.schemas().is_empty());
        assert_eq!(inner.schemas().len(), 1);
    }

    #[test]
    fn registry_get_resolves_custom_prefix_bidirectionally() {
        struct NamedTool(&'static str);
        impl Tool for NamedTool {
            fn schema(&self) -> ToolSchema {
                ToolSchema {
                    name: self.0.into(),
                    description: "test".into(),
                    parameters: serde_json::json!({"type": "object"}),
                    tags: vec![],
                }
            }
            fn run<'a>(
                &'a self,
                _input: Value,
                _ctx: &'a ToolCtx,
            ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
                Box::pin(async {
                    Ok(ToolOutput {
                        content: "ok".into(),
                        is_error: false,
                    })
                })
            }
        }

        let mut reg = ToolRegistry::new();
        reg.register(Box::new(NamedTool("custom_sqlmap")));
        reg.register(Box::new(NamedTool("nmap")));

        // 1. Tool named custom_sqlmap: can be accessed via custom_sqlmap and sqlmap
        assert!(reg.get("custom_sqlmap").is_some());
        assert!(reg.get("sqlmap").is_some());

        // 2. Tool named nmap: can be accessed via nmap and custom_nmap
        assert!(reg.get("nmap").is_some());
        assert!(reg.get("custom_nmap").is_some());

        // 3. Unknown tool
        assert!(reg.get("unknown_tool").is_none());
    }
}
