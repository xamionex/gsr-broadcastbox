//! The user config file: `~/.config/gsr-broadcastbox/config.toml`.
//!
//! The file is written with defaults the first time the app needs it and is never overwritten afterwards.
//! Missing keys fall back to the built in defaults, so hand trimmed files keep working.

use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item};

use crate::config::Config;

/// Directory holding the app's own files.
pub fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("gsr-broadcastbox"))
}

/// Path of the user config file.
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.toml"))
}

/// Default config file contents, written on first run.
///
/// The keys here and [`Config`] are kept in sync by `default_config_matches_builtin_defaults`.
pub const DEFAULT_CONFIG: &str = r#"# gsr-broadcastbox configuration.
#
# Written with defaults on first run. Edit directly or run `gsr-broadcastbox config`.
# Command line flags and the matching environment variables override these values.

# --- Connection ---

# Stream key used for the WHIP ingest and in the watch URL.
# Empty means the current username.
stream_key = ""

# WHIP ingest endpoint.
whip_url = "https://broadcast.petar.cc/api/whip"

# Base URL of the watch page. The stream key is appended.
watch_base_url = "https://broadcast.petar.cc"

# --- Watch ---

# Use hardware decoding in mpv. Software decoding has lower latency but uses more CPU.
watch_hardware_decode = false

# Extra mpv flags passed to watch, one entry per flag. The default mutes audio.
watch_mpv_args = ["--mute=yes"]

# --- Video ---

# Monitor name such as "DP-1", or "current" to follow the active monitor when a stream starts.
monitor = "current"

# Quality preset: medium, high, very_high or ultra.
quality = "high"

# Video bitrate in kbps, used when bitrate_mode is "cbr".
video_bitrate = 8000

# auto, qp, vbr or cbr. cbr keeps a constant bitrate and caps size.
bitrate_mode = "cbr"

# Frames per second.
fps = 60

# Output size limit, for example 1920x1080. 0x0 keeps the source size.
resolution = "1920x1080"

# Video codec: auto, h264, hevc, av1, vp8 or vp9.
codec = "h264"

# Color range: limited or full.
color_range = "limited"

# --- Audio ---

# Audio codec: opus, aac, flac or mp3.
audio_codec = "opus"

# Audio bitrate in kbps.
audio_bitrate = 128

# Application audio sources mixed into the stream. Entries are joined with "|" for gpu-screen-recorder.
audio_sources = [
    "app-inverse:TeamSpeak",
    "app-inverse:discord",
    "app-inverse:plasmashell",
    "app-inverse:pipewire",
    "app-inverse:linuxsoundboard.local_playback",
    "app-inverse:linuxsoundboard.virtual_mic_feeder",
]

# Refuse to start without the process and stop the stream when it exits.
teamspeak = false

# Process name watched by TeamSpeak mode.
teamspeak_process = "TeamSpeak"

# --- Application ---

# Show the tray indicator while streaming.
tray = true

# Notify on failures and unexpected stops.
notifications = true

# Extra gpu-screen-recorder arguments, appended as-is.
extra_gsr_args = []
"#;

/// Load the config file, creating it with defaults when missing.
///
/// Returns the config and human readable notes for the caller to print.
/// A bad file is reported and the built in defaults are used so a stream can still start.
pub fn load() -> (Config, Vec<String>) {
    match config_path() {
        Some(path) => load_from(&path),
        None => (
            Config::default(),
            vec!["could not resolve a config directory, using built-in defaults".to_string()],
        ),
    }
}

/// [`load`] for an explicit path, used by tests.
fn load_from(path: &Path) -> (Config, Vec<String>) {
    if !path.is_file() {
        return (Config::default(), create_default(path));
    }

    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            return (
                Config::default(),
                vec![format!(
                    "failed to read {}: {e}{}",
                    path.display(),
                    permission_hint(&e, path)
                )],
            );
        }
    };

    match toml::from_str::<Config>(&text) {
        Ok(config) => {
            let mut notes = vec![format!("loaded {}", path.display())];
            if let Ok(doc) = text.parse::<DocumentMut>() {
                let unknown = unknown_keys(&doc);
                if !unknown.is_empty() {
                    notes.push(format!(
                        "ignoring unknown keys in {}: {}",
                        path.display(),
                        unknown.join(", ")
                    ));
                }
            }
            for error in config.validate() {
                notes.push(format!("config: {error}"));
            }
            (config, notes)
        }
        Err(e) => (
            Config::default(),
            vec![format!(
                "failed to parse {}: {e}. Using built-in defaults.",
                path.display()
            )],
        ),
    }
}

/// Write [`DEFAULT_CONFIG`] to `path`, creating parent directories.
fn create_default(path: &Path) -> Vec<String> {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            eprintln!(
                "gsr-broadcastbox: failed to create {}: {e}{}",
                parent.display(),
                permission_hint(&e, parent)
            );
            return vec![format!("failed to create {}", parent.display())];
        }
    }
    match write_private(path, DEFAULT_CONFIG) {
        Ok(()) => vec![format!("created default config at {}", path.display())],
        Err(e) => vec![format!(
            "failed to write {}: {e}{}",
            path.display(),
            permission_hint(&e, path)
        )],
    }
}

/// Top level keys the app does not know about.
fn unknown_keys(doc: &DocumentMut) -> Vec<String> {
    let defaults = default_document();
    let known: Vec<&str> = defaults.iter().map(|(key, _)| key).collect();
    doc.iter()
        .filter(|(key, _)| !known.contains(key))
        .map(|(key, _)| key.to_string())
        .collect()
}

/// A hint appended to permission errors.
fn permission_hint(error: &std::io::Error, path: &Path) -> String {
    if error.kind() != std::io::ErrorKind::PermissionDenied {
        return String::new();
    }
    let dir = path.parent().unwrap_or(path);
    let user = std::env::var("USER").unwrap_or_else(|_| "$USER".to_string());
    format!(" (check ownership: sudo chown -R {user} {})", dir.display())
}

/// Parse [`DEFAULT_CONFIG`] into an editable document.
pub fn default_document() -> DocumentMut {
    DEFAULT_CONFIG
        .parse::<DocumentMut>()
        .expect("DEFAULT_CONFIG is valid TOML")
}

/// Read the config file for editing, falling back to the defaults.
///
/// Returns the document and an optional warning for the editor.
pub fn load_document(path: &Path) -> (DocumentMut, Option<String>) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (default_document(), None);
    };
    match text.parse::<DocumentMut>() {
        Ok(doc) => {
            let unknown = unknown_keys(&doc);
            let warning = if unknown.is_empty() {
                None
            } else {
                Some(format!("ignoring unknown keys: {}", unknown.join(", ")))
            };
            (doc, warning)
        }
        Err(e) => (
            default_document(),
            Some(format!(
                "{} could not be parsed ({e}), showing defaults",
                path.display()
            )),
        ),
    }
}

/// Fill in keys that are missing from `doc` using the defaults.
pub fn complete_document(doc: &mut DocumentMut, defaults: &DocumentMut) {
    let keys: Vec<&str> = defaults.iter().map(|(key, _)| key).collect();
    for key in keys {
        if doc.get(key).is_some() {
            continue;
        }
        let Some(source) = defaults.get(key) else {
            continue;
        };
        doc[key] = source.clone();
    }
}

/// Write an edited document back to `path`, creating parent directories.
pub fn save_document(doc: &DocumentMut, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
    }
    write_private(path, &doc.to_string()).map_err(|e| {
        format!(
            "failed to write {}: {e}{}",
            path.display(),
            permission_hint(&e, path)
        )
    })
}

/// Write a file only readable by the user, because it can hold a stream key.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

/// Serialize a config back to TOML for tests and diagnostics.
pub fn to_toml(config: &Config) -> Result<String, String> {
    toml::to_string_pretty(config).map_err(|e| e.to_string())
}

/// Render one field value for diagnostics.
pub fn document_field<'a>(doc: &'a DocumentMut, key: &str) -> Option<&'a Item> {
    doc.get(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gsr_broadcastbox_cfg_{}_{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn default_config_matches_builtin_defaults() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("template parses");
        assert_eq!(
            config,
            Config::default(),
            "template drifted from Config::default"
        );
        assert!(config.validate().is_empty());
    }

    #[test]
    fn default_document_has_every_struct_field() {
        let doc = default_document();
        let config = Config::default();
        let text = to_toml(&config).unwrap();
        let serialized: DocumentMut = text.parse().unwrap();
        for (key, _) in serialized.iter() {
            assert!(doc.get(key).is_some(), "DEFAULT_CONFIG is missing {key}");
        }
    }

    #[test]
    fn partial_config_overrides_only_listed_fields() {
        let config: Config = toml::from_str("quality = \"ultra\"\nmonitor = \"DP-2\"").unwrap();
        assert_eq!(config.quality, "ultra");
        assert_eq!(config.monitor, "DP-2");
        assert_eq!(config.fps, 60);
        assert!(config.tray);
    }

    #[test]
    fn load_creates_default_config_when_missing() {
        let dir = scratch("create");
        let path = dir.join("config.toml");
        let (config, notes) = load_from(&path);

        assert_eq!(config, Config::default());
        assert!(path.is_file());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG);
        assert!(notes[0].contains("created default config"));

        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reports_parse_errors_and_keeps_defaults() {
        let dir = scratch("bad");
        let path = dir.join("config.toml");
        std::fs::write(&path, "this is not toml").unwrap();

        let (config, notes) = load_from(&path);
        assert_eq!(config, Config::default());
        assert!(notes[0].contains("failed to parse"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_document_round_trips_and_keeps_comments() {
        let dir = scratch("save");
        let path = dir.join("sub").join("config.toml");
        let (mut doc, warning) = load_document(&path);
        assert!(warning.is_none());

        doc["quality"] = toml_edit::value("ultra");
        save_document(&doc, &path).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# gsr-broadcastbox configuration."));
        let config: Config = toml::from_str(&text).unwrap();
        assert_eq!(config.quality, "ultra");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn complete_document_adds_missing_keys() {
        let mut doc: DocumentMut = "quality = \"low\"\n".parse().unwrap();
        let defaults = default_document();
        complete_document(&mut doc, &defaults);

        assert_eq!(
            doc.get("quality").and_then(Item::as_str),
            Some("low"),
            "existing values are kept"
        );
        assert!(doc.get("monitor").is_some());
        assert!(doc.get("audio_sources").is_some());
    }
}
