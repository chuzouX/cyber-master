use std::process::{Command, Output, Stdio};

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_cyber"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CYBER_HOME", home.join(".cyber"))
        .env_remove("CYBER_MOCK_PROVIDER")
        .stdin(Stdio::null());
    command
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "Invalid JSON: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

#[test]
fn default_and_tui_reject_nonterminal_without_creating_config() {
    let home = tempfile::tempdir().unwrap();
    for args in [vec!["--mock"], vec!["tui", "--mock"]] {
        let output = command(home.path()).args(args).output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("cyber run"));
    }
    assert!(!home.path().join(".cyber").exists());
}

#[test]
fn setup_rejects_nonterminal_without_creating_config() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path()).arg("setup").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert!(!home.path().join(".cyber").exists());
}

#[test]
fn mock_run_works_without_credentials_and_returns_json() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path())
        .args([
            "run",
            "hello",
            "--mock",
            "--new",
            "--format",
            "json",
            "--allow-tool",
            "list_dir",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result = json(&output);
    assert_eq!(result["success"], true);
    assert!(!result["answer"].as_str().unwrap().is_empty());
    assert!(!result["session_id"].as_str().unwrap().is_empty());
    assert!(home.path().join(".cyber/providers.toml").exists());
}

#[test]
fn json_failure_has_nonzero_exit_status() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path())
        .args([
            "run",
            "hello",
            "--mock",
            "--provider",
            "missing",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result = json(&output);
    assert_eq!(result["success"], false);
    assert!(result["error"].as_str().unwrap().contains("provider"));
}

#[test]
fn noninteractive_tool_denial_is_failure_and_explicit_authorization_succeeds() {
    let home = tempfile::tempdir().unwrap();
    let denied = command(home.path())
        .args(["run", "hello", "--mock", "--new", "--format", "json"])
        .output()
        .unwrap();
    assert!(!denied.status.success());
    assert_eq!(json(&denied)["success"], false);

    let allowed = command(home.path())
        .args([
            "run",
            "hello",
            "--mock",
            "--new",
            "--format",
            "json",
            "--allow-tool",
            "list_dir",
        ])
        .output()
        .unwrap();
    assert!(
        allowed.status.success(),
        "{}",
        String::from_utf8_lossy(&allowed.stderr)
    );
    assert_eq!(json(&allowed)["success"], true);
}

#[test]
fn missing_configuration_in_script_returns_json_without_prompting() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path())
        .env_remove("OPENAI_API_KEY")
        .args(["run", "hello", "--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let result = json(&output);
    assert_eq!(result["success"], false);
    assert!(result["error"].as_str().unwrap().contains("cyber setup"));
    assert!(!home.path().join(".cyber/setup.toml").exists());
}
