//! Workspace and explicitly authorized local tools for Qoder custom models.
//!
//! The Qoder Agent normally owns this authority. Routed Q Switch sessions use
//! this small runtime instead, so a tool is advertised only after the local
//! user enables its corresponding capability in the Qoder panel.

use crate::qoder_acp::mcp_runtime::{McpRuntime, McpToolDefinition};
use crate::qoder_config::QoderToolPolicy;
use futures::StreamExt;
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const MAX_LIST_ENTRIES: usize = 200;
const MAX_FILE_BYTES: usize = 128 * 1024;
const MAX_WRITE_BYTES: usize = 512 * 1024;
const MAX_FILE_LINES: usize = 400;
const MAX_SEARCH_RESULTS: usize = 100;
const MAX_SEARCH_FILE_BYTES: u64 = 256 * 1024;
const MAX_SEARCH_DEPTH: usize = 8;
const MAX_TERMINAL_OUTPUT_BYTES: usize = 128 * 1024;
const MAX_NETWORK_BODY_BYTES: usize = 256 * 1024;
const DEFAULT_TERMINAL_TIMEOUT_MS: u64 = 30_000;
const MAX_TERMINAL_TIMEOUT_MS: u64 = 120_000;

/// A result that is safe to display in Qoder and to send back to the model.
pub struct ToolExecution {
    pub model_content: String,
    /// Compact structured output rendered by Qoder's tool UI. The full tool
    /// output goes only to the next model turn, not the activity feed.
    pub display_output: Value,
    pub success: bool,
}

/// OpenAI-compatible functions offered to a routed custom model.
pub fn definitions(policy: &QoderToolPolicy, mcp_tools: &[McpToolDefinition]) -> Vec<Value> {
    let mut tools = vec![
        json!({"type":"function","function":{"name":"list_dir","description":"List entries in the current Qoder workspace. Only workspace-relative paths are allowed.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Optional workspace-relative directory path."}},"additionalProperties":false}}}),
        json!({"type":"function","function":{"name":"read_file","description":"Read a UTF-8 text file in the current Qoder workspace. Only workspace-relative paths are allowed. The result is line-limited.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative file path."},"start_line":{"type":"integer","minimum":1},"end_line":{"type":"integer","minimum":1}},"required":["path"],"additionalProperties":false}}}),
        json!({"type":"function","function":{"name":"grep_code","description":"Search UTF-8 text files in the current Qoder workspace for a literal query. Only workspace-relative paths are allowed.","parameters":{"type":"object","properties":{"query":{"type":"string","minLength":1},"path":{"type":"string","description":"Optional workspace-relative directory path."}},"required":["query"],"additionalProperties":false}}}),
    ];
    if policy.allow_write {
        tools.push(json!({"type":"function","function":{"name":"write_file","description":"Create or replace a UTF-8 text file inside the current Qoder workspace. Parent directories may be created. This is enabled by the local Q Switch user.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Workspace-relative file path."},"content":{"type":"string"},"overwrite":{"type":"boolean","description":"Whether an existing file may be replaced. Default true."}},"required":["path","content"],"additionalProperties":false}}}));
    }
    if policy.allow_terminal {
        tools.push(json!({"type":"function","function":{"name":"run_terminal","description":"Run a shell command with the current workspace as its working directory. This runs under the local macOS/Windows user account and is enabled explicitly by the local Q Switch user.","parameters":{"type":"object","properties":{"command":{"type":"string"},"cwd":{"type":"string","description":"Optional workspace-relative directory."},"timeout_ms":{"type":"integer","minimum":1,"maximum":120000}},"required":["command"],"additionalProperties":false}}}));
    }
    if policy.allow_network {
        tools.push(json!({"type":"function","function":{"name":"http_request","description":"Make a bounded HTTP/HTTPS request. Local/private network destinations are unavailable unless the local Q Switch user separately permits them.","parameters":{"type":"object","properties":{"url":{"type":"string","format":"uri"},"method":{"type":"string","enum":["GET","HEAD","POST"]},"headers":{"type":"object","additionalProperties":{"type":"string"}},"body":{"type":"string"}},"required":["url"],"additionalProperties":false}}}));
    }
    if policy.allow_mcp {
        tools.extend(mcp_tools.iter().map(McpToolDefinition::as_openai_tool));
    }
    tools
}

/// Qoder UI activity classification for the available tool set.
pub fn qoder_kind(name: &str) -> &'static str {
    match name {
        "list_dir" => "list",
        "read_file" => "read",
        "grep_code" => "search",
        "write_file" => "edit",
        "run_terminal" => "terminal",
        "http_request" => "network",
        name if name.starts_with("mcp__") => "mcp",
        _ => "other",
    }
}

pub async fn execute(
    workspace_root: &Path,
    name: &str,
    arguments: &str,
    policy: &QoderToolPolicy,
    mcp_runtime: &McpRuntime,
) -> ToolExecution {
    let parsed = serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({}));
    let result = match name {
        "list_dir" => list_dir(workspace_root, &parsed),
        "read_file" => read_file(workspace_root, &parsed),
        "grep_code" => grep_code(workspace_root, &parsed),
        "write_file" if policy.allow_write => write_file(workspace_root, &parsed),
        "write_file" => {
            Err("Writing files is disabled in Q Switch for this Qoder route.".to_string())
        }
        "run_terminal" if policy.allow_terminal => run_terminal(workspace_root, &parsed).await,
        "run_terminal" => {
            Err("Terminal access is disabled in Q Switch for this Qoder route.".to_string())
        }
        "http_request" if policy.allow_network => http_request(&parsed, policy).await,
        "http_request" => {
            Err("Network access is disabled in Q Switch for this Qoder route.".to_string())
        }
        name if name.starts_with("mcp__") && policy.allow_mcp => {
            mcp_runtime.call(name, parsed).await
        }
        name if name.starts_with("mcp__") => {
            Err("MCP access is disabled in Q Switch for this Qoder route.".to_string())
        }
        _ => Err(format!("Unsupported Q Switch tool: {name}")),
    };
    match result {
        Ok((content, summary)) => ToolExecution {
            model_content: content,
            display_output: json!({"summary": summary}),
            success: true,
        },
        Err(message) => ToolExecution {
            model_content: format!("Tool error: {message}"),
            display_output: json!({"error": message}),
            success: false,
        },
    }
}

fn list_dir(root: &Path, args: &Value) -> Result<(String, String), String> {
    let directory = resolve_path(root, optional_path(args), true)?;
    let mut entries = fs::read_dir(&directory)
        .map_err(|error| format!("Cannot list directory: {error}"))?
        .filter_map(Result::ok)
        .filter(|entry| !should_skip(&entry.file_name().to_string_lossy()))
        .map(|entry| {
            let kind = entry
                .file_type()
                .ok()
                .map(|kind| if kind.is_dir() { "dir" } else { "file" })
                .unwrap_or("unknown");
            format!("{kind}: {}", display_relative(root, &entry.path()))
        })
        .collect::<Vec<_>>();
    entries.sort();
    let truncated = entries.len() > MAX_LIST_ENTRIES;
    entries.truncate(MAX_LIST_ENTRIES);
    let content = if entries.is_empty() {
        "(directory is empty)".to_string()
    } else {
        entries.join("\n")
    };
    Ok((
        content,
        format!(
            "Listed {} entries{}",
            entries.len(),
            if truncated { " (truncated)" } else { "" }
        ),
    ))
}

fn read_file(root: &Path, args: &Value) -> Result<(String, String), String> {
    let file = resolve_path(root, Some(required_string(args, "path")?), false)?;
    let metadata = fs::metadata(&file).map_err(|error| format!("Cannot inspect file: {error}"))?;
    if !metadata.is_file() {
        return Err("Requested path is not a regular file".to_string());
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(format!(
            "Refusing to read file larger than {MAX_FILE_BYTES} bytes"
        ));
    }
    let text = fs::read_to_string(&file).map_err(|_| "File is not valid UTF-8 text".to_string())?;
    let start = args.get("start_line").and_then(Value::as_u64).unwrap_or(1) as usize;
    if start == 0 {
        return Err("start_line must be at least 1".to_string());
    }
    let end = args
        .get("end_line")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or_else(|| start.saturating_add(MAX_FILE_LINES).saturating_sub(1))
        .min(start.saturating_add(MAX_FILE_LINES).saturating_sub(1));
    let selected = text
        .lines()
        .enumerate()
        .filter(|(index, _)| {
            let line = index + 1;
            line >= start && line <= end
        })
        .map(|(index, line)| format!("{:>5}: {line}", index + 1))
        .collect::<Vec<_>>();
    let content = if selected.is_empty() {
        "(no lines in requested range)".to_string()
    } else {
        selected.join("\n")
    };
    Ok((
        content,
        format!(
            "Read {} lines from {}",
            selected.len(),
            display_relative(root, &file)
        ),
    ))
}

fn grep_code(root: &Path, args: &Value) -> Result<(String, String), String> {
    let query = required_string(args, "query")?;
    if query.len() > 512 {
        return Err("Search query is too long".to_string());
    }
    let directory = resolve_path(root, optional_path(args), true)?;
    let mut results = Vec::new();
    search_directory(root, &directory, query, 0, &mut results)?;
    let truncated = results.len() > MAX_SEARCH_RESULTS;
    results.truncate(MAX_SEARCH_RESULTS);
    let content = if results.is_empty() {
        "(no matches)".to_string()
    } else {
        results.join("\n")
    };
    Ok((
        content,
        format!(
            "Found {} matches{}",
            results.len(),
            if truncated { " (truncated)" } else { "" }
        ),
    ))
}

fn write_file(root: &Path, args: &Value) -> Result<(String, String), String> {
    let requested = required_string(args, "path")?;
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "Missing required string parameter: content".to_string())?;
    if content.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "Refusing to write more than {MAX_WRITE_BYTES} bytes"
        ));
    }
    let target = resolve_write_path(root, requested)?;
    if target.exists()
        && !args
            .get("overwrite")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    {
        return Err("Refusing to overwrite an existing file".to_string());
    }
    if target.exists()
        && fs::metadata(&target)
            .map_err(|error| error.to_string())?
            .is_dir()
    {
        return Err("Requested path is a directory".to_string());
    }
    let parent = target
        .parent()
        .ok_or_else(|| "Workspace file has no parent directory".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("Cannot create parent directories: {error}"))?;
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("Workspace root is unavailable: {error}"))?;
    let canonical_parent = parent
        .canonicalize()
        .map_err(|error| format!("Cannot validate parent directory: {error}"))?;
    if !canonical_parent.starts_with(&canonical_root) {
        return Err("Requested path escapes the current workspace".to_string());
    }
    let file_name = target
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "Invalid workspace file name".to_string())?;
    let temporary =
        canonical_parent.join(format!(".{file_name}.qswitch-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| format!("Cannot create temporary file: {error}"))?;
    file.write_all(content.as_bytes())
        .map_err(|error| format!("Cannot write file: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("Cannot flush file: {error}"))?;
    fs::rename(&temporary, &target).map_err(|error| format!("Cannot replace file: {error}"))?;
    Ok((
        format!(
            "Wrote {} bytes to {}",
            content.len(),
            display_relative(root, &target)
        ),
        format!("Wrote {}", display_relative(root, &target)),
    ))
}

async fn run_terminal(root: &Path, args: &Value) -> Result<(String, String), String> {
    let command = required_string(args, "command")?;
    if command.len() > 16 * 1024 {
        return Err("Terminal command is too long".to_string());
    }
    let cwd = resolve_path(root, optional_string(args, "cwd"), true)?;
    let timeout_ms = args
        .get("timeout_ms")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TERMINAL_TIMEOUT_MS)
        .clamp(1, MAX_TERMINAL_TIMEOUT_MS);
    let mut process = if cfg!(windows) {
        let mut command_process = Command::new("cmd");
        command_process.args(["/C", command]);
        command_process
    } else {
        let mut command_process = Command::new("sh");
        command_process.args(["-lc", command]);
        command_process
    };
    let mut child = process
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Cannot start terminal command: {error}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Cannot capture terminal stdout".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Cannot capture terminal stderr".to_string())?;
    let stdout_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let stderr_task = tokio::spawn(async move {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).await.map(|_| bytes)
    });
    let (status, timed_out) = tokio::select! {
        status = child.wait() => (status.map_err(|error| format!("Cannot wait for terminal command: {error}"))?, false),
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => { let _ = child.kill().await; (child.wait().await.map_err(|error| format!("Cannot stop terminal command: {error}"))?, true) }
    };
    let stdout = stdout_task
        .await
        .map_err(|_| "Cannot collect terminal stdout".to_string())?
        .map_err(|error| format!("Cannot collect terminal stdout: {error}"))?;
    let stderr = stderr_task
        .await
        .map_err(|_| "Cannot collect terminal stderr".to_string())?
        .map_err(|error| format!("Cannot collect terminal stderr: {error}"))?;
    let mut output = String::new();
    if !stdout.is_empty() {
        output.push_str(&String::from_utf8_lossy(&stdout));
    }
    if !stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&String::from_utf8_lossy(&stderr));
    }
    if output.len() > MAX_TERMINAL_OUTPUT_BYTES {
        output.truncate(MAX_TERMINAL_OUTPUT_BYTES);
        output.push_str("\n(output truncated)");
    }
    if timed_out {
        return Err(format!(
            "Terminal command timed out after {timeout_ms} ms{}",
            if output.is_empty() {
                String::new()
            } else {
                format!(":\n{output}")
            }
        ));
    }
    let code = status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    if !status.success() {
        return Err(format!(
            "Terminal command exited with {code}{}",
            if output.is_empty() { "" } else { ":\n" }
        ) + &output);
    }
    Ok((
        if output.is_empty() {
            "(command completed with no output)".to_string()
        } else {
            output
        },
        format!("Terminal completed in {}", display_relative(root, &cwd)),
    ))
}

async fn http_request(args: &Value, policy: &QoderToolPolicy) -> Result<(String, String), String> {
    let raw_url = required_string(args, "url")?;
    let url = url::Url::parse(raw_url).map_err(|_| "Invalid HTTP URL".to_string())?;
    if !matches!(url.scheme(), "http" | "https") || url.username() != "" || url.password().is_some()
    {
        return Err("Only credential-free http/https URLs are allowed".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "HTTP URL has no host".to_string())?;
    if !policy.allow_private_network {
        reject_private_destination(host, url.port_or_known_default().unwrap_or(443)).await?;
    }
    let method = args.get("method").and_then(Value::as_str).unwrap_or("GET");
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|_| "Invalid HTTP method".to_string())?;
    if method != reqwest::Method::GET
        && method != reqwest::Method::HEAD
        && method != reqwest::Method::POST
    {
        return Err("Only GET, HEAD, and POST are allowed".to_string());
    }
    // Validate only the explicitly requested destination. Following a
    // redirect would let a public URL bounce into a blocked local/private
    // address after validation, so return 3xx responses to the model instead.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| format!("Cannot create HTTP client: {error}"))?;
    let mut request = client.request(method.clone(), url.clone());
    if let Some(headers) = args.get("headers").and_then(Value::as_object) {
        if headers.len() > 32 {
            return Err("Too many HTTP headers".to_string());
        }
        for (name, value) in headers {
            if matches!(
                name.to_ascii_lowercase().as_str(),
                "host" | "content-length" | "connection"
            ) {
                return Err("That HTTP header is not allowed".to_string());
            }
            let value = value
                .as_str()
                .ok_or_else(|| "HTTP header values must be strings".to_string())?;
            request = request.header(name, value);
        }
    }
    if let Some(body) = args.get("body").and_then(Value::as_str) {
        if body.len() > MAX_NETWORK_BODY_BYTES {
            return Err("HTTP request body is too large".to_string());
        }
        request = request.body(body.to_string());
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("HTTP request failed: {error}"))?;
    let status = response.status();
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("Cannot read HTTP response: {error}"))?;
        let remaining = MAX_NETWORK_BODY_BYTES.saturating_sub(body.len());
        if remaining == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
    let mut content = String::from_utf8_lossy(&body).into_owned();
    if body.len() == MAX_NETWORK_BODY_BYTES {
        content.push_str("\n(response truncated)");
    }
    Ok((
        if content.is_empty() {
            "(empty response body)".to_string()
        } else {
            content
        },
        format!("{} {}", status.as_u16(), url.origin().ascii_serialization()),
    ))
}

async fn reject_private_destination(host: &str, port: u16) -> Result<(), String> {
    let host_lower = host.to_ascii_lowercase();
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
    {
        return Err("Local network destinations are disabled".to_string());
    }
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![ip]
    } else {
        tokio::time::timeout(
            Duration::from_secs(5),
            tokio::net::lookup_host((host, port)),
        )
        .await
        .map_err(|_| "Cannot validate network destination".to_string())?
        .map_err(|_| "Cannot resolve network destination".to_string())?
        .map(|address| address.ip())
        .collect::<Vec<_>>()
    };
    if addresses.is_empty() || addresses.into_iter().any(is_private_ip) {
        return Err(
            "Private, loopback, and link-local network destinations are disabled".to_string(),
        );
    }
    Ok(())
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_unspecified()
                || ip.is_multicast()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip.is_multicast()
        }
    }
}

fn search_directory(
    root: &Path,
    directory: &Path,
    query: &str,
    depth: usize,
    results: &mut Vec<String>,
) -> Result<(), String> {
    if depth > MAX_SEARCH_DEPTH || results.len() > MAX_SEARCH_RESULTS {
        return Ok(());
    }
    for entry in fs::read_dir(directory)
        .map_err(|error| format!("Cannot search directory: {error}"))?
        .filter_map(Result::ok)
    {
        if results.len() > MAX_SEARCH_RESULTS {
            break;
        }
        if should_skip(&entry.file_name().to_string_lossy()) {
            continue;
        }
        let path = entry.path();
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            search_directory(root, &path, query, depth + 1, results)?;
        } else if file_type.is_file()
            && entry
                .metadata()
                .map(|metadata| metadata.len() <= MAX_SEARCH_FILE_BYTES)
                .unwrap_or(false)
        {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if line.contains(query) {
                    results.push(format!(
                        "{}:{}: {}",
                        display_relative(root, &path),
                        index + 1,
                        line
                    ));
                    if results.len() > MAX_SEARCH_RESULTS {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

fn optional_path(args: &Value) -> Option<&str> {
    args.get("path")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}
fn optional_string<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}
fn required_string<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    optional_string(args, key).ok_or_else(|| format!("Missing required string parameter: {key}"))
}

fn resolve_path(
    root: &Path,
    requested: Option<&str>,
    expect_directory: bool,
) -> Result<PathBuf, String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("Workspace root is unavailable: {error}"))?;
    let relative = Path::new(requested.unwrap_or("."));
    validate_relative(relative)?;
    let target = canonical_root
        .join(relative)
        .canonicalize()
        .map_err(|error| format!("Requested workspace path is unavailable: {error}"))?;
    if !target.starts_with(&canonical_root) {
        return Err("Requested path escapes the current workspace".to_string());
    }
    if expect_directory && !target.is_dir() {
        return Err("Requested path is not a directory".to_string());
    }
    Ok(target)
}

fn resolve_write_path(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let canonical_root = root
        .canonicalize()
        .map_err(|error| format!("Workspace root is unavailable: {error}"))?;
    let relative = Path::new(requested);
    validate_relative(relative)?;
    let target = canonical_root.join(relative);
    if target.exists() {
        let target = target
            .canonicalize()
            .map_err(|error| format!("Requested workspace path is unavailable: {error}"))?;
        if !target.starts_with(&canonical_root) {
            return Err("Requested path escapes the current workspace".to_string());
        }
        return Ok(target);
    }
    let mut ancestor = target.parent();
    while let Some(parent) = ancestor {
        if parent.exists() {
            let canonical_parent = parent
                .canonicalize()
                .map_err(|error| format!("Cannot validate workspace path: {error}"))?;
            if !canonical_parent.starts_with(&canonical_root) {
                return Err("Requested path escapes the current workspace".to_string());
            }
            break;
        }
        ancestor = parent.parent();
    }
    Ok(target)
}

fn validate_relative(relative: &Path) -> Result<(), String> {
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        Err("Only workspace-relative paths are allowed".to_string())
    } else {
        Ok(())
    }
}
fn display_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}
fn should_skip(name: &str) -> bool {
    matches!(name, ".git" | "node_modules" | "target" | ".DS_Store")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_runtime() -> McpRuntime {
        McpRuntime::new(std::sync::Arc::new(crate::database::Database {
            conn: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        }))
    }

    #[test]
    fn power_tools_are_hidden_until_explicitly_enabled() {
        let policy = QoderToolPolicy::default();
        let names = definitions(&policy, &[])
            .into_iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["list_dir", "read_file", "grep_code"]);
        let policy = QoderToolPolicy {
            allow_terminal: true,
            allow_write: true,
            allow_network: true,
            allow_private_network: false,
            allow_mcp: false,
        };
        let names = definitions(&policy, &[])
            .into_iter()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_string))
            .collect::<Vec<_>>();
        assert!(
            names.contains(&"write_file".to_string())
                && names.contains(&"run_terminal".to_string())
                && names.contains(&"http_request".to_string())
        );
    }

    #[tokio::test]
    async fn reads_writes_and_blocks_path_traversal() {
        let dir = tempdir().unwrap();
        let policy = QoderToolPolicy {
            allow_terminal: false,
            allow_write: true,
            allow_network: false,
            allow_private_network: false,
            allow_mcp: false,
        };
        let runtime = test_runtime();
        let output = execute(
            dir.path(),
            "write_file",
            r#"{"path":"nested/sample.txt","content":"one\ntwo"}"#,
            &policy,
            &runtime,
        )
        .await;
        assert!(output.success);
        assert_eq!(
            fs::read_to_string(dir.path().join("nested/sample.txt")).unwrap(),
            "one\ntwo"
        );
        let output = execute(
            dir.path(),
            "write_file",
            r#"{"path":"../outside.txt","content":"no"}"#,
            &policy,
            &runtime,
        )
        .await;
        assert!(!output.success);
    }

    #[tokio::test]
    async fn terminal_starts_in_workspace_and_has_a_timeout() {
        if cfg!(windows) {
            return;
        }
        let dir = tempdir().unwrap();
        let policy = QoderToolPolicy {
            allow_terminal: true,
            ..QoderToolPolicy::default()
        };
        let runtime = test_runtime();
        let output = execute(
            dir.path(),
            "run_terminal",
            r#"{"command":"printf qswitch-terminal"}"#,
            &policy,
            &runtime,
        )
        .await;
        assert!(output.success);
        assert!(output.model_content.contains("qswitch-terminal"));
    }

    #[tokio::test]
    async fn private_network_destinations_are_rejected_by_default() {
        let dir = tempdir().unwrap();
        let runtime = test_runtime();
        let policy = QoderToolPolicy {
            allow_network: true,
            ..QoderToolPolicy::default()
        };
        let output = execute(
            dir.path(),
            "http_request",
            r#"{"url":"http://127.0.0.1/"}"#,
            &policy,
            &runtime,
        )
        .await;
        assert!(!output.success);
    }
}
