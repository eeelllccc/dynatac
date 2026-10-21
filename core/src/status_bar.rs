//! The status bar along the top of the screen.
//!
//! For now it is static: a black bar with the DYNATAC logo in white,
//! left-aligned with the body text. Live status (battery, signal, ...)
//! will be added once the OS has a way to feed it.
//!
//! Caller invariants:
//!   - `s` is the status bar surface (`screen::LAYOUT.status_bar`), or at
//!     least tall enough for the logo (`logo::ink_height(1)` px).

use crate::framebuffer::Ink;
use crate::logo;
use crate::screen::MARGIN;
use crate::surface::Surface;

/// Paint the whole status bar.
pub fn render(s: &mut Surface) {
    s.fill_all(Ink::Black);
    let y = s.height().saturating_sub(logo::ink_height(1)) / 2;
    logo::draw(s, MARGIN, y, 1, 1, Ink::White);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::{FrameBuffer, BYTES_PER_ROW};
    use crate::screen::LAYOUT;

    fn is_black(fb: &FrameBuffer, x: u16, y: u16) -> bool {
        fb.desired_byte(y as usize * BYTES_PER_ROW + x as usize / 8) & (0x80 >> (x % 8)) == 0
    }

    fn rendered() -> FrameBuffer {
        let mut fb = FrameBuffer::new();
        render(&mut Surface::new(&mut fb, LAYOUT.status_bar));
        fb
    }

    #[test]
    fn bar_is_black_with_white_logo_and_nothing_below() {
        let fb = rendered();
        let bar = LAYOUT.status_bar;
        // Corners of the bar are black.
        assert!(is_black(&fb, 0, 0));
        assert!(is_black(&fb, bar.right() - 1, bar.bottom() - 1));
        // Some white pixels inside the bar: the logo.
        let white = (bar.y..bar.bottom())
            .flat_map(|y| (bar.x..bar.right()).map(move |x| (x, y)))
            .filter(|&(x, y)| !is_black(&fb, x, y))
            .count();
        assert!(white > 50, "logo pixels: {}", white);
        // Everything below the bar is untouched.
        for y in bar.bottom()..bar.bottom() + 8 {
            assert!(!is_black(&fb, 0, y));
        }
    }

    #[test]
    fn logo_is_vertically_centred_and_left_aligned() {
        let fb = rendered();
        let bar = LAYOUT.status_bar;
        let white_rows: Vec<u16> = (bar.y..bar.bottom())
            .filter(|&y| (bar.x..bar.right()).any(|x| !is_black(&fb, x, y)))
            .collect();
        let top = *white_rows.first().unwrap();
        let bottom = bar.bottom() - 1 - *white_rows.last().unwrap();
        assert!(top.abs_diff(bottom) <= 1, "top gap {} bottom gap {}", top, bottom);

        let first_white_x = (0..bar.w)
            .find(|&x| (bar.y..bar.bottom()).any(|y| !is_black(&fb, x, y)))
            .unwrap();
        // 'D' starts one pixel into its 8×8 box (0x78 = .####...).
        assert_eq!(first_white_x, MARGIN + 1);
    }
}
