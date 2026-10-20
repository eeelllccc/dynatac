//! L0 transport: the raw byte stream to and from the modem.
//!
//! The layer above (the AT link engine) sees the modem as a byte channel
//! that never silently drops data: every gap in the received stream is
//! reported in place as [`Rx::Lost`]. Anything that carries bytes to and
//! from the modem implements [`Transport`]: the UART on the device, the
//! modem simulator in tests, and later a CMUX virtual channel.
//!
//! This module also holds the pure pieces of the device transport, so they
//! are tested on the host:
//!
//! - [`RxQueue`]: a bounded byte queue that records loss markers instead of
//!   growing or dropping data silently.
//! - [`rx_pipe`]: an [`RxQueue`] shared between a producer thread (the UART
//!   reader) and the consumer, with a blocking read.
//!
//! Stream invariants, for every reader of an [`Rx`] sequence:
//!   - Bytes arrive in the order the modem sent them.
//!   - Between two `Lost` markers (or the start), the bytes are contiguous:
//!     nothing is missing, duplicated or reordered.
//!   - Every gap is reported by exactly one `Lost`, at the position of the
//!     gap (consecutive gaps collapse into one marker). How many bytes were
//!     lost is unknown.
//!   - The device can only place a marker to within the UART driver's
//!     buffered window, so a reader must treat the partial line on each
//!     side of a `Lost` as corrupt.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// One item of the received stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rx {
    /// Bytes from the modem, contiguous with the previous `Data` unless a
    /// `Lost` came in between. Never empty.
    Data(Vec<u8>),
    /// Some bytes between the previous item and the next were lost.
    Lost,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteError(pub String);

/// A byte channel to the modem.
pub trait Transport {
    /// Send all of `bytes`. On error, an unknown prefix may have been sent.
    fn write(&mut self, bytes: &[u8]) -> Result<(), WriteError>;

    /// The next received item, waiting up to `timeout`. `None` if nothing
    /// arrived in time. Returns immediately when something is pending.
    fn read(&mut self, timeout: Duration) -> Option<Rx>;
}

/// A monotonic clock in milliseconds, for driving the pure layers above
/// (the device uses real time, tests the simulator's clock).
pub trait Clock {
    fn now(&self) -> u64;
}

/// The L0 queue capacity the stack is designed for. A reply only gets
/// through if it fits whole (the modem sends it in one burst, faster than
/// it may be consumed), and the largest is a listing of a full SIM:
/// [`super::sms_storage::MAX_LIST_REPLY_BYTES`], ~3.7 KB. Twice that leaves
/// room for URCs and a slow consumer.
pub const RX_QUEUE_BYTES: usize = 8 * 1024;

/// Bounded FIFO of received bytes with loss markers.
///
/// Holds at most `capacity` bytes. Bytes pushed when it is full are dropped
/// and a loss is recorded at that point, so the consumer learns about the
/// gap exactly where it happened.
pub struct RxQueue {
    capacity: usize,
    buf: VecDeque<u8>,
    /// Stream offset of `buf[0]`: bytes ever accepted minus `buf.len()`.
    head: u64,
    /// Stream offsets at which a gap occurred, ascending, no duplicates.
    /// A marker at offset `o` sits just before the byte with offset `o`.
    losses: VecDeque<u64>,
    /// Offset of the most recent gap, even if already popped, so a second
    /// gap at the same position is never reported twice.
    last_loss: Option<u64>,
}

impl RxQueue {
    /// Requires `capacity > 0`.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "RxQueue capacity must be positive");
        Self { capacity, buf: VecDeque::with_capacity(capacity), head: 0, losses: VecDeque::new(), last_loss: None }
    }

    fn tail(&self) -> u64 {
        self.head + self.buf.len() as u64
    }

    /// Append bytes. Returns how many were accepted; if fewer than
    /// `bytes.len()`, the rest were dropped and a loss was recorded after
    /// the accepted ones.
    pub fn push(&mut self, bytes: &[u8]) -> usize {
        let accepted = bytes.len().min(self.capacity - self.buf.len());
        self.buf.extend(&bytes[..accepted]);
        if accepted < bytes.len() {
            self.mark_lost();
        }
        accepted
    }

    /// Record a gap at the current end of the stream (e.g. the UART
    /// hardware FIFO overflowed). Collapses with a gap already recorded
    /// at the same position.
    pub fn mark_lost(&mut self) {
        let at = self.tail();
        if self.last_loss != Some(at) {
            self.losses.push_back(at);
            self.last_loss = Some(at);
        }
    }

    /// Take the next item: all bytes up to the next loss marker, or the
    /// marker itself. `None` when empty.
    pub fn pop(&mut self) -> Option<Rx> {
        if self.losses.front() == Some(&self.head) {
            self.losses.pop_front();
            return Some(Rx::Lost);
        }
        let end = self.losses.front().copied().unwrap_or_else(|| self.tail());
        let n = (end - self.head) as usize;
        if n == 0 {
            return None;
        }
        self.head += n as u64;
        Some(Rx::Data(self.buf.drain(..n).collect()))
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty() && self.losses.is_empty()
    }
}

struct Shared {
    queue: Mutex<RxQueue>,
    ready: Condvar,
}

/// Producer half of an [`rx_pipe`]. Owned by the thread reading the UART.
pub struct RxWriter(Arc<Shared>);

/// Consumer half of an [`rx_pipe`].
pub struct RxReader(Arc<Shared>);

/// A bounded [`RxQueue`] shared between one producer and one consumer.
/// The producer never blocks on the consumer: when the queue is full,
/// bytes are dropped and the loss is reported in the stream.
pub fn rx_pipe(capacity: usize) -> (RxWriter, RxReader) {
    let shared = Arc::new(Shared { queue: Mutex::new(RxQueue::new(capacity)), ready: Condvar::new() });
    (RxWriter(shared.clone()), RxReader(shared))
}

impl RxWriter {
    /// See [`RxQueue::push`].
    pub fn push(&self, bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }
        let accepted = lock(&self.0.queue).push(bytes);
        self.0.ready.notify_one();
        accepted
    }

    /// See [`RxQueue::mark_lost`].
    pub fn mark_lost(&self) {
        lock(&self.0.queue).mark_lost();
        self.0.ready.notify_one();
    }
}

impl RxReader {
    /// The next item, waiting up to `timeout`. `None` on timeout.
    pub fn read(&self, timeout: Duration) -> Option<Rx> {
        let deadline = Instant::now() + timeout;
        let mut queue = lock(&self.0.queue);
        loop {
            if let Some(rx) = queue.pop() {
                return Some(rx);
            }
            let left = deadline.checked_duration_since(Instant::now())?;
            if left.is_zero() {
                return None;
            }
            queue = self.0.ready.wait_timeout(queue, left).unwrap_or_else(|p| p.into_inner()).0;
        }
    }
}

/// A panic on the other thread must not take the stream down with it:
/// the queue's own state is always consistent between calls.
fn lock(m: &Mutex<RxQueue>) -> std::sync::MutexGuard<'_, RxQueue> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn data(b: &[u8]) -> Rx {
        Rx::Data(b.to_vec())
    }

    fn drain(q: &mut RxQueue) -> Vec<Rx> {
        std::iter::from_fn(|| q.pop()).collect()
    }

    #[test]
    fn empty_queue_pops_nothing() {
        let mut q = RxQueue::new(8);
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
    }

    #[test]
    fn pushes_are_merged_into_one_read() {
        let mut q = RxQueue::new(64);
        assert_eq!(q.push(b"\r\nO"), 3);
        assert_eq!(q.push(b"K\r\n"), 3);
        assert_eq!(drain(&mut q), [data(b"\r\nOK\r\n")]);
        assert!(q.is_empty());
    }

    #[test]
    fn overflow_drops_the_excess_and_marks_the_gap() {
        let mut q = RxQueue::new(4);
        assert_eq!(q.push(b"abcdef"), 4);
        assert_eq!(q.pop(), Some(data(b"abcd")));
        assert_eq!(q.push(b"gh"), 2);
        assert_eq!(drain(&mut q), [Rx::Lost, data(b"gh")]);
    }

    #[test]
    fn repeated_overflow_while_full_is_one_gap() {
        let mut q = RxQueue::new(2);
        q.push(b"ab");
        assert_eq!(q.push(b"c"), 0);
        assert_eq!(q.push(b"d"), 0);
        q.mark_lost();
        assert_eq!(drain(&mut q), [data(b"ab"), Rx::Lost]);
    }

    #[test]
    fn mark_lost_splits_the_stream_in_place() {
        let mut q = RxQueue::new(64);
        q.push(b"OK\r\n");
        q.mark_lost();
        q.push(b"RING\r\n");
        assert_eq!(drain(&mut q), [data(b"OK\r\n"), Rx::Lost, data(b"RING\r\n")]);
    }

    #[test]
    fn loss_before_any_data_is_reported_first() {
        let mut q = RxQueue::new(8);
        q.mark_lost();
        q.mark_lost();
        q.push(b"x");
        assert_eq!(drain(&mut q), [Rx::Lost, data(b"x")]);
    }

    #[test]
    fn loss_is_reported_once() {
        let mut q = RxQueue::new(8);
        q.mark_lost();
        assert_eq!(q.pop(), Some(Rx::Lost));
        assert_eq!(q.pop(), None);
        assert!(q.is_empty());
    }

    /// Found by the random schedule test: a gap, read, then another gap
    /// before any new byte must not produce `Lost, Lost`.
    #[test]
    fn loss_after_a_consumed_loss_with_no_data_between_is_one_gap() {
        let mut q = RxQueue::new(8);
        q.push(b"a");
        q.mark_lost();
        assert_eq!(drain(&mut q), [data(b"a"), Rx::Lost]);
        q.mark_lost();
        assert_eq!(q.push(b"bcdefghijk"), 8);
        assert_eq!(drain(&mut q), [data(b"bcdefghi"), Rx::Lost]);
    }

    #[test]
    fn freed_space_is_reused() {
        let mut q = RxQueue::new(3);
        for chunk in [b"abc", b"def", b"ghi"] {
            assert_eq!(q.push(chunk), 3);
            assert_eq!(q.pop(), Some(data(chunk)));
        }
        assert!(q.is_empty());
    }

    /// Seeded random pushes, losses and pops against a reference model of
    /// the stream invariants: output = accepted bytes in order, with
    /// exactly one `Lost` wherever bytes were dropped or a loss marked.
    #[test]
    fn random_schedules_keep_the_stream_invariants() {
        for seed in 1..200u64 {
            let mut rng = seed;
            let mut next = |n: u64| {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                rng % n
            };
            let capacity = 1 + next(16) as usize;
            let mut q = RxQueue::new(capacity);
            let mut expected: Vec<Rx> = Vec::new();
            let mut got: Vec<Rx> = Vec::new();
            let mut counter = 0u8;
            for _ in 0..300 {
                match next(10) {
                    0..=4 => {
                        let chunk: Vec<u8> = (0..next(8))
                            .map(|_| {
                                counter = counter.wrapping_add(1);
                                counter
                            })
                            .collect();
                        let n = q.push(&chunk);
                        push_model(&mut expected, Some(&chunk[..n]));
                        if n < chunk.len() {
                            push_model(&mut expected, None);
                        }
                        assert!(q.buf.len() <= capacity);
                    }
                    5 => {
                        q.mark_lost();
                        push_model(&mut expected, None);
                    }
                    _ => {
                        if let Some(rx) = q.pop() {
                            got.push(rx);
                        }
                    }
                }
            }
            got.extend(drain(&mut q));
            assert_eq!(normalise(got), expected, "seed {seed}");
        }
    }

    /// Append to a model stream: `Some(bytes)` merges into a trailing
    /// `Data`, `None` adds a `Lost` unless one is already last.
    fn push_model(stream: &mut Vec<Rx>, item: Option<&[u8]>) {
        match (item, stream.last_mut()) {
            (Some([]), _) => {}
            (Some(b), Some(Rx::Data(d))) => d.extend_from_slice(b),
            (Some(b), _) => stream.push(Rx::Data(b.to_vec())),
            (None, Some(Rx::Lost)) => {}
            (None, _) => stream.push(Rx::Lost),
        }
    }

    /// Merge adjacent `Data` items (reads may split a run arbitrarily).
    /// Two `Lost` in a row would be a bug, so they are kept as-is.
    fn normalise(items: Vec<Rx>) -> Vec<Rx> {
        let mut out: Vec<Rx> = Vec::new();
        for rx in items {
            match (rx, out.last_mut()) {
                (Rx::Data(b), Some(Rx::Data(d))) => d.extend(b),
                (rx, _) => out.push(rx),
            }
        }
        out
    }

    #[test]
    fn pipe_read_times_out_when_empty() {
        let (_w, r) = rx_pipe(8);
        let start = Instant::now();
        assert_eq!(r.read(Duration::from_millis(20)), None);
        assert!(start.elapsed() >= Duration::from_millis(20));
    }

    #[test]
    fn pipe_read_returns_pending_data_immediately() {
        let (w, r) = rx_pipe(8);
        w.push(b"OK");
        assert_eq!(r.read(Duration::ZERO), Some(data(b"OK")));
    }

    #[test]
    fn pipe_read_wakes_when_producer_pushes() {
        let (w, r) = rx_pipe(64);
        let producer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            w.push(b"RING\r\n");
        });
        assert_eq!(r.read(Duration::from_secs(5)), Some(data(b"RING\r\n")));
        producer.join().unwrap();
    }

    #[test]
    fn pipe_reports_loss_from_producer() {
        let (w, r) = rx_pipe(64);
        w.push(b"a");
        w.mark_lost();
        w.push(b"b");
        let items: Vec<_> = std::iter::from_fn(|| r.read(Duration::ZERO)).collect();
        assert_eq!(items, [data(b"a"), Rx::Lost, data(b"b")]);
    }

    #[test]
    fn pipe_never_blocks_the_producer_on_a_slow_consumer() {
        let (w, r) = rx_pipe(4);
        let producer = thread::spawn(move || (0..100).map(|_| w.push(b"xy")).sum::<usize>());
        let accepted = producer.join().unwrap();
        assert_eq!(accepted, 4);
        let items: Vec<_> = std::iter::from_fn(|| r.read(Duration::ZERO)).collect();
        assert_eq!(items, [data(b"xyxy"), Rx::Lost]);
    }

    #[test]
    fn pipe_delivers_a_long_threaded_stream_intact() {
        let total = 20_000usize;
        let (w, r) = rx_pipe(total);
        let producer = thread::spawn(move || {
            let bytes: Vec<u8> = (0..total).map(|x| x as u8).collect();
            for chunk in bytes.chunks(37) {
                assert_eq!(w.push(chunk), chunk.len());
            }
        });
        let mut got = Vec::new();
        while got.len() < total {
            match r.read(Duration::from_secs(5)) {
                Some(Rx::Data(d)) => got.extend(d),
                Some(Rx::Lost) => panic!("lost at {}", got.len()),
                None => panic!("stalled at {}", got.len()),
            }
        }
        producer.join().unwrap();
        assert!(got.iter().enumerate().all(|(i, &b)| b == i as u8));
    }
}
