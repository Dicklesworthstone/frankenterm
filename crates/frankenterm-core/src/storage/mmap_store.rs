//! Experimental scrollback store for `ft-8vla`.
//!
//! This module keeps an append-only log per pane plus a byte-offset line index.
//! The index allows tail reads to seek directly to the relevant byte window.
//! A later slice can swap the tail read path to true mmap once we expose a
//! safe mapping wrapper that fits this crate's `unsafe_code = forbid` policy.

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

/// Configuration for the scrollback store.
#[derive(Debug, Clone)]
pub struct MmapStoreConfig {
    pub base_dir: PathBuf,
    pub sqlite_fallback_path: Option<PathBuf>,
}

impl MmapStoreConfig {
    #[must_use]
    pub fn new(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            sqlite_fallback_path: None,
        }
    }

    #[must_use]
    pub fn with_sqlite_fallback(mut self, sqlite_fallback_path: PathBuf) -> Self {
        self.sqlite_fallback_path = Some(sqlite_fallback_path);
        self
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
}

/// In-memory per-pane index and file handle.
#[derive(Debug)]
struct PaneFile {
    log_path: PathBuf,
    file: File,
    file_len: u64,
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
            line_offsets,
        })
    }

    fn append_line(&mut self, line: &str) -> Result<(), MmapStoreError> {
        let start = self.file.seek(SeekFrom::End(0))?;
        self.line_offsets.push(LineOffset(start));
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.file_len = start
            .saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX))
            .saturating_add(1);
        Ok(())
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
}

#[derive(Debug)]
struct SqliteFallbackStore {
    conn: Connection,
}

impl SqliteFallbackStore {
    fn open(path: &Path) -> Result<Self, MmapStoreError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
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
}

/// Pane-scoped append/read store.
#[derive(Debug)]
pub struct MmapScrollbackStore {
    base_dir: PathBuf,
    panes: HashMap<PaneId, PaneFile>,
    sqlite_fallback: Option<SqliteFallbackStore>,
    fallback_panes: HashSet<PaneId>,
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

        let mut next_seq = None;
        let append_result: Result<(), MmapStoreError> = (|| {
            let pane = self.pane_mut(pane_id)?;
            next_seq = Some(
                u64::try_from(pane.line_offsets.len())
                    .map_err(|_| MmapStoreError::NumericOverflow("line_count"))?,
            );
            pane.append_line(line)
        })();

        match append_result {
            Ok(()) => {
                if let (Some(sqlite), Some(seq)) = (self.sqlite_fallback.as_mut(), next_seq) {
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
