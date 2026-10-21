//! A drawing surface: a rectangular window onto the framebuffer.
//!
//! Surfaces are how screen zones are handed out. Whoever owns the
//! `FrameBuffer` (the OS / main loop) creates a `Surface` for a zone and
//! lends it to the code that fills that zone. That code can only draw
//! inside the surface's rectangle, so, for example, a program given the
//! body surface cannot touch the status bar.
//!
//! Coordinates are local: (0, 0) is the surface's top-left corner, and
//! `width() × height()` is its size. Everything drawn is clipped to the
//! surface.
//!
//! Callee invariants:
//!   - No method writes a pixel outside the surface's rectangle.
//!   - A sub-surface is always contained in its parent.

use crate::font::Font;
use crate::framebuffer::{FrameBuffer, Ink};
use crate::geometry::Rect;

pub struct Surface<'a> {
    fb: &'a mut FrameBuffer,
    /// Screen-space rectangle, already clipped to the display.
    rect: Rect,
}

impl<'a> Surface<'a> {
    /// A surface over screen rectangle `rect` (clipped to the display).
    pub fn new(fb: &'a mut FrameBuffer, rect: Rect) -> Self {
        Surface { fb, rect: rect.intersect(&Rect::SCREEN) }
    }

    pub fn width(&self) -> u16 {
        self.rect.w
    }

    pub fn height(&self) -> u16 {
        self.rect.h
    }

    /// Where this surface sits on the screen.
    pub fn screen_rect(&self) -> Rect {
        self.rect
    }

    /// A surface for part of this one; `local` is in this surface's
    /// coordinates and is clipped to it.
    pub fn sub(&mut self, local: Rect) -> Surface<'_> {
        let rect = self.to_screen(local);
        Surface { fb: self.fb, rect }
    }

    /// Fill `local` (clipped to the surface) with `ink`.
    pub fn fill(&mut self, local: Rect, ink: Ink) {
        let r = self.to_screen(local);
        if !r.is_empty() {
            self.fb.fill_rect(r.x, r.y, r.w, r.h, ink == Ink::Black);
        }
    }

    /// Fill the whole surface with `ink`.
    pub fn fill_all(&mut self, ink: Ink) {
        self.fill(Rect::new(0, 0, self.rect.w, self.rect.h), ink);
    }

    /// Clear the whole surface to white.
    pub fn clear(&mut self) {
        self.fill_all(Ink::White);
    }

    /// Draw one character cell at local (`x`, `y`): glyph in `ink`,
    /// cell background in the opposite colour. Clipped to the surface.
    pub fn draw_glyph(&mut self, x: u16, y: u16, font: &Font, ch: char, ink: Ink) {
        if x >= self.rect.w || y >= self.rect.h {
            return;
        }
        self.fb.draw_glyph_clipped(self.rect.x + x, self.rect.y + y, font, ch, ink, self.rect);
    }

    /// Draw `text` from local (`x`, `y`), one cell per `char`, stopping at
    /// the first cell that would not fit entirely within the surface's
    /// width. Returns the local x just past the last cell drawn.
    pub fn draw_text(&mut self, x: u16, y: u16, font: &Font, text: &str, ink: Ink) -> u16 {
        let mut cx = x;
        for ch in text.chars() {
            if cx + font.width() > self.rect.w {
                break;
            }
            self.draw_glyph(cx, y, font, ch, ink);
            cx += font.width();
        }
        cx
    }

    /// Local rectangle → screen rectangle, clipped to this surface.
    fn to_screen(&self, local: Rect) -> Rect {
        local.offset(self.rect.x, self.rect.y).intersect(&self.rect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::FontSize;
    use crate::framebuffer::{BYTES_PER_ROW, HEIGHT, WIDTH};

    fn is_black(fb: &FrameBuffer, x: u16, y: u16) -> bool {
        fb.desired_byte(y as usize * BYTES_PER_ROW + x as usize / 8) & (0x80 >> (x % 8)) == 0
    }

    /// Every black pixel on screen lies inside `r`.
    fn ink_only_inside(fb: &FrameBuffer, r: Rect) -> bool {
        (0..HEIGHT).all(|y| {
            (0..WIDTH).all(|x| {
                !is_black(fb, x, y) || (x >= r.x && x < r.right() && y >= r.y && y < r.bottom())
            })
        })
    }

    #[test]
    fn new_clips_to_display() {
        let mut fb = FrameBuffer::new();
        let s = Surface::new(&mut fb, Rect::new(200, 300, 100, 100));
        assert_eq!(s.screen_rect(), Rect::new(200, 300, 40, 20));
    }

    #[test]
    fn coordinates_are_local() {
        let mut fb = FrameBuffer::new();
        let mut s = Surface::new(&mut fb, Rect::new(20, 30, 50, 50));
        s.fill(Rect::new(0, 0, 1, 1), Ink::Black);
        assert!(is_black(&fb, 20, 30));
        assert!(!is_black(&fb, 0, 0));
    }

    #[test]
    fn fill_all_stays_inside() {
        let mut fb = FrameBuffer::new();
        let r = Rect::new(13, 7, 30, 9);
        Surface::new(&mut fb, r).fill_all(Ink::Black);
        assert!(ink_only_inside(&fb, r));
        assert!(is_black(&fb, 13, 7) && is_black(&fb, 42, 15));
    }

    #[test]
    fn glyph_straddling_edge_is_clipped() {
        let mut fb = FrameBuffer::new();
        let r = Rect::new(10, 10, 20, 10);
        let mut s = Surface::new(&mut fb, r);
        // Large cell is 12×24: drawn at local (14, 0) it overhangs right and bottom.
        s.draw_glyph(14, 0, FontSize::Large.font(), '#', Ink::White);
        assert!(ink_only_inside(&fb, r));
    }

    #[test]
    fn draw_text_stops_at_surface_width() {
        let mut fb = FrameBuffer::new();
        let r = Rect::new(4, 0, 20, 14); // room for 2 medium cells
        let mut s = Surface::new(&mut fb, r);
        let end = s.draw_text(0, 0, FontSize::Medium.font(), "WWWW", Ink::Black);
        assert_eq!(end, 16);
        assert!(ink_only_inside(&fb, Rect::new(4, 0, 16, 14)));
    }

    #[test]
    fn sub_surface_is_offset_and_contained() {
        let mut fb = FrameBuffer::new();
        let mut parent = Surface::new(&mut fb, Rect::new(10, 10, 20, 20));
        let mut child = parent.sub(Rect::new(15, 15, 100, 100));
        assert_eq!(child.screen_rect(), Rect::new(25, 25, 5, 5));
        child.fill_all(Ink::Black);
        assert!(ink_only_inside(&fb, Rect::new(25, 25, 5, 5)));
    }
}
