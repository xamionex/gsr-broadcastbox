//! GPU Screen Recorder WHIP broadcaster.
//!
//! The binary has two jobs:
//! - a short lived CLI that starts and stops a detached streaming session
//! - a daemon process that owns gpu-screen-recorder, the tray indicator and the TeamSpeak watcher
//!
//! The configuration is a TOML file written on first run and editable with the built in TUI.

pub mod cli;
pub mod commands;
pub mod config;
pub mod config_file;
pub mod daemon;
pub mod monitors;
pub mod notify;
pub mod process;
pub mod state;
pub mod stream;
pub mod tray;
pub mod tui;

/// Parse the command line and run it.
///
/// Returns the process exit code.
pub fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::run(args) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("gsr-broadcastbox: {message}");
            1
        }
    }
}
