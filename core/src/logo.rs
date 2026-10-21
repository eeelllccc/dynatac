//! The DYNATAC wordmark: seven chunky, all-caps 8×8 letters, drawn at an
//! integer scale. Used large on the lockscreen and small in the status bar.
//!
//! The letter bitmaps are deliberately separate from the text fonts in
//! `font`: the wordmark is a fixed graphic, not text.
//!
//! Geometry at scale (sx, sy):
//!   - Each letter occupies an `8sx × 8sy` box; the logo is `56sx × 8sy`.
//!   - Ink sits in the top `INK_ROWS` source rows of each box; the bottom
//!     two rows are blank. Use `ink_height` to centre the logo vertically.

use crate::framebuffer::Ink;
use crate::surface::Surface;

/// The letters of the logo, in order.
pub const LETTERS: [char; 7] = ['D', 'Y', 'N', 'A', 'T', 'A', 'C'];

/// Source rows (of 8) that contain ink.
pub const INK_ROWS: u16 = 6;

/// Size of the logo's bounding box at the given scale, as (w, h).
pub const fn size(scale_x: u16, scale_y: u16) -> (u16, u16) {
    (8 * scale_x * LETTERS.len() as u16, 8 * scale_y)
}

/// Height of the inked part of the logo at vertical scale `scale_y`.
pub const fn ink_height(scale_y: u16) -> u16 {
    INK_ROWS * scale_y
}

/// Draw the logo with its bounding box's top-left at local (`x`, `y`).
/// Only letter pixels are painted (in `ink`); the background is left
/// as it is, so fill it first if needed.
pub fn draw(s: &mut Surface, x: u16, y: u16, scale_x: u16, scale_y: u16, ink: Ink) {
    for (i, &ch) in LETTERS.iter().enumerate() {
        let Some(glyph) = letter(ch) else { continue };
        let letter_x = x + i as u16 * 8 * scale_x;
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..8u16 {
                if bits & (0x80 >> col) != 0 {
                    let px = letter_x + col * scale_x;
                    let py = y + row as u16 * scale_y;
                    s.fill(crate::geometry::Rect::new(px, py, scale_x, scale_y), ink);
                }
            }
        }
    }
}

/// 8×8 bitmap for one logo letter: 8 rows top to bottom, bit 7 = leftmost
/// pixel, 1 = ink. `None` for letters not in the logo.
fn letter(ch: char) -> Option<[u8; 8]> {
    #[rustfmt::skip]
    let glyph = match ch {
        'A' => [0x18,0x24,0x42,0x7E,0x42,0x42,0x00,0x00],
        'C' => [0x3C,0x42,0x40,0x40,0x42,0x3C,0x00,0x00],
        'D' => [0x78,0x44,0x42,0x42,0x44,0x78,0x00,0x00],
        'N' => [0x42,0x62,0x52,0x4A,0x46,0x42,0x00,0x00],
        'T' => [0x7E,0x08,0x08,0x08,0x08,0x08,0x00,0x00],
        'Y' => [0x42,0x24,0x18,0x08,0x08,0x08,0x00,0x00],
        _ => return None,
    };
    Some(glyph)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::{FrameBuffer, BYTES_PER_ROW};
    use crate::geometry::Rect;

    #[test]
    fn every_letter_has_a_glyph() {
        for ch in LETTERS {
            assert!(letter(ch).is_some(), "{:?}", ch);
        }
    }

    #[test]
    fn ink_stays_in_top_rows() {
        for ch in LETTERS {
            let g = letter(ch).unwrap();
            assert!(g[INK_ROWS as usize..].iter().all(|&b| b == 0), "{:?}", ch);
        }
    }

    #[test]
    fn size_scales() {
        assert_eq!(size(1, 1), (56, 8));
        assert_eq!(size(4, 5), (224, 40));
        assert_eq!(ink_height(5), 30);
    }

    #[test]
    fn white_logo_on_black_paints_white_pixels() {
        let mut fb = FrameBuffer::new();
        let mut s = Surface::new(&mut fb, Rect::new(0, 0, 64, 8));
        s.fill_all(Ink::Black);
        draw(&mut s, 0, 0, 1, 1, Ink::White);
        // Top row of 'D' is 0x78: pixels 1..5 white, pixel 0 black.
        assert_eq!(fb.desired_byte(0), 0x78);
        // Row 6 (blank in every letter) stays black.
        assert_eq!(fb.desired_byte(6 * BYTES_PER_ROW), 0x00);
    }
}
