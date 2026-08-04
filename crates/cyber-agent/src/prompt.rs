//! 系统提示词组装：base + 项目 frontmatter + 安全护栏。

use cyber_core::ProjectContext;

/// 基础系统提示词。
pub const BASE_PROMPT: &str = "你是 Cyber Master，一个网络安全智能体终端助手。\
你遵循用户 .cyber.md 中声明的授权范围与安全护栏。未授权目标一律拒绝执行，\
禁止破坏性操作（删库、DoS、未授权入侵）。回答简洁、可执行。";

/// 组装系统提示词：base + 项目上下文 + rules 护栏段。
///
/// `body`（.cyber.md 正文）暂不注入，避免上下文膨胀（留 P2.2 按需引用）。
pub fn build_system_prompt(project: Option<&ProjectContext>) -> String {
    let mut s = BASE_PROMPT.to_string();
    let Some(p) = project else {
        return s;
    };
    let f = &p.frontmatter;
    s.push_str("\n\n# 项目上下文");
    let mut pushed = false;
    if let Some(v) = &f.project {
        s.push_str(&format!("\n- project: {v}"));
        pushed = true;
    }
    if let Some(v) = &f.scope {
        s.push_str(&format!("\n- scope: {v}"));
        pushed = true;
    }
    if let Some(v) = &f.authorization {
        s.push_str(&format!("\n- authorization: {v}"));
        pushed = true;
    }
    if let Some(v) = &f.owner {
        s.push_str(&format!("\n- owner: {v}"));
        pushed = true;
    }
    if !pushed {
        s.push_str("（frontmatter 无结构化字段）");
    }
    if !f.rules.is_empty() {
        s.push_str("\n\n# 安全护栏（必须遵守）");
        for r in &f.rules {
            s.push_str(&format!("\n- {r}"));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::{ProjectContext, ProjectFrontmatter};

    fn ctx(fm: ProjectFrontmatter) -> ProjectContext {
        ProjectContext {
            frontmatter: fm,
            body: String::new(),
            raw: String::new(),
            path: std::path::PathBuf::new(),
        }
    }

    #[test]
    fn no_project_just_base() {
        let s = build_system_prompt(None);
        assert!(s.contains("Cyber Master"));
        assert!(!s.contains("项目上下文"));
    }

    #[test]
    fn with_full_frontmatter_and_rules() {
        let fm = ProjectFrontmatter {
            project: Some("demo".into()),
            scope: Some("*.example.com".into()),
            authorization: Some("书面授权".into()),
            owner: Some("sec-team".into()),
            rules: vec!["禁止 DoS".into(), "仅工作时间".into()],
        };
        let s = build_system_prompt(Some(&ctx(fm)));
        assert!(s.contains("project: demo"));
        assert!(s.contains("scope: *.example.com"));
        assert!(s.contains("authorization: 书面授权"));
        assert!(s.contains("owner: sec-team"));
        assert!(s.contains("安全护栏"));
        assert!(s.contains("禁止 DoS"));
        assert!(s.contains("仅工作时间"));
    }

    #[test]
    fn empty_frontmatter_shows_placeholder() {
        let s = build_system_prompt(Some(&ctx(ProjectFrontmatter::default())));
        assert!(s.contains("frontmatter 无结构化字段"));
        // BASE_PROMPT 本身含"安全护栏"字样；此处仅校验 rules 段未追加。
        assert!(!s.contains("# 安全护栏"));
        assert!(!s.contains("（必须遵守）"));
    }
}
