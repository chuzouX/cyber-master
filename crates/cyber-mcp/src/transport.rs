//! MCP 传输类型枚举 + stdio 子进程构造。
//!
//! 三种传输（`McpTransport::Stdio` / `Sse` / `Http`）均已在 `connection.rs` 实现：
//! - stdio：`McpConnection::spawn_stdio`（本地子进程 stdin/stdout JSON-RPC）
//! - http：`McpConnection::spawn_http`（Streamable HTTP，POST + `Mcp-Session-Id`）
//! - sse：`McpConnection::spawn_sse`（legacy SSE，长连 GET event-stream + POST endpoint）
//!
//! actor（`connection.rs`）泛型于 `AsyncRead + AsyncWrite`，故测试可直接用
//! `tokio::io::duplex` 模拟 server，无需 trait 对象（`tokio::spawn` 擦除泛型）。

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::config::McpServerSpec;
use crate::error::{McpError, Result};

/// MCP 传输类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    /// 本地子进程（stdin/stdout JSON-RPC）。
    Stdio,
    /// 远程 server（旧规范，已弃用）。
    Sse,
    /// 远程 server（新规范）。
    Http,
}

impl std::fmt::Display for McpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdio => write!(f, "stdio"),
            Self::Sse => write!(f, "sse"),
            Self::Http => write!(f, "http"),
        }
    }
}

/// stdio 传输的子进程句柄 + IO 流。
pub struct StdioTransport {
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

impl StdioTransport {
    /// 按 spec 启动子进程，取 stdin/stdout。
    ///
    /// `command` 缺失或 spawn 失败 → `McpError::SpawnFailed`。
    pub fn spawn(spec: &McpServerSpec) -> Result<Self> {
        let command = spec.command.as_deref().ok_or_else(|| McpError::SpawnFailed {
            server: spec.name.clone(),
            detail: "stdio 传输缺少 `command` 字段".into(),
        })?;
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(&spec.args)
            .envs(spec.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // 不消费 stderr（避免阻塞；server 日志丢弃）
            .kill_on_drop(true); // 父进程退出时 kill 子进程（防泄漏）

        let mut child = cmd.spawn().map_err(|e| McpError::SpawnFailed {
            server: spec.name.clone(),
            detail: format!("spawn `{command}` 失败: {e}"),
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::SpawnFailed {
                server: spec.name.clone(),
                detail: "子进程 stdin 不可用".into(),
            })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::SpawnFailed {
                server: spec.name.clone(),
                detail: "子进程 stdout 不可用".into(),
            })?;
        Ok(Self { child, stdin, stdout })
    }
}

/// 一对 reader/writer，用于 actor（泛型擦除前的载体）。
/// 测试时用 `tokio::io::duplex` 构造。
pub struct IoPair<R, W> {
    pub reader: R,
    pub writer: W,
}

/// 标记 trait：约束 actor 的 reader/writer 类型。
/// 实际上直接用泛型 `R: AsyncRead + Unpin + Send + 'static` 即可，
/// 此 trait 仅为文档化约束存在。
pub trait AsyncIo: AsyncRead + AsyncWrite + Unpin + Send {}
