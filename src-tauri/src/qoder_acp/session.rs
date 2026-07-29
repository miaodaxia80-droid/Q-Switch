//! Session state management for the Qoder ACP agent.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};

/// Per-session state tracked by the ACP handler.
pub struct AcpSession {
    pub selected_model_id: Option<String>,
    pub selected_mode_id: Option<String>,
    /// Cancellation sender — drops to signal the in-flight chat request.
    pub cancel_tx: Option<oneshot::Sender<()>>,
    /// Accumulated message history for multi-turn conversations.
    pub messages: Vec<ChatMessage>,
}

/// A single message in the conversation history.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Tool call IDs emitted by the assistant (if any).
    pub tool_calls: Vec<ToolCallEntry>,
    /// Tool call ID this message is a result for (role = "tool").
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolCallEntry {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

impl AcpSession {
    pub fn new() -> Self {
        Self {
            selected_model_id: None,
            selected_mode_id: None,
            cancel_tx: None,
            messages: Vec::new(),
        }
    }
}

impl Default for AcpSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe session store keyed by session ID.
#[derive(Clone)]
pub struct SessionStore {
    sessions: Arc<Mutex<HashMap<String, AcpSession>>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn create(&self, session_id: &str) {
        self.sessions
            .lock()
            .await
            .insert(session_id.to_string(), AcpSession::new());
    }

    /// Create a local shadow session only when needed. The transparent proxy
    /// lets Qoder's native Agent create the public session ID first; a Q
    /// Switch model can then use that same ID without replacing native state.
    pub async fn ensure(&self, session_id: &str) {
        self.sessions
            .lock()
            .await
            .entry(session_id.to_string())
            .or_insert_with(AcpSession::new);
    }

    pub async fn exists(&self, session_id: &str) -> bool {
        self.sessions.lock().await.contains_key(session_id)
    }

    pub async fn set_model(&self, session_id: &str, model_id: &str) -> bool {
        let mut guard = self.sessions.lock().await;
        if let Some(session) = guard.get_mut(session_id) {
            session.selected_model_id = Some(model_id.to_string());
            true
        } else {
            false
        }
    }

    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> bool {
        let mut guard = self.sessions.lock().await;
        if let Some(session) = guard.get_mut(session_id) {
            session.selected_mode_id = Some(mode_id.to_string());
            true
        } else {
            false
        }
    }

    pub async fn get_model(&self, session_id: &str) -> Option<String> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .and_then(|s| s.selected_model_id.clone())
    }

    /// Set the cancellation sender for the session's in-flight request.
    pub async fn set_cancel_tx(&self, session_id: &str, tx: oneshot::Sender<()>) {
        let mut guard = self.sessions.lock().await;
        if let Some(session) = guard.get_mut(session_id) {
            session.cancel_tx = Some(tx);
        }
    }

    /// Cancel the in-flight request for a session (if any).
    pub async fn cancel(&self, session_id: &str) -> bool {
        let mut guard = self.sessions.lock().await;
        if let Some(session) = guard.get_mut(session_id) {
            if let Some(tx) = session.cancel_tx.take() {
                let _ = tx.send(());
                return true;
            }
        }
        false
    }

    /// Clear the cancellation sender (after request completes).
    pub async fn clear_cancel_tx(&self, session_id: &str) {
        let mut guard = self.sessions.lock().await;
        if let Some(session) = guard.get_mut(session_id) {
            session.cancel_tx = None;
        }
    }

    /// Append a message to the session's history.
    pub async fn append_message(&self, session_id: &str, msg: ChatMessage) {
        let mut guard = self.sessions.lock().await;
        if let Some(session) = guard.get_mut(session_id) {
            session.messages.push(msg);
        }
    }

    /// Get a snapshot of the session's messages.
    pub async fn messages(&self, session_id: &str) -> Vec<ChatMessage> {
        self.sessions
            .lock()
            .await
            .get(session_id)
            .map(|s| s.messages.clone())
            .unwrap_or_default()
    }

    /// Remove a session.
    pub async fn remove(&self, session_id: &str) {
        let mut guard = self.sessions.lock().await;
        // Cancel any in-flight request before removing.
        if let Some(session) = guard.get_mut(session_id) {
            if let Some(tx) = session.cancel_tx.take() {
                let _ = tx.send(());
            }
        }
        guard.remove(session_id);
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}
