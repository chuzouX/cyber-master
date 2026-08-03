//! 事件轮询：把 crossterm 原始事件归约为少量 `Action`。
//!
//! P1 用同步阻塞轮询（`event::poll` + `event::read`）。
//! P2+ 接入 agent 流式后再升级为 tokio 事件总线（见 DESIGN §10.2）。

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// UI 关心的动作集合。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Quit,
    Up,
    Down,
    Left,
    Right,
    Enter,
    Tab,
    Esc,
    /// 打开设置页（`s` 键；P2 Chat 文本输入态需在 handle_action 拦截）。
    OpenSettings,
    /// 未识别按键，UI 可忽略。
    Other,
}

/// 在 `timeout` 内轮询一次输入；超时返回 `Ok(None)`。
///
/// Windows 终端会对同一按键发送 Press / Repeat / Release 三类事件，
/// 仅处理 `KeyEventKind::Press`，避免 `q` 退出等被触发两次。
pub fn next_action(timeout: Duration) -> io::Result<Option<Action>> {
    if !event::poll(timeout)? {
        return Ok(None);
    }
    match event::read()? {
        Event::Key(k) if k.kind == KeyEventKind::Press => Ok(Some(key_to_action(k))),
        _ => Ok(None),
    }
}

fn key_to_action(k: KeyEvent) -> Action {
    // Ctrl+C 视作退出（与 q 等价）
    if let KeyCode::Char('c') = k.code {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
    }
    // `s` 打开设置；Ctrl+S 不映射（保留给未来"保存会话"，见 DESIGN §9.2）。
    if let KeyCode::Char(ch) = k.code {
        if ch == 's' || ch == 'S' {
            if k.modifiers.contains(KeyModifiers::CONTROL) {
                return Action::Other;
            }
            return Action::OpenSettings;
        }
    }
    match k.code {
        KeyCode::Char('q') => Action::Quit,
        KeyCode::Up => Action::Up,
        KeyCode::Down => Action::Down,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Enter => Action::Enter,
        KeyCode::Tab => Action::Tab,
        KeyCode::Esc => Action::Esc,
        _ => Action::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_maps_to_quit() {
        let k = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        assert_eq!(key_to_action(k), Action::Quit);
    }

    #[test]
    fn plain_c_is_other() {
        let k = KeyEvent::new_with_kind_and_state(
            KeyCode::Char('c'),
            KeyModifiers::NONE,
            KeyEventKind::Press,
            crossterm::event::KeyEventState::NONE,
        );
        assert_eq!(key_to_action(k), Action::Other);
    }

    #[test]
    fn enter_tab_esc_mapped() {
        let mk = |code: KeyCode| {
            KeyEvent::new_with_kind_and_state(
                code,
                KeyModifiers::NONE,
                KeyEventKind::Press,
                crossterm::event::KeyEventState::NONE,
            )
        };
        assert_eq!(key_to_action(mk(KeyCode::Enter)), Action::Enter);
        assert_eq!(key_to_action(mk(KeyCode::Tab)), Action::Tab);
        assert_eq!(key_to_action(mk(KeyCode::Esc)), Action::Esc);
        assert_eq!(key_to_action(mk(KeyCode::Up)), Action::Up);
        assert_eq!(key_to_action(mk(KeyCode::Down)), Action::Down);
        assert_eq!(key_to_action(mk(KeyCode::Left)), Action::Left);
        assert_eq!(key_to_action(mk(KeyCode::Right)), Action::Right);
    }

    #[test]
    fn s_opens_settings_ctrl_s_does_not() {
        let mk = |code: KeyCode, mods: KeyModifiers| {
            KeyEvent::new_with_kind_and_state(
                code,
                mods,
                KeyEventKind::Press,
                crossterm::event::KeyEventState::NONE,
            )
        };
        assert_eq!(key_to_action(mk(KeyCode::Char('s'), KeyModifiers::NONE)), Action::OpenSettings);
        assert_eq!(key_to_action(mk(KeyCode::Char('S'), KeyModifiers::SHIFT)), Action::OpenSettings);
        // Ctrl+S 保留给未来"保存会话"，不应打开设置
        assert_eq!(
            key_to_action(mk(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::Other
        );
    }
}
