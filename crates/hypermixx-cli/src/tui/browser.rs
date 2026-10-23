//! The track browser: a Miller column stack on the left and the current list's tracks on the right.
//!
//! The browser owns only view state. The database is passed to the few methods that need to load a
//! column or a track, which keeps rendering testable without a filesystem or an engine.

use std::time::Instant;

use hypermixx_core::KeyFormat;
use hypermixx_library::{LibraryDb, Playlist, Source, TrackRow};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::response::time;
use crate::tui::text;

/// Which side of the browser owns the arrow keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Lists,
    Tracks,
}

/// One row in a left-hand column: a library at the root, or a list below it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    Source(Source),
    Playlist(Playlist),
}

impl Entry {
    pub(crate) fn label(&self) -> String {
        match self {
            Self::Source(source) => source.display_name(),
            Self::Playlist(playlist) => playlist.name.clone(),
        }
    }

    /// Glyph for the entry's kind. The colour is the panel's theme highlight, so a row is
    /// distinguishable by shape rather than by a per-kind palette.
    fn icon(&self) -> &'static str {
        match self {
            Self::Source(source) => match source.origin {
                hypermixx_library::SourceOrigin::Usb => "🔌",
                hypermixx_library::SourceOrigin::Local => "💻",
                hypermixx_library::SourceOrigin::Folder => "📁",
            },
            Self::Playlist(playlist) => match playlist.kind.as_str() {
                "folder" => "📁",
                _ => "▤",
            },
        }
    }
}

/// One visible level of the tree. The last two columns are drawn; the rest are the history that
/// `←` walks back through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Column {
    pub title: String,
    pub entries: Vec<Entry>,
    pub selected: usize,
}

impl Column {
    fn sources(sources: Vec<Source>) -> Self {
        Self {
            title: "libraries".to_owned(),
            entries: sources.into_iter().map(Entry::Source).collect(),
            selected: 0,
        }
    }

    fn playlists(title: String, playlists: Vec<Playlist>) -> Self {
        Self {
            title,
            entries: playlists.into_iter().map(Entry::Playlist).collect(),
            selected: 0,
        }
    }

    fn selected_entry(&self) -> Option<&Entry> {
        self.entries
            .get(self.selected.min(self.entries.len().saturating_sub(1)))
    }

    fn move_selection(&mut self, delta: isize) {
        if self.entries.is_empty() {
            self.selected = 0;
            return;
        }
        let last = self.entries.len() - 1;
        self.selected = (self.selected as isize + delta).clamp(0, last as isize) as usize;
    }
}

/// Browser state. `columns` is the path from the library root to the current list; the two rightmost
/// columns are the ones on screen.
/// Where the last render put the panes and how far each one is scrolled. Mouse handling reads this
/// instead of duplicating the layout maths.
#[derive(Clone, Debug, Default)]
pub struct BrowserLayout {
    pub left1: Rect,
    pub left2: Rect,
    pub tracks: Rect,
    /// Entry index for each rendered visual line (list names may wrap).
    pub left1_lines: Vec<usize>,
    pub left2_lines: Vec<usize>,
    pub tracks_start: usize,
}

pub struct Browser {
    pub columns: Vec<Column>,
    pub tracks: Vec<TrackRow>,
    pub track_selected: usize,
    pub focus: Focus,
    /// How track keys are rendered in the right-hand list.
    pub key_format: KeyFormat,
    pub layout: BrowserLayout,
    /// Last track row click, for double-click detection.
    pub last_track_click: Option<(usize, Instant)>,
}

impl Default for Browser {
    fn default() -> Self {
        Self::new()
    }
}

impl Browser {
    pub fn new() -> Self {
        Self {
            columns: vec![Column::sources(Vec::new())],
            tracks: Vec::new(),
            track_selected: 0,
            focus: Focus::Lists,
            key_format: KeyFormat::Camelot,
            layout: BrowserLayout::default(),
            last_track_click: None,
        }
    }

    /// Loads every source in the cache. Tests and the REPL use this; the TUI passes the configured
    /// subset through [`Browser::load_sources`].
    #[allow(dead_code)] // Used by the browser tests; the TUI passes a filtered source list.
    pub fn load(db: &LibraryDb) -> hypermixx_library::Result<Self> {
        Self::load_sources(db, db.sources()?)
    }

    /// Loads an explicit source list and opens the first one.
    pub fn load_sources(
        db: &LibraryDb,
        sources: Vec<Source>,
    ) -> hypermixx_library::Result<Self> {
        let mut browser = Self::new();
        browser.columns = vec![Column::sources(sources.clone())];
        if let Some(source) = sources.first() {
            let lists = db.root_playlists(source.id)?;
            browser
                .columns
                .push(Column::playlists(source.display_name(), lists));
            browser.refresh_tracks(db)?;
        }
        Ok(browser)
    }

    pub fn focus(&self) -> Focus {
        self.focus
    }

    pub fn selected_track(&self) -> Option<&TrackRow> {
        self.tracks.get(self.track_selected)
    }

    /// Terminal cell to anchor the deck chooser for the selected track.
    pub fn track_anchor(&self) -> (u16, u16) {
        let row = self.track_selected.saturating_sub(self.layout.tracks_start) as u16;
        (
            self.layout.tracks.x.saturating_add(2),
            self.layout.tracks.y.saturating_add(1).saturating_add(row),
        )
    }

    /// Selects a row in a visible column. Clicking a parent column truncates the deeper columns.
    pub fn select(
        &mut self,
        column: usize,
        row: usize,
        db: &LibraryDb,
    ) -> hypermixx_library::Result<()> {
        if column >= self.columns.len() {
            return Ok(());
        }
        self.columns[column].selected =
            row.min(self.columns[column].entries.len().saturating_sub(1));
        if column + 1 < self.columns.len() {
            self.columns.truncate(column + 1);
        }
        self.focus = Focus::Lists;
        self.last_track_click = None;
        self.refresh_tracks(db)
    }

    /// The visible list row under a terminal cell, as `(column index, entry index)`.
    pub fn hit_column(&self, x: u16, y: u16) -> Option<(usize, usize)> {
        let len = self.columns.len();
        let candidates = [
            (len.checked_sub(2), self.layout.left1, &self.layout.left1_lines),
            (len.checked_sub(1), self.layout.left2, &self.layout.left2_lines),
        ];
        for (column, rect, lines) in candidates {
            let Some(column) = column else { continue };
            if !contains(rect, x, y) {
                continue;
            }
            let line = y.saturating_sub(rect.y) as usize;
            if let Some(entry) = lines.get(line) {
                if *entry < self.columns[column].entries.len() {
                    return Some((column, *entry));
                }
            }
        }
        None
    }

    /// The visible track row under a terminal cell.
    pub fn hit_track(&self, x: u16, y: u16) -> Option<usize> {
        let rect = self.layout.tracks;
        if !contains(rect, x, y) || y <= rect.y {
            return None;
        }
        let row = self.layout.tracks_start + (y - rect.y - 1) as usize;
        (row < self.tracks.len()).then_some(row)
    }

    /// Moves inside the focused pane. In the list pane that also refreshes the track preview.
    pub fn move_selection(
        &mut self,
        delta: isize,
        db: &LibraryDb,
    ) -> hypermixx_library::Result<()> {
        match self.focus {
            Focus::Tracks => {
                if !self.tracks.is_empty() {
                    let last = self.tracks.len() - 1;
                    self.track_selected =
                        (self.track_selected as isize + delta).clamp(0, last as isize) as usize;
                }
            }
            Focus::Lists => {
                if let Some(column) = self.columns.last_mut() {
                    column.move_selection(delta);
                }
                self.refresh_tracks(db)?;
            }
        }
        Ok(())
    }

    /// `→` / `open`: descend into the selected list, or enter the track pane at the bottom.
    pub fn open(&mut self, db: &LibraryDb) -> hypermixx_library::Result<()> {
        if self.focus == Focus::Tracks {
            return Ok(());
        }
        let Some(entry) = self
            .columns
            .last()
            .and_then(Column::selected_entry)
            .cloned()
        else {
            return Ok(());
        };
        match entry {
            Entry::Source(source) => {
                let lists = db.root_playlists(source.id)?;
                self.columns
                    .push(Column::playlists(source.display_name(), lists));
                self.refresh_tracks(db)?;
            }
            Entry::Playlist(playlist) => {
                let children = db.child_playlists(playlist.source_id, playlist.id)?;
                if children.is_empty() {
                    self.focus = Focus::Tracks;
                } else {
                    self.columns
                        .push(Column::playlists(playlist.name.clone(), children));
                    self.refresh_tracks(db)?;
                }
            }
        }
        Ok(())
    }

    /// `←` / `exit`: leave the track pane, else pop one level. The root is a no-op.
    pub fn back(&mut self, db: &LibraryDb) -> hypermixx_library::Result<()> {
        if self.focus == Focus::Tracks {
            self.focus = Focus::Lists;
            return Ok(());
        }
        if self.columns.len() > 1 {
            self.columns.pop();
            self.refresh_tracks(db)?;
        }
        Ok(())
    }

    /// Refreshes the right-hand preview from the selected list. Sources have no tracks themselves.
    pub fn refresh_tracks(&mut self, db: &LibraryDb) -> hypermixx_library::Result<()> {
        let playlist = self
            .columns
            .last()
            .and_then(Column::selected_entry)
            .and_then(|entry| match entry {
                Entry::Playlist(playlist) => Some(playlist.clone()),
                Entry::Source(_) => None,
            });
        self.tracks = match playlist {
            Some(playlist) => db.tracks_in_playlist(playlist.id)?,
            None => Vec::new(),
        };
        if self.tracks.is_empty() {
            self.track_selected = 0;
        } else {
            self.track_selected = self.track_selected.min(self.tracks.len() - 1);
        }
        Ok(())
    }
}

/// Draws the browser into `area`. `focused` is true while the browser is the command target.
pub fn render(frame: &mut Frame, area: Rect, browser: &mut Browser, focused: bool) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let border = if focused {
        Color::Cyan
    } else {
        Color::DarkGray
    };
    let block = Block::bordered()
        .border_style(Style::default().fg(border))
        .title(" browser ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    let parts = split_panes(inner);
    browser.layout.left1 = parts[0];
    browser.layout.left2 = parts[2];
    browser.layout.tracks = parts[4];
    let left1_index = browser.columns.len().checked_sub(2);
    let left1 = left1_index.and_then(|index| browser.columns.get(index));
    let left2_index = browser.columns.len().checked_sub(1);
    let left2 = browser.columns.last();
    browser.layout.left1_lines = render_column(frame, parts[0], left1, false, focused, border);
    render_divider(frame, parts[1], border);
    browser.layout.left2_lines = render_column(
        frame,
        parts[2],
        left2,
        browser.focus == Focus::Lists,
        focused,
        border,
    );
    let _ = left2_index;
    render_divider(frame, parts[3], border);
    browser.layout.tracks_start = render_tracks(
        frame,
        parts[4],
        browser,
        browser.focus == Focus::Tracks,
        focused,
    );
}

/// Two list columns : tracks = 1 : 1 : 2, with one-cell separators. The result always adds up to
/// `inner.width` so a narrow terminal cannot push the right column off screen.
fn split_panes(inner: Rect) -> [Rect; 5] {
    let dividers = inner.width.min(2);
    let usable = inner.width.saturating_sub(dividers);
    if usable == 0 {
        return [
            Rect::new(inner.x, inner.y, 0, inner.height),
            Rect::new(inner.x, inner.y, dividers.min(1), inner.height),
            Rect::new(inner.x + dividers.min(1), inner.y, 0, inner.height),
            Rect::new(
                inner.x + dividers.min(1),
                inner.y,
                dividers.saturating_sub(1),
                inner.height,
            ),
            Rect::new(inner.x + dividers, inner.y, 0, inner.height),
        ];
    }
    let first = usable * 15 / 100;
    let second = usable * 15 / 100;
    let tracks = usable - first - second;
    let x0 = inner.x;
    let x1 = x0 + first;
    let x2 = x1 + 1;
    let x3 = x2 + second;
    let x4 = x3 + 1;
    [
        Rect::new(x0, inner.y, first, inner.height),
        Rect::new(x1, inner.y, 1.min(dividers), inner.height),
        Rect::new(x2, inner.y, second, inner.height),
        Rect::new(x3, inner.y, 1.min(dividers.saturating_sub(1)), inner.height),
        Rect::new(x4, inner.y, tracks, inner.height),
    ]
}

fn render_divider(frame: &mut Frame, area: Rect, colour: Color) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    frame.render_widget(
        Block::default()
            .borders(Borders::LEFT)
            .border_style(Style::default().fg(colour)),
        area,
    );
}

fn render_column(
    frame: &mut Frame,
    area: Rect,
    column: Option<&Column>,
    active: bool,
    browser_focused: bool,
    border: Color,
) -> Vec<usize> {
    let Some(column) = column else {
        return Vec::new();
    };
    if area.width == 0 || area.height == 0 {
        return Vec::new();
    }
    let height = area.height as usize;
    let width = area.width as usize;
    if column.entries.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::styled(
                " (empty)",
                Style::default().fg(Color::DarkGray),
            )),
            area,
        );
        return Vec::new();
    }

    // Build every visual line first: a long playlist name wraps, and a continuation line keeps the
    // icon's indent so the selection still reads as one block.
    let mut blocks: Vec<Vec<(bool, String, String)>> = Vec::with_capacity(column.entries.len());
    for entry in &column.entries {
        let icon = entry.icon();
        let prefix = format!(" {icon} ");
        let indent = " ".repeat(text::width(&prefix));
        let budget = width.saturating_sub(text::width(&prefix)).max(1);
        let label = entry.label();
        let wrapped = text::wrap_to_width(&label, budget);
        let mut lines = Vec::with_capacity(wrapped.len());
        for (index, part) in wrapped.into_iter().enumerate() {
            if index == 0 {
                lines.push((true, prefix.clone(), part));
            } else {
                lines.push((false, indent.clone(), part));
            }
        }
        blocks.push(lines);
    }

    let selected = column.selected.min(column.entries.len() - 1);
    let selected_start = blocks.iter().take(selected).map(Vec::len).sum::<usize>();
    let selected_end = selected_start + blocks[selected].len();
    let start_line = selected_end.saturating_sub(height);
    let mut lines = Vec::with_capacity(height);
    let mut visible_entries = Vec::with_capacity(height);
    let mut line_number = 0usize;
    for (entry_index, block) in blocks.iter().enumerate() {
        for (first, prefix, label) in block {
            if line_number < start_line {
                line_number += 1;
                continue;
            }
            if lines.len() >= height {
                break;
            }
            let selected = entry_index == column.selected;
            let style = if selected {
                if active && browser_focused {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                }
            } else {
                Style::default()
            };
            let icon_style = if selected {
                style
            } else if *first {
                Style::default().fg(border)
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.clone(), icon_style),
                Span::styled(label.clone(), style),
            ]));
            visible_entries.push(entry_index);
            line_number += 1;
        }
        if lines.len() >= height {
            break;
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
    visible_entries
}

fn render_tracks(
    frame: &mut Frame,
    area: Rect,
    browser: &Browser,
    active: bool,
    browser_focused: bool,
) -> usize {
    if area.width == 0 || area.height == 0 {
        return 0;
    }
    let widths = track_widths(area.width as usize);
    let start = if browser.tracks.is_empty() {
        0
    } else {
        let height = area.height.saturating_sub(1) as usize;
        window_start(browser.track_selected, browser.tracks.len(), height)
    };
    let header_style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };
    let mut lines = vec![track_cells(
        &["title", "artist", "bpm", "key", "dur"],
        &widths,
        header_style,
    )];
    if browser.tracks.is_empty() {
        lines.push(Line::styled(
            " (empty)",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        let height = area.height.saturating_sub(1) as usize;
        for (index, track) in browser.tracks.iter().enumerate().skip(start).take(height) {
            let selected = index == browser.track_selected;
            let style = if selected {
                if active && browser_focused {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                }
            } else {
                Style::default()
            };
            lines.push(track_line(track, &widths, style, browser.key_format));
        }
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
    start
}

/// Title : artist : bpm : key : duration = approximately 55 : 45 of the flexible remainder, with
/// compact fixed-ish fields. The last column takes the remainder so the row never overflows.
fn track_widths(width: usize) -> [usize; 5] {
    const GAP: usize = 2;
    let usable = width.saturating_sub(GAP * 4);
    if usable < 24 {
        return [width, 0, 0, 0, 0];
    }
    let bpm = (usable * 8 / 100).clamp(3, 5);
    let key = (usable * 8 / 100).clamp(3, 5);
    let dur = (usable * 14 / 100).clamp(6, 9);
    let rest = usable.saturating_sub(bpm + key + dur);
    let title = (rest * 55 / 100).max(1);
    let artist = rest.saturating_sub(title);
    [title, artist, bpm, key, dur]
}

fn track_line(
    track: &TrackRow,
    widths: &[usize; 5],
    style: Style,
    key_format: KeyFormat,
) -> Line<'static> {
    let bpm = track
        .bpm
        .map(|bpm| format!("{bpm:.1}"))
        .unwrap_or_else(|| "--".to_owned());
    let key = track
        .key
        .map(|key| key.format(key_format))
        .unwrap_or_else(|| "--".to_owned());
    let duration = if track.duration_frames == 0 {
        "--:--".to_owned()
    } else {
        time(track.duration_frames, track.sample_rate)
    };
    track_cells(
        &[
            &track.title,
            track.artist.as_deref().unwrap_or("--"),
            &bpm,
            &key,
            &duration,
        ],
        widths,
        style,
    )
}

fn track_cells(values: &[&str; 5], widths: &[usize; 5], style: Style) -> Line<'static> {
    let mut spans = Vec::new();
    for (value, width) in values.iter().zip(widths) {
        if *width == 0 {
            continue;
        }
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(text::pad_to_width(value, *width), style));
    }
    Line::from(spans)
}

fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
}

fn window_start(selected: usize, len: usize, height: usize) -> usize {
    if height == 0 || len <= height {
        return 0;
    }
    selected.saturating_sub(height - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermixx_library::SourceKind;

    #[test]
    fn usb_sources_use_the_usb_icon_and_name_their_format() {
        let source = Source {
            id: 1,
            kind: SourceKind::Rekordbox,
            origin: hypermixx_library::SourceOrigin::Usb,
            name: "stick".to_owned(),
            root: std::path::PathBuf::from("/run/media/stick"),
            last_scan_at: None,
        };
        let entry = Entry::Source(source);
        assert_eq!(entry.icon(), "🔌");
        assert_eq!(entry.label(), "stick (onelibrary)");
    }

    #[test]
    fn local_sources_use_the_computer_icon() {
        let source = Source {
            id: 1,
            kind: SourceKind::Serato,
            origin: hypermixx_library::SourceOrigin::Local,
            name: "Music".to_owned(),
            root: std::path::PathBuf::from("/home/me/Music"),
            last_scan_at: None,
        };
        assert_eq!(Entry::Source(source).icon(), "💻");
    }

    #[test]
    fn playlists_share_the_crate_icon() {
        let playlist = Playlist {
            id: 1,
            source_id: 1,
            parent_id: None,
            name: "list".to_owned(),
            kind: "playlist".to_owned(),
            position: 0,
        };
        assert_eq!(Entry::Playlist(playlist).icon(), "▤");
    }

    #[test]
    fn the_list_columns_get_a_little_more_room() {
        let inner = Rect::new(2, 3, 81, 10);
        let parts = split_panes(inner);
        assert_eq!(parts[0].width, 11);
        assert_eq!(parts[2].width, 11);
        assert_eq!(parts[1].width, 1);
        assert_eq!(parts[3].width, 1);
        assert_eq!(parts[4].width, 57);
        let total: u16 = parts.iter().map(|part| part.width).sum();
        assert_eq!(total, inner.width);
    }

    #[test]
    fn a_tiny_browser_does_not_overflow() {
        let parts = split_panes(Rect::new(0, 0, 1, 1));
        let total: u16 = parts.iter().map(|part| part.width).sum();
        assert!(total <= 1);
    }

    #[test]
    fn a_new_browser_starts_in_the_list_pane() {
        let browser = Browser::new();
        assert_eq!(browser.focus, Focus::Lists);
        assert!(browser.tracks.is_empty());
        assert_eq!(browser.columns.len(), 1);
    }

    #[test]
    fn load_opens_the_first_source_and_back_walks_to_the_root() {
        let dir = std::env::temp_dir().join(format!(
            "hypermixx-browser-{}-{}",
            std::process::id(),
            hypermixx_library::library::db::unix_now()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = LibraryDb::open(dir.join("library.sqlite3")).unwrap();
        let source = db.add_folder_source(&dir, Some("Test")).unwrap();
        db.scan_source(source.id, &mut |_| {}).unwrap();

        let mut browser = Browser::load(&db).unwrap();
        assert_eq!(browser.columns.len(), 2, "sources + root list");
        assert_eq!(browser.focus, Focus::Lists);
        browser.back(&db).unwrap();
        assert_eq!(browser.columns.len(), 1, "back to the source column");
        browser.open(&db).unwrap();
        assert_eq!(browser.columns.len(), 2, "open the source again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn track_columns_fit_the_available_width() {
        for width in [30usize, 40, 61, 120] {
            let widths = track_widths(width);
            let gaps = widths.iter().filter(|width| **width > 0).count().saturating_sub(1) * 2;
            assert!(
                widths.iter().sum::<usize>() + gaps <= width,
                "width {width}: {widths:?}"
            );
            assert!(widths[0] > 0, "title must always have room");
        }
    }

    #[test]
    fn the_window_follows_the_selection() {
        assert_eq!(window_start(0, 20, 10), 0);
        assert_eq!(window_start(15, 20, 10), 6);
        assert_eq!(window_start(4, 20, 10), 0);
    }
}
