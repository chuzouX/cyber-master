//! JSON-RPC 2.0 + MCP 协议类型。
//!
//! JSON-RPC 消息以换行分隔（newline-delimited JSON）经 stdio 传输。
//! MCP 协议字段用 camelCase（`protocolVersion` / `inputSchema` / `serverInfo`），
//! 故 MCP 特定类型加 `#[serde(rename_all = "camelCase")]`。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 请求。
///
/// `id` 为 `None` 时是 notification（无响应，序列化时省略 id 字段）。
/// stdio actor 的 notification 旧用 id=0 占位，现已改用 `notification()` 真·无 id。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest<T> {
    pub jsonrpc: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    pub method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<T>,
}

impl<T> JsonRpcRequest<T> {
    /// 构造一个期望响应的请求（带 id）。
    pub fn new(id: u64, method: impl Into<String>, params: Option<T>) -> Self {
        Self {
            jsonrpc: "2.0",
            id: Some(id),
            method: method.into(),
            params,
        }
    }

    /// 构造一个 notification（无 id，无响应）—— HTTP/SSE 传输需要规范的无 id 请求。
    pub fn notification(method: impl Into<String>, params: Option<T>) -> Self {
        Self {
            jsonrpc: "2.0",
            id: None,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 错误对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JSON-RPC 2.0 响应（`id` 为 `None` 时是 notification，应忽略）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    /// 是否为 notification（无 id，server 主动推送，如 `notifications/initialized`）。
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

// ── MCP 协议特定类型 ──────────────────────────────────────────────────────

/// `initialize` 请求参数。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    pub protocol_version: String,
    pub capabilities: Value,
    pub client_info: ClientInfo,
}

/// `initialize` 响应结果。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default)]
    pub protocol_version: String,
    #[serde(default)]
    pub server_info: Option<ServerInfo>,
    #[serde(default)]
    pub capabilities: Value,
}

/// 客户端/服务端信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientInfo {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// `tools/list` 响应结果。
#[derive(Debug, Clone, Deserialize)]
pub struct ToolListResult {
    #[serde(default)]
    pub tools: Vec<McpToolSchema>,
}

/// MCP server 暴露的工具 schema（来自 `tools/list`）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolSchema {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: Value,
}

/// `tools/call` 请求参数。
#[derive(Debug, Clone, Serialize)]
pub struct CallToolParams {
    pub name: String,
    pub arguments: Value,
}

/// `tools/call` 响应结果。
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<McpContent>,
    #[serde(default)]
    pub is_error: bool,
}

/// `tools/call` 返回的内容块。
#[derive(Debug, Clone, Deserialize)]
pub struct McpContent {
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: String,
}

impl McpContent {
    /// 是否为文本内容块。
    pub fn is_text(&self) -> bool {
        self.kind == "text"
    }
}

/// 当前使用的 MCP 协议版本（2024-11-05）。
pub const PROTOCOL_VERSION: &str = "2024-11-05";

/// 客户端信息（cyber-master）。
pub fn client_info() -> ClientInfo {
    ClientInfo {
        name: "cyber-master".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialize_request_with_params() {
        let req = JsonRpcRequest::new(
            1,
            "tools/call",
            Some(CallToolParams {
                name: "ls".into(),
                arguments: serde_json::json!({"path": "."}),
            }),
        );
        let s = serde_json::to_string(&req).unwrap();
        assert!(s.contains("\"jsonrpc\":\"2.0\""));
        assert!(s.contains("\"id\":1"));
        assert!(s.contains("\"method\":\"tools/call\""));
        assert!(s.contains("\"name\":\"ls\""));
    }

    #[test]
    fn serialize_request_without_params_omits_field() {
        let req: JsonRpcRequest<Value> = JsonRpcRequest::new(2, "tools/list", None);
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("params"), "None params 应被 skip");
    }

    #[test]
    fn deserialize_response_with_result() {
        let raw = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.id, Some(1));
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        assert!(!resp.is_notification());
    }

    #[test]
    fn deserialize_response_with_error() {
        let raw = r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32601,"message":"Method not found"}}"#;
        let resp: JsonRpcResponse = serde_json::from_str(raw).unwrap();
        assert_eq!(resp.id, Some(3));
        let err = resp.error.unwrap();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "Method not found");
    }

    #[test]
    fn deserialize_notification_has_no_id() {
        let raw = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let resp: JsonRpcResponse = serde_json::from_str(raw).unwrap();
        assert!(resp.id.is_none());
        assert!(resp.is_notification());
    }

    #[test]
    fn deserialize_tool_list_result() {
        let raw = r#"{"tools":[{"name":"read","description":"read file","inputSchema":{"type":"object"}}]}"#;
        let r: ToolListResult = serde_json::from_str(raw).unwrap();
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tools[0].name, "read");
        assert_eq!(r.tools[0].description, "read file");
        assert_eq!(r.tools[0].input_schema["type"], "object");
    }

    #[test]
    fn deserialize_call_tool_result() {
        let raw = r#"{"content":[{"type":"text","text":"hello"}],"isError":false}"#;
        let r: CallToolResult = serde_json::from_str(raw).unwrap();
        assert!(!r.is_error);
        assert_eq!(r.content.len(), 1);
        assert!(r.content[0].is_text());
        assert_eq!(r.content[0].text, "hello");
    }

    #[test]
    fn deserialize_call_tool_result_missing_is_error_defaults_false() {
        let raw = r#"{"content":[{"type":"text","text":"x"}]}"#;
        let r: CallToolResult = serde_json::from_str(raw).unwrap();
        assert!(!r.is_error, "缺失 isError 应回退 false");
    }
}
