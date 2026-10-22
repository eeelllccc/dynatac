use super::*;
use crate::spi::wire::tests::{Event, MockWire};

const EPD_CLOCK: Hz = Hz(5_000_000);

fn up() -> (Bus, MockWire) {
    let mut bus = Bus::new();
    let mut w = MockWire::new();
    bus.bring_up(&mut w).unwrap();
    (bus, w)
}

fn t(device: Device, clock: Hz, parts: Vec<Part>) -> Transaction {
    Transaction { device, clock, parts }
}

#[test]
fn bring_up_puts_the_sd_card_in_spi_mode_and_lora_to_sleep() {
    let (bus, w) = up();
    assert!(bus.is_up());
    assert!(w.sd_spi_mode());
    w.assert_idle();
    // MockWire checks B5 (LoRa powered before any clock) itself.
    for pin in [Output::LoraCs, Output::LoraRst, Output::LoraEn, Output::EpdCs, Output::SdCs] {
        assert_eq!(w.output(pin), Some(true), "{pin:?}");
    }
    let frames = w.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].device, Device::SdCard);
    assert_eq!(frames[0].values()[..6], CMD0);
    assert_eq!(frames[1].device, Device::Lora);
    assert_eq!(frames[1].values(), LORA_SLEEP);
}

#[test]
fn lora_gets_its_boot_time_before_busy_is_trusted() {
    let (_, w) = up();
    let ev = w.events();
    let powered = ev.iter().position(|e| *e == Event::Set(Output::LoraEn, true)).unwrap();
    let first_clock = ev.iter().position(|e| matches!(e, Event::Transfer { .. })).unwrap();
    assert!(ev[powered..first_clock].contains(&Event::Pause(LORA_BOOT_US)));
}

#[test]
fn lora_busy_stuck_high_skips_the_sleep_but_the_bus_comes_up() {
    let mut bus = Bus::new();
    let mut w = MockWire::new();
    w.set_input(Input::LoraBusy, true);
    bus.bring_up(&mut w).unwrap();
    assert!(bus.is_up());
    assert!(w.frames().iter().all(|f| f.device != Device::Lora));
    w.assert_idle();
}

#[test]
fn a_failed_lora_sleep_leaves_the_bus_down_and_a_retry_works() {
    let mut bus = Bus::new();
    let mut w = MockWire::new();
    // Idle clocks, CMD0, its response, its release byte; then the sleep.
    w.fail_transfer_after(4);
    assert!(bus.bring_up(&mut w).is_err());
    assert!(!bus.is_up());
    w.assert_no_selection();
    bus.bring_up(&mut w).unwrap();
    assert_eq!(w.frames().last().unwrap().values(), LORA_SLEEP);
}

#[test]
fn run_before_bring_up_is_refused_and_touches_nothing() {
    let mut bus = Bus::new();
    let mut w = MockWire::new();
    let r = bus.run(&mut w, &t(Device::Epaper, EPD_CLOCK, vec![Part::Command(vec![0x12])]));
    assert_eq!(r, Err(BusError::NotBroughtUp));
    assert!(w.events().is_empty());
}

#[test]
fn epaper_commands_go_with_dc_low_and_data_with_dc_high() {
    let (mut bus, mut w) = up();
    let tx = t(Device::Epaper, EPD_CLOCK, vec![Part::Command(vec![0x50]), Part::Write(vec![0xD7, 0x01])]);
    bus.run(&mut w, &tx).unwrap();
    let f = w.frames().last().unwrap();
    assert_eq!(f.device, Device::Epaper);
    let got: Vec<(u8, bool)> = f.bytes.iter().map(|b| (b.value, b.dc)).collect();
    assert_eq!(got, vec![(0x50, false), (0xD7, true), (0x01, true)]);
    assert_eq!(w.output(Output::EpdDc), Some(true), "DC idles high");
    w.assert_idle();
}

#[test]
fn read_and_exchange_return_miso_in_order() {
    let (mut bus, mut w) = up();
    w.reply(Device::SdCard, &[1, 2, 3, 4, 5]);
    let tx = t(
        Device::SdCard,
        Hz(20_000_000),
        vec![Part::Read(2), Part::Write(vec![9]), Part::Exchange(vec![7, 7])],
    );
    // The Write consumes reply 3, which is ignored.
    assert_eq!(bus.run(&mut w, &tx), Ok(vec![1, 2, 4, 5]));
    assert_eq!(w.frames().last().unwrap().values(), vec![0xFF, 0xFF, 9, 7, 7]);
}

#[test]
fn an_absent_device_reads_ff() {
    let (mut bus, mut w) = up();
    let r = bus.run(&mut w, &t(Device::SdCard, Hz(400_000), vec![Part::Read(3)]));
    assert_eq!(r, Ok(vec![0xFF; 3]));
}

#[test]
fn every_transaction_ends_with_a_release_byte_after_deselect() {
    let (mut bus, mut w) = up();
    bus.run(&mut w, &t(Device::Epaper, EPD_CLOCK, vec![Part::Write(vec![1])])).unwrap();
    let ev = w.events();
    let deselect = ev.iter().rposition(|e| *e == Event::Set(Output::EpdCs, true)).unwrap();
    assert_eq!(ev[deselect + 1..].last(), Some(&Event::Transfer { selected: None, len: 1 }));
}

#[test]
fn an_empty_transaction_is_a_chip_select_pulse() {
    let (mut bus, mut w) = up();
    assert_eq!(bus.run(&mut w, &t(Device::Lora, Hz(8_000_000), vec![])), Ok(vec![]));
    let f = w.frames().last().unwrap();
    assert_eq!((f.device, f.bytes.len()), (Device::Lora, 0));
    w.assert_idle();
}

#[test]
fn a_clock_above_the_device_limit_sends_nothing() {
    let (mut bus, mut w) = up();
    let before = w.events().len();
    let r = bus.run(&mut w, &t(Device::Epaper, Hz(10_000_000), vec![Part::Write(vec![1])]));
    assert_eq!(r, Err(BusError::ClockTooFast));
    assert_eq!(w.events().len(), before);
}

#[test]
fn command_parts_are_for_the_epaper_only() {
    let (mut bus, mut w) = up();
    let before = w.events().len();
    let r = bus.run(&mut w, &t(Device::SdCard, Hz(400_000), vec![Part::Command(vec![1])]));
    assert_eq!(r, Err(BusError::InvalidPart));
    assert_eq!(w.events().len(), before);
}

#[test]
fn lora_transactions_run_once_the_bus_is_up() {
    let (mut bus, mut w) = up();
    let tx = t(Device::Lora, Hz(8_000_000), vec![Part::Exchange(vec![0xC0, 0x00])]);
    w.reply(Device::Lora, &[0xA2, 0x22]);
    assert_eq!(bus.run(&mut w, &tx), Ok(vec![0xA2, 0x22]));
    w.assert_idle();
}

#[test]
fn a_wire_failure_mid_transaction_still_deselects_and_releases() {
    let (mut bus, mut w) = up();
    w.fail_transfer_after(1); // the command goes, the data fails
    let tx = t(Device::Epaper, EPD_CLOCK, vec![Part::Command(vec![0x13]), Part::Write(vec![0; 64])]);
    assert!(matches!(bus.run(&mut w, &tx), Err(BusError::Wire(_))));
    w.assert_idle();
    assert_eq!(w.output(Output::EpdCs), Some(true));
    // And the bus keeps working.
    bus.run(&mut w, &tx).unwrap();
}

#[test]
fn a_failed_bring_up_leaves_the_bus_down() {
    let mut bus = Bus::new();
    let mut w = MockWire::new();
    w.fail_transfer_after(0);
    assert!(bus.bring_up(&mut w).is_err());
    assert!(!bus.is_up());
    let r = bus.run(&mut w, &t(Device::SdCard, Hz(400_000), vec![Part::Read(1)]));
    assert_eq!(r, Err(BusError::NotBroughtUp));
    // A retry succeeds.
    bus.bring_up(&mut w).unwrap();
    assert!(w.sd_spi_mode());
}

#[test]
fn a_failed_release_byte_is_sent_before_the_next_selection() {
    let (mut bus, mut w) = up();
    w.fail_transfer_after(1); // the data goes, the release byte fails
    let tx = t(Device::Epaper, EPD_CLOCK, vec![Part::Write(vec![1])]);
    assert!(matches!(bus.run(&mut w, &tx), Err(BusError::Wire(_))));
    assert!(bus.release_owed);
    // MockWire would panic (B2) if the bus selected a device first.
    bus.run(&mut w, &tx).unwrap();
    w.assert_idle();
}

/// Seeded random sequences of transactions (valid and invalid), wire
/// failures and repeated bring-ups: `MockWire` panics on any bus-rule violation,
/// and after every step the bus is idle.
#[test]
fn random_sequences_keep_the_bus_rules() {
    for seed in 1..=200u64 {
        let mut rng = seed;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let (mut bus, mut w) = up();
        for _ in 0..100 {
            match next(10) {
                0 => {
                    let _ = bus.bring_up(&mut w);
                }
                1 | 2 => w.fail_transfer_after(next(3) as usize),
                _ => {
                    let device = Device::ALL[next(3) as usize];
                    let clock = Hz(100_000 * (1 + next(250) as u32));
                    let parts = (0..next(4))
                        .map(|_| {
                            let bytes = vec![next(256) as u8; 1 + next(8) as usize];
                            match next(4) {
                                0 => Part::Command(bytes),
                                1 => Part::Write(bytes),
                                2 => Part::Read(bytes.len()),
                                _ => Part::Exchange(bytes),
                            }
                        })
                        .collect();
                    let _ = bus.run(&mut w, &t(device, clock, parts));
                }
            }
            // Nothing is left selected, and if a release byte is owed the
            // bus knows it.
            w.assert_no_selection();
            assert!(!w.release_pending() || bus.release_owed, "seed {seed}");
        }
    }
}
