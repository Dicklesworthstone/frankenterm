//! Write-ahead log engine for continuous zero-cost state persistence.
//!
//! Provides an append-only log with monotonic sequence numbers, O(1) checkpoint
//! markers, background compaction, and deterministic replay. State mutations are
//! recorded as structured entries; "snapshots" reduce to placing a checkpoint
//! marker (zero cost) rather than serializing full state.
//!
//! # Design
//!
//! ```text
//! Mutations ──→ WAL (append-only) ──→ [Checkpoint│Entry│Entry│Checkpoint│...]
//!                    │
//!       ┌────────────┼────────────┐
//!       │            │            │
//!  Checkpoint    Compaction    Replay
//!  O(1) cost    (background)  (restore)
//! ```
//!
//! # Use cases in FrankenTerm
//!
//! - **Crash recovery**: Replay WAL entries since last checkpoint to restore state.
//! - **Continuous persistence**: Every state change is logged; no periodic "save".
//! - **Audit trail**: WAL entries form an immutable record of all mutations.
//! - **Time-travel**: Replay entries up to a specific sequence to reconstruct
//!   historical state.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

// ── WAL Entry ────────────────────────────────────────────────────

/// A single entry in the write-ahead log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalEntry<T: Clone> {
    /// Monotonically increasing sequence number.
    pub seq: u64,
    /// Timestamp in milliseconds since epoch.
    pub timestamp_ms: u64,
    /// The payload.
    pub kind: EntryKind<T>,
}

/// The kind of WAL entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind<T: Clone> {
    /// A state mutation record.
    Mutation(T),
    /// A checkpoint marker (zero-cost snapshot point).
    Checkpoint,
    /// A compaction marker indicating entries before this seq were compacted.
    CompactionMarker { compacted_through: u64 },
}

// ── Telemetry ───────────────────────────────────────────────────

/// Operational telemetry counters for the WAL engine.
///
/// Uses plain `u64` because `WalEngine` methods take `&mut self`.
#[derive(Debug, Clone, Default)]
pub struct WalTelemetry {
    /// Total mutation entries appended.
    appends: u64,
    /// Total checkpoint markers placed.
    checkpoints: u64,
    /// Total compaction runs.
    compactions: u64,
    /// Total entries removed by compaction.
    entries_compacted: u64,
    /// Total truncate operations.
    truncations: u64,
    /// Total entries removed by truncation.
    entries_truncated: u64,
}

impl WalTelemetry {
    /// Create a new telemetry instance with all counters at zero.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot the current counter values.
    #[must_use]
    pub fn snapshot(&self) -> WalTelemetrySnapshot {
        WalTelemetrySnapshot {
            appends: self.appends,
            checkpoints: self.checkpoints,
            compactions: self.compactions,
            entries_compacted: self.entries_compacted,
            truncations: self.truncations,
            entries_truncated: self.entries_truncated,
        }
    }
}

/// Serializable snapshot of WAL engine telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalTelemetrySnapshot {
    /// Total mutation entries appended.
    pub appends: u64,
    /// Total checkpoint markers placed.
    pub checkpoints: u64,
    /// Total compaction runs.
    pub compactions: u64,
    /// Total entries removed by compaction.
    pub entries_compacted: u64,
    /// Total truncate operations.
    pub truncations: u64,
    /// Total entries removed by truncation.
    pub entries_truncated: u64,
}

// ── WAL Engine ───────────────────────────────────────────────────

/// Configuration for the WAL engine.
#[derive(Debug, Clone)]
pub struct WalConfig {
    /// Maximum number of entries before suggesting compaction.
    pub compaction_threshold: usize,
    /// Maximum entries to retain after compaction (keeps most recent).
    pub max_retained_entries: usize,
}

impl Default for WalConfig {
    fn default() -> Self {
        Self {
            compaction_threshold: 10_000,
            max_retained_entries: 1_000,
        }
    }
}

/// An in-memory write-ahead log engine.
///
/// Entries are stored in a `VecDeque` for efficient append and front-removal
/// during compaction. Sequence numbers are globally unique and monotonically
/// increasing.
#[derive(Debug, Clone)]
pub struct WalEngine<T: Clone> {
    /// The log entries.
    entries: VecDeque<WalEntry<T>>,
    /// Next sequence number to assign.
    next_seq: u64,
    /// Sequence of the last checkpoint.
    last_checkpoint_seq: Option<u64>,
    /// Sequence through which compaction has occurred.
    compacted_through: u64,
    /// Configuration.
    config: WalConfig,
    /// Operational telemetry counters.
    telemetry: WalTelemetry,
}

impl<T: Clone> WalEngine<T> {
    /// Create a new empty WAL engine.
    pub fn new(config: WalConfig) -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 1,
            last_checkpoint_seq: None,
            compacted_through: 0,
            config,
            telemetry: WalTelemetry::new(),
        }
    }

    /// Append a mutation entry to the log.
    ///
    /// Returns the assigned sequence number.
    pub fn append(&mut self, mutation: T, timestamp_ms: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(WalEntry {
            seq,
            timestamp_ms,
            kind: EntryKind::Mutation(mutation),
        });
        self.telemetry.appends += 1;
        seq
    }

    /// Place a checkpoint marker (O(1) "snapshot").
    ///
    /// Returns the checkpoint's sequence number.
    pub fn checkpoint(&mut self, timestamp_ms: u64) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(WalEntry {
            seq,
            timestamp_ms,
            kind: EntryKind::Checkpoint,
        });
        self.last_checkpoint_seq = Some(seq);
        self.telemetry.checkpoints += 1;
        seq
    }

    /// Get the sequence number of the last checkpoint.
    pub fn last_checkpoint(&self) -> Option<u64> {
        self.last_checkpoint_seq
    }

    /// Get the next sequence number that will be assigned.
    pub fn next_seq(&self) -> u64 {
        self.next_seq
    }

    /// Total number of entries in the log (including checkpoints).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Get the sequence number through which compaction has occurred.
    pub fn compacted_through(&self) -> u64 {
        self.compacted_through
    }

    /// Check if compaction is recommended based on configuration.
    pub fn needs_compaction(&self) -> bool {
        self.entries.len() > self.config.compaction_threshold
    }

    /// Get an entry by sequence number.
    pub fn get(&self, seq: u64) -> Option<&WalEntry<T>> {
        if seq == 0 || seq < self.first_seq() {
            return None;
        }
        // Binary search since entries are sorted by seq
        self.entries.iter().find(|e| e.seq == seq)
    }

    /// Get the first (oldest) sequence number in the log.
    pub fn first_seq(&self) -> u64 {
        self.entries.front().map(|e| e.seq).unwrap_or(self.next_seq)
    }

    /// Get the last (newest) sequence number in the log.
    pub fn last_seq(&self) -> u64 {
        self.entries.back().map(|e| e.seq).unwrap_or(0)
    }

    /// Iterate over all entries.
    pub fn iter(&self) -> impl Iterator<Item = &WalEntry<T>> {
        self.entries.iter()
    }

    /// Iterate over entries since (exclusive) a given sequence number.
    pub fn since(&self, seq: u64) -> impl Iterator<Item = &WalEntry<T>> {
        self.entries.iter().filter(move |e| e.seq > seq)
    }

    /// Iterate over mutation entries only (skipping checkpoints).
    pub fn mutations(&self) -> impl Iterator<Item = &WalEntry<T>> {
        self.entries
            .iter()
            .filter(|e| matches!(e.kind, EntryKind::Mutation(_)))
    }

    /// Iterate over entries between two sequence numbers (inclusive).
    pub fn range(&self, from_seq: u64, to_seq: u64) -> impl Iterator<Item = &WalEntry<T>> {
        self.entries
            .iter()
            .filter(move |e| e.seq >= from_seq && e.seq <= to_seq)
    }

    /// Get entries since the last checkpoint.
    ///
    /// If no checkpoint exists, returns all entries.
    pub fn since_last_checkpoint(&self) -> impl Iterator<Item = &WalEntry<T>> {
        let cp_seq = self.last_checkpoint_seq.unwrap_or(0);
        self.entries.iter().filter(move |e| e.seq > cp_seq)
    }

    /// Compact the log by removing old entries.
    ///
    /// Retains entries after the most recent checkpoint (or the last
    /// `max_retained_entries` if no checkpoint exists). Returns the
    /// number of entries removed.
    pub fn compact(&mut self) -> usize {
        let retain_after = if let Some(cp_seq) = self.last_checkpoint_seq {
            // Keep entries after the last checkpoint
            cp_seq
        } else {
            // No checkpoint — keep last max_retained_entries
            let total = self.entries.len();
            if total <= self.config.max_retained_entries {
                return 0;
            }
            let keep_from_idx = total - self.config.max_retained_entries;
            self.entries
                .get(keep_from_idx)
                .map(|e| e.seq.saturating_sub(1))
                .unwrap_or(0)
        };

        let mut removed = 0;
        while let Some(front) = self.entries.front() {
            if front.seq <= retain_after && !matches!(front.kind, EntryKind::Checkpoint) {
                self.entries.pop_front();
                removed += 1;
            } else if front.seq < retain_after && matches!(front.kind, EntryKind::Checkpoint) {
                // Remove old checkpoints too (except the retain_after one)
                self.entries.pop_front();
                removed += 1;
            } else {
                break;
            }
        }

        if removed > 0 {
            self.compacted_through = retain_after;
            // Add compaction marker
            let seq = self.next_seq;
            self.next_seq += 1;
            self.entries.push_back(WalEntry {
                seq,
                timestamp_ms: 0,
                kind: EntryKind::CompactionMarker {
                    compacted_through: retain_after,
                },
            });
            self.telemetry.compactions += 1;
            self.telemetry.entries_compacted += removed as u64;
        }

        removed
    }

    /// Truncate the log to entries up to (inclusive) a given sequence number.
    ///
    /// Useful for "rewinding" to a known good state.
    pub fn truncate_after(&mut self, seq: u64) {
        let before_len = self.entries.len();
        while let Some(back) = self.entries.back() {
            if back.seq > seq {
                self.entries.pop_back();
            } else {
                break;
            }
        }
        let removed = before_len - self.entries.len();
        if removed > 0 {
            self.telemetry.truncations += 1;
            self.telemetry.entries_truncated += removed as u64;
        }
        self.next_seq = seq + 1;
        // Update last_checkpoint if it was truncated
        if let Some(cp) = self.last_checkpoint_seq {
            if cp > seq {
                self.last_checkpoint_seq = self
                    .entries
                    .iter()
                    .rev()
                    .find(|e| matches!(e.kind, EntryKind::Checkpoint))
                    .map(|e| e.seq);
            }
        }
    }

    /// Clear all entries. Resets the engine to empty state.
    ///
    /// Preserves the current sequence counter (entries are never reused).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.last_checkpoint_seq = None;
    }

    /// Access telemetry counters.
    #[must_use]
    pub fn telemetry(&self) -> &WalTelemetry {
        &self.telemetry
    }
}

// ── Replay support ───────────────────────────────────────────────

/// A replay cursor for replaying WAL entries.
///
/// Iterates over mutation entries in order, allowing the caller
/// to reconstruct state by applying each mutation.
pub struct ReplayCursor<'a, T: Clone> {
    entries: std::collections::vec_deque::Iter<'a, WalEntry<T>>,
    from_seq: u64,
    to_seq: u64,
}

impl<'a, T: Clone> ReplayCursor<'a, T> {
    /// Create a replay cursor over a range of sequences.
    pub fn new(wal: &'a WalEngine<T>, from_seq: u64, to_seq: u64) -> Self {
        Self {
            entries: wal.entries.iter(),
            from_seq,
            to_seq,
        }
    }
}

impl<'a, T: Clone> Iterator for ReplayCursor<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = self.entries.next()?;
            if entry.seq < self.from_seq {
                continue;
            }
            if entry.seq > self.to_seq {
                return None;
            }
            if let EntryKind::Mutation(ref data) = entry.kind {
                return Some(data);
            }
        }
    }
}

/// Replay mutations from a WAL engine within a sequence range.
///
/// Returns only the mutation payloads (not checkpoints or markers).
pub fn replay_mutations<T: Clone>(wal: &WalEngine<T>, from_seq: u64, to_seq: u64) -> Vec<&T> {
    ReplayCursor::new(wal, from_seq, to_seq).collect()
}

// ── WAL Statistics ───────────────────────────────────────────────

/// Statistics about the WAL engine state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WalStats {
    /// Total entries in the log.
    pub total_entries: usize,
    /// Number of mutation entries.
    pub mutation_count: usize,
    /// Number of checkpoint entries.
    pub checkpoint_count: usize,
    /// First sequence number.
    pub first_seq: u64,
    /// Last sequence number.
    pub last_seq: u64,
    /// Sequence of last checkpoint.
    pub last_checkpoint_seq: Option<u64>,
    /// Compacted through sequence.
    pub compacted_through: u64,
    /// Whether compaction is recommended.
    pub needs_compaction: bool,
}

impl<T: Clone> WalEngine<T> {
    /// Get statistics about the current WAL state.
    pub fn stats(&self) -> WalStats {
        WalStats {
            total_entries: self.len(),
            mutation_count: self.mutations().count(),
            checkpoint_count: self
                .entries
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::Checkpoint))
                .count(),
            first_seq: self.first_seq(),
            last_seq: self.last_seq(),
            last_checkpoint_seq: self.last_checkpoint_seq,
            compacted_through: self.compacted_through,
            needs_compaction: self.needs_compaction(),
        }
    }
}

// ── CRC32 Integrity ─────────────────────────────────────────────

/// Compute CRC32 checksum over a byte slice.
///
/// Uses the CRC-32/ISO-HDLC polynomial (same as zlib/gzip).
fn crc32_compute(data: &[u8]) -> u32 {
    // CRC-32/ISO-HDLC lookup table (precomputed for polynomial 0xEDB8_8320)
    const TABLE: [u32; 256] = {
        let mut table = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut crc = i as u32;
            let mut j = 0;
            while j < 8 {
                if crc & 1 != 0 {
                    crc = (crc >> 1) ^ 0xEDB8_8320;
                } else {
                    crc >>= 1;
                }
                j += 1;
            }
            table[i] = crc;
            i += 1;
        }
        table
    };

    let mut crc = !0u32;
    for &byte in data {
        let idx = ((crc ^ u32::from(byte)) & 0xFF) as usize;
        crc = (crc >> 8) ^ TABLE[idx];
    }
    !crc
}

// ── Disk-Backed WAL Frame ───────────────────────────────────────

/// A framed WAL record on disk.
///
/// Wire format (big-endian):
/// ```text
/// [4 bytes: payload_len] [payload_len bytes: JSON payload] [4 bytes: crc32]
/// ```
///
/// CRC32 covers the entire `[payload_len ++ payload]` region.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WalFrame {
    payload: Vec<u8>,
    crc32: u32,
}

impl WalFrame {
    /// Create a new frame from a JSON-serializable entry.
    fn from_entry<T: Clone + Serialize>(entry: &WalEntry<T>) -> Result<Self, io::Error> {
        let payload =
            serde_json::to_vec(entry).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if payload.len() > u32::MAX as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("WAL entry payload too large: {} bytes", payload.len()),
            ));
        }
        let mut crc_input = Vec::with_capacity(4 + payload.len());
        crc_input.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        crc_input.extend_from_slice(&payload);
        let crc32 = crc32_compute(&crc_input);
        Ok(Self { payload, crc32 })
    }

    /// Write frame to a writer.
    fn write_to<W: Write>(&self, w: &mut W) -> Result<(), io::Error> {
        let len = self.payload.len() as u32;
        w.write_all(&len.to_be_bytes())?;
        w.write_all(&self.payload)?;
        w.write_all(&self.crc32.to_be_bytes())?;
        Ok(())
    }

    /// Read a frame from a reader. Returns `None` at clean EOF.
    fn read_from<R: BufRead>(r: &mut R) -> Result<Option<Self>, WalCorruptionError> {
        // Read length prefix
        let mut len_buf = [0u8; 4];
        match r.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(WalCorruptionError::Io(e)),
        }
        let payload_len = u32::from_be_bytes(len_buf) as usize;

        // Sanity check: reject absurdly large payloads (>64 MiB).
        if payload_len > 64 * 1024 * 1024 {
            return Err(WalCorruptionError::PayloadTooLarge(payload_len));
        }

        // Read payload
        let mut payload = vec![0u8; payload_len];
        r.read_exact(&mut payload)
            .map_err(|_| WalCorruptionError::TruncatedPayload)?;

        // Read CRC32
        let mut crc_buf = [0u8; 4];
        r.read_exact(&mut crc_buf)
            .map_err(|_| WalCorruptionError::TruncatedCrc)?;
        let stored_crc = u32::from_be_bytes(crc_buf);

        // Verify CRC32
        let mut crc_input = Vec::with_capacity(4 + payload_len);
        crc_input.extend_from_slice(&len_buf);
        crc_input.extend_from_slice(&payload);
        let computed_crc = crc32_compute(&crc_input);

        if stored_crc != computed_crc {
            return Err(WalCorruptionError::CrcMismatch {
                expected: stored_crc,
                computed: computed_crc,
            });
        }

        Ok(Some(Self {
            payload,
            crc32: stored_crc,
        }))
    }

    /// Deserialize the frame payload into a WAL entry.
    fn decode<T: Clone + for<'de> Deserialize<'de>>(
        &self,
    ) -> Result<WalEntry<T>, WalCorruptionError> {
        serde_json::from_slice(&self.payload).map_err(WalCorruptionError::DeserializeFailed)
    }
}

/// Errors during WAL file reading.
#[derive(Debug)]
pub enum WalCorruptionError {
    /// I/O error.
    Io(io::Error),
    /// Payload length exceeds maximum.
    PayloadTooLarge(usize),
    /// Payload bytes truncated.
    TruncatedPayload,
    /// CRC32 trailer truncated.
    TruncatedCrc,
    /// CRC32 mismatch — data corrupted.
    CrcMismatch { expected: u32, computed: u32 },
    /// JSON deserialization failed.
    DeserializeFailed(serde_json::Error),
}

impl std::fmt::Display for WalCorruptionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "WAL I/O error: {e}"),
            Self::PayloadTooLarge(n) => write!(f, "WAL payload too large: {n} bytes"),
            Self::TruncatedPayload => write!(f, "WAL payload truncated"),
            Self::TruncatedCrc => write!(f, "WAL CRC32 trailer truncated"),
            Self::CrcMismatch { expected, computed } => {
                write!(
                    f,
                    "WAL CRC32 mismatch: expected {expected:#010x}, computed {computed:#010x}"
                )
            }
            Self::DeserializeFailed(e) => write!(f, "WAL deserialize failed: {e}"),
        }
    }
}

impl std::error::Error for WalCorruptionError {}

// ── Disk WAL ────────────────────────────────────────────────────

/// Configuration for the disk-backed WAL.
#[derive(Debug, Clone)]
pub struct DiskWalConfig {
    /// In-memory WAL config (compaction thresholds).
    pub wal_config: WalConfig,
    /// Whether to fsync after each write (durability vs performance).
    pub fsync_on_write: bool,
    /// Maximum WAL file size in bytes before suggesting rotation.
    pub max_file_size: u64,
}

impl Default for DiskWalConfig {
    fn default() -> Self {
        Self {
            wal_config: WalConfig::default(),
            fsync_on_write: false,
            max_file_size: 50 * 1024 * 1024, // 50 MiB
        }
    }
}

/// A disk-backed write-ahead log with CRC32 integrity.
///
/// Combines the in-memory `WalEngine` with append-only file I/O.
/// Each entry is framed with a length prefix and CRC32 checksum
/// for crash recovery: on load, entries are read until EOF or
/// CRC failure, and the file is truncated at the last good entry.
pub struct DiskWal<T: Clone + Serialize + for<'de> Deserialize<'de>> {
    /// In-memory engine holding all live entries.
    engine: WalEngine<T>,
    /// Path to the WAL file on disk.
    path: PathBuf,
    /// Configuration.
    config: DiskWalConfig,
    /// Bytes written since last compaction.
    bytes_written: u64,
}

/// Result of loading a WAL from disk.
#[derive(Debug)]
pub struct WalLoadResult {
    /// Number of entries successfully loaded.
    pub entries_loaded: usize,
    /// Number of corrupt entries encountered at tail (truncated away).
    pub corrupt_tail_entries: usize,
    /// Total bytes in WAL file after recovery truncation.
    pub file_size: u64,
}

impl<T: Clone + Serialize + for<'de> Deserialize<'de>> DiskWal<T> {
    /// Create a new disk WAL at the given path.
    ///
    /// If the file exists, it is loaded and recovered. If it does not exist,
    /// a fresh WAL is created.
    pub fn open(
        path: impl Into<PathBuf>,
        config: DiskWalConfig,
    ) -> Result<(Self, WalLoadResult), io::Error> {
        let path = path.into();
        if path.exists() {
            Self::load_existing(path, config)
        } else {
            let wal = Self {
                engine: WalEngine::new(config.wal_config.clone()),
                path,
                config,
                bytes_written: 0,
            };
            let result = WalLoadResult {
                entries_loaded: 0,
                corrupt_tail_entries: 0,
                file_size: 0,
            };
            Ok((wal, result))
        }
    }

    /// Load and recover an existing WAL file.
    fn load_existing(
        path: PathBuf,
        config: DiskWalConfig,
    ) -> Result<(Self, WalLoadResult), io::Error> {
        let file = std::fs::File::open(&path)?;
        let mut reader = BufReader::new(file);
        let mut engine = WalEngine::new(config.wal_config.clone());
        let mut entries_loaded = 0;
        let mut good_offset: u64 = 0;
        let mut corrupt_tail = 0;

        loop {
            let pos = good_offset; // track where this frame starts
            match WalFrame::read_from(&mut reader) {
                Ok(Some(frame)) => {
                    match frame.decode::<T>() {
                        Ok(entry) => {
                            match &entry.kind {
                                EntryKind::Checkpoint => {
                                    engine.entries.push_back(entry.clone());
                                    engine.last_checkpoint_seq = Some(entry.seq);
                                }
                                EntryKind::CompactionMarker { compacted_through } => {
                                    engine.compacted_through = *compacted_through;
                                    engine.entries.push_back(entry.clone());
                                }
                                EntryKind::Mutation(_) => {
                                    engine.entries.push_back(entry.clone());
                                }
                            }
                            if entry.seq >= engine.next_seq {
                                engine.next_seq = entry.seq + 1;
                            }
                            entries_loaded += 1;
                            // Frame size: 4 (len) + payload + 4 (crc)
                            good_offset = pos + 4 + frame.payload.len() as u64 + 4;
                        }
                        Err(_) => {
                            corrupt_tail += 1;
                            break;
                        }
                    }
                }
                Ok(None) => break, // clean EOF
                Err(_) => {
                    corrupt_tail += 1;
                    break;
                }
            }
        }

        // Truncate file at last good entry to recover from partial writes
        if corrupt_tail > 0 {
            let file = std::fs::OpenOptions::new().write(true).open(&path)?;
            file.set_len(good_offset)?;
            if config.fsync_on_write {
                file.sync_all()?;
            }
        }

        let file_size = if path.exists() {
            std::fs::metadata(&path)?.len()
        } else {
            0
        };

        let wal = Self {
            engine,
            path,
            config,
            bytes_written: file_size,
        };
        let result = WalLoadResult {
            entries_loaded,
            corrupt_tail_entries: corrupt_tail,
            file_size,
        };
        Ok((wal, result))
    }

    /// Append a mutation and persist to disk.
    pub fn append(&mut self, mutation: T, timestamp_ms: u64) -> Result<u64, io::Error> {
        let seq = self.engine.append(mutation, timestamp_ms);
        let entry = self.engine.get(seq).expect("just appended").clone();
        self.persist_entry(&entry)?;
        Ok(seq)
    }

    /// Place a checkpoint marker and persist to disk.
    pub fn checkpoint(&mut self, timestamp_ms: u64) -> Result<u64, io::Error> {
        let seq = self.engine.checkpoint(timestamp_ms);
        let entry = self.engine.get(seq).expect("just checkpointed").clone();
        self.persist_entry(&entry)?;
        Ok(seq)
    }

    /// Persist a single entry to the WAL file.
    fn persist_entry(&mut self, entry: &WalEntry<T>) -> Result<(), io::Error> {
        let frame = WalFrame::from_entry(entry)?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let mut writer = BufWriter::new(&file);
        frame.write_to(&mut writer)?;
        writer.flush()?;
        if self.config.fsync_on_write {
            file.sync_all()?;
        }
        self.bytes_written += 4 + frame.payload.len() as u64 + 4;
        Ok(())
    }

    /// Get a reference to the in-memory WAL engine.
    pub fn engine(&self) -> &WalEngine<T> {
        &self.engine
    }

    /// Get a mutable reference to the in-memory WAL engine.
    pub fn engine_mut(&mut self) -> &mut WalEngine<T> {
        &mut self.engine
    }

    /// Check if the WAL file exceeds the max size threshold.
    pub fn exceeds_max_size(&self) -> bool {
        self.bytes_written > self.config.max_file_size
    }

    /// Current WAL file size estimate (bytes written).
    pub fn file_size(&self) -> u64 {
        self.bytes_written
    }

    /// Path to the WAL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Compact the in-memory engine and rewrite the WAL file.
    ///
    /// This creates a new temporary file, writes surviving entries,
    /// then atomically renames over the old file.
    pub fn compact_and_rewrite(&mut self) -> Result<usize, io::Error> {
        let removed = self.engine.compact();
        if removed == 0 {
            return Ok(0);
        }

        // Write all surviving entries to a temp file
        let tmp_path = self.path.with_extension("wal.tmp");
        {
            let file = std::fs::File::create(&tmp_path)?;
            let mut writer = BufWriter::new(&file);
            for entry in self.engine.iter() {
                let frame = WalFrame::from_entry(entry)?;
                frame.write_to(&mut writer)?;
            }
            writer.flush()?;
            if self.config.fsync_on_write {
                file.sync_all()?;
            }
        }

        // Atomic rename
        std::fs::rename(&tmp_path, &self.path)?;
        self.bytes_written = std::fs::metadata(&self.path)?.len();

        Ok(removed)
    }
}

// ── FrankenTerm Mux Mutations ───────────────────────────────────

/// Mutation types for FrankenTerm mux state changes.
///
/// Each variant captures a specific state transition in the mux server.
/// These are the `T` parameter for `WalEngine<MuxMutation>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MuxMutation {
    /// Terminal output captured from a pane.
    PaneOutput { pane_id: u64, data: Vec<u8> },
    /// A new pane was created.
    PaneCreated {
        pane_id: u64,
        rows: u16,
        cols: u16,
        title: String,
    },
    /// A pane was closed.
    PaneClosed { pane_id: u64 },
    /// A pane was resized.
    PaneResized { pane_id: u64, rows: u16, cols: u16 },
    /// Tab layout changed.
    LayoutChanged { tab_id: u64, layout_json: String },
    /// Focus changed to a different pane.
    FocusChanged { pane_id: u64 },
    /// A process started in a pane.
    ProcessStarted {
        pane_id: u64,
        command: String,
        pid: u32,
    },
    /// A process exited in a pane.
    ProcessExited { pane_id: u64, exit_code: i32 },
    /// Configuration value changed.
    ConfigChanged { key: String, value: String },
}

/// Type alias for a mux-specific WAL engine.
pub type MuxWalEngine = WalEngine<MuxMutation>;

/// Type alias for a mux-specific disk WAL.
pub type MuxDiskWal = DiskWal<MuxMutation>;

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::needless_collect)]
mod tests {
    use super::*;

    fn default_wal() -> WalEngine<String> {
        WalEngine::new(WalConfig::default())
    }

    fn small_wal() -> WalEngine<String> {
        WalEngine::new(WalConfig {
            compaction_threshold: 5,
            max_retained_entries: 3,
        })
    }

    #[test]
    fn empty_wal() {
        let wal = default_wal();
        assert!(wal.is_empty());
        assert_eq!(wal.len(), 0);
        assert_eq!(wal.next_seq(), 1);
        assert_eq!(wal.last_checkpoint(), None);
    }

    #[test]
    fn append_mutation() {
        let mut wal = default_wal();
        let seq = wal.append("hello".to_string(), 1000);
        assert_eq!(seq, 1);
        assert_eq!(wal.len(), 1);
        assert_eq!(wal.next_seq(), 2);

        let entry = wal.get(1).unwrap();
        assert_eq!(entry.seq, 1);
        assert_eq!(entry.timestamp_ms, 1000);
        assert!(matches!(&entry.kind, EntryKind::Mutation(s) if s == "hello"));
    }

    #[test]
    fn append_multiple() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("entry_{}", i), i as u64 * 100);
        }
        assert_eq!(wal.len(), 5);
        assert_eq!(wal.first_seq(), 1);
        assert_eq!(wal.last_seq(), 5);
    }

    #[test]
    fn checkpoint() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        let cp = wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        assert_eq!(wal.last_checkpoint(), Some(cp));
        assert_eq!(wal.len(), 3);
    }

    #[test]
    fn since_iterator() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let since_3: Vec<_> = wal.since(3).collect();
        assert_eq!(since_3.len(), 2); // seqs 4 and 5
        assert_eq!(since_3[0].seq, 4);
        assert_eq!(since_3[1].seq, 5);
    }

    #[test]
    fn mutations_iterator() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        let muts: Vec<_> = wal.mutations().collect();
        assert_eq!(muts.len(), 2); // only mutations, no checkpoint
    }

    #[test]
    fn range_iterator() {
        let mut wal = default_wal();
        for i in 0..10 {
            wal.append(format!("e{}", i), i as u64);
        }
        let range: Vec<_> = wal.range(3, 7).collect();
        assert_eq!(range.len(), 5);
        assert_eq!(range[0].seq, 3);
        assert_eq!(range[4].seq, 7);
    }

    #[test]
    fn since_last_checkpoint() {
        let mut wal = default_wal();
        wal.append("before1".to_string(), 100);
        wal.append("before2".to_string(), 200);
        wal.checkpoint(300);
        wal.append("after1".to_string(), 400);
        wal.append("after2".to_string(), 500);

        let since_cp: Vec<_> = wal.since_last_checkpoint().collect();
        assert_eq!(since_cp.len(), 2);
        assert!(matches!(&since_cp[0].kind, EntryKind::Mutation(s) if s == "after1"));
    }

    #[test]
    fn compact_with_checkpoint() {
        let mut wal = small_wal();
        for i in 0..3 {
            wal.append(format!("old_{}", i), i as u64);
        }
        wal.checkpoint(300);
        for i in 0..3 {
            wal.append(format!("new_{}", i), 400 + i as u64);
        }

        let before = wal.len();
        let removed = wal.compact();
        assert!(removed > 0, "should compact old entries");
        assert!(wal.len() < before, "log should be smaller after compaction");

        // New entries should still be accessible
        let muts: Vec<_> = wal.mutations().collect();
        assert!(muts.len() >= 3, "new mutations should survive compaction");
    }

    #[test]
    fn compact_without_checkpoint() {
        let mut wal = WalEngine::new(WalConfig {
            compaction_threshold: 3,
            max_retained_entries: 2,
        });
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let removed = wal.compact();
        assert!(removed > 0);
        assert!(wal.len() <= 4); // at most 2 retained + compaction marker + maybe some
    }

    #[test]
    fn truncate_after() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        wal.truncate_after(3);
        assert_eq!(wal.len(), 3);
        assert_eq!(wal.last_seq(), 3);
        assert_eq!(wal.next_seq(), 4);
    }

    #[test]
    fn truncate_removes_checkpoint() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        wal.checkpoint(400);
        wal.append("c".to_string(), 500);

        wal.truncate_after(3); // keeps a, cp, b
        assert_eq!(wal.last_checkpoint(), Some(2)); // first checkpoint
    }

    #[test]
    fn clear() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let next = wal.next_seq();
        wal.clear();
        assert!(wal.is_empty());
        assert_eq!(wal.next_seq(), next); // sequence preserved
        assert_eq!(wal.last_checkpoint(), None);
    }

    #[test]
    fn replay_mutations() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        wal.append("c".to_string(), 400);

        let replayed: Vec<_> = super::replay_mutations(&wal, 1, 4).into_iter().collect();
        assert_eq!(replayed.len(), 3);
        assert_eq!(*replayed[0], "a");
        assert_eq!(*replayed[1], "b");
        assert_eq!(*replayed[2], "c");
    }

    #[test]
    fn replay_partial() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let replayed: Vec<_> = super::replay_mutations(&wal, 2, 4).into_iter().collect();
        assert_eq!(replayed.len(), 3);
        assert_eq!(*replayed[0], "e1"); // seq 2
    }

    #[test]
    fn get_nonexistent() {
        let wal = default_wal();
        assert!(wal.get(0).is_none());
        assert!(wal.get(99).is_none());
    }

    #[test]
    fn stats() {
        let mut wal = small_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);

        let stats = wal.stats();
        assert_eq!(stats.total_entries, 3);
        assert_eq!(stats.mutation_count, 2);
        assert_eq!(stats.checkpoint_count, 1);
        assert_eq!(stats.first_seq, 1);
        assert_eq!(stats.last_seq, 3);
        assert_eq!(stats.last_checkpoint_seq, Some(2));
    }

    #[test]
    fn stats_serde() {
        let stats = WalStats {
            total_entries: 10,
            mutation_count: 8,
            checkpoint_count: 2,
            first_seq: 1,
            last_seq: 10,
            last_checkpoint_seq: Some(5),
            compacted_through: 0,
            needs_compaction: false,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let back: WalStats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats, back);
    }

    #[test]
    fn needs_compaction() {
        let mut wal = small_wal(); // threshold = 5
        for i in 0..4 {
            wal.append(format!("e{}", i), i as u64);
        }
        assert!(!wal.needs_compaction());
        wal.append("e4".to_string(), 4);
        wal.append("e5".to_string(), 5);
        assert!(wal.needs_compaction());
    }

    #[test]
    fn sequence_monotonic() {
        let mut wal = default_wal();
        let s1 = wal.append("a".to_string(), 0);
        let s2 = wal.checkpoint(1);
        let s3 = wal.append("b".to_string(), 2);
        assert!(s1 < s2);
        assert!(s2 < s3);
    }

    #[test]
    fn entry_kind_serde() {
        let kinds: Vec<EntryKind<String>> = vec![
            EntryKind::Mutation("test".to_string()),
            EntryKind::Checkpoint,
            EntryKind::CompactionMarker {
                compacted_through: 42,
            },
        ];
        for kind in &kinds {
            let json = serde_json::to_string(kind).unwrap();
            let back: EntryKind<String> = serde_json::from_str(&json).unwrap();
            assert_eq!(*kind, back);
        }
    }

    // -------------------------------------------------------------------
    // Batch: DarkBadger wa-1u90p.7.1
    // -------------------------------------------------------------------

    // -- WalConfig --

    #[test]
    fn wal_config_debug_clone() {
        let config = WalConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("WalConfig"), "got: {}", dbg);
        let cloned = config.clone();
        assert_eq!(cloned.compaction_threshold, config.compaction_threshold);
    }

    #[test]
    fn wal_config_default_values() {
        let config = WalConfig::default();
        assert_eq!(config.compaction_threshold, 10_000);
        assert_eq!(config.max_retained_entries, 1_000);
    }

    // -- WalEntry --

    #[test]
    fn wal_entry_debug() {
        let entry = WalEntry {
            seq: 1,
            timestamp_ms: 1000,
            kind: EntryKind::Mutation("test".to_string()),
        };
        let dbg = format!("{:?}", entry);
        assert!(dbg.contains("WalEntry"), "got: {}", dbg);
        assert!(dbg.contains("seq: 1"), "got: {}", dbg);
    }

    #[test]
    fn wal_entry_clone_eq() {
        let entry = WalEntry::<String> {
            seq: 5,
            timestamp_ms: 500,
            kind: EntryKind::Checkpoint,
        };
        let cloned = entry.clone();
        assert_eq!(entry, cloned);
    }

    #[test]
    fn wal_entry_serde_roundtrip() {
        let entry = WalEntry {
            seq: 42,
            timestamp_ms: 9999,
            kind: EntryKind::Mutation("hello".to_string()),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: WalEntry<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(entry, back);
    }

    // -- EntryKind --

    #[test]
    fn entry_kind_debug() {
        let kind = EntryKind::CompactionMarker::<String> {
            compacted_through: 10,
        };
        let dbg = format!("{:?}", kind);
        assert!(dbg.contains("CompactionMarker"), "got: {}", dbg);
    }

    #[test]
    fn entry_kind_clone_eq() {
        let a = EntryKind::Mutation(42i32);
        let b = a.clone();
        assert_eq!(a, b);
        assert_ne!(a, EntryKind::Checkpoint);
    }

    // -- WalEngine --

    #[test]
    fn wal_engine_debug() {
        let wal = default_wal();
        let dbg = format!("{:?}", wal);
        assert!(dbg.contains("WalEngine"), "got: {}", dbg);
    }

    #[test]
    fn wal_engine_clone_independence() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        let mut cloned = wal.clone();
        cloned.append("b".to_string(), 200);
        assert_eq!(wal.len(), 1);
        assert_eq!(cloned.len(), 2);
    }

    #[test]
    fn wal_first_seq_empty() {
        let wal = default_wal();
        // When empty, first_seq returns next_seq
        assert_eq!(wal.first_seq(), wal.next_seq());
    }

    #[test]
    fn wal_last_seq_empty() {
        let wal = default_wal();
        assert_eq!(wal.last_seq(), 0);
    }

    #[test]
    fn wal_compacted_through_initial() {
        let wal = default_wal();
        assert_eq!(wal.compacted_through(), 0);
    }

    #[test]
    fn wal_get_seq_zero() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        // Seq 0 is explicitly handled as None
        assert!(wal.get(0).is_none());
    }

    #[test]
    fn wal_get_below_first_seq() {
        let mut wal = small_wal();
        for i in 0..6 {
            wal.append(format!("e{}", i), i as u64);
        }
        wal.checkpoint(600);
        wal.compact();
        // After compaction, early sequences should be gone
        assert!(wal.get(1).is_none());
    }

    // -- Iterator edge cases --

    #[test]
    fn wal_since_high_seq() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        let result: Vec<_> = wal.since(999).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn wal_range_inverted() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        // from > to should return empty
        let result: Vec<_> = wal.range(5, 2).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn wal_range_beyond_bounds() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        let result: Vec<_> = wal.range(100, 200).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn wal_since_last_checkpoint_no_checkpoint() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.append("b".to_string(), 200);
        // Without checkpoint, returns all entries (since seq 0)
        let result: Vec<_> = wal.since_last_checkpoint().collect();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn wal_mutations_empty() {
        let wal = default_wal();
        assert_eq!(wal.mutations().count(), 0);
    }

    #[test]
    fn wal_iter_empty() {
        let wal = default_wal();
        assert_eq!(wal.iter().count(), 0);
    }

    // -- Compact edge cases --

    #[test]
    fn wal_compact_empty() {
        let mut wal = small_wal();
        let removed = wal.compact();
        assert_eq!(removed, 0);
    }

    #[test]
    fn wal_compact_below_threshold() {
        let mut wal = small_wal(); // threshold = 5
        wal.append("a".to_string(), 100);
        wal.append("b".to_string(), 200);
        // No checkpoint, below max_retained — nothing to compact
        let removed = wal.compact();
        assert_eq!(removed, 0);
    }

    #[test]
    fn wal_compact_updates_compacted_through() {
        let mut wal = small_wal();
        for i in 0..4 {
            wal.append(format!("e{}", i), i as u64);
        }
        wal.checkpoint(400);
        wal.append("after".to_string(), 500);
        assert_eq!(wal.compacted_through(), 0);
        let removed = wal.compact();
        if removed > 0 {
            assert!(wal.compacted_through() > 0);
        }
    }

    // -- Truncate edge cases --

    #[test]
    fn wal_truncate_empty() {
        let mut wal = default_wal();
        wal.truncate_after(0);
        assert!(wal.is_empty());
        assert_eq!(wal.next_seq(), 1);
    }

    #[test]
    fn wal_truncate_beyond_last() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.append("b".to_string(), 200);
        // Truncate beyond last seq is a no-op on entries
        wal.truncate_after(999);
        assert_eq!(wal.len(), 2);
    }

    // -- Clear --

    #[test]
    fn wal_clear_preserves_seq_counter() {
        let mut wal = default_wal();
        for i in 0..5 {
            wal.append(format!("e{}", i), i as u64);
        }
        let seq_before = wal.next_seq();
        wal.clear();
        assert_eq!(wal.next_seq(), seq_before);
    }

    // -- WalStats --

    #[test]
    fn wal_stats_debug_clone() {
        let stats = WalStats {
            total_entries: 5,
            mutation_count: 3,
            checkpoint_count: 2,
            first_seq: 1,
            last_seq: 5,
            last_checkpoint_seq: Some(4),
            compacted_through: 0,
            needs_compaction: false,
        };
        let dbg = format!("{:?}", stats);
        assert!(dbg.contains("WalStats"), "got: {}", dbg);
        let cloned = stats.clone();
        assert_eq!(stats, cloned);
    }

    #[test]
    fn wal_stats_empty() {
        let wal = default_wal();
        let stats = wal.stats();
        assert_eq!(stats.total_entries, 0);
        assert_eq!(stats.mutation_count, 0);
        assert_eq!(stats.checkpoint_count, 0);
        assert_eq!(stats.last_checkpoint_seq, None);
        assert!(!stats.needs_compaction);
    }

    // -- ReplayCursor edge cases --

    #[test]
    fn replay_cursor_empty_wal() {
        let wal = default_wal();
        let result: Vec<_> = ReplayCursor::new(&wal, 1, 10).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn replay_cursor_from_exceeds_to() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.append("b".to_string(), 200);
        let result: Vec<_> = ReplayCursor::new(&wal, 10, 5).collect();
        assert!(result.is_empty());
    }

    #[test]
    fn replay_cursor_skips_checkpoints() {
        let mut wal = default_wal();
        wal.append("a".to_string(), 100);
        wal.checkpoint(200);
        wal.append("b".to_string(), 300);
        let result: Vec<_> = ReplayCursor::new(&wal, 1, 3).collect();
        assert_eq!(result.len(), 2);
        assert_eq!(*result[0], "a");
        assert_eq!(*result[1], "b");
    }

    // -- replay_mutations function --

    #[test]
    fn replay_mutations_empty_wal() {
        let wal = default_wal();
        let result = super::replay_mutations(&wal, 1, 10);
        assert!(result.is_empty());
    }

    // ── CRC32 Tests ──────────────────────────────────────────────

    #[test]
    fn crc32_empty_input() {
        let crc = crc32_compute(b"");
        // CRC32 of empty input is 0x00000000
        assert_eq!(crc, 0x0000_0000);
    }

    #[test]
    fn crc32_known_vector() {
        // CRC32 of "123456789" is 0xCBF43926 (standard test vector)
        let crc = crc32_compute(b"123456789");
        assert_eq!(crc, 0xCBF4_3926);
    }

    #[test]
    fn crc32_deterministic() {
        let data = b"hello world";
        let crc1 = crc32_compute(data);
        let crc2 = crc32_compute(data);
        assert_eq!(crc1, crc2);
    }

    #[test]
    fn crc32_different_inputs_differ() {
        let crc_a = crc32_compute(b"aaa");
        let crc_b = crc32_compute(b"bbb");
        assert_ne!(crc_a, crc_b);
    }

    #[test]
    fn crc32_single_byte() {
        let crc = crc32_compute(b"A");
        // Must be non-zero and deterministic
        assert_ne!(crc, 0);
        assert_eq!(crc, crc32_compute(b"A"));
    }

    // ── WalFrame Tests ──────────────────────────────────────────

    #[test]
    fn wal_frame_roundtrip() {
        let entry = WalEntry {
            seq: 1,
            timestamp_ms: 1000,
            kind: EntryKind::Mutation("hello".to_string()),
        };
        let frame = WalFrame::from_entry(&entry).unwrap();
        let decoded: WalEntry<String> = frame.decode().unwrap();
        assert_eq!(entry, decoded);
    }

    #[test]
    fn wal_frame_write_read_roundtrip() {
        let entry = WalEntry {
            seq: 42,
            timestamp_ms: 9999,
            kind: EntryKind::Checkpoint,
        };
        let frame = WalFrame::from_entry(&entry).unwrap();

        let mut buf = Vec::new();
        frame.write_to(&mut buf).unwrap();

        let mut reader = std::io::BufReader::new(&buf[..]);
        let read_frame = WalFrame::read_from(&mut reader).unwrap().unwrap();
        assert_eq!(frame, read_frame);

        let decoded: WalEntry<String> = read_frame.decode().unwrap();
        assert_eq!(entry, decoded);
    }

    #[test]
    fn wal_frame_read_empty_eof() {
        let buf: &[u8] = &[];
        let mut reader = std::io::BufReader::new(buf);
        let result = WalFrame::read_from(&mut reader).unwrap();
        assert!(result.is_none()); // clean EOF
    }

    #[test]
    fn wal_frame_read_truncated_length() {
        // Only 2 bytes instead of 4 for length prefix
        let buf: &[u8] = &[0x00, 0x01];
        let mut reader = std::io::BufReader::new(buf);
        let result = WalFrame::read_from(&mut reader).unwrap();
        assert!(result.is_none()); // treated as EOF
    }

    #[test]
    fn wal_frame_read_truncated_payload() {
        // Valid length prefix (10 bytes) but only 3 bytes of payload
        let mut buf = Vec::new();
        buf.extend_from_slice(&10u32.to_be_bytes());
        buf.extend_from_slice(&[1, 2, 3]); // only 3 of 10
        let mut reader = std::io::BufReader::new(&buf[..]);
        let result = WalFrame::read_from(&mut reader);
        assert!(matches!(result, Err(WalCorruptionError::TruncatedPayload)));
    }

    #[test]
    fn wal_frame_read_truncated_crc() {
        // Valid length + payload, but CRC truncated
        let payload = b"test";
        let mut buf = Vec::new();
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(payload);
        buf.extend_from_slice(&[0x00, 0x01]); // only 2 of 4 CRC bytes
        let mut reader = std::io::BufReader::new(&buf[..]);
        let result = WalFrame::read_from(&mut reader);
        assert!(matches!(result, Err(WalCorruptionError::TruncatedCrc)));
    }

    #[test]
    fn wal_frame_read_crc_mismatch() {
        let entry = WalEntry {
            seq: 1,
            timestamp_ms: 100,
            kind: EntryKind::Mutation("test".to_string()),
        };
        let frame = WalFrame::from_entry(&entry).unwrap();
        let mut buf = Vec::new();
        frame.write_to(&mut buf).unwrap();

        // Corrupt one byte in the payload
        if buf.len() > 5 {
            buf[5] ^= 0xFF;
        }

        let mut reader = std::io::BufReader::new(&buf[..]);
        let result = WalFrame::read_from(&mut reader);
        assert!(matches!(
            result,
            Err(WalCorruptionError::CrcMismatch { .. })
        ));
    }

    #[test]
    fn wal_frame_payload_too_large() {
        // Craft a length prefix claiming >64 MiB
        let huge_len = 65 * 1024 * 1024u32;
        let mut buf = Vec::new();
        buf.extend_from_slice(&huge_len.to_be_bytes());
        buf.extend_from_slice(&[0u8; 16]); // doesn't matter
        let mut reader = std::io::BufReader::new(&buf[..]);
        let result = WalFrame::read_from(&mut reader);
        assert!(matches!(
            result,
            Err(WalCorruptionError::PayloadTooLarge(_))
        ));
    }

    #[test]
    fn wal_frame_multiple_sequential() {
        let entries: Vec<WalEntry<String>> = (0..5)
            .map(|i| WalEntry {
                seq: i + 1,
                timestamp_ms: i * 100,
                kind: EntryKind::Mutation(format!("entry_{}", i)),
            })
            .collect();

        let mut buf = Vec::new();
        for e in &entries {
            let frame = WalFrame::from_entry(e).unwrap();
            frame.write_to(&mut buf).unwrap();
        }

        let mut reader = std::io::BufReader::new(&buf[..]);
        let mut decoded = Vec::new();
        while let Some(frame) = WalFrame::read_from(&mut reader).unwrap() {
            decoded.push(frame.decode::<String>().unwrap());
        }
        assert_eq!(decoded, entries);
    }

    // ── WalCorruptionError Tests ────────────────────────────────

    #[test]
    fn wal_corruption_error_display() {
        let err = WalCorruptionError::PayloadTooLarge(999);
        let msg = format!("{}", err);
        assert!(msg.contains("999"), "got: {}", msg);

        let err2 = WalCorruptionError::CrcMismatch {
            expected: 0xDEAD,
            computed: 0xBEEF,
        };
        let msg2 = format!("{}", err2);
        assert!(msg2.contains("mismatch"), "got: {}", msg2);

        let err3 = WalCorruptionError::TruncatedPayload;
        assert!(format!("{}", err3).contains("truncated"));

        let err4 = WalCorruptionError::TruncatedCrc;
        assert!(format!("{}", err4).contains("truncated"));
    }

    #[test]
    fn wal_corruption_error_debug() {
        let err = WalCorruptionError::TruncatedPayload;
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("TruncatedPayload"), "got: {}", dbg);
    }

    #[test]
    fn wal_corruption_error_is_std_error() {
        let err = WalCorruptionError::TruncatedPayload;
        let _: &dyn std::error::Error = &err;
    }

    // ── DiskWalConfig Tests ─────────────────────────────────────

    #[test]
    fn disk_wal_config_defaults() {
        let config = DiskWalConfig::default();
        assert!(!config.fsync_on_write);
        assert_eq!(config.max_file_size, 50 * 1024 * 1024);
        assert_eq!(config.wal_config.compaction_threshold, 10_000);
    }

    #[test]
    fn disk_wal_config_debug_clone() {
        let config = DiskWalConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("DiskWalConfig"), "got: {}", dbg);
        let cloned = config.clone();
        assert_eq!(cloned.fsync_on_write, config.fsync_on_write);
    }

    // ── DiskWal Tests ───────────────────────────────────────────

    #[test]
    fn disk_wal_open_new() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");
        let (wal, result) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
        assert_eq!(result.entries_loaded, 0);
        assert_eq!(result.corrupt_tail_entries, 0);
        assert_eq!(result.file_size, 0);
        assert!(wal.engine().is_empty());
        assert_eq!(wal.file_size(), 0);
    }

    #[test]
    fn disk_wal_append_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        // Write entries
        {
            let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            wal.append("hello".to_string(), 100).unwrap();
            wal.append("world".to_string(), 200).unwrap();
            assert_eq!(wal.engine().len(), 2);
        }

        // Reload and verify
        {
            let (wal, result) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            assert_eq!(result.entries_loaded, 2);
            assert_eq!(result.corrupt_tail_entries, 0);
            assert_eq!(wal.engine().len(), 2);

            let e1 = wal.engine().get(1).unwrap();
            assert!(matches!(&e1.kind, EntryKind::Mutation(s) if s == "hello"));
            let e2 = wal.engine().get(2).unwrap();
            assert!(matches!(&e2.kind, EntryKind::Mutation(s) if s == "world"));
        }
    }

    #[test]
    fn disk_wal_checkpoint_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        {
            let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            wal.append("before".to_string(), 100).unwrap();
            wal.checkpoint(200).unwrap();
            wal.append("after".to_string(), 300).unwrap();
        }

        {
            let (wal, result) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            assert_eq!(result.entries_loaded, 3);
            assert_eq!(wal.engine().last_checkpoint(), Some(2));
        }
    }

    #[test]
    fn disk_wal_crash_recovery_truncated_tail() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        // Write two good entries
        {
            let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            wal.append("good1".to_string(), 100).unwrap();
            wal.append("good2".to_string(), 200).unwrap();
        }

        // Simulate crash: append garbage bytes to the file
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            // Write a partial frame (length prefix says 100 bytes, but only 5 follow)
            file.write_all(&100u32.to_be_bytes()).unwrap();
            file.write_all(&[0xDE, 0xAD, 0xBE, 0xEF, 0x42]).unwrap();
        }

        // Recovery should load the two good entries and report corruption
        {
            let (wal, result) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            assert_eq!(result.entries_loaded, 2);
            assert_eq!(result.corrupt_tail_entries, 1);
            assert_eq!(wal.engine().len(), 2);
        }
    }

    #[test]
    fn disk_wal_crash_recovery_bad_crc() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        // Write one good entry
        {
            let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            wal.append("good".to_string(), 100).unwrap();
        }

        let good_size = std::fs::metadata(&path).unwrap().len();

        // Manually append a frame with bad CRC
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            let payload = b"bad payload";
            file.write_all(&(payload.len() as u32).to_be_bytes())
                .unwrap();
            file.write_all(payload).unwrap();
            file.write_all(&0xDEADBEEFu32.to_be_bytes()).unwrap(); // wrong CRC
        }

        // Recovery should load only the good entry
        {
            let (_wal, result) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            assert_eq!(result.entries_loaded, 1);
            assert_eq!(result.corrupt_tail_entries, 1);
            // File should be truncated to good_size
            let actual_size = std::fs::metadata(&path).unwrap().len();
            assert_eq!(actual_size, good_size);
        }
    }

    #[test]
    fn disk_wal_compact_and_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let config = DiskWalConfig {
            wal_config: WalConfig {
                compaction_threshold: 5,
                max_retained_entries: 3,
            },
            ..DiskWalConfig::default()
        };

        {
            let (mut wal, _) = DiskWal::<String>::open(&path, config.clone()).unwrap();
            for i in 0..4 {
                wal.append(format!("old_{}", i), i as u64 * 100).unwrap();
            }
            wal.checkpoint(400).unwrap();
            for i in 0..3 {
                wal.append(format!("new_{}", i), 500 + i as u64 * 100)
                    .unwrap();
            }

            let size_before = wal.file_size();
            let removed = wal.compact_and_rewrite().unwrap();
            assert!(removed > 0, "should have compacted entries");
            assert!(
                wal.file_size() < size_before,
                "file should be smaller after compaction"
            );
        }

        // Verify rewritten file loads correctly
        {
            let (wal, result) = DiskWal::<String>::open(&path, config).unwrap();
            assert_eq!(result.corrupt_tail_entries, 0);
            // Recent entries should survive
            let muts: Vec<_> = wal.engine().mutations().collect();
            assert!(muts.len() >= 3, "new mutations should survive compaction");
        }
    }

    #[test]
    fn disk_wal_compact_no_op_when_nothing_to_compact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
        wal.append("a".to_string(), 100).unwrap();
        let removed = wal.compact_and_rewrite().unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn disk_wal_exceeds_max_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let config = DiskWalConfig {
            max_file_size: 100, // tiny
            ..DiskWalConfig::default()
        };

        let (mut wal, _) = DiskWal::<String>::open(&path, config).unwrap();
        assert!(!wal.exceeds_max_size());

        // Write enough to exceed 100 bytes
        for i in 0..10 {
            wal.append(format!("entry_with_some_data_{}", i), i as u64)
                .unwrap();
        }
        assert!(wal.exceeds_max_size());
    }

    #[test]
    fn disk_wal_path_accessor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mywal.wal");
        let (wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
        assert_eq!(wal.path(), path);
    }

    #[test]
    fn disk_wal_engine_mut_access() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
        wal.append("a".to_string(), 100).unwrap();

        // Access mutable engine to truncate
        wal.engine_mut().truncate_after(0);
        assert!(wal.engine().is_empty());
    }

    #[test]
    fn disk_wal_sequence_continuity_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let last_seq;
        {
            let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            wal.append("a".to_string(), 100).unwrap();
            wal.append("b".to_string(), 200).unwrap();
            last_seq = wal.engine().last_seq();
        }

        {
            let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            let new_seq = wal.append("c".to_string(), 300).unwrap();
            assert!(
                new_seq > last_seq,
                "new seq {} should be > last seq {}",
                new_seq,
                last_seq
            );
        }
    }

    #[test]
    fn disk_wal_fsync_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let config = DiskWalConfig {
            fsync_on_write: true,
            ..DiskWalConfig::default()
        };

        let (mut wal, _) = DiskWal::<String>::open(&path, config).unwrap();
        // Should not panic with fsync enabled
        wal.append("durable".to_string(), 100).unwrap();
        wal.checkpoint(200).unwrap();
    }

    #[test]
    fn disk_wal_many_entries_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let count = 100;
        {
            let (mut wal, _) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            for i in 0..count {
                wal.append(format!("entry_{}", i), i as u64).unwrap();
            }
        }

        {
            let (wal, result) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
            assert_eq!(result.entries_loaded, count);
            assert_eq!(wal.engine().len(), count);
        }
    }

    // ── MuxMutation Tests ───────────────────────────────────────

    #[test]
    fn mux_mutation_pane_output_serde() {
        let m = MuxMutation::PaneOutput {
            pane_id: 42,
            data: vec![0x1B, 0x5B, 0x31, 0x6D], // ESC[1m
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MuxMutation = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn mux_mutation_pane_created_serde() {
        let m = MuxMutation::PaneCreated {
            pane_id: 1,
            rows: 24,
            cols: 80,
            title: "bash".to_string(),
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: MuxMutation = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn mux_mutation_all_variants_serde() {
        let variants: Vec<MuxMutation> = vec![
            MuxMutation::PaneOutput {
                pane_id: 1,
                data: vec![65, 66],
            },
            MuxMutation::PaneCreated {
                pane_id: 2,
                rows: 24,
                cols: 80,
                title: "zsh".into(),
            },
            MuxMutation::PaneClosed { pane_id: 3 },
            MuxMutation::PaneResized {
                pane_id: 4,
                rows: 40,
                cols: 120,
            },
            MuxMutation::LayoutChanged {
                tab_id: 5,
                layout_json: "{}".into(),
            },
            MuxMutation::FocusChanged { pane_id: 6 },
            MuxMutation::ProcessStarted {
                pane_id: 7,
                command: "/bin/bash".into(),
                pid: 1234,
            },
            MuxMutation::ProcessExited {
                pane_id: 8,
                exit_code: 0,
            },
            MuxMutation::ConfigChanged {
                key: "font_size".into(),
                value: "14".into(),
            },
        ];

        for v in &variants {
            let json = serde_json::to_string(v).unwrap();
            let back: MuxMutation = serde_json::from_str(&json).unwrap();
            assert_eq!(*v, back);
        }
    }

    #[test]
    fn mux_mutation_debug_clone() {
        let m = MuxMutation::FocusChanged { pane_id: 42 };
        let dbg = format!("{:?}", m);
        assert!(dbg.contains("FocusChanged"), "got: {}", dbg);
        let cloned = m.clone();
        assert_eq!(m, cloned);
    }

    #[test]
    fn mux_wal_engine_type_alias() {
        let mut wal: MuxWalEngine = WalEngine::new(WalConfig::default());
        let seq = wal.append(
            MuxMutation::PaneCreated {
                pane_id: 1,
                rows: 24,
                cols: 80,
                title: "test".into(),
            },
            1000,
        );
        assert_eq!(seq, 1);
        assert_eq!(wal.len(), 1);
    }

    #[test]
    fn mux_disk_wal_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mux.wal");

        {
            let (mut wal, _) = MuxDiskWal::open(&path, DiskWalConfig::default()).unwrap();
            wal.append(
                MuxMutation::PaneCreated {
                    pane_id: 1,
                    rows: 24,
                    cols: 80,
                    title: "bash".into(),
                },
                100,
            )
            .unwrap();
            wal.append(
                MuxMutation::PaneOutput {
                    pane_id: 1,
                    data: b"hello\n".to_vec(),
                },
                200,
            )
            .unwrap();
            wal.checkpoint(300).unwrap();
            wal.append(MuxMutation::PaneClosed { pane_id: 1 }, 400)
                .unwrap();
        }

        {
            let (wal, result) = MuxDiskWal::open(&path, DiskWalConfig::default()).unwrap();
            assert_eq!(result.entries_loaded, 4);
            assert_eq!(wal.engine().last_checkpoint(), Some(3));

            let muts: Vec<_> = wal.engine().mutations().collect();
            assert_eq!(muts.len(), 3);
        }
    }

    // ── WalLoadResult Tests ─────────────────────────────────────

    #[test]
    fn wal_load_result_debug() {
        let result = WalLoadResult {
            entries_loaded: 10,
            corrupt_tail_entries: 1,
            file_size: 4096,
        };
        let dbg = format!("{:?}", result);
        assert!(dbg.contains("WalLoadResult"), "got: {}", dbg);
        assert!(dbg.contains("10"), "got: {}", dbg);
    }

    // ── Edge Case: Empty file ───────────────────────────────────

    #[test]
    fn disk_wal_open_empty_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.wal");
        // Create empty file
        std::fs::File::create(&path).unwrap();

        let (wal, result) = DiskWal::<String>::open(&path, DiskWalConfig::default()).unwrap();
        assert_eq!(result.entries_loaded, 0);
        assert_eq!(result.corrupt_tail_entries, 0);
        assert!(wal.engine().is_empty());
    }

    // ── Compaction marker survives reload ────────────────────────

    #[test]
    fn disk_wal_compaction_marker_persists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.wal");

        let config = DiskWalConfig {
            wal_config: WalConfig {
                compaction_threshold: 3,
                max_retained_entries: 2,
            },
            ..DiskWalConfig::default()
        };

        {
            let (mut wal, _) = DiskWal::<String>::open(&path, config.clone()).unwrap();
            for i in 0..4 {
                wal.append(format!("e{}", i), i as u64 * 100).unwrap();
            }
            wal.checkpoint(400).unwrap();
            wal.append("after".to_string(), 500).unwrap();
            wal.compact_and_rewrite().unwrap();
            assert!(wal.engine().compacted_through() > 0);
        }

        {
            let (wal, result) = DiskWal::<String>::open(&path, config).unwrap();
            assert_eq!(result.corrupt_tail_entries, 0);
            assert!(
                wal.engine().compacted_through() > 0,
                "compacted_through should persist"
            );
        }
    }
}
