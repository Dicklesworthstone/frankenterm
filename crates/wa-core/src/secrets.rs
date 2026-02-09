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
use crate::accounts::now_ms;
use crate::policy::Redactor;
use crate::storage::{SecretScanReportRecord, Segment, SegmentScanQuery, StorageHandle};

/// Current report schema version for secret scans.
pub const SECRET_SCAN_REPORT_VERSION: u32 = 1;

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

/// Scope definition for secret scans.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecretScanScope {
    /// Filter by pane ID.
    pub pane_id: Option<u64>,
    /// Filter by start time (epoch ms).
    pub since: Option<i64>,
    /// Filter by end time (epoch ms).
    pub until: Option<i64>,
}

impl SecretScanScope {
    pub fn from_options(options: &SecretScanOptions) -> Self {
        Self {
            pane_id: options.pane_id,
            since: options.since,
            until: options.until,
        }
    }
}

/// Compute a stable scope hash for secret scan reports.
pub fn scope_hash(options: &SecretScanOptions) -> Result<String> {
    let scope = SecretScanScope::from_options(options);
    let scope_json = serde_json::to_string(&scope)?;
    Ok(hash_bytes(scope_json.as_bytes()))
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
    /// Report schema version.
    pub report_version: u32,
    /// Scan scope (filters).
    pub scope: SecretScanScope,
    /// When the scan started (epoch ms).
    pub started_at: i64,
    /// When the scan completed (epoch ms).
    pub completed_at: i64,
    /// Segment ID the scan resumed after (if any).
    pub resume_after_id: Option<i64>,
    /// Last segment ID scanned (checkpoint for resume).
    pub last_segment_id: Option<i64>,
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
    fn new(scope: SecretScanScope, resume_after_id: Option<i64>) -> Self {
        let now = now_ms();
        Self {
            report_version: SECRET_SCAN_REPORT_VERSION,
            scope,
            started_at: now,
            completed_at: now,
            resume_after_id,
            last_segment_id: None,
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
        options: SecretScanOptions,
    ) -> Result<SecretScanReport> {
        self.scan_storage_from(storage, options, None).await
    }

    /// Scan stored segments using the latest checkpoint (if available) and
    /// persist a versioned report for incremental resumes.
    pub async fn scan_storage_incremental(
        &self,
        storage: &StorageHandle,
        options: SecretScanOptions,
    ) -> Result<SecretScanReport> {
        let scope = SecretScanScope::from_options(&options);
        let scope_json = serde_json::to_string(&scope)?;
        let scope_hash = hash_bytes(scope_json.as_bytes());
        let checkpoint = storage.latest_secret_scan_report(&scope_hash).await?;
        let resume_after_id = checkpoint.and_then(|report| report.last_segment_id);

        let report = self
            .scan_storage_from(storage, options, resume_after_id)
            .await?;

        let record = SecretScanReportRecord {
            id: 0,
            scope_hash,
            scope_json,
            report_version: i64::from(report.report_version),
            last_segment_id: report.last_segment_id,
            report_json: serde_json::to_string(&report)?,
            created_at: report.completed_at,
        };
        let _ = storage.record_secret_scan_report(record).await?;

        Ok(report)
    }

    async fn scan_storage_from(
        &self,
        storage: &StorageHandle,
        mut options: SecretScanOptions,
        resume_after_id: Option<i64>,
    ) -> Result<SecretScanReport> {
        if options.batch_size == 0 {
            options.batch_size = 1_000;
        }

        let scope = SecretScanScope::from_options(&options);
        let mut report = SecretScanReport::new(scope, resume_after_id);
        let mut after_id = resume_after_id;

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

        report.last_segment_id = after_id;
        report.completed_at = now_ms();
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
    hash_bytes(secret.as_bytes())
}

fn hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
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
    use crate::storage::PaneRecord;

    fn make_segment(id: i64, pane_id: u64, content: &str) -> Segment {
        Segment {
            id,
            pane_id,
            seq: 0,
            content_len: content.len(),
            content: content.to_string(),
            content_hash: None,
            captured_at: 0,
        }
    }

    fn make_report() -> SecretScanReport {
        let options = SecretScanOptions::default();
        let scope = SecretScanScope::from_options(&options);
        SecretScanReport::new(scope, None)
    }

    fn make_pane(pane_id: u64) -> PaneRecord {
        PaneRecord {
            pane_id,
            pane_uuid: None,
            domain: "local".to_string(),
            window_id: None,
            tab_id: None,
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            first_seen_at: 1_000_000_000_000,
            last_seen_at: 1_000_000_000_000,
            observed: true,
            ignore_reason: None,
            last_decision_at: None,
        }
    }

    async fn setup_storage(label: &str) -> (StorageHandle, std::path::PathBuf) {
        let db_path =
            std::env::temp_dir().join(format!("wa_secret_test_{label}_{}.db", std::process::id()));
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(&db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));

        let storage = StorageHandle::new(&db_str).await.expect("open test db");
        // Register pane 1 so foreign-key constraints are satisfied
        storage
            .upsert_pane(make_pane(1))
            .await
            .expect("register pane");
        (storage, db_path)
    }

    async fn teardown(storage: StorageHandle, db_path: &std::path::Path) {
        storage.shutdown().await.expect("shutdown");
        let db_str = db_path.to_string_lossy().to_string();
        let _ = std::fs::remove_file(db_path);
        let _ = std::fs::remove_file(format!("{db_str}-wal"));
        let _ = std::fs::remove_file(format!("{db_str}-shm"));
    }

    // ========================================================================
    // Hashing stability
    // ========================================================================

    #[test]
    fn hash_secret_is_stable() {
        let first = hash_secret("sk-secret-1234567890");
        let second = hash_secret("sk-secret-1234567890");
        assert_eq!(first, second);
    }

    #[test]
    fn hash_secret_different_inputs_differ() {
        let h1 = hash_secret("sk-secret-aaa");
        let h2 = hash_secret("sk-secret-bbb");
        assert_ne!(h1, h2);
    }

    #[test]
    fn hash_bytes_produces_64_hex_chars() {
        let h = hash_bytes(b"test");
        assert_eq!(h.len(), 64, "SHA-256 hex should be 64 chars");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hex_encode_empty() {
        assert_eq!(hex_encode(&[]), "");
    }

    #[test]
    fn hex_encode_known_values() {
        assert_eq!(hex_encode(&[0x00]), "00");
        assert_eq!(hex_encode(&[0xff]), "ff");
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    // ========================================================================
    // SecretScanOptions defaults
    // ========================================================================

    #[test]
    fn default_options_have_sensible_values() {
        let opts = SecretScanOptions::default();
        assert!(opts.pane_id.is_none());
        assert!(opts.since.is_none());
        assert!(opts.until.is_none());
        assert!(opts.max_segments.is_none());
        assert_eq!(opts.batch_size, 1_000);
        assert_eq!(opts.sample_limit, 200);
    }

    // ========================================================================
    // SecretScanScope
    // ========================================================================

    #[test]
    fn scope_from_options_preserves_filters() {
        let opts = SecretScanOptions {
            pane_id: Some(42),
            since: Some(1000),
            until: Some(2000),
            ..Default::default()
        };
        let scope = SecretScanScope::from_options(&opts);
        assert_eq!(scope.pane_id, Some(42));
        assert_eq!(scope.since, Some(1000));
        assert_eq!(scope.until, Some(2000));
    }

    #[test]
    fn scope_default_is_empty() {
        let scope = SecretScanScope::default();
        assert!(scope.pane_id.is_none());
        assert!(scope.since.is_none());
        assert!(scope.until.is_none());
    }

    // ========================================================================
    // scope_hash
    // ========================================================================

    #[test]
    fn scope_hash_is_deterministic() {
        let opts = SecretScanOptions {
            pane_id: Some(1),
            since: Some(100),
            ..Default::default()
        };
        let h1 = scope_hash(&opts).unwrap();
        let h2 = scope_hash(&opts).unwrap();
        assert_eq!(h1, h2);
    }

    #[test]
    fn scope_hash_differs_for_different_filters() {
        let opts1 = SecretScanOptions {
            pane_id: Some(1),
            ..Default::default()
        };
        let opts2 = SecretScanOptions {
            pane_id: Some(2),
            ..Default::default()
        };
        assert_ne!(scope_hash(&opts1).unwrap(), scope_hash(&opts2).unwrap());
    }

    // ========================================================================
    // SecretScanReport::new
    // ========================================================================

    #[test]
    fn new_report_initializes_fields() {
        let report = make_report();
        assert_eq!(report.report_version, SECRET_SCAN_REPORT_VERSION);
        assert_eq!(report.scanned_segments, 0);
        assert_eq!(report.scanned_bytes, 0);
        assert_eq!(report.matches_total, 0);
        assert!(report.matches_by_pattern.is_empty());
        assert!(report.samples.is_empty());
        assert!(report.last_segment_id.is_none());
        assert!(report.resume_after_id.is_none());
    }

    #[test]
    fn new_report_with_resume_preserves_resume_id() {
        let scope = SecretScanScope::default();
        let report = SecretScanReport::new(scope, Some(42));
        assert_eq!(report.resume_after_id, Some(42));
    }

    // ========================================================================
    // scan_segment: pattern detection
    // ========================================================================

    #[test]
    fn scan_segment_does_not_store_raw_secret() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();
        let secret = "sk-abc123456789012345678901234567890123456789012345678901";

        let segment = make_segment(1, 2, &format!("token: {secret}"));

        engine.scan_segment(&segment, &options, &mut report);
        assert!(!report.samples.is_empty());
        for sample in &report.samples {
            assert_ne!(sample.secret_hash, secret);
            // Hash should be a valid 64-char hex string
            assert_eq!(sample.secret_hash.len(), 64);
        }
    }

    #[test]
    fn scan_segment_detects_openai_key() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(
            1,
            1,
            "export OPENAI_API_KEY=sk-abc1234567890abcdef1234567890abcdef12345678",
        );

        engine.scan_segment(&segment, &options, &mut report);
        assert!(report.matches_total > 0, "should detect OpenAI key");
        assert!(
            report.matches_by_pattern.contains_key("openai_key"),
            "pattern name should be openai_key"
        );
    }

    #[test]
    fn scan_segment_detects_anthropic_key() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(
            1,
            1,
            "ANTHROPIC_API_KEY=sk-ant-api03-XXXXXXXXXXXXXXXXXXXXXXXXXXXX",
        );

        engine.scan_segment(&segment, &options, &mut report);
        assert!(report.matches_total > 0, "should detect Anthropic key");
    }

    #[test]
    fn scan_segment_detects_github_token() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(1, 1, "GH_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789");

        engine.scan_segment(&segment, &options, &mut report);
        assert!(report.matches_total > 0, "should detect GitHub token");
    }

    #[test]
    fn scan_segment_detects_aws_access_key() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(1, 1, "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE");

        engine.scan_segment(&segment, &options, &mut report);
        assert!(report.matches_total > 0, "should detect AWS access key");
    }

    #[test]
    fn scan_segment_detects_slack_token() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(1, 1, "SLACK_TOKEN=xoxb-1234567890-abcdefghijklmn");

        engine.scan_segment(&segment, &options, &mut report);
        assert!(report.matches_total > 0, "should detect Slack token");
    }

    #[test]
    fn scan_segment_detects_stripe_key() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(1, 1, "STRIPE_KEY=sk_live_abcdefghijklmnopqrstuvwxyz0123");

        engine.scan_segment(&segment, &options, &mut report);
        assert!(report.matches_total > 0, "should detect Stripe key");
    }

    #[test]
    fn scan_segment_detects_database_url() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(
            1,
            1,
            "DATABASE_URL=postgres://admin:s3cretP4ss@db.host.com:5432/mydb",
        );

        engine.scan_segment(&segment, &options, &mut report);
        assert!(report.matches_total > 0, "should detect database URL");
    }

    #[test]
    fn scan_segment_detects_generic_password() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(1, 1, "password=MyS3cr3tP4ssw0rd!");

        engine.scan_segment(&segment, &options, &mut report);
        assert!(report.matches_total > 0, "should detect generic password");
    }

    #[test]
    fn scan_segment_no_match_on_clean_content() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(1, 1, "Hello world, this is just normal terminal output.");

        engine.scan_segment(&segment, &options, &mut report);
        assert_eq!(
            report.matches_total, 0,
            "clean content should have no matches"
        );
        assert!(report.samples.is_empty());
    }

    #[test]
    fn scan_segment_empty_content() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(1, 1, "");

        engine.scan_segment(&segment, &options, &mut report);
        assert_eq!(report.matches_total, 0);
        assert!(report.samples.is_empty());
    }

    // ========================================================================
    // scan_segment: multiple secrets in one segment
    // ========================================================================

    #[test]
    fn scan_segment_multiple_secrets_counted() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let content = concat!(
            "OPENAI_API_KEY=sk-abc1234567890abcdef1234567890abcdef12345678\n",
            "GH_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789\n",
        );
        let segment = make_segment(1, 1, content);

        engine.scan_segment(&segment, &options, &mut report);
        assert!(
            report.matches_total >= 2,
            "should detect at least 2 secrets, got {}",
            report.matches_total
        );
        assert!(
            report.samples.len() >= 2,
            "should have at least 2 samples, got {}",
            report.samples.len()
        );
    }

    // ========================================================================
    // scan_segment: sample metadata accuracy
    // ========================================================================

    #[test]
    fn scan_segment_sample_preserves_segment_metadata() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = Segment {
            id: 42,
            pane_id: 7,
            seq: 3,
            content: "token: sk-abc1234567890abcdef1234567890abcdef12345678".to_string(),
            content_len: 54,
            content_hash: None,
            captured_at: 1234567890,
        };

        engine.scan_segment(&segment, &options, &mut report);
        assert!(!report.samples.is_empty());
        let sample = &report.samples[0];
        assert_eq!(sample.segment_id, 42);
        assert_eq!(sample.pane_id, 7);
        assert_eq!(sample.captured_at, 1234567890);
        assert!(sample.match_len > 0);
    }

    #[test]
    fn scan_segment_sample_pattern_name_populated() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(
            1,
            1,
            "export OPENAI_API_KEY=sk-abc1234567890abcdef1234567890abcdef12345678",
        );

        engine.scan_segment(&segment, &options, &mut report);
        assert!(!report.samples.is_empty());
        // Pattern name should be non-empty
        assert!(!report.samples[0].pattern.is_empty());
    }

    // ========================================================================
    // scan_segment: sample_limit enforcement
    // ========================================================================

    #[test]
    fn scan_segment_respects_sample_limit() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions {
            sample_limit: 2,
            ..Default::default()
        };
        let mut report = make_report();

        // Create a segment with many secrets
        let content: String = (0..10).fold(String::new(), |mut acc, i| {
            use std::fmt::Write;
            let _ = writeln!(acc, "password=secret_value_{i:02}xx");
            acc
        });
        let segment = make_segment(1, 1, &content);

        engine.scan_segment(&segment, &options, &mut report);
        assert!(
            report.samples.len() <= 2,
            "samples should be capped at limit: got {}",
            report.samples.len()
        );
        // But total matches can exceed the sample limit
        assert!(
            report.matches_total >= report.samples.len() as u64,
            "matches_total should be >= samples count"
        );
    }

    // ========================================================================
    // scan_segment: matches_by_pattern aggregation
    // ========================================================================

    #[test]
    fn scan_segment_aggregates_matches_by_pattern() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        // Two OpenAI keys
        let content = concat!(
            "KEY1=sk-abc1234567890abcdef1234567890abcdef12345678\n",
            "KEY2=sk-def1234567890abcdef1234567890abcdef12345678\n",
        );
        let segment = make_segment(1, 1, content);

        engine.scan_segment(&segment, &options, &mut report);

        // At minimum the openai_key pattern should have count >= 2
        let openai_count = report.matches_by_pattern.get("openai_key").copied();
        assert!(
            openai_count.unwrap_or(0) >= 2,
            "should count multiple openai keys: {:?}",
            report.matches_by_pattern
        );
    }

    // ========================================================================
    // scan_segment: report byte counting
    // ========================================================================

    #[test]
    fn scan_segment_does_not_update_scanned_bytes_or_segments() {
        // scan_segment is a low-level method; callers increment counters
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let segment = make_segment(
            1,
            1,
            "token: sk-abc1234567890abcdef1234567890abcdef12345678",
        );

        engine.scan_segment(&segment, &options, &mut report);
        // scan_segment only updates matches, not counters
        assert_eq!(
            report.scanned_segments, 0,
            "scan_segment doesn't bump counters"
        );
    }

    // ========================================================================
    // Unicode + long-line inputs
    // ========================================================================

    #[test]
    fn scan_segment_unicode_surrounding_content() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        // Secret embedded in unicode text
        let content = "日本語テキスト password=MyS3cr3tP4ss 中文字符 Ёмоджи 🔑";
        let segment = make_segment(1, 1, content);

        engine.scan_segment(&segment, &options, &mut report);
        assert!(
            report.matches_total > 0,
            "should detect secret in unicode text"
        );
        // Verify no raw secret in hash
        for sample in &report.samples {
            assert_ne!(sample.secret_hash, "MyS3cr3tP4ss");
            assert_eq!(sample.secret_hash.len(), 64);
        }
    }

    #[test]
    fn scan_segment_unicode_in_secret_value_no_panic() {
        // Ensure engine doesn't panic on multibyte boundaries
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        let content = "password=пароль_sécurité_密码!";
        let segment = make_segment(1, 1, content);

        // Should not panic, may or may not match depending on regex
        engine.scan_segment(&segment, &options, &mut report);
    }

    #[test]
    fn scan_segment_long_line_input() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        // 10KB line with a secret buried in the middle
        let prefix = "A".repeat(5_000);
        let suffix = "B".repeat(5_000);
        let secret = "sk-abc1234567890abcdef1234567890abcdef12345678";
        let content = format!("{prefix} {secret} {suffix}");
        let segment = make_segment(1, 1, &content);

        engine.scan_segment(&segment, &options, &mut report);
        assert!(
            report.matches_total > 0,
            "should detect secret in long line"
        );
    }

    #[test]
    fn scan_segment_many_short_lines() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();

        // 1000 lines of clean text with one secret on line 500
        let mut lines: Vec<String> = (0..1000)
            .map(|i| format!("line {i}: normal output"))
            .collect();
        lines[500] = "line 500: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789".to_string();
        let content = lines.join("\n");
        let segment = make_segment(1, 1, &content);

        engine.scan_segment(&segment, &options, &mut report);
        assert!(
            report.matches_total > 0,
            "should detect secret among many lines"
        );
    }

    // ========================================================================
    // scan_storage_from: batch + max_segments logic
    // ========================================================================

    #[tokio::test]
    async fn scan_storage_empty_database() {
        let (storage, db_path) = setup_storage("empty").await;

        let engine = SecretScanEngine::new();
        let report = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan");

        assert_eq!(report.scanned_segments, 0);
        assert_eq!(report.scanned_bytes, 0);
        assert_eq!(report.matches_total, 0);
        assert!(report.samples.is_empty());
        assert_eq!(report.report_version, SECRET_SCAN_REPORT_VERSION);

        teardown(storage, &db_path).await;
    }

    #[tokio::test]
    async fn scan_storage_with_secrets_finds_matches() {
        let (storage, db_path) = setup_storage("matches").await;

        // Insert segments with secrets
        storage
            .append_segment(
                1,
                "export OPENAI_API_KEY=sk-abc1234567890abcdef1234567890abcdef12345678",
                None,
            )
            .await
            .expect("insert segment");
        storage
            .append_segment(1, "Hello, no secrets here.", None)
            .await
            .expect("insert segment");
        storage
            .append_segment(1, "GH_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789", None)
            .await
            .expect("insert segment");

        let engine = SecretScanEngine::new();
        let report = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan");

        assert_eq!(report.scanned_segments, 3);
        assert!(report.matches_total >= 2, "should find at least 2 secrets");
        assert!(!report.samples.is_empty(), "should have sample records");
        // No raw secrets in samples
        for sample in &report.samples {
            assert_eq!(sample.secret_hash.len(), 64);
            assert_ne!(
                sample.secret_hash,
                "sk-abc1234567890abcdef1234567890abcdef12345678"
            );
        }

        teardown(storage, &db_path).await;
    }

    #[tokio::test]
    async fn scan_storage_max_segments_caps_scan() {
        let (storage, db_path) = setup_storage("max").await;

        // Insert 5 segments
        for i in 0..5u64 {
            storage
                .append_segment(1, &format!("content {i}"), None)
                .await
                .expect("insert");
        }

        let engine = SecretScanEngine::new();
        let options = SecretScanOptions {
            max_segments: Some(3),
            batch_size: 2, // force multiple batches
            ..Default::default()
        };
        let report = engine.scan_storage(&storage, options).await.expect("scan");

        assert_eq!(report.scanned_segments, 3, "should stop at max_segments");

        teardown(storage, &db_path).await;
    }

    #[tokio::test]
    async fn scan_storage_zero_batch_size_defaults() {
        let (storage, db_path) = setup_storage("batchzero").await;

        storage
            .append_segment(1, "clean text", None)
            .await
            .expect("insert");

        let engine = SecretScanEngine::new();
        let options = SecretScanOptions {
            batch_size: 0, // should default to 1000
            ..Default::default()
        };
        let report = engine.scan_storage(&storage, options).await.expect("scan");
        assert_eq!(report.scanned_segments, 1);

        teardown(storage, &db_path).await;
    }

    // ========================================================================
    // Incremental scan resume
    // ========================================================================

    #[tokio::test]
    async fn scan_storage_incremental_resumes_from_checkpoint() {
        let (storage, db_path) = setup_storage("incr").await;

        // Insert initial segments
        for i in 0..3u64 {
            storage
                .append_segment(
                    1,
                    &format!("line {i}: sk-abc{i}234567890abcdef1234567890abcdef12345678"),
                    None,
                )
                .await
                .expect("insert");
        }

        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();

        // First incremental scan
        let report1 = engine
            .scan_storage_incremental(&storage, options.clone())
            .await
            .expect("first scan");

        assert_eq!(report1.scanned_segments, 3);
        let first_matches = report1.matches_total;
        assert!(first_matches > 0);

        // Insert 2 more segments
        for i in 3..5u64 {
            storage
                .append_segment(
                    1,
                    &format!("line {i}: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ012345678{i}"),
                    None,
                )
                .await
                .expect("insert");
        }

        // Second incremental scan - should only scan new segments
        let report2 = engine
            .scan_storage_incremental(&storage, options)
            .await
            .expect("second scan");

        assert_eq!(
            report2.scanned_segments, 2,
            "incremental should only scan new segments"
        );
        assert!(
            report2.resume_after_id.is_some(),
            "should have resume point"
        );

        teardown(storage, &db_path).await;
    }

    // ========================================================================
    // Report serialization
    // ========================================================================

    #[test]
    fn report_serializes_to_json() {
        let mut report = make_report();
        report.scanned_segments = 10;
        report.matches_total = 3;
        report
            .matches_by_pattern
            .insert("openai_key".to_string(), 2);
        report
            .matches_by_pattern
            .insert("github_token".to_string(), 1);
        report.samples.push(SecretScanSample {
            pattern: "openai_key".to_string(),
            segment_id: 1,
            pane_id: 1,
            captured_at: 0,
            secret_hash: "abcd".repeat(16),
            match_len: 48,
        });

        let json = serde_json::to_string(&report).expect("serialize");
        let deser: SecretScanReport = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deser.scanned_segments, 10);
        assert_eq!(deser.matches_total, 3);
        assert_eq!(deser.samples.len(), 1);
        assert_eq!(deser.samples[0].pattern, "openai_key");
    }

    #[test]
    fn report_json_does_not_contain_raw_secrets() {
        let engine = SecretScanEngine::new();
        let options = SecretScanOptions::default();
        let mut report = make_report();
        let secret = "sk-abc1234567890abcdef1234567890abcdef12345678";
        let segment = make_segment(1, 1, &format!("key={secret}"));

        engine.scan_segment(&segment, &options, &mut report);

        let json = serde_json::to_string(&report).expect("serialize");
        assert!(!json.contains(secret), "JSON should not contain raw secret");
    }

    // ========================================================================
    // SecretScanEngine::new() defaults
    // ========================================================================

    #[test]
    fn engine_default_is_same_as_new() {
        let e1 = SecretScanEngine::new();
        let e2 = SecretScanEngine::default();
        // Both should produce identical scan results
        let options = SecretScanOptions::default();
        let mut r1 = make_report();
        let mut r2 = make_report();

        let segment = make_segment(
            1,
            1,
            "token: sk-abc1234567890abcdef1234567890abcdef12345678",
        );

        e1.scan_segment(&segment, &options, &mut r1);
        e2.scan_segment(&segment, &options, &mut r2);

        assert_eq!(r1.matches_total, r2.matches_total);
        assert_eq!(r1.samples.len(), r2.samples.len());
    }

    // ========================================================================
    // Pane ID filtering (scan_storage_from behavior)
    // ========================================================================

    #[tokio::test]
    async fn scan_storage_filters_by_pane_id() {
        let (storage, db_path) = setup_storage("pane").await;

        // Also register pane 2 for this test
        storage
            .upsert_pane(make_pane(2))
            .await
            .expect("register pane 2");

        // Pane 1: has secret
        storage
            .append_segment(1, "sk-abc1234567890abcdef1234567890abcdef12345678", None)
            .await
            .expect("insert");

        // Pane 2: has secret
        storage
            .append_segment(2, "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789", None)
            .await
            .expect("insert");

        let engine = SecretScanEngine::new();

        // Scan only pane 1
        let options = SecretScanOptions {
            pane_id: Some(1),
            ..Default::default()
        };
        let report = engine.scan_storage(&storage, options).await.expect("scan");

        // Should only find secrets from pane 1
        for sample in &report.samples {
            assert_eq!(sample.pane_id, 1);
        }

        teardown(storage, &db_path).await;
    }

    // ========================================================================
    // E2E: secret scan (redaction-safe) — bd-3uox
    // ========================================================================

    /// Known secret fixtures used across E2E tests.
    const E2E_SECRETS: &[(&str, &str)] = &[
        ("openai", "sk-abc1234567890abcdef1234567890abcdef12345678"),
        ("anthropic", "sk-ant-api03-XXXXXXXXXXXXXXXXXXXXXXXXXXXX"),
        ("github", "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"),
        ("aws", "AKIAIOSFODNN7EXAMPLE"),
        ("slack", "xoxb-1234567890-abcdefghijklmn"),
        ("stripe", "sk_live_abcdefghijklmnopqrstuvwxyz0123"),
        (
            "database",
            "postgres://admin:s3cretP4ss@db.host.com:5432/mydb",
        ),
    ];

    /// E2E: insert diverse secret patterns as fixtures, run scan, verify
    /// that the report JSON never contains any raw secret value.
    #[tokio::test]
    async fn e2e_fixtures_report_never_contains_raw_secrets() {
        let (storage, db_path) = setup_storage("e2e_fixtures").await;

        // Insert each secret as a distinct segment
        for (label, secret) in E2E_SECRETS {
            storage
                .append_segment(1, &format!("{label}_key={secret}"), None)
                .await
                .expect("insert fixture");
        }

        let engine = SecretScanEngine::new();
        let report = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan");

        // Serialize full report to JSON
        let json = serde_json::to_string_pretty(&report).expect("serialize report");

        // Verify no raw secret appears anywhere in the JSON
        for (_label, secret) in E2E_SECRETS {
            assert!(
                !json.contains(secret),
                "JSON must not contain raw secret: {secret}"
            );
        }

        // Verify scan actually found matches
        assert!(
            report.matches_total >= E2E_SECRETS.len() as u64,
            "should find at least {} secrets, got {}",
            E2E_SECRETS.len(),
            report.matches_total
        );

        // All samples have valid hashes (64-char hex)
        for sample in &report.samples {
            assert_eq!(
                sample.secret_hash.len(),
                64,
                "sample hash should be 64 hex chars"
            );
            assert!(
                sample.secret_hash.chars().all(|c| c.is_ascii_hexdigit()),
                "hash should be hex"
            );
        }

        teardown(storage, &db_path).await;
    }

    /// E2E: incremental scan skips already-scanned segments.
    #[tokio::test]
    async fn e2e_incremental_scan_skips_prior_segments() {
        let (storage, db_path) = setup_storage("e2e_incr").await;

        // Phase 1: insert 3 segments with secrets
        for i in 0..3u64 {
            storage
                .append_segment(
                    1,
                    &format!("phase1-{i}: sk-key{i}234567890abcdef1234567890abcdef12345678"),
                    None,
                )
                .await
                .expect("insert phase1");
        }

        let engine = SecretScanEngine::new();
        let opts = SecretScanOptions::default();

        let r1 = engine
            .scan_storage_incremental(&storage, opts.clone())
            .await
            .expect("scan1");
        assert_eq!(r1.scanned_segments, 3);
        let phase1_matches = r1.matches_total;

        // Phase 2: insert 2 more segments (one clean, one with secret)
        storage
            .append_segment(1, "phase2: clean output", None)
            .await
            .expect("insert phase2 clean");
        storage
            .append_segment(
                1,
                "phase2: ghp_NEWTOKEN123456789012345678901234567890",
                None,
            )
            .await
            .expect("insert phase2 secret");

        let r2 = engine
            .scan_storage_incremental(&storage, opts.clone())
            .await
            .expect("scan2");

        // Should only scan the 2 new segments
        assert_eq!(
            r2.scanned_segments, 2,
            "second scan should only cover new segments"
        );
        // Phase 2 had 1 secret
        assert!(r2.matches_total >= 1, "should find the new secret");

        // Phase 3: no new segments → zero work
        let r3 = engine
            .scan_storage_incremental(&storage, opts)
            .await
            .expect("scan3");
        assert_eq!(r3.scanned_segments, 0, "no new segments means no work");

        // Verify the cumulative coverage
        assert!(
            phase1_matches + r2.matches_total >= 4,
            "total matches across phases should be >= 4"
        );

        teardown(storage, &db_path).await;
    }

    /// E2E: report JSON artifact is deterministic and stable.
    #[tokio::test]
    async fn e2e_report_json_artifact_stable() {
        let (storage, db_path) = setup_storage("e2e_json").await;

        // Insert fixtures
        storage
            .append_segment(
                1,
                "export KEY=sk-abc1234567890abcdef1234567890abcdef12345678",
                None,
            )
            .await
            .expect("insert");
        storage
            .append_segment(1, "GH_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789", None)
            .await
            .expect("insert");

        let engine = SecretScanEngine::new();

        // Run two consecutive scans on the same data
        let r1 = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan1");
        let r2 = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan2");

        // Core metrics should be identical
        assert_eq!(r1.scanned_segments, r2.scanned_segments);
        assert_eq!(r1.scanned_bytes, r2.scanned_bytes);
        assert_eq!(r1.matches_total, r2.matches_total);
        assert_eq!(r1.matches_by_pattern, r2.matches_by_pattern);
        assert_eq!(r1.samples.len(), r2.samples.len());

        // Sample hashes should be identical (deterministic SHA-256)
        for (s1, s2) in r1.samples.iter().zip(r2.samples.iter()) {
            assert_eq!(s1.secret_hash, s2.secret_hash, "hashes should be stable");
            assert_eq!(s1.pattern, s2.pattern);
            assert_eq!(s1.segment_id, s2.segment_id);
            assert_eq!(s1.match_len, s2.match_len);
        }

        // Verify report_version is set
        assert_eq!(r1.report_version, SECRET_SCAN_REPORT_VERSION);

        teardown(storage, &db_path).await;
    }

    /// E2E: multi-pattern segment produces correct per-pattern counts.
    #[tokio::test]
    async fn e2e_multi_pattern_per_segment_counts() {
        let (storage, db_path) = setup_storage("e2e_multi").await;

        // Single segment with multiple secret types
        let content = concat!(
            "OPENAI=sk-abc1234567890abcdef1234567890abcdef12345678 ",
            "GH=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 ",
            "STRIPE=sk_live_abcdefghijklmnopqrstuvwxyz0123 ",
            "DB=postgres://root:hunter2@db:5432/prod",
        );
        storage
            .append_segment(1, content, None)
            .await
            .expect("insert");

        let engine = SecretScanEngine::new();
        let report = engine
            .scan_storage(&storage, SecretScanOptions::default())
            .await
            .expect("scan");

        // Should detect at least 4 different patterns
        assert!(
            report.matches_total >= 4,
            "should find at least 4 secrets in multi-pattern segment, got {}",
            report.matches_total
        );

        // matches_by_pattern should have multiple keys
        assert!(
            report.matches_by_pattern.len() >= 3,
            "should have at least 3 distinct patterns, got {:?}",
            report.matches_by_pattern.keys().collect::<Vec<_>>()
        );

        // Serialize and verify no raw secrets
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(!json.contains("hunter2"), "no raw DB password in JSON");
        assert!(
            !json.contains("sk-abc1234567890"),
            "no raw OpenAI key in JSON"
        );

        teardown(storage, &db_path).await;
    }

    /// E2E: sample_limit is respected across scan_storage (not just scan_segment).
    #[tokio::test]
    async fn e2e_sample_limit_across_storage() {
        let (storage, db_path) = setup_storage("e2e_limit").await;

        // Insert many segments, each with a secret
        for i in 0..20u64 {
            storage
                .append_segment(1, &format!("password=super_secret_pass_{i:03}xx"), None)
                .await
                .expect("insert");
        }

        let engine = SecretScanEngine::new();
        let options = SecretScanOptions {
            sample_limit: 5,
            ..Default::default()
        };
        let report = engine.scan_storage(&storage, options).await.expect("scan");

        assert_eq!(report.scanned_segments, 20);
        assert!(report.matches_total >= 20, "should count all matches");
        assert!(
            report.samples.len() <= 5,
            "samples should be capped at limit: got {}",
            report.samples.len()
        );

        teardown(storage, &db_path).await;
    }
}
