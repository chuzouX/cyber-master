use cyber_agent::{Tool, ToolCtx};
use cyber_tools::{CyberChefTool, HashIdentifierTool, HttpRequestTool};
use serde_json::json;
use std::path::PathBuf;

fn test_ctx() -> ToolCtx {
    ToolCtx::new(PathBuf::from("."), vec![], None, vec![])
}

#[tokio::test]
async fn test_cyberchef_detect_and_chain() {
    let tool = CyberChefTool::new();
    let ctx = test_ctx();

    // 1. 测试编码探测 (detect: true)
    let det_res = tool
        .run(
            json!({
                "input": "aGVsbG8gd29ybGQ=",
                "detect": true
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!det_res.is_error);
    let det_val: serde_json::Value = serde_json::from_str(&det_res.content).unwrap();
    let detected = det_val["detected_formats"].as_array().unwrap();
    assert!(detected.iter().any(|d| d["format"] == "Base64"));

    // 2. 测试基于推荐 Recipe 解码
    let dec_res = tool
        .run(
            json!({
                "input": "aGVsbG8gd29ybGQ=",
                "recipe": ["from_base64"]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!dec_res.is_error);
    let dec_val: serde_json::Value = serde_json::from_str(&dec_res.content).unwrap();
    assert_eq!(dec_val["result"], "hello world");
}

#[tokio::test]
async fn test_hash_identifier_and_calc() {
    let tool = HashIdentifierTool;
    let ctx = test_ctx();

    let sha_res = tool
        .run(
            json!({
                "action": "calculate",
                "algorithm": "sha256",
                "input": "admin123"
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(!sha_res.is_error);
    let sha_val: serde_json::Value = serde_json::from_str(&sha_res.content).unwrap();
    let sha_hash = sha_val["result"].as_str().unwrap();
    assert_eq!(
        sha_hash,
        "240be518fabd2724ddb6f04eeb1da5967448d7e831c08c8fa822809f74c720a9"
    );
}

#[tokio::test]
async fn test_http_request_curl_generation() {
    let tool = HttpRequestTool;
    let ctx = test_ctx();

    let res = tool
        .run(
            json!({
                "method": "POST",
                "url": "http://127.0.0.1:9",
                "headers": {
                    "X-Custom-Token": "secret123"
                },
                "body": "payload=test"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(res.content.contains("curl -sk -X POST"));
    assert!(res.content.contains("-H 'X-Custom-Token: secret123'"));
}
