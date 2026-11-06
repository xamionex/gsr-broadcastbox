//! Runtime state shared by the CLI and the daemon.

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::config::Config;

/// A stream that is already running.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamState {
    pub daemon_pid: u32,
    pub stream_pid: u32,
    pub stream_key: String,
    pub watch_url: String,
    pub monitor: String,
    pub quality: String,
    pub teamspeak: bool,
    pub started_at: u64,
    pub log_path: PathBuf,
}

/// Options handed from `start` to the detached daemon.
///
/// This file holds the stream key, so it is written with mode 0600 and removed by the daemon as soon as it is read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingLaunch {
    pub config: Config,
    pub stream_key: String,
    pub monitor: String,
    pub log_path: PathBuf,
}

/// Directory for the state file, preferring the per user runtime directory.
pub fn runtime_dir() -> Result<PathBuf, String> {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let uid = unsafe { libc::getuid() };
            std::env::temp_dir().join(format!("gsr-broadcastbox-{uid}"))
        }
    };
    let dir = if base.ends_with("gsr-broadcastbox") {
        base
    } else {
        base.join("gsr-broadcastbox")
    };
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Path of the running stream state.
pub fn state_path() -> Result<PathBuf, String> {
    Ok(runtime_dir()?.join("stream.json"))
}

/// Path of the pending launch file.
pub fn pending_path() -> Result<PathBuf, String> {
    Ok(runtime_dir()?.join("pending.json"))
}

/// Path of the daemon and gpu-screen-recorder log.
pub fn log_path() -> Result<PathBuf, String> {
    let base = dirs::state_dir()
        .or_else(dirs::cache_dir)
        .ok_or_else(|| "could not resolve a state directory".to_string())?;
    let dir = base.join("gsr-broadcastbox");
    ensure_private_dir(&dir)?;
    Ok(dir.join("stream.log"))
}

/// Write the running stream state atomically.
pub fn write_state(state: &StreamState) -> Result<(), String> {
    write_json_private(&state_path()?, state)
}

/// Read the running stream state, when there is one.
pub fn read_state() -> Result<Option<StreamState>, String> {
    read_json(&state_path()?)
}

/// Remove the state file.
pub fn remove_state() -> Result<(), String> {
    remove_file(&state_path()?)
}

/// Remove the state file only when it still belongs to this daemon.
pub fn remove_state_if_daemon(daemon_pid: u32) -> Result<(), String> {
    match read_state() {
        Ok(Some(state)) if state.daemon_pid == daemon_pid => remove_state(),
        Ok(_) => Ok(()),
        Err(error) => {
            eprintln!("gsr-broadcastbox: ignoring unreadable state file: {error}");
            remove_state()
        }
    }
}

/// Write the pending launch options with owner only permissions.
pub fn write_pending(pending: &PendingLaunch) -> Result<PathBuf, String> {
    let path = pending_path()?;
    write_json_private(&path, pending)?;
    Ok(path)
}

/// Read and delete the pending launch options.
pub fn take_pending(path: &Path) -> Result<Option<PendingLaunch>, String> {
    let pending = read_json(path)?;
    remove_file(path)?;
    Ok(pending)
}

/// Remove a pending launch file.
pub fn remove_pending(path: &Path) -> Result<(), String> {
    remove_file(path)
}

/// Create a directory only readable by the user.
fn ensure_private_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::create_dir_all(path)
        .map_err(|e| format!("failed to create {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("failed to secure {}: {e}", path.display()))?;
    Ok(())
}

/// Write JSON through a temporary file and rename it into place.
fn write_json_private<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let json = serde_json::to_vec_pretty(value)
        .map_err(|e| format!("failed to serialize {}: {e}", path.display()))?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;

    let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
    write_private(&temporary, &json)?;
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("failed to write {}: {e}", path.display())
    })
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("failed to create {}: {e}", path.display()))?;
    file.write_all(bytes)
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    file.sync_all()
        .map_err(|e| format!("failed to sync {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("failed to secure {}: {e}", path.display()))?;
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("failed to parse {}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("failed to read {}: {e}", path.display())),
    }
}

fn remove_file(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("failed to remove {}: {e}", path.display())),
    }
}

/// Unix time in seconds.
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Human readable age of a stream.
pub fn format_age(started_at: u64) -> String {
    let elapsed = now_secs().saturating_sub(started_at);
    if elapsed < 60 {
        format!("{elapsed}s")
    } else if elapsed < 3600 {
        format!("{}m {}s", elapsed / 60, elapsed % 60)
    } else {
        format!("{}h {}m", elapsed / 3600, (elapsed % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gsr_broadcastbox_state_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn state_round_trips_with_private_permissions() {
        let dir = scratch("roundtrip");
        let path = dir.join("stream.json");
        let state = StreamState {
            daemon_pid: 10,
            stream_pid: 11,
            stream_key: "key".to_string(),
            watch_url: "https://example/key".to_string(),
            monitor: "DP-1".to_string(),
            quality: "high".to_string(),
            teamspeak: true,
            started_at: 123,
            log_path: dir.join("stream.log"),
        };
        write_json_private(&path, &state).unwrap();
        let read: Option<StreamState> = read_json(&path).unwrap();
        assert_eq!(read.unwrap().stream_pid, 11);

        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_files_read_as_none() {
        let dir = scratch("missing");
        let read: Option<StreamState> = read_json(&dir.join("absent.json")).unwrap();
        assert!(read.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn age_formatting_is_readable() {
        let now = now_secs();
        assert!(format_age(now).ends_with('s'));
        assert!(format_age(now - 65).contains('m'));
        assert!(format_age(now - 3665).contains('h'));
    }
}
