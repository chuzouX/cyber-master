//! Skill 注册表：扫描全局 + 项目级 skills 目录，聚合去重。
//!
//! 目录结构（DESIGN §7.1）：每个子目录一个 `SKILL.md`，如
//! `~/.cyber/skills/src-recon/SKILL.md`。项目级（`<cwd>/.cyber/skills/`）
//! 覆盖全局同名 skill（按 `frontmatter.name` 去重，Project 优先）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, warn};

use crate::error::SkillError;
use crate::skill::{Skill, SkillSource};

/// Skill 注册表。持有 `Arc<Skill>` 供 `SkillTool` 共享（一个 Skill 可被多次
/// 「调用」——渐进式披露每次返回 body 副本，Skill 本身不可变）。
#[derive(Debug, Default)]
pub struct SkillRegistry {
    skills: Vec<Arc<Skill>>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 扫描全局 + 项目级 skills 目录，加载所有 `SKILL.md`。
    ///
    /// 返回 `(registry, errors)`：单个 Skill 加载失败不阻断其余，失败项进 `errors`
    ///（含路径与错误），调用方据此 warn + toast。项目级目录为 `None` 或不存在时跳过。
    pub fn load_all(
        global_dir: &Path,
        project_dir: Option<&Path>,
    ) -> (Self, Vec<(PathBuf, SkillError)>) {
        let mut skills: Vec<Arc<Skill>> = Vec::new();
        let mut errors: Vec<(PathBuf, SkillError)> = Vec::new();

        // 先加载全局，再加载项目级；项目级覆盖同名（按 frontmatter.name）
        scan_dir(global_dir, SkillSource::Global, &mut skills, &mut errors);
        if let Some(dir) = project_dir {
            scan_dir(dir, SkillSource::Project, &mut skills, &mut errors);
        }

        // 去重：项目级覆盖全局同名（保留 Project，丢弃 Global）
        skills = dedup_by_name(skills);

        debug!(count = skills.len(), errors = errors.len(), "Skill 注册表加载完成");
        (Self { skills }, errors)
    }

    /// 迭代所有 Skill。
    pub fn iter(&self) -> impl Iterator<Item = &Arc<Skill>> {
        self.skills.iter()
    }

    /// Skill 数量。
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// 按名查找 Skill（大小写敏感，与 `skill_<name>` 工具命名一致）。
    pub fn find(&self, name: &str) -> Option<&Arc<Skill>> {
        self.skills.iter().find(|s| s.name() == name)
    }
}

/// 扫描单个目录下的子目录，加载每个 `SKILL.md`。
fn scan_dir(
    dir: &Path,
    source: SkillSource,
    skills: &mut Vec<Arc<Skill>>,
    errors: &mut Vec<(PathBuf, SkillError)>,
) {
    if !dir.exists() {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "读取 skills 目录失败，跳过");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        match Skill::load(&skill_md, source) {
            Ok(s) => skills.push(Arc::new(s)),
            Err(e) => {
                warn!(path = %skill_md.display(), error = %e, "Skill 加载失败，已跳过");
                errors.push((skill_md, e));
            }
        }
    }
}

/// 按 `frontmatter.name` 去重：项目级（Project）覆盖全局（Global）同名。
/// 保留首次出现的（Project 先于 Global 不成立——Global 先加载，Project 后加载，
/// 故同名时 Project 应替换 Global）。实现：按 name 分组，Project 优先保留。
fn dedup_by_name(skills: Vec<Arc<Skill>>) -> Vec<Arc<Skill>> {
    // 后加载的 Project 同名应覆盖先加载的 Global：用「保留最后出现的」语义
    // 但需保留不同 name 的全部。用 HashMap<name, index>，遇同名覆盖 index。
    use std::collections::HashMap;
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let mut out: Vec<Arc<Skill>> = Vec::new();
    for s in skills {
        let name = s.name().to_string();
        if let Some(idx) = by_name.get(&name) {
            // 同名覆盖（Project 后加载，覆盖 Global）
            out[*idx] = s;
        } else {
            by_name.insert(name, out.len());
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_skillreg_test_{}_{}_{}",
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

    fn write_skill(dir: &Path, name: &str, desc: &str) {
        let skill_dir = dir.join(name);
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n# {name} body\n"),
        )
        .unwrap();
    }

    #[test]
    fn load_all_from_global_only() {
        let g = tmpdir("g_only");
        write_skill(&g, "alpha", "a");
        write_skill(&g, "beta", "b");
        let (reg, errs) = SkillRegistry::load_all(&g, None);
        assert!(errs.is_empty());
        assert_eq!(reg.len(), 2);
        assert!(reg.find("alpha").is_some());
        assert!(reg.find("beta").is_some());
        assert!(reg.find("missing").is_none());
        let _ = fs::remove_dir_all(&g);
    }

    #[test]
    fn project_overrides_global_same_name() {
        let g = tmpdir("pog_g");
        let p = tmpdir("pog_p");
        write_skill(&g, "shared", "global version");
        write_skill(&p, "shared", "project version");
        let (reg, _errs) = SkillRegistry::load_all(&g, Some(&p));
        assert_eq!(reg.len(), 1, "同名应去重为 1");
        let s = reg.find("shared").unwrap();
        assert_eq!(s.source, SkillSource::Project, "项目级应覆盖全局");
        assert_eq!(s.frontmatter.description, "project version");
        let _ = fs::remove_dir_all(&g);
        let _ = fs::remove_dir_all(&p);
    }

    #[test]
    fn project_adds_new_skills() {
        let g = tmpdir("pan_g");
        let p = tmpdir("pan_p");
        write_skill(&g, "global_only", "g");
        write_skill(&p, "project_only", "p");
        let (reg, _errs) = SkillRegistry::load_all(&g, Some(&p));
        assert_eq!(reg.len(), 2);
        assert!(reg.find("global_only").is_some());
        assert!(reg.find("project_only").is_some());
        let _ = fs::remove_dir_all(&g);
        let _ = fs::remove_dir_all(&p);
    }

    #[test]
    fn invalid_skill_goes_to_errors_not_panic() {
        let g = tmpdir("err_g");
        let bad_dir = g.join("bad");
        fs::create_dir_all(&bad_dir).unwrap();
        fs::write(bad_dir.join("SKILL.md"), "---\ndescription: no name\n---\nbody\n").unwrap();
        write_skill(&g, "good", "ok");
        let (reg, errs) = SkillRegistry::load_all(&g, None);
        assert_eq!(reg.len(), 1, "有效 skill 应加载");
        assert_eq!(errs.len(), 1, "无效 skill 应进 errors");
        assert!(errs[0].0.to_string_lossy().contains("SKILL.md"));
        let _ = fs::remove_dir_all(&g);
    }

    #[test]
    fn empty_dir_returns_empty() {
        let g = tmpdir("empty");
        let (reg, errs) = SkillRegistry::load_all(&g, None);
        assert!(reg.is_empty());
        assert!(errs.is_empty());
        let _ = fs::remove_dir_all(&g);
    }

    #[test]
    fn nonexistent_dir_returns_empty_no_error() {
        let g = tmpdir("nonexist").join("subdir_that_does_not_exist");
        let (reg, errs) = SkillRegistry::load_all(&g, None);
        assert!(reg.is_empty());
        assert!(errs.is_empty(), "不存在的目录不应产生 error");
    }

    #[test]
    fn ignores_files_without_skill_md() {
        let g = tmpdir("nofm");
        // 子目录无 SKILL.md
        fs::create_dir_all(g.join("no_skill_md")).unwrap();
        // 非 SKILL.md 的文件
        fs::write(g.join("no_skill_md").join("README.md"), "hello").unwrap();
        write_skill(&g, "real", "ok");
        let (reg, errs) = SkillRegistry::load_all(&g, None);
        assert_eq!(reg.len(), 1, "仅 real 应加载");
        assert!(errs.is_empty(), "无 SKILL.md 的子目录应静默跳过，不入 errors");
        let _ = fs::remove_dir_all(&g);
    }
}
