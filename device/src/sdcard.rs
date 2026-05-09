//! SD card filesystem mount/unmount via ESP-IDF SDSPI + FATFS.
//!
//! The SD card shares the SPI2 bus with the e-paper display and LoRa module.
//! ESP-IDF's SPI master driver handles bus arbitration; SDSPI adds the SD
//! card as another device on that already-initialised bus.
//!
//! Hardware connections:
//!   SCK  = GPIO36  (shared SPI2 bus)
//!   MOSI = GPIO33  (shared SPI2 bus)
//!   MISO = GPIO47  (shared SPI2 bus)
//!   CS   = GPIO48  (SD card only)
//!
//! Caller invariants:
//!   - The SPI2 bus must have been initialised (via `SpiDriver::new`) before
//!     calling `mount()`.
//!   - GPIO48 must not be owned by a `PinDriver` when `mount()` is called;
//!     SDSPI takes ownership of the CS pin.
//!
//! Callee invariants:
//!   - `SdCardFs` unmounts and releases all resources when dropped.
//!   - Files are accessed via paths relative to the SD root (no leading `/`).
//!   - The VFS is mounted at `/sdcard`; `SdCardFs` prepends that prefix.

use std::ffi::CString;
use std::ptr;

use esp_idf_svc::sys::{
    esp_vfs_fat_mount_config_t, esp_vfs_fat_sdcard_unmount, esp_vfs_fat_sdspi_mount,
    gpio_num_t_GPIO_NUM_NC, sdmmc_card_t, sdmmc_delay_phase_t_SDMMC_DELAY_PHASE_0,
    sdmmc_host_t, sdmmc_host_t__bindgen_ty_1, sdspi_device_config_t, sdspi_host_do_transaction,
    sdspi_host_get_real_freq, sdspi_host_init, sdspi_host_io_int_enable, sdspi_host_io_int_wait,
    sdspi_host_remove_device, sdspi_host_set_card_clk, spi_host_device_t_SPI2_HOST, ESP_OK,
    SDMMC_FREQ_DEFAULT,
};

use dynatac_core::fs::{FsError, FileSystem};

const MOUNT_POINT: &str = "/sdcard";

// Mirrors the SDSPI_HOST_DEFAULT() C macro.
const SDMMC_HOST_FLAG_SPI: u32 = 1 << 3;
const SDMMC_HOST_FLAG_DEINIT_ARG: u32 = 1 << 5;

fn sdspi_host_default() -> sdmmc_host_t {
    sdmmc_host_t {
        flags: SDMMC_HOST_FLAG_SPI | SDMMC_HOST_FLAG_DEINIT_ARG,
        slot: spi_host_device_t_SPI2_HOST as i32,
        max_freq_khz: SDMMC_FREQ_DEFAULT as i32,
        io_voltage: 3.3,
        init: Some(sdspi_host_init),
        set_bus_width: None,
        get_bus_width: None,
        set_bus_ddr_mode: None,
        set_card_clk: Some(sdspi_host_set_card_clk),
        set_cclk_always_on: None,
        do_transaction: Some(sdspi_host_do_transaction),
        __bindgen_anon_1: sdmmc_host_t__bindgen_ty_1 {
            deinit_p: Some(sdspi_host_remove_device),
        },
        io_int_enable: Some(sdspi_host_io_int_enable),
        io_int_wait: Some(sdspi_host_io_int_wait),
        command_timeout_ms: 0,
        get_real_freq: Some(sdspi_host_get_real_freq),
        input_delay_phase: sdmmc_delay_phase_t_SDMMC_DELAY_PHASE_0,
        set_input_delay: None,
    }
}

/// A mounted SD card filesystem. Unmounts on drop.
pub struct SdCardFs {
    card: *mut sdmmc_card_t,
}

// SAFETY: SdCardFs holds a raw pointer to an sdmmc_card_t allocated by ESP-IDF.
// We never share it across threads; the pointer is only used in Drop.
unsafe impl Send for SdCardFs {}

impl SdCardFs {
    /// Mount the SD card on the SPI2 bus and return a handle.
    ///
    /// Returns `Err` if the card is absent, unreadable, or the mount fails.
    pub fn mount() -> Result<Self, FsError> {
        let mount_point = CString::new(MOUNT_POINT).unwrap();

        let host = sdspi_host_default();

        let slot = sdspi_device_config_t {
            host_id: spi_host_device_t_SPI2_HOST,
            gpio_cs: 48,
            gpio_cd: gpio_num_t_GPIO_NUM_NC,
            gpio_wp: gpio_num_t_GPIO_NUM_NC,
            gpio_int: gpio_num_t_GPIO_NUM_NC,
            gpio_wp_polarity: false,
        };

        let mount_config = esp_vfs_fat_mount_config_t {
            format_if_mount_failed: false,
            max_files: 8,
            allocation_unit_size: 16 * 1024,
            disk_status_check_enable: false,
        };

        let mut card: *mut sdmmc_card_t = ptr::null_mut();

        let err = unsafe {
            esp_vfs_fat_sdspi_mount(
                mount_point.as_ptr(),
                &host,
                &slot,
                &mount_config,
                &mut card,
            )
        };

        if err != ESP_OK as i32 {
            return Err(FsError::Unavailable);
        }

        Ok(SdCardFs { card })
    }

    fn full_path(&self, relative: &str) -> String {
        format!("{}/{}", MOUNT_POINT, relative)
    }
}

impl Drop for SdCardFs {
    fn drop(&mut self) {
        let mount_point = CString::new(MOUNT_POINT).unwrap();
        unsafe {
            esp_vfs_fat_sdcard_unmount(mount_point.as_ptr(), self.card);
        }
    }
}

impl FileSystem for SdCardFs {
    fn read(&self, path: &str) -> Result<Vec<u8>, FsError> {
        std::fs::read(self.full_path(path)).map_err(|e| FsError::Io(e.to_string()))
    }

    fn write(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        let full = self.full_path(path);
        if let Some(parent) = std::path::Path::new(&full).parent() {
            std::fs::create_dir_all(parent).map_err(|e| FsError::Io(e.to_string()))?;
        }
        std::fs::write(full, data).map_err(|e| FsError::Io(e.to_string()))
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<(), FsError> {
        use std::io::Write;
        let full = self.full_path(path);
        if let Some(parent) = std::path::Path::new(&full).parent() {
            std::fs::create_dir_all(parent).map_err(|e| FsError::Io(e.to_string()))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(full)
            .map_err(|e| FsError::Io(e.to_string()))?;
        file.write_all(data).map_err(|e| FsError::Io(e.to_string()))
    }

    fn list_dir(&self, dir: &str) -> Result<Vec<String>, FsError> {
        let full = self.full_path(dir);
        let rd = std::fs::read_dir(&full).map_err(|e| FsError::Io(e.to_string()))?;
        let mut names = Vec::new();
        for entry in rd {
            let entry = entry.map_err(|e| FsError::Io(e.to_string()))?;
            if let Some(name) = entry.file_name().to_str() {
                names.push(name.to_string());
            }
        }
        names.sort();
        Ok(names)
    }

    fn delete(&mut self, path: &str) -> Result<(), FsError> {
        let full = self.full_path(path);
        match std::fs::remove_file(&full) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(FsError::Io(e.to_string())),
        }
    }

    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(&self.full_path(path)).exists()
    }
}
