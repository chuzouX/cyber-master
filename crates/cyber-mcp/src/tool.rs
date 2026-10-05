//! McpTool：把 MCP server 暴露的工具包装成 cyber-agent 的 `Tool`。
//!
//! 命名 `mcp_<server>_<tool>`（非法字符替换 `_`），与 builtins / `skill_<name>` 前缀隔离。
//! `run` 发 `tools/call` 到 server，拼 `content[]` text 为单字符串返回。
//! server 返回 `isError=true` → `ToolOutput.is_error=true`（LLM 看到错误内容）；
//! RPC 失败 → `Err`（agent loop 转为 is_error ToolOutput）。

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use cyber_agent::{AgentError, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde_json::{json, Value};

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

    pub fn server_name(&self) -> &str {
        self.server.server_name()
    }

    pub fn raw_tool_name(&self) -> &str {
        &self.tool_name
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

/// MCP 工具快照信息，用于清单汇总与模型交互。
#[derive(Debug, Clone)]
pub struct McpToolSnapshot {
    pub server_name: String,
    pub original_name: String,
    pub mcp_name: String,
    pub description: String,
    pub input_schema: Value,
}

/// MCP 工具聚合清单工具，将所有已连接 MCP server 暴露的扩展工具收敛为一个元工具暴露给大模型。
pub struct McpToolsListTool {
    tools: Arc<Vec<McpToolSnapshot>>,
}

impl McpToolsListTool {
    pub fn new(tools: Arc<Vec<McpToolSnapshot>>) -> Self {
        Self { tools }
    }

    pub fn from_mcp_tools(tools: &[McpTool]) -> Self {
        let snapshots = tools
            .iter()
            .map(|t| McpToolSnapshot {
                server_name: t.server_name().to_string(),
                original_name: t.raw_tool_name().to_string(),
                mcp_name: t.schema().name,
                description: t.schema().description,
                input_schema: t.schema().parameters,
            })
            .collect();
        Self::new(Arc::new(snapshots))
    }

    pub fn empty() -> Self {
        Self {
            tools: Arc::new(Vec::new()),
        }
    }
}

impl Tool for McpToolsListTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_tools_list".into(),
            description: "获取系统中所有已连接的 MCP（Model Context Protocol）扩展工具清单。工具选择次优级入口（优先级：custom_tools_list > mcp_tools_list > 自己做）。当 custom_tools_list 中未找到所需工具，且需要与外部系统交互（如外部竞赛平台靶机管理/Flag提交/题目浏览，或流量审计/重放等）时调用本工具获取工具名称、所属服务、用途描述与参数规格。获取后可直接以对应工具名称（如 mcp_<server>_<tool>、<server>_<tool> 或简写）发起调用，避免自行编写脚本重复实现。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "可选过滤指定的 MCP 服务名称。留空则返回所有服务。"
                    },
                    "query": {
                        "type": "string",
                        "description": "可选过滤关键词（按工具名称或用途描述过滤）。留空则返回匹配服务下的所有工具。"
                    }
                }
            }),
            tags: vec!["meta".into(), "mcp".into(), "tools".into()],
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, AgentError>> + Send + 'a>> {
        if self.tools.is_empty() {
            return Box::pin(async {
                Ok(ToolOutput {
                    content: "当前未连接任何 MCP 服务。可在 ~/.cyber/mcp/servers.toml 中配置 MCP server，配置后重启生效。".to_string(),
                    is_error: false,
                })
            });
        }

        let server_filter = input
            .get("server")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase);

        let query_filter = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);

        let matches: Vec<&McpToolSnapshot> = self
            .tools
            .iter()
            .filter(|t| {
                if let Some(s) = &server_filter {
                    if !t.server_name.to_lowercase().contains(s) {
                        return false;
                    }
                }
                if let Some(q) = &query_filter {
                    let matched = t.mcp_name.to_lowercase().contains(q)
                        || t.original_name.to_lowercase().contains(q)
                        || t.server_name.to_lowercase().contains(q)
                        || t.description.to_lowercase().contains(q);
                    if !matched {
                        return false;
                    }
                }
                true
            })
            .collect();

        if matches.is_empty() {
            let all_servers: BTreeSet<&str> =
                self.tools.iter().map(|t| t.server_name.as_str()).collect();
            let servers_str = all_servers.into_iter().collect::<Vec<_>>().join(", ");
            let msg = format!(
                "未找到匹配的 MCP 工具（当前共连接 {} 个工具，涉及服务: [{}]）。可清空筛选条件重新查询以列出全部工具。",
                self.tools.len(),
                servers_str
            );
            return Box::pin(async move {
                Ok(ToolOutput {
                    content: msg,
                    is_error: false,
                })
            });
        }

        let mut grouped: BTreeMap<&str, Vec<&McpToolSnapshot>> = BTreeMap::new();
        for tool in &matches {
            grouped.entry(&tool.server_name).or_default().push(tool);
        }

        let mut output = String::new();
        output.push_str(&format!(
            "# MCP 扩展工具清单 (共找到 {} 个工具，涉及 {} 个服务)\n\n",
            matches.len(),
            grouped.len()
        ));

        for (server, group_tools) in grouped {
            output.push_str(&format!(
                "## 服务: {server} ({} 个工具)\n\n",
                group_tools.len()
            ));
            for (idx, tool) in group_tools.iter().enumerate() {
                let short_server = format!("{}_{}", tool.server_name, tool.original_name);
                output.push_str(&format!(
                    "### {}. {} (简写: {} / {})\n",
                    idx + 1,
                    tool.mcp_name,
                    short_server,
                    tool.original_name
                ));
                output.push_str(&format!("- **功能描述**: {}\n", tool.description));
                output.push_str("- **参数规格**:\n");
                let properties = tool
                    .input_schema
                    .get("properties")
                    .and_then(Value::as_object);
                let required_props: HashSet<&str> = tool
                    .input_schema
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();

                if let Some(props) = properties {
                    if props.is_empty() {
                        output.push_str("  - (无参数)\n");
                    } else {
                        for (prop_name, prop_spec) in props {
                            let req_str = if required_props.contains(prop_name.as_str()) {
                                "必填"
                            } else {
                                "可选"
                            };
                            let typ_str = prop_spec
                                .get("type")
                                .and_then(Value::as_str)
                                .unwrap_or("any");
                            let default_str = prop_spec
                                .get("default")
                                .map(|d| format!(", 默认值: {d}"))
                                .unwrap_or_default();
                            let desc_str = prop_spec
                                .get("description")
                                .and_then(Value::as_str)
                                .map(|d| format!(" - {d}"))
                                .unwrap_or_default();
                            output.push_str(&format!(
                                "  - `{prop_name}` ({req_str}, 类型: {typ_str}{default_str}){desc_str}\n"
                            ));
                        }
                    }
                } else {
                    output.push_str("  - (无参数)\n");
                }
                output.push('\n');
            }
        }

        output.push_str("## 调用方式说明：\n");
        output.push_str("1. **直接工具调用**：可直接以完整名 `mcp_<server>_<tool>`、服务前缀名 `<server>_<tool>` 或原名 `<tool>` 发起工具调用，传入上述参数字典。\n");

        Box::pin(async move {
            Ok(ToolOutput {
                content: output,
                is_error: false,
            })
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

    #[tokio::test]
    async fn mcp_tools_list_schema_and_empty_output() {
        let tool = McpToolsListTool::empty();
        let schema = tool.schema();
        assert_eq!(schema.name, "mcp_tools_list");
        assert_eq!(schema.tags, vec!["meta", "mcp", "tools"]);
        assert!(schema.description.contains("MCP"));

        let ctx = ToolCtx::new(std::env::temp_dir(), vec![], None, vec![]);
        let output = tool.run(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("当前未连接任何 MCP 服务"));
    }

    #[tokio::test]
    async fn mcp_tools_list_filters_by_server_and_query() {
        let snapshots = vec![
            McpToolSnapshot {
                server_name: "ctf2".into(),
                original_name: "submit_flag".into(),
                mcp_name: "mcp_ctf2_submit_flag".into(),
                description: "向 CTF2 竞赛平台提交 flag".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "challenge_id": {
                            "type": "integer",
                            "description": "题目 ID"
                        },
                        "flag": {
                            "type": "string",
                            "description": "Flag 字符串"
                        }
                    },
                    "required": ["challenge_id", "flag"]
                }),
            },
            McpToolSnapshot {
                server_name: "ctf2".into(),
                original_name: "list_challenges".into(),
                mcp_name: "mcp_ctf2_list_challenges".into(),
                description: "获取题目列表".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "category": {
                            "type": "string",
                            "description": "题目分类"
                        }
                    }
                }),
            },
            McpToolSnapshot {
                server_name: "burp".into(),
                original_name: "get_proxy_history".into(),
                mcp_name: "mcp_burp_get_proxy_history".into(),
                description: "获取 Burp Suite 代理抓包历史记录".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "limit": {
                            "type": "integer",
                            "description": "返回条数",
                            "default": 10
                        }
                    }
                }),
            },
        ];

        let list_tool = McpToolsListTool::new(Arc::new(snapshots));
        let ctx = ToolCtx::new(std::env::temp_dir(), vec![], None, vec![]);

        // 1. 无过滤词，返回所有工具及格式化信息
        let out_all = list_tool.run(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!out_all.is_error);
        assert!(out_all.content.contains("mcp_ctf2_submit_flag"));
        assert!(out_all.content.contains("mcp_ctf2_list_challenges"));
        assert!(out_all.content.contains("mcp_burp_get_proxy_history"));
        assert!(out_all
            .content
            .contains("简写: ctf2_submit_flag / submit_flag"));
        assert!(out_all
            .content
            .contains("`challenge_id` (必填, 类型: integer) - 题目 ID"));
        assert!(out_all
            .content
            .contains("`limit` (可选, 类型: integer, 默认值: 10) - 返回条数"));

        // 2. 按 server="ctf2" 过滤，仅包含 ctf2 工具
        let out_ctf2 = list_tool
            .run(serde_json::json!({"server": "ctf2"}), &ctx)
            .await
            .unwrap();
        assert!(out_ctf2.content.contains("mcp_ctf2_submit_flag"));
        assert!(out_ctf2.content.contains("mcp_ctf2_list_challenges"));
        assert!(!out_ctf2.content.contains("mcp_burp_get_proxy_history"));

        // 3. 按 query="flag" 过滤，仅包含 submit_flag
        let out_flag = list_tool
            .run(serde_json::json!({"query": "flag"}), &ctx)
            .await
            .unwrap();
        assert!(out_flag.content.contains("mcp_ctf2_submit_flag"));
        assert!(!out_flag.content.contains("mcp_ctf2_list_challenges"));
        assert!(!out_flag.content.contains("mcp_burp_get_proxy_history"));

        // 4. 过滤无匹配项
        let out_empty = list_tool
            .run(serde_json::json!({"query": "nonexistent"}), &ctx)
            .await
            .unwrap();
        assert!(out_empty.content.contains("未找到匹配的 MCP 工具"));
    }

    #[tokio::test]
    async fn mcp_tools_list_from_mcp_tools() {
        let conn = make_conn("ctf2");
        let tool = McpTool::new(
            conn,
            McpToolSchema {
                name: "submit_flag".into(),
                description: "提交 flag".into(),
                input_schema: serde_json::json!({"type": "object"}),
            },
        );
        let list_tool = McpToolsListTool::from_mcp_tools(&[tool]);
        assert_eq!(list_tool.tools.len(), 1);
        assert_eq!(list_tool.tools[0].server_name, "ctf2");
        assert_eq!(list_tool.tools[0].original_name, "submit_flag");
        assert_eq!(list_tool.tools[0].mcp_name, "mcp_ctf2_submit_flag");
    }
}
