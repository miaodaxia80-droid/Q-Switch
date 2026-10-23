use crate::{
    database::Database,
    proxy::server::{Gateway, GatewayStatus},
    qoder_config::{self, QoderCustomProvider, QoderCustomRoute, QoderRouteOption},
};
use std::sync::Arc;
use tauri::{Manager, State};
type AppState = Arc<Gateway>;

#[tauri::command]
async fn start_proxy_server(state: State<'_, AppState>) -> Result<GatewayStatus, String> {
    state.inner().start().await
}
#[tauri::command]
async fn stop_proxy_with_restore(state: State<'_, AppState>) -> Result<(), String> {
    state.stop().await;
    Ok(())
}
#[tauri::command]
async fn get_proxy_status(state: State<'_, AppState>) -> Result<GatewayStatus, String> {
    state.status().await
}
#[tauri::command]
async fn qoder_list_routes(state: State<'_, AppState>) -> Result<Vec<QoderRouteOption>, String> {
    qoder_config::list_qoder_route_options(&state.db)
        .await
        .map_err(Into::into)
}
#[tauri::command]
fn qoder_list_custom_providers(
    state: State<'_, AppState>,
) -> Result<Vec<QoderCustomProvider>, String> {
    qoder_config::list_qoder_custom_providers(&state.db).map_err(Into::into)
}
#[tauri::command]
fn qoder_list_custom_routes(state: State<'_, AppState>) -> Result<Vec<QoderCustomRoute>, String> {
    qoder_config::list_qoder_custom_routes(&state.db).map_err(Into::into)
}
#[tauri::command]
async fn qoder_save_custom_provider(
    state: State<'_, AppState>,
    provider: QoderCustomProvider,
) -> Result<QoderCustomProvider, String> {
    qoder_config::save_qoder_custom_provider(&state.db, provider)
        .await
        .map_err(Into::into)
}
#[tauri::command]
async fn qoder_save_custom_route(
    state: State<'_, AppState>,
    route: QoderCustomRoute,
) -> Result<QoderCustomRoute, String> {
    qoder_config::save_qoder_custom_route(&state.db, route)
        .await
        .map_err(Into::into)
}
#[tauri::command]
async fn qoder_delete_custom_provider(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    qoder_config::delete_qoder_custom_provider(&state.db, &id)
        .await
        .map_err(Into::into)
}
#[tauri::command]
async fn qoder_delete_custom_route(state: State<'_, AppState>, id: String) -> Result<(), String> {
    qoder_config::delete_qoder_custom_route(&state.db, &id)
        .await
        .map_err(Into::into)
}
#[tauri::command]
async fn qoder_sync_native_provider(
    state: State<'_, AppState>,
    target: String,
    route_id: String,
) -> Result<String, String> {
    if !state.status().await?.running {
        return Err("Start the local gateway first".into());
    }
    qoder_config::sync_native_provider(&state.db, &target, &route_id).await
}
#[tauri::command]
fn qoder_remove_native_provider(target: String) -> Result<String, String> {
    qoder_config::remove_native_provider(&target)
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_window(app)
        }))
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .build(),
        )
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(|app| {
            crate::config::load_legacy_directory(&app.path().app_data_dir()?)?;
            let gateway = Arc::new(Gateway::new(Arc::new(Database::new()?)));
            app.manage(gateway.clone());
            // Restore only the Qoder gateway, never scan or modify another client.
            tauri::async_runtime::spawn(async move {
                if let Err(e) = gateway.start().await {
                    log::warn!("{e}");
                }
            });
            let open =
                tauri::menu::MenuItem::with_id(app, "open", "打开 Q Switch", true, None::<&str>)?;
            let quit = tauri::menu::MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = tauri::menu::Menu::with_items(app, &[&open, &quit])?;
            let mut tray = tauri::tray::TrayIconBuilder::new()
                .tooltip("Q Switch")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "open" => show_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            tray.build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            start_proxy_server,
            stop_proxy_with_restore,
            get_proxy_status,
            qoder_list_routes,
            qoder_list_custom_providers,
            qoder_list_custom_routes,
            qoder_save_custom_provider,
            qoder_save_custom_route,
            qoder_delete_custom_provider,
            qoder_delete_custom_route,
            qoder_sync_native_provider,
            qoder_remove_native_provider
        ])
        .run(tauri::generate_context!())
        .expect("Failed to start Q Switch");
}
fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
