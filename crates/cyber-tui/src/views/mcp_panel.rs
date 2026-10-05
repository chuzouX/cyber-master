//! MCP 服务器与工具全屏控制中心视图 (Model Context Protocol Panel)
//!
//! 提供三栏流式高密度布局：
//! - 左栏：MCP 服务列表与连接健康状态、快速筛选
//! - 中栏：当前选中服务配置详情、环境、超时与连通延迟
//! - 右栏：服务暴露的工具清单及其 inputSchema JSON 预览（带语法着色）
//!
//! 包含预设模板抽屉（A 键）、编辑表单模态层（E 键）、删除二次确认（D 键）、测活重连（R 键）
//! 与原子写持久化（Ctrl+S）。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
    Frame,
};
use serde_json::Value;

use cyber_mcp::{
    McpRegistry, McpServerSpec, McpServersConfig, McpToolSchema, McpTransport, MCP_PRESETS,
};

use crate::theme::Theme;
use crate::views::mcp_form::{render_form, McpFormAction, McpFormState};

use super::clip_cells_ellipsis;

/// MCP 面板焦点窗格
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum McpFocusPane {
    #[default]
    ServerList, // 左侧：服务列表
    ServerDetail,  // 中间：当前服务配置/环境/连接健康度
    ToolsList,     // 右侧上部：服务暴露的工具列表
    SchemaPreview, // 右侧下部：选中工具的完整参数 JSON Schema
}

impl McpFocusPane {
    pub fn title(self) -> &'static str {
        match self {
            Self::ServerList => "1. 服务列表 (Servers)",
            Self::ServerDetail => "2. 配置详情 (Config & Health)",
            Self::ToolsList => "3. 工具清单 (Tools)",
            Self::SchemaPreview => "4. 参数 Schema (inputSchema)",
        }
    }
}

/// 单个 MCP 服务的面板展示模型
#[derive(Clone, Debug)]
pub struct McpServerItem {
    pub spec: McpServerSpec,
    pub connected: bool,
    pub tool_count: usize,
    pub latency: Option<Duration>,
    pub error: Option<String>,
    pub tools: Vec<McpToolSchema>,
    pub disabled: bool,
}

impl McpServerItem {
    pub fn from_spec(spec: McpServerSpec, mcp_registry: Option<&McpRegistry>) -> Self {
        let (connected, tool_count, tools) = match mcp_registry {
            Some(mcp) => {
                let cnt = mcp.tool_count(&spec.name);
                let is_conn = cnt.is_some();
                let tool_list = mcp
                    .server_tools(&spec.name)
                    .map(|ts| ts.to_vec())
                    .unwrap_or_default();
                (is_conn, cnt.unwrap_or(0), tool_list)
            }
            None => (false, 0, Vec::new()),
        };

        Self {
            spec,
            connected,
            tool_count,
            latency: if connected {
                Some(Duration::from_millis(12))
            } else {
                None
            },
            error: None,
            tools,
            disabled: false,
        }
    }
}

/// 按键交互产生的动作意图
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpPanelAction {
    None,
    Close,
    Save,
    TestConnection(String),
}

/// MCP 面板运行时完整状态
#[derive(Clone, Debug)]
pub struct McpPanelState {
    pub servers: Vec<McpServerItem>,
    pub focus: McpFocusPane,
    pub selected_server: usize,
    pub selected_tool: usize,
    pub tool_scroll: usize,
    pub schema_scroll: usize,
    pub detail_scroll: usize,
    pub filter: String,
    pub filtering: bool,
    pub testing_connection: bool,
    pub last_test_result: Option<Result<Duration, String>>,
    pub pending_delete: Option<String>,
    pub show_preset_picker: bool,
    pub preset_selected: usize,
    pub show_help: bool,
    pub form: Option<McpFormState>,
    pub dirty: bool,
    pub status_message: Option<(String, Instant)>,
    pub config_path: PathBuf,
    pub show_sensitive_headers: bool,
}

impl Default for McpPanelState {
    fn default() -> Self {
        Self {
            servers: Vec::new(),
            focus: McpFocusPane::ServerList,
            selected_server: 0,
            selected_tool: 0,
            tool_scroll: 0,
            schema_scroll: 0,
            detail_scroll: 0,
            filter: String::new(),
            filtering: false,
            testing_connection: false,
            last_test_result: None,
            pending_delete: None,
            show_preset_picker: false,
            preset_selected: 0,
            show_help: false,
            form: None,
            dirty: false,
            status_message: None,
            config_path: PathBuf::from("~/.cyber/mcp/servers.toml"),
            show_sensitive_headers: false,
        }
    }
}

impl McpPanelState {
    /// 从当前配置和连接注册表构造
    pub fn new(
        config: McpServersConfig,
        mcp_registry: Option<&McpRegistry>,
        config_path: PathBuf,
    ) -> Self {
        let servers: Vec<McpServerItem> = config
            .servers
            .into_iter()
            .map(|spec| McpServerItem::from_spec(spec, mcp_registry))
            .collect();

        Self {
            servers,
            config_path,
            ..Default::default()
        }
    }

    /// 设置临时状态栏提示（自动附带时间戳）
    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status_message = Some((msg.into(), Instant::now()));
    }

    /// 获取有效的状态提示文案（5 秒内有效）
    pub fn current_status(&self) -> Option<&str> {
        self.status_message.as_ref().and_then(|(msg, time)| {
            if time.elapsed() < Duration::from_secs(5) {
                Some(msg.as_str())
            } else {
                None
            }
        })
    }
    /// 切换请求标头敏感数据脱敏/明文展示状态
    pub fn toggle_sensitive_headers(&mut self) {
        self.show_sensitive_headers = !self.show_sensitive_headers;
        if self.show_sensitive_headers {
            self.set_status("已显示请求标头明文数据 (按 Ctrl+D 重新脱敏)");
        } else {
            self.set_status("已脱敏遮蔽请求标头敏感数据 (按 Ctrl+D 查看)");
        }
    }

    /// 当前选中的服务器
    pub fn selected_server_item(&self) -> Option<&McpServerItem> {
        let filtered = self.filtered_server_indices();
        filtered
            .get(self.selected_server)
            .and_then(|&idx| self.servers.get(idx))
    }

    /// 当前选中的服务器（可变引用）
    pub fn selected_server_item_mut(&mut self) -> Option<&mut McpServerItem> {
        let filtered = self.filtered_server_indices();
        let actual_idx = filtered.get(self.selected_server).copied();
        actual_idx.and_then(|idx| self.servers.get_mut(idx))
    }

    /// 满足搜索关键词的服务器原索引列表
    pub fn filtered_server_indices(&self) -> Vec<usize> {
        let kw = self.filter.trim().to_lowercase();
        self.servers
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                if kw.is_empty() {
                    return true;
                }
                s.spec.name.to_lowercase().contains(&kw)
                    || s.tools.iter().any(|t| {
                        t.name.to_lowercase().contains(&kw)
                            || t.description.to_lowercase().contains(&kw)
                    })
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// 当前选中服务器满足过滤的工具列表
    pub fn filtered_tools(&self) -> Vec<&McpToolSchema> {
        let Some(server) = self.selected_server_item() else {
            return Vec::new();
        };
        let kw = self.filter.trim().to_lowercase();
        server
            .tools
            .iter()
            .filter(|t| {
                if kw.is_empty() {
                    return true;
                }
                t.name.to_lowercase().contains(&kw) || t.description.to_lowercase().contains(&kw)
            })
            .collect()
    }

    /// 顺时针切换焦点窗格
    pub fn next_focus(&mut self) {
        self.focus = match self.focus {
            McpFocusPane::ServerList => McpFocusPane::ServerDetail,
            McpFocusPane::ServerDetail => McpFocusPane::ToolsList,
            McpFocusPane::ToolsList => McpFocusPane::SchemaPreview,
            McpFocusPane::SchemaPreview => McpFocusPane::ServerList,
        };
    }

    /// 逆时针切换焦点窗格
    pub fn prev_focus(&mut self) {
        self.focus = match self.focus {
            McpFocusPane::ServerList => McpFocusPane::SchemaPreview,
            McpFocusPane::ServerDetail => McpFocusPane::ServerList,
            McpFocusPane::ToolsList => McpFocusPane::ServerDetail,
            McpFocusPane::SchemaPreview => McpFocusPane::ToolsList,
        };
    }

    /// 光标向上移动
    pub fn move_up(&mut self) {
        match self.focus {
            McpFocusPane::ServerList => {
                let max = self.filtered_server_indices().len();
                if max > 0 && self.selected_server > 0 {
                    self.selected_server -= 1;
                    self.selected_tool = 0;
                    self.tool_scroll = 0;
                    self.schema_scroll = 0;
                    self.detail_scroll = 0;
                    self.pending_delete = None;
                }
            }
            McpFocusPane::ServerDetail => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
            }
            McpFocusPane::ToolsList => {
                if self.selected_tool > 0 {
                    self.selected_tool -= 1;
                    self.schema_scroll = 0;
                    if self.selected_tool < self.tool_scroll {
                        self.tool_scroll = self.selected_tool;
                    }
                }
            }
            McpFocusPane::SchemaPreview => {
                self.schema_scroll = self.schema_scroll.saturating_sub(1);
            }
        }
    }

    /// 光标向下移动
    pub fn move_down(&mut self) {
        match self.focus {
            McpFocusPane::ServerList => {
                let max = self.filtered_server_indices().len();
                if max > 0 && self.selected_server + 1 < max {
                    self.selected_server += 1;
                    self.selected_tool = 0;
                    self.tool_scroll = 0;
                    self.schema_scroll = 0;
                    self.detail_scroll = 0;
                    self.pending_delete = None;
                }
            }
            McpFocusPane::ServerDetail => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
            }
            McpFocusPane::ToolsList => {
                let tools_len = self.filtered_tools().len();
                if tools_len > 0 && self.selected_tool + 1 < tools_len {
                    self.selected_tool += 1;
                    self.schema_scroll = 0;
                }
            }
            McpFocusPane::SchemaPreview => {
                self.schema_scroll = self.schema_scroll.saturating_add(1);
            }
        }
    }

    /// 向上滚动（鼠标滚轮或 PgUp）
    pub fn scroll_up(&mut self, delta: usize) {
        match self.focus {
            McpFocusPane::ServerList => {
                for _ in 0..delta {
                    self.move_up();
                }
            }
            McpFocusPane::ServerDetail => {
                self.detail_scroll = self.detail_scroll.saturating_sub(delta);
            }
            McpFocusPane::ToolsList => {
                self.selected_tool = self.selected_tool.saturating_sub(delta);
                if self.selected_tool < self.tool_scroll {
                    self.tool_scroll = self.selected_tool;
                }
                self.schema_scroll = 0;
            }
            McpFocusPane::SchemaPreview => {
                self.schema_scroll = self.schema_scroll.saturating_sub(delta);
            }
        }
    }

    /// 向下滚动（鼠标滚轮或 PgDn）
    pub fn scroll_down(&mut self, delta: usize) {
        match self.focus {
            McpFocusPane::ServerList => {
                for _ in 0..delta {
                    self.move_down();
                }
            }
            McpFocusPane::ServerDetail => {
                self.detail_scroll = self.detail_scroll.saturating_add(delta);
            }
            McpFocusPane::ToolsList => {
                let tools_len = self.filtered_tools().len();
                if tools_len > 0 {
                    self.selected_tool = (self.selected_tool + delta).min(tools_len - 1);
                    self.schema_scroll = 0;
                }
            }
            McpFocusPane::SchemaPreview => {
                self.schema_scroll = self.schema_scroll.saturating_add(delta);
            }
        }
    }

    /// 切换当前服务的临时启用/禁用状态
    pub fn toggle_disabled(&mut self) {
        let status_info = if let Some(item) = self.selected_server_item_mut() {
            item.disabled = !item.disabled;
            let name = item.spec.name.clone();
            let state = if item.disabled {
                "临时禁用"
            } else {
                "重新启用"
            };
            Some((name, state))
        } else {
            None
        };
        if let Some((name, state)) = status_info {
            self.dirty = true;
            self.set_status(format!("已将 MCP 服务 [{name}] {state}"));
        }
    }

    /// 请求删除当前选中的服务（双击确认逻辑）
    pub fn request_delete(&mut self) -> Option<String> {
        let item = self.selected_server_item()?;
        let name = item.spec.name.clone();
        if self.pending_delete.as_deref() == Some(&name) {
            // 第二次确认
            self.pending_delete = None;
            self.delete_server(&name);
            Some(name)
        } else {
            // 首次按下，提示二次确认
            self.pending_delete = Some(name.clone());
            self.set_status(format!("⚠️ 再按一次 D 确认删除服务 [{name}]，按其他键取消"));
            None
        }
    }

    /// 执行删除已确认的服务
    pub fn delete_server(&mut self, name: &str) {
        self.servers.retain(|s| s.spec.name != name);
        self.dirty = true;
        let filtered_len = self.filtered_server_indices().len();
        if self.selected_server >= filtered_len && filtered_len > 0 {
            self.selected_server = filtered_len - 1;
        }
        self.selected_tool = 0;
        self.tool_scroll = 0;
        self.schema_scroll = 0;
        self.set_status(format!("已删除 MCP 服务 [{name}] (按 Ctrl+S 保存生效)"));
    }

    /// 打开基于预设的新增向导
    pub fn open_preset_picker(&mut self) {
        self.show_preset_picker = true;
        self.preset_selected = 0;
        self.pending_delete = None;
    }

    /// 关闭预设向导
    pub fn close_preset_picker(&mut self) {
        self.show_preset_picker = false;
    }

    /// 选择预设并进入表单编辑
    pub fn confirm_preset(&mut self) {
        if let Some(preset) = MCP_PRESETS.get(self.preset_selected) {
            let spec = preset.to_server_spec();
            let mut form = McpFormState::from_spec(&spec);
            // 预设模式视为新增，解除 original_name 限制
            form.original_name = None;
            form.focused = 0;
            self.form = Some(form);
            self.show_preset_picker = false;
        }
    }

    /// 打开当前选中服务的编辑表单
    pub fn open_edit_form(&mut self) {
        if let Some(server) = self.selected_server_item() {
            let form = McpFormState::from_spec(&server.spec);
            self.form = Some(form);
            self.pending_delete = None;
        }
    }

    /// 保存表单输入到服务器列表
    pub fn save_form(&mut self) -> Result<(), String> {
        let Some(form) = &self.form else {
            return Ok(());
        };
        let existing_names: Vec<&str> = self.servers.iter().map(|s| s.spec.name.as_str()).collect();
        let new_spec = form.into_spec(&existing_names)?;
        let name = new_spec.name.clone();

        if let Some(orig) = &form.original_name {
            if let Some(pos) = self.servers.iter().position(|s| s.spec.name == *orig) {
                self.servers[pos].spec = new_spec;
            }
        } else if let Some(pos) = self.servers.iter().position(|s| s.spec.name == name) {
            self.servers[pos].spec = new_spec;
        } else {
            self.servers.push(McpServerItem {
                spec: new_spec,
                connected: false,
                tool_count: 0,
                latency: None,
                error: None,
                tools: Vec::new(),
                disabled: false,
            });
            let filtered_len = self.filtered_server_indices().len();
            if filtered_len > 0 {
                self.selected_server = filtered_len - 1;
            }
            self.selected_tool = 0;
            self.tool_scroll = 0;
        }

        self.form = None;
        self.dirty = true;
        self.set_status(format!("已更新服务 [{name}] 配置 (按 Ctrl+S 保存生效)"));
        Ok(())
    }

    /// 持久化覆写至 servers.toml
    pub fn save_to_disk(&mut self) -> Result<(), String> {
        let config = McpServersConfig {
            servers: self.servers.iter().map(|s| s.spec.clone()).collect(),
        };
        config
            .save(&self.config_path)
            .map_err(|e| format!("保存 servers.toml 失败: {e}"))?;
        self.dirty = false;
        self.set_status("已将 MCP 服务器配置持久化至磁盘");
        Ok(())
    }

    /// 处理键盘按键分派
    pub fn handle_key(&mut self, key: KeyEvent) -> McpPanelAction {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);

        // 1. 若当前处于编辑表单中，直接委托给表单处理
        if let Some(form) = self.form.as_mut() {
            let action = form.handle_key(key);
            match action {
                McpFormAction::Save => {
                    if let Err(e) = self.save_form() {
                        self.set_status(format!("保存表单错误: {e}"));
                    }
                }
                McpFormAction::Cancel => {
                    self.form = None;
                    self.set_status("已取消编辑");
                }
                McpFormAction::None => {}
            }
            return McpPanelAction::None;
        }

        // 2. 帮助弹窗
        if self.show_help {
            if matches!(
                key.code,
                KeyCode::Esc
                    | KeyCode::Char('?')
                    | KeyCode::Char('q')
                    | KeyCode::Enter
                    | KeyCode::Char(' ')
            ) {
                self.show_help = false;
            }
            return McpPanelAction::None;
        }

        // 3. 预设向导弹窗
        if self.show_preset_picker {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.show_preset_picker = false;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if self.preset_selected > 0 {
                        self.preset_selected -= 1;
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if self.preset_selected + 1 < MCP_PRESETS.len() {
                        self.preset_selected += 1;
                    }
                }
                KeyCode::Char(ch @ '1'..='6') => {
                    let idx = (ch as usize) - ('1' as usize);
                    if idx < MCP_PRESETS.len() {
                        self.preset_selected = idx;
                        self.confirm_preset();
                    }
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.confirm_preset();
                }
                _ => {}
            }
            return McpPanelAction::None;
        }

        // 4. 搜索筛选输入态
        if self.filtering {
            match key.code {
                KeyCode::Esc => {
                    self.filtering = false;
                    self.filter.clear();
                }
                KeyCode::Enter => {
                    self.filtering = false;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.selected_server = 0;
                    self.selected_tool = 0;
                    self.tool_scroll = 0;
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.selected_server = 0;
                    self.selected_tool = 0;
                    self.tool_scroll = 0;
                }
                _ => {}
            }
            return McpPanelAction::None;
        }

        // 5. 常规控制中心键盘导航
        if control && (key.code == KeyCode::Char('s') || key.code == KeyCode::Char('S')) {
            return McpPanelAction::Save;
        }
        if control && (key.code == KeyCode::Char('d') || key.code == KeyCode::Char('D')) {
            self.toggle_sensitive_headers();
            return McpPanelAction::None;
        }

        match key.code {
            KeyCode::Esc => {
                if self.pending_delete.is_some() {
                    self.pending_delete = None;
                    self.set_status("已取消删除操作");
                } else if self.focus == McpFocusPane::SchemaPreview {
                    self.focus = McpFocusPane::ToolsList;
                } else {
                    return McpPanelAction::Close;
                }
            }
            KeyCode::Char('q') | KeyCode::Char('Q') => {
                if self.pending_delete.is_some() {
                    self.pending_delete = None;
                } else {
                    return McpPanelAction::Close;
                }
            }
            KeyCode::Char('?') => {
                self.show_help = true;
            }
            KeyCode::Char('f') | KeyCode::Char('/') => {
                self.filtering = true;
                self.filter.clear();
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prev_focus();
                } else {
                    self.next_focus();
                }
            }
            KeyCode::BackTab => {
                self.prev_focus();
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.prev_focus();
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.next_focus();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_up();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_down();
            }
            KeyCode::PageUp => {
                self.scroll_up(5);
            }
            KeyCode::PageDown => {
                self.scroll_down(5);
            }
            KeyCode::Enter => match self.focus {
                McpFocusPane::ServerList => {
                    self.focus = McpFocusPane::ToolsList;
                }
                McpFocusPane::ServerDetail => {
                    self.focus = McpFocusPane::ToolsList;
                }
                McpFocusPane::ToolsList => {
                    if !self.filtered_tools().is_empty() {
                        self.focus = McpFocusPane::SchemaPreview;
                    }
                }
                McpFocusPane::SchemaPreview => {
                    self.focus = McpFocusPane::ToolsList;
                }
            },
            KeyCode::Char('a') | KeyCode::Char('A') => {
                self.open_preset_picker();
            }
            KeyCode::Char('e') | KeyCode::Char('E') => {
                self.open_edit_form();
            }
            KeyCode::Char('d') | KeyCode::Char('D') => {
                self.request_delete();
            }
            KeyCode::Char(' ') => {
                self.toggle_disabled();
            }
            KeyCode::Char('r') | KeyCode::Char('R') | KeyCode::Char('c') | KeyCode::Char('C') => {
                if let Some(item) = self.selected_server_item() {
                    let name = item.spec.name.clone();
                    return McpPanelAction::TestConnection(name);
                }
            }
            _ => {
                if self.pending_delete.is_some() {
                    self.pending_delete = None;
                    self.set_status("已取消删除操作");
                }
            }
        }

        McpPanelAction::None
    }
}

/// 渲染 MCP 全屏控制中心视图 (布满 100% 视口)
pub fn render_mcp_panel(frame: &mut Frame, area: Rect, state: &mut McpPanelState, theme: &Theme) {
    if area.width < 40 || area.height < 6 {
        return;
    }

    frame.render_widget(Clear, area);

    // 外层边框与标题
    let border_color = if state.dirty {
        Color::Rgb(255, 176, 0)
    } else {
        Color::Rgb(0, 220, 255)
    };
    let connected_count = state.servers.iter().filter(|s| s.connected).count();
    let total_servers = state.servers.len();
    let total_tools: usize = state.servers.iter().map(|s| s.tool_count).sum();

    let dirty_tag = if state.dirty {
        " [● 已修改待保存 (Ctrl+S)]"
    } else {
        ""
    };
    let title_line = Line::from(super::clipped_spans(
        vec![
            Span::styled(
                " 🌐 MCP 服务器与工具控制中心 (Model Context Protocol) ",
                Style::default()
                    .fg(Color::Rgb(0, 240, 255))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("[已连接: {connected_count}/{total_servers} 服务] [工具总数: {total_tools}] [快捷键: ? 帮助]{dirty_tag} "),
                Style::default().fg(Color::DarkGray),
            ),
        ],
        (area.width as usize).saturating_sub(3),
    ));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(title_line)
        .style(Style::default().bg(theme.bg).fg(theme.fg));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 垂直切分：主工作区 + 提示行 + 键位底栏
    let v_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // 工作区
            Constraint::Length(1), // 提示/状态行
            Constraint::Length(1), // 键位导航行
        ])
        .split(inner);

    let work_area = v_chunks[0];
    let tip_area = v_chunks[1];
    let nav_area = v_chunks[2];

    // 流式响应式分栏
    if work_area.width >= 90 {
        // 三栏布局: 30% / 35% / 35%
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Percentage(35),
                Constraint::Percentage(35),
            ])
            .split(work_area);

        render_server_list(frame, h_chunks[0], state, theme);
        render_server_detail(frame, h_chunks[1], state, theme);
        render_tools_and_schema(frame, h_chunks[2], state, theme);
    } else if work_area.width >= 60 {
        // 双栏紧凑布局
        let h_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(work_area);

        render_server_list(frame, h_chunks[0], state, theme);
        if state.focus == McpFocusPane::ServerDetail {
            render_server_detail(frame, h_chunks[1], state, theme);
        } else {
            render_tools_and_schema(frame, h_chunks[1], state, theme);
        }
    } else {
        // 单栏极小屏幕
        match state.focus {
            McpFocusPane::ServerList => render_server_list(frame, work_area, state, theme),
            McpFocusPane::ServerDetail => render_server_detail(frame, work_area, state, theme),
            McpFocusPane::ToolsList | McpFocusPane::SchemaPreview => {
                render_tools_and_schema(frame, work_area, state, theme)
            }
        }
    }

    // 渲染底栏
    render_footer(frame, tip_area, nav_area, state, theme);

    // 弹层渲染
    if state.show_preset_picker {
        render_preset_picker(frame, area, state, theme);
    } else if state.show_help {
        render_help_modal(frame, area, theme);
    } else if let Some(form) = state.form.as_mut() {
        form.prepare_render(theme);
        render_form(frame, area, theme, form);
    }
}

/// 脱敏遮蔽敏感标头数据（支持 Bearer Token、Basic Auth 及普通密钥凭据）
pub(crate) fn mask_sensitive_header(val: &str) -> String {
    let trimmed = val.trim();
    if trimmed.is_empty() {
        return "(空)".to_string();
    }
    let lower = trimmed.to_lowercase();
    if lower.starts_with("bearer ") {
        "Bearer •••••••••••• (已脱敏保护)".to_string()
    } else if lower.starts_with("basic ") {
        "Basic ••••••••".to_string()
    } else {
        "•••••••••••• (已脱敏保护)".to_string()
    }
}

/// 渲染左栏：服务列表与健康摘要
fn render_server_list(frame: &mut Frame, area: Rect, state: &McpPanelState, theme: &Theme) {
    let is_focused = state.focus == McpFocusPane::ServerList;
    let border_color = if is_focused {
        theme.accent
    } else {
        Color::DarkGray
    };

    let mut title_spans = vec![Span::styled(
        " 1. MCP 服务列表 (Servers) ",
        Style::default()
            .fg(if is_focused { theme.accent } else { theme.fg })
            .add_modifier(if is_focused {
                Modifier::BOLD
            } else {
                Modifier::empty()
            }),
    )];
    if state.filtering || !state.filter.is_empty() {
        title_spans.push(Span::styled(
            format!("[搜索: {}] ", state.filter),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Line::from(super::clipped_spans(
            title_spans,
            (area.width as usize).saturating_sub(3),
        )));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let filtered = state.filtered_server_indices();
    if filtered.is_empty() {
        let msg = if state.filter.is_empty() {
            "（暂无配置的 MCP 服务）\n按 A 键快速添加官方预设模板"
        } else {
            "（无匹配的 MCP 服务）\n按 Esc 清空筛选条件"
        };
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(theme.muted)),
            inner,
        );
        return;
    }

    let mut lines = Vec::new();
    for (list_idx, &server_idx) in filtered.iter().enumerate() {
        let server = &state.servers[server_idx];
        let is_selected = list_idx == state.selected_server;

        let marker = if is_selected { "▶ " } else { "  " };
        let (status_badge, status_color) = if server.disabled {
            ("[⏸]", Color::Rgb(255, 176, 0))
        } else if server.connected {
            ("[●]", Color::Green)
        } else if server.error.is_some() {
            ("[✖]", Color::Red)
        } else {
            ("[○]", Color::DarkGray)
        };

        let row_style = if is_selected {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };

        let name_style = if server.disabled {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::CROSSED_OUT)
        } else if is_selected {
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };

        let max_w = (inner.width as usize).saturating_sub(1);
        let tools_text = format!("({} 工具)", server.tool_count);
        use unicode_width::UnicodeWidthStr;
        let marker_w = UnicodeWidthStr::width(marker);
        let badge_w = UnicodeWidthStr::width(status_badge);
        let tools_w = UnicodeWidthStr::width(tools_text.as_str());
        let name_raw_w = UnicodeWidthStr::width(server.spec.name.as_str());

        let show_tools = max_w > marker_w + badge_w + 2 + name_raw_w + tools_w;
        let fixed_w = marker_w + badge_w + 1 + if show_tools { 1 + tools_w } else { 0 };
        let name_budget = max_w.saturating_sub(fixed_w);
        let clipped_name = clip_cells_ellipsis(&server.spec.name, name_budget);

        // 第一行：状态图标 + 服务名称 + 可选工具数
        let mut row_spans = vec![
            Span::styled(marker, Style::default().fg(theme.accent)),
            Span::styled(status_badge, Style::default().fg(status_color)),
            Span::raw(" "),
            Span::styled(clipped_name, name_style),
        ];
        if show_tools {
            row_spans.push(Span::raw(" "));
            row_spans.push(Span::styled(
                tools_text,
                Style::default().fg(if server.tool_count > 0 {
                    Color::Cyan
                } else {
                    theme.muted
                }),
            ));
        }
        lines.push(Line::from(row_spans).style(row_style));

        // 第二行：传输协议 + 超时 + 延迟摘要
        let rtt_str = server
            .latency
            .map(|d| format!("{}ms", d.as_millis()))
            .unwrap_or_else(|| "--".to_string());

        let sub_info = format!(
            "    传输: {:<5} │ 超时: {:>2}s │ 延迟: {:>4}",
            format!("{:?}", server.spec.transport).to_lowercase(),
            server.spec.timeout_secs,
            rtt_str,
        );
        let clipped_sub = clip_cells_ellipsis(&sub_info, max_w);
        lines.push(Line::styled(clipped_sub, Style::default().fg(theme.muted)).style(row_style));

        // 分割微空行
        lines.push(Line::raw(""));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

/// 渲染中栏：选中服务配置与连通健康度
fn render_server_detail(frame: &mut Frame, area: Rect, state: &McpPanelState, theme: &Theme) {
    let is_focused = state.focus == McpFocusPane::ServerDetail;
    let border_color = if is_focused {
        theme.accent
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Line::styled(
            " 2. 服务配置与连通度 (Config & Health) ",
            Style::default()
                .fg(if is_focused { theme.accent } else { theme.fg })
                .add_modifier(if is_focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let Some(server) = state.selected_server_item() else {
        frame.render_widget(
            Paragraph::new("请先从左侧列表选择一个 MCP 服务")
                .style(Style::default().fg(theme.muted)),
            inner,
        );
        return;
    };

    let mut lines = Vec::new();

    // 基本信息
    lines.push(Line::from(vec![
        Span::styled(
            "服务名称: ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            &server.spec.name,
            Style::default()
                .fg(theme.title)
                .add_modifier(Modifier::BOLD),
        ),
        if server.disabled {
            Span::styled(
                " [已临时禁用]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("")
        },
    ]));

    let transport_desc = match server.spec.transport {
        McpTransport::Stdio => "stdio (本地子进程 JSON-RPC)",
        McpTransport::Http => "http (Streamable HTTP / JSON-RPC)",
        McpTransport::Sse => "sse (Legacy Server-Sent Events)",
    };
    lines.push(Line::from(vec![
        Span::styled("传输协议: ", Style::default().fg(Color::Cyan)),
        Span::raw(transport_desc),
    ]));

    match server.spec.transport {
        McpTransport::Stdio => {
            lines.push(Line::from(vec![
                Span::styled("启动命令: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    server.spec.command.as_deref().unwrap_or("(未设置)"),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            if !server.spec.args.is_empty() {
                lines.push(Line::styled("启动参数: ", Style::default().fg(Color::Cyan)));
                for arg in &server.spec.args {
                    lines.push(Line::styled(
                        format!("  - {arg}"),
                        Style::default().fg(theme.fg),
                    ));
                }
            }
            if !server.spec.env.is_empty() {
                lines.push(Line::styled("环境变量: ", Style::default().fg(Color::Cyan)));
                for (k, v) in &server.spec.env {
                    lines.push(Line::styled(
                        format!("  - {k} = {v}"),
                        Style::default().fg(theme.muted),
                    ));
                }
            }
        }
        McpTransport::Http | McpTransport::Sse => {
            lines.push(Line::from(vec![
                Span::styled("服务端点: ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    server.spec.url.as_deref().unwrap_or("(未设置 URL)"),
                    Style::default().fg(Color::Yellow),
                ),
            ]));
            if !server.spec.headers.is_empty() {
                let header_title = if state.show_sensitive_headers {
                    Span::styled(
                        "请求标头 (已显示完整明文 · 按 Ctrl+D 脱敏隐藏): ",
                        Style::default().fg(Color::Yellow),
                    )
                } else {
                    Span::styled(
                        "请求标头 (默认脱敏保护 · 按 Ctrl+D 查看敏感数据): ",
                        Style::default().fg(Color::Cyan),
                    )
                };
                lines.push(Line::from(vec![header_title]));
                let mut sorted_headers: Vec<_> = server.spec.headers.iter().collect();
                sorted_headers.sort_by_key(|(k, _)| *k);
                for (k, v) in sorted_headers {
                    let display_val = if state.show_sensitive_headers {
                        v.clone()
                    } else {
                        mask_sensitive_header(v)
                    };
                    lines.push(Line::styled(
                        format!("  - {k}: {display_val}"),
                        Style::default().fg(theme.muted),
                    ));
                }
            }
        }
    }

    lines.push(Line::from(vec![
        Span::styled("握手超时: ", Style::default().fg(Color::Cyan)),
        Span::raw(format!("{} 秒", server.spec.timeout_secs)),
    ]));

    lines.push(Line::raw(""));

    // 健康度与运行状态
    lines.push(Line::styled(
        "── 状态与健康度 ──",
        Style::default().fg(Color::DarkGray),
    ));

    let status_line = if server.disabled {
        Line::from(vec![
            Span::styled("运行状态: ", Style::default().fg(theme.muted)),
            Span::styled(
                "[ ⏸ 已临时禁用 ]",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else if server.connected {
        Line::from(vec![
            Span::styled("运行状态: ", Style::default().fg(theme.muted)),
            Span::styled(
                "[ ● 握手成功 / 已就绪 ]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
        ])
    } else if let Some(err) = &server.error {
        Line::from(vec![
            Span::styled("运行状态: ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("[ ✖ 连接异常: {err} ]"),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled("运行状态: ", Style::default().fg(theme.muted)),
            Span::styled(
                "[ ○ 未连接 (按 R 立即测活) ]",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    };
    lines.push(status_line);

    if let Some(lat) = server.latency {
        lines.push(Line::from(vec![
            Span::styled("往返延迟: ", Style::default().fg(theme.muted)),
            Span::styled(
                format!("{}ms", lat.as_millis()),
                Style::default().fg(Color::Cyan),
            ),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("工具数量: ", Style::default().fg(theme.muted)),
        Span::styled(
            format!("{} 个已导出工具", server.tool_count),
            Style::default().fg(theme.fg),
        ),
    ]));

    lines.push(Line::raw(""));
    lines.push(Line::styled(
        "── 操作动作快捷指引 ──",
        Style::default().fg(Color::DarkGray),
    ));
    lines.push(Line::from(vec![
        Span::styled(
            "[R] ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("重新测活   "),
        Span::styled(
            "[E] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("编辑配置"),
    ]));
    lines.push(Line::from(vec![
        Span::styled(
            "[D] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw("删除服务   "),
        Span::styled(
            "[Space] ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("临时启用/禁用"),
    ]));

    let scroll = state.detail_scroll as u16;
    frame.render_widget(
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .wrap(Wrap { trim: false }),
        inner,
    );
}

/// 渲染右栏：暴露工具列表与 JSON Schema 参数预览
fn render_tools_and_schema(
    frame: &mut Frame,
    area: Rect,
    state: &mut McpPanelState,
    theme: &Theme,
) {
    let is_focused =
        state.focus == McpFocusPane::ToolsList || state.focus == McpFocusPane::SchemaPreview;
    let border_color = if is_focused {
        theme.accent
    } else {
        Color::DarkGray
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
        .title(Line::styled(
            " 3. 暴露工具与参数 (Tools & Schema) ",
            Style::default()
                .fg(if is_focused { theme.accent } else { theme.fg })
                .add_modifier(if is_focused {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                }),
        ));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let total_tools = state.filtered_tools().len();
    if total_tools == 0 {
        let empty_msg = "该服务当前未导出任何可用工具。\n\n排查建议：\n1. 若服务正在启动，按 R 重新连接探测；\n2. 若为 SSE 端点，请确保 transport 配置为 sse；\n3. 若为本地子进程，请检查命令与参数是否可在本地终端执行。";
        frame.render_widget(
            Paragraph::new(empty_msg)
                .style(Style::default().fg(theme.muted))
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }

    // 上下切分：上半部工具选择列表 (40%)，下半部 JSON Schema 详情 (60%)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(inner);

    let tools_block_base = Block::default().borders(Borders::BOTTOM);
    let tools_inner = tools_block_base.inner(chunks[0]);
    let visible_count = tools_inner.height as usize;
    if visible_count == 0 {
        return;
    }

    // 光标与滚动视口高亮跟随：确保 selected_tool 严格落在 [tool_scroll, tool_scroll + visible_count)
    if state.selected_tool >= total_tools {
        state.selected_tool = total_tools.saturating_sub(1);
    }
    if state.selected_tool < state.tool_scroll {
        state.tool_scroll = state.selected_tool;
    } else if state.selected_tool >= state.tool_scroll + visible_count {
        state.tool_scroll = state.selected_tool + 1 - visible_count;
    }
    let max_scroll = total_tools.saturating_sub(visible_count);
    if state.tool_scroll > max_scroll {
        state.tool_scroll = max_scroll;
    }

    // 1. 上半部：工具列表（严格按可视窗口切片，杜绝边框越界）
    let tools = state.filtered_tools();
    let max_text_width = (tools_inner.width as usize).saturating_sub(4);
    let mut tool_lines = Vec::new();
    for (idx, tool) in tools
        .iter()
        .enumerate()
        .skip(state.tool_scroll)
        .take(visible_count)
    {
        let is_selected = idx == state.selected_tool;
        let marker = if is_selected { "▶ " } else { "  " };

        let style = if is_selected {
            Style::default()
                .bg(theme.sel_bg)
                .fg(theme.title)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };

        let single_line_desc = tool.description.replace(['\n', '\r', '\t'], " ");
        let full_text = format!("{:<24} - {}", tool.name, single_line_desc);
        let clipped = clip_cells_ellipsis(&full_text, max_text_width);

        tool_lines.push(
            Line::from(vec![
                Span::styled(marker, Style::default().fg(theme.accent)),
                Span::styled(clipped, style),
            ])
            .style(if is_selected {
                Style::default().bg(theme.sel_bg)
            } else {
                Style::default()
            }),
        );
    }

    // 标题滚动指示器
    let up_ind = if state.tool_scroll > 0 { "▲ " } else { "" };
    let down_ind = if state.tool_scroll + visible_count < total_tools {
        " ▼"
    } else {
        ""
    };
    let tools_block = Block::default()
        .borders(Borders::BOTTOM)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            format!(
                " 工具列表 {}({}/{}){} ",
                up_ind,
                state.selected_tool + 1,
                total_tools,
                down_ind
            ),
            Style::default().fg(if state.focus == McpFocusPane::ToolsList {
                theme.accent
            } else {
                theme.muted
            }),
        ));
    frame.render_widget(tools_block, chunks[0]);
    frame.render_widget(Paragraph::new(tool_lines), tools_inner);

    // 2. 下半部：选中工具的 JSON Schema 参数预览
    let selected_tool_opt = tools.get(state.selected_tool).copied();
    if let Some(tool) = selected_tool_opt {
        let schema_block = Block::default().title(Span::styled(
            format!(" [{}] inputSchema 参数规范 (按 Enter 聚焦滚动) ", tool.name),
            Style::default().fg(if state.focus == McpFocusPane::SchemaPreview {
                theme.accent
            } else {
                theme.muted
            }),
        ));
        let schema_inner = schema_block.inner(chunks[1]);
        frame.render_widget(schema_block, chunks[1]);

        let schema_lines = format_json_schema(&tool.input_schema);
        let scroll = state.schema_scroll as u16;
        frame.render_widget(
            Paragraph::new(schema_lines)
                .scroll((scroll, 0))
                .wrap(Wrap { trim: false }),
            schema_inner,
        );
    }
}

/// 对 JSON Schema 进行带高亮着色的格式化
fn format_json_schema(value: &Value) -> Vec<Line<'static>> {
    let json_text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    let mut lines = Vec::new();

    for line_str in json_text.lines() {
        let trimmed = line_str.trim_start();
        let indent = line_str.len() - trimmed.len();
        let indent_str = " ".repeat(indent);

        if trimmed.starts_with('"') && trimmed.contains("\":") {
            if let Some(colon_pos) = trimmed.find("\":") {
                let key_part = &trimmed[..=colon_pos];
                let val_part = &trimmed[colon_pos + 1..];

                lines.push(Line::from(vec![
                    Span::raw(indent_str),
                    Span::styled(
                        key_part.to_string(),
                        Style::default()
                            .fg(Color::Rgb(0, 220, 255))
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        val_part.to_string(),
                        Style::default().fg(Color::Rgb(160, 230, 160)),
                    ),
                ]));
                continue;
            }
        }

        let line_color = if trimmed.starts_with('{')
            || trimmed.starts_with('}')
            || trimmed.starts_with('[')
            || trimmed.starts_with(']')
        {
            Color::DarkGray
        } else if trimmed.starts_with('"') {
            Color::Rgb(160, 230, 160)
        } else {
            Color::Rgb(255, 176, 0)
        };

        lines.push(Line::from(vec![
            Span::raw(indent_str),
            Span::styled(trimmed.to_string(), Style::default().fg(line_color)),
        ]));
    }

    lines
}

/// 渲染底栏：提示信息与全局键位映射
fn render_footer(
    frame: &mut Frame,
    tip_area: Rect,
    nav_area: Rect,
    state: &McpPanelState,
    theme: &Theme,
) {
    let tip_text = if let Some(status) = state.current_status() {
        Span::styled(
            format!(" 📢 {status}"),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        match state.focus {
            McpFocusPane::ServerList => Span::styled(
                " 💡 提示: ↑/↓ 选择服务器 · Tab 切换右侧详情 · A 载入官方热门预设 · E 编辑 · D 删除 · R 测活",
                Style::default().fg(theme.muted),
            ),
            McpFocusPane::ServerDetail => Span::styled(
                " 💡 提示: 中栏展示当前服务启动命令、传输协议与握手延迟 · Ctrl+D 查看/脱敏请求标头 · 按 Tab 切换工具清单",
                Style::default().fg(theme.muted),
            ),
            McpFocusPane::ToolsList => Span::styled(
                " 💡 提示: ↑/↓ 选择工具 · Enter 深入展开 inputSchema 预览 · / 关键字过滤",
                Style::default().fg(theme.muted),
            ),
            McpFocusPane::SchemaPreview => Span::styled(
                " 💡 提示: ↑/↓/PgUp/PgDn 滚动参数 Schema 文本 · Esc 返回工具清单",
                Style::default().fg(theme.muted),
            ),
        }
    };
    frame.render_widget(Paragraph::new(Line::from(tip_text)), tip_area);

    let nav_spans = vec![
        Span::styled(
            " [Tab] ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("切窗格  "),
        Span::styled(
            " [A] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("添加预设  "),
        Span::styled(
            " [E] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("编辑  "),
        Span::styled(
            " [D] ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Span::raw("删除  "),
        Span::styled(
            " [R] ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("重新测活  "),
        Span::styled(
            " [Space] ",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("禁用/启用  "),
        Span::styled(
            " [Ctrl+S] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("保存配置  "),
        Span::styled(
            " [Ctrl+D] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("敏感标头  "),
        Span::styled(" [Esc] ", Style::default().fg(theme.muted)),
        Span::raw("返回对话"),
    ];
    frame.render_widget(Paragraph::new(Line::from(nav_spans)), nav_area);
}

/// 渲染 A 键唤出的预设选择抽屉 (Mockup 3)
fn render_preset_picker(frame: &mut Frame, area: Rect, state: &McpPanelState, theme: &Theme) {
    let width = (area.width.saturating_sub(6)).clamp(50, 96);
    let height = (area.height.saturating_sub(4)).clamp(14, 24);

    let popup_area = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Green))
        .title(Line::styled(
            " ➕ 添加 MCP 服务 (Add MCP Server) ── [按 Esc 取消] ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ))
        .style(Style::default().bg(theme.bg).fg(theme.fg));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),                            // 引导文案
            Constraint::Length(MCP_PRESETS.len() as u16 + 1), // 预设列表
            Constraint::Length(1),                            // 分割线
            Constraint::Min(4),                               // 预设预览
            Constraint::Length(1),                            // 底部操作
        ])
        .split(inner);

    frame.render_widget(
        Paragraph::new("请选择内置官方热门预设模板 (按 1-6 快速选择，或 ↑/↓ 移动并回车)：")
            .style(Style::default().fg(theme.fg)),
        chunks[0],
    );

    let mut preset_lines = Vec::new();
    for (i, p) in MCP_PRESETS.iter().enumerate() {
        let is_selected = i == state.preset_selected;
        let prefix = if is_selected { "▶ " } else { "  " };

        let line_style = if is_selected {
            Style::default()
                .bg(theme.sel_bg)
                .fg(theme.title)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };

        preset_lines.push(
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Green)),
                Span::styled(
                    format!("[{}] ", i + 1),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{:<30}", p.display_name), line_style),
                Span::raw(" "),
                Span::styled(p.description, Style::default().fg(theme.muted)),
            ])
            .style(if is_selected {
                Style::default().bg(theme.sel_bg)
            } else {
                Style::default()
            }),
        );
    }
    frame.render_widget(Paragraph::new(preset_lines), chunks[1]);

    let div = "─".repeat(chunks[2].width as usize);
    frame.render_widget(
        Paragraph::new(div).style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    if let Some(p) = MCP_PRESETS.get(state.preset_selected) {
        let preview_lines = vec![
            Line::styled(
                "预设配置详情预览:",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Line::from(vec![
                Span::styled("  服务名称: ", Style::default().fg(theme.muted)),
                Span::styled(
                    p.name,
                    Style::default()
                        .fg(theme.title)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("  传输协议: ", Style::default().fg(theme.muted)),
                Span::raw(format!("{:?}", p.transport)),
            ]),
            Line::from(vec![
                Span::styled("  启动命令: ", Style::default().fg(theme.muted)),
                Span::styled(
                    p.command.unwrap_or("(URL 远程连接)"),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(" "),
                Span::raw(p.args.join(" ")),
            ]),
            Line::from(vec![
                Span::styled("  说明描述: ", Style::default().fg(theme.muted)),
                Span::styled(p.description, Style::default().fg(theme.fg)),
            ]),
        ];
        frame.render_widget(Paragraph::new(preview_lines), chunks[3]);
    }

    frame.render_widget(
        Paragraph::new("操作: Enter 确定载入预设并编辑 · 1-6 键直达 · Esc 取消")
            .style(Style::default().fg(theme.muted)),
        chunks[4],
    );
}

/// 渲染快捷键帮助弹窗
fn render_help_modal(frame: &mut Frame, area: Rect, theme: &Theme) {
    let width = 64.min(area.width.saturating_sub(4));
    let height = 18.min(area.height.saturating_sub(2));

    let popup_area = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" 快捷键指南 (Hotkeys) ")
        .style(Style::default().bg(theme.bg).fg(theme.fg));

    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let help_text = vec![
        Line::styled(
            "Tab / Shift+Tab   在服务列表、详情与工具间循环切换焦点",
            Style::default().fg(theme.fg),
        ),
        Line::styled(
            "← / ↓ / ↑ / →     在窗格间移动焦点或选择当前条目",
            Style::default().fg(theme.fg),
        ),
        Line::styled(
            "h / j / k / l     Vim 风格方向导航",
            Style::default().fg(theme.fg),
        ),
        Line::styled(
            "A                 打开官方预设模板抽屉 / 添加新服务",
            Style::default().fg(Color::Green),
        ),
        Line::styled(
            "E                 编辑当前高亮服务器的配置参数",
            Style::default().fg(Color::Yellow),
        ),
        Line::styled(
            "D                 删除当前服务器（双击确认安全机制）",
            Style::default().fg(Color::Red),
        ),
        Line::styled(
            "R / C             重新连接该 MCP 服务并探测工具",
            Style::default().fg(Color::Cyan),
        ),
        Line::styled(
            "Space             临时禁用或重新启用该服务器",
            Style::default().fg(Color::Magenta),
        ),
        Line::styled(
            "Enter             深入展开工具 Schema 预览 / 确认选择",
            Style::default().fg(theme.title),
        ),
        Line::styled(
            "f 或 /            搜索过滤服务名与暴露工具关键字",
            Style::default().fg(Color::Yellow),
        ),
        Line::styled(
            "Ctrl+S            将修改立即原子保存到 servers.toml",
            Style::default().fg(Color::Green),
        ),
        Line::styled(
            "Ctrl+D            显示/脱敏隐藏请求标头中的敏感数据",
            Style::default().fg(Color::Yellow),
        ),
        Line::styled(
            "Esc / q           退出当前弹窗或返回 CLI 对话主界面",
            Style::default().fg(theme.muted),
        ),
    ];

    frame.render_widget(Paragraph::new(help_text), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    fn sample_panel_state() -> McpPanelState {
        let spec1 = McpServerSpec {
            name: "filesystem".into(),
            transport: McpTransport::Stdio,
            command: Some("npx".into()),
            args: vec![
                "-y".into(),
                "@modelcontextprotocol/server-filesystem".into(),
                ".".into(),
            ],
            env: std::collections::HashMap::new(),
            url: None,
            headers: std::collections::HashMap::new(),
            timeout_secs: 5,
        };
        let spec2 = McpServerSpec {
            name: "fetch".into(),
            transport: McpTransport::Stdio,
            command: Some("uvx".into()),
            args: vec!["mcp-server-fetch".into()],
            env: std::collections::HashMap::new(),
            url: None,
            headers: std::collections::HashMap::new(),
            timeout_secs: 8,
        };
        let tool1 = McpToolSchema {
            name: "read_file".into(),
            description: "读取文件内容".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "文件绝对路径" }
                },
                "required": ["path"]
            }),
        };
        let tool2 = McpToolSchema {
            name: "write_file".into(),
            description: "写入文件内容".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "content": { "type": "string" }
                },
                "required": ["path", "content"]
            }),
        };

        let item1 = McpServerItem {
            spec: spec1,
            connected: true,
            tool_count: 2,
            latency: Some(Duration::from_millis(12)),
            error: None,
            tools: vec![tool1, tool2],
            disabled: false,
        };
        let item2 = McpServerItem {
            spec: spec2,
            connected: false,
            tool_count: 0,
            latency: None,
            error: None,
            tools: Vec::new(),
            disabled: false,
        };

        McpPanelState {
            servers: vec![item1, item2],
            focus: McpFocusPane::ServerList,
            selected_server: 0,
            selected_tool: 0,
            tool_scroll: 0,
            schema_scroll: 0,
            detail_scroll: 0,
            filter: String::new(),
            filtering: false,
            testing_connection: false,
            last_test_result: None,
            pending_delete: None,
            show_preset_picker: false,
            preset_selected: 0,
            show_help: false,
            form: None,
            dirty: false,
            status_message: None,
            config_path: PathBuf::from("test_servers.toml"),
            show_sensitive_headers: false,
        }
    }

    #[test]
    fn test_focus_navigation_cycle() {
        let mut state = sample_panel_state();
        assert_eq!(state.focus, McpFocusPane::ServerList);

        state.next_focus();
        assert_eq!(state.focus, McpFocusPane::ServerDetail);

        state.next_focus();
        assert_eq!(state.focus, McpFocusPane::ToolsList);

        state.next_focus();
        assert_eq!(state.focus, McpFocusPane::SchemaPreview);

        state.next_focus();
        assert_eq!(state.focus, McpFocusPane::ServerList);

        state.prev_focus();
        assert_eq!(state.focus, McpFocusPane::SchemaPreview);
    }

    #[test]
    fn test_server_list_navigation() {
        let mut state = sample_panel_state();
        assert_eq!(state.selected_server, 0);

        state.move_down();
        assert_eq!(state.selected_server, 1);

        state.move_down(); // clamp at max
        assert_eq!(state.selected_server, 1);

        state.move_up();
        assert_eq!(state.selected_server, 0);
    }

    #[test]
    fn test_tool_list_navigation_and_schema_scroll() {
        let mut state = sample_panel_state();
        state.focus = McpFocusPane::ToolsList;
        assert_eq!(state.selected_tool, 0);

        state.move_down();
        assert_eq!(state.selected_tool, 1);

        state.focus = McpFocusPane::SchemaPreview;
        assert_eq!(state.schema_scroll, 0);
        state.move_down();
        assert_eq!(state.schema_scroll, 1);
        state.scroll_down(5);
        assert_eq!(state.schema_scroll, 6);
        state.scroll_up(4);
        assert_eq!(state.schema_scroll, 2);
    }

    #[test]
    fn test_toggle_disabled() {
        let mut state = sample_panel_state();
        assert!(!state.servers[0].disabled);
        assert!(!state.dirty);

        state.toggle_disabled();
        assert!(state.servers[0].disabled);
        assert!(state.dirty);

        state.toggle_disabled();
        assert!(!state.servers[0].disabled);
    }

    #[test]
    fn test_delete_double_press_confirmation() {
        let mut state = sample_panel_state();
        assert_eq!(state.servers.len(), 2);

        // 第一次按 D：触发二次确认拦截
        let confirmed = state.request_delete();
        assert!(confirmed.is_none());
        assert_eq!(state.pending_delete.as_deref(), Some("filesystem"));
        assert_eq!(state.servers.len(), 2);

        // 第二次按 D：确认删除
        let confirmed = state.request_delete();
        assert_eq!(confirmed.as_deref(), Some("filesystem"));
        assert_eq!(state.servers.len(), 1);
        assert_eq!(state.servers[0].spec.name, "fetch");
        assert!(state.dirty);
    }

    #[test]
    fn test_preset_picker_and_confirm() {
        let mut state = sample_panel_state();
        state.open_preset_picker();
        assert!(state.show_preset_picker);
        assert_eq!(state.preset_selected, 0);

        // 选择 sqlite 预设 (index 3)
        state.preset_selected = 3;
        state.confirm_preset();
        assert!(!state.show_preset_picker);
        assert!(state.form.is_some());
        let form = state.form.as_ref().unwrap();
        assert_eq!(form.name, "sqlite");
        assert!(!form.is_edit()); // 预设模式视为添加新服务
    }

    #[test]
    fn test_filtering() {
        let mut state = sample_panel_state();
        assert_eq!(state.filtered_server_indices().len(), 2);

        state.filter = "fetch".into();
        let filtered = state.filtered_server_indices();
        assert_eq!(filtered.len(), 1);
        assert_eq!(state.servers[filtered[0]].spec.name, "fetch");

        state.filter = "read_file".into(); // 搜工具名
        let filtered = state.filtered_server_indices();
        assert_eq!(filtered.len(), 1);
        assert_eq!(state.servers[filtered[0]].spec.name, "filesystem");

        state.filter.clear();
        assert_eq!(state.filtered_server_indices().len(), 2);
    }

    #[test]
    fn test_render_mcp_panel_three_columns() {
        let mut state = sample_panel_state();
        let theme = Theme::resolve("cyberpunk");
        let mut terminal = Terminal::new(TestBackend::new(120, 36)).unwrap();

        terminal
            .draw(|frame| {
                render_mcp_panel(frame, frame.area(), &mut state, &theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        assert!(content.contains("MCP"));
        assert!(content.contains("filesystem"));
        assert!(content.contains("fetch"));
        assert!(content.contains("read_file"));
        assert!(content.contains("inputSchema"));
    }

    #[test]
    fn test_render_mcp_panel_compact() {
        let mut state = sample_panel_state();
        let theme = Theme::resolve("cyberpunk");
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();

        terminal
            .draw(|frame| {
                render_mcp_panel(frame, frame.area(), &mut state, &theme);
            })
            .unwrap();

        let buffer = terminal.backend().buffer();
        let content: String = buffer.content().iter().map(|cell| cell.symbol()).collect();

        assert!(content.contains("filesyst"));
    }

    #[test]
    fn test_form_modal_is_opaque_over_panel_content() {
        // 回归：表单模态层必须先 Clear，避免底层面板文字透过空白行（样式镂空）混入。
        let mut state = sample_panel_state();
        let spec = state.servers[0].spec.clone();
        state.form = Some(crate::views::mcp_form::McpFormState::from_spec(&spec));
        let theme = Theme::resolve("cyberpunk");
        let (w, h) = (100u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();

        terminal
            .draw(|frame| {
                render_mcp_panel(frame, frame.area(), &mut state, &theme);
            })
            .unwrap();

        // 表单模态居中于 72% x 82%，据此推算内部区域
        let modal_w = w * 72 / 100;
        let modal_h = h * 82 / 100;
        let mx = (w - modal_w) / 2;
        let my = (h - modal_h) / 2;
        let buffer = terminal.backend().buffer();

        // 模态内部（去除边框）拼接文本；宽字符单元格含填充空白，需归一化
        let mut inner_text = String::new();
        for y in (my + 1)..(my + modal_h - 1) {
            for x in (mx + 1)..(mx + modal_w - 1) {
                if let Some(c) = buffer.cell((x, y)) {
                    inner_text.push_str(c.symbol());
                }
            }
        }
        let normalized: String = inner_text.chars().filter(|c| !c.is_whitespace()).collect();

        // 中栏专属文案不得出现在表单模态内部
        for bleed in ["握手超时", "状态与健康度", "往返延迟", "重新测活"] {
            assert!(
                !normalized.contains(bleed),
                "底层面板文字 {bleed:?} 不应透过表单模态: {normalized:?}"
            );
        }
        // 表单自身内容仍在
        assert!(normalized.contains("transport"));
    }

    #[test]
    fn test_ctrl_d_toggles_sensitive_headers() {
        let mut state = sample_panel_state();
        let mut headers = std::collections::HashMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer sk-test-super-secret-12345".to_string(),
        );
        headers.insert("Cookie".to_string(), "session=xyz".to_string());
        state.servers[0].spec = McpServerSpec {
            name: "remote".into(),
            transport: McpTransport::Http,
            command: None,
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            url: Some("https://api.example.com/mcp".into()),
            headers,
            timeout_secs: 5,
        };
        state.focus = McpFocusPane::ServerDetail;
        state.selected_server = 0;

        let mut terminal = Terminal::new(TestBackend::new(200, 40)).unwrap();

        let render = |state: &mut McpPanelState, terminal: &mut Terminal<TestBackend>| {
            let theme = Theme::resolve("cyberpunk");
            terminal
                .draw(|frame| render_mcp_panel(frame, frame.area(), state, &theme))
                .unwrap();
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>()
        };

        // 初始：默认脱敏，不得泄露明文
        assert!(!state.show_sensitive_headers);
        let content = render(&mut state, &mut terminal);
        assert!(content.contains("••••••••"), "默认应展示脱敏占位符");
        assert!(
            !content.contains("sk-test-super-secret-12345"),
            "默认不得泄露 Bearer Token 明文"
        );
        assert!(!content.contains("session=xyz"), "默认不得泄露 Cookie 明文");

        // 触发 Ctrl+D：切换为明文
        state.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(state.show_sensitive_headers);
        let content = render(&mut state, &mut terminal);
        assert!(
            content.contains("Bearer sk-test-super-secret-12345"),
            "明文模式应展示完整 Bearer Token"
        );
        assert!(
            content.contains("session=xyz"),
            "明文模式应展示 Cookie 明文"
        );

        // 再次 Ctrl+D：恢复脱敏
        state.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(!state.show_sensitive_headers);
        let content = render(&mut state, &mut terminal);
        assert!(!content.contains("sk-test-super-secret-12345"));
        assert!(content.contains("••••••••"));
    }

    #[test]
    fn test_mask_sensitive_header_formats() {
        assert_eq!(mask_sensitive_header("   "), "(空)");
        assert_eq!(
            mask_sensitive_header("Bearer abc123"),
            "Bearer •••••••••••• (已脱敏保护)"
        );
        assert_eq!(mask_sensitive_header("basic Zm9v"), "Basic ••••••••");
        assert_eq!(
            mask_sensitive_header("raw-secret"),
            "•••••••••••• (已脱敏保护)"
        );
    }

    #[test]
    fn test_tool_list_focus_following_and_no_overflow() {
        let mut state = sample_panel_state();
        state.focus = McpFocusPane::ToolsList;
        state.servers[0].tools = (0..25)
            .map(|i| McpToolSchema {
                name: format!("tool_{i:02}"),
                description: format!("desc {i}"),
                input_schema: json!({ "type": "object" }),
            })
            .collect();
        state.servers[0].tool_count = 25;

        let theme = Theme::resolve("cyberpunk");
        let area = Rect::new(0, 0, 60, 20);
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).unwrap();

        let row_text = |terminal: &Terminal<TestBackend>, y: u16| -> String {
            let buffer = terminal.backend().buffer();
            (0..60)
                .map(|x| buffer.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                .collect()
        };

        // 首屏渲染确定可视行数
        terminal
            .draw(|frame| render_tools_and_schema(frame, area, &mut state, &theme))
            .unwrap();
        let visible = (0..25)
            .filter(|i| {
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|c| c.symbol())
                    .collect();
                text.contains(&format!("tool_{i:02}"))
            })
            .count();
        assert!(
            visible > 0 && visible < 25,
            "可视行数应在合理区间: {visible}"
        );

        // 模拟按下 15 次 Down
        for _ in 0..15 {
            state.move_down();
        }
        assert_eq!(state.selected_tool, 15);

        terminal
            .draw(|frame| render_tools_and_schema(frame, area, &mut state, &theme))
            .unwrap();

        // 焦点跟随：选中项严格落在 [tool_scroll, tool_scroll + visible) 区间
        assert!(state.tool_scroll <= state.selected_tool);
        assert!(state.selected_tool < state.tool_scroll + visible);
        assert_eq!(
            state.tool_scroll,
            state.selected_tool + 1 - visible,
            "视口应紧随光标推进"
        );

        let first = state.tool_scroll;
        let last = first + visible - 1;
        assert_eq!(last, 15);

        // 视口仅含当前窗口内工具，顶部与下一项不得越界渲染
        for i in 0..visible {
            let name = format!("tool_{:02}", first + i);
            let found = (0..20).any(|y| row_text(&terminal, y).contains(&name));
            assert!(found, "视口内应包含 {name}");
        }
        let all_text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!all_text.contains("tool_00"), "顶部 tool_00 应已滚出视口");
        assert!(
            !all_text.contains(&format!("tool_{:02}", last + 1)),
            "下一个工具不得越界渲染"
        );

        // ▶ 指针行必须落在工具列表内部行（即某个工具行），不得出现在边框/下方 schema 区
        let marker_row = (0..20)
            .find(|&y| row_text(&terminal, y).contains('▶'))
            .expect("应能找到 ▶ 指针行");
        let marker_line = row_text(&terminal, marker_row);
        assert!(
            marker_line.contains(&format!("tool_{:02}", state.selected_tool)),
            "▶ 指针应与选中工具同物理行: {marker_line:?}"
        );
        let tool_rows: Vec<u16> = (0..20)
            .filter(|&y| row_text(&terminal, y).contains("tool_"))
            .collect();
        assert!(
            tool_rows.contains(&marker_row),
            "▶ 指针行 {marker_row} 必须处于工具列表内部行 {tool_rows:?}"
        );
    }
}
