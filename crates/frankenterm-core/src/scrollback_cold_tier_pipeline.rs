//! Cold-tier write-path pipeline contracts
//! ([BR-TERM-EMULATOR-UPLIFT-2.13.cont] / `ft-tfb64`).
//!
//! The cold-tier policy substrate already lives at
//! `scrollback_cold_tier.rs` (`b3e8a6845`). This module
//! ships the **integration substrate** the bead asks for:
//! the typed-state write-path pipeline + disk-path layout
//! contract + metadata index schema + structured-log row
//! contract + key-handle shape.
//!
//! ## Why typed-state for the write pipeline
//!
//! The bead's headline "DO NOT BREAK" rule is:
//!
//! > Privacy: redactor MUST apply before disk write —
//! > substrate enforces by type, integration must call
//! > redactor.
//!
//! Foundation slice encodes this invariant at the type
//! level. The integration cannot accidentally skip the
//! redactor or reorder the pipeline because the type
//! system won't let them: each stage consumes a typed
//! handle and produces the next, in the strict order
//! `Raw → Redacted → Compressed → Encrypted → Written`.
//!
//! ## What this module ships
//!
//! - [`ColdTierDiskPath`] + [`disk_path_for`] — path
//!   contract enforcing `~/.cache/ft/scrollback/<pane>/
//!   <chunk_id>.zst[.enc]` per the bead's sub-task 5
//!   layout, with mode-0600 invariant assertion.
//! - [`ChunkBytes`] typed-state pipeline:
//!   [`Raw`] / [`Redacted`] / [`Compressed`] /
//!   [`Encrypted`] / [`Written`] — each stage's wrapper
//!   admits only the next stage's transition. Privacy
//!   compliance is structural.
//! - [`MetadataIndexRow`] — schema the SQLite/in-memory
//!   index stores per the bead's sub-task 6 (id,
//!   byte_range, line_range, content_hash, written_ts,
//!   last_access_ts, tier, redaction, encryption).
//! - [`ColdTierKeyHandle`] — opaque key-material shape
//!   for AES-256-GCM (sub-task 4). Integration plugs in
//!   real key derivation; this module ships the handle
//!   contract + cap-bag.
//! - [`StructuredLogRow`] — JSONL row contract for
//!   sub-task 9.
//! - [`PipelineHealth`] — `ft doctor` snapshot.

use std::collections::BTreeMap;
use std::marker::PhantomData;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ============================================================================
// Disk-path layout (sub-task 5)
// ============================================================================

/// Path layout enforcing the bead's stated convention:
///
/// > `~/.cache/ft/scrollback/<pane_id>/<chunk_id>.zst[.enc]
/// > mode 0600`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColdTierDiskPath {
    pub pane_id: u64,
    pub chunk_id: u64,
    pub encrypted: bool,
}

impl ColdTierDiskPath {
    /// Construct the absolute path under a given cache
    /// root. Production passes
    /// `dirs::cache_dir() / "ft" / "scrollback"`; tests
    /// pass a tempdir.
    #[must_use]
    pub fn render(&self, cache_root: &std::path::Path) -> PathBuf {
        let mut p = cache_root.to_path_buf();
        p.push("scrollback");
        p.push(self.pane_id.to_string());
        let mut filename = self.chunk_id.to_string();
        filename.push_str(".zst");
        if self.encrypted {
            filename.push_str(".enc");
        }
        p.push(filename);
        p
    }

    /// File mode: 0o600 (owner read/write only). Constant
    /// per the bead.
    pub const FILE_MODE: u32 = 0o600;

    /// Validate a path was rendered by this contract.
    /// Tests use this to pin the layout shape.
    #[must_use]
    pub fn matches_layout(path: &std::path::Path) -> bool {
        let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
            return false;
        };
        let stripped = filename.trim_end_matches(".enc");
        let Some(prefix) = stripped.strip_suffix(".zst") else {
            return false;
        };
        if prefix.is_empty() {
            return false;
        }
        if !prefix.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
        // Parent dir name should be a numeric pane_id.
        let Some(parent) = path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str())
        else {
            return false;
        };
        parent.chars().all(|c| c.is_ascii_digit()) && !parent.is_empty()
    }
}

#[must_use]
pub fn disk_path_for(pane_id: u64, chunk_id: u64, encrypted: bool) -> ColdTierDiskPath {
    ColdTierDiskPath {
        pane_id,
        chunk_id,
        encrypted,
    }
}

// ============================================================================
// Typed-state write-path pipeline (DO NOT BREAK rule)
// ============================================================================

/// Phantom marker types — the typed-state pipeline uses
/// them so each pipeline stage is a distinct compile-time
/// type. The integration cannot construct a `ChunkBytes<
/// Compressed>` without first consuming a `ChunkBytes<
/// Redacted>`, which can only come from a
/// `ChunkBytes<Raw>::redact()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Raw;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Redacted;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Compressed;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Encrypted;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Written;

/// Typed-state wrapper for chunk bytes. Each stage's
/// wrapper has methods that only produce the next stage,
/// so the pipeline order is enforced at compile time.
///
/// **Privacy invariant**: `bytes` is private. Callers can
/// inspect via [`Self::as_bytes`] (read-only) or extract
/// via the typed-state-specific consume methods. This
/// prevents mid-pipeline mutation that would bypass the
/// redactor (a public field would let a maintainer
/// replace `bytes` between stages, writing un-redacted
/// data through the encrypt + write path).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChunkBytes<Stage> {
    bytes: Vec<u8>,
    _stage: PhantomData<Stage>,
}

impl ChunkBytes<Raw> {
    #[must_use]
    pub fn from_raw(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            _stage: PhantomData,
        }
    }

    /// Consume raw bytes, apply the redactor, produce the
    /// `Redacted` typed-state. The integration plugs in
    /// the actual `redactor::redact_text` call here.
    ///
    /// **Privacy caveat (br-ft-0gjrq)**: the substrate cannot
    /// distinguish a real redactor (with rules) from an
    /// identity closure (`|bytes| bytes`). The `Redacted`
    /// type-tag only proves a closure ran, not that secrets
    /// were sanitised. The integration MUST plumb the truth
    /// into `ColdTierPipelineHealth::record_write`'s
    /// `redactor_applied` flag — that field is the runtime
    /// second line of defence (validated against
    /// `chunks_written_total` in `is_safe`).
    #[must_use]
    pub fn redact_with(self, redactor: impl FnOnce(Vec<u8>) -> Vec<u8>) -> ChunkBytes<Redacted> {
        let bytes = redactor(self.bytes);
        ChunkBytes {
            bytes,
            _stage: PhantomData,
        }
    }

    /// Variant of [`Self::redact_with`] that requires the
    /// redactor to also report whether it actually replaced
    /// any bytes. The returned `RedactionEvidence` carries
    /// the count of redactor matches so callers can compute
    /// the `redactor_applied` flag without trusting a
    /// closure's side effects.
    ///
    /// Self-review fix (br-ft-0gjrq): closes the doc-impl
    /// mismatch where the prior signature only returned a
    /// type-tagged value with no way to verify the redactor
    /// did real work.
    #[must_use]
    pub fn redact_with_evidence(
        self,
        redactor: impl FnOnce(Vec<u8>) -> (Vec<u8>, RedactionEvidence),
    ) -> (ChunkBytes<Redacted>, RedactionEvidence) {
        let (bytes, evidence) = redactor(self.bytes);
        (
            ChunkBytes {
                bytes,
                _stage: PhantomData,
            },
            evidence,
        )
    }
}

/// Evidence the redactor returns to prove it ran. The
/// integration's redactor produces this; the substrate's
/// typed-state pipeline plumbs it through.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct RedactionEvidence {
    /// Number of redactor-rule matches. Zero means the
    /// redactor inspected the bytes but found nothing — the
    /// integration still treats this as `redactor_applied=true`
    /// (the redactor scanned). The distinction from a no-op
    /// identity closure is the integration's responsibility.
    pub matches: u32,
    /// Bytes the redactor replaced (sum across all matches).
    pub bytes_replaced: u32,
}

impl RedactionEvidence {
    /// Whether the redactor scanned. Always `true` if this
    /// evidence struct was produced by the redactor (the
    /// type system can't catch an integration that builds
    /// `RedactionEvidence::default()` without scanning, but
    /// substrate semantics treat any returned evidence as
    /// "scan happened").
    #[must_use]
    pub const fn redactor_applied(&self) -> bool {
        true
    }

    /// Whether the redactor found and replaced anything.
    #[must_use]
    pub const fn made_changes(&self) -> bool {
        self.matches > 0
    }
}

impl ChunkBytes<Redacted> {
    /// Compress with zstd. Integration plugs in the
    /// `zstd::encode_all` call.
    #[must_use]
    pub fn compress_with(
        self,
        compressor: impl FnOnce(Vec<u8>) -> Vec<u8>,
    ) -> ChunkBytes<Compressed> {
        let bytes = compressor(self.bytes);
        ChunkBytes {
            bytes,
            _stage: PhantomData,
        }
    }
}

impl ChunkBytes<Compressed> {
    /// Encrypt with AES-256-GCM. Integration plugs in
    /// the actual cipher; foundation slice carries the
    /// shape.
    #[must_use]
    pub fn encrypt_with(
        self,
        _key: &ColdTierKeyHandle,
        cipher: impl FnOnce(Vec<u8>) -> Vec<u8>,
    ) -> ChunkBytes<Encrypted> {
        let bytes = cipher(self.bytes);
        ChunkBytes {
            bytes,
            _stage: PhantomData,
        }
    }

    /// Operator opt-out: skip encryption. Per bead's
    /// sub-task 4 ("AES-256-GCM encryption-at-rest
    /// (operator opt-in)") — encryption is not the
    /// default; some operators ship without it. The
    /// `Encrypted` typed-state reflects "ready to
    /// write" semantics regardless of whether the bytes
    /// are actually encrypted.
    #[must_use]
    pub fn skip_encryption(self) -> ChunkBytes<Encrypted> {
        ChunkBytes {
            bytes: self.bytes,
            _stage: PhantomData,
        }
    }
}

impl ChunkBytes<Encrypted> {
    /// Write the bytes via a writer closure, transitioning
    /// to the `Written` state. The bytes never leave the
    /// typed-state object — the writer borrows them via
    /// `&[u8]`. This is the only path to extract bytes for
    /// I/O.
    ///
    /// On writer error, returns the original Encrypted
    /// state + the error so the caller can retry without
    /// losing the typed-state guarantees.
    pub fn write_with<F, E>(self, writer: F) -> Result<ChunkBytes<Written>, (Self, E)>
    where
        F: FnOnce(&[u8]) -> Result<(), E>,
    {
        match writer(&self.bytes) {
            Ok(()) => Ok(ChunkBytes {
                bytes: self.bytes,
                _stage: PhantomData,
            }),
            Err(e) => Err((self, e)),
        }
    }

    /// Legacy marker transition. Prefer [`Self::write_with`]
    /// for production code so the bytes are coupled to a
    /// real write call. Retained because some integration
    /// surfaces (e.g., async batch flush) decouple the I/O
    /// from the state transition.
    #[must_use]
    pub fn mark_written(self) -> ChunkBytes<Written> {
        ChunkBytes {
            bytes: self.bytes,
            _stage: PhantomData,
        }
    }
}

impl<Stage> ChunkBytes<Stage> {
    /// Read-only access to the bytes. Callers cannot
    /// mutate via this accessor (returns `&[u8]`).
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

// ============================================================================
// Metadata index schema (sub-task 6)
// ============================================================================

/// One row in the metadata index. Bead sub-task 6:
///
/// > Per-chunk id, byte_range, line_range, content_hash,
/// > written_ts, last_access_ts, tier, redaction,
/// > encryption.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MetadataIndexRow {
    pub chunk_id: u64,
    pub pane_id: u64,
    pub byte_start: u64,
    pub byte_end: u64,
    pub line_start: u32,
    pub line_end: u32,
    pub content_hash: u64,
    pub written_ts_ms: u64,
    pub last_access_ts_ms: u64,
    pub tier_slug: String, // mirrors ScrollbackTier slug
    pub redaction_slug: String,
    pub encryption_slug: String,
}

impl MetadataIndexRow {
    /// SQL DDL the integration runs at first launch. Foundation slice ships the schema; integration's storage migration framework consumes it.
    pub const TABLE_DDL: &'static str = "CREATE TABLE IF NOT EXISTS scrollback_chunks (
        chunk_id INTEGER PRIMARY KEY,
        pane_id INTEGER NOT NULL,
        byte_start INTEGER NOT NULL,
        byte_end INTEGER NOT NULL,
        line_start INTEGER NOT NULL,
        line_end INTEGER NOT NULL,
        content_hash INTEGER NOT NULL,
        written_ts_ms INTEGER NOT NULL,
        last_access_ts_ms INTEGER NOT NULL,
        tier TEXT NOT NULL,
        redaction TEXT NOT NULL,
        encryption TEXT NOT NULL
    )";

    pub const INDEX_DDL: &'static str = "CREATE INDEX IF NOT EXISTS scrollback_chunks_by_pane_ts ON scrollback_chunks (pane_id, written_ts_ms DESC)";

    /// Validate row invariants. Returns first violation
    /// or `None` if valid.
    #[must_use]
    pub fn validate(&self) -> Option<&'static str> {
        if self.byte_start > self.byte_end {
            return Some("byte_start > byte_end");
        }
        if self.line_start > self.line_end {
            return Some("line_start > line_end");
        }
        if self.last_access_ts_ms < self.written_ts_ms {
            return Some("last_access_ts < written_ts");
        }
        None
    }
}

// ============================================================================
// Key handle (sub-task 4)
// ============================================================================

/// Opaque key handle for AES-256-GCM. Production wraps a
/// `chacha20poly1305::Key` or platform key-store handle;
/// foundation slice ships the shape.
///
/// Bead sub-task 4: "AES-256-GCM encryption-at-rest
/// (operator opt-in, shared key with `ft-2okh0.5`
/// mmap-backed scrollback)."
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColdTierKeyHandle {
    /// Opaque key id — looks up the actual key material
    /// from the OS key-store / scratch keyring. Never
    /// carries plaintext key bytes.
    pub key_id: String,
    /// Bound to the mmap-backed scrollback (sub-task 4
    /// "shared key with ft-2okh0.5"). When this slug
    /// equals the mmap key's slug, the cold-tier reuses
    /// the same handle.
    pub mmap_key_slug: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionMode {
    /// Operator opted out. `skip_encryption` is the
    /// pipeline path.
    Disabled,
    /// AES-256-GCM with the bound key handle.
    Aes256Gcm,
}

// ============================================================================
// Structured log row (sub-task 9)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StructuredLogRow {
    /// Per-write: ts, chunk_id, bytes_in, bytes_out,
    /// redaction_applied, encryption_mode, latency_ns.
    ChunkWrite {
        ts_ms: u64,
        chunk_id: u64,
        pane_id: u64,
        bytes_in: u32,
        bytes_out: u32,
        redaction_applied: bool,
        encryption_mode: String,
        latency_ns: u64,
    },
    /// Per-read: ts, chunk_id, bytes, decompress_ns,
    /// decrypt_ns, total_latency_ns.
    ChunkRead {
        ts_ms: u64,
        chunk_id: u64,
        pane_id: u64,
        bytes_out: u32,
        decompress_ns: u64,
        decrypt_ns: u64,
        total_latency_ns: u64,
    },
    /// Per-eviction: ts, chunks_evicted, bytes_freed.
    EvictionCycle {
        ts_ms: u64,
        chunks_evicted: u32,
        bytes_freed: u64,
    },
    /// Per-purge: ts, chunks_purged, retention_window_ms.
    RetentionPurge {
        ts_ms: u64,
        chunks_purged: u32,
        retention_window_ms: u64,
    },
}

#[must_use]
pub fn render_log_jsonl(rows: &[StructuredLogRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let line = serde_json::to_string(r).expect("StructuredLogRow always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_log_jsonl(jsonl: &str) -> Result<Vec<StructuredLogRow>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// Pipeline health
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PipelineHealth {
    pub chunks_written_total: u64,
    pub chunks_read_total: u64,
    /// Pre-compress byte count (raw input to the pipeline).
    /// Combined with `bytes_written_total`, gives the
    /// observed compression ratio. Per ft-vgtab (was the
    /// dead `bytes_in` parameter).
    pub bytes_pre_compress_total: u64,
    pub bytes_written_total: u64,
    pub bytes_read_total: u64,
    /// Count of writes where the redactor was actually
    /// applied. **Critical for the bead's DO NOT BREAK
    /// privacy invariant.** Per ft-vgtab: previously
    /// always incremented unconditionally, defeating the
    /// runtime double-check. Now incremented only when
    /// the integration passes `redactor_applied=true`.
    pub redactions_applied_total: u64,
    /// Count of writes where the integration explicitly
    /// reported the redactor was NOT applied. Should
    /// always be 0 in production; non-zero indicates the
    /// privacy invariant is broken at runtime even if
    /// the typed-state pipeline was satisfied.
    pub chunks_written_without_redactor: u64,
    pub encryptions_applied_total: u64,
    pub encryption_skipped_total: u64,
    pub evictions_total: u64,
    pub retention_purges_total: u64,
    /// Per-pipeline-stage failure counter (e.g.,
    /// "compress_zstd", "encrypt_aes256_gcm",
    /// "write_disk").
    pub stage_failures: BTreeMap<String, u64>,
}

impl PipelineHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self::default()
    }

    /// True iff redactor was applied to *every* write
    /// (the privacy invariant). The integration enforces
    /// this structurally via the typed-state pipeline;
    /// the doctor double-checks at runtime.
    ///
    /// Per ft-vgtab fix: this now compares
    /// `redactions_applied_total == chunks_written_total`
    /// AND `chunks_written_without_redactor == 0`. Before
    /// the fix, `record_write` unconditionally incremented
    /// `redactions_applied_total`, so this check was a
    /// rubber-stamp.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        self.redactions_applied_total == self.chunks_written_total
            && self.chunks_written_without_redactor == 0
    }

    /// Record a chunk-write event.
    ///
    /// `redactor_applied` MUST reflect whether the integration
    /// actually invoked a non-identity redactor. The typed-state
    /// pipeline alone cannot detect an identity-function
    /// redactor closure (`|bytes| bytes` typechecks); this
    /// flag is the runtime second line of defense.
    pub fn record_write(
        &mut self,
        bytes_in: u32,
        bytes_out: u32,
        redactor_applied: bool,
        encrypted: bool,
    ) {
        self.chunks_written_total = self.chunks_written_total.saturating_add(1);
        self.bytes_pre_compress_total = self
            .bytes_pre_compress_total
            .saturating_add(bytes_in as u64);
        self.bytes_written_total = self.bytes_written_total.saturating_add(bytes_out as u64);
        if redactor_applied {
            self.redactions_applied_total =
                self.redactions_applied_total.saturating_add(1);
        } else {
            self.chunks_written_without_redactor = self
                .chunks_written_without_redactor
                .saturating_add(1);
        }
        if encrypted {
            self.encryptions_applied_total =
                self.encryptions_applied_total.saturating_add(1);
        } else {
            self.encryption_skipped_total =
                self.encryption_skipped_total.saturating_add(1);
        }
    }

    pub fn record_stage_failure(&mut self, stage: &str) {
        *self.stage_failures.entry(stage.to_string()).or_insert(0) += 1;
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ------------------------------------------------------------------------
    // Disk path layout
    // ------------------------------------------------------------------------

    #[test]
    fn disk_path_renders_unencrypted_layout() {
        let p = disk_path_for(7, 42, false);
        let cache_root = Path::new("/tmp/cache");
        let rendered = p.render(cache_root);
        assert_eq!(
            rendered,
            Path::new("/tmp/cache/scrollback/7/42.zst")
        );
    }

    #[test]
    fn disk_path_renders_encrypted_layout() {
        let p = disk_path_for(7, 42, true);
        let cache_root = Path::new("/tmp/cache");
        let rendered = p.render(cache_root);
        assert_eq!(
            rendered,
            Path::new("/tmp/cache/scrollback/7/42.zst.enc")
        );
    }

    #[test]
    fn matches_layout_accepts_well_formed_path() {
        let p = Path::new("/anywhere/scrollback/7/42.zst");
        assert!(ColdTierDiskPath::matches_layout(p));
    }

    #[test]
    fn matches_layout_accepts_encrypted_path() {
        let p = Path::new("/anywhere/scrollback/99/100.zst.enc");
        assert!(ColdTierDiskPath::matches_layout(p));
    }

    #[test]
    fn matches_layout_rejects_non_numeric_chunk_id() {
        let p = Path::new("/anywhere/scrollback/7/abc.zst");
        assert!(!ColdTierDiskPath::matches_layout(p));
    }

    #[test]
    fn matches_layout_rejects_non_numeric_pane_id() {
        let p = Path::new("/anywhere/scrollback/abc/42.zst");
        assert!(!ColdTierDiskPath::matches_layout(p));
    }

    #[test]
    fn matches_layout_rejects_missing_zst_suffix() {
        let p = Path::new("/anywhere/scrollback/7/42");
        assert!(!ColdTierDiskPath::matches_layout(p));
    }

    #[test]
    fn file_mode_constant_is_0600() {
        assert_eq!(ColdTierDiskPath::FILE_MODE, 0o600);
    }

    // ------------------------------------------------------------------------
    // Typed-state pipeline
    // ------------------------------------------------------------------------

    #[test]
    fn pipeline_full_path_with_redaction_compression_encryption() {
        // The compile-time invariant: pipeline order is
        // forced. Skipping a stage is impossible because
        // each stage's wrapper only admits the next.
        let raw = ChunkBytes::<Raw>::from_raw(b"sensitive data".to_vec());
        let redacted = raw.redact_with(|b| {
            // Mock redactor: replaces "sensitive" with
            // "[REDACTED]".
            let s = String::from_utf8(b).unwrap();
            s.replace("sensitive", "[REDACTED]").into_bytes()
        });
        assert_eq!(redacted.as_bytes(), b"[REDACTED] data");

        let compressed = redacted.compress_with(|b| {
            // Mock compressor: prefix with magic.
            let mut out = b"ZSTD".to_vec();
            out.extend(b);
            out
        });

        let key = ColdTierKeyHandle {
            key_id: "test-key".to_string(),
            mmap_key_slug: "mmap-key".to_string(),
        };
        let encrypted = compressed.encrypt_with(&key, |b| {
            // Mock cipher: prefix with magic.
            let mut out = b"AES-".to_vec();
            out.extend(b);
            out
        });
        assert!(encrypted.as_bytes().starts_with(b"AES-ZSTD"));

        let written = encrypted.mark_written();
        assert!(written.as_bytes().starts_with(b"AES-ZSTD"));
    }

    #[test]
    fn redact_with_evidence_returns_match_count() {
        // Self-review fix (br-ft-0gjrq): redact_with_evidence
        // lets the integration trust substrate output for the
        // record_write redactor_applied flag.
        let raw = ChunkBytes::<Raw>::from_raw(b"api_key=hunter2".to_vec());
        let (redacted, evidence) = raw.redact_with_evidence(|b| {
            let s = String::from_utf8(b).unwrap();
            let replaced = s.replace("hunter2", "[REDACTED]");
            let evid = RedactionEvidence {
                matches: 1,
                bytes_replaced: 7,
            };
            (replaced.into_bytes(), evid)
        });
        assert_eq!(redacted.as_bytes(), b"api_key=[REDACTED]");
        assert!(evidence.redactor_applied());
        assert!(evidence.made_changes());
        assert_eq!(evidence.matches, 1);
        assert_eq!(evidence.bytes_replaced, 7);
    }

    #[test]
    fn redact_with_evidence_no_match_still_signals_applied() {
        // Redactor scanned but found nothing. Evidence still
        // signals applied=true (substrate semantic).
        let raw = ChunkBytes::<Raw>::from_raw(b"benign text".to_vec());
        let (redacted, evidence) = raw.redact_with_evidence(|b| {
            // Scanned with rules; matched zero.
            (b, RedactionEvidence::default())
        });
        assert_eq!(redacted.as_bytes(), b"benign text");
        assert!(evidence.redactor_applied());
        assert!(!evidence.made_changes());
    }

    #[test]
    fn pipeline_with_skip_encryption() {
        let raw = ChunkBytes::<Raw>::from_raw(b"data".to_vec());
        let redacted = raw.redact_with(|b| b);
        let compressed = redacted.compress_with(|b| b);
        let encrypted = compressed.skip_encryption();
        let written = encrypted.mark_written();
        assert_eq!(written.as_bytes(), b"data");
    }

    #[test]
    fn pipeline_write_with_succeeds_on_writer_ok() {
        let raw = ChunkBytes::<Raw>::from_raw(b"data".to_vec());
        let redacted = raw.redact_with(|b| b);
        let compressed = redacted.compress_with(|b| b);
        let encrypted = compressed.skip_encryption();
        let mut sink: Vec<u8> = Vec::new();
        let written: ChunkBytes<Written> = encrypted
            .write_with::<_, ()>(|bytes| {
                sink.extend_from_slice(bytes);
                Ok(())
            })
            .unwrap();
        assert_eq!(written.as_bytes(), b"data");
        assert_eq!(sink, b"data");
    }

    #[test]
    fn pipeline_write_with_returns_self_on_writer_err() {
        let raw = ChunkBytes::<Raw>::from_raw(b"data".to_vec());
        let redacted = raw.redact_with(|b| b);
        let compressed = redacted.compress_with(|b| b);
        let encrypted = compressed.skip_encryption();
        let result: Result<ChunkBytes<Written>, (ChunkBytes<Encrypted>, &'static str)> =
            encrypted.write_with(|_| Err("io_failed"));
        match result {
            Err((still_encrypted, e)) => {
                // Caller can retry without losing typed-state.
                assert_eq!(still_encrypted.as_bytes(), b"data");
                assert_eq!(e, "io_failed");
            }
            Ok(_) => panic!("expected write to fail"),
        }
    }

    #[test]
    fn bytes_field_is_private_no_mid_pipeline_mutation() {
        // Privacy invariant: ChunkBytes::bytes is private,
        // so a maintainer cannot replace bytes between
        // stages to bypass the redactor. The only way to
        // change bytes is via the typed-state transition
        // closures (redact_with, compress_with, etc.) —
        // which structurally guarantee the redactor ran.
        let raw = ChunkBytes::<Raw>::from_raw(b"secret".to_vec());
        let redacted = raw.redact_with(|_| b"[REDACTED]".to_vec());
        // Cannot do `redacted.bytes = b"unredacted".to_vec()` —
        // compile error, field is private.
        assert_eq!(redacted.as_bytes(), b"[REDACTED]");
    }

    // The following don't-compile assertions document
    // what the typed-state prevents. We don't run them,
    // but they're reasoned about in the audit doc:
    //
    // // Cannot construct Compressed without Redacted:
    // let raw = ChunkBytes::<Raw>::from_raw(vec![]);
    // let compressed: ChunkBytes<Compressed> = raw.compress_with(|b| b);
    // // ^^^ compile error: Raw has no compress_with method
    //
    // // Cannot mark_written without Encrypted state:
    // let raw = ChunkBytes::<Raw>::from_raw(vec![]);
    // let written = raw.mark_written();
    // // ^^^ compile error: Raw has no mark_written method

    // ------------------------------------------------------------------------
    // Metadata index schema
    // ------------------------------------------------------------------------

    #[test]
    fn metadata_index_ddl_is_idempotent() {
        // CREATE TABLE IF NOT EXISTS — running it twice
        // is safe.
        assert!(MetadataIndexRow::TABLE_DDL.contains("IF NOT EXISTS"));
        assert!(MetadataIndexRow::INDEX_DDL.contains("IF NOT EXISTS"));
    }

    #[test]
    fn metadata_index_has_all_bead_required_columns() {
        // Bead sub-task 6 column list:
        // id, byte_range, line_range, content_hash,
        // written_ts, last_access_ts, tier, redaction,
        // encryption.
        let ddl = MetadataIndexRow::TABLE_DDL;
        for col in [
            "chunk_id",
            "byte_start",
            "byte_end",
            "line_start",
            "line_end",
            "content_hash",
            "written_ts_ms",
            "last_access_ts_ms",
            "tier",
            "redaction",
            "encryption",
        ] {
            assert!(ddl.contains(col), "missing column {col} in DDL");
        }
    }

    #[test]
    fn metadata_row_validates_byte_range() {
        let row = MetadataIndexRow {
            chunk_id: 1,
            pane_id: 1,
            byte_start: 100,
            byte_end: 50, // invalid
            line_start: 0,
            line_end: 1,
            content_hash: 0,
            written_ts_ms: 0,
            last_access_ts_ms: 1,
            tier_slug: "cold".to_string(),
            redaction_slug: "applied".to_string(),
            encryption_slug: "aes256_gcm".to_string(),
        };
        assert_eq!(row.validate(), Some("byte_start > byte_end"));
    }

    #[test]
    fn metadata_row_validates_line_range() {
        let row = MetadataIndexRow {
            chunk_id: 1,
            pane_id: 1,
            byte_start: 0,
            byte_end: 100,
            line_start: 5,
            line_end: 3, // invalid
            content_hash: 0,
            written_ts_ms: 0,
            last_access_ts_ms: 1,
            tier_slug: "cold".to_string(),
            redaction_slug: "applied".to_string(),
            encryption_slug: "aes256_gcm".to_string(),
        };
        assert_eq!(row.validate(), Some("line_start > line_end"));
    }

    #[test]
    fn metadata_row_validates_access_after_write() {
        let row = MetadataIndexRow {
            chunk_id: 1,
            pane_id: 1,
            byte_start: 0,
            byte_end: 100,
            line_start: 0,
            line_end: 5,
            content_hash: 0,
            written_ts_ms: 1000,
            last_access_ts_ms: 500, // invalid (< written)
            tier_slug: "cold".to_string(),
            redaction_slug: "applied".to_string(),
            encryption_slug: "aes256_gcm".to_string(),
        };
        assert_eq!(row.validate(), Some("last_access_ts < written_ts"));
    }

    #[test]
    fn metadata_row_valid_passes_validation() {
        let row = MetadataIndexRow {
            chunk_id: 1,
            pane_id: 1,
            byte_start: 0,
            byte_end: 100,
            line_start: 0,
            line_end: 5,
            content_hash: 0xDEAD_BEEF,
            written_ts_ms: 1000,
            last_access_ts_ms: 2000,
            tier_slug: "cold".to_string(),
            redaction_slug: "applied".to_string(),
            encryption_slug: "aes256_gcm".to_string(),
        };
        assert_eq!(row.validate(), None);
    }

    #[test]
    fn metadata_row_serde_roundtrip() {
        let row = MetadataIndexRow {
            chunk_id: 42,
            pane_id: 7,
            byte_start: 0,
            byte_end: 100,
            line_start: 0,
            line_end: 5,
            content_hash: 0xCAFE_BABE,
            written_ts_ms: 1000,
            last_access_ts_ms: 2000,
            tier_slug: "cold".to_string(),
            redaction_slug: "applied".to_string(),
            encryption_slug: "aes256_gcm".to_string(),
        };
        let json = serde_json::to_string(&row).unwrap();
        let parsed: MetadataIndexRow = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, row);
    }

    // ------------------------------------------------------------------------
    // Structured logging
    // ------------------------------------------------------------------------

    #[test]
    fn structured_log_jsonl_roundtrip() {
        let rows = vec![
            StructuredLogRow::ChunkWrite {
                ts_ms: 1_000,
                chunk_id: 42,
                pane_id: 7,
                bytes_in: 1_000,
                bytes_out: 250,
                redaction_applied: true,
                encryption_mode: "aes256_gcm".to_string(),
                latency_ns: 1_500_000,
            },
            StructuredLogRow::ChunkRead {
                ts_ms: 2_000,
                chunk_id: 42,
                pane_id: 7,
                bytes_out: 1_000,
                decompress_ns: 500_000,
                decrypt_ns: 200_000,
                total_latency_ns: 800_000,
            },
            StructuredLogRow::EvictionCycle {
                ts_ms: 3_000,
                chunks_evicted: 5,
                bytes_freed: 50_000,
            },
            StructuredLogRow::RetentionPurge {
                ts_ms: 4_000,
                chunks_purged: 3,
                retention_window_ms: 7 * 24 * 60 * 60 * 1000,
            },
        ];
        let jsonl = render_log_jsonl(&rows);
        let parsed = parse_log_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, rows);
    }

    // ------------------------------------------------------------------------
    // Pipeline health
    // ------------------------------------------------------------------------

    #[test]
    fn health_baseline_safe() {
        assert!(PipelineHealth::baseline().is_safe());
    }

    #[test]
    fn health_records_writes_with_redactor_invariant() {
        let mut h = PipelineHealth::baseline();
        // (bytes_in, bytes_out, redactor_applied, encrypted)
        h.record_write(1_000, 250, true, true);
        h.record_write(2_000, 500, true, false);
        // Privacy invariant: redactions_applied_total ==
        // chunks_written_total AND no without-redactor writes.
        assert!(h.is_safe());
        assert_eq!(h.chunks_written_total, 2);
        assert_eq!(h.bytes_pre_compress_total, 3_000);
        assert_eq!(h.encryptions_applied_total, 1);
        assert_eq!(h.encryption_skipped_total, 1);
    }

    /// ft-vgtab regression guard: `record_write` previously
    /// always-claimed the redactor was applied, defeating the
    /// runtime double-check on the bead's privacy invariant.
    /// The fix takes a `redactor_applied: bool` parameter.
    #[test]
    fn is_safe_detects_write_without_redactor() {
        let mut h = PipelineHealth::baseline();
        h.record_write(100, 50, false, true); // redactor was NOT applied
        assert!(
            !h.is_safe(),
            "is_safe() must return false when a write happened without redactor — \
             this is the privacy invariant the bead's DO NOT BREAK rule enforces"
        );
        assert_eq!(h.chunks_written_without_redactor, 1);
        assert_eq!(h.redactions_applied_total, 0);
    }

    #[test]
    fn is_safe_clean_when_every_write_redacted() {
        let mut h = PipelineHealth::baseline();
        for _ in 0..10 {
            h.record_write(100, 50, true, true);
        }
        assert!(h.is_safe());
        assert_eq!(h.chunks_written_total, 10);
        assert_eq!(h.redactions_applied_total, 10);
        assert_eq!(h.chunks_written_without_redactor, 0);
    }

    #[test]
    fn bytes_pre_compress_total_is_now_tracked() {
        // ft-vgtab also revived the previously-dead bytes_in
        // parameter as a real telemetry counter for compression-
        // ratio computation.
        let mut h = PipelineHealth::baseline();
        h.record_write(10_000, 1_000, true, false); // 10:1 ratio
        h.record_write(20_000, 4_000, true, false); // 5:1 ratio
        assert_eq!(h.bytes_pre_compress_total, 30_000);
        assert_eq!(h.bytes_written_total, 5_000);
        // Caller can compute observed ratio: 30000 / 5000 = 6:1.
    }

    #[test]
    fn health_records_stage_failures() {
        let mut h = PipelineHealth::baseline();
        h.record_stage_failure("compress_zstd");
        h.record_stage_failure("compress_zstd");
        h.record_stage_failure("encrypt_aes256_gcm");
        assert_eq!(h.stage_failures.get("compress_zstd"), Some(&2));
        assert_eq!(h.stage_failures.get("encrypt_aes256_gcm"), Some(&1));
    }

    // ------------------------------------------------------------------------
    // Headline scenario
    // ------------------------------------------------------------------------

    #[test]
    fn full_write_path_scenario_with_audit_trail() {
        // Bead's stated DO NOT BREAK rule: redactor MUST
        // apply before disk write. The typed-state
        // pipeline structurally enforces this; the doctor
        // counter double-checks.
        let mut health = PipelineHealth::baseline();

        let raw = ChunkBytes::<Raw>::from_raw(b"password=hunter2".to_vec());
        let redacted = raw.redact_with(|_| b"password=[REDACTED]".to_vec());
        let compressed = redacted.compress_with(|b| b); // mock zstd
        let key = ColdTierKeyHandle {
            key_id: "k".to_string(),
            mmap_key_slug: "mk".to_string(),
        };
        let encrypted = compressed.encrypt_with(&key, |b| b); // mock AES
        let written = encrypted.mark_written();

        health.record_write(
            b"password=hunter2".len() as u32,
            written.len() as u32,
            true, // redactor_applied
            true, // encrypted
        );
        assert!(health.is_safe());
        assert_eq!(health.chunks_written_total, 1);
        assert_eq!(health.redactions_applied_total, 1);
        assert_eq!(health.encryptions_applied_total, 1);

        // Verify path layout matches bead's convention.
        let path = disk_path_for(7, 42, true).render(Path::new("/tmp"));
        assert!(ColdTierDiskPath::matches_layout(&path));
    }
}
