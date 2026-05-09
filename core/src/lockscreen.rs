// Lockscreen rendering: a chunky, all-caps DYNATAC logo across the
// bottom of the e-paper display.
//
// We don't ship a separate large font for this. Instead the existing
// 8×8 bitmap font in `framebuffer` is scaled up by an integer factor:
// each source pixel becomes a `SCALE_X × SCALE_Y` rectangle in the
// destination framebuffer. This is "chunky, straight-edged, all caps"
// by construction, and reuses code that's already shipping.
//
// Layout:
//   - Each glyph is 8×8 source → 32×40 destination (SCALE_X=4, SCALE_Y=5).
//   - Logo width = 7 letters × 32 = 224 px, centred in 240 → 8 px margins.
//   - Logo y position is `LOGO_Y_TOP`, leaving an 8 px margin from the
//     bottom of the 320-row display.
//
// Caller invariants:
//   - The framebuffer is the standard `WIDTH × HEIGHT` (240×320) buffer.
//   - The caller is responsible for flushing after `render`.

use crate::framebuffer::{glyph_8x8, FrameBuffer, HEIGHT, WIDTH};

/// Per-pixel horizontal scale.
pub const SCALE_X: u16 = 4;
/// Per-pixel vertical scale.
pub const SCALE_Y: u16 = 5;

/// Width of one scaled glyph in pixels.
pub const GLYPH_W: u16 = 8 * SCALE_X; // 32
/// Height of one scaled glyph in pixels.
pub const GLYPH_H: u16 = 8 * SCALE_Y; // 40

/// The letters of the logo, in order.
pub const LOGO_LETTERS: [char; 7] = ['D', 'Y', 'N', 'A', 'T', 'A', 'C'];

/// Total logo width in pixels.
pub const LOGO_WIDTH: u16 = GLYPH_W * (LOGO_LETTERS.len() as u16); // 224
/// Total logo height in pixels.
pub const LOGO_HEIGHT: u16 = GLYPH_H; // 40

/// Bottom margin between the logo and the bottom edge of the display.
pub const BOTTOM_MARGIN: u16 = 8;

/// Top-left x of the logo on the display (centred horizontally).
pub const LOGO_X_LEFT: u16 = (WIDTH - LOGO_WIDTH) / 2; // 8
/// Top-left y of the logo (anchored near the bottom of the display).
pub const LOGO_Y_TOP: u16 = HEIGHT - LOGO_HEIGHT - BOTTOM_MARGIN; // 272

/// Render the lockscreen into the framebuffer: clear everything to
/// white and stamp the DYNATAC logo across the bottom.
///
/// After this returns the framebuffer is fully dirty; the caller
/// should flush it with a partial update before powering the EPD down.
pub fn render(fb: &mut FrameBuffer) {
    fb.clear_all_desired();
    draw_logo(fb);
}

/// Draw just the logo into the framebuffer, leaving the rest of the
/// buffer untouched. Used by `render` and exposed separately for tests.
pub fn draw_logo(fb: &mut FrameBuffer) {
    for (letter_idx, &ch) in LOGO_LETTERS.iter().enumerate() {
        let glyph = match glyph_8x8(ch) {
            Some(g) => g,
            None => continue,
        };
        let glyph_x_offset = LOGO_X_LEFT + (letter_idx as u16) * GLYPH_W;

        for src_row in 0..8u16 {
            let row_byte = glyph[src_row as usize];
            for src_col in 0..8u16 {
                let bit = 7 - src_col;
                let on = (row_byte >> bit) & 1 == 1;
                if !on {
                    continue;
                }
                let dst_x = glyph_x_offset + src_col * SCALE_X;
                let dst_y = LOGO_Y_TOP + src_row * SCALE_Y;
                fb.fill_rect(dst_x, dst_y, SCALE_X, SCALE_Y, true);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::BYTES_PER_ROW;

    #[test]
    fn dimensions_match_display() {
        assert_eq!(LOGO_WIDTH, 224);
        assert_eq!(LOGO_HEIGHT, 40);
        assert_eq!(LOGO_X_LEFT, 8);
        assert_eq!(LOGO_X_LEFT + LOGO_WIDTH, 232);
        assert!(LOGO_X_LEFT + LOGO_WIDTH <= WIDTH);
        assert_eq!(LOGO_Y_TOP, 272);
        assert!(LOGO_Y_TOP + LOGO_HEIGHT + BOTTOM_MARGIN == HEIGHT);
    }

    #[test]
    fn render_clears_then_draws() {
        let mut fb = FrameBuffer::new();
        // Pre-soil the top of the framebuffer; render must wipe it.
        fb.draw_char(0, 0, 'X');
        render(&mut fb);

        // Top row should be white again.
        assert_eq!(fb.desired_byte(0), 0xFF);
        assert_eq!(fb.desired_byte(BYTES_PER_ROW - 1), 0xFF);
    }

    #[test]
    fn logo_paints_some_black_pixels_in_target_band() {
        let mut fb = FrameBuffer::new();
        render(&mut fb);

        // Count black bytes in the logo band.
        let band_start = LOGO_Y_TOP as usize * BYTES_PER_ROW;
        let band_end = (LOGO_Y_TOP + LOGO_HEIGHT) as usize * BYTES_PER_ROW;
        let mut nonwhite = 0;
        for i in band_start..band_end {
            if fb.desired_byte(i) != 0xFF {
                nonwhite += 1;
            }
        }
        assert!(
            nonwhite > 50,
            "expected the logo band to contain plenty of black pixels, got {}",
            nonwhite
        );
    }

    #[test]
    fn area_above_logo_stays_white() {
        let mut fb = FrameBuffer::new();
        render(&mut fb);

        // Everything above LOGO_Y_TOP must be white.
        for y in 0..LOGO_Y_TOP as usize {
            for col in 0..BYTES_PER_ROW {
                assert_eq!(
                    fb.desired_byte(y * BYTES_PER_ROW + col),
                    0xFF,
                    "non-white pixel above logo at y={} col={}",
                    y,
                    col
                );
            }
        }
    }

    #[test]
    fn d_top_row_has_black_pixels() {
        // Sanity: the 8x8 'D' glyph has bits set in row 0, so the
        // scaled-up logo should have black pixels at the top of the D.
        let mut fb = FrameBuffer::new();
        render(&mut fb);

        // The first row of the D in the framebuffer is at LOGO_Y_TOP.
        let row_start = LOGO_Y_TOP as usize * BYTES_PER_ROW;
        // Check the bytes covering the D's column range
        // (LOGO_X_LEFT..LOGO_X_LEFT+GLYPH_W → bytes 1..4).
        let mut nonwhite = 0;
        for col in 1..4 {
            if fb.desired_byte(row_start + col) != 0xFF {
                nonwhite += 1;
            }
        }
        assert!(nonwhite > 0, "expected black pixels in first scaled row of 'D'");
    }
}
