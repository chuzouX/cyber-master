//! 启动配置闸门：校验有效配置，缺失或不可用时进入全屏设置向导。
//!
//! 交互式向导本体位于 `cyber-tui`（设置中心全屏面板），本模块只负责
//! 「是否可用 / 是否被中断 / 何时拉起向导」的判定，不构建 registries、
//! 不启动 MCP。

use std::path::Path;

use color_eyre::eyre::bail;
use color_eyre::Result;

use cyber_core::setup::{configured, effective_config, setup_state, SetupState};
use cyber_core::{Config, Paths, ProvidersConfig};

/// Gate real-provider startup; mock mode performs no configuration IO.
pub fn ensure_configured(cwd: &Path, mock: bool, interactive: bool) -> Result<()> {
    if mock {
        return Ok(());
    }
    let paths = Paths::detect()?;
    ensure_with_paths(&paths, cwd, interactive)
}

/// Validate the provider actually selected by a script, not the global default.
pub fn ensure_run_configured(
    cwd: &Path,
    mock: bool,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    if mock {
        return Ok(());
    }
    let paths = Paths::detect()?;
    ensure_run_with_paths(&paths, cwd, provider, model)
}

fn ensure_run_with_paths(
    paths: &Paths,
    cwd: &Path,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    cyber_core::init::ensure_global_init(paths)?;
    if setup_state(paths)? == SetupState::InProgress {
        bail!("Setup was interrupted during saving. Run `cyber setup` to complete it before running a task.");
    }
    let (mut config, mut providers) = effective_config(paths, cwd)?;
    apply_run_overrides(&mut config, &mut providers, provider, model);
    if !configured(&config, &providers) {
        bail!("Selected provider configuration is missing or unusable. Run `cyber setup`; check --provider, --model and the credential environment variable.");
    }
    Ok(())
}

fn apply_run_overrides(
    config: &mut Config,
    providers: &mut ProvidersConfig,
    provider: Option<&str>,
    model: Option<&str>,
) {
    if let Some(provider) = provider {
        config.agent.default_provider = provider.into();
    }
    if let (Some(model), Some(provider)) = (
        model,
        providers.providers.get_mut(&config.agent.default_provider),
    ) {
        provider.model = model.into();
    }
}

fn ensure_with_paths(paths: &Paths, cwd: &Path, interactive: bool) -> Result<()> {
    cyber_core::init::ensure_global_init(paths)?;
    let mut state = setup_state(paths)?;
    // An interrupted multi-file commit takes priority over apparently usable settings.
    if state != SetupState::InProgress {
        let (config, providers) = effective_config(paths, cwd)?;
        if configured(&config, &providers) {
            return Ok(());
        }
    }
    if interactive && state != SetupState::Completed {
        if state == SetupState::InProgress {
            eprintln!(
                "Previous setup was interrupted during saving; restarting setup to complete it."
            );
        }
        match cyber_tui::run_setup_blocking(cwd, false) {
            Ok(true) => {}
            Ok(false) => {}
            Err(e) => return Err(e),
        }
        state = setup_state(paths)?;
        let (config, providers) = effective_config(paths, cwd)?;
        if state == SetupState::Completed && configured(&config, &providers) {
            return Ok(());
        }
    }
    if state == SetupState::InProgress {
        bail!(
            "Setup was interrupted during saving. Run `cyber setup` to complete it before startup."
        );
    }
    bail!("Provider configuration is missing or unusable. Run `cyber setup`; check the selected provider, endpoint, model and credential environment variable (including project overrides).");
}

#[cfg(test)]
mod tests {
    use super::*;
    use cyber_core::setup::{read_value, save_setup, write_private, STATE_FILE};

    #[test]
    fn mock_bypasses_all_configuration_io() {
        ensure_configured(Path::new("nonexistent"), true, false).unwrap();
    }

    #[test]
    fn validates_existing_provider_without_completion_marker() {
        let mut config = Config::default();
        config.agent.default_provider = "ollama".into();
        let mut providers = ProvidersConfig::default_template();
        assert!(configured(&config, &providers));
        providers.providers.get_mut("ollama").unwrap().model.clear();
        assert!(!configured(&config, &providers));
        config.agent.default_provider = "missing".into();
        assert!(!configured(&config, &providers));
        config.agent.default_provider = "openai".into();
        providers.providers.get_mut("openai").unwrap().api_key =
            "${CYBER_SETUP_TEST_UNSET_48172}".into();
        assert!(!configured(&config, &providers));
    }

    #[test]
    fn script_overrides_validate_selected_provider_instead_of_default() {
        let mut config = Config::default();
        let mut providers = ProvidersConfig::default_template();
        providers
            .providers
            .get_mut("openai")
            .unwrap()
            .api_key
            .clear();
        assert!(!configured(&config, &providers));
        apply_run_overrides(&mut config, &mut providers, Some("ollama"), Some("local"));
        assert!(configured(&config, &providers));
        assert_eq!(providers.providers["ollama"].model, "local");
        apply_run_overrides(&mut config, &mut providers, Some("missing"), None);
        assert!(!configured(&config, &providers));
    }

    #[test]
    fn noninteractive_missing_credentials_points_to_setup_without_marking_complete() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        let mut providers = read_value(&paths.providers_file).unwrap();
        providers["providers"]["openai"]["api_key"] = toml::Value::String(String::new());
        write_private(
            &paths.providers_file,
            toml::to_string(&providers).unwrap().as_bytes(),
        )
        .unwrap();
        let error = ensure_with_paths(&paths, dir.path(), false).unwrap_err();
        assert!(error.to_string().contains("cyber setup"));
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
        write_private(&paths.cyber_home.join(STATE_FILE), b"completed = true\n").unwrap();
        assert!(
            ensure_with_paths(&paths, dir.path(), false).is_err(),
            "Completion state must not bypass unusable configuration"
        );
    }

    #[test]
    fn project_provider_override_is_respected_without_loading_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        std::fs::create_dir(dir.path().join(".cyber")).unwrap();
        std::fs::write(
            dir.path().join(".cyber/config.toml"),
            "[agent]\ndefault_provider = 'ollama'",
        )
        .unwrap();
        std::fs::write(&paths.mcp_servers_file, "not even valid TOML").unwrap();
        ensure_with_paths(&paths, dir.path(), false).unwrap();
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
        std::fs::write(
            dir.path().join(".cyber/config.toml"),
            "[agent]\ndefault_provider = 'missing'",
        )
        .unwrap();
        assert!(ensure_with_paths(&paths, dir.path(), false).is_err());
    }

    #[test]
    fn interrupted_save_blocks_valid_configuration_until_setup_is_completed() {
        for failed_write in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::at(dir.path().join("global")).unwrap();
            cyber_core::init::ensure_global_init(&paths).unwrap();
            let mut config = read_value(&paths.config_file).unwrap();
            config["agent"]["default_provider"] = toml::Value::String("ollama".into());
            let providers = read_value(&paths.providers_file).unwrap();
            // Start with an already usable, completed setup to also exercise reconfiguration.
            save_setup(&paths, &config, &providers, write_private).unwrap();
            let config_before = std::fs::read(&paths.config_file).unwrap();
            let providers_before = std::fs::read(&paths.providers_file).unwrap();
            let mut writes = 0;
            let result = save_setup(&paths, &config, &providers, |path, data| {
                let current = writes;
                writes += 1;
                if current == failed_write {
                    return Err(cyber_core::CoreError::Config(
                        "injected save failure".into(),
                    ));
                }
                write_private(path, data)
            });
            assert!(result.is_err());
            let (effective, typed_providers) = effective_config(&paths, dir.path()).unwrap();
            assert!(configured(&effective, &typed_providers));
            if failed_write == 0 {
                // No intent published means no user config changed and the old completion is valid.
                assert_eq!(std::fs::read(&paths.config_file).unwrap(), config_before);
                assert_eq!(
                    std::fs::read(&paths.providers_file).unwrap(),
                    providers_before
                );
                assert!(setup_state(&paths).unwrap() == SetupState::Completed);
                ensure_with_paths(&paths, dir.path(), false).unwrap();
                continue;
            }
            assert!(setup_state(&paths).unwrap() == SetupState::InProgress);
            let error = ensure_with_paths(&paths, dir.path(), false)
                .unwrap_err()
                .to_string();
            assert!(error.contains("interrupted"));
            assert!(error.contains("cyber setup"));
            // A script override must not bypass the unfinished commit either.
            let error = ensure_run_with_paths(&paths, dir.path(), Some("ollama"), Some("local"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("interrupted"));
            assert!(error.contains("cyber setup"));
            save_setup(&paths, &config, &providers, write_private).unwrap();
            assert!(setup_state(&paths).unwrap() == SetupState::Completed);
            ensure_with_paths(&paths, dir.path(), false).unwrap();
            ensure_run_with_paths(&paths, dir.path(), Some("ollama"), Some("local")).unwrap();
        }
    }

    #[test]
    fn in_progress_takes_priority_over_completed_and_invalid_provider_toml() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        write_private(
            &paths.cyber_home.join(STATE_FILE),
            b"in_progress = true\ncompleted = true\n",
        )
        .unwrap();
        std::fs::write(&paths.providers_file, "not valid TOML").unwrap();
        assert!(setup_state(&paths).unwrap() == SetupState::InProgress);
        assert!(ensure_with_paths(&paths, dir.path(), false)
            .unwrap_err()
            .to_string()
            .contains("interrupted"));
        assert!(
            ensure_run_with_paths(&paths, dir.path(), Some("ollama"), None)
                .unwrap_err()
                .to_string()
                .contains("interrupted")
        );
    }

    #[test]
    fn interactive_usable_configuration_never_opens_the_wizard() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        let mut config = read_value(&paths.config_file).unwrap();
        config["agent"]["default_provider"] = toml::Value::String("ollama".into());
        write_private(
            &paths.config_file,
            toml::to_string(&config).unwrap().as_bytes(),
        )
        .unwrap();
        // An existing usable configuration, with no setup history, must never force a wizard.
        ensure_with_paths(&paths, dir.path(), true).unwrap();
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
    }

    #[test]
    fn interactive_interrupted_setup_without_a_terminal_keeps_in_progress_state() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        let mut config = read_value(&paths.config_file).unwrap();
        config["agent"]["default_provider"] = toml::Value::String("ollama".into());
        write_private(
            &paths.config_file,
            toml::to_string(&config).unwrap().as_bytes(),
        )
        .unwrap();
        write_private(
            &paths.cyber_home.join(STATE_FILE),
            b"in_progress = true\ncompleted = false\n",
        )
        .unwrap();
        let before = std::fs::read(&paths.config_file).unwrap();
        // Captured stdio is not a terminal, so the wizard refuses to start; the
        // interrupted marker must survive untouched for the next attempt.
        assert!(ensure_with_paths(&paths, dir.path(), true).is_err());
        assert!(setup_state(&paths).unwrap() == SetupState::InProgress);
        assert_eq!(std::fs::read(&paths.config_file).unwrap(), before);
    }
}
