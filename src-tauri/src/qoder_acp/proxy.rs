//! Transparent Qoder native-Agent transport adapter.
//!
//! Qoder's Electron UI discovers its local Agent through `.info.json` and
//! speaks WebSocket + LSP-framed JSON-RPC. Replacing that Agent would break
//! official Qoder models. The desktop path therefore publishes a temporary,
//! Q Switch-owned discovery endpoint and forwards every non-Q-Switch frame
//! byte-for-byte at the JSON-RPC level. It reclaims native discovery refreshes
//! while active and restores the latest native record when stopped. Only
//! explicitly mapped carrier sessions are answered locally through Q Switch's
//! `/qoder/v1` gateway.

use crate::database::Database;
use crate::qoder_acp::handler::{self, AcpHandlerState};
use crate::qoder_acp::info_writer::NativeInfoSnapshot;
use crate::qoder_acp::lsp_framer::LspFramer;
use crate::qoder_acp::wire;
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener, UnixStream};
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeProxyStatus {
    pub active: bool,
    pub adapter_port: Option<u16>,
    pub adapter_ipc_path: Option<String>,
    pub native_port: Option<u16>,
    pub native_pid: Option<u32>,
    /// Recently selected, unmapped Qoder native BYOK record IDs. These are
    /// non-secret identifiers retained in memory only for the current adapter
    /// process, so the user can create an explicit carrier mapping without
    /// reading Qoder internals or logs.
    pub observed_custom_model_ids: Vec<String>,
}

/// Running adapter handle. It owns the native discovery-file snapshot needed
/// to restore Qoder atomically when the user disables the adapter.
pub struct NativeProxyHandle {
    adapter_port: u16,
    adapter_ipc_path: PathBuf,
    endpoint: Arc<NativeEndpoint>,
    adapter_pid: u32,
    latest_snapshot: Arc<Mutex<NativeInfoSnapshot>>,
    observed_custom_model_ids: ObservedCustomModelIds,
    info_path: PathBuf,
    shutdown_tx: watch::Sender<bool>,
    join_handle: Option<JoinHandle<()>>,
    monitor_handle: Option<JoinHandle<()>>,
}

impl NativeProxyHandle {
    pub fn status(&self) -> NativeProxyStatus {
        NativeProxyStatus {
            active: true,
            adapter_port: Some(self.adapter_port),
            adapter_ipc_path: Some(self.adapter_ipc_path.to_string_lossy().into_owned()),
            native_port: Some(self.endpoint.port.load(Ordering::Relaxed)),
            native_pid: Some(self.endpoint.pid.load(Ordering::Relaxed)),
            observed_custom_model_ids: self
                .observed_custom_model_ids
                .lock()
                .map(|ids| ids.clone())
                .unwrap_or_default(),
        }
    }
}

/// Native Agent endpoint may change when Qoder restarts or refreshes its
/// background Agent.  Atomics keep new UI connections pointed at the newest
/// endpoint without interrupting existing, already-connected sessions.
struct NativeEndpoint {
    port: AtomicU16,
    pid: AtomicU32,
    ipc_server_path: StdMutex<PathBuf>,
}

impl NativeEndpoint {
    fn new(port: u16, pid: u32, ipc_server_path: PathBuf) -> Self {
        Self {
            port: AtomicU16::new(port),
            pid: AtomicU32::new(pid),
            ipc_server_path: StdMutex::new(ipc_server_path),
        }
    }

    fn update(&self, port: u16, pid: u32, ipc_server_path: PathBuf) {
        self.port.store(port, Ordering::Relaxed);
        self.pid.store(pid, Ordering::Relaxed);
        if let Ok(mut current_path) = self.ipc_server_path.lock() {
            *current_path = ipc_server_path;
        }
    }

    fn ipc_server_path(&self) -> PathBuf {
        self.ipc_server_path
            .lock()
            .map(|path| path.clone())
            .unwrap_or_default()
    }
}

/// Start the adapter against an already-running native Qoder Agent.
///
/// The Qoder package is never modified. Qoder's Electron processes discover
/// their Agent through the WebSocket port and Unix IPC path published in
/// `.info.json`, so the adapter takes over that discovery record and relays
/// both transports, leaving the native Agent as the downstream for official
/// models and unmapped BYOK sessions.
pub async fn start_native_proxy(db: Arc<Database>) -> Result<NativeProxyHandle, String> {
    start_native_proxy_at(db, crate::qoder_config::get_qoder_info_path()).await
}

/// Start the adapter against a specific discovery file. Tests pass temporary
/// fixture paths; production uses Qoder's real `.info.json`.
async fn start_native_proxy_at(
    db: Arc<Database>,
    info_path: PathBuf,
) -> Result<NativeProxyHandle, String> {
    match crate::qoder_acp::info_writer::reclaim_stale_adapter_info(&info_path) {
        Ok(true) => log::warn!("[qoder_adapter] recovered a stale Q Switch discovery record"),
        Ok(false) => {}
        Err(error) => log::warn!("[qoder_adapter] cannot inspect stale discovery record: {error}"),
    }
    let snapshot = crate::qoder_acp::info_writer::capture_native_info_from(&info_path).map_err(|error| {
        format!(
            "Qoder native Agent is not ready. Start Qoder first and wait for its local Agent: {error}"
        )
    })?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| format!("Failed to bind Qoder native adapter: {error}"))?;
    let adapter_port = listener
        .local_addr()
        .map_err(|error| format!("Failed to read Qoder adapter address: {error}"))?
        .port();
    let adapter_pid = std::process::id();
    // macOS limits Unix-domain socket paths to roughly 104 bytes, so the
    // adapter's IPC listener lives at a short, unique path under /tmp.
    let adapter_ipc_path = PathBuf::from("/tmp").join(format!(
        "qswitch-qoder-{}-{}.sock",
        adapter_pid,
        uuid::Uuid::new_v4().simple()
    ));
    let ipc_listener = UnixListener::bind(&adapter_ipc_path)
        .map_err(|error| format!("Failed to bind Qoder IPC adapter: {error}"))?;
    crate::qoder_acp::info_writer::write_adapter_info_to(
        &info_path,
        &snapshot,
        adapter_port,
        &adapter_ipc_path,
        adapter_pid,
    )
    .map_err(|error| {
        let _ = std::fs::remove_file(&adapter_ipc_path);
        format!("Failed to publish Qoder adapter endpoint: {error}")
    })?;
    let native_ipc_path = snapshot.ipc_server_path.clone();

    let endpoint = Arc::new(NativeEndpoint::new(
        snapshot.websocket_port,
        snapshot.pid,
        native_ipc_path,
    ));
    let latest_snapshot = Arc::new(Mutex::new(snapshot.clone()));
    let native_port = snapshot.websocket_port;
    let native_pid = snapshot.pid;
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let connection_shutdown_tx = shutdown_tx.clone();
    let handler_state = Arc::new(AcpHandlerState::new(db));
    let endpoint_for_listener = endpoint.clone();
    let observed_custom_model_ids: ObservedCustomModelIds = Arc::new(StdMutex::new(Vec::new()));
    let observed_for_listener = observed_custom_model_ids.clone();
    let join_handle = tokio::spawn(async move {
        log::info!(
            "[qoder_adapter] listening on 127.0.0.1:{adapter_port} and IPC socket; native Agent=127.0.0.1:{native_port} pid={native_pid}"
        );
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (stream, peer) = match result {
                        Ok(stream) => stream,
                        Err(error) => {
                            log::warn!("[qoder_adapter] accept error: {error}");
                            continue;
                        }
                    };
                    let state = handler_state.clone();
                    let connection_shutdown = connection_shutdown_tx.subscribe();
                    let endpoint = endpoint_for_listener.clone();
                    let observed_custom_model_ids = observed_for_listener.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_connection(
                            stream,
                            state,
                            endpoint,
                            observed_custom_model_ids,
                            connection_shutdown,
                        ).await {
                            log::warn!("[qoder_adapter] connection from {peer} closed: {error}");
                        }
                    });
                }
                result = ipc_listener.accept() => {
                    let (stream, _) = match result {
                        Ok(stream) => stream,
                        Err(error) => {
                            log::warn!("[qoder_adapter] IPC accept error: {error}");
                            continue;
                        }
                    };
                    let state = handler_state.clone();
                    let connection_shutdown = connection_shutdown_tx.subscribe();
                    let endpoint = endpoint_for_listener.clone();
                    let observed_custom_model_ids = observed_for_listener.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_ipc_connection(
                            stream,
                            state,
                            endpoint,
                            observed_custom_model_ids,
                            connection_shutdown,
                        ).await {
                            log::warn!("[qoder_adapter] IPC connection closed: {error}");
                        }
                    });
                }
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
            }
        }
        log::info!("[qoder_adapter] stopped");
    });

    // Qoder's native Agent republishes `.info.json` periodically and after
    // every restart. The monitor reclaims the discovery record within a tick
    // so new Electron connections keep reaching this adapter, and tracks the
    // latest native endpoint as the relay downstream.
    let monitor_handle = tokio::spawn({
        let monitor_info_path = info_path.clone();
        let monitor_adapter_ipc_path = adapter_ipc_path.clone();
        let monitor_endpoint = endpoint.clone();
        let monitor_snapshot = latest_snapshot.clone();
        let monitor_shutdown_rx = shutdown_tx.subscribe();
        async move {
            monitor_native_discovery(
                monitor_info_path,
                adapter_port,
                monitor_adapter_ipc_path,
                adapter_pid,
                monitor_endpoint,
                monitor_snapshot,
                monitor_shutdown_rx,
            )
            .await;
        }
    });

    Ok(NativeProxyHandle {
        adapter_port,
        adapter_ipc_path,
        endpoint,
        adapter_pid,
        latest_snapshot,
        observed_custom_model_ids,
        info_path,
        shutdown_tx,
        join_handle: Some(join_handle),
        monitor_handle: Some(monitor_handle),
    })
}

async fn monitor_native_discovery(
    info_path: PathBuf,
    adapter_port: u16,
    adapter_ipc_path: PathBuf,
    adapter_pid: u32,
    endpoint: Arc<NativeEndpoint>,
    latest_snapshot: Arc<Mutex<NativeInfoSnapshot>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(std::time::Duration::from_millis(250));
    // Skip interval's immediate first tick: we just wrote a known-good owned
    // adapter record during startup.
    ticker.tick().await;
    let mut conflict_owner = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
        }

        match crate::qoder_acp::info_writer::adapter_owner_pid_from(&info_path) {
            Ok(Some(owner)) if owner == adapter_pid => continue,
            Ok(Some(owner)) => {
                if conflict_owner != Some(owner) {
                    log::warn!(
                        "[qoder_adapter] .info.json is now owned by another Q Switch process ({owner}); leaving it untouched"
                    );
                    conflict_owner = Some(owner);
                }
                continue;
            }
            Ok(None) => conflict_owner = None,
            Err(error) => {
                log::debug!("[qoder_adapter] cannot inspect native .info.json yet: {error}");
                continue;
            }
        }

        let snapshot = match crate::qoder_acp::info_writer::capture_native_info_from(&info_path) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                log::debug!("[qoder_adapter] native .info.json refresh is incomplete: {error}");
                continue;
            }
        };
        let native_port = snapshot.websocket_port;
        let native_pid = snapshot.pid;
        let native_ipc_path = snapshot.ipc_server_path.clone();
        if let Err(error) = crate::qoder_acp::info_writer::write_adapter_info_to(
            &info_path,
            &snapshot,
            adapter_port,
            &adapter_ipc_path,
            adapter_pid,
        ) {
            log::warn!("[qoder_adapter] cannot reclaim native .info.json: {error}");
            continue;
        }
        let coordinates_changed = endpoint.port.load(Ordering::Relaxed) != native_port
            || endpoint.pid.load(Ordering::Relaxed) != native_pid;
        endpoint.update(native_port, native_pid, native_ipc_path);
        *latest_snapshot.lock().await = snapshot;
        if coordinates_changed {
            log::info!(
                "[qoder_adapter] reattached after native Agent refresh: 127.0.0.1:{native_port} pid={native_pid}"
            );
        } else {
            log::debug!("[qoder_adapter] reclaimed periodic native .info.json refresh");
        }
    }
}

/// Stop the adapter and restore Qoder's original discovery record if it still
/// belongs to this Q Switch process.  A newer native Agent is never clobbered.
pub async fn stop_native_proxy(mut handle: NativeProxyHandle) -> Result<(), String> {
    let _ = handle.shutdown_tx.send(true);
    if let Some(join_handle) = handle.join_handle.take() {
        match tokio::time::timeout(std::time::Duration::from_secs(3), join_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!("[qoder_adapter] server task failed: {error}"),
            Err(_) => log::warn!("[qoder_adapter] shutdown timed out"),
        }
    }
    if let Some(monitor_handle) = handle.monitor_handle.take() {
        match tokio::time::timeout(std::time::Duration::from_secs(3), monitor_handle).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => log::warn!("[qoder_adapter] monitor task failed: {error}"),
            Err(_) => log::warn!("[qoder_adapter] monitor shutdown timed out"),
        }
    }
    let snapshot = handle.latest_snapshot.lock().await.clone();
    let restore_result = match crate::qoder_acp::info_writer::restore_native_info_to(
        &handle.info_path,
        &snapshot,
        handle.adapter_pid,
    ) {
        Ok(true) => {
            log::info!("[qoder_adapter] restored native Qoder .info.json");
            Ok(())
        }
        Ok(false) => {
            log::info!("[qoder_adapter] native .info.json changed; nothing restored");
            Ok(())
        }
        Err(error) => Err(format!("Failed to restore Qoder native endpoint: {error}")),
    };
    match std::fs::remove_file(&handle.adapter_ipc_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => log::warn!("[qoder_adapter] cannot remove IPC socket: {error}"),
    }
    restore_result
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionRoute {
    QSwitch,
    /// The Qoder session selected a native BYOK carrier that has not been
    /// explicitly mapped yet. We let `session/set_model` reach the native
    /// Agent so its local session state stays coherent, but reject every
    /// subsequent prompt locally to prevent an accidental paid fallback.
    UnmappedCarrier,
}

type SessionRoutes = Arc<Mutex<HashMap<String, SessionRoute>>>;
type ObservedCustomModelIds = Arc<StdMutex<Vec<String>>>;

#[derive(Clone, Debug)]
enum PendingSessionSelection {
    QSwitch {
        route_id: String,
        carrier_model_id: Option<String>,
    },
    UnmappedCarrier,
}

/// Qoder 1.18 attaches the selected carrier to `session/new`, while the
/// native Agent assigns the actual session ID before the following
/// `session/prompt`. Keep this short-lived, connection-local selection until
/// that first prompt supplies the real session ID.
type PendingSessionRoute = Arc<Mutex<Option<PendingSessionSelection>>>;

#[derive(Clone)]
struct ConnectionRouting {
    routes: SessionRoutes,
    pending_route: PendingSessionRoute,
}

const MAX_OBSERVED_CUSTOM_MODEL_IDS: usize = 8;

/// Record an unmapped native BYOK record ID in recency order. The value is a
/// local `model_*` identifier rather than a credential, prompt, or response,
/// and is intentionally never persisted or logged.
fn observe_custom_model_id(observed: &ObservedCustomModelIds, raw_model_id: &str) {
    let Ok(mut ids) = observed.lock() else {
        return;
    };
    if let Some(position) = ids.iter().position(|id| id == raw_model_id) {
        ids.remove(position);
    }
    ids.insert(0, raw_model_id.to_string());
    ids.truncate(MAX_OBSERVED_CUSTOM_MODEL_IDS);
}

/// Read only documented model-selector fields from an ACP request. Keeping
/// this narrow prevents the adapter from inspecting prompt blocks, keys, or
/// model output merely to discover a carrier identifier.
fn request_model_id(params: &Value) -> Option<&str> {
    params
        .get("_meta")
        .and_then(|meta| meta.get("ai-coding/model"))
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("_meta")
                .and_then(|meta| meta.get("ai-coding/model-id"))
                .and_then(Value::as_str)
        })
        .or_else(|| params.get("modelId").and_then(Value::as_str))
        .filter(|model_id| !model_id.trim().is_empty())
}

/// Binds the carrier selected in the most recent transparent `session/new` to
/// the session ID carried by its first prompt. It never inspects the prompt
/// blocks or native Agent responses.
async fn bind_pending_session_route(
    state: &Arc<AcpHandlerState>,
    routes: &SessionRoutes,
    pending_route: &PendingSessionRoute,
    session_id: &str,
) -> Option<SessionRoute> {
    let pending_route = pending_route.lock().await.take()?;

    match pending_route {
        PendingSessionSelection::QSwitch {
            route_id,
            carrier_model_id,
        } => {
            state.sessions.ensure(session_id).await;
            let _ = state.sessions.set_model(session_id, &route_id).await;
            routes
                .lock()
                .await
                .insert(session_id.to_string(), SessionRoute::QSwitch);
            if let Some(carrier_model_id) = carrier_model_id {
                log::info!(
                    "[qoder_adapter] session {session_id} mapped carrier {carrier_model_id} to {route_id}"
                );
            } else {
                log::info!("[qoder_adapter] session {session_id} selected {route_id}");
            }
            Some(SessionRoute::QSwitch)
        }
        PendingSessionSelection::UnmappedCarrier => {
            routes
                .lock()
                .await
                .insert(session_id.to_string(), SessionRoute::UnmappedCarrier);
            Some(SessionRoute::UnmappedCarrier)
        }
    }
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    state: Arc<AcpHandlerState>,
    endpoint: Arc<NativeEndpoint>,
    observed_custom_model_ids: ObservedCustomModelIds,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let client = accept_async(stream).await?;
    let native_port = endpoint.port.load(Ordering::Relaxed);
    let native_url = format!("ws://127.0.0.1:{native_port}");
    let (native, _) = connect_async(native_url).await?;
    let (mut client_sender, mut client_receiver) = client.split();
    let (mut native_sender, mut native_receiver) = native.split();

    // Exactly one task owns the UI socket writer. Native Agent replies and
    // Q Switch local events share this ordered queue.
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(message) = ui_rx.recv().await {
            if client_sender.send(message).await.is_err() {
                break;
            }
        }
    });

    let routing = ConnectionRouting {
        routes: Arc::new(Mutex::new(HashMap::new())),
        pending_route: Arc::new(Mutex::new(None)),
    };
    let native_to_ui = ui_tx.clone();
    let native_reader = tokio::spawn(async move {
        while let Some(message) = native_receiver.next().await {
            match message {
                Ok(message) => {
                    if native_to_ui.send(message).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    log::debug!("[qoder_adapter] native Agent read error: {error}");
                    break;
                }
            }
        }
    });

    let (local_tx, mut local_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let local_to_ui = ui_tx.clone();
    let local_writer = tokio::spawn(async move {
        while let Some(frame) = local_rx.recv().await {
            if local_to_ui.send(Message::Binary(frame)).is_err() {
                break;
            }
        }
    });

    let mut framer = LspFramer::new();

    loop {
        let message = tokio::select! {
            message = client_receiver.next() => match message {
                Some(message) => message?,
                None => break,
            },
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
        };
        match message {
            Message::Text(text) => {
                forward_client_data(
                    text.as_bytes(),
                    &mut framer,
                    &mut native_sender,
                    &state,
                    &routing,
                    &observed_custom_model_ids,
                    &local_tx,
                )
                .await?;
            }
            Message::Binary(data) => {
                forward_client_data(
                    &data,
                    &mut framer,
                    &mut native_sender,
                    &state,
                    &routing,
                    &observed_custom_model_ids,
                    &local_tx,
                )
                .await?;
            }
            Message::Ping(payload) => native_sender.send(Message::Ping(payload)).await?,
            Message::Pong(payload) => native_sender.send(Message::Pong(payload)).await?,
            Message::Close(frame) => {
                let _ = native_sender.send(Message::Close(frame)).await;
                break;
            }
            _ => {}
        }
    }

    drop(local_tx);
    drop(ui_tx);
    let local_session_ids: Vec<String> = routing.routes.lock().await.keys().cloned().collect();
    for session_id in local_session_ids {
        state.sessions.remove(&session_id).await;
    }
    native_reader.abort();
    let _ = native_reader.await;
    let _ = local_writer.await;
    let _ = writer.await;
    Ok(())
}

/// Relay Qoder's Unix IPC transport. Qoder's Electron components discover
/// both a WebSocket port and an `ipcServerPath` in `.info.json`; the adapter
/// publishes its own values for both, so each new connection is relayed to
/// the native Agent regardless of which transport a component picks.
async fn handle_ipc_connection(
    stream: UnixStream,
    state: Arc<AcpHandlerState>,
    endpoint: Arc<NativeEndpoint>,
    observed_custom_model_ids: ObservedCustomModelIds,
    mut shutdown_rx: watch::Receiver<bool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let native_ipc_path = endpoint.ipc_server_path();
    if native_ipc_path.as_os_str().is_empty() {
        return Err("Qoder native IPC endpoint is unavailable".into());
    }
    let native = UnixStream::connect(&native_ipc_path).await?;
    let (mut client_reader, mut client_writer) = stream.into_split();
    let (mut native_reader, mut native_writer) = native.into_split();

    // Exactly one task owns the Electron-side IPC writer. Native Agent chunks
    // and locally generated Q Switch ACP messages use the same ordered queue.
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(async move {
        while let Some(bytes) = ui_rx.recv().await {
            if client_writer.write_all(&bytes).await.is_err() {
                break;
            }
        }
    });

    let routing = ConnectionRouting {
        routes: Arc::new(Mutex::new(HashMap::new())),
        pending_route: Arc::new(Mutex::new(None)),
    };
    let native_to_ui = ui_tx.clone();
    let native_reader = tokio::spawn(async move {
        let mut buffer = vec![0_u8; 8192];
        loop {
            let size = match native_reader.read(&mut buffer).await {
                Ok(size) => size,
                Err(error) => {
                    log::debug!("[qoder_adapter] native IPC read error: {error}");
                    break;
                }
            };
            if size == 0 || native_to_ui.send(buffer[..size].to_vec()).is_err() {
                break;
            }
        }
    });

    let mut framer = LspFramer::new();
    let mut read_buffer = vec![0_u8; 8192];

    loop {
        let size = tokio::select! {
            result = client_reader.read(&mut read_buffer) => result?,
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
        };
        if size == 0 {
            break;
        }
        forward_ipc_client_data(
            &read_buffer[..size],
            &mut framer,
            &mut native_writer,
            &state,
            &routing,
            &observed_custom_model_ids,
            &ui_tx,
        )
        .await?;
    }

    drop(ui_tx);
    let local_session_ids: Vec<String> = routing.routes.lock().await.keys().cloned().collect();
    for session_id in local_session_ids {
        state.sessions.remove(&session_id).await;
    }
    native_reader.abort();
    let _ = native_reader.await;
    let _ = writer.await;
    Ok(())
}

async fn forward_ipc_client_data<W>(
    data: &[u8],
    framer: &mut LspFramer,
    native_writer: &mut W,
    state: &Arc<AcpHandlerState>,
    routing: &ConnectionRouting,
    observed_custom_model_ids: &ObservedCustomModelIds,
    local_tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    W: AsyncWrite + Unpin,
{
    for frame in framer.append(data)? {
        let message = match serde_json::from_slice::<Value>(&frame) {
            Ok(message) => message,
            Err(_) => {
                native_writer.write_all(&LspFramer::encode(&frame)).await?;
                continue;
            }
        };
        state.observe_workspace_message(&message).await;
        if handle_qswitch_message(
            state,
            &routing.routes,
            &routing.pending_route,
            observed_custom_model_ids,
            message.clone(),
            local_tx,
        )
        .await
        {
            continue;
        }
        native_writer.write_all(&LspFramer::encode(&frame)).await?;
    }
    Ok(())
}

async fn forward_client_data<S>(
    data: &[u8],
    framer: &mut LspFramer,
    native_sender: &mut S,
    state: &Arc<AcpHandlerState>,
    routing: &ConnectionRouting,
    observed_custom_model_ids: &ObservedCustomModelIds,
    local_tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    for frame in framer.append(data)? {
        let message = match serde_json::from_slice::<Value>(&frame) {
            Ok(message) => message,
            Err(_) => {
                native_sender
                    .send(Message::Binary(LspFramer::encode(&frame)))
                    .await?;
                continue;
            }
        };
        state.observe_workspace_message(&message).await;
        if handle_qswitch_message(
            state,
            &routing.routes,
            &routing.pending_route,
            observed_custom_model_ids,
            message.clone(),
            local_tx,
        )
        .await
        {
            continue;
        }
        native_sender
            .send(Message::Binary(LspFramer::encode(&frame)))
            .await?;
    }
    Ok(())
}

/// Return true only for messages that must be consumed by the Q Switch local
/// adapter. Everything else is transparently forwarded to Qoder's Agent.
async fn handle_qswitch_message(
    state: &Arc<AcpHandlerState>,
    routes: &SessionRoutes,
    pending_route: &PendingSessionRoute,
    observed_custom_model_ids: &ObservedCustomModelIds,
    message: Value,
    local_tx: &mpsc::UnboundedSender<Vec<u8>>,
) -> bool {
    let method = match message.get("method").and_then(Value::as_str) {
        Some(method) => method,
        None => return false,
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let session_id = params.get("sessionId").and_then(Value::as_str);

    // Quest 1.18 creates its session with the carrier embedded in
    // `params._meta["ai-coding/model-id"]`; its first prompt supplies the
    // native session ID that lets Q Switch bind the selected route.
    if method == "session/new" {
        let Some(model_id) = request_model_id(&params) else {
            return false;
        };
        let raw_model_id = model_id.strip_prefix("custom:").unwrap_or(model_id);
        let route_selection = match crate::qoder_config::resolve_qoder_route_selection(
            &state.db, model_id,
        ) {
            Ok(route_selection) => route_selection,
            Err(error) if raw_model_id.starts_with("qswitch_") => {
                wire::send_error(
                    local_tx,
                    &message.get("id").cloned(),
                    -32004,
                    &error.to_string(),
                );
                return true;
            }
            Err(error) => {
                log::warn!(
                    "[qoder_adapter] ignoring invalid carrier mapping for new session model {raw_model_id}: {error}"
                );
                None
            }
        };
        if let Some(route_selection) = route_selection {
            *pending_route.lock().await = Some(PendingSessionSelection::QSwitch {
                route_id: route_selection.route_id,
                carrier_model_id: route_selection.carrier_model_id,
            });
        } else if model_id.starts_with("custom:") {
            observe_custom_model_id(observed_custom_model_ids, raw_model_id);
            *pending_route.lock().await = Some(PendingSessionSelection::UnmappedCarrier);
        }
        return false;
    }

    // Quest uses its own model-selection method name while retaining the same
    // sessionId/modelId payload shape as the standard ACP method.
    if matches!(method, "session/set_model" | "session/set_model/quest") {
        *pending_route.lock().await = None;
        let model_id = match params.get("modelId").and_then(Value::as_str) {
            Some(model_id) => model_id,
            None => return false,
        };
        let raw_model_id = model_id.strip_prefix("custom:").unwrap_or(model_id);
        let route_selection = match crate::qoder_config::resolve_qoder_route_selection(
            &state.db, model_id,
        ) {
            Ok(route_selection) => route_selection,
            Err(error) if raw_model_id.starts_with("qswitch_") => {
                wire::send_error(
                    local_tx,
                    &message.get("id").cloned(),
                    -32004,
                    &error.to_string(),
                );
                return true;
            }
            Err(error) => {
                log::warn!(
                        "[qoder_adapter] ignoring invalid carrier mapping for native model {raw_model_id}: {error}"
                    );
                None
            }
        };
        if let Some(route_selection) = route_selection {
            let Some(session_id) = session_id else {
                wire::send_error(
                    local_tx,
                    &message.get("id").cloned(),
                    -32602,
                    "Invalid params: missing sessionId.",
                );
                return true;
            };
            state.sessions.ensure(session_id).await;
            let _ = state
                .sessions
                .set_model(session_id, &route_selection.route_id)
                .await;
            routes
                .lock()
                .await
                .insert(session_id.to_string(), SessionRoute::QSwitch);
            wire::send_result(local_tx, &message.get("id").cloned(), serde_json::json!({}));
            if let Some(carrier_model_id) = route_selection.carrier_model_id {
                log::info!(
                    "[qoder_adapter] session {session_id} mapped carrier {carrier_model_id} to {}",
                    route_selection.route_id
                );
            } else {
                log::info!("[qoder_adapter] session {session_id} selected {raw_model_id}");
            }
            return true;
        }
        if raw_model_id.starts_with("qswitch_") {
            wire::send_error(
                local_tx,
                &message.get("id").cloned(),
                -32004,
                "Unknown Q Switch model route. Refresh the Q Switch Qoder manifest first.",
            );
            return true;
        }
        if model_id.starts_with("custom:") {
            observe_custom_model_id(observed_custom_model_ids, raw_model_id);
        }
        if let Some(session_id) = session_id {
            if model_id.starts_with("custom:") {
                routes
                    .lock()
                    .await
                    .insert(session_id.to_string(), SessionRoute::UnmappedCarrier);
            } else {
                routes.lock().await.remove(session_id);
            }
        }
        return false;
    }

    let mut session_route = if let Some(session_id) = session_id {
        routes.lock().await.get(session_id).copied()
    } else {
        None
    };
    if method == "session/prompt" && session_route.is_none() {
        if let Some(session_id) = session_id {
            session_route =
                bind_pending_session_route(state, routes, pending_route, session_id).await;
        }
    }
    let is_qswitch_session = session_route == Some(SessionRoute::QSwitch);
    if !is_qswitch_session && method == "session/prompt" {
        if session_route == Some(SessionRoute::UnmappedCarrier) {
            wire::send_error(
                local_tx,
                &message.get("id").cloned(),
                -32004,
                "Qoder BYOK carrier is not mapped to Q Switch. Save a route mapping and select the carrier again; the native provider was not contacted.",
            );
            return true;
        }
        if let (Some(session_id), Some(model_id)) = (session_id, request_model_id(&params)) {
            let route_selection = match crate::qoder_config::resolve_qoder_route_selection(
                &state.db, model_id,
            ) {
                Ok(route_selection) => route_selection,
                Err(error) => {
                    log::warn!(
                            "[qoder_adapter] ignoring invalid carrier mapping for prompt model {model_id}: {error}"
                        );
                    None
                }
            };
            if let Some(route_selection) = route_selection {
                state.sessions.ensure(session_id).await;
                let _ = state
                    .sessions
                    .set_model(session_id, &route_selection.route_id)
                    .await;
                routes
                    .lock()
                    .await
                    .insert(session_id.to_string(), SessionRoute::QSwitch);
                let state = state.clone();
                let local_tx = local_tx.clone();
                tokio::spawn(async move {
                    handler::handle_message(&state, message, &local_tx).await;
                });
                return true;
            }
            if let Some(carrier_model_id) = model_id.strip_prefix("custom:") {
                // A first prompt is the point where some Qoder Quest builds
                // finally materialize their selected BYOK model. Do not allow
                // that discovery request to reach the paid native provider:
                // show the local carrier ID, require an explicit mapping, and
                // let the user resend after Q Switch owns the route.
                observe_custom_model_id(observed_custom_model_ids, carrier_model_id);
                wire::send_error(
                    local_tx,
                    &message.get("id").cloned(),
                    -32004,
                    "Qoder BYOK carrier observed but not mapped. Save a Q Switch route mapping and resend; the native provider was not contacted.",
                );
                return true;
            }
        }
    }

    if !is_qswitch_session {
        return false;
    }

    match method {
        "session/prompt" => {
            let state = state.clone();
            let local_tx = local_tx.clone();
            tokio::spawn(async move {
                handler::handle_message(&state, message, &local_tx).await;
            });
            true
        }
        "session/set_mode" | "session/cancel" => {
            handler::handle_message(state, message, local_tx).await;
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qswitch_model_prefix_is_normalized() {
        assert_eq!(
            "custom:qswitch_123".strip_prefix("custom:").unwrap(),
            "qswitch_123"
        );
    }

    #[test]
    fn official_model_never_matches_qswitch_prefix() {
        assert!(!"gpt-5".starts_with("qswitch_"));
        assert!(!"custom:org-model"
            .strip_prefix("custom:")
            .unwrap()
            .starts_with("qswitch_"));
    }

    #[test]
    fn observed_custom_model_ids_are_recent_deduplicated_and_bounded() {
        let observed: ObservedCustomModelIds = Arc::new(StdMutex::new(Vec::new()));
        for index in 0..=MAX_OBSERVED_CUSTOM_MODEL_IDS {
            observe_custom_model_id(&observed, &format!("model_{index}"));
        }

        let ids = observed.lock().unwrap().clone();
        assert_eq!(ids.len(), MAX_OBSERVED_CUSTOM_MODEL_IDS);
        assert_eq!(ids.first().unwrap(), "model_8");
        assert_eq!(ids.last().unwrap(), "model_1");

        observe_custom_model_id(&observed, "model_4");
        let ids = observed.lock().unwrap().clone();
        assert_eq!(ids.len(), MAX_OBSERVED_CUSTOM_MODEL_IDS);
        assert_eq!(ids.first().unwrap(), "model_4");
        assert_eq!(ids.iter().filter(|id| *id == "model_4").count(), 1);
    }

    #[test]
    fn request_model_id_reads_only_supported_selector_fields() {
        let metadata = serde_json::json!({
            "_meta": { "ai-coding/model": "custom:model_from_metadata" },
            "prompt": [{ "type": "text", "text": "must not be inspected" }]
        });
        assert_eq!(
            request_model_id(&metadata),
            Some("custom:model_from_metadata")
        );

        let direct = serde_json::json!({ "modelId": "custom:model_direct" });
        assert_eq!(request_model_id(&direct), Some("custom:model_direct"));

        let quest_new = serde_json::json!({
            "_meta": { "ai-coding/model-id": "custom:model_from_quest_new" }
        });
        assert_eq!(
            request_model_id(&quest_new),
            Some("custom:model_from_quest_new")
        );
        assert_eq!(request_model_id(&serde_json::json!({})), None);
    }

    #[tokio::test]
    async fn unmapped_carrier_session_blocks_prompt_without_model_metadata() {
        let state = Arc::new(AcpHandlerState::new(Arc::new(Database {
            conn: StdMutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        })));
        let routes: SessionRoutes = Arc::new(Mutex::new(HashMap::new()));
        let pending_route: PendingSessionRoute = Arc::new(Mutex::new(None));
        let observed: ObservedCustomModelIds = Arc::new(StdMutex::new(Vec::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();

        let set_model = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "session/set_model",
            "params": {
                "sessionId": "unmapped-carrier-session",
                "modelId": "custom:model_unmapped_carrier"
            }
        });
        assert!(
            !handle_qswitch_message(&state, &routes, &pending_route, &observed, set_model, &tx)
                .await,
            "the native Agent still receives only the state-setting frame"
        );
        assert_eq!(
            routes.lock().await.get("unmapped-carrier-session"),
            Some(&SessionRoute::UnmappedCarrier)
        );

        let quest_set_model = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/set_model/quest",
            "params": {
                "sessionId": "quest-carrier-session",
                "modelId": "custom:model_quest_carrier"
            }
        });
        assert!(
            !handle_qswitch_message(
                &state,
                &routes,
                &pending_route,
                &observed,
                quest_set_model,
                &tx,
            )
            .await,
            "Quest model selection remains transparent until its carrier is explicitly mapped"
        );
        assert_eq!(
            routes.lock().await.get("quest-carrier-session"),
            Some(&SessionRoute::UnmappedCarrier)
        );

        let prompt = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/prompt",
            "params": { "sessionId": "unmapped-carrier-session" }
        });
        assert!(
            handle_qswitch_message(&state, &routes, &pending_route, &observed, prompt, &tx).await
        );

        let framed_error = rx.recv().await.unwrap();
        let mut framer = LspFramer::new();
        let body = framer.append(&framed_error).unwrap().remove(0);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["id"], 2);
        assert_eq!(error["error"]["code"], -32004);
    }

    #[tokio::test]
    async fn pending_session_route_binds_on_first_prompt() {
        let state = Arc::new(AcpHandlerState::new(Arc::new(Database {
            conn: StdMutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        })));
        let routes: SessionRoutes = Arc::new(Mutex::new(HashMap::new()));
        let pending_route: PendingSessionRoute =
            Arc::new(Mutex::new(Some(PendingSessionSelection::QSwitch {
                route_id: "qswitch_test_route".to_string(),
                carrier_model_id: Some("model_test_carrier".to_string()),
            })));

        bind_pending_session_route(&state, &routes, &pending_route, "native-created-session").await;

        assert_eq!(
            routes.lock().await.get("native-created-session"),
            Some(&SessionRoute::QSwitch)
        );
        assert!(pending_route.lock().await.is_none());
    }

    #[tokio::test]
    #[ignore = "requires loopback socket permission unavailable in the sandbox"]
    async fn native_frames_are_transparently_relayed_and_info_is_restored() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        let native_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let native_port = native_listener.local_addr().unwrap().port();
        let original_info = format!(
            r#"{{"websocketPort":{native_port},"pid":4242,"ipcServerPath":"/tmp/qoder-native.sock","isDev":false}}"#
        );
        std::fs::write(&info_path, &original_info).unwrap();

        let native_task = tokio::spawn(async move {
            let (stream, _) = native_listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            while let Some(message) = socket.next().await {
                let message = message.unwrap();
                socket.send(message).await.unwrap();
            }
        });

        let db = Arc::new(Database {
            conn: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        });
        let handle = start_native_proxy_at(db, info_path.clone()).await.unwrap();
        let active: Value = serde_json::from_slice(&std::fs::read(&info_path).unwrap()).unwrap();
        assert_eq!(active["websocketPort"], handle.adapter_port);
        assert_eq!(active["qswitchAdapter"]["nativeWebsocketPort"], native_port);

        let (mut client, _) = connect_async(format!("ws://127.0.0.1:{}", handle.adapter_port))
            .await
            .unwrap();
        let frame = LspFramer::encode(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#);
        client
            .send(Message::Binary(frame.clone().into()))
            .await
            .unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response, Message::Binary(frame.into()));
        let _ = client.close(None).await;
        drop(client);

        stop_native_proxy(handle).await.unwrap();
        assert_eq!(std::fs::read_to_string(&info_path).unwrap(), original_info);
        native_task.abort();
    }

    #[tokio::test]
    #[ignore = "requires loopback socket permission unavailable in the sandbox"]
    async fn native_ipc_frames_are_relayed_and_unmapped_custom_model_is_observed() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        let native_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let native_port = native_listener.local_addr().unwrap().port();
        let native_ipc_path = tmp.path().join("native.sock");
        let native_ipc_listener = UnixListener::bind(&native_ipc_path).unwrap();
        let original_info = format!(
            r#"{{"websocketPort":{native_port},"pid":4242,"ipcServerPath":"{}","isDev":false}}"#,
            native_ipc_path.to_string_lossy()
        );
        std::fs::write(&info_path, &original_info).unwrap();

        let native_task = tokio::spawn(async move {
            let (mut stream, _) = native_ipc_listener.accept().await.unwrap();
            let mut buffer = vec![0_u8; 8192];
            loop {
                let size = stream.read(&mut buffer).await.unwrap();
                if size == 0 {
                    break;
                }
                stream.write_all(&buffer[..size]).await.unwrap();
            }
        });

        let db = Arc::new(Database {
            conn: StdMutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        });
        let handle = start_native_proxy_at(db, info_path.clone()).await.unwrap();
        let active: Value = serde_json::from_slice(&std::fs::read(&info_path).unwrap()).unwrap();
        assert_eq!(
            active["ipcServerPath"],
            handle.adapter_ipc_path.to_string_lossy().as_ref()
        );

        let mut client = UnixStream::connect(&handle.adapter_ipc_path).await.unwrap();
        let frame = LspFramer::encode(
            br#"{"jsonrpc":"2.0","id":1,"method":"session/set_model","params":{"sessionId":"native-carrier","modelId":"custom:model_ipc_carrier"}}"#,
        );
        client.write_all(&frame).await.unwrap();
        let mut response = vec![0_u8; 8192];
        let size = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(&response[..size], frame.as_slice());
        assert_eq!(
            handle.status().observed_custom_model_ids,
            vec!["model_ipc_carrier"]
        );

        drop(client);
        stop_native_proxy(handle).await.unwrap();
        assert_eq!(std::fs::read_to_string(&info_path).unwrap(), original_info);
        native_task.abort();
        drop(native_listener);
    }

    #[tokio::test]
    #[ignore = "requires loopback socket permission unavailable in the sandbox"]
    async fn unmapped_native_custom_model_is_observed_and_transparently_relayed() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        let native_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let native_port = native_listener.local_addr().unwrap().port();
        let original_info = format!(
            r#"{{"websocketPort":{native_port},"pid":4242,"ipcServerPath":"/tmp/qoder-native.sock","isDev":false}}"#
        );
        std::fs::write(&info_path, &original_info).unwrap();

        let native_task = tokio::spawn(async move {
            let (stream, _) = native_listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            while let Some(message) = socket.next().await {
                let message = message.unwrap();
                socket.send(message).await.unwrap();
            }
        });

        let db = Arc::new(Database {
            conn: StdMutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        });
        let handle = start_native_proxy_at(db, info_path.clone()).await.unwrap();
        let (mut client, _) = connect_async(format!("ws://127.0.0.1:{}", handle.adapter_port))
            .await
            .unwrap();
        let frame = LspFramer::encode(
            br#"{"jsonrpc":"2.0","id":1,"method":"session/set_model","params":{"sessionId":"native-carrier","modelId":"custom:model_carrier_test"}}"#,
        );
        client
            .send(Message::Binary(frame.clone().into()))
            .await
            .unwrap();
        let response = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(response, Message::Binary(frame.into()));
        assert_eq!(
            handle.status().observed_custom_model_ids,
            vec!["model_carrier_test"]
        );

        let _ = client.close(None).await;
        drop(client);
        stop_native_proxy(handle).await.unwrap();
        assert_eq!(std::fs::read_to_string(&info_path).unwrap(), original_info);
        native_task.abort();
    }

    #[tokio::test]
    #[ignore = "requires loopback socket permission unavailable in the sandbox"]
    async fn native_info_refresh_is_reclaimed_and_restores_latest_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        let first_native = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_port = first_native.local_addr().unwrap().port();
        let original = format!(
            r#"{{"websocketPort":{first_port},"pid":4242,"ipcServerPath":"/tmp/qoder-first.sock","isDev":false}}"#
        );
        std::fs::write(&info_path, original).unwrap();

        let db = Arc::new(Database {
            conn: std::sync::Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
        });
        let handle = start_native_proxy_at(db, info_path.clone()).await.unwrap();
        let adapter_port = handle.adapter_port;

        let refreshed_native = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refreshed_port = refreshed_native.local_addr().unwrap().port();
        let refreshed = format!(
            r#"{{"websocketPort":{refreshed_port},"pid":5252,"ipcServerPath":"/tmp/qoder-refreshed.sock","isDev":false,"refresh":true}}"#
        );
        std::fs::write(&info_path, &refreshed).unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let current: Value =
                    serde_json::from_slice(&std::fs::read(&info_path).unwrap()).unwrap();
                if current["websocketPort"] == adapter_port
                    && current["qswitchAdapter"]["nativeWebsocketPort"] == refreshed_port
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("adapter should reclaim a refreshed native discovery record");
        assert_eq!(handle.status().native_port, Some(refreshed_port));
        assert_eq!(handle.status().native_pid, Some(5252));

        stop_native_proxy(handle).await.unwrap();
        assert_eq!(std::fs::read_to_string(&info_path).unwrap(), refreshed);
        drop(first_native);
        drop(refreshed_native);
    }
}
