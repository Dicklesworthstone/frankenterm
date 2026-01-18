//! Storage layer with SQLite and FTS5
//!
//! Provides persistent storage for captured output, events, and workflows.

use serde::{Deserialize, Serialize};

/// A captured segment of pane output
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    /// Unique segment ID
    pub id: i64,
    /// Pane this segment belongs to
    pub pane_id: u64,
    /// Sequence number within the pane (monotonically increasing)
    pub seq: u64,
    /// The captured text content
    pub content: String,
    /// Timestamp when captured
    pub captured_at: i64,
}

/// A gap event indicating discontinuous capture
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Gap {
    /// Unique gap ID
    pub id: i64,
    /// Pane where gap occurred
    pub pane_id: u64,
    /// Sequence number before gap
    pub seq_before: u64,
    /// Sequence number after gap
    pub seq_after: u64,
    /// Reason for gap
    pub reason: String,
    /// Timestamp of gap detection
    pub detected_at: i64,
}

/// Storage handle for async operations
pub struct StorageHandle {
    // TODO: Implement writer thread + read pool
    _db_path: String,
}

impl StorageHandle {
    /// Create a new storage handle
    pub async fn new(db_path: &str) -> crate::Result<Self> {
        // TODO: Initialize database and writer thread
        Ok(Self {
            _db_path: db_path.to_string(),
        })
    }

    /// Append a segment to storage
    pub async fn append_segment(&self, _pane_id: u64, _content: &str) -> crate::Result<Segment> {
        // TODO: Implement segment append
        todo!("Implement segment append")
    }

    /// Record a gap event
    pub async fn record_gap(&self, _pane_id: u64, _reason: &str) -> crate::Result<Gap> {
        // TODO: Implement gap recording
        todo!("Implement gap recording")
    }

    /// Search segments using FTS5
    pub async fn search(&self, _query: &str) -> crate::Result<Vec<Segment>> {
        // TODO: Implement FTS search
        todo!("Implement FTS search")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_serializes() {
        let segment = Segment {
            id: 1,
            pane_id: 42,
            seq: 100,
            content: "Hello, world!".to_string(),
            captured_at: 1_234_567_890,
        };

        let json = serde_json::to_string(&segment).unwrap();
        assert!(json.contains("Hello, world!"));
    }
}
