# Terminus Font (Bold)

Source: terminus-font 4.49.1, https://terminus-font.sourceforge.net/
Licence: SIL Open Font License 1.1 (see `OFL.TXT`).

| file           | size  | used as           |
|----------------|-------|-------------------|
| `ter-u12b.bdf` | 6×12  | `FontSize::Small`  |
| `ter-u14b.bdf` | 8×14  | `FontSize::Medium` |
| `ter-u24b.bdf` | 12×24 | `FontSize::Large`  |

The Rust glyph tables in `core/src/font/terminus_*.rs` are generated from
these files by `python3 tools/fontgen.py`. Edit the character set there,
not the generated files.
