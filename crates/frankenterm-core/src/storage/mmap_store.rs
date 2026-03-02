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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("create temp dir")
    }

    fn file_only_store(dir: &Path) -> MmapScrollbackStore {
        let config = MmapStoreConfig::new(dir.to_path_buf());
        MmapScrollbackStore::new(config).expect("create store")
    }

    fn hybrid_store(dir: &Path, db_path: &Path) -> MmapScrollbackStore {
        let config = MmapStoreConfig::new(dir.to_path_buf())
            .with_sqlite_fallback(db_path.to_path_buf());
        MmapScrollbackStore::new(config).expect("create hybrid store")
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
        assert_eq!(
            offsets,
            vec![LineOffset(0), LineOffset(5), LineOffset(15)]
        );
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
        assert_eq!(
            format!("{err}"),
            "offset 100 exceeds file length 50"
        );
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
        assert_eq!(
            offsets,
            vec![LineOffset(0), LineOffset(3), LineOffset(7)]
        );
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
