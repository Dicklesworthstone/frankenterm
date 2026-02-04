//! Secret scanning engine.
//!
//! Reuses the policy redaction patterns to detect secrets in stored segments.
//!
//! The scanner never returns raw secret values. It only emits hashes and
//! redacted-safe metadata.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Result;
use crate::policy::Redactor;
use crate::storage::{Segment, SegmentScanQuery, StorageHandle};

/// Options for secret scans over stored segments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretScanOptions {
    /// Filter by pane ID.
    pub pane_id: Option<u64>,
    /// Filter by start time (epoch ms).
    pub since: Option<i64>,
    /// Filter by end time (epoch ms).
    pub until: Option<i64>,
    /// Maximum segments to scan (None = unlimited).
    pub max_segments: Option<usize>,
    /// Batch size for incremental reads.
    pub batch_size: usize,
    /// Maximum number of sample records to retain.
    pub sample_limit: usize,
}

impl Default for SecretScanOptions {
    fn default() -> Self {
        Self {
            pane_id: None,
            since: None,
            until: None,
            max_segments: None,
            batch_size: 1_000,
            sample_limit: 200,
        }
    }
}

/// A single redaction-safe secret match sample.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretScanSample {
    /// Pattern name for the match.
    pub pattern: String,
    /// Segment ID containing the match.
    pub segment_id: i64,
    /// Pane ID containing the match.
    pub pane_id: u64,
    /// Segment capture timestamp (epoch ms).
    pub captured_at: i64,
    /// Hash of the matched secret bytes (SHA-256 hex).
    pub secret_hash: String,
    /// Length in bytes of the matched secret.
    pub match_len: usize,
}

/// Report produced by a secret scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecretScanReport {
    /// Total segments scanned.
    pub scanned_segments: u64,
    /// Total bytes scanned.
    pub scanned_bytes: u64,
    /// Total secret matches across all patterns.
    pub matches_total: u64,
    /// Matches per pattern.
    pub matches_by_pattern: BTreeMap<String, u64>,
    /// Sampled matches (hashes only).
    pub samples: Vec<SecretScanSample>,
}

impl SecretScanReport {
    fn new() -> Self {
        Self {
            scanned_segments: 0,
            scanned_bytes: 0,
            matches_total: 0,
            matches_by_pattern: BTreeMap::new(),
            samples: Vec::new(),
        }
    }
}

/// Secret scanning engine.
#[derive(Debug, Default)]
pub struct SecretScanEngine {
    redactor: Redactor,
}

impl SecretScanEngine {
    /// Create a new scanner using the default redaction patterns.
    #[must_use]
    pub fn new() -> Self {
        Self {
            redactor: Redactor::new(),
        }
    }

    /// Scan stored segments for secrets using the redaction patterns.
    ///
    /// The scan is performed in batches to avoid loading the full database
    /// into memory.
    pub async fn scan_storage(
        &self,
        storage: &StorageHandle,
        mut options: SecretScanOptions,
    ) -> Result<SecretScanReport> {
        if options.batch_size == 0 {
            options.batch_size = 1_000;
        }

        let mut report = SecretScanReport::new();
        let mut after_id: Option<i64> = None;

        loop {
            let remaining = options
                .max_segments
                .map(|max| max.saturating_sub(report.scanned_segments as usize));
            if matches!(remaining, Some(0)) {
                break;
            }

            let limit = remaining
                .map(|remain| remain.min(options.batch_size))
                .unwrap_or(options.batch_size);

            let query = SegmentScanQuery {
                after_id,
                pane_id: options.pane_id,
                since: options.since,
                until: options.until,
                limit,
            };

            let batch = storage.scan_segments(query).await?;
            if batch.is_empty() {
                break;
            }

            for segment in &batch {
                self.scan_segment(segment, &options, &mut report);
                report.scanned_segments += 1;
                report.scanned_bytes += segment.content_len as u64;

                if let Some(max) = options.max_segments {
                    if report.scanned_segments as usize >= max {
                        break;
                    }
                }
            }

            after_id = batch.last().map(|seg| seg.id);
            if batch.len() < limit {
                break;
            }
        }

        Ok(report)
    }

    fn scan_segment(
        &self,
        segment: &Segment,
        options: &SecretScanOptions,
        report: &mut SecretScanReport,
    ) {
        let detections = self.redactor.detect(&segment.content);
        if detections.is_empty() {
            return;
        }

        for (pattern, start, end) in detections {
            report.matches_total += 1;
            *report
                .matches_by_pattern
                .entry(pattern.to_string())
                .or_insert(0) += 1;

            if report.samples.len() >= options.sample_limit {
                continue;
            }

            let Some(secret) = segment.content.get(start..end) else {
                continue;
            };

            let secret_hash = hash_secret(secret);
            report.samples.push(SecretScanSample {
                pattern: pattern.to_string(),
                segment_id: segment.id,
                pane_id: segment.pane_id,
                captured_at: segment.captured_at,
                secret_hash,
                match_len: secret.len(),
            });
        }
    }
}

fn hash_secret(secret: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(secret.as_bytes());
    let digest = hasher.finalize();
    hex_encode(&digest)
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_secret_is_stable() {
        let first = hash_secret("sk-secret-1234567890");
        let second = hash_secret("sk-secret-1234567890");
        assert_eq!(first, second);
    }

    #[test]
    fn scan_segment_does_not_store_raw_secret() {
        let engine = SecretScanEngine::new();
        let mut report = SecretScanReport::new();
        let options = SecretScanOptions::default();

        let segment = Segment {
            id: 1,
            pane_id: 2,
            seq: 0,
            content: "token: sk-abc123456789012345678901234567890123456789012345678901".to_string(),
            content_len: 79,
            content_hash: None,
            captured_at: 0,
        };

        engine.scan_segment(&segment, &options, &mut report);
        assert_eq!(report.samples.len(), 1);
        let sample = &report.samples[0];
        assert_ne!(
            sample.secret_hash,
            "sk-abc123456789012345678901234567890123456789012345678901"
        );
    }
}
