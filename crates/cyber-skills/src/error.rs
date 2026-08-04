//! Skill 层错误。
//!
//! 刻意不扩展 `cyber-core::CoreError`：SKILL.md 解析错误属 skills 层职责，
//! 且 core 不应引入 serde_yaml 之外的 skills 语义。既有 core/tui 测试不受影响。

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, SkillError>;

#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    #[error("io/读取失败: {0}")]
    Core(#[from] cyber_core::CoreError),

    #[error("frontmatter YAML 解析失败: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("SKILL.md 缺少必填字段 `name`：{0}")]
    MissingName(String),

    #[error("SKILL.md 路径不是文件: {0}")]
    NotAFile(PathBuf),
}
