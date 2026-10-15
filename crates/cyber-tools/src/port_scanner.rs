//! port_scanner 工具：轻量级快速异步端口与服务探测器。
//!
//! 基于 Tokio 异步任务的高并发 TCP 端口扫描与 Banner 探测：
//! - 扫描前对目标进行一次性 DNS 预解析，消除多端口重复 DNS 查询
//! - 基于 JoinSet 的流式并发控流，防止大范围扫描时协程与内存暴增
//! - 支持指定单端口、逗号分隔端口列表、范围 (如 1-1024) 或预置 "top100"
//! - 支持抓取 Banner 信息提取服务版本指纹
//! - 可控的超时与并发数限制，避免丢包或打挂本地连接池
//! - 纯 Rust 原生实现，无平台依赖，无需 root 权限

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

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
                        "description": "最大并发探测数（默认 100，范围 1-500）",
                        "minimum": 1,
                        "maximum": 500
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
                .unwrap_or(100)
                .clamp(1, 500) as usize;

            let target_host = target.trim();

            // 1. 一次性解析目标主机为 IP 地址（消除对每个端口重复发起 DNS 查询的瓶颈与封禁风险）
            let target_ip: IpAddr = if let Ok(ip) = target_host.parse::<IpAddr>() {
                ip
            } else {
                match tokio::net::lookup_host(format!("{target_host}:80")).await {
                    Ok(mut addrs) => match addrs.next() {
                        Some(sa) => sa.ip(),
                        None => {
                            return Ok(ToolOutput {
                                content: format!("无法解析域名对应的 IP 地址: {target_host}"),
                                is_error: true,
                            });
                        }
                    },
                    Err(e) => {
                        return Ok(ToolOutput {
                            content: format!("DNS 域名解析失败: {target_host} ({e})"),
                            is_error: true,
                        });
                    }
                }
            };

            let ports = parse_port_spec(ports_spec);
            let total_ports = ports.len();

            let semaphore = Arc::new(Semaphore::new(concurrency));
            let start_time = Instant::now();
            let timeout = Duration::from_millis(timeout_ms);

            // 2. 使用 JoinSet 进行有界流式并发扫描（控制常驻 task 数量，防止端口全开时协程堆积）
            let mut join_set = JoinSet::new();
            let mut open_ports = Vec::new();

            for port in ports {
                let sem = Arc::clone(&semaphore);
                let ip = target_ip;

                // 当活动任务超过并发限制的两倍时，消费部分结果，维持常数级内存开销
                while join_set.len() >= concurrency * 2 {
                    if let Some(Ok(Some(info))) = join_set.join_next().await {
                        open_ports.push(info);
                    }
                }

                join_set.spawn(async move {
                    let _permit = match sem.acquire().await {
                        Ok(p) => p,
                        Err(_) => return None,
                    };
                    scan_single_port_ip(ip, port, grab_banner, timeout).await
                });
            }

            // 收集剩余全部任务结果
            while let Some(Ok(Some(info))) = join_set.join_next().await {
                open_ports.push(info);
            }

            open_ports.sort_by_key(|p| p.port);
            let duration_ms = start_time.elapsed().as_millis();

            let res = json!({
                "target": target_host,
                "target_ip": target_ip.to_string(),
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

async fn scan_single_port_ip(
    ip: IpAddr,
    port: u16,
    grab_banner: bool,
    timeout: Duration,
) -> Option<OpenPortInfo> {
    let socket_addr = SocketAddr::new(ip, port);
    let start = Instant::now();

    // 直接对 IP 地址发起 TCP 连接（0 DNS 解析延迟）
    let connect_future = TcpStream::connect(&socket_addr);
    let mut stream = match tokio::time::timeout(timeout, connect_future).await {
        Ok(Ok(s)) => s,
        _ => return None,
    };

    let response_ms = start.elapsed().as_millis();
    let service = guess_service(port);
    let mut banner = None;

    if grab_banner {
        let mut buf = [0u8; 1024];
        // 针对常见 HTTP 服务发送轻量探测头激发响应
        let probe = match port {
            80 | 8080 | 8000 | 8888 | 3000 | 5000 => {
                Some(b"HEAD / HTTP/1.0\r\nHost: target\r\n\r\n".as_slice())
            }
            443 | 8443 => None, // TLS 端口明文发送会重置连接，跳过明文探针
            _ => None,
        };

        if let Some(p) = probe {
            let _ = tokio::time::timeout(Duration::from_millis(300), stream.write_all(p)).await;
        }

        let read_future = stream.read(&mut buf);
        if let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(500), read_future).await {
            if n > 0 {
                let s = String::from_utf8_lossy(&buf[..n])
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if !s.is_empty() {
                    banner = Some(s);
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
        22 | 2222 => "SSH",
        23 => "Telnet",
        25 | 465 | 587 => "SMTP",
        53 => "DNS",
        80 | 81 | 8080 | 8000 | 8888 | 3000 | 5000 => "HTTP",
        110 | 995 => "POP3",
        135 | 139 | 445 => "SMB/RPC",
        143 | 993 => "IMAP",
        389 | 636 => "LDAP",
        443 | 8443 => "HTTPS",
        1080 => "SOCKS",
        1433 => "MSSQL",
        1521 => "Oracle",
        2049 => "NFS",
        2181 => "ZooKeeper",
        3306 => "MySQL",
        3389 => "RDP",
        5432 => "PostgreSQL",
        5672 => "RabbitMQ",
        5900 => "VNC",
        6379 => "Redis",
        7001 => "WebLogic",
        8081 | 8088 | 9000 => "HTTP-Alt",
        9200 | 9300 => "Elasticsearch",
        11211 => "Memcached",
        27017 | 27018 => "MongoDB",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_port_spec() {
        assert_eq!(parse_port_spec("80,443"), vec![80, 443]);
        assert_eq!(parse_port_spec("80-83"), vec![80, 81, 82, 83]);
        assert_eq!(parse_port_spec("top100").len(), TOP_100_PORTS.len());
        // 去重和排序
        assert_eq!(parse_port_spec("80,80,22"), vec![22, 80]);
    }
}
