//! 内置工具集（P2.2）：read_file / write_file / list_dir / find_file / shell / web_fetch / download_file。
//!
//! P6 将在 cyber-tools 增加安全工具（subfinder/nmap/nuclei…）并实现 `Tool` 注入
//! 统一工具表。本模块仅注册 P2.2 的基础工具。

pub(crate) mod ctf_challenge;
mod custom_tool;
mod delegate_tasks;
mod download_file;
mod find_file;
pub(crate) mod guard;
pub mod inspect_image;
mod list_dir;
mod read_file;
mod save_memory;
mod search_tools;
pub(crate) mod shell;
mod todo;
mod web_fetch;
mod write_file;

mod bg;

use crate::tool::ToolRegistry;

/// 注册全部内置工具到 `reg`。
pub fn register_builtins(reg: &mut ToolRegistry) {
    reg.register(Box::new(read_file::ReadFileTool));
    reg.register(Box::new(write_file::WriteFileTool));
    reg.register(Box::new(list_dir::ListDirTool));
    reg.register(Box::new(find_file::FindFileTool));
    reg.register(Box::new(shell::ShellTool::default()));
    reg.register(Box::new(web_fetch::WebFetchTool));
    reg.register(Box::new(download_file::DownloadFileTool));
    reg.register(Box::new(todo::TodoTool::default()));
    reg.register(Box::new(inspect_image::InspectImageTool));
    reg.register(Box::new(bg::BgShellTool));
    reg.register(Box::new(bg::BgStatusTool));
    reg.register(Box::new(bg::BgKillTool));
}

/// 内置工具名（供 TUI `/tools` 命令展示，避免重复构造 registry）。
pub fn builtin_tool_names() -> &'static [&'static str] {
    &[
        "read_file",
        "write_file",
        "list_dir",
        "find_file",
        "shell",
        "web_fetch",
        "download_file",
        "todo",
        "inspect_image",
        "ctf_challenge",
        "save_memory",
        "bg_shell",
        "bg_status",
        "bg_kill",
    ]
}

pub use bg::{BgKillTool, BgShellTool, BgStatusTool};

pub use ctf_challenge::CtfChallengeTool;
pub use custom_tool::CustomTool;
pub use delegate_tasks::DelegateTasksTool;
pub use inspect_image::InspectImageTool;
pub use save_memory::SaveMemoryTool;
pub use search_tools::SearchToolsTool;
pub use todo::TodoTool;
