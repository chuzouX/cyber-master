//! Provider 表单模态层：新增 / 编辑单个 LLM 服务商。
//!
//! 从 Settings（`a`/`e`）或 Chat（`/provider add|edit`）进入，作为顶层 `Mode::ProviderForm`
//! 渲染。字段：name / kind / base_url / api_key / model / max_tokens / temperature +
//! 价格（input_per_m / output_per_m / cache_hit_per_m，可选，用于 TUI 显示成本）+
//! 三个按钮：拉取模型 / 保存 / 取消。
//!
//! 文本字段用单个复用 `TextArea`：Enter 进入编辑（load 值）→ 输入 → Enter 提交 / Esc 取消编辑。
//! `kind` 用 ←→ 循环 `PROVIDER_KINDS`。「拉取模型」异步 GET `{base}/models`，结果经 mpsc
//! 回传 App → `deliver_fetch`，弹出 picker 选中后回填 model 字段。

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame,
};
use std::cell::Cell;
use tui_textarea::TextArea;

use cyber_core::{ModelConfig, PriceConfig, ProviderConfig, ProviderConfig as _Cfg, ProvidersConfig, PROVIDER_KINDS};

use crate::theme::Theme;

/// 字段类型（决定编辑方式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    Text,
    Enum,
    Button,
}

struct FieldDef {
    label: &'static str,
    kind: FieldKind,
}

/// 价格货币选项（←→ 循环）。与 `PriceConfig::currency` 对应。
const CURRENCIES: &[(&str, &str)] = &[("usd", "美元"), ("cny", "人民币")];

/// 字段顺序即焦点导航顺序（Up/Down 循环）。
/// 0-4 provider 基本字段，5-13 为当前 model 的个性化参数（含价格及单位），
/// 14-15 高级选项，16=拉取模型，17=保存，18=取消。
const FIELDS: &[FieldDef] = &[
    FieldDef { label: "名称 name", kind: FieldKind::Text },
    FieldDef { label: "类型 kind", kind: FieldKind::Enum },
    FieldDef { label: "base_url", kind: FieldKind::Text },
    FieldDef { label: "api_key", kind: FieldKind::Text },
    FieldDef { label: "model", kind: FieldKind::Text },
    FieldDef { label: "别名 alias（显示名，留空用 model id）", kind: FieldKind::Text },
    FieldDef { label: "上下文长度 context_length", kind: FieldKind::Text },
    FieldDef { label: "max_tokens（最大输出 token）", kind: FieldKind::Text },
    FieldDef { label: "temperature（温度）", kind: FieldKind::Text },
    FieldDef { label: "输入价格 /M (input_per_m)", kind: FieldKind::Text },
    FieldDef { label: "输出价格 /M (output_per_m)", kind: FieldKind::Text },
    FieldDef { label: "缓存命中价格 /M (cache_hit_per_m)", kind: FieldKind::Text },
    FieldDef { label: "价格单位 currency", kind: FieldKind::Enum },
    FieldDef { label: "备注 notes", kind: FieldKind::Text },
    FieldDef { label: "自定义对话端点 chat_endpoint（留空默认 {base_url}/chat/completions）", kind: FieldKind::Text },
    FieldDef { label: "自定义模型列表端点 models_endpoint（留空默认 {base_url}/models）", kind: FieldKind::Text },
    FieldDef { label: "拉取模型", kind: FieldKind::Button },
    FieldDef { label: "保存", kind: FieldKind::Button },
    FieldDef { label: "取消", kind: FieldKind::Button },
];
const IDX_KIND: usize = 1;
const IDX_MODEL: usize = 4;
const IDX_CURRENCY: usize = 12;
const IDX_FETCH: usize = 16;
const IDX_SAVE: usize = 17;
const IDX_CANCEL: usize = 18;

/// 表单按键的副作用意图，由 App 解释执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormAction {
    None,
    Save,
    Cancel,
    Fetch,
    Toast(String),
}

/// Provider 表单状态。
pub struct ProviderFormState {
    pub name: String,
    pub kind_idx: usize,
    /// 价格货币索引（CURRENCIES），0=usd, 1=cny。
    pub currency_idx: usize,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    // ── 当前 model 的参数（per-model，存入 models map）──
    pub alias: String,
    pub context_length: String,
    pub max_tokens: String,
    pub temperature: String,
    /// 每百万输入 token 价格（美元），空串 = 未配置。
    pub price_input: String,
    /// 每百万输出 token 价格（美元），空串 = 未配置。
    pub price_output: String,
    /// 每百万缓存命中输入 token 价格（美元），空串 = 未配置。
    pub price_cache_hit: String,
    pub notes: String,
    /// 自定义对话端点（高级选项）。空 = 使用默认。
    pub chat_endpoint: String,
    /// 自定义模型列表端点（高级选项）。空 = 使用默认。
    pub models_endpoint: String,
    // ── 工作副本 ──
    /// provider 的 models map 工作副本（编辑期间维护，保存时写回）。
    pub models: std::collections::HashMap<String, ModelConfig>,
    /// 已知 model 列表（←→ 切换用）：models map 的 key + 当前 model + 拉取到的 model。
    pub known_models: Vec<String>,
    /// provider 级默认值（model 无 per-model 配置时的回退）。
    pub provider_max_tokens: u32,
    pub provider_temperature: f32,
    pub provider_price: Option<PriceConfig>,
    // ── UI 状态 ──
    /// `Some` = 编辑现有（值为原始 name）；`None` = 新增。
    pub original_name: Option<String>,
    pub focused: usize,
    pub editing: bool,
    pub textarea: TextArea<'static>,
    pub fetching: bool,
    pub fetch_id: u64,
    pub fetch_error: Option<String>,
    pub fetched_models: Vec<String>,
    pub picker_open: bool,
    pub picker_selected: usize,
    /// Picker 滚动偏移（render 时按选中项自动调整，Cell 供 &self render 写回）。
    pub picker_scroll: Cell<usize>,
}

impl ProviderFormState {
    /// 新增模式：默认值（openai / 空 url / 4096 / 0.7）。
    pub fn empty() -> Self {
        let mut textarea = TextArea::default();
        textarea.set_placeholder_text("输入…");
        Self {
            name: String::new(),
            kind_idx: 0,
            currency_idx: 0,
            base_url: String::new(),
            api_key: String::new(),
            model: String::new(),
            alias: String::new(),
            context_length: String::new(),
            max_tokens: "4096".into(),
            temperature: "0.7".into(),
            price_input: String::new(),
            price_output: String::new(),
            price_cache_hit: String::new(),
            notes: String::new(),
            chat_endpoint: String::new(),
            models_endpoint: String::new(),
            models: std::collections::HashMap::new(),
            known_models: Vec::new(),
            provider_max_tokens: 4096,
            provider_temperature: 0.7,
            provider_price: None,
            original_name: None,
            focused: 0,
            editing: false,
            textarea,
            fetching: false,
            fetch_id: 0,
            fetch_error: None,
            fetched_models: Vec::new(),
            picker_open: false,
            picker_selected: 0,
            picker_scroll: Cell::new(0),
        }
    }

    /// 编辑模式：从现有 provider 装载。
    pub fn from_provider(name: &str, cfg: &ProviderConfig) -> Self {
        let kind_idx = PROVIDER_KINDS
            .iter()
            .position(|k| *k == cfg.kind)
            .unwrap_or(0);
        let mut s = Self {
            name: name.to_string(),
            kind_idx,
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            models: cfg.models.clone(),
            known_models: {
                let mut list: Vec<String> = cfg.models.keys().cloned().collect();
                let cur = cfg.model.trim().to_string();
                if !cur.is_empty() && !list.contains(&cur) {
                    list.push(cur);
                }
                list.sort();
                list
            },
            provider_max_tokens: cfg.max_tokens,
            provider_temperature: cfg.temperature,
            provider_price: cfg.price.clone(),
            chat_endpoint: cfg.chat_endpoint.clone().unwrap_or_default(),
            models_endpoint: cfg.models_endpoint.clone().unwrap_or_default(),
            original_name: Some(name.to_string()),
            ..Self::empty()
        };
        // 装载当前 model 的参数（per-model 优先，回退到 provider 级）
        s.load_model_params(&cfg.model.clone());
        // 编辑模式焦点先停在 base_url（name 一般不改）
        s.focused = 2;
        s
    }

    /// 从 `models[model]` 装载参数到表单字段；per-model 字段为 None 时回退到 provider 级默认。
    fn load_model_params(&mut self, model: &str) {
        let mc = self.models.get(model);
        self.alias = mc.and_then(|m| m.alias.clone()).unwrap_or_default();
        self.context_length = mc
            .and_then(|m| m.context_length)
            .map(|v| v.to_string())
            .unwrap_or_default();
        self.max_tokens = mc
            .and_then(|m| m.max_tokens)
            .map(|v| v.to_string())
            .unwrap_or_else(|| self.provider_max_tokens.to_string());
        self.temperature = mc
            .and_then(|m| m.temperature)
            .map(|v| v.to_string())
            .unwrap_or_else(|| self.provider_temperature.to_string());
        let price = mc.and_then(|m| m.price.as_ref()).or(self.provider_price.as_ref());
        self.price_input = price
            .and_then(|p| p.input_per_m)
            .map(|v| v.to_string())
            .unwrap_or_default();
        self.price_output = price
            .and_then(|p| p.output_per_m)
            .map(|v| v.to_string())
            .unwrap_or_default();
        self.price_cache_hit = price
            .and_then(|p| p.cache_hit_per_m)
            .map(|v| v.to_string())
            .unwrap_or_default();
        // 装载货币
        self.currency_idx = match price.and_then(|p| p.currency.as_deref()) {
            Some("cny") => 1,
            _ => 0, // "usd" 或 None → 默认美元
        };
        self.notes = mc.and_then(|m| m.notes.clone()).unwrap_or_default();
    }

    /// 将当前表单参数字段保存到 `models[model]`。model 为空则跳过。
    fn save_model_params(&mut self, model: &str) {
        if model.trim().is_empty() {
            return;
        }
        let mc = ModelConfig {
            alias: if self.alias.trim().is_empty() {
                None
            } else {
                Some(self.alias.trim().to_string())
            },
            context_length: self.context_length.trim().parse().ok(),
            max_tokens: self.max_tokens.trim().parse().ok(),
            temperature: self.temperature.trim().parse().ok(),
            price: self.build_price(),
            notes: if self.notes.trim().is_empty() {
                None
            } else {
                Some(self.notes.trim().to_string())
            },
        };
        self.models.insert(model.trim().to_string(), mc);
    }

    /// ←→ 切换 model：保存当前 model 参数，装载新 model 参数。
    /// dir > 0 下一个，dir < 0 上一个。known_models 不足 2 个时无操作。
    fn switch_model(&mut self, dir: i32) {
        if self.known_models.len() <= 1 {
            return;
        }
        let current = self.model.trim().to_string();
        let n = self.known_models.len();
        let new_pos = match self.known_models.iter().position(|m| *m == current) {
            Some(p) => {
                if dir > 0 {
                    (p + 1) % n
                } else {
                    (p + n - 1) % n
                }
            }
            None => 0,
        };
        let new_model = self.known_models[new_pos].clone();
        if new_model != current {
            if !current.is_empty() {
                self.save_model_params(&current);
            }
            self.model = new_model.clone();
            self.load_model_params(&new_model);
        }
    }

    pub fn is_edit(&self) -> bool {
        self.original_name.is_some()
    }

    pub fn kind(&self) -> &'static str {
        PROVIDER_KINDS[self.kind_idx]
    }

    /// 当前价格货币代码："usd" 或 "cny"。
    pub fn currency(&self) -> &'static str {
        CURRENCIES[self.currency_idx].0
    }

    /// 当前价格货币显示名："美元" 或 "人民币"。
    pub fn currency_label(&self) -> &'static str {
        CURRENCIES[self.currency_idx].1
    }

    /// chat_endpoint 转为 Option：空串 → None，否则 Some。
    fn chat_endpoint_opt(&self) -> Option<String> {
        let s = self.chat_endpoint.trim();
        if s.is_empty() { None } else { Some(s.to_string()) }
    }

    /// models_endpoint 转为 Option：空串 → None，否则 Some。
    fn models_endpoint_opt(&self) -> Option<String> {
        let s = self.models_endpoint.trim();
        if s.is_empty() { None } else { Some(s.to_string()) }
    }

    /// 当前表单值的快照（用于 fetch，即使未校验通过也能拉取）。
    pub fn to_provider_config_snapshot(&self) -> ProviderConfig {
        ProviderConfig {
            kind: self.kind().to_string(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            model: self.model.clone(),
            max_tokens: self.max_tokens.trim().parse().unwrap_or(4096),
            temperature: self.temperature.trim().parse().unwrap_or(0.7),
            price: self.build_price(),
            models: self.models.clone(),
            chat_endpoint: self.chat_endpoint_opt(),
            models_endpoint: self.models_endpoint_opt(),
        }
    }

    /// 从表单价格字段构建 `PriceConfig`。全部为空时返回 None。
    fn build_price(&self) -> Option<cyber_core::PriceConfig> {
        let input = self.price_input.trim().parse::<f64>().ok();
        let output = self.price_output.trim().parse::<f64>().ok();
        let cache_hit = self.price_cache_hit.trim().parse::<f64>().ok();
        if input.is_none() && output.is_none() && cache_hit.is_none() {
            return None;
        }
        Some(cyber_core::PriceConfig {
            input_per_m: input,
            output_per_m: output,
            cache_hit_per_m: cache_hit,
            currency: Some(self.currency().to_string()),
        })
    }

    /// 校验并构造 `(name, ProviderConfig)`。失败返回错误文案（App 弹 toast）。
    pub fn into_provider(
        &self,
        existing: &ProvidersConfig,
    ) -> Result<(String, ProviderConfig), String> {
        let name = self.name.trim().to_string();
        if name.is_empty() {
            return Err("名称不能为空".into());
        }
        let original = self.original_name.as_deref().unwrap_or("");
        if name != original && existing.providers.contains_key(&name) {
            return Err(format!("名称 '{name}' 已存在"));
        }
        let base_url = self.base_url.trim().to_string();
        if base_url.is_empty() {
            return Err("base_url 不能为空".into());
        }
        let max_tokens: u32 = self
            .max_tokens
            .trim()
            .parse()
            .map_err(|_| "max_tokens 必须是数字".to_string())?;
        let temperature: f32 = self
            .temperature
            .trim()
            .parse()
            .map_err(|_| "temperature 必须是数字".to_string())?;
        // 价格字段校验：非空时必须是有效数字
        for (label, val) in [
            ("input_per_m", &self.price_input),
            ("output_per_m", &self.price_output),
            ("cache_hit_per_m", &self.price_cache_hit),
        ] {
            let trimmed = val.trim();
            if !trimmed.is_empty() {
                trimmed
                    .parse::<f64>()
                    .map_err(|_| format!("{label} 必须是数字"))?;
            }
        }
        Ok((
            name,
            ProviderConfig {
                kind: self.kind().to_string(),
                base_url,
                api_key: self.api_key.trim().to_string(),
                model: self.model.trim().to_string(),
                max_tokens,
                temperature,
                price: self.build_price(),
                models: self.build_models_map(max_tokens, temperature),
                chat_endpoint: self.chat_endpoint_opt(),
                models_endpoint: self.models_endpoint_opt(),
            },
        ))
    }

    /// 构建保存后的 models map：克隆工作副本，将当前表单参数写入 `models[self.model]`。
    /// model 为空时返回原始工作副本（不插入新条目）。
    fn build_models_map(&self, max_tokens: u32, temperature: f32) -> std::collections::HashMap<String, ModelConfig> {
        let mut models = self.models.clone();
        let model_id = self.model.trim();
        if !model_id.is_empty() {
            let mc = ModelConfig {
                alias: if self.alias.trim().is_empty() {
                    None
                } else {
                    Some(self.alias.trim().to_string())
                },
                context_length: self.context_length.trim().parse().ok(),
                max_tokens: Some(max_tokens),
                temperature: Some(temperature),
                price: self.build_price(),
                notes: if self.notes.trim().is_empty() {
                    None
                } else {
                    Some(self.notes.trim().to_string())
                },
            };
            models.insert(model_id.to_string(), mc);
        }
        models
    }

    fn get_field(&self, idx: usize) -> String {
        match idx {
            0 => self.name.clone(),
            1 => self.kind().to_string(),
            2 => self.base_url.clone(),
            3 => self.api_key.clone(),
            4 => self.model.clone(),
            5 => self.alias.clone(),
            6 => self.context_length.clone(),
            7 => self.max_tokens.clone(),
            8 => self.temperature.clone(),
            9 => self.price_input.clone(),
            10 => self.price_output.clone(),
            11 => self.price_cache_hit.clone(),
            12 => self.currency_label().to_string(),
            13 => self.notes.clone(),
            14 => self.chat_endpoint.clone(),
            15 => self.models_endpoint.clone(),
            _ => String::new(),
        }
    }

    fn set_field(&mut self, idx: usize, val: String) {
        match idx {
            0 => self.name = val,
            2 => self.base_url = val,
            3 => self.api_key = val,
            4 => self.model = val,
            5 => self.alias = val,
            6 => self.context_length = val,
            7 => self.max_tokens = val,
            8 => self.temperature = val,
            9 => self.price_input = val,
            10 => self.price_output = val,
            11 => self.price_cache_hit = val,
            13 => self.notes = val,
            14 => self.chat_endpoint = val,
            15 => self.models_endpoint = val,
            _ => {}
        }
    }

    fn is_text_field(idx: usize) -> bool {
        matches!(idx, 0 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 13 | 14 | 15)
    }

    fn start_editing(&mut self, idx: usize) {
        let val = self.get_field(idx);
        self.textarea.clear();
        self.textarea.insert_str(&val);
        self.editing = true;
    }

    /// 处理一个按键，返回副作用意图。`existing` 保留供未来实时校验（当前校验在 `into_provider`）。
    pub fn handle_key(&mut self, k: KeyEvent, _existing: &ProvidersConfig) -> FormAction {
        // Ctrl+C / Ctrl+Q 不在此处理（App 层统一作退出）
        if self.picker_open {
            return self.handle_picker_key(k);
        }
        if self.editing {
            return self.handle_editing_key(k);
        }
        match k.code {
            KeyCode::Up => {
                self.focused = (self.focused + FIELDS.len() - 1) % FIELDS.len();
                FormAction::None
            }
            KeyCode::Down => {
                self.focused = (self.focused + 1) % FIELDS.len();
                FormAction::None
            }
            KeyCode::Left => {
                if self.focused == IDX_KIND {
                    self.kind_idx =
                        (self.kind_idx + PROVIDER_KINDS.len() - 1) % PROVIDER_KINDS.len();
                } else if self.focused == IDX_MODEL {
                    self.switch_model(-1);
                } else if self.focused == IDX_CURRENCY {
                    self.currency_idx =
                        (self.currency_idx + CURRENCIES.len() - 1) % CURRENCIES.len();
                }
                FormAction::None
            }
            KeyCode::Right => {
                if self.focused == IDX_KIND {
                    self.kind_idx = (self.kind_idx + 1) % PROVIDER_KINDS.len();
                } else if self.focused == IDX_MODEL {
                    self.switch_model(1);
                } else if self.focused == IDX_CURRENCY {
                    self.currency_idx = (self.currency_idx + 1) % CURRENCIES.len();
                }
                FormAction::None
            }
            KeyCode::Enter => {
                match self.focused {
                    IDX_FETCH => {
                        if self.fetching {
                            FormAction::None
                        } else {
                            FormAction::Fetch
                        }
                    }
                    IDX_SAVE => FormAction::Save,
                    IDX_CANCEL => FormAction::Cancel,
                    IDX_KIND | IDX_CURRENCY => FormAction::None,
                    idx if Self::is_text_field(idx) => {
                        self.start_editing(idx);
                        FormAction::None
                    }
                    _ => FormAction::None,
                }
            }
            KeyCode::Esc => FormAction::Cancel,
            _ => FormAction::None,
        }
    }

    fn handle_editing_key(&mut self, k: KeyEvent) -> FormAction {
        match k.code {
            KeyCode::Enter => {
                let val = self.textarea.lines().join("\n");
                // 提交 model 字段时：先保存旧 model 参数，再装载新 model 参数
                if self.focused == IDX_MODEL {
                    let old_model = self.model.clone();
                    let new_model = val.trim().to_string();
                    if !old_model.is_empty() && old_model != new_model {
                        self.save_model_params(&old_model);
                    }
                    self.model = new_model.clone();
                    if old_model != new_model {
                        self.load_model_params(&new_model);
                    }
                    // 将新 model 加入 known_models（去重 + 排序）
                    if !new_model.is_empty() && !self.known_models.contains(&new_model) {
                        self.known_models.push(new_model.clone());
                        self.known_models.sort();
                    }
                } else {
                    self.set_field(self.focused, val);
                }
                self.editing = false;
                FormAction::None
            }
            KeyCode::Esc => {
                self.editing = false; // 丢弃改动
                FormAction::None
            }
            _ => {
                self.textarea.input(k);
                FormAction::None
            }
        }
    }

    fn handle_picker_key(&mut self, k: KeyEvent) -> FormAction {
        if self.fetched_models.is_empty() {
            self.picker_open = false;
            return FormAction::None;
        }
        let n = self.fetched_models.len();
        match k.code {
            KeyCode::Up => {
                self.picker_selected = (self.picker_selected + n - 1) % n;
                FormAction::None
            }
            KeyCode::Down => {
                self.picker_selected = (self.picker_selected + 1) % n;
                FormAction::None
            }
            KeyCode::Enter => {
                let m = self.fetched_models[self.picker_selected].clone();
                // 保存旧 model 参数，再装载新 model 参数
                let old_model = self.model.clone();
                if !old_model.is_empty() && old_model != m {
                    self.save_model_params(&old_model);
                }
                self.model = m.clone();
                if old_model != m {
                    self.load_model_params(&m);
                }
                self.picker_open = false;
                FormAction::None
            }
            KeyCode::Esc => {
                self.picker_open = false;
                FormAction::None
            }
            _ => FormAction::None,
        }
    }

    /// 发起拉取：bump fetch_id（防 stale）+ 置 fetching。返回 fetch_id 供 App spawn 任务。
    pub fn start_fetch(&mut self) -> u64 {
        self.fetch_id = self.fetch_id.wrapping_add(1);
        self.fetching = true;
        self.fetch_error = None;
        self.fetched_models.clear();
        self.picker_open = false;
        self.fetch_id
    }

    /// 接收拉取结果。fetch_id 不匹配（已发起新一轮或 form 重开）则丢弃。
    pub fn deliver_fetch(&mut self, fetch_id: u64, result: Result<Vec<String>, String>) {
        if fetch_id != self.fetch_id {
            return;
        }
        self.fetching = false;
        match result {
            Ok(models) => {
                if models.is_empty() {
                    self.fetch_error = Some("未返回任何模型".into());
                } else {
                    self.fetched_models = models;
                    // 将拉取到的 model 加入 known_models（去重 + 排序）
                    for m in &self.fetched_models {
                        if !self.known_models.contains(m) {
                            self.known_models.push(m.clone());
                        }
                    }
                    self.known_models.sort();
                    self.picker_selected = 0;
                    self.picker_scroll = Cell::new(0);
                    self.picker_open = true;
                }
            }
            Err(e) => {
                self.fetch_error = Some(e);
            }
        }
    }

    /// draw 前 `&mut self` 应用 textarea 样式（绕过 render `&self` 限制，同 ChatState 模式）。
    pub fn prepare_render(&mut self, theme: &Theme) {
        self.textarea.set_block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.border))
                .title(
                    Line::from(format!(" {} ", FIELDS[self.focused].label))
                        .style(Style::default().fg(theme.title)),
                ),
        );
        self.textarea
            .set_style(Style::default().fg(theme.fg).bg(theme.bg));
        self.textarea
            .set_placeholder_style(Style::default().fg(theme.muted));
    }
}

/// 渲染表单模态层（居中）。
pub fn render_form(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    let modal = centered_rect(72, 82, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .title(
            Line::from(if state.is_edit() {
                format!(" 编辑 Provider: {} ", state.original_name.as_deref().unwrap_or(""))
            } else {
                " 添加 Provider ".to_string()
            })
            .style(Style::default().fg(theme.title).add_modifier(Modifier::BOLD)),
        )
        .style(Style::default().bg(theme.bg).fg(theme.fg))
        .padding(Padding::new(2, 2, 1, 1));
    let inner = block.inner(modal);
    frame.render_widget(block, modal);

    let chunks = Layout::vertical([
        Constraint::Min(0),   // 字段列表
        Constraint::Length(3), // 编辑器 / picker / hint
        Constraint::Length(1), // 状态行
        Constraint::Length(1), // 按钮行
    ])
    .split(inner);

    render_fields(frame, chunks[0], theme, state);
    render_editor(frame, chunks[1], theme, state);
    render_status(frame, chunks[2], theme, state);
    render_buttons(frame, chunks[3], theme, state);
}

fn render_fields(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    let mut lines: Vec<Line> = Vec::new();
    let sep_width = area.width.saturating_sub(2).min(60) as usize;
    for (i, f) in FIELDS.iter().enumerate() {
        if f.kind == FieldKind::Button {
            continue; // 按钮单独渲染
        }
        // 在 per-model 区域前插入分隔线和标题
        if i == IDX_MODEL {
            lines.push(Line::from("").style(Style::default().bg(theme.bg)));
            lines.push(
                Line::from("─".repeat(sep_width))
                    .style(Style::default().fg(theme.border).bg(theme.bg)),
            );
            lines.push(
                Line::from(" 模型个性化配置").style(
                    Style::default()
                        .fg(theme.title)
                        .add_modifier(Modifier::BOLD)
                        .bg(theme.bg),
                ),
            );
        }
        // 在高级选项前插入分隔线和标题
        if i == 14 {
            lines.push(Line::from("").style(Style::default().bg(theme.bg)));
            lines.push(
                Line::from("─".repeat(sep_width))
                    .style(Style::default().fg(theme.border).bg(theme.bg)),
            );
            lines.push(
                Line::from(" 高级选项").style(
                    Style::default()
                        .fg(theme.title)
                        .add_modifier(Modifier::BOLD)
                        .bg(theme.bg),
                ),
            );
        }
        let selected = i == state.focused && !state.editing && !state.picker_open;
        let marker = if selected { "▸ " } else { "  " };
        let value: String = if i == IDX_KIND {
            format!("{}  ←→", state.kind())
        } else if i == IDX_MODEL {
            if state.model.is_empty() {
                "(空，Enter 输入或拉取模型)".to_string()
            } else {
                format!("{}  ←→", state.model)
            }
        } else if i == IDX_CURRENCY {
            format!("{}  ←→", state.currency_label())
        } else if i == 3 {
            // api_key 脱敏显示
            mask_key(&state.api_key)
        } else {
            state.get_field(i)
        };
        let editing_marker = if selected && state.editing { " [编辑中]" } else { "" };
        let row_style = if selected {
            Style::default().bg(theme.sel_bg)
        } else {
            Style::default().bg(theme.bg)
        };
        lines.push(
            Line::from(vec![
                Span::raw(marker.to_string()),
                Span::styled(f.label.to_string(), Style::default().fg(theme.fg)),
                Span::raw(" : "),
                Span::styled(
                    format!("{value}{editing_marker}"),
                    Style::default().fg(theme.title).add_modifier(Modifier::BOLD),
                ),
            ])
            .style(row_style),
        );
    }
    frame.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn render_editor(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    if state.editing {
        frame.render_widget(&state.textarea, area);
        return;
    }
    if state.picker_open && !state.fetched_models.is_empty() {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(
            Line::from(" 选择模型 (Enter 选中 / Esc 关闭)")
                .style(Style::default().fg(theme.muted)),
        );
        for (i, m) in state.fetched_models.iter().enumerate() {
            let selected = i == state.picker_selected;
            let marker = if selected { "▸ " } else { "  " };
            let style = if selected {
                Style::default().bg(theme.sel_bg).fg(theme.sel_fg)
            } else {
                Style::default().fg(theme.fg)
            };
            lines.push(Line::from(format!("{marker}{m}")).style(style));
        }
        // 粘性滚动：选中项溢出视口时自动调整（首行是标题，故 +1 偏移）
        let visible_h = area.height.saturating_sub(1) as usize; // 标题占 1 行
        let items = state.fetched_models.len();
        let prev = state.picker_scroll.get().min(items.saturating_sub(visible_h));
        let sel = state.picker_selected;
        let scroll = if items <= visible_h {
            0
        } else if sel < prev {
            sel
        } else if sel >= prev + visible_h {
            (sel + 1).saturating_sub(visible_h).min(items.saturating_sub(visible_h))
        } else {
            prev
        };
        state.picker_scroll.set(scroll);
        // 渲染时跳过标题 + scroll 行
        let visible_lines: Vec<Line> = lines
            .into_iter()
            .skip(1 + scroll) // 1 标题 + scroll 偏移
            .take(visible_h)
            .collect();
        frame.render_widget(
            Paragraph::new(visible_lines).style(Style::default().bg(theme.bg)),
            area,
        );
        return;
    }
    let hint = if state.fetching {
        " 拉取中…"
    } else {
        " Enter 编辑字段 · ←→ 切换 kind/model/currency · Esc 取消"
    };
    frame.render_widget(
        Paragraph::new(Line::from(hint)).style(Style::default().fg(theme.muted)),
        area,
    );
}

fn render_status(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    let line = if let Some(err) = &state.fetch_error {
        Line::from(format!(" ⚠ {err}")).style(Style::default().fg(theme.accent))
    } else if state.fetching {
        Line::from(" ⏳ 正在拉取模型列表…").style(Style::default().fg(theme.muted))
    } else {
        Line::from("")
    };
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme.bg)),
        area,
    );
}

fn render_buttons(frame: &mut Frame, area: Rect, theme: &Theme, state: &ProviderFormState) {
    let buttons = [(IDX_FETCH, "拉取模型"), (IDX_SAVE, "保存"), (IDX_CANCEL, "取消")];
    let mut spans: Vec<Span> = Vec::new();
    spans.push(Span::raw(" "));
    for (idx, label) in buttons {
        let active = state.focused == idx && !state.editing && !state.picker_open;
        let marker = if active { "▸[" } else { " [" };
        let close = "] ";
        let style = if active {
            Style::default().bg(theme.sel_bg).fg(theme.accent).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.fg)
        };
        spans.push(Span::styled(format!("{marker}{label}{close}"), style));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(theme.bg)),
        area,
    );
}

/// 脱敏 api_key：空 → (未设置)；`${ENV}` 原样；明文 → (已设置)。
/// 与 settings.rs 的 mask_key 同语义（复制以避免跨模块 pub 可见性扩散）。
fn mask_key(k: &str) -> String {
    if k.is_empty() {
        "(未设置)".into()
    } else if k.starts_with("${") {
        k.into()
    } else {
        "(已设置)".into()
    }
}

/// 居中算子区域（percent_x 宽，percent_y 高）。
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let pop_h = area.height.saturating_mul(percent_y) / 100;
    let pop_w = area.width.saturating_mul(percent_x) / 100;
    let y = area.y + (area.height.saturating_sub(pop_h)) / 2;
    let x = area.x + (area.width.saturating_sub(pop_w)) / 2;
    Rect::new(x, y, pop_w, pop_h)
}

// 抑制未使用导入警告（`_Cfg` 别名保留供未来扩展）。
#[allow(unused_imports)]
use _Cfg as _ProviderCfgAlias;

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn empty_defaults() {
        let s = ProviderFormState::empty();
        assert!(!s.is_edit());
        assert_eq!(s.kind(), "openai");
        assert_eq!(s.max_tokens, "4096");
        assert_eq!(s.temperature, "0.7");
        assert_eq!(s.focused, 0);
    }

    #[test]
    fn from_provider_loads_fields_and_is_edit() {
        let cfg = ProviderConfig {
            kind: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            api_key: "${ANTHROPIC_API_KEY}".into(),
            model: "claude-sonnet-4-5".into(),
            max_tokens: 8192,
            temperature: 0.3,
            ..Default::default()
        };
        let s = ProviderFormState::from_provider("anthropic", &cfg);
        assert!(s.is_edit());
        assert_eq!(s.original_name.as_deref(), Some("anthropic"));
        assert_eq!(s.kind(), "anthropic");
        assert_eq!(s.base_url, "https://api.anthropic.com");
        assert_eq!(s.max_tokens, "8192");
    }

    #[test]
    fn kind_cycle_via_left_right() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_KIND;
        s.handle_key(key(KeyCode::Right), &ProvidersConfig::default());
        assert_eq!(s.kind(), "anthropic");
        s.handle_key(key(KeyCode::Left), &ProvidersConfig::default());
        assert_eq!(s.kind(), "openai");
        // 回绕
        s.handle_key(key(KeyCode::Left), &ProvidersConfig::default());
        assert_eq!(s.kind(), "responses");
    }

    #[test]
    fn into_provider_validates_empty_name() {
        let s = ProviderFormState::empty();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("名称"));
    }

    #[test]
    fn into_provider_validates_empty_base_url() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("base_url"));
    }

    #[test]
    fn into_provider_rejects_duplicate_name_on_add() {
        let mut s = ProviderFormState::empty();
        s.name = "openai".into();
        s.base_url = "https://x".into();
        let existing = ProvidersConfig::default_template();
        let err = s.into_provider(&existing).unwrap_err();
        assert!(err.contains("已存在"));
    }

    #[test]
    fn into_provider_allows_keep_name_on_edit() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            ..Default::default()
        };
        let s = ProviderFormState::from_provider("openai", &cfg);
        // 同名编辑应通过
        let (name, _) = s.into_provider(&ProvidersConfig::default_template()).unwrap();
        assert_eq!(name, "openai");
    }

    #[test]
    fn into_provider_rejects_bad_number() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.max_tokens = "abc".into();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("max_tokens"));
    }

    #[test]
    fn enter_text_field_starts_editing_enter_commits() {
        let mut s = ProviderFormState::empty();
        s.focused = 0; // name
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::None);
        assert!(s.editing);
        // 输入字符
        s.handle_key(key(KeyCode::Char('z')), &ProvidersConfig::default());
        s.handle_key(key(KeyCode::Char('z')), &ProvidersConfig::default());
        // Enter 提交
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default());
        assert!(!s.editing);
        assert_eq!(s.name, "zz");
    }

    #[test]
    fn editing_esc_discards() {
        let mut s = ProviderFormState::empty();
        s.focused = 2; // base_url
        s.base_url = "old".into();
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()); // 进入编辑，load "old"
        s.handle_key(key(KeyCode::Char('X')), &ProvidersConfig::default());
        s.handle_key(key(KeyCode::Esc), &ProvidersConfig::default()); // 丢弃
        assert!(!s.editing);
        assert_eq!(s.base_url, "old", "Esc 应丢弃编辑");
    }

    #[test]
    fn save_button_returns_save_action() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_SAVE;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::Save);
    }

    #[test]
    fn cancel_button_and_esc_return_cancel() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_CANCEL;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::Cancel);
        assert_eq!(s.handle_key(key(KeyCode::Esc), &ProvidersConfig::default()), FormAction::Cancel);
    }

    #[test]
    fn fetch_button_returns_fetch_action() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_FETCH;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::Fetch);
    }

    #[test]
    fn fetch_button_noop_while_fetching() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_FETCH;
        s.fetching = true;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::None);
    }

    #[test]
    fn start_fetch_bumps_id_and_sets_fetching() {
        let mut s = ProviderFormState::empty();
        let id1 = s.start_fetch();
        assert!(s.fetching);
        let id2 = s.start_fetch();
        assert_ne!(id1, id2);
    }

    #[test]
    fn deliver_fetch_stale_id_ignored() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id.wrapping_sub(1), Ok(vec!["m".into()]));
        assert!(s.fetching, "stale 结果应被忽略");
        assert!(s.fetched_models.is_empty());
    }

    #[test]
    fn deliver_fetch_success_opens_picker() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id, Ok(vec!["gpt-4o".into(), "gpt-4o-mini".into()]));
        assert!(!s.fetching);
        assert_eq!(s.fetched_models.len(), 2);
        assert!(s.picker_open);
        assert_eq!(s.picker_selected, 0);
    }

    #[test]
    fn deliver_fetch_error_stores_message() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id, Err("timeout".into()));
        assert!(!s.fetching);
        assert_eq!(s.fetch_error.as_deref(), Some("timeout"));
        assert!(!s.picker_open);
    }

    #[test]
    fn picker_enter_selects_model() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id, Ok(vec!["a".into(), "b".into()]));
        assert!(s.picker_open);
        s.handle_key(key(KeyCode::Down), &ProvidersConfig::default()); // → b
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()); // 选中 b
        assert!(!s.picker_open);
        assert_eq!(s.model, "b");
    }

    #[test]
    fn to_provider_config_snapshot_parses_numbers() {
        let mut s = ProviderFormState::empty();
        s.base_url = "https://x".into();
        s.max_tokens = "2048".into();
        s.temperature = "0.1".into();
        let cfg = s.to_provider_config_snapshot();
        assert_eq!(cfg.max_tokens, 2048);
        assert!((cfg.temperature - 0.1).abs() < 1e-6);
    }

    #[test]
    fn render_form_does_not_panic() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut s = ProviderFormState::from_provider(
            "openai",
            &ProviderConfig {
                kind: "openai".into(),
                base_url: "https://api.openai.com/v1".into(),
                api_key: "${OPENAI_API_KEY}".into(),
                model: "gpt-4o".into(),
                ..Default::default()
            },
        );
        s.prepare_render(&crate::theme::Theme::resolve("cyberpunk"));
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_form(f, f.area(), &crate::theme::Theme::resolve("cyberpunk"), &s))
            .unwrap();
    }

    #[test]
    fn from_provider_loads_price_fields() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            price: Some(cyber_core::PriceConfig {
                input_per_m: Some(2.5),
                output_per_m: Some(10.0),
                cache_hit_per_m: Some(0.3),
                ..Default::default()
            }),
            ..Default::default()
        };
        let s = ProviderFormState::from_provider("foo", &cfg);
        assert_eq!(s.price_input, "2.5");
        assert_eq!(s.price_output, "10");
        assert!((s.price_cache_hit.parse::<f64>().unwrap() - 0.3).abs() < 1e-6);
    }

    #[test]
    fn from_provider_price_none_yields_empty_strings() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            ..Default::default()
        };
        let s = ProviderFormState::from_provider("foo", &cfg);
        assert!(s.price_input.is_empty());
        assert!(s.price_output.is_empty());
        assert!(s.price_cache_hit.is_empty());
    }

    #[test]
    fn build_price_none_when_all_empty() {
        let s = ProviderFormState::empty();
        assert!(s.build_price().is_none());
    }

    #[test]
    fn build_price_partial_fields() {
        let mut s = ProviderFormState::empty();
        s.price_output = "7.5".into();
        let p = s.build_price().expect("部分字段也应返回 Some");
        assert!(p.input_per_m.is_none());
        assert!((p.output_per_m.unwrap() - 7.5).abs() < 1e-9);
        assert!(p.cache_hit_per_m.is_none());
    }

    #[test]
    fn into_provider_accepts_valid_prices() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.price_input = "2.5".into();
        s.price_output = "10".into();
        s.price_cache_hit = "0.3".into();
        let (_, cfg) = s.into_provider(&ProvidersConfig::default()).unwrap();
        let p = cfg.price.expect("已配置价格应存在");
        assert!((p.input_per_m.unwrap() - 2.5).abs() < 1e-9);
        assert!((p.output_per_m.unwrap() - 10.0).abs() < 1e-9);
        assert!((p.cache_hit_per_m.unwrap() - 0.3).abs() < 1e-9);
    }

    #[test]
    fn into_provider_rejects_bad_price_input() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.price_input = "abc".into();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("input_per_m"), "err = {err}");
    }

    #[test]
    fn into_provider_rejects_bad_price_output() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.price_output = "x".into();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("output_per_m"), "err = {err}");
    }

    #[test]
    fn into_provider_rejects_bad_price_cache_hit() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.price_cache_hit = "?".into();
        let err = s.into_provider(&ProvidersConfig::default()).unwrap_err();
        assert!(err.contains("cache_hit_per_m"), "err = {err}");
    }

    #[test]
    fn into_provider_accepts_empty_prices() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        // 全部价格留空应通过且 price = None
        let (_, cfg) = s.into_provider(&ProvidersConfig::default()).unwrap();
        assert!(cfg.price.is_none());
    }

    #[test]
    fn price_fields_are_text_editable() {
        // 新索引：9=input, 10=output, 11=cache_hit
        assert!(ProviderFormState::is_text_field(9));
        assert!(ProviderFormState::is_text_field(10));
        assert!(ProviderFormState::is_text_field(11));
    }

    #[test]
    fn price_fields_get_set_roundtrip() {
        let mut s = ProviderFormState::empty();
        s.set_field(9, "1.1".into());
        s.set_field(10, "2.2".into());
        s.set_field(11, "3.3".into());
        assert_eq!(s.get_field(9), "1.1");
        assert_eq!(s.get_field(10), "2.2");
        assert_eq!(s.get_field(11), "3.3");
    }

    // ── per-model 参数测试 ──

    #[test]
    fn from_provider_loads_per_model_params() {
        let mut cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            model: "gpt-4o".into(),
            max_tokens: 4096,
            temperature: 0.7,
            ..Default::default()
        };
        cfg.models.insert(
            "gpt-4o".into(),
            ModelConfig {
                alias: Some("我的GPT".into()),
                context_length: Some(128000),
                max_tokens: Some(8192),
                temperature: Some(0.3),
                notes: Some("备注".into()),
                ..Default::default()
            },
        );
        let s = ProviderFormState::from_provider("openai", &cfg);
        assert_eq!(s.alias, "我的GPT");
        assert_eq!(s.context_length, "128000");
        assert_eq!(s.max_tokens, "8192");
        assert!((s.temperature.parse::<f32>().unwrap() - 0.3).abs() < 1e-6);
        assert_eq!(s.notes, "备注");
    }

    #[test]
    fn from_provider_falls_back_to_provider_level() {
        // model 不在 models map → 回退到 provider 级
        let cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            model: "gpt-4o".into(),
            max_tokens: 4096,
            temperature: 0.7,
            ..Default::default()
        };
        let s = ProviderFormState::from_provider("openai", &cfg);
        assert_eq!(s.max_tokens, "4096");
        assert!((s.temperature.parse::<f32>().unwrap() - 0.7).abs() < 1e-6);
        assert!(s.alias.is_empty());
        assert!(s.context_length.is_empty());
        assert!(s.notes.is_empty());
    }

    #[test]
    fn into_provider_writes_per_model_config() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.model = "gpt-4o".into();
        s.alias = "我的GPT".into();
        s.context_length = "128000".into();
        s.max_tokens = "8192".into();
        s.temperature = "0.3".into();
        s.notes = "备注".into();
        let (_, cfg) = s.into_provider(&ProvidersConfig::default()).unwrap();
        let mc = cfg.models.get("gpt-4o").expect("应有 per-model 配置");
        assert_eq!(mc.alias, Some("我的GPT".into()));
        assert_eq!(mc.context_length, Some(128000));
        assert_eq!(mc.max_tokens, Some(8192));
        assert!((mc.temperature.unwrap() - 0.3).abs() < 1e-6);
        assert_eq!(mc.notes, Some("备注".into()));
    }

    #[test]
    fn into_provider_empty_model_skips_models_insert() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.model = "".into();
        let (_, cfg) = s.into_provider(&ProvidersConfig::default()).unwrap();
        assert!(cfg.models.is_empty());
    }

    #[test]
    fn into_provider_preserves_existing_models() {
        // 编辑时已有其他 model 的配置，保存当前 model 不应丢失其他
        let mut existing = std::collections::HashMap::new();
        existing.insert(
            "gpt-4o-mini".into(),
            ModelConfig {
                alias: Some("Mini".into()),
                ..Default::default()
            },
        );
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.model = "gpt-4o".into();
        s.alias = "GPT4o".into();
        s.models = existing;
        let (_, cfg) = s.into_provider(&ProvidersConfig::default()).unwrap();
        // 两个 model 都应在
        assert!(cfg.models.contains_key("gpt-4o"));
        assert!(cfg.models.contains_key("gpt-4o-mini"));
        assert_eq!(cfg.models["gpt-4o-mini"].alias, Some("Mini".into()));
        assert_eq!(cfg.models["gpt-4o"].alias, Some("GPT4o".into()));
    }

    #[test]
    fn model_field_commit_syncs_params() {
        // 编辑 model 字段并提交 → 应保存旧 model 参数，装载新 model 参数
        let mut s = ProviderFormState::empty();
        s.model = "gpt-4o".into();
        s.alias = "OldAlias".into();
        s.max_tokens = "8192".into();
        // 预设 gpt-4o-mini 的 per-model 配置
        s.models.insert(
            "gpt-4o-mini".into(),
            ModelConfig {
                alias: Some("Mini".into()),
                max_tokens: Some(2048),
                ..Default::default()
            },
        );
        // 进入编辑 model 字段
        s.focused = IDX_MODEL;
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default());
        assert!(s.editing);
        // 清空 textarea（"gpt-4o" 有 6 个字符）
        for _ in 0..6 {
            s.handle_key(key(KeyCode::Backspace), &ProvidersConfig::default());
        }
        // 逐字符输入 "gpt-4o-mini"
        for ch in "gpt-4o-mini".chars() {
            s.handle_key(key(KeyCode::Char(ch)), &ProvidersConfig::default());
        }
        // 提交
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default());
        assert!(!s.editing);
        // model 已切换
        assert_eq!(s.model, "gpt-4o-mini");
        // 旧 model 参数已保存到 models map
        assert_eq!(s.models["gpt-4o"].alias, Some("OldAlias".into()));
        assert_eq!(s.models["gpt-4o"].max_tokens, Some(8192));
        // 新 model 参数已装载
        assert_eq!(s.alias, "Mini");
        assert_eq!(s.max_tokens, "2048");
    }

    #[test]
    fn alias_and_notes_are_text_editable() {
        assert!(ProviderFormState::is_text_field(5));  // alias
        assert!(ProviderFormState::is_text_field(6));  // context_length
        assert!(!ProviderFormState::is_text_field(12)); // currency (Enum)
        assert!(ProviderFormState::is_text_field(13)); // notes
    }

    #[test]
    fn alias_context_length_notes_get_set_roundtrip() {
        let mut s = ProviderFormState::empty();
        s.set_field(5, "别名".into());
        s.set_field(6, "96000".into());
        s.set_field(13, "备注内容".into());
        assert_eq!(s.get_field(5), "别名");
        assert_eq!(s.get_field(6), "96000");
        assert_eq!(s.get_field(13), "备注内容");
    }

    #[test]
    fn empty_alias_becomes_none_in_models_map() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.model = "gpt-4o".into();
        s.alias = "   ".into(); // 空白
        s.notes = "".into();
        let (_, cfg) = s.into_provider(&ProvidersConfig::default()).unwrap();
        let mc = &cfg.models["gpt-4o"];
        assert!(mc.alias.is_none(), "空白 alias 应为 None");
        assert!(mc.notes.is_none(), "空 notes 应为 None");
    }

    #[test]
    fn to_provider_config_snapshot_includes_models() {
        let mut s = ProviderFormState::empty();
        s.model = "gpt-4o".into();
        s.models.insert(
            "gpt-4o-mini".into(),
            ModelConfig {
                alias: Some("Mini".into()),
                ..Default::default()
            },
        );
        let snap = s.to_provider_config_snapshot();
        assert!(snap.models.contains_key("gpt-4o-mini"));
    }

    // ── ←→ model 切换测试 ──

    #[test]
    fn from_provider_initializes_known_models() {
        let mut cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            model: "gpt-4o".into(),
            ..Default::default()
        };
        cfg.models.insert("gpt-4o-mini".into(), ModelConfig::default());
        let s = ProviderFormState::from_provider("openai", &cfg);
        assert!(s.known_models.contains(&"gpt-4o".to_string()));
        assert!(s.known_models.contains(&"gpt-4o-mini".to_string()));
    }

    #[test]
    fn deliver_fetch_adds_to_known_models() {
        let mut s = ProviderFormState::empty();
        let id = s.start_fetch();
        s.deliver_fetch(id, Ok(vec!["m1".into(), "m2".into()]));
        assert!(s.known_models.contains(&"m1".to_string()));
        assert!(s.known_models.contains(&"m2".to_string()));
    }

    #[test]
    fn switch_model_cycles_and_syncs_params() {
        let mut s = ProviderFormState::empty();
        s.model = "model-a".into();
        s.alias = "AliasA".into();
        s.max_tokens = "1000".into();
        s.models.insert(
            "model-b".into(),
            ModelConfig {
                alias: Some("AliasB".into()),
                max_tokens: Some(2000),
                ..Default::default()
            },
        );
        s.known_models = vec!["model-a".into(), "model-b".into()];
        s.focused = IDX_MODEL;

        // → 切换到 model-b
        s.handle_key(key(KeyCode::Right), &ProvidersConfig::default());
        assert_eq!(s.model, "model-b");
        assert_eq!(s.alias, "AliasB");
        assert_eq!(s.max_tokens, "2000");
        // 旧 model-a 参数已保存到 map
        assert_eq!(s.models["model-a"].alias, Some("AliasA".into()));
        assert_eq!(s.models["model-a"].max_tokens, Some(1000));

        // ← 切换回 model-a
        s.handle_key(key(KeyCode::Left), &ProvidersConfig::default());
        assert_eq!(s.model, "model-a");
        assert_eq!(s.alias, "AliasA");
        assert_eq!(s.max_tokens, "1000");
    }

    #[test]
    fn switch_model_noop_with_single_model() {
        let mut s = ProviderFormState::empty();
        s.model = "only".into();
        s.known_models = vec!["only".into()];
        s.focused = IDX_MODEL;
        s.handle_key(key(KeyCode::Right), &ProvidersConfig::default());
        assert_eq!(s.model, "only", "仅 1 个 model 时 ←→ 应无操作");
    }

    #[test]
    fn switch_model_noop_with_empty_known_models() {
        let mut s = ProviderFormState::empty();
        s.model = "x".into();
        s.focused = IDX_MODEL;
        s.handle_key(key(KeyCode::Right), &ProvidersConfig::default());
        assert_eq!(s.model, "x", "known_models 为空时 ←→ 应无操作");
    }

    #[test]
    fn manual_model_entry_adds_to_known_models() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_MODEL;
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default());
        for ch in "new-model".chars() {
            s.handle_key(key(KeyCode::Char(ch)), &ProvidersConfig::default());
        }
        s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default());
        assert!(!s.editing);
        assert_eq!(s.model, "new-model");
        assert!(s.known_models.contains(&"new-model".to_string()));
    }

    // ── 价格货币 (currency) 测试 ──

    #[test]
    fn currency_defaults_to_usd() {
        let s = ProviderFormState::empty();
        assert_eq!(s.currency(), "usd");
        assert_eq!(s.currency_label(), "美元");
    }

    #[test]
    fn currency_cycles_via_left_right() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_CURRENCY;
        // → 切换到 cny
        s.handle_key(key(KeyCode::Right), &ProvidersConfig::default());
        assert_eq!(s.currency(), "cny");
        assert_eq!(s.currency_label(), "人民币");
        // → 回绕到 usd
        s.handle_key(key(KeyCode::Right), &ProvidersConfig::default());
        assert_eq!(s.currency(), "usd");
        // ← 回绕到 cny
        s.handle_key(key(KeyCode::Left), &ProvidersConfig::default());
        assert_eq!(s.currency(), "cny");
    }

    #[test]
    fn from_provider_loads_currency_cny() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            price: Some(cyber_core::PriceConfig {
                input_per_m: Some(2.5),
                currency: Some("cny".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let s = ProviderFormState::from_provider("foo", &cfg);
        assert_eq!(s.currency(), "cny");
        assert_eq!(s.currency_label(), "人民币");
    }

    #[test]
    fn from_provider_currency_none_defaults_usd() {
        let cfg = ProviderConfig {
            kind: "openai".into(),
            base_url: "https://x".into(),
            price: Some(cyber_core::PriceConfig {
                input_per_m: Some(2.5),
                ..Default::default()
            }),
            ..Default::default()
        };
        let s = ProviderFormState::from_provider("foo", &cfg);
        assert_eq!(s.currency(), "usd");
    }

    #[test]
    fn build_price_includes_currency() {
        let mut s = ProviderFormState::empty();
        s.price_input = "2.5".into();
        s.currency_idx = 1; // cny
        let p = s.build_price().expect("应有 price");
        assert_eq!(p.currency, Some("cny".into()));
    }

    #[test]
    fn into_provider_writes_currency() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        s.price_input = "2.5".into();
        s.currency_idx = 1; // cny
        let (_, cfg) = s.into_provider(&ProvidersConfig::default()).unwrap();
        let p = cfg.price.expect("应有 price");
        assert_eq!(p.currency, Some("cny".into()));
    }

    #[test]
    fn into_provider_no_price_no_currency() {
        let mut s = ProviderFormState::empty();
        s.name = "foo".into();
        s.base_url = "https://x".into();
        // 全部价格留空 → price = None，currency 也不保存
        let (_, cfg) = s.into_provider(&ProvidersConfig::default()).unwrap();
        assert!(cfg.price.is_none());
    }

    #[test]
    fn currency_enter_is_noop() {
        let mut s = ProviderFormState::empty();
        s.focused = IDX_CURRENCY;
        assert_eq!(s.handle_key(key(KeyCode::Enter), &ProvidersConfig::default()), FormAction::None);
        assert!(!s.editing, "currency 字段 Enter 不应进入编辑模式");
    }
}
