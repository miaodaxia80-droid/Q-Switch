//! Minimal MCP client runtime used only by explicitly Qoder-enabled servers.
//!
//! Qoder's own bundle is never changed.  The ACP adapter presents MCP tools to
//! the routed model as ordinary OpenAI function tools and translates calls to
//! MCP JSON-RPC.  Stdio servers and stateless Streamable HTTP servers are
//! supported; legacy SSE-only configurations remain visible in Q Switch but
//! are intentionally not exposed to a Qoder session.

use crate::app_config::McpServer;
use crate::database::Database;
use futures::StreamExt;
use serde_json::{json, Value};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;

const MCP_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MCP_RESPONSE_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone)]
pub struct McpToolDefinition {
    pub function_name: String,
    server_id: String,
    tool_name: String,
    description: String,
    input_schema: Value,
}

impl McpToolDefinition {
    pub fn as_openai_tool(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.function_name,
                "description": self.description,
                "parameters": self.input_schema,
            }
        })
    }
}

pub struct McpRuntime {
    db: Arc<Database>,
}

impl McpRuntime {
    pub fn new(db: Arc<Database>) -> Self {
        Self { db }
    }

    /// Discover tools from MCP servers whose Qoder switch is enabled. A
    /// failed/unavailable server is omitted from the model tool list rather
    /// than making ordinary workspace tools fail.
    pub async fn load_tools(&self) -> Vec<McpToolDefinition> {
        let servers = match self.db.get_all_mcp_servers() {
            Ok(servers) => servers,
            Err(error) => {
                log::warn!("[qoder_mcp] cannot load MCP server config: {error}");
                return Vec::new();
            }
        };

        let mut definitions = Vec::new();
        for server in servers.values().filter(|server| server.apps.qoder) {
            match discover_server_tools(server).await {
                Ok(tools) => definitions.extend(tools.into_iter().map(|tool| {
                    let tool_name = tool.get("name").and_then(Value::as_str).unwrap_or("tool");
                    let description = tool
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("MCP tool")
                        .to_string();
                    let input_schema = tool
                        .get("inputSchema")
                        .or_else(|| tool.get("input_schema"))
                        .cloned()
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({"type": "object", "additionalProperties": true}));
                    McpToolDefinition {
                        function_name: function_name(&server.id, tool_name),
                        server_id: server.id.clone(),
                        tool_name: tool_name.to_string(),
                        description: format!("{}: {description}", server.name),
                        input_schema,
                    }
                })),
                Err(error) => {
                    // Do not log server config, command arguments, request
                    // headers, tool input, or any response body here.
                    log::warn!(
                        "[qoder_mcp] tool discovery failed for {}: {error}",
                        server.name
                    );
                }
            }
        }
        definitions
    }

    pub async fn call(&self, function: &str, arguments: Value) -> Result<(String, String), String> {
        let definitions = self.load_tools().await;
        let definition = definitions
            .into_iter()
            .find(|definition| definition.function_name == function)
            .ok_or_else(|| "MCP tool is unavailable. Check that its Qoder switch is enabled and the server is running.".to_string())?;
        let server = self
            .db
            .get_all_mcp_servers()
            .map_err(|error| format!("Cannot load MCP server: {error}"))?
            .shift_remove(&definition.server_id)
            .ok_or_else(|| "MCP server configuration was removed.".to_string())?;
        let result = call_server_tool(&server, &definition.tool_name, arguments).await?;
        let content = render_mcp_result(&result);
        Ok((
            content,
            format!("{}: {}", server.name, definition.tool_name),
        ))
    }
}

fn function_name(server_id: &str, tool_name: &str) -> String {
    format!(
        "mcp__{}__{}",
        identifier_component(server_id),
        identifier_component(tool_name)
    )
}

fn identifier_component(value: &str) -> String {
    let output = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if output.is_empty() {
        "server".to_string()
    } else {
        output.chars().take(48).collect()
    }
}

async fn discover_server_tools(server: &McpServer) -> Result<Vec<Value>, String> {
    let result = rpc_server(server, "tools/list", json!({})).await?;
    result
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "MCP tools/list response did not contain a tools array".to_string())
}

async fn call_server_tool(
    server: &McpServer,
    name: &str,
    arguments: Value,
) -> Result<Value, String> {
    rpc_server(
        server,
        "tools/call",
        json!({"name": name, "arguments": arguments}),
    )
    .await
}

async fn rpc_server(server: &McpServer, method: &str, params: Value) -> Result<Value, String> {
    match server.server.get("type").and_then(Value::as_str).unwrap_or("stdio") {
        "stdio" => rpc_stdio(server, method, params).await,
        "http" => rpc_http(server, method, params).await,
        "sse" => Err("Legacy SSE MCP servers are not supported by the Qoder adapter yet. Use stdio or Streamable HTTP.".to_string()),
        other => Err(format!("Unsupported MCP transport: {other}")),
    }
}

async fn rpc_stdio(server: &McpServer, method: &str, params: Value) -> Result<Value, String> {
    let command = server
        .server
        .get("command")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "MCP stdio server has no command.".to_string())?;
    // On macOS, Command::new("node") resolves against the GUI process's PATH,
    // which is not the user's shell PATH. Use the conventional Node location
    // for the current CPU architecture when the user configured a bare node
    // command. Absolute user-configured commands stay untouched.
    #[cfg(target_os = "macos")]
    let use_macos_node_launcher = command == "node";
    #[cfg(not(target_os = "macos"))]
    let use_macos_node_launcher = false;
    let mut process = if use_macos_node_launcher {
        Command::new(macos_node_executable())
    } else {
        Command::new(command)
    };
    if let Some(args) = server.server.get("args").and_then(Value::as_array) {
        process.args(args.iter().filter_map(Value::as_str));
    }
    if let Some(env) = server.server.get("env").and_then(Value::as_object) {
        process.envs(
            env.iter()
                .filter_map(|(key, value)| value.as_str().map(|value| (key, value))),
        );
    }
    if let Some(cwd) = server.server.get("cwd").and_then(Value::as_str) {
        process.current_dir(cwd);
    }
    let mut child = process
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("Cannot start MCP stdio server: {error}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Cannot open MCP stdin.".to_string())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Cannot open MCP stdout.".to_string())?;
    let mut client = StdioRpcClient {
        stdin,
        lines: BufReader::new(stdout).lines(),
        next_id: 1,
    };

    let result = async {
        client
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "q-switch-qoder", "version": "0.1"}
                }),
            )
            .await?;
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        client.request(method, params).await
    };
    let result = timeout(MCP_TIMEOUT, result).await;
    let _ = child.kill().await;
    result.map_err(|_| "MCP stdio request timed out.".to_string())?
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn macos_node_executable() -> &'static str {
    "/opt/homebrew/bin/node"
}

#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
fn macos_node_executable() -> &'static str {
    "/usr/local/bin/node"
}

struct StdioRpcClient {
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
}

impl StdioRpcClient {
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let Some(line) = self
                .lines
                .next_line()
                .await
                .map_err(|error| format!("Cannot read MCP response: {error}"))?
            else {
                return Err("MCP server closed its stdout before replying.".to_string());
            };
            if line.len() > MAX_MCP_RESPONSE_BYTES {
                return Err("MCP response is too large.".to_string());
            }
            let response: Value = serde_json::from_str(&line)
                .map_err(|_| "MCP server emitted invalid JSON-RPC.".to_string())?;
            if response.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = response.get("error") {
                return Err(format!("MCP error: {}", concise_json(error)));
            }
            return response
                .get("result")
                .cloned()
                .ok_or_else(|| "MCP response has no result.".to_string());
        }
    }

    async fn send(&mut self, value: Value) -> Result<(), String> {
        let mut line = serde_json::to_vec(&value)
            .map_err(|error| format!("Cannot encode MCP request: {error}"))?;
        line.push(b'\n');
        self.stdin
            .write_all(&line)
            .await
            .map_err(|error| format!("Cannot write MCP request: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("Cannot flush MCP request: {error}"))
    }
}

async fn rpc_http(server: &McpServer, method: &str, params: Value) -> Result<Value, String> {
    let url = server
        .server
        .get("url")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "MCP HTTP server has no URL.".to_string())?;
    let client = reqwest::Client::builder()
        .timeout(MCP_TIMEOUT)
        .build()
        .map_err(|error| error.to_string())?;
    let mut session = HttpRpcClient {
        client,
        url,
        headers: headers_from_server(server)?,
        session_id: None,
        next_id: 1,
    };
    session
        .request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "q-switch-qoder", "version": "0.1"}
            }),
        )
        .await?;
    session
        .notify("notifications/initialized", json!({}))
        .await?;
    session.request(method, params).await
}

struct HttpRpcClient<'a> {
    client: reqwest::Client,
    url: &'a str,
    headers: reqwest::header::HeaderMap,
    session_id: Option<reqwest::header::HeaderValue>,
    next_id: u64,
}

impl HttpRpcClient<'_> {
    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let mut request = self
            .client
            .post(self.url)
            .headers(self.headers.clone())
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
        if let Some(session_id) = &self.session_id {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("MCP HTTP notification failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "MCP HTTP notification returned {}",
                response.status()
            ));
        }
        if let Some(session_id) = response.headers().get("Mcp-Session-Id") {
            self.session_id = Some(session_id.clone());
        }
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        if response.get("id").and_then(Value::as_u64) != Some(id) {
            return Err("MCP HTTP server returned a mismatched response ID.".to_string());
        }
        if let Some(error) = response.get("error") {
            return Err(format!("MCP error: {}", concise_json(error)));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| "MCP response has no result.".to_string())
    }

    async fn send(&mut self, value: Value) -> Result<Value, String> {
        let mut request = self
            .client
            .post(self.url)
            .headers(self.headers.clone())
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .json(&value);
        if let Some(session_id) = &self.session_id {
            request = request.header("Mcp-Session-Id", session_id);
        }
        let response = request
            .send()
            .await
            .map_err(|error| format!("MCP HTTP request failed: {error}"))?;
        if !response.status().is_success() {
            return Err(format!("MCP HTTP request returned {}", response.status()));
        }
        if let Some(session_id) = response.headers().get("Mcp-Session-Id") {
            self.session_id = Some(session_id.clone());
        }
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| format!("Cannot read MCP HTTP response: {error}"))?;
            if body.len().saturating_add(chunk.len()) > MAX_MCP_RESPONSE_BYTES {
                return Err("MCP HTTP response is too large.".to_string());
            }
            body.extend_from_slice(&chunk);
        }
        parse_http_jsonrpc(&String::from_utf8_lossy(&body))
    }
}

fn headers_from_server(server: &McpServer) -> Result<reqwest::header::HeaderMap, String> {
    let mut headers = reqwest::header::HeaderMap::new();
    let Some(values) = server.server.get("headers").and_then(Value::as_object) else {
        return Ok(headers);
    };
    for (name, value) in values {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "MCP header name is invalid.".to_string())?;
        let value = value
            .as_str()
            .ok_or_else(|| "MCP header values must be strings.".to_string())?;
        let value = reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| "MCP header value is invalid.".to_string())?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn parse_http_jsonrpc(body: &str) -> Result<Value, String> {
    if let Ok(value) = serde_json::from_str(body) {
        return Ok(value);
    }
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            if let Ok(value) = serde_json::from_str(data.trim()) {
                return Ok(value);
            }
        }
    }
    Err("MCP HTTP server emitted invalid JSON-RPC.".to_string())
}

fn render_mcp_result(result: &Value) -> String {
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return concise_json(result);
    };
    let text = content
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    if text.is_empty() {
        concise_json(result)
    } else {
        truncate_text(&text)
    }
}

fn concise_json(value: &Value) -> String {
    truncate_text(
        &serde_json::to_string(value).unwrap_or_else(|_| "MCP result unavailable".to_string()),
    )
}
fn truncate_text(value: &str) -> String {
    value.chars().take(MAX_MCP_RESPONSE_BYTES).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_stdio_server() -> McpServer {
        // The test server implements just enough line-delimited MCP JSON-RPC
        // to exercise initialize -> tools/list and initialize -> tools/call.
        let script = r#"
const rl = require('readline').createInterface({ input: process.stdin });
rl.on('line', (line) => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  let result = {};
  if (request.method === 'tools/list') {
    result = { tools: [{ name: 'echo', description: 'Echo test input', inputSchema: { type: 'object', properties: { text: { type: 'string' } } } }] };
  } else if (request.method === 'tools/call') {
    result = { content: [{ type: 'text', text: request.params.arguments.text }] };
  }
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result }) + '\n');
});
"#;
        McpServer {
            id: "test-server".to_string(),
            name: "Test server".to_string(),
            server: json!({"type": "stdio", "command": "node", "args": ["-e", script]}),
            apps: Default::default(),
            description: None,
            homepage: None,
            docs: None,
            tags: Vec::new(),
        }
    }

    #[test]
    fn generated_function_names_are_stable_and_safe() {
        assert_eq!(
            function_name("server with spaces", "a/tool"),
            "mcp__server_with_spaces__a_tool"
        );
    }

    #[test]
    fn parses_json_and_sse_http_responses() {
        assert_eq!(
            parse_http_jsonrpc(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap()["id"],
            1
        );
        assert_eq!(
            parse_http_jsonrpc(
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n"
            )
            .unwrap()["id"],
            2
        );
    }

    #[tokio::test]
    async fn stdio_mcp_discovery_and_call_use_the_protocol_handshake() {
        let server = test_stdio_server();
        let tools = discover_server_tools(&server).await.unwrap();
        assert_eq!(tools[0]["name"], "echo");
        let result = call_server_tool(&server, "echo", json!({"text": "hello"}))
            .await
            .unwrap();
        assert_eq!(render_mcp_result(&result), "hello");
    }
}
