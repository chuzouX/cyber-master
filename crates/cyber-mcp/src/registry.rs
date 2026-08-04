//! McpRegistry：管理多个 MCP server 连接，统一 shutdown。
//!
//! `connect_all` 并行 spawn 所有 server（per-server `timeout_secs`），成功者收集
//! `McpTool` 注入统一工具表，失败者入 errors 供调用方 warn + toast。`shutdown_all`
//! 发 Shutdown 信号并 await actor 退出。
//!
//! 连接跨 agent turn 长存：`McpRegistry` 由 `App` 持有，`McpTool` 经 `Arc<McpConnection>`
//! 共享同一连接，工具表用 `Arc<ToolRegistry>` 跨轮复用。

use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;
use tracing::warn;

use crate::config::{McpServerSpec, McpServersConfig};
use crate::connection::McpConnection;
use crate::error::McpError;
use crate::tool::McpTool;
use crate::transport::McpTransport;

/// 一个已注册的 MCP server 连接。
struct RegisteredServer {
    name: String,
    conn: Arc<McpConnection>,
    /// actor task handle；shutdown 时 take + await。`Mutex` 仅短暂持有（不跨 await）。
    handle: Mutex<Option<JoinHandle<()>>>,
}

/// MCP server 连接注册表。
///
/// 持有所有已成功握手的 `McpConnection`，供 `/mcp` 命令展示状态与退出时统一关闭。
/// `connect_all` 返回的 `McpTool` 列表由调用方注入 `ToolRegistry`，与 registry 分离
/// （registry 管生命周期，tool 管调用）。
pub struct McpRegistry {
    servers: Vec<RegisteredServer>,
}

impl McpRegistry {
    /// 并行连接 `config` 中所有 server，返回 `(registry, tools, errors)`。
    ///
    /// - stdio server：`spawn_stdio`（子进程 + actor + 握手，带 spec 超时）
    /// - http server：`spawn_http`（Streamable HTTP，每次 call 一个 POST + `Mcp-Session-Id` 回带）
    /// - sse server：`spawn_sse`（legacy SSE，长连 GET event-stream + POST endpoint）
    ///
    /// 成功 → 其工具包成 `McpTool` 入 `tools`；失败 → 入 `errors`。
    /// 单个 server 失败不阻断其余（降级为仅可用 server）。`errors` 供调用方 toast。
    pub async fn connect_all(
        config: &McpServersConfig,
    ) -> (Self, Vec<McpTool>, Vec<(String, McpError)>) {
        // 并行 spawn 每个 server（握手是 IO 密集型，并行加速启动）
        let futs: Vec<_> = config
            .servers
            .iter()
            .map(|spec| async move {
                let result = connect_one(spec).await;
                (spec.name.clone(), result)
            })
            .collect();
        let results = futures::future::join_all(futs).await;

        let mut servers = Vec::new();
        let mut tools = Vec::new();
        let mut errors = Vec::new();

        for (name, result) in results {
            match result {
                Ok((conn, handle)) => {
                    // 收集该 server 的所有工具（命名 mcp_<server>_<tool>）
                    for mcp_schema in conn.tools() {
                        tools.push(McpTool::new(conn.clone(), mcp_schema.clone()));
                    }
                    servers.push(RegisteredServer {
                        name,
                        conn,
                        handle: Mutex::new(Some(handle)),
                    });
                }
                Err(e) => {
                    warn!(server = %name, error = %e, "MCP server 连接失败，跳过");
                    errors.push((name, e));
                }
            }
        }

        (Self { servers }, tools, errors)
    }

    /// 空注册表（mock 模式或无 server 时用）。
    pub fn empty() -> Self {
        Self { servers: Vec::new() }
    }

    /// 已连接的 server 名称列表（供 `/mcp` 展示，顺序同 `servers.toml`）。
    pub fn server_names(&self) -> Vec<&str> {
        self.servers.iter().map(|s| s.name.as_str()).collect()
    }

    /// 已连接 server 数量。
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// 是否无连接。
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// 关闭所有连接：发 Shutdown 信号 + await actor 退出。
    ///
    /// 先对所有连接发 Shutdown（让 actor 退出 read 循环），再逐个 await handle
    /// （确保子进程资源回收）。幂等：重复调用无副作用（handle 已 take）。
    pub async fn shutdown_all(&self) {
        // 先发 Shutdown 信号（actor 收到后 shutdown writer 并退出）
        for s in &self.servers {
            s.conn.shutdown();
        }
        // 再 await 所有 actor handle（取出后置 None，幂等）
        for s in &self.servers {
            let handle = s.handle.lock().unwrap().take();
            if let Some(handle) = handle {
                let _ = handle.await;
            }
        }
    }
}

impl std::fmt::Debug for McpRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpRegistry")
            .field("servers", &self.servers.len())
            .finish()
    }
}

/// 连接单个 server：按 transport 分派到对应 spawn 函数。
async fn connect_one(
    spec: &McpServerSpec,
) -> Result<(Arc<McpConnection>, JoinHandle<()>), McpError> {
    match spec.transport {
        McpTransport::Stdio => McpConnection::spawn_stdio(spec).await,
        McpTransport::Http => McpConnection::spawn_http(spec).await,
        McpTransport::Sse => McpConnection::spawn_sse(spec).await,
    }
}
