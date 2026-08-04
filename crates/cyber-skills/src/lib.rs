//! cyber-skills: Skill 加载、注册、Tool 暴露。
//!
//! P3 实现：SKILL.md frontmatter 解析（复用 `.cyber.md` 的 BOM + `---` + serde_yaml
//! 模式）、目录扫描（全局 `~/.cyber/skills/` + 项目级 `<cwd>/.cyber/skills/`，项目级
//! 覆盖同名）、渐进式披露（Skill 包成 `skill_<name>` 工具，schema 含描述+触发词，
//! 调用返回 body）。
//!
//! v0.1 仅显式触发（`/skill <name>` 命令 + `skill_<name>` 工具调用）；不做 triggers
//! 自动匹配（保留 `triggers` 字段供未来用）。
//!
//! 通过实现 `cyber_agent::Tool` trait 注入统一工具表（cyber-agent 不反向依赖本 crate）。

pub mod error;
pub mod frontmatter;
pub mod registry;
pub mod skill;
pub mod tool;

pub use error::{Result, SkillError};
pub use frontmatter::SkillFrontmatter;
pub use registry::SkillRegistry;
pub use skill::{Skill, SkillSource};
pub use tool::SkillTool;
