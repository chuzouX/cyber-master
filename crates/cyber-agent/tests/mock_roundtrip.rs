//! Mock provider 端到端往返：验证 run_stream 串联 prompt/factory/stream/事件转发。

use cyber_agent::{run_stream, AgentEvent};
use cyber_core::{Config, ProjectContext, ProjectFrontmatter, ProvidersConfig};

#[tokio::test]
async fn mock_roundtrip_no_project() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let config = Config::default(); // default_provider = "openai"
    let providers = ProvidersConfig::default_template();
    let handle = tokio::spawn(run_stream(
        config,
        providers,
        None,
        "你好".into(),
        vec![],
        tx,
        true, // mock
    ));

    let mut started = false;
    let mut tokens = String::new();
    let mut got_done = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Started => started = true,
            AgentEvent::Token(t) => tokens.push_str(&t),
            AgentEvent::Done => {
                got_done = true;
                break;
            }
            AgentEvent::Error(m) => panic!("mock 不应产生错误: {m}"),
        }
    }
    assert!(started, "应先收到 Started");
    assert_eq!(tokens, "收到：你好");
    assert!(got_done, "应以 Done 结束");
    handle.await.unwrap();
}

#[tokio::test]
async fn mock_roundtrip_with_history() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let config = Config::default();
    let providers = ProvidersConfig::default_template();
    use cyber_agent::{Message, Role};
    let history = vec![
        Message { role: Role::User, content: "第一句".into() },
        Message { role: Role::Assistant, content: "回复".into() },
    ];
    let handle = tokio::spawn(run_stream(
        config,
        providers,
        None,
        "第二句".into(),
        history,
        tx,
        true,
    ));

    let mut tokens = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Token(t) => tokens.push_str(&t),
            AgentEvent::Done => break,
            AgentEvent::Error(m) => panic!("{m}"),
            AgentEvent::Started => {}
        }
    }
    // mock 只回放最后一条 user 消息
    assert_eq!(tokens, "收到：第二句");
    handle.await.unwrap();
}

#[tokio::test]
async fn mock_roundtrip_with_project_rules() {
    // 验证带项目上下文不破坏流式（rules 注入 prompt，但 mock 忽略 prompt）
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let config = Config::default();
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
    let handle = tokio::spawn(run_stream(
        config,
        providers,
        Some(project),
        "test".into(),
        vec![],
        tx,
        true,
    ));

    let mut tokens = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Token(t) => tokens.push_str(&t),
            AgentEvent::Done => break,
            AgentEvent::Error(m) => panic!("{m}"),
            AgentEvent::Started => {}
        }
    }
    assert_eq!(tokens, "收到：test");
    handle.await.unwrap();
}

#[tokio::test]
async fn run_stream_unknown_provider_sends_error() {
    // default_provider 指向不存在的条目 → AgentEvent::Error（而非 panic）
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
    let mut config = Config::default();
    config.agent.default_provider = "nonexistent".into();
    let providers = ProvidersConfig::default_template();
    let handle = tokio::spawn(run_stream(
        config,
        providers,
        None,
        "hi".into(),
        vec![],
        tx,
        false, // 非 mock，走真实 lookup → 找不到 → Error
    ));

    let mut got_error = false;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::Error(_) => {
                got_error = true;
                break;
            }
            AgentEvent::Started => {}
            AgentEvent::Done => break,
            AgentEvent::Token(_) => panic!("不应有 token"),
        }
    }
    assert!(got_error, "未知 provider 应产生 Error 事件");
    handle.await.unwrap();
}
