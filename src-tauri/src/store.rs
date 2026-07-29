use crate::database::Database;
use crate::services::{ProxyService, UsageCache};
use std::sync::Arc;
use tokio::sync::Mutex;

/// 全局应用状态
pub struct AppState {
    pub db: Arc<Database>,
    pub proxy_service: ProxyService,
    pub usage_cache: Arc<UsageCache>,
    /// Optional transparent adapter in front of Qoder's native local Agent.
    /// It is deliberately separate from the HTTP proxy service: this handle
    /// owns the `.info.json` snapshot required to restore Qoder on disable.
    pub qoder_native_adapter: Mutex<Option<crate::qoder_acp::NativeProxyHandle>>,
}

impl AppState {
    /// 创建新的应用状态
    pub fn new(db: Arc<Database>) -> Self {
        let proxy_service = ProxyService::new(db.clone());

        Self {
            db,
            proxy_service,
            usage_cache: Arc::new(UsageCache::new()),
            qoder_native_adapter: Mutex::new(None),
        }
    }
}
