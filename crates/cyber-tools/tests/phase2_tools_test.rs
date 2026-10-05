use cyber_agent::{Tool, ToolCtx};
use cyber_tools::{DnsReconTool, PortScannerTool};
use serde_json::json;
use std::path::PathBuf;

fn test_ctx() -> ToolCtx {
    ToolCtx::new(PathBuf::from("."), vec![], None, vec![])
}

#[tokio::test]
async fn test_port_scanner_schema_and_execution() {
    let tool = PortScannerTool;
    let ctx = test_ctx();

    // 扫描本地 127.0.0.1 的保留不可达端口 9，验证异步超时与结构化返回
    let res = tool
        .run(
            json!({
                "target": "127.0.0.1",
                "ports": "9",
                "grab_banner": false,
                "timeout_ms": 100
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!res.is_error);
    let val: serde_json::Value = serde_json::from_str(&res.content).unwrap();
    assert_eq!(val["target"], "127.0.0.1");
    assert_eq!(val["ports_scanned"], 1);
}

#[tokio::test]
async fn test_dns_recon_schema_and_resolve() {
    let tool = DnsReconTool;
    let ctx = test_ctx();

    let res = tool
        .run(
            json!({
                "domain": "localhost",
                "action": "resolve"
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!res.is_error);
    let val: serde_json::Value = serde_json::from_str(&res.content).unwrap();
    assert_eq!(val["domain"], "localhost");
    assert_eq!(val["action"], "resolve");
}
