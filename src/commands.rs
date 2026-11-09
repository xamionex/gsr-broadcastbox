//! CLI operations: start, stop, restart, status and monitor listing.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::state::{self, PendingLaunch};
use crate::{config_file, daemon, monitors, notify, process, stream};

/// Command line overrides for `start` and `restart`.
#[derive(Debug, Default, Clone)]
pub struct StartOptions {
    pub stream_key: Option<String>,
    pub quality: Option<String>,
    pub monitor: Option<String>,
    pub teamspeak: Option<bool>,
    pub tray: Option<bool>,
    pub foreground: bool,
}

/// Start a stream, detached by default.
pub fn start(options: StartOptions) -> Result<(), String> {
    let mut config = load_config();
    apply_options(&mut config, &options);

    let stream_key = config::resolve_stream_key(&config.stream_key);
    let mut errors = config.validate();
    if let Err(error) = validate_stream_key(&stream_key) {
        errors.push(error);
    }
    if !errors.is_empty() {
        let message = format!("configuration errors:\n  - {}", errors.join("\n  - "));
        notify::send(config.notifications, "Stream not started", &errors[0]);
        return Err(message);
    }

    if existing_stream()? {
        return Ok(());
    }
    if let Some(pid) = stream::legacy_stream_pid() {
        return Err(format!(
            "the old stream.sh stream is still running (PID: {pid}). Run `gsr-broadcastbox stop` first."
        ));
    }

    if let Err(error) = stream::ensure_gsr_available() {
        notify::send(config.notifications, "Stream not started", &error);
        return Err(error);
    }

    if config.teamspeak && !process::process_running(&[config.teamspeak_process.as_str()]) {
        let message = format!("{} is not running", config.teamspeak_process);
        notify::send(config.notifications, "Stream not started", &message);
        return Err(message);
    }

    let detected = monitors::detect_monitors();
    let active = monitors::active_monitor();
    let resolved_monitor =
        match monitors::choose_monitor(&config.monitor, &detected, active.as_deref()) {
            Ok(monitor) => monitor,
            Err(error) => {
                notify::send(config.notifications, "Stream not started", &error);
                return Err(error);
            }
        };

    let log_path = state::log_path()?;
    let _ = std::fs::remove_file(&log_path);
    let log_handle = stream::open_log(&log_path)?;

    let pending = PendingLaunch {
        config: config.clone(),
        stream_key: stream_key.clone(),
        monitor: resolved_monitor.clone(),
        log_path: log_path.clone(),
    };
    let pending_path = state::write_pending(&pending)?;

    if options.foreground {
        return match daemon::run(pending_path) {
            Ok(()) => {
                println!("Stream stopped");
                Ok(())
            }
            Err(error) => {
                notify::send(config.notifications, "Stream failed", &error);
                Err(error)
            }
        };
    }

    drop(log_handle);
    spawn_daemon(&pending_path, &log_path, &config)
}

fn spawn_daemon(
    pending_path: &std::path::Path,
    log_path: &std::path::Path,
    config: &Config,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;

    let executable = std::env::current_exe()
        .map_err(|e| format!("could not resolve the running executable: {e}"))?;
    let log_handle = stream::open_log(log_path)?;
    let stderr = log_handle
        .try_clone()
        .map_err(|e| format!("could not duplicate the log handle: {e}"))?;

    let mut command = Command::new(executable);
    command
        .arg("daemon")
        .arg("--pending")
        .arg(pending_path)
        .stdin(Stdio::null())
        .stdout(log_handle)
        .stderr(stderr);
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = state::remove_pending(pending_path);
            return Err(format!("failed to start the daemon: {error}"));
        }
    };
    let daemon_pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(8);

    loop {
        match state::read_state() {
            Ok(Some(state)) if state.daemon_pid == daemon_pid && process::pid_alive(daemon_pid) => {
                println!("Stream started (PID: {}).", state.stream_pid);
                println!("Watch: {}", state.watch_url);
                println!("Monitor: {} | Quality: {}", state.monitor, state.quality);
                println!("Log: {}", state.log_path.display());
                if config.teamspeak {
                    println!("TeamSpeak mode: on");
                }
                if config.tray {
                    println!("Tray indicator: on");
                }
                return Ok(());
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("gsr-broadcastbox: could not read the stream state: {error}");
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                let _ = state::remove_pending(pending_path);
                let tail = stream::tail_log(log_path, 30);
                let mut message = format!("stream failed to start ({status})");
                if !tail.is_empty() {
                    message.push('\n');
                    message.push_str(&tail);
                }
                notify::send(
                    config.notifications,
                    "Stream failed",
                    &format!(
                        "gpu-screen-recorder did not start. Log: {}",
                        log_path.display()
                    ),
                );
                return Err(message);
            }
            Ok(None) => {}
            Err(error) => {
                let _ = state::remove_pending(pending_path);
                return Err(format!("failed to wait for the daemon: {error}"));
            }
        }

        if Instant::now() >= deadline {
            let _ = process::signal(daemon_pid, libc::SIGTERM);
            if !process::wait_dead(daemon_pid, Duration::from_secs(2)) {
                process::terminate(daemon_pid, Duration::from_secs(1));
            }
            let _ = state::remove_pending(pending_path);
            let _ = state::remove_state();
            let tail = stream::tail_log(log_path, 30);
            let mut message = "timed out waiting for the stream to start".to_string();
            if !tail.is_empty() {
                message.push('\n');
                message.push_str(&tail);
            }
            notify::send(
                config.notifications,
                "Stream failed",
                &format!("startup timed out. Log: {}", log_path.display()),
            );
            return Err(message);
        }

        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Play the live stream in mpv.
pub fn watch(stream_key: Option<String>) -> Result<(), String> {
    let mut config = load_config();
    if let Some(key) = stream_key {
        config.stream_key = key;
    }

    let stream_key = config::resolve_stream_key(&config.stream_key);
    let mut errors = config.validate();
    if let Err(error) = validate_stream_key(&stream_key) {
        errors.push(error);
    }
    if !errors.is_empty() {
        return Err(format!(
            "configuration errors:\n  - {}",
            errors.join("\n  - ")
        ));
    }

    crate::watch::run(&config, &stream_key)
}

/// Stop a running stream and clean up the old script helpers.
pub fn stop() -> Result<(), String> {
    let mut stopped = false;

    match state::read_state() {
        Ok(Some(state)) => {
            if process::is_our_daemon(state.daemon_pid) {
                let _ = process::signal(state.daemon_pid, libc::SIGTERM);
                if !process::wait_dead(state.daemon_pid, Duration::from_secs(9)) {
                    process::terminate(state.daemon_pid, Duration::from_secs(2));
                }
                stopped = true;
            }

            if process::is_gsr_whip(state.stream_pid) {
                process::terminate(state.stream_pid, Duration::from_secs(5));
                stopped = true;
            }

            state::remove_state()?;
            if stopped {
                println!("Stream stopped");
            }
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("gsr-broadcastbox: ignoring unreadable state file: {error}");
            let _ = state::remove_state();
        }
    }

    for description in stream::stop_legacy() {
        println!("Stopped {description}");
        stopped = true;
    }

    if !stopped {
        println!("Stream is not running");
    }
    Ok(())
}

/// Print the current stream state. Returns the process exit code.
pub fn status() -> Result<i32, String> {
    match state::read_state() {
        Ok(Some(state)) if process::is_gsr_whip(state.stream_pid) => {
            println!("Stream is running");
            println!("PID: {}", state.stream_pid);
            println!("Watch: {}", state.watch_url);
            println!("Monitor: {} | Quality: {}", state.monitor, state.quality);
            println!("Uptime: {}", state::format_age(state.started_at));
            if state.teamspeak {
                println!("TeamSpeak mode: on");
            }
            if !process::pid_alive(state.daemon_pid) {
                println!("Note: the tray daemon is not running");
            }
            println!("Log: {}", state.log_path.display());
            Ok(0)
        }
        Ok(state) => {
            if state.is_some() {
                let _ = state::remove_state();
            }
            if let Some(pid) = stream::legacy_stream_pid() {
                println!("Stream is running from the old stream.sh (PID: {pid})");
                println!("Run `gsr-broadcastbox stop` to migrate.");
                return Ok(0);
            }
            println!("Stream is not running");
            Ok(1)
        }
        Err(error) => {
            eprintln!("gsr-broadcastbox: {error}");
            println!("Stream is not running");
            Ok(1)
        }
    }
}

/// Print detected monitors, marking the active one.
pub fn list_monitors() -> Result<(), String> {
    let detected = monitors::detect_monitors();
    let active = monitors::active_monitor();

    if detected.is_empty() {
        println!("No monitors detected");
        return Ok(());
    }

    if let Some(active) = &active {
        println!("Current monitor: {active}");
    }
    for monitor in detected {
        let marker = if active.as_deref() == Some(monitor.name.as_str()) {
            " (current)"
        } else {
            ""
        };
        if monitor.detail.is_empty() {
            println!("  {}{marker}", monitor.name);
        } else {
            println!("  {} ({}){marker}", monitor.name, monitor.detail);
        }
    }
    Ok(())
}

/// Check for a stream that is already running and clean up stale state.
fn existing_stream() -> Result<bool, String> {
    match state::read_state() {
        Ok(Some(state)) => {
            if process::is_gsr_whip(state.stream_pid) {
                println!("Stream is already running (PID: {}).", state.stream_pid);
                println!("Watch: {}", state.watch_url);
                return Ok(true);
            }

            if process::is_our_daemon(state.daemon_pid) {
                let _ = process::signal(state.daemon_pid, libc::SIGTERM);
                if !process::wait_dead(state.daemon_pid, Duration::from_secs(9)) {
                    process::terminate(state.daemon_pid, Duration::from_secs(2));
                }
            }
            state::remove_state()?;
            Ok(false)
        }
        Ok(None) => Ok(false),
        Err(error) => {
            eprintln!("gsr-broadcastbox: removing unreadable state file: {error}");
            let _ = state::remove_state();
            Ok(false)
        }
    }
}

fn load_config() -> Config {
    let (mut config, notes) = config_file::load();
    for note in notes {
        if !note.starts_with("loaded ") && !note.starts_with("config: ") {
            eprintln!("gsr-broadcastbox: {note}");
        }
    }
    apply_environment(&mut config);
    config
}

fn validate_stream_key(stream_key: &str) -> Result<(), String> {
    if stream_key.contains('/')
        || stream_key.contains('?')
        || stream_key.contains('#')
        || stream_key.chars().any(char::is_whitespace)
    {
        return Err(format!(
            "stream key {stream_key:?} cannot contain whitespace, '/', '?' or '#'"
        ));
    }
    Ok(())
}

fn apply_environment(config: &mut Config) {
    if let Some(value) = non_empty_env("STREAM_KEY") {
        config.stream_key = value;
    }
    if let Some(value) = non_empty_env("STREAM_QUALITY") {
        config.quality = value;
    }
    if let Some(value) = non_empty_env("STREAM_MONITOR") {
        config.monitor = value;
    }
    if let Some(value) = non_empty_env("WHIP_URL") {
        config.whip_url = value;
    }
    if let Some(value) = non_empty_env("WATCH_BASE_URL") {
        config.watch_base_url = value;
    }
    if let Ok(value) = std::env::var("TEAMSPEAK_MODE") {
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => config.teamspeak = true,
            "0" | "false" | "no" | "off" => config.teamspeak = false,
            _ => {}
        }
    }
}

fn apply_options(config: &mut Config, options: &StartOptions) {
    if let Some(value) = &options.stream_key {
        config.stream_key = value.clone();
    }
    if let Some(value) = &options.quality {
        config.quality = value.clone();
    }
    if let Some(value) = &options.monitor {
        config.monitor = value.clone();
    }
    if let Some(value) = options.teamspeak {
        config.teamspeak = value;
    }
    if let Some(value) = options.tray {
        config.tray = value;
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}
