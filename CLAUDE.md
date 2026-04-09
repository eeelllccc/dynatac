This is the beginning of a very big project to write a text-based operating system for a LilyGo T-Deck-Pro device. The hardware includes:

MCU: ESP32-S3
Flash / PSRAM: 16M / 8M
LoRa: SX1262
GPS: MIA-M10Q
Display: GDEQ031T10 (320x240)
4G-Module: A7682E
Battery Capacity: 305070 (1400mAh)
Battery Chip: BQ25896 (0x6B), BQ27220 (0x55)
Touch: CST328 (0x1A)
Gyroscope: BHI260AP (0x28)
Keyboard: TCA8418 (0x34)

I'm building ontop of ESP-IDF, so I can use the Rust std lib.

Simple, interpretable, encapsulated, functional code is a priority (like when using OCaml). I want small modules of code that each expose an interface. It's clearly documented what invarients the caller and callee must uphold, and in this way the encapsulation should allow small modules to be independently designed and tested.

The T-Deck-Pro repo is included as a directory, there are lots of useful things including examples to get the different bits of hardware to work. But don't explore the T-Deck-Pro directory speculatively, use `rg` to search it with a specific search command and ask permission before reading files in that directory.

## Digital twin / TDD

The project is a Cargo workspace: `core/` (pure logic, no ESP-IDF deps) and `device/` (thin hardware wrappers). All OS-level logic lives in `core/` and is tested on the host with `cargo test -p dynatac-core`.

When adding support for a new hardware module (GPS, LoRa, battery, etc.):
1. Extract the pure logic into `core/` (e.g. NMEA parsing, packet framing, state machines).
2. Create a thin hardware wrapper in `device/` that delegates to the core logic.
3. Write tests in `core/` against the pure logic -- this builds up a digital twin of the device that can be exercised without hardware.
