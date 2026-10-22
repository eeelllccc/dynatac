# SPI bus driver: design

Status: draft for review, 2026-10-21. Based on `spi-research.md` (cited as R§n). Part of the OS
design (`os-design.md`, "SPI driver"). Covers the interfaces and invariants. The SD and LoRa
protocols get their own design passes when they are built; the bus is designed for all three
devices from the start.

## Goals

1. The spi task is the only code that touches SPI2 and the pins of its devices.
2. No device can disturb another. Nothing breaks the bus rules that the hardware imposes
   (R§7), and the simulator checks every rule in every test.
3. No device can stall the others. Waiting (BUSY, card busy, radio busy) is a state, never a
   loop. Long writes are cut into chunks so other devices get turns.
4. Every request gets exactly one response. Every wait has a deadline. A fault is reported and
   recovered from, never hung on.
5. All logic is in `core/`, tested on the host against a wire-level simulator. The device code
   only carries out pin changes and byte transfers.

## Layers

Same shape as the modem stack:

```
 kernel ── Request ──►  Spi            (core) scheduler: one transaction at a time, round-robin
        ◄── Response ─   ├ Epaper       (core) UC8253 protocol state machine
                         ├ SdCard       (core) SD-over-SPI protocol (later)
                         └ Radio        (core) SX1262 protocol (later)
                         Bus            (core) runs one Transaction on a Wire, enforcing bus rules
                         Wire trait     ── device: spi_master + GPIO   | test: MockWire + simulators
```

### Wire: what the hardware can do (L0)

The smallest interface the device has to implement: raw pins and raw bytes. It has no idea of
devices, CS rules or protocols.

```rust
pub enum Pin { EpdCs, EpdDc, EpdBusy, SdCs, LoraCs, LoraRst, LoraBusy, LoraDio1, LoraEn }

pub trait Wire {
    fn set(&mut self, pin: Pin, high: bool);
    fn get(&mut self, pin: Pin) -> bool;
    /// Clock `out` at `clock`, mode 0, capturing MISO into `input` (same length) if given.
    /// No CS handling.
    fn transfer(&mut self, clock: Clock, out: &[u8], input: Option<&mut [u8]>) -> Result<(), WireError>;
    /// Busy-wait at least `us` microseconds (only ever ≤ a few µs; see the Radio gate).
    fn pause_us(&mut self, us: u32);
}
```

The device implementation uses one `spi_device` per clock setting (R§5, R§7.6), with CS = -1
(verify at bring-up), and DMA for transfers over 64 bytes. Output pins: CSs, DC, LoRa RST and EN.
Input pins: the two BUSYs and DIO1.

### Bus: one transaction, with the bus rules (core)

A transaction is data. The `Bus` carries it out over a `Wire`:

```rust
pub struct Transaction { pub device: Device, pub parts: Vec<Part> }
pub enum Part {
    Command(Vec<u8>),     // e-paper only: DC low
    Write(Vec<u8>),       // DC high for the e-paper
    Read(usize),          // send 0xFF, keep what comes back
    Exchange(Vec<u8>),    // full duplex, keep what comes back
}
```

`Bus::run(&mut wire, &Transaction) -> Result<Vec<u8>, WireError>` does, in order: check that
every CS is high; set the device's clock; CS low; the parts; CS high; **one 0xFF byte with every
CS high** (the SD MISO release, R§3.3). Before any of that, the bus must have been brought up.

**Bus invariants** (the `Bus` guarantees them; `MockWire` checks them on every call):
- B1 At most one CS is low at any time, and only inside a transaction.
- B2 Every transaction ends with all CSs high plus one release byte, before the next CS goes low.
- B3 Each device's clock is within its limit: e-paper ≤ 5.88 MHz, SD ≤ 400 kHz until initialised
  then ≤ 25 MHz, LoRa ≤ 16 MHz.
- B4 Bring-up order: all CSs high, LoRa powered, ≥ 74 clocks to the SD card
  with CS high, then CMD0, all before any other device's first transaction (R§3.3, R§7.1).
- B5 No clocks while the LoRa supply is off: an unpowered SX1262 on SCK/MOSI stops the e-paper
  hearing anything (measured, see Build notes). So LoRa is powered, deselected and out of reset
  from bring-up on, and bring-up puts it to sleep (SetSleep, ~160 nA). *(This replaced the first
  B5, "LoRa CS and RST low while unpowered", which hardware proved wrong.)*

### Device protocols (core)

Each device is a pure state machine with one interface. It never touches the bus itself; it
says what it wants next:

```rust
pub enum Next {
    Idle,                                   // nothing to do
    Transaction(Transaction),               // ready to run now
    WaitPin { pin: Pin, high: bool, deadline: Millis }, // e.g. e-paper BUSY high
    WaitUntil(Millis),                      // a timed step (reset pulse, retry back-off)
}

trait Protocol {
    fn next(&mut self, now: Millis) -> Next;
    /// Result of the transaction it last returned, or a timed-out wait.
    fn on_result(&mut self, result: Result<Vec<u8>, Fault>, now: Millis);
}
```

The waits are per device. The e-paper refreshing (BUSY low) doesn't stop the scheduler running
a radio transaction.

**Protocol invariants** (each protocol guarantees them; the simulators check them):
- P1 A device's transactions only start when its gate is open:
  - e-paper: BUSY high, except for the read commands;
  - LoRa: BUSY low, and BUSY not sampled within 600 ns of the end of a command (the protocol puts
    a `pause_us(1)` before sampling; R§4);
  - SD: not busy (probe with `Read`).
- P2 Every wait has a deadline. Timeouts are the datasheet's, or 10 s for an e-paper refresh
  (GxEPD2's value).
- P3 Long writes are chunked. E-paper image data goes in transactions of ≤ 4092 bytes (the DMA
  maximum, R§5), with CS raised between chunks; the controller allows it (R§2.1: it recommends
  CS high every 8 bits, and GxEPD2 sends each command parameter in its own CS frame). So no
  transaction holds the bus for more than ~7 ms at 5 MHz.

### Spi: the scheduler (core)

```rust
impl Spi {
    pub fn submit(&mut self, request: Request, now: Millis) -> RequestId;
    /// Run every transaction that is ready, round-robin across devices, until none is.
    pub fn step<W: Wire>(&mut self, wire: &mut W, now: Millis, out: &mut Vec<Response>);
    /// When to call `step` next: the earliest deadline, and the pin levels being waited on.
    pub fn wake(&self) -> Wake;
}
```

- **Round-robin, no priorities.** Each device with a transaction ready gets one, then the turn
  passes to the next device. Within a device, transactions run in order.
- **Invariant S1: a ready device waits at most one transaction from each other device**, about
  7 ms in total. The only large transactions are image chunks: 4092 bytes at 5 MHz ≈ 6.5 ms. An
  SD block takes ≈ 0.2 ms at 25 MHz, and a LoRa command or packet read ≈ 0.1 ms. The tightest
  real deadline is the radio's: a received packet must be read before the next one overwrites
  it, ≥ ~60 ms apart at the vendor's settings (SF10, 250 kHz), ~10 ms at aggressive ones. If
  radio settings ever make 7 ms too long, the fix is smaller image chunks, not priorities.
- Requests (first version: e-paper only):
  - `Draw { region, data, mode: Partial | Full }` → `Drawn(Result<(), Fault>)`, sent once the
    refresh has finished and the controller's "previous" buffer matches.
  - `PowerOff` → `Done`.
  - SD: block read/write. LoRa: configure, send, packets received as events. Both later.
- **Every request gets exactly one response.**
- **After any e-paper fault the panel's contents are unknown.** The response says so, and the
  kernel's next draw is a full refresh of the whole screen.

### The spi task (device)

```
loop {
    wait on the task notification, until spi.wake().deadline;  // the only wait (os-design rule 3)
    take every queued request → spi.submit
    spi.step(&mut wire, now, &mut out) → send responses to the kernel
    arm GPIO interrupts for spi.wake().pins, then re-read those pins  // no lost wake-ups
}
```

Requests from the kernel and GPIO interrupts (BUSY edges, DIO1) both raise the task
notification, so the task still waits in exactly one place. Device invariant: arm the
interrupt, *then* re-read the pin, then sleep only if the level isn't already the one awaited.
That closes the race where the edge came before the arm.

## Testing (the digital twin)

- **`MockWire`** (test builds only, as the modem's `Sim`): tracks every pin level and a simulated
  clock, and routes the bytes of each transfer to whichever simulated device has CS low. It
  **panics on any bus-rule violation** (B1–B5), and each simulator panics on any protocol
  violation for its chip:
  - **`SimEpaper`**: UC8253 command set as we use it; BUSY low for a configured time after PON,
    POF and DRF; commands other than REV/FLG/VV while BUSY is low are violations; keeps the
    "previous" and "current" buffers so tests can check what the panel shows.
  - **`SimSdCard`**: SD-mode until CMD0; drives MISO until a clock after CS high (so a missing
    release byte corrupts the next device's read); busy after writes. Later: CRC, removal.
  - **`SimRadio`**: BUSY timing (high ≥ 600 ns after commands, high during sleep, ~3.5 ms cold
    wake); unpowered = MISO floating, reads 0xFF.
- **Faults** (the modem's `faults.rs` approach): BUSY stuck high or low, a refresh far slower than
  usual, `WireError`s, the card absent / removed / busy forever, the radio unpowered mid-command.
  Asserted invariants: every request answered once, no simulator panics, recovery afterwards.
- **Golden checks:** the e-paper's command sequences compared against GxEPD2's for the same
  operation.

## Hardware: what needs testing, and when

Nothing in this design is blocked on unknown hardware behaviour. The datasheets and the vendor
code settle everything the interfaces depend on. These get checked at the first bring-up, in
one example (`spi_check`, like `modem_hal_check`), not as separate manual tests:
- the e-paper at ≤ 5.88 MHz: refresh works and refresh times are unchanged;
- CS = -1 on `spi_device` with our own CS pins; log the actual clocks;
- image data chunked with CS raised between chunks still displays correctly (P3);
- BUSY edge interrupts arrive, with no lost wake-ups;
- measured partial/full refresh and power-on/off times, which set the simulator's defaults.

The SD and LoRa specifics (card timeouts, radio wake times on this module) are checked when those
protocols are built.

## Build order

1. `Wire`, `Transaction`, `Bus` with B1–B5; `MockWire` with rule checks; tests.
2. `SimEpaper`, then the `Epaper` protocol (replacing `device/src/display.rs`'s logic); fault tests.
3. `Spi` scheduler with the e-paper only; `Wake`.
4. Device: the `Wire` implementation and the spi task; `spi_check` on hardware.
5. Framebuffer handover (os-design: the kernel owns it; `take_dirty_region` is committed only on
   `Drawn(Ok)`).
6. Later: `SdCard` (replacing the ESP-IDF sdspi mount), `Radio`.

## Build notes

Things that came up while building, worth knowing later.

- **Step 1 done** (`core/src/spi/{wire,bus}.rs`, `wire/tests.rs` = `MockWire`). 30 tests, including
  200 seeded random sequences of valid and invalid transactions, power changes and wire failures.
- **CMD0 is part of bus bring-up, not the SD protocol.** B4 is a bus-safety rule: the card must be
  in SPI mode before any other device is used, card or no card. So `Bus::bring_up` sends CMD0 and
  ignores the answer; the SD protocol later runs its full init (and may repeat CMD0).
- ~~LoRa counts as "on the bus" only when powered and out of reset; power sequence on/off.~~
  Superseded by the hardware finding below: LoRa is never switched off while the bus is in use.
- **Bug found by the random test: a failed release byte.** If the release byte after a transaction
  failed, the next transaction selected a device while the SD card might still be driving MISO
  (B2). The bus now records the owed byte and sends it before the next selection.
- B3 in `MockWire` also checks the SD card's 400 kHz limit before SPI mode. After that, the slow
  init clock is the SD protocol's job.
- **Step 2 done** (`core/src/spi/{protocol,epaper}.rs`, `epaper/tests/` with `SimEpaper`). Each
  request is planned up front as a plain list of steps, so every command sequence can be read and
  tested. The partial-draw sequence is pinned against the one GxEPD2 and our previous driver send.
- **Bug found by the random test: the start-of-job BUSY gate.** If a transaction failed *after* a
  busy command reached the panel (e.g. the refresh went out but its release byte failed), the next
  job checked BUSY at once, before it had fallen, and sent commands to a busy panel. Every job now
  settles 1 ms before its BUSY check: it costs 1 ms per request and depends on no history.
- **Limit: BUSY stuck high can't be detected.** It looks like a very fast panel: commands sent
  mid-refresh are ignored and the image may be wrong, but the protocol keeps answering. Detecting
  it would need status reads (FLG over 3-wire SDA, ≤ 2.86 MHz), not built. Low risk, noted.
- **Power policy left to the kernel.** After a refresh the drive voltages stay on (each soft reset
  switches them off anyway, and every partial draw does a soft reset, so a partial costs a
  ~50 ms power-on). `PowerOff` exists for the kernel to call when idle. Worth measuring whether
  skipping the soft reset between draws is safe; it would save ~50 ms per draw. That needs a
  hardware test, but not before the kernel exists.
- **The simulator tolerates commands cut off by a wire failure only when a test opts in**
  (`tolerate_cut_off`), so normal tests still catch wrong parameter counts.
- **Step 3 done** (`core/src/spi/scheduler.rs`: `Spi`, `DeviceProtocol`, `Wake`). Devices are a list
  of protocols behind one trait, so tests can add a `MockProtocol` for a second device. S1 is
  measured: with an SD stand-in always ready, the e-paper and it alternate, and the worst SD wait
  is one e-paper chunk (6–7 ms of bus time).
- **Design flaw found by the S1 test: a stale clock in `step`.** `step` used one `now` for its
  whole run. A device with many transactions could hold the bus for hundreds of ms while another
  device's 1 ms settle never "expired", so it starved. The wire now has a `Clock` (as the modem
  has `Transport + Clock`), and `step` reads it fresh each time round. `MockWire`'s time advances
  with bus traffic, as real time does.
- Requests for a device with no protocol yet (SD, LoRa) get `Response::NoDevice`, so every request
  is still answered.
- A failed bus bring-up is retried every 100 ms. Requests wait meanwhile, so if the bus never comes
  up they are never answered. A `WireError` means the ESP-IDF driver itself failed, which
  shouldn't happen; a supervisor (os-design open question 3) is the place for it.
- **Step 4 done, not yet run on hardware** (`device/src/spi_wire.rs`, `device/src/spi_service.rs`,
  `device/examples/spi_check.rs`; builds cleanly). **For you to run:**
  `./flash-example.sh spi_check | tee captures/spi_check.log`. It checks bring-up and the actual
  clocks, a full draw at 5 MHz with chunked data (look for eight even bands with a border), 20
  timed partial draws (proves BUSY interrupts wake the task), and power-off.
- **FreeRTOS tick = 10 ms, so every 1 ms wait sleeps 10 ms.** `TickType::new_millis` rounds up to
  whole ticks and `CONFIG_FREERTOS_HZ` is the default 100. A partial draw has ~6 settles, so about
  +60 ms per draw. The fix is `CONFIG_FREERTOS_HZ=1000` (already commented out in
  `sdkconfig.defaults`), but it changes the whole firmware's timing and tick-interrupt rate, so
  **decision for you**. `spi_check`'s partial-draw timing shows the real cost first.
- The spi task waits only on its task notification. Pin interrupts are level-triggered on the
  awaited level and armed before the pin is re-read, so no wake-up is lost (`spi_service.rs`).
- `spi_device` per clock with CS = -1. ESP32-S3's SPI2 allows a limited number of devices per bus
  (6, to verify); we use one per distinct clock (today 400 kHz and 5 MHz).
- **Step 5 (core part) done:** `FrameBuffer::begin_flush` / `end_flush(ok)`. `committed` changes
  only on success, a failure forces the next flush to be full, and the first flush after boot is
  full (the e-paper keeps its old image). `Flush` converts into an e-paper `Request`.
  `framebuffer_flushes_reach_the_panel_exactly` runs it end to end through `Spi` and `SimEpaper`,
  including a failed draw.
- **Found while doing step 5: every keystroke refreshed the whole terminal area.** The terminal
  clears and redraws its whole body on every key, and the dirty region was the bounding box of
  everything *drawn*. The new flush diffs against `committed`, so a refresh covers only the bytes
  that changed (often one character cell), and none at all when nothing changed. That's less
  data and less ghosting. The old driver still uses `take_dirty_region` until `main.rs` moves to
  the kernel loop (os-design migration step 4).
- **Hardware finding (2026-10-22): an unpowered LoRa module kills the bus.** `spi_check` printed
  `CHECK PASS` but the screen didn't change: draws "finished" in 57–83 ms, so BUSY never fell and
  the panel had ignored every command. (`spi_check` now fails a draw that is impossibly fast.)
  `device/examples/epd_probe.rs` powered the panel on under each LoRa pin state
  (`captures/epd_probe.log`):

  | LoRa EN / CS / RST | PON holds BUSY low |
  |---|---|
  | low / low / low (old B5), at 5 and at 10 MHz | never — panel deaf |
  | low / high / high (old firmware) | 34 ms — works |
  | high / high / high | 34 ms — works; full refresh 912 ms, image correct |

  The clock was irrelevant. The old firmware only worked because CS/RST high back-powered the
  chip through its ESD diodes. GPIO46 reads low undriven (strap pull-down), so LoRa was never
  properly powered before. Fix: B5 is now "no clocks while LoRa is unpowered"; `Bus::bring_up`
  powers it (EN, CS, RST high), waits 5 ms plus BUSY low, and sends SetSleep `84 00`. If BUSY
  stays high (e.g. already asleep after a bring-up retry) the sleep is skipped with a warning.
  `set_lora_power` / `set_lora_reset` are gone: the LoRa protocol will add tested versions. Open:
  whether LoRa RST low (powered) disturbs the bus, to check when the LoRa protocol is built.
- **Timings measured** (100 µs sampling): PON holds BUSY low 34 ms, POF 38 ms, full refresh
  (E5 5A) 912 ms. The soft reset never pulled BUSY low measurably (≥ 50 µs per the datasheet, so
  shorter than our sampling; harmless, we wait for high).

- **`spi_check` passed on hardware (2026-10-22, `captures/spi_check.log`).** Full draw 1117 ms,
  partials 731–750 ms (the old driver: ~700 ms; the ~40 ms extra is 10 ms ticks on each 1 ms
  settle, see the tick note), bands and the square staircase drawn correctly, so 5 MHz and
  chunked data work. `PowerOff` took 3 ms because there was nothing to do: every partial ends
  with a soft reset (writing the old-frame buffer), which already switches the drive voltages off.
