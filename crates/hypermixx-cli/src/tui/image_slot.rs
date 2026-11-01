//! A still image drawn into a rectangle through the Kitty graphics protocol.
//!
//! The waveform has its own transport because it scrolls; a cover and a track overview do not move,
//! so they only need the two operations this module keeps: transmit-once-then-place, and a way to
//! forget everything (after an overlay covered the area, or the rectangle changed shape).
//!
//! Two rules make it behave under a modal:
//!
//! * While a picker is open nothing is transmitted **or placed**, and the slot is forgotten, so the
//!   popup is drawn over plain cells instead of over an image the terminal keeps showing.
//! * The next frame after the popup closes transmits again and re-places, which is what stops the
//!   "stale image left behind at the waveform" artefact.

use std::num::NonZeroU16;

use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::Frame;

use super::waveform_view::{kitty, next_image_id};

const UNIT_WIDTH: CellDiffOption = CellDiffOption::ForcedWidth(NonZeroU16::new(1).unwrap());

/// One image placement, keyed by a fingerprint of what it was drawn from.
pub struct ImageSlot {
    id: u32,
    placement: u32,
    /// `None` forces a transmit on the next draw.
    key: Option<u64>,
    /// The rectangle the current image was placed in, in cells.
    area: Rect,
    /// The transmitted image's pixel size, needed to place it again unchanged.
    source: (u32, u32),
    /// The superseded image, deleted one frame later so the replacement is visible first.
    pending_delete: Option<u32>,
}

impl ImageSlot {
    pub fn new() -> Self {
        Self {
            id: next_image_id(),
            placement: next_image_id(),
            key: None,
            area: Rect::default(),
            source: (0, 0),
            pending_delete: None,
        }
    }

    /// Forgets the transmitted image. The next [`draw`](Self::draw) transmits again.
    pub fn reset(&mut self) {
        self.key = None;
    }

    /// Whether the terminal is currently holding an image for this slot.
    #[cfg(test)]
    pub fn is_transmitted(&self) -> bool {
        self.key.is_some()
    }

    /// Draws `build`'s RGBA image into `area`.
    ///
    /// `build` is only called when the image has to be re-transmitted (a new fingerprint, a new
    /// rectangle, or after a [`reset`](Self::reset)); an unchanged still costs one `a=p` escape per
    /// frame, which is what keeps it on screen while the rest of the frame is redrawn.
    pub fn draw(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        key: u64,
        build: impl FnOnce() -> (u32, u32, Vec<u8>),
    ) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let rebuild = self.key != Some(key) || self.area != area;
        self.area = area;
        self.key = Some(key);

        let mut sequence = String::new();
        if rebuild {
            let (width, height, rgba) = build();
            if width == 0 || height == 0 {
                return;
            }
            // Erase whatever was in the rectangle first (braille from a suppressed frame, or the
            // modal that covered it): the image is drawn behind text, so stale glyphs would show
            // through it.
            clear_cells(&mut sequence, area);
            let old = self.id;
            let new = next_image_id();
            self.placement = next_image_id();
            self.source = (width, height);
            sequence.push_str(&kitty::transmit_rgba(&rgba, width, height, new));
            sequence.push_str(&kitty::place(
                new,
                self.placement,
                area,
                kitty::SourceRect {
                    x: 0,
                    y: 0,
                    width,
                    height,
                },
            ));
            if let Some(pending) = self.pending_delete.take() {
                sequence.push_str(&kitty::delete(pending));
            }
            self.pending_delete = Some(old);
            self.id = new;
        } else {
            sequence.push_str(&kitty::place(
                self.id,
                self.placement,
                area,
                kitty::SourceRect {
                    x: 0,
                    y: 0,
                    width: self.source.0,
                    height: self.source.1,
                },
            ));
        }

        // Own the rectangle: the image is drawn by the terminal, so ratatui must not paint cells
        // over it, and the escape sequence rides in the first cell.
        let buffer = frame.buffer_mut();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_symbol(" ").set_diff_option(CellDiffOption::Skip);
                }
            }
        }
        if let Some(cell) = buffer.cell_mut((area.x, area.y)) {
            cell.set_symbol(&sequence)
                .set_fg(Color::Reset)
                .set_bg(Color::Reset)
                .set_style(Style::default())
                .set_diff_option(UNIT_WIDTH);
        }
    }
}

/// Erases an area's character cells without moving the final cursor.
fn clear_cells(out: &mut String, area: Rect) {
    use std::fmt::Write;
    out.push_str("\x1b[0m\x1b7");
    for row in 0..area.height {
        write!(
            out,
            "\x1b[{};{}H\x1b[{}X",
            area.y + row + 1,
            area.x + 1,
            area.width
        )
        .expect("writing to a String cannot fail");
    }
    out.push_str("\x1b8");
}

impl Default for ImageSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// The two still images a deck shows.
#[derive(Default)]
pub struct DeckImages {
    pub cover: ImageSlot,
    pub overview: ImageSlot,
}

impl DeckImages {
    pub fn new() -> Self {
        Self {
            cover: ImageSlot::new(),
            overview: ImageSlot::new(),
        }
    }

    /// Drops both transmitted images. The per-deck paths reset individually when an overlay covers
    /// them; this is the whole-deck escape hatch a modal opening uses.
    pub fn reset(&mut self) {
        self.cover.reset();
        self.overview.reset();
    }
}
