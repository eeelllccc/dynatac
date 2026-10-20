// L0 transport for the A7682E on the T-Deck-Pro: the modem UART as a
// byte channel that reports lost bytes instead of dropping them silently.
// Pure logic and the stream invariants are in `dynatac_core::modem::transport`.
//
// Hardware connections:
//   UART1 TX → GPIO10 → modem RXD   (board header: BOARD_A7682E_RXD 10)
//   UART1 RX ← GPIO11 ← modem TXD   (board header: BOARD_A7682E_TXD 11)
//   The header names pins from the modem's side; easy to swap by mistake.
//   115200 8N1, no flow control (RTS/CTS are not wired).
//
// How loss is detected: a dedicated thread is the only reader of the UART
// and copies everything into an `rx_pipe` as soon as the driver has it.
// The ESP-IDF driver reports a hardware FIFO overflow (bytes discarded)
// and framing / parity errors (bytes corrupted) on its event queue; the
// thread turns each into a loss marker. If the consumer falls behind, the
// pipe itself fills and marks the loss.
//
// Caller invariants:
//   - `UartTransport` is the only user of UART1 and GPIO10/11.
//   - Keep reading: an unread pipe drops (and reports) new bytes once full.
//
// Callee invariants:
//   - The reader thread never blocks on the consumer.
//   - Bytes already in the driver's buffer when an error event is seen
//     are delivered before the loss marker (they were received first).

use std::thread;
use std::time::Duration;

use esp_idf_svc::hal::{
    delay::TickType,
    gpio::{AnyIOPin, Gpio10, Gpio11},
    uart::{config::Config, UartDriver, UartEventPayload, UartRxDriver, UartTxDriver, UART1},
    units::Hertz,
};
use esp_idf_svc::sys::EspError;

use dynatac_core::modem::transport::{rx_pipe, Rx, RxReader, RxWriter, Transport, WriteError};

/// Driver ring buffer: bytes waiting for the reader thread. The thread
/// empties it every few ms, so this only needs to cover scheduling hiccups.
const DRIVER_RX_BUFFER: usize = 2048;
/// Driver events (data / overflow / errors). An event that doesn't fit is
/// dropped by the ISR, so keep it large relative to the drain interval.
const DRIVER_EVENT_QUEUE: usize = 32;
/// Upper bound on how long the reader sleeps between drains.
const DRAIN_INTERVAL_MS: u64 = 10;

pub struct UartTransport {
    tx: UartTxDriver<'static>,
    rx: RxReader,
}

impl UartTransport {
    /// Open UART1 and start the reader thread. `pipe_capacity` is how
    /// many received bytes may wait for the consumer before loss.
    pub fn start(
        uart: UART1,
        tx_pin: Gpio10,
        rx_pin: Gpio11,
        pipe_capacity: usize,
    ) -> Result<Self, EspError> {
        let config = Config::new()
            .baudrate(Hertz(115_200))
            .rx_fifo_size(DRIVER_RX_BUFFER)
            .queue_size(DRIVER_EVENT_QUEUE);
        let driver = UartDriver::new(
            uart, tx_pin, rx_pin, None::<AnyIOPin>, None::<AnyIOPin>, &config,
        )?;
        let (tx, rx_driver) = driver.into_split();
        let (writer, reader) = rx_pipe(pipe_capacity);
        thread::Builder::new()
            .name("modem-rx".into())
            .stack_size(6 * 1024)
            .spawn(move || reader_loop(rx_driver, writer))
            .expect("spawn modem-rx thread");
        Ok(Self { tx, rx: reader })
    }

    /// The two directions, for callers that send and receive on
    /// different threads.
    pub fn into_parts(self) -> (UartTxDriver<'static>, RxReader) {
        (self.tx, self.rx)
    }
}

impl Transport for UartTransport {
    fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError> {
        write_all(&mut self.tx, bytes)
    }

    fn read(&mut self, timeout: Duration) -> Option<Rx> {
        self.rx.read(timeout)
    }
}

/// Write every byte (the driver may accept a prefix when its TX buffer
/// is full).
pub fn write_all(tx: &mut UartTxDriver<'static>, mut bytes: &[u8]) -> Result<(), WriteError> {
    while !bytes.is_empty() {
        let n = tx.write(bytes).map_err(|e| WriteError(format!("{e:?}")))?;
        bytes = &bytes[n..];
    }
    Ok(())
}

fn reader_loop(rx: UartRxDriver<'static>, pipe: RxWriter) {
    let events = rx.event_queue().expect("UART configured with an event queue");
    let wait = TickType::new_millis(DRAIN_INTERVAL_MS).ticks();
    let mut buf = [0u8; 512];
    loop {
        // Sleep until the driver posts an event, then take every pending
        // event: any of them may announce a loss.
        let mut lost = false;
        let mut next = events.recv_front(wait);
        while let Some((event, _)) = next {
            lost |= is_loss(event.payload());
            next = events.recv_front(0);
        }
        // Deliver everything already buffered: it was received before the
        // loss was reported, so it belongs in front of the marker.
        // `read` reports an empty buffer as ESP_ERR_TIMEOUT, so ask how much
        // is buffered first and treat any read error as a real failure.
        loop {
            let pending = match rx.count() {
                Ok(n) => n,
                Err(e) => {
                    log::error!("modem-rx: uart buffered-length query failed: {e:?}");
                    lost = true;
                    break;
                }
            };
            if pending == 0 {
                break;
            }
            let want = pending.min(buf.len());
            match rx.read(&mut buf[..want], 0) {
                Ok(n) => {
                    pipe.push(&buf[..n]);
                }
                Err(e) => {
                    log::error!("modem-rx: uart read of {want} buffered bytes failed: {e:?}");
                    lost = true;
                    break;
                }
            }
        }
        if lost {
            pipe.mark_lost();
        }
    }
}

fn is_loss(event: UartEventPayload) -> bool {
    match event {
        UartEventPayload::RxFifoOverflow
        | UartEventPayload::FrameError
        | UartEventPayload::ParityError => {
            log::warn!("modem-rx: {event:?}, bytes lost");
            true
        }
        // RxBufferFull is not a loss: the driver holds the bytes in the
        // hardware FIFO until we read. Loss only follows as RxFifoOverflow.
        _ => false,
    }
}
