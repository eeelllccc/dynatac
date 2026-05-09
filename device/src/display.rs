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
//   - The FrameBuffer's committed buffer always mirrors the controller's "previous"
//     buffer (0x10).

use std::thread::sleep;
use std::time::Duration;

use esp_idf_svc::hal::gpio::{AnyIOPin, AnyOutputPin, Input, Output, PinDriver};
use esp_idf_svc::hal::spi::{SpiDeviceDriver, SpiDriver};
use esp_idf_svc::hal::sys::EspError;

use dynatac_core::framebuffer::{FrameBuffer, BUF_LEN};
use dynatac_core::lockscreen;

pub use dynatac_core::framebuffer::{WIDTH, HEIGHT};

/// Driver for the GDEQ031T10 e-paper display (UC8253 controller, 240×320).
///
/// Wraps a `FrameBuffer` for pixel manipulation and manages SPI communication
/// with the UC8253 controller for hardware refreshes.
///
/// Call `draw_char` / `clear_line` to update the desired buffer, then call
/// `try_flush` in a loop.  `try_flush` is non-blocking: if the display is
/// still refreshing it returns immediately.
pub struct Epd<'d> {
    spi: SpiDeviceDriver<'d, SpiDriver<'d>>,
    cs:   PinDriver<'d, AnyOutputPin, Output>,
    dc:   PinDriver<'d, AnyOutputPin, Output>,
    busy: PinDriver<'d, AnyIOPin,     Input>,

    init_done:    bool,
    power_is_on:  bool,

    pub fb: FrameBuffer,
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
            fb: FrameBuffer::new(),
        }
    }

    // --- Low-level SPI helpers ------------------------------------------------

    fn write_command(&mut self, cmd: u8) -> Result<(), EspError> {
        self.dc.set_low()?;
        self.cs.set_low()?;
        self.spi.write(&[cmd])?;
        self.cs.set_high()?;
        self.dc.set_high()?;
        Ok(())
    }

    fn write_data(&mut self, data: &[u8]) -> Result<(), EspError> {
        self.cs.set_low()?;
        self.spi.write(data)?;
        self.cs.set_high()?;
        Ok(())
    }

    fn wait_busy(&self) {
        // 10 ms >= one FreeRTOS tick (portTICK_PERIOD_MS=10), so usleep()
        // calls vTaskDelay(1) instead of busy-waiting with esp_rom_delay_us.
        // This keeps the IDLE task alive and prevents the task watchdog from
        // firing during long full-panel refreshes (which can take >5 s).
        sleep(Duration::from_millis(10));
        let mut timeout_ms: u32 = 10_000;
        while self.busy.is_low() {
            sleep(Duration::from_millis(10));
            timeout_ms = timeout_ms.saturating_sub(10);
            if timeout_ms == 0 {
                log::warn!("EPD BUSY timeout");
                break;
            }
        }
    }

    // --- Controller state machine ----------------------------------------------

    fn init_display(&mut self) -> Result<(), EspError> {
        self.write_command(0x00)?;
        self.write_data(&[0x1E, 0x0D])?;
        sleep(Duration::from_millis(1));
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

    /// Bring the panel driving voltages back up after a `power_down`.
    /// Idempotent. Called automatically by the partial / full update
    /// helpers, but exposed publicly so the lock/unlock path can keep
    /// the panel powered down across light sleep and explicitly bring
    /// it back when the user wakes the device.
    pub fn power_on(&mut self) -> Result<(), EspError> {
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

    fn set_partial_window(&mut self, x: u16, y: u16, w: u16, h: u16) -> Result<(), EspError> {
        let x_start = x & 0xFFF8;
        let x_end   = (x + w - 1) | 0x0007;
        let y_end   = y + h - 1;
        self.write_command(0x90)?;
        self.write_data(&[
            x_start as u8,
            x_end   as u8,
            (y      >> 8) as u8, (y      & 0xFF) as u8,
            (y_end  >> 8) as u8, (y_end  & 0xFF) as u8,
            0x01,
        ])?;
        Ok(())
    }

    fn update_full(&mut self) -> Result<(), EspError> {
        self.write_command(0xE0)?; self.write_data(&[0x02])?;
        self.write_command(0xE5)?; self.write_data(&[0x5A])?;
        self.write_command(0x50)?; self.write_data(&[0x97])?;
        self.power_on()?;
        self.write_command(0x12)?;
        self.wait_busy();
        self.init_done = false;
        Ok(())
    }

    fn update_partial(&mut self) -> Result<(), EspError> {
        self.write_command(0xE0)?; self.write_data(&[0x02])?;
        self.write_command(0xE5)?; self.write_data(&[0x79])?;
        self.write_command(0x50)?; self.write_data(&[0xD7])?;
        self.power_on()?;
        self.write_command(0x12)?;
        self.wait_busy();
        self.init_done = false;
        Ok(())
    }

    fn write_region(
        &mut self,
        register: u8,
        x: u16, y: u16, w: u16, h: u16,
        data: &[u8],
    ) -> Result<(), EspError> {
        self.ensure_init()?;
        self.write_command(0x91)?;
        self.set_partial_window(x, y, w, h)?;
        self.write_command(register)?;
        self.write_data(data)?;
        self.write_command(0x92)?;
        Ok(())
    }

    // --- Public API -----------------------------------------------------------

    /// Clear the entire display to white and perform a full refresh.
    pub fn clear(&mut self) -> Result<(), EspError> {
        self.fb.clear();

        self.ensure_init()?;

        let white = vec![0xFFu8; BUF_LEN];
        self.write_command(0x10)?;
        self.write_data(&white)?;
        self.write_command(0x13)?;
        self.write_data(&white)?;

        self.update_full()
    }

    /// Draw a character into the desired buffer. Does NOT push to hardware.
    pub fn draw_char(&mut self, x: u8, y: u16, ch: char) -> Result<(), EspError> {
        self.fb.draw_char(x, y, ch);
        Ok(())
    }

    /// Clear the 8-pixel-tall strip at row `y` across the full display width.
    pub fn clear_line(&mut self, y: u16) {
        self.fb.clear_line(y);
    }

    /// Non-blocking flush. Sends the minimal dirty region to hardware.
    pub fn try_flush(&mut self) -> Result<bool, EspError> {
        let region = match self.fb.take_dirty_region() {
            Some(r) => r,
            None => return Ok(false),
        };

        self.write_region(0x13, region.x, region.y, region.w, region.h, &region.data)?;

        self.ensure_init()?;
        self.write_command(0x91)?;
        self.set_partial_window(region.x, region.y, region.w, region.h)?;
        self.update_partial()?;
        self.write_command(0x92)?;

        self.write_region(0x10, region.x, region.y, region.w, region.h, &region.data)?;

        Ok(true)
    }

    /// Turn off the panel driving voltages.
    pub fn power_down(&mut self) -> Result<(), EspError> {
        self.power_off()
    }

    /// Render the lockscreen (white background + DYNATAC logo) using
    /// a full-screen refresh. The framebuffer is left in the same
    /// state as the displayed image so subsequent dirty-region tracking
    /// remains correct.
    ///
    /// After this returns the panel is still powered on; the caller
    /// should call `power_down` before entering light sleep.
    pub fn present_lockscreen(&mut self) -> Result<(), EspError> {
        // Draw into the framebuffer.
        lockscreen::render(&mut self.fb);
        // Drop the dirty bookkeeping — we're going to paint the whole
        // screen with a full refresh, not a partial flush.
        let _ = self.fb.take_dirty_region();

        self.ensure_init()?;

        // The full-refresh path expects both buffers (0x10 = previous,
        // 0x13 = new) to receive a full WIDTH×HEIGHT image.
        let mut full = vec![0u8; BUF_LEN];
        for i in 0..BUF_LEN {
            full[i] = self.fb.desired_byte(i);
        }
        self.write_command(0x10)?;
        self.write_data(&full)?;
        self.write_command(0x13)?;
        self.write_data(&full)?;

        self.update_full()
    }
}
