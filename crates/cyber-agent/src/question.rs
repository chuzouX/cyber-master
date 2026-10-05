//! 交互式提问模式 (AskUser / Clarify) 核心数据模型与 Broker。
//!
//! 当模型推理中遇到需求模糊、技术分支选型或配置缺失时，通过 `ask_user`
//! 向用户发起提问。`QuestionBroker` 负责在模型调用协程与前端 TUI 交互协程之间
//! 桥接异步请求与响应。

use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, oneshot};

/// 问题中的单个可选项
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QuestionOption {
    /// 选项标题/标签
    pub label: String,
    /// 选项的详细说明、利弊权衡分析
    #[serde(default)]
    pub description: Option<String>,
    /// 是否为模型推荐选项
    #[serde(default)]
    pub recommended: bool,
}

/// 单个问题项
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QuestionItem {
    /// 问题标识符，例如 "auth_type", "database"
    pub id: String,
    /// 问题具体内容
    pub question: String,
    /// 问题所属分类/标签，例如 "技术选型", "数据库"
    #[serde(default)]
    pub header: Option<String>,
    /// 备选项列表
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// 是否允许多选，默认 false（单选）
    #[serde(default)]
    pub multi: bool,
}

/// 用户对单个问题的答复
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QuestionAnswer {
    /// 问题标识符
    pub id: String,
    /// 题目内容
    pub question: String,
    /// 用户选中的选项 label 列表
    pub selected: Vec<String>,
    /// 用户补充的自定义输入文本（若有）
    #[serde(default)]
    pub custom: Option<String>,
}

/// 前端交互提问请求
#[derive(Debug)]
pub struct QuestionRequest {
    /// 请求唯一标识
    pub id: String,
    /// 问题列表（支持多题）
    pub questions: Vec<QuestionItem>,
    /// 答复回传通道
    pub reply: oneshot::Sender<QuestionResponse>,
}

/// 提问响应结果
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct QuestionResponse {
    /// 每道题目的作答情况
    pub answers: Vec<QuestionAnswer>,
    /// 用户是否主动取消（Esc 跳过）
    pub cancelled: bool,
}

/// 请求唯一 ID 生成器
fn question_nonce() -> String {
    static NONCE_SEQ: AtomicU64 = AtomicU64::new(1);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NONCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let state = RandomState::new();
    format!(
        "q-{:016x}{:016x}",
        state.hash_one((time, sequence, 0)),
        state.hash_one((time, sequence, 1))
    )
}

/// 提问中介 (QuestionBroker)
///
/// 具备线程安全与非阻塞通道，支持交互式与无交互 (headless) 两种运行模式：
/// - 交互式：向 UI 投递 `QuestionRequest`，挂起当前协程等待用户选择或输入；
/// - Headless：自动采纳每题推荐项（或首项），绝不死锁或挂起。
pub struct QuestionBroker {
    requests: Mutex<Option<mpsc::UnboundedSender<QuestionRequest>>>,
}

impl std::fmt::Debug for QuestionBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QuestionBroker")
            .field("interactive", &self.is_interactive())
            .finish()
    }
}

impl QuestionBroker {
    /// 创建无 UI 交互的 broker（在非交互模式或测试中使用），自动采纳推荐选项
    pub fn headless() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(None),
        })
    }

    /// 创建交互式 broker，返回 broker 引用以及接收用户提问请求的 channel
    pub fn interactive() -> (Arc<Self>, mpsc::UnboundedReceiver<QuestionRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                requests: Mutex::new(Some(tx)),
            }),
            rx,
        )
    }

    /// 检查当前是否挂载了交互式通道
    pub fn is_interactive(&self) -> bool {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// 切换为 headless 模式（断开交互通道）
    pub fn set_headless(&self) {
        *self.requests.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 挂载交互式发送端
    pub fn attach(&self, tx: mpsc::UnboundedSender<QuestionRequest>) {
        *self.requests.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    }

    /// 向用户发起提问。
    /// 若处于 headless 模式或 channel 断开，则自动采纳推荐选项（不阻塞）。
    /// 若处于交互模式，则挂起当前异步任务等待前端确认。
    pub async fn ask(&self, questions: Vec<QuestionItem>) -> QuestionResponse {
        if questions.is_empty() {
            return QuestionResponse {
                answers: Vec::new(),
                cancelled: false,
            };
        }

        let sender = self
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();

        let Some(requests) = sender else {
            return Self::auto_pick(&questions);
        };

        let (reply, rx) = oneshot::channel();
        let req_id = question_nonce();
        if requests
            .send(QuestionRequest {
                id: req_id,
                questions: questions.clone(),
                reply,
            })
            .is_err()
        {
            return Self::auto_pick(&questions);
        }

        match rx.await {
            Ok(resp) => resp,
            Err(_) => Self::auto_pick(&questions),
        }
    }

    /// 自动采纳推荐选项的策略（若无推荐则取首项，若选项为空则为空）
    pub fn auto_pick(questions: &[QuestionItem]) -> QuestionResponse {
        let answers = questions
            .iter()
            .map(|item| {
                let mut selected = Vec::new();
                let recommended_opts: Vec<&QuestionOption> =
                    item.options.iter().filter(|o| o.recommended).collect();

                if !recommended_opts.is_empty() {
                    if item.multi {
                        selected = recommended_opts.iter().map(|o| o.label.clone()).collect();
                    } else {
                        selected = vec![recommended_opts[0].label.clone()];
                    }
                } else if !item.options.is_empty() {
                    selected = vec![item.options[0].label.clone()];
                }

                QuestionAnswer {
                    id: item.id.clone(),
                    question: item.question.clone(),
                    selected,
                    custom: None,
                }
            })
            .collect();

        QuestionResponse {
            answers,
            cancelled: false,
        }
    }
}
