//! 对话历史持久化：按工作目录 hash 存储到 `~/.cyber/history/{cwd_hash}.json`。
//!
//! 设计：每个 cwd 对应一个 JSON 文件（`ChatEntry` 数组）。App 启动时加载对应历史，
//! 在 Done/Error/cancel/clear/quit 时写盘。仅持久化 `ChatEntry` 序列（含工具调用记录），
//! 不持久化输入框文本与流式 buffer。
//!
//! hash 用 FNV-1a 64bit 自实现——`std::collections::hash_map::DefaultHasher` 跨 Rust
//! 版本不稳定，会导致升级后历史文件名变化、历史"丢失"。

use std::path::{Path, PathBuf};

use tracing::{debug, warn};

use crate::chat::ChatEntry;

/// 由 cwd 计算稳定的 16 位十六进制 hash（FNV-1a 64bit）。
pub fn cwd_hash(cwd: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in cwd.to_string_lossy().as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// 历史 JSON 文件路径：`history_dir/{cwd_hash}.json`。
pub fn history_file(history_dir: &Path, cwd: &Path) -> PathBuf {
    history_dir.join(format!("{}.json", cwd_hash(cwd)))
}

/// 加载某 cwd 对应的历史条目。文件缺失或解析失败均返回空 Vec（不阻断启动）。
pub fn load(history_dir: &Path, cwd: &Path) -> Vec<ChatEntry> {
    let file = history_file(history_dir, cwd);
    let data = match std::fs::read(&file) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %file.display(), "无历史文件，以空历史启动");
            return Vec::new();
        }
        Err(e) => {
            warn!(error = %e, path = %file.display(), "读取历史文件失败，忽略并以空历史启动");
            return Vec::new();
        }
    };
    match serde_json::from_slice::<Vec<ChatEntry>>(&data) {
        Ok(entries) => {
            debug!(count = entries.len(), path = %file.display(), "已加载对话历史");
            entries
        }
        Err(e) => {
            warn!(error = %e, path = %file.display(), "历史文件反序列化失败，忽略并以空历史启动");
            Vec::new()
        }
    }
}

/// 保存历史条目到 `history_dir/{cwd_hash}.json`（原子写：tmp→rename）。
/// `history_dir` 不存在时自动创建。保存失败仅记日志（不影响会话）。
pub fn save(history_dir: &Path, cwd: &Path, entries: &[ChatEntry]) {
    let file = history_file(history_dir, cwd);
    if let Err(e) = std::fs::create_dir_all(history_dir) {
        warn!(error = %e, dir = %history_dir.display(), "创建 history 目录失败，跳过保存");
        return;
    }
    let data = match serde_json::to_vec_pretty(entries) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "历史序列化失败，跳过保存");
            return;
        }
    };
    let tmp = file.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, &data) {
        warn!(error = %e, path = %tmp.display(), "写历史 tmp 失败");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, &file) {
        warn!(error = %e, path = %file.display(), "rename 历史 tmp→json 失败");
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    debug!(count = entries.len(), path = %file.display(), "对话历史已保存");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_history_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cwd_hash_stable_and_distinct() {
        let a = cwd_hash(Path::new("/home/user/proj"));
        let b = cwd_hash(Path::new("/home/user/proj"));
        let c = cwd_hash(Path::new("/home/user/other"));
        assert_eq!(a, b, "同路径 hash 应稳定一致");
        assert_ne!(a, c, "不同路径 hash 应不同");
        assert_eq!(a.len(), 16, "hash 应为 16 位十六进制");
        assert!(
            a.chars().all(|ch| ch.is_ascii_hexdigit()),
            "hash 应仅含十六进制字符"
        );
    }

    #[test]
    fn save_load_roundtrip_preserves_all_variants() {
        let dir = temp_dir("rt");
        let cwd = Path::new("/tmp/proj");
        let entries = vec![
            ChatEntry::User("你好".into()),
            ChatEntry::Assistant("收到：你好".into()),
            ChatEntry::ToolCall {
                id: "1".into(),
                name: "list_dir".into(),
                arguments: "{\"path\":\".\"}".into(),
            },
            ChatEntry::ToolResult {
                id: "1".into(),
                name: "list_dir".into(),
                output: "a.txt\nb.txt".into(),
                is_error: false,
            },
            ChatEntry::System("系统提示".into()),
        ];
        save(&dir, cwd, &entries);
        let loaded = load(&dir, cwd);
        assert_eq!(loaded.len(), entries.len(), "往返应保留全部条目");
        assert!(matches!(&loaded[0], ChatEntry::User(c) if c == "你好"));
        assert!(matches!(&loaded[1], ChatEntry::Assistant(c) if c == "收到：你好"));
        assert!(
            matches!(&loaded[2], ChatEntry::ToolCall { name, arguments, .. } if name == "list_dir" && arguments == "{\"path\":\".\"}")
        );
        assert!(
            matches!(&loaded[3], ChatEntry::ToolResult { output, is_error, .. } if output == "a.txt\nb.txt" && !is_error)
        );
        assert!(matches!(&loaded[4], ChatEntry::System(c) if c == "系统提示"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = temp_dir("missing");
        let loaded = load(&dir, Path::new("/nonexistent/proj"));
        assert!(loaded.is_empty(), "文件不存在应返回空 Vec（非错误）");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_creates_history_dir_if_missing() {
        let dir = std::env::temp_dir().join(format!(
            "cyber_history_nest_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!dir.exists(), "前提：目录不存在");
        save(&dir, Path::new("/p"), &[ChatEntry::User("x".into())]);
        assert!(dir.exists(), "save 应自动创建 history_dir");
        assert!(
            history_file(&dir, Path::new("/p")).exists(),
            "历史文件应已写入"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_cwd_isolates_history() {
        let dir = temp_dir("iso");
        save(&dir, Path::new("/a"), &[ChatEntry::User("A".into())]);
        save(&dir, Path::new("/b"), &[ChatEntry::User("B".into())]);
        let a = load(&dir, Path::new("/a"));
        let b = load(&dir, Path::new("/b"));
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert!(matches!(&a[0], ChatEntry::User(c) if c == "A"));
        assert!(matches!(&b[0], ChatEntry::User(c) if c == "B"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_empty_overwrites_previous() {
        // /clear 后保存空数组应覆盖旧历史，而非保留
        let dir = temp_dir("overwrite");
        let cwd = Path::new("/p");
        save(&dir, cwd, &[ChatEntry::User("old".into())]);
        save(&dir, cwd, &[]);
        let loaded = load(&dir, cwd);
        assert!(loaded.is_empty(), "保存空应覆盖旧历史");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_returns_empty_without_panic() {
        let dir = temp_dir("corrupt");
        let cwd = Path::new("/p");
        let file = history_file(&dir, cwd);
        std::fs::write(&file, b"NOT JSON {{{").unwrap();
        let loaded = load(&dir, cwd);
        assert!(loaded.is_empty(), "损坏文件应回退空历史而非 panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn history_file_uses_cwd_hash_filename() {
        let dir = temp_dir("fname");
        let cwd = Path::new("/some/path");
        let f = history_file(&dir, cwd);
        let expected_name = format!("{}.json", cwd_hash(cwd));
        assert_eq!(
            f.file_name().unwrap().to_string_lossy(),
            expected_name,
            "文件名应为 cwd_hash.json"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
