//! Sixel transport for the waveform image.
//!
//! Sixel is immediate-mode: every refresh sends a complete image. The palette is tiny and fixed,
//! and the encoder emits runs per sixel band, which keeps a deck-sized image cheap enough for the
//! 30Hz target. The image includes the playhead because overlay text on sixel is not reliable
//! across terminals.

use std::fmt::Write;

use ratatui::layout::Rect;

use super::raster::{Palette, Raster, PALETTE_LEN};

/// Build the complete escape sequence placed in the first cell of the image area.
pub fn sequence(raster: &Raster, palette: &Palette, area: Rect) -> String {
    let mut out = String::with_capacity(raster.indices.len() / 2 + 512);
    clear_area(&mut out, area);

    // `P0;0;0` = default aspect/mode; `q` enters sixel mode.
    out.push_str("\x1bP0;0;0q");
    write!(out, "\"1;1;{};{}", raster.width, raster.height)
        .expect("writing to a String cannot fail");
    for index in 0..PALETTE_LEN {
        let [r, g, b] = palette.rgb(index as u8);
        write!(
            out,
            "#{index};2;{};{};{}",
            r as u32 * 100 / 255,
            g as u32 * 100 / 255,
            b as u32 * 100 / 255
        )
        .expect("writing to a String cannot fail");
    }

    let bands = raster.height.div_ceil(6);
    for band in 0..bands {
        for color in 0..PALETTE_LEN {
            // A colour with no pixels in this band needs no cursor movement: the previous pass
            // ended with `$`, so the next one still starts at the left edge.
            let present = (0..raster.width).any(|x| {
                (0..6).any(|bit| {
                    let y = band * 6 + bit;
                    y < raster.height
                        && usize::from(
                            raster.indices[(y as usize) * (raster.width as usize) + x as usize],
                        ) == color
                })
            });
            if !present {
                continue;
            }
            write!(out, "#{color}").expect("writing to a String cannot fail");
            let mut run_mask = 0u8;
            let mut run_len = 0usize;
            for x in 0..raster.width {
                let mut mask = 0u8;
                for bit in 0..6 {
                    let y = band * 6 + bit;
                    if y >= raster.height {
                        break;
                    }
                    let index = (y as usize) * (raster.width as usize) + x as usize;
                    if usize::from(raster.indices[index]) == color {
                        mask |= 1 << bit;
                    }
                }
                if mask == run_mask {
                    run_len += 1;
                } else {
                    push_run(&mut out, run_mask, run_len);
                    run_mask = mask;
                    run_len = 1;
                }
            }
            push_run(&mut out, run_mask, run_len);
            // Return to the start of this sixel line so the next palette colour overlays it.
            out.push('$');
        }
        out.push('-');
    }
    out.push_str("\x1b\\");
    out
}

fn push_run(out: &mut String, mask: u8, count: usize) {
    if count == 0 {
        return;
    }
    // Mask 0 is `?`: it draws nothing but still advances the sixel cursor over columns this
    // colour does not own. Skipping it would shift every later run left.
    let ch = char::from(b'?' + mask);
    if count > 3 {
        write!(out, "!{count}{ch}").expect("writing to a String cannot fail");
    } else {
        for _ in 0..count {
            out.push(ch);
        }
    }
}

fn clear_area(out: &mut String, area: Rect) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::preference::WaveformColors;

    #[test]
    fn a_flat_image_encodes_to_one_colour_and_runs() {
        let raster = Raster {
            width: 8,
            height: 6,
            indices: vec![0; 48],
        };
        let palette = Palette::from_colors(WaveformColors::default());
        let seq = sequence(&raster, &palette, Rect::new(0, 0, 8, 1));
        assert!(seq.starts_with("\x1b[0m\x1b7"));
        assert!(seq.contains("\x1bP0;0;0q\"1;1;8;6"));
        assert!(seq.contains("#0;2;0;0;0"));
        // Background is one run of eight pixels for each of the palette's colour passes.
        assert!(seq.contains("!8~"), "a full six-pixel band is six one-bits: {seq}");
        assert!(seq.ends_with("\x1b\\"));
    }

    #[test]
    fn absent_columns_advance_the_sixel_cursor() {
        let mut raster = Raster {
            width: 4,
            height: 6,
            indices: vec![0; 24],
        };
        for y in 0..6 {
            raster.indices[(y * 4 + 2) as usize] = 7;
        }
        let palette = Palette::from_colors(WaveformColors::default());
        let seq = sequence(&raster, &palette, Rect::new(0, 0, 4, 1));
        // The playhead owns column 2; the two zero-mask columns before it must still be emitted
        // (`?`) or the colour would be drawn at the left edge instead.
        assert!(seq.contains("#7??~?$"), "{seq}");
    }

    #[test]
    fn the_encoder_uses_the_requested_palette() {
        let palette = Palette::from_colors(WaveformColors {
            low: ratatui::style::Color::Rgb(255, 0, 0),
            mid: ratatui::style::Color::Rgb(0, 255, 0),
            high: ratatui::style::Color::Rgb(0, 0, 255),
        });
        let raster = Raster {
            width: 1,
            height: 6,
            indices: vec![1; 6],
        };
        let seq = sequence(&raster, &palette, Rect::new(0, 0, 1, 1));
        assert!(seq.contains("#1;2;100;0;0"), "low red: {seq}");
    }
}
