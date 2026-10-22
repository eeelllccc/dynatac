# SPI bus: research findings

Status: research done 2026-10-21, before any driver code. This is the hardware evidence the spi
task (see `os-design.md`, "SPI driver") is built on, in the role `captures/README.md` plays for
the modem. Facts are tagged with their source. "Verify" marks things to confirm on hardware.

Sources:
- **[SCH]** board schematic, `T-Deck-Pro/hardware/T-Deckpro v1.0 24-05-16/` (pages 2–4, 6)
- **[PINS]** `T-Deck-Pro/examples/factory/utilities.h`; **[FAC]** `examples/factory/factory.ino`,
  `peri_lora.cpp`: the vendor firmware
- **[UC]** `T-Deck-Pro/hardware/UC8253.pdf` (UC8253c A0.6); **[GX]** GxEPD2
  `lib/GxEPD2/src/gdeq/GxEPD2_310_GDEQ031T10.cpp` and `GxEPD2_EPD.cpp`
- **[SX]** Semtech SX1261/2 datasheet rev 1.2 (June 2019); **[S62F]**
  `T-Deck-Pro/hardware/S62F_Lora.pdf`
- **[ELM]** elm-chan, "How to Use MMC/SDC" (http://elm-chan.org/docs/mmc/mmc_e.html)
- **[IDF]** ESP-IDF SPI Master Driver docs, ESP32-S3
  (https://docs.espressif.com/projects/esp-idf/en/latest/esp32s3/api-reference/peripherals/spi_master.html)

## 1. The bus and its devices

One SPI bus (we use SPI2), three devices. **[PINS] [SCH]**

| Signal | GPIO | Notes |
|---|---|---|
| SCK | 36 | shared |
| MOSI | 33 | shared |
| MISO | 47 | shared by SD and LoRa; the e-paper has no MISO |
| E-paper CS / DC / BUSY | 34 / 35 / 37 | BUSY low = busy. No reset line (see 2.4) |
| SD CS | 48 | no card-detect line to the ESP32 **[SCH]** |
| LoRa CS / RST / BUSY / DIO1 | 3 / 4 / 6 / 5 | DIO1 is the IRQ line ("LORA_INT") |
| LoRa power enable | 46 | `LORA_EN` switches `LORA_VDD` **[SCH]** |

Power: the e-paper and the SD card are on the always-on 3.3 V rail. The LoRa module is powered
only while GPIO46 is high. **[SCH]**

These don't route through the IO_MUX SPI2 pins (CS0 10, SCLK 12, MISO 13, MOSI 11), so the GPIO
matrix is used. That costs nothing at ≤ 40 MHz. **[IDF]** All our devices run far slower.

Strapping pins: GPIO3 (LoRa CS) and GPIO46 (LoRa enable) are ESP32-S3 strapping pins, so their
level at reset matters. GPIO46 is pulled down at reset, so LoRa starts unpowered. Verify: nothing
on the board pulls GPIO3 in a way that changes boot behaviour.

### Vendor firmware: how they share the bus **[FAC]**
- At boot: all EN pins high (LoRa powered), then **every CS driven high before `SPI.begin`**
  ("LORA, SD, EPD use the same SPI, in order to avoid mutual influence").
- One recursive mutex around every use of any device. Before each use, all CSs go high.
- **The mutex is held through the whole e-paper refresh**, BUSY wait included (GxEPD2 waits in
  `_waitWhileBusy`). The bus is idle for most of that time. Our design serves the other devices
  then instead (see 2.3).
- E-paper clock **2 MHz** (`FACTORY_EPD_SPI_HZ`). GxEPD2's own default is 4 MHz.
- Init order: e-paper, LoRa, (I2C devices), SD.

## 2. E-paper: GDEQ031T10 with a UC8253 controller

### 2.1 Interface **[UC] [SCH]**
- 4-wire SPI, mode 0, MSB first, 8-bit units. DC low = command, DC high = data.
- **Write clock: TSCYCW ≥ 170 ns, so ≤ 5.88 MHz** (AC characteristics, 4-wire table, VDD = 2.3 V).
  The feature list says "up to 20 MHz", but the timing table is the guarantee.
  **Our driver runs at 10 MHz today: out of spec.** It works on our board, but that isn't a
  guarantee. Read clock: ≥ 350 ns, so ≤ 2.86 MHz.
- CS setup 60 ns, CS hold 65 ns, DC setup/hold 30 ns: trivial at these clocks.
- "To prevent malfunction due to noise, it is recommended to set the CSB signal to HIGH every
  8 bits" (the serial counter resets on the CSB rising edge). GxEPD2 ignores this and streams
  whole buffers with CS low. Decide: per-byte CS (≈ 9.6 k CS toggles per full frame) or streaming.
  Streaming is what every known driver does; we stream, and keep CS high between commands.
- Reads go over SDA, i.e. 3-wire half-duplex on our MOSI line, because the panel has no MISO.
  We don't need reads: BUSY gives the status.

### 2.2 BUSY **[UC] [GX]**
- Commands that start internal work pull BUSY_N low until done: PON (power on), POF (power off),
  DRF (refresh), DSLP (deep sleep), PMES, AUTO, TSC, LPD, AMV, APG.
- **While a refresh runs, the panel only accepts the read commands (REV, FLG, VV).** Everything
  else must wait for BUSY high.
- The first status read is allowed no sooner than 200 µs after BUSY falls.
- After soft reset (PSR with RST_N = 0), BUSY stays low ≥ 50 µs.
- Typical durations (GxEPD2 constants): power on/off ~50 ms each, full refresh ~1.0 s (fast
  mode) or ~3.1 s (temperature-compensated), partial refresh ~0.65 s. GxEPD2 times out after
  10 s.

### 2.3 What this means for the bus
- A partial refresh sends ≤ 2 × 9600 bytes (old and new image buffers, 0x10 and 0x13), about
  33 ms at 5 MHz in total, then leaves the bus idle for ~0.65 s of BUSY. **The bus is ours for
  over 90 % of each refresh**, so the scheduler serves SD and LoRa while BUSY is low. Rule: no
  e-paper transaction while BUSY is low. Other devices' transactions (e-paper CS high) are fine.
- BUSY high → e-paper ready. BUSY stuck low past a timeout is a fault to report, never a hang.

### 2.4 Power and reset **[UC] [SCH] [GX]**
- **The panel's RST_N is tied to a power-on RC network on the board, not to a GPIO** (pinmap
  `BOARD_EPD_RST -1`). Deep sleep (DSLP 0x07, check code 0xA5) can only be left by a hardware
  reset, so **on this board deep sleep is a one-way trip until power is cycled.** GxEPD2 skips
  DSLP when there is no reset pin. So do we. Lowest-power state: POF (power off the charge
  pump); the image stays.
- After a hardware reset, wait ≥ 1 ms before the first command (commands meanwhile are ignored).
- Soft reset: PSR (0x00) with RST_N bit = 0, e.g. `0x1e, 0x0d` (what GxEPD2's `_InitDisplay`
  sends, then `0x1f, 0x0d`).
- GxEPD2's "fast" refreshes force a temperature (CCSET 0xE0 = 0x02, TSSET 0xE5 = 0x5A full /
  0x79 partial) instead of using the on-chip sensor. That is faster, but outside the normal
  temperature range the image degrades. Our driver does the same for partials. Note it as a
  policy choice, not a fact of the panel.

## 3. SD card in SPI mode **[ELM]**

### 3.1 Bring-up
- ≥ 1 ms after power; SCK **100–400 kHz**; CS **and MOSI high**; **≥ 74 clocks**.
- CMD0 with a valid CRC → R1 = 0x01 (idle). In the idle state only CMD0/1/8/ACMD41/58/59 work.
- CMD8 (arg 0x1AA, valid CRC) → R7. Illegal command → SDv1/MMC. Otherwise the low 12 bits must
  echo 0x1AA.
- CMD55 + ACMD41 (HCS bit 30 set), repeated until R1 = 0x00. "Can take hundreds of ms." We need
  a timeout: 1 s is the common choice; verify against the SD spec.
- CMD58 → OCR. CCS (bit 30) set = block addressing (SDHC/SDXC).
- CMD16(512) for byte-addressed cards.
- Then raise SCK, usually to 25 MHz (TRAN_SPEED).
- **CRC is off by default in SPI mode. CMD59 turns it on.** We turn it on: without it, a bit
  flip on the bus is silent data corruption (the lesson from the modem UART, which has no
  checksum at all). Verify the CRC cost is acceptable.

### 3.2 Transactions
- Response arrives 0–8 bytes after the command (keep MOSI high by sending 0xFF).
- R1b: busy (MISO held low) after the response.
- Single-block read CMD17: wait for the data token, read 512 bytes + 2 CRC bytes.
- Write CMD24: data packet → data response → busy. Multi-block: CMD18 + CMD12 (discard the stuff
  byte), CMD25 + Stop Tran token.
- **Check busy before each command rather than waiting after each one.** The card may be
  deselected while busy and keeps working; it holds MISO low again when reselected.
- Token values and write timeouts are not on the elm-chan page: take them from the SD Physical
  Layer Simplified Spec (to fetch).

### 3.3 Bus-sharing hazards
1. **The card drives MISO synchronously with SCK, not with CS.** After CS goes high, **clock one
   more byte** (CS high) so it lets go of MISO. Otherwise it can corrupt the next device's reply.
2. **Put the card into SPI mode before talking to any other device on the bus.** At power-up it
   is in SD mode and doesn't stay quiet just because its CS is high. That holds even if we never
   use the card, and a card can be inserted at any time (see 4).
3. MOSI and MISO need pull-ups (50–100 kΩ). The MISO one "cannot be omitted". The board has
   pull-ups on the card lines **[SCH]**; verify their values.
4. Insertion can dip the 3.3 V rail by ~600 mV, enough to trip a brown-out.

## 4. LoRa: AcSiP S62F (SX1262) **[SX] [S62F] [FAC]**

- S62F = SX1262 with an **embedded 32 MHz TCXO**, 865–928 MHz. **[S62F]** The vendor firmware sets
  a TCXO voltage of 2.4 V (RadioLib `setTCXO(2.4)` = `SetDio3AsTcxoCtrl`) and **DIO2 as the RF
  switch** (`SetDio2AsRfSwitchCtrl`; DIO2 then can't be an IRQ). **[FAC]**
- SPI mode 0, **≤ 16 MHz**. **MISO is high-impedance while NSS is high**, so it shares well.
  NSS-to-SCK setup ≥ 32 ns.
- **BUSY: wait for BUSY low before every command**, read or write. BUSY also goes high on its
  own while the chip handles an internal IRQ. After a write command, BUSY rises within
  TSW ≤ 600 ns of NSS going high, so **never conclude "ready" by sampling BUSY sooner than 600 ns
  after a command.**
- Sleep: BUSY held high (20 kΩ pull-up). Wake on the NSS falling edge, and BUSY stays high until
  STBY_RC: **~3.5 ms cold start, ~340 µs warm**. Same after power-up or reset.
- Reset: NRESET low ~100 µs, then calibration.
- The data buffer (256 bytes) is lost in sleep.
- **Powering: our `main.rs` drives LoRa CS (GPIO3) and RST (GPIO4) high but never sets
  `LORA_EN` (GPIO46), so the module is unpowered with logic-high levels on its pins.** That can
  back-power a chip through its ESD diodes. ~~Rule for the driver: while `LORA_EN` is low, hold
  the LoRa CS and RST low.~~ **Measured wrong (2026-10-22):** with LoRa unpowered and CS/RST low,
  the e-paper ignores everything (the unpowered chip loads SCK/MOSI). The rule is: keep LoRa
  powered whenever the bus is in use, and put it to sleep (SetSleep). See spi-design.md Build
  notes.

## 5. ESP32-S3 SPI master (ESP-IDF `spi_master`) **[IDF]**

- **Thread safety: safe only if each device is used by one task.** Our design is stricter
  (one task for the whole bus), so contention can't happen.
- Transaction kinds: **polling** (busy-wait, shortest; per-transaction overhead ~9–11 µs) or
  **interrupt** (the task sleeps while hardware/DMA works; overhead ~24–26 µs, also costs a
  context switch). Rule of thumb: polling for short commands, interrupt + DMA for image data
  and SD blocks. Don't mix them on a device without draining interrupt results first.
- DMA: buffers in internal DMA-capable memory, 32-bit aligned, length a multiple of 4, or the
  driver silently copies into a temporary buffer. `max_transfer_sz` defaults to **4092 bytes with
  DMA** (64 without). A 9600-byte image buffer is therefore several transactions, or we raise
  `max_transfer_sz`. DMA from PSRAM is possible but "transmission data may be lost" under MSPI
  contention: keep SPI buffers in internal RAM.
- Clock: each `spi_device` has its own clock/mode, applied per transaction. The actual frequency
  is the nearest the hardware can make (`spi_device_get_actual_freq`); log it at init.
- CS: hardware CS per device (`spics_io_num`), or -1 and drive CS ourselves. The docs don't
  spell out the -1 case; to verify.
- Flash operations disable the SPI ISR unless `CONFIG_SPI_MASTER_ISR_IN_IRAM`: interrupt
  transactions stall during flash writes (NVS).
- Light sleep: `SPICOMMON_BUSFLAG_SLP_ALLOW_PD` lets the peripheral power down and recover.
  No other power-management interaction is documented.

## 6. Prior art to evaluate

- **embedded-sdmmc** (Rust): SD over SPI plus FAT16/32. FAT sits on a `BlockDevice` trait, so its
  FAT layer could run over our block layer. Its SD driver is blocking and talks to the bus itself,
  so we'd use only the FAT part (or only its command logic as a reference).
- **sx126x**, **lora-phy** (Rust) and RadioLib (C++, in the T-Deck-Pro repo): command sequences
  and the BUSY handling to copy.
- **GxEPD2** (C++, in the repo): the UC8253 sequences above.
- **elm-chan FatFs**: what ESP-IDF's FAT uses; the reference for FAT behaviour.

## 7. Implications for the driver

1. **Bus bring-up order:** all CSs high, LoRa powered (see §4: unpowered, it blocks the bus), then the SD card
   into SPI mode (74 clocks, CMD0) **before any other device**, card present or not, then the
   e-paper, then LoRa when powered.
2. **Every transaction on any device ends with CS high plus one extra byte** (for the SD card's
   MISO release). That's cheap, and it makes "the SD card never interferes" a bus-level invariant
   instead of an SD-driver detail.
3. **Each device has a "may I start a transaction?" gate the scheduler checks:** e-paper → BUSY
   high; LoRa → BUSY low (and ≥ 600 ns since its last command); SD → not busy (a probe).
   Waiting is a state, never a loop.
4. **Clocks per device:** e-paper ≤ 5.88 MHz (fix today's 10 MHz), SD 400 kHz for init then
   ≤ 25 MHz, LoRa ≤ 16 MHz.
5. **Faults to model in the mock bus:** BUSY stuck (either way), SD absent / removed mid-transfer
   / busy forever / CRC error, LoRa unpowered (MISO floating: the MISO pull-up reads 0xFF), a
   device holding MISO, and a brown-out on SD insertion.
6. **Build on `spi_master`** (one `spi_device` per device for clock and mode, all used only by
   the spi task, CS driven by our code so items 2 and 3 are explicit), rather than driving the
   registers directly. That's less code and well tested, and the single-owner rule removes its
   one weakness (multi-task use). Revisit only if the measured overhead matters.

## 8. Still to find out

- SD Physical Layer Simplified Spec: token values, write busy timeout (commonly 250 ms for
  SDHC, 500 ms for SDXC), ACMD41 timeout (commonly 1 s).
- On hardware: the e-paper at 5 MHz versus 10 MHz (refresh timing should be unchanged; it is
  BUSY-dominated); CS = -1 with our own CS; the actual SPI frequencies; refresh durations;
  BUSY behaviour on POF/PON; pull-up values on the SD lines.
- Whether the e-paper's REV read (3-wire, ≤ 2.86 MHz) is worth having as a presence check.
