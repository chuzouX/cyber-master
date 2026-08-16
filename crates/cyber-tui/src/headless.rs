//! Headless 模式：`cyber run` 非交互式执行一次 agent 任务。
//!
//! 供外部 agent / 脚本通过命令行接管 Cyber Master：不启动 TUI，直接跑一次
//! `run_stream`（含工具调用），结果输出为 text（Markdown）或 JSON（结构化）。
//! 支持会话续接（`--session <id>` 续接既有会话，`--new` 开新会话，默认续接当前）。
//!
//! 复用 TUI 的基础设施：`build_registries`（builtins + Skills + MCP + save_memory）、
//! `history` 模块（session 加载/保存）、`entries_to_messages`（工具链跨轮上下文）。

use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use serde_json::json;
use tracing::warn;

use cyber_agent::{run_stream, AgentEvent, ToolRegistry};
use cyber_core::{load_app_context, MemoryStore, ThinkingIntensity};

use crate::bootstrap::build_registries;
use crate::chat::{entries_to_messages, ChatEntry};
use crate::history::{
    create_session_meta, load_entries, load_index, save_current, save_index,
};

/// `cyber run` 的参数。
#[derive(Debug, Clone)]
pub struct HeadlessArgs {
    /// 任务描述（用户 prompt）。
    pub prompt: String,
    /// 输出格式：`text` | `json`（默认 text）。
    pub format: String,
    /// 续接指定 session id（默认续接当前 session；`--new` 时忽略）。
    pub session: Option<String>,
    /// 新建会话（忽略历史）。
    pub new: bool,
    /// 最大工具调用步数（覆盖 config）。
    pub max_steps: Option<u32>,
    /// 思考强度（覆盖 config）。
    pub think: Option<String>,
    /// 指定 provider（providers.toml 中的名称，覆盖 default_provider）。
    pub provider: Option<String>,
    /// 指定模型 id（覆盖所选 provider 的默认 model）。
    pub model: Option<String>,
}

/// 一次工具调用的记录（JSON 输出用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ToolCallRecord {
    pub name: String,
    pub arguments: String,
    pub output: String,
    pub is_error: bool,
    /// 内部关联 id（不参与 JSON 语义，但保留便于调试）。
    #[serde(skip)]
    pub id: String,
}

/// Headless 执行结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct HeadlessOutcome {
    /// 本次使用的 session id（续接或新建）。
    pub session_id: String,
    /// 最终回答（Markdown 文本）。
    pub answer: String,
    /// 工具调用记录（顺序执行）。
    pub tool_calls: Vec<ToolCallRecord>,
    /// 失败原因（成功为 None）。
    pub error: Option<String>,
}

/// 执行一次 headless agent 任务。返回 outcome；`error` 非空表示任务失败。
pub async fn run_headless(cwd: &Path, args: HeadlessArgs) -> HeadlessOutcome {
    let ctx = match load_app_context(cwd) {
        Ok(c) => c,
        Err(e) => {
            return err_outcome(format!("加载配置失败: {e}"));
        }
    };
    let mock = std::env::var("CYBER_MOCK_PROVIDER").is_ok_and(|v| v == "1");
    let (registries, boot_errors) = build_registries(&ctx.paths, cwd, mock).await;
    for e in &boot_errors {
        warn!(error = %e, "headless 注册表构建警告");
    }

    // 读取两层用户记忆（全局 + 项目级），注入系统提示词
    let memory = MemoryStore::new(
        ctx.paths.memory_file.clone(),
        cwd.join(".cyber").join("memory.md"),
    )
    .load_all();

    // 会话解析：--new 新建；--session <id> 续接指定；默认续接当前
    let mut idx = load_index(&ctx.paths.history_dir, cwd);
    let session_id = if args.new {
        let meta = create_session_meta();
        idx.sessions.push(meta.clone());
        idx.current = meta.id.clone();
        save_index(&ctx.paths.history_dir, cwd, &idx);
        meta.id
    } else if let Some(sid) = &args.session {
        // 续接指定会话：把 current 指向它（后续 TUI 打开也看到该会话）
        idx.current = sid.clone();
        sid.clone()
    } else {
        idx.current.clone()
    };

    let mut entries = if args.new {
        Vec::new()
    } else {
        load_entries(&ctx.paths.history_dir, cwd, &session_id)
    };
    let history = entries_to_messages(&entries);

    // 覆盖配置（max_steps / think / provider / model）
    let mut config = ctx.config;
    let mut providers = ctx.providers;
    if let Some(ms) = args.max_steps {
        config.agent.max_steps = ms;
    }
    if let Some(p) = &args.provider {
        // 校验 provider 存在
        if !providers.providers.contains_key(p) {
            let mut names: Vec<&str> = providers.providers.keys().map(String::as_str).collect();
            names.sort();
            return err_outcome(format!(
                "provider '{p}' 不存在（可用：{}）",
                names.join(", ")
            ));
        }
        config.agent.default_provider = p.clone();
    }
    if let Some(m) = &args.model {
        // 覆盖所选 provider 的默认 model
        if let Some(cfg) = providers.providers.get_mut(&config.agent.default_provider) {
            cfg.model = m.clone();
        }
    }
    let intensity = args
        .think
        .as_deref()
        .and_then(|t| ThinkingIntensity::from_str(t.trim()))
        .unwrap_or(config.agent.thinking_intensity);

    // 驱动 agent 任务并收集事件
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
    let gen = 0u64;
    let registry: Arc<ToolRegistry> = registries.tools.clone();
    let handle = tokio::spawn(run_stream(
        config,
        providers,
        ctx.project,
        args.prompt.clone(),
        history,
        tx,
        gen,
        mock,
        cwd.to_path_buf(),
        registry,
        false, // ctf_enabled（headless 无 CTF 面板，ctf_challenge 工具仍可用）
        intensity,
        memory,
    ));

    let mut answer = String::new();
    let mut tool_calls: Vec<ToolCallRecord> = Vec::new();
    let mut error: Option<String> = None;

    while let Some((_g, ev)) = rx.recv().await {
        match ev {
            AgentEvent::Token(t) => {
                answer.push_str(&t);
                // text 模式：流式输出到 stdout，外部 agent 可实时读取
                if args.format == "text" {
                    print!("{t}");
                    let _ = std::io::stdout().flush();
                }
            }
            AgentEvent::Reasoning(t) => {
                // 思考过程：text 模式输出到 stderr（不污染 stdout 的最终回答）
                if args.format == "text" {
                    eprint!("{t}");
                }
            }
            AgentEvent::ToolCall { id, name, arguments } => {
                tool_calls.push(ToolCallRecord {
                    id,
                    name,
                    arguments,
                    output: String::new(),
                    is_error: false,
                });
            }
            AgentEvent::ToolResult { id, name, output, is_error } => {
                // 按工具调用 id 关联结果（工具串行执行，id 唯一）
                if let Some(rec) = tool_calls.iter_mut().find(|r| r.id == id) {
                    rec.output = output.clone();
                    rec.is_error = is_error;
                } else {
                    tool_calls.push(ToolCallRecord {
                        id,
                        name,
                        arguments: String::new(),
                        output,
                        is_error,
                    });
                }
                // text 模式：工具输出摘要到 stderr（stdout 只保留最终回答）
                if args.format == "text" {
                    eprintln!();
                }
            }
            AgentEvent::Error(m) => {
                error = Some(m.clone());
                if args.format == "text" {
                    eprintln!("\n[error] {m}");
                }
            }
            AgentEvent::Done => break,
            _ => {}
        }
    }
    let _ = handle.await;

    // 持久化会话：追加本次 user prompt + 工具链 + assistant 回答
    entries.push(ChatEntry::User(args.prompt));
    for tc in &tool_calls {
        entries.push(ChatEntry::ToolCall {
            id: tc.id.clone(),
            name: tc.name.clone(),
            arguments: tc.arguments.clone(),
        });
        entries.push(ChatEntry::ToolResult {
            id: tc.id.clone(),
            name: tc.name.clone(),
            output: tc.output.clone(),
            is_error: tc.is_error,
        });
    }
    if !answer.trim().is_empty() {
        entries.push(ChatEntry::Assistant(answer.clone()));
    }
    save_current(&ctx.paths.history_dir, cwd, &mut idx, &entries);

    HeadlessOutcome {
        session_id,
        answer,
        tool_calls,
        error,
    }
}

/// 失败时的 outcome（session_id 空、无工具调用）。
fn err_outcome(msg: String) -> HeadlessOutcome {
    HeadlessOutcome {
        session_id: String::new(),
        answer: String::new(),
        tool_calls: Vec::new(),
        error: Some(msg),
    }
}

/// 把 outcome 序列化为 JSON 字符串（供 `--format json`）。
pub fn outcome_to_json(o: &HeadlessOutcome) -> String {
    let tool_calls: Vec<serde_json::Value> = o
        .tool_calls
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "arguments": t.arguments,
                "output": t.output,
                "is_error": t.is_error,
            })
        })
        .collect();
    json!({
        "session_id": o.session_id,
        "success": o.error.is_none(),
        "answer": o.answer,
        "tool_calls": tool_calls,
        "error": o.error,
    })
    .to_string()
}
