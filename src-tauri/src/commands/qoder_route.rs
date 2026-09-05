//! Tauri command for exporting Qoder's Q Switch routing catalog.

use crate::qoder_acp::NativeProxyStatus;
use crate::qoder_config::{
    QoderCarrierMapping, QoderCustomProvider, QoderCustomRoute, QoderRouteOption, QoderToolPolicy,
};
use crate::store::AppState;

/// Refresh the stable model manifest consumed by the future native adapter.
#[tauri::command]
pub async fn qoder_sync_model_manifest(
    state: tauri::State<'_, AppState>,
) -> Result<String, String> {
    crate::qoder_config::sync_qoder_model_manifest(&state.db)
        .await
        .map(|path| path.to_string_lossy().into_owned())
        .map_err(|e| e.to_string())
}

/// List stable Q Switch routes available for Qoder carrier mappings.
#[tauri::command]
pub async fn qoder_list_routes(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<QoderRouteOption>, String> {
    crate::qoder_config::list_qoder_route_options(&state.db)
        .await
        .map_err(|error| error.to_string())
}

/// List native Qoder carrier model mappings stored by Q Switch.
#[tauri::command]
pub async fn qoder_list_carrier_mappings(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<QoderCarrierMapping>, String> {
    crate::qoder_config::list_qoder_carrier_mappings(&state.db).map_err(|error| error.to_string())
}

/// Map a Qoder native BYOK carrier model to one Q Switch route.
#[tauri::command]
pub async fn qoder_save_carrier_mapping(
    state: tauri::State<'_, AppState>,
    carrier_model_id: String,
    route_id: String,
) -> Result<QoderCarrierMapping, String> {
    crate::qoder_config::sync_qoder_model_manifest(&state.db)
        .await
        .map_err(|error| format!("Failed to refresh Qoder route catalog: {error}"))?;
    crate::qoder_config::save_qoder_carrier_mapping(&state.db, &carrier_model_id, &route_id)
        .map_err(|error| error.to_string())
}

/// Remove a carrier mapping without changing Qoder's native configuration.
#[tauri::command]
pub async fn qoder_remove_carrier_mapping(
    state: tauri::State<'_, AppState>,
    carrier_model_id: String,
) -> Result<(), String> {
    crate::qoder_config::remove_qoder_carrier_mapping(&state.db, &carrier_model_id)
        .map_err(|error| error.to_string())
}

/// Read the local capability policy for routed Qoder sessions.
#[tauri::command]
pub async fn qoder_get_tool_policy(
    state: tauri::State<'_, AppState>,
) -> Result<QoderToolPolicy, String> {
    crate::qoder_config::get_qoder_tool_policy(&state.db).map_err(|error| error.to_string())
}

/// Persist the local user's explicit capability grants for routed Qoder
/// sessions. This policy controls which tools are advertised to the upstream
/// model; disabled capabilities are not merely denied at execution time.
#[tauri::command]
pub async fn qoder_set_tool_policy(
    state: tauri::State<'_, AppState>,
    policy: QoderToolPolicy,
) -> Result<QoderToolPolicy, String> {
    crate::qoder_config::save_qoder_tool_policy(&state.db, &policy)
        .map_err(|error| error.to_string())
}

/// Read the in-memory native transport-adapter state. The adapter is only
/// active for the current Q Switch process and never inferred from a stale
/// `.info.json` file.
#[tauri::command]
pub async fn qoder_get_native_adapter_status(
    state: tauri::State<'_, AppState>,
) -> Result<NativeProxyStatus, String> {
    let guard = state.qoder_native_adapter.lock().await;
    Ok(guard
        .as_ref()
        .map(|handle| handle.status())
        .unwrap_or(NativeProxyStatus {
            active: false,
            adapter_port: None,
            adapter_ipc_path: None,
            native_port: None,
            native_pid: None,
            observed_custom_model_ids: Vec::new(),
            client_connected: false,
            active_client_connections: 0,
        }))
}

/// Publish the transparent loopback adapter in front of an already-running
/// Qoder native Agent. No Qoder bundle file is modified.
#[tauri::command]
pub async fn qoder_start_native_adapter(
    state: tauri::State<'_, AppState>,
) -> Result<NativeProxyStatus, String> {
    if !state.proxy_service.is_running().await {
        return Err(
            "Start the Q Switch local routing service before enabling the Qoder native adapter."
                .to_string(),
        );
    }
    crate::qoder_config::sync_qoder_model_manifest(&state.db)
        .await
        .map_err(|error| format!("Failed to refresh Qoder route manifest: {error}"))?;

    let mut guard = state.qoder_native_adapter.lock().await;
    if let Some(handle) = guard.as_ref() {
        return Ok(handle.status());
    }
    let handle = crate::qoder_acp::start_native_proxy(state.db.clone()).await?;
    let status = handle.status();
    *guard = Some(handle);
    Ok(status)
}

/// Stop the adapter and restore the exact native Agent discovery record when
/// it is still owned by this Q Switch process.
#[tauri::command]
pub async fn qoder_stop_native_adapter(
    state: tauri::State<'_, AppState>,
) -> Result<NativeProxyStatus, String> {
    let handle = state.qoder_native_adapter.lock().await.take();
    if let Some(handle) = handle {
        crate::qoder_acp::stop_native_proxy(handle).await?;
    }
    Ok(NativeProxyStatus {
        active: false,
        adapter_port: None,
        adapter_ipc_path: None,
        native_port: None,
        native_pid: None,
        observed_custom_model_ids: Vec::new(),
        client_connected: false,
        active_client_connections: 0,
    })
}

/// List user-defined Qoder custom routes (arbitrary model name + base URL).
#[tauri::command]
pub async fn qoder_list_custom_routes(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<QoderCustomRoute>, String> {
    crate::qoder_config::list_qoder_custom_routes(&state.db).map_err(|error| error.to_string())
}

/// Create or update a Qoder custom route, mirroring it as a hidden provider.
#[tauri::command]
pub async fn qoder_save_custom_route(
    state: tauri::State<'_, AppState>,
    route: QoderCustomRoute,
) -> Result<QoderCustomRoute, String> {
    crate::qoder_config::save_qoder_custom_route(&state.db, route)
        .await
        .map_err(|error| error.to_string())
}

/// Delete a Qoder custom MODEL and refresh its parent provider mirror.
#[tauri::command]
pub async fn qoder_delete_custom_route(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    crate::qoder_config::delete_qoder_custom_route(&state.db, &id)
        .await
        .map_err(|error| error.to_string())
}

/// List custom Qoder providers (API keys are masked to presence-only).
#[tauri::command]
pub async fn qoder_list_custom_providers(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<QoderCustomProvider>, String> {
    crate::qoder_config::list_qoder_custom_providers(&state.db).map_err(|error| error.to_string())
}

/// Create or update a custom Qoder provider (shared base URL / key / format).
#[tauri::command]
pub async fn qoder_save_custom_provider(
    state: tauri::State<'_, AppState>,
    provider: QoderCustomProvider,
) -> Result<QoderCustomProvider, String> {
    crate::qoder_config::save_qoder_custom_provider(&state.db, provider)
        .await
        .map_err(|error| error.to_string())
}

/// Delete a custom Qoder provider, its models and hidden mirror.
#[tauri::command]
pub async fn qoder_delete_custom_provider(
    state: tauri::State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    crate::qoder_config::delete_qoder_custom_provider(&state.db, &id)
        .await
        .map_err(|error| error.to_string())
}
