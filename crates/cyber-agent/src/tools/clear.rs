//! clear 工具：清空当前任务清单（Todo List）。
//!
//! 支持 Agent 在多步骤任务全部完成、阶段重置或重新规划时直接清空 Todo 任务列表。

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use cyber_core::TodoItem;
use serde_json::{json, Value};

use crate::error::Result;
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};

/// 清空任务清单工具。
#[derive(Clone)]
pub struct ClearTool {
    todos: Arc<Mutex<Vec<TodoItem>>>,
}

impl Default for ClearTool {
    fn default() -> Self {
        Self {
            todos: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl ClearTool {
    pub fn new(todos: Arc<Mutex<Vec<TodoItem>>>) -> Self {
        Self { todos }
    }

    /// 获取共享的任务项列表。
    pub fn todos(&self) -> Arc<Mutex<Vec<TodoItem>>> {
        Arc::clone(&self.todos)
    }
}

impl Tool for ClearTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "clear".into(),
            description: "清空当前任务清单（Todo List）。当任务已结束、需要重置所有待办步骤或开启全新任务规划时调用。".into(),
            tags: vec!["task".into(), "planning".into(), "todo".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "reason": {
                        "type": "string",
                        "description": "清空待办列表的原因或说明（可选）"
                    }
                }
            }),
        }
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let mut list = match self.todos.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            let count = list.len();
            list.clear();

            let reason = input
                .get("reason")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());

            let content = match (count, reason) {
                (0, Some(r)) => format!("任务清单本就为空，无需清空。说明：{r}"),
                (0, None) => "任务清单已清空（当前清单为空）。".to_string(),
                (n, Some(r)) => format!("任务清单已清空（已清除 {n} 项任务）。说明：{r}"),
                (n, None) => format!("任务清单已清空（已清除 {n} 项任务）。"),
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
    use cyber_core::TodoStatus;

    fn setup_clear_tool() -> (ClearTool, Arc<Mutex<Vec<TodoItem>>>) {
        let items = Arc::new(Mutex::new(Vec::new()));
        let tool = ClearTool::new(items.clone());
        (tool, items)
    }
    #[tokio::test]
    async fn test_clear_tool_empty() {
        let (tool, items) = setup_clear_tool();
        let ctx = ToolCtx::new(std::path::PathBuf::from("."), Vec::new(), None, Vec::new());

        let out = tool.run(json!({}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("任务清单已清空"));
        let list = match items.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_clear_tool_with_items_and_reason() {
        let (tool, items) = setup_clear_tool();
        let ctx = ToolCtx::new(std::path::PathBuf::from("."), Vec::new(), None, Vec::new());

        {
            let mut list = match items.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            list.push(TodoItem::new("1", "收集信息", TodoStatus::Completed));
            list.push(TodoItem::new("2", "漏洞验证", TodoStatus::InProgress));
        }

        let out = tool
            .run(
                json!({
                    "reason": "进入下一阶段"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("已清除 2 项任务"));
        assert!(out.content.contains("进入下一阶段"));
        let list = match items.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn test_clear_tool_shares_state_with_todo_tool() {
        use crate::tools::TodoTool;

        let items = Arc::new(Mutex::new(Vec::new()));
        let todo_tool = TodoTool::new(items.clone());
        let clear_tool = ClearTool::new(items.clone());
        let ctx = ToolCtx::new(std::path::PathBuf::from("."), Vec::new(), None, Vec::new());

        // 使用 todo 工具添加任务
        todo_tool
            .run(
                json!({
                    "action": "add",
                    "title": "测试任务"
                }),
                &ctx,
            )
            .await
            .unwrap();

        {
            let list = match items.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            assert_eq!(list.len(), 1);
        }

        // 使用 clear 工具清空
        let out = clear_tool.run(json!({}), &ctx).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("已清除 1 项任务"));
        let list = match items.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        assert!(list.is_empty());
    }
}
