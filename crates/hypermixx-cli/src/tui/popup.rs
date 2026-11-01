//! Every modal, in both front-ends, is the same widget: **one flat list in a centred box**.
//!
//! `→` (or `Enter`) goes one level deeper, `←` comes back, `Esc` closes. A deeper level replaces the
//! list inside the same box, so the eye never moves and there is never a tree, a second pane, or a
//! nested window. The settings screen, the controller guide's device chooser, its menus and both
//! file/port pickers all draw through here, which is what keeps them looking like one program.
//!
//! Colours come from `preference.toml` only: accent for the border, the cursor bar and the row that
//! is on the path to the focus; dim for hints and notes; text for everything else. The title is
//! plain text — no highlighted bar.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use super::text::{pad_to_width, truncate_to_width};
use crate::preference::Theme;

/// One row of a modal list.
#[derive(Clone, Debug)]
pub struct Row {
    pub label: String,
    /// Secondary text, right of the label and always dim.
    pub note: String,
    /// A one-character gutter mark: `●` current binding, `◆` this controller's hardware.
    pub mark: char,
    pub dim: bool,
}

impl Default for Row {
    fn default() -> Self {
        Self {
            label: String::new(),
            note: String::new(),
            // A blank gutter, not `char`'s NUL: it has to occupy a cell.
            mark: ' ',
            dim: false,
        }
    }
}

impl Row {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    pub fn mark(mut self, mark: char) -> Self {
        self.mark = mark;
        self
    }

    /// A row that is informational only: a path, a section heading, a device's channel pair.
    pub fn dim(mut self) -> Self {
        self.dim = true;
        self
    }
}

/// A modal: a title, a flat list, one cursor, one hint line.
#[derive(Clone, Debug, Default)]
pub struct View {
    /// The title, without the surrounding spaces. Deeper levels append ` › level`.
    pub title: String,
    pub rows: Vec<Row>,
    pub cursor: usize,
    pub hint: String,
    /// A line between the list and the hint, for a typed file name.
    pub footer: Option<String>,
}

impl View {
    pub fn new(title: impl Into<String>, rows: Vec<Row>) -> Self {
        Self {
            title: title.into(),
            rows,
            cursor: 0,
            hint: String::new(),
            footer: None,
        }
    }

    pub fn cursor(mut self, cursor: usize) -> Self {
        self.cursor = cursor.min(self.rows.len().saturating_sub(1));
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = hint.into();
        self
    }

    pub fn footer(mut self, footer: impl Into<String>) -> Self {
        self.footer = Some(footer.into());
        self
    }
}

/// The box `view` needs: wide enough for its widest row, tall enough for its rows, never larger
/// than the screen (minus a one-cell margin so the deck still shows around it).
pub fn rect(area: Rect, view: &View) -> Rect {
    let widest = view
        .rows
        .iter()
        .map(|row| row.label.chars().count() + row.note.chars().count() + 6)
        .chain(std::iter::once(view.title.chars().count() + 4))
        // +3: the leading space the hint is drawn with, and the two borders.
        .chain(std::iter::once(view.hint.chars().count() + 3))
        .max()
        .unwrap_or(0) as u16;
    let height = view.rows.len() as u16 + 3 + u16::from(view.footer.is_some());
    let width = widest.clamp(28, area.width.saturating_sub(2).max(28));
    let height = height.min(area.height.saturating_sub(2).max(4));
    centred(area, width, height)
}

/// Draws `view` over `area`. Callers draw it last: a modal owns the screen.
pub fn render(frame: &mut Frame, area: Rect, theme: &Theme, view: &View) {
    let footer = view.footer.is_some();
    let popup = rect(area, view);
    if popup.width < 12 || popup.height < 4 {
        return;
    }
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_style(Style::default().fg(theme.accent))
        .title(format!(" {} ", view.title));
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    if inner.height == 0 {
        return;
    }
    let rows_height = inner.height.saturating_sub(1 + u16::from(footer)) as usize;
    let start = view.cursor.saturating_sub(rows_height.saturating_sub(1));
    let label_width = view
        .rows
        .iter()
        .map(|row| row.label.chars().count())
        .max()
        .unwrap_or(0)
        .min(inner.width.saturating_sub(12) as usize);
    let mut lines = Vec::with_capacity(rows_height);
    for (index, row) in view.rows.iter().enumerate().skip(start).take(rows_height) {
        let focused = index == view.cursor;
        let style = if focused {
            Style::default()
                .fg(Color::Black)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD)
        } else if row.dim {
            Style::default().fg(theme.dim)
        } else {
            Style::default().fg(theme.text)
        };
        let note_style = if focused {
            style
        } else {
            Style::default().fg(theme.dim)
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(
                    "{}{} {} ",
                    if focused { ">" } else { " " },
                    row.mark,
                    pad_to_width(&row.label, label_width)
                ),
                style,
            ),
            Span::styled(
                truncate_to_width(&row.note, inner.width as usize),
                note_style,
            ),
        ]));
    }
    if lines.is_empty() && rows_height > 0 {
        lines.push(Line::from(Span::styled(
            " (nothing here)",
            Style::default().fg(theme.dim),
        )));
    }
    frame.render_widget(Paragraph::new(lines), inner);
    let mut y = inner.y + rows_height as u16;
    if let Some(footer) = &view.footer {
        if y < inner.bottom() {
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(" ", Style::default()),
                    Span::styled(footer.clone(), Style::default().fg(theme.text)),
                ])),
                Rect::new(inner.x, y, inner.width, 1),
            );
            y += 1;
        }
    }
    if y < inner.bottom() && !view.hint.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                truncate_to_width(&format!(" {}", view.hint), inner.width as usize),
                Style::default().fg(theme.dim),
            ))),
            Rect::new(inner.x, y, inner.width, 1),
        );
    }
}

fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw(view: &View, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = Theme::default();
        terminal
            .draw(|frame| render(frame, frame.area(), &theme, view))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    #[test]
    fn the_title_is_plain_and_the_cursor_is_the_only_highlight() {
        let view = View::new(
            "controller › input",
            vec![Row::new("Party Mix III").mark('◆'), Row::new("Virtual")],
        )
        .hint("↑↓ move · → pick · Esc close");
        let text = draw(&view, 80, 12);
        assert!(text.contains(" controller › input "), "{text}");
        assert!(text.contains(">◆ Party Mix III"), "{text}");
        assert!(text.contains("Esc close"), "{text}");
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_cursor_visible() {
        let rows: Vec<Row> = (0..40).map(|i| Row::new(format!("row{i}"))).collect();
        let view = View::new("list", rows).cursor(30);
        let text = draw(&view, 40, 10);
        assert!(text.contains(">  row30"), "{text}");
        assert!(!text.contains(" row0  "), "{text}");
    }

    #[test]
    fn one_row_is_enough_to_draw_and_nothing_panics_when_it_does_not_fit() {
        let view = View::new("tiny", vec![Row::new("a")]);
        assert!(!draw(&view, 8, 4).is_empty());
        assert_eq!(draw(&view, 2, 2), "  \n  \n");
    }
}
