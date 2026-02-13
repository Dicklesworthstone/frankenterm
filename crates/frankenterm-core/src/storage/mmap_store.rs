//! Experimental mmap-backed scrollback store scaffold for `wa-8vla`.
//!
//! This module is intentionally isolated from production wiring in this slice.
//! It provides a concrete API and index invariants that later slices can
//! integrate into `storage.rs` behind a feature/config gate.
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs::{File, OpenOptions, create_dir_all};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Pane identifier.
pub type PaneId = u64;

/// Byte offset for a line start in the pane log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LineOffset(pub u64);

/// Configuration for the scrollback store.
#[derive(Debug, Clone)]
pub struct MmapStoreConfig {
    pub base_dir: PathBuf,
}

impl MmapStoreConfig {
    #[must_use]
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }
}

/// Error type for the scaffold store.
#[derive(Debug, thiserror::Error)]
pub enum MmapStoreError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unknown pane: {0}")]
    UnknownPane(PaneId),
}

/// In-memory per-pane index and file handle.
#[derive(Debug)]
struct PaneFile {
    log_path: PathBuf,
    file: File,
    line_offsets: Vec<LineOffset>,
}

impl PaneFile {
    fn open(base_dir: &Path, pane_id: PaneId) -> Result<Self, MmapStoreError> {
        let log_path = base_dir.join(format!("{pane_id}.log"));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&log_path)?;

        Ok(Self {
            log_path,
            file,
            line_offsets: Vec::new(),
        })
    }

    fn append_line(&mut self, line: &str) -> Result<(), MmapStoreError> {
        let start = self.file.seek(SeekFrom::End(0))?;
        self.line_offsets.push(LineOffset(start));
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        Ok(())
    }

    fn tail_lines(&self, n: usize) -> Result<Vec<String>, MmapStoreError> {
        if n == 0 {
            return Ok(Vec::new());
        }

        let mut reader = BufReader::new(File::open(&self.log_path)?);
        let mut lines = Vec::new();
        let mut buf = String::new();

        while reader.read_line(&mut buf)? != 0 {
            if buf.ends_with('\n') {
                buf.pop();
                if buf.ends_with('\r') {
                    buf.pop();
                }
            }
            lines.push(buf.clone());
            buf.clear();
        }

        let keep = lines.len().saturating_sub(n);
        Ok(lines.split_off(keep))
    }
}

/// Pane-scoped append/read store.
#[derive(Debug)]
pub struct MmapScrollbackStore {
    base_dir: PathBuf,
    panes: HashMap<PaneId, PaneFile>,
}

impl MmapScrollbackStore {
    pub fn new(config: MmapStoreConfig) -> Result<Self, MmapStoreError> {
        create_dir_all(&config.base_dir)?;
        Ok(Self {
            base_dir: config.base_dir,
            panes: HashMap::new(),
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

    pub fn append_line(&mut self, pane_id: PaneId, line: &str) -> Result<(), MmapStoreError> {
        self.pane_mut(pane_id)?.append_line(line)
    }

    pub fn tail_lines(&self, pane_id: PaneId, n: usize) -> Result<Vec<String>, MmapStoreError> {
        let pane = self
            .panes
            .get(&pane_id)
            .ok_or(MmapStoreError::UnknownPane(pane_id))?;
        pane.tail_lines(n)
    }

    #[must_use]
    pub fn line_count(&self, pane_id: PaneId) -> usize {
        self.panes.get(&pane_id).map_or(0, |p| p.line_offsets.len())
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
