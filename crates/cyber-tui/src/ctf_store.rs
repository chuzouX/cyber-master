//! CTF 题目持久化：加载/保存 `~/.cyber/ctf/challenges.json` + writeup 文件。
//!
//! Session 级隔离：每个 session 的题目存储在 `{ctf_dir}/sessions/{session_id}.json`，
//! 切换 session 时由 App 负责保存旧 session + 加载新 session。

use std::path::{Path, PathBuf};

use cyber_core::CtfChallenge;
use tracing::warn;

/// 从 `ctf_dir/challenges.json` 加载题目列表（全局回退用）。
///
/// 文件不存在或解析失败时返回空列表（不阻断启动）。
pub fn load_challenges(ctf_dir: &Path) -> Vec<CtfChallenge> {
    let path = ctf_dir.join("challenges.json");
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<Vec<CtfChallenge>>(&content) {
            Ok(list) => list,
            Err(e) => {
                warn!(error = %e, path = %path.display(), "challenges.json 解析失败");
                Vec::new()
            }
        },
        Err(e) => {
            warn!(error = %e, path = %path.display(), "challenges.json 读取失败");
            Vec::new()
        }
    }
}

/// 保存题目列表到 `ctf_dir/challenges.json`。
pub fn save_challenges(ctf_dir: &Path, challenges: &[CtfChallenge]) {
    let path = ctf_dir.join("challenges.json");
    if let Err(e) = std::fs::create_dir_all(ctf_dir) {
        warn!(error = %e, dir = %ctf_dir.display(), "CTF 目录创建失败");
        return;
    }
    match serde_json::to_string_pretty(challenges) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                warn!(error = %e, path = %path.display(), "challenges.json 写入失败");
            }
        }
        Err(e) => warn!(error = %e, "challenges 序列化失败"),
    }
}

/// Session 级题目文件路径：`{ctf_dir}/sessions/{session_id}.json`。
fn session_challenges_path(ctf_dir: &Path, session_id: &str) -> PathBuf {
    ctf_dir.join("sessions").join(format!("{session_id}.json"))
}

/// 加载指定 session 的题目列表。
///
/// 文件不存在时返回空列表（新 session 无题目）。
pub fn load_session_challenges(ctf_dir: &Path, session_id: &str) -> Vec<CtfChallenge> {
    let path = session_challenges_path(ctf_dir, session_id);
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str::<Vec<CtfChallenge>>(&content).unwrap_or_else(|e| {
            warn!(error = %e, path = %path.display(), "session challenges 解析失败");
            Vec::new()
        }),
        Err(e) => {
            warn!(error = %e, path = %path.display(), "session challenges 读取失败");
            Vec::new()
        }
    }
}

/// 保存题目列表到 session 级文件 `{ctf_dir}/sessions/{session_id}.json`。
pub fn save_session_challenges(ctf_dir: &Path, session_id: &str, challenges: &[CtfChallenge]) {
    let path = session_challenges_path(ctf_dir, session_id);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(error = %e, dir = %parent.display(), "session challenges 目录创建失败");
            return;
        }
    }
    match serde_json::to_string_pretty(challenges) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                warn!(error = %e, path = %path.display(), "session challenges 写入失败");
            }
        }
        Err(e) => warn!(error = %e, "session challenges 序列化失败"),
    }
}

/// 删除指定 session 的题目文件（session 删除时调用）。
pub fn delete_session_challenges(ctf_dir: &Path, session_id: &str) {
    let path = session_challenges_path(ctf_dir, session_id);
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            warn!(error = %e, path = %path.display(), "session challenges 删除失败");
        }
    }
}

/// 保存 writeup 到 `{ctf_dir}/{category}/{name}/writeup.md`。
///
/// 自动创建目录。EXP 等附属文件也放在同一目录 `{ctf_dir}/{category}/{name}/`。
/// 返回写入的文件路径（失败时返回 None）。
pub fn save_writeup(
    ctf_dir: &Path,
    challenge: &CtfChallenge,
    content: &str,
) -> Option<std::path::PathBuf> {
    let dir = ctf_dir
        .join(challenge.category.as_str())
        .join(&challenge.name);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(error = %e, dir = %dir.display(), "writeup 目录创建失败");
        return None;
    }
    let path = dir.join("writeup.md");
    if let Err(e) = std::fs::write(&path, content) {
        warn!(error = %e, path = %path.display(), "writeup 写入失败");
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::{CtfCategory, CtfStatus};

    fn temp_dir() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "cyber_ctf_store_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn load_nonexistent_returns_empty() {
        let dir = temp_dir();
        let list = load_challenges(&dir);
        assert!(list.is_empty());
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = temp_dir();
        let mut c = CtfChallenge::new("test".into(), CtfCategory::Web);
        c.description = "A test".into();
        c.status = CtfStatus::Solved;
        c.flag = Some("flag{test}".into());

        save_challenges(&dir, &[c.clone()]);
        let loaded = load_challenges(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "test");
        assert_eq!(loaded[0].category, CtfCategory::Web);
        assert_eq!(loaded[0].status, CtfStatus::Solved);
        assert_eq!(loaded[0].flag.as_deref(), Some("flag{test}"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writeup_creates_file() {
        let dir = temp_dir();
        let c = CtfChallenge::new("test-wp".into(), CtfCategory::Pwn);
        let path = save_writeup(&dir, &c, "# Writeup\ncontent");
        assert!(path.is_some());
        let path = path.unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Writeup"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
