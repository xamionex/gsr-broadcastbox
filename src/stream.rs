//! gpu-screen-recorder command construction, process control and old script cleanup.

use std::fs::File;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::process;

/// Read the tail of a log file for error reports.
pub fn tail_log(path: &std::path::Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Open the log with owner only permissions and append mode.
pub fn open_log(path: &std::path::Path) -> Result<File, String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("failed to open {}: {e}", path.display()))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(file)
}

/// Arguments passed to gpu-screen-recorder.
pub fn build_gsr_args(config: &Config, monitor: &str) -> Vec<String> {
    let mut args = vec![
        "-w".to_string(),
        monitor.to_string(),
        "-c".to_string(),
        "whip".to_string(),
        "-k".to_string(),
        config.codec.clone(),
        "-ac".to_string(),
        config.audio_codec.clone(),
        "-ab".to_string(),
        config.audio_bitrate.to_string(),
        "-f".to_string(),
        config.fps.to_string(),
        "-s".to_string(),
        config.resolution.clone(),
        "-bm".to_string(),
        config.bitrate_mode.clone(),
        "-q".to_string(),
        config.quality_argument(),
        "-cr".to_string(),
        config.color_range.clone(),
    ];
    if let Some(audio) = config.audio_argument() {
        args.push("-a".to_string());
        args.push(audio);
    }
    args.push("-o".to_string());
    args.push(config.whip_url.clone());
    args.push("-v".to_string());
    args.push("no".to_string());

    for argument in &config.extra_gsr_args {
        let argument = argument.trim();
        if !argument.is_empty() {
            args.push(argument.to_string());
        }
    }
    args
}

/// Make sure gpu-screen-recorder is installed and runnable.
pub fn ensure_gsr_available() -> Result<(), String> {
    match Command::new("gpu-screen-recorder")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!(
            "gpu-screen-recorder --version failed with {status}"
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(
            "gpu-screen-recorder is not installed. Install it from your distribution or from https://git.dec05eba.com/gpu-screen-recorder"
                .to_string(),
        ),
        Err(e) => Err(format!("could not run gpu-screen-recorder: {e}")),
    }
}

/// Spawn gpu-screen-recorder with the stream key in the environment, never in the process arguments.
pub fn spawn_gsr(
    config: &Config,
    monitor: &str,
    stream_key: &str,
    log_path: &std::path::Path,
) -> Result<Child, String> {
    use std::os::unix::process::CommandExt;

    let log = open_log(log_path)?;
    let stderr = log
        .try_clone()
        .map_err(|e| format!("failed to duplicate the log handle: {e}"))?;

    Command::new("gpu-screen-recorder")
        .args(build_gsr_args(config, monitor))
        .env("GSR_AUTH", stream_key)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr)
        .process_group(0)
        .spawn()
        .map_err(|e| format!("failed to start gpu-screen-recorder: {e}"))
}

/// Wait for an owned child, polling so a signal can interrupt the wait.
pub fn wait_child(child: &mut Child, timeout: Duration) -> Result<Option<ExitStatus>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status)),
            Ok(None) => {}
            Err(e) => return Err(format!("failed to wait for gpu-screen-recorder: {e}")),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Stop an owned child, escalating from SIGINT to SIGTERM to SIGKILL.
pub fn stop_child(child: &mut Child, pid: u32, grace: Duration) -> Result<ExitStatus, String> {
    if let Some(status) = child
        .try_wait()
        .map_err(|e| format!("failed to check gpu-screen-recorder: {e}"))?
    {
        return Ok(status);
    }

    process::signal(pid, libc::SIGINT);
    if let Some(status) = wait_child(child, grace)? {
        return Ok(status);
    }

    process::signal(pid, libc::SIGTERM);
    if let Some(status) = wait_child(child, Duration::from_secs(2))? {
        return Ok(status);
    }

    process::signal(pid, libc::SIGKILL);
    wait_child(child, Duration::from_secs(1))?
        .ok_or_else(|| "gpu-screen-recorder did not exit after SIGKILL".to_string())
}

// --- Migration cleanup for the old stream.sh helpers ---

const LEGACY_PID_FILE: &str = "/tmp/gsr-whip-stream.pid";
const LEGACY_KEY_FILE: &str = "/tmp/gsr-whip-stream.key";
const LEGACY_TRAY_PID_FILE: &str = "/tmp/gsr-whip-stream-tray.pid";
const LEGACY_TS_PID_FILE: &str = "/tmp/gsr-whip-stream-ts.pid";

fn read_pid(path: &str) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

/// PID of a stream started by the old `stream.sh`, when it is still running.
pub fn legacy_stream_pid() -> Option<u32> {
    let pid = read_pid(LEGACY_PID_FILE)?;
    if process::is_gsr_whip(pid) {
        Some(pid)
    } else {
        None
    }
}

/// Stop the old shell stream, tray helper and TeamSpeak watcher.
///
/// Returns descriptions of everything that was stopped or removed.
pub fn stop_legacy() -> Vec<String> {
    let mut stopped = Vec::new();

    if let Some(pid) = legacy_stream_pid() {
        process::terminate(pid, Duration::from_secs(5));
        stopped.push(format!("old stream.sh stream (PID: {pid})"));
    }

    if let Some(pid) = read_pid(LEGACY_TRAY_PID_FILE) {
        if process::pid_alive(pid) && process::cmdline_contains(pid, "stream-tray.py") {
            process::terminate(pid, Duration::from_secs(2));
            stopped.push(format!("old tray indicator (PID: {pid})"));
        }
    }

    if let Some(pid) = read_pid(LEGACY_TS_PID_FILE) {
        if process::pid_alive(pid) && process::cmdline_contains(pid, "gsr-ts-watch") {
            process::terminate(pid, Duration::from_secs(2));
            stopped.push(format!("old TeamSpeak watcher (PID: {pid})"));
        }
    }

    for path in [
        LEGACY_PID_FILE,
        LEGACY_KEY_FILE,
        LEGACY_TRAY_PID_FILE,
        LEGACY_TS_PID_FILE,
    ] {
        if std::fs::remove_file(path).is_ok() {
            stopped.push(format!("old state file {path}"));
        }
    }

    stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_args_match_the_broadcast_setup() {
        let config = Config::default();
        let args = build_gsr_args(&config, "DP-1");
        let joined = args.join(" ");

        assert!(joined.contains("-w DP-1"));
        assert!(joined.contains("-c whip"));
        assert!(joined.contains("-k h264"));
        assert!(joined.contains("-ac opus"));
        assert!(joined.contains("-ab 128"));
        assert!(joined.contains("-f 60"));
        assert!(joined.contains("-s 1920x1080"));
        assert!(joined.contains("-bm qp"));
        assert!(joined.contains("-q high"));
        assert!(joined.contains("-cr limited"));
        assert!(joined.contains("-a app-inverse:TeamSpeak|app-inverse:discord"));
        assert!(joined.contains("-o https://broadcast.petar.cc/api/whip"));
        assert!(joined.contains("-v no"));
    }

    #[test]
    fn cbr_passes_the_video_bitrate() {
        let config = Config {
            bitrate_mode: "cbr".to_string(),
            video_bitrate: 6000,
            ..Config::default()
        };
        let args = build_gsr_args(&config, "DP-1");
        let q = args
            .iter()
            .position(|argument| argument == "-q")
            .map(|index| args[index + 1].as_str());
        assert_eq!(q, Some("6000"));
    }

    #[test]
    fn extra_args_are_appended() {
        let config = Config {
            extra_gsr_args: vec!["-keyint".to_string(), "1".to_string()],
            ..Config::default()
        };
        let args = build_gsr_args(&config, "DP-1");
        assert_eq!(
            &args[args.len() - 2..],
            &["-keyint".to_string(), "1".to_string()]
        );
    }

    #[test]
    fn audio_is_omitted_when_no_sources_are_configured() {
        let mut config = Config::default();
        config.audio_sources.clear();
        let args = build_gsr_args(&config, "DP-1");
        assert!(!args.iter().any(|argument| argument == "-a"));
    }
}
