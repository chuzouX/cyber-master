//! Mock provider 端到端往返：验证 run_stream 串联 prompt/factory/stream/事件转发。
//!
//! P2.2：通道类型改为 `(u64, AgentEvent)`（generation 计数器），run_stream 增加
//! `gen` + `cwd` 两个参数。本测试固定 `gen=0`、`cwd=tempdir()`。
//!
//! echo 测试（前 4 个）设 `auto_tool_call=false` 以保持 echo 模式；
//! `mock_tool_loop_roundtrip` 用默认 `auto_tool_call=true` 验证 agent loop 全链路。

use std::sync::Arc;

use cyber_agent::{run_stream, AgentEvent, ToolRegistry};
use cyber_core::{Config, ProjectContext, ProjectFrontmatter, ProvidersConfig};
use tokio::sync::mpsc::UnboundedReceiver;

/// 固定 generation 计数器与临时工作目录，启动一次 run_stream。
/// 工具表统一用内置工具（read_file/write_file/list_dir/shell），与 P2 行为一致。
fn spawn_run(
    config: Config,
    providers: ProvidersConfig,
    project: Option<ProjectContext>,
    user_input: &str,
    history: Vec<cyber_agent::Message>,
    mock: bool,
) -> (
    tokio::task::JoinHandle<()>,
    UnboundedReceiver<(u64, AgentEvent)>,
) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
    let cwd = std::env::temp_dir();
    let registry = Arc::new(ToolRegistry::with_builtins());
    let handle = tokio::spawn(run_stream(
        config,
        providers,
        project,
        user_input.into(),
        history,
        tx,
        0, // gen
        mock,
        cwd,
        registry,
        false,    // ctf_enabled
        cyber_core::ThinkingIntensity::Middle,
    ));
    (handle, rx)
}

/// echo 模式配置：关闭 auto_tool_call，使 mock 走逐字符 echo 路径。
fn echo_config() -> Config {
    let mut c = Config::default();
    c.agent.auto_tool_call = false;
    c
}

#[tokio::test]
async fn mock_roundtrip_no_project() {
    let config = echo_config();
    let providers = ProvidersConfig::default_template();
    let (handle, mut rx) = spawn_run(config, providers, None, "你好", vec![], true);

    let mut started = false;
    let mut tokens = String::new();
    let mut got_done = false;
    while let Some((_, ev)) = rx.recv().await {
        match ev {
            AgentEvent::Started => started = true,
            AgentEvent::Token(t) => tokens.push_str(&t),
            AgentEvent::Done => {
                got_done = true;
                break;
            }
            AgentEvent::Error(m) => panic!("mock 不应产生错误: {m}"),
            _ => {}
        }
    }
    assert!(started, "应先收到 Started");
    assert_eq!(tokens, "收到：你好");
    assert!(got_done, "应以 Done 结束");
    handle.await.unwrap();
}

#[tokio::test]
async fn mock_roundtrip_with_history() {
    let config = echo_config();
    let providers = ProvidersConfig::default_template();
    use cyber_agent::Message;
    let history = vec![Message::user("第一句"), Message::assistant("回复")];
    let (handle, mut rx) = spawn_run(config, providers, None, "第二句", history, true);

    let mut tokens = String::new();
    while let Some((_, ev)) = rx.recv().await {
        match ev {
            AgentEvent::Token(t) => tokens.push_str(&t),
            AgentEvent::Done => break,
            AgentEvent::Error(m) => panic!("{m}"),
            AgentEvent::Started => {}
            _ => {}
        }
    }
    // mock 只回放最后一条 user 消息
    assert_eq!(tokens, "收到：第二句");
    handle.await.unwrap();
}

#[tokio::test]
async fn mock_roundtrip_with_project_rules() {
    // 验证带项目上下文不破坏流式（rules 注入 prompt，但 mock 忽略 prompt）
    let config = echo_config();
    let providers = ProvidersConfig::default_template();
    let project = ProjectContext {
        frontmatter: ProjectFrontmatter {
            project: Some("demo".into()),
            scope: Some("*.example.com".into()),
            authorization: None,
            owner: None,
            rules: vec!["禁止 DoS".into()],
        },
        body: String::new(),
        raw: String::new(),
        path: std::path::PathBuf::new(),
    };
    let (handle, mut rx) = spawn_run(config, providers, Some(project), "test", vec![], true);

    let mut tokens = String::new();
    while let Some((_, ev)) = rx.recv().await {
        match ev {
            AgentEvent::Token(t) => tokens.push_str(&t),
            AgentEvent::Done => break,
            AgentEvent::Error(m) => panic!("{m}"),
            AgentEvent::Started => {}
            _ => {}
        }
    }
    assert_eq!(tokens, "收到：test");
    handle.await.unwrap();
}

#[tokio::test]
async fn run_stream_unknown_provider_sends_error() {
    // default_provider 指向不存在的条目 → AgentEvent::Error（而非 panic）
    let mut config = Config::default();
    config.agent.default_provider = "nonexistent".into();
    let providers = ProvidersConfig::default_template();
    let (handle, mut rx) = spawn_run(config, providers, None, "hi", vec![], false);

    let mut got_error = false;
    while let Some((_, ev)) = rx.recv().await {
        match ev {
            AgentEvent::Error(_) => {
                got_error = true;
                break;
            }
            AgentEvent::Started => {}
            AgentEvent::Done => break,
            AgentEvent::Token(_) => panic!("不应有 token"),
            _ => {}
        }
    }
    assert!(got_error, "未知 provider 应产生 Error 事件");
    handle.await.unwrap();
}

/// 验证 agent loop 全链路：mock 第一步发文本 + list_dir 工具调用 → agent 执行
/// list_dir → 工具结果回灌 → mock 第二步发最终文本 → Done。
#[tokio::test]
async fn mock_tool_loop_roundtrip() {
    // 默认 config：auto_tool_call=true，触发 mock 的 tool-loop 模式
    let config = Config::default();
    let providers = ProvidersConfig::default_template();
    let (handle, mut rx) = spawn_run(config, providers, None, "查看目录", vec![], true);

    let mut started = false;
    let mut first_text = String::new();
    let mut tool_call_seen = false;
    let mut tool_result_seen = false;
    let mut final_text = String::new();
    let mut got_done = false;

    while let Some((_, ev)) = rx.recv().await {
        match ev {
            AgentEvent::Started => started = true,
            AgentEvent::Token(t) => {
                if !tool_call_seen {
                    first_text.push_str(&t);
                } else {
                    final_text.push_str(&t);
                }
            }
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
            } => {
                tool_call_seen = true;
                assert_eq!(name, "list_dir", "应为 list_dir 工具调用");
                assert_eq!(arguments, "{\"path\":\".\"}");
                assert!(!id.is_empty(), "工具调用 id 应非空");
            }
            AgentEvent::ToolResult {
                is_error, output, ..
            } => {
                tool_result_seen = true;
                assert!(!is_error, "list_dir 不应失败");
                // output 是 tempdir 的内容（可能为空字符串，但不应是错误信息）
                assert!(
                    !output.contains("护栏") && !output.contains("错误"),
                    "工具结果不应是错误: {output}"
                );
            }
            AgentEvent::Done => {
                got_done = true;
                break;
            }
            AgentEvent::Usage(_) => {}
            AgentEvent::ContextUpdate { .. }
            | AgentEvent::Compacting { .. }
            | AgentEvent::Compacted { .. } => {}
            AgentEvent::Reasoning(_) => {}
            AgentEvent::ToolProgress { .. } => {}
            AgentEvent::Error(m) => panic!("tool-loop 不应产生错误: {m}"),
        }
    }

    assert!(started, "应先收到 Started");
    assert!(
        !first_text.is_empty(),
        "第一步应先发文本（tool call 前导）"
    );
    assert!(tool_call_seen, "应收到 list_dir 工具调用");
    assert!(tool_result_seen, "应收到工具结果");
    assert!(
        !final_text.is_empty(),
        "第二步应发最终文本（工具结果回灌后）"
    );
    assert!(got_done, "应以 Done 结束");
    handle.await.unwrap();
}

/// 验证正常两步收敛的 tool-loop **不会误触发死循环检测**：
/// mock 第一步发 list_dir，第二步收敛发最终文本。仅 1 轮工具调用 →
/// LoopDetector repeat_count=1 < 3 → 不触发。最终文本应为 mock 正常完成文案，
/// 不含「陷入循环」提示。
#[tokio::test]
async fn mock_tool_loop_not_loop_detected() {
    let config = Config::default();
    let providers = ProvidersConfig::default_template();
    let (handle, mut rx) = spawn_run(config, providers, None, "查看目录", vec![], true);

    let mut final_text = String::new();
    let mut tool_call_count = 0;
    let mut got_done = false;

    while let Some((_, ev)) = rx.recv().await {
        match ev {
            AgentEvent::Started => {}
            AgentEvent::Token(t) => final_text.push_str(&t),
            AgentEvent::ToolCall { .. } => tool_call_count += 1,
            AgentEvent::ToolResult { .. } => {}
            AgentEvent::Done => {
                got_done = true;
                break;
            }
            AgentEvent::Usage(_) => {}
            AgentEvent::ContextUpdate { .. }
            | AgentEvent::Compacting { .. }
            | AgentEvent::Compacted { .. } => {}
            AgentEvent::Reasoning(_) => {}
            AgentEvent::ToolProgress { .. } => {}
            AgentEvent::Error(m) => panic!("不应产生错误: {m}"),
        }
    }

    assert_eq!(tool_call_count, 1, "正常 tool-loop 应仅 1 次工具调用");
    assert!(
        !final_text.contains("陷入循环"),
        "正常收敛不应触发死循环提示: {final_text}"
    );
    assert!(
        final_text.contains("任务完成"),
        "应为 mock 正常完成文案: {final_text}"
    );
    assert!(got_done);
    handle.await.unwrap();
}

/// 验证 max_steps 耗尽时走**优雅收尾**而非直接报错：max_steps=1 使 step 0（含工具调用）
/// 后循环即耗尽 → 追加「步数上限」user 提示 + 无工具收尾流式（mock echo 模式回放该提示）
/// → 发 `Done`（非 `Error`）。断言：有工具调用/结果、有收尾总结文本（含「步数上限」）、
/// 以 `Done` 结束、无 `Error`。
#[tokio::test]
async fn mock_max_steps_exhaustion_does_graceful_summary() {
    let mut config = Config::default();
    config.agent.max_steps = 1; // step 0 有工具调用 → 循环耗尽 → 收尾总结
    // auto_tool_call 保持默认 true（tools 非空 → mock tool-loop 第一步发工具调用）
    let providers = ProvidersConfig::default_template();
    let (handle, mut rx) = spawn_run(config, providers, None, "查看目录", vec![], true);

    let mut tool_call_seen = false;
    let mut tool_result_seen = false;
    let mut summary_text = String::new();
    let mut got_done = false;

    while let Some((_, ev)) = rx.recv().await {
        match ev {
            AgentEvent::Started => {}
            AgentEvent::Token(t) => {
                if tool_result_seen {
                    summary_text.push_str(&t); // 收尾总结文本
                }
            }
            AgentEvent::ToolCall { name, .. } => {
                tool_call_seen = true;
                assert_eq!(name, "list_dir");
            }
            AgentEvent::ToolResult { .. } => {
                tool_result_seen = true;
            }
            AgentEvent::Done => {
                got_done = true;
                break;
            }
            AgentEvent::Usage(_) => {}
            AgentEvent::ContextUpdate { .. }
            | AgentEvent::Compacting { .. }
            | AgentEvent::Compacted { .. } => {}
            AgentEvent::Reasoning(_) => {}
            AgentEvent::ToolProgress { .. } => {}
            AgentEvent::Error(m) => panic!("max_steps 耗尽不应产生 Error（应优雅收尾）: {m}"),
        }
    }

    assert!(tool_call_seen, "step 0 应有工具调用");
    assert!(tool_result_seen, "应有工具结果回灌");
    assert!(
        !summary_text.is_empty(),
        "耗尽后应有收尾总结文本（非裸中断）"
    );
    assert!(
        summary_text.contains("步数上限"),
        "收尾总结应含步数上限提示: {summary_text}"
    );
    assert!(got_done, "应以 Done 结束（非 Error）");
    handle.await.unwrap();
}

/// 验证 cancel（generation bump）后 stale 事件被隔离：启动任务后立即以更高 gen
/// 发送的事件不应影响断言。此测试通过确认正常 tool-loop 完成来间接验证 gen 路径。
#[tokio::test]
async fn mock_tool_loop_respects_generation_tag() {
    let config = Config::default();
    let providers = ProvidersConfig::default_template();
    // 使用 gen=5 启动，事件应全部携带 gen=5
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, AgentEvent)>();
    let cwd = std::env::temp_dir();
    let registry = Arc::new(ToolRegistry::with_builtins());
    let handle = tokio::spawn(run_stream(
        config,
        providers,
        None,
        "查看目录".into(),
        vec![],
        tx,
        5, // gen=5
        true,
        cwd,
        registry,
        false,    // ctf_enabled
        cyber_core::ThinkingIntensity::Middle,
    ));

    let mut gens = Vec::new();
    while let Some((gen, ev)) = rx.recv().await {
        gens.push(gen);
        if matches!(ev, AgentEvent::Done | AgentEvent::Error(_)) {
            break;
        }
    }
    // 所有事件应携带 gen=5
    assert!(
        gens.iter().all(|&g| g == 5),
        "所有事件应携带 gen=5，实际: {gens:?}"
    );
    handle.await.unwrap();
}
