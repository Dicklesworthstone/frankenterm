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
//! (ft-odrq7). Appends themselves are still buffered (no per-line fsync), so a
//! crash can drop the most recent unsynced lines; durable append is tracked
//! separately under ft-2okh0.5.1.

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
}

const COLD_ERASURE_DATA_SHARDS: usize = 3;
const COLD_ERASURE_TOTAL_SHARDS: usize = 5;
const COLD_ERASURE_VERSION: u8 = 1;
const COLD_ERASURE_MAGIC: [u8; 8] = *b"FTRSLOG1";
const COLD_ERASURE_HEADER_LEN: usize = 8 + 1 + 1 + 1 + 1 + 8 + 4 + 4 + 4;
const GF_PRIM: u32 = 0x11d;

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
    let padded_len = chunk_size.saturating_mul(COLD_ERASURE_DATA_SHARDS);
    let mut padded = Vec::with_capacity(padded_len);
    padded.extend_from_slice(data);
    padded.resize(padded_len, 0);
    let payload_crc32 = crc32_ieee(data);

    let mut shards = Vec::with_capacity(COLD_ERASURE_TOTAL_SHARDS);
    for index in 0..COLD_ERASURE_TOTAL_SHARDS {
        let row = cold_erasure_generator_row(
            u8::try_from(index).map_err(|_| MmapStoreError::NumericOverflow("shard_index"))?,
        )?;
        let mut bytes = vec![0u8; chunk_size];
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
    let mut padded = vec![0u8; shard_len.saturating_mul(COLD_ERASURE_DATA_SHARDS)];
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
    let decoded = padded[..original_len].to_vec();
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
        let mut encoded = Vec::with_capacity(COLD_ERASURE_HEADER_LEN + self.bytes.len());
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
        let bytes = encoded[32..].to_vec();
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
    file_len: u64,
    base_seq: u64,
    line_offsets: Vec<LineOffset>,
}

impl PaneFile {
    fn scan_offsets(path: &Path) -> Result<(Vec<LineOffset>, u64), MmapStoreError> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut line_offsets = Vec::new();
        let mut cursor = 0u64;
        let mut line_buf = Vec::new();

        loop {
            let bytes_read = reader.read_until(b'\n', &mut line_buf)?;
            if bytes_read == 0 {
                break;
            }
            line_offsets.push(LineOffset(cursor));
            cursor = cursor.saturating_add(u64::try_from(bytes_read).unwrap_or(u64::MAX));
            line_buf.clear();
        }

        Ok((line_offsets, cursor))
    }

    fn open(base_dir: &Path, pane_id: PaneId) -> Result<Self, MmapStoreError> {
        let log_path = base_dir.join(format!("{pane_id}.log"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&log_path)?;
        let (line_offsets, file_len) = Self::scan_offsets(&log_path)?;

        Ok(Self {
            log_path,
            file,
            file_len,
            base_seq: 0,
            line_offsets,
        })
    }

    fn append_line(&mut self, line: &str) -> Result<u64, MmapStoreError> {
        let start = self.file.seek(SeekFrom::End(0))?;
        let seq = self
            .base_seq
            .saturating_add(u64::try_from(self.line_offsets.len()).unwrap_or(u64::MAX));
        self.line_offsets.push(LineOffset(start));
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file_len = start
            .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
            .saturating_add(1);
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
        let start_offset = self.line_offsets[start_index].0;
        if start_offset > self.file_len {
            return Err(MmapStoreError::OffsetOutOfBounds {
                offset: start_offset,
                len: self.file_len,
            });
        }
        let actual_len = std::fs::metadata(&self.log_path)?.len();
        if start_offset > actual_len {
            return Err(MmapStoreError::OffsetOutOfBounds {
                offset: start_offset,
                len: actual_len,
            });
        }

        let mut tail_file = File::open(&self.log_path)?;
        tail_file.seek(SeekFrom::Start(start_offset))?;
        let mut tail_bytes = Vec::new();
        tail_file.read_to_end(&mut tail_bytes)?;

        let mut lines: Vec<String> = tail_bytes
            .split(|byte| *byte == b'\n')
            .map(|line_bytes| {
                let line_bytes = line_bytes.strip_suffix(b"\r").unwrap_or(line_bytes);
                String::from_utf8_lossy(line_bytes).to_string()
            })
            .collect();

        // Drop split()'s trailing empty segment when input ends with '\n'.
        if tail_bytes.ends_with(b"\n") {
            let _ = lines.pop();
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

        let end = self
            .line_offsets
            .get(index + 1)
            .map(|offset| offset.0)
            .unwrap_or(self.file_len);
        let len = end.saturating_sub(start.0);
        let mut file = File::open(&self.log_path)?;
        file.seek(SeekFrom::Start(start.0))?;
        let len = usize::try_from(len).map_err(|_| MmapStoreError::NumericOverflow("line_len"))?;
        let mut bytes = vec![0u8; len];
        file.read_exact(&mut bytes)?;
        while matches!(bytes.last(), Some(b'\n' | b'\r')) {
            bytes.pop();
        }
        Ok(Some(String::from_utf8_lossy(&bytes).to_string()))
    }

    fn prune_before(&mut self, seq: u64) {
        if seq <= self.base_seq {
            return;
        }
        let drop_count = usize::try_from(seq - self.base_seq)
            .unwrap_or(usize::MAX)
            .min(self.line_offsets.len());
        self.line_offsets.drain(0..drop_count);
        self.base_seq = self
            .base_seq
            .saturating_add(u64::try_from(drop_count).unwrap_or(u64::MAX));
    }

    fn clear(&mut self) -> Result<(), MmapStoreError> {
        self.file.set_len(0)?;
        self.file.flush()?;
        self.file_len = 0;
        self.base_seq = 0;
        self.line_offsets.clear();
        Ok(())
    }

    fn read_all_bytes(&self) -> Result<Vec<u8>, MmapStoreError> {
        let mut file = File::open(&self.log_path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    fn write_erasure_sidecars(&self) -> Result<(), MmapStoreError> {
        let bytes = self.read_all_bytes()?;
        let shards = cold_erasure_encode(&bytes)?;
        for shard in shards {
            let path = cold_erasure_shard_path(&self.log_path, shard.index)?;
            let mut tmp_path = path.clone();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    MmapStoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "erasure shard path has no file name",
                    ))
                })?;
            tmp_path.set_file_name(format!("{name}.tmp"));
            {
                let mut file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp_path)?;
                file.write_all(&shard.encode_bytes()?)?;
                file.sync_all()?;
            }
            std::fs::rename(&tmp_path, &path)?;
        }
        if let Some(dir) = self.log_path.parent() {
            if let Ok(dir_handle) = File::open(dir) {
                let _ = dir_handle.sync_all();
            }
        }
        Ok(())
    }

    fn recover_from_erasure_sidecars(&self) -> Result<Vec<u8>, MmapStoreError> {
        let mut shards = Vec::new();
        for index in 0..COLD_ERASURE_TOTAL_SHARDS {
            let index =
                u8::try_from(index).map_err(|_| MmapStoreError::NumericOverflow("shard_index"))?;
            let path = cold_erasure_shard_path(&self.log_path, index)?;
            match std::fs::read(&path) {
                Ok(bytes) => shards.push(ColdErasureShard::decode_bytes(&bytes)?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        cold_erasure_decode(&shards)
    }

    fn stale_prefix_bytes(&self) -> u64 {
        self.line_offsets
            .first()
            .map(|offset| offset.0)
            .unwrap_or(self.file_len)
    }

    fn compact_retained_prefix(&mut self) -> Result<bool, MmapStoreError> {
        let stale_bytes = self.stale_prefix_bytes();
        if stale_bytes == 0 {
            return Ok(false);
        }
        if self.line_offsets.is_empty() {
            self.clear()?;
            return Ok(true);
        }

        let retained_len = self.file_len.saturating_sub(stale_bytes);
        let retained_len = usize::try_from(retained_len)
            .map_err(|_| MmapStoreError::NumericOverflow("file_len"))?;
        let mut retained = Vec::with_capacity(retained_len);
        let mut source = File::open(&self.log_path)?;
        source.seek(SeekFrom::Start(stale_bytes))?;
        source.read_to_end(&mut retained)?;

        // Crash-safe compaction (ft-odrq7): write the retained suffix to a
        // sibling temp file, fsync it, then atomically rename it over the live
        // log. The previous implementation reopened the live log with
        // `truncate(true)` — zeroing it in place — and wrote the suffix back
        // with only a buffered `flush()` (no fsync). A crash between the
        // truncate and a durable write lost the ENTIRE retained scrollback for
        // the pane, not just the stale prefix it was supposed to drop. With
        // temp + fsync + rename, the live log is at every instant either the
        // old (full) file or the new (compacted) file — never truncated.
        let tmp_path = {
            let mut path = self.log_path.clone();
            let name = self
                .log_path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    MmapStoreError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "scrollback log path has no file name",
                    ))
                })?;
            path.set_file_name(format!("{name}.compact.tmp"));
            path
        };
        {
            // `truncate(true)` here only clears any stale temp from a previously
            // interrupted compaction; the live log is untouched until the rename.
            let mut compacted = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp_path)?;
            compacted.write_all(&retained)?;
            // Persist the retained bytes before the rename so the swap can never
            // expose a partially written compacted log.
            compacted.sync_all()?;
        }
        std::fs::rename(&tmp_path, &self.log_path)?;
        // Make the rename itself durable: without a directory fsync a crash
        // after the rename but before the directory entry reaches disk could
        // resurrect the pre-compaction file. Best-effort and portable — opening
        // a directory as a `File` is not supported on every platform.
        if let Some(dir) = self.log_path.parent() {
            if let Ok(dir_handle) = File::open(dir) {
                let _ = dir_handle.sync_all();
            }
        }

        self.file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.log_path)?;
        self.file_len = u64::try_from(retained.len())
            .map_err(|_| MmapStoreError::NumericOverflow("file_len"))?;
        for offset in &mut self.line_offsets {
            offset.0 = offset.0.saturating_sub(stale_bytes);
        }
        Ok(true)
    }

    fn retained_bytes(&self) -> u64 {
        let Some(first) = self.line_offsets.first() else {
            return 0;
        };
        self.file_len.saturating_sub(first.0)
    }

    fn oldest_seq(&self) -> Option<u64> {
        (!self.line_offsets.is_empty()).then_some(self.base_seq)
    }

    fn file_bytes(&self) -> u64 {
        self.file_len
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
             PRAGMA synchronous=NORMAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS mmap_scrollback_lines (
                 pane_id INTEGER NOT NULL,
                 seq INTEGER NOT NULL,
                 content TEXT NOT NULL,
                 PRIMARY KEY (pane_id, seq)
             );
             CREATE INDEX IF NOT EXISTS idx_mmap_scrollback_lines_pane_seq
                 ON mmap_scrollback_lines(pane_id, seq DESC);",
        )?;

        Ok(Self { conn })
    }

    fn append_line_with_seq(
        &self,
        pane_id: PaneId,
        seq: u64,
        line: &str,
    ) -> Result<(), MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let seq_i64 = i64::try_from(seq).map_err(|_| MmapStoreError::NumericOverflow("seq"))?;

        self.conn.execute(
            "INSERT OR REPLACE INTO mmap_scrollback_lines (pane_id, seq, content)
             VALUES (?1, ?2, ?3)",
            params![pane_id_i64, seq_i64, line],
        )?;

        Ok(())
    }

    fn append_line_auto_seq(&self, pane_id: PaneId, line: &str) -> Result<(), MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        let next_seq_i64: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq) + 1, 0)
             FROM mmap_scrollback_lines
             WHERE pane_id = ?1",
            [pane_id_i64],
            |row| row.get(0),
        )?;
        let next_seq =
            u64::try_from(next_seq_i64).map_err(|_| MmapStoreError::NumericOverflow("seq"))?;
        self.append_line_with_seq(pane_id, next_seq, line)
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

    fn clear_pane(&self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        let pane_id_i64 =
            i64::try_from(pane_id).map_err(|_| MmapStoreError::NumericOverflow("pane_id"))?;
        self.conn.execute(
            "DELETE FROM mmap_scrollback_lines WHERE pane_id = ?1",
            [pane_id_i64],
        )?;
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
    ) -> Result<(), MmapStoreError> {
        let sqlite = self
            .sqlite_fallback
            .as_mut()
            .ok_or(MmapStoreError::UnknownPane(pane_id))?;
        sqlite.append_line_auto_seq(pane_id, line)
    }

    fn tail_lines_sqlite(&self, pane_id: PaneId, n: usize) -> Result<Vec<String>, MmapStoreError> {
        let sqlite = self
            .sqlite_fallback
            .as_ref()
            .ok_or(MmapStoreError::UnknownPane(pane_id))?;
        let lines = sqlite.tail_lines(pane_id, n)?;
        if lines.is_empty() && sqlite.line_count(pane_id)? == 0 {
            return Err(MmapStoreError::UnknownPane(pane_id));
        }
        Ok(lines)
    }

    pub fn ensure_pane(&mut self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) {
            return Ok(());
        }

        match self.pane_mut(pane_id) {
            Ok(_pane) => Ok(()),
            Err(err) => {
                if self.sqlite_fallback.is_some() {
                    self.fallback_panes.insert(pane_id);
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    pub fn append_line(&mut self, pane_id: PaneId, line: &str) -> Result<(), MmapStoreError> {
        if self.fallback_panes.contains(&pane_id) {
            return self.append_line_sqlite_only(pane_id, line);
        }

        let append_result: Result<u64, MmapStoreError> = (|| {
            let pane = self.pane_mut(pane_id)?;
            pane.append_line(line)
        })();

        match append_result {
            Ok(seq) => {
                if let Some(sqlite) = self.sqlite_fallback.as_mut() {
                    sqlite.append_line_with_seq(pane_id, seq, line)?;
                }
                Ok(())
            }
            Err(err) => {
                if self.sqlite_fallback.is_some() {
                    self.fallback_panes.insert(pane_id);
                    self.append_line_sqlite_only(pane_id, line)
                } else {
                    Err(err)
                }
            }
        }
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
        if self.fallback_panes.contains(&pane_id) {
            return self
                .sqlite_fallback
                .as_ref()
                .ok_or(MmapStoreError::UnknownPane(pane_id))?
                .prune_before(pane_id, seq);
        }

        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.prune_before(seq);
        }
        if let Some(sqlite) = self.sqlite_fallback.as_ref() {
            sqlite.prune_before(pane_id, seq)?;
        }
        Ok(())
    }

    pub fn clear_pane(&mut self, pane_id: PaneId) -> Result<(), MmapStoreError> {
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.clear()?;
        }
        if let Some(sqlite) = self.sqlite_fallback.as_ref() {
            sqlite.clear_pane(pane_id)?;
        }
        if self.cold_erasure == ColdErasureMode::ReedSolomon {
            if let Some(pane) = self.panes.get(&pane_id) {
                pane.write_erasure_sidecars()?;
            }
        }
        self.fallback_panes.remove(&pane_id);
        Ok(())
    }

    pub fn refresh_pane_erasure_shards(&mut self, pane_id: PaneId) -> Result<bool, MmapStoreError> {
        if self.cold_erasure != ColdErasureMode::ReedSolomon
            || self.fallback_panes.contains(&pane_id)
        {
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
        if self.cold_erasure != ColdErasureMode::ReedSolomon
            || self.fallback_panes.contains(&pane_id)
        {
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
                .line_count(pane_id)
                .ok()
                .and_then(|count| (count > 0).then_some(PaneStorageMode::SqliteFallback))
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
        assert!(lines.is_empty());
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

        let tmp = dir.path().join("1.log.compact.tmp");
        assert!(
            !tmp.exists(),
            "compaction temp must be renamed away, not leaked"
        );

        // Reading the raw file proves the retained bytes are the WHOLE file —
        // i.e. produced by the rename, not re-grown from a zeroed-in-place log.
        let raw = std::fs::read_to_string(dir.path().join("1.log")).unwrap();
        assert_eq!(raw, "line-6\nline-7\n");
        assert_eq!(store.tail_lines(1, 10).unwrap(), vec!["line-6", "line-7"]);
        assert_eq!(store.oldest_seq(1), Some(6));
    }

    #[test]
    fn compaction_recovers_from_leftover_temp_of_interrupted_run() {
        // A crash during a prior compaction can leave a stale
        // `<pane>.log.compact.tmp`. The next compaction must overwrite
        // (truncate) it, not append to or trip over it, so the retained data
        // stays correct (ft-odrq7).
        let dir = temp_dir();
        let mut store = file_only_store(dir.path());

        for idx in 0..8 {
            store.append_line(1, &format!("row-{idx}")).unwrap();
        }
        store.prune_before(1, 6).unwrap();

        // Simulate the crash-leftover temp of an interrupted compaction.
        std::fs::write(
            dir.path().join("1.log.compact.tmp"),
            b"GARBAGE-FROM-INTERRUPTED-COMPACTION\n",
        )
        .unwrap();

        assert!(store.compact_pane_if_stale(1, 1).unwrap());

        let raw = std::fs::read_to_string(dir.path().join("1.log")).unwrap();
        assert_eq!(
            raw, "row-6\nrow-7\n",
            "leftover temp must not corrupt the compacted log"
        );
        assert!(!raw.contains("GARBAGE"));
        assert_eq!(store.tail_lines(1, 10).unwrap(), vec!["row-6", "row-7"]);
        assert!(
            !dir.path().join("1.log.compact.tmp").exists(),
            "temp consumed by rename"
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
        assert_eq!(raw, "line-5\nline-6\nline-7\n");
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
    fn hybrid_store_appends_to_both() {
        let dir = temp_dir();
        let db_path = dir.path().join("fallback.db");
        let mut store = hybrid_store(dir.path(), &db_path);

        store.append_line(1, "hello").unwrap();
        store.append_line(1, "world").unwrap();

        let lines = store.tail_lines(1, 10).unwrap();
        assert_eq!(lines, vec!["hello", "world"]);
        assert_eq!(store.line_count(1), 2);
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

    // --- SQLite-only fallback store ---

    #[test]
    fn sqlite_fallback_store_basic() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");
        let sqlite = SqliteFallbackStore::open(&db_path).unwrap();

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
        let sqlite = SqliteFallbackStore::open(&db_path).unwrap();

        sqlite.append_line_auto_seq(1, "data").unwrap();

        let lines = sqlite.tail_lines(1, 0).unwrap();
        assert!(lines.is_empty());
    }

    #[test]
    fn sqlite_fallback_store_multiple_panes() {
        let dir = temp_dir();
        let db_path = dir.path().join("test.db");
        let sqlite = SqliteFallbackStore::open(&db_path).unwrap();

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
        let sqlite = SqliteFallbackStore::open(&db_path).unwrap();

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
        let sqlite = SqliteFallbackStore::open(&db_path).unwrap();

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
