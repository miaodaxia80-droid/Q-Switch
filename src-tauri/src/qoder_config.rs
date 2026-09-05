use crate::config::get_app_config_dir;
use crate::database::Database;
use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

const QODER_CARRIER_MAPPINGS_SETTING: &str = "qoder_carrier_mappings_v1";
const QODER_TOOL_POLICY_SETTING: &str = "qoder_tool_policy_v1";
/// Custom MODELS (one provider owns many models).
const QODER_CUSTOM_ROUTES_SETTING: &str = "qoder_custom_routes_v1";
/// Custom PROVIDERS (shared base URL / key / API format for many models).
const QODER_CUSTOM_PROVIDERS_SETTING: &str = "qoder_custom_providers_v1";
/// One-time backup of the pre-tri-protocol flat routes, kept for rollback.
const QODER_LEGACY_ROUTES_BACKUP_SETTING: &str = "qoder_custom_routes_v1_pre_triprotocol_backup";

pub const QODER_API_FORMAT_OPENAI_CHAT: &str = "openai_chat";
#[cfg(test)]
pub const QODER_API_FORMAT_ANTHROPIC: &str = "anthropic_messages";
#[cfg(test)]
pub const QODER_API_FORMAT_RESPONSES: &str = "openai_responses";

fn default_true() -> bool {
    true
}

fn default_max_tokens() -> u64 {
    128_000
}

/// A user-defined Qoder **provider**: one shared connection (base URL, key,
/// API format) that owns one or more custom models.
///
/// The API key is stored locally only and is never serialized back to the UI
/// (`skip_serializing`); `has_api_key` lets the editor show whether one exists.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct QoderCustomProvider {
    /// Stable provider uuid (simple, no prefix).
    pub id: String,
    /// Provider display name.
    pub name: String,
    /// Base URL or full endpoint, validated against the selected API format.
    pub base_url: String,
    /// Local-only upstream credential. Persisted in the local DB, but stripped
    /// by `mask_custom_provider` before any list/save response reaches the UI.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Derived for list responses (not persisted).
    pub has_api_key: bool,
    /// `openai_chat` | `anthropic_messages` | `openai_responses`.
    pub api_format: String,
    /// false = append the format path to `base_url`; true = use it verbatim.
    pub is_full_url: bool,
    /// Override the Anthropic `anthropic-version` header (optional).
    pub anthropic_version: Option<String>,
    pub enabled: bool,
}

impl Default for QoderCustomProvider {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            base_url: String::new(),
            api_key: None,
            has_api_key: false,
            api_format: QODER_API_FORMAT_OPENAI_CHAT.to_string(),
            is_full_url: false,
            anthropic_version: None,
            enabled: true,
        }
    }
}

/// A custom **model** owned by a [`QoderCustomProvider`].
///
/// Its stable Q Switch route id is derived from the provider and model uuids
/// (`qswitch_<provider>_<model>`), so renaming the upstream `model` never
/// breaks an existing Qoder carrier mapping.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct QoderCustomRoute {
    /// Stable model uuid (simple). Combined with `provider_id` for route id.
    pub id: String,
    /// Owning [`QoderCustomProvider::id`].
    pub provider_id: String,
    /// Display name for this specific model.
    pub name: String,
    /// Real upstream model name (arbitrary, editable without losing identity).
    pub model: String,
    /// Optional thinking intensity (`low` / `medium` / `high`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(default = "default_true")]
    pub is_reasoning: bool,
    #[serde(default = "default_max_tokens")]
    pub max_input_tokens: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
    // --- Legacy flat-route fields, present only in pre-migration records. ---
    #[serde(skip_serializing)]
    pub base_url: Option<String>,
    #[serde(skip_serializing)]
    pub api_key: Option<String>,
}

impl Default for QoderCustomRoute {
    fn default() -> Self {
        Self {
            id: String::new(),
            provider_id: String::new(),
            name: String::new(),
            model: String::new(),
            reasoning_effort: None,
            is_reasoning: true,
            max_input_tokens: 128_000,
            enabled: true,
            base_url: None,
            api_key: None,
        }
    }
}

/// Stable Q Switch route id exposed to Qoder for a provider/model pair.
pub fn stable_qoder_route_id(provider_id: &str, model_id: &str) -> String {
    format!("qswitch_{provider_id}_{model_id}")
}

/// The hidden `qoder` provider id mirroring a custom provider in the DB.
fn provider_id_for_custom_provider(provider_uuid: &str) -> String {
    format!("qswitch-custom-{provider_uuid}")
}

/// Legacy hidden provider id (pre-tri-protocol flat routes), retained so the
/// migration can recompute and remove the old mirror providers / route hashes.
fn legacy_provider_id_for_route(route_id: &str) -> String {
    format!("qswitch-custom-{route_id}")
}

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
    /// Optional thinking intensity applied by the local gateway before
    /// forwarding (`low` / `medium` / `high`). Absent for provider routes
    /// that do not configure one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_effort: Option<String>,
    /// Upstream wire format used by Q Switch: openai_chat / anthropic_messages
    /// / openai_responses. Qoder itself always speaks Chat to the gateway.
    #[serde(default = "default_api_format", rename = "apiFormat")]
    api_format: String,
    /// Whether the provider address is a full endpoint (no path appended).
    #[serde(default, rename = "isFullUrl")]
    is_full_url: bool,
}

fn default_api_format() -> String {
    QODER_API_FORMAT_OPENAI_CHAT.to_string()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QoderRouteTarget {
    pub provider_id: String,
    pub upstream_model: String,
    /// Optional `reasoning_effort` the gateway should inject. `None` means
    /// leave the client-provided value untouched.
    pub reasoning_effort: Option<String>,
    /// Upstream wire format the gateway must convert to.
    pub api_format: String,
    /// Whether the provider address is a full endpoint.
    pub is_full_url: bool,
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
            reasoning_effort: entry.reasoning_effort.clone(),
            api_format: entry.api_format.clone(),
            is_full_url: entry.is_full_url,
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

fn listed_qoder_custom_routes(db: &Database) -> Result<Vec<QoderCustomRoute>, AppError> {
    let Some(raw) = db.get_setting(QODER_CUSTOM_ROUTES_SETTING)? else {
        return Ok(Vec::new());
    };
    let mut routes: Vec<QoderCustomRoute> = serde_json::from_str(&raw)
        .map_err(|error| AppError::Config(format!("Parse Qoder custom routes failed: {error}")))?;
    routes.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(routes)
}

/// Persist the custom-route list back to the settings table.
fn store_qoder_custom_routes(db: &Database, routes: &[QoderCustomRoute]) -> Result<(), AppError> {
    let serialized = serde_json::to_string(routes).map_err(|error| {
        AppError::Config(format!("Serialize Qoder custom routes failed: {error}"))
    })?;
    db.set_setting(QODER_CUSTOM_ROUTES_SETTING, &serialized)
}

// ---------------------------------------------------------------------------
// Custom providers (shared connection) persistence
// ---------------------------------------------------------------------------

fn load_custom_providers_raw(db: &Database) -> Result<Vec<QoderCustomProvider>, AppError> {
    let Some(raw) = db.get_setting(QODER_CUSTOM_PROVIDERS_SETTING)? else {
        return Ok(Vec::new());
    };
    serde_json::from_str(&raw)
        .map_err(|error| AppError::Config(format!("Parse Qoder custom providers failed: {error}")))
}

fn store_custom_providers(
    db: &Database,
    providers: &[QoderCustomProvider],
) -> Result<(), AppError> {
    let serialized = serde_json::to_string(providers).map_err(|error| {
        AppError::Config(format!("Serialize Qoder custom providers failed: {error}"))
    })?;
    db.set_setting(QODER_CUSTOM_PROVIDERS_SETTING, &serialized)
}

/// Strip the secret before returning a provider to the UI; report presence only.
fn mask_custom_provider(mut provider: QoderCustomProvider) -> QoderCustomProvider {
    provider.has_api_key = provider
        .api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|k| !k.is_empty());
    provider.api_key = None;
    provider
}

/// Mirror one custom provider (with all its models) as a hidden `qoder`
/// provider so manifest generation and gateway forwarding resolve every model.
fn sync_custom_provider_mirror(
    db: &Database,
    provider: &QoderCustomProvider,
    models: &[QoderCustomRoute],
) -> Result<(), AppError> {
    let mut config = serde_json::Map::new();
    config.insert("base_url".to_string(), json!(provider.base_url));
    if let Some(api_key) = provider
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        config.insert("apiKey".to_string(), json!(api_key));
    }
    config.insert("apiFormat".to_string(), json!(provider.api_format));
    config.insert("isFullUrl".to_string(), json!(provider.is_full_url));
    if let Some(version) = provider
        .anthropic_version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        config.insert("anthropicVersion".to_string(), json!(version));
    }
    let catalog: Vec<Value> = models
        .iter()
        .filter(|model| model.enabled)
        .map(|model| {
            json!({
                "model": model.model,
                "displayName": model.name,
                "contextWindow": model.max_input_tokens,
                "is_reasoning": model.is_reasoning,
                "routeId": stable_qoder_route_id(&provider.id, &model.id),
                "reasoning_effort": model.reasoning_effort,
            })
        })
        .collect();
    config.insert("modelCatalog".to_string(), json!({ "models": catalog }));

    let hidden = crate::provider::Provider::with_id(
        provider_id_for_custom_provider(&provider.id),
        provider.name.clone(),
        Value::Object(config),
        None,
    );
    db.save_provider("qoder", &hidden).map_err(|error| {
        AppError::Config(format!(
            "Persist Qoder custom provider mirror failed: {error}"
        ))
    })?;
    Ok(())
}

/// Re-create the hidden mirror for one provider from its persisted models.
fn resync_custom_provider(db: &Database, provider_id: &str) -> Result<(), AppError> {
    let Some(provider) = load_custom_providers_raw(db)?
        .into_iter()
        .find(|item| item.id == provider_id)
    else {
        return Ok(());
    };
    let models = listed_qoder_custom_routes(db)?
        .into_iter()
        .filter(|model| model.provider_id == provider_id)
        .collect::<Vec<_>>();
    if models.is_empty() {
        db.delete_provider("qoder", &provider_id_for_custom_provider(provider_id))?;
    } else {
        sync_custom_provider_mirror(db, &provider, &models)?;
    }
    Ok(())
}

fn normalize_reasoning_effort(effort: Option<String>) -> Result<Option<String>, AppError> {
    match effort.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => {
            let lower = value.to_ascii_lowercase();
            if !matches!(lower.as_str(), "low" | "medium" | "high") {
                return Err(AppError::Config(
                    "reasoning_effort must be one of: low, medium, high".to_string(),
                ));
            }
            Ok(Some(lower))
        }
        _ => Ok(None),
    }
}

fn normalize_custom_provider(
    mut provider: QoderCustomProvider,
    existing: Option<&QoderCustomProvider>,
) -> Result<QoderCustomProvider, AppError> {
    provider.name = provider.name.trim().to_string();
    provider.base_url = provider.base_url.trim().to_string();
    if provider.id.trim().is_empty() {
        provider.id = uuid::Uuid::new_v4().simple().to_string();
    }
    provider.id = provider.id.trim().to_string();
    if provider.name.is_empty() {
        return Err(AppError::Config(
            "Provider name cannot be empty.".to_string(),
        ));
    }
    if provider.base_url.is_empty() {
        return Err(AppError::Config(
            "Provider base URL cannot be empty.".to_string(),
        ));
    }
    // Validate the API format and that the address is reachable as a URL.
    let format = crate::proxy::providers::qoder_wire::QoderApiFormat::parse(&provider.api_format)
        .map_err(|e| AppError::Config(e.to_string()))?;
    provider.api_format = format.as_str().to_string();
    crate::proxy::providers::qoder_wire::build_upstream_endpoint(
        &provider.base_url,
        format,
        provider.is_full_url,
    )
    .map_err(|e| AppError::Config(e.to_string()))?;
    // API key merge: None = keep stored; Some("") = clear; Some(non-empty) = set.
    provider.api_key = match provider.api_key {
        Some(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        None => existing.and_then(|item| item.api_key.clone()),
    };
    provider.has_api_key = provider
        .api_key
        .as_deref()
        .map(str::trim)
        .is_some_and(|key| !key.is_empty());
    if !provider.enabled {
        provider.enabled = false;
    } else {
        provider.enabled = true;
    }
    Ok(provider)
}

fn normalize_custom_model(
    mut model: QoderCustomRoute,
    known_provider_ids: &[String],
) -> Result<QoderCustomRoute, AppError> {
    model.name = model.name.trim().to_string();
    model.model = model.model.trim().to_string();
    model.provider_id = model.provider_id.trim().to_string();
    if model.id.trim().is_empty() {
        model.id = uuid::Uuid::new_v4().simple().to_string();
    }
    model.id = model.id.trim().to_string();
    if model.provider_id.is_empty() {
        return Err(AppError::Config(
            "Custom model must belong to a provider.".to_string(),
        ));
    }
    if !known_provider_ids.iter().any(|id| id == &model.provider_id) {
        return Err(AppError::Config(
            "Custom model references an unknown provider.".to_string(),
        ));
    }
    if model.name.is_empty() {
        return Err(AppError::Config(
            "Model display name cannot be empty.".to_string(),
        ));
    }
    if model.model.is_empty() {
        return Err(AppError::Config(
            "Upstream model name cannot be empty.".to_string(),
        ));
    }
    model.reasoning_effort = normalize_reasoning_effort(model.reasoning_effort)?;
    // Legacy flat fields never survive normalization.
    model.base_url = None;
    model.api_key = None;
    Ok(model)
}

/// Reproduce the pre-tri-protocol hash-derived route id so the migration can
/// rewrite existing carrier mappings to the new stable ids.
fn legacy_hash_route_id(hidden_provider_id: &str, upstream_model: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(hidden_provider_id.as_bytes());
    digest.update([0]);
    digest.update(upstream_model.as_bytes());
    let hash = format!("{:x}", digest.finalize());
    format!("qswitch_{}", &hash[..16])
}

/// One-time migration: split the old flat `qoder_custom_routes_v1` records
/// (each carrying its own base URL/key) into providers + models with stable
/// ids, rewrite carrier mappings, and keep a backup of the legacy raw setting.
/// Idempotent: no-op once every model has a non-empty `provider_id`.
fn ensure_custom_model_migration(db: &Database) -> Result<(), AppError> {
    let Some(legacy_raw) = db.get_setting(QODER_CUSTOM_ROUTES_SETTING)? else {
        return Ok(());
    };
    let parsed: Vec<QoderCustomRoute> = match serde_json::from_str(&legacy_raw) {
        Ok(routes) => routes,
        // Corrupt/unknown shape: leave untouched rather than destroying data.
        Err(_) => return Ok(()),
    };
    let has_legacy = parsed
        .iter()
        .any(|route| route.provider_id.is_empty() && route.base_url.is_some());
    if !has_legacy {
        return Ok(());
    }

    log::info!(
        "[QoderConfig] migrating {} flat custom routes to provider/model",
        parsed.len()
    );
    let mut providers = load_custom_providers_raw(db)?;
    let mut migrated_models: Vec<QoderCustomRoute> = Vec::new();
    let mut route_id_rewrite: HashMap<String, String> = HashMap::new();
    let mut legacy_hidden_provider_ids = Vec::new();

    for old in parsed {
        if !old.provider_id.is_empty() {
            migrated_models.push(old);
            continue;
        }
        let provider_uuid = uuid::Uuid::new_v4().simple().to_string();
        let model_uuid = uuid::Uuid::new_v4().simple().to_string();
        let base_url = old.base_url.clone().unwrap_or_default();
        let api_key = old.api_key.clone();
        let provider = QoderCustomProvider {
            id: provider_uuid.clone(),
            name: old.name.clone(),
            base_url,
            has_api_key: api_key.is_some(),
            api_key,
            api_format: QODER_API_FORMAT_OPENAI_CHAT.to_string(),
            is_full_url: false,
            anthropic_version: None,
            enabled: true,
        };
        let old_hidden = legacy_provider_id_for_route(&old.id);
        let old_route = legacy_hash_route_id(&old_hidden, &old.model);
        let new_route = stable_qoder_route_id(&provider_uuid, &model_uuid);
        route_id_rewrite.insert(old_route, new_route);
        // Remove old flat-route mirrors only after every new setting and
        // mirror has been persisted successfully. This keeps rollback data
        // usable if any later step fails.
        legacy_hidden_provider_ids.push(old_hidden);
        providers.push(provider);
        migrated_models.push(QoderCustomRoute {
            id: model_uuid,
            provider_id: provider_uuid,
            name: old.name,
            model: old.model,
            reasoning_effort: old.reasoning_effort,
            is_reasoning: old.is_reasoning,
            max_input_tokens: old.max_input_tokens,
            enabled: true,
            base_url: None,
            api_key: None,
        });
    }

    // Rewrite carrier mappings BEFORE declaring success.
    if !route_id_rewrite.is_empty() {
        if let Ok(mut mappings) = list_qoder_carrier_mappings(db) {
            let mut changed = false;
            for mapping in &mut mappings {
                if let Some(new_route) = route_id_rewrite.get(&mapping.route_id) {
                    mapping.route_id = new_route.clone();
                    changed = true;
                }
            }
            if changed {
                let serialized = serde_json::to_string(&mappings).map_err(|error| {
                    AppError::Config(format!(
                        "Serialize rewritten carrier mappings failed: {error}"
                    ))
                })?;
                db.set_setting(QODER_CARRIER_MAPPINGS_SETTING, &serialized)?;
            }
        }
    }

    store_custom_providers(db, &providers)?;
    store_qoder_custom_routes(db, &migrated_models)?;
    // Keep the legacy payload for rollback (never hard-delete on first migration).
    if db
        .get_setting(QODER_LEGACY_ROUTES_BACKUP_SETTING)?
        .is_none()
    {
        db.set_setting(QODER_LEGACY_ROUTES_BACKUP_SETTING, &legacy_raw)?;
    }
    for provider in &providers {
        let models = migrated_models
            .iter()
            .filter(|model| model.provider_id == provider.id)
            .cloned()
            .collect::<Vec<_>>();
        sync_custom_provider_mirror(db, provider, &models)?;
    }
    for old_hidden in legacy_hidden_provider_ids {
        db.delete_provider("qoder", &old_hidden)?;
    }
    log::info!(
        "[QoderConfig] migration complete: {} providers, {} models, {} mappings rewritten",
        providers.len(),
        migrated_models.len(),
        route_id_rewrite.len()
    );
    Ok(())
}

pub fn list_qoder_custom_providers(db: &Database) -> Result<Vec<QoderCustomProvider>, AppError> {
    ensure_custom_model_migration(db)?;
    let mut providers = load_custom_providers_raw(db)?
        .into_iter()
        .map(mask_custom_provider)
        .collect::<Vec<_>>();
    providers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(providers)
}

/// Create or update a custom provider and re-mirror all of its models.
pub async fn save_qoder_custom_provider(
    db: &Database,
    provider: QoderCustomProvider,
) -> Result<QoderCustomProvider, AppError> {
    ensure_custom_model_migration(db)?;
    let mut providers = load_custom_providers_raw(db)?;
    let existing = providers
        .iter()
        .find(|item| item.id == provider.id || (!provider.id.is_empty() && item.id == provider.id))
        .cloned();
    let normalized = normalize_custom_provider(provider, existing.as_ref())?;
    providers.retain(|item| item.id != normalized.id);
    providers.push(normalized.clone());
    store_custom_providers(db, &providers)?;
    let models = listed_qoder_custom_routes(db)?
        .into_iter()
        .filter(|model| model.provider_id == normalized.id)
        .collect::<Vec<_>>();
    sync_custom_provider_mirror(db, &normalized, &models)?;
    sync_qoder_model_manifest(db).await?;
    log::info!(
        "[QoderConfig] persisted custom provider {} ({}, {} models)",
        normalized.name,
        normalized.api_format,
        models.len()
    );
    Ok(mask_custom_provider(normalized))
}

/// Delete a provider, all of its models, its hidden mirror and any carrier
/// mappings that pointed at its routes.
pub async fn delete_qoder_custom_provider(db: &Database, id: &str) -> Result<(), AppError> {
    ensure_custom_model_migration(db)?;
    let mut providers = load_custom_providers_raw(db)?;
    providers.retain(|item| item.id != id);
    store_custom_providers(db, &providers)?;

    let mut routes = listed_qoder_custom_routes(db)?;
    let removed_route_ids: std::collections::HashSet<String> = routes
        .iter()
        .filter(|model| model.provider_id == id)
        .map(|model| stable_qoder_route_id(id, &model.id))
        .collect();
    routes.retain(|model| model.provider_id != id);
    store_qoder_custom_routes(db, &routes)?;

    let _ = db.delete_provider("qoder", &provider_id_for_custom_provider(id));
    if !removed_route_ids.is_empty() {
        let mut mappings = list_qoder_carrier_mappings(db)?;
        mappings.retain(|mapping| !removed_route_ids.contains(&mapping.route_id));
        let serialized = serde_json::to_string(&mappings).map_err(|error| {
            AppError::Config(format!("Serialize pruned carrier mappings failed: {error}"))
        })?;
        db.set_setting(QODER_CARRIER_MAPPINGS_SETTING, &serialized)?;
    }
    sync_qoder_model_manifest(db).await?;
    log::info!("[QoderConfig] deleted custom provider {id}");
    Ok(())
}

pub fn list_qoder_custom_routes(db: &Database) -> Result<Vec<QoderCustomRoute>, AppError> {
    ensure_custom_model_migration(db)?;
    listed_qoder_custom_routes(db)
}

/// Create or update a custom MODEL under an existing provider.
pub async fn save_qoder_custom_route(
    db: &Database,
    route: QoderCustomRoute,
) -> Result<QoderCustomRoute, AppError> {
    ensure_custom_model_migration(db)?;
    let provider_ids = load_custom_providers_raw(db)?
        .into_iter()
        .map(|provider| provider.id)
        .collect::<Vec<_>>();
    let normalized = normalize_custom_model(route, &provider_ids)?;
    let mut routes = listed_qoder_custom_routes(db)?;
    routes.retain(|entry| entry.id != normalized.id);
    routes.push(normalized.clone());
    store_qoder_custom_routes(db, &routes)?;
    resync_custom_provider(db, &normalized.provider_id)?;
    sync_qoder_model_manifest(db).await?;
    log::info!(
        "[QoderConfig] persisted custom model {} -> {} under provider {}",
        normalized.name,
        normalized.model,
        normalized.provider_id
    );
    Ok(normalized)
}

/// Delete a model and refresh its parent provider mirror / manifest.
pub async fn delete_qoder_custom_route(db: &Database, id: &str) -> Result<(), AppError> {
    ensure_custom_model_migration(db)?;
    let mut routes = listed_qoder_custom_routes(db)?;
    let provider_id = routes
        .iter()
        .find(|entry| entry.id == id)
        .map(|entry| entry.provider_id.clone());
    routes.retain(|entry| entry.id != id);
    store_qoder_custom_routes(db, &routes)?;
    if let Some(provider_id) = provider_id {
        resync_custom_provider(db, &provider_id)?;
    }
    sync_qoder_model_manifest(db).await?;
    log::info!("[QoderConfig] deleted custom model {id}");
    Ok(())
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
    // Bring any legacy flat routes up to provider/model before listing mirrors.
    ensure_custom_model_migration(db)?;
    let providers = db.get_all_providers("qoder")?;
    let proxy_config = db.get_proxy_config().await?;
    let gateway_base_url = format!("http://127.0.0.1:{}/qoder/v1", proxy_config.listen_port);

    let mut models = Vec::new();
    for provider in providers.values() {
        let settings = provider.settings_config.as_object();
        // Provider-level upstream protocol (Q Switch-internal).
        let api_format = settings
            .and_then(|value| value.get("apiFormat").or_else(|| value.get("api_format")))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| QODER_API_FORMAT_OPENAI_CHAT.to_string());
        let is_full_url = settings
            .and_then(|value| value.get("isFullUrl").or_else(|| value.get("is_full_url")))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let provider_effort = settings
            .and_then(|value| value.get("reasoning_effort"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|effort| !effort.is_empty())
            .map(str::to_string);

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

        // (model, display, max_tokens, is_reasoning, stable routeId, per-model effort)
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
                let stable_route = item
                    .get("routeId")
                    .or_else(|| item.get("route_id"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|route| !route.is_empty())
                    .map(str::to_string);
                let model_effort = item
                    .get("reasoning_effort")
                    .or_else(|| item.get("reasoningEffort"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|effort| !effort.is_empty())
                    .map(str::to_string);
                Some((
                    model.to_string(),
                    display_name.to_string(),
                    max_input_tokens,
                    is_reasoning,
                    stable_route,
                    model_effort,
                ))
            })
            .chain(
                default_model
                    .into_iter()
                    .map(|model| (model.clone(), model, 128_000, false, None, None)),
            );

        for (model, display_name, max_input_tokens, is_reasoning, stable_route, model_effort) in
            candidates
        {
            // Prefer the stable provider/model route id; fall back to hashing
            // for non-custom qoder providers.
            let route_id = stable_route.unwrap_or_else(|| {
                let mut digest = Sha256::new();
                digest.update(provider.id.as_bytes());
                digest.update([0]);
                digest.update(model.as_bytes());
                let route_hash = format!("{:x}", digest.finalize());
                format!("qswitch_{}", &route_hash[..16])
            });
            let reasoning_effort = model_effort.or(provider_effort.clone());
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
                reasoning_effort,
                api_format: api_format.clone(),
                is_full_url,
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

    fn test_provider(id: &str, format: &str) -> QoderCustomProvider {
        QoderCustomProvider {
            id: id.to_string(),
            name: "Mirror Provider".to_string(),
            base_url: "https://gateway.example.com/v1".to_string(),
            api_key: Some("sk-secret".to_string()),
            has_api_key: true,
            api_format: format.to_string(),
            is_full_url: false,
            anthropic_version: None,
            enabled: true,
        }
    }

    fn test_model(provider_id: &str, id: &str, model: &str) -> QoderCustomRoute {
        QoderCustomRoute {
            id: id.to_string(),
            provider_id: provider_id.to_string(),
            name: "Mirror Model".to_string(),
            model: model.to_string(),
            reasoning_effort: Some("high".to_string()),
            is_reasoning: true,
            max_input_tokens: 200_000,
            enabled: true,
            base_url: None,
            api_key: None,
        }
    }

    #[test]
    fn custom_provider_is_mirrored_as_a_hidden_qoder_provider() {
        let db = Database::memory().unwrap();
        let provider = test_provider("prov1", QODER_API_FORMAT_ANTHROPIC);
        let model = test_model("prov1", "model1", "claude-upstream");
        sync_custom_provider_mirror(&db, &provider, &[model.clone()]).unwrap();

        let hidden = db
            .get_provider_by_id(&provider_id_for_custom_provider("prov1"), "qoder")
            .unwrap()
            .expect("hidden provider must exist");
        assert_eq!(hidden.name, "Mirror Provider");
        let config = hidden.settings_config.clone();
        assert_eq!(config["base_url"], "https://gateway.example.com/v1");
        assert_eq!(config["apiKey"], "sk-secret");
        assert_eq!(config["apiFormat"], QODER_API_FORMAT_ANTHROPIC);
        assert_eq!(config["isFullUrl"], false);
        let catalog = &config["modelCatalog"]["models"][0];
        assert_eq!(catalog["model"], "claude-upstream");
        assert_eq!(catalog["routeId"], stable_qoder_route_id("prov1", "model1"));
    }

    #[test]
    fn stable_route_id_survives_upstream_model_rename() {
        let first = stable_qoder_route_id("prov", "model-uuid");
        // Renaming the upstream model name does NOT change the route id.
        assert_eq!(first, stable_qoder_route_id("prov", "model-uuid"));
        assert!(first.starts_with("qswitch_prov_"));
    }

    #[test]
    fn custom_model_list_round_trips_through_settings() {
        let db = Database::memory().unwrap();
        let model = test_model("prov1", "model_list", "gpt-list-test");
        store_qoder_custom_routes(&db, &[model.clone()]).unwrap();
        let listed = listed_qoder_custom_routes(&db).unwrap();
        assert_eq!(listed, vec![model]);
    }

    #[test]
    fn provider_validation_normalizes_and_rejects_bad_inputs() {
        let good = QoderCustomProvider {
            id: String::new(),
            name: "  Acme  ".to_string(),
            base_url: " https://api.anthropic.com ".to_string(),
            api_key: Some(" key ".to_string()),
            has_api_key: false,
            api_format: "anthropic".to_string(), // alias accepted
            is_full_url: false,
            anthropic_version: None,
            enabled: true,
        };
        let normalized = normalize_custom_provider(good, None).unwrap();
        assert_eq!(normalized.name, "Acme");
        assert_eq!(normalized.api_format, QODER_API_FORMAT_ANTHROPIC);
        assert_eq!(normalized.api_key.as_deref(), Some("key"));
        assert!(normalized.has_api_key);
        assert!(!normalized.id.is_empty()); // generated

        // None key keeps the existing stored key.
        let keep = QoderCustomProvider {
            id: normalized.id.clone(),
            name: "Acme".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            api_key: None,
            has_api_key: false,
            api_format: QODER_API_FORMAT_ANTHROPIC.to_string(),
            is_full_url: false,
            anthropic_version: None,
            enabled: true,
        };
        let kept = normalize_custom_provider(keep, Some(&normalized)).unwrap();
        assert_eq!(kept.api_key.as_deref(), Some("key"));

        let bad_format = QoderCustomProvider {
            id: "x".to_string(),
            name: "Bad".to_string(),
            base_url: "https://example.com".to_string(),
            api_key: None,
            has_api_key: false,
            api_format: "weird-proto".to_string(),
            is_full_url: false,
            anthropic_version: None,
            enabled: true,
        };
        assert!(normalize_custom_provider(bad_format, None).is_err());

        let bad_url = QoderCustomProvider {
            id: "x".to_string(),
            name: "Bad".to_string(),
            base_url: "ftp://example.com".to_string(),
            api_key: None,
            has_api_key: false,
            api_format: QODER_API_FORMAT_OPENAI_CHAT.to_string(),
            is_full_url: false,
            anthropic_version: None,
            enabled: true,
        };
        assert!(normalize_custom_provider(bad_url, None).is_err());
    }

    #[test]
    fn model_validation_requires_known_provider_and_valid_effort() {
        let known = vec!["prov1".to_string()];
        let ok = test_model("prov1", "", "model-x");
        let normalized = normalize_custom_model(ok, &known).unwrap();
        assert!(!normalized.id.is_empty());
        assert_eq!(normalized.provider_id, "prov1");

        let orphan = test_model("missing", "m", "m");
        assert!(normalize_custom_model(orphan, &known).is_err());

        let mut bad_effort = test_model("prov1", "m2", "m");
        bad_effort.reasoning_effort = Some("insane".to_string());
        assert!(normalize_custom_model(bad_effort, &known).is_err());
    }

    #[test]
    fn legacy_flat_routes_migrate_to_provider_and_model_with_stable_ids() {
        let db = Database::memory().unwrap();
        // Reproduce a pre-tri-protocol flat route (camelCase, baseUrl on model).
        let legacy = serde_json::json!([{
            "id": "custom_old1",
            "name": "Old Model",
            "model": "gpt-old",
            "baseUrl": "https://legacy.example.com/v1",
            "apiKey": "sk-legacy",
            "reasoningEffort": "medium",
            "isReasoning": true,
            "maxInputTokens": 100000
        }]);
        db.set_setting(QODER_CUSTOM_ROUTES_SETTING, &legacy.to_string())
            .unwrap();
        // Existing carrier mapping that points at the OLD hash-derived route.
        let old_hidden = legacy_provider_id_for_route("custom_old1");
        let old_route = legacy_hash_route_id(&old_hidden, "gpt-old");
        db.set_setting(
            QODER_CARRIER_MAPPINGS_SETTING,
            &serde_json::json!([{"carrierModelId":"native_carrier","routeId":old_route}])
                .to_string(),
        )
        .unwrap();

        ensure_custom_model_migration(&db).unwrap();
        // Idempotent.
        ensure_custom_model_migration(&db).unwrap();

        let providers = load_custom_providers_raw(&db).unwrap();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].base_url, "https://legacy.example.com/v1");
        assert_eq!(providers[0].api_key.as_deref(), Some("sk-legacy"));
        assert_eq!(providers[0].api_format, QODER_API_FORMAT_OPENAI_CHAT);

        let models = listed_qoder_custom_routes(&db).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].provider_id, providers[0].id);
        assert_eq!(models[0].model, "gpt-old");
        let stable = stable_qoder_route_id(&providers[0].id, &models[0].id);

        // Carrier mapping rewritten to the stable id; old hidden provider gone.
        let mappings = list_qoder_carrier_mappings(&db).unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].route_id, stable);
        assert!(db
            .get_provider_by_id(&old_hidden, "qoder")
            .unwrap()
            .is_none());
        // Legacy payload backed up, not deleted.
        assert!(db
            .get_setting(QODER_LEGACY_ROUTES_BACKUP_SETTING)
            .unwrap()
            .is_some());
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
            reasoning_effort: None,
            api_format: QODER_API_FORMAT_RESPONSES.to_string(),
            is_full_url: true,
        };
        let value = serde_json::to_value(entry).expect("serialize manifest entry");
        assert_eq!(value["id"], "qswitch_123");
        assert_eq!(value["route_id"], "qswitch_123");
        assert_eq!(value["provider_id"], "provider-a");
        assert_eq!(value["model"], "upstream-model");
        assert_eq!(value["type"], "openai");
        assert_eq!(value["apiFormat"], QODER_API_FORMAT_RESPONSES);
        assert_eq!(value["isFullUrl"], true);
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
                reasoning_effort: None,
                api_format: QODER_API_FORMAT_OPENAI_CHAT.to_string(),
                is_full_url: false,
            }],
        };
        let expected = QoderRouteTarget {
            provider_id: "provider-a".to_string(),
            upstream_model: "upstream-model".to_string(),
            reasoning_effort: None,
            api_format: QODER_API_FORMAT_OPENAI_CHAT.to_string(),
            is_full_url: false,
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
