//! Storage row-shape types extracted from `storage.rs`
//! (br-ft-bbhwz / ft-dn2tu Phase 2.3).
//!
//! This is the destination for the bulk of `storage.rs`'s
//! pub-struct surface. The full move is large (~1,440 lines per
//! the bead's audit). This first slice ships a subset of pure
//! data types whose impls have no private-helper deps:
//! `Segment`, `CheckpointResult`, `DatabasePageStats` (+ impl),
//! `Gap`, `FtsSyncConfig` (+ Default).
//!
//! Each type is re-exported from `storage.rs` so external
//! callers see no API change.
//!
//! Cross-references:
//! - parent: ft-dn2tu storage.rs split
//! - prior: ft-6qkx1 (Phase 2 schema_ddl extraction at 412d277f2)
//! - sibling: ft-94ito (Phase 2.2 — migrations_types.rs)
//! - follow-up: cont-bead for the remaining ~25 types

use serde::{Deserialize, Serialize};

/// A captured segment of pane output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    /// Unique segment ID.
    pub id: i64,
    /// Pane this segment belongs to.
    pub pane_id: u64,
    /// Sequence number within the pane (monotonically increasing).
    pub seq: u64,
    /// The captured text content.
    pub content: String,
    /// Content length (cached).
    pub content_len: usize,
    /// Optional content hash for overlap detection.
    pub content_hash: Option<String>,
    /// Timestamp when captured (epoch ms).
    pub captured_at: i64,
}

/// Result of a WAL checkpoint operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointResult {
    /// Number of WAL frames checkpointed.
    pub wal_pages: i64,
    /// Whether `PRAGMA optimize` was also run.
    pub optimized: bool,
}

/// SQLite page-level space usage, used for vacuum decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabasePageStats {
    /// Total number of pages in the database file.
    pub page_count: i64,
    /// Number of free pages currently on the freelist.
    pub free_pages: i64,
}

impl DatabasePageStats {
    /// Ratio of free pages to total pages, bounded to `[0.0, 1.0]`.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn free_ratio(&self) -> f64 {
        if self.page_count <= 0 || self.free_pages <= 0 {
            return 0.0;
        }
        let bounded_free = self.free_pages.min(self.page_count);
        bounded_free as f64 / self.page_count as f64
    }
}

/// A gap event indicating discontinuous capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gap {
    /// Unique gap ID.
    pub id: i64,
    /// Pane where gap occurred.
    pub pane_id: u64,
    /// Sequence number before gap.
    pub seq_before: u64,
    /// Sequence number after gap.
    pub seq_after: u64,
    /// Reason for gap.
    pub reason: String,
    /// Timestamp of gap detection (epoch ms).
    pub detected_at: i64,
}

/// Configuration for FTS sync batching.
#[derive(Debug, Clone)]
pub struct FtsSyncConfig {
    /// Maximum segments per batch.
    pub batch_size: usize,
    /// Maximum bytes per batch.
    pub max_batch_bytes: usize,
    /// Whether to commit progress after each batch.
    pub commit_progress: bool,
}

impl Default for FtsSyncConfig {
    fn default() -> Self {
        Self {
            batch_size: 100,
            max_batch_bytes: 1_048_576, // 1 MB
            commit_progress: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// br-ft-bbhwz: free_ratio handles edge cases — empty db,
    /// freelist > total pages, both zero. Pinned at the type
    /// extraction boundary so the impl semantics survive any
    /// future refactor.
    #[test]
    fn database_page_stats_free_ratio_clamps_and_handles_zero() {
        // Both zero → 0.0.
        assert_eq!(
            DatabasePageStats {
                page_count: 0,
                free_pages: 0
            }
            .free_ratio(),
            0.0,
        );
        // Negative inputs (shouldn't happen in practice but
        // SQLite returns i64) → 0.0.
        assert_eq!(
            DatabasePageStats {
                page_count: -1,
                free_pages: 50
            }
            .free_ratio(),
            0.0,
        );
        // free > total → bounded to total → 1.0.
        assert_eq!(
            DatabasePageStats {
                page_count: 100,
                free_pages: 200
            }
            .free_ratio(),
            1.0,
        );
        // Normal case → exact ratio.
        let r = DatabasePageStats {
            page_count: 1000,
            free_pages: 250,
        }
        .free_ratio();
        assert!((r - 0.25).abs() < 1e-9);
    }

    /// br-ft-bbhwz: FtsSyncConfig::default values are stable.
    /// Operators tuning batch sizes downstream rely on the
    /// 100-segment / 1-MB / commit_progress=true defaults.
    #[test]
    fn fts_sync_config_default_is_stable() {
        let cfg = FtsSyncConfig::default();
        assert_eq!(cfg.batch_size, 100);
        assert_eq!(cfg.max_batch_bytes, 1_048_576);
        assert!(cfg.commit_progress);
    }

    /// br-ft-bbhwz: Segment serde round-trip preserves every
    /// field byte-for-byte. The wire format downstream callers
    /// depend on cannot drift across the extraction boundary.
    #[test]
    fn segment_serde_roundtrip() {
        let s = Segment {
            id: 42,
            pane_id: 7,
            seq: 1234,
            content: "hello".to_string(),
            content_len: 5,
            content_hash: Some("abc123".to_string()),
            captured_at: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Segment = serde_json::from_str(&json).unwrap();
        assert_eq!(s.id, back.id);
        assert_eq!(s.pane_id, back.pane_id);
        assert_eq!(s.seq, back.seq);
        assert_eq!(s.content, back.content);
        assert_eq!(s.content_len, back.content_len);
        assert_eq!(s.content_hash, back.content_hash);
        assert_eq!(s.captured_at, back.captured_at);
    }

    /// br-ft-bbhwz: Gap serde round-trip — same contract.
    #[test]
    fn gap_serde_roundtrip() {
        let g = Gap {
            id: 1,
            pane_id: 7,
            seq_before: 100,
            seq_after: 110,
            reason: "daemon restart".to_string(),
            detected_at: 1_700_000_000_000,
        };
        let json = serde_json::to_string(&g).unwrap();
        let back: Gap = serde_json::from_str(&json).unwrap();
        assert_eq!(g.id, back.id);
        assert_eq!(g.pane_id, back.pane_id);
        assert_eq!(g.seq_before, back.seq_before);
        assert_eq!(g.seq_after, back.seq_after);
        assert_eq!(g.reason, back.reason);
        assert_eq!(g.detected_at, back.detected_at);
    }
}
