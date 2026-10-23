//! Native CLI/App provider wiring, following yetone/magpie's
//! internal/agent/qoder.go. Qoder owns tool execution and permissions.
use super::{QoderModelManifest, QoderModelManifestEntry};
use crate::{config::atomic_write, database::Database};
use serde_json::{json, Value};
use std::{fs, path::PathBuf, sync::Mutex};

const PROVIDER: &str = "qswitch";
const LOCAL_KEY: &str = "qswitch-local";
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

fn settings_path(target: &str) -> Result<PathBuf, String> {
    let (env, dir) = match target {
        "qoder" => ("QODER_CONFIG_DIR", ".qoder"),
        "qoder-cn" => ("QODERCN_CONFIG_DIR", ".qoder-cn"),
        _ => return Err("Unknown Qoder target".into()),
    };
    let root = std::env::var_os(env)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(dir)))
        .ok_or("Cannot locate Qoder configuration directory")?;
    Ok(root.join("settings.json"))
}

fn read_settings(path: &std::path::Path) -> Result<Value, String> {
    let value = match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| "Qoder settings.json is invalid JSON; no changes made".to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(error) => return Err(format!("Cannot read Qoder settings: {error}")),
    };
    validate_settings(&value)?;
    Ok(value)
}

fn validate_settings(value: &Value) -> Result<(), String> {
    if !value.is_object()
        || ["providers", "model"]
            .iter()
            .any(|key| value.get(*key).is_some_and(|field| !field.is_object()))
    {
        return Err("Qoder settings, providers and model must be JSON objects".into());
    }
    if value
        .pointer("/model/name")
        .is_some_and(|name| !name.is_string())
    {
        return Err("Qoder model.name must be a string".into());
    }
    if value
        .pointer("/model/preferences")
        .is_some_and(|p| !p.is_object())
    {
        return Err("Qoder model.preferences must be an object".into());
    }
    Ok(())
}

fn owned(provider: &Value) -> bool {
    provider.get("apiKey").and_then(Value::as_str) == Some(LOCAL_KEY)
        && provider.get("protocol").and_then(Value::as_str) == Some("openai")
}

fn provider(models: &[QoderModelManifestEntry], selected: &str, old: Option<&Value>) -> Value {
    // Preserve user routing, timeouts, display name, and any future Qoder fields.
    let mut out = old.cloned().unwrap_or_else(|| json!({}));
    if out.get("displayName").is_none() {
        out["displayName"] = json!("Q Switch");
    }
    out["protocol"] = json!("openai");
    out["baseUrl"] = json!(models[0].base_url);
    out["apiKey"] = json!(LOCAL_KEY);
    out["model"] = json!(selected);
    out["models"] = Value::Array(
        models
            .iter()
            .map(|entry| {
                let mut caps = json!({"tools": true, "vision": false});
                if entry.is_reasoning {
                    caps["thinking"] = json!({
                        "modes": ["enabled"], "supportsEffort": true,
                        "supportedEffortLevels": entry.reasoning_effort.iter().collect::<Vec<_>>(),
                        "requiresBudgetForEnabled": false
                    });
                    if entry.reasoning_effort.is_none() {
                        caps["thinking"] = json!({"modes": ["enabled"], "supportsEffort": false});
                    }
                }
                json!({"model": entry.route_id, "displayName": entry.display_name,
            "contextWindow": entry.max_input_tokens, "capabilities": caps})
            })
            .collect(),
    );
    out
}

fn install(
    settings: &mut Value,
    manifest: &QoderModelManifest,
    selected: &str,
) -> Result<(), String> {
    validate_settings(settings)?;
    let entry = manifest
        .models
        .iter()
        .find(|m| m.route_id == selected)
        .ok_or("Select an enabled Q Switch route before syncing")?;
    let previous = settings.pointer("/providers/qswitch");
    if previous.is_some_and(|p| !owned(p)) {
        return Err(
            "A provider named qswitch already exists and is not managed by Q Switch".into(),
        );
    }
    let next = provider(&manifest.models, selected, previous);
    let root = settings.as_object_mut().ok_or("Invalid settings object")?;
    root.entry("providers").or_insert_with(|| json!({}))[PROVIDER] = next;
    let model = root.entry("model").or_insert_with(|| json!({}));
    let name = format!("{PROVIDER}/{selected}");
    model["name"] = json!(name);
    // Match Qoder 1.1.6x's per-model preferences; retain global reasoningEffort
    // and preferences for every other provider. Don't overwrite user tuning.
    if let Some(effort) = &entry.reasoning_effort {
        let model = model.as_object_mut().ok_or("Invalid model object")?;
        let prefs = model.entry("preferences").or_insert_with(|| json!({}));
        if prefs.get(&name).is_none() {
            prefs[&name] = json!({"reasoning": {"effort": effort}});
        }
    }
    Ok(())
}

fn uninstall(settings: &mut Value, backup: &Value) -> Result<(), String> {
    validate_settings(settings)?;
    if settings
        .pointer("/providers/qswitch")
        .is_some_and(|p| !owned(p))
    {
        return Err("The qswitch provider was replaced externally; no changes made".into());
    }
    if let Some(providers) = settings.get_mut("providers").and_then(Value::as_object_mut) {
        providers.remove(PROVIDER);
    }
    if let Some(model) = settings.get_mut("model").and_then(Value::as_object_mut) {
        if model
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| n.starts_with("qswitch/"))
        {
            if let Some(name) = backup.get("previousModelName").and_then(Value::as_str) {
                model.insert("name".into(), json!(name));
            } else {
                model.remove("name");
            }
        }
        // Preferences are user tuning; retain them for a later re-install.
    }
    Ok(())
}

fn write_json(path: &std::path::Path, value: &Value) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(value).map_err(|error| error.to_string())?;
    atomic_write(path, &bytes).map_err(|error| error.to_string())
}

pub async fn sync_native_provider(
    db: &Database,
    target: &str,
    selected: &str,
) -> Result<String, String> {
    let manifest_path = super::sync_qoder_model_manifest(db)
        .await
        .map_err(|e| e.to_string())?;
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| "Qoder settings lock poisoned")?;
    let manifest: QoderModelManifest =
        serde_json::from_slice(&fs::read(manifest_path).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let path = settings_path(target)?;
    sync_at(&path, &manifest, selected)?;
    Ok(path.to_string_lossy().into_owned())
}

fn sync_at(
    path: &std::path::Path,
    manifest: &QoderModelManifest,
    selected: &str,
) -> Result<(), String> {
    let mut settings = read_settings(path)?;
    let backup_path = path.with_file_name("settings.qswitch-native-backup.json");
    if backup_path.exists() {
        read_backup(&backup_path)?;
    } else if settings.pointer("/providers/qswitch").is_some_and(owned) {
        return Err("Q Switch selection backup is missing; no changes made".into());
    }
    let old_name = settings
        .pointer("/model/name")
        .cloned()
        .unwrap_or(Value::Null);
    install(&mut settings, manifest, selected)?;
    if !backup_path.exists() {
        // Selection only: no provider keys or other sensitive settings in backup.
        write_json(
            &backup_path,
            &json!({"version": 1, "previousModelName": old_name}),
        )?;
    }
    write_json(path, &settings)?;
    Ok(())
}

fn read_backup(path: &std::path::Path) -> Result<Value, String> {
    let backup: Value = serde_json::from_slice(&fs::read(path).map_err(|e| e.to_string())?)
        .map_err(|_| "Invalid Q Switch selection backup; no changes made")?;
    let name_valid = backup
        .get("previousModelName")
        .is_some_and(|name| name.is_string() || name.is_null());
    if backup.get("version") != Some(&json!(1)) || !name_valid {
        return Err("Invalid Q Switch selection backup; no changes made".into());
    }
    Ok(backup)
}

pub fn remove_native_provider(target: &str) -> Result<String, String> {
    let _guard = CONFIG_LOCK
        .lock()
        .map_err(|_| "Qoder settings lock poisoned")?;
    let path = settings_path(target)?;
    remove_at(&path)?;
    Ok(path.to_string_lossy().into_owned())
}

fn remove_at(path: &std::path::Path) -> Result<(), String> {
    let backup_path = path.with_file_name("settings.qswitch-native-backup.json");
    // No saved installation: a repeat remove is a harmless no-op.
    if !backup_path.exists() {
        if read_settings(path)?
            .pointer("/providers/qswitch")
            .is_some_and(owned)
        {
            return Err("Q Switch selection backup is missing; no changes made".into());
        }
        return Ok(());
    }
    let backup = read_backup(&backup_path)?;
    let mut settings = read_settings(path)?;
    uninstall(&mut settings, &backup)?;
    write_json(path, &settings)?;
    fs::remove_file(&backup_path).map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> QoderModelManifest {
        serde_json::from_value(json!({"version": 3, "models": [{
            "id": "qswitch_a_b", "route_id": "qswitch_a_b", "provider_id": "p",
            "model": "upstream-name", "displayName": "Provider / Model",
            "base_url": "http://127.0.0.1:15731/qoder/v1", "type": "openai",
            "transport_status": "native-adapter-required", "max_input_tokens": 200000,
            "is_reasoning": true, "reasoning_effort": "high"
        }]}))
        .unwrap()
    }

    #[test]
    fn native_schema_uses_route_ids_and_preserves_user_settings() {
        let mut settings = json!({"theme": "dark", "providers": {"other": {"apiKey": "secret"}},
        "model": {"name": "other/model", "reasoningEffort": "low", "preferences": {
            "other/model": {"reasoning": {"effort": "low"}}
        }}});
        install(&mut settings, &catalog(), "qswitch_a_b").unwrap();
        assert_eq!(settings["model"]["name"], "qswitch/qswitch_a_b");
        let p = &settings["providers"][PROVIDER];
        assert_eq!(p["baseUrl"], "http://127.0.0.1:15731/qoder/v1");
        assert_eq!(p["apiKey"], LOCAL_KEY);
        assert_eq!(p["models"][0]["model"], "qswitch_a_b");
        assert_eq!(p["models"][0]["capabilities"]["tools"], true);
        assert_eq!(settings["providers"]["other"]["apiKey"], "secret");
        assert_eq!(settings["model"]["reasoningEffort"], "low");
        settings["providers"][PROVIDER]["routing"] = json!({"compact": "small"});
        settings["providers"][PROVIDER]["displayName"] = json!("My gateway");
        install(&mut settings, &catalog(), "qswitch_a_b").unwrap();
        assert_eq!(
            settings["providers"][PROVIDER]["routing"]["compact"],
            "small"
        );
        assert_eq!(settings["providers"][PROVIDER]["displayName"], "My gateway");
        uninstall(&mut settings, &json!({"previousModelName": "other/model"})).unwrap();
        assert_eq!(settings["model"]["name"], "other/model");
        assert!(settings["providers"].get(PROVIDER).is_none());
        assert_eq!(settings["theme"], "dark");
    }

    #[test]
    fn refuses_conflicts_invalid_shapes_and_unknown_routes() {
        for mut value in [
            json!([]),
            json!({"model": "auto"}),
            json!({"providers": []}),
            json!({"model": {"preferences": []}}),
            json!({"providers": {"qswitch": {"apiKey": "someone-elses-key"}}}),
        ] {
            let before = value.clone();
            assert!(install(&mut value, &catalog(), "qswitch_a_b").is_err());
            assert_eq!(value, before);
        }
        assert!(install(&mut json!({}), &catalog(), "missing").is_err());
    }

    #[test]
    fn removal_keeps_a_model_chosen_after_installation() {
        let mut value = json!({});
        install(&mut value, &catalog(), "qswitch_a_b").unwrap();
        value["model"]["name"] = json!("user/new-choice");
        uninstall(&mut value, &json!({"previousModelName": "old/model"})).unwrap();
        assert_eq!(value["model"]["name"], "user/new-choice");
    }

    #[test]
    fn target_is_validated_before_any_file_access() {
        assert!(settings_path("../../other").is_err());
    }

    #[test]
    fn file_sync_resync_and_removal_restore_original_selection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_json(
            &path,
            &json!({"model": {"name": "original"}, "unrelated": true}),
        )
        .unwrap();
        sync_at(&path, &catalog(), "qswitch_a_b").unwrap();
        let backup = path.with_file_name("settings.qswitch-native-backup.json");
        assert_eq!(
            read_settings(&path).unwrap()["model"]["name"],
            "qswitch/qswitch_a_b"
        );
        sync_at(&path, &catalog(), "qswitch_a_b").unwrap();
        let saved: Value = serde_json::from_slice(&fs::read(&backup).unwrap()).unwrap();
        assert_eq!(saved["previousModelName"], "original");
        remove_at(&path).unwrap();
        assert_eq!(read_settings(&path).unwrap()["model"]["name"], "original");
        assert_eq!(read_settings(&path).unwrap()["unrelated"], true);
        assert!(!backup.exists());
        remove_at(&path).unwrap();
    }

    #[test]
    fn invalid_file_is_never_overwritten_and_external_provider_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        fs::write(&path, b"{invalid").unwrap();
        assert!(sync_at(&path, &catalog(), "qswitch_a_b").is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{invalid");
        fs::remove_file(&path).unwrap();
        sync_at(&path, &catalog(), "qswitch_a_b").unwrap();
        let mut settings = read_settings(&path).unwrap();
        settings["providers"][PROVIDER]["apiKey"] = json!("external");
        write_json(&path, &settings).unwrap();
        assert!(remove_at(&path).is_err());
        assert_eq!(read_settings(&path).unwrap(), settings);
    }

    #[test]
    fn missing_or_corrupt_selection_backup_blocks_further_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        sync_at(&path, &catalog(), "qswitch_a_b").unwrap();
        let before = fs::read(&path).unwrap();
        let backup = path.with_file_name("settings.qswitch-native-backup.json");
        fs::remove_file(&backup).unwrap();
        assert!(sync_at(&path, &catalog(), "qswitch_a_b").is_err());
        assert!(remove_at(&path).is_err());
        fs::write(&backup, b"{}").unwrap();
        assert!(sync_at(&path, &catalog(), "qswitch_a_b").is_err());
        assert!(remove_at(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
    }
}
