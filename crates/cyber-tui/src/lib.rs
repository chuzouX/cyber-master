//! cyber-tui: ratatui 界面层。
//!
//! P1 范围：TUI 主循环 + Welcome 启动页 + Chat/Workflow/Dashboard 占位页。
//! 后续阶段：
//! - Chat 视图接入真实 agent（P2）
//! - Workflow 节点 DAG 画布（P4）
//! - Dashboard / 日志视图（P5）

pub mod app;
pub mod event;
pub mod theme;
pub mod views;

pub use app::{App, Mode};
pub use theme::Theme;
