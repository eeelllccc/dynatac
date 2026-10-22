// Platform-independent framebuffer for a 240×320 1-bit (black/white) display.
//
// Two-buffer architecture:
//   - `desired`   — what we want on screen (updated instantly by draw calls)
//   - `committed` — what is physically on the display
//
// Call `draw_glyph` / `draw_text` / `fill_rect` / `clear_line` to update
// `desired`, then flush in two steps (the kernel's side of the spi task):
//   - `begin_flush` gives what to send: `Flush::Full` while the panel's
//     contents are unknown (at boot: the e-paper keeps its old image; after
//     a failed draw), else `Flush::Partial` with the smallest byte-aligned
//     rectangle where `desired` differs from `committed`, or `None` if
//     nothing differs.
//   - `end_flush(ok)` once the draw has finished: `committed` takes the sent
//     data only if `ok`; otherwise the contents become unknown.
// Drawing may continue while a flush is in flight; the next flush covers it.
//
// Callee invariants:
//   - `committed` changes only on `end_flush(true)`, so it never claims
//     more than the panel was told to show.
//   - A partial region is 8-pixel aligned horizontally and tight: its edge
//     rows and byte columns each contain a changed byte.
//   - Drawing the same pixels again doesn't cause a flush.
//
// Caller invariants:
//   - At most one flush in flight: `end_flush` exactly once after each
//     `begin_flush` that returned `Some`, before the next `begin_flush`.
//
// `take_dirty_region` is the old one-step API (commits immediately), kept
// for `device/src/display.rs` until the kernel loop replaces it.

use crate::font::Font;
use crate::geometry::Rect;

/// Pixel colour on the 1-bit panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    Black,
    White,
}

pub const WIDTH: u16 = 240;
pub const HEIGHT: u16 = 320;
pub const BYTES_PER_ROW: usize = WIDTH as usize / 8; // 30
pub const BUF_LEN: usize = BYTES_PER_ROW * HEIGHT as usize; // 9600

/// The region that changed since the last flush.
///
/// `x`, `y`, `w`, `h` are in pixels (x and w are multiples of 8).
/// `data` contains the new pixel bytes for this region, row by row,
/// ready to be written to the display controller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirtyRegion {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub data: Vec<u8>,
}

/// Platform-independent framebuffer for a 240×320 1-bit display.
///
/// Two-buffer architecture:
///   - `desired`   — what we want on screen (updated instantly by draw calls)
///   - `committed` — what is physically on the display (mirrors controller state)
///
/// Call `draw_glyph` / `draw_text` / `clear_line` to update `desired`, then call
/// `take_dirty_region` to extract the changed rectangle for hardware flushing.
pub struct FrameBuffer {
    /// What we want on screen.
    desired: Box<[u8; BUF_LEN]>,
    /// What the display currently shows.
    committed: Box<[u8; BUF_LEN]>,
    /// Dirty bounding box: (x_min, y_min, x_max, y_max) in pixels, 8-aligned.
    /// `None` means nothing was drawn since the last flush began.
    dirty: Option<(u16, u16, u16, u16)>,
    /// `committed` is what the panel shows (false at boot: unknown).
    known: bool,
    in_flight: Option<Flush>,
}

/// What to send to the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flush {
    /// The whole screen, `BUF_LEN` bytes.
    Full(Vec<u8>),
    Partial(DirtyRegion),
}

impl FrameBuffer {
    /// Create a new framebuffer, initialised to all-white (0xFF).
    pub fn new() -> Self {
        FrameBuffer {
            desired: Box::new([0xFF; BUF_LEN]),
            committed: Box::new([0xFF; BUF_LEN]),
            dirty: None,
            known: false,
            in_flight: None,
        }
    }

    /// What to send next, if anything (see the module comment).
    pub fn begin_flush(&mut self) -> Option<Flush> {
        debug_assert!(self.in_flight.is_none(), "begin_flush with a flush in flight");
        if self.in_flight.is_some() {
            return None;
        }
        let flush = if self.known {
            let (x0, y0, x1, y1) = self.dirty.take()?;
            let (x, y, w, h) = self.changed_within(x0, y0, x1, y1)?;
            let data = Self::extract_region(&self.desired, x, y, w, h);
            Flush::Partial(DirtyRegion { x, y, w, h, data })
        } else {
            self.dirty = None;
            Flush::Full(self.desired.to_vec())
        };
        self.in_flight = Some(flush.clone());
        Some(flush)
    }

    /// The flush begun last has finished; `ok` if the panel shows it.
    pub fn end_flush(&mut self, ok: bool) {
        let Some(flush) = self.in_flight.take() else {
            debug_assert!(false, "end_flush with no flush in flight");
            return;
        };
        if !ok {
            self.known = false;
            return;
        }
        match flush {
            Flush::Full(data) => {
                self.committed.copy_from_slice(&data);
                self.known = true;
            }
            Flush::Partial(r) => self.write_committed(&r),
        }
    }

    /// Forget what the panel shows: the next flush is full (clears
    /// ghosting, e.g. after the lockscreen).
    pub fn invalidate(&mut self) {
        self.known = false;
    }

    /// Whether `begin_flush` may have something to send.
    pub fn needs_flush(&self) -> bool {
        self.in_flight.is_none() && (!self.known || self.dirty.is_some())
    }

    /// Whether a flush is in flight.
    pub fn flushing(&self) -> bool {
        self.in_flight.is_some()
    }

    /// The smallest byte-aligned rectangle within the box where `desired`
    /// differs from `committed`: (x, y, w, h) in pixels.
    fn changed_within(&self, x0: u16, y0: u16, x1: u16, y1: u16) -> Option<(u16, u16, u16, u16)> {
        let (bx0, bx1) = (x0 as usize / 8, x1 as usize / 8);
        let mut found: Option<(usize, usize, usize, usize)> = None;
        for y in y0 as usize..y1 as usize {
            for bx in bx0..bx1 {
                let i = y * BYTES_PER_ROW + bx;
                if self.desired[i] != self.committed[i] {
                    found = Some(match found {
                        None => (bx, y, bx, y),
                        Some((a, b, c, d)) => (a.min(bx), b.min(y), c.max(bx), d.max(y)),
                    });
                }
            }
        }
        let (a, b, c, d) = found?;
        Some(((a * 8) as u16, b as u16, ((c - a + 1) * 8) as u16, (d - b + 1) as u16))
    }

    fn write_committed(&mut self, r: &DirtyRegion) {
        let bytes_w = (r.w / 8) as usize;
        let x_byte = (r.x / 8) as usize;
        for row in 0..r.h as usize {
            let at = (r.y as usize + row) * BYTES_PER_ROW + x_byte;
            self.committed[at..at + bytes_w].copy_from_slice(&r.data[row * bytes_w..(row + 1) * bytes_w]);
        }
    }

    /// Reset both buffers to white and clear the dirty region.
    /// Call this after performing a full hardware refresh.
    pub fn clear(&mut self) {
        self.desired.fill(0xFF);
        self.committed.fill(0xFF);
        self.dirty = None;
        self.known = true;
    }

    /// Draw one character cell with its top-left corner at pixel (x, y).
    ///
    /// The whole `font.width() × font.height()` cell is painted: glyph
    /// pixels black, the rest white, so drawing over old text replaces it.
    /// `x` need not be aligned. Pixels falling outside the display are
    /// clipped. Characters the font lacks draw as the replacement glyph.
    pub fn draw_glyph(&mut self, x: u16, y: u16, font: &Font, ch: char) {
        self.draw_glyph_clipped(x, y, font, ch, Ink::Black, Rect::SCREEN);
    }

    /// Like `draw_glyph`, but glyph pixels are drawn in `ink` (the cell
    /// background in the opposite colour), and only pixels inside `clip`
    /// are touched.
    pub fn draw_glyph_clipped(
        &mut self,
        x: u16,
        y: u16,
        font: &Font,
        ch: char,
        ink: Ink,
        clip: Rect,
    ) {
        let glyph = font.glyph(ch);
        let cell = Rect::new(x, y, glyph.width, glyph.height);
        let area = cell.intersect(&clip).intersect(&Rect::SCREEN);
        if area.is_empty() {
            return;
        }
        let black = ink == Ink::Black;
        for py in area.y..area.bottom() {
            for px in area.x..area.right() {
                self.set_pixel(px, py, glyph.is_set(px - x, py - y) == black);
            }
        }
        self.mark_dirty_unaligned(area.x, area.y, area.right(), area.bottom());
    }

    /// Draw `text` left to right starting at pixel (x, y), one cell per
    /// `char`. Stops at the first character whose cell would not fit
    /// entirely on the display. Returns the x just past the last cell drawn.
    pub fn draw_text(&mut self, x: u16, y: u16, font: &Font, text: &str) -> u16 {
        let mut cx = x;
        for ch in text.chars() {
            if cx + font.width() > WIDTH {
                break;
            }
            self.draw_glyph(cx, y, font, ch);
            cx += font.width();
        }
        cx
    }

    /// Fill an axis-aligned rectangle in the desired buffer.
    ///
    /// `on` = true draws black; false draws white. Coordinates are in
    /// pixels and may be unaligned. The dirty region is expanded to an
    /// 8-pixel column boundary so the partial flush stays well-formed.
    pub fn fill_rect(&mut self, x: u16, y: u16, w: u16, h: u16, on: bool) {
        if w == 0 || h == 0 || x >= WIDTH || y >= HEIGHT {
            return;
        }
        let x_end = x.saturating_add(w).min(WIDTH);
        let y_end = y.saturating_add(h).min(HEIGHT);
        for py in y..y_end {
            for px in x..x_end {
                self.set_pixel(px, py, on);
            }
        }
        self.mark_dirty_unaligned(x, y, x_end, y_end);
    }

    /// Clear the entire desired buffer to white. Marks the full display
    /// dirty so the next flush repaints everything.
    pub fn clear_all_desired(&mut self) {
        self.desired.fill(0xFF);
        self.mark_dirty(0, 0, WIDTH, HEIGHT);
    }

    /// Clear the full-width strip of `height` pixel rows starting at `y`.
    pub fn clear_line(&mut self, y: u16, height: u16) {
        self.fill_rect(0, y, WIDTH, height, false);
    }

    /// Take the dirty region, if any. Returns the bounding rectangle and pixel
    /// data, and marks committed as matching desired for that region.
    ///
    /// Returns `None` if nothing has changed since the last flush.
    pub fn take_dirty_region(&mut self) -> Option<DirtyRegion> {
        let (x_min, y_min, x_max, y_max) = self.dirty.take()?;

        let w = x_max - x_min;
        let h = y_max - y_min;
        let data = Self::extract_region(&self.desired, x_min, y_min, w, h);

        // Update committed to match desired in this region.
        let bytes_w = (w / 8) as usize;
        let x_byte = (x_min / 8) as usize;
        for row in 0..h as usize {
            let buf_start = (y_min as usize + row) * BYTES_PER_ROW + x_byte;
            let data_start = row * bytes_w;
            self.committed[buf_start..buf_start + bytes_w]
                .copy_from_slice(&data[data_start..data_start + bytes_w]);
        }

        Some(DirtyRegion { x: x_min, y: y_min, w, h, data })
    }

    /// Read the desired buffer byte at a given index (for testing).
    pub fn desired_byte(&self, index: usize) -> u8 {
        self.desired[index]
    }

    /// Read the committed buffer byte at a given index (for testing).
    pub fn committed_byte(&self, index: usize) -> u8 {
        self.committed[index]
    }

    /// Whether there is a pending dirty region.
    pub fn is_dirty(&self) -> bool {
        self.dirty.is_some()
    }

    // --- Internal helpers --------------------------------------------------------

    /// Set one pixel in `desired`. `on` = black. Caller ensures it's on screen.
    fn set_pixel(&mut self, x: u16, y: u16, on: bool) {
        let idx = y as usize * BYTES_PER_ROW + x as usize / 8;
        let bit = 0x80u8 >> (x % 8);
        if on {
            self.desired[idx] &= !bit;
        } else {
            self.desired[idx] |= bit;
        }
    }

    /// Mark the pixel span [x, x_end) × [y, y_end) dirty, widened to whole bytes.
    fn mark_dirty_unaligned(&mut self, x: u16, y: u16, x_end: u16, y_end: u16) {
        let aligned_x = x & !7;
        let aligned_end = (x_end + 7) & !7;
        self.mark_dirty(aligned_x, y, aligned_end - aligned_x, y_end - y);
    }

    fn mark_dirty(&mut self, x: u16, y: u16, w: u16, h: u16) {
        let (x_min, y_min, x_max, y_max) = match self.dirty {
            Some((x0, y0, x1, y1)) => (
                x0.min(x),
                y0.min(y),
                x1.max(x + w),
                y1.max(y + h),
            ),
            None => (x, y, x + w, y + h),
        };
        self.dirty = Some((x_min, y_min, x_max, y_max));
    }

    fn extract_region(buf: &[u8; BUF_LEN], x: u16, y: u16, w: u16, h: u16) -> Vec<u8> {
        let bytes_w = (w / 8) as usize;
        let x_byte = (x / 8) as usize;
        let mut region = Vec::with_capacity(bytes_w * h as usize);
        for row in 0..h as usize {
            let row_start = (y as usize + row) * BYTES_PER_ROW + x_byte;
            region.extend_from_slice(&buf[row_start..row_start + bytes_w]);
        }
        region
    }
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font::FontSize;

    fn is_black(fb: &FrameBuffer, x: u16, y: u16) -> bool {
        let byte = fb.desired_byte(y as usize * BYTES_PER_ROW + x as usize / 8);
        byte & (0x80 >> (x % 8)) == 0
    }

    /// Assert the screen at (x, y) shows exactly the glyph for `ch`.
    fn assert_glyph_at(fb: &FrameBuffer, x: u16, y: u16, size: FontSize, ch: char) {
        let g = size.font().glyph(ch);
        for gy in 0..g.height {
            for gx in 0..g.width {
                assert_eq!(
                    is_black(fb, x + gx, y + gy),
                    g.is_set(gx, gy),
                    "{:?} {:?} pixel ({}, {})",
                    size, ch, gx, gy
                );
            }
        }
    }

    #[test]
    fn the_first_flush_is_full_because_the_panel_keeps_its_old_image() {
        let mut fb = FrameBuffer::new();
        assert!(fb.needs_flush());
        let Some(Flush::Full(data)) = fb.begin_flush() else { panic!("expected full") };
        assert_eq!(data, vec![0xFF; BUF_LEN]);
        assert!(fb.flushing() && !fb.needs_flush());
        fb.end_flush(true);
        assert!(!fb.needs_flush());
        assert_eq!(fb.begin_flush(), None);
    }

    fn known() -> FrameBuffer {
        let mut fb = FrameBuffer::new();
        fb.begin_flush();
        fb.end_flush(true);
        fb
    }

    #[test]
    fn a_partial_region_is_tight_and_byte_aligned() {
        let mut fb = known();
        fb.fill_rect(13, 7, 1, 1, true); // one pixel
        fb.fill_rect(30, 9, 2, 1, true); // and two more, two rows down
        let Some(Flush::Partial(r)) = fb.begin_flush() else { panic!("expected partial") };
        assert_eq!((r.x, r.y, r.w, r.h), (8, 7, 24, 3));
        assert_eq!(r.data.len(), 3 * 3);
        fb.end_flush(true);
        for i in 0..BUF_LEN {
            assert_eq!(fb.committed_byte(i), fb.desired_byte(i), "byte {i}");
        }
    }

    #[test]
    fn redrawing_identical_pixels_needs_no_refresh() {
        let mut fb = known();
        fb.draw_text(0, 0, FontSize::Medium.font(), "hello");
        fb.begin_flush();
        fb.end_flush(true);
        // The terminal clears and redraws its whole area on every key.
        fb.fill_rect(0, 0, WIDTH, 40, false);
        fb.draw_text(0, 0, FontSize::Medium.font(), "hello");
        assert!(fb.needs_flush(), "something was drawn");
        assert_eq!(fb.begin_flush(), None, "but nothing changed");
    }

    #[test]
    fn only_the_changed_bytes_are_sent_after_a_full_area_redraw() {
        let mut fb = known();
        fb.draw_text(0, 0, FontSize::Medium.font(), "hello");
        fb.begin_flush();
        fb.end_flush(true);
        fb.fill_rect(0, 0, WIDTH, HEIGHT, false);
        fb.draw_text(0, 0, FontSize::Medium.font(), "hellp");
        let Some(Flush::Partial(r)) = fb.begin_flush() else { panic!("expected partial") };
        let cell = FontSize::Medium.font().width();
        assert!(r.x <= 4 * cell && r.x + r.w >= 5 * cell && r.w <= 16, "{:?}", (r.x, r.w));
    }

    #[test]
    fn drawing_during_a_flush_is_covered_by_the_next() {
        let mut fb = known();
        fb.fill_rect(0, 0, 8, 1, true);
        let first = fb.begin_flush().unwrap();
        fb.fill_rect(100, 100, 8, 1, true); // while the panel refreshes
        fb.end_flush(true);
        let Some(Flush::Partial(r)) = fb.begin_flush() else { panic!("expected partial") };
        assert_eq!((r.x, r.y, r.w, r.h), (96, 100, 16, 1));
        assert_ne!(Some(first), Some(Flush::Partial(r)));
    }

    #[test]
    fn a_failed_flush_makes_the_next_one_full() {
        let mut fb = known();
        fb.fill_rect(0, 0, 8, 1, true);
        fb.begin_flush();
        fb.end_flush(false);
        assert!(fb.needs_flush());
        let Some(Flush::Full(data)) = fb.begin_flush() else { panic!("expected full") };
        assert_eq!(data[0], 0x00);
        fb.end_flush(true);
        assert_eq!(fb.begin_flush(), None);
    }

    #[test]
    fn committed_is_untouched_until_the_flush_succeeds() {
        let mut fb = known();
        fb.fill_rect(0, 0, 8, 1, true);
        fb.begin_flush();
        assert_eq!(fb.committed_byte(0), 0xFF);
        fb.end_flush(true);
        assert_eq!(fb.committed_byte(0), 0x00);
    }

    #[test]
    fn new_framebuffer_is_all_white() {
        let fb = FrameBuffer::new();
        assert_eq!(fb.desired_byte(0), 0xFF);
        assert_eq!(fb.desired_byte(BUF_LEN - 1), 0xFF);
        assert!(!fb.is_dirty());
    }

    #[test]
    fn draw_glyph_paints_every_size() {
        for size in FontSize::ALL {
            let mut fb = FrameBuffer::new();
            fb.draw_glyph(0, 0, size.font(), 'A');
            assert_glyph_at(&fb, 0, 0, size, 'A');
        }
    }

    #[test]
    fn draw_glyph_at_unaligned_x() {
        // Small glyphs are 6 wide, so the third cell starts at x=12,
        // straddling bytes 1 and 2.
        let mut fb = FrameBuffer::new();
        fb.draw_glyph(12, 5, FontSize::Small.font(), 'M');
        assert_glyph_at(&fb, 12, 5, FontSize::Small, 'M');
        // Neighbouring pixels are untouched.
        for y in 5..17 {
            assert!(!is_black(&fb, 11, y));
            assert!(!is_black(&fb, 18, y));
        }
    }

    #[test]
    fn draw_glyph_replaces_previous_cell_contents() {
        let mut fb = FrameBuffer::new();
        let font = FontSize::Medium.font();
        fb.draw_glyph(8, 0, font, '#');
        fb.draw_glyph(8, 0, font, '.');
        assert_glyph_at(&fb, 8, 0, FontSize::Medium, '.');
        fb.draw_glyph(8, 0, font, ' ');
        assert!(!fb.desired.iter().any(|&b| b != 0xFF), "space erases the cell");
    }

    #[test]
    fn draw_glyph_uses_replacement_for_unknown_chars() {
        let mut fb = FrameBuffer::new();
        fb.draw_glyph(0, 0, FontSize::Medium.font(), '😀');
        assert_glyph_at(&fb, 0, 0, FontSize::Medium, crate::font::REPLACEMENT);
    }

    #[test]
    fn draw_glyph_clips_at_display_edges() {
        let mut fb = FrameBuffer::new();
        let font = FontSize::Large.font();
        fb.draw_glyph(WIDTH - 4, HEIGHT - 4, font, '#'); // mostly off screen
        fb.draw_glyph(WIDTH, 0, font, '#'); // entirely off screen
        let r = fb.take_dirty_region().unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (WIDTH - 8, HEIGHT - 4, 8, 4));
    }

    #[test]
    fn draw_glyph_clipped_white_ink_inverts_cell() {
        let mut fb = FrameBuffer::new();
        let font = FontSize::Small.font();
        fb.draw_glyph_clipped(0, 0, font, 'A', Ink::White, Rect::SCREEN);
        let g = font.glyph('A');
        for y in 0..g.height {
            for x in 0..g.width {
                assert_eq!(is_black(&fb, x, y), !g.is_set(x, y), "({}, {})", x, y);
            }
        }
    }

    #[test]
    fn draw_glyph_clipped_touches_only_clip_area() {
        let mut fb = FrameBuffer::new();
        let font = FontSize::Large.font();
        // Clip to the left 5 columns and top 3 rows of the cell at (8, 8).
        let clip = Rect::new(8, 8, 5, 3);
        fb.draw_glyph_clipped(8, 8, font, '#', Ink::White, clip);
        for y in 0..40 {
            for x in 0..40 {
                if !(8..13).contains(&x) || !(8..11).contains(&y) {
                    assert!(!is_black(&fb, x, y), "outside clip at ({}, {})", x, y);
                }
            }
        }
        let r = fb.take_dirty_region().unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (8, 8, 8, 3));
    }

    #[test]
    fn draw_text_advances_by_cell_width_and_returns_end() {
        let mut fb = FrameBuffer::new();
        let end = fb.draw_text(3, 10, FontSize::Small.font(), "Hi!");
        assert_eq!(end, 3 + 3 * 6);
        assert_glyph_at(&fb, 3, 10, FontSize::Small, 'H');
        assert_glyph_at(&fb, 9, 10, FontSize::Small, 'i');
        assert_glyph_at(&fb, 15, 10, FontSize::Small, '!');
    }

    #[test]
    fn draw_text_stops_before_a_cell_that_would_not_fit() {
        let mut fb = FrameBuffer::new();
        let font = FontSize::Large.font(); // 20 cells across
        let end = fb.draw_text(0, 0, font, &"W".repeat(25));
        assert_eq!(end, 240);
        let mut fb2 = FrameBuffer::new();
        assert_eq!(fb2.draw_text(234, 0, font, "W"), 234, "half a cell left: nothing drawn");
        assert!(!fb2.is_dirty());
    }

    #[test]
    fn clear_line_whitens_strip_of_given_height() {
        let mut fb = FrameBuffer::new();
        fb.fill_rect(0, 0, WIDTH, 30, true);
        fb.clear_line(10, 14);
        for y in 0..30 {
            let expect_black = !(10..24).contains(&y);
            assert_eq!(is_black(&fb, 0, y), expect_black, "y={}", y);
            assert_eq!(is_black(&fb, WIDTH - 1, y), expect_black, "y={}", y);
        }
    }

    #[test]
    fn dirty_region_of_unaligned_glyph_is_byte_aligned() {
        let mut fb = FrameBuffer::new();
        fb.draw_glyph(12, 100, FontSize::Small.font(), 'Z'); // pixels 12..18
        let r = fb.take_dirty_region().unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (8, 100, 16, 12));
        assert_eq!(r.data.len(), 2 * 12);
    }

    #[test]
    fn dirty_tracking_merges_regions() {
        let mut fb = FrameBuffer::new();
        let font = FontSize::Medium.font();
        fb.draw_glyph(0, 0, font, 'A');
        fb.draw_glyph(16, 0, font, 'B');

        let r = fb.take_dirty_region().unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (0, 0, 24, 14));
        assert_eq!(r.data.len(), 3 * 14);
    }

    #[test]
    fn take_dirty_region_clears_dirty() {
        let mut fb = FrameBuffer::new();
        fb.draw_glyph(0, 0, FontSize::Medium.font(), 'A');
        let _ = fb.take_dirty_region();

        assert!(!fb.is_dirty());
        assert!(fb.take_dirty_region().is_none());
    }

    #[test]
    fn take_dirty_region_updates_committed() {
        let mut fb = FrameBuffer::new();
        fb.draw_glyph(0, 0, FontSize::Medium.font(), 'A');
        let _ = fb.take_dirty_region();

        for i in 0..14 * BYTES_PER_ROW {
            assert_eq!(fb.desired_byte(i), fb.committed_byte(i), "byte {}", i);
        }
    }

    #[test]
    fn clear_resets_everything() {
        let mut fb = FrameBuffer::new();
        fb.draw_glyph(0, 0, FontSize::Medium.font(), 'X');
        fb.clear();

        assert!(!fb.is_dirty());
        assert_eq!(fb.desired_byte(0), 0xFF);
        assert_eq!(fb.committed_byte(0), 0xFF);
    }

    #[test]
    fn draw_glyph_flush_with_bottom_edge() {
        let mut fb = FrameBuffer::new();
        fb.draw_glyph(0, HEIGHT - 24, FontSize::Large.font(), '!');
        let r = fb.take_dirty_region().unwrap();
        assert_eq!((r.y, r.h), (HEIGHT - 24, 24));
    }
}
