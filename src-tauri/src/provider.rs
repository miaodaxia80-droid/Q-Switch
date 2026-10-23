use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub settings_config: Value,
}
impl Provider {
    pub fn with_id(
        id: String,
        name: String,
        settings_config: Value,
        _website_url: Option<String>,
    ) -> Self {
        Self {
            id,
            name,
            settings_config,
        }
    }
}
