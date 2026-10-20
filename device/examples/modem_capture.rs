//! Record raw A7682E UART traffic as a trace, for building the modem
//! simulator from real behaviour (see `core/src/modem/trace.rs`).
//!
//! Every byte the modem sends, every byte we send, RI pin edges, power
//! events and user markers are printed to the serial console as `TRACE`
//! lines with millisecond timestamps. Save the monitor output to a file:
//!
//!   ./flash-example.sh modem_capture | tee captures/<scenario>.log
//!
//! Capture never misses a byte because of the UI: the L0 transport
//! (`src/modem_transport.rs`) reads the UART on its own thread into a
//! 16 KB pipe and reports any loss as `# LOST` in the trace. A second
//! thread logs what it receives and samples RI, while the main thread
//! handles keyboard, screen and sending. Commands are sent one at a
//! time — the next is only sent after a final result code (or a 10 s
//! timeout), as the AT manual requires.
//!
//! The modem is powered on at boot and logged from the first byte, so the
//! boot output is part of the trace.
//!
//! Keys (menu):
//!   i  init: ATE0, CMEE=2, CVHU=0, CLCC=1, CLIP=1, COUTGAIN=7, ATI, CGMR
//!   c  call a number          a  answer (ATA)       h  hang up (AT+CHUP)
//!   l  list calls (AT+CLCC)   t  type any command   m  add a marker note
//!   n  PWRKEY power-on pulse  o  PWRKEY power-off pulse
//!   x  hard power cut (POWER_EN low 3 s) then power on
//!   s  SMS state (read only): CPMS=?, CPMS?, CNMI?, CMGF?, CSCA?, CSMS?
//!   p  PDU mode (AT+CMGF=0)   k  new-SMS indication (AT+CNMI=2,1,0,0,0)
//!   g  list all stored SMS (AT+CMGL=4, PDU mode)
//!   d  network / SMS-domain diagnostics (read only; unsupported ones answer ERROR)
//!   w  send a short GSM text  u  send a 2-part UCS2 text (to a number entered)
//!   ?  show this help
//! In text entry: Enter submits, Alt+Backspace cancels.

#[path = "../src/display.rs"]
mod display;
#[path = "../src/i2c_bus.rs"]
mod i2c_bus;
#[path = "../src/keyboard.rs"]
mod keyboard;
#[path = "../src/modem_transport.rs"]
mod modem_transport;

use std::collections::VecDeque;
use std::sync::mpsc;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use esp_idf_svc::hal::{
    gpio::{
        AnyInputPin, AnyOutputPin, Gpio7, IOPin, Input, InputPin, Output, OutputPin, PinDriver,
        Pull,
    },
    i2c::{I2cConfig, I2cDriver},
    peripherals::Peripherals,
    spi::{config::DriverConfig, Dma, SpiConfig, SpiDeviceDriver, SpiDriver},
    uart::UartTxDriver,
    units::Hertz,
};

use display::Epd;
use dynatac_core::battery::{BQ27220_ADDR, REG_VOLTAGE};
use dynatac_core::keymap::KeyEvent;
use dynatac_core::modem::at::{classify, AtEvent, AtParser, LineClass};
use dynatac_core::modem::sms_pdu;
use dynatac_core::modem::trace::{Direction, TraceRecord};
use dynatac_core::modem::transport::{Rx, RxReader};
use dynatac_core::terminal::{Terminal, TerminalAction};
use i2c_bus::{I2cBus, I2cDevice};
use keyboard::Keyboard;
use modem_transport::{write_all, UartTransport};

const KEYBOARD_ADDR: u8 = 0x34;
const ROW_HEIGHT: u16 = 10;
const COLS: usize = 30;
const ROWS: usize = (display::HEIGHT / ROW_HEIGHT) as usize;
const RX_PIPE: usize = 16 * 1024;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const REDRAW_INTERVAL: Duration = Duration::from_millis(300);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const SLOW_LOOP: Duration = Duration::from_millis(500);

const INIT_COMMANDS: &[&str] = &[
    "ATE0", "AT+CMEE=2", "AT+CVHU=0", "AT+CLCC=1", "AT+CLIP=1", "AT+COUTGAIN=7", "ATI",
    "AT+CGMR",
];

const SMS_STATE: &[&str] =
    &["AT+CPMS=?", "AT+CPMS?", "AT+CNMI?", "AT+CMGF?", "AT+CSCA?", "AT+CSMS?"];
/// Why might texts reach the modem only at attach? Registration per domain,
/// operator and RAT, signal, network mode, UE mode (CS/PS: SMS over SGs),
/// IMS registration (SMS over IMS), SMS service preference.
const NET_DIAG: &[&str] = &[
    "AT+CREG?", "AT+CEREG?", "AT+CGREG?", "AT+COPS?", "AT+CPSI?", "AT+CSQ", "AT+CNMP?",
    "AT+CEMODE?", "AT+CIREG?", "AT+CGSMS?",
];
/// `AT+CMGS` waits for the network: the manual allows up to 120 s.
const SEND_TIMEOUT: Duration = Duration::from_secs(120);
const SHORT_TEXT: &str = "dynatac test: short GSM text";
/// Two UCS2 parts (> 70 UTF-16 units, non-GSM characters).
const LONG_TEXT: &str =
    "dynatac test: ünïcödé ☎ 👋 — long enough to need two parts as UCS2, which allows only 67 units per part with a header.";

const HELP: &[&str] = &[
    "i init  c call  a answer",
    "h hangup  l list  t type",
    "m marker  ? help",
    "n pwr-on  o pwr-off  x hard-cut",
    "s sms-state  p pdu  k cnmi",
    "g list-sms  w send  u send-long",
    "d net-diag  v read-sms-1",
    "r delete-sms-1  e erase-all-sms",
];

type Pwr = PinDriver<'static, AnyOutputPin, Output>;

/// Print one trace record. `println!` holds the stdout lock for the whole
/// line, so records from the two threads never interleave mid-line.
fn emit(start: Instant, dir: Direction, bytes: &[u8]) {
    let t_ms = start.elapsed().as_millis() as u64;
    println!("{}", TraceRecord::new(t_ms, dir, bytes).format());
}

fn note(start: Instant, text: &str) {
    emit(start, Direction::Note, text.as_bytes());
}

/// Logs every received chunk (and loss) as it arrives and forwards it to
/// the UI thread; samples RI between reads (≤ 10 ms apart, fine for RI's
/// ≥ 60 ms pulses).
fn rx_loop(
    rx: RxReader,
    ri: PinDriver<'static, Gpio7, Input>,
    start: Instant,
    to_ui: mpsc::Sender<Rx>,
) {
    let mut ri_low = ri.is_low();
    let mut last_beat = Instant::now();
    note(start, if ri_low { "ri low" } else { "ri high" });
    loop {
        if last_beat.elapsed() >= HEARTBEAT_INTERVAL {
            last_beat = Instant::now();
            note(start, "rx alive");
        }
        match rx.read(Duration::from_millis(10)) {
            Some(Rx::Data(d)) => {
                emit(start, Direction::Rx, &d);
                let _ = to_ui.send(Rx::Data(d));
            }
            Some(Rx::Lost) => {
                note(start, "LOST");
                let _ = to_ui.send(Rx::Lost);
            }
            None => {}
        }
        let low = ri.is_low();
        if low != ri_low {
            ri_low = low;
            note(start, if low { "ri low" } else { "ri high" });
        }
    }
}

/// What the text-entry line is collecting.
#[derive(Clone, Copy)]
enum Entry {
    Number,
    Command,
    Marker,
    /// Destination for a test SMS with this text.
    Sms(&'static str),
}

/// A queued command, with the payload to write at its `> ` prompt.
struct Queued {
    cmd: String,
    payload: Option<Vec<u8>>,
    timeout: Duration,
}

struct InFlight {
    cmd: String,
    payload: Option<Vec<u8>>,
    timeout: Duration,
    sent: Instant,
}

/// Commands waiting to be sent, one in flight at a time.
struct Sender {
    tx: UartTxDriver<'static>,
    queue: VecDeque<Queued>,
    in_flight: Option<InFlight>,
}

impl Sender {
    fn enqueue(&mut self, cmd: &str) {
        self.queue.push_back(Queued { cmd: cmd.to_string(), payload: None, timeout: COMMAND_TIMEOUT });
    }

    /// A command that prompts (`AT+CMGS`): `payload` and Ctrl-Z follow the prompt.
    fn enqueue_with_payload(&mut self, cmd: &str, payload: &str, timeout: Duration) {
        let mut p = payload.as_bytes().to_vec();
        p.push(0x1A);
        self.queue.push_back(Queued { cmd: cmd.to_string(), payload: Some(p), timeout });
    }

    fn write(&mut self, bytes: &[u8], start: Instant) {
        emit(start, Direction::Tx, bytes);
        if let Err(e) = write_all(&mut self.tx, bytes) {
            note(start, &format!("tx error: {:?}", e));
        }
    }

    /// Send the next queued command if none is in flight. Returns the
    /// command sent, for display.
    fn pump(&mut self, start: Instant) -> Option<String> {
        if let Some(f) = &self.in_flight {
            if f.sent.elapsed() < f.timeout {
                return None;
            }
            note(start, &format!("timeout: {}", f.cmd));
            self.in_flight = None;
        }
        let q = self.queue.pop_front()?;
        let mut bytes = q.cmd.clone().into_bytes();
        bytes.push(b'\r');
        self.write(&bytes, start);
        self.in_flight = Some(InFlight { cmd: q.cmd.clone(), payload: q.payload, timeout: q.timeout, sent: Instant::now() });
        Some(q.cmd)
    }

    /// The modem's `> ` prompt: write the in-flight command's payload, or
    /// cancel with ESC if it has none. Returns whether a payload went out.
    fn prompt(&mut self, start: Instant) -> bool {
        match self.in_flight.as_mut().and_then(|f| f.payload.take()) {
            Some(p) => {
                self.write(&p, start);
                true
            }
            None => {
                self.write(&[0x1B], start);
                false
            }
        }
    }

    /// A final result code arrived: the in-flight command is done.
    fn complete(&mut self) {
        self.in_flight = None;
    }

    /// Power events invalidate whatever was in flight or queued.
    fn reset(&mut self) {
        self.in_flight = None;
        self.queue.clear();
    }
}

fn is_final(line: &str) -> bool {
    match classify(line) {
        LineClass::Ok
        | LineClass::Error
        | LineClass::CmeError(_)
        | LineClass::CmsError(_)
        | LineClass::NoCarrier => true,
        _ => matches!(line.trim(), "BUSY" | "NO ANSWER" | "NO DIALTONE"),
    }
}

fn pulse_pwrkey(pwrkey: &mut Pwr, hold: Duration) {
    // The T-Deck-Pro inverts PWRKEY on-board: HIGH presses the button.
    let _ = pwrkey.set_low();
    sleep(Duration::from_millis(10));
    let _ = pwrkey.set_high();
    sleep(hold);
    let _ = pwrkey.set_low();
}

fn power_on(pwrkey: &mut Pwr, start: Instant) {
    note(start, "pwrkey on-pulse start");
    pulse_pwrkey(pwrkey, Duration::from_millis(1000));
    note(start, "pwrkey on-pulse end");
}

fn power_off(pwrkey: &mut Pwr, start: Instant) {
    note(start, "pwrkey off-pulse start");
    pulse_pwrkey(pwrkey, Duration::from_millis(3000));
    note(start, "pwrkey off-pulse end");
}

fn hard_cut(power_en: &mut Pwr, pwrkey: &mut Pwr, start: Instant) {
    note(start, "power_en low");
    let _ = power_en.set_low();
    sleep(Duration::from_secs(3));
    let _ = power_en.set_high();
    note(start, "power_en high");
    sleep(Duration::from_millis(100));
    power_on(pwrkey, start);
}

fn render(epd: &mut Epd, term: &Terminal) {
    for row in 0..ROWS {
        epd.clear_line(row as u16 * ROW_HEIGHT);
    }
    for cell in term.render() {
        let _ = epd.draw_char(cell.col * 8, cell.row as u16 * ROW_HEIGHT, cell.ch);
    }
    let _ = epd.try_flush();
}

fn main() {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let start = Instant::now();

    let peripherals = Peripherals::take().unwrap();
    let pins = peripherals.pins;

    // Deselect the other SPI devices sharing the bus with the display.
    let mut lora_cs = PinDriver::output(pins.gpio3).unwrap();
    lora_cs.set_high().unwrap();
    let mut lora_rst = PinDriver::output(pins.gpio4).unwrap();
    lora_rst.set_high().unwrap();
    let mut sd_cs = PinDriver::output(pins.gpio48).unwrap();
    sd_cs.set_high().unwrap();

    let spi_driver = SpiDriver::new(
        peripherals.spi2, pins.gpio36, pins.gpio33, Some(pins.gpio47),
        &DriverConfig::new().dma(Dma::Auto(4096)),
    ).unwrap();
    let spi_device = SpiDeviceDriver::new(
        spi_driver, None::<AnyOutputPin>,
        &SpiConfig::new().baudrate(Hertz(10_000_000)),
    ).unwrap();
    let cs = PinDriver::output(pins.gpio34.downgrade_output()).unwrap();
    let dc = PinDriver::output(pins.gpio35.downgrade_output()).unwrap();
    let busy = PinDriver::input(pins.gpio37.downgrade()).unwrap();
    let mut epd = Epd::new(spi_device, cs, dc, busy);

    let i2c = I2cDriver::new(
        peripherals.i2c0, pins.gpio13, pins.gpio14,
        &I2cConfig::new().baudrate(Hertz(100_000)),
    ).unwrap();
    let i2c_bus = I2cBus::new(i2c);
    let mut kb = Keyboard::new(i2c_bus.device(KEYBOARD_ADDR)).unwrap();
    // A second device on the same bus, read in the heartbeat: tells a dead
    // I2C bus apart from a reset keyboard chip, and shows supply sag.
    let gauge = i2c_bus.device(BQ27220_ADDR);
    // TCA8418 interrupt output (active low): asserted while key events wait.
    let kb_irq = PinDriver::input(pins.gpio15.downgrade_input()).unwrap();

    let (tx, rx) = UartTransport::start(peripherals.uart1, pins.gpio10, pins.gpio11, RX_PIPE)
        .unwrap()
        .into_parts();

    let mut ri = PinDriver::input(pins.gpio7).unwrap();
    ri.set_pull(Pull::Up).unwrap();

    let mut pwrkey: Pwr = PinDriver::output(pins.gpio40.downgrade_output()).unwrap();
    let mut power_en: Pwr = PinDriver::output(pins.gpio41.downgrade_output()).unwrap();
    let _ = pwrkey.set_low();

    note(start, "capture start");
    let (to_ui, from_rx) = mpsc::channel::<Rx>();
    thread::Builder::new()
        .stack_size(8 * 1024)
        .spawn(move || rx_loop(rx, ri, start, to_ui))
        .unwrap();

    epd.clear().unwrap();
    let mut term = Terminal::new("> ", COLS, ROWS);
    term.set_show_input(false);
    term.push_output("=== Modem capture ===");
    for line in HELP {
        term.push_output(line);
    }
    render(&mut epd, &term);

    note(start, "power_en high");
    let _ = power_en.set_high();
    sleep(Duration::from_millis(100));
    power_on(&mut pwrkey, start);
    term.push_output("# powered on; wait, then i");

    let mut sender = Sender { tx, queue: VecDeque::new(), in_flight: None };
    let mut parser = AtParser::new();
    let mut entry: Option<Entry> = None;
    let mut dirty = true;
    let mut last_render = Instant::now();
    let mut health = Health::new();

    loop {
        let loop_start = Instant::now();
        health.beat(&mut kb, &gauge, &kb_irq, start);

        // Received bytes → display lines; final codes release the sender.
        while let Ok(rx) = from_rx.try_recv() {
            let chunk = match rx {
                Rx::Data(d) => d,
                Rx::Lost => {
                    // The partial line on either side of a gap is corrupt.
                    parser.reset();
                    term.push_output("! bytes lost");
                    dirty = true;
                    continue;
                }
            };
            for event in parser.feed(&chunk) {
                match event {
                    AtEvent::Line(line) => {
                        if is_final(&line) {
                            sender.complete();
                        }
                        term.push_output(&format!("< {}", line));
                    }
                    AtEvent::Prompt => {
                        let sent = sender.prompt(start);
                        term.push_output(if sent { "< > (payload sent)" } else { "< > (ESC: no payload)" });
                    }
                }
                dirty = true;
            }
        }

        if let Some(cmd) = sender.pump(start) {
            term.push_output(&format!("> {}", cmd));
            dirty = true;
        }

        let polled = kb.poll();
        if let Err(e) = &polled {
            health.kb_error(e, start);
        }
        if let Ok(Some(key)) = polled {
            note(start, &format!("key {:?}", key));
            dirty = true;
            match entry {
                Some(kind) => match key {
                    KeyEvent::Cancel => {
                        entry = None;
                        term.set_show_input(false);
                        term.push_output("(cancelled)");
                    }
                    key => {
                        if let TerminalAction::Execute(text) = term.handle_key(key) {
                            entry = None;
                            term.set_show_input(false);
                            submit(kind, text.trim(), &mut sender, &mut term, start);
                        }
                    }
                },
                None => match key {
                    KeyEvent::Char('i') => INIT_COMMANDS.iter().for_each(|c| sender.enqueue(c)),
                    KeyEvent::Char('a') => sender.enqueue("ATA"),
                    KeyEvent::Char('h') => sender.enqueue("AT+CHUP"),
                    KeyEvent::Char('l') => sender.enqueue("AT+CLCC"),
                    KeyEvent::Char('c') => begin(&mut entry, Entry::Number, "number:", &mut term),
                    KeyEvent::Char('t') => begin(&mut entry, Entry::Command, "command:", &mut term),
                    KeyEvent::Char('m') => begin(&mut entry, Entry::Marker, "marker:", &mut term),
                    KeyEvent::Char('s') => SMS_STATE.iter().for_each(|c| sender.enqueue(c)),
                    KeyEvent::Char('p') => sender.enqueue("AT+CMGF=0"),
                    KeyEvent::Char('k') => sender.enqueue("AT+CNMI=2,1,0,0,0"),
                    KeyEvent::Char('g') => sender.enqueue("AT+CMGL=4"),
                    KeyEvent::Char('v') => sender.enqueue("AT+CMGR=1"),
                    KeyEvent::Char('r') => sender.enqueue("AT+CMGD=1"),
                    KeyEvent::Char('e') => sender.enqueue("AT+CMGD=1,4"),
                    KeyEvent::Char('d') => NET_DIAG.iter().for_each(|c| sender.enqueue(c)),
                    KeyEvent::Char('w') => begin(&mut entry, Entry::Sms(SHORT_TEXT), "sms to:", &mut term),
                    KeyEvent::Char('u') => begin(&mut entry, Entry::Sms(LONG_TEXT), "sms to:", &mut term),
                    KeyEvent::Char('n') => {
                        sender.reset();
                        term.push_output("# power-on pulse");
                        render(&mut epd, &term);
                        power_on(&mut pwrkey, start);
                    }
                    KeyEvent::Char('o') => {
                        sender.reset();
                        term.push_output("# power-off pulse (3 s)");
                        render(&mut epd, &term);
                        power_off(&mut pwrkey, start);
                    }
                    KeyEvent::Char('x') => {
                        sender.reset();
                        term.push_output("# hard cut (3 s) + power-on");
                        render(&mut epd, &term);
                        hard_cut(&mut power_en, &mut pwrkey, start);
                    }
                    KeyEvent::Char('?') => HELP.iter().for_each(|l| term.push_output(l)),
                    _ => dirty = false,
                },
            }
        }

        // E-paper refreshes are slow; batch them. Capture is unaffected
        // because the RX thread keeps reading meanwhile.
        if dirty && last_render.elapsed() >= REDRAW_INTERVAL {
            render(&mut epd, &term);
            dirty = false;
            last_render = Instant::now();
        }

        health.loops += 1;
        let took = loop_start.elapsed();
        if took >= SLOW_LOOP {
            note(start, &format!("slow loop: {} ms", took.as_millis()));
        }

        sleep(Duration::from_millis(5));
    }
}

/// Diagnostics for the main thread, logged as notes. Added after the UI
/// went unresponsive when a call connected: the heartbeat distinguishes a
/// blocked main loop (heartbeats stop), a failing I2C bus (keyboard and
/// gauge both error), and a reset keyboard chip (config lost, gauge fine).
struct Health {
    loops: u64,
    kb_errors: u64,
    last_beat: Instant,
    last_error_note: Option<Instant>,
}

impl Health {
    fn new() -> Self {
        Self { loops: 0, kb_errors: 0, last_beat: Instant::now(), last_error_note: None }
    }

    /// Log keyboard errors, at most once a second (with the running count).
    fn kb_error(&mut self, e: &esp_idf_svc::sys::EspError, start: Instant) {
        self.kb_errors += 1;
        if self.last_error_note.map_or(true, |t| t.elapsed() >= Duration::from_secs(1)) {
            self.last_error_note = Some(Instant::now());
            note(start, &format!("kb error #{}: {:?}", self.kb_errors, e));
        }
    }

    /// Every `HEARTBEAT_INTERVAL`: log liveness, keyboard config state and
    /// battery voltage. Re-initialises the keyboard if its config was lost.
    fn beat(
        &mut self,
        kb: &mut Keyboard,
        gauge: &I2cDevice,
        kb_irq: &PinDriver<'static, AnyInputPin, Input>,
        start: Instant,
    ) {
        if self.last_beat.elapsed() < HEARTBEAT_INTERVAL {
            return;
        }
        self.last_beat = Instant::now();
        let cfg = match kb.is_configured() {
            Ok(true) => "ok".to_string(),
            Ok(false) => match kb.reinit() {
                Ok(()) => "LOST, reinit ok".to_string(),
                Err(e) => format!("LOST, reinit err {:?}", e),
            },
            Err(e) => format!("err {:?}", e),
        };
        // INT_STAT, KEY_LCK_EC (event count + lock bits), GPIO_DAT_STAT1..3.
        let regs = match kb.status_regs() {
            Ok(r) => format!("{:02x?}", r),
            Err(e) => format!("err {:?}", e),
        };
        let irq = if kb_irq.is_low() { "low" } else { "high" };
        let mut v = [0u8; 2];
        let vbat = match gauge.write_read(&[REG_VOLTAGE], &mut v) {
            Ok(()) => format!("{}mV", u16::from_le_bytes(v)),
            Err(e) => format!("err {:?}", e),
        };
        note(
            start,
            &format!(
                "alive loops={} kb_errors={} kb_cfg={} kb_regs={} kb_irq={} vbat={}",
                self.loops, self.kb_errors, cfg, regs, irq, vbat
            ),
        );
    }
}

fn begin(entry: &mut Option<Entry>, kind: Entry, hint: &str, term: &mut Terminal) {
    *entry = Some(kind);
    term.push_output(hint);
    term.set_show_input(true);
}

fn submit(kind: Entry, text: &str, sender: &mut Sender, term: &mut Terminal, start: Instant) {
    match kind {
        Entry::Number => {
            if !text.is_empty() && text.chars().all(|c| c.is_ascii_digit() || c == '+') {
                sender.enqueue(&format!("ATD{};", text));
            } else {
                term.push_output("invalid number");
            }
        }
        Entry::Command if !text.is_empty() => sender.enqueue(text),
        Entry::Command => {}
        Entry::Sms(body) => match sms_pdu::encode_submit(text, body, 1) {
            Ok(parts) => {
                // PDU mode first: the PDU is meaningless in text mode.
                sender.enqueue("AT+CMGF=0");
                for p in parts {
                    sender.enqueue_with_payload(&format!("AT+CMGS={}", p.tpdu_len()), p.hex(), SEND_TIMEOUT);
                }
            }
            Err(e) => term.push_output(&format!("cannot encode: {:?}", e)),
        },
        Entry::Marker => {
            note(start, &format!("mark: {}", text));
            term.push_output(&format!("# mark: {}", text));
        }
    }
}
