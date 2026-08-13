//! 内置工具集（P2.2）：read_file / write_file / list_dir / find_file / shell / web_fetch / download_file。
//!
//! P6 将在 cyber-tools 增加安全工具（subfinder/nmap/nuclei…）并实现 `Tool` 注入
//! 统一工具表。本模块仅注册 P2.2 的基础工具。

mod ctf_challenge;
mod download_file;
mod find_file;
mod guard;
mod list_dir;
mod read_file;
mod save_memory;
mod shell;
mod web_fetch;
mod write_file;

use crate::tool::ToolRegistry;

/// 注册全部内置工具到 `reg`。
pub fn register_builtins(reg: &mut ToolRegistry) {
    reg.register(Box::new(read_file::ReadFileTool));
    reg.register(Box::new(write_file::WriteFileTool));
    reg.register(Box::new(list_dir::ListDirTool));
    reg.register(Box::new(find_file::FindFileTool));
    reg.register(Box::new(shell::ShellTool));
    reg.register(Box::new(web_fetch::WebFetchTool));
    reg.register(Box::new(download_file::DownloadFileTool));
}

/// 内置工具名（供 TUI `/tools` 命令展示，避免重复构造 registry）。
pub fn builtin_tool_names() -> &'static [&'static str] {
    &["read_file", "write_file", "list_dir", "find_file", "shell", "web_fetch", "download_file", "ctf_challenge", "save_memory"]
}

pub use ctf_challenge::CtfChallengeTool;
pub use save_memory::SaveMemoryTool;
