//! Runtime configuration and validation.

use serde::{Deserialize, Serialize};

pub const QUALITIES: &[&str] = &["medium", "high", "very_high", "ultra"];
pub const CODECS: &[&str] = &["auto", "h264", "hevc", "av1", "vp8", "vp9"];
pub const BITRATE_MODES: &[&str] = &["auto", "qp", "vbr", "cbr"];
pub const COLOR_RANGES: &[&str] = &["limited", "full"];
pub const AUDIO_CODECS: &[&str] = &["opus", "aac", "flac", "mp3"];

/// Settings read from `config.toml`, overridable from the environment and command line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Stream key used for the WHIP ingest and the watch URL. Empty means the current username.
    pub stream_key: String,
    /// WHIP ingest endpoint passed to `gpu-screen-recorder -o`.
    pub whip_url: String,
    /// Base URL of the watch page, the stream key is appended.
    pub watch_base_url: String,
    /// Monitor name such as `DP-1`, or `current` to follow the active monitor when the stream starts.
    pub monitor: String,
    /// Quality preset used in QP and VBR modes.
    pub quality: String,
    /// Video bitrate in kbps, used when `bitrate_mode` is `cbr`.
    pub video_bitrate: u32,
    /// `auto`, `qp`, `vbr` or `cbr`.
    pub bitrate_mode: String,
    pub fps: u32,
    /// Output size limit, for example `1920x1080`. `0x0` keeps the source size.
    pub resolution: String,
    pub codec: String,
    /// `limited` or `full`.
    pub color_range: String,
    pub audio_codec: String,
    pub audio_bitrate: u32,
    /// Application audio sources, joined with `|` for gpu-screen-recorder.
    pub audio_sources: Vec<String>,
    /// Stop the stream when the TeamSpeak process exits.
    pub teamspeak: bool,
    /// Process name checked by the TeamSpeak watcher.
    pub teamspeak_process: String,
    /// Show the tray indicator.
    pub tray: bool,
    /// Use desktop notifications for failures and unexpected stops.
    pub notifications: bool,
    /// Extra arguments appended to the gpu-screen-recorder command.
    pub extra_gsr_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            stream_key: String::new(),
            whip_url: "https://broadcast.petar.cc/api/whip".to_string(),
            watch_base_url: "https://broadcast.petar.cc".to_string(),
            monitor: "current".to_string(),
            quality: "high".to_string(),
            video_bitrate: 8000,
            bitrate_mode: "qp".to_string(),
            fps: 60,
            resolution: "1920x1080".to_string(),
            codec: "h264".to_string(),
            color_range: "limited".to_string(),
            audio_codec: "opus".to_string(),
            audio_bitrate: 128,
            audio_sources: vec![
                "app-inverse:TeamSpeak".to_string(),
                "app-inverse:discord".to_string(),
                "app-inverse:plasmashell".to_string(),
                "app-inverse:pipewire".to_string(),
                "app-inverse:linuxsoundboard.local_playback".to_string(),
                "app-inverse:linuxsoundboard.virtual_mic_feeder".to_string(),
            ],
            teamspeak: false,
            teamspeak_process: "TeamSpeak".to_string(),
            tray: true,
            notifications: true,
            extra_gsr_args: Vec::new(),
        }
    }
}

impl Config {
    /// Value passed to `-q`: a preset for QP/VBR, a bitrate for CBR.
    pub fn quality_argument(&self) -> String {
        if self.bitrate_mode == "cbr" {
            self.video_bitrate.to_string()
        } else {
            self.quality.clone()
        }
    }

    /// Application audio sources joined for `-a`, or `None` when no source is configured.
    pub fn audio_argument(&self) -> Option<String> {
        let sources: Vec<&str> = self
            .audio_sources
            .iter()
            .map(|source| source.trim())
            .filter(|source| !source.is_empty())
            .collect();
        if sources.is_empty() {
            None
        } else {
            Some(sources.join("|"))
        }
    }

    /// Everything that would stop a stream from starting, as human readable messages.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if !is_http_url(&self.whip_url) {
            errors.push(format!(
                "whip_url must start with http:// or https:// (got {:?})",
                self.whip_url
            ));
        }
        if !is_http_url(&self.watch_base_url) {
            errors.push(format!(
                "watch_base_url must start with http:// or https:// (got {:?})",
                self.watch_base_url
            ));
        }
        if self.monitor.trim().is_empty() {
            errors.push("monitor must be a monitor name or \"current\"".to_string());
        }
        if !QUALITIES.contains(&self.quality.as_str()) {
            errors.push(format!(
                "quality must be one of {} (got {:?})",
                QUALITIES.join(", "),
                self.quality
            ));
        }
        if !CODECS.contains(&self.codec.as_str()) {
            errors.push(format!(
                "codec must be one of {} (got {:?})",
                CODECS.join(", "),
                self.codec
            ));
        }
        if !BITRATE_MODES.contains(&self.bitrate_mode.as_str()) {
            errors.push(format!(
                "bitrate_mode must be one of {} (got {:?})",
                BITRATE_MODES.join(", "),
                self.bitrate_mode
            ));
        }
        if !COLOR_RANGES.contains(&self.color_range.as_str()) {
            errors.push(format!(
                "color_range must be one of {} (got {:?})",
                COLOR_RANGES.join(", "),
                self.color_range
            ));
        }
        if !AUDIO_CODECS.contains(&self.audio_codec.as_str()) {
            errors.push(format!(
                "audio_codec must be one of {} (got {:?})",
                AUDIO_CODECS.join(", "),
                self.audio_codec
            ));
        }
        if self.fps == 0 || self.fps > 480 {
            errors.push(format!("fps must be between 1 and 480 (got {})", self.fps));
        }
        if self.video_bitrate == 0 || self.video_bitrate > 200_000 {
            errors.push(format!(
                "video_bitrate must be between 1 and 200000 kbps (got {})",
                self.video_bitrate
            ));
        }
        if self.audio_bitrate > 512 {
            errors.push(format!(
                "audio_bitrate must be 512 kbps or less (got {})",
                self.audio_bitrate
            ));
        }
        if !valid_resolution(&self.resolution) {
            errors.push(format!(
                "resolution must look like 1920x1080 or 0x0 (got {:?})",
                self.resolution
            ));
        }
        if self.teamspeak && self.teamspeak_process.trim().is_empty() {
            errors.push("teamspeak_process must name the process to watch".to_string());
        }
        if self
            .extra_gsr_args
            .iter()
            .any(|argument| argument.trim().is_empty())
        {
            errors.push("extra_gsr_args contains an empty entry".to_string());
        }
        errors
    }
}

/// Stream key used for this session: the configured value, else the current username.
pub fn resolve_stream_key(configured: &str) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        return configured.to_string();
    }
    for variable in ["USER", "LOGNAME"] {
        if let Ok(value) = std::env::var(variable) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_string();
            }
        }
    }
    let output = std::process::Command::new("id").arg("-un").output();
    if let Ok(output) = output {
        let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !value.is_empty() {
            return value;
        }
    }
    "stream".to_string()
}

/// Watch page for a key, with the trailing slash on the base URL normalized.
pub fn watch_url(base: &str, key: &str) -> String {
    format!("{}/{}", base.trim_end_matches('/'), key)
}

fn is_http_url(value: &str) -> bool {
    let value = value.trim();
    value.starts_with("http://") || value.starts_with("https://")
}

fn valid_resolution(value: &str) -> bool {
    let value = value.trim();
    let Some((width, height)) = value.split_once('x') else {
        return false;
    };
    if width.is_empty() || height.is_empty() {
        return false;
    }
    width.chars().all(|c| c.is_ascii_digit()) && height.chars().all(|c| c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        assert!(Config::default().validate().is_empty());
    }

    #[test]
    fn cbr_uses_the_video_bitrate_for_q() {
        let mut config = Config {
            bitrate_mode: "cbr".to_string(),
            video_bitrate: 12000,
            ..Config::default()
        };
        assert_eq!(config.quality_argument(), "12000");

        config.bitrate_mode = "qp".to_string();
        config.quality = "very_high".to_string();
        assert_eq!(config.quality_argument(), "very_high");
    }

    #[test]
    fn audio_sources_are_joined_and_empties_skipped() {
        let mut config = Config::default();
        assert_eq!(
            config.audio_argument().as_deref(),
            Some(
                "app-inverse:TeamSpeak|app-inverse:discord|app-inverse:plasmashell|app-inverse:pipewire|app-inverse:linuxsoundboard.local_playback|app-inverse:linuxsoundboard.virtual_mic_feeder"
            )
        );

        config.audio_sources = vec![" one ".to_string(), String::new(), "two".to_string()];
        assert_eq!(config.audio_argument().as_deref(), Some("one|two"));

        config.audio_sources.clear();
        assert_eq!(config.audio_argument(), None);
    }

    #[test]
    fn invalid_values_are_reported() {
        let config = Config {
            whip_url: "ftp://example".to_string(),
            quality: "best".to_string(),
            fps: 0,
            resolution: "wide".to_string(),
            audio_bitrate: 999,
            ..Config::default()
        };

        let errors = config.validate();
        assert!(errors.iter().any(|e| e.contains("whip_url")));
        assert!(errors.iter().any(|e| e.contains("quality")));
        assert!(errors.iter().any(|e| e.contains("fps")));
        assert!(errors.iter().any(|e| e.contains("resolution")));
        assert!(errors.iter().any(|e| e.contains("audio_bitrate")));
    }

    #[test]
    fn watch_url_normalizes_the_base() {
        assert_eq!(
            watch_url("https://broadcast.petar.cc/", "petar"),
            "https://broadcast.petar.cc/petar"
        );
        assert_eq!(
            watch_url("https://broadcast.petar.cc", "petar"),
            "https://broadcast.petar.cc/petar"
        );
    }

    #[test]
    fn resolution_parser_accepts_common_forms() {
        assert!(valid_resolution("1920x1080"));
        assert!(valid_resolution("0x0"));
        assert!(!valid_resolution("1920×1080"));
        assert!(!valid_resolution("1920"));
        assert!(!valid_resolution("x1080"));
    }
}
