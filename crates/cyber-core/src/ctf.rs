//! CTF 题目数据模型。
//!
//! 题目持久化在 `~/.cyber/ctf/challenges.json`（JSON 数组），
//! writeup 文件在 `~/.cyber/ctf/writeup/{category}/{name}/writeup.md`。

use std::fmt;

use serde::{Deserialize, Serialize};

/// CTF 题目分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CtfCategory {
    Misc,
    Web,
    Reverse,
    Pwn,
    Crypto,
}

impl fmt::Display for CtfCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

impl CtfCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Misc => "misc",
            Self::Web => "web",
            Self::Reverse => "reverse",
            Self::Pwn => "pwn",
            Self::Crypto => "crypto",
        }
    }

    /// 大写标签（面板显示用，如 `[WEB]`）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Misc => "MISC",
            Self::Web => "WEB",
            Self::Reverse => "REVERSE",
            Self::Pwn => "PWN",
            Self::Crypto => "CRYPTO",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "misc" => Some(Self::Misc),
            "web" => Some(Self::Web),
            "reverse" | "re" => Some(Self::Reverse),
            "pwn" => Some(Self::Pwn),
            "crypto" => Some(Self::Crypto),
            _ => None,
        }
    }
}

impl Default for CtfCategory {
    fn default() -> Self {
        Self::Misc
    }
}

/// CTF 题目状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CtfStatus {
    #[default]
    InProgress,
    Solved,
}

impl CtfStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::InProgress => "进行中",
            Self::Solved => "已完成",
        }
    }
}

/// 一道 CTF 题目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CtfChallenge {
    /// 唯一 ID（简短 UUID 前缀）。
    pub id: String,
    /// 题目名称。
    pub name: String,
    /// 题目分类。
    #[serde(default)]
    pub category: CtfCategory,
    /// 题目描述。
    #[serde(default)]
    pub description: String,
    /// 靶机地址（如 `nc 1.2.3.4 1234` 或 URL）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Flag 值（解出后填入）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    /// 标签列表。
    #[serde(default)]
    pub tags: Vec<String>,
    /// 题目状态。
    #[serde(default)]
    pub status: CtfStatus,
    /// 开始解题时间（`HH:MM` 格式）。
    #[serde(default)]
    pub start_time: String,
    /// 结束解题时间（`HH:MM` 格式，未完成时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_time: Option<String>,
    /// Writeup 内容（None = 未写）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writeup: Option<String>,
    /// 关键知识点/卡点。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_points: Option<String>,
}

impl CtfChallenge {
    /// 创建一道新题目（进行中状态，当前时间作为开始时间）。
    pub fn new(name: String, category: CtfCategory) -> Self {
        Self {
            id: short_id(),
            name,
            category,
            description: String::new(),
            target: None,
            flag: None,
            tags: Vec::new(),
            status: CtfStatus::InProgress,
            start_time: current_time_str(),
            end_time: None,
            writeup: None,
            key_points: None,
        }
    }

    /// 是否已解出。
    pub fn is_solved(&self) -> bool {
        self.status == CtfStatus::Solved
    }

    /// 是否已写 writeup。
    pub fn has_writeup(&self) -> bool {
        self.writeup.is_some()
    }

    /// 用时（已解出且有结束时间时返回，格式如 `10m53s`）。
    pub fn duration_str(&self) -> Option<String> {
        let end = self.end_time.as_ref()?;
        if !self.is_solved() {
            return None;
        }
        let dur = duration_between(&self.start_time, end);
        Some(dur)
    }
}

/// 生成 8 字符简短 ID。
fn short_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{now:08x}")[..8].to_string()
}

/// 当前时间字符串（`HH:MM` 格式）。
fn current_time_str() -> String {
    // 不引入 chrono 依赖；用 SystemTime 手算
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // UTC+8（Asia/Shanghai）
    let local = secs + 8 * 3600;
    let h = (local / 3600) % 24;
    let m = (local / 60) % 60;
    format!("{h:02}:{m:02}")
}

/// 计算两个 `HH:MM` 时间点之间的用时（格式如 `10m53s`）。
/// 跨天时按 24h 取模处理（简化：CTF 题目通常在同天内完成）。
fn duration_between(start: &str, end: &str) -> String {
    let parse = |s: &str| -> Option<u64> {
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() != 2 {
            return None;
        }
        let h: u64 = parts[0].parse().ok()?;
        let m: u64 = parts[1].parse().ok()?;
        Some(h * 60 + m)
    };
    let s = parse(start).unwrap_or(0);
    let e = parse(end).unwrap_or(s);
    let diff = if e >= s { e - s } else { 0 };
    let h = diff / 60;
    let m = diff % 60;
    if h > 0 {
        format!("{h}h{m}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn category_roundtrip() {
        for c in [
            CtfCategory::Misc,
            CtfCategory::Web,
            CtfCategory::Reverse,
            CtfCategory::Pwn,
            CtfCategory::Crypto,
        ] {
            let s = serde_json::to_string(&c).unwrap();
            let c2: CtfCategory = serde_json::from_str(&s).unwrap();
            assert_eq!(c, c2);
        }
    }

    #[test]
    fn category_from_str() {
        assert_eq!(CtfCategory::from_str("web"), Some(CtfCategory::Web));
        assert_eq!(CtfCategory::from_str("WEB"), Some(CtfCategory::Web));
        assert_eq!(CtfCategory::from_str("re"), Some(CtfCategory::Reverse));
        assert_eq!(CtfCategory::from_str("unknown"), None);
    }

    #[test]
    fn challenge_new_is_in_progress() {
        let c = CtfChallenge::new("test".into(), CtfCategory::Web);
        assert_eq!(c.status, CtfStatus::InProgress);
        assert!(!c.is_solved());
        assert!(!c.has_writeup());
        assert!(c.duration_str().is_none());
    }

    #[test]
    fn challenge_duration() {
        let mut c = CtfChallenge::new("test".into(), CtfCategory::Pwn);
        c.status = CtfStatus::Solved;
        c.start_time = "14:00".into();
        c.end_time = Some("14:53".into());
        assert_eq!(c.duration_str().as_deref(), Some("53m"));
    }

    #[test]
    fn challenge_duration_hours() {
        let mut c = CtfChallenge::new("test".into(), CtfCategory::Crypto);
        c.status = CtfStatus::Solved;
        c.start_time = "10:00".into();
        c.end_time = Some("12:30".into());
        assert_eq!(c.duration_str().as_deref(), Some("2h30m"));
    }

    #[test]
    fn challenge_serialize_deserialize() {
        let c = CtfChallenge::new("题目".into(), CtfCategory::Web);
        let json = serde_json::to_string(&c).unwrap();
        let c2: CtfChallenge = serde_json::from_str(&json).unwrap();
        assert_eq!(c.name, c2.name);
        assert_eq!(c.category, c2.category);
        assert_eq!(c.status, c2.status);
    }
}
