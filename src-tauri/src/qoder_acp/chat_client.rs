//! OpenAI Chat Completions client with SSE streaming support.
//!
//! Sends a streaming `POST /v1/chat/completions` request and emits events
//! on a channel for each parsed delta (text chunk, reasoning chunk, tool
//! call delta, or finish reason).

use crate::database::Database;
use crate::error::AppError;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

/// Streaming event emitted by the chat client.
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Text content delta.
    TextDelta(String),
    /// Reasoning content delta.
    ReasoningDelta(String),
    /// A tool call delta arrived. The complete call is accumulated into the
    /// stream result before Qoder receives its stable tool lifecycle.
    ToolCallDelta,
    /// Finish reason (stop, tool_calls, length, etc.).
    FinishReason(String),
    /// The SSE stream ended.
    Done,
}

/// Resolved route for a chat completion request.
#[derive(Clone)]
pub struct ChatRoute {
    pub base_url: String,
    pub model: String,
    pub auth_header: Option<(String, String)>,
}

impl ChatRoute {
    /// Resolve a Q Switch route from the non-secret Qoder manifest.
    ///
    /// The native adapter never reads or forwards an upstream provider key.
    /// It submits the opaque `qswitch_*` model ID to Q Switch's loopback
    /// endpoint, which resolves the fixed provider and upstream model itself.
    pub async fn from_qswitch_gateway(
        db: &Arc<Database>,
        model_hint: Option<&str>,
    ) -> Result<Self, AppError> {
        let model = model_hint
            .map(|value| value.strip_prefix("custom:").unwrap_or(value))
            .filter(|value| value.starts_with("qswitch_"))
            .ok_or_else(|| {
                AppError::Message(
                    "Q Switch native adapter requires a custom:qswitch_* model".to_string(),
                )
            })?;
        if crate::qoder_config::resolve_qoder_route(model)?.is_none() {
            return Err(AppError::Message(
                "Unknown Q Switch model route. Refresh the Qoder route manifest first.".to_string(),
            ));
        }
        let proxy_config = db.get_proxy_config().await?;
        Ok(Self {
            base_url: format!("http://127.0.0.1:{}/qoder/v1", proxy_config.listen_port),
            model: model.to_string(),
            auth_header: None,
        })
    }
}

/// Send a streaming chat completion request.
///
/// Events are emitted on `event_tx`.  The `cancel_rx` half can be dropped
/// to abort early.
///
/// Returns the accumulated result on success.
pub async fn stream_chat_completion(
    route: &ChatRoute,
    messages: &[crate::qoder_acp::session::ChatMessage],
    tools: &[Value],
    event_tx: mpsc::UnboundedSender<StreamEvent>,
    cancel_rx: oneshot::Receiver<()>,
    session_id: Option<&str>,
) -> Result<StreamResult, AppError> {
    let url = build_request_url(&route.base_url);
    let client = reqwest::Client::new();

    let body = build_request_body(route, messages, tools);

    let mut request = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&body);

    if let Some((header_name, header_value)) = &route.auth_header {
        request = request.header(header_name, header_value);
    }
    // Carry the stable ACP session id so the gateway can continue Anthropic
    // signed-thinking / Responses function-call state across tool rounds.
    if let Some(session_id) = session_id.map(str::trim).filter(|id| !id.is_empty()) {
        request = request.header(
            crate::proxy::providers::qoder_wire::QODER_SESSION_HEADER,
            session_id,
        );
    }

    let response = request
        .send()
        .await
        .map_err(|e| AppError::Message(format!("Failed to send chat request: {e}")))?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(AppError::Message(format!(
            "Chat completions returned HTTP {status}: {text}"
        )));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut result = StreamResult::default();

    tokio::select! {
        _ = cancel_rx => {
            return Err(AppError::Message("Request cancelled".to_string()));
        }
        result_inner = async {
            while let Some(chunk_result) = stream.next().await {
                let chunk = chunk_result.map_err(|e| {
                    AppError::Message(format!("Stream error: {e}"))
                })?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(event_str) = take_next_sse_event(&mut buffer) {
                    if let Some(data) = sse_event_data(&event_str) {
                        if data == "[DONE]" {
                            let _ = event_tx.send(StreamEvent::Done);
                            return Ok::<(), AppError>(());
                        }
                        if let Ok(chunk) = serde_json::from_str::<OpenAIStreamChunk>(&data) {
                            process_stream_chunk(&chunk, &mut result, &event_tx);
                        }
                    }
                }
            }
            Ok::<(), AppError>(())
        } => {
            result_inner?;
        }
    }

    Ok(result)
}

/// Pop one complete Server-Sent Event from a response buffer.  Both LF and
/// CRLF separators are valid SSE framing and the upstream routes in Q Switch
/// can use either form depending on the provider connection.
fn take_next_sse_event(buffer: &mut String) -> Option<String> {
    let lf = buffer.find("\n\n").map(|index| (index, 2));
    let crlf = buffer.find("\r\n\r\n").map(|index| (index, 4));
    let (index, separator_len) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(found), None) | (None, Some(found)) => found,
        (None, None) => return None,
    };
    let event = buffer[..index].to_string();
    buffer.drain(..index + separator_len);
    Some(event)
}

/// Extract the data payload from an SSE event without depending on a
/// provider-specific choice of `data:` whitespace or line endings.
fn sse_event_data(event: &str) -> Option<String> {
    let data_lines: Vec<&str> = event
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
        .collect();
    (!data_lines.is_empty()).then(|| data_lines.join("\n"))
}

fn build_request_body(
    route: &ChatRoute,
    messages: &[crate::qoder_acp::session::ChatMessage],
    tools: &[Value],
) -> Value {
    // Convert session messages to OpenAI format.
    let openai_messages: Vec<Value> = messages
        .iter()
        .map(|m| {
            if m.role == "tool" {
                json!({
                    "role": "tool",
                    "content": m.content,
                    "tool_call_id": m.tool_call_id.as_deref().unwrap_or("")
                })
            } else if !m.tool_calls.is_empty() {
                let tool_calls: Vec<Value> = m
                    .tool_calls
                    .iter()
                    .map(|tc| {
                        json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments
                            }
                        })
                    })
                    .collect();
                json!({
                    "role": m.role,
                    "content": m.content,
                    "tool_calls": tool_calls
                })
            } else {
                json!({"role": m.role, "content": m.content})
            }
        })
        .collect();

    let mut body = json!({
        "model": route.model,
        "messages": openai_messages,
        "stream": true
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.to_vec());
        body["tool_choice"] = Value::String("auto".to_string());
    }
    body
}

fn build_request_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else if base.ends_with("/v1/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/v1/chat/completions")
    }
}

#[derive(Default)]
pub struct StreamResult {
    pub text: String,
    pub reasoning: String,
    pub finish_reason: Option<String>,
    pub tool_calls: Vec<AccumulatedToolCall>,
}

#[derive(Default, Clone)]
pub struct AccumulatedToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

fn process_stream_chunk(
    chunk: &OpenAIStreamChunk,
    result: &mut StreamResult,
    event_tx: &mpsc::UnboundedSender<StreamEvent>,
) {
    for choice in &chunk.choices {
        if let Some(content) = &choice.delta.content {
            if !content.is_empty() {
                result.text.push_str(content);
                let _ = event_tx.send(StreamEvent::TextDelta(content.clone()));
            }
        }

        if let Some(reasoning) = &choice.delta.reasoning {
            if !reasoning.is_empty() {
                result.reasoning.push_str(reasoning);
                let _ = event_tx.send(StreamEvent::ReasoningDelta(reasoning.clone()));
            }
        }

        if let Some(tool_calls) = &choice.delta.tool_calls {
            for tc in tool_calls {
                let idx = tc.index;
                while result.tool_calls.len() <= idx {
                    result.tool_calls.push(AccumulatedToolCall::default());
                }
                let entry = &mut result.tool_calls[idx];
                if let Some(id) = &tc.id {
                    if !id.is_empty() {
                        entry.id = id.clone();
                    }
                }
                if let Some(func) = &tc.function {
                    if let Some(name) = &func.name {
                        if !name.is_empty() {
                            entry.name = name.clone();
                        }
                    }
                    if let Some(args) = &func.arguments {
                        if !args.is_empty() {
                            entry.arguments.push_str(args);
                        }
                    }
                }
                let _ = event_tx.send(StreamEvent::ToolCallDelta);
            }
        }

        if let Some(reason) = &choice.finish_reason {
            if !reason.is_empty() {
                result.finish_reason = Some(reason.clone());
                let _ = event_tx.send(StreamEvent::FinishReason(reason.clone()));
            }
        }
    }
}

// ============================================================================
// OpenAI SSE data structures
// ============================================================================

#[derive(Debug, Deserialize)]
struct OpenAIStreamChunk {
    #[serde(default)]
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default, alias = "reasoning_content")]
    reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DeltaToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "type", default)]
    call_type: Option<String>,
    #[serde(default)]
    function: Option<DeltaFunction>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DeltaFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qoder_acp::session::ChatMessage;

    #[test]
    fn request_body_includes_openai_tools() {
        let route = ChatRoute {
            base_url: "http://127.0.0.1:15731/qoder/v1".to_string(),
            model: "qswitch_route".to_string(),
            auth_header: None,
        };
        let messages = vec![ChatMessage {
            role: "user".to_string(),
            content: "Inspect the workspace".to_string(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }];
        let body = build_request_body(
            &route,
            &messages,
            &crate::qoder_acp::tools::definitions(
                &crate::qoder_config::QoderToolPolicy::default(),
                &[],
            ),
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["tools"][0]["function"]["name"], "list_dir");
    }

    #[test]
    fn request_body_omits_tools_when_none_are_available() {
        let route = ChatRoute {
            base_url: "http://127.0.0.1:15731/qoder/v1".to_string(),
            model: "qswitch_route".to_string(),
            auth_header: None,
        };
        let body = build_request_body(&route, &[], &[]);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn sse_framing_accepts_lf_and_crlf_event_separators() {
        let mut buffer = "data: {\"first\":true}\r\n\r\ndata: {\"second\":true}\n\n".to_string();

        let first = take_next_sse_event(&mut buffer).expect("first CRLF event");
        let second = take_next_sse_event(&mut buffer).expect("second LF event");

        assert_eq!(sse_event_data(&first).as_deref(), Some("{\"first\":true}"));
        assert_eq!(
            sse_event_data(&second).as_deref(),
            Some("{\"second\":true}")
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn sse_data_supports_standard_multiline_payloads() {
        let event = "event: message\r\ndata: first\r\ndata: second";
        assert_eq!(sse_event_data(event).as_deref(), Some("first\nsecond"));
    }
}
