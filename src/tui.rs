//! Interactive config editor.
//!
//! A ratatui form over the TOML document: booleans toggle, choices cycle, text and lists are edited inline,
//! and the monitor row opens a picker with "Current Monitor" followed by every detected output.
//! Saving rewrites the config through `toml_edit`, so comments and formatting survive.
//! The editor runs as an action: it exits after saving or discarding and never starts a stream.
//! Ctrl+C always leaves without saving.

use std::io::IsTerminal;
use std::path::PathBuf;

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use toml_edit::{Array, DocumentMut, Item, Value};

use crate::config::{self, Config};
use crate::config_file;
use crate::monitors::{self, Monitor};

/// One editable setting in the form.
struct Field {
    key: &'static str,
    label: &'static str,
    help: &'static str,
    section: &'static str,
}

/// The settings shown by the editor, in display order.
const FIELDS: &[Field] = &[
    Field {
        key: "stream_key",
        label: "Stream key",
        help: "Empty uses your username; it is stored in this file",
        section: "Connection",
    },
    Field {
        key: "whip_url",
        label: "WHIP URL",
        help: "Ingest endpoint passed to gpu-screen-recorder",
        section: "Connection",
    },
    Field {
        key: "watch_base_url",
        label: "Watch base URL",
        help: "The stream key is appended to this page",
        section: "Connection",
    },
    Field {
        key: "watch_hardware_decode",
        label: "Watch hardware decoding",
        help: "Off uses software decoding with four threads for lower latency",
        section: "Watch",
    },
    Field {
        key: "watch_mpv_args",
        label: "Watch mpv args",
        help: "Extra mpv flags, one per entry; the default mutes audio",
        section: "Watch",
    },
    Field {
        key: "monitor",
        label: "Monitor",
        help: "Current Monitor follows the active output when a stream starts",
        section: "Video",
    },
    Field {
        key: "quality",
        label: "Quality",
        help: "Preset used in QP and VBR modes",
        section: "Video",
    },
    Field {
        key: "bitrate_mode",
        label: "Bitrate mode",
        help: "auto, qp, vbr or cbr",
        section: "Video",
    },
    Field {
        key: "video_bitrate",
        label: "Video bitrate",
        help: "kbps, used when bitrate_mode is cbr",
        section: "Video",
    },
    Field {
        key: "fps",
        label: "FPS",
        help: "Frames per second",
        section: "Video",
    },
    Field {
        key: "resolution",
        label: "Resolution",
        help: "Output size limit such as 1920x1080; 0x0 keeps the source size",
        section: "Video",
    },
    Field {
        key: "codec",
        label: "Codec",
        help: "Video codec",
        section: "Video",
    },
    Field {
        key: "color_range",
        label: "Color range",
        help: "limited or full",
        section: "Video",
    },
    Field {
        key: "audio_codec",
        label: "Audio codec",
        help: "Audio codec",
        section: "Audio",
    },
    Field {
        key: "audio_bitrate",
        label: "Audio bitrate",
        help: "kbps",
        section: "Audio",
    },
    Field {
        key: "audio_sources",
        label: "Audio sources",
        help: "Application sources mixed into the stream, for example app-inverse:discord",
        section: "Audio",
    },
    Field {
        key: "teamspeak",
        label: "TeamSpeak mode",
        help: "Refuse to start without TeamSpeak and stop when it exits",
        section: "Application",
    },
    Field {
        key: "teamspeak_process",
        label: "TeamSpeak process",
        help: "Process name checked by TeamSpeak mode",
        section: "Application",
    },
    Field {
        key: "tray",
        label: "Tray indicator",
        help: "Show a tray icon while streaming",
        section: "Application",
    },
    Field {
        key: "notifications",
        label: "Notifications",
        help: "Notify on failures and unexpected stops",
        section: "Application",
    },
    Field {
        key: "extra_gsr_args",
        label: "Extra gsr args",
        help: "Extra gpu-screen-recorder arguments, appended as-is",
        section: "Application",
    },
];

/// Kind of a setting, taken from the value in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Bool,
    Integer,
    Text,
    List,
}

impl Kind {
    fn of(item: &Item) -> Kind {
        if item.is_bool() {
            Kind::Bool
        } else if item.is_integer() {
            Kind::Integer
        } else if item.is_array() {
            Kind::List
        } else {
            Kind::Text
        }
    }
}

/// A row in the form: a section heading or a selectable field.
enum Entry {
    Section(&'static str),
    Field(usize),
}

/// Values a setting cycles through.
fn choices_for(key: &str) -> &'static [&'static str] {
    match key {
        "quality" => config::QUALITIES,
        "codec" => config::CODECS,
        "bitrate_mode" => config::BITRATE_MODES,
        "color_range" => config::COLOR_RANGES,
        "audio_codec" => config::AUDIO_CODECS,
        _ => &[],
    }
}

/// Accepted range for an integer setting.
fn integer_range(key: &str) -> Option<(i64, i64)> {
    match key {
        "fps" => Some((1, 480)),
        "video_bitrate" => Some((1, 200_000)),
        "audio_bitrate" => Some((0, 512)),
        _ => None,
    }
}

/// Example entry shown while editing a list setting.
fn list_example(key: &str) -> &'static str {
    match key {
        "audio_sources" => "app-inverse:discord",
        "watch_mpv_args" => "--mute=yes",
        "extra_gsr_args" => "-keyint 1",
        _ => "",
    }
}

/// Inline text editing state.
struct TextEdit {
    key: String,
    /// Index of the list entry being edited, when a list entry is edited.
    entry: Option<usize>,
    /// True when the edit inserts a new entry at `entry` instead of replacing one.
    adding: bool,
    buffer: String,
    /// Cursor position as a character index into `buffer`.
    cursor: usize,
}

/// Editor mode.
enum Mode {
    Form,
    EditText(TextEdit),
    EditList,
    PickMonitor,
}

struct Editor {
    doc: DocumentMut,
    path: PathBuf,
    mode: Mode,
    /// Set by Ctrl+C: leave without saving.
    cancelled: bool,
    /// Set after a successful save, for a message once the terminal is restored.
    saved: bool,
    selection: usize,
    list_selection: usize,
    picker_selection: usize,
    dirty: bool,
    confirm_quit: bool,
    status: String,
    /// Outputs detected when the editor opened or the last time detection was refreshed.
    monitors: Vec<Monitor>,
    /// Output the editor's own terminal is on, when the compositor knows.
    active_monitor: Option<String>,
}

impl Editor {
    fn new(
        path: PathBuf,
        mut doc: DocumentMut,
        monitors: Vec<Monitor>,
        active_monitor: Option<String>,
    ) -> Editor {
        // `focused` is an old alias for `current`; show it as the picker's first entry.
        if let Some(value) = doc.get_mut("monitor").and_then(Item::as_value_mut) {
            if value.as_str() == Some("focused") {
                *value = Value::from("current");
            }
        }

        Editor {
            doc,
            path,
            mode: Mode::Form,
            cancelled: false,
            saved: false,
            selection: 0,
            list_selection: 0,
            picker_selection: 0,
            dirty: false,
            confirm_quit: false,
            status: String::new(),
            monitors,
            active_monitor,
        }
    }

    // ---- document access ----

    fn kind(&self, key: &str) -> Kind {
        self.doc.get(key).map(Kind::of).unwrap_or(Kind::Text)
    }

    fn get_bool(&self, key: &str) -> bool {
        self.doc.get(key).and_then(Item::as_bool).unwrap_or(false)
    }

    fn get_integer(&self, key: &str) -> i64 {
        self.doc.get(key).and_then(Item::as_integer).unwrap_or(0)
    }

    fn get_text(&self, key: &str) -> String {
        self.doc
            .get(key)
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn get_list(&self, key: &str) -> Vec<String> {
        self.doc
            .get(key)
            .and_then(Item::as_array)
            .map(|array| {
                array
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Replace the value of `key`, keeping the comments around it.
    fn set_value(&mut self, key: &str, new: Value) {
        match self.doc.get_mut(key).and_then(Item::as_value_mut) {
            Some(value) => {
                let decor = value.decor().clone();
                *value = new;
                *value.decor_mut() = decor;
            }
            None => {
                self.doc[key] = Item::Value(new);
            }
        }
        self.dirty = true;
    }

    fn set_list(&mut self, key: &str, items: &[String]) {
        let mut array = Array::new();
        for item in items {
            array.push(item.as_str());
        }
        self.set_value(key, Value::Array(array));
    }

    // ---- entries and selection ----

    fn entries(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        let mut section = "";
        for (index, field) in FIELDS.iter().enumerate() {
            if field.section != section {
                section = field.section;
                entries.push(Entry::Section(section));
            }
            entries.push(Entry::Field(index));
        }
        entries
    }

    /// Index into [`FIELDS`] for the selected row.
    fn selected_field_index(&self) -> usize {
        match self.entries().get(self.selection) {
            Some(Entry::Field(index)) => *index,
            _ => 0,
        }
    }

    fn selected_field(&self) -> &'static Field {
        &FIELDS[self.selected_field_index()]
    }

    /// Move the selection by `delta` rows, skipping section headings.
    fn move_selection(&mut self, delta: isize) {
        let entries = self.entries();
        let count = entries.len();
        if count == 0 {
            return;
        }
        let mut index = self.selection as isize + delta;
        while index >= 0 && (index as usize) < count {
            if matches!(entries[index as usize], Entry::Field(_)) {
                self.selection = index as usize;
                return;
            }
            index += delta.signum();
        }
        if delta < 0 {
            self.jump_to_first();
        } else {
            self.jump_to_last();
        }
    }

    fn jump_to_first(&mut self) {
        if let Some(index) = self
            .entries()
            .iter()
            .position(|entry| matches!(entry, Entry::Field(_)))
        {
            self.selection = index;
        }
    }

    fn jump_to_last(&mut self) {
        if let Some(index) = self
            .entries()
            .iter()
            .rposition(|entry| matches!(entry, Entry::Field(_)))
        {
            self.selection = index;
        }
    }

    /// Keep the cursor on a selectable row.
    fn normalize_selection(&mut self) {
        if matches!(self.entries().get(self.selection), Some(Entry::Field(_))) {
            return;
        }
        self.jump_to_first();
    }

    // ---- monitors ----

    fn refresh_monitors(&mut self) {
        self.monitors = monitors::detect_monitors();
        self.active_monitor = monitors::active_monitor();
    }

    fn monitor_choices(&self) -> Vec<String> {
        monitors::monitor_choices(&self.monitors, &self.get_text("monitor"))
    }

    fn monitor_label(&self, choice: &str) -> String {
        if choice == "current" {
            return match self.active_monitor.as_deref() {
                Some(active) if !active.is_empty() => {
                    format!("Current Monitor (follows {active})")
                }
                _ => "Current Monitor (first detected monitor)".to_string(),
            };
        }
        let mut label = monitors::describe_monitor(choice, &self.monitors);
        if self.active_monitor.as_deref() == Some(choice) {
            label.push_str("  [active]");
        }
        label
    }

    fn monitor_summary(&self) -> String {
        let names: Vec<&str> = self
            .monitors
            .iter()
            .map(|monitor| monitor.name.as_str())
            .collect();
        let detected = if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        };
        match self.active_monitor.as_deref() {
            Some(active) if !active.is_empty() => {
                format!("Active: {active} | Detected: {detected}")
            }
            _ => format!("Detected: {detected}"),
        }
    }

    fn open_monitor_picker(&mut self) {
        self.refresh_monitors();
        let choices = self.monitor_choices();
        let current = self.get_text("monitor");
        self.picker_selection = choices
            .iter()
            .position(|choice| *choice == current)
            .unwrap_or(0);
        self.mode = Mode::PickMonitor;
    }

    fn apply_monitor_choice(&mut self) {
        let choices = self.monitor_choices();
        if let Some(choice) = choices.get(self.picker_selection).cloned() {
            let label = self.monitor_label(&choice);
            self.set_value("monitor", Value::from(choice));
            self.status = format!("Monitor: {label}");
        }
        self.mode = Mode::Form;
    }

    fn step_monitor(&mut self, delta: i64) {
        let choices = self.monitor_choices();
        if choices.len() <= 1 {
            self.status = "No monitors detected".to_string();
            return;
        }
        let current = self.get_text("monitor");
        let index = choices
            .iter()
            .position(|choice| *choice == current)
            .unwrap_or(0) as i64;
        let len = choices.len() as i64;
        let next = (index + delta).rem_euclid(len) as usize;
        self.set_value("monitor", Value::from(choices[next].clone()));
    }

    // ---- actions ----

    /// Toggle a boolean, cycle a choice, or open the right editor.
    fn activate(&mut self) {
        let field = self.selected_field();
        if field.key == "monitor" {
            self.open_monitor_picker();
            return;
        }
        match self.kind(field.key) {
            Kind::Bool => {
                let value = self.get_bool(field.key);
                self.set_value(field.key, Value::from(!value));
            }
            Kind::List => {
                self.mode = Mode::EditList;
                self.list_selection = 0;
            }
            Kind::Text if !choices_for(field.key).is_empty() => self.cycle_choice(1),
            Kind::Integer | Kind::Text => self.start_text_edit(field.key),
        }
    }

    fn start_text_edit(&mut self, key: &str) {
        let buffer = if self.kind(key) == Kind::Integer {
            self.get_integer(key).to_string()
        } else {
            self.get_text(key)
        };
        self.mode = Mode::EditText(TextEdit {
            key: key.to_string(),
            entry: None,
            adding: false,
            cursor: buffer.chars().count(),
            buffer,
        });
    }

    /// Move a choice setting to its previous or next allowed value.
    fn cycle_choice(&mut self, delta: i64) {
        let field = self.selected_field();
        if field.key == "monitor" {
            self.step_monitor(delta);
            return;
        }
        let choices = choices_for(field.key);
        if choices.is_empty() {
            return;
        }
        let current = self.get_text(field.key);
        let index = choices
            .iter()
            .position(|choice| *choice == current)
            .unwrap_or(0) as i64;
        let len = choices.len() as i64;
        let next = (index + delta).rem_euclid(len) as usize;
        self.set_value(field.key, Value::from(choices[next]));
    }

    /// Validate and write the document. Returns true when it was saved.
    fn save(&mut self) -> bool {
        let text = self.doc.to_string();
        let config: Config = match toml::from_str(&text) {
            Ok(config) => config,
            Err(error) => {
                self.status = format!("Not saved: {error}");
                return false;
            }
        };
        let errors = config.validate();
        if let Some(error) = errors.first() {
            self.status = format!("Not saved: {error}");
            return false;
        }
        match config_file::save_document(&self.doc, &self.path) {
            Ok(()) => {
                self.dirty = false;
                self.saved = true;
                self.status = format!(
                    "Saved to {}. Restart a running stream to apply.",
                    self.path.display()
                );
                true
            }
            Err(error) => {
                self.status = format!("Save failed: {error}");
                false
            }
        }
    }

    // ---- input ----

    /// Handle one key press; returns `true` when the editor should exit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if key.kind != KeyEventKind::Press {
            return false;
        }

        // Ctrl+C leaves at once, whatever is open: no save and no confirm.
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.cancelled = true;
            return true;
        }

        if self.confirm_quit {
            self.confirm_quit = false;
            match key.code {
                KeyCode::Char('s') | KeyCode::Char('y') | KeyCode::Enter => return self.save(),
                KeyCode::Char('d') | KeyCode::Char('n') => return true,
                _ => return false,
            }
        }

        match self.mode {
            Mode::Form => return self.handle_form(key),
            Mode::EditList => self.handle_edit_list(key),
            Mode::EditText(_) => self.handle_edit_text(key),
            Mode::PickMonitor => self.handle_pick_monitor(key),
        }
        false
    }

    fn handle_form(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                if self.dirty {
                    self.confirm_quit = true;
                    false
                } else {
                    true
                }
            }
            KeyCode::Char('s') => {
                self.save();
                false
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(-1);
                false
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(1);
                false
            }
            KeyCode::PageUp => {
                self.move_selection(-5);
                false
            }
            KeyCode::PageDown => {
                self.move_selection(5);
                false
            }
            KeyCode::Home => {
                self.jump_to_first();
                false
            }
            KeyCode::End => {
                self.jump_to_last();
                false
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.activate();
                false
            }
            KeyCode::Left => {
                self.cycle_choice(-1);
                false
            }
            KeyCode::Right => {
                self.cycle_choice(1);
                false
            }
            KeyCode::Char('r') => {
                self.refresh_monitors();
                self.status = self.monitor_summary();
                false
            }
            _ => false,
        }
    }

    fn handle_pick_monitor(&mut self, key: KeyEvent) {
        let choices = self.monitor_choices();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.mode = Mode::Form,
            KeyCode::Up | KeyCode::Char('k') => {
                self.picker_selection = self.picker_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.picker_selection + 1 < choices.len() {
                    self.picker_selection += 1;
                }
            }
            KeyCode::Home => self.picker_selection = 0,
            KeyCode::End => {
                self.picker_selection = choices.len().saturating_sub(1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.apply_monitor_choice(),
            KeyCode::Char('r') => {
                self.refresh_monitors();
                let len = self.monitor_choices().len();
                self.picker_selection = self.picker_selection.min(len.saturating_sub(1));
                self.status = self.monitor_summary();
            }
            _ => {}
        }
    }

    fn handle_edit_text(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.cancel_edit();
                return;
            }
            KeyCode::Enter => {
                self.commit_edit();
                return;
            }
            _ => {}
        }

        let Mode::EditText(edit) = &mut self.mode else {
            return;
        };
        match key.code {
            KeyCode::Backspace => {
                if edit.cursor > 0 {
                    edit.cursor -= 1;
                    let byte = char_byte_index(&edit.buffer, edit.cursor);
                    edit.buffer.remove(byte);
                }
            }
            KeyCode::Delete => {
                if edit.cursor < edit.buffer.chars().count() {
                    let byte = char_byte_index(&edit.buffer, edit.cursor);
                    edit.buffer.remove(byte);
                }
            }
            KeyCode::Left => edit.cursor = edit.cursor.saturating_sub(1),
            KeyCode::Right => {
                edit.cursor = (edit.cursor + 1).min(edit.buffer.chars().count());
            }
            KeyCode::Home => edit.cursor = 0,
            KeyCode::End => edit.cursor = edit.buffer.chars().count(),
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let byte = char_byte_index(&edit.buffer, edit.cursor);
                edit.buffer.insert(byte, c);
                edit.cursor += 1;
            }
            _ => {}
        }
    }

    /// Leave the text editor without writing anything.
    fn cancel_edit(&mut self) {
        let target = match &self.mode {
            Mode::EditText(edit) if edit.entry.is_some() => Mode::EditList,
            _ => Mode::Form,
        };
        self.mode = target;
    }

    /// Write the text buffer back into the document, or into the edited list entry.
    fn commit_edit(&mut self) {
        let Mode::EditText(edit) = &self.mode else {
            return;
        };
        let key = edit.key.clone();
        let buffer = edit.buffer.clone();
        let entry = edit.entry;
        let adding = edit.adding;

        if let Some(index) = entry {
            let mut items = self.get_list(&key);
            if adding {
                let at = index.min(items.len());
                if buffer.trim().is_empty() {
                    self.status = format!("{key}: empty entry not added");
                } else {
                    items.insert(at, buffer);
                    self.set_list(&key, &items);
                    self.list_selection = at;
                }
            } else if index < items.len() {
                if buffer.trim().is_empty() {
                    self.status = format!("{key}: empty entry not saved");
                } else {
                    items[index] = buffer;
                    self.set_list(&key, &items);
                    self.list_selection = index;
                }
            }
            // Stay in the list the entry came from instead of jumping back to the form.
            self.mode = Mode::EditList;
            return;
        }

        match self.kind(&key) {
            Kind::Integer => match buffer.trim().parse::<i64>() {
                Ok(value) => match integer_range(&key) {
                    Some((minimum, maximum)) if value < minimum || value > maximum => {
                        self.status = format!(
                            "{key}: must be between {minimum} and {maximum}, keeping the old value"
                        );
                    }
                    _ if value < 0 => {
                        self.status = format!(
                            "{key}: negative values are not allowed, keeping the old value"
                        );
                    }
                    _ => self.set_value(&key, Value::from(value)),
                },
                Err(_) => {
                    self.status =
                        format!("{key}: '{buffer}' is not a number, keeping the old value");
                }
            },
            _ => self.set_value(&key, Value::from(buffer)),
        }
        self.mode = Mode::Form;
    }

    fn handle_edit_list(&mut self, key: KeyEvent) {
        let field = self.selected_field();
        let field_key = field.key.to_string();
        let items = self.get_list(&field_key);

        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.mode = Mode::Form;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.list_selection = self.list_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.list_selection + 1 < items.len() {
                    self.list_selection += 1;
                }
            }
            KeyCode::Char('a') => {
                // The entry is only inserted when the text is committed, so cancelling adds nothing.
                self.mode = Mode::EditText(TextEdit {
                    key: field_key,
                    entry: Some(items.len()),
                    adding: true,
                    buffer: String::new(),
                    cursor: 0,
                });
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                if let Some(value) = items.get(self.list_selection) {
                    self.mode = Mode::EditText(TextEdit {
                        key: field_key,
                        entry: Some(self.list_selection),
                        adding: false,
                        buffer: value.clone(),
                        cursor: value.chars().count(),
                    });
                }
            }
            KeyCode::Char('d') | KeyCode::Delete if self.list_selection < items.len() => {
                let mut items = items;
                items.remove(self.list_selection);
                self.set_list(&field_key, &items);
                self.list_selection = self.list_selection.min(items.len().saturating_sub(1));
            }
            _ => {}
        }
    }

    // ---- rendering ----

    fn render(&mut self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(5),
            ])
            .split(frame.area());

        self.render_header(frame, chunks[0]);
        if matches!(self.mode, Mode::PickMonitor) {
            self.render_monitor_picker(frame, chunks[1]);
        } else if matches!(self.mode, Mode::EditList) {
            self.render_list_editor(frame, chunks[1]);
        } else {
            self.render_form(frame, chunks[1]);
        }
        self.render_footer(frame, chunks[2]);

        if self.confirm_quit {
            self.render_confirm(frame);
        }
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let modified = if self.dirty { " (modified)" } else { "" };
        let text = Line::from(Span::styled("gsr-broadcastbox config", Style::new().bold()));
        let path = Line::from(format!("{}{modified}", self.path.display())).dim();
        let block = Paragraph::new(vec![text, path]).block(Block::default().borders(Borders::ALL));
        frame.render_widget(block, area);
    }

    fn render_form(&mut self, frame: &mut Frame, area: Rect) {
        self.normalize_selection();
        let entries = self.entries();
        let items: Vec<ListItem> = entries
            .iter()
            .map(|entry| match entry {
                Entry::Section(title) => ListItem::new(Line::from(format!("-- {title} --")).bold()),
                Entry::Field(index) => {
                    let field = &FIELDS[*index];
                    let mut spans = vec![
                        Span::raw(format!("{:<20}", field.label)),
                        Span::raw("  "),
                        self.value_span(field.key),
                    ];
                    if field.key == "monitor" {
                        if let Some(active) = &self.active_monitor {
                            spans.push(Span::styled(
                                format!("   (active: {active})"),
                                Style::new().dim(),
                            ));
                        }
                    }
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();

        let mut state = ListState::default();
        state.select(Some(self.selection));
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Settings"))
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);

        if let Mode::EditText(edit) = &self.mode {
            render_text_edit(frame, area, edit);
        }
    }

    /// Colored preview of a setting's value.
    fn value_span(&self, key: &str) -> Span<'static> {
        match self.kind(key) {
            Kind::Bool => {
                if self.get_bool(key) {
                    Span::styled("on", Style::new().fg(Color::Green))
                } else {
                    Span::styled("off", Style::new().fg(Color::Red))
                }
            }
            Kind::Integer => Span::styled(
                self.get_integer(key).to_string(),
                Style::new().fg(Color::Cyan),
            ),
            Kind::List => {
                let entries = self.get_list(key);
                Span::styled(
                    format!("[{} entries]", entries.len()),
                    Style::new().fg(Color::Magenta),
                )
            }
            Kind::Text if key == "stream_key" => {
                let value = self.get_text(key);
                if value.trim().is_empty() {
                    Span::styled("(username)", Style::new().dim())
                } else {
                    Span::styled(value, Style::new().fg(Color::Cyan))
                }
            }
            Kind::Text if key == "monitor" => {
                let value = self.get_text(key);
                if value == "current" || value == "focused" {
                    Span::styled("Current Monitor", Style::new().fg(Color::Cyan))
                } else if value.is_empty() {
                    Span::styled("(unset)", Style::new().dim())
                } else {
                    Span::styled(value, Style::new().fg(Color::Cyan))
                }
            }
            Kind::Text => {
                let value = self.get_text(key);
                if value.is_empty() {
                    Span::styled("(empty)", Style::new().dim())
                } else {
                    Span::styled(value, Style::new().fg(Color::Cyan))
                }
            }
        }
    }

    fn render_list_editor(&self, frame: &mut Frame, area: Rect) {
        let field = self.selected_field();
        let key = field.key.to_string();
        let items = self.get_list(&key);
        let entries: Vec<ListItem> = items
            .iter()
            .map(|item| {
                if item.is_empty() {
                    let example = list_example(&key);
                    let text = if example.is_empty() {
                        "(empty entry)".to_string()
                    } else {
                        format!("(example: {example})")
                    };
                    ListItem::new(Line::from(text).dim())
                } else {
                    ListItem::new(Line::from(item.clone()))
                }
            })
            .collect();

        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(self.list_selection.min(items.len() - 1)));
        }
        let list = List::new(entries)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("{} ({key})", field.label)),
            )
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);

        if let Mode::EditText(edit) = &self.mode {
            render_text_edit(frame, area, edit);
        }
    }

    fn render_monitor_picker(&self, frame: &mut Frame, area: Rect) {
        let choices = self.monitor_choices();
        let items: Vec<ListItem> = choices
            .iter()
            .map(|choice| {
                let label = self.monitor_label(choice);
                if choice == "current" {
                    ListItem::new(Line::from(label).bold())
                } else {
                    ListItem::new(Line::from(label))
                }
            })
            .collect();

        let mut state = ListState::default();
        if !choices.is_empty() {
            state.select(Some(self.picker_selection.min(choices.len() - 1)));
        }
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Select monitor"),
            )
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        let field = self.selected_field();
        let keys = match self.mode {
            Mode::EditText(_) => {
                "enter commit   esc cancel   <-/-> move   backspace delete   ctrl+c quit without saving"
            }
            Mode::EditList => {
                "up/down select   enter edit   a add   d delete   esc back   ctrl+c quit without saving"
            }
            Mode::PickMonitor => {
                "up/down select   enter choose   r refresh detection   esc back   ctrl+c quit without saving"
            }
            Mode::Form => {
                "up/down select   enter edit/toggle   left/right cycle   s save   q quit   ctrl+c quit without saving"
            }
        };
        let help = match self.mode {
            Mode::PickMonitor => format!("{}. {}", field.help, self.monitor_summary()),
            _ => field.help.to_string(),
        };
        let mut lines = vec![Line::from(help), Line::from(keys).dim()];
        if !self.status.is_empty() {
            lines.push(Line::from(self.status.clone()));
        }
        let block = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL))
            .wrap(Wrap { trim: true });
        frame.render_widget(block, area);
    }

    fn render_confirm(&self, frame: &mut Frame) {
        let area = centered_rect(frame.area(), 60, 5);
        let text = Paragraph::new(vec![
            Line::from("Unsaved changes").bold(),
            Line::from("s save and quit    d discard and quit    esc cancel"),
        ])
        .block(Block::default().borders(Borders::ALL).title("Quit"));
        frame.render_widget(Clear, area);
        frame.render_widget(text, area);
    }

    /// Draw, read keys, repeat.
    fn run_loop(&mut self, terminal: &mut DefaultTerminal) -> Result<(), String> {
        loop {
            terminal
                .draw(|frame| self.render(frame))
                .map_err(|e| format!("failed to draw the config editor: {e}"))?;
            let event = event::read().map_err(|e| format!("failed to read input: {e}"))?;
            if let Event::Key(key) = event {
                if self.handle_key(key) {
                    return Ok(());
                }
            }
        }
    }
}

/// Byte index of character `index` in `text` (or its length when past the end).
fn char_byte_index(text: &str, index: usize) -> usize {
    text.char_indices()
        .nth(index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// A rectangle of `width` x `height` centered in `area`.
fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

/// Draw the inline text editor and place the terminal cursor at its position.
fn render_text_edit(frame: &mut Frame, area: Rect, edit: &TextEdit) {
    let width = area.width.saturating_sub(4).clamp(20, 100);
    let popup = centered_rect(area, width, 3);
    let prefix: String = edit.buffer.chars().take(edit.cursor).collect();
    let suffix: String = edit.buffer.chars().skip(edit.cursor).collect();
    let line = Line::from(vec![
        Span::styled(prefix.clone(), Style::new().fg(Color::Cyan)),
        Span::styled(suffix, Style::new().fg(Color::Cyan)),
    ]);
    let block = Block::default().borders(Borders::ALL).title("Editing");
    frame.render_widget(Clear, popup);
    frame.render_widget(Paragraph::new(line).block(block), popup);

    let cursor_x = popup.x + 1 + prefix.chars().count().min(u16::MAX as usize) as u16;
    let cursor_y = popup.y + 1;
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Open the config editor. Returns an error message for the caller to print.
pub fn run() -> Result<(), String> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err("the config editor needs an interactive terminal".to_string());
    }
    let Some(path) = config_file::config_path() else {
        return Err("could not resolve a config directory".to_string());
    };

    let (mut doc, warning) = config_file::load_document(&path);
    let defaults = config_file::default_document();
    config_file::complete_document(&mut doc, &defaults);

    let mut editor = Editor::new(
        path,
        doc,
        monitors::detect_monitors(),
        monitors::active_monitor(),
    );
    editor.jump_to_first();
    if let Some(warning) = warning {
        editor.status = warning;
    } else if editor.monitors.is_empty() {
        editor.status = "No monitors detected; current falls back to screen".to_string();
    }

    let mut terminal = ratatui::try_init()
        .map_err(|error| format!("failed to initialize the terminal: {error}"))?;
    let result = editor.run_loop(&mut terminal);
    let restore =
        ratatui::try_restore().map_err(|error| format!("failed to restore the terminal: {error}"));

    match (result, restore) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => {
            if editor.cancelled {
                println!("Config unchanged");
            } else if editor.saved {
                println!("Saved {}", editor.path.display());
            }
            Ok(())
        }
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

    fn editor(monitors: Vec<Monitor>, active: Option<&str>) -> Editor {
        let mut doc = config_file::default_document();
        let defaults = config_file::default_document();
        config_file::complete_document(&mut doc, &defaults);
        Editor::new(
            PathBuf::from("/tmp/config.toml"),
            doc,
            monitors,
            active.map(str::to_string),
        )
    }

    #[test]
    fn every_field_exists_in_the_default_config() {
        let defaults = config_file::default_document();
        for field in FIELDS {
            assert!(
                defaults.get(field.key).is_some(),
                "DEFAULT_CONFIG is missing {}",
                field.key
            );
        }
    }

    #[test]
    fn default_fields_have_the_expected_kinds() {
        let editor = editor(Vec::new(), None);
        assert_eq!(editor.kind("teamspeak"), Kind::Bool);
        assert_eq!(editor.kind("watch_hardware_decode"), Kind::Bool);
        assert_eq!(editor.kind("watch_mpv_args"), Kind::List);
        assert_eq!(editor.kind("fps"), Kind::Integer);
        assert_eq!(editor.kind("quality"), Kind::Text);
        assert_eq!(editor.kind("audio_sources"), Kind::List);
    }

    #[test]
    fn monitor_picker_starts_with_current_and_lists_detected_outputs() {
        let editor = editor(
            vec![
                monitor("DP-1", "1920x1080 at 0,0"),
                monitor("DP-3", "2560x1440 at 1920,0"),
            ],
            Some("DP-3"),
        );
        assert_eq!(editor.monitor_choices(), vec!["current", "DP-1", "DP-3"]);
        assert!(editor.monitor_label("current").contains("DP-3"));
        assert!(editor.monitor_label("DP-3").contains("[active]"));
        assert!(editor.monitor_label("DP-1").contains("1920x1080"));
    }

    #[test]
    fn a_configured_output_that_is_missing_is_kept_in_the_picker() {
        let editor = editor(vec![monitor("DP-1", "")], Some("DP-1"));
        let mut doc = editor.doc.clone();
        doc["monitor"] = toml_edit::value("HDMI-A-1");
        let editor = Editor::new(
            PathBuf::from("/tmp/config.toml"),
            doc,
            vec![monitor("DP-1", "")],
            Some("DP-1".to_string()),
        );
        assert_eq!(
            editor.monitor_choices(),
            vec!["current", "DP-1", "HDMI-A-1"]
        );
    }

    #[test]
    fn focused_is_shown_as_current() {
        let mut doc = config_file::default_document();
        doc["monitor"] = toml_edit::value("focused");
        let editor = Editor::new(PathBuf::from("/tmp/config.toml"), doc, Vec::new(), None);
        assert_eq!(editor.get_text("monitor"), "current");
    }

    #[test]
    fn integer_ranges_match_the_validator() {
        assert_eq!(integer_range("fps"), Some((1, 480)));
        assert_eq!(integer_range("video_bitrate"), Some((1, 200_000)));
        assert_eq!(integer_range("audio_bitrate"), Some((0, 512)));
        assert_eq!(integer_range("stream_key"), None);
    }
}
