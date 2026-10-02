//! Terminal-only configuration setup. Never builds registries or starts MCP.

use std::io::{self, IsTerminal, Write};
use std::path::Path;

use color_eyre::eyre::{bail, eyre, WrapErr};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use cyber_core::{Config, Paths, ProviderConfig, ProvidersConfig, PROVIDER_KINDS};
use toml::Value;

const STATE_FILE: &str = "setup.toml";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupState {
    NotStarted,
    InProgress,
    Completed,
}

fn setup_state(paths: &Paths) -> Result<SetupState> {
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

/// Gate real-provider startup; mock mode performs no configuration IO.
pub fn ensure_configured(cwd: &Path, mock: bool, interactive: bool) -> Result<()> {
    if mock {
        return Ok(());
    }
    let paths = Paths::detect()?;
    ensure_with_paths(&paths, cwd, interactive, run_setup)
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

fn ensure_with_paths(
    paths: &Paths,
    cwd: &Path,
    interactive: bool,
    setup: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
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
        setup(cwd)?;
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

/// Configure an existing provider, asking for confirmation before any user-config writes.
pub fn run_setup(cwd: &Path) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("`cyber setup` requires an interactive terminal (stdin and stderr).");
    }
    let paths = Paths::detect()?;
    cyber_core::init::ensure_global_init(&paths)?;
    let config_before = std::fs::read(&paths.config_file)?;
    let providers_before = std::fs::read(&paths.providers_file)?;
    let mut config = read_value(&paths.config_file)?;
    let mut providers = read_value(&paths.providers_file)?;
    let typed: ProvidersConfig = decode(&providers, &paths.providers_file)?;
    let names = typed.sorted_names();
    if names.is_empty() {
        bail!("No existing providers in providers.toml. Restore a provider entry before running `cyber setup`.");
    }
    eprintln!("Cyber setup (offline). Type :cancel, press Ctrl-C or Escape to cancel.");
    eprintln!("Only global configuration is saved; project overrides remain unchanged.");
    for (index, name) in names.iter().enumerate() {
        eprintln!("  {}. {}", index + 1, name.escape_default());
    }
    let choice = prompt("Provider number", "", false)?;
    let index = choice.parse::<usize>().ok().and_then(|n| n.checked_sub(1));
    let name = index
        .and_then(|n| names.get(n))
        .ok_or_else(|| eyre!("Invalid provider number; run `cyber setup` again."))?;
    let mut provider = typed.providers[name].clone();
    if !PROVIDER_KINDS.contains(&provider.kind.as_str()) {
        bail!("Selected provider kind is unsupported.");
    }
    let endpoint_default = if valid_endpoint(&provider.base_url) {
        &provider.base_url
    } else {
        ""
    };
    provider.base_url = prompt("Endpoint (base URL)", endpoint_default, false)?;
    if !valid_endpoint(&provider.base_url) {
        bail!("Endpoint must be an HTTP(S) URL with a host and no embedded credentials.");
    }
    eprintln!("Credential: e = environment variable reference, s = hidden secret, k = keep existing, n = none (Ollama only).");
    let mode = prompt("Credential mode", "k", false)?;
    match mode.as_str() {
        "e" => {
            let variable = prompt("Environment variable name", "", false)?;
            if !valid_env_name(&variable) {
                bail!("Invalid environment variable name.");
            }
            provider.api_key = format!("${{{variable}}}");
            if std::env::var(&variable).map_or(true, |v| v.trim().is_empty()) {
                eprintln!("The variable is currently unset/empty; set it before using Cyber.");
            }
        }
        "s" => {
            eprintln!("The secret will be stored in your user-local providers.toml, never in project configuration.");
            provider.api_key = prompt("API key (hidden)", "", true)?;
        }
        "k" => {}
        "n" if provider.kind == "ollama" => provider.api_key.clear(),
        _ => {
            bail!("Invalid credential mode.");
        }
    }
    if provider.kind != "ollama" && provider.api_key.trim().is_empty() {
        bail!("This provider requires a credential or environment variable reference.");
    }
    provider.model = prompt("Model ID", &provider.model, false)?;
    provider.normalize();
    if provider.model.is_empty() {
        bail!("Model ID must not be empty.");
    }
    eprintln!(
        "Save provider {} and model {}? Credentials are not displayed.",
        name.escape_default(),
        provider.model.escape_default()
    );
    if prompt("Confirm [y/N]", "n", false)? != "y" {
        bail!("Setup cancelled; no user configuration or completion state saved.");
    }
    update_values(&mut config, &mut providers, name, &provider)?;
    // Do not overwrite settings changed while the user was answering prompts.
    if std::fs::read(&paths.config_file)? != config_before
        || std::fs::read(&paths.providers_file)? != providers_before
    {
        bail!("Configuration changed during setup; nothing saved. Run `cyber setup` again.");
    }
    save_setup(&paths, &config, &providers, write_private)?;
    eprintln!("Setup saved. No network checks were performed; a future `cyber doctor` can check connectivity.");
    let (effective, providers) = effective_config(&paths, cwd)?;
    if !configured(&effective, &providers) {
        eprintln!("Configuration is saved, but credentials or project overrides still need attention before startup.");
    }
    Ok(())
}

fn save_setup(
    paths: &Paths,
    config: &Value,
    providers: &Value,
    mut write: impl FnMut(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    let config = toml::to_string_pretty(config)?;
    let providers = toml::to_string_pretty(providers)?;
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

fn read_value(path: &Path) -> Result<Value> {
    let raw = cyber_core::fsutil::read_utf8(path)?;
    // TOML errors can contain source lines, including plaintext credentials.
    toml::from_str(&raw).map_err(|_| {
        eyre!(
            "Invalid TOML in {}; fix it before `cyber setup`.",
            path.display()
        )
    })
}

fn decode<T: serde::de::DeserializeOwned>(value: &Value, path: &Path) -> Result<T> {
    value.clone().try_into().map_err(|_| {
        eyre!(
            "Invalid configuration structure in {}; fix it before `cyber setup`.",
            path.display()
        )
    })
}

fn effective_config(paths: &Paths, cwd: &Path) -> Result<(Config, ProvidersConfig)> {
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

fn merge(base: &mut Value, over: Value) {
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

fn configured(config: &Config, providers: &ProvidersConfig) -> bool {
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

fn valid_endpoint(endpoint: &str) -> bool {
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

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn update_values(
    config: &mut Value,
    providers: &mut Value,
    name: &str,
    provider: &ProviderConfig,
) -> Result<()> {
    let config = config
        .as_table_mut()
        .ok_or_else(|| eyre!("Config must be a TOML table."))?;
    let agent = config
        .entry("agent")
        .or_insert_with(|| Value::Table(Default::default()));
    agent
        .as_table_mut()
        .ok_or_else(|| eyre!("agent must be a table."))?
        .insert("default_provider".into(), Value::String(name.into()));
    let table = providers
        .as_table_mut()
        .ok_or_else(|| eyre!("Providers must be a TOML table."))?;
    table.insert("default_provider".into(), Value::String(name.into()));
    let selected = table
        .get_mut("providers")
        .and_then(Value::as_table_mut)
        .and_then(|t| t.get_mut(name))
        .and_then(Value::as_table_mut)
        .ok_or_else(|| eyre!("Selected provider must be a table."))?;
    for (key, value) in [
        ("base_url", &provider.base_url),
        ("api_key", &provider.api_key),
        ("model", &provider.model),
    ] {
        selected.insert(key.into(), Value::String(value.clone()));
    }
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("Configuration path has no parent."))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(windows)]
    {
        // Remove inherited access before writing secrets; OWNER RIGHTS grants only the file owner.
        let output = std::process::Command::new("icacls")
            .arg(temp.path())
            .args(["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
            .output()
            .wrap_err("Unable to restrict configuration file permissions")?;
        if !output.status.success() {
            bail!("Unable to restrict configuration file permissions; nothing written.");
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
    temp.persist(path)
        .map_err(|e| eyre!("Unable to replace {}: {}", path.display(), e.error))?;
    Ok(())
}

fn prompt(label: &str, default: &str, hidden: bool) -> Result<String> {
    if hidden || default.is_empty() {
        eprint!("{label}: ");
    } else {
        eprint!("{label} [{}]: ", default.escape_default());
    }
    io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode().wrap_err("Unable to read terminal input")?;
    struct RawMode;
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    let guard = RawMode;
    let mut input = String::new();
    let result = loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc => break Err(eyre!("Setup cancelled.")),
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                break Err(eyre!("Setup cancelled."))
            }
            KeyCode::Enter => break Ok(()),
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c)
                if !c.is_control() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                input.push(c);
            }
            _ => {}
        }
        if !hidden {
            eprint!("\r\x1b[2K{label}: {}", input.escape_default());
            io::stderr().flush()?;
        }
    };
    drop(guard);
    eprintln!();
    result?;
    if input == ":cancel" {
        bail!("Setup cancelled.");
    }
    Ok(if input.trim().is_empty() {
        default.into()
    } else {
        input.trim().into()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn value_updates_preserve_unknown_entries_and_other_providers() {
        let mut config: Value = toml::from_str("extra = 42\n[agent]\nmax_steps = 7").unwrap();
        let mut providers: Value = toml::from_str(
            "custom = true\n[providers.local]\nunknown = 99\n[providers.other]\nmodel = 'keep'",
        )
        .unwrap();
        let provider = ProviderConfig {
            base_url: "http://localhost:11434".into(),
            model: "local-model".into(),
            ..Default::default()
        };
        update_values(&mut config, &mut providers, "local", &provider).unwrap();
        assert_eq!(config["extra"].as_integer(), Some(42));
        assert_eq!(config["agent"]["max_steps"].as_integer(), Some(7));
        assert_eq!(
            providers["providers"]["local"]["unknown"].as_integer(),
            Some(99)
        );
        assert_eq!(
            providers["providers"]["other"]["model"].as_str(),
            Some("keep")
        );
    }

    #[test]
    fn private_write_replaces_existing_file_without_backup_secrets() {
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
    fn rejects_unsafe_endpoints_and_invalid_env_names() {
        assert!(valid_endpoint("https://example.com/v1"));
        assert!(!valid_endpoint("https://user:secret@example.com"));
        assert!(!valid_endpoint("file:///tmp/key"));
        assert!(valid_env_name("OPENAI_API_KEY"));
        assert!(!valid_env_name("1KEY"));
        assert!(!valid_env_name("KEY}"));
    }

    #[test]
    fn noninteractive_missing_credentials_points_to_setup_without_marking_complete() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        let mut providers = read_value(&paths.providers_file).unwrap();
        providers["providers"]["openai"]["api_key"] = Value::String(String::new());
        write_private(
            &paths.providers_file,
            toml::to_string(&providers).unwrap().as_bytes(),
        )
        .unwrap();
        let error = ensure_with_paths(&paths, dir.path(), false, run_setup).unwrap_err();
        assert!(error.to_string().contains("cyber setup"));
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
        write_private(&paths.cyber_home.join(STATE_FILE), b"completed = true\n").unwrap();
        assert!(
            ensure_with_paths(&paths, dir.path(), false, run_setup).is_err(),
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
        ensure_with_paths(&paths, dir.path(), false, run_setup).unwrap();
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
        std::fs::write(
            dir.path().join(".cyber/config.toml"),
            "[agent]\ndefault_provider = 'missing'",
        )
        .unwrap();
        assert!(ensure_with_paths(&paths, dir.path(), false, run_setup).is_err());
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
    fn interrupted_save_blocks_valid_configuration_until_setup_is_completed() {
        for failed_write in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::at(dir.path().join("global")).unwrap();
            cyber_core::init::ensure_global_init(&paths).unwrap();
            let mut config = read_value(&paths.config_file).unwrap();
            config["agent"]["default_provider"] = Value::String("ollama".into());
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
                    bail!("injected save failure");
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
                ensure_with_paths(&paths, dir.path(), false, run_setup).unwrap();
                continue;
            }
            assert!(setup_state(&paths).unwrap() == SetupState::InProgress);
            let error = ensure_with_paths(&paths, dir.path(), false, run_setup)
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
            ensure_with_paths(&paths, dir.path(), false, run_setup).unwrap();
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
        assert!(ensure_with_paths(&paths, dir.path(), false, run_setup)
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
    fn interactive_recovery_restarts_setup_and_cancellation_keeps_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        let mut config = read_value(&paths.config_file).unwrap();
        config["agent"]["default_provider"] = Value::String("ollama".into());
        let providers = read_value(&paths.providers_file).unwrap();
        write_private(
            &paths.config_file,
            toml::to_string(&config).unwrap().as_bytes(),
        )
        .unwrap();
        // An existing usable configuration, with no setup history, must never force a wizard.
        ensure_with_paths(&paths, dir.path(), true, |_| panic!("Unexpected setup")).unwrap();
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
        write_private(
            &paths.cyber_home.join(STATE_FILE),
            b"in_progress = true\ncompleted = false\n",
        )
        .unwrap();
        let before = std::fs::read(&paths.config_file).unwrap();
        let error = ensure_with_paths(&paths, dir.path(), true, |_| {
            bail!("Setup cancelled.");
        })
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(setup_state(&paths).unwrap() == SetupState::InProgress);
        assert_eq!(std::fs::read(&paths.config_file).unwrap(), before);
        let mut restarted = false;
        ensure_with_paths(&paths, dir.path(), true, |cwd| {
            assert_eq!(cwd, dir.path());
            restarted = true;
            save_setup(&paths, &config, &providers, write_private)
        })
        .unwrap();
        assert!(restarted);
        assert!(setup_state(&paths).unwrap() == SetupState::Completed);
    }
}
