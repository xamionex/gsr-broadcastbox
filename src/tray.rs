//! Native StatusNotifierItem tray indicator.

use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{ToolTip, Tray};

use crate::daemon::DaemonEvent;

/// Everything the menu needs to show while a stream is running.
pub struct TrayOptions {
    pub stream_key: String,
    pub watch_url: String,
    pub monitor: String,
    pub quality: String,
    pub teamspeak: bool,
}

/// The tray item and its menu callbacks.
pub struct BroadcastTray {
    stream_key: String,
    watch_url: String,
    monitor: String,
    quality: String,
    teamspeak: bool,
    events: Sender<DaemonEvent>,
}

impl Tray for BroadcastTray {
    fn id(&self) -> String {
        "gsr-broadcastbox".to_string()
    }

    fn icon_name(&self) -> String {
        "media-record".to_string()
    }

    fn title(&self) -> String {
        "gsr-broadcastbox".to_string()
    }

    fn tool_tip(&self) -> ToolTip {
        let mut description = format!(
            "Streaming: {}\n{}\n{} | {}",
            self.stream_key, self.watch_url, self.monitor, self.quality
        );
        if self.teamspeak {
            description.push_str("\nTeamSpeak mode");
        }
        ToolTip {
            title: "gsr-broadcastbox".to_string(),
            description,
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        open_url(&self.watch_url);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let mut items = vec![
            StandardItem {
                label: format!("Streaming: {}", self.stream_key),
                enabled: false,
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: format!("{} | {}", self.monitor, self.quality),
                enabled: false,
                ..Default::default()
            }
            .into(),
        ];

        if self.teamspeak {
            items.push(
                StandardItem {
                    label: "TeamSpeak mode: on".to_string(),
                    enabled: false,
                    ..Default::default()
                }
                .into(),
            );
        }

        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Open stream page".to_string(),
                icon_name: "web-browser".to_string(),
                activate: Box::new(|tray: &mut Self| open_url(&tray.watch_url)),
                ..Default::default()
            }
            .into(),
        );
        items.push(
            StandardItem {
                label: "Stop stream".to_string(),
                icon_name: "media-playback-stop".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.events.send(DaemonEvent::Stop);
                }),
                ..Default::default()
            }
            .into(),
        );
        items.push(MenuItem::Separator);
        items.push(
            StandardItem {
                label: "Quit indicator".to_string(),
                icon_name: "application-exit".to_string(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.events.send(DaemonEvent::QuitTray);
                }),
                ..Default::default()
            }
            .into(),
        );

        items
    }
}

/// Start the tray in ksni's background thread.
///
/// A missing StatusNotifierWatcher is not fatal, the stream keeps running without an indicator.
pub fn spawn(options: TrayOptions, events: Sender<DaemonEvent>) -> Option<Handle<BroadcastTray>> {
    let tray = BroadcastTray {
        stream_key: options.stream_key,
        watch_url: options.watch_url,
        monitor: options.monitor,
        quality: options.quality,
        teamspeak: options.teamspeak,
        events,
    };
    match tray.assume_sni_available(true).spawn() {
        Ok(handle) => Some(handle),
        Err(error) => {
            eprintln!("gsr-broadcastbox: could not start the tray indicator: {error}");
            None
        }
    }
}

/// Open a URL with the desktop handler without blocking the tray thread.
fn open_url(url: &str) {
    Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
}
