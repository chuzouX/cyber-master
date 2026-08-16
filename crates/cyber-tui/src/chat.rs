//! Chat 模式状态：对话条目历史 + tui-textarea 输入框 + 流式缓冲。
//!
//! `ChatState` 持有 Chat 模式全部可变状态。`submit()` 把输入框文本转为一条 user
//! 条目并切到 streaming 态（清空输入框、置 `streaming=true`、返回待发送文本）。
//! 流式 token 由 App 的 `handle_agent_event` 写入 `streaming_buffer`；
//! ToolCall/ToolResult 事件先 flush buffer 为 assistant 条目再 push；
//! Done/Error 时 `finalize_stream` 把残留 buffer 定稿为 assistant 条目并退出 streaming 态。
//!
//! 跨轮历史（`history()`）只取 User/Assistant 文本，剥离 ToolCall/ToolResult
//! （工具链仅在单次 spawn 内部维护，避免历史膨胀 + provider 翻译复杂度）。

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use cyber_agent::{Message, ToolCall};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};
use tui_textarea::TextArea;
use unicode_width::UnicodeWidthChar;

use crate::slash::CommandSpec;
use crate::theme::Theme;

/// 粘贴检测器：基于按键时间间隔区分人打字与粘贴。
///
/// 人打字间隔 > 50ms，粘贴逐字符间隔 < 30ms（Windows 终端约 5-20ms，Unix < 2ms）。
/// 用 30ms 阈值区分：
/// - 快速连续的无修饰键 Char/Enter → 缓冲（粘贴中）
/// - 间隔 ≥ 30ms → 正常处理
///
/// 缓冲期间 Enter 被转为 `\n` 存入 buffer，不触发 Submit。
/// buffer 在以下情况 flush（整块插入 textarea）：
/// 1. 非快速键到达（`FlushThenProcess`）
/// 2. 特殊键到达（Ctrl+C/Esc/方向键等，`FlushThenProcess`）
/// 3. tick 兜底：距上次按键 > 50ms（`flush_if_stale`）
pub struct PasteDetector {
    buffer: String,
    last_key_time: Option<Instant>,
}

/// `PasteDetector::observe` 的返回值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyDisposition {
    /// 正常处理当前键（非粘贴、或 buffer 已 flush 后继续处理）。
    Process,
    /// 当前键已存入 buffer，不要处理。
    Buffer,
    /// 先 flush buffer 整块插入，再正常处理当前键。
    FlushThenProcess,
}

/// 快速按键间隔阈值：< 30ms 视为粘贴。
///
/// 2ms 仅适用 Unix（bracketed paste 不可用时粘贴近瞬时）；Windows 终端粘贴
/// 字符间隔约 5-20ms，2ms 阈值会导致粘贴的 Enter 被当作普通按键触发 Submit。
/// 30ms 仍远低于人打字间隔（> 50ms），不会误缓冲正常输入。
const RAPID_THRESHOLD: Duration = Duration::from_millis(30);
/// flush 兜底超时：距上次按键 > 50ms 时 flush（tick 调用）。
const FLUSH_TIMEOUT: Duration = Duration::from_millis(50);

impl PasteDetector {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
            last_key_time: None,
        }
    }

    /// 观察一个按键，返回处理方式。
    ///
    /// 只缓冲**无修饰键的 Char 和 Enter**；带修饰键（Ctrl+C 等）或特殊键
    /// （Esc/方向键等）会先 flush 再正常处理。
    pub fn observe(&mut self, k: KeyEvent) -> KeyDisposition {
        let now = Instant::now();
        let is_rapid = self
            .last_key_time
            .map(|t| now.duration_since(t) < RAPID_THRESHOLD)
            .unwrap_or(false);

        // 只缓冲无修饰键的 Char 和 Enter
        let bufferable = k.modifiers == KeyModifiers::NONE
            && matches!(k.code, KeyCode::Char(_) | KeyCode::Enter);

        let disposition = if bufferable && is_rapid {
            let c = match k.code {
                KeyCode::Char(c) => c,
                KeyCode::Enter => '\n',
                _ => unreachable!(),
            };
            self.buffer.push(c);
            KeyDisposition::Buffer
        } else if !self.buffer.is_empty() {
            // 非快速键或特殊键，但 buffer 有内容 → 先 flush 再处理
            KeyDisposition::FlushThenProcess
        } else {
            KeyDisposition::Process
        };

        self.last_key_time = Some(now);
        disposition
    }

    /// 取出 buffer 内容（整块插入 textarea）。
    pub fn flush(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            None
        } else {
            Some(std::mem::take(&mut self.buffer))
        }
    }

    /// tick 兜底：buffer 非空且距上次按键 > 50ms 时 flush。
    pub fn flush_if_stale(&mut self) -> Option<String> {
        if self.buffer.is_empty() {
            return None;
        }
        if let Some(t) = self.last_key_time {
            if Instant::now().duration_since(t) >= FLUSH_TIMEOUT {
                return Some(std::mem::take(&mut self.buffer));
            }
        }
        None
    }
}

impl Default for PasteDetector {
    fn default() -> Self {
        Self::new()
    }
}

/// 一条对话条目（user / assistant / 工具调用 / 工具结果 / system）。
///
/// `ToolCall` 带 `pending` 标记：结果未到时显示 `▶`，结果到达后由紧随的 `ToolResult`
/// 条目展示输出。工具链（ToolCall+ToolResult）不入跨轮历史。
///
/// `Serialize/Deserialize` 用于历史持久化（`~/.cyber/history/{cwd_hash}.json`）。
///
/// 用 **adjacently tagged**（`tag` + `content`）而非 internally tagged：后者无法
/// 序列化 newtype 变体（`User(String)` 等，payload 非 map）。adjacent 表示形如
/// `{"kind":"User","data":"你好"}` / `{"kind":"ToolCall","data":{...}}`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data")]
pub enum ChatEntry {
    User(String),
    Assistant(String),
    /// 思考过程（DeepSeek reasoning_content）。默认折叠为最新 3 行，Ctrl+O 展开。
    Thinking(String),
    /// 工具调用（agent 即将执行）。`arguments` 为 JSON 字符串。
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// 工具执行结果（含输出或错误）。
    ToolResult {
        id: String,
        name: String,
        output: String,
        is_error: bool,
    },
    System(String),
}

/// `scroll_y` 的哨兵值：表示"跟随底部"（auto-follow），流式新内容自动滚到底。
/// 用 `usize::MAX` 避免额外 `Option` 字段，且任何真实偏移都远小于此值。
const SCROLL_FOLLOW: usize = usize::MAX;

/// 工具结果回显折叠阈值：输出行数超过此值时默认折叠为最后 N 行，
/// 提示 Ctrl+O 展开（参考 Claude Code 的折叠行为）。
const TOOL_RESULT_FOLD_THRESHOLD: usize = 3;

/// 斜杠命令补全菜单状态。
///
/// 输入以 `/` 开头且不含空格时打开（`update_slash_menu` 每次输入后刷新）；按前缀过滤
/// `COMMANDS`。Up/Down 选择，Enter/Tab 补全命令名 + 空格（`slash_menu_complete`），
/// Esc 关闭。菜单打开时 Up/Down 由菜单消费（`slash_menu_key`），不传给 textarea。
///
/// 二级参数补全：命令名补全后若输入为 `/cmd <partial_param>`，自动切换到 Param 模式，
/// 按 `param_suggestions(cmd)` 过滤参数建议，Tab 补全参数。
#[derive(Debug, Default)]
pub struct SlashMenu {
    /// 是否打开。
    pub open: bool,
    /// 当前选中项索引（在 `filtered` / `params` 中）。
    pub selected: usize,
    /// 前缀过滤后的命令列表（Command 模式使用）。
    pub filtered: Vec<&'static CommandSpec>,
    /// 当前菜单模式。
    pub mode: SlashMenuMode,
    /// 前缀过滤后的参数建议（Param 模式使用）。
    pub params: Vec<&'static str>,
}

/// 菜单模式：命令名补全 or 参数补全。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum SlashMenuMode {
    /// 一级：补全命令名（`/think`）。
    #[default]
    Command,
    /// 二级：补全参数（`/think hi` → `high`）。
    Param,
}

impl SlashMenu {
    pub fn close(&mut self) {
        self.open = false;
        self.filtered.clear();
        self.params.clear();
        self.selected = 0;
        self.mode = SlashMenuMode::Command;
    }
}

/// 输入历史：记录已发送消息，供 ↑/↓ 在**空输入框**时呼出（shell 风格）。
///
/// 不单独持久化——由 chat history 的 `ChatEntry::User` 条目派生（`seed_input_history`
/// 在 App 启动加载历史后调用），新提交经 `record` 追加。跨会话呼出靠下次启动重新 seed。
///
/// 浏览态语义：`browse=None` 表示正在输入新内容；首次 ↑（输入框为空）进入浏览态指向
/// 最新条目，继续 ↑ 往更早、↓ 往更新，↓ 到头清空输入并退出浏览态（回到最新）。
/// 输入框非空时 ↑/↓ 不呼出（交 textarea 移光标，保留多行编辑）。
#[derive(Default)]
pub struct InputHistory {
    /// 已发送文本（oldest → newest），相邻去重。
    entries: Vec<String>,
    /// 当前浏览索引；`None` = 未浏览态。
    browse: Option<usize>,
}

impl InputHistory {
    /// 记录一条已发送文本：trim 后为空跳过，与末条相同则跳过（相邻去重），并退出浏览态。
    fn record(&mut self, text: &str) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        if self.entries.last().map(|s| s.as_str()) == Some(t) {
            self.browse = None;
            return;
        }
        self.entries.push(t.to_string());
        self.browse = None;
    }

    /// 从 `ChatEntry::User` 文本序列填充（App 启动加载历史后调用，跨会话呼出）。
    fn seed(&mut self, user_texts: impl IntoIterator<Item = String>) {
        self.entries.clear();
        self.browse = None;
        for t in user_texts {
            // 复用 record 的去重逻辑，保持一致
            self.record(&t);
        }
    }
}

/// Chat 模式状态。
pub struct ChatState {
    /// 已完成的对话条目（按时间序）。
    pub entries: Vec<ChatEntry>,
    /// 多行输入框。
    pub input: TextArea<'static>,
    /// 是否正在流式生成（true 时禁用提交）。
    pub streaming: bool,
    /// 当前流式响应的累积 buffer；Done 时转为 assistant 条目。
    pub streaming_buffer: String,
    /// 当前流式思考过程的累积 buffer（DeepSeek reasoning_content）；
    /// Done 时转为 Thinking 条目（位于对应 Assistant 条目之前）。
    pub thinking_buffer: String,
    /// 当前工具执行的增量输出累积（shell 逐行 stdout/stderr 经 ToolProgress 推送）。
    /// ToolResult 到达后定稿为 ToolResult 条目并清空。渲染时作为流式 tail 追加在
    /// 对应 ToolCall 条目之后（带 ▌ 光标），实现「边执行边输出」。
    pub streaming_tool_output: String,
    /// 已完成条目的渲染行缓存（不含流式 tail 与空态引导）。
    /// 仅当 `entries.len()` 变化或 `cache_dirty`（如 theme 切换）时重建，
    /// 避免每帧重新 tokenize 全部条目。流式 tail 由 view 每帧追加（量小）。
    cached_history: Vec<Line<'static>>,
    /// 缓存对应的 `entries.len()`，不匹配时触发重建。
    cached_entries_len: usize,
    /// 强制重建标记（theme 切换等 entries.len() 不变但内容样式需刷新的场景）。
    cache_dirty: bool,
    /// 历史区垂直滚动偏移（绝对顶部行号）；`SCROLL_FOLLOW` = 跟随底部。
    /// 由按键处理（`&mut self`）修改；render（`&self`）只读 + 经 `Cell` 回写度量。
    pub scroll_y: usize,
    /// 上一帧渲染的历史总行数（含 wrap 折行）。render 经 `Cell` 回写，按键处理读取
    /// 以计算 PageUp/PageDown 的页大小与 max_scroll。首帧前为 0（按键 no-op 安全）。
    last_total_lines: Cell<usize>,
    /// 上一帧渲染的历史区可见高度。同上。
    last_visible_height: Cell<usize>,
    /// 预折行缓存：把已完成条目 + 流式 tail 按当前可视宽度拆成单行 `Line`，
    /// render 直接取可见窗口切片（O(visible)），避免每帧 `Paragraph::line_count` +
    /// `Wrap` 重算（O(N)）与全量 clone —— 滚动跟手性的关键。
    /// key = (entries.len, streaming_buffer.len, width)；theme 切换经 `invalidate_cache`
    /// 置 `valid=false` 强制重建。render 以 `&self` 经 `RefCell` 内部可变更新。
    wrapped: RefCell<WrappedCache>,
    /// 斜杠命令补全菜单状态。
    pub slash_menu: SlashMenu,
    /// 输入历史（↑/↓ 在空输入框时呼出）。
    pub input_history: InputHistory,
    /// 已展开的工具结果条目索引（按 entries 下标）。
    /// 默认折叠（仅显示最后 `TOOL_RESULT_FOLD_THRESHOLD` 行），Ctrl+O 切换。
    expanded_tool_results: HashSet<usize>,
    /// 上次 `prepare_render` 使用的终端宽度。宽度变化时须重建 `cached_history`，
    /// 因为工具结果折叠阈值基于可视行数（受宽度影响）。
    last_render_width: u16,
    /// 粘贴检测器：基于按键时间间隔区分人打字与粘贴。
    pub paste_detector: PasteDetector,
}

/// `wrapped` 预折行缓存的载体。
#[derive(Default)]
struct WrappedCache {
    /// 已折行的单行 `Line`（entries + tail）。
    lines: Vec<Line<'static>>,
    /// 缓存键：(entries.len, streaming_buffer.len, thinking_buffer.len, streaming_tool_output.len, width)。
    key: (usize, usize, usize, usize, u16),
    /// 是否有效（theme 切换等 entries.len 不变但样式需刷新时置 false）。
    valid: bool,
}

impl ChatState {
    pub fn new() -> Self {
        let mut input = TextArea::default();
        // 占位提示文本（样式在 render 时按当前 theme 应用，因 theme 可被 Settings 实时切换）
        input.set_placeholder_text("输入消息，Enter 发送，Shift+Enter 换行…");
        Self {
            entries: Vec::new(),
            input,
            streaming: false,
            streaming_buffer: String::new(),
            thinking_buffer: String::new(),
            streaming_tool_output: String::new(),
            cached_history: Vec::new(),
            cached_entries_len: 0,
            cache_dirty: true,
            scroll_y: SCROLL_FOLLOW,
            last_total_lines: Cell::new(0),
            last_visible_height: Cell::new(0),
            wrapped: RefCell::new(WrappedCache::default()),
            slash_menu: SlashMenu::default(),
            input_history: InputHistory::default(),
            expanded_tool_results: HashSet::new(),
            last_render_width: 0,
            paste_detector: PasteDetector::new(),
        }
    }

    /// 在 draw 前（`&mut self` 上下文）调用：若 `entries.len()` 变化、缓存被标脏
    ///（如 theme 切换）或终端宽度变化，则重建已完成条目的渲染行缓存。流式 tail 不入
    /// 缓存，由 view 每帧追加。render 以 `&self` 经 `cached_history()` 只读复用，避免
    /// 每帧重建。`width` 为历史区可用宽度，用于工具结果折叠阈值的可视行数计算。
    pub fn prepare_render(&mut self, theme: &Theme, width: u16) {
        if self.cache_dirty
            || self.cached_entries_len != self.entries.len()
            || self.last_render_width != width
        {
            self.cached_history =
                render_entries(&self.entries, theme, &self.expanded_tool_results, width);
            self.cached_entries_len = self.entries.len();
            self.last_render_width = width;
            self.cache_dirty = false;
        }
    }

    /// 标记缓存需要重建（entries.len() 不变但样式需刷新时调用，如 theme 切换）。
    /// 同时使预折行缓存失效（颜色变了但折行数不变，key 不会变 → 须强制 valid=false）。
    pub fn invalidate_cache(&mut self) {
        self.cache_dirty = true;
        self.wrapped.get_mut().valid = false;
    }

    // ── 历史滚动 ───────────────────────────────────────────────────────────
    // `scroll_y` 为绝对顶部行号；`SCROLL_FOLLOW` 表示跟随底部。度量
    //（`last_total_lines`/`last_visible_height`）由 render 每帧经 `Cell` 回写，
    // 按键处理据此计算 max_scroll 与页大小。首帧前度量为 0 → 滚动 no-op（安全）。

    /// 滚动历史区：`delta` 正向下（向底部），负向上。`page` 大小由上一帧可见高度决定。
    /// 到达底部时切回 `SCROLL_FOLLOW`（auto-follow），新流式内容自动跟随。
    pub fn scroll_history(&mut self, delta: i32) {
        let total = self.last_total_lines.get();
        let visible = self.last_visible_height.get();
        let max_scroll = total.saturating_sub(visible);
        let cur = if self.scroll_y == SCROLL_FOLLOW {
            max_scroll
        } else {
            self.scroll_y.min(max_scroll)
        };
        let new = (cur as i32 + delta).clamp(0, max_scroll as i32) as usize;
        // 到底部 → 跟随；否则记录绝对偏移（内容增长时视图钉在原内容，不滑向新内容）
        self.scroll_y = if new >= max_scroll { SCROLL_FOLLOW } else { new };
    }

    /// 跳到最新（底部）并恢复 auto-follow。submit/clear/cancel 后调用。
    pub fn scroll_to_bottom(&mut self) {
        self.scroll_y = SCROLL_FOLLOW;
    }

    /// 是否正在跟随底部（未上滚）。render 据此决定是否显示"已滚动"指示。
    pub fn is_following_bottom(&self) -> bool {
        self.scroll_y == SCROLL_FOLLOW
    }

    /// render 每帧回写度量（总行数 / 可见高度），供下次按键滚动计算。
    pub fn set_scroll_metrics(&self, total: usize, visible: usize) {
        self.last_total_lines.set(total);
        self.last_visible_height.set(visible);
    }

    /// 返回上一帧渲染的历史区可见高度，供按键处理计算 PageUp/PageDown 的页大小。
    pub fn last_visible_height_get(&self) -> usize {
        self.last_visible_height.get()
    }

    /// 返回当前应在 Paragraph::scroll 使用的顶部偏移（已按 max_scroll 钳制）。
    /// `SCROLL_FOLLOW` 解析为 max_scroll（底部）。
    pub fn resolved_scroll_offset(&self, max_scroll: usize) -> usize {
        if self.scroll_y == SCROLL_FOLLOW {
            max_scroll
        } else {
            self.scroll_y.min(max_scroll)
        }
    }

    // ── 预折行缓存 ──────────────────────────────────────────────────────────
    // render（`&self`）每帧调用 `wrapped_lines(theme, width)`：若 key 不变且 valid，
    // 直接复用缓存（滚动时内容未变 → O(1) 命中）；否则重建（折行 O(N)，仅在内容/宽度/
    // theme 变化时）。返回 `Ref<Vec<Line>>` 供 render 切可见窗口。

    /// 返回按 `width` 预折行的全部历史行（entries + 流式 tail）。key 命中则复用缓存。
    /// 调用方持 `Ref` 期间不可再 `borrow_mut`（render 切片后立即 drop）。
    pub fn wrapped_lines(&self, theme: &Theme, width: u16) -> std::cell::Ref<'_, Vec<Line<'static>>> {
        {
            let mut wc = self.wrapped.borrow_mut();
            let key = (
                self.entries.len(),
                self.streaming_buffer.len(),
                self.thinking_buffer.len(),
                self.streaming_tool_output.len(),
                width,
            );
            if !wc.valid || wc.key != key {
                let unwrapped = self.build_render_lines(theme, width);
                wc.lines = wrap_lines(&unwrapped, width as usize);
                wc.key = key;
                wc.valid = true;
            }
        }
        std::cell::Ref::map(self.wrapped.borrow(), |wc| &wc.lines)
    }

    /// 构建未折行的全部历史行：复用 `cached_history`（prepare_render 维护），未就绪时
    ///（如单测直接调 render 未先 prepare）回退现场构建；流式期追加 tail。
    fn build_render_lines(&self, theme: &Theme, width: u16) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let cached = self.cached_history();
        if cached.is_empty() && !self.entries.is_empty() {
            lines.extend(render_entries(
                &self.entries,
                theme,
                &self.expanded_tool_results,
                width,
            ));
        } else {
            lines.extend_from_slice(cached);
        }
        if self.streaming {
            lines.extend(build_streaming_tail(
                &self.streaming_buffer,
                &self.thinking_buffer,
                &self.streaming_tool_output,
                theme,
            ));
        }
        lines
    }

    // ── 斜杠命令补全菜单 ────────────────────────────────────────────────────

    /// 根据当前输入刷新补全菜单：
    /// - `/cmd`（无空格）→ Command 模式，按前缀过滤命令名
    /// - `/cmd partial`（含一个空格，cmd 有固定参数集）→ Param 模式，按前缀过滤参数
    /// - 其他 → 关闭
    pub fn update_slash_menu(&mut self) {
        let line: String = self.input.lines().first().cloned().unwrap_or_default();
        let trimmed = line.trim_start();
        if trimmed.starts_with('/') && !trimmed.contains(' ') {
            // 一级：命令名补全
            let filtered = crate::slash::filter_commands(trimmed);
            if filtered.is_empty() {
                self.slash_menu.close();
            } else {
                if !self.slash_menu.open
                    || self.slash_menu.mode != SlashMenuMode::Command
                    || self.slash_menu.filtered != filtered
                {
                    if self.slash_menu.selected >= filtered.len() {
                        self.slash_menu.selected = 0;
                    }
                    self.slash_menu.filtered = filtered.clone();
                    self.slash_menu.params.clear();
                    self.slash_menu.mode = SlashMenuMode::Command;
                }
                self.slash_menu.open = true;
            }
        } else if trimmed.starts_with('/') {
            // 二级：参数补全（仅一个空格、参数部分无空格时）
            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let cmd = parts.next().unwrap_or("");
            let param = parts.next().unwrap_or("").trim_start();
            // 参数中已有空格 → 参数已完成，不再补全
            if param.contains(' ') {
                self.slash_menu.close();
                return;
            }
            let suggestions = crate::slash::param_suggestions(cmd);
            if suggestions.is_empty() {
                self.slash_menu.close();
                return;
            }
            let filtered: Vec<&'static str> = if param.is_empty() {
                suggestions
            } else {
                suggestions
                    .into_iter()
                    .filter(|s| s.starts_with(param))
                    .collect()
            };
            if filtered.is_empty() {
                self.slash_menu.close();
            } else {
                if !self.slash_menu.open
                    || self.slash_menu.mode != SlashMenuMode::Param
                    || self.slash_menu.params != filtered
                {
                    if self.slash_menu.selected >= filtered.len() {
                        self.slash_menu.selected = 0;
                    }
                    self.slash_menu.params = filtered.clone();
                    self.slash_menu.filtered.clear();
                    self.slash_menu.mode = SlashMenuMode::Param;
                }
                self.slash_menu.open = true;
            }
        } else {
            self.slash_menu.close();
        }
    }

    /// 菜单打开时处理导航键。返回 `true` 表示已消费（不传给 textarea / 不触发其他动作）。
    /// Up/Down 选择，Enter/Tab 补全，Esc 关闭；其余键返回 `false` 交正常输入路径。
    pub fn slash_menu_key(&mut self, k: KeyEvent) -> bool {
        if !self.slash_menu.open {
            return false;
        }
        match k.code {
            KeyCode::Up => {
                self.slash_menu_up();
                true
            }
            KeyCode::Down => {
                self.slash_menu_down();
                true
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.slash_menu_complete();
                true
            }
            KeyCode::Esc => {
                self.slash_menu.close();
                true
            }
            _ => false,
        }
    }

    fn slash_menu_up(&mut self) {
        let n = match self.slash_menu.mode {
            SlashMenuMode::Command => self.slash_menu.filtered.len(),
            SlashMenuMode::Param => self.slash_menu.params.len(),
        };
        if n > 0 {
            self.slash_menu.selected = (self.slash_menu.selected + n - 1) % n;
        }
    }

    fn slash_menu_down(&mut self) {
        let n = match self.slash_menu.mode {
            SlashMenuMode::Command => self.slash_menu.filtered.len(),
            SlashMenuMode::Param => self.slash_menu.params.len(),
        };
        if n > 0 {
            self.slash_menu.selected = (self.slash_menu.selected + 1) % n;
        }
    }

    /// 补全：
    /// - Command 模式：用选中命令名 + 空格替换输入，然后刷新菜单（若有参数建议则自动进入 Param 模式）
    /// - Param 模式：用选中参数替换参数部分，关闭菜单
    fn slash_menu_complete(&mut self) {
        match self.slash_menu.mode {
            SlashMenuMode::Command => {
                if let Some(spec) = self.slash_menu.filtered.get(self.slash_menu.selected).copied() {
                    self.input.clear();
                    self.input.insert_str(format!("{} ", spec.name));
                }
                // 补全命令名后刷新：若有参数建议则自动进入 Param 模式
                self.update_slash_menu();
            }
            SlashMenuMode::Param => {
                if let Some(param) = self.slash_menu.params.get(self.slash_menu.selected).copied() {
                    let line: String = self.input.lines().first().cloned().unwrap_or_default();
                    let trimmed = line.trim_start();
                    let cmd_end = trimmed.find(' ').unwrap_or(trimmed.len());
                    let cmd = &trimmed[..cmd_end];
                    self.input.clear();
                    self.input.insert_str(format!("{cmd} {param}"));
                }
                self.slash_menu.close();
            }
        }
    }

    // ── 输入历史呼出（↑/↓）──────────────────────────────────────────────────
    // 输入框为空时 ↑ 呼出更早、↓ 呼出更新、↓ 到头清空；非空时返回 false 交 textarea
    // 移光标（保留多行编辑）。流式期由调用方（App）拦截不调用此处。

    /// ↑：空输入（或浏览中）时呼出更早的已发送消息。
    /// 返回 `true` = 已呼出（输入框已替换）；`false` = 未处理（交 textarea 移光标）。
    pub fn history_prev(&mut self) -> bool {
        if self.input_history.entries.is_empty() {
            return false;
        }
        match self.input_history.browse {
            None => {
                if !self.input_empty() {
                    return false; // 非空输入：交 textarea 移光标
                }
                let i = self.input_history.entries.len() - 1;
                self.input_history.browse = Some(i);
                self.load_history_entry(i);
                true
            }
            Some(i) => {
                if i == 0 {
                    return true; // 已到最早，保持当前
                }
                let ni = i - 1;
                self.input_history.browse = Some(ni);
                self.load_history_entry(ni);
                true
            }
        }
    }

    /// ↓：浏览中呼出更新的条目；到头清空输入并退出浏览态（回到最新）。
    /// 返回 `true` = 已处理；`false` = 未浏览态（交 textarea 移光标）。
    pub fn history_next(&mut self) -> bool {
        match self.input_history.browse {
            None => false, // 未浏览：交 textarea
            Some(i) => {
                if i + 1 >= self.input_history.entries.len() {
                    self.input_history.browse = None;
                    self.input.clear(); // 回到最新（空输入）
                } else {
                    let ni = i + 1;
                    self.input_history.browse = Some(ni);
                    self.load_history_entry(ni);
                }
                true
            }
        }
    }

    /// 用 `entries[i]` 替换输入框内容（clear + insert_str，沿用 slash_menu_complete 模式）。
    fn load_history_entry(&mut self, i: usize) {
        self.input.clear();
        self.input.insert_str(&self.input_history.entries[i]);
    }

    /// 输入框是否全空（所有行为空串）。
    fn input_empty(&self) -> bool {
        self.input.lines().iter().all(|l| l.is_empty())
    }

    /// 从已完成条目的 `ChatEntry::User` 文本填充输入历史（App 启动加载历史后调用）。
    pub fn seed_input_history(&mut self) {
        let user_texts: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                ChatEntry::User(c) => Some(c.clone()),
                _ => None,
            })
            .collect();
        self.input_history.seed(user_texts);
    }

    /// 已完成条目的渲染行（只读）。调用前须先经 `prepare_render` 刷新。
    pub fn cached_history(&self) -> &[Line<'static>] {
        &self.cached_history
    }

    /// 尝试提交输入：流式期或空输入返回 `None`；否则返回 `(待发送文本, 上下文历史)`，
    /// push user 条目（用于显示）、清空输入、置 streaming。
    ///
    /// 返回的 `history` **不含当前输入**——因为 agent 的 `run_stream` 会在内部把
    /// `user_input` append 到 history。若 history 含当前输入会导致重复。
    pub fn submit(&mut self) -> Option<(String, Vec<Message>)> {
        if self.streaming {
            return None;
        }
        let text: String = self.input.lines().join("\n");
        if text.trim().is_empty() {
            return None;
        }
        self.input_history.record(&text); // 记录到输入历史（清空输入前）
        self.input.clear();
        // history 必须在 push 当前 user 条目之前取（run_stream 会再 append user_input）
        let history = self.history();
        self.entries.push(ChatEntry::User(text.clone()));
        self.streaming = true;
        self.streaming_buffer.clear();
        self.thinking_buffer.clear();
        self.scroll_to_bottom(); // 新提交：跟随新响应
        self.slash_menu.close();
        Some((text, history))
    }

    /// 插入粘贴文本（bracketed paste）：整块插入输入框，不触发提交。
    /// 流式期忽略。粘贴后刷新斜杠菜单（可能以 `/` 开头）。
    pub fn paste(&mut self, text: &str) {
        if self.streaming {
            return;
        }
        self.input.insert_str(text);
        self.update_slash_menu();
    }

    /// 把当前 `streaming_buffer` 定稿为一条 assistant 条目（仅 buffer 非空时 push），
    /// **不**改变 streaming 态。用于 ToolCall 到达前先把已累积的 assistant 文本落盘。
    pub fn flush_streaming_to_assistant(&mut self) {
        // 先 flush 思考过程（位于 Assistant 条目之前）
        if !self.thinking_buffer.is_empty() {
            let content = std::mem::take(&mut self.thinking_buffer);
            self.entries.push(ChatEntry::Thinking(content));
        }
        if !self.streaming_buffer.is_empty() {
            let content = std::mem::take(&mut self.streaming_buffer);
            self.entries.push(ChatEntry::Assistant(content));
        }
    }

    /// 收到一次工具调用事件：先 flush buffer（若有 assistant 前导文本），再 push ToolCall。
    pub fn push_tool_call(&mut self, id: String, name: String, arguments: String) {
        self.flush_streaming_to_assistant();
        // 防御：新工具调用开始前清空残留的流式输出（正常情况下上一工具的 ToolResult 已清空）
        self.streaming_tool_output.clear();
        self.entries.push(ChatEntry::ToolCall { id, name, arguments });
    }

    /// 收到工具执行增量输出（ToolProgress）：累积进 `streaming_tool_output`，由流式
    /// tail 实时渲染（带 ▌ 光标）。工具串行执行，故单缓冲即可。
    pub fn push_tool_progress(&mut self, chunk: &str) {
        self.streaming_tool_output.push_str(chunk);
    }

    /// 收到工具结果事件：push ToolResult（紧随对应 ToolCall，无需 flush），并清空流式输出。
    pub fn push_tool_result(&mut self, id: String, name: String, output: String, is_error: bool) {
        self.streaming_tool_output.clear();
        self.entries
            .push(ChatEntry::ToolResult { id, name, output, is_error });
    }

    /// 切换最后一个可折叠条目（工具结果或思考过程）的展开/折叠状态（Ctrl+O）。
    ///
    /// 从 entries 末尾向前找第一个 ToolResult 或 Thinking 条目。索引以 `entries`
    /// 下标为准（条目只追加不插入，下标稳定）。无可折叠条目时 no-op。
    /// 切换后 `invalidate_cache` 强制重渲染。
    pub fn toggle_last_tool_result_expansion(&mut self) {
        if let Some(idx) = self.entries.iter().rposition(|e| {
            matches!(e, ChatEntry::ToolResult { .. } | ChatEntry::Thinking(_))
        }) {
            if self.expanded_tool_results.contains(&idx) {
                self.expanded_tool_results.remove(&idx);
            } else {
                self.expanded_tool_results.insert(idx);
            }
            self.invalidate_cache();
        }
    }

    /// 把当前 `streaming_buffer` 定稿为一条 assistant 条目，退出 streaming 态。
    /// buffer 为空时不 push（避免空 assistant 条目；工具链已有 ToolCall/ToolResult 记录）。
    pub fn finalize_stream(&mut self) {
        self.flush_streaming_to_assistant();
        self.streaming_tool_output.clear();
        self.streaming = false;
    }

    /// 取消流式（Esc）：保留已生成的文本（flush 为条目），退出 streaming 态。
    ///
    /// 不能简单丢弃 buffer：截停前的部分文本/思考过程是下次继续的上下文依据。
    /// 丢弃会导致「截停后失去记忆」——再次提问时模型看不到之前说到哪了。
    /// 工具调用/结果条目已在事件到达时 push，这里只需 flush 残留文本。
    pub fn cancel_stream(&mut self) {
        self.flush_streaming_to_assistant();
        self.streaming_tool_output.clear();
        self.streaming = false;
    }

    /// 清空全部对话条目（`/clear` 命令）。不改变 streaming 态（流式期不允许清空）。
    pub fn clear(&mut self) {
        self.entries.clear();
        self.streaming_buffer.clear();
        self.thinking_buffer.clear();
        self.streaming_tool_output.clear();
        self.scroll_to_bottom();
        self.slash_menu.close();
        self.expanded_tool_results.clear();
    }

    /// 取走输入框文本（不清屏、不 push 条目、不改 streaming），用于斜杠命令。
    /// 返回的文本已去尾换行。
    pub fn take_input(&mut self) -> String {
        let text: String = self.input.lines().join("\n");
        self.input.clear();
        self.slash_menu.close();
        text
    }

    /// 把已完成的条目转为 agent `Message`（作为下一次请求的 history 上下文）。
    ///
    /// 保留工具链：`ToolCall` 合并到其前导 assistant 消息的 `tool_calls`，
    /// `ToolResult` 转为 `role=Tool` 消息（带 `tool_call_id`）。这样被截停或自然结束
    /// 后，下一次请求能看到之前执行了哪些工具、结果如何，避免 agent「从头再来」。
    /// 仅剥离 Thinking/System（思考过程与系统提示不回灌跨轮上下文）。
    ///
    /// 截停可能留下**孤立的 ToolCall**（已声明调用但工具结果未返回即被 abort）。
    /// 这类 ToolCall 若转为 assistant(tool_calls) 而无对应 tool 消息，OpenAI 会报
    /// `tool_calls must be followed by tool messages`。故只保留有对应 ToolResult 的
    /// 完整工具链，孤立 ToolCall 跳过。
    pub fn history(&self) -> Vec<Message> {
        // 收集所有已完成的工具调用 id（有 ToolResult 才算完整）
        let completed: HashSet<&str> = self
            .entries
            .iter()
            .filter_map(|e| match e {
                ChatEntry::ToolResult { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();

        let mut out: Vec<Message> = Vec::new();
        // 待定 assistant 消息：前导文本已入，后续完整 ToolCall 追加其 tool_calls。
        // ToolResult 到达时先收尾（flush）再 push tool 消息，保证 tool 紧跟在
        // 对应 assistant(tool_calls) 之后（OpenAI/Anthropic 协议要求）。
        let mut pending_assistant: Option<Message> = None;

        for e in &self.entries {
            match e {
                ChatEntry::User(c) => {
                    if let Some(m) = pending_assistant.take() {
                        out.push(m);
                    }
                    out.push(Message::user(c.clone()));
                }
                ChatEntry::Assistant(c) => {
                    if let Some(m) = pending_assistant.take() {
                        out.push(m);
                    }
                    pending_assistant = Some(Message::assistant(c.clone()));
                }
                ChatEntry::ToolCall { id, name, arguments } => {
                    // 仅保留有对应 ToolResult 的完整工具调用；孤立 ToolCall 跳过。
                    if completed.contains(id.as_str()) {
                        let m = pending_assistant
                            .get_or_insert_with(|| Message::assistant(String::new()));
                        m.tool_calls.push(ToolCall {
                            id: id.clone(),
                            name: name.clone(),
                            arguments: arguments.clone(),
                        });
                    }
                }
                ChatEntry::ToolResult { id, output, .. } => {
                    if let Some(m) = pending_assistant.take() {
                        out.push(m);
                    }
                    out.push(Message::tool(id.clone(), output.clone()));
                }
                _ => {} // Thinking / System 不回灌跨轮上下文
            }
        }
        if let Some(m) = pending_assistant.take() {
            out.push(m);
        }
        out
    }
}

impl Default for ChatState {
    fn default() -> Self {
        Self::new()
    }
}

// ── 条目 → 渲染行 ───────────────────────────────────────────────────────────
// 以下函数把已完成 `ChatEntry` 转为带样式的 `Line<'static>`（含条目间空行）。
// 供 `ChatState::prepare_render` 缓存复用，`views::chat` 经 `cached_history()` 读取，
// 避免每帧重新 tokenize 全部条目。流式 tail（光标行）由 view 现场构建，不入此缓存。

/// 把已完成条目序列渲染为 `Line<'static>`（每条目后附一空行作分隔）。
///
/// `expanded` 标记哪些 `ToolResult` 条目（按下标）已展开——默认折叠仅显示最后
/// `TOOL_RESULT_FOLD_THRESHOLD` 可视行，展开后显示全部并附折叠提示。`width` 为
/// 历史区可用宽度，用于计算工具结果的可视行数（单行长输出也会触发折叠）。
pub fn render_entries(
    entries: &[ChatEntry],
    theme: &Theme,
    expanded: &HashSet<usize>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        match entry {
            ChatEntry::User(content) => {
                push_role_lines(&mut lines, "[user]", theme.accent, content, theme);
            }
            ChatEntry::Assistant(content) => {
                push_assistant_lines(&mut lines, theme, content);
            }
            ChatEntry::Thinking(content) => {
                let is_expanded = expanded.contains(&i);
                push_thinking_lines(&mut lines, theme, content, is_expanded);
            }
            ChatEntry::System(content) => {
                push_role_lines(&mut lines, "[system]", theme.muted, content, theme);
            }
            ChatEntry::ToolCall {
                id: _,
                name,
                arguments,
            } => {
                push_tool_call(&mut lines, theme, name, arguments);
            }
            ChatEntry::ToolResult {
                id: _,
                name: _,
                output,
                is_error,
            } => {
                let is_expanded = expanded.contains(&i);
                push_tool_result(&mut lines, theme, output, *is_error, is_expanded, width);
            }
        }
        lines.push(Line::from("")); // 条目间空行
    }
    lines
}

/// 构建流式 tail：把 `streaming_buffer` 作为进行中的 assistant 消息（末行带 ▌ 光标）。
///
/// 标签 `[assistant]` 独占一行，随后是 Markdown 渲染的 buffer 内容；buffer 可能为
/// 不完整 markdown（未闭合 `**` / 围栏），`markdown::render` 会把未闭合格式降级为
/// 纯文本。空 buffer 显示标签行 + 等待光标。由 `build_render_lines` 调用并入预折行
/// 缓存（量小，每帧随 buffer 变化重建）。
///
/// 若 `thinking_buffer` 非空，在 assistant 标签前渲染思考过程（最新 3 行 + 折叠提示）。
///
/// 若 `tool_out` 非空（工具执行中，streaming_buffer 已 flush），改为渲染工具增量输出：
/// 每行带 `→` 前缀（首行）/ 缩进（续行），末行带 ▌ 光标，实现「边执行边输出」。
fn build_streaming_tail(buffer: &str, thinking: &str, tool_out: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // 思考过程（最新 3 行）
    if !thinking.is_empty() {
        push_thinking_lines_streaming(&mut lines, theme, thinking);
    }
    // 工具执行增量输出（与 assistant buffer 互斥：工具运行时 buffer 已 flush 为空）
    if !tool_out.is_empty() {
        let all_lines: Vec<&str> = tool_out.lines().collect();
        for (i, l) in all_lines.iter().enumerate() {
            let prefix = if i == 0 { "    → " } else { "      " };
            let mut spans = vec![
                Span::styled(prefix, Style::default().fg(theme.muted)),
                Span::styled(l.to_string(), Style::default().fg(theme.fg)),
            ];
            if i == all_lines.len() - 1 {
                spans.push(Span::styled("▌", Style::default().fg(theme.accent)));
            }
            lines.push(Line::from(spans));
        }
        return lines;
    }
    lines.push(Line::from(Span::styled(
        "[assistant]",
        Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
    )));
    if buffer.is_empty() {
        lines.push(Line::from(Span::styled(
            "▌",
            Style::default().fg(theme.accent),
        )));
        return lines;
    }
    let mut md_lines = crate::markdown::render(buffer, theme);
    if md_lines.is_empty() {
        md_lines.push(Line::from(""));
    }
    // 末行追加 ▌ 光标
    let last = md_lines.last_mut().expect("md_lines 至少 1 行");
    last.spans
        .push(Span::styled("▌", Style::default().fg(theme.accent)));
    lines.extend(md_lines);
    lines
}

/// 流式期间渲染思考过程：只显示最新 3 行 + 折叠提示。
fn push_thinking_lines_streaming(lines: &mut Vec<Line<'static>>, theme: &Theme, text: &str) {
    let all_lines: Vec<&str> = text.lines().collect();
    lines.push(Line::from(vec![
        Span::styled("💭 ", Style::default()),
        Span::styled(
            "思考过程",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::DIM),
        ),
    ]));
    let show_count = 3.min(all_lines.len());
    let start = all_lines.len().saturating_sub(show_count);
    for line in &all_lines[start..] {
        lines.push(Line::from(vec![Span::styled(
            format!("  {line}"),
            Style::default().fg(theme.muted),
        )]));
    }
    if all_lines.len() > 3 {
        lines.push(Line::from(vec![Span::styled(
            format!("  ⋮ +{} 行 · Ctrl+O 展开", all_lines.len() - 3),
            Style::default().fg(theme.muted).add_modifier(Modifier::DIM),
        )]));
    }
}

/// 把未折行的 `Line` 列表按 `width` 拆成单行 `Line`（贪婪字符宽度填充，CJK 占 2 列）。
///
/// 替代 `Paragraph::line_count` + `Wrap` 的每帧 O(N) 重算：折行结果缓存于
/// `WrappedCache`，render 直接切片可见窗口。样式按 span 边界保留（相邻同 style 字符
/// 合并为一个 span）。`width == 0` 时不折行（避免除零，回退原样）。
fn wrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return lines.to_vec();
    }
    let mut out: Vec<Line<'static>> = Vec::new();
    for line in lines {
        // 展平为 (char, 显示宽度, style)，按字符贪婪填行
        let mut cells: Vec<(char, usize, Style)> = Vec::new();
        for span in &line.spans {
            for ch in span.content.chars() {
                // tab 按 1 列计（与历史区行为一致，避免对齐抖动）
                let w = if ch == '\t' {
                    1
                } else {
                    UnicodeWidthChar::width(ch).unwrap_or(0)
                };
                cells.push((ch, w, span.style));
            }
        }
        if cells.is_empty() {
            out.push(Line::from(""));
            continue;
        }
        let mut row_spans: Vec<Span<'static>> = Vec::new();
        let mut row_w: usize = 0;
        let mut cur_str = String::new();
        let mut cur_style: Option<Style> = None;
        for (ch, w, st) in cells {
            // 当前行非空且加入 ch 会超宽 → 先收尾当前 span 与行
            if !cur_str.is_empty() && row_w + w > width {
                row_spans.push(Span::styled(
                    std::mem::take(&mut cur_str),
                    cur_style.unwrap_or_default(),
                ));
                out.push(Line::from(std::mem::take(&mut row_spans)));
                row_w = 0;
                cur_style = None;
            }
            // style 边界：切出新 span
            if cur_style != Some(st) {
                if !cur_str.is_empty() {
                    row_spans.push(Span::styled(
                        std::mem::take(&mut cur_str),
                        cur_style.unwrap_or_default(),
                    ));
                }
                cur_style = Some(st);
            }
            cur_str.push(ch);
            row_w += w;
        }
        // 收尾最后一个 span 与行
        if !cur_str.is_empty() {
            row_spans.push(Span::styled(cur_str, cur_style.unwrap_or_default()));
        }
        out.push(Line::from(row_spans));
    }
    out
}

/// 把一条 user/assistant/system 文本按行展开为带标签的 `Line`。
/// 首行带 `label`（如 `[user]`），续行缩进对齐；空内容仍占一行标签。
fn push_role_lines(
    lines: &mut Vec<Line<'static>>,
    label: &str,
    label_fg: Color,
    content: &str,
    theme: &Theme,
) {
    for (i, text_line) in content.lines().enumerate() {
        let prefix = if i == 0 { label } else { "        " };
        lines.push(Line::from(vec![
            Span::styled(
                format!("{prefix} "),
                Style::default().fg(label_fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(text_line.to_string(), Style::default().fg(theme.fg)),
        ]));
    }
    // 空内容消息至少占一行
    if content.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!("{label} "),
            Style::default().fg(label_fg).add_modifier(Modifier::BOLD),
        )]));
    }
}

/// 渲染一条 assistant 条目：标签独占一行，随后是 Markdown 渲染的内容行。
///
/// 与 User/System 的内联标签不同——assistant 输出常含代码块 / 标题 / 列表等块级结构，
/// 标签独占一行可避免块级缩进与标签前缀错位，Markdown 内容自然铺开。空内容仍保留
/// 标签行（至少占一行，便于辨识空回复）。流式 buffer 复用同一渲染路径（见 views::chat）。
fn push_assistant_lines(lines: &mut Vec<Line<'static>>, theme: &Theme, content: &str) {
    lines.push(Line::from(Span::styled(
        "[assistant]",
        Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
    )));
    let md_lines = crate::markdown::render(content, theme);
    lines.extend(md_lines);
}

/// 渲染一次工具调用：`  ▶ [tool] name(arguments)`。
/// `▶` 与工具名用 accent 高亮，arguments 用 muted；空参 `{}` 显示为 `()`。
fn push_tool_call(lines: &mut Vec<Line<'static>>, theme: &Theme, name: &str, arguments: &str) {
    let mut spans = vec![
        Span::styled(
            "  ▶ ",
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("[tool] {name}"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    let args_display = if arguments.is_empty() || arguments == "{}" {
        "()".to_string()
    } else {
        format!("({arguments})")
    };
    spans.push(Span::styled(
        args_display,
        Style::default().fg(theme.muted),
    ));
    lines.push(Line::from(spans));
}

/// 渲染思考过程条目：标题行 + 内容行。
///
/// 折叠态（默认）：仅显示最新 3 行 + `⋮ +M 行已折叠 · Ctrl+O 展开` 提示。
/// 展开态：显示全部行 + `⋮ 共 N 行 · Ctrl+O 折叠` 提示。
/// ≤3 行时照常全显，无折叠提示。
fn push_thinking_lines(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    content: &str,
    expanded: bool,
) {
    let all_lines: Vec<&str> = content.lines().collect();
    let count = all_lines.len();
    lines.push(Line::from(vec![
        Span::styled("💭 ", Style::default()),
        Span::styled(
            "思考过程",
            Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::BOLD | Modifier::DIM),
        ),
    ]));
    let folded = count > TOOL_RESULT_FOLD_THRESHOLD && !expanded;
    if folded {
        let show = TOOL_RESULT_FOLD_THRESHOLD;
        let hidden = count - show;
        let start = count - show;
        for line in &all_lines[start..] {
            lines.push(Line::from(vec![Span::styled(
                format!("  {line}"),
                Style::default().fg(theme.muted),
            )]));
        }
        lines.push(Line::from(vec![
            Span::styled("  ⋮ ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("+{hidden} 行已折叠 · Ctrl+O 展开"),
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
    } else {
        for line in &all_lines {
            lines.push(Line::from(vec![Span::styled(
                format!("  {line}"),
                Style::default().fg(theme.muted),
            )]));
        }
        if count > TOOL_RESULT_FOLD_THRESHOLD {
            lines.push(Line::from(vec![
                Span::styled("  ⋮ ", Style::default().fg(theme.muted)),
                Span::styled(
                    format!("共 {count} 行 · Ctrl+O 折叠"),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::DIM),
                ),
            ]));
        }
    }
}

/// 渲染工具结果：成功 `    → output`（muted/fg），错误 `    ✗ output`（红色）。
///
/// 多行 output 逐行展开，续行缩进对齐首行内容；空 output 仍显示标记。
///
/// 折叠行为（参考 Claude Code）：输出 **可视行数**（按 `width` 折行后）超过
/// `TOOL_RESULT_FOLD_THRESHOLD` 且未展开时，仅渲染最后 N 可视行，并在上方加
/// `⋮ +M 行已折叠 · Ctrl+O 展开` 提示；展开后渲染全部行，并在末尾附
/// `⋮ 共 N 行 · Ctrl+O 折叠` 提示。≤阈值行数照常全显，无提示。
/// 使用可视行数而非源行数，可确保单行长输出（如 MCP JSON 回显）也能正确折叠。
fn push_tool_result(
    lines: &mut Vec<Line<'static>>,
    theme: &Theme,
    output: &str,
    is_error: bool,
    expanded: bool,
    width: u16,
) {
    let (marker, marker_color, content_color) = if is_error {
        ("✗", Color::Red, Color::Red)
    } else {
        ("→", theme.muted, theme.fg)
    };
    // 内容宽度 = 总宽 - 前缀（"    {marker} " = 6 列）。前缀由 push_result_lines 添加。
    let content_width = (width as usize).saturating_sub(6);
    let visual_lines = wrap_string_to_visual_lines(output, content_width);
    let visual_count = visual_lines.len();
    let folded = visual_count > TOOL_RESULT_FOLD_THRESHOLD && !expanded;

    // 折叠提示行（折叠态放顶部，展开态放底部）
    let fold_hint_top = |lines: &mut Vec<Line<'static>>, hidden: usize| {
        lines.push(Line::from(vec![
            Span::styled("    ⋮ ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("+{hidden} 行已折叠 · Ctrl+O 展开"),
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
    };
    let fold_hint_bottom = |lines: &mut Vec<Line<'static>>, total: usize| {
        lines.push(Line::from(vec![
            Span::styled("    ⋮ ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("共 {total} 行 · Ctrl+O 折叠"),
                Style::default()
                    .fg(theme.muted)
                    .add_modifier(Modifier::DIM),
            ),
        ]));
    };

    if visual_lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            format!("    {marker} "),
            Style::default()
                .fg(marker_color)
                .add_modifier(Modifier::BOLD),
        )]));
        return;
    }

    // 折叠态：先输出折叠提示，再输出最后 N 可视行
    if folded {
        let hidden = visual_count - TOOL_RESULT_FOLD_THRESHOLD;
        fold_hint_top(lines, hidden);
        let start = visual_count - TOOL_RESULT_FOLD_THRESHOLD;
        push_result_lines(
            lines,
            &visual_lines[start..],
            marker,
            marker_color,
            content_color,
        );
    } else {
        // 全显（≤阈值 或 已展开）
        push_result_lines(lines, &visual_lines, marker, marker_color, content_color);
        // 已展开且超阈值：追加底部折叠提示
        if visual_count > TOOL_RESULT_FOLD_THRESHOLD && expanded {
            fold_hint_bottom(lines, visual_count);
        }
    }
}

/// 渲染工具结果的多行内容：首行带 `marker`，续行缩进对齐首行内容。
fn push_result_lines(
    lines: &mut Vec<Line<'static>>,
    out_lines: &[String],
    marker: &str,
    marker_color: Color,
    content_color: Color,
) {
    for (i, line) in out_lines.iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("    {marker} "),
                    Style::default()
                        .fg(marker_color)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(line.clone(), Style::default().fg(content_color)),
            ]));
        } else {
            // 续行缩进对齐首行内容（6 空格 = "    {marker} " 宽度）
            lines.push(Line::from(vec![
                Span::styled("      ", Style::default()),
                Span::styled(line.clone(), Style::default().fg(content_color)),
            ]));
        }
    }
}

/// 把字符串按 `width` 列宽贪婪折行为可视行列表（CJK 占 2 列，tab 占 1 列）。
///
/// 与 `wrap_lines` 的折行逻辑一致，但操作于纯字符串（不含 span/style），用于工具结果
/// 折叠阈值的可视行数计算。空字符串返回空 Vec（与 `str::lines()` 一致），`"\n"` 返回
/// `[""]`。`width == 0` 时不折行，每条源行原样返回（避免除零）。
fn wrap_string_to_visual_lines(s: &str, width: usize) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut result: Vec<String> = Vec::new();
    for source_line in s.lines() {
        if source_line.is_empty() {
            result.push(String::new());
            continue;
        }
        if width == 0 {
            result.push(source_line.to_string());
            continue;
        }
        let mut current = String::new();
        let mut current_w: usize = 0;
        for ch in source_line.chars() {
            let w = if ch == '\t' {
                1
            } else {
                UnicodeWidthChar::width(ch).unwrap_or(0)
            };
            if !current.is_empty() && current_w + w > width {
                result.push(std::mem::take(&mut current));
                current_w = 0;
            }
            current.push(ch);
            current_w += w;
        }
        result.push(current);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use cyber_agent::Role;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new_with_kind_and_state(code, KeyModifiers::NONE, KeyEventKind::Press, KeyEventState::NONE)
    }

    fn key_with_mods(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new_with_kind_and_state(code, mods, KeyEventKind::Press, KeyEventState::NONE)
    }

    #[test]
    fn paste_detector_first_key_is_process() {
        let mut pd = PasteDetector::new();
        assert_eq!(pd.observe(key(KeyCode::Char('a'))), KeyDisposition::Process);
    }

    #[test]
    fn paste_detector_rapid_keys_are_buffered() {
        let mut pd = PasteDetector::new();
        pd.observe(key(KeyCode::Char('a'))); // 第一次：Process
        // 第二次立即：Buffer
        assert_eq!(pd.observe(key(KeyCode::Char('b'))), KeyDisposition::Buffer);
        assert_eq!(pd.observe(key(KeyCode::Char('c'))), KeyDisposition::Buffer);
        let flushed = pd.flush().unwrap();
        assert_eq!(flushed, "bc");
    }

    #[test]
    fn paste_detector_enter_becomes_newline_in_buffer() {
        let mut pd = PasteDetector::new();
        pd.observe(key(KeyCode::Char('a'))); // 第一次：Process
        assert_eq!(pd.observe(key(KeyCode::Enter)), KeyDisposition::Buffer);
        assert_eq!(pd.observe(key(KeyCode::Char('b'))), KeyDisposition::Buffer);
        let flushed = pd.flush().unwrap();
        assert_eq!(flushed, "\nb");
    }

    #[test]
    fn paste_detector_ctrl_c_not_buffered() {
        let mut pd = PasteDetector::new();
        pd.observe(key(KeyCode::Char('a'))); // Process（首次）
        pd.observe(key(KeyCode::Char('b'))); // Buffer（快速）
        // Ctrl+C：带修饰键 → 不缓冲，先 flush buffer 再处理 Ctrl+C
        assert_eq!(
            pd.observe(key_with_mods(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            KeyDisposition::FlushThenProcess
        );
        let flushed = pd.flush().unwrap();
        assert_eq!(flushed, "b");
    }

    #[test]
    fn paste_detector_esc_flushes_buffer() {
        let mut pd = PasteDetector::new();
        pd.observe(key(KeyCode::Char('a'))); // Process
        pd.observe(key(KeyCode::Char('b'))); // Buffer
        // Esc：特殊键 → FlushThenProcess
        assert_eq!(pd.observe(key(KeyCode::Esc)), KeyDisposition::FlushThenProcess);
        let flushed = pd.flush().unwrap();
        assert_eq!(flushed, "b");
    }

    #[test]
    fn paste_detector_flush_if_stale_returns_none_when_empty() {
        let mut pd = PasteDetector::new();
        assert!(pd.flush_if_stale().is_none());
    }

    #[test]
    fn paste_detector_flush_if_stale_after_timeout() {
        let mut pd = PasteDetector::new();
        pd.observe(key(KeyCode::Char('a'))); // Process
        pd.observe(key(KeyCode::Char('b'))); // Buffer
        // 手动设置 last_key_time 为很久以前
        pd.last_key_time = Some(Instant::now() - Duration::from_millis(100));
        let flushed = pd.flush_if_stale();
        assert_eq!(flushed.as_deref(), Some("b"));
        // 再调应返回 None（buffer 已清）
        assert!(pd.flush_if_stale().is_none());
    }

    #[test]
    fn paste_detector_flush_if_stale_not_yet() {
        let mut pd = PasteDetector::new();
        pd.observe(key(KeyCode::Char('a'))); // Process
        pd.observe(key(KeyCode::Char('b'))); // Buffer
        // 刚刚按键，还没超时
        assert!(pd.flush_if_stale().is_none());
    }

    #[test]
    fn wrap_string_to_visual_lines_basic() {
        // 空字符串 → 空 Vec
        assert!(wrap_string_to_visual_lines("", 10).is_empty());
        // 无换行短串 → 1 行
        assert_eq!(wrap_string_to_visual_lines("hello", 10), vec!["hello"]);
        // 多行 → 每源行一项
        assert_eq!(
            wrap_string_to_visual_lines("a\nb\nc", 10),
            vec!["a", "b", "c"]
        );
        // 仅换行 → 1 个空行
        assert_eq!(wrap_string_to_visual_lines("\n", 10), vec![""]);
    }

    #[test]
    fn wrap_string_to_visual_lines_wraps_long_line() {
        // 20 字符在宽 5 时 → 4 行
        let result = wrap_string_to_visual_lines("aaaaaaaaaaaaaaaaaaaa", 5);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0], "aaaaa");
        assert_eq!(result[3], "aaaaa");
        // width == 0 → 不折行
        let result0 = wrap_string_to_visual_lines("aaaaaaaaaaaaaaaaaaaa", 0);
        assert_eq!(result0, vec!["aaaaaaaaaaaaaaaaaaaa"]);
    }

    #[test]
    fn wrap_string_to_visual_lines_cjk_width() {
        // CJK 字符占 2 列：4 个中文字 = 8 列，宽 4 → 2 行
        let result = wrap_string_to_visual_lines("你好世界", 4);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "你好");
        assert_eq!(result[1], "世界");
    }

    #[test]
    fn submit_returns_text_history_and_pushes_user_entry() {
        let mut s = ChatState::new();
        s.input.insert_str("你好");
        let (text, history) = s.submit().expect("非空非流式应返回 Some");
        assert_eq!(text, "你好");
        assert!(history.is_empty(), "首次提交 history 应为空（不含当前输入）");
        assert_eq!(s.entries.len(), 1);
        assert!(matches!(s.entries[0], ChatEntry::User(ref c) if c == "你好"));
        assert!(s.streaming, "submit 后应进入 streaming 态");
        assert!(s.input.lines().iter().all(|l| l.is_empty()), "输入框应清空");
    }

    #[test]
    fn submit_during_streaming_returns_none() {
        let mut s = ChatState::new();
        s.input.insert_str("x");
        s.submit();
        s.input.insert_str("y");
        let second = s.submit();
        assert!(second.is_none(), "流式期 submit 应返回 None");
    }

    #[test]
    fn submit_empty_returns_none() {
        let mut s = ChatState::new();
        assert!(s.submit().is_none());
        assert!(s.entries.is_empty());
        assert!(!s.streaming);
    }

    #[test]
    fn submit_whitespace_only_returns_none() {
        let mut s = ChatState::new();
        s.input.insert_str("   \n  ");
        assert!(s.submit().is_none());
        assert!(s.entries.is_empty());
    }

    #[test]
    fn submit_history_excludes_current_input() {
        // 已有一条 user + 一条 assistant 条目后，再次 submit
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q1".into()));
        s.entries.push(ChatEntry::Assistant("a1".into()));
        s.input.insert_str("q2");
        let (text, history) = s.submit().unwrap();
        assert_eq!(text, "q2");
        assert_eq!(history.len(), 2, "history 应含 2 条先前消息，不含当前 q2");
        assert_eq!(history[0].content, "q1");
        assert_eq!(history[1].content, "a1");
        assert_eq!(s.entries.len(), 3, "entries 应含 3 条（含刚 push 的 q2）");
    }

    #[test]
    fn finalize_stream_appends_assistant_and_clears_buffer() {
        let mut s = ChatState::new();
        s.streaming = true;
        s.streaming_buffer.push_str("收到：hi");
        s.finalize_stream();
        assert!(!s.streaming);
        assert_eq!(s.entries.len(), 1);
        assert!(matches!(s.entries[0], ChatEntry::Assistant(ref c) if c == "收到：hi"));
        assert!(s.streaming_buffer.is_empty());
    }

    #[test]
    fn finalize_empty_buffer_no_entry() {
        // 空缓冲 finalize 不 push assistant 条目（避免空条目），但仍退出 streaming
        let mut s = ChatState::new();
        s.streaming = true;
        s.finalize_stream();
        assert!(!s.streaming);
        assert!(s.entries.is_empty(), "空 buffer 不应 push 条目");
    }

    #[test]
    fn cancel_stream_preserves_buffer_as_assistant_entry() {
        let mut s = ChatState::new();
        s.streaming = true;
        s.streaming_buffer.push_str("部分");
        s.cancel_stream();
        assert!(!s.streaming);
        // 截停不应丢弃已生成的文本，而是 flush 为 assistant 条目（保留上下文）
        assert_eq!(s.entries.len(), 1);
        assert!(matches!(&s.entries[0], ChatEntry::Assistant(c) if c == "部分"));
        assert!(s.streaming_buffer.is_empty());
    }

    #[test]
    fn history_excludes_streaming_buffer() {
        let mut s = ChatState::new();
        s.input.insert_str("a");
        s.submit();
        s.streaming_buffer.push_str("部分");
        let h = s.history();
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].role, Role::User);
        assert_eq!(h[0].content, "a");
    }

    #[test]
    fn history_preserves_tool_chain() {
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q".into()));
        s.entries.push(ChatEntry::ToolCall {
            id: "1".into(),
            name: "list_dir".into(),
            arguments: "{}".into(),
        });
        s.entries.push(ChatEntry::ToolResult {
            id: "1".into(),
            name: "list_dir".into(),
            output: "a.txt".into(),
            is_error: false,
        });
        s.entries.push(ChatEntry::Assistant("done".into()));
        let h = s.history();
        // 保留工具链：User + assistant(tool_calls) + tool 结果 + assistant
        assert_eq!(h.len(), 4, "history 应保留工具链");
        assert_eq!(h[0].role, Role::User);
        assert_eq!(h[0].content, "q");
        // ToolCall 合并为 assistant 消息的 tool_calls
        assert_eq!(h[1].role, Role::Assistant);
        assert_eq!(h[1].tool_calls.len(), 1);
        assert_eq!(h[1].tool_calls[0].name, "list_dir");
        // ToolResult 转为 tool 消息
        assert_eq!(h[2].role, Role::Tool);
        assert_eq!(h[2].tool_call_id.as_deref(), Some("1"));
        assert_eq!(h[2].content, "a.txt");
        assert_eq!(h[3].content, "done");
    }

    #[test]
    fn history_merges_leading_text_with_tool_call() {
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q".into()));
        s.entries.push(ChatEntry::Assistant("让我看看".into()));
        s.entries.push(ChatEntry::ToolCall {
            id: "2".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        });
        s.entries.push(ChatEntry::ToolResult {
            id: "2".into(),
            name: "shell".into(),
            output: "ok".into(),
            is_error: false,
        });
        let h = s.history();
        // 前导文本与 tool_call 合并到同一条 assistant 消息
        assert_eq!(h.len(), 3);
        assert_eq!(h[1].role, Role::Assistant);
        assert_eq!(h[1].content, "让我看看");
        assert_eq!(h[1].tool_calls.len(), 1);
        assert_eq!(h[1].tool_calls[0].name, "shell");
        assert_eq!(h[2].role, Role::Tool);
    }

    #[test]
    fn history_skips_orphan_tool_call() {
        // 截停可能留下孤立 ToolCall（无对应 ToolResult）。它若转成 assistant(tool_calls)
        // 而无 tool 消息会导致 OpenAI 400。故应被跳过。
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q".into()));
        s.entries.push(ChatEntry::Assistant("前导".into()));
        s.entries.push(ChatEntry::ToolCall {
            id: "orphan".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        });
        // 无 ToolResult("orphan") → 孤立
        let h = s.history();
        assert_eq!(h.len(), 2, "孤立 ToolCall 应被跳过");
        assert_eq!(h[0].content, "q");
        assert_eq!(h[1].content, "前导");
        // 前导文本应是纯文本 assistant（无 tool_calls）
        assert!(h[1].tool_calls.is_empty());
    }

    #[test]
    fn history_keeps_complete_tool_chain_among_orphans() {
        // 孤立 + 完整的混合：完整的保留，孤立的跳过
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q".into()));
        s.entries.push(ChatEntry::ToolCall {
            id: "orphan".into(),
            name: "shell".into(),
            arguments: "{}".into(),
        });
        s.entries.push(ChatEntry::ToolCall {
            id: "ok".into(),
            name: "list_dir".into(),
            arguments: "{}".into(),
        });
        s.entries.push(ChatEntry::ToolResult {
            id: "ok".into(),
            name: "list_dir".into(),
            output: "a.txt".into(),
            is_error: false,
        });
        let h = s.history();
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].content, "q");
        // 只保留完整工具链 ok
        assert_eq!(h[1].role, Role::Assistant);
        assert_eq!(h[1].tool_calls.len(), 1);
        assert_eq!(h[1].tool_calls[0].id, "ok");
        assert_eq!(h[2].role, Role::Tool);
        assert_eq!(h[2].tool_call_id.as_deref(), Some("ok"));
    }

    #[test]
    fn multiline_input_joins_with_newline() {
        let mut s = ChatState::new();
        s.input.insert_str("第一行");
        s.input.insert_newline();
        s.input.insert_str("第二行");
        let (text, _) = s.submit().unwrap();
        assert_eq!(text, "第一行\n第二行");
    }

    #[test]
    fn push_tool_call_flushes_buffer_first() {
        let mut s = ChatState::new();
        s.streaming = true;
        s.streaming_buffer.push_str("让我看看");
        s.push_tool_call("c1".into(), "list_dir".into(), "{\"path\":\".\"}".into());
        // buffer 应被 flush 为 assistant 条目，再 push tool call
        assert_eq!(s.entries.len(), 2);
        assert!(matches!(s.entries[0], ChatEntry::Assistant(ref c) if c == "让我看看"));
        assert!(matches!(&s.entries[1], ChatEntry::ToolCall { name, .. } if name == "list_dir"));
        assert!(s.streaming_buffer.is_empty());
        assert!(s.streaming, "push_tool_call 不应退出 streaming");
    }

    #[test]
    fn push_tool_result_appends_after_call() {
        let mut s = ChatState::new();
        s.streaming = true;
        s.push_tool_call("c1".into(), "list_dir".into(), "{}".into());
        s.push_tool_result("c1".into(), "list_dir".into(), "a.txt".into(), false);
        assert_eq!(s.entries.len(), 2);
        assert!(matches!(&s.entries[1], ChatEntry::ToolResult { output, is_error, .. } if output == "a.txt" && !is_error));
    }

    /// 工具字符串转单行拼接（含 marker），便于断言折叠提示是否出现。
    fn tool_lines_to_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("")
    }

    #[test]
    fn tool_result_folded_when_exceeds_threshold() {
        let theme = Theme::resolve("cyberpunk");
        let expanded = HashSet::new();
        let entries = vec![ChatEntry::ToolResult {
            id: "c1".into(),
            name: "list_dir".into(),
            output: "l1\nl2\nl3\nl4\nl5".into(),
            is_error: false,
        }];
        let lines = render_entries(&entries, &theme, &expanded, 80);
        let text = tool_lines_to_text(&lines);
        // 5 行 > 3 → 折叠，出现提示与 +2（隐藏 2 行）
        assert!(text.contains("Ctrl+O 展开"), "折叠态应含 Ctrl+O 展开提示: {text}");
        assert!(text.contains("+2 行已折叠"), "应显示隐藏行数: {text}");
        // 仅显示最后 3 行（l3/l4/l5），不含 l1/l2
        assert!(!text.contains("l1"), "折叠态不应含 l1: {text}");
        assert!(!text.contains("l2"), "折叠态不应含 l2: {text}");
        assert!(text.contains("l3") && text.contains("l4") && text.contains("l5"));
    }

    #[test]
    fn tool_result_not_folded_at_or_below_threshold() {
        let theme = Theme::resolve("cyberpunk");
        let expanded = HashSet::new();
        // 恰好 3 行 → 不折叠
        let entries = vec![ChatEntry::ToolResult {
            id: "c1".into(),
            name: "list_dir".into(),
            output: "l1\nl2\nl3".into(),
            is_error: false,
        }];
        let lines = render_entries(&entries, &theme, &expanded, 80);
        let text = tool_lines_to_text(&lines);
        assert!(!text.contains("Ctrl+O"), "≤阈值不应有折叠提示: {text}");
        assert!(text.contains("l1") && text.contains("l2") && text.contains("l3"));
    }

    #[test]
    fn tool_result_expanded_shows_all_lines_and_collapse_hint() {
        let theme = Theme::resolve("cyberpunk");
        let mut expanded = HashSet::new();
        expanded.insert(0);
        let entries = vec![ChatEntry::ToolResult {
            id: "c1".into(),
            name: "list_dir".into(),
            output: "l1\nl2\nl3\nl4\nl5".into(),
            is_error: false,
        }];
        let lines = render_entries(&entries, &theme, &expanded, 80);
        let text = tool_lines_to_text(&lines);
        assert!(text.contains("Ctrl+O 折叠"), "展开态应含折叠提示: {text}");
        assert!(text.contains("共 5 行"), "应显示总行数: {text}");
        // 全部行可见
        for l in ["l1", "l2", "l3", "l4", "l5"] {
            assert!(text.contains(l), "展开态应含 {l}: {text}");
        }
    }

    #[test]
    fn tool_result_single_long_line_folded_by_visual_width() {
        // 回归测试：单行超长输出（如 MCP JSON 回显）按可视行数折叠，而非源行数。
        // 窄宽度（20 列）下，80 字符的单行会折为多行 → 触发折叠。
        let theme = Theme::resolve("cyberpunk");
        let expanded = HashSet::new();
        let long_json = "{\"has_response\":true,\"url\":\"http://example.com/view.php\",\"status_code\":200}";
        let entries = vec![ChatEntry::ToolResult {
            id: "c1".into(),
            name: "mcp_tool".into(),
            output: long_json.into(),
            is_error: false,
        }];
        // 宽度 20 → 内容宽 14 → 80 字符约 6 可视行 > 3 → 折叠
        let lines = render_entries(&entries, &theme, &expanded, 20);
        let text = tool_lines_to_text(&lines);
        assert!(
            text.contains("Ctrl+O 展开"),
            "单行长输出应按可视行数折叠: {text}"
        );
        // 宽度 200 → 内容宽 194 → 80 字符 1 可视行 ≤ 3 → 不折叠
        let lines_wide = render_entries(&entries, &theme, &expanded, 200);
        let text_wide = tool_lines_to_text(&lines_wide);
        assert!(
            !text_wide.contains("Ctrl+O"),
            "宽终端下单行不超阈值不应折叠: {text_wide}"
        );
    }

    #[test]
    fn toggle_last_tool_result_expansion_flips_state() {
        let theme = Theme::resolve("cyberpunk");
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::ToolResult {
            id: "c1".into(),
            name: "list_dir".into(),
            output: "l1\nl2\nl3\nl4\nl5".into(),
            is_error: false,
        });
        // 默认折叠
        s.prepare_render(&theme, 80);
        let folded_text = tool_lines_to_text(s.cached_history());
        assert!(folded_text.contains("Ctrl+O 展开"));
        assert!(!folded_text.contains("l1"));
        // 展开
        s.toggle_last_tool_result_expansion();
        s.prepare_render(&theme, 80);
        let expanded_text = tool_lines_to_text(s.cached_history());
        assert!(expanded_text.contains("Ctrl+O 折叠"));
        assert!(expanded_text.contains("l1"));
        // 再次切换 → 折叠
        s.toggle_last_tool_result_expansion();
        s.prepare_render(&theme, 80);
        let refolded_text = tool_lines_to_text(s.cached_history());
        assert!(refolded_text.contains("Ctrl+O 展开"));
        assert!(!refolded_text.contains("l1"));
    }

    #[test]
    fn toggle_with_no_tool_result_is_noop() {
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("hi".into()));
        s.toggle_last_tool_result_expansion(); // 不应 panic
        assert!(s.expanded_tool_results.is_empty());
    }

    #[test]
    fn toggle_targets_last_tool_result_among_many() {
        let theme = Theme::resolve("cyberpunk");
        let mut s = ChatState::new();
        // idx 0: ToolResult（5 行，折叠）
        s.entries.push(ChatEntry::ToolResult {
            id: "c1".into(),
            name: "t1".into(),
            output: "a1\na2\na3\na4\na5".into(),
            is_error: false,
        });
        s.entries.push(ChatEntry::Assistant("中间文本".into()));
        // idx 2: 最后一个 ToolResult（5 行，折叠）
        s.entries.push(ChatEntry::ToolResult {
            id: "c2".into(),
            name: "t2".into(),
            output: "b1\nb2\nb3\nb4\nb5".into(),
            is_error: false,
        });
        s.toggle_last_tool_result_expansion();
        // 仅 idx 2 应展开（idx 0 仍折叠）
        assert!(s.expanded_tool_results.contains(&2));
        assert!(!s.expanded_tool_results.contains(&0));
        s.prepare_render(&theme, 80);
        let text = tool_lines_to_text(s.cached_history());
        // idx 2 展开（含 b1 全显 + 折叠提示），idx 0 折叠（含 Ctrl+O 展开 + 不含 a1）
        assert!(text.contains("b1"));
        assert!(text.contains("Ctrl+O 折叠"));
        assert!(!text.contains("a1"));
    }

    #[test]
    fn clear_empties_entries() {
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("a".into()));
        s.entries.push(ChatEntry::Assistant("b".into()));
        s.clear();
        assert!(s.entries.is_empty());
    }

    #[test]
    fn take_input_returns_text_and_clears() {
        let mut s = ChatState::new();
        s.input.insert_str("hello");
        let text = s.take_input();
        assert_eq!(text, "hello");
        assert!(s.input.lines().join("").is_empty());
        assert!(!s.streaming, "take_input 不应改 streaming");
        assert!(s.entries.is_empty(), "take_input 不 push 条目");
    }

    #[test]
    fn prepare_render_caches_entries_lines() {
        let theme = Theme::resolve("cyberpunk");
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("你好".into()));
        s.entries.push(ChatEntry::Assistant("收到".into()));
        // 首次 prepare：缓存为空 + dirty → 重建
        s.prepare_render(&theme, 80);
        let cached = s.cached_history();
        assert!(!cached.is_empty(), "有条目则缓存非空");
        // render_entries 在每条目后附一空行；2 条目 → 至少 4 行（含分隔空行）
        assert!(cached.len() >= 4);
    }

    #[test]
    fn prepare_render_rebuilds_only_on_entry_count_change() {
        let theme = Theme::resolve("cyberpunk");
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q".into()));
        s.prepare_render(&theme, 80);
        let len_after_first = s.cached_history().len();
        // 无 entries 变化再次 prepare：缓存长度不变（复用，不重建）
        s.prepare_render(&theme, 80);
        assert_eq!(s.cached_history().len(), len_after_first);
        // 新增条目 → 重建，缓存增长
        s.entries.push(ChatEntry::Assistant("a".into()));
        s.prepare_render(&theme, 80);
        assert!(
            s.cached_history().len() > len_after_first,
            "条目增加后缓存应重建并增长"
        );
    }

    #[test]
    fn invalidate_cache_forces_rebuild_on_theme_change() {
        let theme_a = Theme::resolve("cyberpunk");
        let theme_b = Theme::resolve("nord");
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("hi".into()));
        s.prepare_render(&theme_a, 80);
        // entries.len() 不变，仅 theme 变 → 须 invalidate 才会重建
        s.invalidate_cache();
        s.prepare_render(&theme_b, 80);
        // 重建后缓存仍非空（内容样式已用 theme_b，这里仅验证重建发生不 panic）
        assert!(!s.cached_history().is_empty());
    }

    #[test]
    fn chatentry_serde_adjacent_tag_roundtrip() {
        // 验证 adjacently-tagged 表示可正确往返（含 newtype 与 struct 变体）
        let entries = vec![
            ChatEntry::User("你好".into()),
            ChatEntry::Assistant("multi\nline".into()),
            ChatEntry::ToolCall {
                id: "c1".into(),
                name: "list_dir".into(),
                arguments: "{\"path\":\".\"}".into(),
            },
            ChatEntry::ToolResult {
                id: "c1".into(),
                name: "list_dir".into(),
                output: "a.txt".into(),
                is_error: true,
            },
            ChatEntry::System("sys".into()),
        ];
        let json = serde_json::to_string(&entries).expect("序列化应成功");
        // adjacent tag：每个对象含 "kind" 与 "data"
        assert!(json.contains("\"kind\":\"User\""), "json: {json}");
        assert!(json.contains("\"kind\":\"ToolCall\""), "json: {json}");
        assert!(json.contains("\"data\":\"你好\""), "json: {json}");
        let loaded: Vec<ChatEntry> = serde_json::from_str(&json).expect("反序列化应成功");
        assert_eq!(loaded.len(), entries.len());
        assert!(matches!(&loaded[0], ChatEntry::User(c) if c == "你好"));
        assert!(
            matches!(&loaded[3], ChatEntry::ToolResult { is_error, output, .. } if *is_error && output == "a.txt")
        );
    }

    // ---- 历史滚动 ----

    #[test]
    fn new_state_follows_bottom() {
        let s = ChatState::new();
        assert!(s.is_following_bottom(), "新建状态应跟随底部");
    }

    #[test]
    fn scroll_history_no_op_without_metrics() {
        // 首帧前度量为 0：滚动应 no-op 且不 panic
        let mut s = ChatState::new();
        s.scroll_history(-5);
        assert!(s.is_following_bottom(), "max_scroll=0 时滚动到底 = 跟随");
    }

    #[test]
    fn scroll_up_then_follow_breaks() {
        let mut s = ChatState::new();
        // 模拟 render 回写：20 行总，10 行可见 → max_scroll=10
        s.set_scroll_metrics(20, 10);
        s.scroll_history(-4); // 上滚 4 行
        assert!(!s.is_following_bottom(), "上滚后应脱离跟随");
        // resolved 偏移 = 10 - 4 = 6
        assert_eq!(s.resolved_scroll_offset(10), 6);
    }

    #[test]
    fn scroll_down_to_bottom_re_follows() {
        let mut s = ChatState::new();
        s.set_scroll_metrics(20, 10);
        s.scroll_history(-4); // 偏移 6
        s.scroll_history(4); // 回到底 → 跟随
        assert!(s.is_following_bottom());
        assert_eq!(s.resolved_scroll_offset(10), 10);
    }

    #[test]
    fn scroll_clamps_at_top() {
        let mut s = ChatState::new();
        s.set_scroll_metrics(20, 10);
        s.scroll_history(-100); // 远超顶部
        assert_eq!(s.resolved_scroll_offset(10), 0, "顶部钳制为 0");
    }

    #[test]
    fn page_scroll_uses_visible_height() {
        let mut s = ChatState::new();
        s.set_scroll_metrics(40, 10);
        // PageUp: delta = -(visible=10)
        s.scroll_history(-10);
        assert_eq!(s.resolved_scroll_offset(30), 20, "max=30, 上滚 10 → 偏移 20");
    }

    #[test]
    fn scroll_to_bottom_resets_follow() {
        let mut s = ChatState::new();
        s.set_scroll_metrics(20, 10);
        s.scroll_history(-4);
        s.scroll_to_bottom();
        assert!(s.is_following_bottom());
    }

    #[test]
    fn submit_resets_scroll_to_follow() {
        let mut s = ChatState::new();
        s.set_scroll_metrics(20, 10);
        s.scroll_history(-4);
        s.input.insert_str("x");
        s.submit();
        assert!(s.is_following_bottom(), "submit 后应恢复跟随新响应");
    }

    #[test]
    fn clear_resets_scroll_to_follow() {
        let mut s = ChatState::new();
        s.set_scroll_metrics(20, 10);
        s.scroll_history(-4);
        s.clear();
        assert!(s.is_following_bottom());
    }

    // ---- 斜杠命令补全菜单 ----

    #[test]
    fn slash_menu_opens_on_slash_input() {
        let mut s = ChatState::new();
        s.input.insert_str("/");
        s.update_slash_menu();
        assert!(s.slash_menu.open, "输入 `/` 应打开菜单");
        assert_eq!(s.slash_menu.filtered.len(), crate::slash::COMMANDS.len());
    }

    #[test]
    fn slash_menu_filters_by_prefix() {
        let mut s = ChatState::new();
        s.input.insert_str("/mo");
        s.update_slash_menu();
        assert!(s.slash_menu.open);
        let names: Vec<&str> = s.slash_menu.filtered.iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["/mode", "/model"]);
    }

    #[test]
    fn slash_menu_closes_on_space_for_paramless_command() {
        let mut s = ChatState::new();
        s.input.insert_str("/help ");
        s.update_slash_menu();
        assert!(!s.slash_menu.open, "无参数命令含空格应关闭菜单");
    }

    #[test]
    fn slash_menu_param_mode_opens_for_think() {
        let mut s = ChatState::new();
        s.input.insert_str("/think ");
        s.update_slash_menu();
        assert!(s.slash_menu.open, "/think 应进入参数补全模式");
        assert_eq!(s.slash_menu.mode, crate::chat::SlashMenuMode::Param);
        assert_eq!(s.slash_menu.params.len(), 5);
    }

    #[test]
    fn slash_menu_param_filters_by_prefix() {
        let mut s = ChatState::new();
        s.input.insert_str("/think h");
        s.update_slash_menu();
        assert!(s.slash_menu.open);
        assert_eq!(s.slash_menu.params, vec!["high"]);
    }

    #[test]
    fn slash_menu_param_closes_on_no_match() {
        let mut s = ChatState::new();
        s.input.insert_str("/think xyz");
        s.update_slash_menu();
        assert!(!s.slash_menu.open, "参数无匹配应关闭菜单");
    }

    #[test]
    fn slash_menu_param_closes_on_second_space() {
        let mut s = ChatState::new();
        s.input.insert_str("/think low ");
        s.update_slash_menu();
        assert!(!s.slash_menu.open, "参数含空格（已完成）应关闭菜单");
    }

    #[test]
    fn slash_menu_param_complete_via_tab() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = ChatState::new();
        s.input.insert_str("/think au");
        s.update_slash_menu();
        assert!(s.slash_menu.open);
        assert_eq!(s.slash_menu.params, vec!["auto"]);
        // Tab 补全参数
        assert!(s.slash_menu_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert!(!s.slash_menu.open, "补全后应关闭菜单");
        assert_eq!(s.input.lines().join(""), "/think auto");
    }

    #[test]
    fn slash_menu_command_complete_then_enters_param_mode() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = ChatState::new();
        s.input.insert_str("/thin");
        s.update_slash_menu();
        assert!(s.slash_menu.open);
        assert_eq!(s.slash_menu.mode, crate::chat::SlashMenuMode::Command);
        // Enter 补全命令名 → 自动进入 Param 模式
        assert!(s.slash_menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        assert_eq!(s.input.lines().join(""), "/think ");
        assert!(s.slash_menu.open, "有参数建议时应自动进入 Param 模式");
        assert_eq!(s.slash_menu.mode, crate::chat::SlashMenuMode::Param);
    }

    #[test]
    fn slash_menu_closes_on_non_slash() {
        let mut s = ChatState::new();
        s.input.insert_str("/");
        s.update_slash_menu();
        assert!(s.slash_menu.open);
        s.input.clear();
        s.input.insert_str("hello");
        s.update_slash_menu();
        assert!(!s.slash_menu.open);
    }

    #[test]
    fn slash_menu_closes_when_no_match() {
        let mut s = ChatState::new();
        s.input.insert_str("/zzz");
        s.update_slash_menu();
        assert!(!s.slash_menu.open, "无匹配应关闭菜单");
    }

    #[test]
    fn slash_menu_key_navigates_and_completes() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        fn key(code: KeyCode) -> KeyEvent {
            KeyEvent::new(code, KeyModifiers::NONE)
        }
        let mut s = ChatState::new();
        s.input.insert_str("/mo");
        s.update_slash_menu();
        assert!(s.slash_menu.open);
        // Down: 0 → 1 (/model)
        assert!(s.slash_menu_key(key(KeyCode::Down)));
        assert_eq!(s.slash_menu.selected, 1);
        // Enter: 补全 /model + 空格，关闭菜单
        assert!(s.slash_menu_key(key(KeyCode::Enter)));
        assert!(!s.slash_menu.open);
        assert_eq!(s.input.lines().join(""), "/model ");
    }

    #[test]
    fn slash_menu_key_esc_closes_without_complete() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = ChatState::new();
        s.input.insert_str("/he");
        s.update_slash_menu();
        assert!(s.slash_menu.open);
        assert!(s.slash_menu_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!s.slash_menu.open, "Esc 应关闭菜单");
        assert_eq!(s.input.lines().join(""), "/he", "Esc 不补全，保留原输入");
    }

    #[test]
    fn slash_menu_key_returns_false_when_closed() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = ChatState::new();
        // 未打开：任何键都返回 false（交正常输入路径）
        assert!(!s.slash_menu_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        assert!(!s.slash_menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    }

    #[test]
    fn slash_menu_key_unconsumed_for_letters() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut s = ChatState::new();
        s.input.insert_str("/");
        s.update_slash_menu();
        // 字母键未被菜单消费 → 返回 false（App 会把字符送 textarea 再 refilter）
        assert!(!s.slash_menu_key(KeyEvent::new(
            KeyCode::Char('m'),
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn take_input_closes_slash_menu() {
        let mut s = ChatState::new();
        s.input.insert_str("/mo");
        s.update_slash_menu();
        assert!(s.slash_menu.open);
        let _ = s.take_input();
        assert!(!s.slash_menu.open);
    }

    // ---- 输入历史呼出（↑/↓）----

    fn submit_text(s: &mut ChatState, text: &str) {
        s.input.insert_str(text);
        // submit 置 streaming=true；为连续提交，复位 streaming
        let _ = s.submit();
        s.streaming = false;
    }

    #[test]
    fn submit_records_to_input_history() {
        let mut s = ChatState::new();
        submit_text(&mut s, "你好");
        submit_text(&mut s, "再见");
        assert_eq!(s.input_history.entries, vec!["你好", "再见"]);
    }

    #[test]
    fn input_history_record_dedups_adjacent() {
        let mut s = ChatState::new();
        submit_text(&mut s, "same");
        submit_text(&mut s, "same"); // 相邻重复 → 跳过
        submit_text(&mut s, "other");
        assert_eq!(s.input_history.entries, vec!["same", "other"]);
    }

    #[test]
    fn input_history_record_skips_blank() {
        let mut s = ChatState::new();
        s.input.insert_str("   ");
        let _ = s.submit(); // 空白 → None，不记录
        assert!(s.input_history.entries.is_empty());
    }

    #[test]
    fn history_prev_recalls_only_when_input_empty() {
        let mut s = ChatState::new();
        submit_text(&mut s, "first");
        submit_text(&mut s, "second");
        // 非空输入：↑ 不呼出（返回 false）
        s.input.insert_str("typing");
        assert!(!s.history_prev());
        // 清空后 ↑ 呼出最新
        s.input.clear();
        assert!(s.history_prev());
        assert_eq!(s.input.lines().join("\n"), "second");
    }

    #[test]
    fn history_prev_navigates_older() {
        let mut s = ChatState::new();
        submit_text(&mut s, "a");
        submit_text(&mut s, "b");
        submit_text(&mut s, "c");
        s.input.clear();
        // ↑ → c（最新），再 ↑ → b，再 ↑ → a，再 ↑ → 保持 a
        assert!(s.history_prev());
        assert_eq!(s.input.lines().join("\n"), "c");
        assert!(s.history_prev());
        assert_eq!(s.input.lines().join("\n"), "b");
        assert!(s.history_prev());
        assert_eq!(s.input.lines().join("\n"), "a");
        assert!(s.history_prev(), "已到最早仍返回 true（保持）");
        assert_eq!(s.input.lines().join("\n"), "a");
    }

    #[test]
    fn history_next_navigates_newer_and_clears_at_end() {
        let mut s = ChatState::new();
        submit_text(&mut s, "a");
        submit_text(&mut s, "b");
        s.input.clear();
        // 先 ↑ 进入浏览态并走到最早 a
        s.history_prev(); // ↑ → b（最新）
        s.history_prev(); // ↑ → a（最早）
        s.history_prev(); // 已到最早，保持 a
        assert_eq!(s.input.lines().join("\n"), "a");
        // ↓ → b
        assert!(s.history_next());
        assert_eq!(s.input.lines().join("\n"), "b");
        // ↓ 到头 → 清空 + 退出浏览
        assert!(s.history_next());
        assert!(s.input.lines().iter().all(|l| l.is_empty()), "到头应清空输入");
        // 再 ↓：未浏览态 → 返回 false
        assert!(!s.history_next());
    }

    #[test]
    fn history_next_not_browsing_returns_false() {
        let mut s = ChatState::new();
        submit_text(&mut s, "a");
        // 未按过 ↑（未浏览）→ ↓ 不处理
        assert!(!s.history_next());
    }

    #[test]
    fn history_prev_empty_history_returns_false() {
        let mut s = ChatState::new();
        s.input.clear();
        assert!(!s.history_prev(), "无历史时 ↑ 返回 false");
    }

    #[test]
    fn seed_input_history_from_user_entries() {
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("q1".into()));
        s.entries.push(ChatEntry::Assistant("a1".into()));
        s.entries.push(ChatEntry::User("q2".into()));
        s.seed_input_history();
        assert_eq!(s.input_history.entries, vec!["q1", "q2"], "应只取 User 条目");
    }

    #[test]
    fn seed_input_history_dedups_adjacent() {
        let mut s = ChatState::new();
        s.entries.push(ChatEntry::User("dup".into()));
        s.entries.push(ChatEntry::User("dup".into())); // 相邻重复
        s.entries.push(ChatEntry::User("uniq".into()));
        s.seed_input_history();
        assert_eq!(s.input_history.entries, vec!["dup", "uniq"]);
    }
}
