//! Pixel rectangles.
//!
//! `Rect` is plain data used to describe screen zones (see `screen`) and
//! the areas surfaces may draw into (see `surface`). All operations are
//! pure and saturate rather than underflow, so a rectangle that is
//! shrunk or split too far just becomes empty (zero width or height).

use crate::framebuffer::{HEIGHT, WIDTH};

/// An axis-aligned rectangle: top-left corner (`x`, `y`), size `w × h`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    /// The whole display.
    pub const SCREEN: Rect = Rect::new(0, 0, WIDTH, HEIGHT);

    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Rect { x, y, w, h }
    }

    /// One past the rightmost column.
    pub const fn right(&self) -> u16 {
        self.x.saturating_add(self.w)
    }

    /// One past the bottom row.
    pub const fn bottom(&self) -> u16 {
        self.y.saturating_add(self.h)
    }

    pub const fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// Shrink by `px` on every side.
    pub const fn inset(&self, px: u16) -> Rect {
        Rect::new(
            self.x.saturating_add(px),
            self.y.saturating_add(px),
            self.w.saturating_sub(px.saturating_mul(2)),
            self.h.saturating_sub(px.saturating_mul(2)),
        )
    }

    /// Cut a strip of height `h` off the top: returns (top strip, rest).
    /// If `h` exceeds the height, the strip is the whole rect and the
    /// rest is empty.
    pub const fn split_top(&self, h: u16) -> (Rect, Rect) {
        let h = if h < self.h { h } else { self.h };
        (
            Rect::new(self.x, self.y, self.w, h),
            Rect::new(self.x, self.y + h, self.w, self.h - h),
        )
    }

    /// The overlap of two rectangles (empty if they don't overlap).
    pub fn intersect(&self, other: &Rect) -> Rect {
        let x = self.x.max(other.x);
        let y = self.y.max(other.y);
        let right = self.right().min(other.right());
        let bottom = self.bottom().min(other.bottom());
        Rect::new(x, y, right.saturating_sub(x), bottom.saturating_sub(y))
    }

    /// Move by (`dx`, `dy`), e.g. from a surface's local coordinates
    /// to screen coordinates.
    pub const fn offset(&self, dx: u16, dy: u16) -> Rect {
        Rect::new(self.x.saturating_add(dx), self.y.saturating_add(dy), self.w, self.h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inset_shrinks_every_side() {
        assert_eq!(Rect::new(0, 10, 240, 100).inset(4), Rect::new(4, 14, 232, 92));
        assert!(Rect::new(0, 0, 6, 6).inset(4).is_empty());
    }

    #[test]
    fn split_top_partitions() {
        let (top, rest) = Rect::SCREEN.split_top(14);
        assert_eq!(top, Rect::new(0, 0, 240, 14));
        assert_eq!(rest, Rect::new(0, 14, 240, 306));
        let (all, none) = Rect::new(0, 0, 10, 5).split_top(9);
        assert_eq!(all, Rect::new(0, 0, 10, 5));
        assert!(none.is_empty());
    }

    #[test]
    fn intersect_overlapping_and_disjoint() {
        let a = Rect::new(0, 0, 10, 10);
        assert_eq!(a.intersect(&Rect::new(5, 5, 10, 10)), Rect::new(5, 5, 5, 5));
        assert!(a.intersect(&Rect::new(20, 0, 5, 5)).is_empty());
        assert_eq!(a.intersect(&a), a);
    }

    #[test]
    fn offset_translates() {
        assert_eq!(Rect::new(1, 2, 3, 4).offset(10, 20), Rect::new(11, 22, 3, 4));
    }
}
