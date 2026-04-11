// Shared I2C bus wrapper for the T-Deck-Pro.
//
// The board has a single I2C master (GPIO13/14) with several slaves:
// keyboard (0x34), touch (0x1A), gyro (0x28), fuel gauge (0x55),
// charger (0x6B). Each driver gets an `I2cDevice` handle, and the
// underlying `I2cDriver` is borrowed only for the duration of a
// single transaction via `RefCell`.
//
// Caller invariants:
//   - Single-threaded use only. Nested transactions on the same bus
//     will panic at the `RefCell` borrow.
//   - The `I2cBus` must outlive every `I2cDevice` it hands out.

use std::cell::RefCell;

use esp_idf_svc::hal::i2c::I2cDriver;
use esp_idf_svc::hal::sys::EspError;

const TIMEOUT_MS: u32 = 100;

/// Owns the underlying I2C master and hands out per-slave device handles.
pub struct I2cBus<'d> {
    inner: RefCell<I2cDriver<'d>>,
}

impl<'d> I2cBus<'d> {
    pub fn new(driver: I2cDriver<'d>) -> Self {
        Self {
            inner: RefCell::new(driver),
        }
    }

    /// Create a handle bound to one 7-bit slave address.
    pub fn device(&self, addr: u8) -> I2cDevice<'_, 'd> {
        I2cDevice { bus: self, addr }
    }
}

/// A lightweight handle representing one slave on the shared bus.
/// Cheap to create; borrows the bus only during each transaction.
pub struct I2cDevice<'a, 'd> {
    bus: &'a I2cBus<'d>,
    addr: u8,
}

impl<'a, 'd> I2cDevice<'a, 'd> {
    pub fn write(&self, bytes: &[u8]) -> Result<(), EspError> {
        self.bus
            .inner
            .borrow_mut()
            .write(self.addr, bytes, TIMEOUT_MS)
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<(), EspError> {
        self.bus
            .inner
            .borrow_mut()
            .read(self.addr, buf, TIMEOUT_MS)
    }

    pub fn write_read(&self, bytes: &[u8], buf: &mut [u8]) -> Result<(), EspError> {
        self.bus
            .inner
            .borrow_mut()
            .write_read(self.addr, bytes, buf, TIMEOUT_MS)
    }
}
