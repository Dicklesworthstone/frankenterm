//! The reader -> parser byte ring of one pane (ft-yccm0.3.1.1).
//!
//! Before this ring the pane reader wrote PTY output into an AF_UNIX
//! socketpair that the parser read back: two syscalls and two kernel copies
//! per chunk. Here the reader copies bytes into preallocated slots and
//! publishes them; the parser parses each slot in place and releases it.
//!
//! Slots move between two lock-free queues, `filled` (reader -> parser, in
//! publication order) and `free` (parser -> reader). Owning a `Box<Slot>` is
//! the right to touch its bytes, so the exchange is safe Rust: no shared
//! mutable memory, only ownership transfer through `crossbeam::ArrayQueue`.
//! Every slot carries the byte sequence number of its first byte, and the
//! consumer checks continuity, so a lost or reordered slot is detected rather
//! than parsed.
//!
//! Waiting is park-based, never a spin:
//! - When no slot is free the reader parks, and stops reading the PTY, so
//!   the kernel throttles the child. The parser unparks it after releasing a
//!   slot, if the reader announced that it was waiting.
//! - When nothing is published the parser sleeps in `poll` on its wake
//!   socket. The reader writes a wake byte after publishing, but only if the
//!   parser announced that it was going to sleep, so a busy pipeline makes no
//!   syscalls at all.
//!
//! - While the reader gathers a batch (ft-yccm0.3.1.2) it announces it; a
//!   parser that goes to sleep then interrupts the gather, so bytes already
//!   read are published at once instead of after the gather's wait.
//!
//! Every announcement is Dekker-style: each side stores its own flag, issues
//! a sequentially consistent fence, then re-checks the other side's state.
//! At least one side then sees the other, so a wakeup is never lost.

use crossbeam::queue::ArrayQueue;
use parking_lot::Mutex;
use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::Arc;

struct Slot {
    bytes: Box<[u8]>,
    len: usize,
    seq_start: u64,
}

struct PaneByteRing {
    filled: ArrayQueue<Box<Slot>>,
    free: ArrayQueue<Box<Slot>>,
    producer_closed: AtomicBool,
    consumer_closed: AtomicBool,
    parser_waiting: AtomicBool,
    reader_waiting: AtomicBool,
    reader_gathering: AtomicBool,
    reader_thread: Mutex<Option<std::thread::Thread>>,
    #[cfg(test)]
    reader_parks: std::sync::atomic::AtomicUsize,
}

impl PaneByteRing {
    fn unpark_reader_if_waiting(&self) {
        fence(Ordering::SeqCst);
        if self.reader_waiting.load(Ordering::SeqCst) {
            if let Some(reader) = self.reader_thread.lock().as_ref() {
                reader.unpark();
            }
        }
    }

    fn close_consumer(&self) {
        self.consumer_closed.store(true, Ordering::SeqCst);
        self.unpark_reader_if_waiting();
    }
}

/// Builds one pane's ring: `slots` preallocated slots of `slot_bytes` each.
pub(crate) fn pane_byte_ring(slots: usize, slot_bytes: usize) -> (RingProducer, RingConsumer) {
    let slots = slots.max(1);
    let slot_bytes = slot_bytes.max(1);
    let free = ArrayQueue::new(slots);
    for _ in 0..slots {
        let slot = Box::new(Slot {
            bytes: vec![0_u8; slot_bytes].into_boxed_slice(),
            len: 0,
            seq_start: 0,
        });
        if free.push(slot).is_err() {
            unreachable!("a fresh free queue holds every slot");
        }
    }
    let ring = Arc::new(PaneByteRing {
        filled: ArrayQueue::new(slots),
        free,
        producer_closed: AtomicBool::new(false),
        consumer_closed: AtomicBool::new(false),
        parser_waiting: AtomicBool::new(false),
        reader_waiting: AtomicBool::new(false),
        reader_gathering: AtomicBool::new(false),
        reader_thread: Mutex::new(None),
        #[cfg(test)]
        reader_parks: std::sync::atomic::AtomicUsize::new(0),
    });
    (
        RingProducer {
            ring: Arc::clone(&ring),
            next_seq: 0,
            closed: false,
        },
        RingConsumer {
            ring,
            current: None,
            cursor: 0,
            next_seq: 0,
            closed: false,
            broken: None,
        },
    )
}

/// The reader's end. Exactly one exists per ring.
pub(crate) struct RingProducer {
    ring: Arc<PaneByteRing>,
    next_seq: u64,
    closed: bool,
}

/// Ring signals usable from any thread without the producer: closing the
/// parser's side, and the reader's gather announcement.
#[derive(Clone)]
pub(crate) struct RingHandle(Arc<PaneByteRing>);

impl RingHandle {
    /// The parser is gone: a parked or later write fails with BrokenPipe.
    pub(crate) fn close_consumer(&self) {
        self.0.close_consumer();
    }

    /// The reader announces that it is gathering a batch. False: the parser
    /// is already idle, so publish what was read now. Pair with `end_gather`.
    pub(crate) fn begin_gather(&self) -> bool {
        self.0.reader_gathering.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        !self.0.parser_waiting.load(Ordering::SeqCst)
    }

    pub(crate) fn end_gather(&self) {
        self.0.reader_gathering.store(false, Ordering::SeqCst);
    }

    /// True while the parser sleeps for want of output.
    pub(crate) fn parser_idle(&self) -> bool {
        self.0.parser_waiting.load(Ordering::SeqCst)
    }
}

impl RingProducer {
    pub(crate) fn handle(&self) -> RingHandle {
        RingHandle(Arc::clone(&self.ring))
    }

    /// Publishes a prefix of `bytes` into the free slots without blocking and
    /// returns its length. `WouldBlock` means no slot is free and nothing was
    /// published; `BrokenPipe` means the parser is gone. After publishing,
    /// `wake` runs if the parser announced it is going to sleep.
    pub(crate) fn write(&mut self, bytes: &[u8], wake: impl FnOnce()) -> std::io::Result<usize> {
        if self.closed {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        if self.ring.consumer_closed.load(Ordering::Acquire) {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        let mut written = 0;
        while written < bytes.len() {
            let Some(mut slot) = self.ring.free.pop() else {
                break;
            };
            let take = (bytes.len() - written).min(slot.bytes.len());
            slot.bytes[..take].copy_from_slice(&bytes[written..written + take]);
            slot.len = take;
            slot.seq_start = self.next_seq;
            self.next_seq += take as u64;
            written += take;
            if self.ring.filled.push(slot).is_err() {
                unreachable!("filled and free together never hold more than every slot");
            }
        }
        if written == 0 {
            return Err(std::io::ErrorKind::WouldBlock.into());
        }
        fence(Ordering::SeqCst);
        if self.ring.parser_waiting.load(Ordering::SeqCst) {
            wake();
        }
        Ok(written)
    }

    /// Parks until a slot is free or the parser is gone (`BrokenPipe`).
    pub(crate) fn wait_writable(&self) -> std::io::Result<()> {
        loop {
            if self.ring.consumer_closed.load(Ordering::Acquire) {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            if !self.ring.free.is_empty() {
                return Ok(());
            }
            {
                let mut reader = self.ring.reader_thread.lock();
                if reader.is_none() {
                    *reader = Some(std::thread::current());
                }
            }
            self.ring.reader_waiting.store(true, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if self.ring.free.is_empty() && !self.ring.consumer_closed.load(Ordering::SeqCst) {
                #[cfg(test)]
                self.ring.reader_parks.fetch_add(1, Ordering::Relaxed);
                std::thread::park();
            }
            self.ring.reader_waiting.store(false, Ordering::SeqCst);
        }
    }

    /// No more bytes follow: once it has drained, the parser sees EOF.
    /// `wake` runs if the parser announced it is going to sleep.
    pub(crate) fn close(&mut self, wake: impl FnOnce()) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.ring.producer_closed.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if self.ring.parser_waiting.load(Ordering::SeqCst) {
            wake();
        }
    }

    #[cfg(test)]
    pub(crate) fn published(&self) -> u64 {
        self.next_seq
    }
}

impl Drop for RingProducer {
    fn drop(&mut self) {
        self.close(|| {});
    }
}

fn discontinuity(observed: u64, expected: u64) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!(
            "pane byte ring slot starts at byte {} but byte {} was next",
            observed, expected
        ),
    )
}

/// What the parser finds in the ring.
pub(crate) enum RingRead<'a> {
    /// Unparsed bytes at the head of the current slot.
    Bytes(&'a [u8]),
    /// Nothing is published yet.
    Empty,
    /// The reader closed and every published byte has been consumed.
    Closed,
}

/// The parser's end. Exactly one exists per ring.
pub(crate) struct RingConsumer {
    ring: Arc<PaneByteRing>,
    current: Option<Box<Slot>>,
    cursor: usize,
    next_seq: u64,
    closed: bool,
    /// A discontinuity was seen; every later read refuses.
    broken: Option<(u64, u64)>,
}

impl RingConsumer {
    fn current_has_bytes(&self) -> bool {
        self.current
            .as_ref()
            .map_or(false, |slot| self.cursor < slot.len)
    }

    /// True when a read would find bytes or EOF, so the parser should not
    /// sleep.
    pub(crate) fn has_data_or_eof(&self) -> bool {
        self.current_has_bytes()
            || !self.ring.filled.is_empty()
            || self.ring.producer_closed.load(Ordering::Acquire)
    }

    /// Announces that the parser is about to sleep and re-checks. True: sleep
    /// is safe, because any later publication wakes the parser. False:
    /// something arrived, do not sleep. Pair every true with `end_sleep`.
    pub(crate) fn announce_sleep(&self) -> bool {
        self.ring.parser_waiting.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        if self.has_data_or_eof() {
            self.ring.parser_waiting.store(false, Ordering::SeqCst);
            return false;
        }
        true
    }

    pub(crate) fn end_sleep(&self) {
        self.ring.parser_waiting.store(false, Ordering::SeqCst);
    }

    /// After a true `announce_sleep`: is the reader holding read bytes back
    /// in a gather? If so the parser interrupts it before sleeping. Read after
    /// the announcement's fence, it pairs with `RingHandle::begin_gather`.
    pub(crate) fn reader_gathering(&self) -> bool {
        self.ring.reader_gathering.load(Ordering::SeqCst)
    }

    /// Up to `max` unparsed bytes, in publication order. `InvalidData` means
    /// a slot did not start where the previous one ended.
    pub(crate) fn peek(&mut self, max: usize) -> std::io::Result<RingRead<'_>> {
        if let Some((observed, expected)) = self.broken {
            return Err(discontinuity(observed, expected));
        }
        if max == 0 {
            return Ok(RingRead::Empty);
        }
        if !self.current_has_bytes() {
            self.release_current();
            let next = match self.ring.filled.pop() {
                Some(slot) => Some(slot),
                // A slot published before close is visible once close is.
                None if self.ring.producer_closed.load(Ordering::Acquire) => self.ring.filled.pop(),
                None => None,
            };
            match next {
                Some(slot) => {
                    if slot.seq_start != self.next_seq {
                        let broken = (slot.seq_start, self.next_seq);
                        self.broken = Some(broken);
                        if self.ring.free.push(slot).is_err() {
                            unreachable!(
                                "filled and free together never hold more than every slot"
                            );
                        }
                        return Err(discontinuity(broken.0, broken.1));
                    }
                    self.current = Some(slot);
                    self.cursor = 0;
                }
                None if self.ring.producer_closed.load(Ordering::Acquire) => {
                    return Ok(RingRead::Closed);
                }
                None => return Ok(RingRead::Empty),
            }
        }
        let slot = self
            .current
            .as_ref()
            .expect("a current slot with unparsed bytes");
        let end = slot.len.min(self.cursor.saturating_add(max));
        Ok(RingRead::Bytes(&slot.bytes[self.cursor..end]))
    }

    /// Marks `n` bytes of the last `peek` as parsed; an exhausted slot goes
    /// back to the reader at once.
    pub(crate) fn consume(&mut self, n: usize) {
        let len = self.current.as_ref().map_or(0, |slot| slot.len);
        let n = n.min(len.saturating_sub(self.cursor));
        self.cursor += n;
        self.next_seq += n as u64;
        if self.cursor >= len {
            self.release_current();
        }
    }

    fn release_current(&mut self) {
        if let Some(mut slot) = self.current.take() {
            slot.len = 0;
            self.cursor = 0;
            if self.ring.free.push(slot).is_err() {
                unreachable!("filled and free together never hold more than every slot");
            }
            self.ring.unpark_reader_if_waiting();
        }
    }

    /// Bytes parsed so far, by sequence number.
    #[cfg(test)]
    pub(crate) fn consumed(&self) -> u64 {
        self.next_seq
    }

    /// The parser is gone: the reader's writes fail and a parked reader
    /// wakes.
    pub(crate) fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            self.ring.close_consumer();
        }
    }
}

impl Drop for RingConsumer {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::time::{Duration, Instant};

    fn drain_all(consumer: &mut RingConsumer, max: usize) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            match consumer.peek(max).unwrap() {
                RingRead::Bytes(bytes) => {
                    let n = bytes.len();
                    out.extend_from_slice(bytes);
                    consumer.consume(n);
                }
                RingRead::Empty | RingRead::Closed => return out,
            }
        }
    }

    #[test]
    fn bytes_arrive_once_in_order_with_slot_sequence_numbers() {
        let (mut producer, mut consumer) = pane_byte_ring(3, 4);
        assert_eq!(producer.write(b"0123456789", || {}).unwrap(), 10);
        assert!(matches!(
            producer.write(b"x", || {}),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
        ));
        assert_eq!(drain_all(&mut consumer, 3), b"0123456789");
        assert_eq!(consumer.consumed(), 10);
        assert_eq!(producer.write(b"abcdefghijkl", || {}).unwrap(), 12);
        assert_eq!(producer.published(), 22);
        producer.close(|| {});
        assert_eq!(drain_all(&mut consumer, 100), b"abcdefghijkl");
        assert!(matches!(consumer.peek(1).unwrap(), RingRead::Closed));
    }

    #[test]
    fn a_partially_parsed_slot_is_not_released_and_resumes_where_it_stopped() {
        let (mut producer, mut consumer) = pane_byte_ring(1, 8);
        assert_eq!(producer.write(b"abcdefgh", || {}).unwrap(), 8);
        match consumer.peek(3).unwrap() {
            RingRead::Bytes(bytes) => assert_eq!(bytes, b"abc"),
            _ => panic!("published bytes are readable"),
        }
        consumer.consume(3);
        assert!(
            producer.write(b"z", || {}).is_err(),
            "the half-parsed slot still belongs to the parser"
        );
        match consumer.peek(64).unwrap() {
            RingRead::Bytes(bytes) => assert_eq!(bytes, b"defgh"),
            _ => panic!("the rest of the slot follows"),
        }
        consumer.consume(5);
        assert_eq!(producer.write(b"z", || {}).unwrap(), 1);
    }

    #[test]
    fn the_parser_is_woken_only_when_it_announced_sleep() {
        let (mut producer, consumer) = pane_byte_ring(2, 4);
        let wakes = std::cell::Cell::new(0);
        producer.write(b"a", || wakes.set(wakes.get() + 1)).unwrap();
        assert_eq!(wakes.get(), 0, "a busy parser costs the reader no wake");
        assert!(!consumer.announce_sleep(), "pending bytes forbid sleep");
        let (mut producer, mut consumer) = pane_byte_ring(2, 4);
        assert!(consumer.announce_sleep());
        producer.write(b"b", || wakes.set(wakes.get() + 1)).unwrap();
        assert_eq!(wakes.get(), 1);
        consumer.end_sleep();
        assert_eq!(drain_all(&mut consumer, 4), b"b");
        assert!(consumer.announce_sleep());
        producer.close(|| wakes.set(wakes.get() + 1));
        assert_eq!(wakes.get(), 2, "close wakes a sleeping parser for EOF");
    }

    #[test]
    fn a_sleeping_parser_and_a_gathering_reader_see_each_other() {
        let (producer, consumer) = pane_byte_ring(2, 4);
        let handle = producer.handle();
        // The reader gathers first: the parser that then sleeps sees it.
        assert!(handle.begin_gather());
        assert!(consumer.announce_sleep());
        assert!(consumer.reader_gathering());
        consumer.end_sleep();
        handle.end_gather();
        // The parser sleeps first: the reader that then gathers sees it.
        assert!(consumer.announce_sleep());
        assert!(!handle.begin_gather());
        assert!(handle.parser_idle());
        handle.end_gather();
        consumer.end_sleep();
        assert!(!handle.parser_idle());
        assert!(!consumer.reader_gathering());
    }

    #[test]
    fn racing_gather_and_sleep_announcements_never_miss_each_other() {
        for _ in 0..20_000 {
            let (producer, consumer) = pane_byte_ring(1, 1);
            let handle = producer.handle();
            let start = Arc::new(std::sync::Barrier::new(2));
            let reader_start = Arc::clone(&start);
            let reader = std::thread::spawn(move || {
                reader_start.wait();
                !handle.begin_gather()
            });
            start.wait();
            let parser_saw_gather = consumer.announce_sleep() && consumer.reader_gathering();
            let reader_saw_idle = reader.join().unwrap();
            assert!(
                parser_saw_gather || reader_saw_idle,
                "neither side saw the other: a gathered batch could wait out its gather"
            );
            drop(producer);
        }
    }

    #[test]
    fn a_gone_parser_fails_writes_and_releases_a_parked_reader() {
        let (mut producer, mut consumer) = pane_byte_ring(1, 4);
        producer.write(b"full", || {}).unwrap();
        let reader = std::thread::spawn(move || {
            let waited = producer.wait_writable();
            (waited, producer.write(b"late", || {}))
        });
        while consumer.ring.reader_parks.load(Ordering::Relaxed) == 0 {
            std::thread::yield_now();
        }
        consumer.close();
        let (waited, wrote) = reader.join().unwrap();
        assert_eq!(waited.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
        assert_eq!(wrote.unwrap_err().kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn a_discontinuous_slot_is_refused() {
        let (mut producer, mut consumer) = pane_byte_ring(2, 4);
        producer.write(b"abcd", || {}).unwrap();
        producer.next_seq += 1;
        producer.write(b"efgh", || {}).unwrap();
        assert_eq!(drain_all_until_error(&mut consumer), b"abcd");
        assert_eq!(
            consumer.peek(64).err().map(|error| error.kind()),
            Some(std::io::ErrorKind::InvalidData),
            "a broken ring stays broken"
        );
    }

    fn drain_all_until_error(consumer: &mut RingConsumer) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            match consumer.peek(64) {
                Ok(RingRead::Bytes(bytes)) => {
                    let n = bytes.len();
                    out.extend_from_slice(bytes);
                    consumer.consume(n);
                }
                Ok(_) => panic!("the discontinuity must be reported"),
                Err(error) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
                    return out;
                }
            }
        }
    }

    /// One concurrent run: the reader writes `chunks` (parking when the ring
    /// is full), the parser reads with `reads` max sizes (sleeping via a
    /// condvar standing in for the wake socket). Every byte must arrive
    /// exactly once, in order, and neither side may hang.
    fn concurrent_run(slots: usize, slot_bytes: usize, chunks: Vec<Vec<u8>>, reads: Vec<usize>) {
        let (mut producer, mut consumer) = pane_byte_ring(slots, slot_bytes);
        let wake = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let expected: Vec<u8> = chunks.concat();
        let reader_wake = Arc::clone(&wake);
        let reader = std::thread::spawn(move || {
            let notify = || {
                let (flag, condvar) = &*reader_wake;
                *flag.lock().unwrap() = true;
                condvar.notify_one();
            };
            for chunk in chunks {
                let mut offset = 0;
                while offset < chunk.len() {
                    match producer.write(&chunk[offset..], notify) {
                        Ok(n) => offset += n,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            producer.wait_writable().unwrap();
                        }
                        Err(error) => panic!("reader write failed: {}", error),
                    }
                }
            }
            producer.close(notify);
        });
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut observed = Vec::with_capacity(expected.len());
        let mut read_index = 0;
        loop {
            assert!(Instant::now() < deadline, "the parser hung: a lost wakeup");
            let max = reads[read_index % reads.len()].max(1);
            read_index += 1;
            match consumer.peek(max).unwrap() {
                RingRead::Bytes(bytes) => {
                    let n = bytes.len();
                    assert!(n <= max);
                    observed.extend_from_slice(bytes);
                    consumer.consume(n);
                }
                RingRead::Closed => break,
                RingRead::Empty => {
                    if consumer.announce_sleep() {
                        let (flag, condvar) = &*wake;
                        let mut woken = flag.lock().unwrap();
                        while !*woken {
                            let (guard, timeout) = condvar
                                .wait_timeout(woken, Duration::from_secs(10))
                                .unwrap();
                            woken = guard;
                            assert!(
                                !timeout.timed_out(),
                                "the parser slept through a publication"
                            );
                        }
                        *woken = false;
                        consumer.end_sleep();
                    }
                }
            }
        }
        reader.join().unwrap();
        assert_eq!(observed.len(), expected.len());
        assert!(
            observed == expected,
            "bytes were lost, duplicated or reordered"
        );
        assert_eq!(consumer.consumed(), expected.len() as u64);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn randomized_schedules_deliver_every_byte_once_in_order(
            slots in 1_usize..5,
            slot_bytes in 1_usize..64,
            chunks in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..200), 1..40),
            reads in proptest::collection::vec(1_usize..300, 1..8),
        ) {
            concurrent_run(slots, slot_bytes, chunks, reads);
        }
    }

    #[test]
    fn stress_a_tiny_ring_through_many_megabytes_without_a_lost_wakeup() {
        let chunks: Vec<Vec<u8>> = (0..4096_u32)
            .map(|i| {
                let len = 1 + (i as usize * 7919) % 2048;
                (0..len).map(|j| (i as usize + j) as u8).collect()
            })
            .collect();
        concurrent_run(2, 1024, chunks, vec![1, 17, 4096, 300, 1024]);
    }

    #[test]
    fn a_full_ring_parks_the_reader_instead_of_spinning() {
        let (mut producer, mut consumer) = pane_byte_ring(1, 16);
        producer.write(&[7; 16], || {}).unwrap();
        let ring = Arc::clone(&consumer.ring);
        let reader = std::thread::spawn(move || producer.wait_writable());
        while ring.reader_parks.load(Ordering::Relaxed) == 0 {
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(50));
        // park may return spuriously, but a spinning reader would re-check
        // thousands of times in 50 ms.
        assert!(
            ring.reader_parks.load(Ordering::Relaxed) <= 3,
            "a parked reader stays parked until a slot is released"
        );
        assert_eq!(drain_all(&mut consumer, 16), vec![7; 16]);
        reader.join().unwrap().unwrap();
    }
}
