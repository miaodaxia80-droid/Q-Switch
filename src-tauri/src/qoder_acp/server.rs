//! Qoder ACP WebSocket server.
//!
//! Listens on `127.0.0.1:0` (random port) and accepts WebSocket connections
//! from Qoder's Electron UI.  Each connection reads LSP-framed JSON-RPC
//! messages and dispatches them to the ACP handler.

use crate::qoder_acp::handler::{self, AcpHandlerState};
use crate::qoder_acp::info_writer;
use crate::qoder_acp::lsp_framer::LspFramer;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

/// Running ACP server handle.
pub struct AcpServerHandle {
    pub port: u16,
    pub shutdown_tx: oneshot::Sender<()>,
    pub join_handle: Option<JoinHandle<()>>,
    pub pid: u32,
}

/// Start the ACP WebSocket server on a random localhost port.
///
/// Writes `.info.json` on success so Qoder can discover the port.
pub async fn start(
    db: Arc<crate::database::Database>,
) -> Result<AcpServerHandle, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("Failed to bind ACP WebSocket: {e}"))?;

    let port = listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("Failed to get local addr: {e}"))?;

    let pid = std::process::id();

    // Write .info.json so Qoder can find us.
    info_writer::write_info(port, pid).map_err(|e| format!("Failed to write .info.json: {e}"))?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handler_state = Arc::new(AcpHandlerState::new(db));

    let handle = tokio::spawn(async move {
        let mut shutdown_rx = shutdown_rx;
        log::info!("[qoder_acp] WebSocket server listening on 127.0.0.1:{port}");

        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (stream, addr) = match result {
                        Ok(s) => s,
                        Err(e) => {
                            log::error!("[qoder_acp] accept error: {e}");
                            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                            continue;
                        }
                    };

                    let state = handler_state.clone();
                    tokio::spawn(async move {
                        log::debug!("[qoder_acp] connection from {addr}");
                        if let Err(e) = handle_connection(stream, state).await {
                            log::warn!("[qoder_acp] connection error: {e}");
                        }
                    });
                }
                _ = &mut shutdown_rx => {
                    log::info!("[qoder_acp] shutting down WebSocket server");
                    break;
                }
            }
        }

        // Clean up .info.json on shutdown.
        info_writer::clean_info(pid);
    });

    Ok(AcpServerHandle {
        port,
        shutdown_tx,
        join_handle: Some(handle),
        pid,
    })
}

/// Stop the ACP server.
pub async fn stop(handle: Option<AcpServerHandle>) -> Result<(), String> {
    if let Some(mut h) = handle {
        let _ = h.shutdown_tx.send(());
        if let Some(jh) = h.join_handle.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(3), jh).await {
                Ok(Ok(())) => log::info!("[qoder_acp] server stopped cleanly"),
                Ok(Err(e)) => log::warn!("[qoder_acp] server task error: {e}"),
                Err(_) => log::warn!("[qoder_acp] server stop timed out"),
            }
        }
        // Clean up .info.json in case the task didn't.
        info_writer::clean_info(h.pid);
    }
    Ok(())
}

/// Handle a single WebSocket connection.
async fn handle_connection(
    stream: tokio::net::TcpStream,
    state: Arc<AcpHandlerState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let ws_stream = accept_async(stream).await?;

    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // Channel for outbound messages (handler -> WebSocket sender).
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

    // Spawn a writer task that forwards framed messages to the WebSocket.
    let writer_task = tokio::spawn(async move {
        while let Some(framed) = rx.recv().await {
            if ws_sender.send(Message::Binary(framed.into())).await.is_err() {
                break;
            }
        }
    });

    let mut framer = LspFramer::new();

    // Read loop.
    while let Some(msg_result) = ws_receiver.next().await {
        let msg = match msg_result {
            Ok(m) => m,
            Err(e) => {
                log::debug!("[qoder_acp] WebSocket read error: {e}");
                break;
            }
        };

        match msg {
            Message::Text(text) => {
                let frames = match framer.append(text.as_bytes()) {
                    Ok(f) => f,
                    Err(e) => {
                        log::warn!("[qoder_acp] LSP frame parse error: {e}");
                        break;
                    }
                };
                for frame in frames {
                    process_frame(&state, &frame, &tx).await;
                }
            }
            Message::Binary(data) => {
                let frames = match framer.append(&data) {
                    Ok(f) => f,
                    Err(e) => {
                        log::warn!("[qoder_acp] LSP frame parse error: {e}");
                        break;
                    }
                };
                for frame in frames {
                    process_frame(&state, &frame, &tx).await;
                }
            }
            Message::Ping(_) => {
                // tokio-tungstenite auto-replies to pings by default.
            }
            Message::Close(_) => {
                log::debug!("[qoder_acp] client closed connection");
                break;
            }
            _ => {}
        }
    }

    // Drop the sender to stop the writer task.
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

/// Parse a single LSP frame as JSON-RPC and dispatch to the handler.
async fn process_frame(
    state: &AcpHandlerState,
    frame: &[u8],
    tx: &mpsc::UnboundedSender<Vec<u8>>,
) {
    let message: Value = match serde_json::from_slice(frame) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("[qoder_acp] failed to parse JSON-RPC: {e}");
            return;
        }
    };

    log::debug!(
        "[qoder_acp] dispatching method: {}",
        message.get("method").and_then(|v| v.as_str()).unwrap_or("?")
    );

    handler::handle_message(state, message, tx).await;
}
