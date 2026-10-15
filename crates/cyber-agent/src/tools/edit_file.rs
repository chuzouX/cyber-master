//! edit_file 工具：对已有文本文件进行精准局部修改或替换（避免全量重写大文件）。
//!
//! 支持两种修改模式：
//! 1. 锚点字符串替换（推荐）：提供待替换的唯一字符串 `old_str` 与新内容 `new_str`。
//! 2. 行范围替换：提供 `line_start` 与 `line_end`（1-based 行号）替换为 `new_str`。
//!
//! 安全措施：
//! - 写路径限制在工作目录内，禁止路径跳跃逃逸。
//! - 仅允许编辑已存在的文件（不存在时提示使用 `write_file` 创建）。
//! - 锚点模式默认强制要求 `old_str` 唯一匹配，避免误伤其它相同代码。

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::{check_write_path, resolve_under_cwd};

pub struct EditFileTool;

impl Tool for EditFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "edit_file".into(),
            description: "对已有文本文件进行精准局部修改或替换（避免全量重写大文件）。支持两种模式：1. 锚点字符串替换：提供文件中唯一出现的 old_str 与替换内容 new_str（推荐，最稳定）；2. 行范围替换：指定 line_start 与 line_end（1-based 闭区间）替换为 new_str。禁止路径逃逸出工作目录。".into(),
            tags: vec![],
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径（相对工作目录或绝对路径）"
                    },
                    "old_str": {
                        "type": "string",
                        "description": "待替换的原始内容片段。必须在文件中唯一出现以确保精准定位；若不唯一请包含更多前置/后置上下文代码行"
                    },
                    "new_str": {
                        "type": "string",
                        "description": "替换后的新内容片段（删除指定代码片段时可传空字符串）"
                    },
                    "line_start": {
                        "type": "integer",
                        "description": "要替换的起始行号（1-based，闭区间）。若未指定 old_str，将按行号范围替换",
                        "minimum": 1
                    },
                    "line_end": {
                        "type": "integer",
                        "description": "要替换的结束行号（1-based，闭区间）。默认为 line_start",
                        "minimum": 1
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "当使用 old_str 替换时，是否替换文件中出现的所有匹配项（默认 false，要求唯一匹配）"
                    }
                },
                "required": ["path", "new_str"]
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = input
                .get("path")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| AgentError::Provider("edit_file 工具缺少 path 参数".into()))?;

            let new_str = input
                .get("new_str")
                .and_then(Value::as_str)
                .ok_or_else(|| AgentError::Provider("edit_file 工具缺少 new_str 参数".into()))?;

            let path_obj = Path::new(path_str);
            check_write_path(path_obj, ctx).map_err(AgentError::Provider)?;
            let resolved = resolve_under_cwd(path_obj, &ctx.cwd).map_err(AgentError::Provider)?;

            if !resolved.exists() {
                return Err(AgentError::Provider(format!(
                    "文件不存在: {path_str}。如果是创建新文件，请使用 write_file 工具"
                )));
            }

            if resolved.is_dir() {
                return Err(AgentError::Provider(format!(
                    "路径指向目录而非文件: {path_str}"
                )));
            }

            let raw_content = std::fs::read_to_string(&resolved).map_err(|e| {
                AgentError::Provider(format!("读取文件 {} 失败: {e}", resolved.display()))
            })?;

            let old_str_opt = input.get("old_str").and_then(Value::as_str);
            let line_start_opt = input.get("line_start").and_then(Value::as_u64);

            let (updated_content, summary) = if let Some(old_str) = old_str_opt {
                if old_str.is_empty() {
                    return Err(AgentError::Provider(
                        "edit_file 工具传入了空的 old_str。如需在文件特定行插入或替换，请指定非空 old_str 或使用 line_start".into(),
                    ));
                }

                let replace_all = input
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                apply_anchor_replacement(&raw_content, old_str, new_str, replace_all, path_str)?
            } else if let Some(line_start) = line_start_opt {
                let line_end = input
                    .get("line_end")
                    .and_then(Value::as_u64)
                    .unwrap_or(line_start);

                apply_line_range_replacement(
                    &raw_content,
                    line_start as usize,
                    line_end as usize,
                    new_str,
                    path_str,
                )?
            } else {
                return Err(AgentError::Provider(
                    "edit_file 工具需要修改指令：请提供 old_str 进行锚点替换，或提供 line_start 进行行范围替换".into(),
                ));
            };

            std::fs::write(&resolved, &updated_content).map_err(|e| {
                AgentError::Provider(format!("写入文件 {} 失败: {e}", resolved.display()))
            })?;

            Ok(ToolOutput {
                content: summary,
                is_error: false,
            })
        })
    }
}

/// 格式化修改前后的差异片段摘要。
fn format_diff(old: &str, new: &str) -> String {
    let mut out = String::new();
    let old_trimmed = old.trim_end_matches(['\r', '\n']);
    let new_trimmed = new.trim_end_matches(['\r', '\n']);

    for line in old_trimmed.lines() {
        out.push_str(&format!("- {line}\n"));
    }
    for line in new_trimmed.lines() {
        out.push_str(&format!("+ {line}\n"));
    }
    out
}

/// 计算指定字节偏移量对应的 1-based 行号。
fn find_line_number(content: &str, byte_offset: usize) -> usize {
    let clamped = byte_offset.min(content.len());
    content[..clamped].chars().filter(|&c| c == '\n').count() + 1
}

/// 执行锚点字符串替换，支持行尾换行符自动兼容与唯一定位校验。
fn apply_anchor_replacement(
    content: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
    path_display: &str,
) -> Result<(String, String)> {
    // 1. 直接精确匹配
    if content.contains(old_str) {
        return execute_str_replace(content, old_str, new_str, replace_all, path_display);
    }

    // 2. 跨平台换行符自动自适应匹配（CRLF vs LF）
    if content.contains("\r\n") && !old_str.contains("\r\n") && old_str.contains('\n') {
        let normalized_old = old_str.replace('\n', "\r\n");
        let normalized_new = new_str.replace('\n', "\r\n");
        if content.contains(&normalized_old) {
            return execute_str_replace(
                content,
                &normalized_old,
                &normalized_new,
                replace_all,
                path_display,
            );
        }
    } else if !content.contains("\r\n") && old_str.contains("\r\n") {
        let normalized_old = old_str.replace("\r\n", "\n");
        let normalized_new = new_str.replace("\r\n", "\n");
        if content.contains(&normalized_old) {
            return execute_str_replace(
                content,
                &normalized_old,
                &normalized_new,
                replace_all,
                path_display,
            );
        }
    }

    Err(AgentError::Provider(format!(
        "未在文件 {path_display} 中找到匹配的 old_str。请先使用 read_file 检查文件当前内容与空格，确保提供足够的上下文行"
    )))
}

/// 执行字符串替换并生成差异摘要。
fn execute_str_replace(
    content: &str,
    target_old: &str,
    target_new: &str,
    replace_all: bool,
    path_display: &str,
) -> Result<(String, String)> {
    let matches: Vec<_> = content.match_indices(target_old).collect();
    let count = matches.len();

    if count == 0 {
        return Err(AgentError::Provider(format!(
            "未在文件 {path_display} 中找到匹配的 old_str"
        )));
    }

    if count > 1 && !replace_all {
        let line_numbers: Vec<String> = matches
            .iter()
            .take(5)
            .map(|(offset, _)| find_line_number(content, *offset).to_string())
            .collect();
        let lines_hint = line_numbers.join(", ");
        return Err(AgentError::Provider(format!(
            "指定的 old_str 在文件 {path_display} 中匹配到 {count} 处（出现行号如：第 {lines_hint} 行...），定位不唯一。请在 old_str 中包含更多前置/后置代码行作为上下文以唯一锁定修改位置，或者设置 replace_all=true。"
        )));
    }

    let first_match_offset = matches[0].0;
    let start_line = find_line_number(content, first_match_offset);

    let updated_content = if replace_all {
        content.replace(target_old, target_new)
    } else {
        content.replacen(target_old, target_new, 1)
    };

    let diff = format_diff(target_old, target_new);
    let summary = if replace_all {
        format!("已成功修改 {path_display}（共替换 {count} 处匹配）：\n\n{diff}")
    } else {
        format!("已成功修改 {path_display}（在第 {start_line} 行替换）：\n\n{diff}")
    };

    Ok((updated_content, summary))
}

/// 执行行号范围替换（1-based 闭区间）。
fn apply_line_range_replacement(
    content: &str,
    line_start: usize,
    line_end: usize,
    new_str: &str,
    path_display: &str,
) -> Result<(String, String)> {
    let line_ending = if content.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };

    let lines: Vec<&str> = if line_ending == "\r\n" {
        content.split("\r\n").collect()
    } else {
        content.split('\n').collect()
    };

    let has_trailing_newline = content.ends_with(line_ending);
    let actual_lines = if has_trailing_newline && lines.last() == Some(&"") {
        &lines[..lines.len() - 1]
    } else {
        &lines[..]
    };

    let total_lines = actual_lines.len();

    if line_start < 1 || line_start > total_lines {
        return Err(AgentError::Provider(format!(
            "起始行 line_start ({line_start}) 超出文件总行数 (共 {total_lines} 行)"
        )));
    }

    if line_end < line_start || line_end > total_lines {
        return Err(AgentError::Provider(format!(
            "结束行 line_end ({line_end}) 无效，应满足 {line_start} <= line_end <= {total_lines}"
        )));
    }

    let start_idx = line_start - 1;
    let end_idx = line_end - 1;

    let removed_lines = &actual_lines[start_idx..=end_idx];
    let old_snippet = removed_lines.join(line_ending);

    let mut new_lines = Vec::new();
    new_lines.extend_from_slice(&actual_lines[..start_idx]);
    if !new_str.is_empty() {
        for l in new_str.split(line_ending) {
            new_lines.push(l);
        }
    }
    new_lines.extend_from_slice(&actual_lines[end_idx + 1..]);

    let mut updated_content = new_lines.join(line_ending);
    if has_trailing_newline {
        updated_content.push_str(line_ending);
    }

    let diff = format_diff(&old_snippet, new_str);
    let summary = if line_start == line_end {
        format!("已成功修改 {path_display}（替换第 {line_start} 行）：\n\n{diff}")
    } else {
        format!("已成功修改 {path_display}（替换第 {line_start}-{line_end} 行）：\n\n{diff}")
    };

    Ok((updated_content, summary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn setup_test_file(filename: &str, content: &str) -> (PathBuf, ToolCtx) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(1);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("cyber_edit_test_{}_{id}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file_path = dir.join(filename);
        std::fs::write(&file_path, content).unwrap();
        let ctx = ToolCtx::new(dir, vec![], None, Vec::new());
        (file_path, ctx)
    }

    #[tokio::test]
    async fn test_edit_file_tool_old_str_exact_replacement() {
        let initial = "fn main() {\n    let timeout = 30;\n    println!(\"{timeout}\");\n}\n";
        let (file_path, ctx) = setup_test_file("test_main.rs", initial);

        let tool = EditFileTool;
        let res = tool
            .run(
                json!({
                    "path": "test_main.rs",
                    "old_str": "let timeout = 30;",
                    "new_str": "let timeout = 60;"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        assert!(res.content.contains("第 2 行替换"));
        assert!(res.content.contains("- let timeout = 30;"));
        assert!(res.content.contains("+ let timeout = 60;"));

        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(
            updated,
            "fn main() {\n    let timeout = 60;\n    println!(\"{timeout}\");\n}\n"
        );

        let _ = std::fs::remove_dir_all(file_path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_edit_file_tool_old_str_not_found() {
        let initial = "hello world\n";
        let (file_path, ctx) = setup_test_file("not_found.txt", initial);

        let tool = EditFileTool;
        let res = tool
            .run(
                json!({
                    "path": "not_found.txt",
                    "old_str": "goodbye world",
                    "new_str": "replacement"
                }),
                &ctx,
            )
            .await;

        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(err_msg.contains("未在文件"));

        let _ = std::fs::remove_dir_all(file_path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_edit_file_tool_old_str_multiple_matches() {
        let initial = "let a = 1;\nlet b = 2;\nlet a = 1;\n";
        let (file_path, ctx) = setup_test_file("dup.rs", initial);

        let tool = EditFileTool;
        let res = tool
            .run(
                json!({
                    "path": "dup.rs",
                    "old_str": "let a = 1;",
                    "new_str": "let a = 99;"
                }),
                &ctx,
            )
            .await;

        assert!(res.is_err());
        let err_msg = res.unwrap_err().to_string();
        assert!(err_msg.contains("定位不唯一"));
        assert!(err_msg.contains("匹配到 2 处"));

        // 设置 replace_all = true 后允许全部替换
        let ok_res = tool
            .run(
                json!({
                    "path": "dup.rs",
                    "old_str": "let a = 1;",
                    "new_str": "let a = 99;",
                    "replace_all": true
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!ok_res.is_error);
        assert!(ok_res.content.contains("共替换 2 处匹配"));

        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "let a = 99;\nlet b = 2;\nlet a = 99;\n");

        let _ = std::fs::remove_dir_all(file_path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_edit_file_tool_line_range_replacement() {
        let initial = "line 1\nline 2\nline 3\nline 4\n";
        let (file_path, ctx) = setup_test_file("lines.txt", initial);

        let tool = EditFileTool;
        let res = tool
            .run(
                json!({
                    "path": "lines.txt",
                    "line_start": 2,
                    "line_end": 3,
                    "new_str": "new line 2\nnew line 3"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        assert!(res.content.contains("替换第 2-3 行"));

        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "line 1\nnew line 2\nnew line 3\nline 4\n");

        let _ = std::fs::remove_dir_all(file_path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_edit_file_tool_line_out_of_bounds() {
        let initial = "line 1\nline 2\n";
        let (file_path, ctx) = setup_test_file("bounds.txt", initial);

        let tool = EditFileTool;
        let res = tool
            .run(
                json!({
                    "path": "bounds.txt",
                    "line_start": 10,
                    "new_str": "invalid"
                }),
                &ctx,
            )
            .await;

        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("超出文件总行数"));

        let _ = std::fs::remove_dir_all(file_path.parent().unwrap());
    }

    #[tokio::test]
    async fn test_edit_file_tool_rejects_missing_file() {
        let dir = std::env::temp_dir().join(format!("cyber_edit_missing_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let ctx = ToolCtx::new(dir.clone(), vec![], None, Vec::new());

        let tool = EditFileTool;
        let res = tool
            .run(
                json!({
                    "path": "nonexistent.rs",
                    "old_str": "a",
                    "new_str": "b"
                }),
                &ctx,
            )
            .await;

        assert!(res.is_err());
        assert!(res.unwrap_err().to_string().contains("文件不存在"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_file_tool_rejects_traversal() {
        let dir = std::env::temp_dir().join(format!("cyber_edit_escape_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let ctx = ToolCtx::new(dir.clone(), vec![], None, Vec::new());

        let tool = EditFileTool;
        let res = tool
            .run(
                json!({
                    "path": "../../outside.rs",
                    "old_str": "a",
                    "new_str": "b"
                }),
                &ctx,
            )
            .await;

        assert!(res.is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_edit_file_tool_crlf_lf_tolerance() {
        let initial_crlf = "fn foo() {\r\n    println!(\"hi\");\r\n}\r\n";
        let (file_path, ctx) = setup_test_file("crlf.rs", initial_crlf);

        let tool = EditFileTool;
        let res = tool
            .run(
                json!({
                    "path": "crlf.rs",
                    "old_str": "    println!(\"hi\");\n",
                    "new_str": "    println!(\"hello\");\n"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!res.is_error);
        let updated = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(updated, "fn foo() {\r\n    println!(\"hello\");\r\n}\r\n");

        let _ = std::fs::remove_dir_all(file_path.parent().unwrap());
    }
}
