//! Monitor detection and the `current` monitor resolution.
//!
//! GPU Screen Recorder can list monitors itself. When it cannot, the usual compositor
//! tools are tried and the DRM connectors are used as a last resort.

use std::process::Command;

/// A connected output with an optional short description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Monitor {
    pub name: String,
    /// Resolution and position, for example `1920x1080 at 1920,0`; empty when unknown.
    pub detail: String,
}

impl Monitor {
    fn new(name: &str) -> Monitor {
        Monitor {
            name: name.to_string(),
            detail: String::new(),
        }
    }
}

/// Run `program` with `args`, returning its stdout when it succeeds.
fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Compose a detail string from optional mode and position parts.
fn monitor_detail(mode: &str, position: &str) -> String {
    match (mode.is_empty(), position.is_empty()) {
        (false, false) => format!("{mode} at {position}"),
        (false, true) => mode.to_string(),
        (true, false) => format!("at {position}"),
        (true, true) => String::new(),
    }
}

/// Parse `gpu-screen-recorder --list-monitors`, which prints `NAME|RESOLUTION` per line.
fn monitors_from_gsr(text: &str) -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (name, detail) = line
            .split_once('|')
            .map(|(name, detail)| (name.trim(), detail.trim()))
            .unwrap_or((line, ""));
        if name.is_empty() || out.iter().any(|monitor| monitor.name == name) {
            continue;
        }
        out.push(Monitor {
            name: name.to_string(),
            detail: detail.to_string(),
        });
    }
    out
}

/// Parse one `wl_output` block of `wayland-info` output.
fn monitor_from_wayland_info_block(lines: &[&str]) -> Monitor {
    let mut monitor = Monitor::new("");
    let mut position = String::new();

    for (index, line) in lines.iter().enumerate() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            monitor.name = value.trim().trim_matches('\'').trim().to_string();
        } else if let Some(rest) = line.strip_prefix("x:") {
            let parts: Vec<&str> = rest.split(',').collect();
            let x = parts.first().map(|part| part.trim()).unwrap_or("");
            let y = parts
                .get(1)
                .and_then(|part| part.trim().strip_prefix("y:"))
                .map(str::trim)
                .unwrap_or("");
            if !x.is_empty() && !y.is_empty() {
                position = format!("{x},{y}");
            }
        } else if let Some(rest) = line.strip_prefix("width:") {
            let current = lines
                .get(index + 1)
                .map(|flags| flags.contains("flags:") && flags.contains("current"))
                .unwrap_or(false);
            if current {
                let width = rest.split_whitespace().next().unwrap_or("");
                let height = line
                    .split("height:")
                    .nth(1)
                    .and_then(|height| height.split_whitespace().next())
                    .unwrap_or("");
                if !width.is_empty() && !height.is_empty() {
                    monitor.detail = monitor_detail(&format!("{width}x{height}"), &position);
                }
            }
        }
    }

    if !position.is_empty() && monitor.detail.is_empty() {
        monitor.detail = monitor_detail("", &position);
    }
    monitor
}

fn push_wayland_info_block(block: &[&str], out: &mut Vec<Monitor>) {
    if block.is_empty() {
        return;
    }
    let monitor = monitor_from_wayland_info_block(block);
    if !monitor.name.is_empty() && !out.contains(&monitor) {
        out.push(monitor);
    }
}

/// wl_output entries from `wayland-info`, one block per monitor.
fn monitors_from_wayland_info(text: &str) -> Vec<Monitor> {
    let mut out: Vec<Monitor> = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    let mut in_output = false;

    for line in text.lines() {
        if line.starts_with("interface:") {
            if in_output {
                push_wayland_info_block(&block, &mut out);
            }
            block.clear();
            in_output = line.contains("'wl_output'");
            continue;
        }
        if in_output {
            block.push(line);
        }
    }
    if in_output {
        push_wayland_info_block(&block, &mut out);
    }
    out
}

/// Monitor names from the JSON listings of `wlr-randr --json`, `hyprctl monitors -j` and `swaymsg -t get_outputs`.
fn monitors_from_json(text: &str) -> Vec<Monitor> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("name").and_then(|name| name.as_str()))
                .map(Monitor::new)
                .collect()
        })
        .unwrap_or_default()
}

/// Remove ANSI color escapes, which kscreen-doctor uses in its output.
fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Connected outputs from `kscreen-doctor -o`.
fn monitors_from_kscreen(text: &str) -> Vec<Monitor> {
    let clean = strip_ansi(text);
    let lines: Vec<&str> = clean.lines().collect();

    let mut out: Vec<Monitor> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Some(rest) = line.trim().strip_prefix("Output:") else {
            continue;
        };
        let Some(name) = rest.split_whitespace().nth(1) else {
            continue;
        };
        let connected = lines[index + 1..]
            .iter()
            .take(3)
            .any(|candidate| candidate.trim() == "connected");
        if !connected || out.iter().any(|monitor| monitor.name == name) {
            continue;
        }

        let mut monitor = Monitor::new(name);
        if let Some(geometry) = lines[index + 1..]
            .iter()
            .take(12)
            .find(|candidate| candidate.trim_start().starts_with("Geometry:"))
        {
            let parts: Vec<&str> = geometry
                .trim_start()
                .trim_start_matches("Geometry:")
                .split_whitespace()
                .collect();
            if parts.len() >= 2 {
                monitor.detail = monitor_detail(parts[1], parts[0]);
            }
        }
        out.push(monitor);
    }
    out
}

/// Connected connector names from `/sys/class/drm`, for example `card1-DP-1` becomes `DP-1`.
fn monitors_from_drm() -> Vec<Monitor> {
    let Ok(entries) = std::fs::read_dir("/sys/class/drm") else {
        return Vec::new();
    };
    let mut out: Vec<Monitor> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        if status.trim() != "connected" {
            continue;
        }
        let Some((_, connector)) = name.split_once('-') else {
            continue;
        };
        if connector.is_empty() || out.iter().any(|monitor| monitor.name == connector) {
            continue;
        }
        out.push(Monitor::new(connector));
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Monitors known to the session.
///
/// GPU Screen Recorder is asked first, then the compositor tools, then the DRM connectors.
pub fn detect_monitors() -> Vec<Monitor> {
    type Source = (
        &'static str,
        &'static [&'static str],
        fn(&str) -> Vec<Monitor>,
    );

    let attempts: &[Source] = &[
        (
            "gpu-screen-recorder",
            &["--list-monitors"],
            monitors_from_gsr,
        ),
        ("wayland-info", &[], monitors_from_wayland_info),
        ("wlr-randr", &["--json"], monitors_from_json),
        ("hyprctl", &["monitors", "-j"], monitors_from_json),
        ("swaymsg", &["-t", "get_outputs"], monitors_from_json),
        ("kscreen-doctor", &["-o"], monitors_from_kscreen),
    ];
    for (program, args, parse) in attempts {
        if let Some(stdout) = command_stdout(program, args) {
            let monitors = parse(&stdout);
            if !monitors.is_empty() {
                return monitors;
            }
        }
    }
    monitors_from_drm()
}

/// The output the active window, workspace or cursor is on.
pub fn active_monitor() -> Option<String> {
    for program in ["qdbus6", "qdbus"] {
        if let Some(output) = command_stdout(
            program,
            &["org.kde.KWin", "/KWin", "org.kde.KWin.activeOutputName"],
        ) {
            let name = output.trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }

    if let Some(output) = command_stdout("hyprctl", &["activeworkspace", "-j"]) {
        if let Some(name) = json_string_field(&output, "monitor") {
            return Some(name);
        }
    }

    if let Some(output) = command_stdout("swaymsg", &["-t", "get_workspaces"]) {
        if let Some(name) = focused_workspace_output(&output) {
            return Some(name);
        }
    }

    active_monitor_x11()
}

/// X11 fallback: the monitor under the cursor, else the primary output.
fn active_monitor_x11() -> Option<String> {
    let point = command_stdout("xdotool", &["getmouselocation", "--shell"])
        .and_then(|output| parse_xdotool_location(&output));
    let layout = command_stdout("xrandr", &["--query"])?;
    monitor_from_xrandr(&layout, point)
}

fn parse_xdotool_location(text: &str) -> Option<(i64, i64)> {
    let mut x = None;
    let mut y = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("X=") {
            x = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("Y=") {
            y = value.trim().parse().ok();
        }
    }
    Some((x?, y?))
}

/// Find the connected output containing `point`, or the primary output when no point is known.
fn monitor_from_xrandr(text: &str, point: Option<(i64, i64)>) -> Option<String> {
    let mut primary = None;
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(name) = parts.next() else {
            continue;
        };
        if parts.next() != Some("connected") {
            continue;
        }
        let is_primary = line.split_whitespace().any(|part| part == "primary");
        let geometry = line.split_whitespace().find_map(parse_xrandr_geometry);
        if is_primary && primary.is_none() {
            primary = Some(name.to_string());
        }
        let (Some((width, height, x, y)), Some((cursor_x, cursor_y))) = (geometry, point) else {
            continue;
        };
        if cursor_x >= x && cursor_x < x + width && cursor_y >= y && cursor_y < y + height {
            return Some(name.to_string());
        }
    }
    primary
}

/// Parse a geometry token such as `1920x1080+0+0` into width, height, x, y.
fn parse_xrandr_geometry(token: &str) -> Option<(i64, i64, i64, i64)> {
    let (mode, rest) = token.split_once('+')?;
    let (x, y) = rest.split_once('+')?;
    let (width, height) = mode.split_once('x')?;
    Some((
        width.parse().ok()?,
        height.parse().ok()?,
        x.parse().ok()?,
        y.parse().ok()?,
    ))
}

/// Read a top level string field from a JSON object.
fn json_string_field(text: &str, field: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let found = value.get(field)?.as_str()?.trim();
    if found.is_empty() {
        None
    } else {
        Some(found.to_string())
    }
}

/// Output of the focused workspace in `swaymsg -t get_workspaces` output.
fn focused_workspace_output(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let workspaces = value.as_array()?;
    workspaces
        .iter()
        .find(|workspace| workspace.get("focused").and_then(|f| f.as_bool()) == Some(true))
        .and_then(|workspace| workspace.get("output"))
        .and_then(|output| output.as_str())
        .map(str::trim)
        .filter(|output| !output.is_empty())
        .map(str::to_string)
}

/// Resolve the configured monitor to a value gpu-screen-recorder accepts.
///
/// `current` follows the active monitor, then the first detected monitor, then `screen`.
/// A named monitor must be connected when detection worked.
pub fn choose_monitor(
    configured: &str,
    detected: &[Monitor],
    active: Option<&str>,
) -> Result<String, String> {
    let configured = configured.trim();
    if configured.is_empty() || configured == "current" || configured == "focused" {
        if let Some(active) = active.map(str::trim).filter(|name| !name.is_empty()) {
            return Ok(active.to_string());
        }
        if let Some(monitor) = detected.first() {
            return Ok(monitor.name.clone());
        }
        return Ok("screen".to_string());
    }

    if detected.is_empty() || detected.iter().any(|monitor| monitor.name == configured) {
        return Ok(configured.to_string());
    }

    let available: Vec<&str> = detected
        .iter()
        .map(|monitor| monitor.name.as_str())
        .collect();
    Err(format!(
        "monitor {configured:?} is not connected (available: {}). Use \"current\" or pick one with `gsr-broadcastbox config`.",
        available.join(", ")
    ))
}

/// Values shown by the monitor picker: `current` followed by every detected output.
pub fn monitor_choices(detected: &[Monitor], configured: &str) -> Vec<String> {
    let mut choices = vec!["current".to_string()];
    for monitor in detected {
        if !choices.contains(&monitor.name) {
            choices.push(monitor.name.clone());
        }
    }
    let configured = configured.trim();
    if !configured.is_empty()
        && configured != "current"
        && configured != "focused"
        && !choices.iter().any(|choice| choice == configured)
    {
        choices.push(configured.to_string());
    }
    choices
}

/// Describe a monitor for the TUI.
pub fn describe_monitor(name: &str, detected: &[Monitor]) -> String {
    if name == "current" {
        return "Current Monitor".to_string();
    }
    match detected.iter().find(|monitor| monitor.name == name) {
        Some(monitor) if !monitor.detail.is_empty() => {
            format!("{} ({})", monitor.name, monitor.detail)
        }
        _ => name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: &str, detail: &str) -> Monitor {
        Monitor {
            name: name.to_string(),
            detail: detail.to_string(),
        }
    }

    #[test]
    fn gsr_monitors_are_parsed() {
        let text = "DP-1|1920x1080\nDP-3|2560x1440\n";
        assert_eq!(
            monitors_from_gsr(text),
            vec![monitor("DP-1", "1920x1080"), monitor("DP-3", "2560x1440")]
        );
        assert_eq!(monitors_from_gsr("DP-1\n"), vec![monitor("DP-1", "")]);
        assert!(monitors_from_gsr("").is_empty());
    }

    #[test]
    fn wayland_info_monitors_are_parsed() {
        let text = r#"interface: 'wl_output', version: 4, name: 65
    name: DP-1
    x: 1920, y: 0, scale: 1,
    mode:
        width: 1920 px, height: 1080 px, refresh: 165.001 Hz,
        flags: current
interface: 'wl_output', version: 4, name: 66
    name: DP-2
    x: 0, y: 0, scale: 1,
    mode:
        width: 1920 px, height: 1080 px, refresh: 60.000 Hz,
        flags: current
"#;
        assert_eq!(
            monitors_from_wayland_info(text),
            vec![
                monitor("DP-1", "1920x1080 at 1920,0"),
                monitor("DP-2", "1920x1080 at 0,0"),
            ]
        );
    }

    #[test]
    fn json_monitors_are_parsed() {
        let hyprctl = r#"[{"id":0,"name":"DP-1","description":"Dell","monitor":"DP-1"}]"#;
        assert_eq!(monitors_from_json(hyprctl), vec![monitor("DP-1", "")]);

        let wlr_randr = r#"[{"name":"eDP-1"},{"name":"HDMI-A-1"}]"#;
        assert_eq!(
            monitors_from_json(wlr_randr),
            vec![monitor("eDP-1", ""), monitor("HDMI-A-1", "")]
        );
        assert!(monitors_from_json("not json").is_empty());
    }

    #[test]
    fn kscreen_monitors_are_parsed_and_colors_stripped() {
        let text = "\u{1b}[01;32mOutput: \u{1b}[01;32m1 \u{1b}[01;32mDP-2 \u{1b}[01;32m0x1234\n\u{1b}[01;32m\tconnected\n\tGeometry: 1920,0 1920x1080\n";
        assert_eq!(
            monitors_from_kscreen(text),
            vec![monitor("DP-2", "1920x1080 at 1920,0")]
        );
    }

    #[test]
    fn xrandr_geometry_and_primary_are_understood() {
        let text = "eDP-1 connected 1920x1080+0+0 (normal left inverted right x axis y axis)\nDP-1 connected primary 2560x1440+1920+0 (normal left inverted right x axis y axis)\n";
        assert_eq!(
            monitor_from_xrandr(text, Some((100, 100))),
            Some("eDP-1".to_string())
        );
        assert_eq!(
            monitor_from_xrandr(text, Some((2000, 100))),
            Some("DP-1".to_string())
        );
        assert_eq!(monitor_from_xrandr(text, None), Some("DP-1".to_string()));
        assert_eq!(
            parse_xrandr_geometry("2560x1440+1920+0"),
            Some((2560, 1440, 1920, 0))
        );
    }

    #[test]
    fn hyprland_active_monitor_is_parsed() {
        let text = r#"{"id":1,"name":"1","monitor":"DP-2","windows":2}"#;
        assert_eq!(json_string_field(text, "monitor"), Some("DP-2".to_string()));
        assert_eq!(json_string_field("{}", "monitor"), None);
        assert_eq!(json_string_field(r#"{"monitor":""}"#, "monitor"), None);
        assert_eq!(json_string_field("not json", "monitor"), None);
    }

    #[test]
    fn current_monitor_resolves_from_active_then_detected_then_screen() {
        let detected = vec![monitor("DP-1", ""), monitor("DP-3", "")];
        assert_eq!(
            choose_monitor("current", &detected, Some("DP-3")).unwrap(),
            "DP-3"
        );
        assert_eq!(choose_monitor("current", &detected, None).unwrap(), "DP-1");
        assert_eq!(choose_monitor("current", &[], None).unwrap(), "screen");
    }

    #[test]
    fn a_named_monitor_must_be_connected_when_detection_works() {
        let detected = vec![monitor("DP-1", ""), monitor("DP-3", "")];
        assert_eq!(choose_monitor("DP-3", &detected, None).unwrap(), "DP-3");
        let error = choose_monitor("DP-2", &detected, None).unwrap_err();
        assert!(error.contains("DP-2"));
        assert!(error.contains("DP-1"));
        assert_eq!(choose_monitor("DP-2", &[], None).unwrap(), "DP-2");
    }

    #[test]
    fn monitor_choices_start_with_current_and_keep_unknown_values() {
        let detected = vec![monitor("DP-1", ""), monitor("DP-3", "")];
        assert_eq!(
            monitor_choices(&detected, "current"),
            vec!["current", "DP-1", "DP-3"]
        );
        assert_eq!(
            monitor_choices(&detected, "DP-2"),
            vec!["current", "DP-1", "DP-3", "DP-2"]
        );
    }
}
