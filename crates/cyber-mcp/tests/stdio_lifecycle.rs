use std::io::{BufRead, Write};
use std::time::Duration;

use cyber_mcp::config::McpServerSpec;
use cyber_mcp::connection::McpConnection;
use cyber_mcp::transport::McpTransport;
use serde_json::{json, Value};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};

// Run only this test in the child, with environment scoped to Command::envs.
#[test]
fn stdio_helper() {
    let Ok(address) = std::env::var("CYBER_MCP_STDIO_HELPER") else {
        return;
    };
    let _lifetime = std::net::TcpStream::connect(address).unwrap();
    let mode = std::env::var("CYBER_MCP_STDIO_MODE").unwrap();
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        if mode == "hang" {
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        }
        let result = match request["method"].as_str().unwrap() {
            "initialize" if mode == "error" => {
                println!(
                    "{}",
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": "failed"}})
                );
                std::io::stdout().flush().unwrap();
                continue;
            }
            "initialize" => json!({
                "protocolVersion": "2024-11-05", "capabilities": {},
                "serverInfo": {"name": "child", "version": "1"}
            }),
            "tools/list" => json!({"tools": [{"name": "ping", "inputSchema": {"type": "object"}}]}),
            "tools/call" => {
                json!({"content": [{"type": "text", "text": "pong"}], "isError": false})
            }
            method => panic!("unexpected method: {method}"),
        };
        println!("{}", json!({"jsonrpc": "2.0", "id": id, "result": result}));
        std::io::stdout().flush().unwrap();
    }
    // Ignore stdin EOF deliberately: shutdown must kill and reap, not just close IO.
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

async fn spawn_helper(
    mode: &str,
) -> (
    tokio::task::JoinHandle<
        cyber_mcp::error::Result<(std::sync::Arc<McpConnection>, tokio::task::JoinHandle<()>)>,
    >,
    TcpStream,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let spec = McpServerSpec {
        name: "child".into(),
        transport: McpTransport::Stdio,
        command: Some(std::env::current_exe().unwrap().to_str().unwrap().into()),
        args: vec![
            "--exact".into(),
            "stdio_helper".into(),
            "--nocapture".into(),
            "--quiet".into(),
        ],
        env: [
            (
                "CYBER_MCP_STDIO_HELPER".into(),
                listener.local_addr().unwrap().to_string(),
            ),
            ("CYBER_MCP_STDIO_MODE".into(), mode.into()),
        ]
        .into(),
        url: None,
        headers: Default::default(),
        timeout_secs: 1,
    };
    let task = tokio::spawn(async move { McpConnection::spawn_stdio(&spec).await });
    let (socket, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
        .await
        .unwrap()
        .unwrap();
    (task, socket)
}

async fn assert_child_exited(mut socket: TcpStream) {
    let mut byte = [0];
    match tokio::time::timeout(Duration::from_secs(5), socket.read(&mut byte))
        .await
        .expect("child still alive")
    {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => panic!("expected closed process lifetime socket: {other:?}"),
    }
}

#[tokio::test]
async fn handshake_keeps_child_alive_until_shutdown() {
    let (task, mut socket) = spawn_helper("ok").await;
    let (conn, handle) = task.await.unwrap().unwrap();
    assert_eq!(conn.tools()[0].name, "ping");
    assert_eq!(
        conn.call_tool("ping", json!({})).await.unwrap().content[0].text,
        "pong"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.read(&mut [0]))
            .await
            .is_err()
    );
    conn.shutdown();
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .unwrap()
        .unwrap();
    assert_child_exited(socket).await;
}

#[tokio::test]
async fn failed_and_timed_out_handshakes_kill_child() {
    for mode in ["error", "hang"] {
        let (task, socket) = spawn_helper(mode).await;
        assert!(task.await.unwrap().is_err());
        assert_child_exited(socket).await;
    }
}

#[tokio::test]
async fn cancelled_handshake_kills_child() {
    let (task, socket) = spawn_helper("hang").await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_child_exited(socket).await;
}

#[tokio::test]
async fn dropping_connection_or_aborting_actor_kills_child() {
    for abort_actor in [false, true] {
        let (task, socket) = spawn_helper("ok").await;
        let (conn, handle) = task.await.unwrap().unwrap();
        if abort_actor {
            handle.abort();
        } else {
            drop(conn);
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap();
        assert_child_exited(socket).await;
    }
}
