//! read_file 工具：读取文本文件内容（流式分页读取，每页 64KB；支持行号与行范围读取）。

use std::fs::File;
use std::future::Future;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::resolve_under_cwd;
use crate::vision::{detect_image_mime, mime_from_extension};

/// 每页最大字节数（64KB）。
const PAGE_BYTES: usize = 64 * 1024;

pub struct ReadFileTool;

impl Tool for ReadFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "read_file".into(),
            description: "读取文本文件内容（相对工作目录或绝对路径）。支持流式分页读取（每页 64KB）及行号/行范围读取（如 line_start/line_end，方便配合 edit_file 精准定位修改）；自动拦截图片与二进制文件并给出工具建议。".into(),
            tags: vec![],
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对工作目录或绝对路径）"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "字节偏移量（默认 0，按字节分页读取）。文件超过一页时返回值会提示总大小和下一页 offset",
                        "minimum": 0
                    },
                    "line_start": {
                        "type": "integer",
                        "description": "起始行号（1-based 闭区间，按行读取）。指定后将按行展示并附带行号，方便与 edit_file 联动",
                        "minimum": 1
                    },
                    "line_end": {
                        "type": "integer",
                        "description": "结束行号（1-based 闭区间，按行读取）。默认读取至 line_start + 100 行或文件末尾",
                        "minimum": 1
                    },
                    "line_numbers": {
                        "type": "boolean",
                        "description": "是否在输出中为每行附带行号（如 '  42 | let x = 1;'，默认 false；若指定了 line_start 则默认为 true）"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path = input
                .get("path")
                .and_then(|v| v.as_str())
                .ok_or_else(|| AgentError::Provider("read_file 缺少 path 参数".into()))?;

            let resolved =
                resolve_under_cwd(Path::new(path), &ctx.cwd).map_err(AgentError::Provider)?;

            let mut file = File::open(&resolved).map_err(|e| {
                AgentError::Provider(format!("打开文件 {} 失败: {e}", resolved.display()))
            })?;

            let total = file
                .metadata()
                .map_err(|e| AgentError::Provider(format!("读取文件元数据失败: {e}")))?
                .len() as usize;

            // 预读取前 512 字节检测文件类型（图片、二进制魔数）
            let mut magic = [0u8; 512];
            let n_magic = file.read(&mut magic).unwrap_or(0);
            let magic_header = &magic[..n_magic];

            // 1. 图片格式检测
            if let (Some(mime), true) = (
                detect_image_mime(magic_header),
                mime_from_extension(&resolved).is_some(),
            ) {
                return Ok(ToolOutput {
                    content: format!(
                        "（检测到该文件为图片格式 [{mime}]，已跳过文本打印。请使用 inspect_image 工具进行视觉多模态分析）"
                    ),
                    is_error: false,
                });
            }

            // 2. 常见二进制非文本文件探测（避免乱码占用上下文）
            if is_binary_content(magic_header) {
                return Ok(ToolOutput {
                    content: format!(
                        "（检测到该文件为二进制非文本文件，总大小 {total} 字节，已跳过文本打印。如需分析二进制结构与安全机制，请使用 binary_inspect 工具）"
                    ),
                    is_error: false,
                });
            }

            // 3. 判断是否使用行模式读取（line_start / line_end / line_numbers）
            let line_start_opt = input
                .get("line_start")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let line_end_opt = input
                .get("line_end")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let line_numbers_flag = input.get("line_numbers").and_then(Value::as_bool);

            let is_line_mode = line_start_opt.is_some()
                || line_end_opt.is_some()
                || line_numbers_flag.unwrap_or(false);

            if is_line_mode {
                file.seek(SeekFrom::Start(0))
                    .map_err(|e| AgentError::Provider(format!("定位文件起始失败: {e}")))?;

                return read_by_lines(
                    file,
                    line_start_opt,
                    line_end_opt,
                    line_numbers_flag,
                    &resolved,
                );
            }

            // 4. 原生流式字节分页模式（按需 Seek + Take 64KB，避免大文件一次性全量加载进内存）
            let offset = input
                .get("offset")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(0);

            // offset 超过文件末尾 → 空内容 + 提示
            if offset >= total {
                return Ok(ToolOutput {
                    content: format!("（offset {offset} 已超过文件末尾，文件总大小 {total} 字节）"),
                    is_error: false,
                });
            }

            file.seek(SeekFrom::Start(offset as u64))
                .map_err(|e| AgentError::Provider(format!("定位文件偏移量失败: {e}")))?;

            let mut buffer = Vec::with_capacity(PAGE_BYTES);
            let bytes_read = (&mut file)
                .take(PAGE_BYTES as u64)
                .read_to_end(&mut buffer)
                .map_err(|e| AgentError::Provider(format!("读取文件内容失败: {e}")))?;

            let end = offset + bytes_read;
            let mut content = String::from_utf8_lossy(&buffer).into_owned();
            let has_more = end < total;

            if has_more {
                content.push_str(&format!(
                    "\n\n[已截断：本页 offset={offset}-{end}，总大小 {total} 字节。读取下一页请传 offset={end}]"
                ));
            } else if offset > 0 {
                content.push_str(&format!(
                    "\n\n[最后一页：offset={offset}-{end}，总大小 {total} 字节]"
                ));
            }

            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

/// 按行读取并格式化输出（带行号与行范围截取）。
fn read_by_lines(
    file: File,
    line_start_opt: Option<usize>,
    line_end_opt: Option<usize>,
    line_numbers_flag: Option<bool>,
    resolved: &Path,
) -> Result<ToolOutput> {
    let reader = BufReader::new(file);
    let start_line = line_start_opt.unwrap_or(1);
    let end_line = line_end_opt.unwrap_or_else(|| {
        if line_start_opt.is_some() {
            start_line.saturating_add(100)
        } else {
            usize::MAX
        }
    });

    let with_numbers =
        line_numbers_flag.unwrap_or(line_start_opt.is_some() || line_end_opt.is_some());

    let mut selected_lines = Vec::new();
    let mut current_line = 0usize;
    let mut actual_start = 0usize;
    let mut actual_end = 0usize;

    for line_res in reader.lines() {
        let line = line_res.map_err(|e| AgentError::Provider(format!("按行解码文件失败: {e}")))?;
        current_line += 1;

        if current_line >= start_line && current_line <= end_line {
            if actual_start == 0 {
                actual_start = current_line;
            }
            actual_end = current_line;

            if with_numbers {
                selected_lines.push(format!("{current_line:>5} | {line}"));
            } else {
                selected_lines.push(line);
            }
        }
    }

    let total_lines = current_line;

    if selected_lines.is_empty() {
        if start_line > total_lines {
            return Ok(ToolOutput {
                content: format!(
                    "（起始行 line_start {start_line} 已超过文件总行数，该文件共 {total_lines} 行）"
                ),
                is_error: false,
            });
        }
        return Ok(ToolOutput {
            content: "（文件为空）".to_string(),
            is_error: false,
        });
    }

    let body = selected_lines.join("\n");
    let content = if line_start_opt.is_some() || line_end_opt.is_some() {
        format!(
            "[{path_display} 第 {actual_start}-{actual_end} 行（共 {total_lines} 行）]:\n{body}",
            path_display = resolved.display()
        )
    } else {
        body
    };

    Ok(ToolOutput {
        content,
        is_error: false,
    })
}

/// 检测开头字节是否属于常见二进制文件格式或含有空字节。
fn is_binary_content(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }

    // 1. 常见二进制 Magic Number
    if bytes.starts_with(b"\x7fELF") // Linux ELF
        || bytes.starts_with(b"MZ") // Windows PE/EXE/DLL
        || bytes.starts_with(b"PK\x03\x04") // ZIP / JAR / APK
        || bytes.starts_with(b"\x1f\x8b") // GZIP
        || bytes.starts_with(b"%PDF-") // PDF
        || bytes.starts_with(b"\x00asm") // WASM
        || bytes.starts_with(b"\xfe\xed\xfa") // Mach-O 32
        || bytes.starts_with(b"\xcf\xfa\xed\xfe") // Mach-O 64
        || bytes.starts_with(b"\xca\xfe\xba\xbe") // Java class
        || bytes.starts_with(b"SQLite format 3\0")
    {
        return true;
    }

    // 2. UTF-16 BOM 跳过空字节判定
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        return false;
    }

    // 3. 检测前 512 字节是否存在 Null 字节（0x00）
    bytes.iter().take(512).any(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(dir: &std::path::Path) -> ToolCtx {
        ToolCtx::new(dir.to_path_buf(), vec![], None, Vec::new())
    }

    #[tokio::test]
    async fn reads_existing_file() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_basic");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("hello.txt");
        std::fs::write(&f, "你好世界").unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "hello.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "你好世界");
        assert!(!out.is_error);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_binary_print_for_image_files_and_suggests_inspect_image() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_image");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("test_logo.png");
        let png_bytes = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x54, 0x78, 0x9C,
            0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00,
            0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&f, png_bytes).unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "test_logo.png"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("检测到该文件为图片格式 [image/png]"));
        assert!(out.content.contains("inspect_image"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_binary_elf_and_suggests_binary_inspect() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_elf");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("dummy_elf");
        let elf_bytes = b"\x7fELF\x02\x01\x01\x00\0\0\0\0\0\0\0\0";
        std::fs::write(&f, elf_bytes).unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "dummy_elf"}), &ctx)
            .await
            .unwrap();
        assert!(!out.is_error);
        assert!(out.content.contains("二进制非文本文件"));
        assert!(out.content.contains("binary_inspect"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_file_is_error() {
        let ctx = ctx_with(&std::env::temp_dir());
        let out = ReadFileTool
            .run(json!({"path": "nonexistent_xyz.txt"}), &ctx)
            .await;
        assert!(out.is_err());
    }

    #[tokio::test]
    async fn small_file_no_truncation_hint() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_small");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("small.txt");
        std::fs::write(&f, "short content").unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "small.txt"}), &ctx)
            .await
            .unwrap();
        assert_eq!(out.content, "short content");
        assert!(!out.content.contains("截断"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn large_file_first_page_has_next_hint() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_large");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("large.bin");
        // 写 128KB（两页）
        let data = vec![b'A'; 128 * 1024];
        std::fs::write(&f, &data).unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "large.bin"}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("截断"), "大文件第一页应提示截断");
        assert!(out.content.contains("offset=65536"), "应提示下一页 offset");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn large_file_second_page_reads_rest() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_page2");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("large.bin");
        // 写 128KB（两页）
        let data = vec![b'B'; 128 * 1024];
        std::fs::write(&f, &data).unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "large.bin", "offset": 65536}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("最后一页"), "第二页应提示是最后一页");
        assert!(!out.content.contains("offset=131072"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn offset_beyond_end_returns_empty() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_beyond");
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("tiny.txt");
        std::fs::write(&f, "hi").unwrap();
        let ctx = ctx_with(&dir);
        let out = ReadFileTool
            .run(json!({"path": "tiny.txt", "offset": 9999}), &ctx)
            .await
            .unwrap();
        assert!(out.content.contains("超过文件末尾"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reads_by_line_range_with_line_numbers() {
        let dir = std::env::temp_dir().join("cyber_read_file_test_lines");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("code.rs");
        let code = "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n}\n";
        std::fs::write(&f, code).unwrap();
        let ctx = ctx_with(&dir);

        let out = ReadFileTool
            .run(
                json!({
                    "path": "code.rs",
                    "line_start": 2,
                    "line_end": 4
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!out.is_error);
        assert!(out.content.contains("第 2-4 行"));
        assert!(out.content.contains("    2 |     let a = 1;"));
        assert!(out.content.contains("    3 |     let b = 2;"));
        assert!(out.content.contains("    4 |     let c = 3;"));
        assert!(!out.content.contains("fn main()"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
