use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{CoreError, Result};
use crate::fsutil::read_utf8;

/// `.cyber.md` 的 YAML frontmatter，提供结构化字段与安全护栏。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectFrontmatter {
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub authorization: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    /// 安全规则，注入 agent 系统提示词作为护栏。
    #[serde(default)]
    pub rules: Vec<String>,
}

/// 已解析的项目上下文。
#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub frontmatter: ProjectFrontmatter,
    pub body: String,
    pub raw: String,
    pub path: PathBuf,
}

impl ProjectContext {
    /// 读取并解析 `.cyber.md` 文件。
    pub fn load(path: &Path) -> Result<Self> {
        let raw = read_utf8(path)?;
        let (frontmatter, body) = parse(&raw)?;
        debug!(
            path = %path.display(),
            project = ?frontmatter.project,
            rules = frontmatter.rules.len(),
            "解析 .cyber.md 完成"
        );
        Ok(Self {
            frontmatter,
            body,
            raw,
            path: path.to_path_buf(),
        })
    }

    /// 安全规则（注入 agent 系统提示词）。
    pub fn rules(&self) -> &[String] {
        &self.frontmatter.rules
    }
}

/// 解析 frontmatter + 正文。
///
/// 约定：文件首行为 `---`，下一个独占一行的 `---` 为 frontmatter 结束，
/// 其后为 markdown 正文。无 frontmatter 时整体作为正文。
pub fn parse(raw: &str) -> Result<(ProjectFrontmatter, String)> {
    let s = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = s.lines();

    // 首行必须是 `---`
    match lines.next() {
        Some(l) if l.trim() == "---" => {}
        _ => return Ok((ProjectFrontmatter::default(), s.to_string())),
    }

    // 收集 frontmatter 直到下一个独占一行的 `---`
    let mut fm_lines = Vec::new();
    let mut found_end = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            found_end = true;
            break;
        }
        fm_lines.push(line);
    }
    if !found_end {
        // 没有结束分隔符，视作无 frontmatter
        return Ok((ProjectFrontmatter::default(), s.to_string()));
    }

    let fm_src = fm_lines.join("\n");
    let body: String = lines.collect::<Vec<_>>().join("\n");
    let frontmatter: ProjectFrontmatter =
        serde_yaml::from_str(&fm_src).map_err(CoreError::Yaml)?;
    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        // YAML 中含 `*` 等特殊字符的值必须加引号（*.example.com 会被识别为 alias）
        let raw = "---\nproject: demo\nscope: \"*.example.com\"\nrules:\n  - 禁止 DoS\n---\n# 正文\n说明文字\n";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.project.as_deref(), Some("demo"));
        assert_eq!(fm.scope.as_deref(), Some("*.example.com"));
        assert_eq!(fm.rules, vec!["禁止 DoS"]);
        assert!(body.contains("# 正文"));
    }

    #[test]
    fn no_frontmatter_treats_all_as_body() {
        let raw = "# 只有正文\n没有 frontmatter\n";
        let (fm, body) = parse(raw).unwrap();
        assert!(fm.project.is_none());
        assert!(body.contains("只有正文"));
    }
}
