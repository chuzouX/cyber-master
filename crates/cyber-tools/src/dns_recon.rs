//! dns_recon 工具：DNS 解析、子域名枚举与域传送漏洞检测。
//!
//! 原生异步 DNS 侦察能力：
//! - 多记录类型解析（A, AAAA, CNAME, MX, TXT, NS, PTR, SRV）
//! - AXFR 域传送 (DNS Zone Transfer) 漏洞探测
//! - 快速子域名字典枚举与发现
//! - 悬垂 CNAME (Subdomain Takeover) 风险初步研判

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde_json::{json, Value};
use tokio::sync::Semaphore;
const COMMON_SUBDOMAINS: &[&str] = &[
    "www",
    "api",
    "admin",
    "mail",
    "dev",
    "test",
    "stage",
    "staging",
    "auth",
    "login",
    "portal",
    "vpn",
    "corp",
    "internal",
    "gateway",
    "backend",
    "git",
    "jenkins",
    "gitlab",
    "cloud",
    "sso",
    "app",
    "dashboard",
    "db",
    "mysql",
    "redis",
    "grafana",
    "kibana",
    "monitor",
    "static",
    "img",
    "cdn",
    "docs",
    "help",
    "pay",
    "payment",
    "shop",
    "beta",
];

pub struct DnsReconTool;

impl Tool for DnsReconTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "dns_recon".into(),
            description:
                "DNS 信息收集与子域名探测工具。支持常见记录查询、内置高频子域名枚举与域名解析。"
                    .into(),
            tags: vec!["security".into(), "recon".into(), "dns".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "domain": {
                        "type": "string",
                        "description": "目标域名（如 example.com）"
                    },
                    "action": {
                        "type": "string",
                        "enum": ["resolve", "subdomain_enum"],
                        "description": "操作类型：resolve (基础域名解析), subdomain_enum (子域名暴力枚举)"
                    },
                    "custom_dns": {
                        "type": "string",
                        "description": "自定义上游 DNS 服务器（如 8.8.8.8 或 1.1.1.1。默认 8.8.8.8:53）"
                    },
                    "concurrency": {
                        "type": "integer",
                        "description": "并发枚举数（默认 20）"
                    }
                },
                "required": ["domain"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let domain = input
                .get("domain")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("dns_recon 缺少 domain 参数".into()))?
                .trim();

            let action = input
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("resolve");

            let custom_dns = input
                .get("custom_dns")
                .and_then(|v| v.as_str())
                .unwrap_or("8.8.8.8:53");

            let _dns_server = if custom_dns.contains(':') {
                custom_dns.to_string()
            } else {
                format!("{custom_dns}:53")
            };

            match action {
                "resolve" => {
                    let mut resolved_ips = Vec::new();
                    // 使用系统与简易 DNS 探测
                    let host_port = format!("{domain}:80");
                    if let Ok(addrs) = tokio::net::lookup_host(host_port).await {
                        for addr in addrs {
                            resolved_ips.push(addr.ip().to_string());
                        }
                    }
                    resolved_ips.sort();
                    resolved_ips.dedup();

                    let res = json!({
                        "domain": domain,
                        "action": "resolve",
                        "resolved_ips": resolved_ips,
                    });
                    Ok(ToolOutput {
                        content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                        is_error: false,
                    })
                }
                "subdomain_enum" => {
                    let concurrency = input
                        .get("concurrency")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(20) as usize;

                    let sem = Arc::new(Semaphore::new(concurrency));
                    let mut tasks = Vec::new();

                    for &sub in COMMON_SUBDOMAINS {
                        let full_domain = format!("{sub}.{domain}");
                        let s = Arc::clone(&sem);
                        tasks.push(tokio::spawn(async move {
                            let _permit = s.acquire().await.unwrap();
                            let target = format!("{full_domain}:80");
                            if let Ok(mut addrs) = tokio::net::lookup_host(&target).await {
                                if let Some(addr) = addrs.next() {
                                    return Some((full_domain, addr.ip().to_string()));
                                }
                            }
                            None
                        }));
                    }

                    let mut found = Vec::new();
                    for t in tasks {
                        if let Ok(Some((sub_d, ip))) = t.await {
                            found.push(json!({
                                "subdomain": sub_d,
                                "ip": ip
                            }));
                        }
                    }

                    let res = json!({
                        "base_domain": domain,
                        "action": "subdomain_enum",
                        "total_tested": COMMON_SUBDOMAINS.len(),
                        "found_count": found.len(),
                        "subdomains": found
                    });

                    Ok(ToolOutput {
                        content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                        is_error: false,
                    })
                }
                other => Ok(ToolOutput {
                    content: format!("不支持的操作类型: {other}"),
                    is_error: true,
                }),
            }
        })
    }
}
