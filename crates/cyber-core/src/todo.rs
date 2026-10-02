//! Todo 任务模型与状态定义。
//!
//! 用于结构化任务管理能力，供 TodoTool、CLI / TUI、Slash 命令及历史持久化共享。

use serde::{Deserialize, Serialize};

/// 任务执行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl TodoStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pending" | "todo" | "p" => Some(Self::Pending),
            "in_progress" | "inprogress" | "doing" | "run" | "running" | "ip" => {
                Some(Self::InProgress)
            }
            "completed" | "done" | "complete" | "finished" | "c" => Some(Self::Completed),
            "failed" | "error" | "fail" | "f" => Some(Self::Failed),
            _ => None,
        }
    }

    /// 显示符号：[ ] 未开始，[>] 进行中，[x] 已完成，[!] 失败。
    pub fn symbol(&self) -> &'static str {
        match self {
            Self::Pending => "[ ]",
            Self::InProgress => "[>]",
            Self::Completed => "[x]",
            Self::Failed => "[!]",
        }
    }
}

impl std::fmt::Display for TodoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Serialize for TodoStatus {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for TodoStatus {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        TodoStatus::parse(&s).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "invalid todo status: {s}, expected pending, in_progress, completed, failed"
            ))
        })
    }
}

/// 单个 Todo 任务项。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub title: String,
    pub status: TodoStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl TodoItem {
    pub fn new(id: impl Into<String>, title: impl Into<String>, status: TodoStatus) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            status,
            notes: None,
        }
    }

    pub fn with_notes(mut self, notes: impl Into<String>) -> Self {
        self.notes = Some(notes.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_parse_and_serialize() {
        assert_eq!(TodoStatus::parse("pending"), Some(TodoStatus::Pending));
        assert_eq!(
            TodoStatus::parse("InProgress"),
            Some(TodoStatus::InProgress)
        );
        assert_eq!(TodoStatus::parse("done"), Some(TodoStatus::Completed));
        assert_eq!(TodoStatus::parse("failed"), Some(TodoStatus::Failed));
        assert_eq!(TodoStatus::parse("unknown"), None);

        let json = serde_json::to_string(&TodoStatus::InProgress).unwrap();
        assert_eq!(json, "\"in_progress\"");

        let de: TodoStatus = serde_json::from_str("\"completed\"").unwrap();
        assert_eq!(de, TodoStatus::Completed);

        let de2: TodoStatus = serde_json::from_str("\"InProgress\"").unwrap();
        assert_eq!(de2, TodoStatus::InProgress);
    }

    #[test]
    fn item_serde_roundtrip() {
        let item = TodoItem::new("1", "Scan targets", TodoStatus::InProgress)
            .with_notes("Scanning ports 80, 443");
        let json = serde_json::to_string(&item).unwrap();
        let de: TodoItem = serde_json::from_str(&json).unwrap();
        assert_eq!(item, de);
    }
}
