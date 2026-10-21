//! Caller-driven terminal egress. The byte and descriptor rings are allocated
//! once; neither parser replies nor user input can grow them or spawn a thread.

use super::{WriteClass, WriterBacklog};
use crate::config::NewlineCanon;
use std::collections::VecDeque;
use std::io::{self, Write};

/// Per-terminal egress limits, independent of screen, parser and scrollback
/// limits. Embedders must additionally budget the number of live terminals and
/// every downstream queue. Bytes remain charged until the sink accepts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedOutputLimits {
    pub input_bytes: usize,
    pub reply_bytes: usize,
    pub input_units: usize,
    pub reply_units: usize,
    pub maximum_unit_bytes: usize,
    pub maximum_paste_bytes: usize,
    pub maximum_drain_bytes: usize,
}

impl Default for BoundedOutputLimits {
    fn default() -> Self {
        Self {
            input_bytes: 128 * 1024,
            reply_bytes: 16 * 1024,
            input_units: 512,
            reply_units: 128,
            maximum_unit_bytes: 64 * 1024,
            maximum_paste_bytes: 64 * 1024 - 12,
            maximum_drain_bytes: 4096,
        }
    }
}

impl BoundedOutputLimits {
    fn validate(self) -> io::Result<Self> {
        if !(1..=1024 * 1024).contains(&self.input_bytes)
            || !(1..=256 * 1024).contains(&self.reply_bytes)
            || !(1..=4096).contains(&self.input_units)
            || !(1..=1024).contains(&self.reply_units)
            || !(1..=self.input_bytes).contains(&self.maximum_unit_bytes)
            || !(1..=self.maximum_unit_bytes).contains(&self.maximum_paste_bytes)
            || !(1..=65536).contains(&self.maximum_drain_bytes)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid bounded terminal output limits",
            ));
        }
        Ok(self)
    }
}

/// The sink's result for exactly the offered prefix. `Accepted` means that a
/// bounded downstream owner took that prefix, not that a remote PTY processed
/// it. `WouldBlock` guarantees that no byte was accepted. `Failed` makes the
/// offered prefix uncertain and permanently closes this output owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedWriteResult {
    Accepted(usize),
    WouldBlock,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedOutputDrain {
    Idle,
    WouldBlock,
    Accepted {
        sequence: u64,
        class: WriteClass,
        bytes: usize,
        unit_complete: bool,
    },
    Closed(BoundedOutputSettlement),
}

/// An immutable copy of one complete, never partially drained parser reply.
/// Copying does not transfer ownership or release the reply's reservation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedReplyCopy {
    pub sequence: u64,
    pub bytes: usize,
}

/// Settlement of a complete reply copied into the embedding's bounded buffer.
/// `Accepted` means another bounded owner admitted the whole reply. `Dropped`
/// means no bytes were admitted; it increments this owner's drop counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedReplyDisposition {
    Accepted,
    Dropped,
}

/// Content-free close evidence. The uncertain prefix belongs to the first
/// remaining unit; no later byte was offered to the sink. A driver must not
/// replay the uncertain prefix or claim that accepted input was executed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoundedOutputSettlement {
    pub accepted_through: u64,
    pub first_remaining_sequence: Option<u64>,
    pub first_remaining_accepted_prefix: usize,
    pub definitely_not_accepted_input_bytes: usize,
    pub definitely_not_accepted_reply_bytes: usize,
    pub uncertain_input_bytes: usize,
    pub uncertain_reply_bytes: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoundedOutputStatus {
    pub pending_input_bytes: usize,
    pub pending_reply_bytes: usize,
    pub pending_input_units: usize,
    pub pending_reply_units: usize,
    pub staging_bytes: usize,
    pub admitted_through: u64,
    pub accepted_through: u64,
    pub dropped_replies: u64,
    pub dropped_reply_bytes: u64,
    pub closed: bool,
}

struct Unit {
    sequence: u64,
    class: WriteClass,
    remaining: usize,
    accepted: usize,
}

pub(super) struct BoundedOutputWriter {
    limits: BoundedOutputLimits,
    bytes: VecDeque<u8>,
    units: VecDeque<Unit>,
    class: WriteClass,
    input_batch: bool,
    staging: usize,
    rejected: Option<io::ErrorKind>,
    status: BoundedOutputStatus,
    // Set before invoking a sink. If it panics, a later call cannot replay the
    // offered bytes: the owner closes with this prefix explicitly uncertain.
    offered: usize,
    settlement: Option<BoundedOutputSettlement>,
}

impl BoundedOutputWriter {
    pub(super) fn new(limits: BoundedOutputLimits) -> io::Result<Self> {
        let limits = limits.validate()?;
        let mut bytes = VecDeque::new();
        bytes
            .try_reserve_exact(limits.input_bytes + limits.reply_bytes)
            .map_err(|_| io::Error::other("terminal byte ring allocation failed"))?;
        let mut units = VecDeque::new();
        units
            .try_reserve_exact(limits.input_units + limits.reply_units)
            .map_err(|_| io::Error::other("terminal unit ring allocation failed"))?;
        Ok(Self {
            limits,
            bytes,
            units,
            class: WriteClass::Reply,
            input_batch: false,
            staging: 0,
            rejected: None,
            status: BoundedOutputStatus::default(),
            offered: 0,
            settlement: None,
        })
    }

    pub(super) fn class(&self) -> WriteClass {
        self.class
    }

    pub(super) fn set_class(&mut self, class: WriteClass) {
        // The TerminalState boundary flushes before switching. Keep an
        // unexpected caller from relabelling already reserved bytes anyway.
        if self.class != class {
            let _ = self.flush();
            self.class = class;
        }
    }

    pub(super) fn begin_input_batch(&mut self) {
        self.input_batch = true;
    }

    pub(super) fn finish_input_batch(&mut self, commit: bool) -> io::Result<()> {
        self.input_batch = false;
        if !commit {
            if self.staging != 0 {
                self.bytes.truncate(self.bytes.len() - self.staging);
                self.release_bytes(self.class, self.staging);
                self.release_unit(self.class);
                self.staging = 0;
            }
            self.rejected = None;
            return Ok(());
        }
        self.flush()
    }

    pub(super) fn status(&self) -> BoundedOutputStatus {
        BoundedOutputStatus {
            staging_bytes: self.staging,
            closed: self.settlement.is_some() || self.offered != 0,
            ..self.status
        }
    }

    pub(super) fn backlog(&self) -> WriterBacklog {
        WriterBacklog {
            pending_input_bytes: self.status.pending_input_bytes,
            pending_reply_bytes: self.status.pending_reply_bytes,
            dropped_replies: self.status.dropped_replies,
            dropped_reply_bytes: self.status.dropped_reply_bytes,
        }
    }

    fn available(&self, count: usize) -> io::Result<()> {
        if self.settlement.is_some() || self.offered != 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.status.admitted_through == u64::MAX {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let (bytes, byte_limit, units, unit_limit) = match self.class {
            WriteClass::UserInput => (
                self.status.pending_input_bytes,
                self.limits.input_bytes,
                self.status.pending_input_units,
                self.limits.input_units,
            ),
            WriteClass::Reply => (
                self.status.pending_reply_bytes,
                self.limits.reply_bytes,
                self.status.pending_reply_units,
                self.limits.reply_units,
            ),
        };
        if count > byte_limit.saturating_sub(bytes)
            || count > self.limits.maximum_unit_bytes.saturating_sub(self.staging)
            || (self.staging == 0 && units >= unit_limit)
        {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }

    fn charge_bytes(&mut self, class: WriteClass, count: usize) {
        match class {
            WriteClass::UserInput => self.status.pending_input_bytes += count,
            WriteClass::Reply => self.status.pending_reply_bytes += count,
        }
    }

    fn release_bytes(&mut self, class: WriteClass, count: usize) {
        match class {
            WriteClass::UserInput => self.status.pending_input_bytes -= count,
            WriteClass::Reply => self.status.pending_reply_bytes -= count,
        }
    }

    fn charge_unit(&mut self, class: WriteClass) {
        match class {
            WriteClass::UserInput => self.status.pending_input_units += 1,
            WriteClass::Reply => self.status.pending_reply_units += 1,
        }
    }

    fn release_unit(&mut self, class: WriteClass) {
        match class {
            WriteClass::UserInput => self.status.pending_input_units -= 1,
            WriteClass::Reply => self.status.pending_reply_units -= 1,
        }
    }

    fn reject(&mut self, count: usize, kind: io::ErrorKind) {
        let staged = self.staging;
        if staged != 0 {
            self.bytes.truncate(self.bytes.len() - staged);
            self.release_bytes(self.class, staged);
            self.release_unit(self.class);
            self.staging = 0;
        }
        if self.class == WriteClass::Reply {
            if self.rejected.is_none() {
                self.status.dropped_replies = self.status.dropped_replies.saturating_add(1);
            }
            self.status.dropped_reply_bytes = self
                .status
                .dropped_reply_bytes
                .saturating_add(staged as u64)
                .saturating_add(count as u64);
        }
        self.rejected = Some(kind);
    }

    /// Strict mobile paste uses the canonical newline implementation but
    /// refuses ESC/C1/non-text controls instead of rewriting executable bytes.
    /// Count first, reserve the complete unit, then write directly into the
    /// fixed byte ring. No complete-paste temporary allocation is needed.
    pub(super) fn paste(
        &mut self,
        text: &str,
        bracketed: bool,
        canon: NewlineCanon,
    ) -> io::Result<()> {
        if self.staging != 0 || self.rejected.is_some() || self.class != WriteClass::UserInput {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if text.len() > self.limits.maximum_paste_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "paste exceeds bounded terminal output limit",
            ));
        }
        if text.chars().any(|c| {
            (c < ' ' && !matches!(c, '\t' | '\n' | '\r')) || ('\u{7f}'..='\u{9f}').contains(&c)
        }) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "paste contains a non-text control; nothing was sent",
            ));
        }
        let canon = if bracketed { NewlineCanon::None } else { canon };
        // The raw paste limit is at most 1 MiB, and canonicalization expands
        // each raw byte by at most two. This sum cannot overflow usize.
        let mut encoded_len = if bracketed { 12 } else { 0 };
        canon.for_each_chunk(text, |chunk| encoded_len += chunk.len());
        self.available(encoded_len)?;
        if encoded_len == 0 {
            return Ok(());
        }
        self.charge_unit(self.class);
        self.charge_bytes(self.class, encoded_len);
        if bracketed {
            self.bytes.extend(b"\x1b[200~");
        }
        canon.for_each_chunk(text, |chunk| self.bytes.extend(chunk.as_bytes()));
        if bracketed {
            self.bytes.extend(b"\x1b[201~");
        }
        self.staging = encoded_len;
        self.flush()
    }

    pub(super) fn drain<F>(&mut self, maximum: usize, sink: F) -> io::Result<BoundedOutputDrain>
    where
        F: FnOnce(WriteClass, &[u8]) -> BoundedWriteResult,
    {
        if maximum == 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if self.settlement.is_some() || self.offered != 0 {
            return Ok(BoundedOutputDrain::Closed(self.close()));
        }
        let Some(unit) = self.units.front() else {
            return Ok(BoundedOutputDrain::Idle);
        };
        let class = unit.class;
        let sequence = unit.sequence;
        let (first, second) = self.bytes.as_slices();
        let contiguous = if first.is_empty() { second } else { first };
        let length = contiguous
            .len()
            .min(unit.remaining)
            .min(maximum)
            .min(self.limits.maximum_drain_bytes);
        self.offered = length;
        let result = sink(class, &contiguous[..length]);
        match result {
            BoundedWriteResult::WouldBlock | BoundedWriteResult::Accepted(0) => {
                self.offered = 0;
                Ok(BoundedOutputDrain::WouldBlock)
            }
            BoundedWriteResult::Accepted(count) if count <= length => {
                self.offered = 0;
                self.bytes.drain(..count);
                self.release_bytes(class, count);
                let unit = self.units.front_mut().expect("offered unit remains owned");
                unit.remaining -= count;
                unit.accepted += count;
                let unit_complete = unit.remaining == 0;
                if unit_complete {
                    self.units.pop_front();
                    self.release_unit(class);
                    self.status.accepted_through = sequence;
                }
                Ok(BoundedOutputDrain::Accepted {
                    sequence,
                    class,
                    bytes: count,
                    unit_complete,
                })
            }
            BoundedWriteResult::Failed | BoundedWriteResult::Accepted(_) => {
                Ok(BoundedOutputDrain::Closed(self.close()))
            }
        }
    }

    pub(super) fn copy_reply(&self, output: &mut [u8]) -> io::Result<Option<BoundedReplyCopy>> {
        if self.settlement.is_some() || self.offered != 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let Some(unit) = self.units.front() else {
            return Ok(None);
        };
        if unit.class != WriteClass::Reply || unit.accepted != 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        if output.len() < unit.remaining {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        for (destination, source) in output.iter_mut().zip(self.bytes.iter()).take(unit.remaining) {
            *destination = *source;
        }
        Ok(Some(BoundedReplyCopy {
            sequence: unit.sequence,
            bytes: unit.remaining,
        }))
    }

    pub(super) fn settle_reply(
        &mut self,
        sequence: u64,
        disposition: BoundedReplyDisposition,
    ) -> io::Result<()> {
        if self.settlement.is_some() || self.offered != 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let unit = self.units.front().ok_or(io::ErrorKind::InvalidInput)?;
        if unit.sequence != sequence || unit.class != WriteClass::Reply || unit.accepted != 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let bytes = unit.remaining;
        self.bytes.drain(..bytes);
        self.units.pop_front();
        self.release_bytes(WriteClass::Reply, bytes);
        self.release_unit(WriteClass::Reply);
        match disposition {
            BoundedReplyDisposition::Accepted => self.status.accepted_through = sequence,
            BoundedReplyDisposition::Dropped => {
                self.status.dropped_replies = self.status.dropped_replies.saturating_add(1);
                self.status.dropped_reply_bytes = self.status.dropped_reply_bytes.saturating_add(bytes as u64);
            }
        }
        Ok(())
    }

    pub(super) fn close(&mut self) -> BoundedOutputSettlement {
        if let Some(settlement) = self.settlement {
            return settlement;
        }
        let first = self.units.front();
        let mut settlement = BoundedOutputSettlement {
            accepted_through: self.status.accepted_through,
            first_remaining_sequence: first.map(|unit| unit.sequence),
            first_remaining_accepted_prefix: first.map_or(0, |unit| unit.accepted),
            definitely_not_accepted_input_bytes: self.status.pending_input_bytes,
            definitely_not_accepted_reply_bytes: self.status.pending_reply_bytes,
            ..BoundedOutputSettlement::default()
        };
        if let Some(unit) = first {
            match unit.class {
                WriteClass::UserInput => {
                    settlement.uncertain_input_bytes = self.offered;
                    settlement.definitely_not_accepted_input_bytes -= self.offered;
                }
                WriteClass::Reply => {
                    settlement.uncertain_reply_bytes = self.offered;
                    settlement.definitely_not_accepted_reply_bytes -= self.offered;
                }
            }
        }
        self.bytes.clear();
        self.units.clear();
        self.staging = 0;
        self.rejected = None;
        self.offered = 0;
        self.status.pending_input_bytes = 0;
        self.status.pending_reply_bytes = 0;
        self.status.pending_input_units = 0;
        self.status.pending_reply_units = 0;
        self.status.closed = true;
        self.settlement = Some(settlement);
        settlement
    }
}

impl Write for BoundedOutputWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let failure = self
            .rejected
            .or_else(|| self.available(bytes.len()).err().map(|e| e.kind()));
        if let Some(kind) = failure {
            self.reject(bytes.len(), kind);
            return if self.class == WriteClass::Reply && kind == io::ErrorKind::WouldBlock {
                Ok(bytes.len())
            } else {
                Err(kind.into())
            };
        }
        if self.staging == 0 {
            self.charge_unit(self.class);
        }
        self.charge_bytes(self.class, bytes.len());
        self.bytes.extend(bytes);
        self.staging += bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.input_batch {
            // Nested keyboard/mouse encoders can flush several times in one
            // logical event. Retain the complete event until its outer owner
            // commits, and retain rejection until that same boundary.
            if let Some(kind) = self.rejected {
                return Err(kind.into());
            }
            if self.settlement.is_some() || self.offered != 0 {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            return Ok(());
        }
        if let Some(kind) = self.rejected.take() {
            return if self.class == WriteClass::Reply && kind == io::ErrorKind::WouldBlock {
                Ok(())
            } else {
                Err(kind.into())
            };
        }
        if self.settlement.is_some() || self.offered != 0 {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        if self.staging != 0 {
            self.status.admitted_through += 1;
            self.units.push_back(Unit {
                sequence: self.status.admitted_through,
                class: self.class,
                remaining: self.staging,
                accepted: 0,
            });
            self.staging = 0;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::color::ColorPalette;
    use crate::{
        KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind, Terminal,
        TerminalConfiguration, TerminalSize,
    };
    use std::sync::Arc;

    fn limits() -> BoundedOutputLimits {
        BoundedOutputLimits {
            input_bytes: 32,
            reply_bytes: 8,
            input_units: 4,
            reply_units: 2,
            maximum_unit_bytes: 32,
            maximum_paste_bytes: 20,
            maximum_drain_bytes: 8,
        }
    }

    fn writer() -> BoundedOutputWriter {
        BoundedOutputWriter::new(limits()).unwrap()
    }

    fn input(writer: &mut BoundedOutputWriter, bytes: &[u8]) -> io::Result<()> {
        writer.set_class(WriteClass::UserInput);
        let result = writer.write_all(bytes);
        let flush = writer.flush();
        writer.set_class(WriteClass::Reply);
        result.and(flush)
    }

    fn collect(writer: &mut BoundedOutputWriter) -> Vec<u8> {
        let mut output = Vec::new();
        loop {
            let result = writer
                .drain(4096, |_, bytes| {
                    output.extend_from_slice(bytes);
                    BoundedWriteResult::Accepted(bytes.len())
                })
                .unwrap();
            if result == BoundedOutputDrain::Idle {
                break;
            }
            assert!(matches!(result, BoundedOutputDrain::Accepted { .. }));
        }
        output
    }

    #[test]
    fn whole_reply_copy_keeps_reservations_across_ring_wrap_and_exact_settlement() {
        let mut configured = limits();
        configured.reply_bytes = 32;
        configured.reply_units = 4;
        let mut writer = BoundedOutputWriter::new(configured).unwrap();
        for _ in 0..2 {
            writer.write_all(b"abc").unwrap();
            writer.flush().unwrap();
        }
        let mut wrapped = false;
        // Keep two units resident so the ring's head advances without an
        // empty-ring normalization hiding the fragmented-unit case.
        for _ in 0..=writer.bytes.capacity() {
            let before = writer.status();
            let mut refused = [0xa5; 2];
            assert_eq!(writer.copy_reply(&mut refused).unwrap_err().kind(), io::ErrorKind::WouldBlock);
            assert_eq!(refused, [0xa5; 2]);
            let mut copied = [0; 3];
            let reply = writer.copy_reply(&mut copied).unwrap().unwrap();
            wrapped |= writer.bytes.as_slices().0.len() < reply.bytes;
            assert_eq!(&copied, b"abc");
            assert_eq!(writer.status(), before);
            assert_eq!(writer.settle_reply(reply.sequence + 1, BoundedReplyDisposition::Accepted).unwrap_err().kind(), io::ErrorKind::InvalidInput);
            assert_eq!(writer.status(), before);
            writer.settle_reply(reply.sequence, BoundedReplyDisposition::Accepted).unwrap();
            assert_eq!(writer.settle_reply(reply.sequence, BoundedReplyDisposition::Accepted).unwrap_err().kind(), io::ErrorKind::InvalidInput);
            writer.write_all(b"abc").unwrap();
            writer.flush().unwrap();
        }
        assert!(wrapped, "a complete reply must be copied across both ring slices");
        let reply = writer.copy_reply(&mut [0; 3]).unwrap().unwrap();
        writer.settle_reply(reply.sequence, BoundedReplyDisposition::Dropped).unwrap();
        assert_eq!(writer.status().dropped_replies, 1);
        assert_eq!(writer.status().dropped_reply_bytes, 3);
        assert_eq!(writer.status().pending_reply_bytes, 3);
    }

    #[test]
    fn whole_reply_protocol_refuses_a_previously_partially_drained_unit() {
        let mut writer = writer();
        writer.write_all(b"reply").unwrap();
        writer.flush().unwrap();
        let sequence = writer.status().admitted_through;
        writer.drain(2, |_, _| BoundedWriteResult::Accepted(2)).unwrap();
        let before = writer.status();
        let mut output = [0xa5; 8];
        assert_eq!(writer.copy_reply(&mut output).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(output, [0xa5; 8]);
        assert_eq!(writer.settle_reply(sequence, BoundedReplyDisposition::Dropped).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(writer.status(), before);
        assert_eq!(collect(&mut writer), b"ply");
    }

    #[test]
    fn partial_acceptance_and_would_block_keep_exact_reservations_and_order() {
        let mut writer = writer();
        input(&mut writer, b"abcdefgh").unwrap();
        writer.write_all(b"reply").unwrap();
        writer.flush().unwrap();
        input(&mut writer, b"tail").unwrap();
        let before = writer.status();
        assert_eq!(
            writer.drain(3, |class, bytes| {
                assert_eq!(class, WriteClass::UserInput);
                assert_eq!(bytes, b"abc");
                BoundedWriteResult::WouldBlock
            }).unwrap(),
            BoundedOutputDrain::WouldBlock
        );
        assert_eq!(writer.status(), before);
        assert_eq!(
            writer.drain(3, |_, bytes| {
                assert_eq!(bytes, b"abc");
                BoundedWriteResult::Accepted(2)
            }).unwrap(),
            BoundedOutputDrain::Accepted {
                sequence: 1,
                class: WriteClass::UserInput,
                bytes: 2,
                unit_complete: false,
            }
        );
        assert_eq!(writer.status().pending_input_bytes, 10);
        assert_eq!(writer.status().pending_input_units, 2);
        assert_eq!(writer.status().accepted_through, 0);
        assert_eq!(collect(&mut writer), b"cdefghreplytail");
        assert_eq!(writer.status().accepted_through, 3);
        assert_eq!(writer.status().pending_input_bytes, 0);
        assert_eq!(writer.status().pending_reply_bytes, 0);
    }

    #[test]
    fn record_and_byte_bounds_refuse_whole_input_without_late_prefixes() {
        let mut writer = BoundedOutputWriter::new(BoundedOutputLimits {
            input_units: 1,
            ..limits()
        }).unwrap();
        input(&mut writer, b"held").unwrap();
        assert_eq!(input(&mut writer, b"later").unwrap_err().kind(), io::ErrorKind::WouldBlock);
        writer.drain(1, |_, _| BoundedWriteResult::Accepted(1)).unwrap();
        assert_eq!(input(&mut writer, b"later").unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(collect(&mut writer), b"eld");
        input(&mut writer, &[b'x'; 32]).unwrap();
        assert_eq!(input(&mut writer, b"y").unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(collect(&mut writer), [b'x'; 32]);
        input(&mut writer, b"ok").unwrap();
        assert_eq!(collect(&mut writer), b"ok");
    }

    #[test]
    fn overflowing_formatted_reply_is_dropped_whole_and_input_reserve_survives() {
        let mut writer = writer();
        writer.write_all(b"1234567").unwrap();
        writer.write_all(b"89").unwrap();
        writer.write_all(b"end").unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.status().dropped_replies, 1);
        assert_eq!(writer.status().dropped_reply_bytes, 12);
        assert_eq!(writer.status().pending_reply_units, 0);
        writer.write_all(b"12345678").unwrap();
        writer.flush().unwrap();
        writer.write_all(b"discard").unwrap();
        writer.flush().unwrap();
        input(&mut writer, b"user").unwrap();
        assert_eq!(collect(&mut writer), b"12345678user");
        assert_eq!(writer.status().dropped_replies, 2);
    }

    #[test]
    fn fragmented_user_unit_rolls_back_on_capacity_failure() {
        let mut writer = writer();
        writer.set_class(WriteClass::UserInput);
        writer.write_all(&[b'a'; 30]).unwrap();
        assert_eq!(writer.write_all(b"too much").unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(writer.flush().unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(writer.status().pending_input_bytes, 0);
        assert_eq!(writer.status().pending_input_units, 0);
        assert_eq!(collect(&mut writer), b"");
        input(&mut writer, b"next").unwrap();
        assert_eq!(collect(&mut writer), b"next");
    }

    #[test]
    fn failed_sink_distinguishes_accepted_prefix_uncertain_tail_and_unoffered_units() {
        let mut writer = writer();
        input(&mut writer, b"abcdef").unwrap();
        input(&mut writer, b"later").unwrap();
        writer.write_all(b"reply").unwrap();
        writer.flush().unwrap();
        writer.drain(2, |_, _| BoundedWriteResult::Accepted(2)).unwrap();
        let BoundedOutputDrain::Closed(settlement) = writer
            .drain(3, |_, bytes| {
                assert_eq!(bytes, b"cde");
                BoundedWriteResult::Failed
            }).unwrap() else { panic!("failed sink must close"); };
        assert_eq!(settlement.first_remaining_sequence, Some(1));
        assert_eq!(settlement.first_remaining_accepted_prefix, 2);
        assert_eq!(settlement.accepted_through, 0);
        assert_eq!(settlement.uncertain_input_bytes, 3);
        assert_eq!(settlement.definitely_not_accepted_input_bytes, 6);
        assert_eq!(settlement.definitely_not_accepted_reply_bytes, 5);
        assert_eq!(writer.close(), settlement);
        assert_eq!(input(&mut writer, b"never").unwrap_err().kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(writer.drain(1, |_, _| panic!("closed sink must not run")).unwrap(),
                   BoundedOutputDrain::Closed(settlement));
    }

    #[test]
    fn invalid_acceptance_count_and_panicking_sink_cannot_replay_input() {
        let mut invalid = writer();
        input(&mut invalid, b"abcdef").unwrap();
        let BoundedOutputDrain::Closed(settlement) = invalid
            .drain(2, |_, _| BoundedWriteResult::Accepted(3)).unwrap()
            else { panic!("invalid count must close"); };
        assert_eq!(settlement.uncertain_input_bytes, 2);
        assert_eq!(settlement.definitely_not_accepted_input_bytes, 4);

        let mut panicked = writer();
        input(&mut panicked, b"abcdef").unwrap();
        // Tests inspect the unwind; production recovery belongs to the
        // embedding boundary's canonical recoverable-panic contract.
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = panicked.drain(3, |_, _| panic!("sink panic"));
        })).is_err());
        let BoundedOutputDrain::Closed(settlement) = panicked
            .drain(3, |_, _| panic!("uncertain prefix must never be replayed")).unwrap()
            else { panic!("panicked sink must close"); };
        assert_eq!(settlement.uncertain_input_bytes, 3);
        assert_eq!(settlement.definitely_not_accepted_input_bytes, 3);
    }

    #[test]
    fn fixed_rings_wrap_without_growing_and_empty_flushes_use_no_records() {
        let mut writer = writer();
        let capacity = (writer.bytes.capacity(), writer.units.capacity());
        input(&mut writer, b"hold").unwrap();
        for _ in 0..200 {
            for _ in 0..20 { writer.flush().unwrap(); }
            input(&mut writer, b"a wrapped ring payload").unwrap();
            let mut prefix = Vec::new();
            while prefix.len() < 4 {
                writer.drain(4 - prefix.len(), |_, bytes| {
                    prefix.extend_from_slice(bytes);
                    BoundedWriteResult::Accepted(bytes.len())
                }).unwrap();
            }
            assert_eq!(prefix, b"hold");
            input(&mut writer, b"hold").unwrap();
            let mut payload = Vec::new();
            while payload.len() < 22 {
                writer.drain(22 - payload.len(), |_, bytes| {
                    payload.extend_from_slice(bytes);
                    BoundedWriteResult::Accepted(bytes.len())
                }).unwrap();
            }
            assert_eq!(payload, b"a wrapped ring payload");
            // The ring never becomes empty during this loop, so its front
            // and tail must wrap instead of being reset between iterations.
            assert_eq!(writer.status().pending_input_bytes, 4);
            assert_eq!(writer.status().pending_input_units, 1);
            assert_eq!((writer.bytes.capacity(), writer.units.capacity()), capacity);
        }
        assert_eq!(collect(&mut writer), b"hold");
        assert_eq!(writer.status().admitted_through, 401);
        assert_eq!(writer.status().pending_input_units, 0);
        assert_eq!(writer.status().pending_reply_units, 0);
    }

    #[test]
    fn paste_reserves_wrappers_and_expanded_newlines_before_writing() {
        let mut writer = writer();
        input(&mut writer, &[b'x'; 21]).unwrap();
        writer.set_class(WriteClass::UserInput);
        assert_eq!(writer.paste("ok", true, NewlineCanon::None).unwrap_err().kind(),
                   io::ErrorKind::WouldBlock);
        assert_eq!(collect(&mut writer), [b'x'; 21]);
        writer.paste("界\r\nx\n", false, NewlineCanon::CarriageReturnAndLineFeed).unwrap();
        assert_eq!(collect(&mut writer), "界\r\nx\r\n".as_bytes());
        writer.paste("界\r\nx\n", true, NewlineCanon::CarriageReturnAndLineFeed).unwrap();
        assert_eq!(collect(&mut writer), "\x1b[200~界\r\nx\n\x1b[201~".as_bytes());
        for text in ["x\x1b[201~bad", "x\u{9b}bad", "x\u{7f}bad", "x\0bad"] {
            assert_eq!(writer.paste(text, true, NewlineCanon::None).unwrap_err().kind(),
                       io::ErrorKind::InvalidInput);
            assert_eq!(collect(&mut writer), b"");
        }
        assert_eq!(writer.paste(&"a".repeat(21), true, NewlineCanon::None).unwrap_err().kind(),
                   io::ErrorKind::InvalidInput);
        assert_eq!(collect(&mut writer), b"");
    }

    #[test]
    fn invalid_limits_and_sequence_exhaustion_fail_without_admission() {
        assert!(BoundedOutputWriter::new(BoundedOutputLimits { input_bytes: usize::MAX, ..limits() }).is_err());
        assert!(BoundedOutputWriter::new(BoundedOutputLimits { maximum_drain_bytes: 0, ..limits() }).is_err());
        let mut writer = writer();
        writer.status.admitted_through = u64::MAX;
        assert_eq!(input(&mut writer, b"x").unwrap_err().kind(), io::ErrorKind::InvalidData);
        assert_eq!(writer.status().pending_input_bytes, 0);
        assert_eq!(collect(&mut writer), b"");
    }

    #[derive(Debug)]
    struct Config;
    impl TerminalConfiguration for Config {
        fn scrollback_size(&self) -> usize { 8 }
        fn color_palette(&self) -> ColorPalette { ColorPalette::default() }
        fn canonicalize_pasted_newlines(&self) -> NewlineCanon { NewlineCanon::CarriageReturn }
    }

    fn terminal(limits: BoundedOutputLimits) -> Terminal {
        Terminal::new_bounded_output(
            TerminalSize { rows: 4, cols: 20, pixel_width: 160, pixel_height: 64, dpi: 96 },
            Arc::new(Config), "FrankenTerm", "test", limits,
        ).unwrap()
    }

    fn collect_terminal(terminal: &mut Terminal) -> Vec<u8> {
        let mut output = Vec::new();
        loop {
            let result = terminal.drain_bounded_output(4096, |_, bytes| {
                output.extend_from_slice(bytes);
                BoundedWriteResult::Accepted(bytes.len())
            }).unwrap();
            if result == BoundedOutputDrain::Idle { break; }
            assert!(matches!(result, BoundedOutputDrain::Accepted { .. }));
        }
        output
    }

    #[test]
    fn canonical_parser_and_keyboard_share_the_caller_driven_ring() {
        let mut terminal = terminal(limits());
        assert_eq!(terminal.writer.capacity(), 0);
        terminal.advance_bytes(b"hello\x1b[5n\x1b[?1h");
        terminal.key_down(KeyCode::UpArrow, KeyModifiers::NONE).unwrap();
        terminal.send_paste("a\r\nb\n").unwrap();
        assert_eq!(collect_terminal(&mut terminal), b"\x1b[0n\x1bOAa\rb\r");
        assert_eq!(terminal.bounded_output_status().unwrap().accepted_through, 3);
        assert_eq!(terminal.writer_barrier().wait(std::time::Duration::ZERO).unwrap_err().kind(),
                   io::ErrorKind::Unsupported);
    }

    #[test]
    fn canonical_reply_flood_stays_bounded_and_never_starves_user_input() {
        let mut terminal = terminal(limits());
        for _ in 0..100 { terminal.advance_bytes(b"\x1b[5n"); }
        terminal.key_down(KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        let status = terminal.bounded_output_status().unwrap();
        assert_eq!(status.pending_reply_bytes, 8);
        assert_eq!(status.pending_reply_units, 2);
        assert_eq!(status.dropped_replies, 98);
        assert_eq!(collect_terminal(&mut terminal), b"\x1b[0n\x1b[0nx");
    }

    #[test]
    fn canonical_bracketed_paste_refusal_never_emits_a_prefix_or_stale_tail() {
        let mut terminal = terminal(limits());
        terminal.advance_bytes(b"\x1b[?2004h");
        terminal.write_user_input(&[b'x'; 21]).unwrap();
        assert!(terminal.send_paste("ok").is_err());
        assert_eq!(collect_terminal(&mut terminal), [b'x'; 21]);
        terminal.send_paste("ok").unwrap();
        terminal.key_down(KeyCode::Char('!'), KeyModifiers::NONE).unwrap();
        assert_eq!(collect_terminal(&mut terminal), b"\x1b[200~ok\x1b[201~!");
        terminal.close_bounded_output().unwrap();
        assert!(terminal.send_paste("never").is_err());
        assert!(terminal.key_down(KeyCode::Char('x'), KeyModifiers::NONE).is_err());
    }

    fn mouse(kind: MouseEventKind, button: MouseButton) -> MouseEvent {
        MouseEvent {
            kind,
            button,
            modifiers: KeyModifiers::NONE,
            x: 1,
            y: 1,
            x_pixel_offset: 0,
            y_pixel_offset: 0,
        }
    }

    #[test]
    fn a_mouse_wheel_event_is_atomic_across_nested_key_flushes() {
        let mut terminal = terminal(BoundedOutputLimits {
            input_bytes: 8,
            maximum_unit_bytes: 8,
            maximum_paste_bytes: 8,
            ..limits()
        });
        terminal.advance_bytes(b"\x1b[?1049h");
        // Default wheel speed emits three cursor keys (nine bytes). Earlier
        // flushes must not publish a prefix before the third key is refused.
        assert!(terminal.mouse_event(mouse(MouseEventKind::Press, MouseButton::WheelUp(1))).is_err());
        assert_eq!(collect_terminal(&mut terminal), b"");
        assert_eq!(terminal.bounded_output_status().unwrap().admitted_through, 0);
        terminal.key_down(KeyCode::Char('x'), KeyModifiers::NONE).unwrap();
        assert_eq!(collect_terminal(&mut terminal), b"x");
    }

    #[test]
    fn refused_mouse_press_release_and_move_preserve_retry_state() {
        let mut terminal = terminal(limits());
        terminal.advance_bytes(b"\x1b[?1003h\x1b[?1006h");
        terminal.write_user_input(&[b'x'; 31]).unwrap();
        assert!(terminal.mouse_event(mouse(MouseEventKind::Press, MouseButton::Left)).is_err());
        assert!(terminal.current_mouse_buttons.is_empty());
        collect_terminal(&mut terminal);
        terminal.mouse_event(mouse(MouseEventKind::Press, MouseButton::Left)).unwrap();
        assert_eq!(collect_terminal(&mut terminal), b"\x1b[<0;2;2M");

        terminal.write_user_input(&[b'x'; 31]).unwrap();
        assert!(terminal.mouse_event(mouse(MouseEventKind::Release, MouseButton::Left)).is_err());
        assert_eq!(terminal.current_mouse_buttons, vec![MouseButton::Left]);
        collect_terminal(&mut terminal);
        terminal.mouse_event(mouse(MouseEventKind::Release, MouseButton::Left)).unwrap();
        assert_eq!(collect_terminal(&mut terminal), b"\x1b[<0;2;2m");
        assert!(terminal.current_mouse_buttons.is_empty());

        terminal.write_user_input(&[b'x'; 31]).unwrap();
        assert!(terminal.mouse_event(mouse(MouseEventKind::Move, MouseButton::None)).is_err());
        assert!(terminal.last_mouse_move.is_none());
        collect_terminal(&mut terminal);
        terminal.mouse_event(mouse(MouseEventKind::Move, MouseButton::None)).unwrap();
        assert_eq!(collect_terminal(&mut terminal), b"\x1b[<35;2;2M");
    }

    #[test]
    fn focus_cleanup_and_report_commit_together_or_preserve_source_state() {
        let mut terminal = terminal(limits());
        terminal.advance_bytes(b"\x1b[?1000h\x1b[?1006h\x1b[?1004h");
        terminal.try_bounded_focus_change(true).unwrap();
        collect_terminal(&mut terminal);
        terminal.mouse_event(mouse(MouseEventKind::Press, MouseButton::Left)).unwrap();
        collect_terminal(&mut terminal);
        terminal.write_user_input(&[b'x'; 21]).unwrap();
        // The release report fits alone, but release plus focus-out does not.
        assert!(terminal.try_bounded_focus_change(false).is_err());
        assert!(terminal.focused);
        assert_eq!(terminal.current_mouse_buttons, vec![MouseButton::Left]);
        assert_eq!(collect_terminal(&mut terminal), [b'x'; 21]);
        terminal.try_bounded_focus_change(false).unwrap();
        assert!(!terminal.focused);
        assert!(terminal.current_mouse_buttons.is_empty());
        assert_eq!(collect_terminal(&mut terminal), b"\x1b[<0;1;1m\x1b[O");
    }

    #[test]
    fn input_transaction_reports_final_rejection_and_rolls_back_explicit_errors() {
        let mut terminal = terminal(limits());
        let result = terminal.with_user_input(|term| -> io::Result<()> {
            term.writer.write_all(&[b'x'; 33]).ok();
            Ok(())
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(collect_terminal(&mut terminal), b"");
        let result = terminal.with_user_input(|term| -> io::Result<()> {
            term.write_user_input(b"must roll back")?;
            Err(io::ErrorKind::InvalidInput.into())
        });
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(collect_terminal(&mut terminal), b"");
    }
}
