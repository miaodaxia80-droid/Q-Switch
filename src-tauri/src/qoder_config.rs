use crate::config::get_app_config_dir;
use crate::database::Database;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;

const QODER_CARRIER_MAPPINGS_SETTING: &str = "qoder_carrier_mappings_v1";
const QODER_TOOL_POLICY_SETTING: &str = "qoder_tool_policy_v1";

/// Capabilities offered to a Qoder session that is routed through Qswitch.
///
/// These are deliberately opt-in. A routed model is an external authority,
/// so exposing a tool in its OpenAI `tools` list is equivalent to granting it
/// the capability. Read-only workspace inspection remains available by
/// default; side-effecting tools require the local Qswitch user to enable
/// them explicitly in the Qoder panel.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QoderToolPolicy {
    pub allow_terminal: bool,
    pub allow_write: bool,
    pub allow_network: bool,
    pub allow_private_network: bool,
    pub allow_mcp: bool,
}

pub fn get_qoder_tool_policy(db: &Database) -> Result<QoderToolPolicy, AppError> {
    let Some(raw) = db.get_setting(QODER_TOOL_POLICY_SETTING)? else {
        return Ok(QoderToolPolicy::default());
    };
    serde_json::from_str(&raw)
        .map_err(|error| AppError::Config(format!("Parse Qoder tool policy failed: {error}")))
}

pub fn save_qoder_tool_policy(
    db: &Database,
    policy: &QoderToolPolicy,
) -> Result<QoderToolPolicy, AppError> {
    // A private-network grant is meaningless without the enclosing network
    // grant. Normalize it here so the stored policy has one unambiguous form.
    let mut normalized = policy.clone();
    if !normalized.allow_network {
        normalized.allow_private_network = false;
    }
    let raw = serde_json::to_string(&normalized).map_err(|error| {
        AppError::Config(format!("Serialize Qoder tool policy failed: {error}"))
    })?;
    db.set_setting(QODER_TOOL_POLICY_SETTING, &raw)?;
    Ok(normalized)
}

/// Qoder's local Agent coordination directory.
///
/// Qoder's native Agent publishes `.info.json` here so the Electron main
/// process can discover its local coordination socket.
pub fn get_qoder_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Qoder")
        .join("SharedClientCache")
}

pub fn get_qoder_info_path() -> PathBuf {
    get_qoder_dir().join(".info.json")
}

pub fn get_qoder_model_manifest_path() -> PathBuf {
    get_app_config_dir().join("qoder-models.json")
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct QoderModelManifest {
    version: u32,
    models: Vec<QoderModelManifestEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub(crate) struct QoderModelManifestEntry {
    /// Raw Qoder custom-model record ID.
    ///
    /// Qoder's renderer prepends `custom:` when it selects a record, so the
    /// manifest must keep this value unprefixed.
    id: String,
    /// Opaque model value sent by Qoder to the local gateway.
    route_id: String,
    /// Q Switch provider selected by this route.
    provider_id: String,
    /// Real model name sent to the upstream provider.
    model: String,
    #[serde(rename = "displayName")]
    display_name: String,
    base_url: String,
    #[serde(rename = "type")]
    protocol_type: String,
    /// The loopback router is ready, but Qoder still needs a native transport
    /// adapter before it can send its Agent request to `base_url`.
    transport_status: String,
    #[serde(rename = "max_input_tokens")]
    max_input_tokens: u64,
    is_reasoning: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QoderRouteTarget {
    pub provider_id: String,
    pub upstream_model: String,
}

/// A native Qoder BYOK model ID selected in the Qoder UI.
///
/// The ID is stored without the `custom:` prefix and contains no provider
/// credential. Q Switch resolves it to an opaque local route only when a user
/// explicitly saves a mapping.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct QoderCarrierMapping {
    pub carrier_model_id: String,
    pub route_id: String,
}

/// A non-secret Q Switch route that can be selected for a Qoder carrier.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QoderRouteOption {
    pub route_id: String,
    pub display_name: String,
    pub provider_id: String,
    pub model: String,
}

/// The stable local route selected by a direct Q Switch model or a native
/// Qoder carrier model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QoderRouteSelection {
    pub route_id: String,
    pub carrier_model_id: Option<String>,
}

fn normalize_qoder_custom_id(value: &str) -> &str {
    value.strip_prefix("custom:").unwrap_or(value)
}

fn normalized_nonempty_qoder_custom_id(value: &str) -> Result<String, AppError> {
    let normalized = normalize_qoder_custom_id(value.trim()).trim();
    if normalized.is_empty() {
        return Err(AppError::Config(
            "Qoder model ID cannot be empty.".to_string(),
        ));
    }
    Ok(normalized.to_string())
}

fn resolve_qoder_route_in_manifest(
    manifest: &QoderModelManifest,
    model_id: &str,
) -> Option<QoderRouteTarget> {
    let route_id = normalize_qoder_custom_id(model_id);
    manifest
        .models
        .iter()
        .find(|entry| {
            normalize_qoder_custom_id(&entry.id) == route_id || entry.route_id == route_id
        })
        .map(|entry| QoderRouteTarget {
            provider_id: entry.provider_id.clone(),
            upstream_model: entry.model.clone(),
        })
}

fn read_qoder_model_manifest() -> Result<Option<QoderModelManifest>, AppError> {
    let path = get_qoder_model_manifest_path();
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::io(&path, error)),
    };
    let manifest: QoderModelManifest = serde_json::from_slice(&bytes)
        .map_err(|error| AppError::Config(format!("Parse Qoder model manifest failed: {error}")))?;
    Ok(Some(manifest))
}

pub fn resolve_qoder_route(model_id: &str) -> Result<Option<QoderRouteTarget>, AppError> {
    Ok(read_qoder_model_manifest()?
        .as_ref()
        .and_then(|manifest| resolve_qoder_route_in_manifest(manifest, model_id)))
}

pub fn list_qoder_carrier_mappings(db: &Database) -> Result<Vec<QoderCarrierMapping>, AppError> {
    let Some(raw) = db.get_setting(QODER_CARRIER_MAPPINGS_SETTING)? else {
        return Ok(Vec::new());
    };
    let mut mappings: Vec<QoderCarrierMapping> = serde_json::from_str(&raw).map_err(|error| {
        AppError::Config(format!("Parse Qoder carrier mappings failed: {error}"))
    })?;
    for mapping in &mut mappings {
        mapping.carrier_model_id = normalized_nonempty_qoder_custom_id(&mapping.carrier_model_id)?;
        mapping.route_id = normalized_nonempty_qoder_custom_id(&mapping.route_id)?;
    }
    mappings.sort_by(|left, right| left.carrier_model_id.cmp(&right.carrier_model_id));
    mappings.dedup_by(|left, right| left.carrier_model_id == right.carrier_model_id);
    Ok(mappings)
}

pub fn save_qoder_carrier_mapping(
    db: &Database,
    carrier_model_id: &str,
    route_id: &str,
) -> Result<QoderCarrierMapping, AppError> {
    let mapping = QoderCarrierMapping {
        carrier_model_id: normalized_nonempty_qoder_custom_id(carrier_model_id)?,
        route_id: normalized_nonempty_qoder_custom_id(route_id)?,
    };
    if mapping.carrier_model_id.starts_with("qswitch_") {
        return Err(AppError::Config(
            "A Q Switch route ID cannot be used as a Qoder carrier model ID.".to_string(),
        ));
    }
    if !mapping.route_id.starts_with("qswitch_") {
        return Err(AppError::Config(
            "Qoder carrier mappings must point to a Q Switch route ID.".to_string(),
        ));
    }
    if resolve_qoder_route(&mapping.route_id)?.is_none() {
        return Err(AppError::Config(
            "Unknown Q Switch route. Refresh the Qoder route catalog first.".to_string(),
        ));
    }

    let mut mappings = list_qoder_carrier_mappings(db)?;
    mappings.retain(|entry| entry.carrier_model_id != mapping.carrier_model_id);
    mappings.push(mapping.clone());
    mappings.sort_by(|left, right| left.carrier_model_id.cmp(&right.carrier_model_id));
    let serialized = serde_json::to_string(&mappings).map_err(|error| {
        AppError::Config(format!("Serialize Qoder carrier mappings failed: {error}"))
    })?;
    db.set_setting(QODER_CARRIER_MAPPINGS_SETTING, &serialized)?;
    Ok(mapping)
}

pub fn remove_qoder_carrier_mapping(db: &Database, carrier_model_id: &str) -> Result<(), AppError> {
    let carrier_model_id = normalized_nonempty_qoder_custom_id(carrier_model_id)?;
    let mut mappings = list_qoder_carrier_mappings(db)?;
    mappings.retain(|entry| entry.carrier_model_id != carrier_model_id);
    let serialized = serde_json::to_string(&mappings).map_err(|error| {
        AppError::Config(format!("Serialize Qoder carrier mappings failed: {error}"))
    })?;
    db.set_setting(QODER_CARRIER_MAPPINGS_SETTING, &serialized)
}

/// Resolve a direct `custom:qswitch_*` selection or an explicitly configured
/// native Qoder carrier model into the route accepted by the loopback gateway.
pub fn resolve_qoder_route_selection(
    db: &Database,
    model_id: &str,
) -> Result<Option<QoderRouteSelection>, AppError> {
    let normalized_model_id = normalized_nonempty_qoder_custom_id(model_id)?;
    if resolve_qoder_route(&normalized_model_id)?.is_some() {
        return Ok(Some(QoderRouteSelection {
            route_id: normalized_model_id,
            carrier_model_id: None,
        }));
    }

    let mappings = list_qoder_carrier_mappings(db)?;
    let Some(mapping) = mappings
        .into_iter()
        .find(|entry| entry.carrier_model_id == normalized_model_id)
    else {
        return Ok(None);
    };
    if resolve_qoder_route(&mapping.route_id)?.is_none() {
        return Err(AppError::Config(
            "Qoder carrier mapping refers to a missing Q Switch route. Refresh the route catalog or update the mapping."
                .to_string(),
        ));
    }
    Ok(Some(QoderRouteSelection {
        route_id: mapping.route_id,
        carrier_model_id: Some(mapping.carrier_model_id),
    }))
}

pub async fn list_qoder_route_options(db: &Database) -> Result<Vec<QoderRouteOption>, AppError> {
    let manifest = match read_qoder_model_manifest()? {
        Some(manifest) => manifest,
        None => {
            sync_qoder_model_manifest(db).await?;
            match read_qoder_model_manifest()? {
                Some(manifest) => manifest,
                None => return Ok(Vec::new()),
            }
        }
    };
    if manifest.models.is_empty() {
        return Ok(Vec::new());
    }
    let mut routes: Vec<QoderRouteOption> = manifest
        .models
        .into_iter()
        .map(|entry| QoderRouteOption {
            route_id: entry.route_id,
            display_name: entry.display_name,
            provider_id: entry.provider_id,
            model: entry.model,
        })
        .collect();
    routes.sort_by(|left, right| left.display_name.cmp(&right.display_name));
    routes.dedup_by(|left, right| left.route_id == right.route_id);
    Ok(routes)
}

/// Export the non-secret Q Switch model catalog for the future native
/// transport adapter.
///
/// The file contains only stable route IDs, upstream model names, and the local
/// gateway URL. Provider credentials remain in the CC Switch database.
pub async fn sync_qoder_model_manifest(db: &Database) -> Result<PathBuf, AppError> {
    let providers = db.get_all_providers("qoder")?;
    let proxy_config = db.get_proxy_config().await?;
    let gateway_base_url = format!("http://127.0.0.1:{}/qoder/v1", proxy_config.listen_port);

    let mut models = Vec::new();
    for provider in providers.values() {
        let settings = provider.settings_config.as_object();
        let config_text = settings
            .and_then(|value| value.get("config"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let default_model = config_text
            .parse::<toml::Value>()
            .ok()
            .and_then(|value| {
                value
                    .get("model")
                    .and_then(toml::Value::as_str)
                    .map(str::to_string)
            })
            .filter(|model| !model.trim().is_empty());

        let catalog_models = settings
            .and_then(|value| value.get("modelCatalog"))
            .and_then(|value| value.get("models"))
            .and_then(Value::as_array);

        let candidates = catalog_models
            .into_iter()
            .flat_map(|items| items.iter())
            .filter_map(|item| {
                let model = item
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|model| !model.is_empty())?;
                let display_name = item
                    .get("displayName")
                    .or_else(|| item.get("display_name"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(model);
                let max_input_tokens = item
                    .get("contextWindow")
                    .or_else(|| item.get("context_window"))
                    .and_then(Value::as_u64)
                    .unwrap_or(128_000);
                let is_reasoning = item
                    .get("is_reasoning")
                    .or_else(|| item.get("isReasoning"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                Some((
                    model.to_string(),
                    display_name.to_string(),
                    max_input_tokens,
                    is_reasoning,
                ))
            })
            .chain(
                default_model
                    .into_iter()
                    .map(|model| (model.clone(), model, 128_000, false)),
            );

        for (model, display_name, max_input_tokens, is_reasoning) in candidates {
            let mut digest = Sha256::new();
            digest.update(provider.id.as_bytes());
            digest.update([0]);
            digest.update(model.as_bytes());
            let route_hash = format!("{:x}", digest.finalize());
            let route_id = format!("qswitch_{}", &route_hash[..16]);
            models.push(QoderModelManifestEntry {
                id: route_id.clone(),
                route_id,
                provider_id: provider.id.clone(),
                model,
                display_name: format!("{} / {}", provider.name, display_name),
                base_url: gateway_base_url.clone(),
                protocol_type: "openai".to_string(),
                transport_status: "native-adapter-required".to_string(),
                max_input_tokens,
                is_reasoning,
            });
        }
    }

    models.sort_by(|left, right| left.id.cmp(&right.id));
    models.dedup_by(|left, right| left.id == right.id);

    let path = get_qoder_model_manifest_path();
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("Qoder manifest has no parent directory".to_string()))?;
    fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;

    let manifest = QoderModelManifest { version: 3, models };
    let serialized = serde_json::to_vec_pretty(&manifest)
        .map_err(|e| AppError::Config(format!("Serialize Qoder model manifest failed: {e}")))?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serialized).map_err(|e| AppError::io(&temporary, e))?;
    fs::rename(&temporary, &path).map_err(|e| AppError::io(&path, e))?;

    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_path_is_inside_shared_client_cache() {
        assert!(get_qoder_info_path().ends_with("Qoder/SharedClientCache/.info.json"));
    }

    #[test]
    fn model_manifest_path_uses_qswitch_config_directory() {
        assert!(get_qoder_model_manifest_path().ends_with("qoder-models.json"));
    }

    #[test]
    fn custom_model_ids_are_normalized_without_losing_route_identity() {
        assert_eq!(
            normalize_qoder_custom_id("custom:qswitch_123"),
            "qswitch_123"
        );
        assert_eq!(normalize_qoder_custom_id("qswitch_123"), "qswitch_123");
    }

    #[test]
    fn carrier_model_ids_normalize_the_custom_prefix() {
        assert_eq!(
            normalized_nonempty_qoder_custom_id(" custom:model_123 ").unwrap(),
            "model_123"
        );
        assert!(normalized_nonempty_qoder_custom_id("custom:").is_err());
    }

    #[test]
    fn carrier_mappings_are_loaded_without_the_custom_prefix() {
        let db = Database::memory().unwrap();
        db.set_setting(
            QODER_CARRIER_MAPPINGS_SETTING,
            r#"[{"carrierModelId":"custom:model_123","routeId":"custom:qswitch_abc"}]"#,
        )
        .unwrap();

        assert_eq!(
            list_qoder_carrier_mappings(&db).unwrap(),
            vec![QoderCarrierMapping {
                carrier_model_id: "model_123".to_string(),
                route_id: "qswitch_abc".to_string(),
            }]
        );
    }

    #[test]
    fn tool_policy_defaults_to_read_only_and_normalizes_private_network() {
        let db = Database::memory().unwrap();
        assert_eq!(
            get_qoder_tool_policy(&db).unwrap(),
            QoderToolPolicy::default()
        );

        let saved = save_qoder_tool_policy(
            &db,
            &QoderToolPolicy {
                allow_terminal: true,
                allow_write: true,
                allow_network: false,
                allow_private_network: true,
                allow_mcp: true,
            },
        )
        .unwrap();
        assert!(saved.allow_terminal && saved.allow_write && saved.allow_mcp);
        assert!(!saved.allow_private_network);
        assert_eq!(get_qoder_tool_policy(&db).unwrap(), saved);
    }

    #[test]
    fn manifest_entry_serializes_raw_id_and_provider_route() {
        let entry = QoderModelManifestEntry {
            id: "qswitch_123".to_string(),
            route_id: "qswitch_123".to_string(),
            provider_id: "provider-a".to_string(),
            model: "upstream-model".to_string(),
            display_name: "Provider A / Upstream".to_string(),
            base_url: "http://127.0.0.1:15731/qoder/v1".to_string(),
            protocol_type: "openai".to_string(),
            transport_status: "native-adapter-required".to_string(),
            max_input_tokens: 128_000,
            is_reasoning: false,
        };
        let value = serde_json::to_value(entry).expect("serialize manifest entry");
        assert_eq!(value["id"], "qswitch_123");
        assert_eq!(value["route_id"], "qswitch_123");
        assert_eq!(value["provider_id"], "provider-a");
        assert_eq!(value["model"], "upstream-model");
        assert_eq!(value["type"], "openai");
        assert_eq!(value["transport_status"], "native-adapter-required");
    }

    #[test]
    fn route_resolver_accepts_qoder_full_id_and_gateway_route_id() {
        let manifest = QoderModelManifest {
            version: 3,
            models: vec![QoderModelManifestEntry {
                id: "qswitch_123".to_string(),
                route_id: "qswitch_123".to_string(),
                provider_id: "provider-a".to_string(),
                model: "upstream-model".to_string(),
                display_name: "Provider A / Upstream".to_string(),
                base_url: "http://127.0.0.1:15731/qoder/v1".to_string(),
                protocol_type: "openai".to_string(),
                transport_status: "native-adapter-required".to_string(),
                max_input_tokens: 128_000,
                is_reasoning: false,
            }],
        };
        let expected = QoderRouteTarget {
            provider_id: "provider-a".to_string(),
            upstream_model: "upstream-model".to_string(),
        };
        assert_eq!(
            resolve_qoder_route_in_manifest(&manifest, "custom:qswitch_123"),
            Some(expected.clone())
        );
        assert_eq!(
            resolve_qoder_route_in_manifest(&manifest, "qswitch_123"),
            Some(expected)
        );
    }
}
