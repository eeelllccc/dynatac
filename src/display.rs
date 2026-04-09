// Driver for the GDEQ031T10 e-paper display (240x320, UC8253 controller).
//
// Hardware connections (from test_EPD.ino):
//   SPI SCK  → GPIO36
//   SPI MOSI → GPIO33
//   CS       → GPIO34  (shared SPI bus, manually managed)
//   DC       → GPIO35  (data/command select: LOW=command, HIGH=data)
//   RST      → not connected (-1)
//   BUSY     → GPIO37  (LOW=busy, HIGH=ready)
//
// Caller invariants:
//   - The SPI bus is initialised at 10 MHz, MODE0, MSB-first before constructing `Epd`.
//   - All other devices on the shared SPI bus (LoRa CS, SD CS) must have their CS
//     pins driven HIGH before calling any method on `Epd`, and must not initiate
//     their own SPI transactions while `Epd` methods are running.
//
// Callee invariants:
//   - DC is left HIGH (data mode) after every write_command call.
//   - CS is left HIGH (deasserted) after every SPI transfer.
//   - `committed` always mirrors the controller's "previous" buffer (0x10).
//   - `desired` holds what we want on screen; `try_flush` syncs it to hardware.

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, Input, Output, PinDriver};
use esp_idf_svc::hal::spi::{SpiDeviceDriver, SpiDriver};
use esp_idf_svc::hal::sys::EspError;

// --- Display geometry ---------------------------------------------------------

pub const WIDTH: u16 = 240;
pub const HEIGHT: u16 = 320;
const BYTES_PER_ROW: usize = WIDTH as usize / 8; // 30
const BUF_LEN: usize = BYTES_PER_ROW * HEIGHT as usize; // 9600

// --- 8×8 bitmap font ----------------------------------------------------------
//
// Indexed by (ASCII code − 0x20), covering 0x20..=0x7F (96 characters).
// Each entry is 8 bytes, one per pixel row top-to-bottom.
// Within each byte: bit 7 = leftmost pixel, 1 = black, 0 = white.
//
// Only the characters needed for "Hello World!" and common symbols are drawn
// in detail; the rest are zeroed (blank) and can be added incrementally.

#[rustfmt::skip]
const FONT_8X8: [[u8; 8]; 96] = [
  /* 0x20 ' '  */ [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
  /* 0x21 '!'  */ [0x18,0x18,0x18,0x18,0x18,0x00,0x18,0x00],
  /* 0x22 '"'  */ [0x66,0x66,0x44,0x00,0x00,0x00,0x00,0x00],
  /* 0x23 '#'  */ [0x24,0x7E,0x24,0x24,0x7E,0x24,0x00,0x00],
  /* 0x24 '$'  */ [0x08,0x3E,0x48,0x3C,0x0A,0x7C,0x08,0x00],
  /* 0x25 '%'  */ [0x62,0x64,0x08,0x10,0x26,0x46,0x00,0x00],
  /* 0x26 '&'  */ [0x38,0x44,0x48,0x30,0x4A,0x44,0x3A,0x00],
  /* 0x27 '\'' */ [0x18,0x18,0x10,0x00,0x00,0x00,0x00,0x00],
  /* 0x28 '('  */ [0x08,0x10,0x20,0x20,0x20,0x10,0x08,0x00],
  /* 0x29 ')'  */ [0x20,0x10,0x08,0x08,0x08,0x10,0x20,0x00],
  /* 0x2A '*'  */ [0x00,0x14,0x08,0x3E,0x08,0x14,0x00,0x00],
  /* 0x2B '+'  */ [0x00,0x08,0x08,0x3E,0x08,0x08,0x00,0x00],
  /* 0x2C ','  */ [0x00,0x00,0x00,0x00,0x18,0x18,0x10,0x00],
  /* 0x2D '-'  */ [0x00,0x00,0x00,0x3E,0x00,0x00,0x00,0x00],
  /* 0x2E '.'  */ [0x00,0x00,0x00,0x00,0x00,0x18,0x18,0x00],
  /* 0x2F '/'  */ [0x02,0x04,0x08,0x10,0x20,0x40,0x00,0x00],
  /* 0x30 '0'  */ [0x3C,0x46,0x4A,0x52,0x62,0x3C,0x00,0x00],
  /* 0x31 '1'  */ [0x18,0x28,0x08,0x08,0x08,0x3E,0x00,0x00],
  /* 0x32 '2'  */ [0x3C,0x42,0x02,0x1C,0x20,0x7E,0x00,0x00],
  /* 0x33 '3'  */ [0x3C,0x42,0x0C,0x02,0x42,0x3C,0x00,0x00],
  /* 0x34 '4'  */ [0x08,0x18,0x28,0x48,0x7E,0x08,0x00,0x00],
  /* 0x35 '5'  */ [0x7E,0x40,0x7C,0x02,0x42,0x3C,0x00,0x00],
  /* 0x36 '6'  */ [0x3C,0x40,0x7C,0x42,0x42,0x3C,0x00,0x00],
  /* 0x37 '7'  */ [0x7E,0x04,0x08,0x10,0x10,0x10,0x00,0x00],
  /* 0x38 '8'  */ [0x3C,0x42,0x3C,0x42,0x42,0x3C,0x00,0x00],
  /* 0x39 '9'  */ [0x3C,0x42,0x3E,0x02,0x42,0x3C,0x00,0x00],
  /* 0x3A ':'  */ [0x00,0x18,0x18,0x00,0x18,0x18,0x00,0x00],
  /* 0x3B ';'  */ [0x00,0x18,0x18,0x00,0x18,0x18,0x10,0x00],
  /* 0x3C '<'  */ [0x08,0x10,0x20,0x40,0x20,0x10,0x08,0x00],
  /* 0x3D '='  */ [0x00,0x00,0x7E,0x00,0x7E,0x00,0x00,0x00],
  /* 0x3E '>'  */ [0x20,0x10,0x08,0x04,0x08,0x10,0x20,0x00],
  /* 0x3F '?'  */ [0x3C,0x42,0x04,0x18,0x00,0x18,0x00,0x00],
  /* 0x40 '@'  */ [0x3C,0x42,0x5A,0x56,0x5C,0x40,0x3C,0x00],
  /* 0x41 'A'  */ [0x18,0x24,0x42,0x7E,0x42,0x42,0x00,0x00],
  /* 0x42 'B'  */ [0x7C,0x42,0x7C,0x42,0x42,0x7C,0x00,0x00],
  /* 0x43 'C'  */ [0x3C,0x42,0x40,0x40,0x42,0x3C,0x00,0x00],
  /* 0x44 'D'  */ [0x78,0x44,0x42,0x42,0x44,0x78,0x00,0x00],
  /* 0x45 'E'  */ [0x7E,0x40,0x7C,0x40,0x40,0x7E,0x00,0x00],
  /* 0x46 'F'  */ [0x7E,0x40,0x7C,0x40,0x40,0x40,0x00,0x00],
  /* 0x47 'G'  */ [0x3C,0x40,0x40,0x4E,0x42,0x3C,0x00,0x00],
  /* 0x48 'H'  */ [0x42,0x42,0x42,0x7E,0x42,0x42,0x42,0x00],
  /* 0x49 'I'  */ [0x3E,0x08,0x08,0x08,0x08,0x3E,0x00,0x00],
  /* 0x4A 'J'  */ [0x02,0x02,0x02,0x42,0x42,0x3C,0x00,0x00],
  /* 0x4B 'K'  */ [0x42,0x44,0x78,0x48,0x44,0x42,0x00,0x00],
  /* 0x4C 'L'  */ [0x40,0x40,0x40,0x40,0x40,0x7E,0x00,0x00],
  /* 0x4D 'M'  */ [0x42,0x66,0x5A,0x42,0x42,0x42,0x00,0x00],
  /* 0x4E 'N'  */ [0x42,0x62,0x52,0x4A,0x46,0x42,0x00,0x00],
  /* 0x4F 'O'  */ [0x3C,0x42,0x42,0x42,0x42,0x3C,0x00,0x00],
  /* 0x50 'P'  */ [0x7C,0x42,0x7C,0x40,0x40,0x40,0x00,0x00],
  /* 0x51 'Q'  */ [0x3C,0x42,0x42,0x4A,0x44,0x3A,0x00,0x00],
  /* 0x52 'R'  */ [0x7C,0x42,0x7C,0x48,0x44,0x42,0x00,0x00],
  /* 0x53 'S'  */ [0x3C,0x40,0x3C,0x02,0x42,0x3C,0x00,0x00],
  /* 0x54 'T'  */ [0x7E,0x08,0x08,0x08,0x08,0x08,0x00,0x00],
  /* 0x55 'U'  */ [0x42,0x42,0x42,0x42,0x42,0x3C,0x00,0x00],
  /* 0x56 'V'  */ [0x42,0x42,0x42,0x24,0x18,0x00,0x00,0x00],
  /* 0x57 'W'  */ [0x41,0x41,0x41,0x49,0x55,0x63,0x00,0x00],
  /* 0x58 'X'  */ [0x42,0x24,0x18,0x18,0x24,0x42,0x00,0x00],
  /* 0x59 'Y'  */ [0x42,0x24,0x18,0x08,0x08,0x08,0x00,0x00],
  /* 0x5A 'Z'  */ [0x7E,0x04,0x08,0x10,0x20,0x7E,0x00,0x00],
  /* 0x5B '['  */ [0x1E,0x10,0x10,0x10,0x10,0x1E,0x00,0x00],
  /* 0x5C '\\' */ [0x40,0x20,0x10,0x08,0x04,0x02,0x00,0x00],
  /* 0x5D ']'  */ [0x3C,0x04,0x04,0x04,0x04,0x3C,0x00,0x00],
  /* 0x5E '^'  */ [0x08,0x14,0x22,0x00,0x00,0x00,0x00,0x00],
  /* 0x5F '_'  */ [0x00,0x00,0x00,0x00,0x00,0x00,0x7E,0x00],
  /* 0x60 '`'  */ [0x10,0x08,0x00,0x00,0x00,0x00,0x00,0x00],
  /* 0x61 'a'  */ [0x00,0x00,0x3C,0x02,0x3E,0x42,0x3E,0x00],
  /* 0x62 'b'  */ [0x40,0x40,0x7C,0x42,0x42,0x7C,0x00,0x00],
  /* 0x63 'c'  */ [0x00,0x00,0x3C,0x40,0x40,0x3C,0x00,0x00],
  /* 0x64 'd'  */ [0x02,0x02,0x3E,0x42,0x42,0x3E,0x00,0x00],
  /* 0x65 'e'  */ [0x00,0x00,0x3C,0x42,0x7E,0x40,0x3C,0x00],
  /* 0x66 'f'  */ [0x1C,0x20,0x7C,0x20,0x20,0x20,0x00,0x00],
  /* 0x67 'g'  */ [0x00,0x00,0x3E,0x42,0x3E,0x02,0x3C,0x00],
  /* 0x68 'h'  */ [0x40,0x40,0x7C,0x42,0x42,0x42,0x00,0x00],
  /* 0x69 'i'  */ [0x08,0x00,0x18,0x08,0x08,0x1C,0x00,0x00],
  /* 0x6A 'j'  */ [0x02,0x00,0x06,0x02,0x42,0x3C,0x00,0x00],
  /* 0x6B 'k'  */ [0x40,0x48,0x50,0x60,0x50,0x48,0x00,0x00],
  /* 0x6C 'l'  */ [0x10,0x10,0x10,0x10,0x10,0x10,0x10,0x00],
  /* 0x6D 'm'  */ [0x00,0x00,0x76,0x49,0x49,0x49,0x00,0x00],
  /* 0x6E 'n'  */ [0x00,0x00,0x7C,0x42,0x42,0x42,0x00,0x00],
  /* 0x6F 'o'  */ [0x00,0x00,0x3C,0x42,0x42,0x42,0x3C,0x00],
  /* 0x70 'p'  */ [0x00,0x00,0x7C,0x42,0x7C,0x40,0x40,0x00],
  /* 0x71 'q'  */ [0x00,0x00,0x3E,0x42,0x3E,0x02,0x02,0x00],
  /* 0x72 'r'  */ [0x00,0x00,0x7C,0x40,0x40,0x40,0x00,0x00],
  /* 0x73 's'  */ [0x00,0x00,0x3C,0x40,0x3C,0x02,0x7C,0x00],
  /* 0x74 't'  */ [0x20,0x20,0x7C,0x20,0x20,0x1C,0x00,0x00],
  /* 0x75 'u'  */ [0x00,0x00,0x42,0x42,0x42,0x3E,0x00,0x00],
  /* 0x76 'v'  */ [0x00,0x00,0x42,0x42,0x24,0x18,0x00,0x00],
  /* 0x77 'w'  */ [0x00,0x00,0x41,0x49,0x55,0x22,0x00,0x00],
  /* 0x78 'x'  */ [0x00,0x00,0x42,0x24,0x18,0x24,0x42,0x00],
  /* 0x79 'y'  */ [0x00,0x00,0x42,0x42,0x3E,0x02,0x3C,0x00],
  /* 0x7A 'z'  */ [0x00,0x00,0x7E,0x0C,0x30,0x7E,0x00,0x00],
  /* 0x7B '{'  */ [0x0E,0x08,0x30,0x08,0x08,0x0E,0x00,0x00],
  /* 0x7C '|'  */ [0x08,0x08,0x08,0x08,0x08,0x08,0x00,0x00],
  /* 0x7D '}'  */ [0x38,0x08,0x06,0x08,0x08,0x38,0x00,0x00],
  /* 0x7E '~'  */ [0x30,0x49,0x06,0x00,0x00,0x00,0x00,0x00],
  /* 0x7F DEL  */ [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
];

// --- Driver -------------------------------------------------------------------

/// Driver for the GDEQ031T10 e-paper display (UC8253 controller, 240×320).
///
/// Two-buffer architecture:
///   - `desired`   — what we want on screen (updated instantly by draw calls)
///   - `committed` — what is physically on the display (mirrors controller reg 0x10)
///
/// Call `draw_char` / `clear_line` to update `desired`, then call `try_flush`
/// in a loop.  `try_flush` is non-blocking: if the display is still refreshing
/// it returns immediately, letting the caller keep processing input.  When the
/// display becomes idle, `try_flush` sends the minimal dirty region and kicks
/// off the next partial refresh.
pub struct Epd<'d> {
    spi: SpiDeviceDriver<'d, SpiDriver<'d>>,
    cs:   PinDriver<'d, AnyOutputPin, Output>,
    dc:   PinDriver<'d, AnyOutputPin, Output>,
    busy: PinDriver<'d, AnyIOPin,     Input>,

    init_done:    bool,
    power_is_on:  bool,

    /// What we want on screen.  Updated by `draw_char` / `clear_line`.
    desired: Box<[u8; BUF_LEN]>,
    /// What the controller's 0x10 register holds (last completed refresh).
    committed: Box<[u8; BUF_LEN]>,

    /// Dirty bounding box: (x_min, y_min, x_max, y_max) in pixels, 8-aligned.
    /// `None` means desired == committed (nothing to flush).
    dirty: Option<(u16, u16, u16, u16)>,
}

impl<'d> Epd<'d> {
    pub fn new(
        spi:  SpiDeviceDriver<'d, SpiDriver<'d>>,
        cs:   PinDriver<'d, AnyOutputPin, Output>,
        dc:   PinDriver<'d, AnyOutputPin, Output>,
        busy: PinDriver<'d, AnyIOPin, Input>,
    ) -> Self {
        Epd {
            spi,
            cs,
            dc,
            busy,
            init_done:   false,
            power_is_on: false,
            desired:   Box::new([0xFF; BUF_LEN]),
            committed: Box::new([0xFF; BUF_LEN]),
            dirty: None,
        }
    }

    // --- Low-level SPI helpers ------------------------------------------------

    /// Send a command byte.  DC low during the transfer, returned HIGH after.
    fn write_command(&mut self, cmd: u8) -> Result<(), EspError> {
        self.dc.set_low()?;
        self.cs.set_low()?;
        self.spi.write(&[cmd])?;
        self.cs.set_high()?;
        self.dc.set_high()?;
        Ok(())
    }

    /// Send data bytes.  DC must already be HIGH (left there by write_command).
    fn write_data(&mut self, data: &[u8]) -> Result<(), EspError> {
        self.cs.set_low()?;
        self.spi.write(data)?;
        self.cs.set_high()?;
        Ok(())
    }

    /// Block until the BUSY pin goes HIGH (display is ready).
    /// Busy = LOW, ready = HIGH (UC8253 convention, busy_level = LOW in GxEPD2).
    fn wait_busy(&self) {
        sleep(Duration::from_millis(1));
        let mut timeout_ms: u32 = 5_000;
        while self.busy.is_low() {
            sleep(Duration::from_millis(1));
            timeout_ms = timeout_ms.saturating_sub(1);
            if timeout_ms == 0 {
                log::warn!("EPD BUSY timeout");
                break;
            }
        }
    }

    // --- Controller state machine ----------------------------------------------

    /// Initialise the UC8253.  Called automatically when init_done is false.
    /// No hardware reset (RST not connected); uses soft-reset via panel-setting.
    fn init_display(&mut self) -> Result<(), EspError> {
        // Soft reset
        self.write_command(0x00)?;
        self.write_data(&[0x1E, 0x0D])?;
        sleep(Duration::from_millis(1));
        // Panel setting: KW mode
        self.write_command(0x00)?;
        self.write_data(&[0x1F, 0x0D])?;

        self.init_done   = true;
        self.power_is_on = false;
        Ok(())
    }

    fn ensure_init(&mut self) -> Result<(), EspError> {
        if !self.init_done {
            self.init_display()?;
        }
        Ok(())
    }

    fn power_on(&mut self) -> Result<(), EspError> {
        if !self.power_is_on {
            self.write_command(0x04)?;
            self.wait_busy();
            self.power_is_on = true;
        }
        Ok(())
    }

    fn power_off(&mut self) -> Result<(), EspError> {
        if self.power_is_on {
            self.write_command(0x02)?;
            self.wait_busy();
            self.power_is_on = false;
        }
        Ok(())
    }

    /// Set the partial-update RAM window on the controller.
    /// x and w are snapped to byte boundaries by the controller spec (multiples of 8).
    fn set_partial_window(&mut self, x: u16, y: u16, w: u16, h: u16) -> Result<(), EspError> {
        let x_start = x & 0xFFF8;
        let x_end   = (x + w - 1) | 0x0007;
        let y_end   = y + h - 1;
        self.write_command(0x90)?;  // Partial Window
        self.write_data(&[
            x_start as u8,
            x_end   as u8,
            (y      >> 8) as u8, (y      & 0xFF) as u8,
            (y_end  >> 8) as u8, (y_end  & 0xFF) as u8,
            0x01,
        ])?;
        Ok(())
    }

    /// Trigger a full display refresh (slow, ~1 s).
    /// Sets init_done = false; the next write must call ensure_init() first.
    fn update_full(&mut self) -> Result<(), EspError> {
        // Fast full update: fix temperature at 90 °C equivalent
        self.write_command(0xE0)?; self.write_data(&[0x02])?;
        self.write_command(0xE5)?; self.write_data(&[0x5A])?;
        self.write_command(0x50)?; self.write_data(&[0x97])?;
        self.power_on()?;
        self.write_command(0x12)?; // display refresh
        self.wait_busy();
        self.init_done = false; // controller resets its state after refresh
        Ok(())
    }

    /// Trigger a partial display refresh (~700 ms, blocking).
    fn update_partial(&mut self) -> Result<(), EspError> {
        // Fast partial update: fix temperature at 121 °C equivalent
        self.write_command(0xE0)?; self.write_data(&[0x02])?;
        self.write_command(0xE5)?; self.write_data(&[0x79])?;
        self.write_command(0x50)?; self.write_data(&[0xD7])?;
        self.power_on()?;
        self.write_command(0x12)?; // display refresh
        self.wait_busy();
        self.init_done = false;
        Ok(())
    }

    // --- Frame buffer writes to controller RAM --------------------------------

    /// Write `data` into the controller's RAM register (0x10 = previous, 0x13 = current),
    /// confined to a partial window at (x, y, w, h).  x and w must be multiples of 8.
    ///
    /// Caller invariant: data.len() == (w/8) * h
    fn write_region(
        &mut self,
        register: u8,
        x: u16, y: u16, w: u16, h: u16,
        data: &[u8],
    ) -> Result<(), EspError> {
        self.ensure_init()?;
        self.write_command(0x91)?;              // partial in
        self.set_partial_window(x, y, w, h)?;
        self.write_command(register)?;
        self.write_data(data)?;
        self.write_command(0x92)?;              // partial out
        Ok(())
    }

    /// Expand the dirty bounding box to include the 8×8 cell at (x, y).
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

    /// Extract a rectangular region from a framebuffer into a Vec.
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

    // --- Public API -----------------------------------------------------------

    /// Clear the entire display to white and perform a full refresh.
    /// Blocks until the refresh is complete (~1 s).
    pub fn clear(&mut self) -> Result<(), EspError> {
        self.desired.fill(0xFF);
        self.committed.fill(0xFF);
        self.dirty = None;

        self.ensure_init()?;

        let white = vec![0xFFu8; BUF_LEN];
        self.write_command(0x10)?;
        self.write_data(&white)?;
        self.write_command(0x13)?;
        self.write_data(&white)?;

        self.update_full()
    }

    /// Draw a character into the `desired` buffer at pixel position (x, y).
    ///
    /// Caller invariants:
    ///   - `x` must be a multiple of 8.
    ///   - `y + 8` must not exceed HEIGHT (320).
    ///   - `ch` must be an ASCII character in the range 0x20..=0x7F.
    ///
    /// Does NOT push to the display; call `try_flush` to push changes.
    pub fn draw_char(&mut self, x: u8, y: u16, ch: char) -> Result<(), EspError> {
        let code = ch as usize;
        if !(0x20..=0x7F).contains(&code) {
            return Ok(());
        }
        let glyph = &FONT_8X8[code - 0x20];
        let byte_col = (x as usize) / 8;

        for row in 0..8usize {
            let idx = (y as usize + row) * BYTES_PER_ROW + byte_col;
            if idx < BUF_LEN {
                self.desired[idx] = !glyph[row];
            }
        }
        self.mark_dirty(x as u16 & 0xFFF8, y, 8, 8);
        Ok(())
    }

    /// Draw white (blank) into the `desired` buffer for the 8-pixel-tall strip
    /// at row `y`, across the full display width.
    pub fn clear_line(&mut self, y: u16) {
        let start = y as usize * BYTES_PER_ROW;
        let end = start + BYTES_PER_ROW * 8;
        self.desired[start..end].fill(0xFF);
        self.mark_dirty(0, y, WIDTH, 8);
    }

    /// Non-blocking flush.  If the display is idle and there are dirty pixels,
    /// sends the minimal changed region and kicks off a partial refresh.
    ///
    /// Returns `Ok(true)` if a refresh was started, `Ok(false)` if the display
    /// was busy or there was nothing to flush.
    ///
    /// Caller invariant: `clear` must have been called at least once since power-on.
    pub fn try_flush(&mut self) -> Result<bool, EspError> {
        let (x_min, y_min, x_max, y_max) = match self.dirty.take() {
            Some(d) => d,
            None => return Ok(false),
        };

        let w = x_max - x_min;
        let h = y_max - y_min;
        let region = Self::extract_region(&self.desired, x_min, y_min, w, h);

        // Write new pixels to 0x13.
        self.write_region(0x13, x_min, y_min, w, h, &region)?;

        // Partial refresh (blocking — ~700 ms).
        self.ensure_init()?;
        self.write_command(0x91)?;
        self.set_partial_window(x_min, y_min, w, h)?;
        self.update_partial()?;
        self.write_command(0x92)?;

        // Write same pixels to 0x10 (baseline for next differential refresh).
        self.write_region(0x10, x_min, y_min, w, h, &region)?;

        // Update committed to match.
        let bytes_w = (w / 8) as usize;
        let x_byte = (x_min / 8) as usize;
        for row in 0..h as usize {
            let buf_start = (y_min as usize + row) * BYTES_PER_ROW + x_byte;
            let data_start = row * bytes_w;
            self.committed[buf_start..buf_start + bytes_w]
                .copy_from_slice(&region[data_start..data_start + bytes_w]);
        }

        Ok(true)
    }

    /// Turn off the panel driving voltages.  Call when done displaying.
    pub fn power_down(&mut self) -> Result<(), EspError> {
        self.power_off()
    }
}
