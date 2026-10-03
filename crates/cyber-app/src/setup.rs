//! Terminal-only configuration setup. Never builds registries or starts MCP.

use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::Path;

use color_eyre::eyre::{bail, eyre, WrapErr};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal;
use futures::StreamExt;
use toml::Value;

use cyber_core::{
    load_custom_tools, save_custom_tool, Config, CustomToolConfig, CustomToolParam, Paths,
    ProviderConfig, ProvidersConfig, PROVIDER_KINDS, PROVIDER_PRESETS,
};
use cyber_mcp::{McpConnection, McpServerSpec, McpServersConfig, McpTransport, MCP_PRESETS};

const STATE_FILE: &str = "setup.toml";

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetupState {
    NotStarted,
    InProgress,
    Completed,
}

fn setup_state(paths: &Paths) -> Result<SetupState> {
    let path = paths.cyber_home.join(STATE_FILE);
    if !path.try_exists()? {
        return Ok(SetupState::NotStarted);
    }
    let state = read_value(&path)?;
    if state.get("in_progress").and_then(Value::as_bool) == Some(true) {
        Ok(SetupState::InProgress)
    } else if state.get("completed").and_then(Value::as_bool) == Some(true) {
        Ok(SetupState::Completed)
    } else {
        Ok(SetupState::NotStarted)
    }
}

/// Gate real-provider startup; mock mode performs no configuration IO.
pub fn ensure_configured(cwd: &Path, mock: bool, interactive: bool) -> Result<()> {
    if mock {
        return Ok(());
    }
    let paths = Paths::detect()?;
    ensure_with_paths(&paths, cwd, interactive, run_setup_sync)
}

/// Validate the provider actually selected by a script, not the global default.
pub fn ensure_run_configured(
    cwd: &Path,
    mock: bool,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    if mock {
        return Ok(());
    }
    let paths = Paths::detect()?;
    ensure_run_with_paths(&paths, cwd, provider, model)
}

fn ensure_run_with_paths(
    paths: &Paths,
    cwd: &Path,
    provider: Option<&str>,
    model: Option<&str>,
) -> Result<()> {
    cyber_core::init::ensure_global_init(paths)?;
    if setup_state(paths)? == SetupState::InProgress {
        bail!("Setup was interrupted during saving. Run `cyber setup` to complete it before running a task.");
    }
    let (mut config, mut providers) = effective_config(paths, cwd)?;
    apply_run_overrides(&mut config, &mut providers, provider, model);
    if !configured(&config, &providers) {
        bail!("Selected provider configuration is missing or unusable. Run `cyber setup`; check --provider, --model and the credential environment variable.");
    }
    Ok(())
}

fn apply_run_overrides(
    config: &mut Config,
    providers: &mut ProvidersConfig,
    provider: Option<&str>,
    model: Option<&str>,
) {
    if let Some(provider) = provider {
        config.agent.default_provider = provider.into();
    }
    if let (Some(model), Some(provider)) = (
        model,
        providers.providers.get_mut(&config.agent.default_provider),
    ) {
        provider.model = model.into();
    }
}

fn ensure_with_paths(
    paths: &Paths,
    cwd: &Path,
    interactive: bool,
    setup: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    cyber_core::init::ensure_global_init(paths)?;
    let mut state = setup_state(paths)?;
    // An interrupted multi-file commit takes priority over apparently usable settings.
    if state != SetupState::InProgress {
        let (config, providers) = effective_config(paths, cwd)?;
        if configured(&config, &providers) {
            return Ok(());
        }
    }
    if interactive && state != SetupState::Completed {
        if state == SetupState::InProgress {
            eprintln!(
                "Previous setup was interrupted during saving; restarting setup to complete it."
            );
        }
        setup(cwd)?;
        state = setup_state(paths)?;
        let (config, providers) = effective_config(paths, cwd)?;
        if state == SetupState::Completed && configured(&config, &providers) {
            return Ok(());
        }
    }
    if state == SetupState::InProgress {
        bail!(
            "Setup was interrupted during saving. Run `cyber setup` to complete it before startup."
        );
    }
    bail!("Provider configuration is missing or unusable. Run `cyber setup`; check the selected provider, endpoint, model and credential environment variable (including project overrides).");
}

/// 运行全面的交互式配置向导（支持 Provider、MCP、Custom Tools 与 AI 自动扫描）。
pub async fn run_setup(cwd: &Path) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        bail!("`cyber setup` requires an interactive terminal (stdin and stderr).");
    }
    let paths = Paths::detect()?;
    cyber_core::init::ensure_global_init(&paths)?;

    let mut config_val = read_value(&paths.config_file)?;
    let mut providers_val = read_value(&paths.providers_file)?;
    let (mut config, mut providers) = effective_config(&paths, cwd)?;
    let mut mcp_servers = McpServersConfig::load(&paths.mcp_servers_file).unwrap_or_default();

    loop {
        let (tools, _) = load_custom_tools(&paths.tools_dir);
        let current_prov = if config.agent.default_provider.is_empty() {
            "未设置"
        } else {
            &config.agent.default_provider
        };
        let current_model = providers
            .providers
            .get(&config.agent.default_provider)
            .map(|p| p.model.as_str())
            .unwrap_or("未设置");

        eprintln!("\n╭──────────────────────────────────────────────────────────────╮");
        eprintln!("│                    Cyber Master Setup 向导                   │");
        eprintln!("│      AI 驱动安全终端体系：Provider · MCP · Custom Tools      │");
        eprintln!("╰──────────────────────────────────────────────────────────────╯");
        eprintln!("当前核心状态:");
        eprintln!("  * 默认 Provider : \x1b[1;36m{current_prov}\x1b[0m ({current_model})");
        eprintln!("  * 已配置 MCP   : {} 个", mcp_servers.servers.len());
        eprintln!("  * 已加载自定义工具: {} 个", tools.len());

        let main_items = vec![
            "1. 模型服务商配置 (Provider: 切换默认 / 热门预设添加 / 编辑 / 连通性测试)".to_string(),
            "2. MCP 服务器配置 (MCP: 热门模板预设 / 自定义 stdio 与 SSE / 连接测试)".to_string(),
            "3. 自定义安全工具管理 (Custom Tools: 手动录入 / 查看已有)".to_string(),
            "4. 🤖 AI 智能扫描本地安全工具 (基于当前模型自动探测系统工具并生成配置)".to_string(),
            "5. 保存配置并退出 (Save & Exit)".to_string(),
            "6. 放弃修改并退出 (Exit without saving)".to_string(),
        ];

        let Some(choice) = select_menu("请选择操作:", &main_items, 0)? else {
            eprintln!("已退出向导。");
            break;
        };

        match choice {
            0 => {
                setup_providers_menu(&mut config, &mut providers).await?;
            }
            1 => {
                setup_mcp_menu(&mut mcp_servers).await?;
            }
            2 => {
                setup_custom_tools_menu(&paths).await?;
            }
            3 => {
                setup_ai_tool_scan(&paths, &config, &providers).await?;
            }
            4 => {
                eprintln!("\n正在执行两阶段原子安全保存...");
                sync_values(&mut config_val, &mut providers_val, &config, &providers)?;
                save_setup(&paths, &config_val, &providers_val, write_private)?;
                if let Err(e) = mcp_servers.save(&paths.mcp_servers_file) {
                    eprintln!("\x1b[33m警告: 保存 servers.toml 出现异常: {e}\x1b[0m");
                }
                eprintln!("\x1b[1;32m✓ 全部配置已保存成功！\x1b[0m");
                eprintln!("  * 服务商凭据已隔离存储至 ~/.cyber/providers.toml");
                eprintln!("  * MCP 配置已更新至 ~/.cyber/mcp/servers.toml");
                eprintln!("  * 自定义工具已同步至 ~/.cyber/tools/*.toml");
                eprintln!("\n运行 \x1b[1;36mcyber\x1b[0m 即可开始使用，或运行 \x1b[1;36mcyber tui\x1b[0m 启动多面板界面。");
                break;
            }
            _ => {
                eprintln!("已退出设置，未写入更改。");
                break;
            }
        }
    }

    Ok(())
}

/// 同步包装函数，供尚未迁移至异步调用的测试与入口复用。
pub fn run_setup_sync(cwd: &Path) -> Result<()> {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(run_setup(cwd))),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_setup(cwd))
        }
    }
}

fn sync_values(
    config_val: &mut Value,
    providers_val: &mut Value,
    config: &Config,
    providers: &ProvidersConfig,
) -> Result<()> {
    if let Some(config_table) = config_val.as_table_mut() {
        let agent = config_table
            .entry("agent")
            .or_insert_with(|| Value::Table(Default::default()));
        if let Some(agent_table) = agent.as_table_mut() {
            agent_table.insert(
                "default_provider".into(),
                Value::String(config.agent.default_provider.clone()),
            );
        }
    }
    if let Some(prov_table) = providers_val.as_table_mut() {
        prov_table.insert(
            "default_provider".into(),
            Value::String(providers.default_provider.clone()),
        );
        let provs_entry = prov_table
            .entry("providers")
            .or_insert_with(|| Value::Table(Default::default()));
        if let Some(provs_table) = provs_entry.as_table_mut() {
            provs_table.retain(|k, _| providers.providers.contains_key(k));
            for (name, p) in &providers.providers {
                let p_val = toml::Value::try_from(p)
                    .map_err(|e| eyre!("Failed to serialize provider: {e}"))?;
                if let Some(existing) = provs_table.get_mut(name).and_then(Value::as_table_mut) {
                    if let Value::Table(new_tbl) = p_val {
                        for (k, v) in new_tbl {
                            existing.insert(k, v);
                        }
                    }
                } else {
                    provs_table.insert(name.clone(), p_val);
                }
            }
        }
    }
    Ok(())
}

async fn setup_providers_menu(config: &mut Config, providers: &mut ProvidersConfig) -> Result<()> {
    loop {
        let current_def = if config.agent.default_provider.is_empty() {
            "未设置"
        } else {
            &config.agent.default_provider
        };
        let items = vec![
            format!("1. 切换默认服务商 (当前默认: \x1b[1;33m{current_def}\x1b[0m)"),
            "2. 从热门厂商预设添加 Provider (DeepSeek / 硅基流动 / 百炼 / GLM / Kimi / OpenAI 等)"
                .to_string(),
            "3. 编辑已有 Provider 配置 (Base URL / API Key / Model)".to_string(),
            "4. 测试当前默认 Provider 连通性并拉取可用模型".to_string(),
            "5. 删除已有 Provider".to_string(),
            "6. 返回上一级主菜单".to_string(),
        ];
        let Some(choice) = select_menu("=== 模型服务商 (Provider) 配置 ===", &items, 0)?
        else {
            break;
        };
        match choice {
            0 => {
                let names = providers.sorted_names();
                if names.is_empty() {
                    eprintln!("\x1b[33m当前尚未配置任何 Provider，请先添加。\x1b[0m");
                    continue;
                }
                let def_idx = names
                    .iter()
                    .position(|n| n == &config.agent.default_provider)
                    .unwrap_or(0);
                if let Some(sel) = select_menu("请选择要设为默认的 Provider:", &names, def_idx)?
                {
                    config.agent.default_provider = names[sel].clone();
                    providers.default_provider = names[sel].clone();
                    eprintln!("\x1b[32m✓ 默认 Provider 已设为: {}\x1b[0m", names[sel]);
                }
            }
            1 => {
                add_provider_preset_flow(config, providers).await?;
            }
            2 => {
                edit_provider_flow(providers).await?;
            }
            3 => {
                if let Some(p) = providers.providers.get(&config.agent.default_provider) {
                    eprintln!(
                        "正在测试当前默认 Provider [{}] 连通性...",
                        config.agent.default_provider
                    );
                    match cyber_agent::fetch_models(p).await {
                        Ok(models) => {
                            eprintln!(
                                "\x1b[1;32m✓ 连通性测试成功！探测到 {} 个可用模型\x1b[0m",
                                models.len()
                            );
                            if !models.is_empty() {
                                let mut menu_models = models.clone();
                                menu_models.push(">> 保持当前配置，不修改".to_string());
                                let cur_idx =
                                    models.iter().position(|m| m == &p.model).unwrap_or(0);
                                if let Some(chosen_idx) = select_menu(
                                    "可直接切换为以下探测到的模型:",
                                    &menu_models,
                                    cur_idx,
                                )? {
                                    if chosen_idx < models.len() {
                                        let new_model = models[chosen_idx].clone();
                                        if let Some(p_mut) = providers
                                            .providers
                                            .get_mut(&config.agent.default_provider)
                                        {
                                            p_mut.model = new_model.clone();
                                            eprintln!(
                                                "\x1b[32m✓ 已将默认模型切换为: {new_model}\x1b[0m"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("\x1b[1;31m✗ 连通性测试失败: {e}\x1b[0m");
                        }
                    }
                } else {
                    eprintln!(
                        "\x1b[33m当前默认 Provider [{}] 未找到有效配置。\x1b[0m",
                        config.agent.default_provider
                    );
                }
            }
            4 => {
                let names = providers.sorted_names();
                if names.is_empty() {
                    eprintln!("\x1b[33m暂无可删除的 Provider。\x1b[0m");
                    continue;
                }
                if let Some(sel) = select_menu("请选择要删除的 Provider:", &names, 0)? {
                    let to_remove = &names[sel];
                    if to_remove == &config.agent.default_provider {
                        eprintln!("\x1b[33m警告: 正在删除当前默认 Provider！\x1b[0m");
                    }
                    let confirm = prompt_text(&format!("确认删除 [{to_remove}]? (y/N)"), "n")?;
                    if confirm.as_deref() == Some("y") || confirm.as_deref() == Some("Y") {
                        providers.remove(to_remove);
                        eprintln!("\x1b[32m✓ 已删除 Provider: {to_remove}\x1b[0m");
                        if to_remove == &config.agent.default_provider {
                            config.agent.default_provider = providers
                                .sorted_names()
                                .into_iter()
                                .next()
                                .unwrap_or_default();
                            providers.default_provider = config.agent.default_provider.clone();
                        }
                    }
                }
            }
            _ => break,
        }
    }
    Ok(())
}

async fn add_provider_preset_flow(
    config: &mut Config,
    providers: &mut ProvidersConfig,
) -> Result<()> {
    let preset_items: Vec<String> = PROVIDER_PRESETS
        .iter()
        .map(|p| {
            if p.base_url.is_empty() {
                format!("{} - {}", p.name, p.description)
            } else {
                format!("{} ({}) - {}", p.name, p.base_url, p.description)
            }
        })
        .collect();

    let Some(idx) = select_menu("请选择预设服务商模板:", &preset_items, 0)? else {
        return Ok(());
    };
    let preset = &PROVIDER_PRESETS[idx];

    let name = if preset.id == "custom" {
        let Some(custom_name) = prompt_text(
            "请输入自定义 Provider 标识名 (英文/数字/下划线)",
            "my_provider",
        )?
        else {
            return Ok(());
        };
        custom_name
    } else {
        preset.id.to_string()
    };

    let base_url = if preset.base_url.is_empty() {
        let Some(url) = prompt_text("请输入 Base URL (如 http://localhost:8000/v1)", "")?
        else {
            return Ok(());
        };
        url
    } else {
        let Some(url) = prompt_text("Base URL", preset.base_url)? else {
            return Ok(());
        };
        url
    };

    if !valid_endpoint(&base_url) {
        eprintln!("\x1b[31m错误: Base URL 必须是合法的 HTTP(S) 地址。\x1b[0m");
        return Ok(());
    }

    let mut provider_cfg = ProviderConfig {
        kind: preset.kind.to_string(),
        base_url,
        api_key: String::new(),
        model: preset.default_model.to_string(),
        max_tokens: 4096,
        temperature: 0.7,
        ..Default::default()
    };

    if preset.kind != "ollama" {
        let cred_mode_items = vec![
            format!(
                "1. 引用环境变量 (推荐, 例如 ${{{}}})",
                if preset.env_var_suggestion.is_empty() {
                    "API_KEY"
                } else {
                    preset.env_var_suggestion
                }
            ),
            "2. 直接输入 API 密钥 (密码星号隐藏输入)".to_string(),
            "3. 暂时留空 (稍后手动配置)".to_string(),
        ];
        let mode_choice = select_menu("凭证认证方式:", &cred_mode_items, 0)?.unwrap_or(0);
        match mode_choice {
            0 => {
                let default_var = if preset.env_var_suggestion.is_empty() {
                    "OPENAI_API_KEY"
                } else {
                    preset.env_var_suggestion
                };
                if let Some(var) = prompt_text("环境变量名称", default_var)? {
                    if valid_env_name(&var) {
                        provider_cfg.api_key = format!("${{{var}}}");
                        if std::env::var(&var).map_or(true, |v| v.trim().is_empty()) {
                            eprintln!(
                                "\x1b[33m提示: 当前环境变量 ${var} 尚未在系统中设置值。\x1b[0m"
                            );
                        } else {
                            eprintln!("\x1b[32m✓ 已检测到当前环境变量 ${var} 存在有效值。\x1b[0m");
                        }
                    } else {
                        eprintln!("\x1b[31m无效的环境变量名，已取消凭据设置。\x1b[0m");
                    }
                }
            }
            1 => {
                if let Some(key) = prompt_password("请输入 API Key (输入时将隐藏显示为 *)")?
                {
                    provider_cfg.api_key = key;
                }
            }
            _ => {}
        }
    }

    let model_prompt_default = if provider_cfg.model.is_empty() {
        "gpt-4o"
    } else {
        &provider_cfg.model
    };
    if let Some(model) = prompt_text("默认 Model ID", model_prompt_default)? {
        provider_cfg.model = model;
    }

    let test_now = prompt_text(
        "是否立即发起连通性测试以拉取服务端最新可用模型列表? [Y/n]",
        "y",
    )?;
    if test_now.as_deref() == Some("y") || test_now.as_deref() == Some("Y") {
        eprintln!("正在测试连接到 {} ...", provider_cfg.base_url);
        match cyber_agent::fetch_models(&provider_cfg).await {
            Ok(models) => {
                eprintln!(
                    "\x1b[1;32m✓ 连通性测试成功！服务端发现 {} 个可用模型\x1b[0m",
                    models.len()
                );
                if !models.is_empty() {
                    let mut menu_items = models.clone();
                    menu_items.push(format!(">> 保持当前设定 ({})", provider_cfg.model));
                    let cur_idx = models
                        .iter()
                        .position(|m| m == &provider_cfg.model)
                        .unwrap_or(0);
                    if let Some(sel) =
                        select_menu("请选择该服务商的默认模型:", &menu_items, cur_idx)?
                    {
                        if sel < models.len() {
                            provider_cfg.model = models[sel].clone();
                            eprintln!("\x1b[32m✓ 已选定模型: {}\x1b[0m", provider_cfg.model);
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("\x1b[31m✗ 连通性测试未通过: {e}\x1b[0m");
                let keep = prompt_text("是否仍保留此 Provider 配置? [Y/n]", "y")?;
                if keep.as_deref() != Some("y") && keep.as_deref() != Some("Y") {
                    eprintln!("已取消添加该 Provider。");
                    return Ok(());
                }
            }
        }
    }

    provider_cfg.normalize();
    providers.upsert(&name, provider_cfg);
    eprintln!("\x1b[1;32m✓ 已成功添加 Provider [{name}]\x1b[0m");

    if config.agent.default_provider.is_empty() || config.agent.default_provider == "openai" {
        config.agent.default_provider = name.clone();
        providers.default_provider = name;
    } else {
        let set_def = prompt_text(&format!("是否将 [{name}] 设为默认 Provider? [Y/n]"), "y")?;
        if set_def.as_deref() == Some("y") || set_def.as_deref() == Some("Y") {
            config.agent.default_provider = name.clone();
            providers.default_provider = name;
            eprintln!(
                "\x1b[32m✓ 已将 [{}] 设为默认 Provider\x1b[0m",
                config.agent.default_provider
            );
        }
    }

    Ok(())
}

async fn edit_provider_flow(providers: &mut ProvidersConfig) -> Result<()> {
    let names = providers.sorted_names();
    if names.is_empty() {
        eprintln!("\x1b[33m当前暂无 Provider。\x1b[0m");
        return Ok(());
    }
    let Some(sel) = select_menu("请选择要编辑的 Provider:", &names, 0)? else {
        return Ok(());
    };
    let name = &names[sel];
    let Some(mut p) = providers.providers.get(name).cloned() else {
        return Ok(());
    };

    loop {
        let key_display = if p.api_key.starts_with("${") {
            p.api_key.clone()
        } else if p.api_key.is_empty() {
            "无".to_string()
        } else {
            "******".to_string()
        };
        let items = vec![
            format!("1. Base URL (当前: {})", p.base_url),
            format!("2. API Key / 凭据 (当前: {key_display})"),
            format!("3. Model ID (当前: {})", p.model),
            "4. 连通性测试与拉取模型".to_string(),
            "5. 保存并返回".to_string(),
        ];
        let Some(choice) = select_menu(&format!("=== 编辑 Provider [{name}] ==="), &items, 0)?
        else {
            break;
        };
        match choice {
            0 => {
                if let Some(new_url) = prompt_text("Base URL", &p.base_url)? {
                    if valid_endpoint(&new_url) {
                        p.base_url = new_url;
                    } else {
                        eprintln!("\x1b[31m错误: 无效的 HTTP(S) 地址。\x1b[0m");
                    }
                }
            }
            1 => {
                let cred_items = vec![
                    "1. 输入新的明文 API Key (隐藏输入)".to_string(),
                    "2. 引用环境变量 (${VAR})".to_string(),
                    "3. 清空 Key (仅用于本地 Ollama)".to_string(),
                ];
                if let Some(c) = select_menu("选择凭证修改方式:", &cred_items, 0)? {
                    match c {
                        0 => {
                            if let Some(k) = prompt_password("输入 API Key")? {
                                p.api_key = k;
                            }
                        }
                        1 => {
                            if let Some(v) = prompt_text("环境变量名称", "API_KEY")? {
                                if valid_env_name(&v) {
                                    p.api_key = format!("${{{v}}}");
                                }
                            }
                        }
                        2 => p.api_key.clear(),
                        _ => {}
                    }
                }
            }
            2 => {
                if let Some(m) = prompt_text("Model ID", &p.model)? {
                    p.model = m;
                }
            }
            3 => {
                eprintln!("正在测试连接...");
                match cyber_agent::fetch_models(&p).await {
                    Ok(models) => {
                        eprintln!(
                            "\x1b[32m✓ 连通成功！服务端返回 {} 款模型\x1b[0m",
                            models.len()
                        );
                        if !models.is_empty() {
                            let mut m_items = models.clone();
                            m_items.push(">> 保持当前".to_string());
                            let cur_idx = models.iter().position(|m| m == &p.model).unwrap_or(0);
                            if let Some(s) = select_menu("选择模型:", &m_items, cur_idx)? {
                                if s < models.len() {
                                    p.model = models[s].clone();
                                    eprintln!("\x1b[32m✓ 已选定模型: {}\x1b[0m", p.model);
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("\x1b[31m✗ 连通失败: {e}\x1b[0m"),
                }
            }
            _ => {
                p.normalize();
                providers.upsert(name, p);
                eprintln!("\x1b[32m✓ Provider [{name}] 修改已暂存。\x1b[0m");
                break;
            }
        }
    }
    Ok(())
}

async fn setup_mcp_menu(mcp_config: &mut McpServersConfig) -> Result<()> {
    loop {
        let server_count = mcp_config.servers.len();
        let items = vec![
            format!("1. 查看已有 MCP 服务 (当前已配置: {server_count} 个)"),
            "2. 从热门模板预设添加 MCP (filesystem, fetch, puppeteer, sqlite...)".to_string(),
            "3. 手动添加自定义 MCP 服务 (stdio 或 http/sse)".to_string(),
            "4. 测试已有 MCP 服务连接与握手".to_string(),
            "5. 删除已有 MCP 服务".to_string(),
            "6. 返回上一级主菜单".to_string(),
        ];
        let Some(choice) = select_menu("=== MCP (Model Context Protocol) 服务配置 ===", &items, 0)?
        else {
            break;
        };
        match choice {
            0 => {
                if mcp_config.servers.is_empty() {
                    eprintln!("\x1b[33m尚未配置任何 MCP 服务。\x1b[0m");
                } else {
                    eprintln!("\n当前已配置的 MCP 服务列表:");
                    for (i, s) in mcp_config.servers.iter().enumerate() {
                        let detail = match s.transport {
                            McpTransport::Stdio => format!(
                                "stdio: {} {:?}",
                                s.command.as_deref().unwrap_or(""),
                                s.args
                            ),
                            McpTransport::Http => {
                                format!("http: {}", s.url.as_deref().unwrap_or(""))
                            }
                            McpTransport::Sse => {
                                format!("sse: {}", s.url.as_deref().unwrap_or(""))
                            }
                        };
                        eprintln!("  {}. \x1b[1;36m{}\x1b[0m [{}]", i + 1, s.name, detail);
                    }
                }
            }
            1 => {
                add_mcp_preset_flow(mcp_config).await?;
            }
            2 => {
                add_mcp_custom_flow(mcp_config).await?;
            }
            3 => {
                if mcp_config.servers.is_empty() {
                    eprintln!("\x1b[33m尚未配置任何 MCP 服务可供测试。\x1b[0m");
                    continue;
                }
                let names: Vec<String> =
                    mcp_config.servers.iter().map(|s| s.name.clone()).collect();
                if let Some(sel) = select_menu("请选择要测试连接的 MCP 服务:", &names, 0)?
                {
                    let spec = &mcp_config.servers[sel];
                    eprintln!("正在尝试启动并连接 MCP 服务 [{}] ...", spec.name);
                    match test_mcp_connection(spec).await {
                        Ok(cnt) => eprintln!(
                            "\x1b[32m✓ MCP [{}] 握手成功，可用工具数: {cnt}\x1b[0m",
                            spec.name
                        ),
                        Err(e) => eprintln!("\x1b[31m✗ MCP [{}] 握手失败: {e}\x1b[0m", spec.name),
                    }
                }
            }
            4 => {
                if mcp_config.servers.is_empty() {
                    eprintln!("\x1b[33m尚未配置任何 MCP 服务。\x1b[0m");
                    continue;
                }
                let names: Vec<String> =
                    mcp_config.servers.iter().map(|s| s.name.clone()).collect();
                if let Some(sel) = select_menu("请选择要删除的 MCP 服务:", &names, 0)? {
                    let to_remove = &names[sel];
                    mcp_config.remove(to_remove);
                    eprintln!("\x1b[32m✓ 已删除 MCP 服务: {to_remove}\x1b[0m");
                }
            }
            _ => break,
        }
    }
    Ok(())
}

async fn add_mcp_preset_flow(mcp_config: &mut McpServersConfig) -> Result<()> {
    let preset_items: Vec<String> = MCP_PRESETS
        .iter()
        .map(|p| format!("{} - {}", p.display_name, p.description))
        .collect();
    let Some(idx) = select_menu("请选择预设 MCP 模板:", &preset_items, 0)? else {
        return Ok(());
    };
    let preset = &MCP_PRESETS[idx];
    let mut spec = preset.to_server_spec();

    if let Some(name) = prompt_text("服务标识名", &spec.name)? {
        spec.name = name;
    }

    if spec.transport == McpTransport::Stdio {
        let cmd_default = spec.command.as_deref().unwrap_or("npx");
        if let Some(cmd) = prompt_text("执行程序 (command)", cmd_default)? {
            spec.command = Some(cmd);
        }
        let args_default = spec.args.join(" ");
        if let Some(args_str) = prompt_text("启动参数 (args, 空格分隔)", &args_default)? {
            spec.args = args_str.split_whitespace().map(|s| s.to_string()).collect();
        }
    } else if let Some(url_str) = prompt_text(
        "服务端点 URL",
        spec.url.as_deref().unwrap_or("http://localhost:8000/sse"),
    )? {
        spec.url = Some(url_str);
    }

    let test_now = prompt_text("是否立即发起连接握手测试? [Y/n]", "y")?;
    if test_now.as_deref() == Some("y") || test_now.as_deref() == Some("Y") {
        eprintln!("正在连接 MCP 服务 [{}] ...", spec.name);
        match test_mcp_connection(&spec).await {
            Ok(cnt) => eprintln!("\x1b[32m✓ 握手成功！获取到 {cnt} 个工具。\x1b[0m"),
            Err(e) => {
                eprintln!("\x1b[31m✗ 连接失败: {e}\x1b[0m");
                let keep = prompt_text("是否仍保留此 MCP 服务配置? [Y/n]", "y")?;
                if keep.as_deref() != Some("y") && keep.as_deref() != Some("Y") {
                    eprintln!("已取消添加该 MCP 服务。");
                    return Ok(());
                }
            }
        }
    }

    mcp_config.upsert(spec);
    eprintln!("\x1b[32m✓ 已暂存 MCP 服务配置。\x1b[0m");
    Ok(())
}

async fn add_mcp_custom_flow(mcp_config: &mut McpServersConfig) -> Result<()> {
    let transport_items = vec![
        "1. 本地标准输入输出 (stdio: 命令行子进程)".to_string(),
        "2. 远程 HTTP (Streamable HTTP / JSON-RPC)".to_string(),
        "3. 远程 SSE (Server-Sent Events)".to_string(),
    ];
    let Some(t_choice) = select_menu("请选择 MCP 传输协议类型:", &transport_items, 0)?
    else {
        return Ok(());
    };
    let (transport, is_stdio) = match t_choice {
        0 => (McpTransport::Stdio, true),
        1 => (McpTransport::Http, false),
        _ => (McpTransport::Sse, false),
    };

    let Some(name) = prompt_text("服务唯一标识名称 (name)", "custom_mcp")? else {
        return Ok(());
    };

    let mut spec = McpServerSpec {
        name,
        transport,
        command: None,
        args: Vec::new(),
        env: HashMap::new(),
        url: None,
        headers: HashMap::new(),
        timeout_secs: McpServerSpec::DEFAULT_TIMEOUT,
    };

    if is_stdio {
        let Some(cmd) = prompt_text("可执行文件路径或命令 (command)", "")? else {
            return Ok(());
        };
        spec.command = Some(cmd);
        if let Some(args_str) = prompt_text("启动参数 (args, 空格分隔)", "")? {
            spec.args = args_str.split_whitespace().map(|s| s.to_string()).collect();
        }
    } else {
        let Some(url) = prompt_text("服务 URL 地址 (如 https://scanner.internal/mcp)", "")?
        else {
            return Ok(());
        };
        spec.url = Some(url);
    }

    let test_now = prompt_text("是否立即测试连接握手? [Y/n]", "y")?;
    if test_now.as_deref() == Some("y") || test_now.as_deref() == Some("Y") {
        eprintln!("正在连接测试...");
        match test_mcp_connection(&spec).await {
            Ok(cnt) => eprintln!("\x1b[32m✓ 连接测试成功！发现 {cnt} 个工具。\x1b[0m"),
            Err(e) => eprintln!("\x1b[31m✗ 连接失败: {e}\x1b[0m"),
        }
    }

    mcp_config.upsert(spec);
    eprintln!("\x1b[32m✓ 自定义 MCP 服务已暂存。\x1b[0m");
    Ok(())
}

async fn test_mcp_connection(spec: &McpServerSpec) -> Result<usize> {
    let (conn, handle) = McpConnection::connect(spec).await?;
    let tool_count = conn.tools().len();
    conn.shutdown();
    let _ = tokio::time::timeout(std::time::Duration::from_millis(600), handle).await;
    Ok(tool_count)
}

async fn setup_custom_tools_menu(paths: &Paths) -> Result<()> {
    loop {
        let (tools, _) = load_custom_tools(&paths.tools_dir);
        let items = vec![
            format!("1. 查看已有自定义工具 (当前已加载: {} 个)", tools.len()),
            "2. 手动录入新自定义安全工具 (配置命令模板与占位符参数)".to_string(),
            "3. 删除自定义安全工具".to_string(),
            "4. 返回上一级主菜单".to_string(),
        ];
        let Some(choice) = select_menu("=== 自定义安全工具 (Custom Tools) 管理 ===", &items, 0)?
        else {
            break;
        };
        match choice {
            0 => {
                if tools.is_empty() {
                    eprintln!(
                        "\x1b[33m当前尚未配置任何自定义工具 (目录: {})\x1b[0m",
                        paths.tools_dir.display()
                    );
                } else {
                    eprintln!("\n=== 当前已加载的自定义工具 ===");
                    for (i, t) in tools.iter().enumerate() {
                        eprintln!(
                            "  {}. \x1b[1;36m{}\x1b[0m: {}",
                            i + 1,
                            t.config.name,
                            t.config.description
                        );
                        eprintln!("     命令: \x1b[90m{}\x1b[0m", t.config.command);
                        let params: Vec<String> = t
                            .config
                            .parameters
                            .iter()
                            .map(|p| format!("{{{}}}", p.name))
                            .collect();
                        eprintln!("     参数: \x1b[90m{}\x1b[0m", params.join(", "));
                    }
                }
            }
            1 => {
                add_custom_tool_manual_flow(paths)?;
            }
            2 => {
                if tools.is_empty() {
                    eprintln!("\x1b[33m当前无自定义工具可删除。\x1b[0m");
                    continue;
                }
                let names: Vec<String> = tools.iter().map(|t| t.config.name.clone()).collect();
                if let Some(sel) = select_menu("请选择要删除的工具:", &names, 0)? {
                    let tool_name = &names[sel];
                    let file = paths.tools_dir.join(format!("{tool_name}.toml"));
                    if file.exists() {
                        std::fs::remove_file(&file)?;
                        eprintln!("\x1b[32m✓ 已删除自定义工具文件: {}\x1b[0m", file.display());
                    }
                }
            }
            _ => break,
        }
    }
    Ok(())
}

fn add_custom_tool_manual_flow(paths: &Paths) -> Result<()> {
    eprintln!("\n=== 手动添加自定义安全工具 ===");
    let Some(name) = prompt_text("工具唯一英文名 (如 nmap_quick / sqlmap)", "")? else {
        return Ok(());
    };
    if name.trim().is_empty() {
        eprintln!("\x1b[31m工具名称不能为空。\x1b[0m");
        return Ok(());
    }

    let Some(desc) = prompt_text("工具功能简要中文描述 (提供给 LLM 识别)", "")?
    else {
        return Ok(());
    };

    let Some(cmd) = prompt_text(
        "执行命令模板 (使用 {param} 占位符, 如 nmap -sV -p {port} {target})",
        "",
    )?
    else {
        return Ok(());
    };
    if cmd.trim().is_empty() {
        eprintln!("\x1b[31m执行命令不能为空。\x1b[0m");
        return Ok(());
    }

    let mut parameters = Vec::new();
    eprintln!("\n现在为命令中的占位符配置参数说明 (输入空参数名完成):");
    loop {
        let Some(p_name) = prompt_text("参数名 (如 target / port, 直接回车结束)", "")?
        else {
            break;
        };
        if p_name.trim().is_empty() {
            break;
        }
        let p_desc = prompt_text(&format!("参数 [{p_name}] 的描述"), "")?.unwrap_or_default();
        let p_req = prompt_text(&format!("参数 [{p_name}] 是否必填? [Y/n]"), "y")?;
        let is_required = p_req.as_deref() == Some("y") || p_req.as_deref() == Some("Y");
        let p_def = if !is_required {
            prompt_text(&format!("参数 [{p_name}] 的默认值 (可选)"), "")?
        } else {
            None
        };
        parameters.push(CustomToolParam {
            name: p_name,
            description: p_desc,
            required: is_required,
            default: p_def.filter(|s| !s.is_empty()),
        });
    }

    let config = CustomToolConfig {
        name,
        description: desc,
        command: cmd,
        tags: vec!["custom".to_string(), "security".to_string()],
        parameters,
    };

    let saved = save_custom_tool(&paths.tools_dir, &config)?;
    eprintln!(
        "\x1b[1;32m✓ 已成功创建自定义工具: {}\x1b[0m",
        saved.display()
    );
    Ok(())
}

async fn ask_llm(cfg: &ProviderConfig, system_prompt: &str, user_prompt: &str) -> Result<String> {
    let provider = cyber_agent::provider_factory(cfg, false)?;
    let req = cyber_agent::StreamRequest::new(vec![cyber_agent::Message::user(user_prompt)])
        .with_system(system_prompt);
    let mut stream = provider.stream(req);
    let mut output = String::new();
    while let Some(event) = stream.next().await {
        match event {
            cyber_agent::StreamEvent::Delta(text) => output.push_str(&text),
            cyber_agent::StreamEvent::Error(err) => {
                bail!("模型返回错误: {err}");
            }
            _ => {}
        }
    }
    if output.trim().is_empty() {
        bail!("模型返回内容为空");
    }
    Ok(output)
}

fn extract_toml_block(content: &str) -> Option<String> {
    if let Some(start) = content.find("```toml") {
        let after = &content[start + 7..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim().to_string());
        }
    }
    if let Some(start) = content.find("```") {
        let after = &content[start + 3..];
        if let Some(end) = after.find("```") {
            return Some(after[..end].trim().to_string());
        }
    }
    None
}

fn extract_all_toml_blocks(content: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut search = content;
    while let Some(start) = search.find("```toml") {
        let after = &search[start + 7..];
        if let Some(end) = after.find("```") {
            blocks.push(after[..end].trim().to_string());
            search = &after[end + 3..];
        } else {
            break;
        }
    }
    if blocks.is_empty() {
        if let Some(single) = extract_toml_block(content) {
            blocks.push(single);
        }
    }
    blocks
}

fn find_installed_binary(name: &str) -> Option<String> {
    let cmd = if cfg!(windows) { "where" } else { "which" };
    if let Ok(output) = std::process::Command::new(cmd).arg(name).output() {
        if output.status.success() {
            let out_str = String::from_utf8_lossy(&output.stdout);
            let first_line = out_str.lines().next().unwrap_or(name).trim();
            if !first_line.is_empty() {
                return Some(first_line.to_string());
            }
        }
    }
    None
}

fn fetch_tool_help(name: &str) -> Option<String> {
    let run = |flag: &str| -> Option<String> {
        let child = std::process::Command::new(name)
            .arg(flag)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .ok()?;
        let output = child.wait_with_output().ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = if stdout.trim().len() > 50 {
            stdout.to_string()
        } else {
            stderr.to_string()
        };
        if text.trim().len() > 20 {
            let lines: Vec<&str> = text.lines().take(200).collect();
            Some(lines.join("\n"))
        } else {
            None
        }
    };
    run("-h").or_else(|| run("--help"))
}

fn run_help_for_command(cmd_prefix: &str, tool_name: &str) -> Option<String> {
    let trimmed = cmd_prefix.trim();
    let (prog, args) = if let Some(rest) = trimmed.strip_prefix("python ") {
        ("python", vec![rest.trim().trim_matches('"')])
    } else if let Some(rest) = trimmed.strip_prefix("java -jar ") {
        ("java", vec!["-jar", rest.trim().trim_matches('"')])
    } else {
        (trimmed.trim_matches('"'), Vec::new())
    };

    let run_flag = |flag: &str| -> Option<String> {
        let mut command = std::process::Command::new(prog);
        for a in &args {
            command.arg(a);
        }
        command.arg(flag);
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let child = command.spawn().ok()?;
        let output = child.wait_with_output().ok()?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let text = if stdout.trim().len() > 50 {
            stdout.to_string()
        } else {
            stderr.to_string()
        };
        if text.trim().len() > 20 {
            let lines: Vec<&str> = text.lines().take(200).collect();
            Some(lines.join("\n"))
        } else {
            None
        }
    };

    run_flag("-h")
        .or_else(|| run_flag("--help"))
        .or_else(|| fetch_tool_help(tool_name))
}

fn scan_directory_for_tools(dir: &Path) -> Vec<(String, String, Option<String>)> {
    let mut tools = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return tools;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                let ext_lower = ext.to_ascii_lowercase();
                if ["exe", "bat", "cmd", "sh", "py", "jar"].contains(&ext_lower.as_str()) {
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("tool")
                        .to_string();
                    let cmd = if ext_lower == "py" {
                        format!("python \"{}\"", path.display())
                    } else if ext_lower == "jar" {
                        format!("java -jar \"{}\"", path.display())
                    } else {
                        format!("\"{}\"", path.display())
                    };
                    let help = run_help_for_command(&cmd, &name);
                    tools.push((name, cmd, help));
                }
            }
        } else if path.is_dir() {
            let folder_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if let Ok(sub_entries) = std::fs::read_dir(&path) {
                for sub in sub_entries.flatten() {
                    let sub_path = sub.path();
                    if sub_path.is_file() {
                        let stem = sub_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                        let ext = sub_path
                            .extension()
                            .and_then(|s| s.to_str())
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        if (stem.eq_ignore_ascii_case(&folder_name)
                            || stem == "main"
                            || stem == "run")
                            && ["py", "exe", "jar", "bat", "sh"].contains(&ext.as_str())
                        {
                            let name = folder_name.clone();
                            let cmd = if ext == "py" {
                                format!("python \"{}\"", sub_path.display())
                            } else if ext == "jar" {
                                format!("java -jar \"{}\"", sub_path.display())
                            } else {
                                format!("\"{}\"", sub_path.display())
                            };
                            let help = run_help_for_command(&cmd, &name);
                            tools.push((name, cmd, help));
                            break;
                        }
                    }
                }
            }
        }
    }
    tools
}

async fn process_freeform_prompt(
    paths: &Paths,
    provider_cfg: &ProviderConfig,
    user_prompt: &str,
) -> Result<()> {
    eprintln!("\n正在请求 AI 分析您的提示词并提取安全工具配置...");
    let system_prompt = "你是一个网络安全渗透测试工具专家。请根据用户的需求和本地工具描述，将其封装为一个或多个合法的 Cyber Master CustomToolConfig 配置。\n\
每个工具必须输出为一个独立的 ```toml ... ``` 代码块，不要包含前言或解释。\n\
格式示例：\n\
```toml\n\
name = \"sqlmap\"\n\
description = \"自动化 SQL 注入检测\"\n\
command = \"python D:/tools/sqlmap/sqlmap.py -u {url} --batch {extra}\"\n\
tags = [\"sqli\", \"web\"]\n\
\n\
[[parameters]]\n\
name = \"url\"\n\
description = \"目标 URL 地址\"\n\
required = true\n\
\n\
[[parameters]]\n\
name = \"extra\"\n\
description = \"额外参数\"\n\
required = false\n\
default = \"--smart\"\n\
```";

    let reply = ask_llm(provider_cfg, system_prompt, user_prompt).await?;
    let toml_blocks = extract_all_toml_blocks(&reply);
    if toml_blocks.is_empty() {
        eprintln!("\x1b[31mAI 未能生成有效的 TOML 配置块。\x1b[0m");
        eprintln!("模型输出:\n{reply}");
        return Ok(());
    }

    for toml_code in toml_blocks {
        match toml::from_str::<CustomToolConfig>(&toml_code) {
            Ok(tool_cfg) => {
                eprintln!("\n──────────────────────────────────────────────────────────");
                eprintln!(
                    "\x1b[1;32mAI 已生成工具 [{}] 配置草稿:\x1b[0m",
                    tool_cfg.name
                );
                eprintln!("  名称: \x1b[36m{}\x1b[0m", tool_cfg.name);
                eprintln!("  描述: {}", tool_cfg.description);
                eprintln!("  命令: \x1b[33m{}\x1b[0m", tool_cfg.command);
                let p_names: Vec<String> = tool_cfg
                    .parameters
                    .iter()
                    .map(|p| {
                        if p.required {
                            format!("{}(必填)", p.name)
                        } else {
                            format!("{}(选填)", p.name)
                        }
                    })
                    .collect();
                eprintln!("  参数: {}", p_names.join(", "));
                eprintln!("──────────────────────────────────────────────────────────");

                let confirm = prompt_text(
                    &format!(
                        "是否确认保存为 ~/.cyber/tools/{}.toml? [Y/n]",
                        tool_cfg.name
                    ),
                    "y",
                )?;
                if confirm.as_deref() == Some("y") || confirm.as_deref() == Some("Y") {
                    match save_custom_tool(&paths.tools_dir, &tool_cfg) {
                        Ok(p) => eprintln!("\x1b[1;32m✓ 已成功生成: {}\x1b[0m", p.display()),
                        Err(e) => eprintln!("\x1b[31m保存工具配置失败: {e}\x1b[0m"),
                    }
                } else {
                    eprintln!("已跳过 [{}]。", tool_cfg.name);
                }
            }
            Err(e) => {
                eprintln!("\x1b[31m解析生成的 TOML 失败: {e}\x1b[0m");
            }
        }
    }

    Ok(())
}

async fn process_tool_candidates(
    paths: &Paths,
    provider_cfg: &ProviderConfig,
    tools: Vec<(String, String, Option<String>)>,
) -> Result<()> {
    let items: Vec<(String, bool)> = tools
        .iter()
        .map(|(name, cmd, _)| {
            let configured = paths.tools_dir.join(format!("{name}.toml")).exists();
            let status = if configured {
                " [已配置]"
            } else {
                " [未配置]"
            };
            (format!("{name} - {cmd}{status}"), !configured)
        })
        .collect();

    let Some(selected_indices) = multi_select_menu(
        "发现以下本地工具，请勾选需要让 AI 分析并生成配置的项:",
        &items,
    )?
    else {
        return Ok(());
    };

    if selected_indices.is_empty() {
        eprintln!("未选择任何工具。");
        return Ok(());
    }

    for idx in selected_indices {
        let (name, cmd, help_opt) = &tools[idx];
        eprintln!("\n正在获取 [{name}] 帮助信息并调用 AI 分析...");
        let help_text = help_opt
            .clone()
            .unwrap_or_else(|| fetch_tool_help(name).unwrap_or_else(|| format!("command: {cmd}")));
        let system_prompt = "你是一个网络安全渗透测试工具专家。请根据提供的命令行工具与帮助信息，生成符合 Cyber Master CustomToolConfig 的标准 TOML 配置。";
        let user_prompt = format!(
            "工具名称: {name}\n执行命令基础: {cmd}\n\n帮助文档输出摘要:\n{help_text}\n\n\
            请生成合法的 TOML 配置代码块，要求如下：\n\
            1. 包含 name（字符串，英文小写），description（中文简述），command（命令行模板，保留原执行路径，核心参数用 {{param}} 占位符包裹），tags（如 [\"security\", \"scan\"]）。\n\
            2. 包含 [[parameters]] 数组，必须包含 command 中用到的每个占位符参数（name, description, required, default）。\n\
            3. 只输出 ```toml ... ``` 代码块，严禁包含任何前言或后记解释。"
        );

        eprintln!("正在请求 AI 生成 [{name}] 配置草稿...");
        match ask_llm(provider_cfg, system_prompt, &user_prompt).await {
            Ok(reply) => {
                let toml_code = extract_toml_block(&reply).unwrap_or(reply);
                match toml::from_str::<CustomToolConfig>(&toml_code) {
                    Ok(mut tool_cfg) => {
                        tool_cfg.name = name.clone();
                        eprintln!("\n──────────────────────────────────────────────────────────");
                        eprintln!("\x1b[1;32mAI 已为 [{name}] 生成配置草稿:\x1b[0m");
                        eprintln!("  名称: \x1b[36m{}\x1b[0m", tool_cfg.name);
                        eprintln!("  描述: {}", tool_cfg.description);
                        eprintln!("  命令: \x1b[33m{}\x1b[0m", tool_cfg.command);
                        let p_names: Vec<String> = tool_cfg
                            .parameters
                            .iter()
                            .map(|p| {
                                if p.required {
                                    format!("{}(必填)", p.name)
                                } else {
                                    format!("{}(选填)", p.name)
                                }
                            })
                            .collect();
                        eprintln!("  参数: {}", p_names.join(", "));
                        eprintln!("──────────────────────────────────────────────────────────");

                        let confirm = prompt_text(
                            &format!("是否确认将此配置保存为 ~/.cyber/tools/{name}.toml? [Y/n]"),
                            "y",
                        )?;
                        if confirm.as_deref() == Some("y") || confirm.as_deref() == Some("Y") {
                            match save_custom_tool(&paths.tools_dir, &tool_cfg) {
                                Ok(p) => {
                                    eprintln!("\x1b[1;32m✓ 已成功生成: {}\x1b[0m", p.display())
                                }
                                Err(e) => eprintln!("\x1b[31m保存工具配置失败: {e}\x1b[0m"),
                            }
                        } else {
                            eprintln!("已跳过 [{name}]。");
                        }
                    }
                    Err(e) => {
                        eprintln!("\x1b[31mAI 生成的 TOML 格式无法解析: {e}\x1b[0m");
                        eprintln!("原始输出:\n{toml_code}");
                    }
                }
            }
            Err(e) => {
                eprintln!("\x1b[31mAI 分析 [{name}] 失败: {e}\x1b[0m");
            }
        }
    }

    Ok(())
}

async fn setup_ai_tool_scan(
    paths: &Paths,
    config: &Config,
    providers: &ProvidersConfig,
) -> Result<()> {
    eprintln!("\n╭──────────────────────────────────────────────────────────────╮");
    eprintln!("│              🤖 AI 智能扫描并添加本地安全工具                │");
    eprintln!("╰──────────────────────────────────────────────────────────────╯");

    let Some(provider_cfg) = providers.providers.get(&config.agent.default_provider) else {
        eprintln!(
            "\x1b[31m未配置默认 Provider，AI 智能扫描不可用。请先进入菜单 1 进行配置。\x1b[0m"
        );
        return Ok(());
    };
    if provider_cfg.kind != "ollama" && provider_cfg.resolved_api_key().trim().is_empty() {
        eprintln!(
            "\x1b[31m当前 Provider [{}] 未设置 API Key 或有效环境变量，无法调用 AI 分析。\x1b[0m",
            config.agent.default_provider
        );
        return Ok(());
    }

    eprintln!(
        "当前使用 AI 模型: \x1b[1;36m{} ({})\x1b[0m",
        config.agent.default_provider, provider_cfg.model
    );

    eprintln!("\n\x1b[1;33m提示:\x1b[0m 您可以直接告诉 AI 您的工具位置或需求，例如:");
    eprintln!("  * 目录路径: 如 \x1b[36mD:\\tools\x1b[0m (自动扫描目录下的所有安全工具与脚本)");
    eprintln!("  * 命令提示: 如 \x1b[36m我有 D:\\tools\\fscan.exe 和 python D:\\tools\\sqlmap\\sqlmap.py，请帮我配置\x1b[0m");
    eprintln!("  * 直接回车: 自动探测系统环境变量 PATH 及常见目录中的工具");

    let user_input = prompt_text("\n请输入工具所在目录、具体命令或提示词", "")?;
    let input_str = user_input.as_deref().unwrap_or("").trim().trim_matches('"');

    if !input_str.is_empty() {
        let input_path = Path::new(input_str);
        if input_path.is_dir() {
            eprintln!(
                "\n正在扫描目录 [{}] 下的工具与脚本...",
                input_path.display()
            );
            let scanned = scan_directory_for_tools(input_path);
            if scanned.is_empty() {
                eprintln!(
                    "\x1b[33m在目录 [{}] 下未找到明显的安全可执行文件或脚本。\x1b[0m",
                    input_path.display()
                );
                eprintln!("转为将该目录作为提示词由 AI 深度分析...");
                return process_freeform_prompt(
                    paths,
                    provider_cfg,
                    &format!("我的工具存放在目录: {input_str}"),
                )
                .await;
            }
            return process_tool_candidates(paths, provider_cfg, scanned).await;
        } else if input_path.is_file()
            || input_str.starts_with("python ")
            || input_str.starts_with("java -jar ")
        {
            let name = input_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("tool")
                .to_string();
            let cmd = input_str.to_string();
            let help = run_help_for_command(&cmd, &name);
            return process_tool_candidates(paths, provider_cfg, vec![(name, cmd, help)]).await;
        } else {
            // Free-form natural language prompt
            return process_freeform_prompt(paths, provider_cfg, input_str).await;
        }
    }

    // Default: scan PATH and common directories
    eprintln!("\n正在检测系统 PATH 与常见安全工具目录...");
    let mut found_tools = Vec::new();
    let candidate_defs = [
        ("nmap", "网络端口与服务版本扫描"),
        ("sqlmap", "自动化 SQL 注入检测与利用"),
        ("dirsearch", "Web 路径与敏感目录爆破"),
        ("subfinder", "被动子域名枚举与收集"),
        ("nuclei", "基于社区模板的漏洞快速扫描"),
        ("nikto", "Web 服务器配置缺陷与漏洞扫描"),
        ("hydra", "网络服务登录暴力破解"),
        ("gobuster", "URI 与 DNS 目录爆破"),
        ("ffuf", "高性能 Web Fuzz 模糊测试"),
        ("whatweb", "Web 网站技术指纹识别"),
        ("wpscan", "WordPress 站点安全扫描"),
        ("masscan", "大规模高速端口扫描"),
        ("fscan", "内网综合扫描与弱口令爆破"),
        ("xray", "安全漏洞评估与主动扫描"),
        ("httpx", "HTTP 批量探测与存活识别"),
    ];

    for (name, _desc) in candidate_defs {
        if let Some(bin_path) = find_installed_binary(name) {
            let help = fetch_tool_help(name);
            found_tools.push((name.to_string(), bin_path, help));
        }
    }

    // Also scan common tool directories if they exist
    for common_dir in [
        "D:\\tools",
        "C:\\tools",
        "D:\\SecTools",
        "C:\\SecTools",
        "D:\\Program Files (x86)\\Nmap",
    ] {
        let p = Path::new(common_dir);
        if p.is_dir() {
            let tools = scan_directory_for_tools(p);
            for (name, cmd, help) in tools {
                if !found_tools
                    .iter()
                    .any(|(n, _, _)| n.eq_ignore_ascii_case(&name))
                {
                    found_tools.push((name, cmd, help));
                }
            }
        }
    }

    if found_tools.is_empty() {
        eprintln!("\x1b[33m未在系统 PATH 或常见目录中检测到常见工具。\x1b[0m");
        eprintln!("提示: 您可以重新选择本菜单，并直接输入您存放工具的目录路径 (例如 D:\\tools)。");
        return Ok(());
    }

    process_tool_candidates(paths, provider_cfg, found_tools).await
}

fn save_setup(
    paths: &Paths,
    config: &Value,
    providers: &Value,
    mut write: impl FnMut(&Path, &[u8]) -> Result<()>,
) -> Result<()> {
    let config = toml::to_string_pretty(config)?;
    let providers = toml::to_string_pretty(providers)?;
    let state = paths.cyber_home.join(STATE_FILE);
    // Persist intent before changing either file, including when reconfiguring a completed setup.
    write(
        &state,
        b"version = 1\nin_progress = true\ncompleted = false\n",
    )?;
    write(&paths.providers_file, providers.as_bytes())?;
    write(&paths.config_file, config.as_bytes())?;
    // Only this final write permits startup after a multi-file commit.
    write(
        &state,
        b"version = 1\nin_progress = false\ncompleted = true\n",
    )?;
    Ok(())
}

fn read_value(path: &Path) -> Result<Value> {
    let raw = cyber_core::fsutil::read_utf8(path)?;
    // TOML errors can contain source lines, including plaintext credentials.
    toml::from_str(&raw).map_err(|_| {
        eyre!(
            "Invalid TOML in {}; fix it before `cyber setup`.",
            path.display()
        )
    })
}

fn decode<T: serde::de::DeserializeOwned>(value: &Value, path: &Path) -> Result<T> {
    value.clone().try_into().map_err(|_| {
        eyre!(
            "Invalid configuration structure in {}; fix it before `cyber setup`.",
            path.display()
        )
    })
}

fn effective_config(paths: &Paths, cwd: &Path) -> Result<(Config, ProvidersConfig)> {
    let mut value = read_value(&paths.config_file)?;
    let project = Paths::project_local_dir(cwd).join("config.toml");
    if project.try_exists()? {
        merge(&mut value, read_value(&project)?);
    }
    Ok((
        decode(&value, &paths.config_file)?,
        decode(&read_value(&paths.providers_file)?, &paths.providers_file)?,
    ))
}

fn merge(base: &mut Value, over: Value) {
    if let (Some(base), Value::Table(over)) = (base.as_table_mut(), over) {
        for (key, value) in over {
            if let Some(existing) = base
                .get_mut(&key)
                .filter(|v| v.is_table() && value.is_table())
            {
                merge(existing, value);
            } else {
                base.insert(key, value);
            }
        }
    }
}

fn configured(config: &Config, providers: &ProvidersConfig) -> bool {
    providers
        .providers
        .get(&config.agent.default_provider)
        .is_some_and(|p| {
            PROVIDER_KINDS.contains(&p.kind.as_str())
                && valid_endpoint(&p.base_url)
                && !p.model.trim().is_empty()
                && (p.kind == "ollama" || !p.resolved_api_key().trim().is_empty())
        })
}

fn valid_endpoint(endpoint: &str) -> bool {
    !endpoint.chars().any(char::is_whitespace)
        && url::Url::parse(endpoint).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        })
}

fn valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

#[allow(dead_code)]
fn update_values(
    config: &mut Value,
    providers: &mut Value,
    name: &str,
    provider: &ProviderConfig,
) -> Result<()> {
    let config = config
        .as_table_mut()
        .ok_or_else(|| eyre!("Config must be a TOML table."))?;
    let agent = config
        .entry("agent")
        .or_insert_with(|| Value::Table(Default::default()));
    agent
        .as_table_mut()
        .ok_or_else(|| eyre!("agent must be a table."))?
        .insert("default_provider".into(), Value::String(name.into()));
    let table = providers
        .as_table_mut()
        .ok_or_else(|| eyre!("Providers must be a TOML table."))?;
    table.insert("default_provider".into(), Value::String(name.into()));
    let selected = table
        .get_mut("providers")
        .and_then(Value::as_table_mut)
        .and_then(|t| t.get_mut(name))
        .and_then(Value::as_table_mut)
        .ok_or_else(|| eyre!("Selected provider must be a table."))?;
    for (key, value) in [
        ("base_url", &provider.base_url),
        ("api_key", &provider.api_key),
        ("model", &provider.model),
    ] {
        selected.insert(key.into(), Value::String(value.clone()));
    }
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("Configuration path has no parent."))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(windows)]
    {
        // Remove inherited access before writing secrets; OWNER RIGHTS grants only the file owner.
        let output = std::process::Command::new("icacls")
            .arg(temp.path())
            .args(["/inheritance:r", "/grant:r", "*S-1-3-4:(F)"])
            .output()
            .wrap_err("Unable to restrict configuration file permissions")?;
        if !output.status.success() {
            bail!("Unable to restrict configuration file permissions; nothing written.");
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    temp.write_all(data)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|e| eyre!("Unable to replace {}: {}", path.display(), e.error))?;
    Ok(())
}

/// RAII 守护：在进入终端原始模式时自动在 drop 时恢复终端状态。
struct RawModeGuard;

impl RawModeGuard {
    fn new() -> Result<Self> {
        terminal::enable_raw_mode().wrap_err("无法启用终端原始模式")?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

/// 单选菜单：支持方向键 ↑/↓ 移动光标，Enter 确认，Esc 或 Ctrl+C 返回。
pub fn select_menu(title: &str, items: &[String], default: usize) -> Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut selected = if default < items.len() { default } else { 0 };
    eprintln!("\n\x1b[1;36m{title}\x1b[0m");
    eprintln!("\x1b[90m(使用 ↑/↓ 移动光标，Enter 确认，Esc 返回/取消)\x1b[0m");

    for (i, item) in items.iter().enumerate() {
        if i == selected {
            eprintln!("  \x1b[1;32m> {}\x1b[0m", item);
        } else {
            eprintln!("    \x1b[90m{}\x1b[0m", item);
        }
    }
    io::stderr().flush()?;

    let total_lines = items.len();
    let _raw = RawModeGuard::new()?;

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc => {
                drop(_raw);
                eprintln!();
                return Ok(None);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                drop(_raw);
                eprintln!();
                bail!("操作已取消");
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if selected == 0 {
                    selected = items.len() - 1;
                } else {
                    selected -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if selected + 1 >= items.len() {
                    selected = 0;
                } else {
                    selected += 1;
                }
            }
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = items.len() - 1,
            KeyCode::Enter => {
                drop(_raw);
                eprintln!("\x1b[32m  ✓ 已选择: {}\x1b[0m", items[selected]);
                return Ok(Some(selected));
            }
            _ => continue,
        }

        eprint!("\x1b[{}A\r", total_lines);
        for (i, item) in items.iter().enumerate() {
            if i == selected {
                eprintln!("\x1b[2K  \x1b[1;32m> {}\x1b[0m", item);
            } else {
                eprintln!("\x1b[2K    \x1b[90m{}\x1b[0m", item);
            }
        }
        io::stderr().flush()?;
    }
}

/// 多选菜单：支持方向键 ↑/↓ 移动光标，Space 切换勾选，Enter 确认，Esc 返回。
pub fn multi_select_menu(title: &str, items: &[(String, bool)]) -> Result<Option<Vec<usize>>> {
    if items.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let mut selected_cursor = 0;
    let mut checked: Vec<bool> = items.iter().map(|(_, c)| *c).collect();

    eprintln!("\n\x1b[1;36m{title}\x1b[0m");
    eprintln!("\x1b[90m(使用 ↑/↓ 移动光标，Space 切换勾选，Enter 确认，Esc 返回/取消)\x1b[0m");

    for (i, (item, _)) in items.iter().enumerate() {
        let mark = if checked[i] {
            "\x1b[32m[x]\x1b[0m"
        } else {
            "\x1b[90m[ ]\x1b[0m"
        };
        if i == selected_cursor {
            eprintln!("  > {} \x1b[1;37m{}\x1b[0m", mark, item);
        } else {
            eprintln!("    {} \x1b[90m{}\x1b[0m", mark, item);
        }
    }
    io::stderr().flush()?;

    let total_lines = items.len();
    let _raw = RawModeGuard::new()?;

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc => {
                drop(_raw);
                eprintln!();
                return Ok(None);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                drop(_raw);
                eprintln!();
                bail!("操作已取消");
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if selected_cursor == 0 {
                    selected_cursor = items.len() - 1;
                } else {
                    selected_cursor -= 1;
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if selected_cursor + 1 >= items.len() {
                    selected_cursor = 0;
                } else {
                    selected_cursor += 1;
                }
            }
            KeyCode::Char(' ') => {
                checked[selected_cursor] = !checked[selected_cursor];
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                let all_checked = checked.iter().all(|c| *c);
                checked.fill(!all_checked);
            }
            KeyCode::Enter => {
                drop(_raw);
                let chosen: Vec<usize> = checked
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &c)| if c { Some(i) } else { None })
                    .collect();
                eprintln!("\x1b[32m  ✓ 已选定 {} 项\x1b[0m", chosen.len());
                return Ok(Some(chosen));
            }
            _ => continue,
        }

        eprint!("\x1b[{}A\r", total_lines);
        for (i, (item, _)) in items.iter().enumerate() {
            let mark = if checked[i] {
                "\x1b[32m[x]\x1b[0m"
            } else {
                "\x1b[90m[ ]\x1b[0m"
            };
            if i == selected_cursor {
                eprintln!("\x1b[2K  > {} \x1b[1;37m{}\x1b[0m", mark, item);
            } else {
                eprintln!("\x1b[2K    {} \x1b[90m{}\x1b[0m", mark, item);
            }
        }
        io::stderr().flush()?;
    }
}

/// 交互式文本输入（带默认值回退与 Esc 取消）。
pub fn prompt_text(label: &str, default: &str) -> Result<Option<String>> {
    if default.is_empty() {
        eprint!("{label}: ");
    } else {
        eprint!("{label} \x1b[90m[{}]\x1b[0m: ", default.escape_default());
    }
    io::stderr().flush()?;

    let _raw = RawModeGuard::new()?;
    let mut input = String::new();

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc => {
                drop(_raw);
                eprintln!();
                return Ok(None);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                drop(_raw);
                eprintln!();
                bail!("操作已取消");
            }
            KeyCode::Enter => {
                drop(_raw);
                eprintln!();
                if input == ":cancel" {
                    return Ok(None);
                }
                let val = if input.trim().is_empty() {
                    default.to_string()
                } else {
                    input.trim().to_string()
                };
                return Ok(Some(val));
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c)
                if !c.is_control() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                input.push(c);
            }
            _ => continue,
        }
        if default.is_empty() {
            eprint!("\r\x1b[2K{label}: {input}");
        } else {
            eprint!(
                "\r\x1b[2K{label} \x1b[90m[{}]\x1b[0m: {input}",
                default.escape_default()
            );
        }
        io::stderr().flush()?;
    }
}

/// 交互式密码/敏感词掩码输入（实时打印星号 `*`）。
pub fn prompt_password(label: &str) -> Result<Option<String>> {
    eprint!("{label}: ");
    io::stderr().flush()?;

    let _raw = RawModeGuard::new()?;
    let mut input = String::new();

    loop {
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc => {
                drop(_raw);
                eprintln!();
                return Ok(None);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                drop(_raw);
                eprintln!();
                bail!("操作已取消");
            }
            KeyCode::Enter => {
                drop(_raw);
                eprintln!();
                if input == ":cancel" {
                    return Ok(None);
                }
                return Ok(Some(input.trim().to_string()));
            }
            KeyCode::Backspace => {
                input.pop();
            }
            KeyCode::Char(c)
                if !c.is_control() && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                input.push(c);
            }
            _ => continue,
        }
        let stars = "*".repeat(input.len());
        eprint!("\r\x1b[2K{label}: {stars}");
        io::stderr().flush()?;
    }
}

#[allow(dead_code)]
fn prompt(label: &str, default: &str, hidden: bool) -> Result<String> {
    if hidden {
        prompt_password(label)?.ok_or_else(|| eyre!("Setup cancelled."))
    } else {
        prompt_text(label, default)?.ok_or_else(|| eyre!("Setup cancelled."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_bypasses_all_configuration_io() {
        ensure_configured(Path::new("nonexistent"), true, false).unwrap();
    }

    #[test]
    fn validates_existing_provider_without_completion_marker() {
        let mut config = Config::default();
        config.agent.default_provider = "ollama".into();
        let mut providers = ProvidersConfig::default_template();
        assert!(configured(&config, &providers));
        providers.providers.get_mut("ollama").unwrap().model.clear();
        assert!(!configured(&config, &providers));
        config.agent.default_provider = "missing".into();
        assert!(!configured(&config, &providers));
        config.agent.default_provider = "openai".into();
        providers.providers.get_mut("openai").unwrap().api_key =
            "${CYBER_SETUP_TEST_UNSET_48172}".into();
        assert!(!configured(&config, &providers));
    }

    #[test]
    fn script_overrides_validate_selected_provider_instead_of_default() {
        let mut config = Config::default();
        let mut providers = ProvidersConfig::default_template();
        providers
            .providers
            .get_mut("openai")
            .unwrap()
            .api_key
            .clear();
        assert!(!configured(&config, &providers));
        apply_run_overrides(&mut config, &mut providers, Some("ollama"), Some("local"));
        assert!(configured(&config, &providers));
        assert_eq!(providers.providers["ollama"].model, "local");
        apply_run_overrides(&mut config, &mut providers, Some("missing"), None);
        assert!(!configured(&config, &providers));
    }

    #[test]
    fn value_updates_preserve_unknown_entries_and_other_providers() {
        let mut config: Value = toml::from_str("extra = 42\n[agent]\nmax_steps = 7").unwrap();
        let mut providers: Value = toml::from_str(
            "custom = true\n[providers.local]\nunknown = 99\n[providers.other]\nmodel = 'keep'",
        )
        .unwrap();
        let provider = ProviderConfig {
            base_url: "http://localhost:11434".into(),
            model: "local-model".into(),
            ..Default::default()
        };
        update_values(&mut config, &mut providers, "local", &provider).unwrap();
        assert_eq!(config["extra"].as_integer(), Some(42));
        assert_eq!(config["agent"]["max_steps"].as_integer(), Some(7));
        assert_eq!(
            providers["providers"]["local"]["unknown"].as_integer(),
            Some(99)
        );
        assert_eq!(
            providers["providers"]["other"]["model"].as_str(),
            Some("keep")
        );
    }

    #[test]
    fn private_write_replaces_existing_file_without_backup_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.toml");
        write_private(&path, b"old").unwrap();
        write_private(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert!(!path.with_extension("toml.bak").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn rejects_unsafe_endpoints_and_invalid_env_names() {
        assert!(valid_endpoint("https://example.com/v1"));
        assert!(!valid_endpoint("https://user:secret@example.com"));
        assert!(!valid_endpoint("file:///tmp/key"));
        assert!(valid_env_name("OPENAI_API_KEY"));
        assert!(!valid_env_name("1KEY"));
        assert!(!valid_env_name("KEY}"));
    }

    #[test]
    fn noninteractive_missing_credentials_points_to_setup_without_marking_complete() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        let mut providers = read_value(&paths.providers_file).unwrap();
        providers["providers"]["openai"]["api_key"] = Value::String(String::new());
        write_private(
            &paths.providers_file,
            toml::to_string(&providers).unwrap().as_bytes(),
        )
        .unwrap();
        let error = ensure_with_paths(&paths, dir.path(), false, run_setup_sync).unwrap_err();
        assert!(error.to_string().contains("cyber setup"));
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
        write_private(&paths.cyber_home.join(STATE_FILE), b"completed = true\n").unwrap();
        assert!(
            ensure_with_paths(&paths, dir.path(), false, run_setup_sync).is_err(),
            "Completion state must not bypass unusable configuration"
        );
    }

    #[test]
    fn project_provider_override_is_respected_without_loading_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        std::fs::create_dir(dir.path().join(".cyber")).unwrap();
        std::fs::write(
            dir.path().join(".cyber/config.toml"),
            "[agent]\ndefault_provider = 'ollama'",
        )
        .unwrap();
        std::fs::write(&paths.mcp_servers_file, "not even valid TOML").unwrap();
        ensure_with_paths(&paths, dir.path(), false, run_setup_sync).unwrap();
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
        std::fs::write(
            dir.path().join(".cyber/config.toml"),
            "[agent]\ndefault_provider = 'missing'",
        )
        .unwrap();
        assert!(ensure_with_paths(&paths, dir.path(), false, run_setup_sync).is_err());
    }

    #[test]
    fn parse_errors_do_not_echo_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("providers.toml");
        std::fs::write(&path, "api_key = 'private-secret' garbage").unwrap();
        let error = read_value(&path).unwrap_err().to_string();
        assert!(!error.contains("private-secret"));
        assert!(error.contains("cyber setup"));
    }

    #[test]
    fn interrupted_save_blocks_valid_configuration_until_setup_is_completed() {
        for failed_write in 0..4 {
            let dir = tempfile::tempdir().unwrap();
            let paths = Paths::at(dir.path().join("global")).unwrap();
            cyber_core::init::ensure_global_init(&paths).unwrap();
            let mut config = read_value(&paths.config_file).unwrap();
            config["agent"]["default_provider"] = Value::String("ollama".into());
            let providers = read_value(&paths.providers_file).unwrap();
            // Start with an already usable, completed setup to also exercise reconfiguration.
            save_setup(&paths, &config, &providers, write_private).unwrap();
            let config_before = std::fs::read(&paths.config_file).unwrap();
            let providers_before = std::fs::read(&paths.providers_file).unwrap();
            let mut writes = 0;
            let result = save_setup(&paths, &config, &providers, |path, data| {
                let current = writes;
                writes += 1;
                if current == failed_write {
                    bail!("injected save failure");
                }
                write_private(path, data)
            });
            assert!(result.is_err());
            let (effective, typed_providers) = effective_config(&paths, dir.path()).unwrap();
            assert!(configured(&effective, &typed_providers));
            if failed_write == 0 {
                // No intent published means no user config changed and the old completion is valid.
                assert_eq!(std::fs::read(&paths.config_file).unwrap(), config_before);
                assert_eq!(
                    std::fs::read(&paths.providers_file).unwrap(),
                    providers_before
                );
                assert!(setup_state(&paths).unwrap() == SetupState::Completed);
                ensure_with_paths(&paths, dir.path(), false, run_setup_sync).unwrap();
                continue;
            }
            assert!(setup_state(&paths).unwrap() == SetupState::InProgress);
            let error = ensure_with_paths(&paths, dir.path(), false, run_setup_sync)
                .unwrap_err()
                .to_string();
            assert!(error.contains("interrupted"));
            assert!(error.contains("cyber setup"));
            // A script override must not bypass the unfinished commit either.
            let error = ensure_run_with_paths(&paths, dir.path(), Some("ollama"), Some("local"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("interrupted"));
            assert!(error.contains("cyber setup"));
            save_setup(&paths, &config, &providers, write_private).unwrap();
            assert!(setup_state(&paths).unwrap() == SetupState::Completed);
            ensure_with_paths(&paths, dir.path(), false, run_setup_sync).unwrap();
            ensure_run_with_paths(&paths, dir.path(), Some("ollama"), Some("local")).unwrap();
        }
    }

    #[test]
    fn in_progress_takes_priority_over_completed_and_invalid_provider_toml() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        write_private(
            &paths.cyber_home.join(STATE_FILE),
            b"in_progress = true\ncompleted = true\n",
        )
        .unwrap();
        std::fs::write(&paths.providers_file, "not valid TOML").unwrap();
        assert!(setup_state(&paths).unwrap() == SetupState::InProgress);
        assert!(ensure_with_paths(&paths, dir.path(), false, run_setup_sync)
            .unwrap_err()
            .to_string()
            .contains("interrupted"));
        assert!(
            ensure_run_with_paths(&paths, dir.path(), Some("ollama"), None)
                .unwrap_err()
                .to_string()
                .contains("interrupted")
        );
    }

    #[test]
    fn interactive_recovery_restarts_setup_and_cancellation_keeps_in_progress() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::at(dir.path().join("global")).unwrap();
        cyber_core::init::ensure_global_init(&paths).unwrap();
        let mut config = read_value(&paths.config_file).unwrap();
        config["agent"]["default_provider"] = Value::String("ollama".into());
        let providers = read_value(&paths.providers_file).unwrap();
        write_private(
            &paths.config_file,
            toml::to_string(&config).unwrap().as_bytes(),
        )
        .unwrap();
        // An existing usable configuration, with no setup history, must never force a wizard.
        ensure_with_paths(&paths, dir.path(), true, |_| panic!("Unexpected setup")).unwrap();
        assert!(!paths.cyber_home.join(STATE_FILE).exists());
        write_private(
            &paths.cyber_home.join(STATE_FILE),
            b"in_progress = true\ncompleted = false\n",
        )
        .unwrap();
        let before = std::fs::read(&paths.config_file).unwrap();
        let error = ensure_with_paths(&paths, dir.path(), true, |_| {
            bail!("Setup cancelled.");
        })
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(setup_state(&paths).unwrap() == SetupState::InProgress);
        assert_eq!(std::fs::read(&paths.config_file).unwrap(), before);
        let mut restarted = false;
        ensure_with_paths(&paths, dir.path(), true, |cwd| {
            assert_eq!(cwd, dir.path());
            restarted = true;
            save_setup(&paths, &config, &providers, write_private)
        })
        .unwrap();
        assert!(restarted);
        assert!(setup_state(&paths).unwrap() == SetupState::Completed);
    }
}
