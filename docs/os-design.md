# dynatac OS design

Status: draft, 2026-10-21. Written before the code; change it when the code teaches us something.

This describes how the OS is put together: what runs concurrently, how the parts talk, who owns
the screen, and how we get from the current proof-of-concept code to v1. It does not cover UI
design or the set of kernel services; both are deliberately left open (see "Open questions").

## Goals

In priority order:

1. **Total reliability through simple, interpretable code.** Every module has a small interface
   and documented invariants, and can be reasoned about and tested on its own. Simple beats clever.
2. **Always listening.** Calls, SMS and (later) WhatsApp messages are noticed whatever the user is
   doing.
3. **Always responsive.** No key press is lost, and the screen is never held up by slow work
   elsewhere (network, modem, storage).
4. **Efficient.** The CPU sleeps whenever there is nothing to do. That saves battery and leaves
   headroom for keyboard and display latency.

Scope of change: everything except the modem stack (`core/src/modem/`) is proof-of-concept code
and may be redesigned whenever it conflicts with this document: display driver, keyboard,
shell, terminal, programs.

## What the hardware gives us

- **Two cores** (ESP32-S3, up to 240 MHz). ESP-IDF runs FreeRTOS across both. The WiFi and lwIP
  tasks sit on core 0 by default.
- **FreeRTOS tasks**, created with `std::thread`. They are preemptive and prioritised, and each
  has its own stack (we use 6–12 KB, from 512 KB of internal RAM; 8 MB PSRAM besides). We can
  afford a handful of them, not hundreds.
- **Chips that keep working while our CPU sleeps** are what make "always listening" cheap:
  - keyboard controller (TCA8418): 10-event FIFO, pulls GPIO15 low while it has events;
  - modem (A7682E): listens to the network itself, stores SMS on the SIM, pulses RI (GPIO7) on
    calls, SMS and other notifications;
  - UART: received bytes land in a driver buffer from interrupts;
  - e-paper: refreshes on its own once loaded, signals completion on BUSY (GPIO37), and holds
    its image with no power.
- **Power management** in ESP-IDF: clock scaling when idle (240 → 80 MHz), and automatic light
  sleep when every task is blocked, woken by a timer or a GPIO. It only works if no task polls.

So "always listening" doesn't mean "always running". It means **sleep, and wake when a line
from one of those chips says something happened.**

## Prior art

We are not the first to solve this. What each project lends us:

| Project | Idea we take |
|---|---|
| **Elm architecture** (Model / Update / View, commands as data) | The kernel and programs are `update(state, event) → effects` plus `view(state)`. Effects are data, carried out elsewhere. |
| **Functional core, imperative shell**; **sans-IO protocol stacks** (h11, quinn-proto, rustls) | All logic is pure in `core/`, fed events and time; `device/` only does I/O. The modem stack already works this way. |
| **Hubris** (Oxide's Rust microkernel) | A fixed set of tasks chosen at build time, each owning its hardware, talking only by messages. A supervisor restarts failed tasks. |
| **Erlang/OTP** | Actors with private state and mailboxes; when something fails, return to a known state instead of limping on. |
| **Pebble OS**, **Flipper Zero** | The system owns the screen and the status bar. Apps get a drawing context for *their* area inside a draw callback, and an event loop delivers input to the foreground app. |
| **InfiniTime** (PineTime watch, FreeRTOS) | Close hardware analogue: a system task and a display task connected by message queues, sleeping between events. |
| **Embassy / RTIC** (embedded Rust) | No polling: every task waits on an interrupt or a message, so the CPU idles. We get the same from FreeRTOS blocking waits, without adding an async runtime. |
| **E-reader software** | Combine screen updates; do a full refresh every so often to clear ghosting. |

## The model

```
 GPIO15 IRQ ──► [keyboard task] ─── Key ────┐
 UART / RI  ──► [modem tasks] ───── Modem ──┤
 [net worker] ───────────────────── Net ────┼──► kernel inbox ──► [kernel task]
 [spi task] ─────────────────────── Flushed ┘                       │ step(state, event, now)
                                                                    ├─► Region  ──► spi task
                                                                    ├─► Request ──► modem tasks
                                                                    └─► Job     ──► net worker
```

### Rules

These are the invariants the whole design rests on.

1. **One owner per piece of hardware.** Each peripheral belongs to exactly one task, and nothing
   else touches it. A shared bus counts as one piece of hardware: its owner owns every device on
   it. Each driver then has one job, which is handling its hardware perfectly, and can be tested
   against a mock of that hardware (as the modem is tested against its simulator).
2. **Messages only.** Tasks share no mutable state. They talk through channels.
3. **Each task waits in exactly one place:** its inbox (or an interrupt notification), with a
   timeout equal to its next deadline. It never wakes up just to check for work. When every task
   is waiting, the chip can sleep.
4. **The kernel never blocks on I/O.** Its only wait is its inbox. Slow work is a message to a task.
5. **Logic is pure and lives in `core/`.** Device code is a thin loop: receive, call into
   `core`, carry out the result. Time is passed in as `now`; `core` never reads a clock.
6. **Every queue is bounded, and overflow is reported, never silent** (as L0 does for modem bytes).
7. **Failure returns to a known state.** A panic or a watchdog timeout reboots the device. State
   that must survive a reboot (saved SMS, settings) is written to flash before it is relied on.

### Tasks

The task list follows from the hardware. A new task is justified only by new hardware or by
work that blocks.

| Task | Owns | Wakes on | Sends the kernel |
|---|---|---|---|
| **kernel** (main thread) | All OS state, the framebuffer | Its inbox, or its next deadline | — |
| **keyboard** | I2C bus: keyboard, fuel gauge, charger (later touch, IMU) | GPIO15 interrupt, or a request | `Key`, battery/charger readings |
| **spi** | SPI2 bus: e-paper, SD card, LoRa (SX1262), and their CS / DC / BUSY / reset / IRQ pins | A request, the e-paper's BUSY going high, a LoRa IRQ | `Flushed`, storage results, LoRa packets |
| **modem** (`modem-rx` + `modem-driver`) | UART1, PWRKEY, POWER_EN (later RI, DTR) | UART bytes, a request, a power step | `Modem(Notice)` |
| **net worker** | WiFi control, sockets (HTTP, SMTP, TLS) | A job | Job results, link state |

Notes:
- The keyboard task owns the whole I2C bus because the bus is shared. Battery and charger reads
  take about a millisecond, which doesn't hurt key latency. One owner keeps the existing
  single-threaded `I2cBus` correct with no locks.
- The spi task owns the whole SPI bus for the same reason. It schedules transactions across its
  devices. An e-paper refresh holds the bus only while data is sent; while the panel works (BUSY
  low) the task serves SD and LoRa transactions. Its logic is a pure state machine in `core/`
  tested against a mock bus, with a thin wrapper in `device/` (the modem's layering). See
  "SPI driver" below.
- The net worker runs one job at a time. That caps memory (each TLS session needs tens of KB) and
  keeps it simple.
- Priorities: keyboard > modem-rx > kernel > spi > net worker. Cores: everything works on
  either core. Pinning (UI on core 1, away from the WiFi stack on core 0) is a later tuning step,
  never needed for correctness.

### The kernel

The kernel task is a loop of a few lines:

```rust
loop {
    let event = inbox.recv_timeout(kernel.next_deadline(now) - now); // the only wait
    kernel.step(event, now);                                         // pure, in core/
    // effects go out as channel sends, which never block
}
```

`step` hands the event to whichever parts of the kernel care about it, and they produce effects:
modem requests, a region to draw, a job for the net worker. Effects reach tasks through port
traits whose device implementations only send a message. Tests use mocks and simulators
instead, which is exactly how `CallManager` is written against `ModemPort` today. The whole
kernel therefore runs on the host, against the digital twin.

Events (sketch): `Key(KeyEvent)`, `Modem(Notice)`, `Flushed(Result)`, `Net(..)`, `Battery(..)`,
`Deadline`.

### Screen ownership

The kernel owns the screen. Programs never write to the display.

- **Zones.** `screen::LAYOUT` (the status bar plus the body) is decided by the kernel.
- **Surfaces.** A `Surface` is permission to draw into one zone, and everything drawn is clipped
  to it. During its render phase the kernel lends the body surface to the foreground program
  (`view(&self, surface: &mut Surface)`) and the status bar surface to the status bar code.
  The surface is borrowed, so a program can't keep it and draw later.
- **The framebuffer moves into the kernel.** Today `Epd` owns `fb`. In this design the kernel owns
  the `FrameBuffer`, and drawing is pure memory work in `core`. The spi task only pushes
  bytes to the panel.
- **One region in flight.** When the e-paper is idle and something changed, the kernel takes
  the dirty region and sends it. Drawing during a refresh builds up in the framebuffer. When
  `Flushed` arrives, the next region covers everything that changed meanwhile, so updates are
  combined with no frame queue.
- **A failed flush gives the region back.** `take_dirty_region` currently marks the region
  committed as soon as it is taken. That has to become "committed when `Flushed(Ok)` arrives",
  so the framebuffer's idea of the panel can't drift from the truth.
- Refresh policy (combining updates, how often to do a full refresh, the status bar clock rate)
  lives in the kernel, in one place.

### Programs

A program runs in the kernel as a state machine, Elm-style: it receives events (keys, results of
its requests, notices it subscribed to) and returns effects, and it draws its zone when asked.
It never blocks and never touches hardware. The shell and terminal become one such program.

The exact interface is still open (question 2). The point fixed here: **programs get
capabilities from the kernel (a surface, a way to make requests), never hardware.**

### Kernel services

Some state and logic must live in the kernel rather than in a program. Instead of fixing a list
up front, something becomes a kernel service when at least one of these holds:

1. **It must keep running when no program is in front**, e.g. listening for calls and SMS.
2. **It decides who gets a shared resource**, e.g. screen zones and focus, the network route,
   the modem.
3. **Several programs need the same state**, and separate copies would drift apart.

If none holds, the code belongs in a program or a library. Known so far: screen zones and focus
(rule 2); `CallManager`, `SmsStore`, `Inbox` (rule 1). The rest gets decided as programs move onto
the kernel.

## SPI driver

Decided: one task owns the SPI2 bus and every device on it (rule 1). The current `Epd` driver and
the ESP-IDF SD-over-SPI mount are proof-of-concept code and get replaced. The work starts with
research, as the modem work did: understand the hardware from its manuals before writing the
driver.

**Research: done; findings in `docs/spi-research.md`. Interfaces and invariants: `docs/spi-design.md`.** It settles several things:
- build on ESP-IDF `spi_master`, one `spi_device` per device, all used only by the spi task,
  with CS driven by our code;
- the SD card goes into SPI mode before any other device is used;
- every transaction ends with CS high plus one extra byte (the SD card's MISO release);
- the e-paper write clock is ≤ 5.88 MHz (today's 10 MHz is out of spec);
- the panel's deep sleep is unusable on this board (no reset line);
- the LoRa module must be powered whenever the bus is used (unpowered, it blocks SCK/MOSI;
  measured), so bring-up powers it and puts it to sleep.

The original research list, for reference:
- **ESP32-S3 Technical Reference Manual (GP-SPI2) and the ESP-IDF `spi_master` driver.** Clock
  and mode per device, CS handling, DMA (buffer placement and alignment, maximum transfer size),
  interrupt-driven versus polled transactions, and what happens when a transaction fails.
  Decide whether to build on `spi_master` (called only from the spi task) or drive the registers
  directly.
- **Board wiring (T-Deck-Pro repo, searched with `rg`).** SCK GPIO36, MOSI GPIO33, MISO GPIO47;
  e-paper CS 34 / DC 35 / BUSY 37; SD CS 48; LoRa CS 3 / RST 4, plus LoRa BUSY and DIO1 (to
  find). Which pins can interrupt, and any power-enable lines.
- **E-paper: GDEQ031T10 / UC8253 datasheet.** The command set, the BUSY semantics, the partial
  and full refresh sequences, maximum SPI clock, the deep-sleep and wake sequence.
- **SD card: SD Physical Layer Simplified Specification, SPI mode.** Init (≥74 clocks with CS
  high at ≤400 kHz, CMD0 / CMD8 / ACMD41), CRC, block read and write tokens, busy signalling and
  timeouts. Known bus-sharing hazard: a card may hold MISO after its CS goes high until it sees
  extra clocks.
- **LoRa: SX1262 datasheet.** BUSY must be low before every command, DIO1 interrupts, sleep and
  wake, SPI mode and maximum clock.
- **Prior art to evaluate:** `embedded-sdmmc` (pure-Rust SD over SPI plus FAT, on a block-device
  trait), the `sx126x` / `lora-phy` crates, existing UC8253 drivers (GxEPD2).

**Shape** (the modem's layering, all logic in `core/`):
- **Bus interface** (the L0 equivalent): a trait for one transaction on one device (CS, bytes out,
  bytes in) plus reading a device's status pins. The device implementation is thin. Tests use a
  mock bus that hosts device simulators (`MockEpaper`, `MockSdCard`, `MockSx1262`), plus fault
  injection: timeouts, BUSY stuck low, garbage replies, a card holding MISO.
- **Device protocols:** one pure state machine per device. Waiting (BUSY, card busy, LoRa BUSY)
  is a state, never a sleep loop, so no device can stall the others.
- **Bus scheduler:** runs one transaction at a time, round-robin across the devices with work
  ready, so each waits at most one transaction (≈ 7 ms) per other device. The spi task is a loop
  around it.
- **Filesystem:** the SD layer exposes blocks. Which FAT implementation runs on top is part of the
  research; it must not touch the bus itself.

## Power

Power follows from rule 3. When every task is waiting, the FreeRTOS idle task runs, and with
ESP-IDF power management enabled (`CONFIG_PM_ENABLE`, tickless idle) the chip lowers its clock
or light-sleeps until the next interrupt or deadline. Steps:

1. Remove polling. Today the main loop polls the keyboard every 10 ms, and the modem driver
   steps every 20 ms.
2. Enable clock scaling and automatic light sleep, then measure current draw.
3. Modem-on sleep. The UART can't reliably receive while the chip sleeps, so the modem has to
   wake us with RI, and the modem's own sleep is controlled through DTR (`AT+CSCLK`). Needs
   hardware experiments. Until then, hold a power-management lock while the modem is on.

## Open questions

1. **Connectivity.** Programs should ask for network access without knowing about links:
   WiFi when available, 4G as fallback. Probably a kernel service (rule 2) deciding the route and
   bringing links up and down, with the net worker running socket jobs over whichever link lwIP
   routes through. **Decided: CMUX comes before any kernel connectivity code.** Without CMUX,
   4G data makes the modem deaf to calls and SMS. We will not design or build workarounds that
   hand the modem off between data and calls; the kernel is written assuming calls, SMS and data
   run at the same time.
2. **Program interface.** The event, effect and subscription types; how a program asks for things
   (modem, network, storage); how focus moves (e.g. an incoming call brings the phone to the
   front).
3. **Supervision.** Is "any failure → reboot" enough for v1, or should some tasks (net worker)
   be restarted on their own?
4. **Measure:** partial and full refresh times, idle current today, stack high-water marks.

## Migration

Each step leaves a working device. Until its step comes, existing code keeps running unchanged
inside the new loop, even where it breaks a rule (blocking programs do, for now).

1. **Kernel skeleton in `core/`:** event types, port traits, `step`, deadlines, a host-side
   harness.
2. **SPI driver and spi task** (see "SPI driver"). Research first; then the bus interface, the
   device simulators and the scheduler in `core/`, with the e-paper protocol first. The bus design
   covers all three devices from the start, but SD and LoRa protocols can follow later. Framebuffer
   into the kernel; `Region` / `Flushed` messages; a failed flush gives the region back.
3. **Keyboard task.** Interrupt-driven, no polling; battery and charger reads move there.
4. **`main` becomes the kernel loop.** The shell and programs run inside it unchanged.
5. **Modem into the kernel.** `ModemHandle` notices go to the kernel inbox; the driver waits on one
   queue instead of stepping every 20 ms; `CallManager`, `SmsStore` and `Inbox` live in the kernel;
   `phone` and `sms` move onto them (todo step 7); the old driver is deleted. Cellular data is
   unavailable from here until step 6 (WiFi still works); no stopgap is built.
6. **CMUX, then net worker and connectivity.** CMUX (todo step 8) first, so PPP and AT share the
   modem; only then the connectivity code. Blocking programs (`curl`, `email`, `whatsapp`, `wifi`) become
   jobs.
7. **Power.** Enable power management and measure.

## Build notes

Progress through the migration, and things worth knowing.

- **Step 2 (SPI) done in core and on the device side.** Details and findings are in
  `docs/spi-design.md`, "Build notes". Hardware check `spi_check` is waiting for a run.
- **Step 1 (kernel skeleton) done:** `core/src/kernel.rs`. It holds the lock state, terminal and
  shell, and a display service that owns the framebuffer: one draw in flight, committed only on
  success, panel powered off after 1 s idle and at once when locking or shutting down. Ports are
  traits (`SpiPort`), so `kernel/tests.rs` runs the whole kernel against the real `Spi` scheduler
  with `SimEpaper`: boot screen, typing, lock/unlock, power off, a failed draw.
- **Step 3 (input task) done:** `device/src/input_service.rs` owns the I2C bus (keyboard, fuel
  gauge, charger). Keys arrive via the IRQ line (level interrupt, armed then re-read), so the
  10 ms keyboard polling is gone. The battery and charger are requests to the task, wrapped as
  `BatteryProxy` / `ChargerProxy` so the programs didn't change. The BQ25896 boot setup
  (watchdog, long-press reset) moved here.
- **Step 4 (`main` is the kernel loop) done, builds, not yet run on hardware.** `main` waits only
  on the kernel inbox, until the kernel's deadline. Lock is kernel state; `main` light-sleeps while
  locked and the display is idle.
  - **Removed:** the old `Epd` driver from `main` (examples still use `src/display.rs`), the
    ESP-IDF SD mount (the spi task owns SPI2; SD returns with its protocol), the boot I2C scan, the
    step-by-step power-off messages (now one line), and clearing keyboard modifiers on unlock.
  - **Kept, temporary:** the old blocking in-call loop (`handle_call`), now reading keys from the
    inbox and drawing through the kernel, until step 5.
- **Hardware checks for the new firmware** (`./flash.sh`). One session, roughly in risk order:
  1. Boot: status bar and prompt appear after one full refresh.
  2. Typing: each key appears; refreshes should be smaller (only changed characters) and feel
     quicker than before.
  3. **Lock and unlock, the riskiest part.** Alt+L shows the lockscreen; other keys do nothing;
     Alt+L unlocks with a clean full refresh. Unknown: whether, after a light sleep woken through
     ext0 on GPIO15, the input task's level interrupt still fires. If keys stop working after a
     lock, that's the place to look (`sleep.rs` may need `gpio_wakeup_enable` instead of ext0).
  4. `battery`: readings come back through the input task.
  5. `phone <number>`: u/d volume, Enter hangs up, remote hang-up ends it.
  6. `wifi` / `curl` unchanged.
  7. `power off`: goodbye line, deep sleep, the button wakes it.
