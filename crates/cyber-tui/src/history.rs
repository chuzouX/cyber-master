//! 对话历史持久化（多 session）：按工作目录 hash 存储到 `~/.cyber/history/{cwd_hash}/`。
//!
//! 布局：
//! ```text
//! ~/.cyber/history/{cwd_hash}/
//!   index.json     # { "current": "<id>", "sessions": [SessionMeta] }
//!   {id}.json      # Vec<ChatEntry>
//! ```
//! 每个 cwd 独立一个目录，目录内多个 session 文件 + 一个 index。session 间对话独立，
//! `/sessions read <id>` 可跨 session 读取（仅同 cwd）。
//!
//! 启动时 `load_current` 返回当前 session 的 entries；切换/新建/删除经 App 调用
//! `load_entries` / `save_entries` / `delete_session` + `save_index`。
//!
//! **迁移**：旧版单文件历史 `~/.cyber/history/{cwd_hash}.json`（Vec<ChatEntry>）若存在
//! 且新目录无 index.json → 自动迁移为单 session（id 新生成、title 取首条 User 消息或
//! "默认会话"），旧文件 rename 为 `{cwd_hash}.legacy.bak`。
//!
//! hash 用 FNV-1a 64bit 自实现——`std::collections::hash_map::DefaultHasher` 跨 Rust
//! 版本不稳定，会导致升级后历史文件名变化、历史"丢失"。session id 用 `SystemTime` 纳秒
//! base36 编码（短、单调、无新依赖）。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::chat::ChatEntry;

/// 默认 session 标题（新建未派生 title 时用）。
const DEFAULT_SESSION_TITLE: &str = "新会话";
/// 迁移自旧单文件历史的 session 标题前缀（用于区分新空 session）。
const MIGRATED_SESSION_TITLE: &str = "默认会话";
/// title 派生时从首条 User 消息截取的最大字符数。
const TITLE_MAX_CHARS: usize = 40;

/// 单个 session 的元数据（index.json 中 `sessions` 数组元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// session 唯一 id（base36 纳秒，用作文件名 `{id}.json`）。
    pub id: String,
    /// 显示标题（首条 User 消息前 40 字符 / "新会话" / "默认会话"）。
    pub title: String,
    /// 创建时间（UNIX 秒）。
    pub created_at: u64,
    /// 最近更新时间（UNIX 秒）。
    pub updated_at: u64,
    /// 消息条数（User + Assistant + System + ToolCall + ToolResult 全计）。
    pub message_count: usize,
}

/// session 索引（index.json 内容）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SessionIndex {
    /// 当前激活的 session id。
    pub current: String,
    /// 所有 session 的元数据列表（顺序同创建顺序）。
    pub sessions: Vec<SessionMeta>,
}

impl SessionIndex {
    /// 查找指定 id 的 session meta 不可变引用。
    pub fn get(&self, id: &str) -> Option<&SessionMeta> {
        self.sessions.iter().find(|s| s.id == id)
    }

    /// 查找指定 id 的 session meta 可变引用。
    pub fn get_mut(&mut self, id: &str) -> Option<&mut SessionMeta> {
        self.sessions.iter_mut().find(|s| s.id == id)
    }

    /// 当前 session meta 不可变引用。
    pub fn current_meta(&self) -> Option<&SessionMeta> {
        self.get(&self.current)
    }
}

/// 由 cwd 计算稳定的 16 位十六进制 hash（FNV-1a 64bit）。
pub fn cwd_hash(cwd: &Path) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in cwd.to_string_lossy().as_bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// session 目录路径：`history_dir/{cwd_hash}/`。
pub fn session_dir(history_dir: &Path, cwd: &Path) -> PathBuf {
    history_dir.join(cwd_hash(cwd))
}

/// 旧版单文件历史路径（迁移用）：`history_dir/{cwd_hash}.json`。
fn legacy_history_file(history_dir: &Path, cwd: &Path) -> PathBuf {
    history_dir.join(format!("{}.json", cwd_hash(cwd)))
}

/// index.json 路径。
fn index_file(history_dir: &Path, cwd: &Path) -> PathBuf {
    session_dir(history_dir, cwd).join("index.json")
}

/// 单个 session 的 entries 文件路径：`session_dir/{id}.json`。
fn session_file(history_dir: &Path, cwd: &Path, id: &str) -> PathBuf {
    session_dir(history_dir, cwd).join(format!("{id}.json"))
}

/// 生成新 session id：当前时间纳秒 base36 编码（短、单调）。
pub fn generate_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    base36(nanos)
}

/// 把 u128 编码为 base36 字符串（小写）。
fn base36(mut n: u128) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".into();
    }
    let mut buf = Vec::new();
    while n > 0 {
        buf.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_else(|_| "0".into())
}

/// 当前 UNIX 秒（失败回退 0）。
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 构造一个新 session 的 meta（id 新生成、title "新会话"、now 时间戳）。
pub fn create_session_meta() -> SessionMeta {
    let now = now_secs();
    SessionMeta {
        id: generate_session_id(),
        title: DEFAULT_SESSION_TITLE.into(),
        created_at: now,
        updated_at: now,
        message_count: 0,
    }
}

/// 加载某 cwd 的 session 索引。无 index.json 时尝试迁移旧单文件历史；
/// 都无 → 建默认空 session 并写盘。
pub fn load_index(history_dir: &Path, cwd: &Path) -> SessionIndex {
    let idx_file = index_file(history_dir, cwd);
    if idx_file.exists() {
        match std::fs::read(&idx_file) {
            Ok(data) => match serde_json::from_slice::<SessionIndex>(&data) {
                Ok(idx) => {
                    debug!(
                        path = %idx_file.display(),
                        sessions = idx.sessions.len(),
                        "已加载 session 索引"
                    );
                    return idx;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        path = %idx_file.display(),
                        "index.json 反序列化失败，回退迁移/默认"
                    );
                }
            },
            Err(e) => {
                warn!(
                    error = %e,
                    path = %idx_file.display(),
                    "读取 index.json 失败，回退迁移/默认"
                );
            }
        }
    }
    // 尝试迁移旧单文件历史
    if let Some(idx) = migrate_legacy(history_dir, cwd) {
        return idx;
    }
    // 无任何历史 → 建默认空 session
    let meta = create_session_meta();
    let idx = SessionIndex {
        current: meta.id.clone(),
        sessions: vec![meta],
    };
    save_index(history_dir, cwd, &idx);
    idx
}

/// 保存 session 索引到 `index.json`（原子 tmp→rename）。
pub fn save_index(history_dir: &Path, cwd: &Path, idx: &SessionIndex) {
    let dir = session_dir(history_dir, cwd);
    if ensure_dir(&dir).is_err() {
        return;
    }
    let file = dir.join("index.json");
    let data = match serde_json::to_vec_pretty(idx) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "index 序列化失败，跳过保存");
            return;
        }
    };
    atomic_write(&file, &data, "index");
}

/// 加载某 session 的对话条目。文件缺失/损坏 → 空 Vec（不阻断）。
pub fn load_entries(history_dir: &Path, cwd: &Path, id: &str) -> Vec<ChatEntry> {
    let file = session_file(history_dir, cwd, id);
    let data = match std::fs::read(&file) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %file.display(), "session 文件不存在，以空历史启动");
            return Vec::new();
        }
        Err(e) => {
            warn!(
                error = %e,
                path = %file.display(),
                "读取 session 文件失败，以空历史启动"
            );
            return Vec::new();
        }
    };
    match serde_json::from_slice::<Vec<ChatEntry>>(&data) {
        Ok(entries) => {
            debug!(count = entries.len(), path = %file.display(), "已加载 session 历史");
            entries
        }
        Err(e) => {
            warn!(
                error = %e,
                path = %file.display(),
                "session 文件反序列化失败，以空历史启动"
            );
            Vec::new()
        }
    }
}

/// 保存某 session 的对话条目到 `{id}.json`（原子 tmp→rename）。
pub fn save_entries(history_dir: &Path, cwd: &Path, id: &str, entries: &[ChatEntry]) {
    let dir = session_dir(history_dir, cwd);
    if ensure_dir(&dir).is_err() {
        return;
    }
    let file = session_file(history_dir, cwd, id);
    let data = match serde_json::to_vec_pretty(entries) {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "session 序列化失败，跳过保存");
            return;
        }
    };
    atomic_write(&file, &data, "session");
}

/// 启动用：加载 index + 当前 session 的 entries。
pub fn load_current(history_dir: &Path, cwd: &Path) -> (SessionIndex, Vec<ChatEntry>) {
    let idx = load_index(history_dir, cwd);
    let entries = load_entries(history_dir, cwd, &idx.current);
    (idx, entries)
}

/// 保存当前 session：写 entries 文件 + 刷新 meta（message_count、updated_at、title 派生）+ 写 index。
///
/// **title 派生**：若当前 meta.title 为 "新会话" 且 entries 首条为 User →
/// title = 首 40 字符（避免一直显示 "新会话"）。
pub fn save_current(history_dir: &Path, cwd: &Path, idx: &mut SessionIndex, entries: &[ChatEntry]) {
    // 先派生 title（在写 index 前更新 meta）
    if let Some(meta) = idx.get_mut(&idx.current.clone()) {
        if meta.title == DEFAULT_SESSION_TITLE {
            if let Some(ChatEntry::User(text)) = entries.first() {
                let title: String = text.chars().take(TITLE_MAX_CHARS).collect();
                if !title.trim().is_empty() {
                    meta.title = title;
                }
            }
        }
        meta.message_count = entries.len();
        meta.updated_at = now_secs();
    }
    save_entries(history_dir, cwd, &idx.current, entries);
    save_index(history_dir, cwd, idx);
}

/// 列出所有 session meta（同 `idx.sessions.clone()`，便利 API）。
pub fn list_sessions(history_dir: &Path, cwd: &Path) -> Vec<SessionMeta> {
    load_index(history_dir, cwd).sessions
}

/// 读取某 session 的对话为可读文本（User/Assistant/System 各一行 + 工具调用简表）。
/// 文件缺失/损坏 → None。
pub fn read_session_text(history_dir: &Path, cwd: &Path, id: &str) -> Option<String> {
    let entries = load_entries(history_dir, cwd, id);
    if entries.is_empty() {
        return None;
    }
    let mut out = String::new();
    for e in &entries {
        match e {
            ChatEntry::User(t) => {
                out.push_str("🧑 ");
                out.push_str(t);
                out.push('\n');
            }
            ChatEntry::Assistant(t) => {
                out.push_str("🤖 ");
                out.push_str(t);
                out.push('\n');
            }
            ChatEntry::Thinking(t) => {
                out.push_str("💭 ");
                out.push_str(t);
                out.push('\n');
            }
            ChatEntry::System(t) => {
                out.push_str("ℹ️ ");
                out.push_str(t);
                out.push('\n');
            }
            ChatEntry::ToolCall { name, arguments, .. } => {
                out.push_str(&format!("▶ [{name}] {arguments}\n"));
            }
            ChatEntry::ToolResult { name, output, .. } => {
                out.push_str(&format!("→ [{name}] {output}\n"));
            }
        }
    }
    Some(out)
}

/// 删除某 session：删 `{id}.json` + 从 index 移除 + 重写 index。
/// 返回删除后剩余的 session 数（供调用方判断是否拒绝删除最后一个）。
pub fn delete_session(history_dir: &Path, cwd: &Path, id: &str) -> usize {
    let mut idx = load_index(history_dir, cwd);
    // 删文件
    let file = session_file(history_dir, cwd, id);
    if file.exists() {
        if let Err(e) = std::fs::remove_file(&file) {
            warn!(error = %e, path = %file.display(), "删除 session 文件失败");
        }
    }
    // 从 index 移除
    idx.sessions.retain(|s| s.id != id);
    // 若删的是 current → 切到剩余首个（无剩余则建新空 session）
    if idx.current == id {
        if let Some(first) = idx.sessions.first() {
            idx.current = first.id.clone();
        } else {
            let meta = create_session_meta();
            idx.current = meta.id.clone();
            idx.sessions.push(meta);
        }
    }
    let remaining = idx.sessions.len();
    save_index(history_dir, cwd, &idx);
    remaining
}

// ── 内部工具 ──────────────────────────────────────────────────────────────

/// 迁移旧单文件历史 `history_dir/{cwd_hash}.json` → 新 session 结构。
/// 成功 → 返回新 SessionIndex（已写 index.json + {id}.json，旧文件 rename .legacy.bak）。
/// 无旧文件或迁移失败 → None。
fn migrate_legacy(history_dir: &Path, cwd: &Path) -> Option<SessionIndex> {
    let legacy = legacy_history_file(history_dir, cwd);
    if !legacy.exists() {
        return None;
    }
    let data = match std::fs::read(&legacy) {
        Ok(d) => d,
        Err(e) => {
            warn!(
                error = %e,
                path = %legacy.display(),
                "读取旧历史文件失败，跳过迁移"
            );
            return None;
        }
    };
    let entries: Vec<ChatEntry> = match serde_json::from_slice(&data) {
        Ok(v) => v,
        Err(e) => {
            warn!(
                error = %e,
                path = %legacy.display(),
                "旧历史文件反序列化失败，跳过迁移"
            );
            return None;
        }
    };
    // 建 session：title 取首条 User 前 40 字符或 "默认会话"
    let title = entries
        .first()
        .and_then(|e| match e {
            ChatEntry::User(t) => Some(t.chars().take(TITLE_MAX_CHARS).collect::<String>()),
            _ => None,
        })
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| MIGRATED_SESSION_TITLE.to_string());
    let now = now_secs();
    let meta = SessionMeta {
        id: generate_session_id(),
        title,
        created_at: now,
        updated_at: now,
        message_count: entries.len(),
    };
    let idx = SessionIndex {
        current: meta.id.clone(),
        sessions: vec![meta.clone()],
    };
    // 写新文件
    let dir = session_dir(history_dir, cwd);
    if ensure_dir(&dir).is_err() {
        return None;
    }
    let idx_data = serde_json::to_vec_pretty(&idx).ok()?;
    atomic_write(&index_file(history_dir, cwd), &idx_data, "index");
    let entries_data = serde_json::to_vec_pretty(&entries).ok()?;
    atomic_write(
        &session_file(history_dir, cwd, &meta.id),
        &entries_data,
        "session",
    );
    // 旧文件 rename .legacy.bak（不删除，防回退丢失）
    let bak = legacy.with_extension("json.legacy.bak");
    if let Err(e) = std::fs::rename(&legacy, &bak) {
        warn!(
            error = %e,
            from = %legacy.display(),
            to = %bak.display(),
            "rename 旧历史为 .legacy.bak 失败（不影响新结构，旧文件保留原位）"
        );
    }
    debug!(
        cwd = %cwd.display(),
        sessions = 1,
        entries = entries.len(),
        "已迁移旧单文件历史为新 session 结构"
    );
    Some(idx)
}

/// 确保目录存在（递归创建）。
fn ensure_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir).map_err(|e| {
        warn!(error = %e, dir = %dir.display(), "创建目录失败");
        e
    })
}

/// 原子写：tmp→rename。失败仅 warn（不阻断会话）。
fn atomic_write(file: &Path, data: &[u8], label: &str) {
    let tmp = file.with_extension("json.tmp");
    if let Err(e) = std::fs::write(&tmp, data) {
        warn!(error = %e, path = %tmp.display(), "写 {label} tmp 失败");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, file) {
        warn!(
            error = %e,
            path = %file.display(),
            "rename {label} tmp→json 失败"
        );
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    debug!(path = %file.display(), "{label} 已写入");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_session_{label}_{}_{}",
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

    fn sample_entries() -> Vec<ChatEntry> {
        vec![
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
        ]
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
    fn base36_encodes_correctly() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(1), "1");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(base36(37), "11");
        // 单调性：后生成的 id 字典序更大（同纳秒比较无意义，此处只测编码稳定）
        let id = generate_session_id();
        assert!(!id.is_empty());
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn create_session_meta_has_defaults() {
        let m = create_session_meta();
        assert_eq!(m.title, DEFAULT_SESSION_TITLE);
        assert_eq!(m.message_count, 0);
        assert!(!m.id.is_empty());
    }

    #[test]
    fn load_index_creates_default_when_no_history() {
        let dir = temp_dir("empty");
        let cwd = Path::new("/tmp/empty-proj");
        let idx = load_index(&dir, cwd);
        assert_eq!(idx.sessions.len(), 1, "无历史应建 1 个默认 session");
        assert_eq!(idx.current, idx.sessions[0].id);
        assert_eq!(idx.sessions[0].title, DEFAULT_SESSION_TITLE);
        // index.json 应已写盘
        assert!(index_file(&dir, cwd).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_load_entries_roundtrip_preserves_all_variants() {
        let dir = temp_dir("rt");
        let cwd = Path::new("/tmp/rt-proj");
        let idx = load_index(&dir, cwd);
        let entries = sample_entries();
        save_entries(&dir, cwd, &idx.current, &entries);
        let loaded = load_entries(&dir, cwd, &idx.current);
        assert_eq!(loaded.len(), entries.len());
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
    fn save_current_derives_title_from_first_user() {
        let dir = temp_dir("title");
        let cwd = Path::new("/tmp/title-proj");
        let mut idx = load_index(&dir, cwd);
        assert_eq!(idx.current_meta().unwrap().title, DEFAULT_SESSION_TITLE);
        let entries = vec![
            ChatEntry::User("帮我写一个 Rust hello world 程序".into()),
            ChatEntry::Assistant("好的".into()),
        ];
        save_current(&dir, cwd, &mut idx, &entries);
        let reloaded = load_index(&dir, cwd);
        let title = reloaded.current_meta().unwrap().title.clone();
        assert_ne!(title, DEFAULT_SESSION_TITLE, "title 应已派生");
        assert!(title.contains("帮我写"));
        assert!(title.chars().count() <= TITLE_MAX_CHARS);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_current_updates_message_count_and_timestamp() {
        let dir = temp_dir("count");
        let cwd = Path::new("/tmp/count-proj");
        let mut idx = load_index(&dir, cwd);
        let orig_updated = idx.current_meta().unwrap().updated_at;
        // 强制时间前进（SystemTime 分辨率可能不足，sleep 1s）
        std::thread::sleep(std::time::Duration::from_secs(1));
        let entries = sample_entries();
        save_current(&dir, cwd, &mut idx, &entries);
        let reloaded = load_index(&dir, cwd);
        let meta = reloaded.current_meta().unwrap();
        assert_eq!(meta.message_count, entries.len());
        assert!(meta.updated_at > orig_updated, "updated_at 应前进");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_cwd_isolates_sessions() {
        let dir = temp_dir("iso");
        let cwd_a = Path::new("/iso/a");
        let cwd_b = Path::new("/iso/b");
        let idx_a = load_index(&dir, cwd_a);
        let idx_b = load_index(&dir, cwd_b);
        save_entries(&dir, cwd_a, &idx_a.current, &[ChatEntry::User("A".into())]);
        save_entries(&dir, cwd_b, &idx_b.current, &[ChatEntry::User("B".into())]);
        let a = load_entries(&dir, cwd_a, &idx_a.current);
        let b = load_entries(&dir, cwd_b, &idx_b.current);
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert!(matches!(&a[0], ChatEntry::User(c) if c == "A"));
        assert!(matches!(&b[0], ChatEntry::User(c) if c == "B"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn multiple_sessions_in_same_cwd_isolate_entries() {
        let dir = temp_dir("multi");
        let cwd = Path::new("/tmp/multi-proj");
        let mut idx = load_index(&dir, cwd);
        let s1 = idx.current.clone();
        save_entries(&dir, cwd, &s1, &[ChatEntry::User("first session".into())]);
        // 新建第二个 session
        let meta2 = create_session_meta();
        let s2 = meta2.id.clone();
        idx.sessions.push(meta2);
        idx.current = s2.clone();
        save_index(&dir, cwd, &idx);
        save_entries(&dir, cwd, &s2, &[ChatEntry::User("second session".into())]);
        // 验证隔离
        let e1 = load_entries(&dir, cwd, &s1);
        let e2 = load_entries(&dir, cwd, &s2);
        assert_eq!(e1.len(), 1);
        assert_eq!(e2.len(), 1);
        assert!(matches!(&e1[0], ChatEntry::User(c) if c == "first session"));
        assert!(matches!(&e2[0], ChatEntry::User(c) if c == "second session"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_empty_overwrites_previous() {
        let dir = temp_dir("overwrite");
        let cwd = Path::new("/tmp/ow-proj");
        let idx = load_index(&dir, cwd);
        save_entries(&dir, cwd, &idx.current, &[ChatEntry::User("old".into())]);
        save_entries(&dir, cwd, &idx.current, &[]);
        let loaded = load_entries(&dir, cwd, &idx.current);
        assert!(loaded.is_empty(), "保存空应覆盖旧历史");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_entries_file_returns_empty_without_panic() {
        let dir = temp_dir("corrupt");
        let cwd = Path::new("/tmp/corrupt-proj");
        let idx = load_index(&dir, cwd);
        let file = session_file(&dir, cwd, &idx.current);
        std::fs::write(&file, b"NOT JSON {{{").unwrap();
        let loaded = load_entries(&dir, cwd, &idx.current);
        assert!(loaded.is_empty(), "损坏文件应回退空历史而非 panic");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_legacy_single_file_history() {
        let dir = temp_dir("migrate");
        let cwd = Path::new("/tmp/migrate-proj");
        // 写旧单文件历史
        let legacy = legacy_history_file(&dir, cwd);
        let entries = sample_entries();
        std::fs::write(&legacy, serde_json::to_vec(&entries).unwrap()).unwrap();
        assert!(legacy.exists());

        // load_index 应触发迁移
        let idx = load_index(&dir, cwd);
        assert_eq!(idx.sessions.len(), 1, "迁移应建 1 个 session");
        let migrated = &idx.sessions[0];
        assert_eq!(migrated.message_count, entries.len());
        assert_eq!(migrated.title, "你好", "title 应取首条 User 前 40 字符");
        // index.json + {id}.json 应已写
        assert!(index_file(&dir, cwd).exists());
        assert!(session_file(&dir, cwd, &migrated.id).exists());
        // 旧文件应已 rename 为 .legacy.bak
        assert!(!legacy.exists(), "旧文件应已 rename");
        assert!(legacy.with_extension("json.legacy.bak").exists());

        // 再 load_index 应直接读新结构（不重复迁移）
        let idx2 = load_index(&dir, cwd);
        assert_eq!(idx2.sessions.len(), 1);
        assert_eq!(idx2.sessions[0].id, migrated.id);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrate_legacy_with_no_user_uses_default_title() {
        let dir = temp_dir("migrate-no-user");
        let cwd = Path::new("/tmp/migrate-no-user-proj");
        let legacy = legacy_history_file(&dir, cwd);
        let entries = vec![ChatEntry::System("just system".into())];
        std::fs::write(&legacy, serde_json::to_vec(&entries).unwrap()).unwrap();
        let idx = load_index(&dir, cwd);
        assert_eq!(idx.sessions[0].title, MIGRATED_SESSION_TITLE);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_session_removes_file_and_index_entry() {
        let dir = temp_dir("delete");
        let cwd = Path::new("/tmp/delete-proj");
        let mut idx = load_index(&dir, cwd);
        let s1 = idx.current.clone();
        save_entries(&dir, cwd, &s1, &[ChatEntry::User("s1".into())]);
        // 加第二个 session
        let meta2 = create_session_meta();
        let s2 = meta2.id.clone();
        idx.sessions.push(meta2);
        idx.current = s2.clone();
        save_index(&dir, cwd, &idx);
        save_entries(&dir, cwd, &s2, &[ChatEntry::User("s2".into())]);

        // 删除 s1
        let remaining = delete_session(&dir, cwd, &s1);
        assert_eq!(remaining, 1);
        assert!(!session_file(&dir, cwd, &s1).exists(), "s1 文件应已删");
        let reloaded = load_index(&dir, cwd);
        assert_eq!(reloaded.sessions.len(), 1);
        assert!(reloaded.sessions.iter().all(|s| s.id != s1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_last_session_creates_new_empty() {
        let dir = temp_dir("delete-last");
        let cwd = Path::new("/tmp/delete-last-proj");
        let idx = load_index(&dir, cwd);
        let s1 = idx.current.clone();
        save_entries(&dir, cwd, &s1, &[ChatEntry::User("s1".into())]);

        // 删除唯一 session → 应建新空 session
        let remaining = delete_session(&dir, cwd, &s1);
        assert_eq!(remaining, 1, "删最后一个应自动建新空 session");
        let reloaded = load_index(&dir, cwd);
        assert_ne!(reloaded.current, s1, "current 应已切到新 session");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_current_switches_to_first_remaining() {
        let dir = temp_dir("delete-cur");
        let cwd = Path::new("/tmp/delete-cur-proj");
        let mut idx = load_index(&dir, cwd);
        let s1 = idx.current.clone();
        let meta2 = create_session_meta();
        let s2 = meta2.id.clone();
        idx.sessions.push(meta2);
        idx.current = s2.clone(); // current 是 s2
        save_index(&dir, cwd, &idx);

        // 删除 current (s2) → 应切到 s1
        let remaining = delete_session(&dir, cwd, &s2);
        assert_eq!(remaining, 1);
        let reloaded = load_index(&dir, cwd);
        assert_eq!(reloaded.current, s1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_session_text_formats_entries() {
        let dir = temp_dir("read");
        let cwd = Path::new("/tmp/read-proj");
        let idx = load_index(&dir, cwd);
        let entries = vec![
            ChatEntry::User("hello".into()),
            ChatEntry::Assistant("hi there".into()),
            ChatEntry::System("note".into()),
        ];
        save_entries(&dir, cwd, &idx.current, &entries);
        let text = read_session_text(&dir, cwd, &idx.current).unwrap();
        assert!(text.contains("🧑 hello"));
        assert!(text.contains("🤖 hi there"));
        assert!(text.contains("ℹ️ note"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_session_text_none_for_empty() {
        let dir = temp_dir("read-empty");
        let cwd = Path::new("/tmp/read-empty-proj");
        let idx = load_index(&dir, cwd);
        let text = read_session_text(&dir, cwd, &idx.current);
        assert!(text.is_none(), "空 session 应返回 None");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_current_returns_index_and_entries() {
        let dir = temp_dir("loadcur");
        let cwd = Path::new("/tmp/loadcur-proj");
        // 先存点数据
        let idx = load_index(&dir, cwd);
        save_entries(&dir, cwd, &idx.current, &[ChatEntry::User("loaded".into())]);
        // load_current 应同时返回 index 和 entries
        let (idx2, entries) = load_current(&dir, cwd);
        assert_eq!(idx2.current, idx.current);
        assert_eq!(entries.len(), 1);
        assert!(matches!(&entries[0], ChatEntry::User(c) if c == "loaded"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_dir_uses_cwd_hash_subdir() {
        let dir = temp_dir("sdir");
        let cwd = Path::new("/some/path");
        let sd = session_dir(&dir, cwd);
        let expected_name = cwd_hash(cwd);
        assert_eq!(
            sd.file_name().unwrap().to_string_lossy(),
            expected_name,
            "session 目录名应为 cwd_hash"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_index_get_and_get_mut() {
        let mut idx = SessionIndex {
            current: "a".into(),
            sessions: vec![
                SessionMeta {
                    id: "a".into(),
                    title: "A".into(),
                    created_at: 0,
                    updated_at: 0,
                    message_count: 0,
                },
                SessionMeta {
                    id: "b".into(),
                    title: "B".into(),
                    created_at: 0,
                    updated_at: 0,
                    message_count: 0,
                },
            ],
        };
        assert_eq!(idx.get("a").unwrap().title, "A");
        assert!(idx.get("c").is_none());
        assert_eq!(idx.current_meta().unwrap().title, "A");
        idx.get_mut("b").unwrap().title = "B-modified".into();
        assert_eq!(idx.get("b").unwrap().title, "B-modified");
    }
}
