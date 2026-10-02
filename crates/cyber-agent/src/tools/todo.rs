//! todo 工具：结构化任务与步骤管理。
//!
//! 支持模型与用户共同维护多步骤任务清单，实时追踪执行状态（pending、in_progress、completed、failed）。

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use cyber_core::{TodoItem, TodoStatus};
use serde_json::{json, Value};

use crate::error::Result;
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};

/// Todo 任务管理工具。
#[derive(Clone)]
pub struct TodoTool {
    todos: Arc<Mutex<Vec<TodoItem>>>,
}

impl Default for TodoTool {
    fn default() -> Self {
        Self {
            todos: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl TodoTool {
    pub fn new(todos: Arc<Mutex<Vec<TodoItem>>>) -> Self {
        Self { todos }
    }

    /// 获取共享的任务项列表。
    pub fn todos(&self) -> Arc<Mutex<Vec<TodoItem>>> {
        Arc::clone(&self.todos)
    }

    /// 生成下一个自增 ID。
    fn next_id(items: &[TodoItem]) -> String {
        let max_numeric = items
            .iter()
            .filter_map(|item| item.id.parse::<usize>().ok())
            .max()
            .unwrap_or(0);
        (max_numeric + 1).to_string()
    }

    /// 格式化 Todo 任务列表与统计。
    pub fn format_todos(items: &[TodoItem]) -> String {
        if items.is_empty() {
            return "任务清单为空（可通过 todo action=add 添加任务）".to_string();
        }

        let total = items.len();
        let completed = items
            .iter()
            .filter(|i| i.status == TodoStatus::Completed)
            .count();
        let in_progress = items
            .iter()
            .filter(|i| i.status == TodoStatus::InProgress)
            .count();
        let failed = items
            .iter()
            .filter(|i| i.status == TodoStatus::Failed)
            .count();
        let pending = items
            .iter()
            .filter(|i| i.status == TodoStatus::Pending)
            .count();

        let mut out =
            format!("任务清单 [{completed}/{total} 已完成, {in_progress} 进行中, {pending} 待办");
        if failed > 0 {
            out.push_str(&format!(", {failed} 失败"));
        }
        out.push_str("]:\n");

        for item in items {
            out.push_str(&format!(
                "{} #{} {}",
                item.status.symbol(),
                item.id,
                item.title
            ));
            if let Some(notes) = &item.notes {
                if !notes.trim().is_empty() {
                    out.push_str(&format!(" (备注: {})", notes.trim()));
                }
            }
            out.push('\n');
        }

        out.trim_end().to_string()
    }
}

impl Tool for TodoTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "todo".into(),
            description: "结构化任务清单管理工具。用于多步骤任务规划、状态跟踪与进度展示。执行多步骤任务前请先添加计划清单，开始子任务时更新为 in_progress，完成后更新为 completed，遇到卡点更新为 failed。".into(),
            tags: vec!["task".into(), "planning".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "add", "update", "remove", "clear"],
                        "description": "操作类型：list 查看清单，add 添加任务，update 更新状态/标题/备注，remove 删除任务，clear 清空清单"
                    },
                    "id": {
                        "type": "string",
                        "description": "任务 ID（update 和 remove 操作必填）"
                    },
                    "title": {
                        "type": "string",
                        "description": "任务标题/描述（add 操作必填，update 可选修改标题）"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "completed", "failed"],
                        "description": "任务状态：pending 未开始，in_progress 进行中，completed 已完成，failed 失败"
                    },
                    "notes": {
                        "type": "string",
                        "description": "任务补充说明、卡点原因或执行备注"
                    },
                    "items": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "title": { "type": "string" },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed", "failed"]
                                },
                                "notes": { "type": "string" }
                            },
                            "required": ["title"]
                        },
                        "description": "批量添加的任务列表（用于 add 操作批量规划步骤）"
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
            let action = input
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    if input.get("items").is_some() || input.get("title").is_some() {
                        "add"
                    } else {
                        "list"
                    }
                });

            let mut list = match self.todos.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };

            match action {
                "list" => {
                    let formatted = Self::format_todos(&list);
                    Ok(ToolOutput {
                        content: formatted,
                        is_error: false,
                    })
                }
                "add" => {
                    let mut added_count = 0;

                    // 1. 批量添加
                    if let Some(items) = input.get("items").and_then(Value::as_array) {
                        for item in items {
                            if let Some(title) = item.get("title").and_then(Value::as_str) {
                                let id = item
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string)
                                    .unwrap_or_else(|| Self::next_id(&list));
                                let status = item
                                    .get("status")
                                    .and_then(Value::as_str)
                                    .and_then(TodoStatus::parse)
                                    .unwrap_or(TodoStatus::Pending);
                                let notes = item
                                    .get("notes")
                                    .and_then(Value::as_str)
                                    .map(ToString::to_string);

                                if !title.trim().is_empty() {
                                    list.push(TodoItem {
                                        id,
                                        title: title.trim().to_string(),
                                        status,
                                        notes,
                                    });
                                    added_count += 1;
                                }
                            } else if let Some(title_str) = item.as_str() {
                                if !title_str.trim().is_empty() {
                                    let id = Self::next_id(&list);
                                    list.push(TodoItem::new(
                                        id,
                                        title_str.trim(),
                                        TodoStatus::Pending,
                                    ));
                                    added_count += 1;
                                }
                            }
                        }
                    }

                    // 2. 单条添加（若指定了 title）
                    if let Some(title) = input.get("title").and_then(Value::as_str) {
                        let id = input
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToString::to_string)
                            .unwrap_or_else(|| Self::next_id(&list));
                        let status = input
                            .get("status")
                            .and_then(Value::as_str)
                            .and_then(TodoStatus::parse)
                            .unwrap_or(TodoStatus::Pending);
                        let notes = input
                            .get("notes")
                            .and_then(Value::as_str)
                            .map(ToString::to_string);

                        if !title.trim().is_empty() {
                            list.push(TodoItem {
                                id,
                                title: title.trim().to_string(),
                                status,
                                notes,
                            });
                            added_count += 1;
                        }
                    }
                    if added_count == 0 {
                        return Ok(ToolOutput {
                            content: "添加任务失败：缺少 title 或 items 参数".to_string(),
                            is_error: true,
                        });
                    }

                    let formatted = Self::format_todos(&list);
                    Ok(ToolOutput {
                        content: format!("已成功添加 {added_count} 项任务。\n\n{formatted}"),
                        is_error: false,
                    })
                }
                "update" => {
                    let id = match input.get("id").and_then(Value::as_str) {
                        Some(id) if !id.trim().is_empty() => id.trim(),
                        _ => {
                            return Ok(ToolOutput {
                                content: "更新任务失败：必须提供任务 id 参数".to_string(),
                                is_error: true,
                            })
                        }
                    };

                    let item = match list.iter_mut().find(|i| i.id == id) {
                        Some(item) => item,
                        None => {
                            return Ok(ToolOutput {
                                content: format!("更新任务失败：未找到 ID 为 '{id}' 的任务"),
                                is_error: true,
                            })
                        }
                    };

                    if let Some(status_str) = input.get("status").and_then(Value::as_str) {
                        if let Some(st) = TodoStatus::parse(status_str) {
                            item.status = st;
                        } else {
                            return Ok(ToolOutput {
                                content: format!("无效的任务状态: '{status_str}'，可选值为 pending / in_progress / completed / failed"),
                                is_error: true,
                            });
                        }
                    }

                    if let Some(title) = input.get("title").and_then(Value::as_str) {
                        if !title.trim().is_empty() {
                            item.title = title.trim().to_string();
                        }
                    }

                    if let Some(notes) = input.get("notes").and_then(Value::as_str) {
                        item.notes = Some(notes.trim().to_string());
                    }

                    let formatted = Self::format_todos(&list);
                    Ok(ToolOutput {
                        content: format!("任务 #{id} 已更新。\n\n{formatted}"),
                        is_error: false,
                    })
                }
                "remove" | "delete" => {
                    let id = match input.get("id").and_then(Value::as_str) {
                        Some(id) if !id.trim().is_empty() => id.trim(),
                        _ => {
                            return Ok(ToolOutput {
                                content: "删除任务失败：必须提供任务 id 参数".to_string(),
                                is_error: true,
                            })
                        }
                    };

                    let prev_len = list.len();
                    list.retain(|i| i.id != id);

                    if list.len() == prev_len {
                        return Ok(ToolOutput {
                            content: format!("删除任务失败：未找到 ID 为 '{id}' 的任务"),
                            is_error: true,
                        });
                    }

                    let formatted = Self::format_todos(&list);
                    Ok(ToolOutput {
                        content: format!("任务 #{id} 已删除。\n\n{formatted}"),
                        is_error: false,
                    })
                }
                "clear" => {
                    list.clear();
                    Ok(ToolOutput {
                        content: "任务清单已清空。".to_string(),
                        is_error: false,
                    })
                }
                unknown => Ok(ToolOutput {
                    content: format!(
                        "未知操作: '{unknown}'，支持的操作有 list, add, update, remove, clear"
                    ),
                    is_error: true,
                }),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_tool() -> (TodoTool, Arc<Mutex<Vec<TodoItem>>>) {
        let items = Arc::new(Mutex::new(Vec::new()));
        let tool = TodoTool::new(items.clone());
        (tool, items)
    }

    #[tokio::test]
    async fn test_todo_tool_add_single_and_list() {
        let (tool, items) = setup_tool();
        let ctx = ToolCtx::new(std::path::PathBuf::from("."), Vec::new(), None, Vec::new());

        let out = tool
            .run(
                json!({
                    "action": "add",
                    "title": " reconnaissance ",
                    "status": "pending"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("#1 reconnaissance"));

        {
            let guard = items.lock().unwrap();
            assert_eq!(guard.len(), 1);
            assert_eq!(guard[0].id, "1");
            assert_eq!(guard[0].title, "reconnaissance");
            assert_eq!(guard[0].status, TodoStatus::Pending);
        }

        let out_list = tool.run(json!({ "action": "list" }), &ctx).await.unwrap();
        assert!(!out_list.is_error);
        assert!(out_list.content.contains("[ ] #1 reconnaissance"));
    }

    #[tokio::test]
    async fn test_todo_tool_batch_add() {
        let (tool, items) = setup_tool();
        let ctx = ToolCtx::new(std::path::PathBuf::from("."), Vec::new(), None, Vec::new());

        let out = tool
            .run(
                json!({
                    "action": "add",
                    "items": [
                        { "title": "Step 1: Port scan", "status": "in_progress" },
                        { "title": "Step 2: Web exploit", "notes": "Target port 8080" },
                        "Step 3: Post exploitation"
                    ]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("已成功添加 3 项任务"));

        let guard = items.lock().unwrap();
        assert_eq!(guard.len(), 3);
        assert_eq!(guard[0].status, TodoStatus::InProgress);
        assert_eq!(guard[1].status, TodoStatus::Pending);
        assert_eq!(guard[1].notes.as_deref(), Some("Target port 8080"));
        assert_eq!(guard[2].status, TodoStatus::Pending);
    }

    #[tokio::test]
    async fn test_todo_tool_update_and_remove() {
        let (tool, _) = setup_tool();
        let ctx = ToolCtx::new(std::path::PathBuf::from("."), Vec::new(), None, Vec::new());

        // Add
        tool.run(
            json!({
                "action": "add",
                "title": "Initial step"
            }),
            &ctx,
        )
        .await
        .unwrap();

        // Update status to in_progress
        let out_up = tool
            .run(
                json!({
                    "action": "update",
                    "id": "1",
                    "status": "in_progress",
                    "notes": "Working on it"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out_up.is_error);
        assert!(out_up.content.contains("[>] #1 Initial step"));
        assert!(out_up.content.contains("Working on it"));

        // Update status to completed
        let out_comp = tool
            .run(
                json!({
                    "action": "update",
                    "id": "1",
                    "status": "completed"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out_comp.is_error);
        assert!(out_comp.content.contains("[x] #1 Initial step"));

        // Remove
        let out_rm = tool
            .run(
                json!({
                    "action": "remove",
                    "id": "1"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!out_rm.is_error);
        assert!(out_rm.content.contains("任务 #1 已删除"));

        // List should be empty
        let out_empty = tool.run(json!({ "action": "list" }), &ctx).await.unwrap();
        assert!(out_empty.content.contains("任务清单为空"));
    }

    #[tokio::test]
    async fn test_todo_tool_clear() {
        let (tool, items) = setup_tool();
        let ctx = ToolCtx::new(std::path::PathBuf::from("."), Vec::new(), None, Vec::new());

        tool.run(
            json!({
                "action": "add",
                "title": "Task A"
            }),
            &ctx,
        )
        .await
        .unwrap();

        tool.run(json!({ "action": "clear" }), &ctx).await.unwrap();

        let guard = items.lock().unwrap();
        assert!(guard.is_empty());
    }
}
