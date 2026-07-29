//! Qoder ACP (Agent Control Protocol) adapter module.
//!
//! Provides a local WebSocket server that speaks the LSP/ACP protocol
//! Qoder's Electron UI expects, translating between ACP sessions and
//! OpenAI Chat Completions (streaming) via CC Switch's provider database.
//!
//! Architecture:
//! ```text
//! Qoder Electron UI
//!   -> ws://127.0.0.1:<port>  (WebSocket + LSP frames)
//!   -> handler (ACP JSON-RPC dispatch)
//!   -> chat_client (OpenAI SSE streaming)
//!   -> wire (ACP session/update events back to UI)
//! ```

pub mod chat_client;
pub mod handler;
pub mod info_writer;
pub mod lsp_framer;
pub mod mcp_runtime;
pub mod proxy;
pub mod session;
pub mod tools;
pub mod wire;

pub use proxy::{start_native_proxy, stop_native_proxy, NativeProxyHandle, NativeProxyStatus};
