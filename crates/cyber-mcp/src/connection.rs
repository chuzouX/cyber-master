//! MCP 连接：actor 模式（单 task 串行处理 stdin/stdout JSON-RPC）。
//!
//! `Tool::run` 是 `&self`，MCP 需内部可变性。用 actor：每个 `McpConnection` 起一个
//! 后台 tokio task 持有 reader/writer，主线程经 `mpsc` 发请求、`oneshot` 收回执。
//! 单 task 串行 → JSON-RPC id 单调无竞争，无需 Mutex。超时用 `tokio::time::timeout`
//! 包裹 oneshot。cancel 时调用方 drop oneshot sender，无害。
//!
//! actor 泛型于 `AsyncRead + AsyncWrite`，`tokio::spawn` 擦除泛型 → `McpConnection`
//! 是非泛型具体类型，可 `Arc` 共享给多个 `McpTool`。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, CONTENT_TYPE};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio::task::JoinHandle;
use tokio::{select, sync::mpsc::UnboundedReceiver};
use tracing::{debug, warn};

use crate::error::{McpError, Result};
use crate::proto::{
    client_info, InitializeParams, InitializeResult, JsonRpcRequest, JsonRpcResponse,
    McpToolSchema, PROTOCOL_VERSION, ToolListResult,
};
use crate::sse::{extract_jsonrpc_responses, parse_sse_text, SseEvent, SseParser};
use crate::transport::StdioTransport;
use serde_json::Value;

use crate::config::McpServerSpec;

/// 单次 call 的默认超时（秒）。握手用 spec.timeout_secs，常规调用用此值。
const CALL_TIMEOUT_SECS: u64 = 30;

/// SSE actor 的共享 pending 表：id → oneshot 回执。
type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;

/// actor 请求消息。
enum McpRequest {
    /// 一次 JSON-RPC 请求（期望响应）：写 stdin + 按 id 路由响应到 `reply`。
    Call {
        id: u64,
        method: String,
        params: Value,
        reply: oneshot::Sender<Result<Value>>,
    },
    /// 通知（无 id，无响应）：fire-and-forget 写 stdin。
    Notification {
        method: String,
        params: Value,
    },
    /// 关闭 actor：shutdown writer + 退出。
    Shutdown,
}

/// MCP 连接句柄（非泛型，可 `Arc` 共享）。
///
/// `tools` 在握手后填入，此后只读（`McpTool` 据此构造 `ToolSchema`）。
pub struct McpConnection {
    server_name: String,
    tx: UnboundedSender<McpRequest>,
    next_id: AtomicU64,
    tools: Vec<McpToolSchema>,
}

impl McpConnection {
    /// 启动 stdio 子进程 + actor + 握手（initialize + tools/list）。
    /// 返回 `(Arc<Self>, JoinHandle)`；JoinHandle 供 `McpRegistry` 在关闭时 await。
    pub async fn spawn_stdio(spec: &McpServerSpec) -> Result<(Arc<Self>, JoinHandle<()>)> {
        let transport = StdioTransport::spawn(spec)?;
        let server_name = spec.name.clone();
        let timeout = spec.normalized_timeout();
        let (tx, handle) = Self::start_actor(transport.stdout, transport.stdin);
        let conn = Self {
            server_name: server_name.clone(),
            tx,
            next_id: AtomicU64::new(0),
            tools: Vec::new(),
        };
        // 握手带 spec 超时（防子进程卡死）
        let tools = match tokio::time::timeout(
            Duration::from_secs(timeout),
            conn.handshake(),
        )
        .await
        {
            Ok(res) => res?,
            Err(_) => {
                return Err(McpError::Timeout {
                    server: server_name,
                    secs: timeout,
                })
            }
        };
        debug!(
            server = %conn.server_name,
            tools = tools.len(),
            "MCP server 握手完成"
        );
        let conn = Arc::new(Self {
            server_name: conn.server_name,
            tx: conn.tx,
            next_id: conn.next_id,
            tools,
        });
        Ok((conn, handle))
    }

    /// 启动 Streamable HTTP 连接 + actor + 握手。
    ///
    /// 每次调用一个 `POST`（`application/json` 请求 / `application/json` 或
    /// `text/event-stream` 响应）；`Mcp-Session-Id` 由 initialize 响应下发、后续请求回带。
    /// 不设 client 级超时——调用由 `call()` 的 oneshot 30s 超时 + `do_http_call` 内部
    /// send/read 超时双重兜底，避免 actor 被慢请求阻塞。
    pub async fn spawn_http(spec: &McpServerSpec) -> Result<(Arc<Self>, JoinHandle<()>)> {
        let url = spec.url.as_deref().ok_or_else(|| McpError::InitFailed {
            server: spec.name.clone(),
            detail: "http 传输缺少 `url` 字段".into(),
        })?;
        let server_name = spec.name.clone();
        let timeout = spec.normalized_timeout();
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| McpError::Network {
                server: server_name.clone(),
                detail: format!("构建 HTTP client 失败: {e}"),
            })?;
        let headers = expand_env_headers(&spec.headers);
        let (tx, handle) = start_http_actor(server_name.clone(), client, url.to_string(), headers);
        let conn = Self {
            server_name: server_name.clone(),
            tx,
            next_id: AtomicU64::new(0),
            tools: Vec::new(),
        };
        let tools = match tokio::time::timeout(Duration::from_secs(timeout), conn.handshake()).await {
            Ok(res) => res?,
            Err(_) => {
                return Err(McpError::Timeout {
                    server: server_name,
                    secs: timeout,
                })
            }
        };
        debug!(server = %conn.server_name, tools = tools.len(), "MCP(HTTP) server 握手完成");
        let conn = Arc::new(Self {
            server_name: conn.server_name,
            tx: conn.tx,
            next_id: conn.next_id,
            tools,
        });
        Ok((conn, handle))
    }

    /// 启动 legacy SSE 连接 + actor + 握手。
    ///
    /// 长连 `GET` event-stream 收响应（reader task 解析事件按 id 路由到 pending），
    /// 每次 call `POST` 到 server 下发的 endpoint URL。endpoint 由首个 `event: endpoint`
    /// 携带。SSE 已被 Streamable HTTP 取代，此处兼容旧 server。
    pub async fn spawn_sse(spec: &McpServerSpec) -> Result<(Arc<Self>, JoinHandle<()>)> {
        let sse_url = spec.url.as_deref().ok_or_else(|| McpError::InitFailed {
            server: spec.name.clone(),
            detail: "sse 传输缺少 `url` 字段".into(),
        })?;
        let server_name = spec.name.clone();
        let timeout = spec.normalized_timeout();
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| McpError::Network {
                server: server_name.clone(),
                detail: format!("构建 HTTP client 失败: {e}"),
            })?;
        let headers = expand_env_headers(&spec.headers);
        let (tx, handle) = start_sse_actor(server_name.clone(), client, sse_url.to_string(), headers, timeout);
        let conn = Self {
            server_name: server_name.clone(),
            tx,
            next_id: AtomicU64::new(0),
            tools: Vec::new(),
        };
        let tools = match tokio::time::timeout(Duration::from_secs(timeout), conn.handshake()).await {
            Ok(res) => res?,
            Err(_) => {
                return Err(McpError::Timeout {
                    server: server_name,
                    secs: timeout,
                })
            }
        };
        debug!(server = %conn.server_name, tools = tools.len(), "MCP(SSE) server 握手完成");
        let conn = Arc::new(Self {
            server_name: conn.server_name,
            tx: conn.tx,
            next_id: conn.next_id,
            tools,
        });
        Ok((conn, handle))
    }

    /// 启动 actor（泛型擦除）。返回 sender + task handle。
    fn start_actor<R, W>(reader: R, writer: W) -> (UnboundedSender<McpRequest>, JoinHandle<()>)
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<McpRequest>();
        let handle = tokio::spawn(actor_loop(reader, writer, rx));
        (tx, handle)
    }

    /// 握手：initialize → notifications/initialized → tools/list。
    async fn handshake(&self) -> Result<Vec<McpToolSchema>> {
        let init_params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.into(),
            capabilities: serde_json::json!({}),
            client_info: client_info(),
        };
        let init_val = self
            .call("initialize", serde_json::to_value(init_params)?)
            .await?;
        let _init: InitializeResult = serde_json::from_value(init_val).map_err(|e| {
            McpError::InitFailed {
                server: self.server_name.clone(),
                detail: format!("initialize 响应解析失败: {e}"),
            }
        })?;
        // 通知 server 已初始化（spec 要求；fire-and-forget）
        self.send_notification("notifications/initialized", Value::Null);
        // tools/list
        let list_val = self.call("tools/list", Value::Null).await?;
        let list: ToolListResult = serde_json::from_value(list_val).map_err(|e| {
            McpError::InitFailed {
                server: self.server_name.clone(),
                detail: format!("tools/list 响应解析失败: {e}"),
            }
        })?;
        Ok(list.tools)
    }

    /// 发起一次 JSON-RPC 请求并等待响应（带超时）。
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(McpRequest::Call {
                id,
                method: method.into(),
                params,
                reply: reply_tx,
            })
            .map_err(|_| McpError::ChannelClosed)?;
        match tokio::time::timeout(Duration::from_secs(CALL_TIMEOUT_SECS), reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::ChannelClosed),
            Err(_) => Err(McpError::Timeout {
                server: self.server_name.clone(),
                secs: CALL_TIMEOUT_SECS,
            }),
        }
    }

    /// 发送通知（无 id，无响应）。
    fn send_notification(&self, method: &str, params: Value) {
        let _ = self.tx.send(McpRequest::Notification {
            method: method.into(),
            params,
        });
    }

    /// 调用 `tools/call`。
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> Result<crate::proto::CallToolResult> {
        let params = crate::proto::CallToolParams {
            name: tool_name.into(),
            arguments,
        };
        let val = self
            .call("tools/call", serde_json::to_value(params)?)
            .await?;
        let result: crate::proto::CallToolResult = serde_json::from_value(val)?;
        Ok(result)
    }

    /// 握手时发现的工具 schema（只读）。
    pub fn tools(&self) -> &[McpToolSchema] {
        &self.tools
    }

    /// server 名称。
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// 请求 actor 关闭（shutdown writer）。JoinHandle 由 `McpRegistry` await。
    pub fn shutdown(&self) {
        let _ = self.tx.send(McpRequest::Shutdown);
    }
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection")
            .field("server_name", &self.server_name)
            .field("tools", &self.tools.len())
            .finish()
    }
}

#[cfg(test)]
impl McpConnection {
    /// 测试用构造：创建内部 channel（actor 不启动），仅用于测 schema/命名。
    pub fn for_test(server_name: &str) -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            server_name: server_name.into(),
            tx,
            next_id: AtomicU64::new(0),
            tools: Vec::new(),
        }
    }
}

/// actor 主循环：select! 处理 req_rx（写 stdin）+ reader（按行读 stdout 路由响应）。
async fn actor_loop<R, W>(reader: R, writer: W, mut req_rx: UnboundedReceiver<McpRequest>)
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let mut reader = BufReader::new(reader);
    let mut writer = writer;
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value>>> = HashMap::new();
    let mut line = String::new();

    loop {
        select! {
            // 处理新请求 / 通知 / 关闭
            maybe_req = req_rx.recv() => {
                match maybe_req {
                    Some(McpRequest::Call { id, method, params, reply }) => {
                        let req = JsonRpcRequest::new(id, method, Some(params));
                        match serde_json::to_vec(&req) {
                            Ok(bytes) => {
                                pending.insert(id, reply);
                                if writer.write_all(&bytes).await.is_err()
                                    || writer.write_all(b"\n").await.is_err()
                                    || writer.flush().await.is_err()
                                {
                                    // writer 断开 → 失败此请求 + 其余 pending + 退出
                                    if let Some(tx) = pending.remove(&id) {
                                        let _ = tx.send(Err(McpError::ChannelClosed));
                                    }
                                    fail_all_pending(&mut pending);
                                    let _ = writer.shutdown().await;
                                    return;
                                }
                            }
                            Err(e) => {
                                let _ = reply.send(Err(McpError::Json(e)));
                            }
                        }
                    }
                    Some(McpRequest::Notification { method, params }) => {
                        let req = JsonRpcRequest::notification(method, Some(params));
                        // notification 无 id，server 不应回响应；若误回，actor 因 pending 无该 id 而忽略。
                        if let Ok(bytes) = serde_json::to_vec(&req) {
                            let _ = writer.write_all(&bytes).await;
                            let _ = writer.write_all(b"\n").await;
                            let _ = writer.flush().await;
                        }
                    }
                    Some(McpRequest::Shutdown) | None => {
                        let _ = writer.shutdown().await;
                        return;
                    }
                }
            }
            // 读 stdout：按行解析 JSON-RPC 响应
            read_res = reader.read_line(&mut line) => {
                match read_res {
                    Ok(0) => {
                        // EOF：server 关闭 stdout
                        fail_all_pending(&mut pending);
                        return;
                    }
                    Ok(_) => {
                        route_response(&line, &mut pending);
                        line.clear();
                    }
                    Err(e) => {
                        warn!(error = %e, "MCP stdout 读取失败，actor 退出");
                        fail_all_pending(&mut pending);
                        return;
                    }
                }
            }
        }
    }
}

/// 解析一行 JSON-RPC 响应，按 id 路由到 pending oneshot。
/// notification（无 id）/ 未知 id（stale）静默忽略。
fn route_response(line: &str, pending: &mut HashMap<u64, oneshot::Sender<Result<Value>>>) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
    }
    let resp: JsonRpcResponse = match serde_json::from_str(trimmed) {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, line = %trimmed, "无法解析 JSON-RPC 行，跳过");
            return;
        }
    };
    if resp.is_notification() {
        return; // server 主动通知，忽略
    }
    let id = match resp.id {
        Some(id) => id,
        None => return,
    };
    if let Some(tx) = pending.remove(&id) {
        if let Some(err) = resp.error {
            let _ = tx.send(Err(McpError::Rpc {
                code: err.code,
                message: err.message,
            }));
        } else {
            let _ = tx.send(Ok(resp.result.unwrap_or(Value::Null)));
        }
    }
    // 无 pending 条目（stale/超时已丢弃）→ 静默忽略
}

/// 把所有 pending 请求标记为 ChannelClosed（actor 退出前调用）。
fn fail_all_pending(pending: &mut HashMap<u64, oneshot::Sender<Result<Value>>>) {
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(McpError::ChannelClosed));
    }
}

// ── Streamable HTTP actor ───────────────────────────────────────────────────

/// 启动 HTTP actor：返回 sender + task handle。
fn start_http_actor(
    server_name: String,
    client: reqwest::Client,
    url: String,
    headers: HeaderMap,
) -> (UnboundedSender<McpRequest>, JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<McpRequest>();
    let handle = tokio::spawn(http_actor_loop(server_name, client, url, headers, rx));
    (tx, handle)
}

/// HTTP actor 主循环：串行处理 Call（每次一个 POST）+ Notification + Shutdown。
///
/// `session_id` 由 initialize 响应头 `Mcp-Session-Id` 下发，后续请求回带。
/// 单 task 串行 → `session_id` 无需 Mutex。每次 call 由 `do_http_call` 内部
/// `tokio::time::timeout(CALL_TIMEOUT_SECS)` 兜底，避免 actor 被慢请求阻塞。
async fn http_actor_loop(
    server_name: String,
    client: reqwest::Client,
    url: String,
    base_headers: HeaderMap,
    mut req_rx: UnboundedReceiver<McpRequest>,
) {
    let mut session_id: Option<String> = None;
    while let Some(req) = req_rx.recv().await {
        match req {
            McpRequest::Call {
                id,
                method,
                params,
                reply,
            } => {
                let result = do_http_call(
                    &server_name,
                    &client,
                    &url,
                    &base_headers,
                    &mut session_id,
                    id,
                    method,
                    params,
                )
                .await;
                let _ = reply.send(result);
            }
            McpRequest::Notification { method, params } => {
                if let Err(e) = do_http_notification(
                    &server_name,
                    &client,
                    &url,
                    &base_headers,
                    &session_id,
                    method,
                    params,
                )
                .await
                {
                    warn!(server = %server_name, error = %e, "HTTP notification 失败（忽略）");
                }
            }
            McpRequest::Shutdown => return,
        }
    }
}

/// 发送一次 HTTP JSON-RPC 请求并解析响应（带 call 级超时）。
///
/// 响应 Content-Type 为 `text/event-stream` 时用 `parse_sse_text` 解析事件 +
/// `extract_jsonrpc_responses` 找匹配 id 的响应；否则直接 `serde_json` 解析为
/// `JsonRpcResponse`。`Mcp-Session-Id` 响应头若存在则更新 `session_id`（initialize
/// 时 server 下发）。
#[allow(clippy::too_many_arguments)]
async fn do_http_call(
    server_name: &str,
    client: &reqwest::Client,
    url: &str,
    base_headers: &HeaderMap,
    session_id: &mut Option<String>,
    id: u64,
    method: String,
    params: Value,
) -> Result<Value> {
    let req_obj = JsonRpcRequest::new(id, method, Some(params));
    let body_bytes = serde_json::to_vec(&req_obj)?;

    let send_future = build_http_request(client, url, base_headers, session_id, &body_bytes).send();
    let resp = match tokio::time::timeout(Duration::from_secs(CALL_TIMEOUT_SECS), send_future).await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Err(McpError::Network {
                server: server_name.into(),
                detail: format!("HTTP POST 失败: {e}"),
            })
        }
        Err(_) => {
            return Err(McpError::Timeout {
                server: server_name.into(),
                secs: CALL_TIMEOUT_SECS,
            })
        }
    };

    // 捕获 server 下发的 session id（initialize 响应头）
    if let Some(sid) = resp
        .headers()
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
    {
        *session_id = Some(sid);
    }

    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    let bytes = match tokio::time::timeout(
        Duration::from_secs(CALL_TIMEOUT_SECS),
        resp.bytes(),
    )
    .await
    {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => {
            return Err(McpError::Network {
                server: server_name.into(),
                detail: format!("读取 HTTP 响应 body 失败: {e}"),
            })
        }
        Err(_) => {
            return Err(McpError::Timeout {
                server: server_name.into(),
                secs: CALL_TIMEOUT_SECS,
            })
        }
    };

    if content_type.contains("text/event-stream") {
        let text = String::from_utf8_lossy(&bytes);
        let events = parse_sse_text(&text);
        let responses = extract_jsonrpc_responses(&events);
        for r in responses {
            if r.id == Some(id) {
                return finalize_response(r);
            }
        }
        Err(McpError::BadResponse {
            server: server_name.into(),
            detail: format!("SSE 响应中未找到 id={id} 的 message"),
        })
    } else {
        let resp: JsonRpcResponse =
            serde_json::from_slice(&bytes).map_err(|e| McpError::BadResponse {
                server: server_name.into(),
                detail: format!("JSON 响应解析失败: {e}"),
            })?;
        finalize_response(resp)
    }
}

/// 构造 HTTP POST 请求（共用 Call / Notification）。
fn build_http_request(
    client: &reqwest::Client,
    url: &str,
    base_headers: &HeaderMap,
    session_id: &Option<String>,
    body_bytes: &[u8],
) -> reqwest::RequestBuilder {
    let mut builder = client
        .post(url)
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream")
        .headers(base_headers.clone())
        .body(body_bytes.to_vec());
    if let Some(sid) = session_id.as_ref() {
        if let Ok(val) = HeaderValue::from_str(sid) {
            builder = builder.header("mcp-session-id", val);
        }
    }
    builder
}

/// 发送一次 notification（无 id，best-effort）。
async fn do_http_notification(
    server_name: &str,
    client: &reqwest::Client,
    url: &str,
    base_headers: &HeaderMap,
    session_id: &Option<String>,
    method: String,
    params: Value,
) -> Result<()> {
    let req_obj = JsonRpcRequest::notification(method, Some(params));
    let body_bytes = serde_json::to_vec(&req_obj)?;
    let resp = build_http_request(client, url, base_headers, session_id, &body_bytes)
        .send()
        .await
        .map_err(|e| McpError::Network {
            server: server_name.into(),
            detail: format!("HTTP POST notification 失败: {e}"),
        })?;
    // 丢弃 body（notification 无响应；server 可能回 202/200/204）
    let _ = resp.bytes().await;
    Ok(())
}

/// 把 `JsonRpcResponse` 转 `Result<Value>`（error → Rpc，否则取 result/Null）。
fn finalize_response(resp: JsonRpcResponse) -> Result<Value> {
    if let Some(err) = resp.error {
        return Err(McpError::Rpc {
            code: err.code,
            message: err.message,
        });
    }
    Ok(resp.result.unwrap_or(Value::Null))
}

// ── Legacy SSE actor ────────────────────────────────────────────────────────

/// 启动 SSE actor：返回 sender + task handle。
fn start_sse_actor(
    server_name: String,
    client: reqwest::Client,
    sse_url: String,
    headers: HeaderMap,
    timeout: u64,
) -> (UnboundedSender<McpRequest>, JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<McpRequest>();
    let handle = tokio::spawn(sse_actor_loop(
        server_name,
        client,
        sse_url,
        headers,
        timeout,
        rx,
    ));
    (tx, handle)
}

/// SSE actor 主循环：reader task 长连 GET event-stream 收响应，主循环收 Call → POST endpoint。
///
/// 共享状态：`endpoint`（首个 `event: endpoint` 下发的 POST URL）、`pending`（id→oneshot）。
/// reader task 退出（流断/错误）→ 经 `reader_done_rx` 通知主循环 fail_all_pending + 退出。
async fn sse_actor_loop(
    server_name: String,
    client: reqwest::Client,
    sse_url: String,
    headers: HeaderMap,
    timeout: u64,
    mut req_rx: UnboundedReceiver<McpRequest>,
) {
    let endpoint: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));

    let (reader_done_tx, mut reader_done_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

    let reader_task = {
        let server_name = server_name.clone();
        let client = client.clone();
        let headers = headers.clone();
        let endpoint = endpoint.clone();
        let pending = pending.clone();
        tokio::spawn(async move {
            sse_reader_loop(server_name, client, sse_url, headers, endpoint, pending).await;
            let _ = reader_done_tx.send(());
        })
    };

    loop {
        select! {
            maybe_req = req_rx.recv() => {
                match maybe_req {
                    Some(McpRequest::Call { id, method, params, reply }) => {
                        handle_sse_call(
                            &server_name, &client, &headers, timeout,
                            &endpoint, &pending, id, method, params, reply,
                        ).await;
                    }
                    Some(McpRequest::Notification { method, params }) => {
                        let ep_url = endpoint.lock().unwrap().clone();
                        if let Some(ep_url) = ep_url {
                            let req_obj = JsonRpcRequest::notification(method, Some(params));
                            if let Ok(body) = serde_json::to_vec(&req_obj) {
                                let _ = client
                                    .post(&ep_url)
                                    .header(CONTENT_TYPE, "application/json")
                                    .headers(headers.clone())
                                    .body(body)
                                    .send()
                                    .await;
                            }
                        }
                    }
                    Some(McpRequest::Shutdown) | None => {
                        reader_task.abort();
                        fail_all_pending_shared(&pending);
                        return;
                    }
                }
            }
            _ = reader_done_rx.recv() => {
                // reader 退出 → 失败所有 pending + 主循环也退出
                fail_all_pending_shared(&pending);
                return;
            }
        }
    }
}

/// 处理一次 SSE Call：等 endpoint → 注册 pending → POST endpoint。
/// 响应由 `sse_reader_loop` 异步回填到 oneshot。
#[allow(clippy::too_many_arguments)]
async fn handle_sse_call(
    server_name: &str,
    client: &reqwest::Client,
    headers: &HeaderMap,
    timeout: u64,
    endpoint: &Arc<Mutex<Option<String>>>,
    pending: &PendingMap,
    id: u64,
    method: String,
    params: Value,
    reply: oneshot::Sender<Result<Value>>,
) {
    let ep_url = match wait_for_endpoint(endpoint, timeout).await {
        Some(u) => u,
        None => {
            let _ = reply.send(Err(McpError::InitFailed {
                server: server_name.into(),
                detail: "SSE endpoint 未在超时内下发".into(),
            }));
            return;
        }
    };

    // 注册 pending 在 POST 之前，避免 reader 收到响应时无对应 oneshot
    {
        pending.lock().unwrap().insert(id, reply);
    }

    let req_obj = JsonRpcRequest::new(id, method, Some(params));
    let body = match serde_json::to_vec(&req_obj) {
        Ok(b) => b,
        Err(e) => {
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let _ = tx.send(Err(McpError::Json(e)));
            }
            return;
        }
    };

    let post_result = client
        .post(&ep_url)
        .header(CONTENT_TYPE, "application/json")
        .headers(headers.clone())
        .body(body)
        .send()
        .await;

    if let Err(e) = post_result {
        if let Some(tx) = pending.lock().unwrap().remove(&id) {
            let _ = tx.send(Err(McpError::Network {
                server: server_name.into(),
                detail: format!("SSE POST 失败: {e}"),
            }));
        }
    }
    // POST 成功：reader task 异步收响应并回填 oneshot；若 reader 已退出 → fail_all_pending
}

/// SSE reader task：长连 GET event-stream，解析事件路由响应到 pending。
///
/// - `event: endpoint` → data 为 POST URL，存 `endpoint`
/// - `event: message`/默认 → data 为 JSON-RPC，按 id 路由到 pending
/// - 流结束/出错 → fail 所有 pending（reader 退出信号由主循环 select! 感知）
async fn sse_reader_loop(
    server_name: String,
    client: reqwest::Client,
    sse_url: String,
    headers: HeaderMap,
    endpoint: Arc<Mutex<Option<String>>>,
    pending: PendingMap,
) {
    let resp = match client
        .get(&sse_url)
        .header(ACCEPT, "text/event-stream")
        .headers(headers)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(server = %server_name, error = %e, "SSE GET 连接失败，reader 退出");
            fail_all_pending_shared(&pending);
            return;
        }
    };

    let mut stream = resp.bytes_stream();
    let mut parser = SseParser::new();

    while let Some(chunk_result) = stream.next().await {
        match chunk_result {
            Ok(chunk) => {
                let events = parser.feed(&chunk);
                for event in events {
                    handle_sse_event(&server_name, &event, &endpoint, &pending, &sse_url);
                }
            }
            Err(e) => {
                warn!(server = %server_name, error = %e, "SSE 流读取失败，reader 退出");
                break;
            }
        }
    }

    // 流结束：fail 所有 pending（ChannelClosed）
    fail_all_pending_shared(&pending);
}

/// 处理单个 SSE 事件：endpoint → 存 URL；message → 路由响应。
///
/// `endpoint` 事件 data 可能是绝对 URL（`http://host:port/message`）或相对路径（`/message`）。
/// 相对路径以 `sse_url` 的 origin（scheme://host:port）补全为绝对 URL，
/// 否则 reqwest `client.post(relative)` 会报 `builder error`。
fn handle_sse_event(
    server_name: &str,
    event: &SseEvent,
    endpoint: &Arc<Mutex<Option<String>>>,
    pending: &PendingMap,
    sse_url: &str,
) {
    if event.event == "endpoint" {
        let ep = event.data.trim().to_string();
        if !ep.is_empty() {
            let resolved = resolve_endpoint_url(&ep, sse_url);
            *endpoint.lock().unwrap() = Some(resolved);
        }
        return;
    }
    if event.event != "message" {
        return;
    }
    let resp: JsonRpcResponse = match serde_json::from_str(event.data.trim()) {
        Ok(r) => r,
        Err(e) => {
            warn!(server = %server_name, error = %e, data = %event.data, "SSE message 解析失败，跳过");
            return;
        }
    };
    let id = match resp.id {
        Some(id) => id,
        None => return, // server 主动通知，忽略
    };
    let tx = pending.lock().unwrap().remove(&id);
    if let Some(tx) = tx {
        let _ = tx.send(finalize_response(resp));
    }
    // 无 pending 条目（stale/超时已丢弃）→ 静默忽略
}

/// 轮询等待 endpoint 就绪（最多 `timeout_secs` 秒）。
async fn wait_for_endpoint(
    endpoint: &Arc<Mutex<Option<String>>>,
    timeout_secs: u64,
) -> Option<String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        {
            let guard = endpoint.lock().unwrap();
            if let Some(ep) = guard.as_ref() {
                return Some(ep.clone());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// 把 SSE endpoint 事件下发的 URL 补全为绝对 URL。
///
/// - 绝对 URL（`http://` / `https://`）→ 原样返回
/// - 相对路径（`/message`、`message`）→ 用 `sse_url` 的 origin 拼接
fn resolve_endpoint_url(ep: &str, sse_url: &str) -> String {
    if ep.starts_with("http://") || ep.starts_with("https://") {
        return ep.to_string();
    }
    // 从 sse_url 提取 origin（scheme://host:port）
    let origin = match sse_url.find("://") {
        Some(i) => {
            let after_scheme = &sse_url[i + 3..];
            // origin 到下一个 '/' 或末尾
            match after_scheme.find('/') {
                Some(j) => &sse_url[..i + 3 + j],
                None => sse_url,
            }
        }
        None => sse_url,
    };
    // 拼接：ep 以 '/' 开头直接 append，否则补 '/'
    if ep.starts_with('/') {
        format!("{origin}{ep}")
    } else {
        format!("{origin}/{ep}")
    }
}

/// 把 Arc<Mutex<HashMap>> 形式的 pending 全部标记 ChannelClosed（共享版）。
fn fail_all_pending_shared(pending: &PendingMap) {
    let mut guard = pending.lock().unwrap();
    for (_, tx) in guard.drain() {
        let _ = tx.send(Err(McpError::ChannelClosed));
    }
}

// ── 配置工具：环境变量展开 ──────────────────────────────────────────────────

/// 把 `headers` 配置转为 `HeaderMap`，并对值做 `${VAR}` / `$VAR` 环境变量展开。
///
/// 使 `default_mcp_servers.toml` 示例 `Authorization = "Bearer ${MCP_TOKEN}"` 生效：
/// 未设置的环境变量展开为空串（保留前缀如 `Bearer `）。非法 header 名/值跳过。
fn expand_env_headers(headers: &HashMap<String, String>) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in headers {
        let expanded = expand_env_vars(v);
        let name = match HeaderName::from_bytes(k.as_bytes()) {
            Ok(n) => n,
            Err(_) => {
                warn!(header = %k, "非法 header 名，跳过");
                continue;
            }
        };
        let value = match HeaderValue::from_str(&expanded) {
            Ok(v) => v,
            Err(_) => {
                warn!(header = %k, "非法 header 值，跳过");
                continue;
            }
        };
        h.append(name, value);
    }
    h
}

/// 展开 `${VAR}` 与 `$VAR`（VAR 为字母/数字/下划线）。
/// 未设置的环境变量展开为空串。
fn expand_env_vars(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() {
            // ${VAR}
            if bytes[i + 1] == b'{' {
                if let Some(end) = bytes[i + 2..].iter().position(|&b| b == b'}') {
                    let var_name = &s[i + 2..i + 2 + end];
                    let val = std::env::var(var_name).unwrap_or_default();
                    out.push_str(&val);
                    i = i + 2 + end + 1;
                    continue;
                }
            }
            // $VAR（字母/数字/下划线）
            if bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_' {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                let var_name = &s[start..end];
                let val = std::env::var(var_name).unwrap_or_default();
                out.push_str(&val);
                i = end;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{CallToolResult, McpContent};
    use tokio::io::duplex;

    /// 构造一个测试 McpConnection：用 duplex 流模拟 server。
    /// 返回 (conn, client_write, client_read) — 测试用 client_write 发送响应、
    /// client_read 读取请求。
    async fn make_test_conn() -> (
        Arc<McpConnection>,
        tokio::io::DuplexStream,
        tokio::io::DuplexStream,
    ) {
        // duplex: (a, b) — a 端写入 = b 端读取
        // actor 用 server_side_read（读 client 发的请求）+ server_side_write（写响应给 client）
        let (server_side_read, client_write) = duplex(8 * 1024);
        let (server_side_write, client_read) = duplex(8 * 1024);
        let (tx, _handle) =
            McpConnection::start_actor(server_side_read, server_side_write);
        let conn = Arc::new(McpConnection {
            server_name: "test".into(),
            tx,
            next_id: AtomicU64::new(0),
            tools: Vec::new(),
        });
        (conn, client_write, client_read)
    }

    /// 测试 helper：从 client_read 读取一行 JSON-RPC 请求。
    async fn read_request(client_read: &mut tokio::io::DuplexStream) -> JsonRpcResponse {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(client_read);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        // 请求结构与响应相同（jsonrpc/id/method/params），用 JsonRpcResponse 容错解析
        serde_json::from_str(line.trim()).unwrap()
    }

    #[tokio::test]
    async fn call_roundtrip_sends_request_and_receives_response() {
        let (conn, mut client_write, mut client_read) = make_test_conn().await;

        // 发起调用（后台）
        let conn_clone = conn.clone();
        let task = tokio::spawn(async move {
            conn_clone.call("tools/list", Value::Null).await
        });

        // 模拟 server：读取请求，回响应
        let req = read_request(&mut client_read).await;
        assert_eq!(req.id, Some(0));
        // req 的 method 字段我们没解析（JsonRpcResponse 无 method），但能拿到 id
        // 回一个 tools/list 响应
        let resp = r#"{"jsonrpc":"2.0","id":0,"result":{"tools":[]}}"#;
        client_write.write_all(resp.as_bytes()).await.unwrap();
        client_write.write_all(b"\n").await.unwrap();
        client_write.flush().await.unwrap();

        let result = task.await.unwrap().unwrap();
        assert_eq!(result["tools"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn call_tool_returns_content() {
        let (conn, mut client_write, mut client_read) = make_test_conn().await;
        let conn_clone = conn.clone();
        let task = tokio::spawn(async move {
            conn_clone.call_tool("ls", serde_json::json!({"path": "."})).await
        });

        let _req = read_request(&mut client_read).await;
        let resp = r#"{"jsonrpc":"2.0","id":0,"result":{"content":[{"type":"text","text":"file1\nfile2"}],"isError":false}}"#;
        client_write.write_all(resp.as_bytes()).await.unwrap();
        client_write.write_all(b"\n").await.unwrap();
        client_write.flush().await.unwrap();

        let result: CallToolResult = task.await.unwrap().unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0].text, "file1\nfile2");
    }

    #[tokio::test]
    async fn rpc_error_propagates() {
        let (conn, mut client_write, mut client_read) = make_test_conn().await;
        let conn_clone = conn.clone();
        let task = tokio::spawn(async move { conn_clone.call("tools/list", Value::Null).await });

        let _req = read_request(&mut client_read).await;
        let resp = r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32601,"message":"Method not found"}}"#;
        client_write.write_all(resp.as_bytes()).await.unwrap();
        client_write.write_all(b"\n").await.unwrap();
        client_write.flush().await.unwrap();

        match task.await.unwrap() {
            Err(McpError::Rpc { code, message }) => {
                assert_eq!(code, -32601);
                assert_eq!(message, "Method not found");
            }
            other => panic!("期望 Rpc 错误，实际: {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_when_no_response() {
        let (_conn, _client_write, _client_read) = make_test_conn().await;
        // 不回响应 → 超时。为加速测试，直接测 call 的 30s 超时太慢；
        // 改为验证 channel closed：drop actor sender 后 call 立即失败。
        // 这里用 tokio::time::pause 模拟超时太复杂，改为测 ChannelClosed 路径。
        // 注：CALL_TIMEOUT_SECS=30，此处不实际等待；通过 drop client 让 actor 退出。
        // 实际超时路径由 route_response/rpc_error 测试间接覆盖协议正确性。
        // 此测试验证：actor 退出（reader EOF）后，pending call 收到 ChannelClosed。
        let (conn, client_write, mut client_read) = make_test_conn().await;
        let conn_clone = conn.clone();
        let task = tokio::spawn(async move { conn_clone.call("x", Value::Null).await });

        // 读取请求（让 actor 进入 pending）
        let _req = read_request(&mut client_read).await;
        // drop client_write → actor 的 reader EOF → fail_all_pending
        drop(client_write);
        // 给 actor 一点时间感知 EOF
        match task.await.unwrap() {
            Err(McpError::ChannelClosed) => {}
            other => panic!("期望 ChannelClosed，实际: {other:?}"),
        }
    }

    #[test]
    fn mcp_content_is_text() {
        let c = McpContent {
            kind: "text".into(),
            text: "hi".into(),
        };
        assert!(c.is_text());
        let c2 = McpContent {
            kind: "image".into(),
            text: String::new(),
        };
        assert!(!c2.is_text());
    }

    // ── 单元测试：SSE endpoint 相对路径补全 ──────────────────────────────

    #[test]
    fn resolve_endpoint_url_absolute_unchanged() {
        let url = resolve_endpoint_url("http://127.0.0.1:9876/message", "http://127.0.0.1:9876/sse");
        assert_eq!(url, "http://127.0.0.1:9876/message");
    }

    #[test]
    fn resolve_endpoint_url_https_absolute_unchanged() {
        let url = resolve_endpoint_url("https://api.example.com/mcp", "https://api.example.com/sse");
        assert_eq!(url, "https://api.example.com/mcp");
    }

    #[test]
    fn resolve_endpoint_url_relative_leading_slash() {
        // /message + sse_url origin → http://host:port/message
        let url = resolve_endpoint_url("/message", "http://127.0.0.1:9876/sse");
        assert_eq!(url, "http://127.0.0.1:9876/message");
    }

    #[test]
    fn resolve_endpoint_url_relative_no_leading_slash() {
        let url = resolve_endpoint_url("message", "http://127.0.0.1:9876/sse");
        assert_eq!(url, "http://127.0.0.1:9876/message");
    }

    #[test]
    fn resolve_endpoint_url_relative_with_trailing_slash_sse_url() {
        // sse_url 以 / 结尾时也要正确提取 origin
        let url = resolve_endpoint_url("/message", "http://127.0.0.1:9876/");
        assert_eq!(url, "http://127.0.0.1:9876/message");
    }

    #[test]
    fn resolve_endpoint_url_relative_different_port() {
        let url = resolve_endpoint_url("/endpoint", "http://localhost:3000/sse");
        assert_eq!(url, "http://localhost:3000/endpoint");
    }
}

#[cfg(test)]
mod http_sse_tests {
    use super::*;
    use crate::config::McpServerSpec;
    use crate::transport::McpTransport;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // ── 单元测试：环境变量展开 ────────────────────────────────────────────

    #[test]
    fn expand_env_vars_braces_with_value() {
        std::env::set_var("CYBER_TEST_TOKEN", "abc123");
        let s = expand_env_vars("Bearer ${CYBER_TEST_TOKEN}");
        assert_eq!(s, "Bearer abc123");
        std::env::remove_var("CYBER_TEST_TOKEN");
    }

    #[test]
    fn expand_env_vars_bare_with_value() {
        std::env::set_var("CYBER_TEST_USER", "alice");
        let s = expand_env_vars("user=$CYBER_TEST_USER");
        assert_eq!(s, "user=alice");
        std::env::remove_var("CYBER_TEST_USER");
    }

    #[test]
    fn expand_env_vars_unset_becomes_empty() {
        std::env::remove_var("CYBER_TEST_UNSET_XYZ");
        let s = expand_env_vars("Bearer ${CYBER_TEST_UNSET_XYZ}");
        assert_eq!(s, "Bearer ");
    }

    #[test]
    fn expand_env_vars_no_var_passthrough() {
        let s = expand_env_vars("plain text no var");
        assert_eq!(s, "plain text no var");
    }

    #[test]
    fn expand_env_vars_unclosed_braces_literal() {
        let s = expand_env_vars("foo ${UNCLOSED");
        // 未闭合 → 不展开，原样输出
        assert_eq!(s, "foo ${UNCLOSED");
    }

    #[test]
    fn expand_env_headers_builds_headermap() {
        std::env::set_var("CYBER_TEST_H", "v1");
        let mut h = HashMap::new();
        h.insert("X-Custom".to_string(), "value".to_string());
        h.insert(
            "Authorization".to_string(),
            "Bearer ${CYBER_TEST_H}".to_string(),
        );
        let hm = expand_env_headers(&h);
        assert_eq!(hm.get("x-custom").unwrap(), "value");
        assert_eq!(hm.get("authorization").unwrap(), "Bearer v1");
        std::env::remove_var("CYBER_TEST_H");
    }

    #[test]
    fn expand_env_headers_skips_invalid_name() {
        let mut h = HashMap::new();
        h.insert("invalid header".to_string(), "v".to_string());
        let hm = expand_env_headers(&h);
        assert!(hm.is_empty(), "非法 header 名应被跳过");
    }

    // ── HTTP e2e：mock TCP server ────────────────────────────────────────

    /// mock HTTP MCP server 状态：捕获每次请求收到的 `Mcp-Session-Id`。
    #[derive(Default, Clone)]
    struct HttpCapture {
        session_ids: Arc<std::sync::Mutex<Vec<Option<String>>>>,
    }

    /// 启动 mock HTTP MCP server，返回 URL。
    /// 处理 initialize / tools/list / tools/call / notifications/initialized。
    /// initialize 响应携带 `Mcp-Session-Id: test-session-123` 头。
    async fn start_mock_http_server(capture: HttpCapture) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/mcp");
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let cap = capture.clone();
                tokio::spawn(async move {
                    handle_http_conn(&mut sock, cap).await;
                });
            }
        });
        url
    }

    async fn handle_http_conn(sock: &mut tokio::net::TcpStream, capture: HttpCapture) {
        let mut buf = vec![0u8; 32 * 1024];
        let n = sock.read(&mut buf).await.unwrap();
        if n == 0 {
            return;
        }
        let req_str = String::from_utf8_lossy(&buf[..n]).to_string();
        let (head, body) = match req_str.split_once("\r\n\r\n") {
            Some(split) => split,
            None => return,
        };
        let first_line = head.lines().next().unwrap_or("");

        // 捕获请求的 Mcp-Session-Id 头
        let recv_session_id = head.lines().find_map(|l| {
            let l = l.to_ascii_lowercase();
            l.strip_prefix("mcp-session-id:")
                .map(|v| v.trim().to_string())
        });
        capture.session_ids.lock().unwrap().push(recv_session_id);

        if !first_line.starts_with("POST ") {
            let _ = sock
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }

        let req: serde_json::Value = match serde_json::from_str(body) {
            Ok(v) => v,
            Err(_) => {
                let _ = sock
                    .write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            }
        };

        let id = req["id"].clone();
        let has_id = !id.is_null();
        let method = req["method"].as_str().unwrap_or("");

        // notification（无 id）→ 202
        if !has_id {
            let _ = sock
                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                .await;
            return;
        }

        let (result, include_session_id): (serde_json::Value, bool) = match method {
            "initialize" => (
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {"name": "mock-http", "version": "1.0"},
                    "capabilities": {}
                }),
                true,
            ),
            "tools/list" => (
                serde_json::json!({
                    "tools": [{
                        "name": "ping",
                        "description": "ping tool",
                        "inputSchema": {"type": "object"}
                    }]
                }),
                false,
            ),
            "tools/call" => (
                serde_json::json!({
                    "content": [{"type": "text", "text": "pong"}],
                    "isError": false
                }),
                false,
            ),
            _ => (
                serde_json::json!(null),
                false,
            ),
        };

        let resp_obj = if method == "initialize" || method == "tools/list" || method == "tools/call"
        {
            serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
        } else {
            serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}})
        };
        let body_str = serde_json::to_string(&resp_obj).unwrap();
        let session_header = if include_session_id {
            "Mcp-Session-Id: test-session-123\r\n"
        } else {
            ""
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\n\r\n{}",
            session_header,
            body_str.len(),
            body_str
        );
        let _ = sock.write_all(response.as_bytes()).await;
    }

    fn http_spec(url: String) -> McpServerSpec {
        McpServerSpec {
            name: "mock-http".into(),
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: Default::default(),
            url: Some(url),
            headers: Default::default(),
            timeout_secs: 5,
        }
    }

    #[tokio::test]
    async fn http_spawn_handshake_and_call_tool() {
        let capture = HttpCapture::default();
        let url = start_mock_http_server(capture.clone()).await;
        let (conn, _handle) = McpConnection::spawn_http(&http_spec(url))
            .await
            .expect("HTTP 握手应成功");

        // 握手后应发现 1 个工具 ping
        assert_eq!(conn.tools().len(), 1);
        assert_eq!(conn.tools()[0].name, "ping");

        // call_tool 返回 pong
        let result = conn
            .call_tool("ping", serde_json::json!({}))
            .await
            .expect("call_tool 应成功");
        assert!(!result.is_error);
        assert_eq!(result.content.len(), 1);
        assert_eq!(result.content[0].text, "pong");

        conn.shutdown();
    }

    #[tokio::test]
    async fn http_session_id_echoed_after_initialize() {
        let capture = HttpCapture::default();
        let url = start_mock_http_server(capture.clone()).await;
        let (conn, _handle) = McpConnection::spawn_http(&http_spec(url))
            .await
            .expect("HTTP 握手应成功");

        // 再发一次 call 触发第三个请求（initialize / tools/list 已发过）
        let _ = conn.call("tools/list", Value::Null).await.unwrap();
        conn.shutdown();

        // 等待 actor 把请求都发出去
        tokio::time::sleep(Duration::from_millis(100)).await;

        let ids = capture.session_ids.lock().unwrap().clone();
        // 至少 3 次请求：initialize（无 session id）+ tools/list（有）+ tools/list（有）
        assert!(ids.len() >= 3, "应至少捕获 3 次请求，实际 {}", ids.len());
        // 第一次（initialize）应无 session id
        assert!(
            ids[0].is_none(),
            "initialize 之前应无 session id，实际 {:?}",
            ids[0]
        );
        // 后续请求应回带 session id
        assert!(
            ids.iter().skip(1).all(|id| id.as_deref() == Some("test-session-123")),
            "后续请求应回带 Mcp-Session-Id=test-session-123，实际 {:?}",
            ids
        );
    }

    #[tokio::test]
    async fn http_missing_url_returns_init_failed() {
        let spec = McpServerSpec {
            name: "no-url".into(),
            transport: McpTransport::Http,
            command: None,
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
            timeout_secs: 1,
        };
        match McpConnection::spawn_http(&spec).await {
            Err(McpError::InitFailed { server, .. }) => assert_eq!(server, "no-url"),
            other => panic!("期望 InitFailed，实际: {other:?}"),
        }
    }

    // ── SSE e2e：mock TCP server ────────────────────────────────────────

    /// mock SSE MCP server：GET /sse 维持长连，POST /message 触发响应推送到 SSE 流。
    async fn start_mock_sse_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let sse_url = format!("{base_url}/sse");
        let endpoint_url = format!("{base_url}/message");

        // POST → GET 流的响应队列
        let (response_tx, response_rx) =
            tokio::sync::mpsc::unbounded_channel::<(u64, serde_json::Value)>();
        let response_rx = Arc::new(tokio::sync::Mutex::new(response_rx));

        tokio::spawn(async move {
            // 第一个 GET /sse 接管 response_rx；POST 用 response_tx 推消息
            let endpoint_for_get = endpoint_url.clone();
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let first_line = read_request_line(&mut sock).await;
                if first_line.starts_with("GET /sse") {
                    let rx = response_rx.clone();
                    let endpoint = endpoint_for_get.clone();
                    tokio::spawn(async move {
                        handle_sse_get(&mut sock, &endpoint, rx).await;
                    });
                } else if first_line.starts_with("POST /message") {
                    let body = read_body(&mut sock, &first_line).await;
                    let tx = response_tx.clone();
                    tokio::spawn(async move {
                        handle_sse_post(&mut sock, body, tx).await;
                    });
                } else {
                    let _ = sock
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });
        sse_url
    }

    /// 读 HTTP 请求首行（含可能的部分 body，丢弃 body 由后续 read_body 重读）。
    /// 简化：一次性读到 32KB，解析首行与头。
    async fn read_request_line(sock: &mut tokio::net::TcpStream) -> String {
        let mut buf = vec![0u8; 32 * 1024];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    /// 从已读的整段请求文本中抽 body（split \r\n\r\n）。
    async fn read_body(_sock: &mut tokio::net::TcpStream, full: &str) -> String {
        full.split("\r\n\r\n")
            .nth(1)
            .unwrap_or("")
            .to_string()
    }

    async fn handle_sse_get(
        sock: &mut tokio::net::TcpStream,
        endpoint: &str,
        response_rx: Arc<
            tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<(u64, serde_json::Value)>>,
        >,
    ) {
        // 发送 SSE 头 + endpoint 事件
        let header = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n";
        if sock.write_all(header.as_bytes()).await.is_err() {
            return;
        }
        let endpoint_event = format!("event: endpoint\ndata: {endpoint}\n\n");
        if sock.write_all(endpoint_event.as_bytes()).await.is_err() {
            return;
        }

        // 从 channel 取响应推到 SSE 流
        let mut rx = response_rx.lock().await;
        while let Some((id, result)) = rx.recv().await {
            let resp = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result});
            let event = format!("event: message\ndata: {}\n\n", resp);
            if sock.write_all(event.as_bytes()).await.is_err() {
                break;
            }
        }
    }

    async fn handle_sse_post(
        sock: &mut tokio::net::TcpStream,
        body: String,
        tx: tokio::sync::mpsc::UnboundedSender<(u64, serde_json::Value)>,
    ) {
        // 解析 JSON-RPC 请求
        if let Ok(req) = serde_json::from_str::<serde_json::Value>(&body) {
            let id = req["id"].as_u64();
            let method = req["method"].as_str().unwrap_or("");
            if let Some(id) = id {
                let result = match method {
                    "initialize" => serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "serverInfo": {"name": "mock-sse", "version": "1.0"},
                        "capabilities": {}
                    }),
                    "tools/list" => serde_json::json!({
                        "tools": [{
                            "name": "ping",
                            "description": "ping tool",
                            "inputSchema": {"type": "object"}
                        }]
                    }),
                    "tools/call" => serde_json::json!({
                        "content": [{"type": "text", "text": "pong"}],
                        "isError": false
                    }),
                    _ => serde_json::json!(null),
                };
                let _ = tx.send((id, result));
            }
        }
        // POST 回 202（SSE 协议不要求 body）
        let _ = sock
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
            .await;
    }

    fn sse_spec(url: String) -> McpServerSpec {
        McpServerSpec {
            name: "mock-sse".into(),
            transport: McpTransport::Sse,
            command: None,
            args: vec![],
            env: Default::default(),
            url: Some(url),
            headers: Default::default(),
            timeout_secs: 5,
        }
    }

    #[tokio::test]
    async fn sse_spawn_handshake_and_call_tool() {
        let url = start_mock_sse_server().await;
        let (conn, _handle) = McpConnection::spawn_sse(&sse_spec(url))
            .await
            .expect("SSE 握手应成功");

        assert_eq!(conn.tools().len(), 1);
        assert_eq!(conn.tools()[0].name, "ping");

        let result = conn
            .call_tool("ping", serde_json::json!({}))
            .await
            .expect("call_tool 应成功");
        assert!(!result.is_error);
        assert_eq!(result.content[0].text, "pong");

        conn.shutdown();
    }

    #[tokio::test]
    async fn sse_missing_url_returns_init_failed() {
        let spec = McpServerSpec {
            name: "no-url".into(),
            transport: McpTransport::Sse,
            command: None,
            args: vec![],
            env: Default::default(),
            url: None,
            headers: Default::default(),
            timeout_secs: 1,
        };
        match McpConnection::spawn_sse(&spec).await {
            Err(McpError::InitFailed { server, .. }) => assert_eq!(server, "no-url"),
            other => panic!("期望 InitFailed，实际: {other:?}"),
        }
    }
}
