//! cyberchef 工具：调用用户自定义的 CyberChef 瑞士军刀引擎。
//!
//! 支持 120+ 种高频安全、编码、解码、加密、解密、压缩、混淆与文本转换算子，
//! 采用 Recipe 链式管道操作（例如 `from_base64 to_hex reverse`），
//! 并内置智能**编码探测（Magic / Detect）**能力，自动分析未知密文字符特征并给出推荐 Recipe。
//! 脚本位置：`~/.cyber/tools/scripts/cyberchef.py`（或环境变量 / 固定路径）。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;

use cyber_agent::{AgentError, Result, Tool, ToolCtx, ToolOutput, ToolSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::Command;

pub struct CyberChefTool {
    script_path: Option<PathBuf>,
}

impl Default for CyberChefTool {
    fn default() -> Self {
        Self::new()
    }
}

impl CyberChefTool {
    pub fn new() -> Self {
        let script = resolve_cyberchef_script();
        Self {
            script_path: script,
        }
    }

    pub fn with_script_path(path: PathBuf) -> Self {
        Self {
            script_path: Some(path),
        }
    }
}

fn resolve_cyberchef_script() -> Option<PathBuf> {
    // 1. 尝试从用户主目录 ~/.cyber/tools/scripts/cyberchef.py
    if let Some(home) = dirs::home_dir() {
        let p = home
            .join(".cyber")
            .join("tools")
            .join("scripts")
            .join("cyberchef.py");
        if p.exists() {
            return Some(p);
        }
    }

    // 2. 检查 Windows 常见路径
    let fixed_win = PathBuf::from(r"C:\Users\chuzo\.cyber\tools\scripts\cyberchef.py");
    if fixed_win.exists() {
        return Some(fixed_win);
    }

    None
}

#[derive(Serialize, Deserialize)]
struct DetectedEncoding {
    format: &'static str,
    confidence: &'static str,
    description: &'static str,
    suggested_recipe: Vec<&'static str>,
}

impl Tool for CyberChefTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "cyberchef".into(),
            description: "CyberChef 命令行瑞士军刀工具。支持 120+ 种编码/解码/哈希/加解密/压缩/数据处理算子，支持 Recipe 链式操作及智能编码探测（detect: true 自动识别密文格式并推荐 Recipe）。".into(),
            tags: vec!["security".into(), "crypto".into(), "encoder".into(), "decoder".into(), "cyberchef".into()],
            parameters: json!({
                "type": "object",
                "properties": {
                    "input": {
                        "type": "string",
                        "description": "待处理的输入数据"
                    },
                    "recipe": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "算子链（按顺序执行）。例如 ['to_base64'], ['from_hex', 'reverse'], ['to_xor:0x42'], ['gzip', 'to_base64']"
                    },
                    "detect": {
                        "type": "boolean",
                        "description": "设为 true 时自动分析输入数据的编码/密文格式，返回可能类型及推荐的解码 Recipe"
                    },
                    "args": {
                        "type": "string",
                        "description": "兼容 CLI 模式的参数序列，如 \"<input> <op1> [op2 ...]\" 或 \"<input> detect\""
                    },
                    "list_operations": {
                        "type": "boolean",
                        "description": "设为 true 时列出所有支持的操作算子与帮助"
                    }
                },
                "required": []
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        _ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let script_path = match &self.script_path {
                Some(p) if p.exists() => p.clone(),
                _ => match resolve_cyberchef_script() {
                    Some(p) => p,
                    None => {
                        return Ok(ToolOutput {
                            content: "未找到 cyberchef.py 脚本文件，预期路径：~/.cyber/tools/scripts/cyberchef.py".into(),
                            is_error: true,
                        });
                    }
                },
            };

            let list_ops = input
                .get("list_operations")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if list_ops {
                let output = Command::new("python")
                    .arg(&script_path)
                    .arg("--help")
                    .output()
                    .await
                    .map_err(|e| AgentError::Provider(format!("执行 python 失败: {e}")))?;

                let text = String::from_utf8_lossy(&output.stdout).to_string();
                return Ok(ToolOutput {
                    content: text,
                    is_error: false,
                });
            }

            let raw_args = input.get("args").and_then(|v| v.as_str());
            let (input_data, recipe_ops) = if let Some(args_str) = raw_args {
                let mut parts = Vec::new();
                let mut cur = String::new();
                let mut in_quotes = false;
                let mut quote_char = '"';
                for c in args_str.chars() {
                    if (c == '"' || c == '\'') && !in_quotes {
                        in_quotes = true;
                        quote_char = c;
                    } else if in_quotes && c == quote_char {
                        in_quotes = false;
                    } else if c.is_whitespace() && !in_quotes {
                        if !cur.is_empty() {
                            parts.push(cur.clone());
                            cur.clear();
                        }
                    } else {
                        cur.push(c);
                    }
                }
                if !cur.is_empty() {
                    parts.push(cur);
                }
                if parts.is_empty() {
                    return Ok(ToolOutput {
                        content: "cyberchef args 参数不能为空".into(),
                        is_error: true,
                    });
                }
                let inp = parts[0].clone();
                let ops = parts[1..].to_vec();
                (inp, ops)
            } else {
                let inp = input
                    .get("input")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut ops: Vec<String> = Vec::new();
                if let Some(arr) = input.get("recipe").and_then(|v| v.as_array()) {
                    for item in arr {
                        if let Some(op_str) = item.as_str() {
                            ops.push(op_str.to_string());
                        }
                    }
                }
                (inp, ops)
            };

            if input_data.is_empty() {
                return Ok(ToolOutput {
                    content: "cyberchef 缺少待处理的数据输入 (input 或 args)".into(),
                    is_error: true,
                });
            }

            let detect = input
                .get("detect")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
                || recipe_ops.iter().any(|op| op == "detect" || op == "magic");

            // 1. 如果请求探测编码格式
            if detect || recipe_ops.is_empty() {
                let detections = detect_encodings(&input_data);
                let res = json!({
                    "action": "detect",
                    "input_length": input_data.len(),
                    "detected_formats": detections,
                    "tip": "可直接选用上述 suggested_recipe 传入 recipe 字段进行解码转换"
                });
                return Ok(ToolOutput {
                    content: serde_json::to_string_pretty(&res).unwrap_or_default(),
                    is_error: false,
                });
            }

            // 构建命令：python cyberchef.py "<input>" op1 op2 ...
            let mut cmd = Command::new("python");
            cmd.arg(&script_path);
            cmd.arg(&input_data);
            for op in &recipe_ops {
                cmd.arg(op);
            }

            cmd.stdout(Stdio::piped());
            cmd.stderr(Stdio::piped());

            let output = cmd
                .output()
                .await
                .map_err(|e| AgentError::Provider(format!("调用 cyberchef 进程失败: {e}")))?;

            let stdout = String::from_utf8_lossy(&output.stdout)
                .trim_end()
                .to_string();
            let stderr = String::from_utf8_lossy(&output.stderr)
                .trim_end()
                .to_string();

            if !output.status.success() && stdout.is_empty() {
                return Ok(ToolOutput {
                    content: format!("CyberChef 执行失败:\n{stderr}"),
                    is_error: true,
                });
            }

            let res = json!({
                "recipe": recipe_ops,
                "input_length": input_data.len(),
                "output_length": stdout.len(),
                "result": stdout,
                "stderr": if stderr.is_empty() { None } else { Some(stderr) }
            });

            Ok(ToolOutput {
                content: serde_json::to_string_pretty(&res).unwrap_or(stdout),
                is_error: false,
            })
        })
    }
}

/// 智能编码与格式探测引擎
fn detect_encodings(data: &str) -> Vec<DetectedEncoding> {
    let mut detected = Vec::new();
    let trimmed = data.trim();
    let len = trimmed.len();

    // 1. URL 百分号编码
    if trimmed.contains('%') {
        let pct_count = trimmed.matches('%').count();
        let confidence = if pct_count > 2 || trimmed.starts_with('%') {
            "High"
        } else {
            "Medium"
        };
        detected.push(DetectedEncoding {
            format: "URL Encoding",
            confidence,
            description: "检测到 URL 百分号转义序列（%XX）",
            suggested_recipe: vec!["from_urlenc"],
        });
    }

    // 2. Base64
    let b64_clean: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if b64_clean.len() >= 4
        && b64_clean.len().is_multiple_of(4)
        && b64_clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '=')
    {
        let confidence = if b64_clean.ends_with('=') {
            "High"
        } else {
            "Medium"
        };
        detected.push(DetectedEncoding {
            format: "Base64",
            confidence,
            description: "标准 Base64 编码文本",
            suggested_recipe: vec!["from_base64"],
        });
    }

    // 3. Base64 URL-Safe (包含 - 或 _)
    if b64_clean.len() >= 4
        && (b64_clean.contains('-') || b64_clean.contains('_'))
        && b64_clean
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
    {
        detected.push(DetectedEncoding {
            format: "Base64 URL-Safe",
            confidence: "High",
            description: "URL 安全的 Base64 编码（常见于 JWT Payload 等）",
            suggested_recipe: vec!["from_base64"],
        });
    }

    // 4. Hex / 十六进制
    let hex_clean: String = trimmed
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':' && *c != '-')
        .collect();
    let is_0x = trimmed.starts_with("0x") || trimmed.starts_with("\\x");
    if (hex_clean.len() >= 2
        && hex_clean.len().is_multiple_of(2)
        && hex_clean.chars().all(|c| c.is_ascii_hexdigit()))
        || is_0x
    {
        let confidence = if is_0x || hex_clean.len() >= 8 {
            "High"
        } else {
            "Medium"
        };
        detected.push(DetectedEncoding {
            format: "Hex / Hexadecimal",
            confidence,
            description: "十六进制字节编码（支持纯文本、0x/\\x 前缀或冒号分隔）",
            suggested_recipe: vec!["from_hex"],
        });
    }

    // 5. HTML 实体编码
    if (trimmed.contains("&amp;")
        || trimmed.contains("&lt;")
        || trimmed.contains("&gt;")
        || trimmed.contains("&#"))
        && trimmed.contains(';')
    {
        detected.push(DetectedEncoding {
            format: "HTML Entity",
            confidence: "High",
            description: "HTML 字符实体编码（常见于 XSS/富文本输出）",
            suggested_recipe: vec!["from_htmlenc"],
        });
    }

    // 6. 二进制串 (Binary Stream)
    let bin_clean: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if bin_clean.len() >= 8
        && bin_clean.len().is_multiple_of(8)
        && bin_clean.chars().all(|c| c == '0' || c == '1')
    {
        detected.push(DetectedEncoding {
            format: "Binary (0101...)",
            confidence: "High",
            description: "8位对齐的二进制数字符流",
            suggested_recipe: vec!["from_binary"],
        });
    }

    // 7. Base32
    let b32_clean: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if b32_clean.len() >= 8
        && b32_clean.len().is_multiple_of(8)
        && b32_clean
            .to_uppercase()
            .chars()
            .all(|c| c.is_ascii_uppercase() || ('2'..='7').contains(&c) || c == '=')
        && !b32_clean
            .chars()
            .any(|c| c == '0' || c == '1' || c == '8' || c == '9')
    {
        detected.push(DetectedEncoding {
            format: "Base32",
            confidence: "Medium",
            description: "RFC 4648 Base32 编码文本",
            suggested_recipe: vec!["from_base32"],
        });
    }

    // 8. 摩斯密码 (Morse Code)
    if trimmed
        .chars()
        .all(|c| c == '.' || c == '-' || c == '/' || c == ' ' || c == '_')
        && len >= 3
    {
        detected.push(DetectedEncoding {
            format: "Morse Code",
            confidence: "Medium",
            description: "点划组成的摩斯电码",
            suggested_recipe: vec!["from_morse"],
        });
    }

    // 9. 八进制 / 十进制数字串
    if trimmed
        .split_whitespace()
        .all(|s| s.chars().all(|c| c.is_ascii_digit()))
        && trimmed.contains(' ')
    {
        detected.push(DetectedEncoding {
            format: "Decimal / Octal Array",
            confidence: "Low",
            description: "空格分隔的纯数字序列（可能是十进制或八进制 ASCII 码）",
            suggested_recipe: vec!["from_decimal"],
        });
    }

    detected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_valid() {
        let tool = CyberChefTool::new();
        let schema = tool.schema();
        assert_eq!(schema.name, "cyberchef");
        assert!(schema.tags.contains(&"cyberchef".to_string()));
    }

    #[test]
    fn test_detect_encodings() {
        let detections = detect_encodings("aGVsbG8gd29ybGQ=");
        assert!(detections.iter().any(|d| d.format == "Base64"));

        let hex_det = detect_encodings("68656c6c6f");
        assert!(hex_det.iter().any(|d| d.format.starts_with("Hex")));

        let url_det = detect_encodings("%3Cscript%3Ealert(1)%3C/script%3E");
        assert!(url_det.iter().any(|d| d.format.starts_with("URL")));
    }
}
