//! Experimental scrollback store for `ft-8vla`.
//!
//! This module keeps an append-only log per pane plus a byte-offset line index.
//! The index allows tail reads to seek directly to the relevant byte window.
//! A later slice can swap the tail read path to true mmap once we expose a
//! safe mapping wrapper that fits this crate's `unsafe_code = forbid` policy.
//!
//! Compaction (dropping a stale prefix once enough bytes age out) rewrites the
//! log crash-safely: the retained suffix is written to a sibling temp file,
//! fsync'd, and atomically renamed over the live log, so a crash mid-compaction
//! leaves either the old or the compacted log intact — never a truncated one
//! (ft-odrq7). Successful appends synchronize their data before publishing the
//! in-memory offset, and logical prefix pruning is recorded in a synchronized
//! sequence journal so both content and row identity survive process crashes.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};

/// Pane identifier.
pub type PaneId = u64;

/// Byte offset for a line start in the pane log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineOffset(pub u64);

/// Active storage mode for a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneStorageMode {
    Mmap,
    SqliteFallback,
}

/// Default-off cold-tier erasure mode for mmap scrollback sidecars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColdErasureMode {
    #[default]
    Disabled,
    ReedSolomon,
}

/// Configuration for the scrollback store.
#[derive(Debug, Clone)]
pub struct MmapStoreConfig {
    pub base_dir: PathBuf,
    pub sqlite_fallback_path: Option<PathBuf>,
    pub cold_erasure: ColdErasureMode,
}

impl MmapStoreConfig {
    #[must_use]
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            sqlite_fallback_path: None,
            cold_erasure: ColdErasureMode::Disabled,
        }
    }

    #[must_use]
    pub fn with_sqlite_fallback(mut self, sqlite_fallback_path: PathBuf) -> Self {
        self.sqlite_fallback_path = Some(sqlite_fallback_path);
        self
    }

    #[must_use]
    pub fn with_cold_erasure(mut self, mode: ColdErasureMode) -> Self {
        self.cold_erasure = mode;
        self
    }

    #[must_use]
    pub fn with_cold_erasure_rs(mut self) -> Self {
        self.cold_erasure = ColdErasureMode::ReedSolomon;
        self
    }
}

impl From<crate::config::StorageColdErasureMode> for ColdErasureMode {
    fn from(value: crate::config::StorageColdErasureMode) -> Self {
        match value {
            crate::config::StorageColdErasureMode::Off => Self::Disabled,
            crate::config::StorageColdErasureMode::Rs => Self::ReedSolomon,
        }
    }
}

/// Error type for the scaffold store.
#[derive(Debug, thiserror::Error)]
pub enum MmapStoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("unknown pane: {0}")]
    UnknownPane(PaneId),
    #[error("offset {offset} exceeds file length {len}")]
    OffsetOutOfBounds { offset: u64, len: u64 },
    #[error("numeric conversion overflow for {0}")]
    NumericOverflow(&'static str),
    #[error("not enough erasure shards: have {have}, need {need}")]
    InsufficientErasureShards { have: usize, need: usize },
    #[error("erasure shard {index} failed CRC validation")]
    ErasureShardCrcMismatch { index: u8 },
    #[error("erasure payload failed CRC validation")]
    ErasurePayloadCrcMismatch,
    #[error("invalid erasure shard: {0}")]
    InvalidErasureShard(String),
    #[error("invalid pane log header: {0}")]
    InvalidPaneLogHeader(String),
    #[error("pane log record contains a line delimiter")]
    InvalidLineRecord,
    #[error("pane log record is {bytes} bytes, exceeding the {max} byte limit")]
    PaneLogRecordTooLarge { bytes: u64, max: u64 },
    #[error("pane log record is not valid UTF-8: {0}")]
    InvalidPaneLogUtf8(#[from] std::string::FromUtf8Error),
    #[error("read-only pane snapshot exceeds {limit_name} limit {limit}: observed {observed}")]
    PaneSnapshotLimitExceeded {
        limit_name: &'static str,
        limit: u64,
        observed: u64,
    },
    #[error(
        "pane sequence journal capacity {limit} bytes would be exceeded: attempted {attempted} bytes"
    )]
    PaneSequenceJournalFull { limit: u64, attempted: u64 },
    #[error("deterministic versioned pane ledger identity collides with different content")]
    VersionedPaneIdentityCollision,
    #[error("staged pane ledger failed exact reopen verification")]
    StagedPaneVerificationFailed,
}

const COLD_ERASURE_DATA_SHARDS: usize = 3;
const COLD_ERASURE_TOTAL_SHARDS: usize = 5;
const COLD_ERASURE_VERSION: u8 = 1;
const COLD_ERASURE_MAGIC: [u8; 8] = *b"FTRSLOG1";
const COLD_ERASURE_HEADER_LEN: usize = 8 + 1 + 1 + 1 + 1 + 8 + 4 + 4 + 4;
const COLD_ERASURE_MAX_SHARD_BYTES: u64 = 512 * 1024 * 1024;
const PANE_LOG_HEADER_PREFIX: &[u8] = b"\0FTMMAP1:";
const PANE_BASE_SEQ_JOURNAL_PREFIX: &str = "FTSEQ1:";
const PANE_LOG_MAX_RECORD_BYTES: u64 = 32 * 1024 * 1024;
const PANE_BASE_SEQ_JOURNAL_MAX_BYTES: u64 = 1024 * 1024;
#[cfg(not(test))]
const PANE_BASE_SEQ_JOURNAL_COMPACT_BYTES: u64 = PANE_BASE_SEQ_JOURNAL_MAX_BYTES / 4 * 3;
#[cfg(test)]
const PANE_BASE_SEQ_JOURNAL_COMPACT_BYTES: u64 = 512;
const GF_PRIM: u32 = 0x11d;

/// An immutable, bounded view of one pane log at a stable filesystem identity.
///
/// The snapshot excludes a final non-newline-terminated record because an
/// append acknowledges durability only after writing its delimiter. Opening a
/// snapshot never creates, truncates, repairs, or changes permissions on the
/// source log or sequence journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmapPaneReadSnapshot {
    pub oldest_seq: Option<u64>,
    pub next_seq: u64,
    pub records: Vec<String>,
    pub retained_record_bytes: u64,
    pub committed_bytes: u64,
    pub sequence_bytes: u64,
    pub physical_bytes: u64,
    pub trailing_uncommitted_bytes: u64,
}

/// Fully synchronized, exact replacement ledger that is not yet reachable
/// through its caller-owned publication manifest.
///
/// The ledger ID is nonsecret. Row content remains on private files and is
/// never exposed through `Debug`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MmapStagedPaneLedger {
    pane_id: PaneId,
    record_count: usize,
    record_bytes: u64,
    committed_bytes: u64,
    reused_existing: bool,
}

impl MmapStagedPaneLedger {
    #[must_use]
    pub const fn pane_id(self) -> PaneId {
        self.pane_id
    }

    #[must_use]
    pub const fn record_count(self) -> usize {
        self.record_count
    }

    #[must_use]
    pub const fn record_bytes(self) -> u64 {
        self.record_bytes
    }

    #[must_use]
    pub const fn committed_bytes(self) -> u64 {
        self.committed_bytes
    }

    #[must_use]
    pub const fn reused_existing(self) -> bool {
        self.reused_existing
    }
}

impl std::fmt::Debug for MmapStagedPaneLedger {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MmapStagedPaneLedger")
            .field("pane_id", &self.pane_id)
            .field("record_count", &self.record_count)
            .field("record_bytes", &self.record_bytes)
            .field("committed_bytes", &self.committed_bytes)
            .field("reused_existing", &self.reused_existing)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ColdErasureShard {
    index: u8,
    original_len: u64,
    payload_crc32: u32,
    bytes: Vec<u8>,
}

struct GfTables {
    exp: [u8; 512],
    log: [u8; 256],
}

impl GfTables {
    fn new() -> Self {
        let mut tables = Self {
            exp: [0; 512],
            log: [0; 256],
        };
        let mut value = 1u32;
        for idx in 0..255 {
            tables.exp[idx] = value as u8;
            tables.log[value as usize] = idx as u8;
            value <<= 1;
            if value & 0x100 != 0 {
                value ^= GF_PRIM;
            }
        }
        for idx in 255..512 {
            tables.exp[idx] = tables.exp[idx - 255];
        }
        tables
    }
}

fn gf_tables() -> &'static GfTables {
    static TABLES: std::sync::OnceLock<GfTables> = std::sync::OnceLock::new();
    TABLES.get_or_init(GfTables::new)
}

#[inline]
fn gf_add(lhs: u8, rhs: u8) -> u8 {
    lhs ^ rhs
}

#[inline]
fn gf_mul(lhs: u8, rhs: u8) -> u8 {
    if lhs == 0 || rhs == 0 {
        return 0;
    }
    let tables = gf_tables();
    let log_sum = tables.log[lhs as usize] as usize + tables.log[rhs as usize] as usize;
    tables.exp[log_sum]
}

#[inline]
fn gf_inv(value: u8) -> Result<u8, MmapStoreError> {
    if value == 0 {
        return Err(MmapStoreError::InvalidErasureShard(
            "singular erasure matrix".to_string(),
        ));
    }
    let tables = gf_tables();
    Ok(tables.exp[(255 - tables.log[value as usize] as usize) % 255])
}

#[inline]
fn gf_pow(value: u8, power: usize) -> u8 {
    if power == 0 {
        return 1;
    }
    if value == 0 {
        return 0;
    }
    let tables = gf_tables();
    tables.exp[(tables.log[value as usize] as usize * power) % 255]
}

fn cold_erasure_generator_row(index: u8) -> Result<[u8; COLD_ERASURE_DATA_SHARDS], MmapStoreError> {
    let index_usize = usize::from(index);
    if index_usize >= COLD_ERASURE_TOTAL_SHARDS {
        return Err(MmapStoreError::InvalidErasureShard(format!(
            "shard index {index} out of range"
        )));
    }
    let mut row = [0u8; COLD_ERASURE_DATA_SHARDS];
    if index_usize < COLD_ERASURE_DATA_SHARDS {
        row[index_usize] = 1;
        return Ok(row);
    }

    let eval = index - u8::try_from(COLD_ERASURE_DATA_SHARDS).unwrap_or(0) + 1;
    for (column, coeff) in row.iter_mut().enumerate() {
        *coeff = gf_pow(eval, column);
    }
    Ok(row)
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn cold_erasure_encode(data: &[u8]) -> Result<Vec<ColdErasureShard>, MmapStoreError> {
    let original_len =
        u64::try_from(data.len()).map_err(|_| MmapStoreError::NumericOverflow("erasure_len"))?;
    let chunk_size = data.len().div_ceil(COLD_ERASURE_DATA_SHARDS);
    let padded_len = chunk_size
        .checked_mul(COLD_ERASURE_DATA_SHARDS)
        .ok_or(MmapStoreError::NumericOverflow("erasure_padded_len"))?;
    let mut padded = Vec::new();
    padded
        .try_reserve_exact(padded_len)
        .map_err(|error| MmapStoreError::InvalidErasureShard(error.to_string()))?;
    padded.extend_from_slice(data);
    padded.resize(padded_len, 0);
    let payload_crc32 = crc32_ieee(data);

    let mut shards = Vec::with_capacity(COLD_ERASURE_TOTAL_SHARDS);
    for index in 0..COLD_ERASURE_TOTAL_SHARDS {
        let row = cold_erasure_generator_row(
            u8::try_from(index).map_err(|_| MmapStoreError::NumericOverflow("shard_index"))?,
        )?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(chunk_size)
            .map_err(|error| MmapStoreError::InvalidErasureShard(error.to_string()))?;
        bytes.resize(chunk_size, 0);
        for byte_offset in 0..chunk_size {
            let mut acc = 0u8;
            for column in 0..COLD_ERASURE_DATA_SHARDS {
                let source = padded[column * chunk_size + byte_offset];
                acc = gf_add(acc, gf_mul(row[column], source));
            }
            bytes[byte_offset] = acc;
        }
        shards.push(ColdErasureShard {
            index: u8::try_from(index)
                .map_err(|_| MmapStoreError::NumericOverflow("shard_index"))?,
            original_len,
            payload_crc32,
            bytes,
        });
    }
    Ok(shards)
}

fn cold_erasure_decode(shards: &[ColdErasureShard]) -> Result<Vec<u8>, MmapStoreError> {
    if shards.len() < COLD_ERASURE_DATA_SHARDS {
        return Err(MmapStoreError::InsufficientErasureShards {
            have: shards.len(),
            need: COLD_ERASURE_DATA_SHARDS,
        });
    }

    let used = &shards[..COLD_ERASURE_DATA_SHARDS];
    let shard_len = used[0].bytes.len();
    let original_len = used[0].original_len;
    let payload_crc32 = used[0].payload_crc32;
    let mut seen = [false; COLD_ERASURE_TOTAL_SHARDS];
    let mut matrix = [[0u8; COLD_ERASURE_DATA_SHARDS]; COLD_ERASURE_DATA_SHARDS];

    for (row_index, shard) in used.iter().enumerate() {
        let index = usize::from(shard.index);
        if index >= COLD_ERASURE_TOTAL_SHARDS {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "shard index {} out of range",
                shard.index
            )));
        }
        if seen[index] {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "duplicate shard index {}",
                shard.index
            )));
        }
        seen[index] = true;
        if shard.bytes.len() != shard_len {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "shard {} length {} != {shard_len}",
                shard.index,
                shard.bytes.len()
            )));
        }
        if shard.original_len != original_len || shard.payload_crc32 != payload_crc32 {
            return Err(MmapStoreError::InvalidErasureShard(
                "erasure shard metadata mismatch".to_string(),
            ));
        }
        matrix[row_index] = cold_erasure_generator_row(shard.index)?;
    }

    let inverse = invert_erasure_matrix(matrix)?;
    let padded_len = shard_len
        .checked_mul(COLD_ERASURE_DATA_SHARDS)
        .ok_or(MmapStoreError::NumericOverflow("erasure_padded_len"))?;
    let mut padded = Vec::new();
    padded
        .try_reserve_exact(padded_len)
        .map_err(|error| MmapStoreError::InvalidErasureShard(error.to_string()))?;
    padded.resize(padded_len, 0);
    for byte_offset in 0..shard_len {
        for original_column in 0..COLD_ERASURE_DATA_SHARDS {
            let mut acc = 0u8;
            for source_row in 0..COLD_ERASURE_DATA_SHARDS {
                acc = gf_add(
                    acc,
                    gf_mul(
                        inverse[original_column][source_row],
                        used[source_row].bytes[byte_offset],
                    ),
                );
            }
            padded[original_column * shard_len + byte_offset] = acc;
        }
    }

    let original_len = usize::try_from(original_len)
        .map_err(|_| MmapStoreError::NumericOverflow("erasure_original_len"))?;
    if original_len > padded.len() {
        return Err(MmapStoreError::InvalidErasureShard(format!(
            "original length {original_len} exceeds decoded payload {}",
            padded.len()
        )));
    }
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(original_len)
        .map_err(|error| MmapStoreError::InvalidErasureShard(error.to_string()))?;
    decoded.extend_from_slice(&padded[..original_len]);
    if crc32_ieee(&decoded) != payload_crc32 {
        return Err(MmapStoreError::ErasurePayloadCrcMismatch);
    }
    Ok(decoded)
}

fn invert_erasure_matrix(
    matrix: [[u8; COLD_ERASURE_DATA_SHARDS]; COLD_ERASURE_DATA_SHARDS],
) -> Result<[[u8; COLD_ERASURE_DATA_SHARDS]; COLD_ERASURE_DATA_SHARDS], MmapStoreError> {
    let mut augmented = [[0u8; COLD_ERASURE_DATA_SHARDS * 2]; COLD_ERASURE_DATA_SHARDS];
    for row in 0..COLD_ERASURE_DATA_SHARDS {
        for column in 0..COLD_ERASURE_DATA_SHARDS {
            augmented[row][column] = matrix[row][column];
        }
        augmented[row][COLD_ERASURE_DATA_SHARDS + row] = 1;
    }

    for column in 0..COLD_ERASURE_DATA_SHARDS {
        let pivot = (column..COLD_ERASURE_DATA_SHARDS)
            .find(|row| augmented[*row][column] != 0)
            .ok_or_else(|| {
                MmapStoreError::InvalidErasureShard("singular erasure matrix".to_string())
            })?;
        if pivot != column {
            augmented.swap(column, pivot);
        }

        let pivot_inv = gf_inv(augmented[column][column])?;
        for value in &mut augmented[column] {
            *value = gf_mul(*value, pivot_inv);
        }

        let pivot_row = augmented[column];
        for (row, row_values) in augmented
            .iter_mut()
            .enumerate()
            .take(COLD_ERASURE_DATA_SHARDS)
        {
            if row == column {
                continue;
            }
            let factor = row_values[column];
            if factor == 0 {
                continue;
            }
            for (target, pivot_value) in row_values.iter_mut().zip(pivot_row.iter()) {
                *target = gf_add(*target, gf_mul(factor, *pivot_value));
            }
        }
    }

    let mut inverse = [[0u8; COLD_ERASURE_DATA_SHARDS]; COLD_ERASURE_DATA_SHARDS];
    for row in 0..COLD_ERASURE_DATA_SHARDS {
        for column in 0..COLD_ERASURE_DATA_SHARDS {
            inverse[row][column] = augmented[row][COLD_ERASURE_DATA_SHARDS + column];
        }
    }
    Ok(inverse)
}

impl ColdErasureShard {
    fn encode_bytes(&self) -> Result<Vec<u8>, MmapStoreError> {
        let shard_len = u32::try_from(self.bytes.len())
            .map_err(|_| MmapStoreError::NumericOverflow("erasure_shard_len"))?;
        let encoded_len = COLD_ERASURE_HEADER_LEN
            .checked_add(self.bytes.len())
            .ok_or(MmapStoreError::NumericOverflow("erasure_encoded_len"))?;
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(encoded_len)
            .map_err(|error| MmapStoreError::InvalidErasureShard(error.to_string()))?;
        encoded.extend_from_slice(&COLD_ERASURE_MAGIC);
        encoded.push(COLD_ERASURE_VERSION);
        encoded.push(u8::try_from(COLD_ERASURE_DATA_SHARDS).unwrap_or(0));
        encoded.push(u8::try_from(COLD_ERASURE_TOTAL_SHARDS).unwrap_or(0));
        encoded.push(self.index);
        encoded.extend_from_slice(&self.original_len.to_le_bytes());
        encoded.extend_from_slice(&self.payload_crc32.to_le_bytes());
        encoded.extend_from_slice(&crc32_ieee(&self.bytes).to_le_bytes());
        encoded.extend_from_slice(&shard_len.to_le_bytes());
        encoded.extend_from_slice(&self.bytes);
        Ok(encoded)
    }

    fn decode_bytes(encoded: &[u8]) -> Result<Self, MmapStoreError> {
        if encoded.len() < COLD_ERASURE_HEADER_LEN {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "short shard header: {} bytes",
                encoded.len()
            )));
        }
        if encoded[..8] != COLD_ERASURE_MAGIC {
            return Err(MmapStoreError::InvalidErasureShard(
                "bad shard magic".to_string(),
            ));
        }
        let version = encoded[8];
        if version != COLD_ERASURE_VERSION {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "unsupported shard version {version}"
            )));
        }
        let k = encoded[9];
        let n = encoded[10];
        if usize::from(k) != COLD_ERASURE_DATA_SHARDS || usize::from(n) != COLD_ERASURE_TOTAL_SHARDS
        {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "unsupported erasure geometry k={k} n={n}"
            )));
        }
        let index = encoded[11];
        if usize::from(index) >= COLD_ERASURE_TOTAL_SHARDS {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "shard index {index} out of range"
            )));
        }

        let original_len = u64::from_le_bytes(
            encoded[12..20]
                .try_into()
                .map_err(|_| MmapStoreError::InvalidErasureShard("bad length".to_string()))?,
        );
        let payload_crc32 = u32::from_le_bytes(
            encoded[20..24]
                .try_into()
                .map_err(|_| MmapStoreError::InvalidErasureShard("bad payload crc".to_string()))?,
        );
        let shard_crc32 = u32::from_le_bytes(
            encoded[24..28]
                .try_into()
                .map_err(|_| MmapStoreError::InvalidErasureShard("bad shard crc".to_string()))?,
        );
        let shard_len = u32::from_le_bytes(
            encoded[28..32]
                .try_into()
                .map_err(|_| MmapStoreError::InvalidErasureShard("bad shard len".to_string()))?,
        ) as usize;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(encoded.len() - COLD_ERASURE_HEADER_LEN)
            .map_err(|error| MmapStoreError::InvalidErasureShard(error.to_string()))?;
        bytes.extend_from_slice(&encoded[COLD_ERASURE_HEADER_LEN..]);
        if bytes.len() != shard_len {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "shard {index} length {} != header {shard_len}",
                bytes.len()
            )));
        }
        if crc32_ieee(&bytes) != shard_crc32 {
            return Err(MmapStoreError::ErasureShardCrcMismatch { index });
        }
        Ok(Self {
            index,
            original_len,
            payload_crc32,
            bytes,
        })
    }
}

fn cold_erasure_shard_path(log_path: &Path, index: u8) -> Result<PathBuf, MmapStoreError> {
    let mut path = log_path.to_path_buf();
    let name = log_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            MmapStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "scrollback log path has no file name",
            ))
        })?;
    path.set_file_name(format!("{name}.rs{index:02}"));
    Ok(path)
}

/// In-memory per-pane index and file handle.
#[derive(Debug)]
struct PaneFile {
    log_path: PathBuf,
    file: File,
    base_seq_file: File,
    /// End offset of the last newline-terminated, crash-durable record.
    file_len: u64,
    /// The physical file contains an interrupted record after `file_len`.
    /// Recovery truncates only that uncommitted suffix before the next append.
    trailing_partial: bool,
    /// Byte length of the optional in-band sequence-authority header.
    data_start: u64,
    base_seq: u64,
    line_offsets: Vec<LineOffset>,
}

impl PaneFile {
    fn create_new_private_append_file(path: &Path) -> Result<Option<File>, MmapStoreError> {
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(path) {
            Ok(file) => {
                Self::harden_open_file_permissions(&file)?;
                Self::revalidate_open_file(path, &file)?;
                Ok(Some(file))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn create_versioned(base_dir: &Path, pane_id: PaneId) -> Result<Option<Self>, MmapStoreError> {
        let log_path = base_dir.join(format!("{pane_id}.log"));
        let Some(file) = Self::create_new_private_append_file(&log_path)? else {
            return Ok(None);
        };
        let base_seq_path = Self::base_seq_path(&log_path);
        let Some(base_seq_file) = Self::create_new_private_append_file(&base_seq_path)? else {
            // The new empty log is intentionally retained as an unreachable
            // diagnostic artifact. Synchronize it before refusing this
            // colliding identity; callers never delete uncertain artifacts.
            file.sync_all()?;
            Self::sync_parent_directory(&log_path)?;
            return Ok(None);
        };
        file.sync_all()?;
        base_seq_file.sync_all()?;
        Self::sync_parent_directory(&log_path)?;
        Ok(Some(Self {
            log_path,
            file,
            base_seq_file,
            file_len: 0,
            trailing_partial: false,
            data_start: 0,
            base_seq: 0,
            line_offsets: Vec::new(),
        }))
    }

    fn unique_staging_path(path: &Path, purpose: &str) -> Result<PathBuf, MmapStoreError> {
        static STAGING_SEQUENCE: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);

        let parent = path.parent().ok_or_else(|| {
            MmapStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "pane storage path has no parent directory",
            ))
        })?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                MmapStoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "pane storage path has no UTF-8 file name",
                ))
            })?;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| {
                MmapStoreError::Io(std::io::Error::other(format!(
                    "system clock precedes Unix epoch: {error}"
                )))
            })?
            .as_nanos();
        let sequence = STAGING_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(parent.join(format!(
            ".{name}.{purpose}-{}-{timestamp}-{sequence}",
            std::process::id()
        )))
    }

    fn publish_staging_file(staging: &Path, destination: &Path) -> Result<(), MmapStoreError> {
        let staged_path = tempfile::TempPath::try_from_path(staging.to_path_buf())?;
        match staged_path.persist(destination) {
            Ok(()) => Ok(()),
            Err(mut error) => {
                // The caller did not authorize deleting an unpublished recovery
                // artifact. Retain it for diagnosis if atomic publication fails.
                error.path.disable_cleanup(true);
                Err(error.error.into())
            }
        }
    }

    fn harden_open_file_permissions(file: &File) -> Result<(), MmapStoreError> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }

    fn revalidate_open_file(path: &Path, file: &File) -> Result<(), MmapStoreError> {
        let handle_metadata = file.metadata()?;
        if !handle_metadata.is_file() {
            return Err(MmapStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "pane storage authority is not a regular file",
            )));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            let path_metadata = std::fs::symlink_metadata(path)?;
            if !path_metadata.file_type().is_file()
                || path_metadata.dev() != handle_metadata.dev()
                || path_metadata.ino() != handle_metadata.ino()
            {
                return Err(MmapStoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "pane storage authority changed identity while opening",
                )));
            }
        }
        Ok(())
    }

    fn metadata_changed(
        before: &std::fs::Metadata,
        after: &std::fs::Metadata,
    ) -> Result<bool, MmapStoreError> {
        if before.len() != after.len() || before.modified()? != after.modified()? {
            return Ok(true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            if before.dev() != after.dev()
                || before.ino() != after.ino()
                || before.ctime() != after.ctime()
                || before.ctime_nsec() != after.ctime_nsec()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn open_append_file(path: &Path) -> Result<(File, bool), MmapStoreError> {
        let mut create = OpenOptions::new();
        create.create_new(true).read(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            create.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match create.open(path) {
            Ok(file) => {
                Self::harden_open_file_permissions(&file)?;
                Self::revalidate_open_file(path, &file)?;
                Ok((file, true))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let mut existing = OpenOptions::new();
                existing.read(true).append(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;

                    existing.custom_flags(libc::O_NOFOLLOW);
                }
                let file = existing.open(path)?;
                Self::harden_open_file_permissions(&file)?;
                Self::revalidate_open_file(path, &file)?;
                Ok((file, false))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn sync_parent_directory(path: &Path) -> Result<(), MmapStoreError> {
        #[cfg(not(windows))]
        {
            let parent = path.parent().ok_or_else(|| {
                MmapStoreError::Io(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "pane storage path has no parent directory",
                ))
            })?;
            File::open(parent)?.sync_all()?;
        }
        #[cfg(windows)]
        {
            let _ = path;
        }
        Ok(())
    }

    fn base_seq_path(log_path: &Path) -> PathBuf {
        log_path.with_extension("seq")
    }

    fn scan_base_seq_journal(file: &File) -> Result<Option<u64>, MmapStoreError> {
        let mut file = file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut reader = BufReader::new(file.take(PANE_BASE_SEQ_JOURNAL_MAX_BYTES + 1));
        let mut latest = None;
        let mut record = Vec::new();
        let mut total_bytes = 0u64;
        loop {
            let bytes_read = (&mut reader).take(129).read_until(b'\n', &mut record)?;
            if bytes_read == 0 {
                break;
            }
            total_bytes = total_bytes
                .checked_add(
                    u64::try_from(bytes_read)
                        .map_err(|_| MmapStoreError::NumericOverflow("sequence_journal_bytes"))?,
                )
                .ok_or(MmapStoreError::NumericOverflow("sequence_journal_bytes"))?;
            if total_bytes > PANE_BASE_SEQ_JOURNAL_MAX_BYTES {
                return Err(MmapStoreError::InvalidPaneLogHeader(format!(
                    "base sequence journal exceeds {PANE_BASE_SEQ_JOURNAL_MAX_BYTES} bytes"
                )));
            }
            if record.len() > 128 {
                return Err(MmapStoreError::InvalidPaneLogHeader(
                    "base sequence journal record exceeds 128 bytes".to_string(),
                ));
            }
            if record.ends_with(b"\n") {
                let value = record
                    .strip_prefix(PANE_BASE_SEQ_JOURNAL_PREFIX.as_bytes())
                    .and_then(|bytes| bytes.strip_suffix(b"\n"))
                    .ok_or_else(|| {
                        MmapStoreError::InvalidPaneLogHeader(
                            "malformed base sequence journal record".to_string(),
                        )
                    })?;
                let value = std::str::from_utf8(value)
                    .map_err(|error| MmapStoreError::InvalidPaneLogHeader(error.to_string()))?;
                let value = value
                    .parse::<u64>()
                    .map_err(|error| MmapStoreError::InvalidPaneLogHeader(error.to_string()))?;
                if latest.is_some_and(|previous| value < previous) {
                    return Err(MmapStoreError::InvalidPaneLogHeader(
                        "base sequence journal is not monotonic".to_string(),
                    ));
                }
                latest = Some(value);
            }
            record.clear();
        }
        Ok(latest)
    }

    fn scan_offsets_and_base(
        file: &File,
    ) -> Result<(Vec<LineOffset>, u64, u64, u64), MmapStoreError> {
        Self::scan_offsets_and_base_bounded(file, None, None)
    }

    fn scan_offsets_and_base_bounded(
        file: &File,
        max_records: Option<usize>,
        max_physical_bytes: Option<u64>,
    ) -> Result<(Vec<LineOffset>, u64, u64, u64), MmapStoreError> {
        let mut file = file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let read_limit = max_physical_bytes.map_or(u64::MAX, |limit| limit.saturating_add(1));
        let mut reader = BufReader::new(file.take(read_limit));
        let mut line_offsets = Vec::new();
        let mut cursor = 0u64;
        let mut committed_len = 0u64;
        let mut base_seq = 0u64;
        let mut data_start = 0u64;
        let mut line_buf = Vec::new();

        loop {
            let bytes_read = (&mut reader)
                .take(PANE_LOG_MAX_RECORD_BYTES.saturating_add(1))
                .read_until(b'\n', &mut line_buf)?;
            if bytes_read == 0 {
                break;
            }
            if u64::try_from(line_buf.len()).unwrap_or(u64::MAX) > PANE_LOG_MAX_RECORD_BYTES {
                return Err(MmapStoreError::InvalidPaneLogHeader(format!(
                    "pane log record exceeds {PANE_LOG_MAX_RECORD_BYTES} bytes"
                )));
            }
            let bytes_read = u64::try_from(bytes_read)
                .map_err(|_| MmapStoreError::NumericOverflow("pane_log_cursor"))?;
            let next_cursor = cursor
                .checked_add(bytes_read)
                .ok_or(MmapStoreError::NumericOverflow("pane_log_cursor"))?;
            if let Some(limit) = max_physical_bytes
                && next_cursor > limit
            {
                return Err(MmapStoreError::PaneSnapshotLimitExceeded {
                    limit_name: "physical_bytes",
                    limit,
                    observed: next_cursor,
                });
            }
            let pane_header_candidate = cursor == 0 && line_buf.starts_with(b"\0FTMMAP");
            if line_buf.ends_with(b"\n") {
                if cursor == 0 && line_buf.starts_with(PANE_LOG_HEADER_PREFIX) {
                    let value = line_buf
                        .strip_prefix(PANE_LOG_HEADER_PREFIX)
                        .and_then(|bytes| bytes.strip_suffix(b"\n"))
                        .ok_or_else(|| {
                            MmapStoreError::InvalidPaneLogHeader(
                                "missing newline-terminated base sequence".to_string(),
                            )
                        })?;
                    let value = std::str::from_utf8(value)
                        .map_err(|error| MmapStoreError::InvalidPaneLogHeader(error.to_string()))?;
                    base_seq = value
                        .parse::<u64>()
                        .map_err(|error| MmapStoreError::InvalidPaneLogHeader(error.to_string()))?;
                    data_start = next_cursor;
                } else if pane_header_candidate {
                    return Err(MmapStoreError::InvalidPaneLogHeader(
                        "unsupported or malformed pane log header".to_string(),
                    ));
                } else {
                    if let Some(limit) = max_records {
                        if line_offsets.len() >= limit {
                            return Err(MmapStoreError::PaneSnapshotLimitExceeded {
                                limit_name: "records",
                                limit: u64::try_from(limit).unwrap_or(u64::MAX),
                                observed: u64::try_from(line_offsets.len())
                                    .unwrap_or(u64::MAX)
                                    .saturating_add(1),
                            });
                        }
                    }
                    line_offsets.push(LineOffset(cursor));
                }
                committed_len = next_cursor;
            } else if pane_header_candidate {
                return Err(MmapStoreError::InvalidPaneLogHeader(
                    "torn pane log header".to_string(),
                ));
            }
            cursor = next_cursor;
            line_buf.clear();
        }

        Ok((line_offsets, committed_len, base_seq, data_start))
    }

    #[cfg(test)]
    fn scan_offsets(path: &Path) -> Result<(Vec<LineOffset>, u64), MmapStoreError> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;

            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        let (offsets, committed_len, _base_seq, _data_start) = Self::scan_offsets_and_base(&file)?;
        Ok((offsets, committed_len))
    }

    fn open(base_dir: &Path, pane_id: PaneId) -> Result<Self, MmapStoreError> {
        let log_path = base_dir.join(format!("{pane_id}.log"));
        let base_seq_path = Self::base_seq_path(&log_path);
        let (file, log_created) = Self::open_append_file(&log_path)?;
        let (mut line_offsets, file_len, log_base_seq, data_start) =
            Self::scan_offsets_and_base(&file)?;
        let trailing_partial = file.metadata()?.len() > file_len;
        let (base_seq_file, base_seq_created) = Self::open_append_file(&base_seq_path)?;
        if log_created || base_seq_created {
            Self::sync_parent_directory(&log_path)?;
        }
        let journal_base_seq = if file_len == 0 {
            // An empty synchronized content log is authoritative for clear.
            // Reset any journal value left by a crash between clearing the log
            // and clearing its sequence sidecar.
            base_seq_file.set_len(0)?;
            base_seq_file.sync_data()?;
            None
        } else {
            Self::scan_base_seq_journal(&base_seq_file)?
        };
        let base_seq = journal_base_seq.map_or(log_base_seq, |seq| seq.max(log_base_seq));
        let logically_pruned = base_seq.saturating_sub(log_base_seq);
        let logically_pruned = usize::try_from(logically_pruned)
            .map_err(|_| MmapStoreError::NumericOverflow("logically_pruned"))?;
        if logically_pruned > line_offsets.len() {
            return Err(MmapStoreError::InvalidPaneLogHeader(format!(
                "base sequence journal prunes {logically_pruned} records from a log containing {}",
                line_offsets.len()
            )));
        }
        line_offsets.drain(0..logically_pruned);

        Ok(Self {
            log_path,
            file,
            base_seq_file,
            file_len,
            trailing_partial,
            data_start,
            base_seq,
            line_offsets,
        })
    }

    fn persist_base_seq(&mut self, base_seq: u64) -> Result<(), MmapStoreError> {
        let record = format!("{PANE_BASE_SEQ_JOURNAL_PREFIX}{base_seq}\n");
        let current_len = self.base_seq_file.seek(SeekFrom::End(0))?;
        let attempted = current_len
            .checked_add(
                u64::try_from(record.len())
                    .map_err(|_| MmapStoreError::NumericOverflow("sequence_journal_bytes"))?,
            )
            .ok_or(MmapStoreError::NumericOverflow("sequence_journal_bytes"))?;
        if attempted > PANE_BASE_SEQ_JOURNAL_MAX_BYTES {
            return Err(MmapStoreError::PaneSequenceJournalFull {
                limit: PANE_BASE_SEQ_JOURNAL_MAX_BYTES,
                attempted,
            });
        }
        self.base_seq_file.write_all(record.as_bytes())?;
        self.base_seq_file.flush()?;
        self.base_seq_file.sync_data()?;
        Ok(())
    }

    fn append_line(&mut self, line: &str) -> Result<u64, MmapStoreError> {
        if line
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b'\n' | b'\r'))
        {
            return Err(MmapStoreError::InvalidLineRecord);
        }
        let record_bytes = u64::try_from(line.len())
            .map_err(|_| MmapStoreError::NumericOverflow("line_len"))?
            .checked_add(1)
            .ok_or(MmapStoreError::NumericOverflow("line_len"))?;
        if record_bytes > PANE_LOG_MAX_RECORD_BYTES {
            return Err(MmapStoreError::PaneLogRecordTooLarge {
                bytes: record_bytes,
                max: PANE_LOG_MAX_RECORD_BYTES,
            });
        }
        let mut physical_end = self.file.seek(SeekFrom::End(0))?;
        if self.trailing_partial {
            // The suffix after `file_len` was never newline-terminated and
            // therefore was never acknowledged as a record. Remove exactly
            // that uncommitted tail before appending; otherwise a later reopen
            // would index the tail as a real line once a separator was added.
            self.file.set_len(self.file_len)?;
            self.file.sync_all()?;
            physical_end = self.file_len;
            self.trailing_partial = false;
        }
        let start = physical_end;
        let seq = self
            .base_seq
            .checked_add(
                u64::try_from(self.line_offsets.len())
                    .map_err(|_| MmapStoreError::NumericOverflow("line_count"))?,
            )
            .ok_or(MmapStoreError::NumericOverflow("seq"))?;
        let new_file_len = start
            .checked_add(
                u64::try_from(line.len())
                    .map_err(|_| MmapStoreError::NumericOverflow("line_len"))?,
            )
            .and_then(|len| len.checked_add(1))
            .ok_or(MmapStoreError::NumericOverflow("file_len"))?;
        // Publish the interrupted-tail state before the first write. If any
        // write or durability operation fails, readers keep the prior
        // committed boundary and a later append starts on a fresh line.
        self.trailing_partial = true;
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        // A successful spill is a crash-durability acknowledgement, not just
        // a userspace-buffer acknowledgement. `flush` alone can leave the
        // newest retained line entirely in the kernel page cache when the
        // host or mux process dies. Persist the appended data before making it
        // visible through the in-memory index and returning success.
        self.file.flush()?;
        self.file.sync_data()?;
        self.line_offsets.push(LineOffset(start));
        self.file_len = new_file_len;
        self.trailing_partial = false;
        Ok(seq)
    }

    fn tail_lines(&self, n: usize) -> Result<Vec<String>, MmapStoreError> {
        if n == 0 {
            return Ok(Vec::new());
        }
        if self.line_offsets.is_empty() {
            return Ok(Vec::new());
        }

        let line_count = self.line_offsets.len();
        let start_index = line_count.saturating_sub(n);
        let mut lines = Vec::with_capacity(line_count - start_index);
        for index in start_index..line_count {
            let seq = self
                .base_seq
                .checked_add(
                    u64::try_from(index)
                        .map_err(|_| MmapStoreError::NumericOverflow("line_index"))?,
                )
                .ok_or(MmapStoreError::NumericOverflow("seq"))?;
            if let Some(line) = self.line_at(seq)? {
                lines.push(line);
            }
        }
        Ok(lines)
    }

    fn line_at(&self, seq: u64) -> Result<Option<String>, MmapStoreError> {
        if seq < self.base_seq {
            return Ok(None);
        }
        let index = usize::try_from(seq - self.base_seq)
            .map_err(|_| MmapStoreError::NumericOverflow("line_index"))?;
        let Some(start) = self.line_offsets.get(index).copied() else {
            return Ok(None);
        };
        if start.0 > self.file_len {
            return Err(MmapStoreError::OffsetOutOfBounds {
                offset: start.0,
                len: self.file_len,
            });
        }

        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(start.0))?;
        let readable = self
            .file_len
            .saturating_sub(start.0)
            .min(PANE_LOG_MAX_RECORD_BYTES.saturating_add(1));
        let mut reader = BufReader::new(file.take(readable));
        let mut bytes = Vec::new();
        reader.read_until(b'\n', &mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > PANE_LOG_MAX_RECORD_BYTES {
            return Err(MmapStoreError::InvalidPaneLogHeader(format!(
                "pane log record exceeds {PANE_LOG_MAX_RECORD_BYTES} bytes"
            )));
        }
        if !bytes.ends_with(b"\n") {
            return Ok(None);
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        Ok(Some(String::from_utf8(bytes)?))
    }

    fn prune_before(&mut self, seq: u64) -> Result<(), MmapStoreError> {
        if seq <= self.base_seq {
            return Ok(());
        }
        let drop_count = usize::try_from(seq - self.base_seq)
            .unwrap_or(usize::MAX)
            .min(self.line_offsets.len());
        let next_base_seq = self
            .base_seq
            .checked_add(
                u64::try_from(drop_count)
                    .map_err(|_| MmapStoreError::NumericOverflow("drop_count"))?,
            )
            .ok_or(MmapStoreError::NumericOverflow("base_seq"))?;
        if next_base_seq == self.base_seq {
            return Ok(());
        }
        // Persist the logical prune before changing the in-memory index. A
        // crash after this acknowledgement replays the same prefix drop when
        // the pane log is reopened, even when byte compaction has not run.
        self.persist_base_seq(next_base_seq)?;
        self.line_offsets.drain(0..drop_count);
        self.base_seq = next_base_seq;
        Ok(())
    }

    fn clear(&mut self) -> Result<(), MmapStoreError> {
        self.file.set_len(0)?;
        self.file.flush()?;
        self.file.sync_data()?;
        self.file_len = 0;
        self.trailing_partial = false;
        self.data_start = 0;
        self.base_seq = 0;
        self.line_offsets.clear();
        self.base_seq_file.set_len(0)?;
        self.base_seq_file.flush()?;
        self.base_seq_file.sync_data()?;
        Ok(())
    }

    fn read_all_bytes(&self) -> Result<Vec<u8>, MmapStoreError> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.take(self.file_len).read_to_end(&mut bytes)?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != self.file_len {
            return Err(MmapStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "pane log changed while reading committed bytes",
            )));
        }
        Ok(bytes)
    }

    fn write_erasure_sidecars(&self) -> Result<(), MmapStoreError> {
        let bytes = self.read_all_bytes()?;
        let shards = cold_erasure_encode(&bytes)?;
        for shard in shards {
            let path = cold_erasure_shard_path(&self.log_path, shard.index)?;
            let encoded = shard.encode_bytes()?;
            if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > COLD_ERASURE_MAX_SHARD_BYTES {
                return Err(MmapStoreError::InvalidErasureShard(format!(
                    "encoded shard {} exceeds the {} byte safety limit",
                    shard.index, COLD_ERASURE_MAX_SHARD_BYTES
                )));
            }
            let tmp_path = Self::unique_staging_path(&path, "erasure-installing")?;
            {
                let mut options = OpenOptions::new();
                options.create_new(true).write(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt as _;

                    options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
                }
                let mut file = options.open(&tmp_path)?;
                Self::harden_open_file_permissions(&file)?;
                file.write_all(&encoded)?;
                file.sync_all()?;
                Self::revalidate_open_file(&tmp_path, &file)?;
                Self::publish_staging_file(&tmp_path, &path)?;
                Self::revalidate_open_file(&path, &file)?;
            }
        }
        Self::sync_parent_directory(&self.log_path)?;
        Ok(())
    }

    fn recover_from_erasure_sidecars(&self) -> Result<Vec<u8>, MmapStoreError> {
        let mut shards = Vec::new();
        let mut invalid_shards = Vec::new();
        for index in 0..COLD_ERASURE_TOTAL_SHARDS {
            let index =
                u8::try_from(index).map_err(|_| MmapStoreError::NumericOverflow("shard_index"))?;
            let path = cold_erasure_shard_path(&self.log_path, index)?;
            let mut options = OpenOptions::new();
            options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;

                options.custom_flags(libc::O_NOFOLLOW);
            }
            let mut file = match options.open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            Self::revalidate_open_file(&path, &file)?;
            let metadata = file.metadata()?;
            if metadata.len() > COLD_ERASURE_MAX_SHARD_BYTES {
                return Err(MmapStoreError::InvalidErasureShard(format!(
                    "shard {index} exceeds the {} byte safety limit",
                    COLD_ERASURE_MAX_SHARD_BYTES
                )));
            }
            let capacity = usize::try_from(metadata.len())
                .map_err(|_| MmapStoreError::NumericOverflow("erasure_shard_len"))?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(capacity)
                .map_err(|error| MmapStoreError::InvalidErasureShard(error.to_string()))?;
            (&mut file)
                .take(COLD_ERASURE_MAX_SHARD_BYTES.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if u64::try_from(bytes.len()).unwrap_or(u64::MAX) != metadata.len() {
                return Err(MmapStoreError::InvalidErasureShard(format!(
                    "shard {index} changed length while being read"
                )));
            }
            Self::revalidate_open_file(&path, &file)?;
            match ColdErasureShard::decode_bytes(&bytes) {
                Ok(shard) if shard.index == index => shards.push(shard),
                Ok(shard) => invalid_shards.push(format!(
                    "path index {index} contains shard index {}",
                    shard.index
                )),
                Err(error) => invalid_shards.push(format!("shard {index}: {error}")),
            }
        }
        if shards.len() < COLD_ERASURE_DATA_SHARDS && !invalid_shards.is_empty() {
            return Err(MmapStoreError::InvalidErasureShard(format!(
                "only {} valid shard(s) remain; {}",
                shards.len(),
                invalid_shards.join("; ")
            )));
        }
        shards.sort_by_key(|shard| (shard.original_len, shard.payload_crc32, shard.bytes.len()));
        let mut first_decode_error = None;
        let mut start = 0usize;
        while start < shards.len() {
            let identity = (
                shards[start].original_len,
                shards[start].payload_crc32,
                shards[start].bytes.len(),
            );
            let mut end = start + 1;
            while end < shards.len()
                && (
                    shards[end].original_len,
                    shards[end].payload_crc32,
                    shards[end].bytes.len(),
                ) == identity
            {
                end += 1;
            }
            if end - start >= COLD_ERASURE_DATA_SHARDS {
                match cold_erasure_decode(&shards[start..end]) {
                    Ok(decoded) => return Ok(decoded),
                    Err(error) if first_decode_error.is_none() => first_decode_error = Some(error),
                    Err(_) => {}
                }
            }
            start = end;
        }
        Err(
            first_decode_error.unwrap_or_else(|| MmapStoreError::InsufficientErasureShards {
                have: shards.len(),
                need: COLD_ERASURE_DATA_SHARDS,
            }),
        )
    }

    fn stale_prefix_bytes(&self) -> u64 {
        self.line_offsets
            .first()
            .map(|offset| offset.0.saturating_sub(self.data_start))
            .unwrap_or_else(|| self.file_len.saturating_sub(self.data_start))
    }

    fn compact_retained_prefix(&mut self) -> Result<bool, MmapStoreError> {
        let stale_bytes = self.stale_prefix_bytes();
        if stale_bytes == 0 {
            return Ok(false);
        }
        let retained_start = self
            .data_start
            .checked_add(stale_bytes)
            .ok_or(MmapStoreError::NumericOverflow("retained_start"))?;
        let retained_len = self
            .file_len
            .checked_sub(retained_start)
            .ok_or(MmapStoreError::NumericOverflow("retained_len"))?;
        let retained_len = usize::try_from(retained_len)
            .map_err(|_| MmapStoreError::NumericOverflow("file_len"))?;
        let mut retained = Vec::with_capacity(retained_len);
        let mut source = self.file.try_clone()?;
        source.seek(SeekFrom::Start(retained_start))?;
        source
            .take(u64::try_from(retained_len).unwrap_or(u64::MAX))
            .read_to_end(&mut retained)?;
        if retained.len() != retained_len {
            return Err(MmapStoreError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "pane log changed while preparing compacted retained bytes",
            )));
        }

        // Crash-safe compaction (ft-odrq7): write the retained suffix to a
        // sibling temp file, fsync it, then atomically rename it over the live
        // log. The previous implementation reopened the live log with
        // `truncate(true)` — zeroing it in place — and wrote the suffix back
        // with only a buffered `flush()` (no fsync). A crash between the
        // truncate and a durable write lost the ENTIRE retained scrollback for
        // the pane, not just the stale prefix it was supposed to drop. With
        // temp + fsync + rename, the live log is at every instant either the
        // old (full) file or the new (compacted) file — never truncated.
        let tmp_path = Self::unique_staging_path(&self.log_path, "compact-installing")?;
        let header = format!("\0FTMMAP1:{}\n", self.base_seq);
        let header_len = u64::try_from(header.len())
            .map_err(|_| MmapStoreError::NumericOverflow("header_len"))?;
        let new_file_len = header_len
            .checked_add(
                u64::try_from(retained.len())
                    .map_err(|_| MmapStoreError::NumericOverflow("file_len"))?,
            )
            .ok_or(MmapStoreError::NumericOverflow("file_len"))?;
        let compacted_offsets = self
            .line_offsets
            .iter()
            .map(|offset| {
                offset
                    .0
                    .checked_sub(retained_start)
                    .and_then(|relative| relative.checked_add(header_len))
                    .map(LineOffset)
                    .ok_or(MmapStoreError::NumericOverflow("line_offset"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let compacted_file = {
            let mut options = OpenOptions::new();
            options.create_new(true).read(true).write(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;

                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let mut compacted = options.open(&tmp_path)?;
            compacted.write_all(header.as_bytes())?;
            compacted.write_all(&retained)?;
            // Persist the retained bytes before the rename so the swap can never
            // expose a partially written compacted log.
            compacted.sync_all()?;
            compacted
        };
        Self::revalidate_open_file(&tmp_path, &compacted_file)?;
        Self::publish_staging_file(&tmp_path, &self.log_path)?;
        // The open staged handle names the same inode after rename. Publish it
        // into the object before any later fallible durability operation so an
        // error can never leave subsequent appends writing an unlinked old log.
        self.file = compacted_file;
        self.file_len = new_file_len;
        self.trailing_partial = false;
        self.data_start = header_len;
        self.line_offsets = compacted_offsets;
        // Make the rename itself durable: without a directory fsync a crash
        // after the rename but before the directory entry reaches disk could
        // resurrect the pre-compaction file. Successful compaction requires
        // this acknowledgement on platforms that support directory handles.
        Self::sync_parent_directory(&self.log_path)?;
        // The compacted log header is now the sequence authority. Collapse the
        // append-only journal only after that header and its rename are durable;
        // a crash at any point leaves either the old journal or the new header
        // sufficient to recover the same base sequence.
        self.base_seq_file.set_len(0)?;
        self.base_seq_file.seek(SeekFrom::Start(0))?;
        self.persist_base_seq(self.base_seq)?;
        Ok(true)
    }

    fn retained_bytes(&self) -> u64 {
        let Some(first) = self.line_offsets.first() else {
            return 0;
        };
        self.data_start
            .saturating_add(self.file_len.saturating_sub(first.0))
    }

    fn retained_record_bytes(&self) -> u64 {
        self.line_offsets
            .first()
            .map_or(0, |first| self.file_len.saturating_sub(first.0))
    }

    fn oldest_seq(&self) -> Option<u64> {
        (!self.line_offsets.is_empty()).then_some(self.base_seq)
    }

    fn next_seq(&self) -> Result<u64, MmapStoreError> {
        self.base_seq
            .checked_add(
                u64::try_from(self.line_offsets.len())
                    .map_err(|_| MmapStoreError::NumericOverflow("line_count"))?,
            )
            .ok_or(MmapStoreError::NumericOverflow("seq"))
    }

    fn file_bytes(&self) -> u64 {
        self.file_len
    }

    fn sequence_file_bytes(&self) -> Result<u64, MmapStoreError> {
        Ok(self.base_seq_file.metadata()?.len())
    }
}

#[derive(Debug)]
struct SqliteFallbackStore {
    conn: Connection,
}

impl SqliteFallbackStore {
    fn open(path: &Path) -> Result<Self, MmapStoreError> {
        let conn = Connection::open(path)?;
        // Long-lived fallback store; busy_timeout matches the recipe used
        // by TelemetryStore / session_restore so concurrent writers don't
        // surface SQLITE_BUSY immediately to the scrollback path.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=FULL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS mmap_scrollback_lines (
                 pane_id INTEGER NOT NULL,
                 seq INTEGER NOT NULL,
                 content TEXT NOT NULL,
                 PRIMARY KEY (pane_id, seq)
             );
             CREATE TABLE IF NOT EXISTS mmap_scrollback_fallback_panes (
                 pane_id INTEGER PRIMARY KEY,
                 next_seq INTEGER NOT NULL CHECK(next_seq >= 0)
             );
             CREATE INDEX IF NOT EXISTS idx_mmap_scrollback_lines_pane_seq
                 ON mmap_scrollback_lines(pane_id, seq DESC);",
        )?;

        Ok(Self { conn })
    }

    fn mark_as_authority(&self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        self.conn.execute(
            "INSERT OR IGNORE INTO mmap_scrollback_fallback_panes (pane_id, next_seq)
             SELECT ?1, COALESCE(MAX(seq) + 1, 0)
             FROM mmap_scrollback_lines
             WHERE pane_id = ?1",
            [pane_id_i64],
        )?;
        Ok(())
    }

    fn is_authority(&self, pane_id: PaneId) -> Result<bool, MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM mmap_scrollback_fallback_panes WHERE pane_id = ?1",
            [pane_id_i64],
            |row| row.get(0),
        )?;
        Ok(count == 1)
    }

    fn should_become_authority(&self, pane_id: PaneId) -> Result<bool, MmapStoreError> {
        // Before the authority table existed, hybrid stores mirrored every mmap
        // append into SQLite. Prefer those surviving rows during the one-time
        // transition so a pre-upgrade fallback history is never hidden.
        Ok(self.is_authority(pane_id)? || self.line_count(pane_id)? > 0)
    }

    #[cfg(test)]
    fn append_line_with_seq(
        &mut self,
        pane_id: PaneId,
        seq: u64,
        line: &str,
    ) -> Result<(), MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let seq_i64 = i64::try_from(seq).map_err(|_| MmapStoreError::NumericOverflow("seq"))?;

        let next_seq = seq
            .checked_add(1)
            .ok_or(MmapStoreError::NumericOverflow("seq"))?;
        let next_seq_i64 =
            i64::try_from(next_seq).map_err(|_| MmapStoreError::NumericOverflow("seq"))?;
        let transaction = self.conn.transaction()?;
        transaction.execute(
            "INSERT INTO mmap_scrollback_lines (pane_id, seq, content) VALUES (?1, ?2, ?3)",
            params![pane_id_i64, seq_i64, line],
        )?;
        transaction.execute(
            "INSERT INTO mmap_scrollback_fallback_panes (pane_id, next_seq)
             VALUES (?1, ?2)
             ON CONFLICT(pane_id) DO UPDATE SET next_seq = MAX(next_seq, excluded.next_seq)",
            params![pane_id_i64, next_seq_i64],
        )?;
        transaction.commit()?;

        Ok(())
    }

    fn append_line_auto_seq(&mut self, pane_id: PaneId, line: &str) -> Result<u64, MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let transaction = self.conn.transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO mmap_scrollback_fallback_panes (pane_id, next_seq)
             SELECT ?1, COALESCE(MAX(seq) + 1, 0)
             FROM mmap_scrollback_lines
             WHERE pane_id = ?1",
            [pane_id_i64],
        )?;
        let next_seq_i64: i64 = transaction.query_row(
            "SELECT next_seq FROM mmap_scrollback_fallback_panes WHERE pane_id = ?1",
            [pane_id_i64],
            |row| row.get(0),
        )?;
        let next_seq =
            u64::try_from(next_seq_i64).map_err(|_| MmapStoreError::NumericOverflow("seq"))?;
        let following_seq = next_seq
            .checked_add(1)
            .ok_or(MmapStoreError::NumericOverflow("seq"))?;
        let following_seq_i64 =
            i64::try_from(following_seq).map_err(|_| MmapStoreError::NumericOverflow("seq"))?;
        transaction.execute(
            "INSERT INTO mmap_scrollback_lines (pane_id, seq, content) VALUES (?1, ?2, ?3)",
            params![pane_id_i64, next_seq_i64, line],
        )?;
        let updated = transaction.execute(
            "UPDATE mmap_scrollback_fallback_panes
             SET next_seq = ?2
             WHERE pane_id = ?1 AND next_seq = ?3",
            params![pane_id_i64, following_seq_i64, next_seq_i64],
        )?;
        if updated != 1 {
            return Err(MmapStoreError::InvalidPaneLogHeader(
                "SQLite fallback sequence authority changed during append".to_string(),
            ));
        }
        transaction.commit()?;
        Ok(next_seq)
    }

    fn tail_lines(&self, pane_id: PaneId, n: usize) -> Result<Vec<String>, MmapStoreError> {
        if n == 0 {
            return Ok(Vec::new());
        }

        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let limit_i64 = i64::try_from(n).map_err(|_| MmapStoreError::NumericOverflow("limit"))?;

        let mut stmt = self.conn.prepare(
            "SELECT content
             FROM mmap_scrollback_lines
             WHERE pane_id = ?1
             ORDER BY seq DESC
             LIMIT ?2",
        )?;
        let mut lines: Vec<String> = stmt
            .query_map(params![pane_id_i64, limit_i64], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        lines.reverse();
        Ok(lines)
    }

    fn line_at(&self, pane_id: PaneId, seq: u64) -> Result<Option<String>, MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let seq_i64 = i64::try_from(seq).map_err(|_| MmapStoreError::NumericOverflow("seq"))?;
        let mut stmt = self.conn.prepare(
            "SELECT content
             FROM mmap_scrollback_lines
             WHERE pane_id = ?1 AND seq = ?2",
        )?;
        let mut rows = stmt.query(params![pane_id_i64, seq_i64])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get::<_, String>(0)?))
        } else {
            Ok(None)
        }
    }

    fn prune_before(&self, pane_id: PaneId, seq: u64) -> Result<(), MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let seq_i64 = i64::try_from(seq).map_err(|_| MmapStoreError::NumericOverflow("seq"))?;
        self.conn.execute(
            "DELETE FROM mmap_scrollback_lines WHERE pane_id = ?1 AND seq < ?2",
            params![pane_id_i64, seq_i64],
        )?;
        Ok(())
    }

    fn clear_pane(&mut self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let transaction = self.conn.transaction()?;
        transaction.execute(
            "DELETE FROM mmap_scrollback_lines WHERE pane_id = ?1",
            [pane_id_i64],
        )?;
        transaction.execute(
            "INSERT INTO mmap_scrollback_fallback_panes (pane_id, next_seq)
             VALUES (?1, 0)
             ON CONFLICT(pane_id) DO UPDATE SET next_seq = 0",
            [pane_id_i64],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn line_count(&self, pane_id: PaneId) -> Result<usize, MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let count_i64: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM mmap_scrollback_lines WHERE pane_id = ?1",
            [pane_id_i64],
            |row| row.get(0),
        )?;
        usize::try_from(count_i64).map_err(|_| MmapStoreError::NumericOverflow("line_count"))
    }

    fn oldest_seq(&self, pane_id: PaneId) -> Result<Option<u64>, MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let min_seq: Option<i64> = self.conn.query_row(
            "SELECT MIN(seq) FROM mmap_scrollback_lines WHERE pane_id = ?1",
            [pane_id_i64],
            |row| row.get(0),
        )?;
        min_seq
            .map(|seq| u64::try_from(seq).map_err(|_| MmapStoreError::NumericOverflow("seq")))
            .transpose()
    }

    fn next_seq(&self, pane_id: PaneId) -> Result<u64, MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        if self.is_authority(pane_id)? {
            let next_seq_i64: i64 = self.conn.query_row(
                "SELECT next_seq FROM mmap_scrollback_fallback_panes WHERE pane_id = ?1",
                [pane_id_i64],
                |row| row.get(0),
            )?;
            return u64::try_from(next_seq_i64).map_err(|_| MmapStoreError::NumericOverflow("seq"));
        }
        let max_seq: Option<i64> = self.conn.query_row(
            "SELECT MAX(seq) FROM mmap_scrollback_lines WHERE pane_id = ?1",
            [pane_id_i64],
            |row| row.get(0),
        )?;
        match max_seq {
            Some(seq) => u64::try_from(seq)
                .map_err(|_| MmapStoreError::NumericOverflow("seq"))?
                .checked_add(1)
                .ok_or(MmapStoreError::NumericOverflow("seq")),
            None => Ok(0),
        }
    }
}

/// Pane-scoped append/read store.
#[derive(Debug)]
pub struct MmapScrollbackStore {
    base_dir: PathBuf,
    panes: HashMap<PaneId, PaneFile>,
    sqlite_fallback: Option<SqliteFallbackStore>,
    fallback_panes: HashSet<PaneId>,
    cold_erasure: ColdErasureMode,
}

/// Read one pane log without taking write authority over it.
///
/// This is the forensic/export seam for crash recovery. All limits are checked
/// before the returned vectors can grow beyond caller policy. The source files
/// are opened read-only with symlink refusal where the platform supports it and
/// are revalidated after the read so a concurrent replacement fails closed.
pub fn read_pane_snapshot(
    base_dir: &Path,
    pane_id: PaneId,
    max_records: usize,
    max_record_bytes: u64,
    max_physical_bytes: u64,
) -> Result<MmapPaneReadSnapshot, MmapStoreError> {
    let base_metadata = std::fs::symlink_metadata(base_dir)?;
    if !base_metadata.file_type().is_dir() {
        return Err(MmapStoreError::InvalidPaneLogHeader(
            "pane snapshot base path is not a directory".to_string(),
        ));
    }

    let log_path = base_dir.join(format!("{pane_id}.log"));
    let path_metadata_before = std::fs::symlink_metadata(&log_path)?;
    if !path_metadata_before.file_type().is_file() {
        return Err(MmapStoreError::InvalidPaneLogHeader(
            "pane snapshot log is not a regular file".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if path_metadata_before.permissions().mode() & 0o077 != 0 {
            return Err(MmapStoreError::InvalidPaneLogHeader(
                "pane snapshot log is not private".to_string(),
            ));
        }
    }
    if path_metadata_before.len() > max_physical_bytes {
        return Err(MmapStoreError::PaneSnapshotLimitExceeded {
            limit_name: "physical_bytes",
            limit: max_physical_bytes,
            observed: path_metadata_before.len(),
        });
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(&log_path)?;
    PaneFile::revalidate_open_file(&log_path, &file)?;
    let handle_metadata_before = file.metadata()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        if handle_metadata_before.permissions().mode() & 0o077 != 0 {
            return Err(MmapStoreError::InvalidPaneLogHeader(
                "opened pane snapshot log is not private".to_string(),
            ));
        }
    }
    let (mut line_offsets, committed_bytes, log_base_seq, _data_start) =
        PaneFile::scan_offsets_and_base_bounded(
            &file,
            Some(max_records),
            Some(max_physical_bytes),
        )?;

    let base_seq_path = PaneFile::base_seq_path(&log_path);
    let (journal_base_seq, journal_guard) = match std::fs::symlink_metadata(&base_seq_path) {
        Ok(path_metadata) => {
            if !path_metadata.file_type().is_file() {
                return Err(MmapStoreError::InvalidPaneLogHeader(
                    "pane sequence journal is not a regular file".to_string(),
                ));
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;

                if path_metadata.permissions().mode() & 0o077 != 0 {
                    return Err(MmapStoreError::InvalidPaneLogHeader(
                        "pane sequence journal is not private".to_string(),
                    ));
                }
            }
            if path_metadata.len() > PANE_BASE_SEQ_JOURNAL_MAX_BYTES {
                return Err(MmapStoreError::PaneSnapshotLimitExceeded {
                    limit_name: "sequence_journal_bytes",
                    limit: PANE_BASE_SEQ_JOURNAL_MAX_BYTES,
                    observed: path_metadata.len(),
                });
            }
            let mut journal_options = OpenOptions::new();
            journal_options.read(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt as _;

                journal_options.custom_flags(libc::O_NOFOLLOW);
            }
            let journal = journal_options.open(&base_seq_path)?;
            PaneFile::revalidate_open_file(&base_seq_path, &journal)?;
            let journal_metadata_before = journal.metadata()?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;

                if journal_metadata_before.permissions().mode() & 0o077 != 0 {
                    return Err(MmapStoreError::InvalidPaneLogHeader(
                        "opened pane sequence journal is not private".to_string(),
                    ));
                }
            }
            let value = PaneFile::scan_base_seq_journal(&journal)?;
            let journal_metadata_after = journal.metadata()?;
            if PaneFile::metadata_changed(&journal_metadata_before, &journal_metadata_after)? {
                return Err(MmapStoreError::InvalidPaneLogHeader(
                    "pane sequence journal changed during snapshot".to_string(),
                ));
            }
            PaneFile::revalidate_open_file(&base_seq_path, &journal)?;
            (value, Some((journal, journal_metadata_before)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (None, None),
        Err(error) => return Err(error.into()),
    };

    let base_seq = if committed_bytes == 0 {
        log_base_seq
    } else {
        journal_base_seq.map_or(log_base_seq, |seq| seq.max(log_base_seq))
    };
    let logically_pruned = usize::try_from(base_seq.saturating_sub(log_base_seq))
        .map_err(|_| MmapStoreError::NumericOverflow("logically_pruned"))?;
    if logically_pruned > line_offsets.len() {
        return Err(MmapStoreError::InvalidPaneLogHeader(format!(
            "base sequence journal prunes {logically_pruned} records from a log containing {}",
            line_offsets.len()
        )));
    }
    line_offsets.drain(0..logically_pruned);
    if line_offsets.len() > max_records {
        return Err(MmapStoreError::PaneSnapshotLimitExceeded {
            limit_name: "records",
            limit: u64::try_from(max_records).unwrap_or(u64::MAX),
            observed: u64::try_from(line_offsets.len()).unwrap_or(u64::MAX),
        });
    }

    let mut records = Vec::with_capacity(line_offsets.len());
    let mut record_bytes = 0u64;
    for offset in line_offsets {
        if offset.0 > committed_bytes {
            return Err(MmapStoreError::OffsetOutOfBounds {
                offset: offset.0,
                len: committed_bytes,
            });
        }
        let mut source = file.try_clone()?;
        source.seek(SeekFrom::Start(offset.0))?;
        let readable = committed_bytes
            .saturating_sub(offset.0)
            .min(PANE_LOG_MAX_RECORD_BYTES.saturating_add(1));
        let mut reader = BufReader::new(source.take(readable));
        let mut bytes = Vec::new();
        reader.read_until(b'\n', &mut bytes)?;
        if !bytes.ends_with(b"\n") {
            return Err(MmapStoreError::InvalidPaneLogHeader(
                "indexed pane record is not newline terminated".to_string(),
            ));
        }
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        record_bytes = record_bytes
            .checked_add(
                u64::try_from(bytes.len())
                    .map_err(|_| MmapStoreError::NumericOverflow("pane_snapshot_record_bytes"))?,
            )
            .ok_or(MmapStoreError::NumericOverflow(
                "pane_snapshot_record_bytes",
            ))?;
        let record_count_with_current = records
            .len()
            .checked_add(1)
            .ok_or(MmapStoreError::NumericOverflow("record_delimiters"))?;
        let committed_byte_limit_observation = record_bytes
            .checked_add(
                u64::try_from(record_count_with_current)
                    .map_err(|_| MmapStoreError::NumericOverflow("record_delimiters"))?,
            )
            .ok_or(MmapStoreError::NumericOverflow("committed_bytes"))?;
        if committed_byte_limit_observation > max_record_bytes {
            return Err(MmapStoreError::PaneSnapshotLimitExceeded {
                limit_name: "committed_bytes",
                limit: max_record_bytes,
                observed: committed_byte_limit_observation,
            });
        }
        records.push(String::from_utf8(bytes)?);
    }

    let retained_record_bytes = record_bytes
        .checked_add(
            u64::try_from(records.len())
                .map_err(|_| MmapStoreError::NumericOverflow("record_delimiters"))?,
        )
        .ok_or(MmapStoreError::NumericOverflow("retained_record_bytes"))?;
    let sequence_bytes = journal_guard
        .as_ref()
        .map_or(0, |(_journal, metadata)| metadata.len());
    let handle_metadata_after = file.metadata()?;
    if PaneFile::metadata_changed(&handle_metadata_before, &handle_metadata_after)? {
        return Err(MmapStoreError::InvalidPaneLogHeader(
            "pane log changed during snapshot".to_string(),
        ));
    }
    PaneFile::revalidate_open_file(&log_path, &file)?;
    match journal_guard {
        Some((journal, journal_metadata_before)) => {
            let journal_metadata_after = journal.metadata()?;
            if PaneFile::metadata_changed(&journal_metadata_before, &journal_metadata_after)? {
                return Err(MmapStoreError::InvalidPaneLogHeader(
                    "pane sequence journal changed during snapshot".to_string(),
                ));
            }
            PaneFile::revalidate_open_file(&base_seq_path, &journal)?;
        }
        None => match std::fs::symlink_metadata(&base_seq_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(MmapStoreError::InvalidPaneLogHeader(
                    "pane sequence journal appeared during snapshot".to_string(),
                ));
            }
            Err(error) => return Err(error.into()),
        },
    }

    let record_count = u64::try_from(records.len())
        .map_err(|_| MmapStoreError::NumericOverflow("pane_snapshot_records"))?;
    let next_seq = base_seq
        .checked_add(record_count)
        .ok_or(MmapStoreError::NumericOverflow("pane_snapshot_next_seq"))?;
    Ok(MmapPaneReadSnapshot {
        oldest_seq: (!records.is_empty()).then_some(base_seq),
        next_seq,
        records,
        retained_record_bytes,
        committed_bytes,
        sequence_bytes,
        physical_bytes: handle_metadata_after.len(),
        trailing_uncommitted_bytes: handle_metadata_after.len().saturating_sub(committed_bytes),
    })
}

impl MmapScrollbackStore {
    pub fn new(config: MmapStoreConfig) -> Result<Self, MmapStoreError> {
        create_dir_all(&config.base_dir)?;
        let sqlite_fallback = config
            .sqlite_fallback_path
            .as_deref()
            .map(SqliteFallbackStore::open)
            .transpose()?;

        Ok(Self {
            base_dir: config.base_dir,
            panes: HashMap::new(),
            sqlite_fallback,
            fallback_panes: HashSet::new(),
            cold_erasure: config.cold_erasure,
        })
    }

    /// Open a manifest-selected pane without creating either ledger file.
    pub fn open_existing_pane(&mut self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) || self.sqlite_is_authority(pane_id)? {
            return Err(MmapStoreError::InvalidPaneLogHeader(
                "versioned pane authority cannot resolve through SQLite fallback".to_string(),
            ));
        }
        let log_path = self.base_dir.join(format!("{pane_id}.log"));
        let sequence_path = PaneFile::base_seq_path(&log_path);
        for path in [&log_path, &sequence_path] {
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.file_type().is_file() {
                return Err(MmapStoreError::InvalidPaneLogHeader(
                    "versioned pane ledger path is not a regular file".to_string(),
                ));
            }
        }
        let pane = PaneFile::open(&self.base_dir, pane_id)?;
        self.panes.insert(pane_id, pane);
        Ok(())
    }

    /// Stage a complete replacement ledger under a fresh unreachable pane ID.
    ///
    /// Every record and both ledger files are synchronized before return. The
    /// caller must publish its authenticated pointer manifest separately; a
    /// failure here never modifies any previously published pane ledger.
    /// `pane_id` is a caller-derived deterministic transaction identity. If
    /// its exact bounded slot already exists, this method returns it without
    /// rewriting bytes and marks the receipt as reused; before publication,
    /// the caller must authenticate and semantically compare those opaque
    /// records against its transaction input.
    pub fn stage_versioned_pane_replacement(
        &mut self,
        pane_id: PaneId,
        records: &[String],
        max_records: usize,
        max_record_bytes: u64,
    ) -> Result<MmapStagedPaneLedger, MmapStoreError> {
        if records.len() > max_records {
            return Err(MmapStoreError::PaneSnapshotLimitExceeded {
                limit_name: "records",
                limit: u64::try_from(max_records).unwrap_or(u64::MAX),
                observed: u64::try_from(records.len()).unwrap_or(u64::MAX),
            });
        }
        let record_bytes = records.iter().try_fold(0_u64, |total, record| {
            total
                .checked_add(
                    u64::try_from(record.len())
                        .map_err(|_| MmapStoreError::NumericOverflow("record_bytes"))?,
                )
                .ok_or(MmapStoreError::NumericOverflow("record_bytes"))
        })?;
        let record_delimiters = u64::try_from(records.len())
            .map_err(|_| MmapStoreError::NumericOverflow("record_delimiters"))?;
        let committed_byte_limit_observation = record_bytes
            .checked_add(record_delimiters)
            .ok_or(MmapStoreError::NumericOverflow("committed_bytes"))?;
        if committed_byte_limit_observation > max_record_bytes {
            return Err(MmapStoreError::PaneSnapshotLimitExceeded {
                limit_name: "committed_bytes",
                limit: max_record_bytes,
                observed: committed_byte_limit_observation,
            });
        }

        if pane_id == 0 {
            return Err(MmapStoreError::VersionedPaneIdentityCollision);
        }
        let log_path = self.base_dir.join(format!("{pane_id}.log"));
        let sequence_path = PaneFile::base_seq_path(&log_path);
        let path_is_regular = |path: &Path| -> Result<bool, MmapStoreError> {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.file_type().is_file() => Ok(true),
                Ok(_) => Err(MmapStoreError::VersionedPaneIdentityCollision),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.into()),
            }
        };
        let log_exists = path_is_regular(&log_path)?;
        let sequence_exists = path_is_regular(&sequence_path)?;
        if log_exists != sequence_exists {
            return Err(MmapStoreError::VersionedPaneIdentityCollision);
        }
        if log_exists {
            let pane = PaneFile::open(&self.base_dir, pane_id)?;
            if pane.base_seq != 0
                || pane.data_start != 0
                || pane.trailing_partial
                || pane.line_offsets.len() != records.len()
                || pane.file_len != committed_byte_limit_observation
                || pane.base_seq_file.metadata()?.len() != 0
            {
                return Err(MmapStoreError::VersionedPaneIdentityCollision);
            }
            let staged = MmapStagedPaneLedger {
                pane_id,
                record_count: records.len(),
                record_bytes,
                committed_bytes: pane.file_len,
                reused_existing: true,
            };
            self.panes.insert(pane_id, pane);
            return Ok(staged);
        }

        let Some(mut pane) = PaneFile::create_versioned(&self.base_dir, pane_id)? else {
            // A concurrent creator may have finished the same deterministic
            // transaction. The next retry reopens and validates that bounded
            // slot; never allocate a second random ledger for one transaction.
            return Err(MmapStoreError::VersionedPaneIdentityCollision);
        };
        for record in records {
            pane.append_line(record)?;
        }
        pane.file.sync_all()?;
        pane.base_seq_file.sync_all()?;
        PaneFile::sync_parent_directory(&pane.log_path)?;
        let committed_bytes = pane.file_len;
        if committed_bytes != committed_byte_limit_observation {
            return Err(MmapStoreError::StagedPaneVerificationFailed);
        }
        drop(pane);

        let staged = MmapStagedPaneLedger {
            pane_id,
            record_count: records.len(),
            record_bytes,
            committed_bytes,
            reused_existing: false,
        };
        self.verify_staged_pane_ledger(staged, records)?;
        Ok(staged)
    }

    /// Reopen a staged ledger from disk and prove its exact contiguous bytes.
    /// This is safe both before and after the caller publishes its manifest.
    pub fn verify_staged_pane_ledger(
        &mut self,
        staged: MmapStagedPaneLedger,
        expected_records: &[String],
    ) -> Result<(), MmapStoreError> {
        if expected_records.len() != staged.record_count {
            return Err(MmapStoreError::StagedPaneVerificationFailed);
        }
        let pane = PaneFile::open(&self.base_dir, staged.pane_id)?;
        if pane.base_seq != 0
            || pane.line_offsets.len() != staged.record_count
            || pane.file_len != staged.committed_bytes
            || pane.next_seq()?
                != u64::try_from(staged.record_count)
                    .map_err(|_| MmapStoreError::NumericOverflow("record_count"))?
        {
            return Err(MmapStoreError::StagedPaneVerificationFailed);
        }
        for (sequence, expected) in expected_records.iter().enumerate() {
            let sequence =
                u64::try_from(sequence).map_err(|_| MmapStoreError::NumericOverflow("sequence"))?;
            if pane.line_at(sequence)?.as_deref() != Some(expected.as_str()) {
                return Err(MmapStoreError::StagedPaneVerificationFailed);
            }
        }
        self.panes.insert(staged.pane_id, pane);
        Ok(())
    }

    fn pane_mut(&mut self, pane_id: PaneId) -> Result<&mut PaneFile, MmapStoreError> {
        if !self.panes.contains_key(&pane_id) {
            let pane = PaneFile::open(&self.base_dir, pane_id)?;
            self.panes.insert(pane_id, pane);
        }
        self.panes
            .get_mut(&pane_id)
            .ok_or(MmapStoreError::UnknownPane(pane_id))
    }

    fn append_line_sqlite_only(
        &mut self,
        pane_id: PaneId,
        line: &str,
    ) -> Result<u64, MmapStoreError> {
        let sqlite = self
            .sqlite_fallback
            .as_mut()
            .ok_or(MmapStoreError::UnknownPane(pane_id))?;
        sqlite.mark_as_authority(pane_id)?;
        self.fallback_panes.insert(pane_id);
        sqlite.append_line_auto_seq(pane_id, line)
    }

    fn sqlite_is_authority(&self, pane_id: PaneId) -> Result<bool, MmapStoreError> {
        self.sqlite_fallback
            .as_ref()
            .map_or(Ok(false), |sqlite| sqlite.should_become_authority(pane_id))
    }

    fn activate_sqlite_fallback(&mut self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        let sqlite = self
            .sqlite_fallback
            .as_ref()
            .ok_or(MmapStoreError::UnknownPane(pane_id))?;
        sqlite.mark_as_authority(pane_id)?;
        self.fallback_panes.insert(pane_id);
        Ok(())
    }

    fn tail_lines_sqlite(&self, pane_id: PaneId, n: usize) -> Result<Vec<String>, MmapStoreError> {
        let sqlite = self
            .sqlite_fallback
            .as_ref()
            .ok_or(MmapStoreError::UnknownPane(pane_id))?;
        let lines = sqlite.tail_lines(pane_id, n)?;
        if lines.is_empty() && sqlite.line_count(pane_id)? == 0 && !sqlite.is_authority(pane_id)? {
            return Err(MmapStoreError::UnknownPane(pane_id));
        }
        Ok(lines)
    }

    pub fn ensure_pane(&mut self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) {
            return Ok(());
        }

        if self.sqlite_is_authority(pane_id)? {
            self.activate_sqlite_fallback(pane_id)?;
            return Ok(());
        }

        match self.pane_mut(pane_id) {
            Ok(_pane) => Ok(()),
            Err(err) => {
                if self.sqlite_fallback.is_some() {
                    self.activate_sqlite_fallback(pane_id)?;
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    pub fn append_line(&mut self, pane_id: PaneId, line: &str) -> Result<u64, MmapStoreError> {
        if line
            .as_bytes()
            .iter()
            .any(|byte| matches!(byte, b'\n' | b'\r'))
        {
            return Err(MmapStoreError::InvalidLineRecord);
        }
        let record_bytes = u64::try_from(line.len())
            .map_err(|_| MmapStoreError::NumericOverflow("line_len"))?
            .checked_add(1)
            .ok_or(MmapStoreError::NumericOverflow("line_len"))?;
        if record_bytes > PANE_LOG_MAX_RECORD_BYTES {
            return Err(MmapStoreError::PaneLogRecordTooLarge {
                bytes: record_bytes,
                max: PANE_LOG_MAX_RECORD_BYTES,
            });
        }
        if self.fallback_panes.contains(&pane_id) {
            return self.append_line_sqlite_only(pane_id, line);
        }

        if self.sqlite_is_authority(pane_id)? {
            self.activate_sqlite_fallback(pane_id)?;
            return self.append_line_sqlite_only(pane_id, line);
        }

        if !self.panes.contains_key(&pane_id) {
            if let Err(error) = self.pane_mut(pane_id) {
                if self.sqlite_fallback.is_none() {
                    return Err(error);
                }
                self.activate_sqlite_fallback(pane_id)?;
                return self.append_line_sqlite_only(pane_id, line);
            }
        }

        // Once a pane has an mmap authority, never silently change authorities
        // after a write failure. Doing so can fork sequence histories because
        // mmap and SQLite cannot participate in one atomic transaction.
        self.panes
            .get_mut(&pane_id)
            .ok_or(MmapStoreError::UnknownPane(pane_id))?
            .append_line(line)
    }

    pub fn compact_pane_if_stale(
        &mut self,
        pane_id: PaneId,
        min_stale_bytes: u64,
    ) -> Result<bool, MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) {
            return Ok(false);
        }

        let Some(pane) = self.panes.get_mut(&pane_id) else {
            return Ok(false);
        };
        let stale_bytes = pane.stale_prefix_bytes();
        let retained_bytes = pane.retained_bytes();
        if stale_bytes == 0 {
            return Ok(false);
        }
        if stale_bytes < min_stale_bytes && stale_bytes < retained_bytes {
            return Ok(false);
        }

        pane.compact_retained_prefix().and_then(|compacted| {
            if compacted && self.cold_erasure == ColdErasureMode::ReedSolomon {
                pane.write_erasure_sidecars()?;
            }
            Ok(compacted)
        })
    }

    pub fn tail_lines(&self, pane_id: PaneId, n: usize) -> Result<Vec<String>, MmapStoreError> {
        if n == 0 {
            return Ok(Vec::new());
        }

        if self.fallback_panes.contains(&pane_id) {
            return self.tail_lines_sqlite(pane_id, n);
        }

        let pane = match self.panes.get(&pane_id) {
            Some(pane) => pane,
            None => {
                if self.sqlite_fallback.is_some() {
                    return self.tail_lines_sqlite(pane_id, n);
                }
                return Err(MmapStoreError::UnknownPane(pane_id));
            }
        };
        match pane.tail_lines(n) {
            Ok(lines) => Ok(lines),
            Err(err) => {
                if self.sqlite_fallback.is_some() {
                    match self.tail_lines_sqlite(pane_id, n) {
                        Ok(lines) => Ok(lines),
                        Err(_) => Err(err),
                    }
                } else {
                    Err(err)
                }
            }
        }
    }

    pub fn line_at(&self, pane_id: PaneId, seq: u64) -> Result<Option<String>, MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) {
            return self
                .sqlite_fallback
                .as_ref()
                .ok_or(MmapStoreError::UnknownPane(pane_id))?
                .line_at(pane_id, seq);
        }

        if let Some(pane) = self.panes.get(&pane_id) {
            return pane.line_at(seq);
        }

        if let Some(sqlite) = self.sqlite_fallback.as_ref() {
            return sqlite.line_at(pane_id, seq);
        }

        Err(MmapStoreError::UnknownPane(pane_id))
    }

    pub fn prune_before(&mut self, pane_id: PaneId, seq: u64) -> Result<(), MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) || self.sqlite_is_authority(pane_id)? {
            self.activate_sqlite_fallback(pane_id)?;
            return self
                .sqlite_fallback
                .as_ref()
                .ok_or(MmapStoreError::UnknownPane(pane_id))?
                .prune_before(pane_id, seq);
        }

        let cold_erasure = self.cold_erasure;
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.prune_before(seq)?;
            if pane.base_seq_file.metadata()?.len() >= PANE_BASE_SEQ_JOURNAL_COMPACT_BYTES
                && pane.compact_retained_prefix()?
                && cold_erasure == ColdErasureMode::ReedSolomon
            {
                pane.write_erasure_sidecars()?;
            }
        }
        Ok(())
    }

    pub fn clear_pane(&mut self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) || self.sqlite_is_authority(pane_id)? {
            self.activate_sqlite_fallback(pane_id)?;
            self.sqlite_fallback
                .as_mut()
                .ok_or(MmapStoreError::UnknownPane(pane_id))?
                .clear_pane(pane_id)?;
            return Ok(());
        }
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.clear()?;
        }
        if self.cold_erasure == ColdErasureMode::ReedSolomon {
            if let Some(pane) = self.panes.get(&pane_id) {
                pane.write_erasure_sidecars()?;
            }
        }
        Ok(())
    }

    pub fn refresh_pane_erasure_shards(&mut self, pane_id: PaneId) -> Result<bool, MmapStoreError> {
        if self.cold_erasure != ColdErasureMode::ReedSolomon {
            return Ok(false);
        }
        if self.fallback_panes.contains(&pane_id) || self.sqlite_is_authority(pane_id)? {
            self.activate_sqlite_fallback(pane_id)?;
            return Ok(false);
        }
        let pane = self.pane_mut(pane_id)?;
        pane.write_erasure_sidecars()?;
        Ok(true)
    }

    pub fn recover_pane_bytes_from_erasure_shards(
        &mut self,
        pane_id: PaneId,
    ) -> Result<Option<Vec<u8>>, MmapStoreError> {
        if self.cold_erasure != ColdErasureMode::ReedSolomon {
            return Ok(None);
        }
        if self.fallback_panes.contains(&pane_id) || self.sqlite_is_authority(pane_id)? {
            self.activate_sqlite_fallback(pane_id)?;
            return Ok(None);
        }
        let pane = self.pane_mut(pane_id)?;
        pane.recover_from_erasure_sidecars().map(Some)
    }

    #[must_use]
    pub fn line_count(&self, pane_id: PaneId) -> usize {
        if self.fallback_panes.contains(&pane_id) {
            return self
                .sqlite_fallback
                .as_ref()
                .and_then(|sqlite| sqlite.line_count(pane_id).ok())
                .unwrap_or(0);
        }

        if let Some(pane) = self.panes.get(&pane_id) {
            return pane.line_offsets.len();
        }

        self.sqlite_fallback
            .as_ref()
            .and_then(|sqlite| sqlite.line_count(pane_id).ok())
            .unwrap_or(0)
    }

    #[must_use]
    pub fn retained_bytes(&self, pane_id: PaneId) -> u64 {
        if self.fallback_panes.contains(&pane_id) {
            return 0;
        }
        self.panes
            .get(&pane_id)
            .map(PaneFile::retained_bytes)
            .unwrap_or(0)
    }

    /// Exact newline-delimited bytes occupied by currently reachable records.
    /// This excludes an in-band sequence header and any logically pruned
    /// prefix that remains pending physical compaction.
    #[must_use]
    pub fn retained_record_bytes(&self, pane_id: PaneId) -> u64 {
        if self.fallback_panes.contains(&pane_id) {
            return 0;
        }
        self.panes
            .get(&pane_id)
            .map(PaneFile::retained_record_bytes)
            .unwrap_or(0)
    }

    #[must_use]
    pub fn file_bytes(&self, pane_id: PaneId) -> u64 {
        if self.fallback_panes.contains(&pane_id) {
            return 0;
        }
        self.panes
            .get(&pane_id)
            .map(PaneFile::file_bytes)
            .unwrap_or(0)
    }

    pub fn sequence_file_bytes(&self, pane_id: PaneId) -> Result<u64, MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) {
            return Err(MmapStoreError::InvalidPaneLogHeader(
                "SQLite fallback has no versioned sequence ledger".to_string(),
            ));
        }
        self.panes
            .get(&pane_id)
            .ok_or(MmapStoreError::UnknownPane(pane_id))?
            .sequence_file_bytes()
    }

    #[must_use]
    pub fn oldest_seq(&self, pane_id: PaneId) -> Option<u64> {
        if self.fallback_panes.contains(&pane_id) {
            return self
                .sqlite_fallback
                .as_ref()
                .and_then(|sqlite| sqlite.oldest_seq(pane_id).ok())
                .flatten();
        }

        if let Some(pane) = self.panes.get(&pane_id) {
            return pane.oldest_seq();
        }

        self.sqlite_fallback
            .as_ref()
            .and_then(|sqlite| sqlite.oldest_seq(pane_id).ok())
            .flatten()
    }

    pub fn next_seq(&self, pane_id: PaneId) -> Result<u64, MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) {
            return self
                .sqlite_fallback
                .as_ref()
                .ok_or(MmapStoreError::UnknownPane(pane_id))?
                .next_seq(pane_id);
        }

        if let Some(pane) = self.panes.get(&pane_id) {
            return pane.next_seq();
        }

        self.sqlite_fallback
            .as_ref()
            .ok_or(MmapStoreError::UnknownPane(pane_id))?
            .next_seq(pane_id)
    }

    #[must_use]
    pub fn pane_storage_mode(&self, pane_id: PaneId) -> Option<PaneStorageMode> {
        if self.fallback_panes.contains(&pane_id) {
            return Some(PaneStorageMode::SqliteFallback);
        }
        if self.panes.contains_key(&pane_id) {
            return Some(PaneStorageMode::Mmap);
        }
        self.sqlite_fallback.as_ref().and_then(|sqlite| {
            sqlite
                .should_become_authority(pane_id)
                .ok()
                .and_then(|authority| authority.then_some(PaneStorageMode::SqliteFallback))
        })
    }
}

/// Align an offset down to a page boundary.
#[must_use]
pub fn page_align_down(offset: u64, page_size: u64) -> u64 {
    if page_size == 0 {
        return offset;
    }
    offset - (offset % page_size)
}

/// Build cumulative start offsets from line byte lengths.
#[must_use]
pub fn build_offsets_from_lengths(lengths: &[u64]) -> Vec<LineOffset> {
    let mut offsets = Vec::with_capacity(lengths.len());
    let mut cursor = 0u64;
    for len in lengths {
        offsets.push(LineOffset(cursor));
        cursor = cursor.saturating_add(*len);
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::HashSet;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    fn file_only_store(dir: &Path) -> MmapScrollbackStore {
        let config = MmapStoreConfig::new(dir.to_path_buf());
        MmapScrollbackStore::new(config).expect("create store")
    }

    fn hybrid_store(dir: &Path, db_path: &Path) -> MmapScrollbackStore {
        let config =
            MmapStoreConfig::new(dir.to_path_buf()).with_sqlite_fallback(db_path.to_path_buf());
        MmapScrollbackStore::new(config).expect("create hybrid store")
    }

    fn rs_store(dir: &Path) -> MmapScrollbackStore {
        let config = MmapStoreConfig::new(dir.to_path_buf()).with_cold_erasure_rs();
        MmapScrollbackStore::new(config).expect("create rs store")
    }

    fn erasure_sidecar_paths(dir: &Path, pane_id: PaneId) -> Vec<PathBuf> {
        let log_path = dir.join(format!("{pane_id}.log"));
        (0..COLD_ERASURE_TOTAL_SHARDS)
            .map(|index| {
                cold_erasure_shard_path(&log_path, u8::try_from(index).unwrap())
                    .expect("sidecar path")
            })
            .collect()
    }

    // --- page_align_down ---

    #[test]
    fn page_align_down_zero_page_size_returns_offset() {
        assert_eq!(page_align_down(1234, 0), 1234);
    }

    #[test]
    fn page_align_down_already_aligned() {
        assert_eq!(page_align_down(4096, 4096), 4096);
        assert_eq!(page_align_down(0, 4096), 0);
    }

    #[test]
    fn page_align_down_unaligned() {
        assert_eq!(page_align_down(5000, 4096), 4096);
        assert_eq!(page_align_down(4095, 4096), 0);
        assert_eq!(page_align_down(8193, 4096), 8192);
    }

    #[test]
    fn page_align_down_page_size_one() {
        assert_eq!(page_align_down(42, 1), 42);
    }

    // --- build_offsets_from_lengths ---

    #[test]
    fn build_offsets_empty() {
        let offsets = build_offsets_from_lengths(&[]);
        assert!(offsets.is_empty());
    }

    #[test]
    fn build_offsets_single() {
        let offsets = build_offsets_from_lengths(&[10]);
        assert_eq!(offsets, vec![LineOffset(0)]);
    }

    #[test]
    fn build_offsets_multiple() {
        let offsets = build_offsets_from_lengths(&[5, 10, 3]);
        assert_eq!(offsets, vec![LineOffset(0), LineOffset(5), LineOffset(15)]);
    }

    #[test]
    fn build_offsets_saturating_add_large_values() {
        let offsets = build_offsets_from_lengths(&[u64::MAX - 1, 10]);
        assert_eq!(offsets[0], LineOffset(0));
        assert_eq!(offsets[1], LineOffset(u64::MAX - 1));
    }

    // --- LineOffset ordering ---

    #[test]
    fn line_offset_ord() {
        assert!(LineOffset(0) < LineOffset(1));
        assert_eq!(LineOffset(42), LineOffset(42));
    }

    // --- MmapStoreConfig ---

    #[test]
    fn config_new_has_no_sqlite_fallback() {
        let config = MmapStoreConfig::new(PathBuf::from("/tmp/test"));
        assert!(config.sqlite_fallback_path.is_none());
    }

    #[test]
    fn config_with_sqlite_fallback() {
        let config = MmapStoreConfig::new(PathBuf::from("/tmp/test"))
            .with_sqlite_fallback(PathBuf::from("/tmp/test.db"));
        assert_eq!(
            config.sqlite_fallback_path,
            Some(PathBuf::from("/tmp/test.db"))
        );
    }

    #[test]
    fn config_cold_erasure_is_default_off_and_opt_in() {
        let config = MmapStoreConfig::new(PathBuf::from("/tmp/test"));
        assert_eq!(config.cold_erasure, ColdErasureMode::Disabled);

        let config = MmapStoreConfig::new(PathBuf::from("/tmp/test")).with_cold_erasure_rs();
        assert_eq!(config.cold_erasure, ColdErasureMode::ReedSolomon);
    }

    // --- Cold erasure codec ---

    #[test]
    fn rs_erasure_recovers_from_every_three_of_five_subset() {
        let data = b"cold-tier scrollback retained bytes\nline two\n".to_vec();
        let shards = cold_erasure_encode(&data).unwrap();

        for a in 0..COLD_ERASURE_TOTAL_SHARDS {
            for b in (a + 1)..COLD_ERASURE_TOTAL_SHARDS {
                for c in (b + 1)..COLD_ERASURE_TOTAL_SHARDS {
                    let survivors = vec![shards[a].clone(), shards[b].clone(), shards[c].clone()];
                    let decoded = cold_erasure_decode(&survivors).unwrap();
                    assert_eq!(decoded, data, "subset ({a}, {b}, {c}) failed");
                }
            }
        }
    }

    proptest! {
        #[test]
        fn rs_erasure_fuzz_recovers_after_dropping_up_to_parity(
            data in proptest::collection::vec(any::<u8>(), 0..4096),
            drop_count in 0usize..=2,
            drop_offset in 0usize..COLD_ERASURE_TOTAL_SHARDS,
        ) {
            let shards = cold_erasure_encode(&data).unwrap();
            let dropped: HashSet<usize> = (0..drop_count)
                .map(|offset| (drop_offset + offset) % COLD_ERASURE_TOTAL_SHARDS)
                .collect();
            let survivors: Vec<_> = shards
                .iter()
                .enumerate()
                .filter_map(|(index, shard)| (!dropped.contains(&index)).then_some(shard.clone()))
                .collect();

            prop_assert!(survivors.len() >= COLD_ERASURE_DATA_SHARDS);
            let decoded = cold_erasure_decode(&survivors).unwrap();
            prop_assert_eq!(decoded, data);
        }
    }

    #[test]
    fn rs_erasure_fewer_than_k_survivors_fails_closed() {
        let shards = cold_erasure_encode(b"need at least k survivors").unwrap();
        let err = cold_erasure_decode(&shards[..COLD_ERASURE_DATA_SHARDS - 1]).unwrap_err();
        assert!(matches!(
            err,
            MmapStoreError::InsufficientErasureShards {
                have: 2,
                need: COLD_ERASURE_DATA_SHARDS
            }
        ));
    }

    #[test]
    fn rs_erasure_corrupt_shard_crc_fails_closed() {
        let shards = cold_erasure_encode(b"crc catches corrupted cold shard").unwrap();
        let mut encoded = shards[3].encode_bytes().unwrap();
        let last = encoded.last_mut().expect("non-empty shard encoding");
        *last ^= 0x55;

        let err = ColdErasureShard::decode_bytes(&encoded).unwrap_err();
        assert!(matches!(
            err,
            MmapStoreError::ErasureShardCrcMismatch { index: 3 }
        ));
    }

    // --- MmapStoreError display ---

    #[test]
    fn error_display_unknown_pane() {
        let err = MmapStoreError::UnknownPane(42);
        assert_eq!(format!("{err}"), "unknown pane: 42");
    }

    #[test]
    fn error_display_offset_out_of_bounds() {
        let err = MmapStoreError::OffsetOutOfBounds {
            offset: 100,
            len: 50,
        };
        assert_eq!(format!("{err}"), "offset 100 exceeds file length 50");
    }

    #[test]
    fn error_display_numeric_overflow() {
        let err = MmapStoreError::NumericOverflow("seq");
        assert_eq!(format!("{err}"), "numeric conversion overflow for seq");
    }

    // --- File-backed store: basic operations ---

    #[test]
    fn file_store_append_and_tail() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.append_line(1, "hello").unwrap();
        store.append_line(1, "world").unwrap();

        let lines = store.tail_lines(1, 10).unwrap();
        assert_eq!(lines, vec!["hello", "world"]);
    }

    #[test]
    fn file_store_tail_partial() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        for i in 0..10 {
            store.append_line(1, &format!("line-{i}")).unwrap();
        }

        let last3 = store.tail_lines(1, 3).unwrap();
        assert_eq!(last3, vec!["line-7", "line-8", "line-9"]);
    }

    #[test]
    fn file_store_tail_more_than_exists() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.append_line(1, "only-line").unwrap();

        let lines = store.tail_lines(1, 100).unwrap();
        assert_eq!(lines, vec!["only-line"]);
    }

    #[test]
    fn file_store_tail_zero_returns_empty() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.append_line(1, "data").unwrap();

        let lines = store.tail_lines(1, 0).unwrap();
        assert_eq!(lines, [] as [std::string::String; 0]);
    }

    #[test]
    fn file_store_line_count() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        assert_eq!(store.line_count(1), 0);

        store.append_line(1, "a").unwrap();
        store.append_line(1, "b").unwrap();
        store.append_line(1, "c").unwrap();

        assert_eq!(store.line_count(1), 3);
    }

    #[test]
    fn file_store_multiple_panes() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.append_line(1, "pane1-line1").unwrap();
        store.append_line(2, "pane2-line1").unwrap();
        store.append_line(1, "pane1-line2").unwrap();

        assert_eq!(store.line_count(1), 2);
        assert_eq!(store.line_count(2), 1);

        let p1 = store.tail_lines(1, 10).unwrap();
        assert_eq!(p1, vec!["pane1-line1", "pane1-line2"]);

        let p2 = store.tail_lines(2, 10).unwrap();
        assert_eq!(p2, vec!["pane2-line1"]);
    }

    #[test]
    fn file_store_reads_line_by_seq() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.append_line(1, "zero").unwrap();
        store.append_line(1, "one").unwrap();
        store.append_line(1, "two").unwrap();

        assert_eq!(store.line_at(1, 0).unwrap().as_deref(), Some("zero"));
        assert_eq!(store.line_at(1, 1).unwrap().as_deref(), Some("one"));
        assert_eq!(store.line_at(1, 2).unwrap().as_deref(), Some("two"));
        assert_eq!(store.line_at(1, 3).unwrap(), None);
    }

    #[test]
    fn file_store_prunes_retained_window_metadata() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        for idx in 0..5 {
            store.append_line(1, &format!("line-{idx}")).unwrap();
        }

        store.prune_before(1, 2).unwrap();

        assert_eq!(store.line_count(1), 3);
        assert_eq!(store.line_at(1, 1).unwrap(), None);
        assert_eq!(store.line_at(1, 2).unwrap().as_deref(), Some("line-2"));
        assert_eq!(
            store.tail_lines(1, 10).unwrap(),
            vec!["line-2", "line-3", "line-4"]
        );
    }

    #[test]
    fn file_store_compacts_stale_prefix_after_prune() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        for idx in 0..8 {
            store
                .append_line(1, &format!("line-{idx}-{}", "x".repeat(64)))
                .unwrap();
        }
        let full_file_bytes = store.file_bytes(1);

        store.prune_before(1, 6).unwrap();
        let retained_before_compact = store.retained_bytes(1);
        assert!(
            full_file_bytes > retained_before_compact,
            "prune should leave a stale file prefix before compaction"
        );

        assert!(store.compact_pane_if_stale(1, 1).unwrap());

        assert_eq!(store.file_bytes(1), store.retained_bytes(1));
        assert!(
            store.file_bytes(1) < full_file_bytes,
            "compaction should shrink physical bytes to the retained suffix"
        );
        assert_eq!(store.line_count(1), 2);
        assert_eq!(store.oldest_seq(1), Some(6));
        assert_eq!(
            store.tail_lines(1, 10).unwrap(),
            vec![
                format!("line-6-{}", "x".repeat(64)),
                format!("line-7-{}", "x".repeat(64))
            ]
        );
        let expected_line_6 = format!("line-6-{}", "x".repeat(64));
        assert_eq!(
            store.line_at(1, 6).unwrap().as_deref(),
            Some(expected_line_6.as_str())
        );
    }

    #[test]
    fn sequence_journal_pressure_forces_crash_safe_compaction_before_cap() {
        let dir = temp_dir();
        {
            let mut store = file_only_store(dir.path());
            for idx in 0..100 {
                store.append_line(1, &format!("line-{idx}")).unwrap();
            }
            for oldest in 1..90 {
                store.prune_before(1, oldest).unwrap();
            }
            assert!(
                std::fs::metadata(dir.path().join("1.seq")).unwrap().len()
                    < PANE_BASE_SEQ_JOURNAL_COMPACT_BYTES,
                "journal-pressure compaction must collapse sequence history"
            );
        }

        let mut reopened = file_only_store(dir.path());
        reopened.ensure_pane(1).unwrap();
        assert_eq!(reopened.oldest_seq(1), Some(89));
        assert_eq!(reopened.line_count(1), 11);
        assert_eq!(reopened.line_at(1, 89).unwrap().as_deref(), Some("line-89"));
    }

    #[test]
    fn sequence_journal_capacity_refuses_prune_before_recovery_becomes_unreadable() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());
        store.append_line(1, "zero").unwrap();
        store.append_line(1, "one").unwrap();
        store
            .panes
            .get_mut(&1)
            .unwrap()
            .base_seq_file
            .set_len(PANE_BASE_SEQ_JOURNAL_MAX_BYTES)
            .unwrap();

        let error = store
            .prune_before(1, 1)
            .expect_err("journal capacity must fail before logical authority advances");
        assert!(matches!(
            error,
            MmapStoreError::PaneSequenceJournalFull { .. }
        ));
        assert_eq!(store.oldest_seq(1), Some(0));
        assert_eq!(store.line_count(1), 2);
    }

    #[test]
    fn compaction_is_crash_safe_no_temp_leak_and_retained_intact() {
        // ft-odrq7: compaction must rewrite the log via temp + fsync + atomic
        // rename — never an in-place `truncate(true)`. After a successful
        // compaction the live log holds exactly the retained suffix and no
        // interrupted-compaction temp file survives.
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        for idx in 0..8 {
            store.append_line(1, &format!("line-{idx}")).unwrap();
        }
        store.prune_before(1, 6).unwrap();
        assert!(store.compact_pane_if_stale(1, 1).unwrap());

        let tmp_exists = std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry
                .ok()
                .and_then(|entry| entry.file_name().to_str().map(ToString::to_string))
                .is_some_and(|name| name.starts_with(".1.log.compact-installing-"))
        });
        assert!(
            !tmp_exists,
            "compaction temp must be renamed away, not leaked"
        );

        // Reading the raw file proves the retained bytes are the WHOLE file —
        // i.e. produced by the rename, not re-grown from a zeroed-in-place log.
        let raw = std::fs::read_to_string(dir.path().join("1.log")).unwrap();
        assert_eq!(raw, "\0FTMMAP1:6\nline-6\nline-7\n");
        assert_eq!(store.tail_lines(1, 10).unwrap(), vec!["line-6", "line-7"]);
        assert_eq!(store.oldest_seq(1), Some(6));
    }

    #[test]
    fn compaction_ignores_unowned_legacy_staging_file() {
        // A fixed staging name can be pre-created or hard-linked. Compaction
        // must publish through its own create-new sibling and leave the
        // unowned path untouched.
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        for idx in 0..8 {
            store.append_line(1, &format!("row-{idx}")).unwrap();
        }
        store.prune_before(1, 6).unwrap();

        // Simulate the crash-leftover temp of an interrupted compaction.
        let unowned_target = dir.path().join("unowned-legacy-staging-target");
        std::fs::write(&unowned_target, b"GARBAGE-FROM-INTERRUPTED-COMPACTION\n").unwrap();
        std::fs::hard_link(&unowned_target, dir.path().join("1.log.compact.tmp")).unwrap();

        assert!(store.compact_pane_if_stale(1, 1).unwrap());

        let raw = std::fs::read_to_string(dir.path().join("1.log")).unwrap();
        assert_eq!(
            raw, "\0FTMMAP1:6\nrow-6\nrow-7\n",
            "leftover temp must not corrupt the compacted log"
        );
        assert!(!raw.contains("GARBAGE"));
        assert_eq!(store.tail_lines(1, 10).unwrap(), vec!["row-6", "row-7"]);
        assert_eq!(
            std::fs::read(dir.path().join("1.log.compact.tmp")).unwrap(),
            b"GARBAGE-FROM-INTERRUPTED-COMPACTION\n"
        );
        assert_eq!(
            std::fs::read(unowned_target).unwrap(),
            b"GARBAGE-FROM-INTERRUPTED-COMPACTION\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn compaction_ignores_symlink_at_legacy_staging_name() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir();
        let mut store = file_only_store(dir.path());
        for idx in 0..8 {
            store.append_line(1, &format!("row-{idx}")).unwrap();
        }
        store.prune_before(1, 6).unwrap();

        let target = dir.path().join("unrelated-target");
        std::fs::write(&target, b"do-not-touch").unwrap();
        symlink(&target, dir.path().join("1.log.compact.tmp")).unwrap();

        assert!(store.compact_pane_if_stale(1, 1).unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), b"do-not-touch");
        assert_eq!(
            store.tail_lines(1, 10).unwrap(),
            vec!["row-6".to_string(), "row-7".to_string()]
        );
    }

    #[test]
    fn default_off_compaction_does_not_write_erasure_sidecars() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        for idx in 0..8 {
            store.append_line(1, &format!("line-{idx}")).unwrap();
        }
        store.prune_before(1, 5).unwrap();
        assert!(store.compact_pane_if_stale(1, 1).unwrap());

        let raw = std::fs::read_to_string(dir.path().join("1.log")).unwrap();
        assert_eq!(raw, "\0FTMMAP1:5\nline-5\nline-6\nline-7\n");
        for path in erasure_sidecar_paths(dir.path(), 1) {
            assert!(
                !path.exists(),
                "default-off storage.cold.erasure must not write {}",
                path.display()
            );
        }
    }

    #[test]
    fn rs_compaction_writes_recoverable_sidecars_for_retained_bytes() {
        let dir = temp_dir();
        let mut store = rs_store(dir.path());

        for idx in 0..9 {
            store
                .append_line(1, &format!("row-{idx}-{}", "payload".repeat(idx + 1)))
                .unwrap();
        }
        store.prune_before(1, 4).unwrap();
        assert!(store.compact_pane_if_stale(1, 1).unwrap());

        let raw = std::fs::read(dir.path().join("1.log")).unwrap();
        let paths = erasure_sidecar_paths(dir.path(), 1);
        assert_eq!(paths.len(), COLD_ERASURE_TOTAL_SHARDS);
        for path in &paths {
            assert!(path.exists(), "missing erasure sidecar {}", path.display());
        }

        // Simulate loss of two sidecars without deleting files: use a mixed
        // 3-of-5 survivor set that includes data and parity shards.
        let survivors = [0usize, 3, 4]
            .into_iter()
            .map(|index| {
                let bytes = std::fs::read(&paths[index]).unwrap();
                ColdErasureShard::decode_bytes(&bytes).unwrap()
            })
            .collect::<Vec<_>>();
        let recovered = cold_erasure_decode(&survivors).unwrap();
        assert_eq!(recovered, raw);
        assert_eq!(
            store.recover_pane_bytes_from_erasure_shards(1).unwrap(),
            Some(raw)
        );
    }

    #[test]
    fn rs_refresh_is_explicit_and_off_hot_append_path() {
        let dir = temp_dir();
        let mut store = rs_store(dir.path());

        store.append_line(1, "hot-path-line").unwrap();
        for path in erasure_sidecar_paths(dir.path(), 1) {
            assert!(
                !path.exists(),
                "append path must not synchronously encode erasure sidecar {}",
                path.display()
            );
        }

        assert!(store.refresh_pane_erasure_shards(1).unwrap());
        let raw = std::fs::read(dir.path().join("1.log")).unwrap();
        let recovered = store.recover_pane_bytes_from_erasure_shards(1).unwrap();
        assert_eq!(recovered, Some(raw));
    }

    #[test]
    fn rs_recovery_skips_one_corrupt_shard_when_quorum_remains() {
        let dir = temp_dir();
        let mut store = rs_store(dir.path());
        store
            .append_line(1, "quorum-survives-one-corrupt-shard")
            .unwrap();
        assert!(store.refresh_pane_erasure_shards(1).unwrap());
        let raw = std::fs::read(dir.path().join("1.log")).unwrap();
        let first_sidecar = erasure_sidecar_paths(dir.path(), 1)
            .into_iter()
            .next()
            .expect("first erasure sidecar path");
        let mut corrupt = std::fs::read(&first_sidecar).unwrap();
        let last = corrupt.last_mut().expect("non-empty sidecar");
        *last ^= 0xff;
        std::fs::write(&first_sidecar, corrupt).unwrap();

        assert_eq!(
            store.recover_pane_bytes_from_erasure_shards(1).unwrap(),
            Some(raw)
        );
    }

    #[test]
    fn rs_recovery_selects_coherent_quorum_across_interrupted_publication() {
        let dir = temp_dir();
        let mut store = rs_store(dir.path());
        store.append_line(1, "old-generation").unwrap();
        assert!(store.refresh_pane_erasure_shards(1).unwrap());
        let paths = erasure_sidecar_paths(dir.path(), 1);
        let old_first_shard = std::fs::read(&paths[0]).unwrap();

        store.append_line(1, "new-generation").unwrap();
        assert!(store.refresh_pane_erasure_shards(1).unwrap());
        let expected = std::fs::read(dir.path().join("1.log")).unwrap();

        // Simulate a crash after publishing one shard from the preceding
        // generation over an otherwise complete new-generation set.
        std::fs::write(&paths[0], old_first_shard).unwrap();
        assert_eq!(
            store.recover_pane_bytes_from_erasure_shards(1).unwrap(),
            Some(expected)
        );
    }

    #[cfg(unix)]
    #[test]
    fn rs_recovery_rejects_symlinked_sidecar_without_reading_target() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir();
        let mut store = rs_store(dir.path());
        store.append_line(1, "hot-path-line").unwrap();
        let target = dir.path().join("unrelated-erasure-target");
        std::fs::write(&target, b"not-a-sidecar").unwrap();
        let first_sidecar = erasure_sidecar_paths(dir.path(), 1)
            .into_iter()
            .next()
            .expect("first erasure sidecar path");
        symlink(&target, first_sidecar).unwrap();

        assert!(store.recover_pane_bytes_from_erasure_shards(1).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"not-a-sidecar");
    }

    #[test]
    fn file_store_clear_pane_resets_offsets_and_content() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.append_line(1, "before").unwrap();
        store.clear_pane(1).unwrap();
        store.append_line(1, "after").unwrap();

        assert_eq!(store.line_count(1), 1);
        assert_eq!(store.line_at(1, 0).unwrap().as_deref(), Some("after"));
        assert_eq!(store.tail_lines(1, 10).unwrap(), vec!["after"]);
    }

    #[test]
    fn versioned_replacement_stage_is_exact_durable_and_preserves_published_ledger() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());
        store.append_line(0, "published-predecessor").unwrap();
        let predecessor_log = std::fs::read(dir.path().join("0.log")).unwrap();
        let predecessor_sequence = std::fs::read(dir.path().join("0.seq")).unwrap();
        let replacement_id = (1_u64 << 63) | 77;
        let records = vec!["sealed-row-one".to_string(), "sealed-row-two".to_string()];

        let staged = store
            .stage_versioned_pane_replacement(replacement_id, &records, 2, 1024)
            .expect("stage exact replacement ledger");
        assert_eq!(staged.pane_id(), replacement_id);
        assert_eq!(staged.record_count(), 2);
        assert_eq!(staged.record_bytes(), 28);
        assert_eq!(staged.committed_bytes(), 30);
        assert!(!staged.reused_existing());
        assert_eq!(
            std::fs::read(dir.path().join("0.log")).unwrap(),
            predecessor_log
        );
        assert_eq!(
            std::fs::read(dir.path().join("0.seq")).unwrap(),
            predecessor_sequence
        );
        store
            .verify_staged_pane_ledger(staged, &records)
            .expect("reopen and verify staged ledger");
    }

    #[test]
    fn deterministic_replacement_retry_reuses_one_bounded_ledger_slot() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());
        let replacement_id = (1_u64 << 63) | 78;
        let records = vec!["opaque-record-a".to_string(), "opaque-record-b".to_string()];
        let first = store
            .stage_versioned_pane_replacement(replacement_id, &records, 2, 1024)
            .expect("stage first transaction attempt");
        assert!(!first.reused_existing());
        let log_before = std::fs::read(dir.path().join(format!("{replacement_id}.log"))).unwrap();
        let second = store
            .stage_versioned_pane_replacement(replacement_id, &records, 2, 1024)
            .expect("reuse deterministic transaction attempt");
        assert!(second.reused_existing());
        assert_eq!(first.committed_bytes(), second.committed_bytes());
        assert_eq!(
            std::fs::read(dir.path().join(format!("{replacement_id}.log"))).unwrap(),
            log_before
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&replacement_id.to_string())
                })
                .count(),
            2,
            "one deterministic transaction owns exactly one .log/.seq pair"
        );
    }

    #[test]
    fn empty_versioned_replacement_has_zero_committed_bytes_and_bounds_include_delimiters() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());
        let empty = store
            .stage_versioned_pane_replacement((1_u64 << 63) | 79, &[], 0, 0)
            .expect("stage atomic empty replacement");
        assert_eq!(empty.record_count(), 0);
        assert_eq!(empty.record_bytes(), 0);
        assert_eq!(empty.committed_bytes(), 0);

        let error = store
            .stage_versioned_pane_replacement((1_u64 << 63) | 80, &["four".to_string()], 1, 4)
            .expect_err("record delimiter participates in committed-byte bound");
        assert!(matches!(
            error,
            MmapStoreError::PaneSnapshotLimitExceeded {
                limit_name: "committed_bytes",
                limit: 4,
                observed: 5,
            }
        ));
    }

    #[test]
    fn file_store_unknown_pane_tail_errors() {
        let dir = temp_dir();
        let store = file_only_store(dir.path());

        let err = store.tail_lines(999, 10).unwrap_err();
        assert!(matches!(err, MmapStoreError::UnknownPane(999)));
    }

    // --- Storage mode ---

    #[test]
    fn storage_mode_file_backed() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        assert!(store.pane_storage_mode(1).is_none());

        store.append_line(1, "data").unwrap();
        assert_eq!(store.pane_storage_mode(1), Some(PaneStorageMode::Mmap));
    }

    // --- ensure_pane ---

    #[test]
    fn ensure_pane_creates_file() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.ensure_pane(42).unwrap();
        assert_eq!(store.pane_storage_mode(42), Some(PaneStorageMode::Mmap));
        assert_eq!(store.line_count(42), 0);
    }

    // --- Hybrid store (file + SQLite) ---

    #[test]
    fn hybrid_store_keeps_one_authority_for_healthy_mmap_pane() {
        let dir = temp_dir();
        let db_path = dir.path().join("fallback.db");
        let mut store = hybrid_store(dir.path(), &db_path);

        store.append_line(1, "hello").unwrap();
        store.append_line(1, "world").unwrap();

        let lines = store.tail_lines(1, 10).unwrap();
        assert_eq!(lines, vec!["hello", "world"]);
        assert_eq!(store.line_count(1), 2);

        let conn = Connection::open(&db_path).unwrap();
        let sqlite_line_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mmap_scrollback_lines WHERE pane_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let authority_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mmap_scrollback_fallback_panes WHERE pane_id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(sqlite_line_count, 0);
        assert_eq!(authority_count, 0);
    }

    #[test]
    fn sqlite_fallback_empty_authority_survives_clear_and_restart() {
        let dir = temp_dir();
        let db_path = dir.path().join("fallback.db");

        {
            let mut sqlite = SqliteFallbackStore::open(&db_path).unwrap();
            assert_eq!(sqlite.append_line_auto_seq(7, "old").unwrap(), 0);
            sqlite.clear_pane(7).unwrap();
            assert!(sqlite.is_authority(7).unwrap());
            assert_eq!(sqlite.next_seq(7).unwrap(), 0);
        }

        let mut reopened = hybrid_store(dir.path(), &db_path);
        reopened.ensure_pane(7).unwrap();
        assert_eq!(
            reopened.pane_storage_mode(7),
            Some(PaneStorageMode::SqliteFallback)
        );
        assert_eq!(reopened.tail_lines(7, 10).unwrap(), Vec::<String>::new());
        assert_eq!(reopened.append_line(7, "new").unwrap(), 0);
        assert_eq!(reopened.line_at(7, 0).unwrap().as_deref(), Some("new"));
        assert!(!dir.path().join("7.log").exists());
    }

    #[test]
    fn hybrid_store_sqlite_fallback_for_unknown_pane_tail() {
        let dir = temp_dir();
        let db_path = dir.path().join("fallback.db");

        // Insert data directly into SQLite
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS mmap_scrollback_lines (
                     pane_id INTEGER NOT NULL,
                     seq INTEGER NOT NULL,
                     content TEXT NOT NULL,
                     PRIMARY KEY (pane_id, seq)
                 );
                 INSERT INTO mmap_scrollback_lines VALUES (5, 0, 'sqlite-line-0');
                 INSERT INTO mmap_scrollback_lines VALUES (5, 1, 'sqlite-line-1');",
            )
            .unwrap();
        }

        let store = hybrid_store(dir.path(), &db_path);

        // Pane 5 isn't in file-backed store, should fall through to SQLite
        let lines = store.tail_lines(5, 10).unwrap();
        assert_eq!(lines, vec!["sqlite-line-0", "sqlite-line-1"]);
    }

    #[test]
    fn hybrid_store_storage_mode_sqlite_pane() {
        let dir = temp_dir();
        let db_path = dir.path().join("fallback.db");

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS mmap_scrollback_lines (
                     pane_id INTEGER NOT NULL,
                     seq INTEGER NOT NULL,
                     content TEXT NOT NULL,
                     PRIMARY KEY (pane_id, seq)
                 );
                 INSERT INTO mmap_scrollback_lines VALUES (10, 0, 'data');",
            )
            .unwrap();
        }

        let store = hybrid_store(dir.path(), &db_path);

        // Pane 10 is only in SQLite
        assert_eq!(
            store.pane_storage_mode(10),
            Some(PaneStorageMode::SqliteFallback)
        );
        // Pane 99 is nowhere
        assert!(store.pane_storage_mode(99).is_none());
    }

    #[test]
    fn hybrid_store_ensure_pane_fallback() {
        let dir = temp_dir();
        let db_path = dir.path().join("fallback.db");
        let mut store = hybrid_store(dir.path(), &db_path);

        // ensure_pane should succeed (creates file)
        store.ensure_pane(7).unwrap();
        assert_eq!(store.pane_storage_mode(7), Some(PaneStorageMode::Mmap));
    }

    // --- File-backed: multi-line content ---

    #[test]
    fn file_store_unicode_content() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.append_line(1, "hello \u{1F600}").unwrap();
        store.append_line(1, "\u{4E16}\u{754C}").unwrap();

        let lines = store.tail_lines(1, 10).unwrap();
        assert_eq!(lines, vec!["hello \u{1F600}", "\u{4E16}\u{754C}"]);
    }

    #[test]
    fn file_store_empty_lines() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        store.append_line(1, "").unwrap();
        store.append_line(1, "middle").unwrap();
        store.append_line(1, "").unwrap();

        let lines = store.tail_lines(1, 10).unwrap();
        assert_eq!(lines, vec!["", "middle", ""]);
        assert_eq!(store.line_count(1), 3);
    }

    #[test]
    fn file_store_rejects_embedded_line_delimiters_before_mutation() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        assert!(matches!(
            store.append_line(1, "one\ntwo"),
            Err(MmapStoreError::InvalidLineRecord)
        ));
        assert!(matches!(
            store.append_line(1, "carriage\rreturn"),
            Err(MmapStoreError::InvalidLineRecord)
        ));
        assert_eq!(store.line_count(1), 0);
        assert_eq!(std::fs::read(dir.path().join("1.log")).unwrap(), b"");
    }

    #[test]
    fn file_store_rejects_invalid_utf8_in_committed_record() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("1.log"), b"valid\ninvalid-\xff\n").unwrap();
        let mut store = file_only_store(dir.path());
        store.ensure_pane(1).unwrap();

        assert_eq!(store.line_at(1, 0).unwrap().as_deref(), Some("valid"));
        assert!(matches!(
            store.line_at(1, 1),
            Err(MmapStoreError::InvalidPaneLogUtf8(_))
        ));
    }

    #[test]
    fn file_store_long_line() {
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        let long = "x".repeat(100_000);
        store.append_line(1, &long).unwrap();

        let lines = store.tail_lines(1, 1).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 100_000);
    }

    // --- Persistence: reopen store with existing data ---

    #[test]
    fn file_store_persists_across_reopen() {
        let dir = temp_dir();

        {
            let mut store = file_only_store(dir.path());
            store.append_line(1, "persisted-a").unwrap();
            store.append_line(1, "persisted-b").unwrap();
        }

        // Re-open store from same directory
        let mut store2 = file_only_store(dir.path());
        store2.ensure_pane(1).unwrap();

        let lines = store2.tail_lines(1, 10).unwrap();
        assert_eq!(lines, vec!["persisted-a", "persisted-b"]);
        assert_eq!(store2.line_count(1), 2);
    }

    #[test]
    fn file_store_append_after_reopen() {
        let dir = temp_dir();

        {
            let mut store = file_only_store(dir.path());
            store.append_line(1, "first").unwrap();
        }

        let mut store2 = file_only_store(dir.path());
        store2.append_line(1, "second").unwrap();

        let lines = store2.tail_lines(1, 10).unwrap();
        assert_eq!(lines, vec!["first", "second"]);
    }

    #[test]
    fn file_store_compacted_base_sequence_persists_across_reopen() {
        let dir = temp_dir();

        {
            let mut store = file_only_store(dir.path());
            for idx in 0..8 {
                store.append_line(1, &format!("line-{idx}")).unwrap();
            }
            store.prune_before(1, 6).unwrap();
            assert!(store.compact_pane_if_stale(1, 1).unwrap());
            assert_eq!(store.oldest_seq(1), Some(6));
        }

        let mut reopened = file_only_store(dir.path());
        reopened.ensure_pane(1).unwrap();
        assert_eq!(reopened.oldest_seq(1), Some(6));
        assert_eq!(reopened.line_at(1, 5).unwrap(), None);
        assert_eq!(reopened.line_at(1, 6).unwrap().as_deref(), Some("line-6"));
        assert_eq!(reopened.line_at(1, 7).unwrap().as_deref(), Some("line-7"));
        reopened.append_line(1, "line-8").unwrap();
        assert_eq!(reopened.line_at(1, 8).unwrap().as_deref(), Some("line-8"));
    }

    #[test]
    fn file_store_empty_retention_compaction_preserves_next_sequence() {
        let dir = temp_dir();

        {
            let mut store = file_only_store(dir.path());
            for idx in 0..4 {
                assert_eq!(store.append_line(1, &format!("line-{idx}")).unwrap(), idx);
            }
            store.prune_before(1, 4).unwrap();
            assert_eq!(store.oldest_seq(1), None);
            assert_eq!(store.next_seq(1).unwrap(), 4);
            assert!(store.compact_pane_if_stale(1, 1).unwrap());
            assert_eq!(
                std::fs::read(dir.path().join("1.log")).unwrap(),
                b"\0FTMMAP1:4\n"
            );
        }

        let mut reopened = file_only_store(dir.path());
        reopened.ensure_pane(1).unwrap();
        assert_eq!(reopened.oldest_seq(1), None);
        assert_eq!(reopened.next_seq(1).unwrap(), 4);
        assert_eq!(reopened.append_line(1, "line-4").unwrap(), 4);
        assert_eq!(reopened.line_at(1, 4).unwrap().as_deref(), Some("line-4"));
    }

    #[test]
    fn file_store_pruned_base_sequence_persists_without_compaction() {
        let dir = temp_dir();

        {
            let mut store = file_only_store(dir.path());
            for idx in 0..8 {
                store.append_line(1, &format!("line-{idx}")).unwrap();
            }
            store.prune_before(1, 6).unwrap();
            assert_eq!(store.oldest_seq(1), Some(6));
        }

        // The content log still has its stale prefix, proving that recovery is
        // using the synchronized logical-prune journal rather than compaction.
        let raw = std::fs::read_to_string(dir.path().join("1.log")).unwrap();
        assert!(raw.starts_with("line-0\nline-1\n"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("1.seq")).unwrap(),
            "FTSEQ1:6\n"
        );

        let mut reopened = file_only_store(dir.path());
        reopened.ensure_pane(1).unwrap();
        assert_eq!(reopened.oldest_seq(1), Some(6));
        assert_eq!(reopened.line_at(1, 5).unwrap(), None);
        assert_eq!(reopened.line_at(1, 6).unwrap().as_deref(), Some("line-6"));
        assert_eq!(reopened.line_at(1, 7).unwrap().as_deref(), Some("line-7"));
        reopened.append_line(1, "line-8").unwrap();
        assert_eq!(reopened.line_at(1, 8).unwrap().as_deref(), Some("line-8"));
    }

    #[test]
    fn file_store_clear_resets_persisted_sequence_authority() {
        let dir = temp_dir();

        {
            let mut store = file_only_store(dir.path());
            for idx in 0..4 {
                store.append_line(1, &format!("old-{idx}")).unwrap();
            }
            store.prune_before(1, 3).unwrap();
            store.clear_pane(1).unwrap();
        }

        let mut reopened = file_only_store(dir.path());
        reopened.ensure_pane(1).unwrap();
        assert_eq!(reopened.oldest_seq(1), None);
        assert_eq!(reopened.append_line(1, "new-zero").unwrap(), 0);
        assert_eq!(reopened.line_at(1, 0).unwrap().as_deref(), Some("new-zero"));
        assert_eq!(std::fs::read(dir.path().join("1.seq")).unwrap(), b"");
    }

    #[test]
    fn pane_file_sequence_journal_ignores_torn_tail_and_rejects_complete_corruption() {
        let dir = temp_dir();
        std::fs::write(dir.path().join("1.log"), b"line-0\nline-1\n").unwrap();
        std::fs::write(dir.path().join("1.seq"), b"FTSEQ1:1\nFTSEQ1:").unwrap();
        let pane = PaneFile::open(dir.path(), 1).unwrap();
        assert_eq!(pane.base_seq, 1);
        assert_eq!(pane.line_at(1).unwrap().as_deref(), Some("line-1"));

        std::fs::write(dir.path().join("2.log"), b"line-0\n").unwrap();
        std::fs::write(dir.path().join("2.seq"), b"not-a-sequence\n").unwrap();
        assert!(matches!(
            PaneFile::open(dir.path(), 2),
            Err(MmapStoreError::InvalidPaneLogHeader(_))
        ));

        std::fs::write(dir.path().join("3.log"), b"line-0\nline-1\nline-2\n").unwrap();
        std::fs::write(dir.path().join("3.seq"), b"FTSEQ1:2\nFTSEQ1:1\n").unwrap();
        assert!(matches!(
            PaneFile::open(dir.path(), 3),
            Err(MmapStoreError::InvalidPaneLogHeader(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn pane_file_rejects_symlinked_log_and_sequence_authorities() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir();
        let log_target = dir.path().join("unrelated-log-target");
        std::fs::write(&log_target, b"do-not-read-or-write\n").unwrap();
        symlink(&log_target, dir.path().join("41.log")).unwrap();
        assert!(PaneFile::open(dir.path(), 41).is_err());
        assert_eq!(
            std::fs::read(&log_target).unwrap(),
            b"do-not-read-or-write\n"
        );

        std::fs::write(dir.path().join("42.log"), b"committed\n").unwrap();
        let sequence_target = dir.path().join("unrelated-sequence-target");
        std::fs::write(&sequence_target, b"FTSEQ1:0\n").unwrap();
        symlink(&sequence_target, dir.path().join("42.seq")).unwrap();
        assert!(PaneFile::open(dir.path(), 42).is_err());
        assert_eq!(std::fs::read(&sequence_target).unwrap(), b"FTSEQ1:0\n");
    }

    // --- SQLite-only fallback store ---

    #[test]
    fn sqlite_fallback_store_basic() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");
        let mut sqlite = SqliteFallbackStore::open(&db_path).unwrap();

        sqlite.append_line_auto_seq(1, "line-a").unwrap();
        sqlite.append_line_auto_seq(1, "line-b").unwrap();

        let lines = sqlite.tail_lines(1, 10).unwrap();
        assert_eq!(lines, vec!["line-a", "line-b"]);
        assert_eq!(sqlite.line_count(1).unwrap(), 2);
    }

    #[test]
    fn sqlite_fallback_store_tail_zero() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");
        let mut sqlite = SqliteFallbackStore::open(&db_path).unwrap();

        sqlite.append_line_auto_seq(1, "data").unwrap();

        let lines = sqlite.tail_lines(1, 0).unwrap();
        assert_eq!(lines, [] as [std::string::String; 0]);
    }

    #[test]
    fn sqlite_fallback_store_multiple_panes() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");
        let mut sqlite = SqliteFallbackStore::open(&db_path).unwrap();

        sqlite.append_line_auto_seq(1, "p1-a").unwrap();
        sqlite.append_line_auto_seq(2, "p2-a").unwrap();
        sqlite.append_line_auto_seq(1, "p1-b").unwrap();

        assert_eq!(sqlite.line_count(1).unwrap(), 2);
        assert_eq!(sqlite.line_count(2).unwrap(), 1);
        assert_eq!(sqlite.line_count(99).unwrap(), 0);
    }

    #[test]
    fn sqlite_fallback_store_explicit_seq() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");
        let mut sqlite = SqliteFallbackStore::open(&db_path).unwrap();

        sqlite.append_line_with_seq(1, 0, "zero").unwrap();
        sqlite.append_line_with_seq(1, 1, "one").unwrap();
        sqlite.append_line_with_seq(1, 5, "five").unwrap();

        let lines = sqlite.tail_lines(1, 2).unwrap();
        assert_eq!(lines, vec!["one", "five"]);
    }

    #[test]
    fn sqlite_fallback_store_tail_partial() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");
        let mut sqlite = SqliteFallbackStore::open(&db_path).unwrap();

        for i in 0..20 {
            sqlite
                .append_line_auto_seq(1, &format!("line-{i}"))
                .unwrap();
        }

        let last5 = sqlite.tail_lines(1, 5).unwrap();
        assert_eq!(
            last5,
            vec!["line-15", "line-16", "line-17", "line-18", "line-19"]
        );
    }

    #[test]
    fn sqlite_fallback_full_prune_preserves_next_sequence() {
        let dir = temp_dir();
        let db_path = dir.path().join("sqlite-sequence.db");
        let mut sqlite = SqliteFallbackStore::open(&db_path).unwrap();

        assert_eq!(sqlite.append_line_auto_seq(1, "zero").unwrap(), 0);
        assert_eq!(sqlite.append_line_auto_seq(1, "one").unwrap(), 1);
        sqlite.prune_before(1, 2).unwrap();
        assert_eq!(sqlite.line_count(1).unwrap(), 0);
        assert_eq!(sqlite.next_seq(1).unwrap(), 2);
        assert_eq!(sqlite.append_line_auto_seq(1, "two").unwrap(), 2);
        assert_eq!(sqlite.line_at(1, 2).unwrap().as_deref(), Some("two"));
    }

    // --- PaneFile: scan_offsets ---

    #[test]
    fn pane_file_scan_offsets_empty_file() {
        let dir = temp_dir();
        let path = dir.path().join("empty.log");
        std::fs::write(&path, "").unwrap();

        let (offsets, len) = PaneFile::scan_offsets(&path).unwrap();
        assert!(offsets.is_empty());
        assert_eq!(len, 0);
    }

    #[test]
    fn pane_file_scan_offsets_single_line() {
        let dir = temp_dir();
        let path = dir.path().join("single.log");
        std::fs::write(&path, "hello\n").unwrap();

        let (offsets, len) = PaneFile::scan_offsets(&path).unwrap();
        assert_eq!(offsets, vec![LineOffset(0)]);
        assert_eq!(len, 6); // "hello\n" = 6 bytes
    }

    #[test]
    fn pane_file_scan_offsets_multiple_lines() {
        let dir = temp_dir();
        let path = dir.path().join("multi.log");
        std::fs::write(&path, "ab\ncde\nf\n").unwrap();

        let (offsets, len) = PaneFile::scan_offsets(&path).unwrap();
        // "ab\n" at 0, "cde\n" at 3, "f\n" at 7
        assert_eq!(offsets, vec![LineOffset(0), LineOffset(3), LineOffset(7)]);
        assert_eq!(len, 9);
    }

    #[test]
    fn pane_file_scan_offsets_excludes_torn_trailing_record() {
        let dir = temp_dir();
        let path = dir.path().join("torn.log");
        std::fs::write(&path, b"complete\ninterrupted").unwrap();

        let (offsets, len) = PaneFile::scan_offsets(&path).unwrap();
        assert_eq!(offsets, vec![LineOffset(0)]);
        assert_eq!(len, 9);

        // Use the path expected by PaneFile::open for the actual recovery
        // scenario rather than the standalone scanner fixture above.
        std::fs::write(dir.path().join("42.log"), b"complete\ninterrupted").unwrap();
        let mut pane = PaneFile::open(dir.path(), 42).unwrap();
        assert!(pane.trailing_partial);
        let seq = pane.append_line("after-recovery").unwrap();
        assert_eq!(seq, 1);
        assert_eq!(pane.line_at(0).unwrap().as_deref(), Some("complete"));
        assert_eq!(pane.line_at(1).unwrap().as_deref(), Some("after-recovery"));
        assert_eq!(
            pane.tail_lines(2).unwrap(),
            vec!["complete".to_string(), "after-recovery".to_string()]
        );
        assert_eq!(
            std::fs::read(dir.path().join("42.log")).unwrap(),
            b"complete\nafter-recovery\n"
        );
    }

    #[test]
    fn read_pane_snapshot_is_read_only_and_excludes_torn_tail() {
        let dir = temp_dir();
        let log_path = dir.path().join("7.log");
        std::fs::write(&log_path, b"zero\none\ntorn").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let log_before = std::fs::metadata(&log_path).unwrap();

        let snapshot = read_pane_snapshot(dir.path(), 7, 8, 1024, 1024).unwrap();

        assert_eq!(snapshot.oldest_seq, Some(0));
        assert_eq!(snapshot.next_seq, 2);
        assert_eq!(snapshot.records, vec!["zero", "one"]);
        assert_eq!(snapshot.retained_record_bytes, 9);
        assert_eq!(snapshot.committed_bytes, 9);
        assert_eq!(snapshot.sequence_bytes, 0);
        assert_eq!(snapshot.physical_bytes, 13);
        assert_eq!(snapshot.trailing_uncommitted_bytes, 4);
        assert!(
            !dir.path().join("7.seq").exists(),
            "read-only snapshot must not create a missing sequence journal"
        );
        let log_after = std::fs::metadata(&log_path).unwrap();
        assert_eq!(log_after.len(), log_before.len());
        assert_eq!(log_after.modified().ok(), log_before.modified().ok());
    }

    #[test]
    fn read_pane_snapshot_applies_sequence_journal_and_bounds_scan() {
        let dir = temp_dir();
        let log_path = dir.path().join("9.log");
        let sequence_path = dir.path().join("9.seq");
        std::fs::write(&log_path, b"zero\none\ntwo\n").unwrap();
        std::fs::write(&sequence_path, b"FTSEQ1:2\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600)).unwrap();
            std::fs::set_permissions(&sequence_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }

        let snapshot = read_pane_snapshot(dir.path(), 9, 3, 1024, 1024).unwrap();
        assert_eq!(snapshot.oldest_seq, Some(2));
        assert_eq!(snapshot.next_seq, 3);
        assert_eq!(snapshot.records, vec!["two"]);
        assert_eq!(snapshot.retained_record_bytes, 4);
        assert_eq!(snapshot.committed_bytes, 13);
        assert_eq!(snapshot.sequence_bytes, 9);

        let error = read_pane_snapshot(dir.path(), 9, 2, 1024, 1024)
            .expect_err("physical record scan limit must fail closed before allocation grows");
        assert!(matches!(
            error,
            MmapStoreError::PaneSnapshotLimitExceeded {
                limit_name: "records",
                limit: 2,
                observed: 3,
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn read_pane_snapshot_rejects_non_private_log_authority() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir();
        let log_path = dir.path().join("11.log");
        std::fs::write(&log_path, b"exposed\n").unwrap();
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let error = read_pane_snapshot(dir.path(), 11, 8, 1024, 1024)
            .expect_err("read-only export must refuse a non-private log");
        assert!(error.to_string().contains("snapshot log is not private"));
    }

    // --- Hybrid: fallback_panes behavior ---

    #[test]
    fn hybrid_store_line_count_zero_for_unknown() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");
        let store = hybrid_store(dir.path(), &db_path);

        assert_eq!(store.line_count(999), 0);
    }

    #[test]
    fn hybrid_store_line_count_from_sqlite() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");

        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS mmap_scrollback_lines (
                     pane_id INTEGER NOT NULL,
                     seq INTEGER NOT NULL,
                     content TEXT NOT NULL,
                     PRIMARY KEY (pane_id, seq)
                 );
                 INSERT INTO mmap_scrollback_lines VALUES (3, 0, 'a');
                 INSERT INTO mmap_scrollback_lines VALUES (3, 1, 'b');",
            )
            .unwrap();
        }

        let store = hybrid_store(dir.path(), &db_path);
        // Pane 3 only in SQLite - line_count should find it
        assert_eq!(store.line_count(3), 2);
    }

    // --- PaneStorageMode ---

    #[test]
    fn pane_storage_mode_debug() {
        assert_eq!(format!("{:?}", PaneStorageMode::Mmap), "Mmap");
        assert_eq!(
            format!("{:?}", PaneStorageMode::SqliteFallback),
            "SqliteFallback"
        );
    }

    #[test]
    fn pane_storage_mode_eq() {
        assert_eq!(PaneStorageMode::Mmap, PaneStorageMode::Mmap);
        assert_ne!(PaneStorageMode::Mmap, PaneStorageMode::SqliteFallback);
    }
}
