//! In-TUI pickers: a filesystem browser and a MIDI-port chooser.
//!
//! Both front-ends (`--tui` and `--midi-guide`) embed these, so a path or a port can be chosen
//! without a startup flag. The widgets only read state; navigation is small mutators, which keeps
//! them unit-testable without a terminal. Rendering is a centred popup the caller draws last.

use std::fs;
use std::path::{Path, PathBuf};

use hypermixx_midi::PortInfo;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState};
use ratatui::Frame;

/// One row in a [`FilePicker`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// A directory browser with an optional extension filter.
///
/// Directories are always shown and sorted first; files are filtered to `extensions` (empty = all).
/// Dotfiles are kept, because a map often lives under `~/.config`.
pub struct FilePicker {
    dir: PathBuf,
    entries: Vec<Entry>,
    selected: usize,
    extensions: Vec<String>,
    title: String,
    error: Option<String>,
}

impl FilePicker {
    /// Opens `start`'s directory (or `start` itself if it is one, else the current directory).
    pub fn new(start: Option<&Path>, extensions: &[&str], title: &str) -> Self {
        let mut picker = Self {
            dir: resolve_start(start),
            entries: Vec::new(),
            selected: 0,
            extensions: extensions
                .iter()
                .map(|ext| ext.trim_start_matches('.').to_ascii_lowercase())
                .collect(),
            title: title.to_owned(),
            error: None,
        };
        picker.reload();
        picker
    }

    /// Re-reads the current directory.
    pub fn reload(&mut self) {
        self.entries.clear();
        self.error = None;
        match fs::read_dir(&self.dir) {
            Ok(read) => {
                for entry in read.flatten() {
                    let path = entry.path();
                    let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                    if !is_dir && !self.matches(&path) {
                        continue;
                    }
                    self.entries.push(Entry {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        path,
                        is_dir,
                    });
                }
                self.entries.sort_by(|a, b| {
                    b.is_dir
                        .cmp(&a.is_dir)
                        .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                });
            }
            Err(err) => self.error = Some(err.to_string()),
        }
        if self.selected >= self.entries.len() {
            self.selected = self.entries.len().saturating_sub(1);
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if !self.entries.is_empty() {
            self.selected =
                (self.selected as isize + delta).rem_euclid(self.entries.len() as isize) as usize;
        }
    }

    /// Descends into the highlighted directory, or returns the highlighted file.
    pub fn activate(&mut self) -> Option<PathBuf> {
        let entry = self.entries.get(self.selected)?.clone();
        if entry.is_dir {
            self.dir = entry.path;
            self.selected = 0;
            self.reload();
            None
        } else {
            Some(entry.path)
        }
    }

    /// Goes to the parent directory (no-op at the filesystem root).
    pub fn up(&mut self) {
        if let Some(parent) = self.dir.parent() {
            self.dir = parent.to_path_buf();
            self.selected = 0;
            self.reload();
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn matches(&self, path: &Path) -> bool {
        if self.extensions.is_empty() {
            return true;
        }
        path.extension()
            .map(|ext| {
                let ext = ext.to_string_lossy();
                self.extensions.iter().any(|wanted| wanted.eq_ignore_ascii_case(&ext))
            })
            .unwrap_or(false)
    }
}

/// The list of MIDI input ports, with a selection cursor. Refreshed on demand so a controller
/// plugged in after launch shows up.
pub struct PortList {
    ports: Vec<PortInfo>,
    selected: usize,
    error: Option<String>,
}

impl PortList {
    /// Enumerates the current ports.
    pub fn refresh() -> Self {
        match hypermixx_midi::list_ports() {
            Ok(ports) => Self {
                ports,
                selected: 0,
                error: None,
            },
            Err(err) => Self {
                ports: Vec::new(),
                selected: 0,
                error: Some(err),
            },
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if !self.ports.is_empty() {
            self.selected =
                (self.selected as isize + delta).rem_euclid(self.ports.len() as isize) as usize;
        }
    }

    /// The highlighted port, if any (cloned; the list is tiny).
    pub fn selected_port(&self) -> Option<PortInfo> {
        self.ports.get(self.selected).cloned()
    }

    pub fn selected(&self) -> usize {
        self.selected
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
}

/// Draws `picker` as a centred popup over `area`.
pub fn render_file_picker(frame: &mut Frame, area: Rect, picker: &FilePicker) {
    let popup = centered(area, 72, 72);
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = if picker.entries().is_empty() {
        let text = picker.error().unwrap_or("(empty)");
        vec![ListItem::new(text.to_owned()).style(Style::default().fg(Color::Red))]
    } else {
        picker
            .entries()
            .iter()
            .map(|entry| {
                let (label, style) = if entry.is_dir {
                    (format!("{}/", entry.name), Style::default().fg(Color::Blue))
                } else {
                    (entry.name.clone(), Style::default())
                };
                ListItem::new(label).style(style)
            })
            .collect()
    };
    let title = format!(" {} — {} ", picker.title(), picker.dir().display());
    let list = List::new(items)
        .block(Block::bordered().title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(picker.selected()));
    frame.render_stateful_widget(list, popup, &mut state);
}

/// Draws the port list as a centred popup. `title` lets the caller say what connecting means.
pub fn render_port_list(frame: &mut Frame, area: Rect, ports: &PortList, title: &str) {
    let popup = centered(area, 72, 50);
    frame.render_widget(Clear, popup);
    let items: Vec<ListItem> = if ports.ports.is_empty() {
        let text = ports
            .error()
            .map(str::to_owned)
            .unwrap_or_else(|| "no MIDI input ports".to_owned());
        vec![ListItem::new(text).style(Style::default().fg(Color::Red))]
    } else {
        ports
            .ports
            .iter()
            .map(|port| ListItem::new(format!("  {}  {}", port.index, port.name)))
            .collect()
    };
    let list = List::new(items)
        .block(Block::bordered().title(format!(" {title} ")))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    let mut state = ListState::default().with_selected(Some(ports.selected()));
    frame.render_stateful_widget(list, popup, &mut state);
}

fn resolve_start(start: Option<&Path>) -> PathBuf {
    let cwd = || std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match start {
        Some(path) if path.is_dir() => path.to_path_buf(),
        // `parent()` of a bare filename is `Some("")`, which `read_dir` rejects; fall back to cwd.
        Some(path) => match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            _ => cwd(),
        },
        None => cwd(),
    }
}

fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let percent_x = percent_x.min(100);
    let percent_y = percent_y.min(100);
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("hypermixx-picker-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.toml"), "x = 1").unwrap();
        fs::write(dir.join("b.txt"), "nope").unwrap();
        fs::write(dir.join("sub").join("c.toml"), "y = 2").unwrap();
        dir
    }

    #[test]
    fn the_filter_hides_files_but_keeps_directories() {
        let dir = temp_dir("filter");
        let picker = FilePicker::new(Some(&dir), &["toml"], "map");
        let names: Vec<&str> = picker.entries().iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.toml"], "dirs first, txt filtered out");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn activating_a_directory_descends_and_a_file_selects() {
        let dir = temp_dir("activate");
        let mut picker = FilePicker::new(Some(&dir), &["toml"], "map");
        // First entry is `sub`: descending returns nothing and changes directory.
        assert!(picker.activate().is_none());
        assert!(picker.dir().ends_with("sub"));
        let picked = picker.activate().expect("c.toml should be selectable");
        assert!(picked.ends_with("c.toml"));
        // Going up returns to the original directory.
        picker.up();
        assert_eq!(picker.dir(), dir.as_path());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_empty_extension_list_shows_every_file() {
        let dir = temp_dir("all");
        let picker = FilePicker::new(Some(&dir), &[], "any");
        assert_eq!(picker.entries().len(), 3, "sub + a.toml + b.txt");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_bare_filename_starts_in_the_current_directory() {
        let picker = FilePicker::new(Some(Path::new("midi-map.toml")), &["toml"], "map");
        assert_eq!(picker.dir(), std::env::current_dir().unwrap().as_path());
        assert!(picker.error().is_none(), "{:?}", picker.error());
    }

    #[test]
    fn selection_wraps_and_a_missing_directory_reports_an_error() {
        let mut picker = FilePicker::new(Some(Path::new("/definitely/not/here")), &[], "x");
        assert!(picker.entries().is_empty());
        assert!(picker.error().is_some());
        // Moving on an empty list must not panic or divide by zero.
        picker.move_selection(1);
        assert_eq!(picker.selected(), 0);
    }
}
