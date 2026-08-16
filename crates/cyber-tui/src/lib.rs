//! cyber-tui: ratatui 界面层。
//!
//! P1 范围：TUI 主循环 + Welcome 启动页 + Chat/Workflow/Dashboard 占位页。
//! P2 范围：Chat 接入真实 agent（Provider 流式 + tokio select! 事件总线）。
//! P2.2 范围：ChatEntry 工具调用渲染（▶/→/✗）+ 斜杠命令（slash 模块）+ generation 计数器
//!           + 对话历史持久化（history 模块）+ 流式重绘优化（行缓存 + 自动滚动）。
//!           + Markdown 渲染（markdown 模块，助手消息与流式 buffer）。
//! 后续阶段：
//! - Workflow 节点 DAG 画布（P4）
//! - Dashboard / 日志视图（P5）

pub mod app;
pub mod bootstrap;
pub mod chat;
pub mod ctf_store;
pub mod event;
pub mod headless;
pub mod history;
pub mod markdown;
pub mod slash;
pub mod theme;
pub mod views;

pub use app::{App, AppPaths, AppRegistries, FetchResult, Mode};
pub use bootstrap::build_registries;
pub use chat::ChatState;
pub use cyber_mcp::McpServersConfig;
pub use headless::{outcome_to_json, run_headless, HeadlessArgs, HeadlessOutcome};
pub use theme::Theme;
