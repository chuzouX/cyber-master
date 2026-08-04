use std::path::{Path, PathBuf};

use tracing::{debug, error};

use crate::error::{CoreError, Result};

/// 全局 `~/.cyber` 下所有关键路径的缓存。
#[derive(Debug, Clone)]
pub struct Paths {
    pub cyber_home: PathBuf,
    pub config_file: PathBuf,
    pub providers_file: PathBuf,
    pub skills_dir: PathBuf,
    pub mcp_dir: PathBuf,
    pub mcp_servers_file: PathBuf,
    pub workflows_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub reports_dir: PathBuf,
    pub reports_templates_dir: PathBuf,
    pub history_db: PathBuf,
    /// 对话历史 JSON 目录（P2.2：按 cwd hash 存 `{cwd_hash}.json`）。
    pub history_dir: PathBuf,
    pub assets_db: PathBuf,
    /// CTF 题目数据目录（`~/.cyber/ctf/`）。
    pub ctf_dir: PathBuf,
    /// CTF writeup 输出目录（`~/.cyber/ctf/writeup/`）。
    pub ctf_writeup_dir: PathBuf,
}

impl Paths {
    /// 定位 `~/.cyber`，不保证目录已存在（首次启动时尚未创建）。
    pub fn detect() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| {
            error!("无法定位用户 home 目录（USERPROFILE / FOLDERID_Profile 均不可用）");
            CoreError::NoHomeDir
        })?;
        debug!(home = %home.display(), "定位到用户 home 目录");
        Self::at(home.join(".cyber"))
    }

    /// 在指定位置构造路径集合（便于测试）。
    pub fn at(cyber_home: PathBuf) -> Result<Self> {
        let mcp_dir = cyber_home.join("mcp");
        let reports_dir = cyber_home.join("reports");
        let ctf_dir = cyber_home.join("ctf");
        Ok(Self {
            config_file: cyber_home.join("config.toml"),
            providers_file: cyber_home.join("providers.toml"),
            skills_dir: cyber_home.join("skills"),
            mcp_servers_file: mcp_dir.join("servers.toml"),
            workflows_dir: cyber_home.join("workflows"),
            sessions_dir: cyber_home.join("sessions"),
            logs_dir: cyber_home.join("logs"),
            reports_templates_dir: reports_dir.join("templates"),
            history_db: cyber_home.join("history.db"),
            history_dir: cyber_home.join("history"),
            assets_db: cyber_home.join("assets.db"),
            ctf_writeup_dir: ctf_dir.join("writeup"),
            mcp_dir,
            reports_dir,
            ctf_dir,
            cyber_home,
        })
    }

    /// 项目级 `.cyber/` 目录（CWD 下），可能不存在。
    pub fn project_local_dir(cwd: &Path) -> PathBuf {
        cwd.join(".cyber")
    }

    /// 项目级 `.cyber.md` 文件路径（CWD 下）。
    pub fn project_md_file(cwd: &Path) -> PathBuf {
        cwd.join(".cyber.md")
    }

    /// 项目级 skills 目录（`<cwd>/.cyber/skills/`，可能不存在）。
    ///
    /// 项目级 skill 覆盖全局同名 skill（`SkillRegistry::load_all` 去重时 Project 优先）。
    pub fn project_skills_dir(cwd: &Path) -> PathBuf {
        Self::project_local_dir(cwd).join("skills")
    }
}
