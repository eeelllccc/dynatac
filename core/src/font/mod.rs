//! Monospace bitmap fonts: one family (Terminus Bold) in three sizes.
//!
//! Programs pick a [`FontSize`] and never deal with glyph data directly.
//! Drawing goes through `Surface::draw_glyph` / `draw_text`, or more
//! commonly through `TextGrid`, which lays a font's cells over a surface
//! as rows and columns.
//!
//! | size   | cell  | body zone grid (232×298) |
//! |--------|-------|--------------------------|
//! | Small  | 6×12  | 38 cols × 24 rows        |
//! | Medium | 8×14  | 29 cols × 21 rows        |
//! | Large  | 12×24 | 19 cols × 12 rows        |
//!
//! Every size covers the same character set (printable ASCII, Latin-1,
//! curly quotes, dashes, `…`, `€`, arrows, box drawing and block
//! elements). Anything else renders as the replacement glyph `�`.
//!
//! The glyph tables are generated from `fonts/terminus/*.bdf` by
//! `tools/fontgen.py`; see that script for the bitmap layout.
//!
//! Callee invariants (checked by the tests below for every font):
//!   - `chars` is strictly ascending, so lookup can binary search.
//!   - `bitmaps.len() == chars.len() * height * bytes_per_row`.
//!   - `' '`, `'?'` and [`REPLACEMENT`] are present.
//!   - No glyph sets a bit at or beyond `width` within its row.

mod terminus_12x24;
mod terminus_6x12;
mod terminus_8x14;

/// Drawn in place of any character the font does not cover.
pub const REPLACEMENT: char = '\u{FFFD}';

/// The three font sizes available to programs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FontSize {
    /// 6×12 cells. Dense lists and status text.
    Small,
    /// 8×14 cells. The default for the terminal and general text.
    #[default]
    Medium,
    /// 12×24 cells. Headings and glanceable information.
    Large,
}

impl FontSize {
    pub const ALL: [FontSize; 3] = [FontSize::Small, FontSize::Medium, FontSize::Large];

    /// The glyph data for this size.
    pub fn font(self) -> &'static Font {
        match self {
            FontSize::Small => &terminus_6x12::FONT,
            FontSize::Medium => &terminus_8x14::FONT,
            FontSize::Large => &terminus_12x24::FONT,
        }
    }

    /// Cell width in pixels. Usable in `const` contexts.
    pub const fn width(self) -> u16 {
        match self {
            FontSize::Small => terminus_6x12::WIDTH,
            FontSize::Medium => terminus_8x14::WIDTH,
            FontSize::Large => terminus_12x24::WIDTH,
        }
    }

    /// Cell height (line pitch) in pixels. Usable in `const` contexts.
    pub const fn height(self) -> u16 {
        match self {
            FontSize::Small => terminus_6x12::HEIGHT,
            FontSize::Medium => terminus_8x14::HEIGHT,
            FontSize::Large => terminus_12x24::HEIGHT,
        }
    }
}

/// A fixed-cell bitmap font. Every glyph occupies exactly
/// `width × height` pixels, including any spacing, so text is laid out
/// by stepping `width` across and `height` down with no extra gap.
pub struct Font {
    width: u16,
    height: u16,
    chars: &'static [char],
    bitmaps: &'static [u8],
}

/// One glyph's pixels: `height` rows of `bytes_per_row` bytes, bit 7 of
/// each row's first byte is the leftmost pixel, 1 = ink.
#[derive(Debug, Clone, Copy)]
pub struct Glyph<'a> {
    pub width: u16,
    pub height: u16,
    data: &'a [u8],
}

impl Font {
    /// Used only by the generated tables; see the invariants above.
    pub(crate) const fn new(
        width: u16,
        height: u16,
        chars: &'static [char],
        bitmaps: &'static [u8],
    ) -> Self {
        Font { width, height, chars, bitmaps }
    }

    pub fn width(&self) -> u16 {
        self.width
    }

    pub fn height(&self) -> u16 {
        self.height
    }

    /// How many whole cells fit in a `px_w × px_h` area, as (cols, rows).
    pub fn grid(&self, px_w: u16, px_h: u16) -> (usize, usize) {
        ((px_w / self.width) as usize, (px_h / self.height) as usize)
    }

    /// Whether `ch` has its own glyph (rather than the replacement).
    pub fn has_glyph(&self, ch: char) -> bool {
        self.chars.binary_search(&ch).is_ok()
    }

    /// The glyph for `ch`, or the replacement glyph if `ch` is not covered.
    pub fn glyph(&self, ch: char) -> Glyph<'static> {
        let index = match self.chars.binary_search(&ch) {
            Ok(i) => i,
            Err(_) => self
                .chars
                .binary_search(&REPLACEMENT)
                .expect("every font includes the replacement glyph"),
        };
        let len = self.glyph_len();
        Glyph {
            width: self.width,
            height: self.height,
            data: &self.bitmaps[index * len..(index + 1) * len],
        }
    }

    fn bytes_per_row(&self) -> usize {
        (self.width as usize).div_ceil(8)
    }

    fn glyph_len(&self) -> usize {
        self.height as usize * self.bytes_per_row()
    }
}

impl Glyph<'_> {
    /// Whether the pixel at (`x`, `y`) within the cell is ink.
    /// Out-of-range coordinates are blank.
    pub fn is_set(&self, x: u16, y: u16) -> bool {
        if x >= self.width || y >= self.height {
            return false;
        }
        let bpr = (self.width as usize).div_ceil(8);
        let byte = self.data[y as usize * bpr + x as usize / 8];
        byte & (0x80 >> (x % 8)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a glyph as `#`/`.` rows, for readable assertions.
    fn ascii_art(g: Glyph) -> Vec<String> {
        (0..g.height)
            .map(|y| (0..g.width).map(|x| if g.is_set(x, y) { '#' } else { '.' }).collect())
            .collect()
    }

    #[test]
    fn sizes_match_their_fonts() {
        for size in FontSize::ALL {
            assert_eq!(size.font().width(), size.width(), "{:?}", size);
            assert_eq!(size.font().height(), size.height(), "{:?}", size);
        }
        assert_eq!((FontSize::Small.width(), FontSize::Small.height()), (6, 12));
        assert_eq!((FontSize::Medium.width(), FontSize::Medium.height()), (8, 14));
        assert_eq!((FontSize::Large.width(), FontSize::Large.height()), (12, 24));
    }

    #[test]
    fn table_invariants_hold() {
        for size in FontSize::ALL {
            let f = size.font();
            assert!(f.chars.windows(2).all(|w| w[0] < w[1]), "{:?} not sorted", size);
            assert_eq!(f.bitmaps.len(), f.chars.len() * f.glyph_len(), "{:?}", size);
            for ch in [' ', '?', REPLACEMENT] {
                assert!(f.has_glyph(ch), "{:?} lacks {:?}", size, ch);
            }
            // Padding bits past `width` must be clear.
            let bpr = f.bytes_per_row();
            let spare = (bpr * 8) as u16 - f.width;
            if spare > 0 {
                let mask = (1u8 << spare) - 1;
                for row in f.bitmaps.chunks(bpr) {
                    assert_eq!(row[bpr - 1] & mask, 0, "{:?} sets padding bits", size);
                }
            }
        }
    }

    #[test]
    fn all_sizes_cover_the_same_characters() {
        let small = FontSize::Small.font().chars;
        for size in FontSize::ALL {
            assert_eq!(size.font().chars, small, "{:?}", size);
        }
    }

    #[test]
    fn printable_ascii_and_common_extras_are_covered() {
        let f = FontSize::Medium.font();
        for ch in (0x20u8..0x7F).map(char::from).chain("£€°é…—“”←→─│┌█".chars()) {
            assert!(f.has_glyph(ch), "missing {:?}", ch);
        }
    }

    #[test]
    fn space_is_blank() {
        for size in FontSize::ALL {
            let g = size.font().glyph(' ');
            assert!(ascii_art(g).iter().all(|r| !r.contains('#')), "{:?}", size);
        }
    }

    #[test]
    fn printable_ascii_glyphs_have_ink() {
        for size in FontSize::ALL {
            for ch in (0x21u8..0x7F).map(char::from) {
                let g = size.font().glyph(ch);
                assert!(ascii_art(g).iter().any(|r| r.contains('#')), "{:?} {:?}", size, ch);
            }
        }
    }

    #[test]
    fn uncovered_character_uses_replacement_glyph() {
        let f = FontSize::Small.font();
        assert!(!f.has_glyph('😀'));
        assert_eq!(ascii_art(f.glyph('😀')), ascii_art(f.glyph(REPLACEMENT)));
        assert_ne!(ascii_art(f.glyph('😀')), ascii_art(f.glyph('?')));
    }

    #[test]
    fn medium_capital_a_matches_terminus() {
        let art = ascii_art(FontSize::Medium.font().glyph('A'));
        assert_eq!(
            art,
            [
                "........",
                "........",
                ".#####..",
                "##...##.",
                "##...##.",
                "##...##.",
                "##...##.",
                "#######.",
                "##...##.",
                "##...##.",
                "##...##.",
                "##...##.",
                "........",
                "........",
            ]
        );
    }

    #[test]
    fn large_glyph_spans_two_bytes_per_row() {
        // Terminus 12x24 'H' has both vertical strokes; the right one
        // lives in the second byte of each row.
        let g = FontSize::Large.font().glyph('H');
        let right_stroke = (0..g.height).any(|y| (8..12).any(|x| g.is_set(x, y)));
        assert!(right_stroke);
        assert!(!g.is_set(12, 10), "out of range is blank");
    }

    #[test]
    fn grid_counts_whole_cells() {
        assert_eq!(FontSize::Small.font().grid(240, 320), (40, 26));
        assert_eq!(FontSize::Medium.font().grid(240, 320), (30, 22));
        assert_eq!(FontSize::Large.font().grid(240, 320), (20, 13));
        assert_eq!(FontSize::Medium.font().grid(7, 13), (0, 0));
    }
}
