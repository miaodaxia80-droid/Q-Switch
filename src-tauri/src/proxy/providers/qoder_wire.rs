//! Qoder canonical-Chat ⇆ upstream three-protocol bridge.
//!
//! Qoder always speaks OpenAI **Chat Completions** to the local QSwitch
//! gateway (`/qoder/v1/chat/completions`). This module converts that canonical
//! Chat request into one of three upstream wire formats selected by the
//! provider configuration, and converts the (possibly streaming) upstream
//! response back into Chat Completions / Chat SSE that Qoder understands:
//!
//! * [`QoderApiFormat::OpenAiChat`]        — `/chat/completions` (passthrough)
//! * [`QoderApiFormat::AnthropicMessages`] — `/v1/messages` (x-api-key)
//! * [`QoderApiFormat::OpenAiResponses`]   — `/responses`     (Bearer)
//!
//! The module is intentionally self-contained: it does not reuse the Codex /
//! Claude inbound transform branches (those are keyed to different inbound
//! shapes) and it never stores prompts, full replies or API keys in the
//! continuation state.
//!
//! Every state transition logs at `debug`/`info` with credentials and query
//! strings redacted.

use crate::provider::Provider;
use crate::proxy::ProxyError;
use async_stream::stream;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures::StreamExt;
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ============================================================================
// API format enum
// ============================================================================

/// Upstream wire format configured on a Qoder custom provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QoderApiFormat {
    /// OpenAI Chat Completions (the existing, regression-free path).
    #[default]
    OpenAiChat,
    /// Anthropic Messages (`POST /v1/messages`).
    AnthropicMessages,
    /// OpenAI Responses (`POST /responses`).
    OpenAiResponses,
}

impl QoderApiFormat {
    /// Stable wire string persisted in the provider settings / manifest.
    pub fn as_str(self) -> &'static str {
        match self {
            QoderApiFormat::OpenAiChat => "openai_chat",
            QoderApiFormat::AnthropicMessages => "anthropic_messages",
            QoderApiFormat::OpenAiResponses => "openai_responses",
        }
    }

    /// Path appended to a Base URL when `is_full_url == false`.
    pub fn endpoint_suffix(self) -> &'static str {
        match self {
            QoderApiFormat::OpenAiChat => "/chat/completions",
            QoderApiFormat::AnthropicMessages => "/v1/messages",
            QoderApiFormat::OpenAiResponses => "/responses",
        }
    }

    pub fn parse(value: &str) -> Result<Self, ProxyError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "openai_chat" | "openai" | "chat" | "chat_completions" => {
                Ok(QoderApiFormat::OpenAiChat)
            }
            "anthropic_messages" | "anthropic" | "messages" => {
                Ok(QoderApiFormat::AnthropicMessages)
            }
            "openai_responses" | "responses" => Ok(QoderApiFormat::OpenAiResponses),
            other => Err(ProxyError::ConfigError(format!(
                "Unknown Qoder upstream API format `{other}`. Expected one of: \
                 openai_chat, anthropic_messages, openai_responses."
            ))),
        }
    }
}

/// Default Anthropic protocol version header.
pub const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
/// Internal header carrying the stable ACP `sessionId` to the gateway.
pub const QODER_SESSION_HEADER: &str = "x-qswitch-qoder-session-id";

// ============================================================================
// URL building / validation
// ============================================================================

/// Resolve the concrete upstream endpoint.
///
/// * Base URL mode (`is_full_url == false`): trim a trailing `/` and append the
///   format-specific suffix, *unless* the base already ends in that suffix.
///   Any explicit `?query` on the base is preserved.
/// * Full endpoint mode (`is_full_url == true`): use the value verbatim (after
///   trimming), never appending a path.
///
/// Only `http`/`https` are accepted; userinfo and fragments are rejected. The
/// returned value is safe to log at host+path granularity (the caller must not
/// log the query, which may carry a token).
pub fn build_upstream_endpoint(
    raw: &str,
    format: QoderApiFormat,
    is_full_url: bool,
) -> Result<String, ProxyError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ProxyError::ConfigError(
            "Qoder upstream address cannot be empty.".to_string(),
        ));
    }
    let mut parsed = url::Url::parse(trimmed)
        .map_err(|e| ProxyError::ConfigError(format!("Invalid Qoder upstream URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ProxyError::ConfigError(
            "Qoder upstream address must start with http:// or https://.".to_string(),
        ));
    }
    // Reject embedded credentials: https://user:pass@host/...
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(ProxyError::ConfigError(
            "Qoder upstream URL must not contain a username or password.".to_string(),
        ));
    }
    // Reject fragments outright — they never belong in an API request and are a
    // common copy/paste mistake.
    if parsed.fragment().is_some() {
        return Err(ProxyError::ConfigError(
            "Qoder upstream URL must not contain a URL fragment (#...).".to_string(),
        ));
    }
    // Require a real, non-empty host (guards against `https:///path`).
    if parsed.host().is_none() || parsed.host_str().map(str::trim).unwrap_or("").is_empty() {
        return Err(ProxyError::ConfigError(
            "Qoder upstream URL is missing a host.".to_string(),
        ));
    }

    if is_full_url {
        return Ok(trimmed.to_string());
    }

    let suffix = format.endpoint_suffix();
    let path = parsed.path().trim_end_matches('/');
    // Tolerate users who already pasted the full endpoint in Base URL mode.
    if !path.ends_with(suffix) {
        // Insert the suffix into the PATH so an explicit query stays at the end.
        parsed.set_path(&format!("{path}{suffix}"));
    }
    // `Url` preserves the original query and fragment (already rejected) here.
    Ok(parsed.to_string())
}

/// Redact an endpoint for logging: scheme + host + path only. Never log query.
pub fn redact_endpoint_for_log(endpoint: &str) -> String {
    match url::Url::parse(endpoint) {
        Ok(url) => format!(
            "{}://{}{}",
            url.scheme(),
            url.host_str().unwrap_or("?"),
            url.path()
        ),
        Err(_) => "<unparseable-endpoint>".to_string(),
    }
}

// ============================================================================
// Auth headers
// ============================================================================

/// Reject header values that could inject extra headers / CRLF / control chars.
fn safe_header_value(value: &str) -> Result<HeaderValue, ProxyError> {
    if value.is_empty() {
        return Err(ProxyError::ConfigError(
            "Upstream auth header value is empty.".to_string(),
        ));
    }
    if value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return Err(ProxyError::ConfigError(
            "Upstream credential contains illegal CR/LF/NUL characters.".to_string(),
        ));
    }
    HeaderValue::from_str(value).map_err(|e| {
        ProxyError::ConfigError(format!(
            "Upstream credential is not a valid header value: {e}"
        ))
    })
}

/// Apply the standard authentication headers for a format.
///
/// * Chat / Responses → `Authorization: Bearer <key>`
/// * Anthropic        → `x-api-key: <key>` + `anthropic-version`
///
/// A missing key is tolerated (some local gateways need none); the request
/// simply goes out without an auth header.
pub fn apply_upstream_auth(
    headers: &mut HeaderMap,
    format: QoderApiFormat,
    api_key: Option<&str>,
    anthropic_version: Option<&str>,
) -> Result<(), ProxyError> {
    let key = api_key.map(str::trim).filter(|key| !key.is_empty());
    match format {
        QoderApiFormat::OpenAiChat | QoderApiFormat::OpenAiResponses => {
            if let Some(key) = key {
                headers.insert(
                    axum::http::header::AUTHORIZATION,
                    safe_header_value(&format!("Bearer {key}"))?,
                );
            }
        }
        QoderApiFormat::AnthropicMessages => {
            if let Some(key) = key {
                headers.insert(
                    HeaderName::from_static("x-api-key"),
                    safe_header_value(key)?,
                );
            }
            let version = anthropic_version
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .unwrap_or(DEFAULT_ANTHROPIC_VERSION);
            headers.insert(
                HeaderName::from_static("anthropic-version"),
                safe_header_value(version)?,
            );
        }
    }
    Ok(())
}

// ============================================================================
// Helpers shared by request transforms
// ============================================================================

fn messages_array(body: &Value) -> Vec<Value> {
    body.get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Flatten a Chat `content` (string OR OpenAI content-part array) to plain text.
pub fn content_to_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    Some(text.to_string())
                } else {
                    part.get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Read a requested output ceiling across the various field names providers use.
fn output_ceiling(body: &Value, default: u64) -> u64 {
    for key in ["max_completion_tokens", "max_output_tokens", "max_tokens"] {
        if let Some(value) = body.get(key).and_then(Value::as_u64) {
            if value > 0 {
                return value;
            }
        }
    }
    default
}

fn tool_arguments_to_json_value(raw: &Value) -> Value {
    match raw {
        Value::Object(_) | Value::Array(_) => raw.clone(),
        Value::String(text) if !text.trim().is_empty() => {
            serde_json::from_str(text).unwrap_or_else(|_| json!({}))
        }
        _ => json!({}),
    }
}

// ============================================================================
// Chat -> Anthropic Messages
// ============================================================================

/// Convert a canonical Chat Completions request body to an Anthropic Messages
/// request body. `model` is the resolved real upstream model.
pub fn chat_to_anthropic(chat: Value, model: &str) -> Value {
    let messages = messages_array(&chat);

    // 1. Lift system messages to the top-level `system` field.
    let mut system_text = String::new();
    let mut convo: Vec<Value> = Vec::new();
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                if !system_text.is_empty() {
                    system_text.push('\n');
                }
                system_text.push_str(&text);
            }
        } else {
            convo.push(message);
        }
    }

    // 2. Translate messages, merging consecutive `tool` results into one user
    //    turn (Anthropic groups tool_result blocks under a single user msg).
    let mut out: Vec<Value> = Vec::new();
    let mut pending_tool_results: Vec<Value> = Vec::new();

    let flush_tool_results = |pending: &mut Vec<Value>, out: &mut Vec<Value>| {
        if !pending.is_empty() {
            out.push(json!({ "role": "user", "content": std::mem::take(pending) }));
        }
    };

    for message in convo {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "tool" {
            let tool_use_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
            pending_tool_results.push(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": text,
            }));
            continue;
        }
        flush_tool_results(&mut pending_tool_results, &mut out);

        let mut blocks: Vec<Value> = Vec::new();
        let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
        if !text.is_empty() {
            blocks.push(json!({ "type": "text", "text": text }));
        }
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            for call in tool_calls {
                let id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let function = call.get("function").cloned().unwrap_or_else(|| json!({}));
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let input =
                    tool_arguments_to_json_value(function.get("arguments").unwrap_or(&Value::Null));
                blocks.push(json!({
                    "type": "tool_use",
                    "id": id,
                    "name": name,
                    "input": input,
                }));
            }
        }
        // Anthropic requires non-empty content; keep an assistant turn that only
        // carried tool calls valid by leaving its tool_use blocks intact.
        let anthropic_role = if role == "assistant" {
            "assistant"
        } else {
            "user"
        };
        out.push(json!({ "role": anthropic_role, "content": blocks }));
    }
    flush_tool_results(&mut pending_tool_results, &mut out);

    // 3. Tools: OpenAI function tools -> Anthropic input_schema.
    let anthropic_tools: Vec<Value> = chat
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    let function = tool.get("function")?;
                    let name = function.get("name").and_then(Value::as_str)?;
                    let description = function
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let input_schema = function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    Some(json!({
                        "name": name,
                        "description": description,
                        "input_schema": input_schema,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    // 4. tool_choice mapping.
    let mut request = json!({
        "model": model,
        "messages": out,
        "max_tokens": output_ceiling(&chat, 4096),
        "stream": chat.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if !system_text.is_empty() {
        request["system"] = Value::String(system_text);
    }
    if !anthropic_tools.is_empty() {
        request["tools"] = Value::Array(anthropic_tools);
        if let Some(choice) = map_anthropic_tool_choice(chat.get("tool_choice")) {
            request["tool_choice"] = choice;
        }
    }
    copy_optional_scalar(&chat, &mut request, "temperature");
    copy_optional_scalar(&chat, &mut request, "top_p");
    copy_optional_scalar(&chat, &mut request, "stop");
    request
}

fn map_anthropic_tool_choice(choice: Option<&Value>) -> Option<Value> {
    let value = choice?;
    match value {
        Value::String(kind) => match kind.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            // Anthropic has no "none"; caller drops tools instead.
            "none" => None,
            _ => Some(json!({"type": "auto"})),
        },
        Value::Object(_) => {
            let name = value
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if name.is_empty() {
                Some(json!({"type": "auto"}))
            } else {
                Some(json!({"type": "tool", "name": name}))
            }
        }
        _ => None,
    }
}

fn copy_optional_scalar(src: &Value, dst: &mut Value, key: &str) {
    if let Some(value) = src.get(key) {
        if value.is_number() || value.is_string() {
            dst[key] = value.clone();
        }
    }
}

// ============================================================================
// Chat -> OpenAI Responses
// ============================================================================

/// Convert a canonical Chat Completions request body to an OpenAI Responses
/// request body.
pub fn chat_to_responses(chat: Value, model: &str) -> Value {
    let messages = messages_array(&chat);

    // System messages collapse into `instructions`.
    let mut instructions = String::new();
    let mut input: Vec<Value> = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        if role == "system" {
            let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                if !instructions.is_empty() {
                    instructions.push('\n');
                }
                instructions.push_str(&text);
            }
            continue;
        }
        if role == "tool" {
            let call_id = message
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let output = content_to_text(message.get("content").unwrap_or(&Value::Null));
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            }));
            continue;
        }
        // Assistant tool calls become function_call items.
        if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
            // Any assistant text rides along as an assistant input message.
            // Responses input message content uses `input_text` even when the
            // role is `assistant`; `output_text` is reserved for response
            // output items.
            let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
            if !text.is_empty() {
                input.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "input_text", "text": text}],
                }));
            }
            for call in tool_calls {
                let call_id = call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let function = call.get("function").cloned().unwrap_or_else(|| json!({}));
                let name = function
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let arguments = match function.get("arguments") {
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => other.to_string(),
                    None => String::from("{}"),
                };
                input.push(json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                }));
            }
            continue;
        }
        let text = content_to_text(message.get("content").unwrap_or(&Value::Null));
        let part_type = "input_text";
        input.push(json!({
            "type": "message",
            "role": role,
            "content": [{"type": part_type, "text": text}],
        }));
    }

    let response_tools: Vec<Value> = chat
        .get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    let function = tool.get("function")?;
                    let name = function.get("name").and_then(Value::as_str)?;
                    let description = function
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let parameters = function
                        .get("parameters")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    Some(json!({
                        "type": "function",
                        "name": name,
                        "description": description,
                        "parameters": parameters,
                    }))
                })
                .collect()
        })
        .unwrap_or_default();

    let mut request = json!({
        "model": model,
        "input": input,
        "stream": chat.get("stream").and_then(Value::as_bool).unwrap_or(false),
    });
    if !instructions.is_empty() {
        request["instructions"] = Value::String(instructions);
    }
    let ceiling = output_ceiling(&chat, 0);
    if ceiling > 0 {
        request["max_output_tokens"] = json!(ceiling);
    }
    if !response_tools.is_empty() {
        request["tools"] = Value::Array(response_tools);
        if let Some(choice) = chat.get("tool_choice") {
            request["tool_choice"] = map_responses_tool_choice(choice);
        }
    }
    if let Some(effort) = chat.get("reasoning_effort").and_then(Value::as_str) {
        if !effort.is_empty() {
            request["reasoning"] = json!({ "effort": effort, "summary": "auto" });
        }
    }
    copy_optional_scalar(&chat, &mut request, "temperature");
    copy_optional_scalar(&chat, &mut request, "top_p");
    request
}

fn map_responses_tool_choice(choice: &Value) -> Value {
    match choice {
        Value::String(kind) => match kind.as_str() {
            "required" => json!("required"),
            "none" => json!("none"),
            _ => json!("auto"),
        },
        Value::Object(_) => {
            if let Some(name) = choice
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
            {
                json!({"type": "function", "name": name})
            } else {
                json!("auto")
            }
        }
        _ => json!("auto"),
    }
}

// ============================================================================
// Non-streaming response conversion -> Chat Completions object
// ============================================================================

fn chat_envelope(model: &str, created: i64) -> Value {
    json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion",
        "created": created,
        "model": model,
    })
}

fn usage_to_chat(usage: Option<&Value>) -> Value {
    let prompt_tokens = usage
        .and_then(|u| u.get("input_tokens").or_else(|| u.get("prompt_tokens")))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = usage
        .and_then(|u| {
            u.get("output_tokens")
                .or_else(|| u.get("completion_tokens"))
        })
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": prompt_tokens + completion_tokens,
    })
}

/// Anthropic Messages non-streaming object -> Chat Completions object.
pub fn anthropic_to_chat(anthropic: &Value, model: &str) -> Result<Value, ProxyError> {
    let created = chrono::Utc::now().timestamp();
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(blocks) = anthropic.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(part) = block.get("text").and_then(Value::as_str) {
                        text.push_str(part);
                    }
                }
                Some("tool_use") => {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": input.to_string()},
                    }));
                }
                _ => {}
            }
        }
    }
    let finish = match anthropic.get("stop_reason").and_then(Value::as_str) {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        Some("stop_sequence") => "stop",
        _ => "stop",
    };
    let mut message = json!({"role": "assistant"});
    message["content"] = if text.is_empty() && !tool_calls.is_empty() {
        Value::Null
    } else {
        Value::String(text)
    };
    if !tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let mut out = chat_envelope(model, created);
    out["choices"] = json!([{
        "index": 0,
        "message": message,
        "finish_reason": finish,
    }]);
    out["usage"] = usage_to_chat(anthropic.get("usage"));
    Ok(out)
}

/// OpenAI Responses non-streaming object -> Chat Completions object.
pub fn responses_to_chat(response: &Value, model: &str) -> Result<Value, ProxyError> {
    let created = chrono::Utc::now().timestamp();
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    if let Some(parts) = item.get("content").and_then(Value::as_array) {
                        for part in parts {
                            if let Some(t) = part
                                .get("text")
                                .and_then(Value::as_str)
                                .or_else(|| part.get("output_text").and_then(Value::as_str))
                            {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(t);
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let arguments = match item.get("arguments") {
                        Some(Value::String(s)) => s.clone(),
                        Some(other) => other.to_string(),
                        None => String::from("{}"),
                    };
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": arguments},
                    }));
                }
                _ => {}
            }
        }
    }
    let has_tools = !tool_calls.is_empty();
    let finish = if has_tools {
        "tool_calls"
    } else if response.get("status").and_then(Value::as_str) == Some("incomplete") {
        "length"
    } else {
        "stop"
    };
    let mut message = json!({"role": "assistant"});
    message["content"] = if text.is_empty() && has_tools {
        Value::Null
    } else {
        Value::String(text)
    };
    if has_tools {
        message["tool_calls"] = Value::Array(tool_calls);
    }
    let mut out = chat_envelope(model, created);
    out["choices"] = json!([{
        "index": 0,
        "message": message,
        "finish_reason": finish,
    }]);
    out["usage"] = usage_to_chat(response.get("usage"));
    Ok(out)
}

// ============================================================================
// SSE frame splitting (shared; tolerates LF and CRLF)
// ============================================================================

/// Extract complete SSE events from `buffer`, returning `(event, data)` pairs.
/// Handles both `\n\n` and `\r\n\r\n`; `event:` defaults to `"message"`.
pub fn drain_sse_events(buffer: &mut String) -> Vec<(String, String)> {
    let mut events = Vec::new();
    loop {
        let lf = buffer.find("\n\n").map(|idx| (idx, 2));
        let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));
        let (index, sep_len) = match (lf, crlf) {
            (Some(a), Some(b)) => {
                if a.0 <= b.0 {
                    a
                } else {
                    b
                }
            }
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => break,
        };
        let raw = buffer[..index].to_string();
        buffer.drain(..index + sep_len);
        let mut event = String::from("message");
        let mut data_lines: Vec<&str> = Vec::new();
        for line in raw.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_lines.push(rest.trim_start_matches(' '));
            }
        }
        if !data_lines.is_empty() {
            events.push((event, data_lines.join("\n")));
        }
    }
    events
}

fn sse_data_frame(value: &Value) -> Bytes {
    Bytes::from(format!("data: {}\n\n", value))
}

fn chunk_with_delta(delta: Value, finish_reason: Option<&str>) -> Value {
    let mut choice = json!({"index": 0, "delta": delta});
    if let Some(reason) = finish_reason {
        choice["finish_reason"] = json!(reason);
    } else {
        choice["finish_reason"] = Value::Null;
    }
    json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        "object": "chat.completion.chunk",
        "created": chrono::Utc::now().timestamp(),
        "choices": [choice],
    })
}

// ============================================================================
// Anthropic SSE -> Chat SSE translator (state machine)
// ============================================================================

/// Streaming translator for Anthropic Messages events.
#[derive(Default)]
pub struct AnthropicSseTranslator {
    started: bool,
    /// Anthropic content-block index -> (kind, chat tool_calls index)
    block_to_tool: HashMap<usize, usize>,
    next_tool_index: usize,
    /// Finalized signed `thinking` block waiting to bind to following tool_use(s).
    pending_thinking: Option<Value>,
    /// Content-block index of the in-flight thinking block (if any).
    current_thinking_index: Option<usize>,
    /// Accumulated thinking text for the in-flight thinking block.
    current_thinking_text: String,
    /// Signature delivered by `signature_delta` for the in-flight block.
    current_thinking_signature: String,
    finish_reason: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    model: String,
}

impl AnthropicSseTranslator {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }

    /// Build a Chat chunk stamped with the upstream model name.
    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        let mut chunk = chunk_with_delta(delta, finish_reason);
        chunk["model"] = json!(self.model);
        chunk
    }

    /// Feed one upstream `(event, data)` pair; returns Chat SSE frame bytes.
    pub fn push(&mut self, event: &str, data: &str, session_id: Option<&str>) -> Vec<Bytes> {
        if data == "[DONE]" {
            return self.finish(true);
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(sse_data_frame(
                &self.chunk(json!({"role": "assistant"}), None),
            ));
        }
        match event {
            "message_start" => {
                if let Some(message) = value.get("message") {
                    if let Some(id) = message.get("id").and_then(Value::as_str) {
                        bridge_set_anthropic_message_id(session_id, id);
                    }
                    self.input_tokens = message
                        .get("usage")
                        .and_then(|u| u.get("input_tokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(self.input_tokens);
                }
            }
            "content_block_start" => {
                let block_index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let block = value
                    .get("content_block")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let tool_index = self.next_tool_index;
                        self.next_tool_index += 1;
                        self.block_to_tool.insert(block_index, tool_index);
                        let id = block
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        // Bind the just-completed signed thinking block to this
                        // tool_use so the next turn can replay it in order.
                        if !id.is_empty() {
                            if let Some(thinking) = self.pending_thinking.clone() {
                                bridge_record_thinking(session_id, &[id.clone()], thinking);
                            }
                        }
                        out.push(sse_data_frame(&self.chunk(
                            json!({
                                "tool_calls": [{
                                    "index": tool_index,
                                    "id": id,
                                    "type": "function",
                                    "function": {"name": name, "arguments": ""}
                                }]
                            }),
                            None,
                        )));
                    }
                    Some("thinking") => {
                        self.current_thinking_index = Some(block_index);
                        self.current_thinking_text = block
                            .get("thinking")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        self.current_thinking_signature.clear();
                    }
                    _ => {}
                }
            }
            "content_block_delta" => {
                let block_index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                let delta = value.get("delta").cloned().unwrap_or_else(|| json!({}));
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = delta.get("text").and_then(Value::as_str) {
                            out.push(sse_data_frame(
                                &self.chunk(json!({ "content": text }), None),
                            ));
                        }
                    }
                    Some("thinking_delta") => {
                        let text = delta.get("thinking").and_then(Value::as_str).unwrap_or("");
                        self.current_thinking_text.push_str(text);
                        // Surface reasoning to the Qoder UI as reasoning_content.
                        out.push(sse_data_frame(
                            &self.chunk(json!({ "reasoning_content": text }), None),
                        ));
                    }
                    Some("signature_delta") => {
                        let signature =
                            delta.get("signature").and_then(Value::as_str).unwrap_or("");
                        self.current_thinking_signature = signature.to_string();
                    }
                    Some("input_json_delta") => {
                        if let Some(&tool_index) = self.block_to_tool.get(&block_index) {
                            let partial = delta
                                .get("partial_json")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            out.push(sse_data_frame(&self.chunk(
                                json!({
                                    "tool_calls": [{
                                        "index": tool_index,
                                        "function": {"arguments": partial}
                                    }]
                                }),
                                None,
                            )));
                        }
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                // When the thinking block finishes, materialize the signed
                // block; it binds to the tool_use block(s) that start next.
                let stop_index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                if self.current_thinking_index == Some(stop_index) {
                    let block = json!({
                        "type": "thinking",
                        "thinking": std::mem::take(&mut self.current_thinking_text),
                        "signature": std::mem::take(&mut self.current_thinking_signature),
                    });
                    self.pending_thinking = Some(block);
                    self.current_thinking_index = None;
                }
            }
            "message_delta" => {
                if let Some(reason) = value.get("stop_reason").and_then(Value::as_str) {
                    self.finish_reason = Some(anthropic_stop_to_chat(reason).to_string());
                }
                self.output_tokens = value
                    .get("usage")
                    .and_then(|u| u.get("output_tokens"))
                    .and_then(Value::as_u64)
                    .unwrap_or(self.output_tokens);
            }
            "message_stop" => {
                out.extend(self.finish(false));
            }
            "error" => {
                log::warn!(
                    "[QoderBridge] anthropic upstream error event: {}",
                    sanitize_upstream_error(&value)
                );
                self.finish_reason = Some("stop".to_string());
                out.extend(self.finish(false));
            }
            _ => {}
        }
        out
    }

    fn finish(&mut self, _done: bool) -> Vec<Bytes> {
        let reason = self
            .finish_reason
            .take()
            .unwrap_or_else(|| "stop".to_string());
        let usage = json!({
            "prompt_tokens": self.input_tokens,
            "completion_tokens": self.output_tokens,
            "total_tokens": self.input_tokens + self.output_tokens,
        });
        let mut final_chunk = self.chunk(json!({}), Some(&reason));
        final_chunk["usage"] = usage;
        vec![
            sse_data_frame(&final_chunk),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ]
    }
}

fn anthropic_stop_to_chat(stop: &str) -> &'static str {
    match stop {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        _ => "stop",
    }
}

// ============================================================================
// Responses SSE -> Chat SSE translator (state machine)
// ============================================================================

#[derive(Default)]
pub struct ResponsesSseTranslator {
    started: bool,
    /// Responses output index -> chat tool_calls index
    item_to_tool: HashMap<usize, usize>,
    next_tool_index: usize,
    finish_reason: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    model: String,
}

impl ResponsesSseTranslator {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Default::default()
        }
    }

    /// Build a Chat chunk stamped with the upstream model name.
    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        let mut chunk = chunk_with_delta(delta, finish_reason);
        chunk["model"] = json!(self.model);
        chunk
    }

    pub fn push(&mut self, event: &str, data: &str, _session_id: Option<&str>) -> Vec<Bytes> {
        if data == "[DONE]" {
            return self.finish();
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            out.push(sse_data_frame(
                &self.chunk(json!({"role": "assistant"}), None),
            ));
        }
        match event {
            "response.output_text.delta" => {
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    out.push(sse_data_frame(&self.chunk(json!({"content": text}), None)));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(text) = value.get("delta").and_then(Value::as_str) {
                    out.push(sse_data_frame(
                        &self.chunk(json!({"reasoning_content": text}), None),
                    ));
                }
            }
            "response.output_item.added" => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                let item = value.get("item").cloned().unwrap_or_else(|| json!({}));
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let tool_index = self.next_tool_index;
                    self.next_tool_index += 1;
                    self.item_to_tool.insert(output_index, tool_index);
                    let id = item
                        .get("call_id")
                        .or_else(|| item.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    out.push(sse_data_frame(&self.chunk(
                        json!({
                            "tool_calls": [{
                                "index": tool_index,
                                "id": id,
                                "type": "function",
                                "function": {"name": name, "arguments": ""}
                            }]
                        }),
                        None,
                    )));
                }
            }
            "response.function_call_arguments.delta" => {
                let output_index = value
                    .get("output_index")
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                if let Some(&tool_index) = self.item_to_tool.get(&output_index) {
                    let partial = value.get("delta").and_then(Value::as_str).unwrap_or("");
                    out.push(sse_data_frame(&self.chunk(
                        json!({
                            "tool_calls": [{
                                "index": tool_index,
                                "function": {"arguments": partial}
                            }]
                        }),
                        None,
                    )));
                }
            }
            "response.function_call_arguments.done" => {
                // Arguments are complete; nothing extra to emit (already streamed).
            }
            "response.completed" => {
                if let Some(response) = value.get("response") {
                    let has_function = response
                        .get("output")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items.iter().any(|i| {
                                i.get("type").and_then(Value::as_str) == Some("function_call")
                            })
                        })
                        .unwrap_or(false);
                    self.finish_reason = Some(
                        if has_function {
                            "tool_calls"
                        } else if response.get("status").and_then(Value::as_str)
                            == Some("incomplete")
                        {
                            "length"
                        } else {
                            "stop"
                        }
                        .to_string(),
                    );
                    if let Some(usage) = response.get("usage") {
                        self.input_tokens = usage
                            .get("input_tokens")
                            .or_else(|| usage.get("prompt_tokens"))
                            .and_then(Value::as_u64)
                            .unwrap_or(self.input_tokens);
                        self.output_tokens = usage
                            .get("output_tokens")
                            .or_else(|| usage.get("completion_tokens"))
                            .and_then(Value::as_u64)
                            .unwrap_or(self.output_tokens);
                    }
                }
                out.extend(self.finish());
            }
            "error" => {
                log::warn!(
                    "[QoderBridge] responses upstream error event: {}",
                    sanitize_upstream_error(&value)
                );
                self.finish_reason = Some("stop".to_string());
                out.extend(self.finish());
            }
            _ => {}
        }
        out
    }

    fn finish(&mut self) -> Vec<Bytes> {
        let reason = self
            .finish_reason
            .take()
            .unwrap_or_else(|| "stop".to_string());
        let usage = json!({
            "prompt_tokens": self.input_tokens,
            "completion_tokens": self.output_tokens,
            "total_tokens": self.input_tokens + self.output_tokens,
        });
        let mut final_chunk = self.chunk(json!({}), Some(&reason));
        final_chunk["usage"] = usage;
        vec![
            sse_data_frame(&final_chunk),
            Bytes::from_static(b"data: [DONE]\n\n"),
        ]
    }
}

// ============================================================================
// Continuation bridge state (TTL, in-process only)
// ============================================================================

/// Per-session opaque continuation data. Stores ONLY protocol fields required
/// to continue a tool/thinking exchange — never prompts, replies or keys.
struct SessionBridge {
    last_seen: Instant,
    anthropic_message_id: Option<String>,
    /// Anthropic tool_use id -> signed thinking blocks that must be replayed.
    thinking_by_tool: HashMap<String, Vec<Value>>,
}

impl Default for SessionBridge {
    fn default() -> Self {
        Self {
            last_seen: Instant::now(),
            anthropic_message_id: None,
            thinking_by_tool: HashMap::new(),
        }
    }
}

static BRIDGE_STORE: Lazy<Mutex<HashMap<String, SessionBridge>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

const BRIDGE_TTL: Duration = Duration::from_secs(60 * 60);

fn bridge_touch(session_id: Option<&str>) -> Option<String> {
    let id = session_id?.trim();
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
}

fn bridge_set_anthropic_message_id(session_id: Option<&str>, message_id: &str) {
    let Some(key) = bridge_touch(session_id) else {
        return;
    };
    let mut store = BRIDGE_STORE.lock().unwrap();
    let entry = store.entry(key).or_default();
    entry.last_seen = Instant::now();
    entry.anthropic_message_id = Some(message_id.to_string());
}

fn bridge_record_thinking(session_id: Option<&str>, tool_ids: &[String], thinking: Value) {
    let Some(key) = bridge_touch(session_id) else {
        return;
    };
    let mut store = BRIDGE_STORE.lock().unwrap();
    let entry = store.entry(key).or_default();
    entry.last_seen = Instant::now();
    for tool_id in tool_ids {
        entry
            .thinking_by_tool
            .entry(tool_id.clone())
            .or_default()
            .push(thinking.clone());
    }
}

/// Pull (and clear) the signed thinking blocks that must precede an assistant
/// message containing these tool_use ids when replaying tool results upstream.
pub fn bridge_take_thinking(session_id: Option<&str>, tool_ids: &[String]) -> Vec<Value> {
    let Some(key) = bridge_touch(session_id) else {
        return Vec::new();
    };
    let mut store = BRIDGE_STORE.lock().unwrap();
    let Some(entry) = store.get_mut(&key) else {
        return Vec::new();
    };
    entry.last_seen = Instant::now();
    let mut blocks = Vec::new();
    for tool_id in tool_ids {
        if let Some(think) = entry.thinking_by_tool.remove(tool_id) {
            blocks.extend(think);
        }
    }
    blocks
}

/// Remove all continuation state for a session (session/close / disconnect).
pub fn bridge_drop_session(session_id: &str) {
    let mut store = BRIDGE_STORE.lock().unwrap();
    store.remove(session_id);
}

/// Evict expired sessions. Cheap enough to call opportunistically per request.
pub fn bridge_sweep() {
    let mut store = BRIDGE_STORE.lock().unwrap();
    let now = Instant::now();
    store.retain(|_, v| now.duration_since(v.last_seen) < BRIDGE_TTL);
}

// ============================================================================
// Upstream error sanitization
// ============================================================================

/// Return only a generic category + optional provider request id. We never
/// echo the upstream error body (it can leak internal URLs / credentials).
fn sanitize_upstream_error(value: &Value) -> String {
    let category = value
        .get("error")
        .and_then(|e| e.get("type").or_else(|| e.get("code")))
        .and_then(Value::as_str)
        .map(sanitize_log_token)
        .unwrap_or_else(|| "upstream_error".to_string());
    let request_id = value
        .get("request_id")
        .or_else(|| value.get("requestId"))
        .and_then(Value::as_str)
        .map(sanitize_log_token)
        .unwrap_or_else(|| "-".to_string());
    format!("category={category} request_id={request_id}")
}

/// Keep provider-controlled identifiers out of log injection and error text.
/// This is deliberately lossy: diagnostics need a stable category/id, never
/// the upstream's arbitrary error payload.
fn sanitize_log_token(raw: &str) -> String {
    let token: String = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | ':'))
        .take(96)
        .collect();
    if token.is_empty() {
        "-".to_string()
    } else {
        token
    }
}

fn chat_error_body(status: StatusCode, category: &str, request_id: Option<&str>) -> Value {
    let category = sanitize_log_token(category);
    let request_id = request_id.map(sanitize_log_token);
    json!({
        "error": {
            "message": format!(
                "Upstream request failed (HTTP {status}, {category}). request_id={}.",
                request_id.as_deref().unwrap_or("-")
            ),
            "type": "qoder_upstream_error",
            "code": status.as_u16(),
        }
    })
}

// ============================================================================
// Resolved upstream configuration
// ============================================================================

/// Everything the bridge needs to reach one upstream model.
#[derive(Debug, Clone)]
pub struct QoderUpstreamConfig {
    pub format: QoderApiFormat,
    pub base_url: String,
    pub is_full_url: bool,
    pub api_key: Option<String>,
    pub anthropic_version: Option<String>,
    pub upstream_model: String,
    pub session_id: Option<String>,
}

impl QoderUpstreamConfig {
    /// Build from the hidden Qoder provider (flat `settings_config`) and the
    /// manifest-resolved target.
    pub fn from_provider(
        provider: &Provider,
        format: QoderApiFormat,
        is_full_url: bool,
        upstream_model: &str,
        session_id: Option<String>,
    ) -> Self {
        let settings = provider.settings_config.as_object();
        let base_url = settings
            .and_then(|s| s.get("base_url"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let api_key = settings
            .and_then(|s| s.get("apiKey"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let anthropic_version = settings
            .and_then(|s| {
                s.get("anthropicVersion")
                    .or_else(|| s.get("anthropic_version"))
            })
            .and_then(Value::as_str)
            .map(str::to_string);
        Self {
            format,
            base_url,
            is_full_url,
            api_key,
            anthropic_version,
            upstream_model: upstream_model.to_string(),
            session_id,
        }
    }
}

/// Inject cached signed-thinking blocks into an Anthropic request right before
/// the assistant messages that reference them (multi-turn tool continuation).
pub fn inject_anthropic_thinking_replay(request: &mut Value, session_id: Option<&str>) {
    let Some(messages) = request.get_mut("messages").and_then(Value::as_array_mut) else {
        return;
    };
    for message in messages.iter_mut() {
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(blocks) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        let tool_ids: Vec<String> = blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .filter_map(|b| b.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        if tool_ids.is_empty() {
            continue;
        }
        let thinking = bridge_take_thinking(session_id, &tool_ids);
        if thinking.is_empty() {
            continue;
        }
        if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
            let existing = std::mem::take(content);
            content.extend(thinking);
            content.extend(existing);
        }
    }
}

// ============================================================================
// Upstream exchange (HTTP)
// ============================================================================

fn build_http_client() -> Result<reqwest::Client, ProxyError> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| ProxyError::ForwardFailed(format!("Build Qoder upstream client failed: {e}")))
}

fn sse_response_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache"),
    );
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers
}

/// Run the full Qoder bridge: transform the canonical Chat request, call the
/// upstream in its native protocol, and return a Chat (JSON or SSE) response.
///
/// `chat_body` has already had its `model` rewritten to the real upstream model
/// and its pinned `reasoning_effort` applied by the caller.
pub async fn bridge_qoder_upstream(
    cfg: QoderUpstreamConfig,
    chat_body: Value,
) -> Result<Response, ProxyError> {
    bridge_sweep();
    let format = cfg.format;
    let is_stream = chat_body
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // 1. Build the upstream wire body.
    let mut upstream_body = match format {
        QoderApiFormat::OpenAiChat => chat_body.clone(),
        QoderApiFormat::AnthropicMessages => {
            let mut req = chat_to_anthropic(chat_body.clone(), &cfg.upstream_model);
            inject_anthropic_thinking_replay(&mut req, cfg.session_id.as_deref());
            req
        }
        QoderApiFormat::OpenAiResponses => {
            chat_to_responses(chat_body.clone(), &cfg.upstream_model)
        }
    };
    // Guarantee the real model name reaches the upstream.
    if let Some(object) = upstream_body.as_object_mut() {
        object.insert("model".to_string(), json!(cfg.upstream_model));
    }

    // 2. Endpoint + auth.
    let endpoint = build_upstream_endpoint(&cfg.base_url, format, cfg.is_full_url)?;
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    apply_upstream_auth(
        &mut headers,
        format,
        cfg.api_key.as_deref(),
        cfg.anthropic_version.as_deref(),
    )?;

    log::info!(
        "[QoderBridge] format={} stream={} model={} endpoint={}",
        format.as_str(),
        is_stream,
        cfg.upstream_model,
        redact_endpoint_for_log(&endpoint)
    );

    let client = build_http_client()?;
    let mut request_builder = client.post(&endpoint).headers(headers);
    // Reqwest takes ownership of the JSON body.
    request_builder = request_builder.json(&upstream_body);
    let upstream = request_builder.send().await.map_err(|error| {
        // reqwest's Display implementation may include the full URL. Keep
        // credentials and query parameters out of both logs and the error
        // returned to Qoder; the endpoint was already logged in redacted form.
        let category = if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connect"
        } else {
            "transport"
        };
        log::warn!("[QoderBridge] upstream connection failed ({category})");
        ProxyError::ForwardFailed(format!("Qoder upstream connection failed ({category})."))
    })?;

    let status = upstream.status();
    if !status.is_success() {
        let code = status.as_u16();
        let request_id = upstream
            .headers()
            .get("x-request-id")
            .or_else(|| upstream.headers().get("request-id"))
            .and_then(|v| v.to_str().ok())
            .map(sanitize_log_token);
        // Drain at most a small body purely for local logging; never echo it.
        let bytes = upstream.bytes().await.unwrap_or_default();
        log::warn!(
            "[QoderBridge] upstream returned HTTP {code} (body {} bytes, request_id={})",
            bytes.len(),
            request_id.as_deref().unwrap_or("-")
        );
        let body = chat_error_body(
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
            "http_error",
            request_id.as_deref(),
        );
        return Ok((
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
            axum::Json(body),
        )
            .into_response());
    }

    if !is_stream {
        let bytes = upstream.bytes().await.map_err(|e| {
            ProxyError::ForwardFailed(format!("Read Qoder upstream body failed: {e}"))
        })?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
            ProxyError::TransformError(format!("Parse non-stream upstream body failed: {e}"))
        })?;
        let chat = match format {
            QoderApiFormat::OpenAiChat => value,
            QoderApiFormat::AnthropicMessages => anthropic_to_chat(&value, &cfg.upstream_model)?,
            QoderApiFormat::OpenAiResponses => responses_to_chat(&value, &cfg.upstream_model)?,
        };
        return Ok(axum::Json(chat).into_response());
    }

    // 3. Streaming: translate upstream SSE -> Chat SSE.
    let model = cfg.upstream_model.clone();
    let session_id = cfg.session_id.clone();
    let byte_stream = upstream.bytes_stream();
    let output = stream! {
        let mut buffer = String::new();
        let mut anthropic = AnthropicSseTranslator::new(model.clone());
        let mut responses = ResponsesSseTranslator::new(model.clone());
        futures::pin_mut!(byte_stream);
        while let Some(chunk) = byte_stream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));
                    for (event, data) in drain_sse_events(&mut buffer) {
                        let frames = match format {
                            QoderApiFormat::AnthropicMessages =>
                                anthropic.push(&event, &data, session_id.as_deref()),
                            QoderApiFormat::OpenAiResponses =>
                                responses.push(&event, &data, session_id.as_deref()),
                            QoderApiFormat::OpenAiChat =>
                                vec![Bytes::from(format!("data: {data}\n\n"))],
                        };
                        for frame in frames {
                            yield Ok::<Bytes, std::io::Error>(frame);
                        }
                    }
                }
                Err(error) => {
                    log::warn!("[QoderBridge] upstream stream broken: {error}");
                    let err = chunk_with_delta(
                        json!({}),
                        Some("stop"),
                    );
                    yield Ok(sse_data_frame(&err));
                    yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
                    break;
                }
            }
        }
    };

    Ok((sse_response_headers(), Body::from_stream(output)).into_response())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_parse_round_trips() {
        assert_eq!(
            QoderApiFormat::parse("anthropic_messages").unwrap(),
            QoderApiFormat::AnthropicMessages
        );
        assert_eq!(
            QoderApiFormat::parse("OpenAI_Responses").unwrap(),
            QoderApiFormat::OpenAiResponses
        );
        assert_eq!(
            QoderApiFormat::parse("").unwrap(),
            QoderApiFormat::OpenAiChat
        );
        assert!(QoderApiFormat::parse("weird").is_err());
        assert_eq!(
            QoderApiFormat::AnthropicMessages.endpoint_suffix(),
            "/v1/messages"
        );
        assert_eq!(
            QoderApiFormat::OpenAiResponses.endpoint_suffix(),
            "/responses"
        );
    }

    #[test]
    fn base_url_appends_format_suffix_and_preserves_query() {
        assert_eq!(
            build_upstream_endpoint(
                "https://api.example.com/v1/",
                QoderApiFormat::OpenAiChat,
                false
            )
            .unwrap(),
            "https://api.example.com/v1/chat/completions"
        );
        assert_eq!(
            build_upstream_endpoint(
                "https://api.anthropic.com",
                QoderApiFormat::AnthropicMessages,
                false
            )
            .unwrap(),
            "https://api.anthropic.com/v1/messages"
        );
        assert_eq!(
            build_upstream_endpoint(
                "https://gw.example.com/openai?beta=true",
                QoderApiFormat::OpenAiResponses,
                false
            )
            .unwrap(),
            "https://gw.example.com/openai/responses?beta=true"
        );
        // Already-suffixed base is not doubled.
        assert_eq!(
            build_upstream_endpoint(
                "https://gw.example.com/v1/messages",
                QoderApiFormat::AnthropicMessages,
                false
            )
            .unwrap(),
            "https://gw.example.com/v1/messages"
        );
        // Full URL mode never appends.
        assert_eq!(
            build_upstream_endpoint(
                "https://gw.example.com/custom/chat",
                QoderApiFormat::OpenAiChat,
                true
            )
            .unwrap(),
            "https://gw.example.com/custom/chat"
        );
    }

    #[test]
    fn url_validation_rejects_bad_inputs() {
        for bad in [
            "ftp://example.com/x",
            "https://user:pass@example.com/x",
            "https://example.com/x#frag",
            "",
            "not a url",
        ] {
            assert!(
                build_upstream_endpoint(bad, QoderApiFormat::OpenAiChat, false).is_err(),
                "expected rejection: {bad:?}"
            );
        }
        // WHATWG normalizes extra leading slashes to a real host, which is safe.
        assert!(build_upstream_endpoint(
            "https:///missing-host",
            QoderApiFormat::OpenAiChat,
            false
        )
        .is_ok());
    }

    #[test]
    fn redaction_hides_query() {
        let logged = redact_endpoint_for_log("https://api.example.com/v1/chat?token=secret");
        assert!(logged.contains("/v1/chat"));
        assert!(!logged.contains("secret"));
        assert!(!logged.contains('?'));
    }

    #[test]
    fn auth_headers_match_each_format() {
        // Chat -> Bearer
        let mut headers = HeaderMap::new();
        apply_upstream_auth(&mut headers, QoderApiFormat::OpenAiChat, Some("sk-x"), None).unwrap();
        assert_eq!(
            headers.get(axum::http::header::AUTHORIZATION).unwrap(),
            "Bearer sk-x"
        );
        assert!(headers.get("x-api-key").is_none());

        // Anthropic -> x-api-key + version
        let mut headers = HeaderMap::new();
        apply_upstream_auth(
            &mut headers,
            QoderApiFormat::AnthropicMessages,
            Some("sk-ant"),
            None,
        )
        .unwrap();
        assert_eq!(headers.get("x-api-key").unwrap(), "sk-ant");
        assert_eq!(
            headers.get("anthropic-version").unwrap(),
            DEFAULT_ANTHROPIC_VERSION
        );
        assert!(headers.get(axum::http::header::AUTHORIZATION).is_none());

        // CRLF injection rejected.
        let mut headers = HeaderMap::new();
        assert!(apply_upstream_auth(
            &mut headers,
            QoderApiFormat::OpenAiChat,
            Some("sk\nx-injected: 1"),
            None
        )
        .is_err());
    }

    fn chat_request_with_tool_round() -> Value {
        json!({
            "model": "qswitch_route",
            "stream": false,
            "max_tokens": 1024,
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "list files"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1", "type": "function",
                    "function": {"name": "list_dir", "arguments": "{\"path\":\".\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "a.rs\nb.rs"}
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "list_dir",
                    "description": "list",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                }
            }],
            "tool_choice": "auto"
        })
    }

    #[test]
    fn chat_to_anthropic_lifts_system_and_groups_tool_results() {
        let req = chat_to_anthropic(chat_request_with_tool_round(), "claude-upstream");
        assert_eq!(req["model"], "claude-upstream");
        assert_eq!(req["system"], "be brief");
        assert_eq!(req["max_tokens"], 1024);
        let messages = req["messages"].as_array().unwrap();
        // user, assistant(tool_use), user(grouped tool_result)
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "user");
        let assistant_blocks = messages[1]["content"].as_array().unwrap();
        assert_eq!(assistant_blocks[0]["type"], "tool_use");
        assert_eq!(assistant_blocks[0]["id"], "call_1");
        assert_eq!(assistant_blocks[0]["input"]["path"], ".");
        let result_blocks = messages[2]["content"].as_array().unwrap();
        assert_eq!(result_blocks[0]["type"], "tool_result");
        assert_eq!(result_blocks[0]["tool_use_id"], "call_1");
        assert_eq!(req["tools"][0]["input_schema"]["type"], "object");
        assert_eq!(req["tool_choice"]["type"], "auto");
    }

    #[test]
    fn anthropic_tool_choice_maps_required_to_any_and_none_to_none() {
        assert_eq!(
            map_anthropic_tool_choice(Some(&json!("required"))).unwrap()["type"],
            "any"
        );
        assert!(map_anthropic_tool_choice(Some(&json!("none"))).is_none());
        let specific =
            map_anthropic_tool_choice(Some(&json!({"function": {"name": "foo"}}))).unwrap();
        assert_eq!(specific["type"], "tool");
        assert_eq!(specific["name"], "foo");
    }

    #[test]
    fn chat_to_responses_builds_items_and_reasoning() {
        let mut chat = chat_request_with_tool_round();
        chat["reasoning_effort"] = json!("high");
        let req = chat_to_responses(chat, "responses-model");
        assert_eq!(req["model"], "responses-model");
        assert_eq!(req["instructions"], "be brief");
        let input = req["input"].as_array().unwrap();
        // user message, assistant message(empty text skipped) + function_call,
        // function_call_output => 3 items.
        let kinds: Vec<&str> = input
            .iter()
            .map(|i| i.get("type").and_then(Value::as_str).unwrap())
            .collect();
        assert!(kinds.contains(&"message"));
        assert!(kinds.contains(&"function_call"));
        assert!(kinds.contains(&"function_call_output"));
        let output = input
            .iter()
            .find(|i| i["type"] == "function_call_output")
            .unwrap();
        assert_eq!(output["call_id"], "call_1");
        assert_eq!(req["tools"][0]["type"], "function");
        assert_eq!(req["tool_choice"], "auto");
        assert_eq!(req["reasoning"]["effort"], "high");
        assert_eq!(req["max_output_tokens"], 1024);

        let assistant_history = chat_to_responses(
            json!({"messages":[{"role":"assistant","content":"prior"}]}),
            "responses-model",
        );
        assert_eq!(
            assistant_history["input"][0]["content"][0]["type"],
            "input_text"
        );
    }

    #[test]
    fn responses_tool_choice_maps_strings() {
        assert_eq!(
            map_responses_tool_choice(&json!("required")),
            json!("required")
        );
        assert_eq!(map_responses_tool_choice(&json!("none")), json!("none"));
        assert_eq!(map_responses_tool_choice(&json!("auto")), json!("auto"));
    }

    #[test]
    fn anthropic_non_stream_text_and_tool_use_to_chat() {
        let upstream = json!({
            "id": "msg_1",
            "role": "assistant",
            "content": [
                {"type": "text", "text": "hello"},
                {"type": "tool_use", "id": "tu_1", "name": "list_dir",
                 "input": {"path": "."}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 11, "output_tokens": 7}
        });
        let chat = anthropic_to_chat(&upstream, "m").unwrap();
        let choice = &chat["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(choice["message"]["content"], "hello");
        let call = &choice["message"]["tool_calls"][0];
        assert_eq!(call["id"], "tu_1");
        assert_eq!(call["function"]["name"], "list_dir");
        assert_eq!(call["function"]["arguments"], r#"{"path":"."}"#);
        assert_eq!(chat["usage"]["prompt_tokens"], 11);
        assert_eq!(chat["usage"]["completion_tokens"], 7);
        assert_eq!(chat["usage"]["total_tokens"], 18);
    }

    #[test]
    fn responses_non_stream_to_chat() {
        let upstream = json!({
            "id": "resp_1",
            "status": "completed",
            "output": [
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "done"}]},
                {"type": "function_call", "call_id": "c1", "name": "f",
                 "arguments": "{\"a\":1}"}
            ],
            "usage": {"input_tokens": 3, "output_tokens": 4}
        });
        let chat = responses_to_chat(&upstream, "m").unwrap();
        assert_eq!(chat["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(chat["choices"][0]["message"]["content"], "done");
        assert_eq!(chat["choices"][0]["message"]["tool_calls"][0]["id"], "c1");
        assert_eq!(chat["usage"]["total_tokens"], 7);
    }

    #[test]
    fn sse_drain_handles_lf_crlf_and_multiline() {
        let mut buf =
            String::from("event: x\ndata: {\"a\":1}\n\ndata: line1\r\ndata: line2\r\n\r\n");
        let events = drain_sse_events(&mut buf);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].0, "x");
        assert_eq!(events[0].1, "{\"a\":1}");
        assert_eq!(events[1].1, "line1\nline2");
        assert!(buf.is_empty());
    }

    #[test]
    fn anthropic_sse_text_stream_becomes_chat_sse() {
        let mut t = AnthropicSseTranslator::new("m");
        let frames = t.push(
            "message_start",
            r#"{"message":{"id":"msg_1","usage":{"input_tokens":5}}}"#,
            None,
        );
        assert_eq!(frames.len(), 1); // role
        let frames = [
            frames,
            t.push(
                "content_block_start",
                r#"{"index":0,"content_block":{"type":"text"}}"#,
                None,
            ),
            t.push(
                "content_block_delta",
                r#"{"index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
                None,
            ),
            t.push("content_block_stop", r#"{"index":0}"#, None),
            t.push(
                "message_delta",
                r#"{"stop_reason":"end_turn","usage":{"output_tokens":3}}"#,
                None,
            ),
            t.push("message_stop", r#"{}"#, None),
        ]
        .concat();
        // role + content + final + [DONE]
        assert_eq!(frames.len(), 4);
        let content_frame = parse_frame(&frames[1]);
        assert_eq!(content_frame["choices"][0]["delta"]["content"], "hi");
        let last = String::from_utf8_lossy(frames.last().unwrap());
        assert!(last.contains("[DONE]"));
    }

    #[test]
    fn anthropic_sse_tool_arguments_are_sharded_by_index() {
        let mut t = AnthropicSseTranslator::new("m");
        t.push(
            "message_start",
            r#"{"message":{"usage":{"input_tokens":1}}}"#,
            None,
        );
        let start = t.push(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"tool_use","id":"tu_9","name":"run"}}"#,
            None,
        );
        let start_data = parse_frame(&start[0]);
        assert_eq!(
            start_data["choices"][0]["delta"]["tool_calls"][0]["id"],
            "tu_9"
        );
        let d1 = t.push(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"{\"a\":"}}"#,
            None,
        );
        let d1v = parse_frame(&d1[0]);
        assert_eq!(
            d1v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            r#"{"a":"#
        );
        let d2 = t.push(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"input_json_delta","partial_json":"1}"}}"#,
            None,
        );
        let d2v = parse_frame(&d2[0]);
        assert_eq!(
            d2v["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"],
            "1}"
        );
        t.push(
            "message_delta",
            r#"{"stop_reason":"tool_use","usage":{"output_tokens":2}}"#,
            None,
        );
        let fin = t.push("message_stop", r#"{}"#, None);
        let finv = parse_frame(&fin[0]);
        assert_eq!(finv["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn responses_sse_text_and_function_stream() {
        let mut t = ResponsesSseTranslator::new("m");
        t.push("response.created", r#"{}"#, None);
        let text = t.push("response.output_text.delta", r#"{"delta":"hello"}"#, None);
        assert_eq!(
            parse_frame(&text[0])["choices"][0]["delta"]["content"],
            "hello"
        );
        let added = t.push(
            "response.output_item.added",
            r#"{"output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"f"}}"#,
            None,
        );
        assert_eq!(
            parse_frame(&added[0])["choices"][0]["delta"]["tool_calls"][0]["id"],
            "call_1"
        );
        let args = t.push(
            "response.function_call_arguments.delta",
            r#"{"output_index":0,"delta":"{\"x\":"}"#,
            None,
        );
        assert!(
            parse_frame(&args[0])["choices"][0]["delta"]["tool_calls"][0]
                .get("function")
                .is_some()
        );
        let done = t.push(
            "response.completed",
            r#"{"response":{"status":"completed","output":[{"type":"function_call","call_id":"call_1","name":"f","arguments":"{\"x\":1}"}],"usage":{"input_tokens":1,"output_tokens":2}}}"#,
            None,
        );
        // final chunk + DONE
        assert_eq!(done.len(), 2);
        assert_eq!(
            parse_frame(&done[0])["choices"][0]["finish_reason"],
            "tool_calls"
        );
    }

    #[test]
    fn thinking_blocks_are_stored_and_replayed_for_tool_continuation() {
        let mut t = AnthropicSseTranslator::new("m");
        t.push("message_start", r#"{"message":{}}"#, Some("sess-x"));
        t.push(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            Some("sess-x"),
        );
        t.push(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}"#,
            Some("sess-x"),
        );
        t.push(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"signature_delta","signature":"sig-1"}}"#,
            Some("sess-x"),
        );
        t.push("content_block_stop", r#"{"index":0}"#, Some("sess-x"));
        t.push(
            "content_block_start",
            r#"{"index":1,"content_block":{"type":"tool_use","id":"tu_1","name":"f"}}"#,
            Some("sess-x"),
        );
        t.push("content_block_stop", r#"{"index":1}"#, Some("sess-x"));
        let blocks = bridge_take_thinking(Some("sess-x"), &["tu_1".to_string()]);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["signature"], "sig-1");
        assert!(blocks[0]["thinking"].as_str().unwrap().contains("reason"));
        // Second take is cleared.
        assert!(bridge_take_thinking(Some("sess-x"), &["tu_1".to_string()]).is_empty());
        bridge_drop_session("sess-x");
    }

    fn parse_frame(frame: &Bytes) -> Value {
        let line = String::from_utf8_lossy(frame);
        let data = line.trim_start_matches("data: ").trim();
        serde_json::from_str(data).unwrap()
    }

    #[test]
    fn usage_conversion_handles_both_schemas() {
        let anthropic = json!({"input_tokens": 10, "output_tokens": 5});
        assert_eq!(usage_to_chat(Some(&anthropic))["total_tokens"], 15);
        let openai = json!({"prompt_tokens": 8, "completion_tokens": 2});
        assert_eq!(usage_to_chat(Some(&openai))["total_tokens"], 10);
    }

    #[test]
    fn upstream_error_diagnostics_are_lossy_and_log_safe() {
        let value = json!({
            "error": {"type": "bad\ncategory?secret"},
            "request_id": "req\r\ninternal"
        });
        let diagnostic = sanitize_upstream_error(&value);
        assert_eq!(
            diagnostic,
            "category=badcategorysecret request_id=reqinternal"
        );
        let body = chat_error_body(
            StatusCode::UNAUTHORIZED,
            "http\nerror?secret",
            Some("req\nsecret"),
        );
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("httperrorsecret"));
        assert!(message.contains("reqsecret"));
        assert!(!message.contains('\n'));
    }

    // ----- Local three-protocol contract tests over a real loopback socket.

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    struct MockCapture {
        path: String,
        has_bearer: bool,
        has_x_api_key: bool,
        has_session: bool,
        body: Value,
    }

    /// Spawn a one-shot mock that records the request and replies with `reply`
    /// (raw bytes after the HTTP status/headers), closing the connection.
    async fn spawn_mock(
        reply: &'static str,
        status: u16,
    ) -> (u16, std::sync::mpsc::Receiver<MockCapture>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = std::sync::mpsc::channel();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            let header_end;
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
                if let Some(pos) = find_subsequence(&raw, b"\r\n\r\n") {
                    header_end = pos + 4;
                    break;
                }
            }
            let head = String::from_utf8_lossy(&raw[..header_end]);
            let mut lines = head.split("\r\n");
            let request_line = lines.next().unwrap_or("");
            let path = request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or("/")
                .to_string();
            let lower = head.to_lowercase();
            let content_length = lower
                .lines()
                .find_map(|l| {
                    l.strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().ok())
                })
                .flatten()
                .unwrap_or(0);
            while raw.len() - header_end < content_length {
                let n = socket.read(&mut buf).await.unwrap();
                raw.extend_from_slice(&buf[..n]);
            }
            let body_bytes = &raw[header_end..header_end + content_length];
            let body: Value = serde_json::from_slice(body_bytes).unwrap_or(Value::Null);
            tx.send(MockCapture {
                path,
                has_bearer: lower.contains("authorization: bearer"),
                has_x_api_key: lower.contains("x-api-key:"),
                has_session: lower.contains(QODER_SESSION_HEADER),
                body,
            })
            .unwrap();
            let reason = if status == 200 { "OK" } else { "Unauthorized" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
                reply.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(reply.as_bytes()).await.unwrap();
            socket.flush().await.unwrap();
        });
        (port, rx)
    }

    fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn chat_request() -> Value {
        json!({
            "model": "placeholder",
            "stream": true,
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ]
        })
    }

    fn cfg_for(port: u16, format: QoderApiFormat) -> QoderUpstreamConfig {
        QoderUpstreamConfig {
            format,
            base_url: format!("http://127.0.0.1:{port}"),
            is_full_url: false,
            api_key: Some("sk-test".to_string()),
            anthropic_version: Some("2023-06-01".to_string()),
            upstream_model: "upstream-model".to_string(),
            session_id: Some("sess-contract".to_string()),
        }
    }

    async fn collect_body(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn contract_openai_chat_passthrough() {
        const REPLY: &str =
            "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"delta\":{\"content\":\"MOCK\"}}]}\n\n\
             data: [DONE]\n\n";
        let (port, rx) = spawn_mock(REPLY, 200).await;
        let response =
            bridge_qoder_upstream(cfg_for(port, QoderApiFormat::OpenAiChat), chat_request())
                .await
                .unwrap();
        let text = collect_body(response).await;
        assert!(text.contains("chat.completion.chunk"));
        assert!(text.contains("MOCK"));
        assert!(text.contains("[DONE]"));
        let cap = rx.recv().unwrap();
        assert!(cap.path.ends_with("/chat/completions"), "path={}", cap.path);
        assert!(cap.has_bearer);
        // The internal session header must never leak to the real upstream.
        assert!(!cap.has_session);
        assert!(cap.body.get("messages").is_some());
        assert_eq!(cap.body["model"], "upstream-model");
    }

    #[tokio::test]
    async fn contract_anthropic_messages_converts_both_ways() {
        const REPLY: &str =
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n\
             event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"MOCK\"}}\n\n\
             event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let (port, rx) = spawn_mock(REPLY, 200).await;
        let response = bridge_qoder_upstream(
            cfg_for(port, QoderApiFormat::AnthropicMessages),
            chat_request(),
        )
        .await
        .unwrap();
        let text = collect_body(response).await;
        assert!(text.contains("chat.completion.chunk"), "{text}");
        assert!(text.contains("MOCK"));
        let cap = rx.recv().unwrap();
        assert!(cap.path.ends_with("/v1/messages"), "path={}", cap.path);
        assert!(cap.has_x_api_key, "must use x-api-key");
        assert!(
            !cap.has_session,
            "internal session header must not leak upstream"
        );
        assert!(
            cap.body.get("max_tokens").is_some(),
            "anthropic needs max_tokens"
        );
        assert_eq!(cap.body["system"], json!("be brief"));
    }

    #[tokio::test]
    async fn contract_openai_responses_converts_both_ways() {
        const REPLY: &str =
            "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"MOCK\"}\n\n\
             event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n";
        let (port, rx) = spawn_mock(REPLY, 200).await;
        let response = bridge_qoder_upstream(
            cfg_for(port, QoderApiFormat::OpenAiResponses),
            chat_request(),
        )
        .await
        .unwrap();
        let text = collect_body(response).await;
        assert!(text.contains("chat.completion.chunk"), "{text}");
        assert!(text.contains("MOCK"));
        let cap = rx.recv().unwrap();
        assert!(cap.path.ends_with("/responses"), "path={}", cap.path);
        assert!(cap.has_bearer);
        assert!(
            !cap.has_session,
            "internal session header must not leak upstream"
        );
        assert!(
            cap.body.get("input").is_some(),
            "responses uses input items"
        );
    }

    #[tokio::test]
    async fn contract_upstream_401_becomes_sanitized_chat_error_response() {
        // Upstream replies 401 with a body that must NOT be echoed back.
        let (port, _rx) = spawn_mock("leaked-internal-url-and-secret", 401).await;
        let response = bridge_qoder_upstream(
            cfg_for(port, QoderApiFormat::OpenAiResponses),
            chat_request(),
        )
        .await
        .expect("upstream error becomes a sanitized Response, not a transport Err");
        assert_eq!(response.status(), 401);
        let text = collect_body(response).await;
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(value.is_object(), "sanitized error must be a JSON object");
        assert!(text.contains("http_error"), "{text}");
        assert!(
            !text.contains("leaked-internal-url-and-secret"),
            "must not echo upstream body"
        );
    }
}
