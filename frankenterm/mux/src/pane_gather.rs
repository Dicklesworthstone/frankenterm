//! The pane reader's gather policy (ft-yccm0.3.1.2).
//!
//! On macOS a PTY master returns about 1 KiB per `read` under load (Ghostty
//! makes the same observation, `Exec.zig`). Handing each such read to the
//! parser as its own batch pays the per-batch costs (ring publication, parser
//! wake, lock and fact publication) every kilobyte. The reader instead gathers:
//!
//! - It blocks for the first read of a batch, exactly as before.
//! - Below `min_bytes` it publishes at once, so keypress echo and other
//!   small interactive output never wait.
//! - At or above `min_bytes` it keeps reading what is already there: up to
//!   `spin_checks` zero-timeout readiness checks, then `poll`s of at most
//!   `poll_wait` each. It stops at a full slot, after `max_wait` in total, at
//!   EOF, or as soon as the parser goes idle. An idle parser interrupts the
//!   wait through a self-pipe.
//!
//! The PTY descriptor stays blocking: it shares its open file description
//! with the input writer, whose writes must keep blocking. The policy polls
//! before every read after the first, so it never blocks inside one.

#[cfg(unix)]
use std::io::Read as _;
#[cfg(unix)]
use std::os::fd::{AsFd as _, AsRawFd as _};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GatherPolicy {
    pub(crate) min_bytes: usize,
    pub(crate) spin_checks: u32,
    pub(crate) poll_wait: Duration,
    pub(crate) max_wait: Duration,
}

impl GatherPolicy {
    pub(crate) fn from_config(config: &config::ConfigHandle) -> Self {
        Self {
            min_bytes: config.mux_output_gather_min_bytes,
            spin_checks: config.mux_output_gather_spin_checks,
            poll_wait: Duration::from_micros(config.mux_output_gather_poll_us),
            max_wait: Duration::from_micros(config.mux_output_gather_max_wait_us),
        }
    }
}

/// What a bounded wait for more output found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GatherWait {
    Readable,
    /// The parser went idle and asked for what was already read.
    Interrupted,
    TimedOut,
}

/// The PTY side of a gather, abstracted so the policy can run on a fake fd.
pub(crate) trait GatherSource {
    /// A read. The first read of a batch may block; later reads follow a
    /// `Readable` wait and do not.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    /// Waits at most `timeout` (zero: just check) for readable output or an
    /// interrupt.
    fn wait(&mut self, timeout: Duration) -> std::io::Result<GatherWait>;
    fn now(&mut self) -> Instant;
    /// True once the parser has nothing left to parse.
    fn parser_idle(&mut self) -> bool;
}

/// Why a gathered batch was published.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GatherStop {
    /// Below `min_bytes`, or gathering is disabled: publish at once.
    Small,
    FullSlot,
    MaxWait,
    ParserIdle,
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct GatherStats {
    pub(crate) bytes: usize,
    pub(crate) reads: u32,
    pub(crate) waits: u32,
    pub(crate) waited: Duration,
    pub(crate) stop: GatherStop,
}

/// Fills `buf` with one batch of output under `policy`. `bytes == 0` with
/// `stop == Eof` means the PTY closed before any byte of this batch.
pub(crate) fn gather(
    source: &mut impl GatherSource,
    policy: &GatherPolicy,
    buf: &mut [u8],
) -> std::io::Result<GatherStats> {
    let first = loop {
        match source.read(buf) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            result => break result?,
        }
    };
    let mut stats = GatherStats {
        bytes: first,
        reads: 1,
        waits: 0,
        waited: Duration::ZERO,
        stop: GatherStop::Small,
    };
    if first == 0 {
        stats.stop = GatherStop::Eof;
        return Ok(stats);
    }
    if first >= buf.len() {
        stats.stop = GatherStop::FullSlot;
        return Ok(stats);
    }
    if first < policy.min_bytes || policy.max_wait.is_zero() {
        return Ok(stats);
    }
    let started = source.now();
    let mut empty_checks = 0_u32;
    stats.stop = loop {
        if stats.bytes >= buf.len() {
            break GatherStop::FullSlot;
        }
        if source.parser_idle() {
            break GatherStop::ParserIdle;
        }
        let elapsed = source.now().saturating_duration_since(started);
        if elapsed >= policy.max_wait {
            break GatherStop::MaxWait;
        }
        let timeout = if empty_checks < policy.spin_checks {
            Duration::ZERO
        } else {
            policy.poll_wait.min(policy.max_wait - elapsed)
        };
        stats.waits += 1;
        match source.wait(timeout)? {
            GatherWait::Readable => {
                let read = loop {
                    match source.read(&mut buf[stats.bytes..]) {
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        result => break result?,
                    }
                };
                stats.reads += 1;
                if read == 0 {
                    break GatherStop::Eof;
                }
                stats.bytes += read;
                empty_checks = 0;
            }
            GatherWait::Interrupted => break GatherStop::ParserIdle,
            GatherWait::TimedOut => empty_checks += 1,
        }
    };
    stats.waited = source.now().saturating_duration_since(started);
    Ok(stats)
}

/// The real source for one batch: a blocking PTY descriptor polled before
/// each later read, plus the parser's interrupt socket and the ring's idle
/// flag. The reader announces its gather at the first wait, so the parser
/// sees the announcement only while read bytes are actually held back, and
/// withdraws it when the source is dropped.
#[cfg(unix)]
pub(crate) struct PtyGatherSource<'a> {
    reader: &'a mut dyn portable_pty::PollablePtyReader,
    interrupt: &'a mut filedescriptor::FileDescriptor,
    ring: &'a crate::pane_byte_ring::RingHandle,
    announced: bool,
}

#[cfg(unix)]
impl<'a> PtyGatherSource<'a> {
    pub(crate) fn new(
        reader: &'a mut dyn portable_pty::PollablePtyReader,
        interrupt: &'a mut filedescriptor::FileDescriptor,
        ring: &'a crate::pane_byte_ring::RingHandle,
    ) -> Self {
        Self {
            reader,
            interrupt,
            ring,
            announced: false,
        }
    }

    fn drain_interrupts(&mut self) {
        let mut drained = [0_u8; 64];
        while let Ok(read) = self.interrupt.read(&mut drained) {
            if read == 0 {
                break;
            }
        }
    }
}

#[cfg(unix)]
impl Drop for PtyGatherSource<'_> {
    fn drop(&mut self) {
        if self.announced {
            self.ring.end_gather();
        }
    }
}

#[cfg(unix)]
impl GatherSource for PtyGatherSource<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }

    fn wait(&mut self, timeout: Duration) -> std::io::Result<GatherWait> {
        use filedescriptor::{poll, pollfd, AsRawSocketDescriptor, POLLIN};

        if !self.announced {
            // An interrupt left from an earlier batch must not end this one.
            self.drain_interrupts();
            self.announced = true;
            if !self.ring.begin_gather() {
                return Ok(GatherWait::Interrupted);
            }
        }
        let mut readiness = [
            pollfd {
                fd: self.reader.as_fd().as_raw_fd(),
                events: POLLIN,
                revents: 0,
            },
            pollfd {
                fd: self.interrupt.as_socket_descriptor(),
                events: POLLIN,
                revents: 0,
            },
        ];
        let ready = match poll(&mut readiness, Some(timeout)) {
            Ok(ready) => ready,
            Err(filedescriptor::Error::Poll(error)) | Err(filedescriptor::Error::Io(error))
                if error.kind() == std::io::ErrorKind::Interrupted =>
            {
                return Ok(GatherWait::TimedOut);
            }
            Err(error) => return Err(std::io::Error::other(error)),
        };
        if readiness[1].revents != 0 {
            self.drain_interrupts();
            return Ok(GatherWait::Interrupted);
        }
        if ready > 0 && readiness[0].revents != 0 {
            // POLLIN, or POLLHUP/POLLERR: the read returns data, EOF or the
            // error without blocking.
            return Ok(GatherWait::Readable);
        }
        Ok(GatherWait::TimedOut)
    }

    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn parser_idle(&mut self) -> bool {
        self.ring.parser_idle()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A scripted PTY: chunks become readable at fixed times on a fake clock,
    /// and a read returns at most `read_max` bytes (macOS: about 1 KiB).
    struct FakePty {
        start: Instant,
        now: Duration,
        arrivals: VecDeque<(Duration, usize)>,
        available: usize,
        read_max: usize,
        eof_at: Option<Duration>,
        idle_after_reads: Option<u32>,
        interrupt_at: Option<Duration>,
        reads: u32,
    }

    impl FakePty {
        fn new(arrivals: &[(u64, usize)], read_max: usize) -> Self {
            Self {
                start: Instant::now(),
                now: Duration::ZERO,
                arrivals: arrivals
                    .iter()
                    .map(|&(at_us, bytes)| (Duration::from_micros(at_us), bytes))
                    .collect(),
                available: 0,
                read_max,
                eof_at: None,
                idle_after_reads: None,
                interrupt_at: None,
                reads: 0,
            }
        }

        fn deliver_until(&mut self, now: Duration) {
            while self.arrivals.front().map_or(false, |&(at, _)| at <= now) {
                self.available += self.arrivals.pop_front().unwrap().1;
            }
        }

        fn eof_now(&self) -> bool {
            self.available == 0 && self.eof_at.map_or(false, |at| at <= self.now)
        }
    }

    impl GatherSource for FakePty {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.deliver_until(self.now);
            if self.available == 0 && !self.eof_now() {
                // Block until the next chunk (or EOF) arrives.
                let next = self
                    .arrivals
                    .front()
                    .map(|&(at, _)| at)
                    .or(self.eof_at)
                    .expect("a blocking read with nothing scheduled would hang");
                self.now = self.now.max(next);
                self.deliver_until(self.now);
            }
            self.reads += 1;
            let read = self.available.min(self.read_max).min(buf.len());
            self.available -= read;
            buf[..read].fill(b'x');
            Ok(read)
        }

        fn wait(&mut self, timeout: Duration) -> std::io::Result<GatherWait> {
            self.deliver_until(self.now);
            if self.available > 0 || self.eof_now() {
                return Ok(GatherWait::Readable);
            }
            let deadline = self.now + timeout;
            if let Some(at) = self.interrupt_at.filter(|&at| at <= deadline) {
                self.now = self.now.max(at);
                self.interrupt_at = None;
                return Ok(GatherWait::Interrupted);
            }
            let next = self.arrivals.front().map(|&(at, _)| at);
            match next.filter(|&at| at <= deadline) {
                Some(at) => {
                    self.now = at;
                    self.deliver_until(at);
                    Ok(GatherWait::Readable)
                }
                None => {
                    self.now = deadline;
                    if self.eof_now() {
                        Ok(GatherWait::Readable)
                    } else {
                        Ok(GatherWait::TimedOut)
                    }
                }
            }
        }

        fn now(&mut self) -> Instant {
            self.start + self.now
        }

        fn parser_idle(&mut self) -> bool {
            self.idle_after_reads
                .map_or(false, |reads| self.reads >= reads)
        }
    }

    fn policy() -> GatherPolicy {
        GatherPolicy {
            min_bytes: 1024,
            spin_checks: 16,
            poll_wait: Duration::from_millis(1),
            max_wait: Duration::from_millis(3),
        }
    }

    #[test]
    fn trickle_output_below_the_threshold_is_published_at_once() {
        // A shell echoing keystrokes: one byte every 50 ms.
        let mut pty = FakePty::new(&[(0, 1), (50_000, 1), (100_000, 1)], 1024);
        let mut buf = vec![0; 64 * 1024];
        for _ in 0..3 {
            let stats = gather(&mut pty, &policy(), &mut buf).unwrap();
            assert_eq!((stats.bytes, stats.reads, stats.waits), (1, 1, 0));
            assert_eq!(stats.stop, GatherStop::Small);
            assert_eq!(stats.waited, Duration::ZERO, "echo never waits");
        }
    }

    #[test]
    fn a_burst_already_buffered_fills_the_slot_without_waiting() {
        // 64 KiB are buffered at once; macOS hands them out 1 KiB per read.
        let mut pty = FakePty::new(&[(0, 64 * 1024)], 1024);
        let mut buf = vec![0; 64 * 1024];
        let stats = gather(&mut pty, &policy(), &mut buf).unwrap();
        assert_eq!(stats.bytes, 64 * 1024);
        assert_eq!(stats.reads, 64);
        assert_eq!(stats.stop, GatherStop::FullSlot);
        assert_eq!(stats.waited, Duration::ZERO);
    }

    #[test]
    fn a_flood_is_gathered_until_the_wait_cap() {
        // 1 KiB every 100 us: about 30 KiB arrive within the 3 ms cap.
        let arrivals: Vec<(u64, usize)> = (0..1000).map(|i| (i * 100, 1024)).collect();
        let mut pty = FakePty::new(&arrivals, 1024);
        let mut buf = vec![0; 64 * 1024];
        let stats = gather(&mut pty, &policy(), &mut buf).unwrap();
        assert_eq!(stats.stop, GatherStop::MaxWait);
        assert!(stats.waited >= Duration::from_millis(3));
        assert!(
            stats.waited < Duration::from_micros(3100),
            "{:?}",
            stats.waited
        );
        assert_eq!(stats.bytes, 31 * 1024, "the first read plus one per 100 us");
        // The next batch starts from what is still buffered.
        let next = gather(&mut pty, &policy(), &mut buf).unwrap();
        assert_eq!(next.stop, GatherStop::MaxWait);
    }

    #[test]
    fn a_burst_that_stops_waits_for_more_only_up_to_the_cap() {
        // 2 KiB, then silence: 16 immediate checks, then 1 ms polls, 3 ms total.
        let mut pty = FakePty::new(&[(0, 2048)], 1024);
        let mut buf = vec![0; 64 * 1024];
        let stats = gather(&mut pty, &policy(), &mut buf).unwrap();
        assert_eq!(stats.bytes, 2048);
        assert_eq!(stats.stop, GatherStop::MaxWait);
        assert_eq!(stats.waited, Duration::from_millis(3));
        assert_eq!(
            stats.waits,
            1 + 16 + 3,
            "one read-ahead, 16 checks, three 1 ms polls"
        );
    }

    #[test]
    fn an_idle_parser_ends_the_gather() {
        let arrivals: Vec<(u64, usize)> = (0..100).map(|i| (i * 100, 1024)).collect();
        let mut pty = FakePty::new(&arrivals, 1024);
        pty.idle_after_reads = Some(3);
        let mut buf = vec![0; 64 * 1024];
        let stats = gather(&mut pty, &policy(), &mut buf).unwrap();
        assert_eq!(stats.stop, GatherStop::ParserIdle);
        assert_eq!(stats.reads, 3);
    }

    #[test]
    fn the_parser_interrupt_ends_a_wait_early() {
        let mut pty = FakePty::new(&[(0, 2048)], 1024);
        pty.interrupt_at = Some(Duration::from_micros(1500));
        let mut buf = vec![0; 64 * 1024];
        let stats = gather(&mut pty, &policy(), &mut buf).unwrap();
        assert_eq!(stats.stop, GatherStop::ParserIdle);
        assert_eq!(stats.bytes, 2048);
        assert_eq!(stats.waited, Duration::from_micros(1500));
    }

    #[test]
    fn eof_ends_a_batch_with_the_bytes_it_had() {
        let mut pty = FakePty::new(&[(0, 1500)], 1024);
        pty.eof_at = Some(Duration::from_micros(200));
        let mut buf = vec![0; 64 * 1024];
        let stats = gather(&mut pty, &policy(), &mut buf).unwrap();
        assert_eq!(stats.bytes, 1500);
        assert_eq!(stats.stop, GatherStop::Eof);
        let after = gather(&mut pty, &policy(), &mut buf).unwrap();
        assert_eq!((after.bytes, after.stop), (0, GatherStop::Eof));
    }

    /// The real source on a real descriptor (a socketpair standing in for the
    /// PTY): a buffered burst, a parser that goes idle mid-gather, and EOF.
    #[cfg(unix)]
    #[test]
    fn the_pty_source_gathers_a_real_descriptor_and_honours_the_parser_interrupt() {
        use std::io::Write as _;

        let (mut child, mut master) = filedescriptor::socketpair().unwrap();
        let (mut interrupt_tx, mut interrupt_rx) = filedescriptor::socketpair().unwrap();
        interrupt_tx.set_non_blocking(true).unwrap();
        interrupt_rx.set_non_blocking(true).unwrap();
        let (producer, consumer) = crate::pane_byte_ring::pane_byte_ring(2, 64 * 1024);
        let ring = producer.handle();
        let mut buf = vec![0; 64 * 1024];

        child.write_all(&[b'a'; 3000]).unwrap();
        let stats = {
            let mut source = PtyGatherSource::new(&mut master, &mut interrupt_rx, &ring);
            gather(&mut source, &policy(), &mut buf).unwrap()
        };
        assert_eq!(stats.bytes, 3000);
        assert_eq!(stats.stop, GatherStop::MaxWait);
        assert!(buf[..3000].iter().all(|byte| *byte == b'a'));
        assert!(
            !consumer.reader_gathering(),
            "a finished batch withdraws its gather announcement"
        );

        child.write_all(&[b'b'; 2000]).unwrap();
        let parser = std::thread::spawn(move || {
            while !consumer.reader_gathering() {
                std::thread::yield_now();
            }
            assert!(consumer.announce_sleep());
            assert!(consumer.reader_gathering());
            interrupt_tx.write_all(&[1]).unwrap();
            consumer
        });
        let patient = GatherPolicy {
            poll_wait: Duration::from_secs(2),
            max_wait: Duration::from_secs(10),
            ..policy()
        };
        let started = Instant::now();
        let stats = {
            let mut source = PtyGatherSource::new(&mut master, &mut interrupt_rx, &ring);
            gather(&mut source, &patient, &mut buf).unwrap()
        };
        assert_eq!(stats.stop, GatherStop::ParserIdle);
        assert_eq!(stats.bytes, 2000);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the idle parser cut the gather short"
        );
        let consumer = parser.join().unwrap();
        consumer.end_sleep();

        drop(child);
        let stats = {
            let mut source = PtyGatherSource::new(&mut master, &mut interrupt_rx, &ring);
            gather(&mut source, &policy(), &mut buf).unwrap()
        };
        assert_eq!((stats.bytes, stats.stop), (0, GatherStop::Eof));
        drop(producer);
    }

    #[test]
    fn a_zero_wait_cap_disables_gathering() {
        let mut pty = FakePty::new(&[(0, 64 * 1024)], 1024);
        let mut buf = vec![0; 64 * 1024];
        let off = GatherPolicy {
            max_wait: Duration::ZERO,
            ..policy()
        };
        let stats = gather(&mut pty, &off, &mut buf).unwrap();
        assert_eq!(
            (stats.bytes, stats.reads, stats.stop),
            (1024, 1, GatherStop::Small)
        );
    }
}
