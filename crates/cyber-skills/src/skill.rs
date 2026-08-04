//! Skill 结构与加载。

use std::path::{Path, PathBuf};

use tracing::debug;

use crate::error::{Result, SkillError};
use crate::frontmatter::{parse, SkillFrontmatter};

/// Skill 来源：全局（`~/.cyber/skills/`）或项目级（`<cwd>/.cyber/skills/`）。
/// 项目级覆盖全局同名 skill（`SkillRegistry::load_all` 去重时 Project 优先）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Global,
    Project,
}

/// 一个已加载的 Skill（解析自 `SKILL.md`）。
#[derive(Debug, Clone)]
pub struct Skill {
    pub frontmatter: SkillFrontmatter,
    /// markdown 正文（渐进式披露的"使用说明"，调用 `skill_<name>` 工具时返回）。
    pub body: String,
    /// SKILL.md 文件路径（`/skill list` 展示来源用）。
    pub path: PathBuf,
    pub source: SkillSource,
}

impl Skill {
    /// 读取并解析 `SKILL.md` 文件。
    ///
    /// 用 `cyber_core::fsutil::read_utf8`（区分 IO/编码错误并附路径上下文）+
    /// `frontmatter::parse`（BOM + `---` 分隔 + serde_yaml）。缺 `name` → Err。
    pub fn load(path: &Path, source: SkillSource) -> Result<Self> {
        if !path.is_file() {
            return Err(SkillError::NotAFile(path.to_path_buf()));
        }
        let raw = cyber_core::fsutil::read_utf8(path)?;
        let (frontmatter, body) = parse(&raw)?;
        if frontmatter.name.trim().is_empty() {
            return Err(SkillError::MissingName(path.display().to_string()));
        }
        debug!(
            path = %path.display(),
            name = %frontmatter.name,
            source = ?source,
            triggers = frontmatter.triggers.len(),
            "加载 Skill 完成"
        );
        Ok(Self {
            frontmatter,
            body,
            path: path.to_path_buf(),
            source,
        })
    }

    /// Skill 名称（frontmatter.name 的便捷访问）。
    pub fn name(&self) -> &str {
        &self.frontmatter.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_skill_test_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_skill(dir: &Path, name: &str, content: &str) -> PathBuf {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        let path = skill_dir.join("SKILL.md");
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_parses_valid_skill() {
        let dir = tmpdir("load_ok");
        let path = write_skill(
            &dir,
            "src-recon",
            "---\nname: src-recon\ndescription: 子域收集\ntriggers:\n  - recon\n---\n# 说明\n先 subfinder\n",
        );
        let skill = Skill::load(&path, SkillSource::Global).unwrap();
        assert_eq!(skill.name(), "src-recon");
        assert_eq!(skill.frontmatter.description, "子域收集");
        assert_eq!(skill.frontmatter.triggers, vec!["recon"]);
        assert!(skill.body.contains("subfinder"));
        assert_eq!(skill.source, SkillSource::Global);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_name_errors() {
        let dir = tmpdir("load_noname");
        let path = write_skill(&dir, "bad", "---\ndescription: no name\n---\nbody\n");
        match Skill::load(&path, SkillSource::Global) {
            Err(SkillError::MissingName(p)) => assert!(p.contains("SKILL.md")),
            other => panic!("期望 MissingName，实际: {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_empty_name_errors() {
        let dir = tmpdir("load_emptyname");
        let path = write_skill(&dir, "bad2", "---\nname: \"   \"\n---\nbody\n");
        assert!(Skill::load(&path, SkillSource::Global).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_not_a_file_errors() {
        let dir = tmpdir("load_dir");
        // 传一个目录路径
        match Skill::load(&dir, SkillSource::Global) {
            Err(SkillError::NotAFile(_)) => {}
            other => panic!("期望 NotAFile，实际: {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_strips_bom() {
        let dir = tmpdir("load_bom");
        let path = write_skill(&dir, "bom", "\u{feff}---\nname: bom\n---\nbody\n");
        let skill = Skill::load(&path, SkillSource::Project).unwrap();
        assert_eq!(skill.name(), "bom");
        assert_eq!(skill.source, SkillSource::Project);
        let _ = fs::remove_dir_all(&dir);
    }
}
