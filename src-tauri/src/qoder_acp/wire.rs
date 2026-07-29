//! Wire protocol — helpers for building and sending ACP JSON-RPC messages
//! over the WebSocket as LSP-framed bytes.

use crate::qoder_acp::lsp_framer;
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Send a JSON-RPC result response.
pub fn send_result(tx: &mpsc::UnboundedSender<Vec<u8>>, id: &Option<Value>, result: Value) {
    if let Some(id) = id {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        });
        send_json(tx, &msg);
    }
}

/// Send a JSON-RPC error response.
pub fn send_error(
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    id: &Option<Value>,
    code: i32,
    message: &str,
) {
    if let Some(id) = id {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": code,
                "message": message
            }
        });
        send_json(tx, &msg);
    }
}

/// Send a `session/update` notification with `agent_message_chunk`.
pub fn send_text_chunk(
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    session_id: &str,
    request_id: &str,
    text: &str,
) {
    let msg = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {
                    "type": "text",
                    "text": text
                }
            },
            "_meta": {
                "ai-coding/request-id": request_id,
                "ai-coding/streamed": true
            }
        }
    });
    send_json(tx, &msg);
}

/// Send a `session/update` notification with `agent_thought_chunk`.
pub fn send_thought_chunk(
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    session_id: &str,
    request_id: &str,
    text: &str,
) {
    let msg = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": {
                    "type": "reasoning",
                    "text": text
                }
            },
            "_meta": {
                "ai-coding/request-id": request_id,
                "ai-coding/streamed": true
            }
        }
    });
    send_json(tx, &msg);
}

/// Announce an executing tool call using the state shape Qoder's Quest UI
/// renders.  The adapter always uses a stable toolCallId for its matching
/// completed/failed update.
pub fn send_tool_call_started(
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    session_id: &str,
    request_id: &str,
    tool_call_id: &str,
    tool_name: &str,
    kind: &str,
    raw_input: &Value,
) {
    let msg = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": tool_call_id,
                "toolName": tool_name,
                "kind": kind,
                "status": "in_progress",
                "rawInput": raw_input
            },
            "_meta": {
                "ai-coding/request-id": request_id,
                "ai-coding/streamed": true
            }
        }
    });
    send_json(tx, &msg);
}

/// Mark a previously announced tool call as completed or failed.  `raw_output`
/// is intentionally a compact status object; the full tool content travels in
/// the next OpenAI tool message rather than being duplicated into the UI log.
pub fn send_tool_call_finished(
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    session_id: &str,
    request_id: &str,
    tool_call_id: &str,
    success: bool,
    raw_output: &Value,
) {
    let msg = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "tool_call_update",
                "toolCallId": tool_call_id,
                "status": if success { "completed" } else { "failed" },
                "rawOutput": raw_output
            },
            "_meta": {
                "ai-coding/request-id": request_id,
                "ai-coding/streamed": true
            }
        }
    });
    send_json(tx, &msg);
}

/// Send the `chat_finish` notification and `chat/finish` event.
pub fn send_finish(
    tx: &mpsc::UnboundedSender<Vec<u8>>,
    session_id: &str,
    request_id: &str,
    reason: &str,
    status_code: i32,
) {
    let stop_reason = if status_code == 200 {
        "end_turn"
    } else {
        "error"
    };

    // session/update with chat_finish notification
    let finish_msg = json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": {
                "sessionUpdate": "notification",
                "type": "chat_finish",
                "data": {
                    "requestId": request_id,
                    "sessionId": session_id,
                    "reason": reason,
                    "statusCode": status_code
                },
                "_meta": {
                    "ai-coding/request-id": request_id
                }
            }
        }
    });
    send_json(tx, &finish_msg);

    // chat/finish event (compatibility)
    let chat_finish = json!({
        "jsonrpc": "2.0",
        "method": "chat/finish",
        "params": {
            "sessionId": session_id,
            "requestId": request_id,
            "stopReason": stop_reason,
            "_meta": {
                "ai-coding/request-id": request_id
            }
        }
    });
    send_json(tx, &chat_finish);
}

/// Serialize a JSON value and frame it as LSP, then send via the channel.
fn send_json(tx: &mpsc::UnboundedSender<Vec<u8>>, msg: &Value) {
    let body = serde_json::to_vec(msg).unwrap_or_else(|_| b"{}".to_vec());
    let framed = lsp_framer::LspFramer::encode(&body);
    let _ = tx.send(framed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::qoder_acp::lsp_framer::LspFramer;

    fn decode_message(frame: Vec<u8>) -> Value {
        let mut framer = LspFramer::new();
        let frames = framer.append(&frame).unwrap();
        serde_json::from_slice(&frames[0]).unwrap()
    }

    #[test]
    fn tool_lifecycle_uses_stable_id_and_terminal_status() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        send_tool_call_started(
            &tx,
            "session-1",
            "request-1",
            "call-1",
            "read_file",
            "read",
            &json!({"path": "README.md"}),
        );
        send_tool_call_finished(
            &tx,
            "session-1",
            "request-1",
            "call-1",
            true,
            &json!({"summary": "Read 3 lines"}),
        );
        let started = decode_message(rx.try_recv().unwrap());
        let finished = decode_message(rx.try_recv().unwrap());
        assert_eq!(started["params"]["update"]["toolCallId"], "call-1");
        assert_eq!(started["params"]["update"]["status"], "in_progress");
        assert_eq!(finished["params"]["update"]["toolCallId"], "call-1");
        assert_eq!(finished["params"]["update"]["status"], "completed");
    }
}
