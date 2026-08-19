//! 按标签发现已注册工具。

use std::future::Future;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::Result;
use crate::tool::{Tool, ToolCatalog, ToolCtx, ToolOutput, ToolSchema};

pub struct SearchToolsTool {
    catalog: ToolCatalog,
}

impl SearchToolsTool {
    pub fn new(catalog: ToolCatalog) -> Self {
        Self { catalog }
    }
}

impl Tool for SearchToolsTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "search_tools".into(),
            description: "按标签搜索已注册工具。推荐标签：ctf、recon、web、pwn、crypto、misc。tag 为空时列出所有带标签工具。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tag": {
                        "type": "string",
                        "description": "要搜索的标签；为空时列出所有带标签工具"
                    }
                }
            }),
            tags: vec!["meta".into()],
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        let query = input
            .get("tag")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|tag| !tag.is_empty())
            .map(str::to_lowercase);
        let schemas = self.catalog.read().unwrap_or_else(|e| e.into_inner()).clone();
        Box::pin(async move {
            let matches: Vec<_> = schemas
                .into_iter()
                .filter(|schema| !schema.tags.is_empty())
                .filter(|schema| {
                    query.as_ref().is_none_or(|tag| {
                        schema
                            .tags
                            .iter()
                            .any(|candidate| candidate.to_lowercase().contains(tag))
                    })
                })
                .collect();
            let content = if matches.is_empty() {
                "未找到带该标签的工具。可在 ~/.cyber/tools/*.toml 中定义带 tags 的自定义工具。"
                    .into()
            } else {
                matches
                    .into_iter()
                    .map(|schema| {
                        format!(
                            "- **{}** [{}]: {}",
                            schema.name,
                            schema.tags.join(", "),
                            schema.description
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolRegistry;

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            rules: vec![],
            scope: None,
            env: vec![],
        }
    }

    #[tokio::test]
    async fn matches_tags_case_insensitively_by_substring() {
        let mut registry = ToolRegistry::new();
        let catalog = registry.catalog();
        registry.register(Box::new(SearchToolsTool::new(catalog)));
        let tool = registry.get("search_tools").unwrap();
        let out = tool.run(json!({"tag": "ET"}), &ctx()).await.unwrap();
        assert!(out.content.contains("search_tools"));
    }
}
