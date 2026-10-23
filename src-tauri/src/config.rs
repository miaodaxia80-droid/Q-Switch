use crate::error::AppError;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

static LEGACY_DIRECTORY: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Honor an existing custom directory without retaining the Store plugin.
pub fn load_legacy_directory(app_data: &Path) -> Result<(), AppError> {
    if std::env::var_os("QSWITCH_CONFIG_DIR").is_some() {
        return Ok(());
    }
    let path = app_data.join("app_paths.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(AppError::io(&path, error)),
    };
    let settings: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|e| AppError::json(&path, e))?;
    if let Some(raw) = settings
        .get("app_config_dir_override")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let resolved = if raw == "~" {
            dirs::home_dir().unwrap_or_else(|| PathBuf::from(raw))
        } else if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
            dirs::home_dir()
                .map(|home| home.join(rest))
                .unwrap_or_else(|| PathBuf::from(raw))
        } else {
            PathBuf::from(raw)
        };
        if resolved.is_dir() {
            let _ = LEGACY_DIRECTORY.set(resolved);
        }
    }
    Ok(())
}

pub fn get_app_config_dir() -> PathBuf {
    std::env::var_os("QSWITCH_CONFIG_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| LEGACY_DIRECTORY.get().cloned())
        .unwrap_or_else(|| {
            dirs::home_dir()
                .expect("Home directory unavailable")
                .join(".qswitch")
        })
}

pub fn atomic_write(path: &Path, data: &[u8]) -> Result<(), AppError> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::Config("Invalid file path".into()))?;
    fs::create_dir_all(parent).map_err(|e| AppError::io(parent, e))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| AppError::io(parent, e))?;
    if let Ok(meta) = fs::metadata(path) {
        tmp.as_file()
            .set_permissions(meta.permissions())
            .map_err(|e| AppError::io(path, e))?;
    }
    tmp.write_all(data).map_err(|e| AppError::io(path, e))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| AppError::io(path, e))?;
    tmp.persist(path).map_err(|e| AppError::io(path, e.error))?;
    Ok(())
}
