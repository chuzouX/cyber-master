use cyber_agent::{Tool, ToolCtx};
use cyber_tools::{CyberChefTool, HashIdentifierTool, HttpRequestTool};
use serde_json::json;
use std::path::PathBuf;

fn test_ctx() -> ToolCtx {
    ToolCtx::new(PathBuf::from("."), vec![], None, vec![])
}

#[tokio::test]
async fn test_cyberchef_detect_is_engine_independent() {
    // 显式锁定一个不存在的引擎脚本：探测（detect）是纯 Rust 实现，
    // 不得因为 ~/.cyber/tools/scripts/cyberchef.py 缺失而失败（CI / 新用户环境）。
    let tool = CyberChefTool::with_script_path(PathBuf::from("missing-cyberchef.py"));
    let ctx = test_ctx();

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

    assert!(
        !det_res.is_error,
        "detect 不应依赖外部脚本: {}",
        det_res.content
    );
    let det_val: serde_json::Value = serde_json::from_str(&det_res.content).unwrap();
    let detected = det_val["detected_formats"].as_array().unwrap();
    assert!(detected.iter().any(|d| d["format"] == "Base64"));
}

#[tokio::test]
async fn test_cyberchef_recipe_requires_engine_script() {
    let ctx = test_ctx();

    // 引擎缺失时必须给出明确的脚本缺失提示，而不是伪造成功结果。
    let missing = CyberChefTool::with_script_path(PathBuf::from("missing-cyberchef.py"));
    let res = missing
        .run(
            json!({
                "input": "aGVsbG8gd29ybGQ=",
                "recipe": ["from_base64"]
            }),
            &ctx,
        )
        .await
        .unwrap();
    assert!(res.is_error);
    assert!(
        res.content.contains("cyberchef.py"),
        "应提示脚本缺失: {}",
        res.content
    );

    // 开发机已安装 cyberchef.py + python 时，额外验证真实解码链路：
    // 输出可解析为 JSON 即代表引擎真实执行，必须得到正确结果；否则只能是上述缺失提示。
    let res = CyberChefTool::new()
        .run(
            json!({
                "input": "aGVsbG8gd29ybGQ=",
                "recipe": ["from_base64"]
            }),
            &ctx,
        )
        .await
        .unwrap();
    match serde_json::from_str::<serde_json::Value>(&res.content) {
        Ok(dec_val) => assert_eq!(dec_val["result"], "hello world"),
        Err(_) => assert!(
            res.content.contains("cyberchef.py"),
            "引擎缺失以外的失败原因必须暴露: {}",
            res.content
        ),
    }
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
