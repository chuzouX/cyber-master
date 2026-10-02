//! McpTool：把 MCP server 暴露的工具包装成 cyber-agent 的 `Tool`。
//!
//! 命名 `mcp_<server>_<tool>`（非法字符替换 `_`），与 builtins / `skill_<name>` 前缀隔离。
//! `run` 发 `tools/call` 到 server，拼 `content[]` text 为单字符串返回。
//! server 返回 `isError=true` → `ToolOutput.is_error=true`（LLM 看到错误内容）；
//! RPC 失败 → `Err`（agent loop 转为 is_error ToolOutput）。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use cyber_agent::{AgentError, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde_json::Value;

use crate::connection::McpConnection;
use crate::proto::McpToolSchema;

/// 一个 MCP server 工具的 `Tool` 包装。
pub struct McpTool {
    server: Arc<McpConnection>,
    /// server 端原始工具名（`tools/call` 的 `name` 参数用此，非带前缀名）。
    tool_name: String,
    schema: ToolSchema,
}

impl McpTool {
    pub fn new(server: Arc<McpConnection>, mcp_schema: McpToolSchema) -> Self {
        let schema = ToolSchema {
            name: format!(
                "mcp_{}_{}",
                sanitize(server.server_name()),
                sanitize(&mcp_schema.name)
            ),
            description: if mcp_schema.description.is_empty() {
                format!("[MCP/{}]", server.server_name())
            } else {
                format!("[MCP/{}] {}", server.server_name(), mcp_schema.description)
            },
            tags: vec!["mcp".into(), server.server_name().into()],
            parameters: mcp_schema.input_schema,
        };
        Self {
            server,
            tool_name: mcp_schema.name,
            schema,
        }
    }
}

impl Tool for McpTool {
    fn schema(&self) -> ToolSchema {
        self.schema.clone()
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, AgentError>> + Send + 'a>> {
        let server = self.server.clone();
        let tool_name = self.tool_name.clone();
        Box::pin(async move {
            match server.call_tool(&tool_name, input).await {
                Ok(result) => {
                    let content = result
                        .content
                        .into_iter()
                        .filter_map(|c| if c.is_text() { Some(c.text) } else { None })
                        .collect::<Vec<_>>()
                        .join("\n");
                    Ok(ToolOutput {
                        content,
                        is_error: result.is_error,
                    })
                }
                Err(e) => Err(AgentError::Provider(format!(
                    "MCP 工具 '{tool_name}' 调用失败: {e}"
                ))),
            }
        })
    }
}

/// 把工具名/服务器名中的非 `[a-zA-Z0-9_]` 字符替换为 `_`（保证 LLM 工具名合法）。
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::McpConnection;
    use crate::proto::McpToolSchema;

    fn make_conn(name: &str) -> Arc<McpConnection> {
        Arc::new(McpConnection::for_test(name))
    }

    #[test]
    fn schema_name_prefixed() {
        let conn = make_conn("filesystem");
        let tool = McpTool::new(
            conn,
            McpToolSchema {
                name: "read_file".into(),
                description: "read a file".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        );
        assert_eq!(tool.schema().name, "mcp_filesystem_read_file");
        assert!(tool.schema().description.contains("[MCP/filesystem]"));
        assert!(tool.schema().description.contains("read a file"));
        assert_eq!(tool.schema().tags, vec!["mcp", "filesystem"]);
    }

    #[test]
    fn sanitize_replaces_special_chars() {
        let conn = make_conn("my-server.v2");
        let tool = McpTool::new(
            conn,
            McpToolSchema {
                name: "tool/name".into(),
                description: "d".into(),
                input_schema: serde_json::json!({}),
            },
        );
        assert_eq!(tool.schema().name, "mcp_my_server_v2_tool_name");
        assert_eq!(tool.schema().tags, vec!["mcp", "my-server.v2"]);
    }

    #[test]
    fn empty_description_uses_server_only() {
        let conn = make_conn("x");
        let tool = McpTool::new(
            conn,
            McpToolSchema {
                name: "t".into(),
                description: String::new(),
                input_schema: serde_json::json!({}),
            },
        );
        assert_eq!(tool.schema().description, "[MCP/x]");
        assert_eq!(tool.schema().tags, vec!["mcp", "x"]);
    }

    #[tokio::test]
    async fn registered_mcp_tools_are_discoverable_in_existing_search_catalog() {
        use cyber_agent::tools::SearchToolsTool;
        use cyber_agent::ToolRegistry;

        let mut registry = ToolRegistry::new();
        registry.register(Box::new(SearchToolsTool::new(registry.catalog())));
        let ctx = ToolCtx::new(std::env::temp_dir(), vec![], None, vec![]);
        let before = registry
            .execute("search_tools", serde_json::json!({"tag": "mcp"}), &ctx)
            .await
            .unwrap();
        assert!(!before.content.contains("mcp_my_server_v2_read_file"));

        for server in ["my-server.v2", "other"] {
            registry.register(Box::new(McpTool::new(
                make_conn(server),
                McpToolSchema {
                    name: "read_file".into(),
                    description: "read a file".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            )));
        }

        for (query, finds_first, finds_other) in [
            ("mcp", true, true),
            (" MCP ", true, true),
            ("MY-SERVER.V2", true, false),
            ("server.v2", true, false),
            ("other", false, true),
            ("", true, true),
            ("nonexistent", false, false),
        ] {
            let output = registry
                .execute("search_tools", serde_json::json!({"tag": query}), &ctx)
                .await
                .unwrap();
            assert!(!output.is_error);
            assert_eq!(
                output.content.contains("mcp_my_server_v2_read_file"),
                finds_first,
                "query: {query:?}"
            );
            assert_eq!(
                output.content.contains("mcp_other_read_file"),
                finds_other,
                "query: {query:?}"
            );
            if finds_first {
                assert!(output.content.contains("[mcp, my-server.v2]"));
            }
        }
    }
}
