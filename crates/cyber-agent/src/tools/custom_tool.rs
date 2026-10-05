//! TOML 定义的 shell 工具包装。

use cyber_core::CustomToolConfig;
use serde_json::{json, Map, Value};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

use crate::error::Result;
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::shell::ShellTool;

pub struct CustomTool {
    config: CustomToolConfig,
}

impl CustomTool {
    pub fn new(config: CustomToolConfig) -> Self {
        Self { config }
    }

    fn substitute_command(&self, input: &Value) -> String {
        let mut command = self.config.command.clone();
        for param in &self.config.parameters {
            let mut value = input
                .get(&param.name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| param.default.clone())
                .unwrap_or_default();

            // 智能兼容：若参数为 args 但调用方传入了 input/detect/recipe，自动组装为 CLI 参数
            if param.name == "args" && value.is_empty() {
                if let Some(target_input) = input.get("input").and_then(Value::as_str) {
                    let mut parts = vec![format!("\"{}\"", target_input.replace('"', "\\\""))];
                    let is_detect = input
                        .get("detect")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if is_detect {
                        parts.push("detect".to_string());
                    }
                    if let Some(recipe) = input.get("recipe").and_then(Value::as_array) {
                        for item in recipe {
                            if let Some(op) = item.as_str() {
                                parts.push(op.to_string());
                            }
                        }
                    }
                    if parts.len() == 1 {
                        parts.push("detect".to_string());
                    }
                    value = parts.join(" ");
                }
            }

            command = command.replace(&format!("{{{}}}", param.name), &value);
        }
        command
    }
}

impl Tool for CustomTool {
    fn schema(&self) -> ToolSchema {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for param in &self.config.parameters {
            let mut property = Map::new();
            property.insert("type".into(), json!("string"));
            property.insert("description".into(), json!(param.description));
            if let Some(default) = &param.default {
                property.insert("default".into(), json!(default));
            }
            properties.insert(param.name.clone(), Value::Object(property));
            if param.required {
                required.push(Value::String(param.name.clone()));
            }
        }
        ToolSchema {
            name: format!("custom_{}", self.config.name),
            description: self.config.description.clone(),
            parameters: json!({
                "type": "object",
                "properties": properties,
                "required": required,
            }),
            tags: self.config.tags.clone(),
        }
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
        let command = self.substitute_command(&input);
        Box::pin(async move {
            ShellTool::unchecked()
                .run_streaming(json!({ "command": command }), ctx, progress)
                .await
        })
    }
}

pub struct CustomToolsListTool {
    tools: Arc<Vec<CustomToolConfig>>,
}

impl CustomToolsListTool {
    pub fn new(tools: Arc<Vec<CustomToolConfig>>) -> Self {
        Self { tools }
    }

    pub fn empty() -> Self {
        Self {
            tools: Arc::new(Vec::new()),
        }
    }
}

impl Tool for CustomToolsListTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "custom_tools_list".into(),
            description: "获取系统中所有已配置的自定义安全工具（Custom Tools）清单。最高优先级工具发现入口（优先级：custom_tools_list > mcp_tools_list > 自己做）。当需要执行特定安全任务（如逆向分析/反编译/反汇编、漏洞利用、密码破解、专项扫描、编码解密等）但默认工具列表未列出时，必须优先调用此工具从 custom_* 中查找已有工具，能直接找到工具使用的绝不要自己翻找环境，更不要自己重写工具。获取后可直接以对应工具名称（如 custom_<name> 或简写）发起调用，或使用 shell 工具执行填入参数后的具体命令。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "可选过滤关键词（按工具名称、用途描述或标签过滤）。留空则返回所有已配置的自定义工具。"
                    }
                }
            }),
            tags: vec!["meta".into(), "custom".into(), "security".into()],
        }
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
        _ctx: &'a ToolCtx,
        _progress: Option<UnboundedSender<String>>,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        if self.tools.is_empty() {
            return Box::pin(async {
                Ok(ToolOutput {
                    content: "系统中当前未配置任何自定义工具。可通过系统设置向导或在 ~/.cyber/tools/*.toml 中添加工具。".to_string(),
                    is_error: false,
                })
            });
        }

        let query = input
            .get("query")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_lowercase);

        let matches: Vec<&CustomToolConfig> = self
            .tools
            .iter()
            .filter(|tool| {
                if let Some(q) = &query {
                    tool.name.to_lowercase().contains(q)
                        || tool.description.to_lowercase().contains(q)
                        || tool.command.to_lowercase().contains(q)
                        || tool.tags.iter().any(|t| t.to_lowercase().contains(q))
                } else {
                    true
                }
            })
            .collect();

        if matches.is_empty() {
            let msg = if let Some(q) = &query {
                format!(
                    "未找到与 \"{q}\" 匹配的自定义工具（系统中共有 {} 个自定义工具）。可清空 query 重新查询以列出全部工具。",
                    self.tools.len()
                )
            } else {
                "系统中当前未配置任何自定义工具。可通过系统设置向导或在 ~/.cyber/tools/*.toml 中添加工具。".to_string()
            };
            return Box::pin(async move {
                Ok(ToolOutput {
                    content: msg,
                    is_error: false,
                })
            });
        }

        let mut output = String::new();
        output.push_str(&format!(
            "# 自定义工具清单 (共找到 {} 个工具)\n\n",
            matches.len()
        ));
        for (idx, tool) in matches.iter().enumerate() {
            output.push_str(&format!(
                "### {}. custom_{} (简写: {})\n",
                idx + 1,
                tool.name,
                tool.name
            ));
            output.push_str(&format!("- **功能描述**: {}\n", tool.description));
            let tags_str = if tool.tags.is_empty() {
                "无".to_string()
            } else {
                tool.tags.join(", ")
            };
            output.push_str(&format!("- **标签**: [{}]\n", tags_str));
            output.push_str(&format!("- **命令模板**: `{}`\n", tool.command));
            output.push_str("- **参数规格**:\n");
            if tool.parameters.is_empty() {
                output.push_str("  - (无参数)\n");
            } else {
                for param in &tool.parameters {
                    let req_str = if param.required { "必填" } else { "可选" };
                    let default_str = param
                        .default
                        .as_deref()
                        .map(|d| format!(", 默认值: \"{d}\""))
                        .unwrap_or_default();
                    output.push_str(&format!(
                        "  - `{}` ({}{}) - {}\n",
                        param.name, req_str, default_str, param.description
                    ));
                }
            }
            output.push('\n');
        }
        output.push_str("## 调用方式说明：\n");
        output.push_str("1. **直接工具调用**：可直接以 `custom_<name>` 或 `<name>` 发起工具调用，传入上述参数字典。\n");
        output.push_str("2. **通过 shell 执行**：亦可使用 `shell` 工具，根据命令模板替换对应参数后执行系统命令。\n");

        Box::pin(async move {
            Ok(ToolOutput {
                content: output,
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CustomToolConfig {
        CustomToolConfig {
            name: "echo_value".into(),
            description: "echo a value".into(),
            command: "echo {value} {optional}".into(),
            tags: vec!["ctf".into(), "misc".into()],
            parameters: vec![
                cyber_core::CustomToolParam {
                    name: "value".into(),
                    description: "value".into(),
                    required: true,
                    default: None,
                },
                cyber_core::CustomToolParam {
                    name: "optional".into(),
                    description: "optional".into(),
                    required: false,
                    default: Some("fallback".into()),
                },
            ],
        }
    }

    #[test]
    fn schema_includes_prefix_tags_and_defaults() {
        let schema = CustomTool::new(config()).schema();
        assert_eq!(schema.name, "custom_echo_value");
        assert_eq!(schema.tags, vec!["ctf", "misc"]);
        assert_eq!(schema.parameters["required"], json!(["value"]));
        assert_eq!(
            schema.parameters["properties"]["optional"]["default"],
            "fallback"
        );
    }

    #[test]
    fn substitution_uses_input_then_default_then_empty() {
        let tool = CustomTool::new(config());
        assert_eq!(
            tool.substitute_command(&json!({"value": "provided"})),
            "echo provided fallback"
        );
        assert_eq!(tool.substitute_command(&json!({})), "echo  fallback");
    }

    #[tokio::test]
    async fn custom_tools_list_schema_and_empty_output() {
        let empty_tool = CustomToolsListTool::empty();
        let schema = empty_tool.schema();
        assert_eq!(schema.name, "custom_tools_list");
        assert_eq!(schema.tags, vec!["meta", "custom", "security"]);

        let ctx = ToolCtx::new(std::env::temp_dir(), vec![], None, vec![]);
        let output = empty_tool.run(json!({}), &ctx).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("系统中当前未配置任何自定义工具"));
    }

    #[tokio::test]
    async fn custom_tools_list_filters_and_formats_tools() {
        let tools = vec![
            CustomToolConfig {
                name: "sqlmap".into(),
                description: "SQL 注入自动化检测与利用工具".into(),
                command: "sqlmap -u {url} --batch".into(),
                tags: vec!["sqli".into(), "web".into()],
                parameters: vec![cyber_core::CustomToolParam {
                    name: "url".into(),
                    description: "目标 URL".into(),
                    required: true,
                    default: None,
                }],
            },
            CustomToolConfig {
                name: "hashcat".into(),
                description: "密码哈希破解工具".into(),
                command: "hashcat -m {mode} {hash} {wordlist}".into(),
                tags: vec!["crypto".into(), "password".into()],
                parameters: vec![
                    cyber_core::CustomToolParam {
                        name: "mode".into(),
                        description: "哈希类型代码".into(),
                        required: true,
                        default: None,
                    },
                    cyber_core::CustomToolParam {
                        name: "hash".into(),
                        description: "哈希字符串或文件".into(),
                        required: true,
                        default: None,
                    },
                    cyber_core::CustomToolParam {
                        name: "wordlist".into(),
                        description: "字典路径".into(),
                        required: false,
                        default: Some("/usr/share/wordlists/rockyou.txt".into()),
                    },
                ],
            },
        ];

        let list_tool = CustomToolsListTool::new(Arc::new(tools));
        let ctx = ToolCtx::new(std::env::temp_dir(), vec![], None, vec![]);

        // 1. 无过滤词，返回所有工具及格式化信息
        let all_out = list_tool.run(json!({}), &ctx).await.unwrap();
        assert!(!all_out.is_error);
        assert!(all_out.content.contains("共找到 2 个工具"));
        assert!(all_out.content.contains("custom_sqlmap (简写: sqlmap)"));
        assert!(all_out.content.contains("custom_hashcat (简写: hashcat)"));
        assert!(all_out.content.contains("sqlmap -u {url} --batch"));
        assert!(all_out.content.contains("`url` (必填) - 目标 URL"));
        assert!(all_out
            .content
            .contains("默认值: \"/usr/share/wordlists/rockyou.txt\""));
        assert!(all_out.content.contains("直接工具调用"));

        // 2. 按标签/关键词过滤：sqli
        let sqli_out = list_tool.run(json!({"query": "sqli"}), &ctx).await.unwrap();
        assert!(!sqli_out.is_error);
        assert!(sqli_out.content.contains("共找到 1 个工具"));
        assert!(sqli_out.content.contains("custom_sqlmap"));
        assert!(!sqli_out.content.contains("custom_hashcat"));

        // 3. 不存在的关键词过滤
        let notfound_out = list_tool
            .run(json!({"query": "not_existing_keyword"}), &ctx)
            .await
            .unwrap();
        assert!(!notfound_out.is_error);
        assert!(notfound_out
            .content
            .contains("未找到与 \"not_existing_keyword\" 匹配的自定义工具"));
        assert!(notfound_out.content.contains("系统中共有 2 个自定义工具"));
    }
}
