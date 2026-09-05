//! ACP protocol handler — dispatches JSON-RPC methods from Qoder's UI.
//!
//! Handles `initialize`, `initialized`, `ping`, `session/new`,
//! `session/set_model`, `session/set_mode`, `session/prompt`, and
//! `session/cancel`.

use crate::database::Database;
use crate::qoder_acp::chat_client::{self, ChatRoute, StreamEvent};
use crate::qoder_acp::mcp_runtime::McpRuntime;
use crate::qoder_acp::session::{ChatMessage, SessionStore, ToolCallEntry};
use crate::qoder_acp::tools;
use crate::qoder_acp::wire;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};
use uuid::Uuid;

/// Shared state for the ACP handler.
pub struct AcpHandlerState {
    pub db: Arc<Database>,
    pub sessions: SessionStore,
    pub mcp_runtime: Arc<McpRuntime>,
    workspace_root: Arc<RwLock<Option<PathBuf>>>,
}

impl AcpHandlerState {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            mcp_runtime: Arc::new(McpRuntime::new(db.clone())),
            db,
            sessions: SessionStore::new(),
            workspace_root: Arc::new(RwLock::new(None)),
        }
    }

    /// Learn the workspace from normal LSP/ACP client traffic.  This only
    /// records a local path shape; it never logs workspace content.
    pub async fn observe_workspace_message(&self, message: &Value) {
        let Some(params) = message.get("params") else {
            return;
        };
        let Some(candidate) = workspace_candidate(params) else {
            return;
        };
        let Ok(canonical) = candidate.canonicalize() else {
            return;
        };
        if !canonical.is_dir() {
            return;
        }
        let mut root = self.workspace_root.write().await;
        if root.as_ref() != Some(&canonical) {
            log::info!("[qoder_adapter] observed Qoder workspace root");
            *root = Some(canonical);
        }
    }

    pub async fn workspace_root(&self) -> Option<PathBuf> {
        self.workspace_root.read().await.clone()
    }
}

fn workspace_candidate(params: &Value) -> Option<PathBuf> {
    for key in [
        "rootUri",
        "rootPath",
        "workspaceRoot",
        "workspacePath",
        "cwd",
        "workDir",
    ] {
        if let Some(path) = params
            .get(key)
            .and_then(Value::as_str)
            .and_then(local_path_from_value)
        {
            return Some(path);
        }
    }
    for key in ["workspaceFolders", "folders"] {
        let Some(folders) = params.get(key).and_then(Value::as_array) else {
            continue;
        };
        for folder in folders {
            for field in ["uri", "path"] {
                if let Some(path) = folder
                    .get(field)
                    .and_then(Value::as_str)
                    .and_then(local_path_from_value)
                {
                    return Some(path);
                }
            }
        }
    }
    None
}

fn local_path_from_value(value: &str) -> Option<PathBuf> {
    if let Ok(url) = url::Url::parse(value) {
        return (url.scheme() == "file")
            .then(|| url.to_file_path().ok())
            .flatten();
    }
    (!value.is_empty()).then(|| PathBuf::from(value))
}

#[cfg(test)]
mod workspace_tests {
    use super::*;

    #[test]
    fn workspace_candidate_accepts_file_uri_from_lsp_initialize() {
        let params = json!({
            "workspaceFolders": [{"uri": "file:///tmp/qoder%20workspace", "name": "qoder workspace"}]
        });
        assert_eq!(
            workspace_candidate(&params),
            Some(PathBuf::from("/tmp/qoder workspace"))
        );
    }

    #[test]
    fn workspace_candidate_ignores_non_file_uri() {
        let params = json!({"rootUri": "vscode-remote://ssh-remote/example"});
        assert_eq!(workspace_candidate(&params), None);
    }
}

/// Process a single JSON-RPC message and send the response via `tx`.
pub async fn handle_message(
    state: &AcpHandlerState,
    message: Value,
    tx: &mpsc::UnboundedSender<Vec<u8>>,
) {
    let method = message.get("method").and_then(|v| v.as_str());
    let id = message.get("id").cloned();
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

    let method = match method {
        Some(m) => m,
        None => return,
    };

    match method {
        "initialize" => {
            wire::send_result(tx, &id, initialize_result());
        }
        "initialized" => {
            // No-op notification ack.
        }
        "ping" => {
            wire::send_result(tx, &id, json!({"success": true}));
        }
        "session/new" => {
            let session_id = Uuid::new_v4().to_string();
            state.sessions.create(&session_id).await;
            wire::send_result(tx, &id, json!({"sessionId": session_id}));
        }
        "session/set_model" => {
            let session_id = params.get("sessionId").and_then(|v| v.as_str());
            let model_id = params.get("modelId").and_then(|v| v.as_str());
            match (session_id, model_id) {
                (Some(sid), Some(mid)) => {
                    if state.sessions.set_model(sid, mid).await {
                        wire::send_result(tx, &id, json!({}));
                    } else {
                        wire::send_error(tx, &id, -32001, "Unknown session.");
                    }
                }
                _ => {
                    wire::send_error(tx, &id, -32602, "Invalid params.");
                }
            }
        }
        "session/set_mode" => {
            let session_id = params.get("sessionId").and_then(|v| v.as_str());
            let mode_id = params.get("modeId").and_then(|v| v.as_str());
            match (session_id, mode_id) {
                (Some(sid), Some(mid)) => {
                    if state.sessions.set_mode(sid, mid).await {
                        wire::send_result(tx, &id, json!({}));
                    } else {
                        wire::send_error(tx, &id, -32001, "Unknown session.");
                    }
                }
                _ => {
                    wire::send_error(tx, &id, -32602, "Invalid params.");
                }
            }
        }
        "session/prompt" => {
            handle_prompt(state, &params, &id, tx).await;
        }
        "session/cancel" => {
            let session_id = params.get("sessionId").and_then(|v| v.as_str());
            match session_id {
                Some(sid) => {
                    state.sessions.cancel(sid).await;
                    wire::send_result(tx, &id, json!({}));
                }
                None => {
                    wire::send_error(tx, &id, -32602, "Invalid params.");
                }
            }
        }
        "session/close" => {
            // Tear down the ACP session and drop any Anthropic signed-thinking
            // bridge state bound to it. Responses call ids are carried in the
            // canonical Chat history and need no separate cache.
            if let Some(sid) = params.get("sessionId").and_then(|v| v.as_str()) {
                state.sessions.remove(sid).await;
                crate::proxy::providers::qoder_wire::bridge_drop_session(sid);
                log::info!("[QoderACP] session/close cleaned session {sid}");
            }
            wire::send_result(tx, &id, json!({}));
        }
        _ => {
            if id.is_some() {
                wire::send_error(tx, &id, -32601, &format!("Method not found: {method}"));
            }
        }
    }
}

/// Handle `session/prompt` — the core chat completion flow.
async fn handle_prompt(
    state: &AcpHandlerState,
    params: &Value,
    id: &Option<Value>,
    tx: &mpsc::UnboundedSender<Vec<u8>>,
) {
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(sid) => sid.to_string(),
        None => {
            wire::send_error(tx, id, -32602, "Invalid params: missing sessionId.");
            return;
        }
    };

    let meta = params.get("_meta").cloned().unwrap_or_default();
    let request_id = meta
        .get("ai-coding/request-id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if request_id.is_empty() {
        wire::send_error(
            tx,
            id,
            -32602,
            "Invalid params: missing _meta['ai-coding/request-id'].",
        );
        return;
    }

    // Check session exists.
    if !state.sessions.exists(&session_id).await {
        wire::send_error(tx, id, -32001, "Unknown session.");
        return;
    }

    // Extract prompt text from the blocks array.
    let prompt_text = extract_prompt_text(params);
    if prompt_text.is_empty() {
        wire::send_error(tx, id, -32602, "Invalid params: no text in prompt blocks.");
        return;
    }

    // Append the user's message to the session history.
    state
        .sessions
        .append_message(
            &session_id,
            ChatMessage {
                role: "user".to_string(),
                content: prompt_text.clone(),
                tool_calls: Vec::new(),
                tool_call_id: None,
            },
        )
        .await;

    // Resolve the model hint from the session or _meta.
    let model_hint = state.sessions.get_model(&session_id).await.or_else(|| {
        meta.get("ai-coding/model")
            .and_then(|v| v.as_str())
            .map(String::from)
    });

    // Resolve the opaque Q Switch model through the local gateway.  The
    // adapter deliberately has no upstream credential and cannot silently
    // fall back to a direct provider request.
    let route = match ChatRoute::from_qswitch_gateway(&state.db, model_hint.as_deref()).await {
        Ok(r) => r,
        Err(e) => {
            wire::send_finish(tx, &session_id, &request_id, &e.to_string(), 500);
            wire::send_error(tx, id, -32000, &e.to_string());
            return;
        }
    };

    // A custom-model tool call is a multi-turn operation: model -> tool ->
    // result -> model.  The native Qoder Agent cannot execute it because this
    // mapped session deliberately bypasses native inference, so own the small
    // capability-controlled loop here.
    const MAX_TOOL_ROUNDS: usize = 8;
    let tool_policy = match crate::qoder_config::get_qoder_tool_policy(&state.db) {
        Ok(policy) => policy,
        Err(error) => {
            let message = format!("Cannot load Q Switch Qoder tool policy: {error}");
            wire::send_finish(tx, &session_id, &request_id, &message, 500);
            wire::send_error(tx, id, -32000, &message);
            return;
        }
    };
    let mcp_tools = if tool_policy.allow_mcp {
        state.mcp_runtime.load_tools().await
    } else {
        Vec::new()
    };
    let tool_definitions = tools::definitions(&tool_policy, &mcp_tools);

    for round in 0..MAX_TOOL_ROUNDS {
        let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
        state.sessions.set_cancel_tx(&session_id, cancel_tx).await;

        let messages = state.sessions.messages(&session_id).await;
        let route_for_stream = route.clone();
        let tools_for_stream = tool_definitions.clone();
        let stream_session_id = session_id.clone();
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<StreamEvent>();
        let stream_handle = tokio::spawn(async move {
            chat_client::stream_chat_completion(
                &route_for_stream,
                &messages,
                &tools_for_stream,
                event_tx,
                cancel_rx,
                Some(&stream_session_id),
            )
            .await
        });

        // Text and reasoning are rendered immediately.  Tool deltas are
        // accumulated by the chat client; after their JSON arguments are
        // complete we emit one stable Qoder tool lifecycle instead of sending
        // partial, uncorrelated argument chunks to the UI.
        while let Some(event) = event_rx.recv().await {
            match event {
                StreamEvent::TextDelta(text) => {
                    wire::send_text_chunk(tx, &session_id, &request_id, &text);
                }
                StreamEvent::ReasoningDelta(text) => {
                    wire::send_thought_chunk(tx, &session_id, &request_id, &text);
                }
                StreamEvent::ToolCallDelta => {}
                StreamEvent::FinishReason(reason) => {
                    log::debug!("[qoder_acp] finish_reason: {reason}");
                }
                StreamEvent::Done => {}
            }
        }

        let result = stream_handle.await;
        state.sessions.clear_cancel_tx(&session_id).await;

        let mut stream_result = match result {
            Ok(Ok(stream_result)) => stream_result,
            Ok(Err(error)) => {
                let msg = error.to_string();
                wire::send_finish(tx, &session_id, &request_id, &msg, 500);
                wire::send_error(tx, id, -32000, &msg);
                return;
            }
            Err(error) => {
                let msg = format!("Stream task panicked: {error}");
                wire::send_finish(tx, &session_id, &request_id, &msg, 500);
                wire::send_error(tx, id, -32000, &msg);
                return;
            }
        };

        for (index, call) in stream_result.tool_calls.iter_mut().enumerate() {
            if call.id.trim().is_empty() {
                call.id = format!("qswitch-{request_id}-{round}-{index}");
            }
        }
        let tool_calls: Vec<ToolCallEntry> = stream_result
            .tool_calls
            .iter()
            .map(|call| ToolCallEntry {
                id: call.id.clone(),
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            })
            .collect();
        state
            .sessions
            .append_message(
                &session_id,
                ChatMessage {
                    role: "assistant".to_string(),
                    content: stream_result.text.clone(),
                    tool_calls,
                    tool_call_id: None,
                },
            )
            .await;

        if stream_result.tool_calls.is_empty() {
            let reason = stream_result.finish_reason.as_deref().unwrap_or("end_turn");
            wire::send_finish(tx, &session_id, &request_id, reason, 200);
            wire::send_result(
                tx,
                id,
                json!({
                    "stopReason": reason,
                    "_meta": {"ai-coding/request-id": request_id}
                }),
            );
            return;
        }

        let workspace_root = state.workspace_root().await;
        for call in &stream_result.tool_calls {
            let raw_input = serde_json::from_str::<Value>(&call.arguments)
                .unwrap_or_else(|_| json!({"arguments": call.arguments}));
            let tool_name = if call.name.trim().is_empty() {
                "unknown_tool"
            } else {
                call.name.as_str()
            };
            wire::send_tool_call_started(
                tx,
                &session_id,
                &request_id,
                &call.id,
                tool_name,
                tools::qoder_kind(tool_name),
                &raw_input,
            );
            let execution = match workspace_root.as_deref() {
                Some(root) => tools::execute(
                    root,
                    tool_name,
                    &call.arguments,
                    &tool_policy,
                    &state.mcp_runtime,
                )
                .await,
                None if tool_name.starts_with("mcp__") => tools::execute(
                    std::path::Path::new("."),
                    tool_name,
                    &call.arguments,
                    &tool_policy,
                    &state.mcp_runtime,
                )
                .await,
                None => tools::ToolExecution {
                    model_content: "Tool error: Qoder workspace root is unavailable. Reopen the workspace and retry.".to_string(),
                    display_output: json!({"error": "Qoder workspace root is unavailable"}),
                    success: false,
                },
            };
            // Keep a minimal local audit trail for routed tool calls.  Never
            // record tool arguments, file contents, command output, prompts,
            // or MCP results: those can contain user data or credentials.
            log::info!(
                "[qoder_adapter] tool completed kind={} success={}",
                tools::qoder_kind(tool_name),
                execution.success
            );
            wire::send_tool_call_finished(
                tx,
                &session_id,
                &request_id,
                &call.id,
                execution.success,
                &execution.display_output,
            );
            state
                .sessions
                .append_message(
                    &session_id,
                    ChatMessage {
                        role: "tool".to_string(),
                        content: execution.model_content,
                        tool_calls: Vec::new(),
                        tool_call_id: Some(call.id.clone()),
                    },
                )
                .await;
        }

        if round + 1 == MAX_TOOL_ROUNDS {
            let msg = "Q Switch stopped after the maximum number of tool rounds.";
            wire::send_finish(tx, &session_id, &request_id, msg, 500);
            wire::send_error(tx, id, -32000, msg);
            return;
        }
    }
}

/// Extract the text content from a `session/prompt` params object.
///
/// The `prompt` field is an array of content blocks:
/// ```json
/// [{"type": "text", "text": "..."}]
/// ```
fn extract_prompt_text(params: &Value) -> String {
    let blocks = match params.get("prompt").and_then(|v| v.as_array()) {
        Some(b) => b,
        None => return String::new(),
    };

    blocks
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(|v| v.as_str()) == Some("text") {
                block.get("text").and_then(|v| v.as_str()).map(String::from)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `initialize` result, declaring ACP capabilities.
fn initialize_result() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": {
                "openClose": true,
                "change": 2,
                "save": {}
            },
            "workspace": {
                "workspaceFolders": {
                    "supported": true,
                    "changeNotifications": "workspace/didChangeWorkspaceFolders"
                }
            }
        },
        "serverInfo": {
            "name": "qswitch-qoder-acp",
            "version": "0.1.0"
        },
        "experimental": {
            "features": {
                "supportComputerUse": "false"
            }
        }
    })
}
