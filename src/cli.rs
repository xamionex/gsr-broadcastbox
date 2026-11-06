//! Command line parsing and dispatch.

use std::path::PathBuf;

use crate::commands::{self, StartOptions};
use crate::{daemon, tui};

const HELP: &str = "\
gsr-broadcastbox - GPU Screen Recorder WHIP broadcaster

Usage: gsr-broadcastbox <command> [options]

Commands:
  start [key] [options]   Start streaming in the background
  stop                    Stop the stream and the tray indicator
  restart [options]       Stop and start again
  status                  Show the current stream state
  monitors                List detected monitors
  config                  Edit the configuration in a terminal UI
  help                    Show this help
  version                 Show the version

Common start options:
  -k, --key <key>         Stream key (defaults to stream_key or your username)
  -q, --quality <preset>  medium, high, very_high or ultra
  -m, --monitor <name>    Monitor name, or \"current\" to follow the active output
  -t, --teamspeak         Stop when TeamSpeak exits
      --no-teamspeak      Disable TeamSpeak mode for this run
      --no-tray           Do not show the tray indicator
  -f, --foreground        Keep the stream attached to this terminal

Environment overrides for start:
  STREAM_KEY, STREAM_QUALITY, STREAM_MONITOR, TEAMSPEAK_MODE, WHIP_URL, WATCH_BASE_URL

Configuration file:
  ~/.config/gsr-broadcastbox/config.toml
";

const START_HELP: &str = "\
Usage: gsr-broadcastbox start [key] [options]

Options:
  -k, --key <key>         Stream key (defaults to stream_key or your username)
  -q, --quality <preset>  medium, high, very_high or ultra
  -m, --monitor <name>    Monitor name, or \"current\" to follow the active output
  -t, --teamspeak         Stop when TeamSpeak exits
      --no-teamspeak      Disable TeamSpeak mode for this run
      --tray              Show the tray indicator
      --no-tray           Do not show the tray indicator
  -f, --foreground        Keep the stream attached to this terminal
  -h, --help              Show this help
";

/// Parse the arguments after the binary name and run the command.
///
/// Returns the process exit code.
pub fn run(args: Vec<String>) -> Result<i32, String> {
    let Some(command) = args.first().map(String::as_str) else {
        print!("{HELP}");
        return Ok(0);
    };

    match command {
        "start" => {
            if wants_help(&args[1..]) {
                print!("{START_HELP}");
                return Ok(0);
            }
            let options = parse_start(&args[1..])?;
            commands::start(options)?;
            Ok(0)
        }
        "stop" => {
            commands::stop()?;
            Ok(0)
        }
        "restart" => {
            if wants_help(&args[1..]) {
                print!("{START_HELP}");
                return Ok(0);
            }
            let options = parse_start(&args[1..])?;
            commands::stop()?;
            commands::start(options)?;
            Ok(0)
        }
        "status" => commands::status(),
        "monitors" | "monitor" => {
            commands::list_monitors()?;
            Ok(0)
        }
        "config" | "edit" => {
            tui::run()?;
            Ok(0)
        }
        "daemon" => {
            let pending = parse_pending(&args[1..])?;
            daemon::run(pending)?;
            Ok(0)
        }
        "help" | "--help" | "-h" => {
            print!("{HELP}");
            Ok(0)
        }
        "version" | "--version" | "-V" => {
            println!("gsr-broadcastbox {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        other => Err(format!(
            "unknown command {other:?}. Run `gsr-broadcastbox help`."
        )),
    }
}

fn wants_help(args: &[String]) -> bool {
    args.iter()
        .any(|argument| argument == "--help" || argument == "-h")
}

/// Parse the options accepted by `start` and `restart`.
fn parse_start(args: &[String]) -> Result<StartOptions, String> {
    let mut options = StartOptions::default();
    let mut positional_key: Option<String> = None;

    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        match argument {
            "-k" | "--key" => {
                options.stream_key = Some(next_value(args, &mut index, argument)?);
            }
            "-q" | "--quality" => {
                options.quality = Some(next_value(args, &mut index, argument)?);
            }
            "-m" | "--monitor" => {
                options.monitor = Some(next_value(args, &mut index, argument)?);
            }
            "-t" | "--teamspeak" => options.teamspeak = Some(true),
            "--no-teamspeak" => options.teamspeak = Some(false),
            "--tray" => options.tray = Some(true),
            "--no-tray" => options.tray = Some(false),
            "-f" | "--foreground" => options.foreground = true,
            other if other.starts_with('-') => {
                return Err(format!(
                    "unknown option {other:?} for start. Run `gsr-broadcastbox start --help`."
                ));
            }
            other => {
                if positional_key.is_some() {
                    return Err(format!("unexpected extra argument {other:?}"));
                }
                positional_key = Some(other.to_string());
            }
        }
        index += 1;
    }

    if let Some(key) = positional_key {
        if options.stream_key.is_some() {
            return Err("the stream key was given twice".to_string());
        }
        options.stream_key = Some(key);
    }
    Ok(options)
}

/// Parse `daemon --pending <path>`.
fn parse_pending(args: &[String]) -> Result<PathBuf, String> {
    let mut pending: Option<PathBuf> = None;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--pending" | "-p" => {
                pending = Some(PathBuf::from(next_value(args, &mut index, "--pending")?));
            }
            other => {
                return Err(format!(
                    "unknown option {other:?} for daemon. This command is internal."
                ));
            }
        }
        index += 1;
    }

    pending.ok_or_else(|| "daemon requires --pending <path>".to_string())
}

/// Read the value after an option.
fn next_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, String> {
    *index += 1;
    args.get(*index)
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{flag} needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn positional_and_flag_keys_are_parsed() {
        let options = parse_start(&args(&["alice", "--teamspeak", "--no-tray"])).unwrap();
        assert_eq!(options.stream_key.as_deref(), Some("alice"));
        assert_eq!(options.teamspeak, Some(true));
        assert_eq!(options.tray, Some(false));
        assert!(options.quality.is_none());

        let options = parse_start(&args(&["--key", "bob", "--quality", "ultra"])).unwrap();
        assert_eq!(options.stream_key.as_deref(), Some("bob"));
        assert_eq!(options.quality.as_deref(), Some("ultra"));
    }

    #[test]
    fn a_repeated_key_is_rejected() {
        let error = parse_start(&args(&["alice", "--key", "bob"])).unwrap_err();
        assert!(error.contains("twice"));
    }

    #[test]
    fn option_values_must_not_be_empty() {
        let error = parse_start(&args(&["--quality", ""])).unwrap_err();
        assert!(error.contains("--quality"));
    }

    #[test]
    fn unknown_options_are_rejected() {
        let error = parse_start(&args(&["--loud"])).unwrap_err();
        assert!(error.contains("--loud"));
    }

    #[test]
    fn daemon_requires_a_pending_file() {
        assert!(parse_pending(&args(&[])).is_err());
        let path = parse_pending(&args(&["--pending", "/tmp/pending.json"])).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/pending.json"));
    }
}
