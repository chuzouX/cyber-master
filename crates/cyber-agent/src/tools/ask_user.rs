//! `ask_user` 工具：向用户发起交互式决策与需求澄清提问。
//!
//! 当模型推理中遇到需求模糊、技术架构分支选型、参数配置缺失等不确定性时，
//! 不应武断猜测，而应通过此工具发起 1 个或多个结构化问题。

use std::any::Any;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::error::Result;
use crate::question::{QuestionBroker, QuestionItem, QuestionResponse};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};

/// 需求澄清与决策提问工具
#[derive(Clone)]
pub struct AskUserTool {
    broker: Arc<QuestionBroker>,
}

impl Default for AskUserTool {
    fn default() -> Self {
        Self {
            broker: QuestionBroker::headless(),
        }
    }
}

impl AskUserTool {
    pub fn new(broker: Arc<QuestionBroker>) -> Self {
        Self { broker }
    }

    pub fn broker(&self) -> Arc<QuestionBroker> {
        Arc::clone(&self.broker)
    }

    /// 格式化用户答复为模型可读的提示文本
    pub fn format_response(questions: &[QuestionItem], response: &QuestionResponse) -> String {
        if response.cancelled {
            return "用户跳过了提问交互，未指定选项。请根据最佳工程实践自行推断并说明原因。"
                .to_string();
        }

        if response.answers.is_empty() {
            return "用户已确认，但未作具体选择。请根据最佳实践继续。".to_string();
        }

        let mut lines = vec!["用户已完成决策确认：".to_string()];

        for (idx, ans) in response.answers.iter().enumerate() {
            let num = idx + 1;
            // 匹配原问题项以提取分类标签和推荐标记
            let orig = questions.iter().find(|q| q.id == ans.id);
            let header_str = orig
                .and_then(|q| q.header.as_deref())
                .map(|h| format!("[{h}] "))
                .unwrap_or_default();

            lines.push(format!("{num}. {header_str}{}:", ans.question));

            let mut selected_labels: Vec<String> = Vec::new();
            for sel in &ans.selected {
                let is_rec = orig
                    .and_then(|q| q.options.iter().find(|o| &o.label == sel))
                    .is_some_and(|o| o.recommended);
                if is_rec {
                    selected_labels.push(format!("{sel} (推荐)"));
                } else {
                    selected_labels.push(sel.clone());
                }
            }

            let sel_text = if selected_labels.is_empty() {
                None
            } else {
                Some(selected_labels.join(", "))
            };

            let detail = match (sel_text, ans.custom.as_deref()) {
                (Some(s), Some(c)) if !c.trim().is_empty() => {
                    format!("   - 选中: {}, 补充说明: \"{}\"", s, c.trim())
                }
                (Some(s), _) => {
                    format!("   - 选中: {s}")
                }
                (None, Some(c)) if !c.trim().is_empty() => {
                    format!("   - 补充说明: \"{}\"", c.trim())
                }
                (None, _) => "   - 未指定具体选项".to_string(),
            };
            lines.push(detail);
        }

        lines.join("\n")
    }
}

impl Tool for AskUserTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ask_user".into(),
            description: "向用户发起需求澄清或决策提问。当用户目标模糊、有多种关键架构/工具选型方案、需要补充关键配置时使用。支持单题/多题，支持推荐标记和单选/多选。".into(),
            tags: vec!["interactive".into(), "clarify".into(), "decision".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "向用户提出的问题列表（支持单题或多题）",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "问题唯一标识符，例如 auth_type, db_choice"
                                },
                                "header": {
                                    "type": "string",
                                    "description": "可选的问题分类标签，如 架构选型, 存储方案"
                                },
                                "question": {
                                    "type": "string",
                                    "description": "向用户提出的具体问题内容"
                                },
                                "options": {
                                    "type": "array",
                                    "description": "供用户选择的备选项列表（通常提供 2-4 个选项）",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "选项简短标题"
                                            },
                                            "description": {
                                                "type": "string",
                                                "description": "该选项的详细权衡、利弊分析或说明"
                                            },
                                            "recommended": {
                                                "type": "boolean",
                                                "description": "是否为推荐选项（模型推荐的首选方案）"
                                            }
                                        },
                                        "required": ["label"]
                                    }
                                },
                                "multi": {
                                    "type": "boolean",
                                    "description": "是否允许多选，默认为 false（单选）"
                                }
                            },
                            "required": ["id", "question", "options"]
                        }
                    }
                },
                "required": ["questions"]
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
            let questions_val = match input.get("questions") {
                Some(v) => v,
                None => {
                    return Ok(ToolOutput {
                        content: "参数错误：缺少必填字段 questions。".into(),
                        is_error: true,
                    });
                }
            };

            let questions: Vec<QuestionItem> = match serde_json::from_value(questions_val.clone()) {
                Ok(q) => q,
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("参数解析错误：questions 格式无效 ({e})"),
                        is_error: true,
                    });
                }
            };

            if questions.is_empty() {
                return Ok(ToolOutput {
                    content: "提问列表为空，无需发起用户交互。".into(),
                    is_error: false,
                });
            }

            let response = self.broker.ask(questions.clone()).await;
            let formatted = Self::format_response(&questions, &response);

            Ok(ToolOutput {
                content: formatted,
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::QuestionAnswer;
    use std::path::PathBuf;

    fn test_ctx() -> ToolCtx {
        ToolCtx::new(PathBuf::from("."), Vec::new(), None, Vec::new())
    }

    #[tokio::test]
    async fn test_ask_user_tool_headless_auto_picks_recommended() {
        let tool = AskUserTool::default();
        assert!(!tool.broker().is_interactive());

        let input = json!({
            "questions": [
                {
                    "id": "arch",
                    "header": "技术选型",
                    "question": "请选择后端架构方案",
                    "options": [
                        { "label": "Session Cookie", "description": "传统会话", "recommended": false },
                        { "label": "JWT Token 方案", "description": "无状态认证", "recommended": true },
                        { "label": "OAuth2", "description": "第三方认证", "recommended": false }
                    ],
                    "multi": false
                },
                {
                    "id": "modules",
                    "header": "功能模块",
                    "question": "请勾选需启用的安全模块",
                    "options": [
                        { "label": "Web 漏洞扫描", "recommended": true },
                        { "label": "二进制逆向", "recommended": false },
                        { "label": "流量审计", "recommended": true }
                    ],
                    "multi": true
                },
                {
                    "id": "db",
                    "question": "选择数据库",
                    "options": [
                        { "label": "PostgreSQL", "recommended": false },
                        { "label": "SQLite", "recommended": false }
                    ],
                    "multi": false
                }
            ]
        });

        let ctx = test_ctx();
        let output = tool.run(input, &ctx).await.unwrap();
        assert!(!output.is_error);
        let content = output.content;

        // 1. 验证单选推荐项命中
        assert!(content.contains("1. [技术选型] 请选择后端架构方案:"));
        assert!(content.contains("选中: JWT Token 方案 (推荐)"));

        // 2. 验证多选推荐项全选命中
        assert!(content.contains("2. [功能模块] 请勾选需启用的安全模块:"));
        assert!(content.contains("Web 漏洞扫描 (推荐)"));
        assert!(content.contains("流量审计 (推荐)"));

        // 3. 验证无推荐项时首项命中
        assert!(content.contains("3. 选择数据库:"));
        assert!(content.contains("选中: PostgreSQL"));
    }

    #[tokio::test]
    async fn test_ask_user_tool_interactive_response_roundtrip() {
        let (broker, mut requests) = QuestionBroker::interactive();
        assert!(broker.is_interactive());
        let tool = AskUserTool::new(broker);

        let worker = tokio::spawn(async move {
            let req = requests.recv().await.expect("should receive request");
            assert_eq!(req.questions.len(), 2);
            assert_eq!(req.questions[0].id, "arch");
            assert_eq!(req.questions[1].id, "db");

            let response = QuestionResponse {
                answers: vec![
                    QuestionAnswer {
                        id: "arch".into(),
                        question: "请选择后端架构方案".into(),
                        selected: vec!["JWT Token 方案".into()],
                        custom: None,
                    },
                    QuestionAnswer {
                        id: "db".into(),
                        question: "请选择持久化数据库".into(),
                        selected: vec!["PostgreSQL".into()],
                        custom: Some("需要启用 pgvector 扩展".into()),
                    },
                ],
                cancelled: false,
            };
            req.reply.send(response).unwrap();
        });

        let input = json!({
            "questions": [
                {
                    "id": "arch",
                    "header": "技术选型",
                    "question": "请选择后端架构方案",
                    "options": [
                        { "label": "JWT Token 方案", "recommended": true },
                        { "label": "Session Cookie", "recommended": false }
                    ]
                },
                {
                    "id": "db",
                    "header": "数据库",
                    "question": "请选择持久化数据库",
                    "options": [
                        { "label": "PostgreSQL", "recommended": false },
                        { "label": "MySQL", "recommended": false }
                    ]
                }
            ]
        });

        let ctx = test_ctx();
        let output = tool.run(input, &ctx).await.unwrap();
        worker.await.unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("用户已完成决策确认："));
        assert!(output.content.contains("1. [技术选型] 请选择后端架构方案:"));
        assert!(output.content.contains("- 选中: JWT Token 方案 (推荐)"));
        assert!(output.content.contains("2. [数据库] 请选择持久化数据库:"));
        assert!(output
            .content
            .contains("- 选中: PostgreSQL, 补充说明: \"需要启用 pgvector 扩展\""));
    }

    #[tokio::test]
    async fn test_ask_user_tool_cancelled_returns_fallback_hint() {
        let (broker, mut requests) = QuestionBroker::interactive();
        let tool = AskUserTool::new(broker);

        tokio::spawn(async move {
            let req = requests.recv().await.unwrap();
            let _ = req.reply.send(QuestionResponse {
                answers: vec![],
                cancelled: true,
            });
        });

        let input = json!({
            "questions": [
                {
                    "id": "test",
                    "question": "随意测试",
                    "options": [{ "label": "A" }]
                }
            ]
        });

        let output = tool.run(input, &test_ctx()).await.unwrap();
        assert!(!output.is_error);
        assert_eq!(
            output.content,
            "用户跳过了提问交互，未指定选项。请根据最佳工程实践自行推断并说明原因。"
        );
    }
}
