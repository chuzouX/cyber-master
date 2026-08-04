//! 启动时构建统一工具表（builtins + Skills + MCP）。
//!
//! `build_registries` 在 `App::new` 前调用，组装 `AppRegistries`：
//! 1. 扫描全局 `~/.cyber/skills/` + 项目级 `<cwd>/.cyber/skills/` 加载 Skills
//! 2. `ToolRegistry::with_builtins()` + 注册每个 `SkillTool`
//! 3. 非 mock 时加载 `servers.toml` + `McpRegistry::connect_all` + 注册 `McpTool`
//!
//! 永不返回 Err（保证 TUI 启动）：单个 Skill / MCP server 失败仅收集为 error 字符串，
//! 降级为仅可用部分（builtins + 成功的 skills/mcp）。errors 经 toast 展示给用户。

use std::path::Path;
use std::sync::{Arc, Mutex};

use cyber_agent::{CtfChallengeTool, ToolRegistry};
use cyber_core::{CtfChallenge, Paths};
use cyber_mcp::{McpRegistry, McpServersConfig};
use cyber_skills::{SkillRegistry, SkillTool};
use tracing::warn;

use crate::app::AppRegistries;

/// 构建统一工具表 + Skill / MCP 注册表。
///
/// - `paths`：全局路径缓存（`skills_dir` / `mcp_servers_file`）
/// - `cwd`：当前工作目录（定位项目级 `.cyber/skills/`）
/// - `mock`：true 时跳过 MCP 连接（离线冒烟）
///
/// 返回 `(AppRegistries, errors)`：errors 为非致命失败的描述列表（供 toast）。
/// 永不返回 Err——任何子失败降级处理，保证 TUI 可启动。
pub async fn build_registries(
    paths: &Paths,
    cwd: &Path,
    mock: bool,
) -> (AppRegistries, Vec<String>) {
    let mut errors: Vec<String> = Vec::new();

    // 1. Skills：扫描全局 + 项目级目录（同步 IO）
    let project_skills_dir = Paths::project_skills_dir(cwd);
    let (skills, skill_errors) =
        SkillRegistry::load_all(&paths.skills_dir, Some(&project_skills_dir));
    for (path, e) in skill_errors {
        warn!(path = %path.display(), error = %e, "Skill 加载失败");
        errors.push(format!("Skill 加载失败 {}: {e}", path.display()));
    }

    // 2. 工具表：内置工具 + Skill 工具
    let mut tool_reg = ToolRegistry::with_builtins();
    for skill in skills.iter() {
        tool_reg.register(Box::new(SkillTool::new(skill.clone())));
    }

    // CTF 题目共享状态（工具与 App 共享）
    let ctf_challenges: Arc<Mutex<Vec<CtfChallenge>>> = Arc::new(Mutex::new(
        crate::ctf_store::load_challenges(&paths.ctf_dir),
    ));
    tool_reg.register(Box::new(CtfChallengeTool::new(
        Arc::clone(&ctf_challenges),
        paths.ctf_dir.clone(),
    )));

    // 3. MCP：非 mock 时加载 servers.toml + 并行连接
    let mcp = if !mock {
        let mcp_config = match McpServersConfig::load(&paths.mcp_servers_file) {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!(error = %e, "MCP servers.toml 加载失败，跳过 MCP");
                errors.push(format!("MCP 配置加载失败: {e}"));
                McpServersConfig::default()
            }
        };
        let (mcp_reg, mcp_tools, mcp_errors) = McpRegistry::connect_all(&mcp_config).await;
        for (name, e) in mcp_errors {
            warn!(server = %name, error = %e, "MCP server 连接失败");
            errors.push(format!("MCP server '{name}' 连接失败: {e}"));
        }
        for tool in mcp_tools {
            tool_reg.register(Box::new(tool));
        }
        Some(Arc::new(mcp_reg))
    } else {
        None
    };

    (
        AppRegistries {
            tools: Arc::new(tool_reg),
            skills: Arc::new(skills),
            mcp,
            ctf_challenges: Some(ctf_challenges),
        },
        errors,
    )
}
