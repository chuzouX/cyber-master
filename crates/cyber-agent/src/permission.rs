//! Fail-closed, pre-execution permissions shared by non-TUI clients.
//! The legacy TUI `run_stream` entry does not opt into this policy.

use std::collections::HashSet;
use std::hash::{BuildHasher, RandomState};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    AllowOnce,
    /// Only this tool and these exact JSON arguments, until the session changes.
    AllowSession,
    Deny,
}

pub struct PermissionRequest {
    pub tool: String,
    pub arguments: Value,
    /// Bind terminal responses to this request, never to prefetched input.
    pub nonce: String,
    pub reply: oneshot::Sender<PermissionDecision>,
}

impl PermissionRequest {
    pub fn decision_for(&self, response: &str) -> PermissionDecision {
        match response.trim() {
            response if response == format!("once {}", self.nonce) => PermissionDecision::AllowOnce,
            response if response == format!("session {}", self.nonce) => {
                PermissionDecision::AllowSession
            }
            _ => PermissionDecision::Deny,
        }
    }
}

fn request_nonce() -> String {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    // Mix per-request timing with randomly seeded hashes so prefetched input
    // cannot predict the next request token, without a new dependency.
    let state = RandomState::new();
    format!(
        "{:016x}{:016x}",
        state.hash_one((time, sequence, 0)),
        state.hash_one((time, sequence, 1))
    )
}

pub struct PermissionBroker {
    requests: Option<mpsc::UnboundedSender<PermissionRequest>>,
    grants: Mutex<Vec<(String, Value)>>,
    explicit_tools: HashSet<String>,
    denials: AtomicU64,
}

impl PermissionBroker {
    pub fn deny_all() -> Self {
        Self {
            requests: None,
            grants: Mutex::new(Vec::new()),
            explicit_tools: HashSet::new(),
            denials: AtomicU64::new(0),
        }
    }

    /// Authorize exact tool names, with any arguments, solely from explicit
    /// caller policy. Never populate this from model output or project config.
    /// Names have no wildcard/`all` semantics; clients validate their registry.
    /// Tool implementations still enforce their normal guards.
    pub fn explicit_tools(tool_names: impl IntoIterator<Item = impl AsRef<str>>) -> Self {
        Self {
            explicit_tools: tool_names
                .into_iter()
                .map(|name| name.as_ref().to_owned())
                .collect(),
            ..Self::deny_all()
        }
    }

    pub fn interactive() -> (Self, mpsc::UnboundedReceiver<PermissionRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                requests: Some(tx),
                grants: Mutex::new(Vec::new()),
                explicit_tools: HashSet::new(),
                denials: AtomicU64::new(0),
            },
            rx,
        )
    }

    pub fn clear_session(&self) {
        self.grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Monotonic structured signal, distinct from ordinary tool errors.
    pub fn denial_count(&self) -> u64 {
        self.denials.load(Ordering::Relaxed)
    }

    pub async fn authorize(&self, tool: &str, arguments: &Value) -> bool {
        let allowed = self.decide(tool, arguments).await;
        if !allowed {
            self.denials.fetch_add(1, Ordering::Relaxed);
        }
        allowed
    }

    async fn decide(&self, tool: &str, arguments: &Value) -> bool {
        if self.explicit_tools.contains(tool) {
            return true;
        }
        if self
            .grants
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(name, args)| name == tool && args == arguments)
        {
            return true;
        }
        let Some(requests) = &self.requests else {
            return false;
        };
        let (reply, decision) = oneshot::channel();
        if requests
            .send(PermissionRequest {
                tool: tool.to_owned(),
                arguments: arguments.clone(),
                nonce: request_nonce(),
                reply,
            })
            .is_err()
        {
            return false;
        }
        match decision.await.unwrap_or(PermissionDecision::Deny) {
            PermissionDecision::AllowOnce => true,
            PermissionDecision::AllowSession => {
                self.grants
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((tool.to_owned(), arguments.clone()));
                true
            }
            PermissionDecision::Deny => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn session_grant_matches_exact_tool_and_arguments_and_can_be_cleared() {
        let (broker, mut requests) = PermissionBroker::interactive();
        let broker = Arc::new(broker);
        let worker_broker = broker.clone();
        let worker = tokio::spawn(async move {
            worker_broker
                .authorize("shell", &json!({"command": "echo hello"}))
                .await
        });
        requests
            .recv()
            .await
            .unwrap()
            .reply
            .send(PermissionDecision::AllowSession)
            .unwrap();
        assert!(worker.await.unwrap());
        assert!(
            broker
                .authorize("shell", &json!({"command": "echo hello"}))
                .await
        );
        drop(requests);
        assert!(
            !broker
                .authorize("shell", &json!({"command": "echo other"}))
                .await
        );
        assert!(
            !broker
                .authorize("custom", &json!({"command": "echo hello"}))
                .await
        );
        broker.clear_session();
        assert!(
            !broker
                .authorize("shell", &json!({"command": "echo hello"}))
                .await
        );
    }

    #[tokio::test]
    async fn dropped_reply_denies_without_hanging() {
        let (broker, mut requests) = PermissionBroker::interactive();
        let worker = tokio::spawn(async move { broker.authorize("shell", &json!({})).await });
        drop(requests.recv().await.unwrap());
        assert!(!worker.await.unwrap());
    }

    #[test]
    fn approval_requires_exact_current_nonce() {
        let (reply, _) = oneshot::channel();
        let request = PermissionRequest {
            tool: "shell".into(),
            arguments: json!({}),
            nonce: request_nonce(),
            reply,
        };
        for response in [
            "once",
            "session",
            "once stale-nonce",
            "session stale-nonce",
            "deny",
        ] {
            assert_eq!(request.decision_for(response), PermissionDecision::Deny);
        }
        assert_eq!(
            request.decision_for(&format!("once {}", request.nonce)),
            PermissionDecision::AllowOnce
        );
        assert_eq!(
            request.decision_for(&format!("session {}", request.nonce)),
            PermissionDecision::AllowSession
        );
        assert_eq!(
            request.decision_for(&format!("once {} extra", request.nonce)),
            PermissionDecision::Deny
        );
        assert_ne!(request.nonce, request_nonce());
    }

    #[tokio::test]
    async fn denial_is_structured_for_noninteractive_and_closed_ui() {
        let broker = PermissionBroker::deny_all();
        assert_eq!(broker.denial_count(), 0);
        assert!(!broker.authorize("shell", &json!({})).await);
        assert_eq!(broker.denial_count(), 1);
        let (broker, requests) = PermissionBroker::interactive();
        drop(requests);
        assert!(!broker.authorize("shell", &json!({})).await);
        assert_eq!(broker.denial_count(), 1);
    }

    #[tokio::test]
    async fn explicit_tools_authorizes_only_exact_names_for_any_arguments() {
        let broker = PermissionBroker::explicit_tools(["shell", "shell"]);
        assert!(
            broker
                .authorize("shell", &json!({"command": "echo first"}))
                .await
        );
        assert!(
            broker
                .authorize("shell", &json!({"command": "echo second"}))
                .await
        );
        assert_eq!(broker.denial_count(), 0);
        assert!(!broker.authorize("write_file", &json!({})).await);
        assert!(!broker.authorize("Shell", &json!({})).await);
        assert_eq!(broker.denial_count(), 2);
    }

    #[tokio::test]
    async fn empty_explicit_tools_denies_by_default_and_never_expands_patterns() {
        let broker = PermissionBroker::explicit_tools(Vec::<String>::new());
        assert!(!broker.authorize("shell", &json!({})).await);
        let broker = PermissionBroker::explicit_tools(["*", "all", "shell*"]);
        assert!(!broker.authorize("shell", &json!({})).await);
        assert!(!broker.authorize("write_file", &json!({})).await);
    }
}
