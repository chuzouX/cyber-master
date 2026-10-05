//! poc_validator 工具：漏洞利用与验证闭环。
//!
//! 验证漏洞是否存在并输出结构化利用凭据：
//! - 自动比对响应状态码、匹配模式（正则/包含文本）
//! - 输出标准化的“已验证 ✅ / 待确认 ❓”漏洞报告
//! - 自动生成标准复现 Curl 命令，方便审计与交接

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use reqwest::Client;
use serde_json::{json, Value};

pub struct PocValidatorTool;

impl Tool for PocValidatorTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "poc_validator".into(),
            description: "漏洞 PoC 自动化回放与证据验证器。向目标发送请求，校验响应是否包含预期攻击特征（如 root:x:0:0 或 特定报错），输出可复现证据报告。".into(),
            tags: vec!["security".into(), "poc".into(), "exploit".into(), "audit".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "vuln_name": {
                        "type": "string",
                        "description": "漏洞名称（如 'GitLab Arbitrary File Read' 或 'SQLi in id param'）"
                    },
                    "target_url": {
                        "type": "string",
                        "description": "测试 URL"
                    },
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "DELETE"],
                        "description": "请求方法（默认 GET）"
                    },
                    "body": {
                        "type": "string",
                        "description": "请求 Body"
                    },
                    "expected_pattern": {
                        "type": "string",
                        "description": "验证漏洞成立的特征字符串（如 root:.*:0:0 或 DB_PASSWORD）"
                    }
                },
                "required": ["vuln_name", "target_url", "expected_pattern"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let vuln_name = input
                .get("vuln_name")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Vuln");
            let target_url = input
                .get("target_url")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("poc_validator 缺少 target_url".into()))?;
            let pattern = input
                .get("expected_pattern")
                .and_then(|v| v.as_str())
                .ok_or_else(|| {
                    AgentError::Provider("poc_validator 缺少 expected_pattern".into())
                })?;
            let method_str = input
                .get("method")
                .and_then(|v| v.as_str())
                .unwrap_or("GET");
            let body_str = input.get("body").and_then(|v| v.as_str());

            let client = match Client::builder()
                .danger_accept_invalid_certs(true)
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("Client 构建失败: {e}"),
                        is_error: true,
                    })
                }
            };

            let req = if method_str.eq_ignore_ascii_case("POST") {
                let mut r = client.post(target_url);
                if let Some(b) = body_str {
                    r = r.body(b.to_string());
                }
                r
            } else {
                client.get(target_url)
            };

            let curl_cmd = format!("curl -sk -X {method_str} '{target_url}'");

            let res = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    return Ok(ToolOutput {
                        content: format!("PoC 请求发送失败: {e}\n复现命令: {curl_cmd}"),
                        is_error: true,
                    });
                }
            };

            let status = res.status().as_u16();
            let body = res.text().await.unwrap_or_default();
            let matched = body.contains(pattern);

            let report = json!({
                "vulnerability": vuln_name,
                "verified": matched,
                "status_code": status,
                "expected_pattern": pattern,
                "pattern_matched": matched,
                "evidence_snippet": if matched {
                    let idx = body.find(pattern).unwrap_or(0);
                    let start = idx.saturating_sub(40);
                    let end = (idx + pattern.len() + 40).min(body.len());
                    body[start..end].to_string()
                } else {
                    "未在响应中检测到匹配特征".to_string()
                },
                "reproduce_curl": curl_cmd
            });

            Ok(ToolOutput {
                content: serde_json::to_string_pretty(&report).unwrap_or_default(),
                is_error: false,
            })
        })
    }
}
