//! Qoder-only loopback gateway. No client takeover, telemetry or periodic scans.
use super::{
    providers::qoder_wire::{self, QoderApiFormat, QoderUpstreamConfig},
    ProxyError,
};
use crate::{database::Database, qoder_config};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use tokio::{sync::Mutex, task::JoinHandle};

pub struct Gateway {
    pub db: Arc<Database>,
    task: Mutex<Option<JoinHandle<()>>>,
    requests: AtomicU64,
}
#[derive(Debug, Serialize)]
pub struct GatewayStatus {
    pub running: bool,
    pub address: String,
    pub port: u16,
    pub total_requests: u64,
}
impl Gateway {
    pub fn new(db: Arc<Database>) -> Self {
        Self {
            db,
            task: Mutex::new(None),
            requests: AtomicU64::new(0),
        }
    }
    pub async fn status(&self) -> Result<GatewayStatus, String> {
        Ok(GatewayStatus {
            running: self
                .task
                .lock()
                .await
                .as_ref()
                .is_some_and(|t| !t.is_finished()),
            address: "127.0.0.1".into(),
            port: self
                .db
                .get_proxy_config()
                .await
                .map_err(|e| e.to_string())?
                .listen_port,
            total_requests: self.requests.load(Ordering::Relaxed),
        })
    }
    pub async fn start(self: &Arc<Self>) -> Result<GatewayStatus, String> {
        let mut slot = self.task.lock().await;
        if slot.as_ref().is_some_and(|t| !t.is_finished()) {
            drop(slot);
            return self.status().await;
        }
        qoder_config::sync_qoder_model_manifest(&self.db)
            .await
            .map_err(|e| e.to_string())?;
        let port = self
            .db
            .get_proxy_config()
            .await
            .map_err(|e| e.to_string())?
            .listen_port;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|_| {
                format!("Cannot bind local port {port}; check whether another Q Switch is running")
            })?;
        let router = Router::new()
            .route("/health", get(|| async { Json(json!({"status":"ok"})) }))
            .route("/qoder/v1/models", get(models))
            .route("/qoder/v1/chat/completions", post(chat))
            .layer(DefaultBodyLimit::max(32 * 1024 * 1024))
            .with_state(self.clone());
        *slot = Some(tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, router).await {
                log::error!("Qoder gateway stopped: {error}");
            }
        }));
        drop(slot);
        self.status().await
    }
    pub async fn stop(&self) {
        if let Some(task) = self.task.lock().await.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
async fn models(State(state): State<Arc<Gateway>>) -> Result<Json<Value>, ProxyError> {
    let routes = qoder_config::list_qoder_route_options(&state.db)
        .await
        .map_err(|e| ProxyError::ConfigError(e.to_string()))?;
    Ok(Json(
        json!({"object":"list","data":routes.into_iter().map(|r|json!({"id":r.route_id,"object":"model","owned_by":"qswitch"})).collect::<Vec<_>>()}),
    ))
}
async fn chat(
    State(state): State<Arc<Gateway>>,
    headers: HeaderMap,
    Json(mut body): Json<Value>,
) -> Result<Response, ProxyError> {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer qswitch-local") {
        return Ok((
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"Invalid local gateway credential"}})),
        )
            .into_response());
    }
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| ProxyError::InvalidRequest("model is required".into()))?;
    let route = qoder_config::resolve_qoder_route(model)
        .map_err(|e| ProxyError::ConfigError(e.to_string()))?
        .ok_or_else(|| {
            ProxyError::InvalidRequest("Unknown or disabled Q Switch model route".into())
        })?;
    let provider = state
        .db
        .get_provider_by_id(&route.provider_id, "qoder")
        .map_err(|e| ProxyError::ConfigError(e.to_string()))?
        .ok_or_else(|| ProxyError::ConfigError("Provider no longer exists".into()))?;
    body["model"] = json!(route.upstream_model);
    if body.get("reasoning_effort").is_none() {
        if let Some(effort) = route.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
    }
    let format = QoderApiFormat::parse(&route.api_format)?;
    let session = headers
        .get(qoder_wire::QODER_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    if format == QoderApiFormat::AnthropicMessages && session.is_none() {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tools");
            obj.remove("tool_choice");
        }
    }
    state.requests.fetch_add(1, Ordering::Relaxed);
    qoder_wire::bridge_qoder_upstream(
        QoderUpstreamConfig::from_provider(
            &provider,
            format,
            route.is_full_url,
            &route.upstream_model,
            session,
        ),
        body,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_route_rewrites_model_and_auth_and_rejects_disabled_models() {
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = captured.clone();
        let upstream = Router::new().route("/v1/chat/completions", post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let sink = sink.clone();
            async move {
                assert_eq!(headers["authorization"], "Bearer upstream-secret");
                sink.lock().await.push(body);
                Json(json!({"id":"reply","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = listener.local_addr().unwrap().port();
        let upstream_task = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let db = Arc::new(Database::memory().unwrap());
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        db.set_setting("qoder_listen_port", &port.to_string())
            .unwrap();
        qoder_config::save_qoder_custom_provider(
            &db,
            qoder_config::QoderCustomProvider {
                id: "test-provider".into(),
                name: "Provider".into(),
                base_url: format!("http://127.0.0.1:{upstream_port}/v1"),
                api_key: Some("upstream-secret".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let mut model = qoder_config::save_qoder_custom_route(
            &db,
            qoder_config::QoderCustomRoute {
                id: "test-model".into(),
                provider_id: "test-provider".into(),
                name: "Model".into(),
                model: "actual-model".into(),
                reasoning_effort: Some("high".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let gateway = Arc::new(Gateway::new(db.clone()));
        gateway.start().await.unwrap();
        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/qoder/v1/chat/completions");
        let body = json!({"model":qoder_config::stable_qoder_route_id("test-provider", "test-model"),"messages":[{"role":"user","content":"hello"}],"stream":false});
        let response = client
            .post(&url)
            .bearer_auth("qswitch-local")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(captured.lock().await[0]["model"], "actual-model");
        assert_eq!(captured.lock().await[0]["reasoning_effort"], "high");
        model.enabled = false;
        qoder_config::save_qoder_custom_route(&db, model)
            .await
            .unwrap();
        assert_eq!(
            client
                .post(&url)
                .bearer_auth("qswitch-local")
                .json(&body)
                .send()
                .await
                .unwrap()
                .status(),
            400
        );
        assert_eq!(captured.lock().await.len(), 1);
        gateway.stop().await;
        upstream_task.abort();
    }
    #[tokio::test]
    async fn repeated_start_and_stop_share_one_listener() {
        let db = Arc::new(Database::memory().unwrap());
        // Reserve an ephemeral port, then let the gateway bind it.
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        db.set_setting("qoder_listen_port", &port.to_string())
            .unwrap();
        let gateway = Arc::new(Gateway::new(db));
        assert!(gateway.start().await.unwrap().running);
        assert!(gateway.start().await.unwrap().running);
        let client = reqwest::Client::new();
        let root = format!("http://127.0.0.1:{port}");
        assert_eq!(
            client
                .get(format!("{root}/health"))
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        assert_eq!(
            client
                .post(format!("{root}/v1/messages"))
                .send()
                .await
                .unwrap()
                .status(),
            404
        );
        let response = client
            .post(format!("{root}/qoder/v1/chat/completions"))
            .json(&json!({"model":"missing","messages":[]}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        gateway.stop().await;
        assert!(!gateway.status().await.unwrap().running);
        gateway.stop().await;
    }
}
