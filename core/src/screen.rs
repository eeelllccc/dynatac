//! Screen zones: how the 240×320 panel is divided up.
//!
//! ```text
//! ┌────────────────────┐
//! │ DYNATAC            │  status_bar  (STATUS_BAR_HEIGHT, full width, OS-owned)
//! ├────────────────────┤
//! │ ┌────────────────┐ │
//! │ │      body      │ │  body        (rest of the screen inset by MARGIN,
//! │ └────────────────┘ │               lent to whatever is running)
//! └────────────────────┘
//! ```
//!
//! This module only describes geometry. Whoever owns the framebuffer
//! creates a `Surface` for each zone and hands it to the code that fills
//! that zone, which keeps that code from drawing anywhere else.
//!
//! Callee invariants (tested below):
//!   - Zones lie within the display and do not overlap.
//!   - The body is separated from the status bar and from the display
//!     edges by at least `MARGIN` pixels.

use crate::geometry::Rect;

/// White space between the body and the edges around it.
pub const MARGIN: u16 = 4;

/// Height of the status bar along the top of the screen.
pub const STATUS_BAR_HEIGHT: u16 = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub status_bar: Rect,
    pub body: Rect,
}

impl Layout {
    /// Status bar across the top, body filling the rest inside a margin.
    pub const fn standard() -> Layout {
        let (status_bar, rest) = Rect::SCREEN.split_top(STATUS_BAR_HEIGHT);
        Layout { status_bar, body: rest.inset(MARGIN) }
    }
}

/// The layout in use.
pub const LAYOUT: Layout = Layout::standard();

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::FontSize;

    #[test]
    fn zones_are_on_screen_and_disjoint() {
        let l = LAYOUT;
        for r in [l.status_bar, l.body] {
            assert_eq!(r.intersect(&Rect::SCREEN), r);
            assert!(!r.is_empty());
        }
        assert!(l.status_bar.intersect(&l.body).is_empty());
    }

    #[test]
    fn body_keeps_its_margin() {
        let b = LAYOUT.body;
        assert!(b.x >= MARGIN);
        assert!(b.y >= LAYOUT.status_bar.bottom() + MARGIN);
        assert!(Rect::SCREEN.right() - b.right() >= MARGIN);
        assert!(Rect::SCREEN.bottom() - b.bottom() >= MARGIN);
    }

    #[test]
    fn standard_geometry() {
        assert_eq!(LAYOUT.status_bar, Rect::new(0, 0, 240, 14));
        assert_eq!(LAYOUT.body, Rect::new(4, 18, 232, 298));
    }

    #[test]
    fn body_text_grid_per_font_size() {
        let grid = |s: FontSize| s.font().grid(LAYOUT.body.w, LAYOUT.body.h);
        assert_eq!(grid(FontSize::Small), (38, 24));
        assert_eq!(grid(FontSize::Medium), (29, 21));
        assert_eq!(grid(FontSize::Large), (19, 12));
    }
}
