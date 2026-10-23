pub mod providers {
    pub mod qoder_wire;
}
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("Configuration error: {0}")]
    ConfigError(String),
    #[error("Upstream request failed: {0}")]
    ForwardFailed(String),
    #[error("Protocol conversion failed: {0}")]
    TransformError(String),
    #[error("Invalid request: {0}")]
    InvalidRequest(String),
}
impl IntoResponse for ProxyError {
    fn into_response(self) -> Response {
        let code = if matches!(self, Self::InvalidRequest(_)) {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::BAD_GATEWAY
        };
        (code, Json(json!({"error":{"message":self.to_string()}}))).into_response()
    }
}
pub mod server;
