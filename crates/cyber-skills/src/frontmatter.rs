//! SKILL.md frontmatter 解析。
//!
//! 复用 `cyber-core/src/project.rs:63` 的 parse 模式（BOM strip + `---` 分隔 +
//! serde_yaml），而非泛型化 core 的 parse——避免影响 core 既有 38 个测试。
//! SKILL.md 约定与 `.cyber.md` 一致：首行 `---`，下一个独占一行的 `---` 为
//! frontmatter 结束，其后为 markdown 正文（渐进式披露的"使用说明"）。

use serde::{Deserialize, Serialize};

use crate::error::{Result, SkillError};

/// SKILL.md frontmatter（YAML）。
///
/// - `name`：Skill 唯一标识（必填），用于 `skill_<name>` 工具命名与 `/skill <name>` 查找。
/// - `description`：简短描述，注入工具 schema 供 LLM 判断是否调用（渐进式披露第一层）。
/// - `triggers`：触发词（v0.1 仅展示，不做自动匹配；为未来自动触发预留）。
/// - `tools`：声明依赖的工具（仅文档用途，不强制）。
/// - `allowed_tools`：Skill 运行期间预批准的工具列表（Claude Code 风格，含 MCP 工具
///   如 `mcp__server__tool`）。v0.1 仅文档展示，不强制权限隔离。
/// - `disable_model_invocation`：设为 true 时仅用户可 `/skill <name>` 显式调用，
///   禁止模型自动调用（Claude Code 风格）。SkillTool schema 会标注「仅显式调用」。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SkillFrontmatter {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub triggers: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default, alias = "allowed-tools")]
    pub allowed_tools: Vec<String>,
    #[serde(default, alias = "disable-model-invocation")]
    pub disable_model_invocation: bool,
}

/// 解析 frontmatter + 正文。
///
/// 约定：文件首行为 `---`，下一个独占一行的 `---` 为 frontmatter 结束，
/// 其后为 markdown 正文。无 frontmatter 时整体作为正文（frontmatter 字段全默认）。
/// 自动剥离 UTF-8 BOM（`\u{feff}`），与 `.cyber.md` 一致（serde_yaml 不容忍 BOM）。
pub fn parse(raw: &str) -> Result<(SkillFrontmatter, String)> {
    let s = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = s.lines();

    // 首行必须是 `---`
    match lines.next() {
        Some(l) if l.trim() == "---" => {}
        _ => return Ok((SkillFrontmatter::default(), s.to_string())),
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
        return Ok((SkillFrontmatter::default(), s.to_string()));
    }

    let fm_src = fm_lines.join("\n");
    let body: String = lines.collect::<Vec<_>>().join("\n");
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(&fm_src).map_err(SkillError::Yaml)?;
    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let raw = "---\nname: src-recon\ndescription: SRC 子域收集与存活探测\ntriggers:\n  - 子域\n  - recon\ntools:\n  - subfinder\n---\n# 使用说明\n先 subfinder 再 httpx\n";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.name, "src-recon");
        assert_eq!(fm.description, "SRC 子域收集与存活探测");
        assert_eq!(fm.triggers, vec!["子域", "recon"]);
        assert_eq!(fm.tools, vec!["subfinder"]);
        assert!(body.contains("# 使用说明"));
        assert!(body.contains("httpx"));
    }

    #[test]
    fn parses_frontmatter_with_special_chars() {
        // YAML 中含 `*` 等特殊字符的值必须加引号（*.example.com 会被识别为 alias）
        let raw = "---\nname: scan\ndescription: \"*.example.com 扫描\"\n---\nbody\n";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.name, "scan");
        assert_eq!(fm.description, "*.example.com 扫描");
        assert!(body.contains("body"));
    }

    #[test]
    fn strips_bom() {
        let raw = "\u{feff}---\nname: x\n---\nbody\n";
        let (fm, body) = parse(raw).unwrap();
        assert_eq!(fm.name, "x");
        assert!(body.contains("body"));
    }

    #[test]
    fn no_frontmatter_treats_all_as_body() {
        let raw = "# 只有正文\n没有 frontmatter\n";
        let (fm, body) = parse(raw).unwrap();
        assert!(fm.name.is_empty());
        assert!(body.contains("只有正文"));
    }

    #[test]
    fn no_closing_delimiter_treats_all_as_body() {
        let raw = "---\nname: x\nbut no closing\n";
        let (fm, _body) = parse(raw).unwrap();
        // 没有结束分隔符 → 视作无 frontmatter
        assert!(fm.name.is_empty(), "无结束分隔符应视作无 frontmatter");
    }

    #[test]
    fn empty_frontmatter_fields_default() {
        let raw = "---\nname: minimal\n---\nbody\n";
        let (fm, _body) = parse(raw).unwrap();
        assert_eq!(fm.name, "minimal");
        assert!(fm.description.is_empty());
        assert!(fm.triggers.is_empty());
        assert!(fm.tools.is_empty());
        assert!(fm.allowed_tools.is_empty());
        assert!(!fm.disable_model_invocation);
    }

    #[test]
    fn parses_allowed_tools_snake_case() {
        let raw = "---\nname: x\nallowed_tools:\n  - mcp__ctx__create\n  - read_file\n---\nbody\n";
        let (fm, _) = parse(raw).unwrap();
        assert_eq!(fm.allowed_tools, vec!["mcp__ctx__create", "read_file"]);
    }

    #[test]
    fn parses_allowed_tools_kebab_alias() {
        // Claude Code 风格 kebab-case 别名
        let raw = "---\nname: x\nallowed-tools:\n  - mcp__ctx__search\n---\nbody\n";
        let (fm, _) = parse(raw).unwrap();
        assert_eq!(fm.allowed_tools, vec!["mcp__ctx__search"]);
    }

    #[test]
    fn parses_disable_model_invocation_snake_case() {
        let raw = "---\nname: manual\ndisable_model_invocation: true\n---\nbody\n";
        let (fm, _) = parse(raw).unwrap();
        assert!(fm.disable_model_invocation);
    }

    #[test]
    fn parses_disable_model_invocation_kebab_alias() {
        let raw = "---\nname: manual\ndisable-model-invocation: true\n---\nbody\n";
        let (fm, _) = parse(raw).unwrap();
        assert!(fm.disable_model_invocation);
    }

    #[test]
    fn disable_model_invocation_defaults_false() {
        let raw = "---\nname: auto\n---\nbody\n";
        let (fm, _) = parse(raw).unwrap();
        assert!(!fm.disable_model_invocation);
    }
}
