//! SkillTool：把 Skill 暴露为 cyber-agent 的 `Tool`（渐进式披露）。
//!
//! 每个 Skill 包成 `skill_<name>` 工具：
//! - `schema.description` = skill 描述 + 触发词（LLM 据此判断是否调用 = 第一层披露）
//! - `run` 返回 skill body（详细使用说明 = 第二层披露）
//!
//! 命名 `skill_<name>` 与 builtins / `mcp_<server>_<tool>` 前缀隔离。

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use cyber_agent::{AgentError, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde_json::Value;

use crate::skill::Skill;

/// 把一个 `Skill` 包装成可注入 `ToolRegistry` 的 `Tool`。
pub struct SkillTool {
    skill: Arc<Skill>,
}

impl SkillTool {
    pub fn new(skill: Arc<Skill>) -> Self {
        Self { skill }
    }
}

impl Tool for SkillTool {
    fn schema(&self) -> ToolSchema {
        let s = &self.skill;
        let mut desc = format!("[Skill] {}", s.frontmatter.description);
        if !s.frontmatter.triggers.is_empty() {
            desc.push_str(&format!("\n触发词: {}", s.frontmatter.triggers.join(", ")));
        }
        if !s.frontmatter.allowed_tools.is_empty() {
            desc.push_str(&format!(
                "\n预批准工具: {}",
                s.frontmatter.allowed_tools.join(", ")
            ));
        }
        if s.frontmatter.disable_model_invocation {
            desc.push_str("\n⚠ 仅显式调用（/skill），模型不应自动调用此 Skill。");
        }
        desc.push_str("\n\n调用此工具以获取详细使用说明（渐进式披露）。");
        ToolSchema {
            name: format!("skill_{}", s.name()),
            description: desc,
            // Skill 工具无需参数
            parameters: serde_json::json!({"type": "object", "properties": {}}),
        }
    }

    fn run<'a>(
        &'a self,
        _input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput, AgentError>> + Send + 'a>> {
        let body = self.skill.body.clone();
        let dir = self.skill.path.parent().map(|p| p.to_path_buf());
        Box::pin(async move {
            let mut content = body;
            // 追加知识库目录，使 agent 能用 read_file 读取 skill 引用的 .md 资源文件
            if let Some(dir) = dir.filter(|d| !d.as_os_str().is_empty()) {
                content.push_str("\n\n---\n");
                content.push_str(&format!(
                    "📁 本 Skill 知识库目录: {}\n",
                    dir.display()
                ));
                content.push_str(
                    "上方引用的 .md 资源文件可用 `read_file` 读取（完整路径 = 上述目录 + 文件名），\
                     获取详细技术指引与代码示例。遇到卡点时优先查阅对应知识库文件。",
                );
            }
            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::SkillFrontmatter;

    fn make_skill(name: &str, desc: &str, triggers: Vec<String>, body: &str) -> Arc<Skill> {
        Arc::new(Skill {
            frontmatter: SkillFrontmatter {
                name: name.into(),
                description: desc.into(),
                triggers,
                ..Default::default()
            },
            body: body.into(),
            path: std::path::PathBuf::new(),
            source: crate::skill::SkillSource::Global,
        })
    }

    fn ctx() -> ToolCtx {
        ToolCtx {
            cwd: std::env::temp_dir(),
            rules: vec![],
            scope: None,
        }
    }

    #[test]
    fn schema_name_prefixed_with_skill_() {
        let tool = SkillTool::new(make_skill("src-recon", "子域收集", vec![], "body"));
        assert_eq!(tool.schema().name, "skill_src-recon");
    }

    #[test]
    fn schema_description_contains_description_and_triggers() {
        let tool = SkillTool::new(
            make_skill("x", "扫描工具", vec!["scan".into(), "扫描".into()], "body"),
        );
        let desc = tool.schema().description;
        assert!(desc.contains("扫描工具"), "应含 description");
        assert!(desc.contains("scan"), "应含触发词");
        assert!(desc.contains("扫描"), "应含中文触发词");
        assert!(desc.contains("渐进式披露"));
    }

    #[test]
    fn schema_description_omits_triggers_line_when_empty() {
        let tool = SkillTool::new(make_skill("x", "desc", vec![], "body"));
        assert!(!tool.schema().description.contains("触发词"));
    }

    #[test]
    fn run_returns_body_as_content() {
        let tool = SkillTool::new(make_skill("x", "d", vec![], "# 说明\n先做 A 再做 B"));
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.run(serde_json::Value::Null, &ctx()))
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("# 说明"));
        assert!(out.content.contains("再做 B"));
    }

    #[test]
    fn run_ignores_input() {
        let tool = SkillTool::new(make_skill("x", "d", vec![], "body text"));
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(
                tool.run(
                    serde_json::json!({"ignored": "param"}),
                    &ctx(),
                ),
            )
            .unwrap();
        // 空 path → 不追加知识库目录
        assert_eq!(out.content, "body text");
    }

    #[test]
    fn run_appends_knowledge_base_dir_when_path_set() {
        // 有真实路径的 skill → 输出末尾追加知识库目录提示
        let skill = Skill {
            frontmatter: SkillFrontmatter {
                name: "ctf-web".into(),
                description: "Web 技能".into(),
                ..Default::default()
            },
            body: "见 [database_query_patterns.md](database_query_patterns.md)".into(),
            path: std::path::PathBuf::from("/skills/ctf-web/SKILL.md"),
            source: crate::skill::SkillSource::Global,
        };
        let tool = SkillTool::new(Arc::new(skill));
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.run(serde_json::Value::Null, &ctx()))
            .unwrap();
        assert!(out.content.contains("database_query_patterns.md"), "正文保留: {}", out.content);
        assert!(out.content.contains("知识库目录"), "应含目录提示: {}", out.content);
        assert!(
            out.content.contains("/skills/ctf-web"),
            "应含目录路径: {}",
            out.content
        );
        assert!(out.content.contains("read_file"), "应提示用 read_file: {}", out.content);
    }

    #[test]
    fn run_omits_dir_suffix_for_empty_path() {
        // 空 path → 不追加目录提示，输出 == body
        let tool = SkillTool::new(make_skill("x", "d", vec![], "just body"));
        let out = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(tool.run(serde_json::Value::Null, &ctx()))
            .unwrap();
        assert_eq!(out.content, "just body");
        assert!(!out.content.contains("知识库目录"));
    }

    #[test]
    fn schema_includes_allowed_tools_when_present() {
        let skill = Skill {
            frontmatter: SkillFrontmatter {
                name: "persist".into(),
                description: "保存到 contexts".into(),
                allowed_tools: vec![
                    "mcp__contexts__create".into(),
                    "mcp__contexts__search".into(),
                ],
                ..Default::default()
            },
            body: "body".into(),
            path: std::path::PathBuf::new(),
            source: crate::skill::SkillSource::Global,
        };
        let tool = SkillTool::new(Arc::new(skill));
        let desc = tool.schema().description;
        assert!(desc.contains("预批准工具"), "desc = {desc}");
        assert!(desc.contains("mcp__contexts__create"));
        assert!(desc.contains("mcp__contexts__search"));
    }

    #[test]
    fn schema_marks_disable_model_invocation() {
        let skill = Skill {
            frontmatter: SkillFrontmatter {
                name: "manual".into(),
                description: "仅手动调用".into(),
                disable_model_invocation: true,
                ..Default::default()
            },
            body: "body".into(),
            path: std::path::PathBuf::new(),
            source: crate::skill::SkillSource::Global,
        };
        let tool = SkillTool::new(Arc::new(skill));
        let desc = tool.schema().description;
        assert!(desc.contains("仅显式调用"), "desc = {desc}");
    }

    #[test]
    fn schema_omits_allowed_tools_line_when_empty() {
        let tool = SkillTool::new(make_skill("x", "d", vec![], "body"));
        assert!(!tool.schema().description.contains("预批准工具"));
    }

    #[test]
    fn schema_omits_disable_marker_when_false() {
        let tool = SkillTool::new(make_skill("x", "d", vec![], "body"));
        assert!(!tool.schema().description.contains("仅显式调用"));
    }
}
