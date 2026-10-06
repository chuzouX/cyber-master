# 设置中心新增「9. 工具配置」页：收敛 [tools] 配置 + 注册工具列表

## Context

`cyber`（coding CLI）的设置中心（`Ctrl+,` / `/settings`，`cyber setup` 用的是同一套面板）目前把工具相关配置散落在多处：`tools.web_search` 在「1. Agent & 模型」第 6 行、`tools.prefer_docker` 与只读的 `tools.extra_path` 在「4. 工具与 MCP」页。用户要求新增一个「工具配置」页，把这些 `[tools]` 配置项**真·搬移**（旧位置删除）过来，并参考现有 Skills 列表的做法（每行一项 + Enter 详情弹层）在该页列出现已注册的全部工具。

目标终态：
- 设置中心出现第 9 个标签页「9. 工具配置」（`SettingsTab::ToolConfig`，`cyber setup` 首启向导自动包含它）。
- 该页自上而下：`web_search` 开关、`prefer_docker` 开关、只读 `extra_path` 显示行、`已注册工具 (N 个)` 表头、每个注册工具一行（名称 + 来源徽标 + `[仅内部]` 标记 + 标签预览 + 说明预览，选中行带 `[按 Enter 查看详情]`）、Enter 打开工具详情弹层（名称/来源/对模型可见性/说明/标签/参数 JSON Schema）。
- 旧位置的两行配置与其按键分支、`max_row`、`reset_current_tab` 重置项、提示文案全部删除并按删除后的行号重排。

## Approach

按顺序执行；步骤 1-4 是纯新增（`cargo check -p cyber-tui` 每步应通过），步骤 5 是搬移删除 + 行号重排（既有测试在此步后才需要更新，清单见「Verification」）。

### 步骤 1 — 注册新标签页 `SettingsTab::ToolConfig`

文件：`crates/cyber-tui/src/cli.rs`（`SettingsTab` 定义在 :154-238，`all/title/short_title/compact_title/max_row` 是同一 impl 块）。

1.1 `enum SettingsTab` 末尾（现 `Toolbox,       // 8. 自定义工具库` 之后）追加：

```rust
    ToolConfig,    // 9. 工具配置（[tools] 配置 + 注册工具清单）
```

1.2 `all()`（:167-178）在 `SettingsTab::Toolbox,` 之后追加 `SettingsTab::ToolConfig,`（顺序即标签栏顺序与 `1`-`9` 直达顺序）。

1.2b `enum SettingsTab` 上方的文档注释（:152「共 8 个大类」）改成「共 9 个大类」。

1.2c `next_tab`/`prev_tab`（:442-458）用 `all()` 取模，无需改动（已核对：无字面量 tab 数）。

1.3 三个标题函数各加一条 `match` 分支（保持既有字符串风格与宽度预算）：

```rust
// title()
Self::ToolConfig => "9. 工具配置",
// short_title()
Self::ToolConfig => "9.工具配置",
// compact_title()
Self::ToolConfig => "9.工具配置",
```

（`ToolsMcp`/`Toolbox` 的既有标题串**不改**：本页只新增，不改名旧页。）

1.4 `max_row()`（:219-237）追加：

```rust
            // 0=web_search 1=prefer_docker 2=extra_path(只读)，工具行从 3 起
            Self::ToolConfig => 2 + state.tools.len(),
```

（与 `Self::ToolsMcp => 2 + state.skills.len()` 同构：3 个固定行占 0..=2，第 i 个工具行 = 3+i，故最大行号 = 2+N。）

`render_tab_bar`（:8691 起）**不需要改**：它已按 `area_width` 在 `title`/`short_title`/`compact_title` 三者间降级，并在总宽超限时切换成带 `◄`/`►` 的滑动窗口，围绕当前 tab 展开。9 个标签页在此逻辑下自动可用。

### 步骤 2 — 状态快照：`tools` + `tool_detail`

文件：`crates/cyber-tui/src/cli.rs`。

2.1 在 `SkillSummary`（:248-260）之后新增（与它同风格）：

```rust
/// 设置中心「工具配置」页的快照条目（来源 `ToolRegistry::all_schemas()`）。
#[derive(Clone, Debug, Default)]
pub struct ToolSummary {
    pub name: String,
    /// 展示用来源：`内置` / `自定义` / `MCP` / `Skill` / `其他`。
    pub source: String,
    pub description: String,
    pub tags: Vec<String>,
    /// 参数字 JSON Schema（详情弹层 pretty-print）。
    pub parameters: serde_json::Value,
    /// 是否对模型可见（`ToolRegistry::schemas()` 中不存在 = hidden：自定义工具、MCP 具体工具、
    /// 超过 32 个 Skill 时的 skill_* 工具）。
    pub model_visible: bool,
}

/// 「工具配置」页工具详情弹窗状态。
#[derive(Clone, Debug)]
pub struct ToolDetailModal {
    /// 当前查看的工具在 `settings.tools` 中的下标索引。
    pub tool_index: usize,
    /// 正文滚屏偏移行数。
    pub scroll: usize,
}
```

（`serde_json` 已是 `cyber-tui` 依赖：`crates/cyber-tui/Cargo.toml:28`。）

2.2 `CliSettingsState`（:287-323）新增两个字段，并在 `new()`（:326-357）里初始化 `tools: Vec::new(),` / `tool_detail: None,`：

```rust
    /// 注册工具清单快照（`AppRegistries.tools.all_schemas()`，按名称不区分大小写排序）。
    pub tools: Vec<ToolSummary>,
    /// 「工具配置」页工具详情弹窗。
    pub tool_detail: Option<ToolDetailModal>,
```

2.3 `from_runner`（:359-431）在 `state.custom_tools = ...`（自定义工具快照，:377-382）之后、`state.skills = ...` 之前或之后（两者无依赖）插入：

```rust
        // 工具清单：all_schemas() 含 hidden（自定义/MCP/超量 Skill），schemas() 为模型可见集。
        let visible: std::collections::HashSet<String> = runner
            .registries
            .tools
            .schemas()
            .into_iter()
            .map(|s| s.name)
            .collect();
        let mut tools: Vec<ToolSummary> = runner
            .registries
            .tools
            .all_schemas()
            .into_iter()
            .map(|s| {
                let source = if state.custom_tools.iter().any(|t| t.name == s.name) {
                    "自定义"
                } else if s.name.starts_with("mcp_") {
                    "MCP"
                } else if s.name.starts_with("skill_") {
                    "Skill"
                } else if cyber_agent::builtin_tool_names().contains(&s.name.as_str()) {
                    "内置"
                } else {
                    "其他"
                };
                ToolSummary {
                    model_visible: visible.contains(&s.name),
                    name: s.name,
                    source: source.to_string(),
                    description: s.description,
                    tags: s.tags,
                    parameters: s.parameters,
                }
            })
            .collect();
        tools.sort_by(|a, b| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
                .then_with(|| a.name.cmp(&b.name))
        });
        state.tools = tools;
```

依据：`ToolRegistry::all_schemas()` 与 `schemas()` 均返回 `Vec<ToolSchema>`（`crates/cyber-agent/src/tool.rs:227`/`:235`），`ToolSchema { name, description, parameters, tags }`（同文件 :15-24）；`cyber_agent::builtin_tool_names()` 为 27 个内置工具名（`crates/cyber-agent/src/tools/mod.rs:46`）；自定义工具 schema 名 = 配置里的 `name`（**不一定**带 `custom_` 前缀，故用 `state.custom_tools` 精确比对，见 `crates/cyber-agent/src/tool.rs:294-302` 的双向前缀别名逻辑）；Skill 工具名 = `skill_<name.replace('.', "_")>`（`crates/cyber-skills/src/tool.rs:48-53`）；MCP 具体工具名 = `mcp_<server>_<tool>`。

2.4 `set_tab`（:463-470）与「上一个 tab」切换（:450-460）里，除 `self.skill_detail = None; self.memory_detail = None;` 外追加 `self.tool_detail = None;`（两处都加）。

### 步骤 3 — 新页渲染 `draw_tab_tools_config`

文件：`crates/cyber-tui/src/cli.rs`，把新函数放在 `draw_tab_toolbox`（:6308 起）附近（同一层次的 tab 绘制函数群）。

3.1 函数骨架（`lines`/焦点变量与 `draw_tab_tools_mcp` 同构）：

```rust
/// 「9. 工具配置」页：`[tools]` 配置项（从旧页搬来）+ 注册工具清单。
fn draw_tab_tools_config(
    frame: &mut Frame,
    area: Rect,
    settings: &CliSettingsState,
    _screen: &CliScreen,
) {
    let mut lines: Vec<Line> = vec![Line::styled(
        "  工具与外部环境配置（写入 ~/.cyber/config.toml 的 [tools] 段）",
        Style::default().fg(MUTED),
    )];
    let mut focused_start = 0usize;
    let mut focused_end = 0usize;
    // ... 3.2 三个配置行 + 3.3 工具清单 ...
    render_scrollable_content(frame, area, lines, focused_start, focused_end);
}
```

3.2 三个配置行（复用既有 `render_setting_row(selected, label, value, hint, width)`，见 :5657-5686）：

- 行 0：`render_setting_row(settings.selected_row == 0, "联网搜索与抓取 (Web Search)", if settings.config_draft.tools.web_search { "[ ● 开启 ]" } else { "[ ○ 关闭 ]" }, "启用/禁用 web_fetch 外部网络查询工具".into(), area.width)`
- 行 1：`render_setting_row(settings.selected_row == 1, "优先容器执行 (Docker)", if settings.config_draft.tools.prefer_docker { "[ ● 开启 ]" } else { "[ ○ 关闭 ]" }, "若环境安装 Docker，则优先容器隔离执行".into(), area.width)`
- 行 2：`render_setting_row(settings.selected_row == 2, "额外 PATH (tools.extra_path)", if settings.config_draft.tools.extra_path.is_empty() { "(空)".to_string() } else { settings.config_draft.tools.extra_path.join(";") }, "只读：在 config.toml 的 [tools].extra_path 中编辑（本面板不可改）".into(), area.width)`

（行 2 保持**只读**——与旧「4. 工具与 MCP」页的非可聚焦显示行、TUI `views/settings.rs` 的 `extra_path`（`FieldKind::ReadOnly` + `noop_set`，:314-322）一致；两处都没有编辑入口，本页也不新增编辑交互。）

3.3 空行 + 工具表头（照抄 Skills 表头的写法，:6136-6149）：

```rust
    lines.push(Line::raw(""));
    lines.push(Line::styled(
        format!(
            "  [已注册工具 ({} 个)] (↑/↓ 移动焦点 · Enter 查看详情；含仅内部工具)",
            settings.tools.len()
        ),
        Style::default()
            .fg(CODE)
            .add_modifier(Modifier::BOLD)
            .bg(CODE_BG),
    ));
```

（样式三件套照抄 `draw_tab_tools_mcp` 里 `[已加载 Skills 技能 (N 个)]` 表头那一行，逐字一致，不要自造颜色常量。）

`settings.tools.is_empty()` 时推一行 `Line::styled("    • 暂无已注册工具", Style::default().fg(DIM))` 并跳到 3.4。

3.4 工具行（第 i 个工具 → 焦点行号 `3 + i`）：

```rust
    for (i, tool) in settings.tools.iter().enumerate() {
        let row = 3 + i;
        let is_sel = settings.selected_row == row;
        let start_line = lines.len();
        let pointer = if is_sel { "▶ " } else { "  " };
        let (badge_text, badge_color) = match tool.source.as_str() {
            "内置" => (" [内置] ", CODE),
            "自定义" => (" [自定义] ", SUCCESS),
            "MCP" => (" [MCP] ", ACCENT),
            "Skill" => (" [Skill] ", CODE),
            _ => (" [其他] ", DIM),
        };
        let mut spans = vec![
            Span::styled(pointer, if is_sel { Style::default().fg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::default().fg(DIM) }),
            Span::styled(format!("{:<34}", tool.name), if is_sel { Style::default().fg(FG).add_modifier(Modifier::BOLD) } else { Style::default().fg(FG) }),
            Span::styled(badge_text, Style::default().fg(badge_color)),
        ];
        if !tool.model_visible {
            spans.push(Span::styled(" [仅内部] ", Style::default().fg(DIM)));
        }
        if !tool.tags.is_empty() {
            spans.push(Span::styled(
                format!(" #{} ", tool.tags.join(" #")),
                Style::default().fg(CODE),
            ));
        }
        if !tool.description.is_empty() {
            spans.push(Span::styled(format!(" {}", tool.description), Style::default().fg(MUTED)));
        }
        if is_sel {
            spans.push(Span::styled("  [按 Enter 查看详情]", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)));
        }
        lines.push(Line::from(spans));
        if is_sel {
            focused_start = start_line;
            // 与 draw_tab_tools_mcp 技能行的写法逐字一致：
            focused_end = lines.len().saturating_sub(1);
        }
    }
```

（`render_scrollable_content` 的 `focused_start`/`focused_end` 语义与 Skills 行完全相同——直接照抄 :6141-6213 那段的赋值写法，不要自行改成 `lines.len()`。）

3.5 末尾 `render_scrollable_content(frame, area, lines, focused_start, focused_end);`（:5689-5720，跟随焦点的滚动，与 Skills 列表同款）。

3.6 在 `draw_settings_panel` 的 tab 分派（:8570-8579）追加一行：

```rust
            SettingsTab::ToolConfig => draw_tab_tools_config(frame, chunks[2], settings, screen),
```

### 步骤 4 — 工具详情弹层

4.1 状态：步骤 2.1 已定义 `ToolDetailModal`；`CliSettingsState.tool_detail` 已在步骤 2.2 加入。

4.2 按键拦截：`handle_settings_key` 顶部已有 skill 详情弹层拦截块（:8965-9022：Esc/`q`/`Q` 关 8969；Enter/Space 关 8973；Up/`k` 8977；Down/`j` 8981；PageUp 8985；PageDown 8989；Home 8993；End 8997；Left/`h` 9002 且同步 `settings.selected_row = 3 + detail.skill_index;` 于 9007；Right/`l` 9011 且同步于 9015；`_ => return Ok(false)` 9019）。紧接其后新增同构的 `tool_detail` 块（`if let Some(detail) = settings.tool_detail.as_mut() { ... }`）：

- `Esc`/`q`/`Q`/`Enter`/`Char(' ')` → `settings.tool_detail = None; return Ok(false);`
- `Up`/`Char('k')` → `detail.scroll = detail.scroll.saturating_sub(1);`
- `Down`/`Char('j')` → `detail.scroll = detail.scroll.saturating_add(1);`
- `PageUp` → `saturating_sub(10)`；`PageDown` → `saturating_add(10)`
- `Home` → `detail.scroll = 0`；`End` → `detail.scroll = usize::MAX`（skill 块写法照抄）
- `Left`/`Char('h')` → `let n = settings.tools.len(); if n > 0 { detail.tool_index = (detail.tool_index + n - 1) % n; settings.selected_row = 3 + detail.tool_index; }`
- `Right`/`Char('l')` → `(detail.tool_index + 1) % n` 并同样 `settings.selected_row = 3 + detail.tool_index;`
- 其余键 → `return Ok(false);`

（`settings` 与 `detail` 的借用：skill 块用的是 `settings.skill_detail.as_mut()` + 直接写 `settings.selected_row`——注意 Rust 借用冲突，skill 块是通过 `settings` 的拆分/局部拷贝实现；照抄它的写法即可，不要自行发明 `split_at_mut` 之类。）

4.2b 同时把 `settings.skills` 相关的既有硬编码行号同步改掉：`cli.rs:9007` 与 `cli.rs:9015` 的 `settings.selected_row = 3 + detail.skill_index;` → `2 + detail.skill_index`（步骤 5.2 的行号重排）。

4.3 渲染：新增 `fn draw_tool_detail_modal(frame: &mut Frame, parent: Rect, settings: &CliSettingsState, detail: &ToolDetailModal)`，放在 `draw_skill_detail_modal`（:6862 起）之后，几何与骨架照抄它：`width = (parent.width * 85 / 100).max(70).min(parent.width)`、`height = (parent.height * 85 / 100).max(20).min(parent.height)`、居中、`clear_popup_under(frame, <同一矩形>)`（照抄 skill 弹层传入的那个变量）、`Block::bordered().border_style(ACCENT bold)`，标题 `format!(" 🔧 工具详情 [{}/{}] {} ", idx + 1, total, tool.name)`（用 `clip_cells_ellipsis` 裁到 `width - 2`）；内部 `Layout::vertical([Min(0), Length(1)])`，正文行：

```rust
    名称: {tool.name}
    来源: {tool.source}
    对模型可见: 是 / 否（仅内部：可被显式调用但不出现在模型 tools 数组）
    说明: {tool.description 或 "(无)"}
    标签: {tool.tags.join(", ") 或 "(无)"}
    (空行)
    参数 JSON Schema:
        {serde_json::to_string_pretty(&tool.parameters) 的每一行，前缀 4 空格}
```

（`serde_json::to_string_pretty` 失败时回退 `tool.parameters.to_string()`。）底部提示行与 skill 详情一致：`" [↑/↓/PgUp/PgDn] 滚屏 · [←/→] 切换上/下一个工具 · [Esc/q/Enter] 关闭返回"`。

`tool_index >= settings.tools.len()` 时直接 `return`（不 panic）。滚屏用 `detail.scroll.min(max_scroll)`，与 skill 详情同款。

4.4 在 `draw_settings_panel` 末尾既有的弹层调用（:8771-8777，形式为 `if let Some(detail) = &settings.skill_detail { draw_skill_detail_modal(frame, popup_area, settings, detail); }`）之后追加：

```rust
    if let Some(detail) = settings.tool_detail.as_ref() {
        draw_tool_detail_modal(frame, popup_area, settings, detail);
    }
```

（第三个参数必须是函数内那个设置面板矩形——skill 弹层用的是 `popup_area`，不是 `bounds`/`area`；照抄同名字面量。）

### 步骤 5 — 真·搬移：删除旧位置并按新行号重排

文件：`crates/cyber-tui/src/cli.rs`。

5.1 「1. Agent & 模型」页删掉 web_search 行（`draw_tab_agent_model` 起 :5724；待删行 :5806-5815，`settings.selected_row == 6` / `"联网搜索与抓取 (Web Search)"`）：

- 删除该 `render_setting_row(...)` 调用（4 行参数 + 闭合括号整块）；
- 其后各行的 `settings.selected_row == 7/8/9/10/11`（:5817/5828/5842/5853/5860）改为 `== 6/7/8/9/10`；
- `max_row()` 的 `Self::AgentModel => 11, // 0..=11 (12 rows)`（:221）改为 `10` 并同步注释；
- `handle_settings_key` 的 `SettingsTab::AgentModel` 分支：arm `6 => {...web_search...}`（:9308-9317）整块删除，arm `7/8/9/10/11`（:9318/9328/9347/9348/9363）改为 `6/7/8/9/10`（内部逻辑不变）；
- `reset_current_tab()` 的 `Self::AgentModel` 分支删除 `self.config_draft.tools.web_search = default.tools.web_search;`（:479）；
- 该页 tip（`chunks[3]`，:8583）与 buttons catch-all（~:8685）无行号文案，不改；nav_hint 的 catch-all（:8762-8763）里 `"1-8 直达"` 改为 `"1-9 直达"`。

5.2 「4. 工具与 MCP」页删掉 prefer_docker 行与 extra_path 显示行（`draw_tab_tools_mcp` 起 :6019）：

- 删除 `extra_path_str` 预计算块（:6025-6029）；
- 删除 row 0 prefer_docker 整块（:6035-6049，含 `let row0_start = ...` 与焦点块）；
- 删除非可聚焦的 `额外环境变量 PATH` 显示行（:6083-6088，`render_setting_row(false, "额外环境变量 PATH", extra_path_str, ...)`）；其后的空行（:6089）保留（MCP 概要表头前的分隔）；
- row 1（CTF，:6051-6069）的 `settings.selected_row == 1` → `== 0`；row 2（MCP 控制台，:6071-6081）的 `== 2` → `== 1`；
- 技能行 `let is_sel = settings.selected_row == 3 + i;`（:6141）→ `2 + i`；
- `max_row()` 的 `Self::ToolsMcp => 2 + state.skills.len()`（:224）→ `1 + state.skills.len()`；
- tip（:8586-8592）`if settings.selected_row >= 3` → `>= 2`；
- nav_hint（:8749-8758）两条 arm 里的 `if settings.selected_row >= 3`（:8753）→ `>= 2`，两处 `"1-8 直达分类"` → `"1-9 直达分类"`；
- `handle_settings_key` 的 `SettingsTab::ToolsMcp` 分支：arm `0 => {prefer_docker}`（:9550-9559）删除；arm `1 => {ctf...}`（:9560-9572）→ `0`；arm `2 if key.code == Enter || ' '`（:9573-9587）→ `1`；`idx if idx >= 3 => { let skill_idx = idx - 3; ... }`（:9589-9599）→ `idx >= 2` + `idx - 2`（内部键位 `Enter | 'o' | 'O' | ' '` 与 `SkillDetailModal { skill_index, scroll: 0 }` 不变）；
- 弹层 Left/Right 同步的 `settings.selected_row = 3 + detail.skill_index;`（:9007、:9015）→ `2 + ...`；
- `reset_current_tab()` 的 `Self::ToolsMcp` 分支（:491-493）删除 prefer_docker 重置 → 空分支 `Self::ToolsMcp => {}`。

5.3 新增页的按键与重置：

- `handle_settings_key` 新增 `SettingsTab::ToolConfig` 分支（放在 `SettingsTab::Toolbox => {` 之前或之后均可，缩进与同级 match arm 一致）：

```rust
        SettingsTab::ToolConfig => match settings.selected_row {
            0 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.tools.web_search =
                        !settings.config_draft.tools.web_search;
                    settings.dirty = true;
                }
            }
            1 => {
                if matches!(
                    key.code,
                    KeyCode::Left | KeyCode::Right | KeyCode::Char(' ') | KeyCode::Enter
                ) {
                    settings.config_draft.tools.prefer_docker =
                        !settings.config_draft.tools.prefer_docker;
                    settings.dirty = true;
                }
            }
            2 => {}
            idx if idx >= 3 => {
                let tool_idx = idx - 3;
                if tool_idx < settings.tools.len()
                    && matches!(
                        key.code,
                        KeyCode::Enter
                            | KeyCode::Char('o')
                            | KeyCode::Char('O')
                            | KeyCode::Char(' ')
                    )
                {
                    settings.tool_detail = Some(ToolDetailModal {
                        tool_index: tool_idx,
                        scroll: 0,
                    });
                }
            }
            _ => {}
        },
```

- `reset_current_tab()` 新增 `SettingsTab::ToolConfig => { self.config_draft.tools.web_search = default.tools.web_search; self.config_draft.tools.prefer_docker = default.tools.prefer_docker; }`；
- 数字直达键：`if let KeyCode::Char(ch @ '1'..='8') = key.code { ... tabs.get(ch as usize - '1' as usize) ... }`（:9150-9157）改为 `'1'..='9'`，其余不动（`SettingsTab::all()` 第 9 个即 `ToolConfig`，无需加分支）；
- `render_tab_bar` 的宽度降级链无字面量 tab 数，不改；
- `"1-8"` 字样出现处全部改为 `"1-9"`：setup 模式 nav 提示（:8734）、ToolsMcp 两条 arm（:8754/:8756）、Toolbox arm（:8760）、catch-all arm（:8762-8763）。新页的 nav_hint arm 也直接写 `1-9 直达分类`；
- `draw_settings_panel` 的 tip（:8582-8622）、buttons（:8628-8731）、nav_hint（:8733-8765）三个 match 各追加一条 `SettingsTab::ToolConfig` arm：
  - tip：`"💡 提示: ↑/↓ 选择 · ←/→ / 空格 切换开关 · Enter 查看工具详情 · R 恢复默认 · Tab 轮换分类"`（照抄其他 arm 的 `Line::from(...)` 形式）；
  - buttons：与 `SettingsTab::UiWorkflow`/`Subagents` 这类「无列表编辑」页同款 arm（含 `[ S 保存生效 (Ctrl+S) ]`/`[ Esc 关闭 ]`/`[ R 恢复默认 ]`，宽度自适应分支照抄相邻 arm）。
  - nav_hint：窄宽分支 `"操作: ↑/↓ · Enter 详情 · ←/→ 切开关 · 1-9 直达 · Esc 关闭"`，宽分支 `"操作: ↑/↓ 选择项目 · ←/→ / 空格 切换开关 · Enter/空格/o 查看工具详情 · R 恢复默认 · Tab 轮换 · 1-9 直达 · Esc 关闭"`。

5.4 收尾自检（机械）：`grep -n "selected_row == 6\|selected_row >= 3\|selected_row == 11\|2 + state.skills.len()\|tools.prefer_docker\|tools.web_search" crates/cyber-tui/src/cli.rs` 逐条确认：cli.rs 中 `tools.web_search` 只应出现在「9. 工具配置」页的渲染/按键/重置三处 + `reset` 之外无残留旧位置；`tools.prefer_docker` 同理。

### 步骤 6 — 操作逻辑总表（按键 → 行为 → 代码锚点）

**(a) 设置中心通用键：由 `handle_settings_key` 顶部的既有分支处理，新页**无需**写任何代码（顺序即优先级，见 :9110-9205）：**

| 按键 | 行为 | 代码 |
| --- | --- | --- |
| `Ctrl+S` / `s` / `S` | 保存草稿（`save_settings_state(screen, runner, permissions, false)`，setup 模式走 `setup::commit`，否则 `save_config` + `save_providers`）→ 状态行 `✔ 设置已保存并立即生效` | :9110-9113、:9169-9172 |
| `Ctrl+D` | 吞掉（不做删除、不退出） | :9117-9119 |
| `Esc` | setup 模式：dirty 则提示先保存，否则完成向导退出；普通模式：dirty → `pending_discard_confirm = true`（再按 Esc 丢弃），否则关闭面板 | :9121-9140 |
| `Tab` / `Shift+Tab` / `BackTab` | 下一个 / 上一个分类（`next_tab` / `prev_tab`，纯 `all()` 取模；切页会重置 `selected_row = 0`、清 `skill_detail`/`memory_detail`/`tool_detail`） | :9142-9153 |
| `1`..`9` | 直达分类（`'1'..='8'` → `'1'..='9'`；`9` = 新页） | :9155-9164 |
| `r` / `R` | `reset_current_tab()`（新页重置 `web_search` + `prefer_docker`）→ 状态行 `已将 9. 工具配置 恢复为默认配置` | :9166-9172 |
| `↑`/`k`、`↓`/`j` | `selected_row` ∓1 / ±1（`min(max_row)`；`max_row = 2 + tools.len()`） | :9182-9190 |
| `Home` / `End` | `selected_row = 0` / `= max_row`（末行为最后一个工具） | :9191-9197 |
| `PageUp` / `PageDown` | `selected_row` ∓5 / ±5（clamp 到 `max_row`） | :9198-9205 |
| `F3` / `Ctrl+,` | 关闭设置面板（dirty 时先走丢弃确认） | 面板路由段（`handle_key` 的 F3/Ctrl+, 分支） |

**(b) 新页专属键（步骤 5.3 的新 arm，行号 0-based）：**

| 行 | 按键 | 行为 |
| --- | --- | --- |
| 0 `web_search` | `←` / `→` / `空格` / `Enter` | 翻转 `config_draft.tools.web_search`，`dirty = true`（面板成对显示 `[ ● 开启 ]` / `[ ○ 关闭 ]`） |
| 1 `prefer_docker` | `←` / `→` / `空格` / `Enter` | 翻转 `config_draft.tools.prefer_docker`，`dirty = true` |
| 2 `extra_path`（只读） | 任意键 | 无操作（值只能在 `config.toml` 里改；状态行不变化） |
| 3..`2+N` 工具行 | `Enter` / `空格` / `o` / `O` | 打开 `tool_detail = Some(ToolDetailModal { tool_index: row - 3, scroll: 0 })`（越界时无操作） |
| 3..`2+N` 工具行 | 其他键 | 落到通用键（↑↓/Home/End/PgUp/PgDn `1-9`/`Tab`/`R`/`S`），不改变 `tools` 快照（快照只读，运行期不改） |

**(c) 工具详情弹层键（步骤 4.2 的拦截块，弹层打开时吞掉其余按键）：**

| 按键 | 行为 |
| --- | --- |
| `Esc` / `q` / `Q` / `Enter` / `空格` | 关闭弹层（`tool_detail = None`），回到列表且 `selected_row` 不变 |
| `↑` / `k`、`↓` / `j` | 正文滚屏 ∓1 / ±1 行 |
| `PageUp` / `PageDown` | 滚屏 ∓10 / ±10 行 |
| `Home` / `End` | 滚屏到顶 / 到底（`scroll = usize::MAX` 由渲染端 clamp） |
| `←` / `h`、`→` / `l` | 上/下一个工具（`(idx ± 1) mod tools.len()`），并同步 `settings.selected_row = 3 + tool_index`（列表光标跟随，与 skill 弹层一致） |
| 其他 | 无操作（不透传到底层页面） |

### 步骤 7 — 界面渲染规范（布局数学 + ASCII 示例）

**布局来源（不新增容器）**：新页与其它页共用 `draw_settings_panel` 的 6 段切分（`popup_area = bounds` 全屏；`inner = block.inner(popup_area)` = 宽-2/高-2）：

| chunk | 高度 | 内容 |
| --- | --- | --- |
| `chunks[0]` | 1 | `render_tab_bar(chunks[0].width, settings.tab)`（不需改） |
| `chunks[1]` | 1 | `"─".repeat(width)` 分隔线 |
| `chunks[2]` | `Min(3)`（120×36 时 = 29） | `draw_tab_tools_config(frame, chunks[2], settings, screen)` |
| `chunks[3]` | 1 | tip（新增 ToolConfig arm） |
| `chunks[4]` | 1 | 按钮行（新增 ToolConfig arm） |
| `chunks[5]` | 1 | 键位提示（新增 ToolConfig arm） |

**内容行顺序**（`draw_tab_tools_config` 内，`settings.tools.len() = N`）：

| 行号 | 内容 | 焦点 |
| --- | --- | --- |
| 0 | 标题行「工具与外部环境配置（写入 ~/.cyber/config.toml 的 [tools] 段）」（MUTED） | 不可聚焦 |
| 1 | `web_search` 配置行（`render_setting_row(selected_row == 0, …)`） | `selected_row == 0` |
| 2 | `prefer_docker` 配置行（`== 1`） | `== 1` |
| 3 | `extra_path` 只读行（`== 2`） | `== 2` |
| 4 | 空行 | — |
| 5 | `[已注册工具 (N 个)] (↑/↓ 移动焦点 · Enter 查看详情；含仅内部工具)`（CODE 粗体） | 不可聚焦 |
| 6..`5+N` | 第 i 个工具行（焦点行号 `3 + i`，见下） | `selected_row == 3 + i` |

即「内容第 r 行 ↔ `selected_row = r - 3`」的映射（工具表头在第 5 行、第一个工具在第 6 行 = content 起点之后的第 6 行）。`render_scrollable_content` 只在选中行超出视口时按 `focused_start/focused_end` 跟随滚动，列表本身不加用户可控滚动条。

**工具行跨度（每行 1 行）**：`▶ ` / `  `（2 列，选中 ACCENT 粗体，否则 DIM）→ `{:<34}` 名称（选中 FG 粗体，否则 FG）→ 来源徽标（`[内置]` CODE / `[自定义]` SUCCESS / `[MCP]` ACCENT / `[Skill]` CODE / `[其他]` DIM，各带两侧空格）→ `[仅内部]`（DIM，仅 `!model_visible`）→ ` #{tags} `（CODE，标签之间 ` #`，无标签则跳过）→ ` {description}`（MUTED，空则跳过）→ 选中行追加 `  [按 Enter 查看详情]`（ACCENT 粗体）。整行超宽由 ratatui 自然裁剪（与 Skills 行同款处理，不额外裁剪）。

**示例：120×36 终端、`settings.tab = ToolConfig`、`selected_row = 3`（第一个工具）、N = 31**

```
╭ ⚙ Cyber Master 全局设置中心 (Settings)  [✔ 配置已实时保存] ────────────────────────────────────────────────────╮
│ 1.Agent模型  2.界面交互  3.子任务  4.工具MCP  5.服务商  6.环境记忆  7.系统存储  8.工具库  9.工具配置           │
│ ────────────────────────────────────────────────────────────────────────────────────────────────────────────── │
│   工具与外部环境配置（写入 ~/.cyber/config.toml 的 [tools] 段）                                                │
│ ▶ 联网搜索与抓取 (Web Search)      [ ● 开启 ]    启用/禁用 web_fetch 外部网络查询工具                          │
│   优先容器执行 (Docker)            [ ○ 关闭 ]    若环境安装 Docker，则优先容器隔离执行                         │
│   额外 PATH (tools.extra_path)     (空)          只读：在 config.toml 的 [tools].extra_path 中编辑（本面板不可改）│
│                                                                                                                │
│   [已注册工具 (31 个)] (↑/↓ 移动焦点 · Enter 查看详情；含仅内部工具)                                           │
│ ▶ ask_user                        [内置]                                向用户提出澄清问题并等待回答            │
│   bg_kill                         [内置]                                终止指定后台任务                      │
│   binary_inspect                  [内置]                                二进制文件静态分析（ELF/PE 结构、字符串）│
│   custom_nmap                     [自定义] [仅内部] #扫描 #recon        端口与服务指纹扫描                     │
│   delegate_tasks                  [内置]                                并行委派子 agent 执行独立子任务        │
│   dns_recon                       [内置]                                DNS 记录与子域枚举                    │
│   …（滚动跟随选中行）…                                                                                         │
│   read_file                       [内置]                                读取文件内容（支持行范围）             │
│   shell                           [内置]                                执行 shell 命令（含审批）              │
│   use_skill                       [内置] #meta #skill                   获取指定 Skill 的详细使用说明         │
│   web_fetch                       [内置]                                抓取网页内容                          │
│                                                                                                                │
│ 💡 提示: ↑/↓ 选择 · ←/→ / 空格 切换开关 · Enter 查看工具详情 · R 恢复默认 · Tab 轮换分类                      │
│ [ S 保存生效 (Ctrl+S) ]  [ Esc 关闭 ]  [ R 恢复默认 ]                                                          │
│ 操作: ↑/↓ 选择项目 · ←/→ / 空格 切换开关 · Enter/空格/o 查看工具详情 · R 恢复默认 · Tab 轮换 · 1-9 直达 · Esc 关闭│
╰────────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
```

（示例中的列宽是示意；实际横向留白由既有 `render_setting_row` 的 `PREFIX_W` 与 `clip_cells_ellipsis` 决定，实现时**直接复用**该 helper，不手写间距。tab 栏在 120 宽时走 `short_title` 档：9 项合计 112 列 ≤ 118，无 `◄`/`►`。）

**宽度自适应**：tab 栏档位由 `render_tab_bar` 自动选择——≥120 宽（inner ≥118）用 `short_title`（112 列）、100/90 宽用 `compact_title`（合计 86 列）、≤80 宽（inner 78）超出后变成围绕当前页的滑动窗口（形如 `◄ 8.工具  9.工具配置 ►`）；新页的 tip/按钮/键位提示三行各有窄宽分支（照抄 `ToolsMcp`/`EnvMemory` 的 `if chunks[5].width < 90 { … }` 写法）。

**示例：工具详情弹层（90×22 终端）**

```
╭ 🔧 工具详情 [4/31] custom_nmap ────────────────────────────────────────────────────────╮
│   名称: custom_nmap                                                                     │
│   来源: 自定义                                                                          │
│   对模型可见: 否（仅内部：可被显式调用，但不出现在模型 tools 数组）                      │
│   说明: 端口与服务指纹扫描                                                              │
│   标签: 扫描, recon                                                                     │
│                                                                                          │
│   参数 JSON Schema:                                                                      │
│       {                                                                                  │
│         "type": "object",                                                                │
│         "properties": {                                                                  │
│           "target": {                                                                    │
│             "type": "string",                                                            │
│             "description": "目标主机或网段"                                              │
│           }                                                                              │
│         },                                                                               │
│         "required": ["target"]                                                           │
│       }                                                                                  │
│   …（PgUp/PgDn/Home/End 滚屏，滚动偏移由 detail.scroll 控制）…                            │
│ [↑/↓/PgUp/PgDn] 滚屏 · [←/→] 切换上/下一个工具 · [Esc/q/Enter] 关闭返回                 │
╰──────────────────────────────────────────────────────────────────────────────────────────╯
```

（几何：`width = (parent.width * 85 / 100).max(70).min(parent.width)`、`height = (parent.height * 85 / 100).max(20).min(parent.height)`，居中；`clear_popup_under` 先清底；标题用 `clip_cells_ellipsis` 裁到 `width - 2`。`tool_index >= tools.len()` 直接返回。）

## Critical files & anchors

1. `crates/cyber-tui/src/cli.rs` —— 全部改动都在此文件：
   - `SettingsTab` 定义与 `all/title/short_title/compact_title/max_row`：:154-238
   - `SkillSummary`（新区块放它后面）：:248-260
   - `SkillDetailModal` / `MemoryDetailModal`（新 `ToolDetailModal` 照抄）：:261-283
   - `CliSettingsState` 字段 + `new()` + `from_runner` + `set_tab`/`reset_current_tab`：:287-505
   - `render_setting_row` / `render_scrollable_content`：:5657-5720
   - `draw_tab_agent_model`：:5700-6xxx（web_search 行 :5805-5813）
   - `draw_tab_tools_mcp`：:6019-6215（prefer_docker :6030-6040、extra_path 显示行 :6074-6081）
   - `draw_tab_toolbox`（新函数放它后）：:6308 起
   - `draw_skill_detail_modal` / `clear_popup_under`（新弹层照抄）：:6862 起
   - `draw_settings_panel`（tab 分派 :8570-8579、tip :8582-8622、buttons :8628-8731、nav_hint :8733-8765、弹层调用 :8771-8777）：:8505-8780
   - `render_tab_bar`：:8691 起（无改动，仅验证窄宽可用）
   - `handle_settings_key`（弹层拦截 :8963、数字直达 :9120、`R` 重置 :9163、AgentModel 分支 :9250+、ToolsMcp 分支 :9551-9604）：:8890-9800
2. `crates/cyber-tui/src/headless.rs:460` —— `SessionRunner.registries: AppRegistries`（`from_runner` 取 `tools`/`skills` 快照的来源）。
3. `crates/cyber-agent/src/tool.rs:15-24 / :227 / :235` —— `ToolSchema` 与 `schemas()`/`all_schemas()`。
4. `crates/cyber-agent/src/tools/mod.rs:46` —— `builtin_tool_names()`（27 个内置工具名）。
5. `crates/cyber-skills/src/tool.rs:48-53`、`crates/cyber-tui/src/bootstrap.rs:61-98` —— `skill_*` 命名、自定义工具/MCP 工具注册为 hidden 的事实依据。

## Verification

### 自动化测试（新增，全部放 `crates/cyber-tui/src/cli.rs` 的 `#[cfg(test)] mod tests` 内，复用既有 helper：`crate::headless::tests::test_runner()`、`CliScreen::new(&owner)`、`render(&mut screen, w, h)`、`handle_key(...)`/`input_key(...)`、`settings_key_with_runner(...)`）

1. `settings_tool_config_tab_lists_tools_and_moved_rows`：
   - 输入：`test_runner()` → `CliScreen::new` → `screen.panel = Some(Panel::Settings)` → `screen.settings = Some(CliSettingsState::from_runner(&owner))` → `settings.tab = SettingsTab::ToolConfig` → `render(&mut screen, 120, 40).replace(' ', "")`。
   - 期望（逐条，均为步骤 6/7 规范的可观察对应物）：
     - 标签栏含 `"9.工具配置"`；
     - 含 `"工具与外部环境配置"`、`"联网搜索与抓取(WebSearch)"`、`"优先容器执行(Docker)"`、`"额外PATH(tools.extra_path)"`、`"已注册工具("`；
     - 三类配置行与工具表头的**出现顺序**与规范一致（用 `find` 比较字节下标：标题 < web_search < prefer_docker < extra_path < 已注册工具(）；
     - 至少包含 3 个真实工具名（如 `"shell"`、`"read_file"`、`"todo"`）与至少一次 `"[内置]"`；
     - 若 `settings.tools.iter().any(|t| !t.model_visible)` 则渲染串含 `"[仅内部]"`（test_runner 的注册表含自定义/隐藏工具时成立；不成立时跳过该断言，不要为了让断言通过去改注册表）；
     - `SettingsTab::ToolConfig.max_row(settings) == 2 + settings.tools.len()` 且 `settings.tools.len() >= 20`（内置工具 27 个，见 `crates/cyber-agent/src/tools/mod.rs:46`）。
2. `settings_tool_config_toggles_flag_dirty_and_persist`：
   - 输入：同上准备，`settings.selected_row = 0` → `handle_key(Enter)`；再 `settings.selected_row = 1` → `handle_key(Char(' '))`；然后 `handle_key(Ctrl+S)`。
   - 期望：两次按键后 `settings.config_draft.tools.web_search` 与 `.prefer_docker` 都取反、`settings.dirty == true`；Ctrl+S 后 `std::fs::read_to_string(&owner.ctx.paths.config_file)` 中出现翻转后的 `web_search = ...` 与 `prefer_docker = ...`，且 `settings.dirty == false`（照抄既有 settings 保存测试的读盘断言写法）。
3. `settings_tool_config_enter_opens_tool_detail_and_esc_closes`：
   - 输入：同上准备，`settings.selected_row = 3` → `handle_key(Enter)`。
   - 期望：`settings.tool_detail.as_ref().unwrap().tool_index == 0`；`render(120, 40)` 含 `"工具详情"`、`"[1/N]"`（N = 工具数）、`"名称:"`、`"来源:"`、`"对模型可见:"`、`"参数JSONSchema:"`、该工具名与 `"[←/→]切换上/下一个工具"`；再断言 `handle_key(Down)` 后 `detail.scroll == 1`（弹层吞键且不改 `selected_row`）、`handle_key(Right)` 后 `tool_index == 1` 且 `settings.selected_row == 4`（列表光标同步）；`handle_key(Esc)` 后 `tool_detail.is_none()`、`selected_row` 保持 4、`screen.panel` 仍为 `Some(Panel::Settings)`。
4. `settings_moved_rows_are_gone_and_renumbered`：
   - 输入：分别 `set_tab(SettingsTab::AgentModel)` 与 `set_tab(SettingsTab::ToolsMcp)` 后 `render(120, 40)`。
   - 期望：AgentModel 页渲染串**不含** `"联网搜索与抓取"`，`SettingsTab::AgentModel.max_row(settings) == 10`，且 `settings.selected_row = 6; handle_key(Enter)` 改变的是 `config_draft.agent.vision.enabled`（即原第 7 行上移为第 6 行）；ToolsMcp 页渲染串**不含** `"优先容器执行"` 与 `"额外环境变量PATH"`，`SettingsTab::ToolsMcp.max_row(settings) == 1 + settings.skills.len()`（`test_runner()` 的临时 skills 目录为空 → 实际为 1），且 `selected_row = 0; handle_key(Enter)` 翻转 `screen.ctf_enabled`、`selected_row = 1; handle_key(Enter)` 使 `screen.panel == Some(Panel::Mcp)`；技能行位移（`3+i` → `2+i`）由既有 `test_skill_detail_modal_navigation_and_rendering`（它自建 skills）覆盖，本测试不重复。
5. `settings_digit_nine_selects_tool_config_tab`：
   - 输入：`panel = Some(Panel::Settings)` + `settings = from_runner`，`handle_key(Char('9'))`。
   - 期望：`settings.tab == SettingsTab::ToolConfig` 且 `selected_row == 0`。

### 既有测试的更新（逐条，含行号与新的期望值）

跑 `cargo test -p cyber-tui --lib`，按新行为更新下列断言（**不是**恢复旧行为）：

1. `settings_tab_navigation_and_direct_keys`（:17793-17860）：`expected_tabs` 数组（:17812-17820）追加 `SettingsTab::ToolConfig`（8→9 项）；`direct_keys`（:17841-17851）追加 `('9', SettingsTab::ToolConfig)`。
2. `settings_panel_rendering_all_tabs`（:18418-18456）：循环 `for tab_idx in 1..=7` 改为 `1..=8`（否则新页永远不参与渲染断言）。
3. `settings_agent_model_retry_configuration`（:18053-18178）：`SettingsTab::AgentModel.max_row(...) == 11`（:18070-18073）→ `10`；`settings.selected_row = 10`（:18076）→ `9`；`selected_row = 11`（~:18100）→ `10`。
4. `test_tools_mcp_max_row_with_skills`（:18647-18671）：期望 `2 / 4 / 117`（:18650/:18662/:18670）→ `1 / 3 / 116`。
5. `test_skill_detail_modal_navigation_and_rendering`（:18673-18862）：`settings.selected_row = 3`（:18735）→ `2`；断言 `selected_row == 4`（~:18800）→ `3`；`selected_row == 3`（:18818/:18831/:18844）→ `2`。
6. `settings_discard_confirm_save_and_exit`（:18320-18363）：`selected_row = 10`（:18329）→ `9`。
7. `settings_single_key_s_saves_in_place`（:18365-18416）：`selected_row = 10`（:18374）→ `9`。
8. `settings_panel_renders_fullscreen_canvas`（:20919-20930）：渲染 smoke，无需改断言；但必须确认 120×36 下标签栏含 `Agent` 且新页可达（若标签栏降级导致断言文案变化，按渲染实际值更新）。
9. 无需改动（已核对）：`settings_toolbox_tab_lists_tools_and_gates_scan_in_setup_mode`（:17655-17722，Toolbox 索引在其后不变）、`settings_in_place_value_adjustments`（:17864-18052）、`test_detail_modals_border_survives_underlying_wide_text`（:21429-21503）、`settings_tab_7_focus_following_and_rendering`（:18458-18495）、`settings_provider_and_env_vertical_focus_following`（:18497-18645）、`settings_unsaved_discard_guard`（:18180-18246）、`settings_save_and_persist`（:18248-18318）、`setup_mode_*`、`settings_panel_open_via_action_and_key`（:17724-17791）。没有任何测试断言 `SettingsTab::all().len()`，也没有测试断言 `web_search`/`prefer_docker` 的翻转或 `extra_path` 展示行（新增测试第 2、4 条即补上这层覆盖）。

### 命令与前置条件

工作目录 `C:/Users/chuzo/Desktop/Project/agent/cyber_master`：

```
cargo test -p cyber-tui --lib
cargo test --workspace --lib
```

### 真实界面冒烟（隔离 home，不碰用户真实配置）

1. `cargo build`。
2. 在 pty 里启动：`CYBER_HOME=target/smoke-home ./target/debug/cyber.exe --mock`（`--mock` 跳过首启配置闸门；`target/smoke-home` 为一次性目录，冒烟后删除）。
3. 输入 `/settings` + Enter 打开设置中心；按 `9` → 断言屏幕出现 `9. 工具配置` 且该页显示 `联网搜索与抓取 (Web Search)`、`优先容器执行 (Docker)`、`额外 PATH`、`已注册工具 (N 个)`、至少 3 个工具名与 `[内置]` 徽标。
4. 按 `Enter`（第 0 行 = web_search）→ 断言该项显示翻转为 `[ ○ 关闭 ]`；按 `Ctrl+S` → 断言 `target/smoke-home/config.toml` 出现 `web_search = false`。
5. 按 `1` → 断言「1. Agent & 模型」页**不再**出现 `联网搜索与抓取`；按 `4` → 断言「4. 工具与 MCP」页**不再**出现 `优先容器执行` 与 `额外环境变量 PATH`，且该页首行现在是 `CTF 渗透答题模式 (CTF Mode)`。
6. （可选，箭头键在本地 pty 中继上不可靠）用 `j`/`k` 或 `↓` 移动到工具行并按 `Enter`，断言弹层标题为 `工具详情 [1/N]`。

## Assumptions & contingencies

- **顺序与依赖**：步骤 1-4 与 5 有依赖（新页必须先存在，才能删旧行而不丢功能）；步骤 6 的测试在步骤 5 之后才可能全绿。若执行中途需要保持「随时可编译」，可在步骤 1-4 完成后先跑一次 `cargo check -p cyber-tui`。
- **`extra_path` 保持只读**：用户要求「把 setup 的工具配置内容搬过来」，setup 里 `extra_path` 也是只读展示（CLI 非可聚焦行、TUI `ReadOnly` + `noop_set`）。若实测希望可编辑，需要新增编辑交互（超出本次范围），届时改为「按 Enter 进入行内编辑并写回 `config_draft.tools.extra_path`」。
- **隐藏工具是否列出**：本页用 `all_schemas()`（含 hidden：自定义工具、MCP 具体工具、超 32 个 Skill 时的 `skill_*`），并给非模型可见项加 `[仅内部]` 徽标——与 CLI `/tools` 用 `all_schemas()` 的现状一致（`crates/cyber-tui/src/cli_commands.rs:1273-1284`）。若实测认为隐藏工具噪音太大，改为只用 `schemas()` 并按 `[仅内部]` 徽标逻辑一并删除；不要改成两处口径不一致。
- **`ToolsMcp` 页文案**：搬走后该页只剩 CTF 模式 + MCP 控制台 + Skills 列表，标题仍保留 `4. 工具与 MCP`（不改名以避免牵连文档与既有测试）。若实测认为标题误导，改成 `4. 扩展与 MCP` 需同步 `title/short_title/compact_title` 三处与相关测试。
- **`self.tools` 快照时机**：与 skills/custom_tools 一样在 `from_runner` 一次性快照（打开设置中心时）。运行期新增的自定义工具（`/toolbox add`）要重开设置中心才会出现在列表——与现有 skills/custom_tools 行为一致。
