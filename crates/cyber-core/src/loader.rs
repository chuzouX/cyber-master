use std::path::Path;

use toml::Value;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::error::{CoreError, Result};
use crate::fsutil::read_utf8;
use crate::init::ensure_global_init;
use crate::paths::Paths;
use crate::project::ProjectContext;
use crate::providers::ProvidersConfig;

/// 配置层加载的聚合产物。
#[derive(Debug, Clone)]
pub struct AppContext {
    pub config: Config,
    pub providers: ProvidersConfig,
    pub project: Option<ProjectContext>,
    pub paths: Paths,
    pub is_first_run: bool,
}

/// 加载全部配置：全局 `~/.cyber` → 项目级 `.cyber/config.toml` 覆盖 → `.cyber.md`。
///
/// `cwd` 通常是 `std::env::current_dir()`。
pub fn load_app_context(cwd: &Path) -> Result<AppContext> {
    info!(cwd = %cwd.display(), "开始加载配置");
    let paths = Paths::detect()?;
    let is_first_run = ensure_global_init(&paths)?;

    // 1. 全局 config.toml
    debug!(path = %paths.config_file.display(), "读取全局 config.toml");
    let global_raw = read_utf8(&paths.config_file)?;
    let base: Value = toml::from_str(&global_raw)?;

    // 2. 项目级 .cyber/config.toml 覆盖（deep merge）
    let project_cfg_path = Paths::project_local_dir(cwd).join("config.toml");
    let merged = if project_cfg_path.exists() {
        debug!(path = %project_cfg_path.display(), "读取项目级 .cyber/config.toml 覆盖");
        let over_raw = read_utf8(&project_cfg_path)?;
        let over: Value = toml::from_str(&over_raw)?;
        merge_tables(base, over)
    } else {
        base
    };

    // 合并结果序列化往返，保证反序列化为强类型 Config
    let merged_str = toml::to_string(&merged).map_err(|e| {
        warn!(error = %e, "合并后的配置序列化失败（内部错误，请报告 bug）");
        CoreError::TomlSer(e.to_string())
    })?;
    let config: Config = toml::from_str(&merged_str)?;

    // 3. providers.toml
    let providers = load_providers(&paths.providers_file)?;

    // 4. .cyber.md（项目说明 + 护栏）
    let project = load_project_md(cwd)?;

    info!(
        first_run = is_first_run,
        has_project = project.is_some(),
        theme = %config.ui.theme,
        "配置加载完成"
    );
    Ok(AppContext {
        config,
        providers,
        project,
        paths,
        is_first_run,
    })
}

fn load_providers(path: &Path) -> Result<ProvidersConfig> {
    if !path.exists() {
        debug!(path = %path.display(), "providers.toml 不存在，使用默认三家模板");
        return Ok(ProvidersConfig::default_template());
    }
    debug!(path = %path.display(), "读取 providers.toml");
    let raw = read_utf8(path)?;
    let cfg: ProvidersConfig = toml::from_str(&raw)?;
    Ok(cfg)
}

fn load_project_md(cwd: &Path) -> Result<Option<ProjectContext>> {
    let md_path = Paths::project_md_file(cwd);
    if !md_path.exists() {
        debug!(cwd = %cwd.display(), "无 .cyber.md，将进入 Welcome 启动页");
        return Ok(None);
    }
    debug!(path = %md_path.display(), "读取 .cyber.md");
    Ok(Some(ProjectContext::load(&md_path)?))
}

/// 把配置写回 `~/.cyber/config.toml`（原子写 + `.bak` 备份）。
///
/// 写入的是合并后的 `Config`。已知限制：
/// - toml crate 不保注释，原文件的行内注释（`# ...`）会丢失；
/// - 若存在项目级 `.cyber/config.toml` 覆盖，被覆盖的字段重启后仍会被项目级覆盖
///   （保存仅写全局，项目级在加载时仍 deep-merge 上去）。
pub fn save_config(config: &Config, path: &Path) -> Result<()> {
    let toml_str = toml::to_string_pretty(config).map_err(|e| {
        warn!(error = %e, "配置序列化失败（内部错误，请报告 bug）");
        CoreError::TomlSer(e.to_string())
    })?;
    atomic_write(path, toml_str.as_bytes())?;
    info!(path = %path.display(), "配置已保存");
    Ok(())
}

/// 把 providers 写回 `~/.cyber/providers.toml`（原子写 + `.bak` 备份）。
///
/// 与 `save_config` 同限制：toml crate 不保注释，原文件行内注释会丢失。
/// 在 TUI Provider CRUD（Settings 保存 / Chat 立即持久化）时调用。
pub fn save_providers(providers: &ProvidersConfig, path: &Path) -> Result<()> {
    let toml_str = toml::to_string_pretty(providers).map_err(|e| {
        warn!(error = %e, "providers 序列化失败（内部错误，请报告 bug）");
        CoreError::TomlSer(e.to_string())
    })?;
    atomic_write(path, toml_str.as_bytes())?;
    info!(path = %path.display(), "providers 已保存");
    Ok(())
}

/// 原子写：写 `.tmp` → 备份旧文件为 `.bak` → rename `.tmp` 覆盖目标。
///
/// 主 rename 失败时尝试从 `.bak` 恢复，避免写坏原文件。
pub fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    let bak = path.with_extension("toml.bak");
    std::fs::write(&tmp, data)?;
    let had_old = path.exists();
    if had_old {
        // 备份旧文件（同目录 rename 可靠；Windows 上会替换已有 .bak）。
        // 备份失败不阻断主流程，仅记日志。
        if let Err(e) = std::fs::rename(path, &bak) {
            warn!(bak = %bak.display(), error = %e, "备份旧 config.toml 失败（忽略，继续写入）");
        }
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        warn!(path = %path.display(), error = %e, "rename tmp→config.toml 失败，尝试恢复");
        let _ = std::fs::remove_file(&tmp);
        if had_old {
            let _ = std::fs::rename(&bak, path);
        }
        return Err(e.into());
    }
    Ok(())
}

/// 递归 deep merge：`over` 覆盖 `base`，table 递归合并。
fn merge_tables(mut base: Value, over: Value) -> Value {
    if let (Some(b), Some(o)) = (base.as_table_mut(), over.as_table()) {
        for (k, v) in o {
            match b.get_mut(k) {
                Some(existing) if existing.is_table() && v.is_table() => {
                    let taken = existing.clone();
                    *existing = merge_tables(taken, v.clone());
                }
                _ => {
                    b.insert(k.clone(), v.clone());
                }
            }
        }
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderConfig;

    #[test]
    fn merge_tables_overrides_scalars_and_recurses() {
        let base: Value =
            toml::from_str("ui = { theme = \"a\", mouse = true }\n[agent]\nmax_steps = 10").unwrap();
        let over: Value = toml::from_str("ui = { theme = \"b\" }").unwrap();
        let merged = merge_tables(base, over);
        let s = toml::to_string(&merged).unwrap();
        let cfg: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg.ui.theme, "b", "覆盖字段应生效");
        assert!(cfg.ui.mouse, "未覆盖字段应保留");
        assert_eq!(cfg.agent.max_steps, 10, "未涉及段落应保留");
    }

    #[test]
    fn merge_tables_replaces_non_table_values() {
        let base: Value = toml::from_str("[agent]\nmax_steps = 10").unwrap();
        let over: Value = toml::from_str("[agent]\nmax_steps = 99").unwrap();
        let merged = merge_tables(base, over);
        let s = toml::to_string(&merged).unwrap();
        let cfg: Config = toml::from_str(&s).unwrap();
        assert_eq!(cfg.agent.max_steps, 99);
    }

    /// 验证 toml crate 对 UTF-8 BOM 的行为（Windows 记事本/PowerShell 5.1 常写入 BOM）。
    #[test]
    fn toml_handles_utf8_bom() {
        let with_bom = "\u{feff}[agent]\nmax_steps = 10\n";
        let res: std::result::Result<Config, _> = toml::from_str(with_bom);
        if let Err(e) = res {
            panic!("toml 解析带 UTF-8 BOM 的内容失败 → Windows 兼容性问题确认: {e}");
        }
    }

    fn temp_dir_unique(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_{label}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_config_roundtrip_preserves_values() {
        let dir = temp_dir_unique("save_rt");
        let path = dir.join("config.toml");
        let mut cfg = Config::default();
        cfg.ui.theme = "dracula".into();
        cfg.ui.mouse = false;
        cfg.agent.default_provider = "ollama".into();
        cfg.agent.max_steps = 99;
        cfg.workflow.default_timeout_secs = 7200;
        cfg.tools.extra_path = vec!["/usr/local/bin".into(), "D:\\工具\\bin".into()];
        cfg.storage.log_level = "debug".into();

        save_config(&cfg, &path).unwrap();
        assert!(path.exists(), "保存后文件应存在");

        let raw = std::fs::read_to_string(&path).unwrap();
        let loaded: Config = toml::from_str(&raw).unwrap();
        assert_eq!(loaded.ui.theme, "dracula");
        assert!(!loaded.ui.mouse);
        assert_eq!(loaded.agent.default_provider, "ollama");
        assert_eq!(loaded.agent.max_steps, 99);
        assert_eq!(loaded.workflow.default_timeout_secs, 7200);
        assert_eq!(
            loaded.tools.extra_path,
            vec!["/usr/local/bin".to_string(), "D:\\工具\\bin".to_string()],
            "Vec 与 UTF-8 路径应往返保持"
        );
        assert_eq!(loaded.storage.log_level, "debug");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_config_creates_bak_on_overwrite() {
        let dir = temp_dir_unique("save_bak");
        let path = dir.join("config.toml");
        let bak = path.with_extension("toml.bak");

        let mut cfg1 = Config::default();
        cfg1.ui.theme = "nord".into();
        save_config(&cfg1, &path).unwrap();
        assert!(!bak.exists(), "首次保存不应产生 .bak");

        let mut cfg2 = Config::default();
        cfg2.ui.theme = "gruvbox".into();
        save_config(&cfg2, &path).unwrap();
        assert!(bak.exists(), "二次保存应创建 .bak 备份");

        let bak_cfg: Config = toml::from_str(&std::fs::read_to_string(&bak).unwrap()).unwrap();
        assert_eq!(bak_cfg.ui.theme, "nord", ".bak 应保留首次版本");
        let cur_cfg: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(cur_cfg.ui.theme, "gruvbox", "当前文件应为新版本");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_providers_roundtrip_preserves_values() {
        let dir = temp_dir_unique("save_prov_rt");
        let path = dir.join("providers.toml");
        let mut providers = ProvidersConfig::default_template();
        providers.default_provider = "ollama".into();
        providers.upsert(
            "custom",
            ProviderConfig {
                kind: "openai-compatible".into(),
                base_url: "https://gateway.local/v1/".into(),
                api_key: "${CUSTOM_KEY}".into(),
                model: "gpt-4o-mini".into(),
                max_tokens: 8192,
                temperature: 0.5,
                ..Default::default()
            },
        );

        save_providers(&providers, &path).unwrap();
        assert!(path.exists(), "保存后 providers.toml 应存在");

        let raw = std::fs::read_to_string(&path).unwrap();
        let loaded: ProvidersConfig = toml::from_str(&raw).unwrap();
        assert_eq!(loaded.default_provider, "ollama");
        assert!(loaded.providers.contains_key("custom"));
        assert_eq!(loaded.providers["custom"].kind, "openai-compatible");
        assert_eq!(loaded.providers["custom"].base_url, "https://gateway.local/v1");
        assert_eq!(loaded.providers["custom"].api_key, "${CUSTOM_KEY}");
        assert_eq!(loaded.providers["custom"].max_tokens, 8192);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_providers_creates_bak_on_overwrite() {
        let dir = temp_dir_unique("save_prov_bak");
        let path = dir.join("providers.toml");
        let bak = path.with_extension("toml.bak");

        let p1 = ProvidersConfig::default_template();
        save_providers(&p1, &path).unwrap();
        assert!(!bak.exists(), "首次保存不应产生 .bak");

        let mut p2 = ProvidersConfig::default();
        p2.upsert(
            "only",
            ProviderConfig {
                kind: "ollama".into(),
                base_url: "http://x".into(),
                ..Default::default()
            },
        );
        save_providers(&p2, &path).unwrap();
        assert!(bak.exists(), "二次保存应创建 .bak 备份");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
