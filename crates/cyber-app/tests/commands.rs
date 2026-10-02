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
fn mock_run_delegates_parallel_subagents() {
    let home = tempfile::tempdir().unwrap();
    let output = command(home.path())
        .args([
            "run",
            "delegate: compare two independent checks",
            "--mock",
            "--new",
            "--format",
            "json",
            "--allow-tool",
            "delegate_tasks",
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
    let calls = result["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["name"], "delegate_tasks");
    let delegated: serde_json::Value =
        serde_json::from_str(calls[0]["output"].as_str().unwrap()).unwrap();
    let results = delegated["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["name"], "check-one");
    assert_eq!(results[0]["status"], "completed");
    assert_eq!(results[1]["name"], "check-two");
    assert_eq!(results[1]["status"], "completed");
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
#[test]
fn help_and_version_output_are_formatted_and_informative() {
    let home = tempfile::tempdir().unwrap();
    let help_out = command(home.path()).arg("--help").output().unwrap();
    assert!(help_out.status.success());
    let help_str = String::from_utf8_lossy(&help_out.stdout);
    assert!(help_str.contains("运行模式"));
    assert!(help_str.contains("常用示例"));
    assert!(help_str.contains("tui"));
    assert!(help_str.contains("setup"));
    assert!(help_str.contains("run"));

    let run_help_out = command(home.path())
        .args(["run", "--help"])
        .output()
        .unwrap();
    assert!(run_help_out.status.success());
    let run_help_str = String::from_utf8_lossy(&run_help_out.stdout);
    assert!(run_help_str.contains("PROMPT"));
    assert!(run_help_str.contains("示例:"));

    let ver_out = command(home.path()).arg("--version").output().unwrap();
    assert!(ver_out.status.success());
    let ver_str = String::from_utf8_lossy(&ver_out.stdout);
    assert!(ver_str.contains("cyber"));
}
