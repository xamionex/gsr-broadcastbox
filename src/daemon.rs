//! The detached process that owns gpu-screen-recorder, the tray and the TeamSpeak watcher.

use std::path::PathBuf;
use std::process::ExitStatus;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::config::Config;
use crate::state::{self, PendingLaunch, StreamState};
use crate::{notify, process, stream, tray};

/// Events sent by the tray and the TeamSpeak watcher.
pub enum DaemonEvent {
    Stop,
    QuitTray,
    TeamspeakMissing,
}

/// Signals are handled by a flag, the wait loop polls it.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_signal: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

fn install_signals() {
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

enum StopReason {
    User,
    Shutdown,
    Teamspeak,
    Child(ExitStatus),
}

/// Run a pending launch until the stream stops.
pub fn run(pending_path: PathBuf) -> Result<(), String> {
    install_signals();

    let PendingLaunch {
        config,
        stream_key,
        monitor,
        log_path,
    } = state::take_pending(&pending_path)?.ok_or_else(|| {
        format!(
            "pending launch file {} is missing or already consumed",
            pending_path.display()
        )
    })?;

    let quality = config.quality_label();
    eprintln!(
        "[daemon] starting stream for {} on {} ({}, {} fps)",
        stream_key, monitor, quality, config.fps
    );

    let mut child = stream::spawn_gsr(&config, &monitor, &stream_key, &log_path)?;
    let stream_pid = child.id();

    // Give gpu-screen-recorder a moment to fail on an invalid argument before reporting success.
    std::thread::sleep(Duration::from_millis(700));
    if let Some(status) = child
        .try_wait()
        .map_err(|e| format!("failed to check gpu-screen-recorder: {e}"))?
    {
        let tail = stream::tail_log(&log_path, 20);
        return Err(format!(
            "gpu-screen-recorder exited immediately ({status})\n{tail}"
        ));
    }

    let watch_url = crate::config::watch_url(&config.watch_base_url, &stream_key);
    let state_value = StreamState {
        daemon_pid: process::current_pid(),
        stream_pid,
        stream_key: stream_key.clone(),
        watch_url: watch_url.clone(),
        monitor: monitor.clone(),
        quality: quality.clone(),
        teamspeak: config.teamspeak,
        started_at: state::now_secs(),
        log_path: log_path.clone(),
    };
    if let Err(error) = state::write_state(&state_value) {
        let _ = stream::stop_child(&mut child, stream_pid, Duration::from_secs(5));
        return Err(error);
    }

    eprintln!(
        "[daemon] stream started (PID: {stream_pid}), watch: {watch_url}, log: {}",
        log_path.display()
    );

    let (events, receiver) = mpsc::channel();
    let mut tray_handle = if config.tray {
        tray::spawn(
            tray::TrayOptions {
                stream_key: stream_key.clone(),
                watch_url: watch_url.clone(),
                monitor: monitor.clone(),
                quality: quality.clone(),
                teamspeak: config.teamspeak,
            },
            events.clone(),
        )
    } else {
        None
    };

    let teamspeak_stop = Arc::new(AtomicBool::new(false));
    let teamspeak_thread = if config.teamspeak {
        Some(spawn_teamspeak_watch(
            config.teamspeak_process.clone(),
            events,
            Arc::clone(&teamspeak_stop),
        ))
    } else {
        None
    };

    let mut reason: Option<StopReason> = None;
    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            reason = Some(StopReason::Shutdown);
            break;
        }

        while let Ok(event) = receiver.try_recv() {
            match event {
                DaemonEvent::Stop => {
                    reason = Some(StopReason::User);
                    break;
                }
                DaemonEvent::QuitTray => {
                    if let Some(handle) = tray_handle.take() {
                        handle.shutdown().wait();
                    }
                }
                DaemonEvent::TeamspeakMissing => {
                    reason = Some(StopReason::Teamspeak);
                    break;
                }
            }
        }
        if reason.is_some() {
            break;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                reason = Some(StopReason::Child(status));
                break;
            }
            Ok(None) => {}
            Err(error) => {
                eprintln!("[daemon] failed to wait for gpu-screen-recorder: {error}");
                reason = Some(StopReason::Shutdown);
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    teamspeak_stop.store(true, Ordering::Relaxed);
    if let Some(handle) = teamspeak_thread {
        let _ = handle.join();
    }
    if let Some(handle) = tray_handle.take() {
        handle.shutdown().wait();
    }

    let outcome = match reason {
        Some(StopReason::Child(status)) => status,
        _ => stream::stop_child(&mut child, stream_pid, Duration::from_secs(5))?,
    };

    state::remove_state_if_daemon(process::current_pid())?;
    eprintln!(
        "[daemon] stream stopped ({outcome}), watch: {watch_url}, log: {}",
        log_path.display()
    );

    if let Some(reason) = reason {
        notify_for_reason(&config, &reason, &log_path, outcome);
    }

    Ok(())
}

fn spawn_teamspeak_watch(
    process_name: String,
    events: mpsc::Sender<DaemonEvent>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut misses = 0;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(1));
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if process::process_running(&[process_name.as_str()]) {
                misses = 0;
            } else {
                misses += 1;
                if misses >= 2 {
                    let _ = events.send(DaemonEvent::TeamspeakMissing);
                    return;
                }
            }
        }
    })
}

fn notify_for_reason(
    config: &Config,
    reason: &StopReason,
    log_path: &std::path::Path,
    outcome: ExitStatus,
) {
    let log = log_path.display();
    match reason {
        StopReason::User | StopReason::Shutdown => {}
        StopReason::Teamspeak => notify::send(
            config.notifications,
            "Stream stopped",
            "TeamSpeak is not running",
        ),
        StopReason::Child(status) if status.success() => notify::send(
            config.notifications,
            "Stream stopped",
            &format!("gpu-screen-recorder exited. Log: {log}"),
        ),
        StopReason::Child(_) => notify::send(
            config.notifications,
            "Stream stopped",
            &format!("gpu-screen-recorder exited with {outcome}. Log: {log}"),
        ),
    }
}
