//! TUI 视图：每个模式一个渲染函数（P1 占位级）。
//!
//! P1 不引入 `View` trait，所有视图为纯函数 `fn render(frame, area, theme, ...)`，
//! 状态全部集中在 [`crate::app::App`]，便于 P2+ 演进为 trait + 状态机。

pub mod about;
pub mod chat;
pub mod ctf_edit_form;
pub mod ctf_panel;
pub mod env_form;
pub mod mcp_form;
pub mod model_picker;
pub mod providers;
pub mod sessions;
pub mod settings;
pub mod welcome;
