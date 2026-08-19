//! TOML 定义的 shell 工具包装。

use std::future::Future;
use std::pin::Pin;

use cyber_core::CustomToolConfig;
use serde_json::{json, Map, Value};
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
            let value = input
                .get(&param.name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| param.default.clone())
                .unwrap_or_default();
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
        assert_eq!(schema.parameters["properties"]["optional"]["default"], "fallback");
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
}
