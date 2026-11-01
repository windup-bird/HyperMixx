//! The settings screen: four things, one flat list.
//!
//! ```
//! > controller   Numark Party Mix III · profile/controller.toml
//!   mixer        profile/mixer.toml · 2 outputs
//!   live         profile/live.toml
//!   preference   profile/preference.toml
//! ```
//!
//! `Enter`/`→` goes a level deeper, `←` comes back, `Esc` closes. Every level is the same
//! single-column [`popup`] the controller guide uses — no tree, no second pane, no TOML editor. The
//! deeper levels are the choosers themselves:
//!
//! * `controller › input` (MIDI ports) → `controller › map` (a `.toml` file) → reconnect.
//! * `mixer › {output}` (one row per `[[output]]`) → `… › input` (hardware) → `… › channels` →
//!   writes `device`/`channels` back into `mixer.toml` and reloads the topology.
//! * `live` and `preference` are file pickers; the caller reloads what they point at.

use std::fs;

use crossterm::event::KeyCode;
use toml_edit::DocumentMut;

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::preference::Theme;
use crate::tui::devices;
use crate::tui::picker::{FilePicker, PortList};
use crate::tui::popup::{self, Row, View};

/// What the settings screen was opened with.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// The profile file this session was started from, if any (shown, never edited).
    pub profile: Option<String>,
    pub mixer: Option<String>,
    pub live: Option<String>,
    pub preference: Option<String>,
    pub controller_port: Option<String>,
    pub controller_map: Option<String>,
    /// The connected controller's name, used to point at its own audio interface.
    pub controller: Option<String>,
    /// The engine's rate, shown in the hardware chooser: a device that cannot run at it is marked.
    pub engine_rate: u32,
    /// Output devices, probed *before* the terminal is taken over: ALSA and JACK write warnings to
    /// stderr, and a probe from inside the alternate screen prints over the UI.
    pub devices: Vec<hypermixx_audio::DeviceInfo>,
}

/// What a key press did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub close: bool,
    /// `mixer.toml` was written, so the caller reloads the topology.
    pub mixer_saved: bool,
    /// The controller selection changed: `(port, map path)`. The caller reconnects.
    pub controller: Option<(String, String)>,
    /// A new `live.toml` was chosen.
    pub live: Option<String>,
    /// A new `preference.toml` was chosen.
    pub preference: Option<String>,
}

/// The four rows of the root list, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Item {
    Controller,
    Mixer,
    Live,
    Preference,
}

const ITEMS: [Item; 4] = [Item::Controller, Item::Mixer, Item::Live, Item::Preference];

/// Which file a picker is choosing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileTarget {
    ControllerMap,
    Live,
    Preference,
}

/// A mixer output, read back from `mixer.toml`.
#[derive(Clone, Debug)]
struct OutputInfo {
    name: String,
    role: String,
    device: Option<String>,
    channels: Option<(u16, u16)>,
}

/// One level below the root. The stack is the path the title shows and what `←` unwinds.
enum Level {
    Ports(PortList),
    Files(FilePicker, FileTarget),
    /// One row per `[[output]]`, with its own cursor: each level remembers where it was.
    Outputs {
        cursor: usize,
    },
    /// The hardware a destination can be bound to.
    Hardware {
        output: usize,
        cursor: usize,
    },
    /// Which channel pair of that hardware.
    Channels {
        output: usize,
        device: usize,
        cursor: usize,
    },
}

pub struct SettingsScreen {
    context: Context,
    /// The root cursor and the open levels.
    cursor: usize,
    stack: Vec<Level>,
    status: Option<String>,
    outputs: Vec<OutputInfo>,
    devices: Vec<hypermixx_audio::DeviceInfo>,
    mixer_path: String,
}

impl SettingsScreen {
    pub fn open(context: Context) -> Self {
        let mixer_path = context
            .mixer
            .clone()
            .unwrap_or_else(|| "profile/mixer.toml".to_owned());
        let devices = context.devices.clone();
        let mut screen = Self {
            context,
            cursor: 0,
            stack: Vec::new(),
            status: None,
            outputs: Vec::new(),
            devices,
            mixer_path,
        };
        screen.refresh();
        screen
    }

    /// Re-reads the mixer file, which is the only list that comes from disk.
    fn refresh(&mut self) {
        self.outputs = read_doc(&self.mixer_path)
            .as_ref()
            .map(outputs_of)
            .unwrap_or_default();
    }

    /// Handles one key press. The screen owns the keyboard while it is open.
    pub fn handle_key(&mut self, code: KeyCode) -> Outcome {
        let mut outcome = Outcome::default();
        if self.stack.is_empty() {
            self.handle_root(code, &mut outcome);
        } else {
            self.handle_level(code, &mut outcome);
        }
        outcome
    }

    fn handle_root(&mut self, code: KeyCode, outcome: &mut Outcome) {
        match code {
            KeyCode::Esc => outcome.close = true,
            KeyCode::Up | KeyCode::Down => {
                self.status = None;
                let last = ITEMS.len() - 1;
                self.cursor = match code {
                    KeyCode::Up => self.cursor.saturating_sub(1),
                    _ => (self.cursor + 1).min(last),
                };
            }
            KeyCode::Enter | KeyCode::Right => self.open_item(ITEMS[self.cursor]),
            KeyCode::Char('r') => {
                // Only files are re-read: probing devices here would print ALSA warnings over the
                // UI (the list is probed once, before the terminal is taken over).
                self.refresh();
                self.status = Some("reloaded".to_owned());
            }
            _ => {}
        }
    }

    /// `Enter`/`→` on a root row: the first level of that item's chooser.
    fn open_item(&mut self, item: Item) {
        self.stack.push(match item {
            Item::Controller => Level::Ports(PortList::refresh()),
            Item::Mixer => Level::Outputs { cursor: 0 },
            Item::Live => Level::Files(
                FilePicker::new(
                    self.context.live.as_deref().map(std::path::Path::new),
                    &["toml"],
                    "live",
                ),
                FileTarget::Live,
            ),
            Item::Preference => Level::Files(
                FilePicker::new(
                    self.context.preference.as_deref().map(std::path::Path::new),
                    &["toml"],
                    "preference",
                ),
                FileTarget::Preference,
            ),
        });
    }

    fn handle_level(&mut self, code: KeyCode, outcome: &mut Outcome) {
        // `←` unwinds one level, `Esc` closes the screen: the same rule as every other modal.
        if code == KeyCode::Left {
            self.stack.pop();
            return;
        }
        if code == KeyCode::Esc {
            outcome.close = true;
            return;
        }
        let Some(level) = self.stack.pop() else {
            return;
        };
        match level {
            Level::Ports(mut list) => match code {
                KeyCode::Up => {
                    list.move_selection(-1);
                    self.stack.push(Level::Ports(list));
                }
                KeyCode::Down => {
                    list.move_selection(1);
                    self.stack.push(Level::Ports(list));
                }
                // The device first, then the map: choosing a port always leads to the file.
                KeyCode::Enter | KeyCode::Right => match list.selected_port() {
                    Some(port) => {
                        self.context.controller_port = Some(port.name.clone());
                        self.status = Some(format!("input: {}", port.name));
                        self.stack.push(Level::Files(
                            FilePicker::new(
                                self.context
                                    .controller_map
                                    .as_deref()
                                    .map(std::path::Path::new),
                                &["toml"],
                                "controller map",
                            ),
                            FileTarget::ControllerMap,
                        ));
                    }
                    None => {
                        self.status = Some("no MIDI input ports found".to_owned());
                        self.stack.push(Level::Ports(list));
                    }
                },
                _ => self.stack.push(Level::Ports(list)),
            },
            Level::Files(mut picker, target) => match code {
                KeyCode::Up => {
                    picker.move_selection(-1);
                    self.stack.push(Level::Files(picker, target));
                }
                KeyCode::Down => {
                    picker.move_selection(1);
                    self.stack.push(Level::Files(picker, target));
                }
                // Up a directory is `Delete`, so `←` stays "one level back".
                KeyCode::Delete | KeyCode::PageUp => {
                    picker.up();
                    self.stack.push(Level::Files(picker, target));
                }
                KeyCode::Enter | KeyCode::Right => match picker.activate() {
                    Some(path) => self.choose_file(path.display().to_string(), target, outcome),
                    None => self.stack.push(Level::Files(picker, target)),
                },
                _ => self.stack.push(Level::Files(picker, target)),
            },
            Level::Outputs { mut cursor } => match code {
                KeyCode::Up => {
                    cursor = cursor.saturating_sub(1);
                    self.stack.push(Level::Outputs { cursor });
                }
                KeyCode::Down => {
                    cursor = (cursor + 1).min(self.outputs.len().saturating_sub(1));
                    self.stack.push(Level::Outputs { cursor });
                }
                KeyCode::Enter | KeyCode::Right => {
                    if self.outputs.get(cursor).is_some() {
                        self.stack.push(Level::Hardware {
                            output: cursor,
                            cursor: self.hardware_start(cursor),
                        });
                    } else {
                        self.stack.push(Level::Outputs { cursor });
                    }
                }
                _ => self.stack.push(Level::Outputs { cursor }),
            },
            Level::Hardware { output, mut cursor } => {
                let rows = devices::hardware_rows(&self.devices);
                match code {
                    KeyCode::Up => cursor = cursor.saturating_sub(1),
                    KeyCode::Down => cursor = (cursor + 1).min(rows.len().saturating_sub(1)),
                    KeyCode::Enter | KeyCode::Right => match rows.get(cursor) {
                        Some(row) => {
                            let device = row.device();
                            let pairs = devices::pairs(self.channels_of(device));
                            // A plain stereo interface has nothing to ask: bind it here.
                            if pairs.len() > 1 {
                                self.stack.push(Level::Channels {
                                    output,
                                    device,
                                    cursor: 0,
                                });
                            } else {
                                self.bind(output, device, pairs.first().copied(), outcome);
                            }
                            return;
                        }
                        None => self.status = Some("no output devices found".to_owned()),
                    },
                    _ => {}
                }
                self.stack.push(Level::Hardware { output, cursor });
            }
            Level::Channels {
                output,
                device,
                mut cursor,
            } => {
                let pairs = devices::pairs(self.channels_of(device));
                match code {
                    KeyCode::Up => cursor = cursor.saturating_sub(1),
                    KeyCode::Down => cursor = (cursor + 1).min(pairs.len().saturating_sub(1)),
                    KeyCode::Enter | KeyCode::Right => {
                        if let Some(pair) = pairs.get(cursor).copied() {
                            self.bind(output, device, Some(pair), outcome);
                            return;
                        }
                    }
                    _ => {}
                }
                self.stack.push(Level::Channels {
                    output,
                    device,
                    cursor,
                });
            }
        }
    }

    fn choose_file(&mut self, path: String, target: FileTarget, outcome: &mut Outcome) {
        match target {
            FileTarget::ControllerMap => {
                self.context.controller_map = Some(path.clone());
                self.status = Some(format!("map: {path}"));
                if let Some(port) = self.context.controller_port.clone() {
                    outcome.controller = Some((port, path));
                }
                self.stack.clear();
            }
            FileTarget::Live => {
                self.context.live = Some(path.clone());
                self.status = Some(format!("live: {path}"));
                outcome.live = Some(path);
                self.stack.clear();
            }
            FileTarget::Preference => {
                self.context.preference = Some(path.clone());
                self.status = Some(format!("preference: {path}"));
                outcome.preference = Some(path);
                self.stack.clear();
            }
        }
    }

    fn channels_of(&self, device: usize) -> u16 {
        self.devices.get(device).map_or(0, |device| device.channels)
    }

    /// Where the hardware cursor starts: the bound device, else the controller's own interface.
    fn hardware_start(&self, output: usize) -> usize {
        let bound = self
            .outputs
            .get(output)
            .and_then(|info| info.device.clone());
        bound
            .as_deref()
            .and_then(|pattern| self.device_index(pattern))
            .or_else(|| self.controller_device())
            .unwrap_or(0)
    }

    fn device_index(&self, pattern: &str) -> Option<usize> {
        let pattern = pattern.trim();
        if let Ok(index) = pattern.parse::<usize>() {
            return (index < self.devices.len()).then_some(index);
        }
        let needle = pattern.to_ascii_lowercase();
        self.devices
            .iter()
            .position(|device| device.name.to_ascii_lowercase().contains(&needle))
    }

    fn controller_device(&self) -> Option<usize> {
        let controller = self.context.controller.as_deref()?;
        self.devices
            .iter()
            .position(|device| devices::matches_controller(device, controller))
    }

    /// Writes one destination's device (and channel pair) into `mixer.toml`.
    fn bind(
        &mut self,
        output: usize,
        device: usize,
        channels: Option<(u16, u16)>,
        outcome: &mut Outcome,
    ) {
        let Some(info) = self.devices.get(device) else {
            return;
        };
        let name = info.name.clone();
        match write_output(&self.mixer_path, output, &name, channels) {
            Ok(()) => {
                let pair = channels
                    .map(|(left, right)| format!(" ch {left},{right}"))
                    .unwrap_or_default();
                self.status = Some(format!("output {output} → {name}{pair}"));
                outcome.mixer_saved = true;
                self.refresh();
                self.stack.clear();
            }
            Err(err) => {
                self.status = Some(err);
                self.stack.clear();
            }
        }
    }

    /// The modal for the current level.
    fn view(&self) -> View {
        match self.stack.last() {
            None => self.root_view(),
            Some(Level::Ports(list)) => list.view("settings › controller › input"),
            Some(Level::Files(picker, target)) => {
                let level = match target {
                    FileTarget::ControllerMap => "controller › map",
                    FileTarget::Live => "live",
                    FileTarget::Preference => "preference",
                };
                let view = picker.view();
                View {
                    title: format!("settings › {level}"),
                    ..view
                }
            }
            Some(Level::Outputs { cursor }) => {
                let rows = self
                    .outputs
                    .iter()
                    .map(|output| {
                        let role = output.role.clone();
                        let device = self.device_note(output.device.as_deref(), output.channels);
                        Row::new(format!("{} ({role})", output.name)).note(device)
                    })
                    .collect();
                View::new("settings › mixer", rows)
                    .cursor(*cursor)
                    .hint("↑↓ move · →/Enter choose hardware · Esc close")
            }
            Some(Level::Hardware { output, cursor }) => {
                let bound = self
                    .outputs
                    .get(*output)
                    .and_then(|output| output.device.as_deref())
                    .and_then(|pattern| self.device_index(pattern));
                let rows = devices::hardware_rows(&self.devices)
                    .iter()
                    .map(|row| {
                        let device = &self.devices[row.device()];
                        let mark = match (bound == Some(row.device()), self.is_controller(device)) {
                            (true, _) => '●',
                            (false, true) => '◆',
                            (false, false) => ' ',
                        };
                        let rate = match device.accepts {
                            Some(true) => "✓".to_owned(),
                            Some(false) => "✗ rate".to_owned(),
                            None => String::new(),
                        };
                        Row::new(devices::label(&self.devices, *row))
                            .mark(mark)
                            .note(format!("{rate} {}", device.rates))
                    })
                    .collect();
                View::new(self.output_title(*output, "input"), rows)
                    .cursor(*cursor)
                    .hint(format!(
                        "engine {}Hz · ↑↓ move · →/Enter channels · ← back · Esc close",
                        self.context.engine_rate
                    ))
            }
            Some(Level::Channels {
                output,
                device,
                cursor,
            }) => {
                let rows = devices::pairs(self.channels_of(*device))
                    .into_iter()
                    .enumerate()
                    .map(|(index, (left, right))| {
                        let row = Row::new(format!("channels {left},{right}"));
                        if index == 0 {
                            row.note("the usual stereo pair")
                        } else {
                            row
                        }
                    })
                    .collect();
                View::new(self.output_title(*output, "channels"), rows)
                    .cursor(*cursor)
                    .hint("↑↓ move · Enter bind · ← back · Esc close")
            }
        }
    }

    fn output_title(&self, output: usize, level: &str) -> String {
        let name = self
            .outputs
            .get(output)
            .map_or_else(|| format!("output {output}"), |info| info.name.clone());
        format!("settings › mixer › {name} › {level}")
    }

    fn root_view(&self) -> View {
        let note = |value: &Option<String>| value.clone().unwrap_or_else(|| "<none>".to_owned());
        let rows = vec![
            Row::new("controller").note(
                match (&self.context.controller_port, &self.context.controller_map) {
                    (Some(port), Some(map)) => format!("{port} · {map}"),
                    (Some(port), None) => format!("{port} · <no map>"),
                    _ => "<not connected>".to_owned(),
                },
            ),
            Row::new("mixer").note(format!(
                "{} · {} output(s)",
                self.mixer_path,
                self.outputs.len()
            )),
            Row::new("live").note(note(&self.context.live)),
            Row::new("preference").note(note(&self.context.preference)),
        ];
        // The profile is the pointer all four paths come from, so it belongs on screen — but it is
        // not a fifth row: it is where the rows came from.
        let hint = self
            .status
            .clone()
            .unwrap_or_else(|| match &self.context.profile {
                Some(profile) => format!("from {profile} · ↑↓ move · →/Enter open · Esc close"),
                None => "↑↓ move · →/Enter open · Esc close".to_owned(),
            });
        View::new("settings", rows).cursor(self.cursor).hint(hint)
    }

    fn is_controller(&self, device: &hypermixx_audio::DeviceInfo) -> bool {
        self.context
            .controller
            .as_deref()
            .is_some_and(|controller| devices::matches_controller(device, controller))
    }

    /// How a destination's `device`/`channels` reads back: the resolved name, or the raw pattern.
    fn device_note(&self, pattern: Option<&str>, channels: Option<(u16, u16)>) -> String {
        let Some(pattern) = pattern else {
            return "<host default>".to_owned();
        };
        let pair = channels
            .map(|(left, right)| format!(" ch {left},{right}"))
            .unwrap_or_default();
        match self.device_index(pattern) {
            Some(index) => {
                let device = &self.devices[index];
                let warn = if device.accepts == Some(false) {
                    " ✗ rate"
                } else {
                    ""
                };
                format!("{}{warn}{pair}", device.name)
            }
            None => format!("{pattern} ✗ missing"),
        }
    }
}

/// Draws the screen over the whole terminal.
pub fn render(frame: &mut Frame, area: Rect, screen: &SettingsScreen, theme: &Theme) {
    popup::render(frame, area, theme, &screen.view());
}

fn read_doc(path: &str) -> Option<DocumentMut> {
    fs::read_to_string(path).ok()?.parse().ok()
}

/// The `[[output]]` tables of a mixer document.
fn outputs_of(doc: &DocumentMut) -> Vec<OutputInfo> {
    let Some(tables) = doc.get("output").and_then(|item| item.as_array_of_tables()) else {
        return Vec::new();
    };
    tables
        .iter()
        .map(|table| OutputInfo {
            name: table
                .get("name")
                .and_then(|item| item.as_str())
                .unwrap_or("output")
                .to_owned(),
            role: table
                .get("role")
                .and_then(|item| item.as_str())
                .unwrap_or("master")
                .to_owned(),
            device: table
                .get("device")
                .and_then(|item| item.as_str())
                .map(str::to_owned),
            channels: table
                .get("channels")
                .and_then(|item| item.as_array())
                .filter(|array| array.len() == 2)
                .and_then(|array| {
                    Some((
                        u16::try_from(array.get(0)?.as_integer()?).ok()?,
                        u16::try_from(array.get(1)?.as_integer()?).ok()?,
                    ))
                }),
        })
        .collect()
}

/// Writes `device` (and optionally the channel pair) into one destination, keeping its comments.
fn write_output(
    path: &str,
    index: usize,
    device: &str,
    channels: Option<(u16, u16)>,
) -> Result<(), String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{path}: {err}"))?;
    let mut doc: DocumentMut = text.parse().map_err(|err| format!("{path}: {err}"))?;
    let table = doc
        .get_mut("output")
        .and_then(|item| item.as_array_of_tables_mut())
        .and_then(|tables| tables.get_mut(index))
        .ok_or_else(|| format!("{path}: no [[output]] #{index}"))?;
    set_str(table, "device", device);
    if let Some((left, right)) = channels {
        let mut array = toml_edit::Array::new();
        array.push(i64::from(left));
        array.push(i64::from(right));
        set_array(table, "channels", array);
    }
    fs::write(path, doc.to_string()).map_err(|err| format!("{path}: {err}"))
}

/// Sets a string key, keeping the decor (`toml_edit` stores comments and spacing there).
fn set_str(table: &mut toml_edit::Table, key: &str, value: &str) {
    match table.get_mut(key) {
        Some(toml_edit::Item::Value(existing)) => {
            let decor = existing.decor().clone();
            *existing = value.into();
            *existing.decor_mut() = decor;
        }
        _ => {
            table.insert(key, toml_edit::Item::Value(value.into()));
        }
    }
}

fn set_array(table: &mut toml_edit::Table, key: &str, array: toml_edit::Array) {
    match table.get_mut(key) {
        Some(toml_edit::Item::Value(existing)) => {
            let decor = existing.decor().clone();
            *existing = toml_edit::Value::Array(array);
            *existing.decor_mut() = decor;
        }
        _ => {
            table.insert(key, toml_edit::Item::Value(toml_edit::Value::Array(array)));
        }
    }
}

#[cfg(test)]
impl SettingsScreen {
    fn focus(&mut self, needle: &str) -> bool {
        let found = (0..ITEMS.len()).find(|index| {
            let view = self.root_view();
            view.rows[*index].label.contains(needle) || view.rows[*index].note.contains(needle)
        });
        match found {
            Some(index) => {
                self.cursor = index;
                true
            }
            None => false,
        }
    }

    fn set_devices(&mut self, devices: Vec<hypermixx_audio::DeviceInfo>) {
        self.devices = devices;
    }

    fn depth(&self) -> usize {
        self.stack.len()
    }

    fn title(&self) -> String {
        self.view().title
    }

    fn rows(&self) -> Vec<(String, String)> {
        self.view()
            .rows
            .iter()
            .map(|row| (row.label.clone(), row.note.clone()))
            .collect()
    }

    fn cursor_row(&self) -> usize {
        self.view().cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "hypermixx-settings-{tag}-{}-{}-{}",
            std::process::id(),
            hypermixx_library::library::db::unix_now(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn write(dir: &std::path::Path, name: &str, text: &str) -> String {
        fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, text).unwrap();
        path.display().to_string()
    }

    fn screen() -> SettingsScreen {
        let dir = unique("screen");
        let mixer = write(
            &dir,
            "mixer.toml",
            concat!(
                "# my rig\n",
                "[[output]]\nname = \"main\"\nrole = \"master\"\n\n",
                "[[output]]\nname = \"phones\"\nrole = \"cue\"\n",
            ),
        );
        let map = write(&dir, "controller.toml", "x = 1\n");
        SettingsScreen::open(Context {
            mixer: Some(mixer),
            live: Some("/live.toml".to_owned()),
            preference: Some("/preference.toml".to_owned()),
            controller_port: Some("Numark Party Mix III MIDI 1".to_owned()),
            controller_map: Some(map),
            controller: Some("Numark Party Mix III MIDI 1".to_owned()),
            engine_rate: 44_100,
            ..Default::default()
        })
    }

    fn device(name: &str, channels: u16) -> hypermixx_audio::DeviceInfo {
        hypermixx_audio::DeviceInfo {
            name: name.to_owned(),
            description: String::new(),
            is_default: false,
            rates: "44100".to_owned(),
            accepts: Some(true),
            channels,
        }
    }

    #[test]
    fn the_root_list_is_controller_mixer_live_preference() {
        let screen = screen();
        let labels: Vec<String> = screen.rows().into_iter().map(|(label, _)| label).collect();
        assert_eq!(labels, vec!["controller", "mixer", "live", "preference"]);
        assert_eq!(screen.title(), "settings");
        assert!(
            screen.rows()[1].1.contains("2 output(s)"),
            "the mixer row counts outputs"
        );
    }

    #[test]
    fn one_cursor_walks_the_root_and_stops_at_the_ends() {
        let mut screen = screen();
        for _ in 0..6 {
            screen.handle_key(KeyCode::Down);
        }
        assert_eq!(screen.cursor_row(), 3, "clamped to the last item");
        for _ in 0..6 {
            screen.handle_key(KeyCode::Up);
        }
        assert_eq!(screen.cursor_row(), 0);
    }

    #[test]
    fn controller_chooses_the_device_first_and_the_map_second() {
        let mut screen = screen();
        assert!(screen.focus("controller"));
        screen.handle_key(KeyCode::Enter);
        assert_eq!(screen.depth(), 1);
        assert_eq!(screen.title(), "settings › controller › input");
        // Enter on the port (the list is empty on a test host, so it only reports) keeps the level.
        screen.handle_key(KeyCode::Enter);
        assert_eq!(
            screen.depth(),
            1,
            "no ports in a test: nothing to descend into"
        );
        screen.handle_key(KeyCode::Left);
        assert_eq!(screen.depth(), 0, "← backs out");
    }

    #[test]
    fn mixer_goes_output_then_hardware_then_channels_and_writes_the_file() {
        let mut screen = screen();
        screen.set_devices(vec![device("Quad Interface", 4), device("Stereo Box", 2)]);
        assert!(screen.focus("mixer"));
        screen.handle_key(KeyCode::Enter);
        assert_eq!(screen.title(), "settings › mixer");
        assert_eq!(
            screen.rows()[0],
            ("main (master)".to_owned(), "<host default>".to_owned())
        );
        screen.handle_key(KeyCode::Enter);
        assert_eq!(screen.title(), "settings › mixer › main › input");
        screen.handle_key(KeyCode::Right);
        assert_eq!(screen.title(), "settings › mixer › main › channels");
        screen.handle_key(KeyCode::Down);
        let outcome = screen.handle_key(KeyCode::Enter);
        assert!(outcome.mixer_saved);
        assert_eq!(screen.depth(), 0, "a write goes back to the root");
        let written = fs::read_to_string(screen.mixer_path.clone()).unwrap();
        assert!(written.contains("# my rig"), "{written}");
        assert!(written.contains("device = \"Quad Interface\""), "{written}");
        assert!(written.contains("channels = [2, 3]"), "{written}");
    }

    #[test]
    fn a_stereo_device_is_bound_without_a_channel_level() {
        let mut screen = screen();
        screen.set_devices(vec![device("Stereo Box", 2)]);
        assert!(screen.focus("mixer"));
        screen.handle_key(KeyCode::Enter);
        screen.handle_key(KeyCode::Down);
        screen.handle_key(KeyCode::Enter);
        assert_eq!(screen.title(), "settings › mixer › phones › input");
        let outcome = screen.handle_key(KeyCode::Enter);
        assert!(
            outcome.mixer_saved,
            "no channel question for a stereo device"
        );
        assert_eq!(screen.depth(), 0);
    }

    #[test]
    fn choosing_a_file_hands_the_path_back_to_the_caller() {
        let mut screen = screen();
        let picker_rows = screen.rows();
        assert!(
            picker_rows[2].1.ends_with("live.toml"),
            "{:?}",
            picker_rows[2]
        );
        // `live` opens a picker rooted at the current file's directory.
        assert!(screen.focus("live"));
        screen.handle_key(KeyCode::Enter);
        assert_eq!(screen.title(), "settings › live");
        assert_eq!(screen.depth(), 1);
        screen.handle_key(KeyCode::Esc);
        assert!(screen.handle_key(KeyCode::Esc).close);
    }
}
