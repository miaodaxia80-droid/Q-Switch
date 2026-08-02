//! Writes and cleans up Qoder's `.info.json` discovery file.
//!
//! Qoder's Electron main process reads this file to find the local
//! WebSocket port of the ACP agent.

use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Command, Stdio};

const ADAPTER_MARKER: &str = "qswitchAdapter";

/// Native Qoder Agent discovery record captured before Q Switch takes over
/// the Electron-side WebSocket connection.
///
/// The original JSON is retained byte-for-byte so stopping the adapter can
/// restore Qoder's own Agent instead of leaving a stale loopback endpoint.
#[derive(Debug, Clone)]
pub struct NativeInfoSnapshot {
    pub websocket_port: u16,
    pub pid: u32,
    pub ipc_server_path: PathBuf,
    original_bytes: Vec<u8>,
    original_value: Value,
}

pub(crate) fn capture_native_info_from(path: &Path) -> io::Result<NativeInfoSnapshot> {
    let original_bytes = fs::read(path)?;
    let original_value: Value = serde_json::from_slice(&original_bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Qoder .info.json: {error}"),
        )
    })?;

    if original_value.get(ADAPTER_MARKER).is_some() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Q Switch native adapter is already active",
        ));
    }

    let websocket_port = original_value
        .get("websocketPort")
        .and_then(Value::as_u64)
        .filter(|port| (1..=u16::MAX as u64).contains(port))
        .map(|port| port as u16)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Qoder .info.json has no valid websocketPort",
            )
        })?;
    let pid = original_value
        .get("pid")
        .and_then(Value::as_u64)
        .filter(|pid| (1..=u32::MAX as u64).contains(pid))
        .map(|pid| pid as u32)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Qoder .info.json has no valid pid",
            )
        })?;
    let ipc_server_path = original_value
        .get("ipcServerPath")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Qoder .info.json has no valid ipcServerPath",
            )
        })?;

    Ok(NativeInfoSnapshot {
        websocket_port,
        pid,
        ipc_server_path,
        original_bytes,
        original_value,
    })
}

/// Return the owning Q Switch process for an adapter record, if present.
/// This makes periodic monitoring safe: another Q Switch instance is never
/// overwritten merely because it uses the same discovery file.
pub(crate) fn adapter_owner_pid_from(path: &Path) -> io::Result<Option<u32>> {
    let value: Value = serde_json::from_slice(&fs::read(path)?).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid Qoder .info.json: {error}"),
        )
    })?;
    Ok(value
        .get(ADAPTER_MARKER)
        .and_then(|marker| marker.get("ownerPid"))
        .and_then(Value::as_u64)
        .filter(|pid| (1..=u32::MAX as u64).contains(pid))
        .map(|pid| pid as u32))
}

/// Recover Qoder's native discovery record if a previous Q Switch process
/// died without running its normal shutdown hook.
///
/// The adapter marker retains only the native connection coordinates.  We
/// restore them only after confirming that the recorded owner PID no longer
/// exists, so a second Q Switch instance never overwrites a live adapter.
pub(crate) fn reclaim_stale_adapter_info(path: &Path) -> io::Result<bool> {
    let current_bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut current: Value = serde_json::from_slice(&current_bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid active Qoder .info.json: {error}"),
        )
    })?;
    let Some(marker) = current.get(ADAPTER_MARKER) else {
        return Ok(false);
    };
    let owner_pid = marker
        .get("ownerPid")
        .and_then(Value::as_u64)
        .filter(|pid| (1..=u32::MAX as u64).contains(pid))
        .map(|pid| pid as u32)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Q Switch adapter record has no valid owner PID",
            )
        })?;
    if process_is_alive(owner_pid) {
        return Ok(false);
    }
    let native_port = marker
        .get("nativeWebsocketPort")
        .and_then(Value::as_u64)
        .filter(|port| (1..=u16::MAX as u64).contains(port))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Q Switch adapter record has no valid native WebSocket port",
            )
        })?;
    let native_pid = marker
        .get("nativePid")
        .and_then(Value::as_u64)
        .filter(|pid| (1..=u32::MAX as u64).contains(pid))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Q Switch adapter record has no valid native PID",
            )
        })?;
    let native_ipc_path = marker
        .get("nativeIpcServerPath")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Q Switch adapter record has no native IPC path",
            )
        })?;
    let object = current.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Qoder .info.json must be a JSON object",
        )
    })?;
    object.insert("websocketPort".to_string(), json!(native_port));
    object.insert("pid".to_string(), json!(native_pid));
    object.insert("ipcServerPath".to_string(), json!(native_ipc_path));
    object.remove(ADAPTER_MARKER);
    let bytes = serde_json::to_vec_pretty(&current).map_err(io::Error::other)?;
    atomic_write(path, &bytes)?;
    Ok(true)
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    Command::new("/bin/kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // Qoder's current local ACP adapter is macOS-only. Refuse to infer a
    // process state on other platforms rather than risk overwriting a live
    // adapter record.
    true
}

pub(crate) fn write_adapter_info_to(
    path: &Path,
    snapshot: &NativeInfoSnapshot,
    adapter_port: u16,
    adapter_ipc_server_path: &Path,
    adapter_pid: u32,
) -> io::Result<()> {
    let mut info = snapshot.original_value.clone();
    let object = info.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Qoder .info.json must be a JSON object",
        )
    })?;
    object.insert("websocketPort".to_string(), json!(adapter_port));
    object.insert(
        "ipcServerPath".to_string(),
        json!(adapter_ipc_server_path.to_string_lossy()),
    );
    object.insert("pid".to_string(), json!(adapter_pid));
    object.insert(
        ADAPTER_MARKER.to_string(),
        json!({
            "version": 1,
            "ownerPid": adapter_pid,
            "nativeWebsocketPort": snapshot.websocket_port,
            "nativePid": snapshot.pid,
            "nativeIpcServerPath": snapshot.ipc_server_path,
        }),
    );
    let bytes = serde_json::to_vec_pretty(&info).map_err(io::Error::other)?;
    atomic_write(path, &bytes)
}

pub(crate) fn restore_native_info_to(
    path: &Path,
    snapshot: &NativeInfoSnapshot,
    adapter_pid: u32,
) -> io::Result<bool> {
    let current_bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let current: Value = serde_json::from_slice(&current_bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid active Qoder .info.json: {error}"),
        )
    })?;
    let owner_pid = current
        .get(ADAPTER_MARKER)
        .and_then(|value| value.get("ownerPid"))
        .and_then(Value::as_u64);
    if owner_pid != Some(adapter_pid as u64) {
        return Ok(false);
    }
    atomic_write(path, &snapshot.original_bytes)?;
    Ok(true)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Qoder .info.json has no parent",
        )
    })?;
    fs::create_dir_all(parent)?;
    // A unique temporary name keeps concurrent writers (e.g. two Q Switch
    // instances, or parallel tests) from truncating each other's in-flight
    // temp file; the final rename is atomic and the last writer wins.
    let temporary = parent.join(format!(
        ".{}.qswitch-{}-{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("info.json"),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn adapter_info_preserves_native_agent_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        fs::write(
            &info_path,
            br#"{"websocketPort":12345,"pid":99999,"ipcServerPath":"/tmp/native.sock","isDev":false}"#,
        )
        .unwrap();
        let snapshot = capture_native_info_from(&info_path).unwrap();
        assert_eq!(snapshot.websocket_port, 12345);
        assert_eq!(snapshot.pid, 99999);
    }

    #[test]
    fn adapter_record_round_trips_exact_native_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        let original = br#"{"websocketPort":32123,"pid":9876,"ipcServerPath":"/tmp/native.sock","isDev":false}"#;
        fs::write(&info_path, original).unwrap();

        let snapshot = capture_native_info_from(&info_path).unwrap();
        write_adapter_info_to(
            &info_path,
            &snapshot,
            45678,
            Path::new("/tmp/adapter.sock"),
            1234,
        )
        .unwrap();
        let adapter: Value = serde_json::from_slice(&fs::read(&info_path).unwrap()).unwrap();
        assert_eq!(adapter["websocketPort"], 45678);
        assert_eq!(adapter["ipcServerPath"], "/tmp/adapter.sock");
        assert_eq!(adapter["pid"], 1234);
        assert_eq!(adapter[ADAPTER_MARKER]["nativeWebsocketPort"], 32123);

        assert!(restore_native_info_to(&info_path, &snapshot, 1234).unwrap());
        assert_eq!(fs::read(&info_path).unwrap(), original);
    }

    #[test]
    fn stale_adapter_cleanup_never_clobbers_new_native_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        fs::write(
            &info_path,
            br#"{"websocketPort":32123,"pid":9876,"ipcServerPath":"/tmp/native.sock"}"#,
        )
        .unwrap();
        let snapshot = capture_native_info_from(&info_path).unwrap();
        write_adapter_info_to(
            &info_path,
            &snapshot,
            45678,
            Path::new("/tmp/adapter.sock"),
            1234,
        )
        .unwrap();

        let new_native =
            br#"{"websocketPort":11111,"pid":4321,"ipcServerPath":"/tmp/new-native.sock"}"#;
        fs::write(&info_path, new_native).unwrap();
        assert!(!restore_native_info_to(&info_path, &snapshot, 1234).unwrap());
        assert_eq!(fs::read(&info_path).unwrap(), new_native);
    }

    #[test]
    fn reports_adapter_owner_without_treating_native_record_as_owned() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        fs::write(
            &info_path,
            br#"{"websocketPort":32123,"pid":9876,"ipcServerPath":"/tmp/native.sock"}"#,
        )
        .unwrap();
        assert_eq!(adapter_owner_pid_from(&info_path).unwrap(), None);

        let snapshot = capture_native_info_from(&info_path).unwrap();
        write_adapter_info_to(
            &info_path,
            &snapshot,
            45678,
            Path::new("/tmp/adapter.sock"),
            1234,
        )
        .unwrap();
        assert_eq!(adapter_owner_pid_from(&info_path).unwrap(), Some(1234));
    }

    #[test]
    fn reclaims_only_a_dead_adapter_owner() {
        let tmp = tempfile::tempdir().unwrap();
        let info_path = tmp.path().join(".info.json");
        fs::write(
            &info_path,
            br#"{"websocketPort":45678,"pid":999,"ipcServerPath":"/tmp/adapter.sock","isDev":false,"qswitchAdapter":{"version":1,"ownerPid":4294967295,"nativeWebsocketPort":32123,"nativePid":9876,"nativeIpcServerPath":"/tmp/native.sock"}}"#,
        )
        .unwrap();
        assert!(reclaim_stale_adapter_info(&info_path).unwrap());
        let restored: Value = serde_json::from_slice(&fs::read(&info_path).unwrap()).unwrap();
        assert_eq!(restored["websocketPort"], 32123);
        assert_eq!(restored["pid"], 9876);
        assert_eq!(restored["ipcServerPath"], "/tmp/native.sock");
        assert!(restored.get(ADAPTER_MARKER).is_none());

        write_adapter_info_to(
            &info_path,
            &capture_native_info_from(&info_path).unwrap(),
            45678,
            Path::new("/tmp/adapter.sock"),
            std::process::id(),
        )
        .unwrap();
        assert!(!reclaim_stale_adapter_info(&info_path).unwrap());
    }
}
