# gsr-broadcastbox

A WHIP broadcaster built around [gpu-screen-recorder](https://git.dec05eba.com/gpu-screen-recorder).

It replaces the `stream.sh` and `stream-tray.py` helpers with one Rust binary:

- `gsr-broadcastbox start` starts a detached stream and a tray indicator,
- `gsr-broadcastbox config` opens a terminal UI for every setting,
- the monitor picker offers `Current Monitor` plus every detected output,
- TeamSpeak mode stops the stream when TeamSpeak exits,
- the stream key is passed through `GSR_AUTH`, never through the process arguments.

## Requirements

- Linux with a GPU supported by gpu-screen-recorder.
- `gpu-screen-recorder` with WHIP support in `PATH` (`-c whip`). Monitor listing uses `gpu-screen-recorder --list-monitors`.
- A WHIP ingest endpoint and a stream key.
- Optional: a StatusNotifierItem tray host for the indicator.
- Optional: `notify-send` for failure notifications.
- Optional detection tools: `qdbus6`/`qdbus` on KWin, `hyprctl` on Hyprland, `swaymsg` on Sway, `xdotool` and `xrandr` on X11.

Without a compositor tool, `Current Monitor` falls back to the first monitor reported by gpu-screen-recorder, then to `screen`.

## Install

Root install, the default:

```sh
curl -fsSL https://raw.githubusercontent.com/xamionex/gsr-broadcastbox/main/install.sh | sh
```

Installs the binary to `/usr/local/bin/gsr-broadcastbox` and asks for sudo when needed.

User install:

```sh
curl -fsSL https://raw.githubusercontent.com/xamionex/gsr-broadcastbox/main/install.sh | sh -s -- --user
```

Installs to `~/.local/bin/gsr-broadcastbox`. Make sure that directory is on `PATH`.

From a checkout:

```sh
cargo build --release
install -m 755 target/release/gsr-broadcastbox ~/.local/bin/gsr-broadcastbox
```

## Quick start

```sh
gsr-broadcastbox config    # pick the monitor, quality and stream key
gsr-broadcastbox start     # detached stream with a tray indicator
gsr-broadcastbox status    # PID, watch URL, monitor, uptime, log path
gsr-broadcastbox stop
```

The first run creates `~/.config/gsr-broadcastbox/config.toml` with commented defaults. The file is never overwritten afterwards.

An empty `stream_key` uses your username. `monitor = "current"` follows the output with the active window, workspace or cursor when the stream starts.

`watch` uses software decoding with four threads by default for the lowest latency. Set `watch_hardware_decode = true` to use mpv hardware decoding instead. Hardware decoding uses less CPU but measured higher latency. Playback starts muted. Add mpv flags with `watch_mpv_args`, or repeat `--mpv-arg` for a single run. Later flags win, so `gsr-broadcastbox watch --mpv-arg=--mute=no` restores audio.

## Commands

| Command | Effect |
| --- | --- |
| `start [key] [options]` | Start a detached stream. Prints the watch URL and log path. |
| `watch [key] [options]` | Play the stream in mpv. Muted by default. |
| `stop` | Stop the stream, the daemon and the tray. Also cleans up the old `stream.sh` helpers. |
| `restart [options]` | Stop and start again. |
| `status` | Print stream state. Exits with 1 when nothing is running. |
| `monitors` | List detected monitors and mark the active one. |
| `config` | Open the terminal config editor. |
| `help`, `version` | Show help or the version. |

Start options:

| Option | Effect |
| --- | --- |
| `-k`, `--key <key>` | Stream key for this run. A positional key is also accepted. |
| `-q`, `--quality <preset>` | `medium`, `high`, `very_high` or `ultra` for QP/VBR modes. |
| `-m`, `--monitor <name>` | Monitor name, or `current`. |
| `-t`, `--teamspeak` | Enable TeamSpeak mode for this run. |
| `--no-teamspeak` | Disable TeamSpeak mode for this run. |
| `--tray` / `--no-tray` | Show or hide the tray indicator. |
| `-f`, `--foreground` | Keep the stream attached to the terminal instead of detaching. |

Watch options:

| Option | Effect |
| --- | --- |
| `-k`, `--key <key>` | Stream key for this run. A positional key is also accepted. |
| `--mpv-arg <flag>` | Extra mpv flag for this run. Repeatable, and `--mpv-arg=<flag>` also works. |

Environment overrides for `start` and `restart`:

```sh
STREAM_KEY=alice STREAM_QUALITY=ultra STREAM_MONITOR=DP-1 TEAMSPEAK_MODE=1 \
WHIP_URL=https://example.test/api/whip WATCH_BASE_URL=https://example.test \
gsr-broadcastbox start
```

Command line flags override the environment, and the environment overrides the config file.

## Configuration

`~/.config/gsr-broadcastbox/config.toml`:

| Key | Default | Meaning |
| --- | --- | --- |
| `stream_key` | `""` | WHIP stream key. Empty means the current username. |
| `whip_url` | `https://broadcast.petar.cc/api/whip` | WHIP ingest endpoint. |
| `watch_base_url` | `https://broadcast.petar.cc` | Watch page base, the key is appended. |
| `watch_hardware_decode` | `false` | Use mpv hardware decoding. Software decoding has lower latency. |
| `watch_mpv_args` | `["--mute=yes"]` | Extra mpv flags, one entry per flag. Audio is muted by default. |
| `monitor` | `current` | Monitor name such as `DP-1`, or `current`. |
| `quality` | `high` | `medium`, `high`, `very_high` or `ultra`. Used in QP and VBR modes. |
| `video_bitrate` | `8000` | kbps, used when `bitrate_mode` is `cbr`. |
| `bitrate_mode` | `cbr` | `auto`, `qp`, `vbr` or `cbr`. |
| `fps` | `60` | Frames per second, 1 to 480. |
| `resolution` | `1920x1080` | Output size limit. `0x0` keeps the source size. |
| `codec` | `h264` | `auto`, `h264`, `hevc`, `av1`, `vp8` or `vp9`. |
| `color_range` | `limited` | `limited` or `full`. |
| `audio_codec` | `opus` | `opus`, `aac`, `flac` or `mp3`. |
| `audio_bitrate` | `128` | kbps, up to 512. |
| `audio_sources` | application list | Entries are joined with `|` for `-a`. See below. |
| `teamspeak` | `false` | Refuse to start without TeamSpeak and stop when it exits. |
| `teamspeak_process` | `TeamSpeak` | Process name checked by TeamSpeak mode. |
| `tray` | `true` | Show the tray indicator while streaming. |
| `notifications` | `true` | Notify on failures and unexpected stops. |
| `extra_gsr_args` | `[]` | Extra gpu-screen-recorder arguments, appended as-is. |

The default audio sources are:

```toml
audio_sources = [
    "app-inverse:TeamSpeak",
    "app-inverse:discord",
    "app-inverse:plasmashell",
    "app-inverse:pipewire",
    "app-inverse:linuxsoundboard.local_playback",
    "app-inverse:linuxsoundboard.virtual_mic_feeder",
]
```

Remove sources you do not use. An empty list omits `-a` entirely.

## Config editor

`gsr-broadcastbox config` needs an interactive terminal.

- up/down or `j`/`k` select, page up/down and home/end jump,
- enter or space toggles a boolean, opens a text or list editor, or opens the monitor picker,
- left/right cycles quality, bitrate mode, codec, color range, audio codec and the monitor,
- `s` validates and saves, `q` quits (with a save/discard prompt when there are unsaved changes),
- `ctrl+c` quits at once without saving,
- `r` refreshes monitor detection.

The monitor picker starts with `Current Monitor`, which follows the active output when a stream starts, followed by every detected output. The active output is marked, and an output that is configured but currently disconnected is kept in the list so selection never silently drops it.

In the list editor, enter edits an entry, `a` adds one, `d` deletes one, and esc returns to the form.

Saving validates the whole document before writing and keeps comments and formatting through `toml_edit`.

## Runtime files

| File | Purpose |
| --- | --- |
| `~/.config/gsr-broadcastbox/config.toml` | Configuration, mode 0600. |
| `$XDG_RUNTIME_DIR/gsr-broadcastbox/stream.json` | Running stream state, mode 0600. |
| `$XDG_RUNTIME_DIR/gsr-broadcastbox/pending.json` | Launch options with the stream key, mode 0600, removed as soon as the daemon reads it. |
| `$XDG_STATE_HOME/gsr-broadcastbox/stream.log` | Daemon and gpu-screen-recorder log. |

`XDG_RUNTIME_DIR` defaults to `/tmp/gsr-broadcastbox-<uid>` when unset. `XDG_STATE_HOME` defaults to `~/.local/state`.

The stream key is never placed in the gpu-screen-recorder arguments. It is passed in the `GSR_AUTH` environment variable, and the handoff file that holds it is readable only by your user.

## Architecture

- The CLI validates the config, resolves the monitor, writes a pending launch file and starts a detached daemon in its own session.
- The daemon consumes the pending file, spawns gpu-screen-recorder, writes the running state, then owns the tray and the TeamSpeak watcher.
- Stopping the stream from the tray, `gsr-broadcastbox stop`, or a signal goes through the same shutdown path: stop the child, stop the watcher and tray, remove the state file.

## Migration from the old scripts

`gsr-broadcastbox stop` also stops processes and removes state files left by `stream.sh` and `stream-tray.py`:

- `/tmp/gsr-whip-stream.pid`
- `/tmp/gsr-whip-stream.key`
- `/tmp/gsr-whip-stream-tray.pid`
- `/tmp/gsr-whip-stream-ts.pid`

`start` refuses to run while an old `stream.sh` stream is still active.

## Development

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets
cargo build --release
```

Pushing a tag such as `v0.1.0` builds and publishes the Linux binary through `.github/workflows/build.yml`. The workflow uses `cargo-zigbuild` against glibc 2.28 for compatibility with older distributions and SteamOS.
