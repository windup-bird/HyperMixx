//! In-TUI pickers: a filesystem browser and a MIDI-port chooser.
//!
//! Both front-ends embed these, so a path or a port can be chosen without a startup flag. The
//! widgets only read state; navigation is small mutators, which keeps them unit-testable without a
//! terminal. Drawing is [`View`](crate::tui::popup::View) construction — the same flat list every
//! other modal uses.

use std::fs;
use std::path::{Path, PathBuf};

use hypermixx_midi::PortInfo;

use crate::tui::popup::{Row, View};

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
    /// `Some` in save mode: the file name to write in the current directory. Typing edits it; Enter
    /// overwrites that path. Open mode leaves it `None`.
    filename: Option<String>,
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
            filename: None,
        };
        picker.reload();
        picker
    }

    /// A save-as picker: the same browser plus an editable file name. The default name is used when
    /// the caller has a hint (the current map path) but no loaded file.
    pub fn new_save(
        start: Option<&Path>,
        extensions: &[&str],
        title: &str,
        default_name: &str,
    ) -> Self {
        let mut picker = Self::new(start, extensions, title);
        picker.filename = Some(default_name.to_owned());
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
                    b.is_dir.cmp(&a.is_dir).then_with(|| {
                        a.name
                            .to_ascii_lowercase()
                            .cmp(&b.name.to_ascii_lowercase())
                    })
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
    pub fn save_mode(&self) -> bool {
        self.filename.is_some()
    }

    #[cfg(test)]
    pub fn filename(&self) -> Option<&str> {
        self.filename.as_deref()
    }

    pub fn push_filename_char(&mut self, ch: char) {
        if let Some(name) = &mut self.filename {
            name.push(ch);
        }
    }

    pub fn pop_filename_char(&mut self) {
        if let Some(name) = &mut self.filename {
            name.pop();
        }
    }

    pub fn set_filename(&mut self, name: impl Into<String>) {
        if self.filename.is_some() {
            self.filename = Some(name.into());
        }
    }

    /// The path Enter writes in save mode: the typed name in the current directory.
    pub fn save_path(&self) -> Option<PathBuf> {
        let name = self.filename.as_deref()?.trim();
        (!name.is_empty()).then(|| self.dir.join(name))
    }

    /// The highlighted row, cloned so the caller can decide whether it is a directory.
    pub fn selected_entry(&self) -> Option<Entry> {
        self.entries.get(self.selected).cloned()
    }

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

    #[cfg(test)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    #[cfg(test)]
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    #[cfg(test)]
    pub fn selected(&self) -> usize {
        self.selected
    }

    #[cfg(test)]
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
                self.extensions
                    .iter()
                    .any(|wanted| wanted.eq_ignore_ascii_case(&ext))
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

}

impl FilePicker {
    /// The modal for this picker: directories first, `name/` for a directory, a dim path in the
    /// note column so it is obvious where Enter writes.
    pub fn view(&self) -> View {
        let mut rows: Vec<Row> = Vec::with_capacity(self.entries.len());
        if self.entries.is_empty() {
            rows.push(
                Row::new(self.error.as_deref().unwrap_or("(empty)"))
                    .note(self.dir.display().to_string())
                    .dim(),
            );
        }
        for entry in &self.entries {
            let label = if entry.is_dir {
                format!("{}/", entry.name)
            } else {
                entry.name.clone()
            };
            rows.push(Row::new(label));
        }
        let mut view = View::new(format!(" {} — {} ", self.title, self.dir.display()), rows)
            .cursor(self.selected)
            .hint(match self.save_mode() {
                true => "↑↓ move · → open · ⌦ up · ← back · ⌫ name · Enter save",
                false => "↑↓ move · →/Enter open · ⌦ up · ← back · Esc close",
            });
        if let Some(name) = &self.filename {
            view = view.footer(format!("name: {name}▌"));
        }
        view
    }
}

impl PortList {
    /// The modal for the port list. `title` is the screen's own title, e.g. `controller › input`.
    pub fn view(&self, title: &str) -> View {
        let mut rows: Vec<Row> = self
            .ports
            .iter()
            .map(|port| Row::new(port.name.clone()).note(port.index.to_string()))
            .collect();
        if rows.is_empty() {
            rows.push(
                Row::new(
                    self.error
                        .as_deref()
                        .unwrap_or("no MIDI input ports found")
                        .to_owned(),
                )
                .dim(),
            );
        }
        View::new(title, rows)
            .cursor(self.selected)
            .hint("↑↓ move · →/Enter connect · Esc close")
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("hypermixx-picker-{name}-{}", std::process::id()));
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
    fn save_mode_edits_a_filename_in_the_current_directory() {
        let dir = temp_dir("save");
        let mut picker = FilePicker::new_save(Some(&dir), &["toml"], "save", "map.toml");
        assert!(picker.save_mode());
        picker.pop_filename_char();
        picker.pop_filename_char();
        picker.pop_filename_char();
        picker.pop_filename_char();
        picker.push_filename_char('x');
        assert_eq!(picker.filename(), Some("map.x"));
        assert_eq!(picker.save_path(), Some(dir.join("map.x")));
        fs::remove_dir_all(&dir).ok();
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
