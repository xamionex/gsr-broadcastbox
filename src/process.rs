//! Process inspection and termination helpers.

use std::path::Path;
use std::time::{Duration, Instant};

/// True when a process with this pid exists.
pub fn pid_alive(pid: u32) -> bool {
    if pid <= 1 {
        return false;
    }
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if result == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// Command line of a process, split at the NUL separators in `/proc/<pid>/cmdline`.
pub fn cmdline(pid: u32) -> Vec<String> {
    let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

/// Basename of a command line argument.
fn program_name(argument: &str) -> &str {
    Path::new(argument)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(argument)
}

/// True when this process is a gpu-screen-recorder WHIP stream.
pub fn is_gsr_whip(pid: u32) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let args = cmdline(pid);
    let Some(first) = args.first() else {
        return false;
    };
    if program_name(first) != "gpu-screen-recorder" {
        return false;
    }
    whip_flag(&args)
}

fn whip_flag(args: &[String]) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == "-c" && pair[1] == "whip")
        || args.iter().any(|argument| argument == "-cwhip")
}

/// True when this process is a gsr-broadcastbox daemon, which guards against PID reuse.
pub fn is_our_daemon(pid: u32) -> bool {
    if !pid_alive(pid) {
        return false;
    }
    let args = cmdline(pid);
    let Some(first) = args.first() else {
        return false;
    };
    program_name(first) == "gsr-broadcastbox" && args.iter().any(|argument| argument == "daemon")
}

/// Find running gpu-screen-recorder WHIP processes.
pub fn find_gsr_whip() -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|name| name.chars().all(|c| c.is_ascii_digit()))
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if is_gsr_whip(pid) {
            found.push(pid);
        }
    }
    found.sort_unstable();
    found
}

/// True when a running process has one of these `comm` names or command basenames.
pub fn process_running(names: &[&str]) -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|name| name.chars().all(|c| c.is_ascii_digit()))
        else {
            continue;
        };
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if names.contains(&comm.trim()) {
            return true;
        }
        let args = cmdline(pid.parse().unwrap_or(0));
        if let Some(first) = args.first() {
            if names.contains(&program_name(first)) {
                return true;
            }
        }
    }
    false
}

/// Send a signal, returning false when the process is already gone.
pub fn signal(pid: u32, signal: i32) -> bool {
    if pid <= 1 {
        return false;
    }
    unsafe { libc::kill(pid as libc::pid_t, signal) == 0 }
}

/// Wait until a process disappears or the timeout expires.
///
/// Only use this for processes this one does not own, since an unreaped child
/// still answers `kill(pid, 0)`.
pub fn wait_dead(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !pid_alive(pid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Ask a process to stop, escalating from SIGINT to SIGTERM to SIGKILL.
pub fn terminate(pid: u32, grace: Duration) {
    if !pid_alive(pid) {
        return;
    }
    signal(pid, libc::SIGINT);
    if wait_dead(pid, grace) {
        return;
    }

    signal(pid, libc::SIGTERM);
    if wait_dead(pid, Duration::from_secs(2)) {
        return;
    }

    signal(pid, libc::SIGKILL);
    wait_dead(pid, Duration::from_secs(1));
}

/// True when the command line mentions a marker, used to recognize the old helper processes.
pub fn cmdline_contains(pid: u32, marker: &str) -> bool {
    cmdline(pid)
        .iter()
        .any(|argument| argument.contains(marker))
}

/// Current process id as `u32`.
pub fn current_pid() -> u32 {
    std::process::id()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_process_is_alive() {
        assert!(pid_alive(current_pid()));
        assert!(!pid_alive(0));
        assert!(!pid_alive(1));
    }

    #[test]
    fn whisper_flags_are_recognized() {
        let args = vec![
            "gpu-screen-recorder".to_string(),
            "-c".to_string(),
            "whip".to_string(),
        ];
        assert!(whip_flag(&args));
        let args = vec!["gpu-screen-recorder".to_string(), "-cwhip".to_string()];
        assert!(whip_flag(&args));
        let args = vec![
            "gpu-screen-recorder".to_string(),
            "-c".to_string(),
            "mp4".to_string(),
        ];
        assert!(!whip_flag(&args));
    }

    #[test]
    fn program_name_strips_directories() {
        assert_eq!(
            program_name("/usr/bin/gpu-screen-recorder"),
            "gpu-screen-recorder"
        );
        assert_eq!(program_name("gpu-screen-recorder"), "gpu-screen-recorder");
    }
}
