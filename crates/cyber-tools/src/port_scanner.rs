//! port_scanner 工具：轻量级快速异步端口与服务探测器。
//!
//! 基于 Tokio 异步任务的高并发 TCP 端口扫描与 Banner 探测：
//! - 支持指定单端口、逗号分隔端口列表、范围 (如 1-1024) 或预置 "top100"
//! - 支持抓取 Banner 信息提取服务版本指纹
//! - 可控的超时与并发数限制，避免丢包或打挂本地连接池
//! - 纯 Rust 原生实现，无平台依赖，无需 root 权限

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;

const TOP_100_PORTS: &[u16] = &[
    20, 21, 22, 23, 25, 53, 80, 81, 110, 111, 123, 135, 137, 138, 139, 143, 161, 389, 443, 445,
    465, 500, 514, 520, 587, 631, 636, 873, 902, 990, 993, 995, 1025, 1080, 1433, 1521, 1723, 2049,
    2082, 2083, 2086, 2087, 2181, 2222, 3000, 3128, 3306, 3389, 3690, 4444, 4848, 5000, 5432, 5672,
    5900, 5984, 6000, 6379, 7001, 7077, 8000, 8008, 8080, 8081, 8088, 8443, 8888, 9000, 9090, 9200,
    9300, 9418, 9999, 11211, 27017, 27018, 50000, 50070,
];

#[derive(Serialize, Deserialize)]
struct OpenPortInfo {
    port: u16,
    service: &'static str,
    banner: Option<String>,
    response_ms: u128,
}

pub struct PortScannerTool;

impl Tool for PortScannerTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "port_scanner".into(),
            description: "轻量级快速异步端口与服务探测器。基于 Tokio 并发扫描开放 TCP 端口并提取 Banner，避免调用外部 nmap/masscan 的环境依赖。".into(),
            tags: vec!["security".into(), "recon".into(), "network".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "目标 IP 或域名（如 127.0.0.1 或 example.com）"
                    },
                    "ports": {
                        "type": "string",
                        "description": "端口范围（如 top100, 1-1024, 或 80,443,3306,8080。默认 top100）"
                    },
                    "grab_banner": {
                        "type": "boolean",
                        "description": "是否尝试读取服务的欢迎 Banner（默认 true）"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "每个端口的探测超时毫秒数（默认 800ms）",
                        "minimum": 100
                    },
                    "concurrency": {
                        "type": "integer",
                        "description": "最大并发探测数（默认 100）",
                        "minimum": 1
                    }
                },
                "required": ["target"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let target = input
                .get("target")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("port_scanner 缺少 target 参数".into()))?;

            let ports_spec = input
                .get("ports")
                .and_then(|v| v.as_str())
                .unwrap_or("top100");
            let grab_banner = input
                .get("grab_banner")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let timeout_ms = input
                .get("timeout_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(800);
            let concurrency = input
                .get("concurrency")
                .and_then(|v| v.as_u64())
                .unwrap_or(100) as usize;

            let target_host = target.trim();
            let ports = parse_port_spec(ports_spec);
            let total_ports = ports.len();

            let semaphore = Arc::new(Semaphore::new(concurrency));
            let start_time = Instant::now();

            let mut tasks = Vec::new();
            for port in ports {
                let sem = Arc::clone(&semaphore);
                let host = target_host.to_string();
                tasks.push(tokio::spawn(async move {
                    let _permit = sem.acquire().await.unwrap();
                    scan_single_port(&host, port, grab_banner, Duration::from_millis(timeout_ms))
                        .await
                }));
            }

            let mut open_ports = Vec::new();
            for task in tasks {
                if let Ok(Some(info)) = task.await {
                    open_ports.push(info);
                }
            }

            open_ports.sort_by_key(|p| p.port);
            let duration_ms = start_time.elapsed().as_millis();

            let res = json!({
                "target": target_host,
                "ports_scanned": total_ports,
                "open_ports_count": open_ports.len(),
                "duration_ms": duration_ms,
                "open_ports": open_ports
            });

            Ok(ToolOutput {
                content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                is_error: false,
            })
        })
    }
}

fn parse_port_spec(spec: &str) -> Vec<u16> {
    let trimmed = spec.trim().to_lowercase();
    if trimmed == "top100" || trimmed.is_empty() {
        return TOP_100_PORTS.to_vec();
    }

    let mut ports = Vec::new();
    for part in trimmed.split(',') {
        let p = part.trim();
        if let Some((start_s, end_s)) = p.split_once('-') {
            if let (Ok(s), Ok(e)) = (start_s.trim().parse::<u16>(), end_s.trim().parse::<u16>()) {
                let min = s.min(e);
                let max = s.max(e);
                for port in min..=max {
                    ports.push(port);
                }
            }
        } else if let Ok(single) = p.parse::<u16>() {
            ports.push(single);
        }
    }

    ports.sort_unstable();
    ports.dedup();
    ports
}

async fn scan_single_port(
    host: &str,
    port: u16,
    grab_banner: bool,
    timeout: Duration,
) -> Option<OpenPortInfo> {
    let addr_str = format!("{host}:{port}");
    let start = Instant::now();

    // 尝试建立 TCP 连接
    let connect_future = TcpStream::connect(&addr_str);
    let mut stream = match tokio::time::timeout(timeout, connect_future).await {
        Ok(Ok(s)) => s,
        _ => return None,
    };

    let response_ms = start.elapsed().as_millis();
    let service = guess_service(port);
    let mut banner = None;

    if grab_banner {
        let mut buf = [0u8; 1024];
        // 如果是 HTTP 相关端口，先发一个简单 HEAD 请求诱发响应
        if port == 80 || port == 8080 || port == 8000 || port == 8888 {
            let req = format!("HEAD / HTTP/1.0\r\nHost: {host}\r\n\r\n");
            let _ = stream.write_all(req.as_bytes()).await;
        }

        let read_future = stream.read(&mut buf);
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(500), read_future).await {
            if n > 0 {
                let s = String::from_utf8_lossy(&buf[..n]);
                let first_line = s.lines().next().unwrap_or("").trim();
                if !first_line.is_empty() {
                    banner = Some(first_line.to_string());
                }
            }
        }
    }

    Some(OpenPortInfo {
        port,
        service,
        banner,
        response_ms,
    })
}

fn guess_service(port: u16) -> &'static str {
    match port {
        21 => "FTP",
        22 => "SSH",
        23 => "Telnet",
        25 => "SMTP",
        53 => "DNS",
        80 | 8080 | 8000 | 8888 => "HTTP",
        110 => "POP3",
        135 => "MSRPC",
        139 | 445 => "SMB",
        143 => "IMAP",
        389 => "LDAP",
        443 | 8443 => "HTTPS",
        1433 => "MSSQL",
        1521 => "Oracle",
        3306 => "MySQL",
        3389 => "RDP",
        5432 => "PostgreSQL",
        6379 => "Redis",
        8081 => "HTTP-Alt",
        9200 => "Elasticsearch",
        27017 => "MongoDB",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ports() {
        let ports = parse_port_spec("80, 443, 8080-8082");
        assert_eq!(ports, vec![80, 443, 8080, 8081, 8082]);
    }
}
