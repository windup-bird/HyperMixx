//! The track browser: folders/playlists on the left, the selected folder's tracks on the right.
//!
//! This is deliberately only the frame for now: the panes are laid out and the data model is in
//! place, but scanning, metadata and selection are the next step. Keeping it state-only (like
//! `picker.rs`) means the layout can be tested without a filesystem or an engine.

use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::tui::text;

/// Which side of the browser owns the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Folders,
    Tracks,
}

/// One entry in the left-hand folder / playlist pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderEntry {
    pub name: String,
    pub path: PathBuf,
}

/// One row in the right-hand track list. Filled in when scanning lands; empty for now.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackRow {
    pub title: String,
    pub artist: String,
    pub bpm: Option<f32>,
    pub key: Option<String>,
    pub duration_frames: u64,
    pub path: PathBuf,
}

/// Browser state. Rendering reads it; navigation will mutate it.
pub struct Browser {
    /// Root the folder pane opens at (today only shown once scanning exists).
    pub root: PathBuf,
    pub pane: Pane,
    pub folders: Vec<FolderEntry>,
    pub tracks: Vec<TrackRow>,
    #[allow(dead_code)] // P2 navigation state; kept now so the struct shape does not churn later.
    pub folder_selected: usize,
    #[allow(dead_code)] // P2 navigation state; see above.
    pub track_selected: usize,
}

impl Browser {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            pane: Pane::Folders,
            folders: Vec::new(),
            tracks: Vec::new(),
            folder_selected: 0,
            track_selected: 0,
        }
    }
}

/// Draws the browser into `area`. `focused` is true while the browser is the command target.
pub fn render(frame: &mut Frame, area: Rect, browser: &Browser, focused: bool) {
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

    let (folders_area, divider_area, tracks_area) = split_panes(inner);
    render_folders(frame, folders_area, browser);
    if divider_area.width > 0 {
        frame.render_widget(
            Block::default()
                .borders(Borders::LEFT)
                .border_style(Style::default().fg(border)),
            divider_area,
        );
    }
    render_tracks(frame, tracks_area, browser);
}

/// Folder pane : separator (1 cell) : track pane = 1 : 3.
fn split_panes(inner: Rect) -> (Rect, Rect, Rect) {
    let usable = inner.width.saturating_sub(1);
    let folders_width = usable / 4;
    let tracks_width = usable - folders_width;
    let folders = Rect::new(inner.x, inner.y, folders_width, inner.height);
    let divider = Rect::new(inner.x + folders_width, inner.y, 1, inner.height);
    let tracks = Rect::new(
        inner.x + folders_width + 1,
        inner.y,
        tracks_width,
        inner.height,
    );
    (folders, divider, tracks)
}

fn render_folders(frame: &mut Frame, area: Rect, browser: &Browser) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut lines = vec![Line::from(heading(
        " folders/playlists",
        browser.pane == Pane::Folders,
    ))];
    let root = browser.root.display().to_string();
    lines.push(Line::styled(
        format!(
            " {}",
            text::truncate_to_width(&root, area.width.saturating_sub(2) as usize)
        ),
        Style::default().fg(Color::DarkGray),
    ));
    if browser.folders.is_empty() {
        lines.push(Line::styled(
            " (empty)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn render_tracks(frame: &mut Frame, area: Rect, browser: &Browser) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let mut lines = vec![Line::from(vec![
        heading(" title", browser.pane == Pane::Tracks),
        heading("    artist", browser.pane == Pane::Tracks),
        heading("    bpm", browser.pane == Pane::Tracks),
        heading("  key", browser.pane == Pane::Tracks),
        heading("      dur", browser.pane == Pane::Tracks),
    ])];
    if browser.tracks.is_empty() {
        lines.push(Line::styled(
            " (empty)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), area);
}

fn heading(text: &str, active: bool) -> Span<'static> {
    let style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };
    Span::styled(text.to_owned(), style)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_panes_are_one_to_three_with_a_one_cell_divider() {
        let inner = Rect::new(2, 3, 81, 10);
        let (folders, divider, tracks) = split_panes(inner);
        assert_eq!(divider.width, 1);
        assert_eq!(folders.width + divider.width + tracks.width, inner.width);
        assert_eq!(folders.width, 20);
        assert_eq!(tracks.width, 60);
    }

    #[test]
    fn a_tiny_browser_does_not_overflow() {
        let (folders, divider, tracks) = split_panes(Rect::new(0, 0, 1, 1));
        assert_eq!(folders.width, 0);
        assert_eq!(divider.width, 1);
        assert_eq!(tracks.width, 0);
    }

    #[test]
    fn a_new_browser_starts_empty_on_the_folder_pane() {
        let browser = Browser::new(PathBuf::from("/music"));
        assert_eq!(browser.pane, Pane::Folders);
        assert!(browser.folders.is_empty());
        assert!(browser.tracks.is_empty());
    }
}
