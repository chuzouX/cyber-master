//! 用户记忆管理：全局 + 项目级两层 markdown 文件。
//!
//! 类似 ChatGPT/Codex 的跨会话记忆 + Claude Code 的项目记忆：
//! - 全局记忆：`~/.cyber/memory.md`，跨所有项目共享（用户偏好、身份、通用约定）
//! - 项目级记忆：`<cwd>/.cyber/memory.md`，仅当前项目（项目特定约定、进度）
//!
//! 两层记忆合并后注入系统提示词；agent 通过 `save_memory` 工具或 `/memory` 命令写入。
//! 文件为纯 markdown，每行一条 `- 内容` 条目，便于阅读与手动编辑。

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::Result;

/// 记忆作用域：全局（跨项目）或项目级（当前工作目录）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryScope {
    Global,
    Project,
}

impl MemoryScope {
    /// 从字符串解析作用域。`project`/`local` → 项目级，其余（含空）→ 全局。
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "project" | "local" => MemoryScope::Project,
            _ => MemoryScope::Global,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub index: usize,
    pub content: String,
}


/// 记忆存储：封装两层记忆文件的读写。
#[derive(Debug, Clone)]
pub struct MemoryStore {
    global_file: PathBuf,
    project_file: PathBuf,
}

impl MemoryStore {
    pub fn new(global_file: PathBuf, project_file: PathBuf) -> Self {
        Self {
            global_file,
            project_file,
        }
    }

    /// 读取全局记忆（文件不存在 → 空串）。
    pub fn load_global(&self) -> String {
        read_if_exists(&self.global_file)
    }

    /// 读取项目级记忆（文件不存在 → 空串）。
    pub fn load_project(&self) -> String {
        read_if_exists(&self.project_file)
    }

    /// 合并两层记忆（全局 + 项目级），供系统提示词注入。两层均空时返回空串。
    pub fn load_all(&self) -> String {
        let g = self.load_global();
        let p = self.load_project();
        match (g.is_empty(), p.is_empty()) {
            (true, true) => String::new(),
            (false, true) => g,
            (true, false) => p,
            (false, false) => format!("{g}\n{p}"),
        }
    }

    /// 追加一条记忆到指定作用域。空白文本忽略。
    pub fn append(&self, scope: MemoryScope, text: &str) -> Result<()> {
        let path = match scope {
            MemoryScope::Global => &self.global_file,
            MemoryScope::Project => &self.project_file,
        };
        append_memory(path, text)
    }

    pub fn entries(&self, scope: MemoryScope) -> Vec<MemoryEntry> {
        let path = match scope { MemoryScope::Global => &self.global_file, MemoryScope::Project => &self.project_file };
        read_if_exists(path).lines().filter_map(|line| line.strip_prefix("- ")).enumerate().map(|(i, content)| MemoryEntry { index: i + 1, content: content.to_string() }).collect()
    }

    pub fn update(&self, scope: MemoryScope, index: usize, text: &str) -> Result<()> {
        let path = match scope { MemoryScope::Global => &self.global_file, MemoryScope::Project => &self.project_file };
        rewrite_entry(path, index, Some(text))
    }

    pub fn delete(&self, scope: MemoryScope, index: usize) -> Result<()> {
        let path = match scope { MemoryScope::Global => &self.global_file, MemoryScope::Project => &self.project_file };
        rewrite_entry(path, index, None)
    }

}
/// 读取文件内容；不存在或读取失败返回空串（记忆非关键，失败静默降级）。
fn read_if_exists(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// 追加一条 `- 内容` 到记忆文件。文件不存在时创建；父目录不存在时创建。
fn append_memory(path: &Path, text: &str) -> Result<()> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // 确保与已有内容之间有换行分隔
    let existing = read_if_exists(path);
    let prefix = if existing.is_empty() || existing.ends_with('\n') {
        String::new()
    } else {
        "\n".to_string()
    };
    let content = format!("{prefix}- {t}\n");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(content.as_bytes())?;
    Ok(())
}

fn rewrite_entry(path: &Path, index: usize, replacement: Option<&str>) -> Result<()> {
    if index == 0 { return Err(crate::error::CoreError::Config("记忆编号从 1 开始".into())); }
    let source = std::fs::read_to_string(path)?;
    let trailing_newline = source.ends_with('\n');
    let mut lines: Vec<String> = source.split('\n').map(ToString::to_string).collect();
    if trailing_newline { lines.pop(); }
    let line_index = lines.iter().enumerate().filter(|(_, line)| line.starts_with("- ")).nth(index - 1).map(|(i, _)| i).ok_or_else(|| crate::error::CoreError::Config(format!("未找到第 {index} 条记忆")))?;
    if let Some(text) = replacement {
        let content = text.lines().map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>().join(" ");
        if content.is_empty() { return Err(crate::error::CoreError::Config("记忆内容不能为空".into())); }
        lines[line_index] = format!("- {content}");
    } else {
        lines.remove(line_index);
    }
    let mut rewritten = lines.join("\n");
    if trailing_newline && !rewritten.is_empty() { rewritten.push('\n'); }
    std::fs::write(path, rewritten)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cyber_memory_{}_{}",
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn scope_parse_global_default() {
        assert_eq!(MemoryScope::parse(""), MemoryScope::Global);
        assert_eq!(MemoryScope::parse("global"), MemoryScope::Global);
        assert_eq!(MemoryScope::parse("unknown"), MemoryScope::Global);
    }

    #[test]
    fn scope_parse_project() {
        assert_eq!(MemoryScope::parse("project"), MemoryScope::Project);
        assert_eq!(MemoryScope::parse("local"), MemoryScope::Project);
        assert_eq!(MemoryScope::parse("PROJECT"), MemoryScope::Project);
    }

    #[test]
    fn append_creates_file_with_bullet() {
        let g = tmp_path("g1");
        let p = tmp_path("p1");
        let store = MemoryStore::new(g.clone(), p);
        store.append(MemoryScope::Global, "用户偏好 Python").unwrap();
        let content = std::fs::read_to_string(&g).unwrap();
        assert_eq!(content, "- 用户偏好 Python\n");
        let _ = std::fs::remove_file(&g);
    }

    #[test]
    fn append_multiple_entries_on_newlines() {
        let g = tmp_path("g2");
        let p = tmp_path("p2");
        let store = MemoryStore::new(g.clone(), p);
        store.append(MemoryScope::Global, "第一条").unwrap();
        store.append(MemoryScope::Global, "第二条").unwrap();
        let content = std::fs::read_to_string(&g).unwrap();
        assert_eq!(content, "- 第一条\n- 第二条\n");
        let _ = std::fs::remove_file(&g);
    }

    #[test]
    fn append_blank_is_noop() {
        let g = tmp_path("g3");
        let p = tmp_path("p3");
        let store = MemoryStore::new(g.clone(), p);
        store.append(MemoryScope::Global, "   ").unwrap();
        assert!(!g.exists(), "空白文本不应创建文件");
    }

    #[test]
    fn load_all_merges_two_layers() {
        let g = tmp_path("g4");
        let p = tmp_path("p4");
        let store = MemoryStore::new(g.clone(), p.clone());
        store.append(MemoryScope::Global, "全局记忆").unwrap();
        store.append(MemoryScope::Project, "项目记忆").unwrap();
        let all = store.load_all();
        assert!(all.contains("全局记忆"));
        assert!(all.contains("项目记忆"));
        let _ = std::fs::remove_file(&g);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn load_all_empty_when_no_files() {
        let g = tmp_path("g5");
        let p = tmp_path("p5");
        let store = MemoryStore::new(g, p);
        assert_eq!(store.load_all(), "");
    }
}
