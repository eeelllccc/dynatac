use std::fmt;

pub const CANVAS_WIDTH: u16 = 362;
pub const CANVAS_HEIGHT: u16 = 298;
pub const VIEWPORT_WIDTH: u16 = 232;
pub const VIEWPORT_HEIGHT: u16 = 298;
pub const STRIDE: usize = (CANVAS_WIDTH as usize + 7) / 8;
pub const BITMAP_LEN: usize = STRIDE * CANVAS_HEIGHT as usize;

/// Packed monochrome meteogram. Zero bits are black and one bits are white.
pub struct Meteogram {
    pixels: Box<[u8]>,
}

impl fmt::Debug for Meteogram {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Meteogram")
            .field("width", &CANVAS_WIDTH)
            .field("height", &CANVAS_HEIGHT)
            .field("bytes", &self.pixels.len())
            .finish()
    }
}

impl Meteogram {
    pub(crate) fn white() -> Self {
        Self {
            pixels: vec![0xff; BITMAP_LEN].into_boxed_slice(),
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.pixels
    }

    pub fn max_scroll_x(&self) -> u16 {
        CANVAS_WIDTH - VIEWPORT_WIDTH
    }

    pub fn is_black(&self, x: u16, y: u16) -> bool {
        if x >= CANVAS_WIDTH || y >= CANVAS_HEIGHT {
            return false;
        }
        let index = y as usize * STRIDE + x as usize / 8;
        self.pixels[index] & (0x80 >> (x & 7)) == 0
    }

    pub(crate) fn set(&mut self, x: i32, y: i32, black: bool) {
        if x < 0 || y < 0 || x >= CANVAS_WIDTH as i32 || y >= CANVAS_HEIGHT as i32 {
            return;
        }
        let index = y as usize * STRIDE + x as usize / 8;
        let mask = 0x80 >> (x & 7);
        if black {
            self.pixels[index] &= !mask;
        } else {
            self.pixels[index] |= mask;
        }
    }

    pub(crate) fn point(&mut self, x: i32, y: i32) {
        self.set(x, y, true);
    }
    pub(crate) fn white_point(&mut self, x: i32, y: i32) {
        self.set(x, y, false);
    }

    pub(crate) fn horizontal(&mut self, x0: i32, x1: i32, y: i32, black: bool) {
        for x in x0.min(x1)..=x0.max(x1) {
            self.set(x, y, black);
        }
    }

    pub(crate) fn vertical(&mut self, x: i32, y0: i32, y1: i32, black: bool) {
        for y in y0.min(y1)..=y0.max(y1) {
            self.set(x, y, black);
        }
    }

    pub(crate) fn rect_outline(&mut self, left: i32, top: i32, right: i32, bottom: i32) {
        self.horizontal(left, right, top, true);
        self.horizontal(left, right, bottom, true);
        self.vertical(left, top, bottom, true);
        self.vertical(right, top, bottom, true);
    }

    pub fn black_runs(&self, scroll_x: u16) -> BlackRuns<'_> {
        BlackRuns {
            bitmap: self,
            scroll_x: scroll_x.min(self.max_scroll_x()),
            x: 0,
            y: 0,
        }
    }
}

/// Iterator of `(x, y, width)` black runs in viewport-local coordinates.
pub struct BlackRuns<'a> {
    bitmap: &'a Meteogram,
    scroll_x: u16,
    x: u16,
    y: u16,
}

impl Iterator for BlackRuns<'_> {
    type Item = (u16, u16, u16);

    fn next(&mut self) -> Option<Self::Item> {
        while self.y < VIEWPORT_HEIGHT {
            while self.x < VIEWPORT_WIDTH && !self.bitmap.is_black(self.scroll_x + self.x, self.y) {
                self.x += 1;
            }
            if self.x == VIEWPORT_WIDTH {
                self.x = 0;
                self.y += 1;
                continue;
            }
            let start = self.x;
            while self.x < VIEWPORT_WIDTH && self.bitmap.is_black(self.scroll_x + self.x, self.y) {
                self.x += 1;
            }
            return Some((start, self.y, self.x - start));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_contract_and_runs() {
        let mut bitmap = Meteogram::white();
        bitmap.point(3, 2);
        bitmap.point(4, 2);
        bitmap.point(7, 2);
        assert_eq!(bitmap.bytes().len(), 13_708);
        assert_eq!(
            bitmap.black_runs(0).collect::<Vec<_>>(),
            vec![(3, 2, 2), (7, 2, 1)]
        );
    }

    #[test]
    fn viewport_scroll_is_clamped() {
        let bitmap = Meteogram::white();
        assert_eq!(bitmap.max_scroll_x(), 130);
        assert_eq!(bitmap.black_runs(999).count(), 0);
    }
}
