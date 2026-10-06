//! 首次配置向导的共享内核：完成标记（`setup.toml`）、有效配置校验与两阶段原子保存。
//!
//! 门禁语义（`configured`）与保存语义（`commit`）是 `cyber setup` / 启动闸门 /
//! 设置中心（TUI）三处的单一来源。

use std::io::Write;
use std::path::Path;

use serde::de::DeserializeOwned;
use toml::Value;

use crate::config::Config;
use crate::error::{CoreError, Result};
use crate::paths::Paths;
use crate::providers::{ProvidersConfig, PROVIDER_KINDS};

/// 完成标记文件名（位于 `~/.cyber/`）。
pub const STATE_FILE: &str = "setup.toml";

/// 首次配置的状态机。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupState {
    NotStarted,
    InProgress,
    Completed,
}

/// 读取 `~/.cyber/setup.toml` 判定当前状态。
pub fn setup_state(paths: &Paths) -> Result<SetupState> {
    let path = paths.cyber_home.join(STATE_FILE);
    if !path.try_exists()? {
        return Ok(SetupState::NotStarted);
    }
    let state = read_value(&path)?;
    if state.get("in_progress").and_then(Value::as_bool) == Some(true) {
        Ok(SetupState::InProgress)
    } else if state.get("completed").and_then(Value::as_bool) == Some(true) {
        Ok(SetupState::Completed)
    } else {
        Ok(SetupState::NotStarted)
    }
}

/// 解析一个 TOML 文件为原始 `Value`。
///
/// 解析错误不回显文件内容：TOML 报错常带源码行，可能包含明文凭据。
pub fn read_value(path: &Path) -> Result<Value> {
    let raw = crate::fsutil::read_utf8(path)?;
    toml::from_str(&raw).map_err(|_| {
        CoreError::Config(format!(
            "Invalid TOML in {}; fix it before `cyber setup`.",
            path.display()
        ))
    })
}

/// 把原始 `Value` 解码为强类型配置（结构不匹配时只报路径，不回显内容）。
pub fn decode<T: DeserializeOwned>(value: &Value, path: &Path) -> Result<T> {
    value.clone().try_into().map_err(|_| {
        CoreError::Config(format!(
            "Invalid configuration structure in {}; fix it before `cyber setup`.",
            path.display()
        ))
    })
}

/// 深合并 TOML 表（`over` 覆盖 `base`），用于项目级 `.cyber/config.toml` 叠加。
pub fn merge(base: &mut Value, over: Value) {
    if let (Some(base), Value::Table(over)) = (base.as_table_mut(), over) {
        for (key, value) in over {
            if let Some(existing) = base
                .get_mut(&key)
                .filter(|v| v.is_table() && value.is_table())
            {
                merge(existing, value);
            } else {
                base.insert(key, value);
            }
        }
    }
}

/// 读取全局配置并叠加项目级覆盖，返回强类型配置（不加载 MCP）。
pub fn effective_config(paths: &Paths, cwd: &Path) -> Result<(Config, ProvidersConfig)> {
    let mut value = read_value(&paths.config_file)?;
    let project = Paths::project_local_dir(cwd).join("config.toml");
    if project.try_exists()? {
        merge(&mut value, read_value(&project)?);
    }
    Ok((
        decode(&value, &paths.config_file)?,
        decode(&read_value(&paths.providers_file)?, &paths.providers_file)?,
    ))
}

/// 判定默认 Provider 是否可用（kind 合法 + 端点安全 + 模型非空 + 有凭据）。
pub fn configured(config: &Config, providers: &ProvidersConfig) -> bool {
    providers
        .providers
        .get(&config.agent.default_provider)
        .is_some_and(|p| {
            PROVIDER_KINDS.contains(&p.kind.as_str())
                && valid_endpoint(&p.base_url)
                && !p.model.trim().is_empty()
                && (p.kind == "ollama" || !p.resolved_api_key().trim().is_empty())
        })
}

/// 端点必须是 http/https、带 host、且不携带内嵌凭据 / query / fragment。
pub fn valid_endpoint(endpoint: &str) -> bool {
    !endpoint.chars().any(char::is_whitespace)
        && url::Url::parse(endpoint).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
}

/// 把强类型配置同步进原始 TOML（保留未知键与未知 provider 条目）。
pub fn sync_values(
    config_val: &mut Value,
    providers_val: &mut Value,
    config: &Config,
    providers: &ProvidersConfig,
) -> Result<()> {
    if let Some(config_table) = config_val.as_table_mut() {
        let agent = config_table
            .entry("agent")
            .or_insert_with(|| Value::Table(Default::default()));
        if let Some(agent_table) = agent.as_table_mut() {
            agent_table.insert(
                "default_provider".into(),
                Value::String(config.agent.default_provider.clone()),
            );
            let vision = agent_table
                .entry("vision")
                .or_insert_with(|| Value::Table(Default::default()));
            if let Some(vision_table) = vision.as_table_mut() {
                vision_table.insert(
                    "enabled".into(),
                    Value::Boolean(config.agent.vision.enabled),
                );
                vision_table.insert(
                    "provider".into(),
                    Value::String(config.agent.vision.provider.clone()),
                );
                vision_table.insert(
                    "model".into(),
                    Value::String(config.agent.vision.model.clone()),
                );
                vision_table.insert(
                    "prompt".into(),
                    Value::String(config.agent.vision.prompt.clone()),
                );
                vision_table.insert(
                    "detail".into(),
                    Value::String(config.agent.vision.detail.clone()),
                );
            }
        }
    }
    if let Some(prov_table) = providers_val.as_table_mut() {
        prov_table.insert(
            "default_provider".into(),
            Value::String(providers.default_provider.clone()),
        );
        let provs_entry = prov_table
            .entry("providers")
            .or_insert_with(|| Value::Table(Default::default()));
        if let Some(provs_table) = provs_entry.as_table_mut() {
            provs_table.retain(|k, _| providers.providers.contains_key(k));
            for (name, p) in &providers.providers {
                let p_val = toml::Value::try_from(p)
                    .map_err(|e| CoreError::Config(format!("Failed to serialize provider: {e}")))?;
                if let Some(existing) = provs_table.get_mut(name).and_then(Value::as_table_mut) {
                    if let Value::Table(new_tbl) = p_val {
                        for (k, v) in new_tbl {
                            existing.insert(k, v);
                        }
                    }
                } else {
                    provs_table.insert(name.clone(), p_val);
                }
            }
        }
    }
    Ok(())
}

/// 私有（最小权限）原子写入：同目录临时文件 → 收紧 ACL → 落盘。
///
/// 不生成 `.bak`：`providers.toml` 含明文凭据，任何备份都是凭据泄漏面。
pub fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| CoreError::Config("Configuration path has no parent.".into()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(windows)]
    {
        // Remove inherited access before writing secrets; OWNER RIGHTS grants only the file owner.
        let output = std::process::Command::new("icacls")
            .arg(temp.path())
            .args(["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
            .output()
            .map_err(|e| {
                CoreError::Config(format!(
                    "Unable to restrict configuration file permissions: {e}"
                ))
            })?;
        if !output.status.success() {
            return Err(CoreError::Config(
                "Unable to restrict configuration file permissions; nothing written.".into(),
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|e| {
        CoreError::Config(format!("Unable to replace {}: {}", path.display(), e.error))
    })?;
    Ok(())
}

/// 两阶段提交：先写 `in_progress` 意图，再写两份配置，最后标记 `completed`。
///
/// 任何中途失败都留下可检测的 `in_progress` 标记，启动闸门据此拒绝启动。
/// `write` 参数化仅为测试注入失败点；生产路径恒为 [`write_private`]。
pub fn save_setup(
    paths: &Paths,
    config: &Value,
    providers: &Value,
    mut write: impl FnMut(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    let config = toml::to_string_pretty(config).map_err(|e| CoreError::TomlSer(e.to_string()))?;
    let providers =
        toml::to_string_pretty(providers).map_err(|e| CoreError::TomlSer(e.to_string()))?;
    let state = paths.cyber_home.join(STATE_FILE);
    // Persist intent before changing either file, including when reconfiguring a completed setup.
    write(
        &state,
        b"version = 1\nin_progress = true\ncompleted = false\n",
    )?;
    write(&paths.providers_file, providers.as_bytes())?;
    write(&paths.config_file, config.as_bytes())?;
    // Only this final write permits startup after a multi-file commit.
    write(
        &state,
        b"version = 1\nin_progress = false\ncompleted = true\n",
    )?;
    Ok(())
}

/// 保存设置中心草稿：读盘原始 TOML → 同步草稿 → 两阶段原子提交。
pub fn commit(paths: &Paths, config: &Config, providers: &ProvidersConfig) -> Result<()> {
    let mut config_val = read_value(&paths.config_file)?;
    let mut providers_val = read_value(&paths.providers_file)?;
    sync_values(&mut config_val, &mut providers_val, config, providers)?;
    save_setup(paths, &config_val, &providers_val, write_private)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initialized_paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        crate::init::ensure_global_init(&paths).unwrap();
        (dir, paths)
    }

    #[test]
    fn sync_values_synchronizes_vision_config() {
        let mut config_val: Value =
            toml::from_str("[agent]\ndefault_provider = 'openai'\n").unwrap();
        let mut providers_val: Value = toml::from_str("[providers]\n").unwrap();
        let mut config = Config::default();
        config.agent.default_provider = "deepseek".into();
        config.agent.vision.enabled = true;
        config.agent.vision.provider = "deepseek".into();
        config.agent.vision.model = "deepseek-flash".into();
        config.agent.vision.detail = "high".into();
        config.agent.vision.prompt = "自定义识图提示词".into();
        let providers = ProvidersConfig::default();

        sync_values(&mut config_val, &mut providers_val, &config, &providers).unwrap();

        assert_eq!(
            config_val["agent"]["default_provider"].as_str(),
            Some("deepseek")
        );
        assert_eq!(
            config_val["agent"]["vision"]["enabled"].as_bool(),
            Some(true)
        );
        assert_eq!(
            config_val["agent"]["vision"]["provider"].as_str(),
            Some("deepseek")
        );
        assert_eq!(
            config_val["agent"]["vision"]["model"].as_str(),
            Some("deepseek-flash")
        );
        assert_eq!(
            config_val["agent"]["vision"]["detail"].as_str(),
            Some("high")
        );
        assert_eq!(
            config_val["agent"]["vision"]["prompt"].as_str(),
            Some("自定义识图提示词")
        );
    }

    #[test]
    fn sync_values_preserves_unknown_entries_and_other_providers() {
        let mut config_val: Value = toml::from_str("extra = 42\n[agent]\nmax_steps = 7").unwrap();
        let mut providers_val: Value = toml::from_str(
            "custom = true\n[providers.local]\nunknown = 99\n[providers.other]\nmodel = 'keep'",
        )
        .unwrap();
        let mut providers = ProvidersConfig::default();
        providers.providers.insert(
            "local".into(),
            crate::providers::ProviderConfig {
                base_url: "http://localhost:11434".into(),
                model: "local-model".into(),
                ..Default::default()
            },
        );

        sync_values(
            &mut config_val,
            &mut providers_val,
            &Config::default(),
            &providers,
        )
        .unwrap();

        assert_eq!(config_val["extra"].as_integer(), Some(42));
        assert_eq!(config_val["agent"]["max_steps"].as_integer(), Some(7));
        assert_eq!(
            providers_val["providers"]["local"]["unknown"].as_integer(),
            Some(99)
        );
        assert_eq!(
            providers_val["providers"]["local"]["model"].as_str(),
            Some("local-model")
        );
        // Providers no longer present in the typed config are pruned.
        assert!(providers_val["providers"].get("other").is_none());
    }

    #[test]
    fn write_private_replaces_existing_file_without_backup_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.toml");
        write_private(&path, b"old").unwrap();
        write_private(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!path.with_extension("toml.bak").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_unsafe_endpoints() {
        assert!(valid_endpoint("https://example.com/v1"));
        assert!(valid_endpoint("http://localhost:11434"));
        assert!(!valid_endpoint("https://user:secret@example.com"));
        assert!(!valid_endpoint("file:///tmp/key"));
        assert!(!valid_endpoint("https://example.com/v1?key=secret"));
        assert!(!valid_endpoint(""));
    }

    #[test]
    fn parse_errors_do_not_echo_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.toml");
        std::fs::write(&path, "api_key = 'private-secret' garbage").unwrap();
        let error = read_value(&path).unwrap_err().to_string();
        assert!(!error.contains("private-secret"));
        assert!(error.contains("cyber setup"));
    }

    #[test]
    fn commit_writes_completed_marker_after_both_files() {
        let (_dir, paths) = initialized_paths();
        let mut config = Config::default();
        config.agent.default_provider = "ollama".into();
        let providers = ProvidersConfig::default_template();

        commit(&paths, &config, &providers).unwrap();

        let state = std::fs::read_to_string(paths.cyber_home.join(STATE_FILE)).unwrap();
        assert!(state.contains("completed = true"));
        assert!(!state.contains("in_progress = true"));
        assert_eq!(setup_state(&paths).unwrap(), SetupState::Completed);
        let written: Config =
            toml::from_str(&std::fs::read_to_string(&paths.config_file).unwrap()).unwrap();
        assert_eq!(written.agent.default_provider, "ollama");
        let providers_val = read_value(&paths.providers_file).unwrap();
        assert!(providers_val["providers"].get("ollama").is_some());
        let (effective, typed) = effective_config(&paths, paths.cyber_home.as_path()).unwrap();
        assert!(configured(&effective, &typed));
    }

    #[test]
    fn commit_marks_in_progress_before_writing_files() {
        let (_dir, paths) = initialized_paths();
        let config_val = read_value(&paths.config_file).unwrap();
        let providers_val = read_value(&paths.providers_file).unwrap();
        let config_before = std::fs::read(&paths.config_file).unwrap();
        let providers_before = std::fs::read(&paths.providers_file).unwrap();

        let mut writes = 0;
        let result = save_setup(&paths, &config_val, &providers_val, |path, data| {
            let current = writes;
            writes += 1;
            if current == 1 {
                return Err(CoreError::Config("injected save failure".into()));
            }
            write_private(path, data)
        });

        assert!(result.is_err());
        assert_eq!(setup_state(&paths).unwrap(), SetupState::InProgress);
        // The intent marker is published first; no user configuration was rewritten.
        assert_eq!(std::fs::read(&paths.config_file).unwrap(), config_before);
        assert_eq!(
            std::fs::read(&paths.providers_file).unwrap(),
            providers_before
        );
    }
}
