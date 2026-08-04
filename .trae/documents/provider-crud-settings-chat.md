# Provider CRUD（Settings + Chat）实施计划

## Context（为什么做）

当前 cyber_master 的服务商（LLM Provider）只能在 `~/.cyber/providers.toml` 手工编辑：TUI 设置页 Providers 段是**只读**（`views/settings.rs:334 editable:false`），Chat 侧 `/model <name>` 仅能在已存在 provider 间切换（`app.rs:427-454`），无法新增/编辑/删除。用户希望参考 `example/wepclaude/dist/src/utils/model/customProviders.js` 的逻辑，在**设置页**与**对话界面**都能管理服务商。

参考实现的核心：provider = `{name, kind(format), baseUrl, apiKey, model}`，支持 upsert / remove / activate，并能 `GET {base}/models` 拉取模型列表。

用户已拍板两档范围：**完整 CRUD**（新增/编辑/删除/设默认）+ **model 字段手动输入 + 「拉取模型」按钮**（异步 GET /models）。

## 关键设计决策

1. **不改 ProviderConfig schema**：保留单 `model: String`（不引入 `models[]`），避免牵动 `agent.rs`/providers.toml 格式。拉取按钮的结果用 model picker 选中后回填 model 字段。
2. **ProviderForm 为顶层 `Mode::ProviderForm`**：可从 Settings（`a`/`e`）和 Chat（`/provider add|edit`）两路进入，复用同一表单。
3. **持久化双轨**：
   - Settings 入口：form Save 只改内存 `self.providers` + 标 `dirty_providers`，**不立即写盘**；统一在 Settings 的「保存设置」行（`save_settings`）一并 `save_config` + `save_providers`。Esc-双击回滚用新增的 `providers_at_entry` 快照。
   - Chat 入口（slash）：form Save 立即 `save_providers` 写盘（Chat 无 Settings 的保存触发点）。
4. **fetch 异步通道**：新增 `UnboundedSender<FetchResult>` 作 `main_loop` 第 4 路 `select!` 分支（与 `agent_rx` 同构）。`FetchResult { fetch_id, result }`，form 每次 fetch bump `fetch_id`，接收时校验防 stale；form 已关则丢弃。
5. **AppPaths 打包**：把 `config_file`/`providers_file`/`history_dir`/`cwd` 4 个 path 参数合并为 `AppPaths`，净参 11→10，避免 `too_many_arguments` 恶化。
6. **删除确认**：双击 `d`（与 Esc-双击同构），`pending_delete_idx: Option<usize>`，任一其他键清除。
7. **default_provider 防悬空**：删除/重命名时若触及 `default_provider`，自动回退到排序后首个剩余 / 同步改名，并 toast。
8. **保留 `/model <name>`** 作 `/provider use` 的 alias（向后兼容）。

## 实施步骤

### Step 1 — cyber-core 后端

**`crates/cyber-core/src/providers.rs`**
- 新增 `pub const PROVIDER_KINDS: &[&str] = &["openai","anthropic","ollama","openai-compatible"];`
- `impl ProviderConfig { pub fn normalize(&mut self) }`：trim + 去尾 `/`（参考 JS `normalizeBaseUrl`）。
- `impl ProvidersConfig { pub fn sorted_names(&self) -> Vec<String>; pub fn upsert(&mut self, name:&str, cfg:ProviderConfig); pub fn remove(&mut self, name:&str) -> Option<ProviderConfig> }`
- 单测：KINDS 长度、normalize、upsert 覆盖、remove、sorted_names 顺序。

**`crates/cyber-core/src/loader.rs`**
- 新增 `pub fn save_providers(providers:&ProvidersConfig, path:&Path) -> Result<()>`（镜像 `save_config:103-111`，复用私有 `atomic_write`，含 `.bak`）。
- `lib.rs` re-export `save_providers`。
- 单测：`save_providers_roundtrip` + `save_providers_creates_bak`（参考 `save_config_roundtrip:208`）。

### Step 2 — cyber-agent fetch

**`crates/cyber-agent/src/models.rs`（新文件）**
- `pub async fn fetch_models(cfg:&ProviderConfig) -> Result<Vec<String>>`：`reqwest::Client::builder().timeout(15s)`，按 kind 试 `{base}/models` 与 `{base}/v1/models`（anthropic 先 v1，其余先 /models，含 ollama fallback），headers 按 kind（anthropic→`x-api-key`+`anthropic-version`；openai/compatible→`Authorization: Bearer`；ollama→无 auth，调 `cyber_core::resolve_api_key`）。
- `pub fn extract_model_ids(payload:&serde_json::Value) -> Vec<String>`：端口 JS `extractModelIds`（处理 `data[]`/`models[]`/`id`/字符串数组，去重 trim）。
- `lib.rs` 加 `pub mod models;` + re-export。
- 单测：`extract_model_ids` 各 payload 形态 + `fetch_endpoints` 顺序（离线，不打真实 HTTP）。

### Step 3 — cyber-tui 视图层

**`crates/cyber-tui/src/views/providers.rs`（新文件）**
- `pub struct ProviderFormState`：字段 `name, kind_idx(usize 索 PROVIDER_KINDS), base_url, api_key, model, max_tokens, temperature, original_name(Option=Edit), focused(FormField 枚举), editing(bool), textarea(TextArea), fetching, fetch_id(u64), fetch_error(Option<String>), fetched_models(Vec), picker_open, picker_selected`。
- 方法：`empty()`(Add) / `from_provider(name,&cfg)`(Edit，反查 kind_idx) / `into_provider(&existing)->Result<(String,ProviderConfig),String>`(校验 name 非空+unique-or-self+base_url 非空+parse 数字) / `kind()` / `handle_key(k,&providers)->FormAction{None,Save,Cancel,Fetch,Toast}` / `start_fetch()->u64` / `deliver_fetch(id,result)`(stale 守卫) / `prepare_render(&mut self,theme)`(editing 时 apply textarea 样式)。
- 文本字段：Enter 进 editing（textarea load 值）→ 输入 → Enter 提交 / Esc 取消编辑；`kind` 用 ←→ 循环；按钮行 Enter 触发。
- `pub fn render_form(frame, area, theme, state)`：居中模态（`Constraint::Percentage` 算 Rect），列字段+值+按钮行，editing 时显 textarea，picker_open 时显模型列表浮层。
- `views/mod.rs` 加 `pub mod providers;`。
- 单测：empty/from_provider/into_provider 校验/kind 循环/deliver_fetch stale/render 不 panic。

**`crates/cyber-tui/src/views/settings.rs`**
- `SettingsState` 扩展：`dirty_providers:bool, provider_selected:usize, pending_delete_idx:Option<usize>`。
- Providers 段保持 `editable:false` 但 App 侧特殊分派（不走 `apply_edit`），新增 `on_providers_section()` / `next_provider(len)` / `prev_provider(len)`(clamp)。
- `render_providers_lines:631` 改交互版：每行加 `▸ ` cursor（provider_selected 处），底部 hint `a 新增 e 编辑 d 删除 Enter 设默认`。
- 单测：pending_delete 状态机、next/prev_provider 边界、render 不 panic。

**`crates/cyber-tui/src/event.rs`**
- `Action` 加 `AddProvider`(`a`) / `EditProvider`(`e`) / `DeleteProvider`(`d`)；`key_to_action` 映射。
- 非 Settings 模式下这三个 Action no-op（Welcome/Workflow/Dashboard 占位）；Chat 走 `ChatAction` 不受影响。
- 单测：a/e/d 映射。

**`crates/cyber-tui/src/slash.rs`**
- `SlashCommand` 加 `Provider(String)`（原始 args）；`parse` 加 `"/provider"` 分支；`HELP_TEXT` 追加 `/provider` 行。
- 单测：`/provider` + 5 子命令 + 大小写 + HELP_TEXT 含。

### Step 4 — cyber-tui app 集成

**`crates/cyber-tui/src/app.rs`**（最大改动）
- `Mode` 加 `ProviderForm`。
- 新增 `pub struct AppPaths { config_file, providers_file, history_dir, cwd }`（放 app.rs）。
- `App` 加字段：`provider_form:Option<ProviderFormState>, providers_at_entry:ProvidersConfig, paths:AppPaths, fetch_tx:UnboundedSender<FetchResult>`。
- `App::new` 签名改：用 `AppPaths` 替换 3 path 参数 + 加 `fetch_tx`。更新 6 处 call site（`main.rs:88` + `app.rs` 的 `make_app` helper 与 4 处直接调用）。
- `run()`/`main_loop()` 加 `fetch_rx:&mut UnboundedReceiver<FetchResult>`，`select!` 加第 4 路 `fetch_rx.recv()` → `handle_fetch_result`。
- 三处进 Settings 入口（`OpenSettings`×2 + Welcome 第 4 项）补 `providers_at_entry = self.providers.clone()`；`exit_settings` 二次 Esc 补 `self.providers = self.providers_at_entry.clone()`；`save_settings` 补 `save_providers` + 重置 `providers_at_entry` + `dirty_providers=false`。
- `handle_action`（Settings）：Providers 段分派 `AddProvider/EditProvider/DeleteProvider(双击d)/Enter(设默认)/Up/Down`。
- 新增 `handle_provider_form_key`：委托 `form.handle_key`，按 `FormAction` 分派（Save 按 `prev_mode` 双轨持久化；Cancel 回 prev_mode；Fetch spawn `fetch_models` 任务发 `fetch_tx`；Toast 弹提示）。
- `handle_fetch_result`：`mode!=ProviderForm` 丢弃；否则 `deliver_fetch`。
- `render_main` 加 `Mode::ProviderForm => views::providers::render_form`；`render_status_bar` 加 form hint；`style_chat_input` 在 `ProviderForm` 模式改调 `provider_form.prepare_render`。
- `handle_slash_command` 加 `SlashCommand::Provider(args)`：解析子命令 `list`(空)/`add`/`use <name>`/`remove <name>`/`edit <name>`，流式期阻止（与 /model 一致）。
- 重命名同步（R5）：form Save 时若 `original_name==default_provider && original_name!=new_name`，同步改 `default_provider`。
- 新增测试（~20）：add/edit 打开 form、双击 d 删除、default 回退、Enter 设默认、name 冲突校验、Settings 延迟 vs Chat 立即持久化、fetch deliver/drop、slash 5 子命令、Esc 回滚 providers、render 不 panic。

### Step 5 — cyber-app + docs

**`crates/cyber-app/src/main.rs`**
- `mpsc::unbounded_channel::<FetchResult>()`；构造 `AppPaths`；`App::new` 新签名传 `AppPaths`+`fetch_tx`；`.run(agent_rx, fetch_rx)`。

**`docs/DESIGN.md`**：§9.4 Providers 段改「可交互」；新增 §9.5 Provider Form 模态层（字段/键位/fetch/picker/持久化双轨）；§3.2 加 provider 管理；§9.2 键位表加 `a/e/d`。
**`docs/PROGRESS.md`**：新增 §2.14 Provider CRUD + fetch models 勾选清单 + 变更日志；总览表 P2 行说明追加。

## 验证

- `cargo test --workspace`：预期 183 → ~230 全过（+~45 新测）。
- `cargo clippy --workspace --all-targets -- -D warnings` 干净。
- 手测：`cargo run -- --mock` 进 Settings→Providers 段，`a` 新增 provider（填 openai-compatible + 本地 base），「拉取模型」按钮验证异步 fetch + picker 回填；`e` 编辑、`d` 双击删除、`Enter` 设默认、Esc-双击回滚；Chat 内 `/provider add|edit|remove|use|list` 全链路；真实 OpenAI/Anthropic/ollama fetch（带 key）验证端点与 headers。
- Esc-rollback 全链路：Settings 内增改 provider 不保存 → Esc 双击 → `providers.toml` 未变。
- 跨模式一致性：Chat `/provider add` 后切 Settings，列表即时刷新。
