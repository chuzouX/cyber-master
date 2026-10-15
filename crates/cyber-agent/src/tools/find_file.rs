//! find_file 工具：按文件名/内容特征查找文件（跨平台、UTF-8 安全）。
//!
//! 替代 `shell` + `grep`/`find` 组合：用 Rust 原生遍历，无平台依赖、无编码问题。
//! 支持文件名子串匹配 + 可选文件内容关键词搜索 + 递归深度控制。
//! 默认自动忽略 .git, target, node_modules, dist, __pycache__ 等构建噪目录。

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use serde_json::{json, Value};

use crate::error::{AgentError, Result};
use crate::tool::{Tool, ToolCtx, ToolOutput, ToolSchema};
use crate::tools::guard::resolve_under_cwd;

const DEFAULT_LIMIT: usize = 200;
const MAX_CONTENT_SEARCH_FILE_BYTES: u64 = 5 * 1024 * 1024; // 5MB

/// 默认排除的高噪音、中间编译产物及依赖缓存目录。
const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".cyber",
    ".cache",
    ".idea",
    ".vscode",
    "dist",
    "build",
    "__pycache__",
    ".svn",
    ".hg",
];

/// 常见二进制文件扩展名（内容搜索时直接跳过，避免读取无意义乱码）。
const BINARY_EXTENSIONS: &[&str] = &[
    "exe", "dll", "so", "dylib", "bin", "o", "a", "png", "jpg", "jpeg", "gif", "ico", "webp",
    "pdf", "zip", "tar", "gz", "7z", "bz2", "xz", "iso", "db", "sqlite", "sqlite3", "parquet",
    "wasm", "pyc", "class",
];

pub struct FindFileTool;

impl Tool for FindFileTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "find_file".into(),
            description:
                "在指定目录中按文件名或内容关键词快速检索文件（跨平台、支持递归、默认自动过滤 target/node_modules/.git 等高噪构建目录、防大文件与大量结果撑爆）。"
                    .into(),
            tags: vec![],
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "搜索目录（相对工作目录或绝对路径，默认工作目录）"
                    },
                    "pattern": {
                        "type": "string",
                        "description": "文件名匹配关键词（子串匹配，不区分大小写）。为空则匹配所有文件"
                    },
                    "content": {
                        "type": "string",
                        "description": "可选：文件内容关键词（子串匹配，不区分大小写）。提供时额外搜索文件文本内容"
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "是否递归子目录（默认 true）"
                    },
                    "max_depth": {
                        "type": "integer",
                        "description": "最大递归深度（0=仅当前目录，省略=无限制）"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "最大返回匹配文件条数（默认 200，范围 1-1000）",
                        "minimum": 1,
                        "maximum": 1000
                    },
                    "include_ignored": {
                        "type": "boolean",
                        "description": "是否包含默认被忽略的构建与依赖目录（如 .git, target, node_modules, dist, __pycache__ 等，默认 false）"
                    }
                }
            }),
        }
    }

    fn run<'a>(
        &'a self,
        input: Value,
        ctx: &'a ToolCtx,
    ) -> Pin<Box<dyn Future<Output = Result<ToolOutput>> + Send + 'a>> {
        Box::pin(async move {
            let path_str = input.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let content_kw = input.get("content").and_then(|v| v.as_str());
            let recursive = input
                .get("recursive")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let max_depth = input
                .get("max_depth")
                .and_then(|v| v.as_u64())
                .map(|d| d as usize);
            let limit = input
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|l| (l as usize).clamp(1, 1000))
                .unwrap_or(DEFAULT_LIMIT);
            let include_ignored = input
                .get("include_ignored")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let resolved =
                resolve_under_cwd(Path::new(path_str), &ctx.cwd).map_err(AgentError::Provider)?;

            let pattern_lower = pattern.to_lowercase();
            let content_lower = content_kw.map(|s| s.to_lowercase());

            let mut matches: Vec<String> = Vec::new();
            // 多取 1 条以便判断是否发生了截断
            let scan_cap = limit.saturating_add(1);

            walk_dir(
                &resolved,
                &pattern_lower,
                content_lower.as_deref(),
                recursive,
                max_depth,
                0,
                include_ignored,
                scan_cap,
                &mut matches,
            );

            if matches.is_empty() {
                return Ok(ToolOutput {
                    content: format!("未找到匹配文件（搜索目录: {}）", resolved.display()),
                    is_error: false,
                });
            }

            let is_truncated = matches.len() > limit;
            if is_truncated {
                matches.truncate(limit);
            }

            // 按路径排序，输出相对路径
            matches.sort();
            let display: Vec<String> = matches
                .iter()
                .map(|p| {
                    let path = Path::new(p);
                    // 输出相对路径（相对搜索根目录），更易读
                    if let Ok(rel) = path.strip_prefix(&resolved) {
                        rel.to_string_lossy().replace('\\', "/")
                    } else {
                        path.to_string_lossy().replace('\\', "/")
                    }
                })
                .collect();

            let mut content = format!("找到 {} 个文件:\n{}", display.len(), display.join("\n"));
            if is_truncated {
                content.push_str(&format!(
                    "\n\n[提示：结果已截断显示前 {limit} 项。可通过指定更具体的 pattern、path 或调大 limit 查看更多]"
                ));
            }

            Ok(ToolOutput {
                content,
                is_error: false,
            })
        })
    }
}

/// 判断目录名是否属于默认忽略的构建/依赖高噪目录。
fn is_ignored_dir(name: &str) -> bool {
    let lower = name.to_lowercase();
    DEFAULT_IGNORED_DIRS.iter().any(|&ig| lower == ig)
}

/// 检查文件扩展名是否为二进制格式。
fn is_binary_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|ext| {
            let lower = ext.to_lowercase();
            BINARY_EXTENSIONS.iter().any(|&b| lower == b)
        })
        .unwrap_or(false)
}

/// 递归遍历目录，收集匹配的文件路径。
///
/// - `pattern_lower` 是文件名匹配子串（小写），空串匹配所有
/// - `content_lower` 是内容关键词（小写），None 时不搜索内容
/// - `recursive` 控制是否进入子目录
/// - `max_depth` = Some(n) 时限制深度，None 无限制
/// - `depth` 是当前深度（从 0 开始）
/// - `include_ignored` 控制是否跳过 .git/target/node_modules 等高噪目录
/// - `matches_cap` 为达到上限时提前终止遍历
#[allow(clippy::too_many_arguments)]
fn walk_dir(
    dir: &Path,
    pattern_lower: &str,
    content_lower: Option<&str>,
    recursive: bool,
    max_depth: Option<usize>,
    depth: usize,
    include_ignored: bool,
    matches_cap: usize,
    out: &mut Vec<String>,
) {
    if out.len() >= matches_cap {
        return;
    }

    // 深度检查：max_depth=0 表示仅当前目录，>0 表示限制层数
    if let Some(md) = max_depth {
        if depth > md {
            return;
        }
    }

    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };

    for entry in rd.flatten() {
        if out.len() >= matches_cap {
            break;
        }

        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if file_type.is_file() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            // 文件名匹配
            if !pattern_lower.is_empty() && !name.contains(pattern_lower) {
                continue;
            }
            // 内容匹配（可选）
            if let Some(kw) = content_lower {
                // 跳过已知二进制后缀
                if is_binary_extension(&path) {
                    continue;
                }
                // 检查单文件体积，跳过超过 5MB 的大文件
                if let Ok(meta) = entry.metadata() {
                    if meta.len() > MAX_CONTENT_SEARCH_FILE_BYTES {
                        continue;
                    }
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if !content.to_lowercase().contains(kw) {
                        continue;
                    }
                } else {
                    // 读取失败（非 UTF-8 编码等）→ 跳过
                    continue;
                }
            }
            out.push(path.to_string_lossy().into_owned());
        } else if file_type.is_dir() && recursive {
            let dir_name = entry.file_name().to_string_lossy().to_lowercase();
            // 默认跳过构建中间件与高噪目录
            if !include_ignored && is_ignored_dir(&dir_name) {
                continue;
            }

            let next_depth = depth + 1;
            if let Some(md) = max_depth {
                if next_depth > md {
                    continue;
                }
            }
            walk_dir(
                &path,
                pattern_lower,
                content_lower,
                recursive,
                max_depth,
                next_depth,
                include_ignored,
                matches_cap,
                out,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCtx;
    use std::path::PathBuf;

    fn ctx(cwd: &Path) -> ToolCtx {
        ToolCtx::new(cwd.to_path_buf(), vec![], None, Vec::new())
    }

    fn setup_test_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "cyber_find_file_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // 结构：
        // dir/
        //   nacos_default.md      "Nacos 默认口令"
        //   nacos_sqli.md         "Nacos SQL注入"
        //   other.txt             "hello"
        //   sub/
        //     nacos_rce.md        "Nacos RCE"
        //     deep/
        //       nacos_deep.md     "深层 Nacos"
        std::fs::create_dir_all(dir.join("sub/deep")).unwrap();
        std::fs::write(dir.join("nacos_default.md"), "Nacos 默认口令漏洞").unwrap();
        std::fs::write(dir.join("nacos_sqli.md"), "Nacos SQL注入漏洞").unwrap();
        std::fs::write(dir.join("other.txt"), "hello world").unwrap();
        std::fs::write(dir.join("sub/nacos_rce.md"), "Nacos RCE 远程代码执行").unwrap();
        std::fs::write(dir.join("sub/deep/nacos_deep.md"), "深层 Nacos 文件").unwrap();
        dir
    }

    #[tokio::test]
    async fn finds_by_filename_substring() {
        let dir = setup_test_dir();
        let c = ctx(&dir);
        let out = FindFileTool
            .run(json!({"path": ".", "pattern": "nacos"}), &c)
            .await
            .unwrap();
        assert!(out.content.contains("nacos_default.md"));
        assert!(out.content.contains("nacos_sqli.md"));
        assert!(out.content.contains("nacos_rce.md"));
        assert!(out.content.contains("nacos_deep.md"));
        assert!(!out.content.contains("other.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn finds_by_content_keyword() {
        let dir = setup_test_dir();
        let c = ctx(&dir);
        let out = FindFileTool
            .run(
                json!({"path": ".", "pattern": "", "content": "默认口令"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.content.contains("nacos_default.md"));
        assert!(!out.content.contains("nacos_sqli.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn combines_name_and_content() {
        let dir = setup_test_dir();
        let c = ctx(&dir);
        let out = FindFileTool
            .run(
                json!({"path": ".", "pattern": "nacos", "content": "RCE"}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.content.contains("nacos_rce.md"));
        assert!(!out.content.contains("nacos_default.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn non_recursive_only_current_dir() {
        let dir = setup_test_dir();
        let c = ctx(&dir);
        let out = FindFileTool
            .run(
                json!({"path": ".", "pattern": "nacos", "recursive": false}),
                &c,
            )
            .await
            .unwrap();
        assert!(out.content.contains("nacos_default.md"));
        assert!(out.content.contains("nacos_sqli.md"));
        assert!(
            !out.content.contains("nacos_rce.md"),
            "非递归不应包含子目录文件"
        );
        assert!(!out.content.contains("nacos_deep.md"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn max_depth_limits_recursion() {
        let dir = setup_test_dir();
        let c = ctx(&dir);
        // max_depth=1: 当前目录 + 一层子目录，不含 deep/
        let out = FindFileTool
            .run(json!({"path": ".", "pattern": "nacos", "max_depth": 1}), &c)
            .await
            .unwrap();
        assert!(out.content.contains("nacos_default.md"));
        assert!(
            out.content.contains("nacos_rce.md"),
            "depth=1 应包含 sub/ 下的文件"
        );
        assert!(
            !out.content.contains("nacos_deep.md"),
            "depth=1 不应包含 sub/deep/ 下的文件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_matches_returns_message() {
        let dir = setup_test_dir();
        let c = ctx(&dir);
        let out = FindFileTool
            .run(json!({"path": ".", "pattern": "nonexistent_xyz"}), &c)
            .await
            .unwrap();
        assert!(out.content.contains("未找到"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn case_insensitive_match() {
        let dir = setup_test_dir();
        let c = ctx(&dir);
        let out = FindFileTool
            .run(json!({"path": ".", "pattern": "NACOS"}), &c)
            .await
            .unwrap();
        assert!(out.content.contains("nacos_default.md"), "应不区分大小写");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn ignores_target_and_git_by_default() {
        let dir = std::env::temp_dir().join(format!(
            "cyber_find_ignore_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("target/debug")).unwrap();

        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join(".git/config"), "core").unwrap();
        std::fs::write(dir.join("target/debug/build.rs"), "cargo").unwrap();

        let c = ctx(&dir);

        // 默认过滤高噪目录
        let out_default = FindFileTool.run(json!({"path": "."}), &c).await.unwrap();
        assert!(out_default.content.contains("src/main.rs"));
        assert!(!out_default.content.contains(".git"));
        assert!(!out_default.content.contains("target"));

        // 显式开启 include_ignored
        let out_included = FindFileTool
            .run(json!({"path": ".", "include_ignored": true}), &c)
            .await
            .unwrap();
        assert!(out_included.content.contains("src/main.rs"));
        assert!(out_included.content.contains(".git/config"));
        assert!(out_included.content.contains("target/debug/build.rs"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn limits_output_count_with_truncation_notice() {
        let dir = std::env::temp_dir().join(format!(
            "cyber_find_limit_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        for i in 1..=10 {
            std::fs::write(dir.join(format!("file_{i:02}.txt")), "content").unwrap();
        }

        let c = ctx(&dir);
        let out = FindFileTool
            .run(json!({"path": ".", "pattern": "file_", "limit": 4}), &c)
            .await
            .unwrap();

        assert!(out.content.contains("找到 4 个文件:"));
        assert!(out.content.contains("结果已截断显示前 4 项"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
