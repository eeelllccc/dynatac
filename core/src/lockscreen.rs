// Lockscreen rendering: the chunky, all-caps DYNATAC logo (see `logo`)
// large across the bottom of the e-paper display.
//
// Layout:
//   - Each logo letter is 8×8 source → 32×40 destination (SCALE_X=4, SCALE_Y=5).
//   - Logo width = 7 letters × 32 = 224 px, centred in 240 → 8 px margins.
//   - Logo y position is `LOGO_Y_TOP`, leaving an 8 px margin from the
//     bottom of the 320-row display.
//
// Caller invariants:
//   - The framebuffer is the standard `WIDTH × HEIGHT` (240×320) buffer.
//   - The caller is responsible for flushing after `render`.

use crate::framebuffer::{FrameBuffer, Ink, HEIGHT, WIDTH};
use crate::geometry::Rect;
use crate::logo;
use crate::surface::Surface;

/// Per-pixel horizontal scale.
pub const SCALE_X: u16 = 4;
/// Per-pixel vertical scale.
pub const SCALE_Y: u16 = 5;

/// Total logo width in pixels.
pub const LOGO_WIDTH: u16 = logo::size(SCALE_X, SCALE_Y).0; // 224
/// Total logo height in pixels.
pub const LOGO_HEIGHT: u16 = logo::size(SCALE_X, SCALE_Y).1; // 40

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
    let mut screen = Surface::new(fb, Rect::SCREEN);
    logo::draw(&mut screen, LOGO_X_LEFT, LOGO_Y_TOP, SCALE_X, SCALE_Y, Ink::Black);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::BYTES_PER_ROW;

    /// FNV-1a over the whole rendered framebuffer.
    fn render_hash() -> u64 {
        let mut fb = FrameBuffer::new();
        render(&mut fb);
        (0..crate::framebuffer::BUF_LEN).fold(0xcbf29ce484222325u64, |h, i| {
            (h ^ fb.desired_byte(i) as u64).wrapping_mul(0x100000001b3)
        })
    }

    #[test]
    fn lockscreen_image_is_unchanged() {
        // Golden value: the lockscreen must stay pixel-identical.
        assert_eq!(render_hash(), GOLDEN);
    }
    const GOLDEN: u64 = 4075822102365783067;

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
        fb.fill_rect(0, 0, WIDTH, 8, true);
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
        // Sanity: the 'D' glyph has bits set in row 0, so the
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
