// Light-sleep helpers for the lockscreen path.
//
// On the T-Deck-Pro the keyboard controller (TCA8418) raises an
// open-drain IRQ on GPIO15 whenever a key event lands in its FIFO.
// We use that line as the wake source: ext0_wakeup on GPIO15 active
// LOW. The MCU sleeps until the user touches a key.
//
// Light sleep preserves RAM and the peripheral state we care about
// (GPIO config, I2C bus configuration, etc.), so when `light_sleep`
// returns the main loop just keeps running with no re-init needed.
//
// Caller invariants:
//   - The wake GPIO must be configured as an Input with an internal
//     pull-up before calling `enter`. We do this in `main` once at boot.
//   - The TCA8418 FIFO should be drained immediately before sleeping;
//     a stale event would keep IRQ asserted and the wake source would
//     fire instantly. (Sleeping then waking immediately is harmless,
//     so this is a soft requirement, not a correctness one.)
//   - Single-threaded entry only. The main loop is the only caller.

use esp_idf_svc::hal::sys::{
    esp, esp_deep_sleep_start, esp_light_sleep_start, esp_sleep_enable_ext0_wakeup, gpio_num_t,
    EspError,
};

/// GPIO pin the TCA8418 keyboard IRQ is wired to (active LOW).
pub const KEYBOARD_IRQ_GPIO: i32 = 15;

/// Enable light-sleep wake on the keyboard IRQ pin and enter light
/// sleep. Blocks until the wake source fires (or the call is rejected
/// because the pin was already in the wake state, in which case the
/// function returns immediately).
pub fn enter() -> Result<(), EspError> {
    // ext0 wakeup: pin level 0 == LOW.
    esp!(unsafe { esp_sleep_enable_ext0_wakeup(KEYBOARD_IRQ_GPIO as gpio_num_t, 0) })?;
    esp!(unsafe { esp_light_sleep_start() })?;
    Ok(())
}

/// Enter deep sleep with no configured wake source.
///
/// The only way to exit this state is pressing the physical power button,
/// which is wired to CHIP_PU (the ESP32-S3 hardware reset line). Pressing it
/// pulls CHIP_PU low, which resets the chip; when released the chip boots
/// fresh from ROM. This is the clean "power off" — the user sees the device
/// sleep and pressing the button starts a normal boot.
///
/// This function never returns.
pub fn power_off() -> ! {
    unsafe { esp_deep_sleep_start() };
    loop {}
}
