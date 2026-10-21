//! A grid of character cells laid over a drawing surface.
//!
//! This is how programs put text on screen: choose a [`FontSize`], lay a
//! grid over the surface they were handed, then address the result by
//! (col, row) instead of pixels.
//!
//! ```ignore
//! let grid = TextGrid::for_surface(FontSize::Medium, &body); // 29 × 21
//! grid.clear(&mut body);
//! grid.draw_row(&mut body, 0, "Messages");
//! ```
//!
//! The grid is anchored at the surface's top-left corner; leftover pixels
//! on the right and bottom stay blank. Text is never wrapped by the grid;
//! callers that need wrapping (like `Terminal`) produce lines no longer
//! than `cols()` themselves.
//!
//! Callee invariants:
//!   - Drawing never touches pixels outside the surface (see `Surface`)
//!     or outside the grid's `w × h` area within it.
//!   - Out-of-range cells and characters past the last column are ignored.

use crate::font::{Font, FontSize};
use crate::framebuffer::Ink;
use crate::geometry::Rect;
use crate::surface::Surface;
use crate::terminal::RenderCell;

#[derive(Clone, Copy)]
pub struct TextGrid {
    font: &'static Font,
    w: u16,
    h: u16,
    cols: usize,
    rows: usize,
}

impl TextGrid {
    /// A grid filling a `w × h` pixel area at a surface's top-left corner.
    pub fn new(size: FontSize, w: u16, h: u16) -> Self {
        let font = size.font();
        let (cols, rows) = font.grid(w, h);
        TextGrid { font, w, h, cols, rows }
    }

    /// A grid filling the whole of `surface`.
    pub fn for_surface(size: FontSize, surface: &Surface) -> Self {
        Self::new(size, surface.width(), surface.height())
    }

    pub fn cols(&self) -> usize {
        self.cols
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn font(&self) -> &'static Font {
        self.font
    }

    /// Surface-local pixel position of the top-left of cell (`col`, `row`).
    pub fn cell_origin(&self, col: usize, row: usize) -> (u16, u16) {
        (col as u16 * self.font.width(), row as u16 * self.font.height())
    }

    /// Whiten the grid's whole area, including any leftover margin.
    pub fn clear(&self, s: &mut Surface) {
        s.fill(Rect::new(0, 0, self.w, self.h), Ink::White);
    }

    /// Draw one character into cell (`col`, `row`).
    pub fn draw_cell(&self, s: &mut Surface, col: usize, row: usize, ch: char) {
        if col >= self.cols || row >= self.rows {
            return;
        }
        let (x, y) = self.cell_origin(col, row);
        s.draw_glyph(x, y, self.font, ch, Ink::Black);
    }

    /// Replace row `row` with `text`: characters past `cols()` are dropped
    /// and the rest of the row is blanked.
    pub fn draw_row(&self, s: &mut Surface, row: usize, text: &str) {
        if row >= self.rows {
            return;
        }
        let mut chars = text.chars();
        for col in 0..self.cols {
            self.draw_cell(s, col, row, chars.next().unwrap_or(' '));
        }
    }

    /// Clear the grid and draw a set of cells, e.g. from `Terminal::render`.
    pub fn draw_cells<'a>(
        &self,
        s: &mut Surface,
        cells: impl IntoIterator<Item = &'a RenderCell>,
    ) {
        self.clear(s);
        for cell in cells {
            self.draw_cell(s, cell.col as usize, cell.row as usize, cell.ch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framebuffer::{FrameBuffer, BYTES_PER_ROW, HEIGHT, WIDTH};

    fn is_black(fb: &FrameBuffer, x: u16, y: u16) -> bool {
        fb.desired_byte(y as usize * BYTES_PER_ROW + x as usize / 8) & (0x80 >> (x % 8)) == 0
    }

    fn any_black_in(fb: &FrameBuffer, x0: u16, y0: u16, x1: u16, y1: u16) -> bool {
        (y0..y1).any(|y| (x0..x1).any(|x| is_black(fb, x, y)))
    }

    #[test]
    fn dimensions_per_size() {
        let dims = |s| {
            let g = TextGrid::new(s, WIDTH, HEIGHT);
            (g.cols(), g.rows())
        };
        assert_eq!(dims(FontSize::Small), (40, 26));
        assert_eq!(dims(FontSize::Medium), (30, 22));
        assert_eq!(dims(FontSize::Large), (20, 13));
    }

    #[test]
    fn for_surface_uses_surface_size() {
        let mut fb = FrameBuffer::new();
        let s = Surface::new(&mut fb, Rect::new(10, 20, 100, 100));
        let g = TextGrid::for_surface(FontSize::Small, &s);
        assert_eq!((g.cols(), g.rows()), (16, 8));
        assert_eq!(g.cell_origin(3, 2), (18, 24));
    }

    #[test]
    fn cells_are_placed_relative_to_the_surface() {
        let mut fb = FrameBuffer::new();
        let mut s = Surface::new(&mut fb, Rect::new(4, 18, 100, 100));
        let g = TextGrid::for_surface(FontSize::Medium, &s);
        g.draw_cell(&mut s, 0, 0, '#');
        assert!(any_black_in(&fb, 4, 18, 12, 32));
        assert!(!any_black_in(&fb, 0, 0, 4, HEIGHT), "left margin untouched");
        assert!(!any_black_in(&fb, 0, 0, WIDTH, 18), "area above untouched");
    }

    #[test]
    fn draw_cell_ignores_out_of_range() {
        let mut fb = FrameBuffer::new();
        let mut s = Surface::new(&mut fb, Rect::new(0, 0, 80, 28)); // 10 × 2
        let g = TextGrid::for_surface(FontSize::Medium, &s);
        g.draw_cell(&mut s, 10, 0, 'X');
        g.draw_cell(&mut s, 0, 2, 'X');
        assert!(!fb.is_dirty());
    }

    #[test]
    fn draw_row_truncates_and_blanks_remainder() {
        let mut fb = FrameBuffer::new();
        let mut s = Surface::new(&mut fb, Rect::new(0, 0, 40, 14)); // 5 cols
        let g = TextGrid::for_surface(FontSize::Medium, &s);
        g.draw_row(&mut s, 0, "#####");
        g.draw_row(&mut s, 0, "##");
        assert!(any_black_in(&fb, 0, 0, 16, 14));
        assert!(!any_black_in(&fb, 16, 0, 40, 14), "old cells blanked");

        let mut s = Surface::new(&mut fb, Rect::new(0, 0, 40, 14));
        g.draw_row(&mut s, 0, "##########");
        assert!(!any_black_in(&fb, 40, 0, WIDTH, 14), "nothing past the grid");
    }

    #[test]
    fn clear_covers_leftover_margin_but_not_outside() {
        let mut fb = FrameBuffer::new();
        fb.fill_rect(0, 0, WIDTH, HEIGHT, true);
        let mut s = Surface::new(&mut fb, Rect::new(0, 0, WIDTH, 30)); // 1 row + 6px spare
        TextGrid::for_surface(FontSize::Large, &s).clear(&mut s);
        assert!(!any_black_in(&fb, 0, 0, WIDTH, 30));
        assert!(is_black(&fb, 0, 30));
    }

    #[test]
    fn draw_cells_renders_terminal_output() {
        use crate::terminal::Terminal;
        let mut fb = FrameBuffer::new();
        fb.fill_rect(0, 300, WIDTH, 20, true); // stale pixels in the bottom margin
        let mut s = Surface::new(&mut fb, Rect::SCREEN);
        let g = TextGrid::for_surface(FontSize::Medium, &s);
        let mut term = Terminal::new("> ", g.cols(), g.rows());
        term.push_output("hello");

        let cells = term.render();
        g.draw_cells(&mut s, &cells);

        // The 'h' of "hello" lands at its cell's pixel origin.
        let cell = cells.iter().find(|c| c.ch == 'h').unwrap();
        let (ox, oy) = g.cell_origin(cell.col as usize, cell.row as usize);
        let h = g.font().glyph('h');
        for y in 0..h.height {
            for x in 0..h.width {
                assert_eq!(is_black(&fb, ox + x, oy + y), h.is_set(x, y));
            }
        }
        assert!(!any_black_in(&fb, 0, 308, WIDTH, HEIGHT), "margin cleared");
    }
}
