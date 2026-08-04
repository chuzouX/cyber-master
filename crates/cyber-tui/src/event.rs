//! 事件归约：把 crossterm 原始 `KeyEvent` 归约为 `Action`（Settings/Welcome）或
//! `ChatAction`（Chat 输入态）。
//!
//! - P1 用同步阻塞轮询（`event::poll` + `event::read`），`next_action` 仍保留供
//!   单测与未来同步场景使用。
//! - P2 主循环改为 `crossterm::event::EventStream`（异步）；App 在 `select!` 内拿到
//!   `Event::Key(k)` 后，Chat 模式走 `chat_key_to_action`，其余模式走 `key_to_action`。
//!
//! Windows 终端会对同一按键发送 Press / Repeat / Release 三类事件，App 层仅处理
//! `KeyEventKind::Press`，避免动作被触发两次。

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// 非 Chat 模式（Welcome/Settings/Workflow/Dashboard）的 UI 动作集合。
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
    /// 打开设置页（`s` 键；Chat 模式下 `s` 是打字字符，改用 Ctrl+, 见 `ChatAction`）。
    OpenSettings,
    /// 切换日志查看器（Ctrl+L）。
    ToggleLogs,
    /// 新增 Provider（Settings Providers 段 `a` 键）。
    AddProvider,
    /// 编辑当前 Provider（Settings Providers 段 `e` 键）。
    EditProvider,
    /// 删除当前 Provider（Settings Providers 段 `d` 键，双击确认）。
    DeleteProvider,
    /// 未识别按键，UI 可忽略。
    Other,
}

/// Chat 模式动作集合（输入态专用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatAction {
    /// 提交输入框内容（无修饰 Enter）。
    Submit,
    /// 输入换行（Shift/Alt+Enter，或 Ctrl+J 兜底——部分终端不报 Shift+Enter）。
    Newline,
    /// 返回 / 取消流式（Esc；流式期取消，非流式期返回 Welcome/上一模式）。
    Back,
    /// 切换模式（Tab）。
    SwitchMode,
    /// 打开设置（Ctrl+,，编辑器惯例，避免与打字 `s` 冲突）。
    OpenSettings,
    /// 切换日志查看器（Ctrl+L）。
    ToggleLogs,
    /// 退出（Ctrl+C / Ctrl+Q；Chat 内 `q` 是打字字符，不退出）。
    Quit,
    /// 历史区上滚一页（PageUp）。
    ScrollPageUp,
    /// 历史区下滚一页（PageDown）。
    ScrollPageDown,
    /// 历史区上滚一行（Ctrl+Up；Up 本身交 textarea 移光标，斜杠菜单打开时由菜单消费）。
    ScrollLineUp,
    /// 历史区下滚一行（Ctrl+Down）。
    ScrollLineDown,
    /// 输入历史呼出更早（普通 Up；空输入框时呼出，非空时 App 层交 textarea 移光标）。
    HistoryPrev,
    /// 输入历史呼出更新（普通 Down；浏览态呼出，非浏览态交 textarea 移光标）。
    HistoryNext,
    /// 普通输入，交 textarea 处理。
    Input,
}

/// 在 `timeout` 内轮询一次输入；超时返回 `Ok(None)`（同步路径，P1 单测用）。
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

/// 把 `KeyEvent` 归约为非 Chat 模式 `Action`。
pub fn key_to_action(k: KeyEvent) -> Action {
    // Ctrl+C 视作退出（与 q 等价）
    if let KeyCode::Char('c') = k.code {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
    }
    // Ctrl+L 切换日志查看器
    if k.code == KeyCode::Char('l') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::ToggleLogs;
    }
    // `s` 打开设置；Ctrl+S 不映射（保留给未来"保存会话"，见 DESIGN §9.2）。
    if let KeyCode::Char(ch) = k.code {
        if ch == 's' || ch == 'S' {
            if k.modifiers.contains(KeyModifiers::CONTROL) {
                return Action::Other;
            }
            return Action::OpenSettings;
        }
        // Provider 管理快捷键（仅在 Settings Providers 段有意义，其余模式 App 层 no-op）
        if k.modifiers.is_empty() {
            match ch {
                'a' => return Action::AddProvider,
                'e' => return Action::EditProvider,
                'd' => return Action::DeleteProvider,
                _ => {}
            }
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

/// 把 `KeyEvent` 归约为 Chat 模式 `ChatAction`。
///
/// Chat 是文本输入态：所有字母（含 `s`/`q`）均为 `Input`（交 textarea）；
/// 退出走 `Ctrl+C`/`Ctrl+Q`，设置走 `Ctrl+,`。
pub fn chat_key_to_action(k: KeyEvent) -> ChatAction {
    // Ctrl+C 全局退出（保留 P1 出口）
    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return ChatAction::Quit;
    }
    // Ctrl+Q 退出（q 在 Chat 内是打字字符，不直接退出）
    if k.code == KeyCode::Char('q') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return ChatAction::Quit;
    }
    // Ctrl+, 打开设置（编辑器惯例，避免与打字 s 冲突）
    if k.code == KeyCode::Char(',') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return ChatAction::OpenSettings;
    }
    // Ctrl+L 切换日志查看器
    if k.code == KeyCode::Char('l') && k.modifiers.contains(KeyModifiers::CONTROL) {
        return ChatAction::ToggleLogs;
    }
    match k.code {
        KeyCode::Enter => {
            // Shift/Alt+Enter → 换行；无修饰 Enter → 提交
            if k.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) {
                ChatAction::Newline
            } else {
                ChatAction::Submit
            }
        }
        // Ctrl+J 兜底换行（部分终端不报 Shift+Enter）
        KeyCode::Char('j') if k.modifiers.contains(KeyModifiers::CONTROL) => ChatAction::Newline,
        // 历史滚动：PageUp/PageDown 整页，Ctrl+Up/Ctrl+Down 单行。
        // 普通 Up/Down：空输入框时呼出输入历史（HistoryPrev/Next），非空时 App 层交
        // textarea 移光标（斜杠菜单打开时由菜单消费，不走到这里）。
        KeyCode::PageUp => ChatAction::ScrollPageUp,
        KeyCode::PageDown => ChatAction::ScrollPageDown,
        KeyCode::Up if k.modifiers.contains(KeyModifiers::CONTROL) => ChatAction::ScrollLineUp,
        KeyCode::Down if k.modifiers.contains(KeyModifiers::CONTROL) => ChatAction::ScrollLineDown,
        KeyCode::Up => ChatAction::HistoryPrev,
        KeyCode::Down => ChatAction::HistoryNext,
        KeyCode::Esc => ChatAction::Back,
        KeyCode::Tab => ChatAction::SwitchMode,
        _ => ChatAction::Input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEventState;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new_with_kind_and_state(code, mods, KeyEventKind::Press, KeyEventState::NONE)
    }

    // ---- 非 Chat 模式（key_to_action）----

    #[test]
    fn ctrl_c_maps_to_quit() {
        assert_eq!(key_to_action(key(KeyCode::Char('c'), KeyModifiers::CONTROL)), Action::Quit);
    }

    #[test]
    fn plain_c_is_other() {
        assert_eq!(key_to_action(key(KeyCode::Char('c'), KeyModifiers::NONE)), Action::Other);
    }

    #[test]
    fn enter_tab_esc_mapped() {
        assert_eq!(key_to_action(key(KeyCode::Enter, KeyModifiers::NONE)), Action::Enter);
        assert_eq!(key_to_action(key(KeyCode::Tab, KeyModifiers::NONE)), Action::Tab);
        assert_eq!(key_to_action(key(KeyCode::Esc, KeyModifiers::NONE)), Action::Esc);
        assert_eq!(key_to_action(key(KeyCode::Up, KeyModifiers::NONE)), Action::Up);
        assert_eq!(key_to_action(key(KeyCode::Down, KeyModifiers::NONE)), Action::Down);
    }

    #[test]
    fn s_opens_settings_ctrl_s_does_not() {
        assert_eq!(key_to_action(key(KeyCode::Char('s'), KeyModifiers::NONE)), Action::OpenSettings);
        assert_eq!(key_to_action(key(KeyCode::Char('S'), KeyModifiers::SHIFT)), Action::OpenSettings);
        assert_eq!(
            key_to_action(key(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            Action::Other
        );
    }

    #[test]
    fn a_e_d_map_to_provider_actions() {
        assert_eq!(key_to_action(key(KeyCode::Char('a'), KeyModifiers::NONE)), Action::AddProvider);
        assert_eq!(key_to_action(key(KeyCode::Char('e'), KeyModifiers::NONE)), Action::EditProvider);
        assert_eq!(key_to_action(key(KeyCode::Char('d'), KeyModifiers::NONE)), Action::DeleteProvider);
    }

    #[test]
    fn ctrl_a_e_d_are_other() {
        // 带修饰键的 a/e/d 不触发 provider 动作（避免与未来快捷键冲突）
        assert_eq!(key_to_action(key(KeyCode::Char('a'), KeyModifiers::CONTROL)), Action::Other);
        assert_eq!(key_to_action(key(KeyCode::Char('e'), KeyModifiers::CONTROL)), Action::Other);
        assert_eq!(key_to_action(key(KeyCode::Char('d'), KeyModifiers::CONTROL)), Action::Other);
    }

    // ---- Chat 模式（chat_key_to_action）----

    #[test]
    fn chat_plain_enter_submits() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Enter, KeyModifiers::NONE)),
            ChatAction::Submit
        );
    }

    #[test]
    fn chat_shift_enter_newline() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Enter, KeyModifiers::SHIFT)),
            ChatAction::Newline
        );
    }

    #[test]
    fn chat_alt_enter_newline() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Enter, KeyModifiers::ALT)),
            ChatAction::Newline
        );
    }

    #[test]
    fn chat_ctrl_j_newline_fallback() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char('j'), KeyModifiers::CONTROL)),
            ChatAction::Newline
        );
    }

    #[test]
    fn chat_esc_back() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Esc, KeyModifiers::NONE)),
            ChatAction::Back
        );
    }

    #[test]
    fn chat_tab_switch_mode() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Tab, KeyModifiers::NONE)),
            ChatAction::SwitchMode
        );
    }

    #[test]
    fn chat_ctrl_comma_open_settings() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char(','), KeyModifiers::CONTROL)),
            ChatAction::OpenSettings
        );
    }

    #[test]
    fn chat_ctrl_c_and_ctrl_q_quit() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ChatAction::Quit
        );
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char('q'), KeyModifiers::CONTROL)),
            ChatAction::Quit
        );
    }

    #[test]
    fn chat_plain_q_is_input_not_quit() {
        // Chat 内 q 是打字字符，不退出
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            ChatAction::Input
        );
    }

    #[test]
    fn chat_plain_s_is_input_not_settings() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char('s'), KeyModifiers::NONE)),
            ChatAction::Input
        );
    }

    #[test]
    fn chat_letters_are_input() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char('a'), KeyModifiers::NONE)),
            ChatAction::Input
        );
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char('Z'), KeyModifiers::SHIFT)),
            ChatAction::Input
        );
    }

    // ---- 历史滚动键映射 ----

    #[test]
    fn chat_page_up_down_scroll() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::PageUp, KeyModifiers::NONE)),
            ChatAction::ScrollPageUp
        );
        assert_eq!(
            chat_key_to_action(key(KeyCode::PageDown, KeyModifiers::NONE)),
            ChatAction::ScrollPageDown
        );
    }

    #[test]
    fn chat_ctrl_up_down_scroll_line() {
        assert_eq!(
            chat_key_to_action(key(KeyCode::Up, KeyModifiers::CONTROL)),
            ChatAction::ScrollLineUp
        );
        assert_eq!(
            chat_key_to_action(key(KeyCode::Down, KeyModifiers::CONTROL)),
            ChatAction::ScrollLineDown
        );
    }

    #[test]
    fn ctrl_l_toggles_logs() {
        assert_eq!(
            key_to_action(key(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Action::ToggleLogs
        );
        assert_eq!(
            chat_key_to_action(key(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            ChatAction::ToggleLogs
        );
    }

    #[test]
    fn chat_plain_up_down_recall_input_history() {
        // 普通 Up/Down 映射到输入历史呼出（空输入框时呼出，非空时 App 层交 textarea 移光标）
        assert_eq!(
            chat_key_to_action(key(KeyCode::Up, KeyModifiers::NONE)),
            ChatAction::HistoryPrev
        );
        assert_eq!(
            chat_key_to_action(key(KeyCode::Down, KeyModifiers::NONE)),
            ChatAction::HistoryNext
        );
    }
}
