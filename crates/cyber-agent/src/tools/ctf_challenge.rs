//! ctf_challenge 工具：agent 调用以注册/更新/解题 CTF 题目。
//!
//! 持有 `Arc<Mutex<Vec<CtfChallenge>>>` 共享状态（与 App 共享 clone），
//! 修改后持久化到 `~/.cyber/ctf/challenges.json`。App 在渲染时读取共享状态。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use cyber_core::ctf::{CtfCategory, CtfChallenge, CtfStatus};
use serde_json::{json, Value};
use tracing::warn;

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};

/// CTF 题目管理工具。
///
/// 共享状态 `challenges` 与 App 持有的 clone 指向同一份数据；
/// `ctf_dir` 用于持久化到 `challenges.json`。
pub struct CtfChallengeTool {
    challenges: Arc<Mutex<Vec<CtfChallenge>>>,
    ctf_dir: PathBuf,
}

impl CtfChallengeTool {
    pub fn new(challenges: Arc<Mutex<Vec<CtfChallenge>>>, ctf_dir: PathBuf) -> Self {
        Self {
            challenges,
            ctf_dir,
        }
    }

    /// 持久化当前 challenges 到 `challenges.json`。
    fn persist(&self) {
        let challenges = self.challenges.lock().unwrap();
        let path = self.ctf_dir.join("challenges.json");
        match serde_json::to_string_pretty(&*challenges) {
            Ok(json) => {
                if let Err(e) = std::fs::create_dir_all(&self.ctf_dir) {
                    warn!(error = %e, dir = %self.ctf_dir.display(), "CTF 目录创建失败");
                    return;
                }
                if let Err(e) = std::fs::write(&path, json) {
                    warn!(error = %e, path = %path.display(), "CTF challenges.json 写入失败");
                }
            }
            Err(e) => warn!(error = %e, "CTF challenges 序列化失败"),
        }
    }
}

impl Tool for CtfChallengeTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "ctf_challenge".into(),
            description: "CTF 题目管理工具。action=register 注册/更新题目（按 name 匹配），action=solve 标记题目已解出并记录 flag，action=list 列出所有题目。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["register", "solve", "list"],
                        "description": "操作类型"
                    },
                    "name": {
                        "type": "string",
                        "description": "题目名称（register/solve 必填）"
                    },
                    "category": {
                        "type": "string",
                        "enum": ["misc", "web", "reverse", "pwn", "crypto"],
                        "description": "题目分类（register 时使用）"
                    },
                    "description": {
                        "type": "string",
                        "description": "题目描述（register 时使用）"
                    },
                    "target": {
                        "type": "string",
                        "description": "靶机地址（register 时使用，如 nc 1.2.3.4 1234）"
                    },
                    "tags": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "标签列表（register 时使用）"
                    },
                    "flag": {
                        "type": "string",
                        "description": "Flag 值（solve 时使用）"
                    },
                    "key_points": {
                        "type": "string",
                        "description": "关键知识点/卡点（solve 时使用）"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let action = input
                .get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("ctf_challenge 缺少 action 参数".into()))?;

            match action {
                "register" => self.do_register(&input),
                "solve" => self.do_solve(&input),
                "list" => self.do_list(),
                other => Err(AgentError::Provider(format!(
                    "ctf_challenge 未知 action: {other}"
                ))),
            }
        })
    }
}

impl CtfChallengeTool {
    fn do_register(&self, input: &Value) -> Result<ToolOutput> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Provider("ctf_challenge register 缺少 name".into()))?;

        let category = input
            .get("category")
            .and_then(|v| v.as_str())
            .and_then(CtfCategory::from_str)
            .unwrap_or_default();

        let description = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let target = input
            .get("target")
            .and_then(|v| v.as_str())
            .map(String::from);

        let tags: Vec<String> = input
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let mut challenges = self.challenges.lock().unwrap();

        // 按 name 匹配：存在则更新，不存在则新增
        if let Some(c) = challenges.iter_mut().find(|c| c.name == name) {
            c.category = category;
            if !description.is_empty() {
                c.description = description.clone();
            }
            if target.is_some() {
                c.target = target.clone();
            }
            if !tags.is_empty() {
                c.tags = tags.clone();
            }
            drop(challenges);
            self.persist();
            Ok(ToolOutput {
                content: format!("已更新题目 [{category}] {name}"),
                is_error: false,
            })
        } else {
            let mut c = CtfChallenge::new(name.into(), category);
            c.description = description;
            c.target = target;
            c.tags = tags;
            challenges.push(c);
            drop(challenges);
            self.persist();
            Ok(ToolOutput {
                content: format!("已注册题目 [{}] {}", category.label(), name),
                is_error: false,
            })
        }
    }

    fn do_solve(&self, input: &Value) -> Result<ToolOutput> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AgentError::Provider("ctf_challenge solve 缺少 name".into()))?;

        let flag = input
            .get("flag")
            .and_then(|v| v.as_str())
            .map(String::from);

        let key_points = input
            .get("key_points")
            .and_then(|v| v.as_str())
            .map(String::from);

        let mut challenges = self.challenges.lock().unwrap();
        let c = challenges
            .iter_mut()
            .find(|c| c.name == name)
            .ok_or_else(|| {
                AgentError::Provider(format!("题目 {name} 不存在，无法标记为已解出"))
            })?;

        c.status = CtfStatus::Solved;
        if let Some(f) = &flag {
            c.flag = Some(f.clone());
        }
        if let Some(kp) = &key_points {
            c.key_points = Some(kp.clone());
        }
        // 记录结束时间
        c.end_time = Some(current_time_str());
        drop(challenges);
        self.persist();

        Ok(ToolOutput {
            content: format!(
                "题目 {name} 已标记为已解出！{}",
                flag.map(|f| format!(" Flag: {f}")).unwrap_or_default()
            ),
            is_error: false,
        })
    }

    fn do_list(&self) -> Result<ToolOutput> {
        let challenges = self.challenges.lock().unwrap();
        if challenges.is_empty() {
            return Ok(ToolOutput {
                content: "当前无 CTF 题目".into(),
                is_error: false,
            });
        }
        let mut lines = Vec::new();
        for (i, c) in challenges.iter().enumerate() {
            lines.push(format!(
                "{}. [{}]{} {}",
                i + 1,
                c.category.label(),
                c.name,
                c.status.label()
            ));
        }
        Ok(ToolOutput {
            content: lines.join("\n"),
            is_error: false,
        })
    }
}

/// 当前时间字符串（`HH:MM` 格式，UTC+8）。
fn current_time_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let local = secs + 8 * 3600;
    let h = (local / 3600) % 24;
    let m = (local / 60) % 60;
    format!("{h:02}:{m:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCtx;
    use std::path::PathBuf;

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: PathBuf::from("."),
            rules: Vec::new(),
            scope: None,
        }
    }

    fn make_tool() -> (CtfChallengeTool, Arc<Mutex<Vec<CtfChallenge>>>) {
        let dir = std::env::temp_dir().join(format!(
            "cyber_ctf_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let challenges = Arc::new(Mutex::new(Vec::new()));
        let tool = CtfChallengeTool::new(challenges.clone(), dir);
        (tool, challenges)
    }

    #[tokio::test]
    async fn register_creates_new_challenge() {
        let (tool, challenges) = make_tool();
        let input = json!({
            "action": "register",
            "name": "test-challenge",
            "category": "web",
            "description": "A test challenge",
            "tags": ["sqli", "auth"]
        });
        let out = tool.run(input, &ctx()).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("test-challenge"));

        let list = challenges.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test-challenge");
        assert_eq!(list[0].category, CtfCategory::Web);
    }

    #[tokio::test]
    async fn register_updates_existing() {
        let (tool, challenges) = make_tool();
        // First register
        let input1 = json!({"action": "register", "name": "c1", "category": "web"});
        tool.run(input1, &ctx()).await.unwrap();
        // Update
        let input2 = json!({"action": "register", "name": "c1", "category": "pwn", "description": "updated"});
        tool.run(input2, &ctx()).await.unwrap();

        let list = challenges.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].category, CtfCategory::Pwn);
        assert_eq!(list[0].description, "updated");
    }

    #[tokio::test]
    async fn solve_marks_solved() {
        let (tool, challenges) = make_tool();
        let reg = json!({"action": "register", "name": "c1", "category": "crypto"});
        tool.run(reg, &ctx()).await.unwrap();

        let solve = json!({"action": "solve", "name": "c1", "flag": "flag{test}", "key_points": "RSA"});
        let out = tool.run(solve, &ctx()).await.unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("flag{test}"));

        let list = challenges.lock().unwrap();
        assert_eq!(list[0].status, CtfStatus::Solved);
        assert_eq!(list[0].flag.as_deref(), Some("flag{test}"));
        assert!(list[0].end_time.is_some());
    }

    #[tokio::test]
    async fn solve_nonexistent_errors() {
        let (tool, _challenges) = make_tool();
        let solve = json!({"action": "solve", "name": "nope", "flag": "flag{}"});
        let out = tool.run(solve, &ctx()).await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn list_returns_summary() {
        let (tool, _challenges) = make_tool();
        tool.run(json!({"action": "register", "name": "c1", "category": "web"}), &ctx()).await.unwrap();
        tool.run(json!({"action": "register", "name": "c2", "category": "pwn"}), &ctx()).await.unwrap();

        let out = tool.run(json!({"action": "list"}), &ctx()).await.unwrap();
        assert!(out.content.contains("c1"));
        assert!(out.content.contains("c2"));
    }

    #[tokio::test]
    async fn list_empty() {
        let (tool, _challenges) = make_tool();
        let out = tool.run(json!({"action": "list"}), &ctx()).await.unwrap();
        assert!(out.content.contains("无 CTF 题目"));
    }
}
