//! Best effort desktop notifications.

use std::process::{Command, Stdio};

/// Send a notification when the user has them enabled.
pub fn send(enabled: bool, summary: &str, body: &str) {
    if !enabled {
        return;
    }
    let mut command = Command::new("notify-send");
    command.arg(summary);
    if !body.is_empty() {
        command.arg(body);
    }
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
}
