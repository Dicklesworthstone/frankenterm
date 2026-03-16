//! Incremental document indexing pipeline for FrankenSearch-backed retrieval.
//!
//! This module focuses on deterministic, low-overhead indexing primitives:
//! - Scrollback chunk extraction (blank-line/time-gap boundaries)
//! - Command-output block extraction (prompt/OSC-133 boundaries)
//! - Agent artifact extraction (errors/tool traces/code blocks)
//! - Content-hash deduplication (SHA-256 on normalized text)
//! - Batched persistence with flush-by-doc-count or flush-by-interval
//! - TTL expiry + size-bound LRU eviction
//! - Optional cass hash dedup hook

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use thiserror::Error;

const INDEX_STATE_FILENAME: &str = "index_state.json";
const INDEX_FORMAT_VERSION: u32 = 1;
const ONE_DAY_MS: i64 = 86_400_000;
const DEFAULT_PROMPT_PATTERN: &str = r"^\s*[\w\-./:@~]+\s*[$#>]\s*$";

static DEFAULT_PROMPT_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(DEFAULT_PROMPT_PATTERN).unwrap());

/// Logical document source used for filtering and diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchDocumentSource {
    Scrollback,
    Command,
    AgentArtifact,
    PaneMetadata,
    Cass,
}

impl SearchDocumentSource {
    /// Stable source tag string.
    #[must_use]
    pub fn as_tag(self) -> &'static str {
        match self {
            Self::Scrollback => "scrollback",
            Self::Command => "command",
            Self::AgentArtifact => "agent",
            Self::PaneMetadata => "pane_metadata",
            Self::Cass => "cass",
        }
    }
}

/// Candidate line from terminal scrollback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScrollbackLine {
    pub text: String,
    pub captured_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

impl ScrollbackLine {
    /// Convenience constructor for tests/callers.
    #[must_use]
    pub fn new(text: impl Into<String>, captured_at_ms: i64) -> Self {
        Self {
            text: text.into(),
            captured_at_ms,
            pane_id: None,
            session_id: None,
        }
    }
}

/// Extracted unit ready for indexing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexableDocument {
    pub source: SearchDocumentSource,
    pub text: String,
    pub captured_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

impl IndexableDocument {
    /// Build a text-bearing indexable document.
    #[must_use]
    pub fn text(
        source: SearchDocumentSource,
        text: impl Into<String>,
        captured_at_ms: i64,
        pane_id: Option<u64>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            source,
            text: text.into(),
            captured_at_ms,
            pane_id,
            session_id,
            metadata: serde_json::Value::Null,
        }
    }

    fn dedupe_basis(&self) -> String {
        if self.source == SearchDocumentSource::PaneMetadata {
            let metadata = serde_json::to_string(&self.metadata).unwrap_or_else(|e| {
                tracing::warn!(error = %e, "search document metadata serialization failed");
                String::new()
            });
            normalize_index_text(&metadata)
        } else {
            normalize_index_text(&self.text)
        }
    }
}

/// Persisted document row in the index state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedDocument {
    pub id: u64,
    pub source: SearchDocumentSource,
    pub source_tag: String,
    pub content_hash: String,
    pub text: String,
    pub captured_at_ms: i64,
    pub indexed_at_ms: i64,
    pub last_accessed_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub size_bytes: u64,
}

/// Index flush trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexFlushReason {
    DocThreshold,
    Interval,
    Manual,
    Reindex,
}

/// Ingestion report for one call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingIngestReport {
    pub submitted_docs: usize,
    pub accepted_docs: usize,
    pub skipped_empty_docs: usize,
    pub skipped_duplicate_docs: usize,
    pub skipped_cass_docs: usize,
    pub skipped_resize_pause_docs: usize,
    pub deferred_rate_limited_docs: usize,
    pub flushed_docs: usize,
    pub expired_docs: usize,
    pub evicted_docs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flush_reason: Option<IndexFlushReason>,
}

/// Tick result for timer-driven flush checks.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingTickResult {
    pub flushed_docs: usize,
    pub expired_docs: usize,
    pub evicted_docs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flush_reason: Option<IndexFlushReason>,
}

/// On-disk index health/size/freshness snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchIndexStats {
    pub index_dir: String,
    pub state_path: String,
    pub format_version: u32,
    pub doc_count: usize,
    pub segment_count: usize,
    pub total_bytes: u64,
    pub pending_docs: usize,
    pub max_index_size_bytes: u64,
    pub ttl_days: u64,
    pub flush_interval_secs: u64,
    pub flush_docs_threshold: usize,
    pub newest_captured_at_ms: Option<i64>,
    pub oldest_captured_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness_age_ms: Option<i64>,
    pub source_counts: HashMap<String, usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_flush_at_ms: Option<i64>,
}

/// Runtime controls for the index lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexingConfig {
    pub index_dir: PathBuf,
    pub max_index_size_bytes: u64,
    pub ttl_days: u64,
    pub flush_interval_secs: u64,
    pub flush_docs_threshold: usize,
    pub max_docs_per_second: u32,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            index_dir: resolve_index_dir("~/.ft/search_index"),
            max_index_size_bytes: 500 * 1024 * 1024,
            ttl_days: 30,
            flush_interval_secs: 5,
            flush_docs_threshold: 50,
            max_docs_per_second: 100,
        }
    }
}

/// Optional cass content-hash lookup used to suppress duplicate indexing.
pub trait CassContentHashProvider {
    /// Return `true` when cass already contains this normalized content hash.
    fn contains_content_hash(&self, content_hash: &str) -> bool;
}

impl<S: std::hash::BuildHasher> CassContentHashProvider for HashSet<String, S> {
    fn contains_content_hash(&self, content_hash: &str) -> bool {
        self.contains(content_hash)
    }
}

/// Errors produced by search indexing operations.
#[derive(Debug, Error)]
pub enum SearchIndexError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("invalid indexing config: {0}")]
    InvalidConfig(String),
    #[error("unsupported index state version: {0}")]
    UnsupportedStateVersion(u32),
}

pub type Result<T> = std::result::Result<T, SearchIndexError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedIndexState {
    format_version: u32,
    next_document_id: u64,
    documents: Vec<IndexedDocument>,
    last_flush_at_ms: Option<i64>,
}

impl Default for PersistedIndexState {
    fn default() -> Self {
        Self {
            format_version: INDEX_FORMAT_VERSION,
            next_document_id: 1,
            documents: Vec::new(),
            last_flush_at_ms: None,
        }
    }
}

#[derive(Debug, Clone)]
struct FlushOutcome {
    flushed_docs: usize,
    expired_docs: usize,
    evicted_docs: usize,
    reason: IndexFlushReason,
}

/// Persistent incremental index with dedup + lifecycle policies.
pub struct SearchIndex {
    config: IndexingConfig,
    state_path: PathBuf,
    state: PersistedIndexState,
    pending: Vec<IndexedDocument>,
    known_hashes: HashSet<String>,
    rate_window_started_ms: i64,
    rate_window_docs: u32,
}

impl SearchIndex {
    /// Open or create an index at `config.index_dir`.
    pub fn open(config: IndexingConfig) -> Result<Self> {
        validate_config(&config)?;
        fs::create_dir_all(&config.index_dir)?;
        let state_path = config.index_dir.join(INDEX_STATE_FILENAME);

        let state = if state_path.exists() {
            let raw = fs::read_to_string(&state_path)?;
            let parsed: PersistedIndexState = serde_json::from_str(&raw)?;
            if parsed.format_version != INDEX_FORMAT_VERSION {
                return Err(SearchIndexError::UnsupportedStateVersion(
                    parsed.format_version,
                ));
            }
            parsed
        } else {
            PersistedIndexState::default()
        };

        let known_hashes = state
            .documents
            .iter()
            .map(|doc| doc.content_hash.clone())
            .collect::<HashSet<_>>();

        Ok(Self {
            config,
            state_path,
            state,
            pending: Vec::new(),
            known_hashes,
            rate_window_started_ms: 0,
            rate_window_docs: 0,
        })
    }

    /// Path to the index state file.
    #[must_use]
    pub fn state_path(&self) -> &Path {
        &self.state_path
    }

    /// Current indexing config.
    #[must_use]
    pub fn config(&self) -> &IndexingConfig {
        &self.config
    }

    /// Current indexed documents.
    #[must_use]
    pub fn documents(&self) -> &[IndexedDocument] {
        &self.state.documents
    }

    /// Number of pending (unflushed) documents.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Ingest extracted documents incrementally.
    ///
    /// When `resize_storm_active` is `true`, ingestion yields and does no work.
    pub fn ingest_documents(
        &mut self,
        docs: &[IndexableDocument],
        now_ms: i64,
        resize_storm_active: bool,
        cass_hashes: Option<&dyn CassContentHashProvider>,
    ) -> Result<IndexingIngestReport> {
        let mut report = IndexingIngestReport {
            submitted_docs: docs.len(),
            ..IndexingIngestReport::default()
        };

        if resize_storm_active {
            report.skipped_resize_pause_docs = docs.len();
            return Ok(report);
        }

        for doc in docs {
            self.reset_rate_window_if_needed(now_ms);
            if self.rate_window_docs >= self.config.max_docs_per_second {
                report.deferred_rate_limited_docs += 1;
                continue;
            }

            let basis = doc.dedupe_basis();
            if basis.is_empty() {
                report.skipped_empty_docs += 1;
                continue;
            }

            let content_hash = sha256_hex(basis.as_bytes());
            let duplicate_pending = self.pending.iter().any(|p| p.content_hash == content_hash);
            if self.known_hashes.contains(&content_hash) || duplicate_pending {
                report.skipped_duplicate_docs += 1;
                continue;
            }

            if cass_hashes.is_some_and(|provider| provider.contains_content_hash(&content_hash)) {
                report.skipped_cass_docs += 1;
                continue;
            }

            let indexed_doc = self.build_indexed_document(doc, now_ms, content_hash);
            self.pending.push(indexed_doc);
            self.rate_window_docs = self.rate_window_docs.saturating_add(1);
            report.accepted_docs += 1;
        }

        if let Some(outcome) = self.flush_if_due(now_ms)? {
            report.flushed_docs = outcome.flushed_docs;
            report.expired_docs = outcome.expired_docs;
            report.evicted_docs = outcome.evicted_docs;
            report.flush_reason = Some(outcome.reason);
        }

        Ok(report)
    }

    /// Trigger interval-based flush checks and maintenance.
    ///
    /// Returns no-op when paused by resize pressure.
    pub fn tick(&mut self, now_ms: i64, resize_storm_active: bool) -> Result<IndexingTickResult> {
        if resize_storm_active {
            return Ok(IndexingTickResult::default());
        }

        let Some(outcome) = self.flush_if_due_by_interval(now_ms)? else {
            return Ok(IndexingTickResult::default());
        };

        Ok(IndexingTickResult {
            flushed_docs: outcome.flushed_docs,
            expired_docs: outcome.expired_docs,
            evicted_docs: outcome.evicted_docs,
            flush_reason: Some(outcome.reason),
        })
    }

    /// Force flush any pending docs.
    pub fn flush_now(
        &mut self,
        now_ms: i64,
        reason: IndexFlushReason,
    ) -> Result<IndexingTickResult> {
        if self.pending.is_empty() && !self.maintenance_due(now_ms) {
            return Ok(IndexingTickResult::default());
        }
        let outcome = self.flush_pending(now_ms, reason)?;
        Ok(IndexingTickResult {
            flushed_docs: outcome.flushed_docs,
            expired_docs: outcome.expired_docs,
            evicted_docs: outcome.evicted_docs,
            flush_reason: Some(outcome.reason),
        })
    }

    /// Manual full reindex from a supplied document stream.
    pub fn reindex_documents(
        &mut self,
        docs: &[IndexableDocument],
        now_ms: i64,
        cass_hashes: Option<&dyn CassContentHashProvider>,
    ) -> Result<IndexingIngestReport> {
        let original_state = self.state.clone();
        let original_pending = self.pending.clone();
        let original_known_hashes = self.known_hashes.clone();
        let original_rate_window_started_ms = self.rate_window_started_ms;
        let original_rate_window_docs = self.rate_window_docs;

        self.state.documents.clear();
        self.pending.clear();
        self.known_hashes.clear();
        self.state.next_document_id = 1;
        self.state.last_flush_at_ms = None;
        self.rate_window_started_ms = 0;
        self.rate_window_docs = 0;

        // Reindex should not be constrained by per-second throttle.
        let original_limit = std::mem::replace(&mut self.config.max_docs_per_second, u32::MAX);
        let reindex_result = (|| {
            let mut report = self.ingest_documents(docs, now_ms, false, cass_hashes)?;
            let flush = self.flush_now(now_ms, IndexFlushReason::Reindex)?;
            report.flushed_docs = report.flushed_docs.saturating_add(flush.flushed_docs);
            report.expired_docs = report.expired_docs.saturating_add(flush.expired_docs);
            report.evicted_docs = report.evicted_docs.saturating_add(flush.evicted_docs);
            report.flush_reason = flush.flush_reason;
            Ok(report)
        })();
        self.config.max_docs_per_second = original_limit;

        match reindex_result {
            Ok(report) => Ok(report),
            Err(err) => {
                self.state = original_state;
                self.pending = original_pending;
                self.known_hashes = original_known_hashes;
                self.rate_window_started_ms = original_rate_window_started_ms;
                self.rate_window_docs = original_rate_window_docs;
                Err(err)
            }
        }
    }

    /// Search indexed documents with simple normalized substring matching.
    pub fn search(&mut self, query: &str, limit: usize, now_ms: i64) -> Vec<IndexedDocument> {
        if limit == 0 {
            return Vec::new();
        }
        let normalized_query = normalize_index_text(query).to_ascii_lowercase();
        if normalized_query.is_empty() {
            return Vec::new();
        }

        let mut ranked: Vec<(usize, i64, u64)> = Vec::new();
        for doc in &mut self.state.documents {
            if doc.text.is_empty() {
                continue;
            }
            let haystack = doc.text.to_ascii_lowercase();
            if haystack.contains(&normalized_query) {
                doc.last_accessed_at_ms = now_ms;
                let occurrences = haystack.matches(&normalized_query).count();
                ranked.push((occurrences, doc.captured_at_ms, doc.id));
            }
        }

        ranked.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| left.2.cmp(&right.2))
        });

        let ids: HashSet<u64> = ranked
            .into_iter()
            .take(limit)
            .map(|(_, _, id)| id)
            .collect();
        let mut output = self
            .state
            .documents
            .iter()
            .filter(|doc| ids.contains(&doc.id))
            .cloned()
            .collect::<Vec<_>>();
        output.sort_by(|left, right| {
            right
                .captured_at_ms
                .cmp(&left.captured_at_ms)
                .then_with(|| left.id.cmp(&right.id))
        });

        if !output.is_empty() {
            let _ = self.persist_state();
        }
        output
    }

    /// Build a health snapshot for robot-mode status surfaces.
    #[must_use]
    pub fn stats(&self, now_ms: i64) -> SearchIndexStats {
        let mut source_counts: HashMap<String, usize> = HashMap::new();
        let mut newest: Option<i64> = None;
        let mut oldest: Option<i64> = None;
        for doc in &self.state.documents {
            *source_counts.entry(doc.source_tag.clone()).or_insert(0) += 1;
            newest = Some(newest.map_or(doc.captured_at_ms, |v| v.max(doc.captured_at_ms)));
            oldest = Some(oldest.map_or(doc.captured_at_ms, |v| v.min(doc.captured_at_ms)));
        }

        SearchIndexStats {
            index_dir: self.config.index_dir.to_string_lossy().to_string(),
            state_path: self.state_path.to_string_lossy().to_string(),
            format_version: self.state.format_version,
            doc_count: self.state.documents.len(),
            segment_count: self.state.documents.len(),
            total_bytes: documents_total_size(&self.state.documents),
            pending_docs: self.pending.len(),
            max_index_size_bytes: self.config.max_index_size_bytes,
            ttl_days: self.config.ttl_days,
            flush_interval_secs: self.config.flush_interval_secs,
            flush_docs_threshold: self.config.flush_docs_threshold,
            newest_captured_at_ms: newest,
            oldest_captured_at_ms: oldest,
            freshness_age_ms: newest.map(|ts| now_ms.saturating_sub(ts)),
            source_counts,
            last_flush_at_ms: self.state.last_flush_at_ms,
        }
    }

    fn build_indexed_document(
        &mut self,
        doc: &IndexableDocument,
        now_ms: i64,
        content_hash: String,
    ) -> IndexedDocument {
        let normalized_text = normalize_index_text(&doc.text);
        let source_tag = doc.source.as_tag().to_string();
        let id = self.state.next_document_id;
        self.state.next_document_id = self.state.next_document_id.saturating_add(1);

        let mut indexed = IndexedDocument {
            id,
            source: doc.source,
            source_tag,
            content_hash,
            text: normalized_text,
            captured_at_ms: doc.captured_at_ms,
            indexed_at_ms: now_ms,
            last_accessed_at_ms: now_ms,
            pane_id: doc.pane_id,
            session_id: doc.session_id.clone(),
            metadata: doc.metadata.clone(),
            size_bytes: 0,
        };
        indexed.size_bytes = estimate_document_size(&indexed);
        indexed
    }

    fn flush_if_due(&mut self, now_ms: i64) -> Result<Option<FlushOutcome>> {
        if self.pending.len() >= self.config.flush_docs_threshold {
            return self
                .flush_pending(now_ms, IndexFlushReason::DocThreshold)
                .map(Some);
        }
        self.flush_if_due_by_interval(now_ms)
    }

    fn flush_if_due_by_interval(&mut self, now_ms: i64) -> Result<Option<FlushOutcome>> {
        if self.pending.is_empty() && !self.maintenance_due(now_ms) {
            return Ok(None);
        }
        let interval_ms = i64::try_from(self.config.flush_interval_secs)
            .unwrap_or(i64::MAX)
            .saturating_mul(1000);
        let baseline_ms = self
            .state
            .last_flush_at_ms
            .or_else(|| self.pending.first().map(|doc| doc.indexed_at_ms))
            .unwrap_or(now_ms);
        if now_ms.saturating_sub(baseline_ms) >= interval_ms || self.maintenance_due(now_ms) {
            return self
                .flush_pending(now_ms, IndexFlushReason::Interval)
                .map(Some);
        }
        Ok(None)
    }

    fn maintenance_due(&self, now_ms: i64) -> bool {
        let Some(last_flush_at_ms) = self.state.last_flush_at_ms else {
            return false;
        };
        let interval_ms = i64::try_from(self.config.flush_interval_secs)
            .unwrap_or(i64::MAX)
            .saturating_mul(1000);
        now_ms.saturating_sub(last_flush_at_ms) >= interval_ms
    }

    fn flush_pending(&mut self, now_ms: i64, reason: IndexFlushReason) -> Result<FlushOutcome> {
        let flushed_docs = self.pending.len();
        if flushed_docs > 0 {
            self.state.documents.append(&mut self.pending);
        }

        self.state.last_flush_at_ms = Some(now_ms);
        let (expired_docs, evicted_docs) = self.apply_maintenance(now_ms);
        self.persist_state()?;

        Ok(FlushOutcome {
            flushed_docs,
            expired_docs,
            evicted_docs,
            reason,
        })
    }

    fn apply_maintenance(&mut self, now_ms: i64) -> (usize, usize) {
        let expired_docs = if self.config.ttl_days > 0 {
            let ttl_days = i64::try_from(self.config.ttl_days).unwrap_or(i64::MAX);
            let cutoff = now_ms.saturating_sub(ttl_days.saturating_mul(ONE_DAY_MS));
            let before = self.state.documents.len();
            self.state
                .documents
                .retain(|doc| doc.captured_at_ms >= cutoff);
            before.saturating_sub(self.state.documents.len())
        } else {
            0usize
        };

        let mut evicted_docs = 0usize;
        if self.config.max_index_size_bytes > 0 {
            let mut total = documents_total_size(&self.state.documents);
            while total > self.config.max_index_size_bytes && !self.state.documents.is_empty() {
                let eviction_idx = self
                    .state
                    .documents
                    .iter()
                    .enumerate()
                    .min_by(|left, right| {
                        left.1
                            .last_accessed_at_ms
                            .cmp(&right.1.last_accessed_at_ms)
                            .then_with(|| left.1.captured_at_ms.cmp(&right.1.captured_at_ms))
                            .then_with(|| left.1.id.cmp(&right.1.id))
                    })
                    .map(|(idx, _)| idx)
                    .unwrap_or(0);
                let removed = self.state.documents.remove(eviction_idx);
                total = total.saturating_sub(removed.size_bytes);
                evicted_docs = evicted_docs.saturating_add(1);
            }
        }

        self.known_hashes = self
            .state
            .documents
            .iter()
            .map(|doc| doc.content_hash.clone())
            .collect();

        (expired_docs, evicted_docs)
    }

    fn persist_state(&self) -> Result<()> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let raw = serde_json::to_vec_pretty(&self.state)?;
        let tmp = self.state_path.with_extension("json.tmp");
        fs::write(&tmp, raw)?;
        fs::rename(tmp, &self.state_path)?;
        Ok(())
    }

    fn reset_rate_window_if_needed(&mut self, now_ms: i64) {
        if self.rate_window_started_ms == 0
            || now_ms < self.rate_window_started_ms
            || now_ms.saturating_sub(self.rate_window_started_ms) >= 1000
        {
            self.rate_window_started_ms = now_ms;
            self.rate_window_docs = 0;
        }
    }
}

fn validate_config(config: &IndexingConfig) -> Result<()> {
    if config.flush_docs_threshold == 0 {
        return Err(SearchIndexError::InvalidConfig(
            "flush_docs_threshold must be >= 1".to_string(),
        ));
    }
    if config.flush_interval_secs == 0 {
        return Err(SearchIndexError::InvalidConfig(
            "flush_interval_secs must be >= 1".to_string(),
        ));
    }
    if config.max_docs_per_second == 0 {
        return Err(SearchIndexError::InvalidConfig(
            "max_docs_per_second must be >= 1".to_string(),
        ));
    }
    if config.index_dir.as_os_str().is_empty() {
        return Err(SearchIndexError::InvalidConfig(
            "index_dir cannot be empty".to_string(),
        ));
    }
    Ok(())
}

fn documents_total_size(docs: &[IndexedDocument]) -> u64 {
    docs.iter().map(|doc| doc.size_bytes).sum()
}

fn estimate_document_size(doc: &IndexedDocument) -> u64 {
    let metadata_len = serde_json::to_vec(&doc.metadata)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    let total = doc
        .text
        .len()
        .saturating_add(doc.content_hash.len())
        .saturating_add(doc.source_tag.len())
        .saturating_add(metadata_len)
        .saturating_add(96);
    u64::try_from(total).unwrap_or(u64::MAX)
}

fn resolve_index_dir(raw: &str) -> PathBuf {
    let trimmed = raw.trim();
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(stripped) = trimmed.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(trimmed)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn strip_ansi_sequences(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            match chars.peek().copied() {
                Some('[') => {
                    let _ = chars.next();
                    // CSI: consume until final byte in '@'..='~'.
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                    continue;
                }
                Some(']') => {
                    let _ = chars.next();
                    // OSC: terminated by BEL or ST (ESC \)
                    loop {
                        match chars.next() {
                            Some('\u{7}') => break,
                            Some('\u{1b}') => {
                                if chars.peek().is_some_and(|next| *next == '\\') {
                                    let _ = chars.next();
                                    break;
                                }
                            }
                            Some(_) => {}
                            None => break,
                        }
                    }
                    continue;
                }
                _ => continue,
            }
        }
        out.push(ch);
    }

    out
}

fn normalize_index_text(text: &str) -> String {
    let stripped = strip_ansi_sequences(text);
    let mut normalized_lines = Vec::new();
    let mut prior_blank = false;

    for line in stripped.lines() {
        let compact = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if compact.is_empty() {
            if !prior_blank {
                normalized_lines.push(String::new());
            }
            prior_blank = true;
            continue;
        }

        prior_blank = false;
        normalized_lines.push(compact);
    }

    normalized_lines.join("\n").trim().to_string()
}

/// Build scrollback chunks split by blank-line boundaries and time gaps.
#[must_use]
pub fn chunk_scrollback_lines(lines: &[ScrollbackLine], gap_ms: i64) -> Vec<IndexableDocument> {
    if lines.is_empty() {
        return Vec::new();
    }

    let mut docs = Vec::new();
    let mut current_lines: Vec<String> = Vec::new();
    let mut current_pane: Option<u64> = None;
    let mut current_session: Option<String> = None;
    let mut last_ts: Option<i64> = None;

    let flush_current = |docs: &mut Vec<IndexableDocument>,
                         current_lines: &mut Vec<String>,
                         current_pane: Option<u64>,
                         current_session: Option<String>,
                         last_ts: Option<i64>| {
        if current_lines.is_empty() {
            return;
        }
        let text = current_lines.join("\n");
        docs.push(IndexableDocument::text(
            SearchDocumentSource::Scrollback,
            text,
            last_ts.unwrap_or_default(),
            current_pane,
            current_session,
        ));
        current_lines.clear();
    };

    for line in lines {
        let normalized = normalize_index_text(&line.text);
        let blank_boundary = normalized.is_empty();
        let gap_boundary =
            last_ts.is_some_and(|ts| gap_ms > 0 && line.captured_at_ms.saturating_sub(ts) > gap_ms);

        if blank_boundary || gap_boundary {
            flush_current(
                &mut docs,
                &mut current_lines,
                current_pane,
                current_session.clone(),
                last_ts,
            );
            current_pane = line.pane_id;
            current_session.clone_from(&line.session_id);
            last_ts = Some(line.captured_at_ms);
            // For blank-boundary lines the line itself is whitespace/empty and
            // should not be included in the next chunk.  For gap-boundary lines
            // the triggering line is real content and must start the new chunk.
            if blank_boundary {
                continue;
            }
            // Fall through so the gap-triggering line is appended below.
        }

        if current_lines.is_empty() {
            current_pane = line.pane_id;
            current_session.clone_from(&line.session_id);
        }
        current_lines.push(line.text.clone());
        last_ts = Some(line.captured_at_ms);
    }

    flush_current(
        &mut docs,
        &mut current_lines,
        current_pane,
        current_session,
        last_ts,
    );
    docs
}

/// Prompt boundary extraction configuration for command output blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandBlockExtractionConfig {
    /// Regex used to identify prompt lines.
    pub prompt_pattern: String,
    /// Time gap forcing command block split.
    pub gap_ms: i64,
}

impl Default for CommandBlockExtractionConfig {
    fn default() -> Self {
        Self {
            prompt_pattern: DEFAULT_PROMPT_PATTERN.to_string(),
            gap_ms: 30_000,
        }
    }
}

/// Extract command output blocks delimited by prompt patterns or OSC 133 markers.
#[must_use]
pub fn extract_command_output_blocks(
    lines: &[ScrollbackLine],
    config: &CommandBlockExtractionConfig,
) -> Vec<IndexableDocument> {
    if lines.is_empty() {
        return Vec::new();
    }

    let prompt_regex = if config.prompt_pattern == DEFAULT_PROMPT_PATTERN {
        Some(DEFAULT_PROMPT_REGEX.clone())
    } else {
        thread_local! {
            static CUSTOM_PROMPT_CACHE: std::cell::RefCell<crate::lru_cache::LruCache<String, Regex>> =
                std::cell::RefCell::new(crate::lru_cache::LruCache::new(16));
        }
        CUSTOM_PROMPT_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if let Some(re) = cache.get(&config.prompt_pattern) {
                Some(re.clone())
            } else if let Ok(re) = Regex::new(&config.prompt_pattern) {
                cache.put(config.prompt_pattern.clone(), re.clone());
                Some(re)
            } else {
                Some(DEFAULT_PROMPT_REGEX.clone())
            }
        })
    };
    let mut docs = Vec::new();
    let mut current_lines: Vec<String> = Vec::new();
    let mut current_pane: Option<u64> = None;
    let mut current_session: Option<String> = None;
    let mut last_ts: Option<i64> = None;
    let mut saw_prompt_boundary = false;

    let flush_current = |docs: &mut Vec<IndexableDocument>,
                         current_lines: &mut Vec<String>,
                         current_pane: Option<u64>,
                         current_session: Option<String>,
                         last_ts: Option<i64>| {
        if current_lines.is_empty() {
            return;
        }
        docs.push(IndexableDocument::text(
            SearchDocumentSource::Command,
            current_lines.join("\n"),
            last_ts.unwrap_or_default(),
            current_pane,
            current_session,
        ));
        current_lines.clear();
    };

    for line in lines {
        let normalized = normalize_index_text(&line.text);
        let prompt_boundary =
            is_prompt_boundary(&line.text, normalized.as_str(), prompt_regex.as_ref());
        let gap_boundary = last_ts.is_some_and(|ts| {
            config.gap_ms > 0 && line.captured_at_ms.saturating_sub(ts) > config.gap_ms
        });

        if prompt_boundary || gap_boundary {
            flush_current(
                &mut docs,
                &mut current_lines,
                current_pane,
                current_session.clone(),
                last_ts,
            );
            current_pane = line.pane_id;
            current_session.clone_from(&line.session_id);
            last_ts = Some(line.captured_at_ms);
            if prompt_boundary {
                saw_prompt_boundary = true;
            }
            continue;
        }

        if current_lines.is_empty() {
            current_pane = line.pane_id;
            current_session.clone_from(&line.session_id);
        }
        current_lines.push(line.text.clone());
        last_ts = Some(line.captured_at_ms);
    }

    flush_current(
        &mut docs,
        &mut current_lines,
        current_pane,
        current_session,
        last_ts,
    );

    if saw_prompt_boundary {
        docs
    } else {
        // Without prompt boundaries, treat all non-empty output as one command block.
        let fallback = lines
            .iter()
            .filter_map(|line| {
                let normalized = normalize_index_text(&line.text);
                (!normalized.is_empty()).then_some(line.text.clone())
            })
            .collect::<Vec<_>>();
        if fallback.is_empty() {
            Vec::new()
        } else {
            vec![IndexableDocument::text(
                SearchDocumentSource::Command,
                fallback.join("\n"),
                lines.last().map_or(0, |line| line.captured_at_ms),
                lines.last().and_then(|line| line.pane_id),
                lines.last().and_then(|line| line.session_id.clone()),
            )]
        }
    }
}

fn is_prompt_boundary(raw: &str, normalized: &str, prompt_regex: Option<&Regex>) -> bool {
    if raw.contains("\u{1b}]133;A") || raw.contains("\u{1b}]133;B") || raw.contains("\u{1b}]133;C")
    {
        return true;
    }
    prompt_regex.is_some_and(|regex| regex.is_match(normalized))
}

/// Extract agent-centric artifacts (errors/tool traces/code blocks) from text.
#[must_use]
pub fn extract_agent_artifacts(
    text: &str,
    captured_at_ms: i64,
    pane_id: Option<u64>,
    session_id: Option<String>,
) -> Vec<IndexableDocument> {
    let mut artifacts = Vec::new();

    // 1) Fenced code blocks
    let mut in_code = false;
    let mut code_lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_code {
                if !code_lines.is_empty() {
                    artifacts.push(IndexableDocument {
                        source: SearchDocumentSource::AgentArtifact,
                        text: code_lines.join("\n"),
                        captured_at_ms,
                        pane_id,
                        session_id: session_id.clone(),
                        metadata: serde_json::json!({ "artifact_kind": "code_block" }),
                    });
                    code_lines.clear();
                }
                in_code = false;
            } else {
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_lines.push(line.to_string());
        }
    }

    // 2) Error/tool trace lines
    for line in text.lines() {
        let normalized = normalize_index_text(line);
        if normalized.is_empty() {
            continue;
        }
        let lower = normalized.to_ascii_lowercase();
        let is_error = lower.contains("error:")
            || lower.contains("exception")
            || lower.contains("traceback")
            || lower.contains("panic");
        let is_tool = lower.contains("tool call")
            || lower.contains("tool result")
            || lower.contains("function call");
        if is_error || is_tool {
            artifacts.push(IndexableDocument {
                source: SearchDocumentSource::AgentArtifact,
                text: normalized,
                captured_at_ms,
                pane_id,
                session_id: session_id.clone(),
                metadata: serde_json::json!({
                    "artifact_kind": if is_error { "error" } else { "tool" }
                }),
            });
        }
    }

    // deterministic dedup by content hash
    let mut seen = HashSet::new();
    artifacts.retain(|doc| {
        let hash = sha256_hex(doc.dedupe_basis().as_bytes());
        seen.insert(hash)
    });
    artifacts
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Instant;

    use tempfile::tempdir;

    fn log_phase(test_name: &str, phase: &str, started: Instant, result: &str) {
        tracing::info!(
            test_name,
            phase,
            duration_ms = started.elapsed().as_millis() as u64,
            result,
            "search_indexing_test"
        );
    }

    fn make_config(dir: &Path) -> IndexingConfig {
        IndexingConfig {
            index_dir: dir.join("index"),
            max_index_size_bytes: 500 * 1024 * 1024,
            ttl_days: 30,
            flush_interval_secs: 5,
            flush_docs_threshold: 50,
            max_docs_per_second: 100,
        }
    }

    fn make_doc(text: &str, ts: i64, source: SearchDocumentSource) -> IndexableDocument {
        IndexableDocument::text(source, text, ts, Some(1), Some("sess-1".to_string()))
    }

    #[test]
    fn test_scrollback_indexing() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let scrollback = vec![
            ScrollbackLine::new("build started", 1_000),
            ScrollbackLine::new("compilation failed with E0425", 1_100),
        ];
        let docs = chunk_scrollback_lines(&scrollback, 5_000);
        let report = index
            .ingest_documents(&docs, 1_150, false, None)
            .expect("ingest");
        assert_eq!(report.accepted_docs, 1);
        assert_eq!(report.flushed_docs, 1);

        let hits = index.search("E0425", 5, 1_200);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].source, SearchDocumentSource::Scrollback);
        log_phase("test_scrollback_indexing", "assert", started, "pass");
    }

    #[test]
    fn test_content_hash_dedup() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc("same content", 1_000, SearchDocumentSource::Scrollback);
        let first = index
            .ingest_documents(std::slice::from_ref(&doc), 1_000, false, None)
            .expect("first ingest");
        let second = index
            .ingest_documents(std::slice::from_ref(&doc), 1_100, false, None)
            .expect("second ingest");

        assert_eq!(first.accepted_docs, 1);
        assert_eq!(second.accepted_docs, 0);
        assert_eq!(second.skipped_duplicate_docs, 1);
        assert_eq!(index.documents().len(), 1);
        log_phase("test_content_hash_dedup", "assert", started, "pass");
    }

    #[test]
    fn test_incremental_update() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 2;
        let mut index = SearchIndex::open(cfg).expect("open");

        let a = make_doc("alpha", 1_000, SearchDocumentSource::Scrollback);
        let b = make_doc("beta", 1_100, SearchDocumentSource::Scrollback);
        let c = make_doc("gamma", 1_200, SearchDocumentSource::Scrollback);

        let _ = index
            .ingest_documents(&[a.clone(), b.clone()], 1_150, false, None)
            .expect("initial ingest");
        assert_eq!(index.documents().len(), 2);

        let update = index
            .ingest_documents(&[a, c], 1_250, false, None)
            .expect("delta ingest");
        let flush = index
            .flush_now(1_260, IndexFlushReason::Manual)
            .expect("flush pending");
        assert_eq!(update.accepted_docs, 1);
        assert_eq!(update.skipped_duplicate_docs, 1);
        assert_eq!(flush.flushed_docs, 1);
        assert_eq!(index.documents().len(), 3);
        log_phase("test_incremental_update", "assert", started, "pass");
    }

    #[test]
    fn test_ttl_expiration() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.ttl_days = 1;
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let now = 10 * ONE_DAY_MS;
        let stale = make_doc(
            "old",
            now - (2 * ONE_DAY_MS),
            SearchDocumentSource::Scrollback,
        );
        let fresh = make_doc("new", now - 1_000, SearchDocumentSource::Scrollback);

        let ingest = index
            .ingest_documents(&[stale, fresh], now, false, None)
            .expect("ingest");
        let tick = index.tick(now + 6_000, false).expect("tick");

        assert_eq!(index.documents().len(), 1);
        assert_eq!(index.documents()[0].text, "new");
        assert!(ingest.expired_docs >= 1);
        assert_eq!(tick.expired_docs, 0);
        log_phase("test_ttl_expiration", "assert", started, "pass");
    }

    #[test]
    fn test_index_size_limit() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.max_index_size_bytes = 260;
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs = vec![
            make_doc(
                "first document with enough bytes",
                1_000,
                SearchDocumentSource::Scrollback,
            ),
            make_doc(
                "second document with enough bytes",
                1_100,
                SearchDocumentSource::Scrollback,
            ),
            make_doc(
                "third document with enough bytes",
                1_200,
                SearchDocumentSource::Scrollback,
            ),
        ];

        let _ = index
            .ingest_documents(&docs, 1_300, false, None)
            .expect("ingest");
        let texts: Vec<String> = index
            .documents()
            .iter()
            .map(|doc| doc.text.clone())
            .collect();
        assert!(
            texts
                .iter()
                .all(|text| text != "first document with enough bytes")
        );
        assert!(index.stats(1_300).total_bytes <= 260);
        log_phase("test_index_size_limit", "assert", started, "pass");
    }

    #[test]
    fn test_indexing_yields_during_resize() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let cfg = make_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs = vec![
            make_doc("during resize", 1_000, SearchDocumentSource::Scrollback),
            make_doc(
                "also during resize",
                1_001,
                SearchDocumentSource::Scrollback,
            ),
        ];
        let report = index
            .ingest_documents(&docs, 1_010, true, None)
            .expect("ingest");
        assert_eq!(report.skipped_resize_pause_docs, 2);
        assert_eq!(index.documents().len(), 0);
        log_phase(
            "test_indexing_yields_during_resize",
            "assert",
            started,
            "pass",
        );
    }

    #[test]
    fn test_index_persistence() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;

        {
            let mut index = SearchIndex::open(cfg.clone()).expect("open writer");
            let doc = make_doc("persist me", 1_000, SearchDocumentSource::Scrollback);
            let _ = index
                .ingest_documents(&[doc], 1_010, false, None)
                .expect("ingest");
        }

        let mut reopened = SearchIndex::open(cfg).expect("open reader");
        let hits = reopened.search("persist me", 5, 2_000);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "persist me");
        log_phase("test_index_persistence", "assert", started, "pass");
    }

    #[test]
    fn test_cass_dedup_integration() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc(
            "cass-owned transcript chunk",
            1_000,
            SearchDocumentSource::Cass,
        );
        let hash = sha256_hex(doc.dedupe_basis().as_bytes());
        let mut cass_hashes = HashSet::new();
        let _ = cass_hashes.insert(hash);

        let report = index
            .ingest_documents(&[doc], 1_010, false, Some(&cass_hashes))
            .expect("ingest");
        assert_eq!(report.skipped_cass_docs, 1);
        assert_eq!(index.documents().len(), 0);
        log_phase("test_cass_dedup_integration", "assert", started, "pass");
    }

    #[test]
    fn test_document_source_tagging() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs = vec![
            make_doc("scroll", 1_000, SearchDocumentSource::Scrollback),
            make_doc("command", 1_001, SearchDocumentSource::Command),
            make_doc("artifact error", 1_002, SearchDocumentSource::AgentArtifact),
        ];
        let _ = index
            .ingest_documents(&docs, 1_010, false, None)
            .expect("ingest");

        let stats = index.stats(1_020);
        assert_eq!(stats.source_counts.get("scrollback"), Some(&1));
        assert_eq!(stats.source_counts.get("command"), Some(&1));
        assert_eq!(stats.source_counts.get("agent"), Some(&1));
        log_phase("test_document_source_tagging", "assert", started, "pass");
    }

    #[test]
    fn test_batch_flush() {
        let started = Instant::now();
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 50;
        cfg.flush_interval_secs = 5;
        let mut index = SearchIndex::open(cfg).expect("open");

        let mut first_batch = Vec::new();
        for i in 0..49 {
            first_batch.push(make_doc(
                &format!("doc-{i}"),
                1_000 + i as i64,
                SearchDocumentSource::Scrollback,
            ));
        }
        let report_a = index
            .ingest_documents(&first_batch, 1_100, false, None)
            .expect("ingest 49");
        assert_eq!(report_a.flushed_docs, 0);
        assert_eq!(index.pending_len(), 49);

        let report_b = index
            .ingest_documents(
                &[make_doc("doc-49", 1_200, SearchDocumentSource::Scrollback)],
                1_200,
                false,
                None,
            )
            .expect("ingest 50th");
        assert_eq!(report_b.flushed_docs, 50);
        assert_eq!(report_b.flush_reason, Some(IndexFlushReason::DocThreshold));

        let report_c = index
            .ingest_documents(
                &[make_doc(
                    "timer-doc",
                    1_300,
                    SearchDocumentSource::Scrollback,
                )],
                1_300,
                false,
                None,
            )
            .expect("ingest timer doc");
        assert_eq!(report_c.flushed_docs, 0);
        let tick = index.tick(6_500, false).expect("interval tick");
        assert_eq!(tick.flushed_docs, 1);
        assert_eq!(tick.flush_reason, Some(IndexFlushReason::Interval));
        log_phase("test_batch_flush", "assert", started, "pass");
    }

    // -----------------------------------------------------------------------
    // SearchDocumentSource unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_document_source_as_tag_all_variants() {
        assert_eq!(SearchDocumentSource::Scrollback.as_tag(), "scrollback");
        assert_eq!(SearchDocumentSource::Command.as_tag(), "command");
        assert_eq!(SearchDocumentSource::AgentArtifact.as_tag(), "agent");
        assert_eq!(SearchDocumentSource::PaneMetadata.as_tag(), "pane_metadata");
        assert_eq!(SearchDocumentSource::Cass.as_tag(), "cass");
    }

    #[test]
    fn test_document_source_serde_roundtrip() {
        let source = SearchDocumentSource::AgentArtifact;
        let json = serde_json::to_string(&source).expect("serialize");
        assert_eq!(json, "\"agent_artifact\"");
        let parsed: SearchDocumentSource = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, source);
    }

    #[test]
    fn test_document_source_equality() {
        assert_eq!(
            SearchDocumentSource::Scrollback,
            SearchDocumentSource::Scrollback
        );
        assert_ne!(
            SearchDocumentSource::Scrollback,
            SearchDocumentSource::Command
        );
    }

    // -----------------------------------------------------------------------
    // ScrollbackLine unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_scrollback_line_new_defaults() {
        let line = ScrollbackLine::new("hello", 42);
        assert_eq!(line.text, "hello");
        assert_eq!(line.captured_at_ms, 42);
        assert!(line.pane_id.is_none());
        assert!(line.session_id.is_none());
    }

    #[test]
    fn test_scrollback_line_serde_roundtrip() {
        let line = ScrollbackLine {
            text: "test line".to_string(),
            captured_at_ms: 1000,
            pane_id: Some(5),
            session_id: Some("sess-1".to_string()),
        };
        let json = serde_json::to_string(&line).expect("serialize");
        let parsed: ScrollbackLine = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, line);
    }

    // -----------------------------------------------------------------------
    // IndexableDocument unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_indexable_document_text_constructor() {
        let doc = IndexableDocument::text(
            SearchDocumentSource::Command,
            "ls -la",
            5000,
            Some(3),
            Some("sess-2".to_string()),
        );
        assert_eq!(doc.source, SearchDocumentSource::Command);
        assert_eq!(doc.text, "ls -la");
        assert_eq!(doc.captured_at_ms, 5000);
        assert_eq!(doc.pane_id, Some(3));
        assert_eq!(doc.session_id.as_deref(), Some("sess-2"));
        assert_eq!(doc.metadata, serde_json::Value::Null);
    }

    #[test]
    fn test_indexable_document_dedupe_basis_text() {
        let doc = IndexableDocument::text(
            SearchDocumentSource::Scrollback,
            "  hello   world  ",
            1000,
            None,
            None,
        );
        let basis = doc.dedupe_basis();
        assert_eq!(basis, "hello world");
    }

    #[test]
    fn test_indexable_document_dedupe_basis_pane_metadata() {
        let mut doc = IndexableDocument::text(
            SearchDocumentSource::PaneMetadata,
            "ignored text",
            1000,
            None,
            None,
        );
        doc.metadata = serde_json::json!({"key": "value"});
        let basis = doc.dedupe_basis();
        // PaneMetadata uses metadata JSON for dedup, not text
        assert!(basis.contains("key"));
        assert!(basis.contains("value"));
    }

    // -----------------------------------------------------------------------
    // normalize_index_text tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_normalize_plain_text() {
        assert_eq!(normalize_index_text("hello world"), "hello world");
    }

    #[test]
    fn test_normalize_collapses_whitespace() {
        assert_eq!(normalize_index_text("  foo   bar  "), "foo bar");
    }

    #[test]
    fn test_normalize_collapses_consecutive_blank_lines() {
        let input = "line1\n\n\n\nline2";
        let result = normalize_index_text(input);
        assert_eq!(result, "line1\n\nline2");
    }

    #[test]
    fn test_normalize_empty_input() {
        assert_eq!(normalize_index_text(""), "");
    }

    #[test]
    fn test_normalize_only_whitespace() {
        assert_eq!(normalize_index_text("   \n   \n   "), "");
    }

    // -----------------------------------------------------------------------
    // strip_ansi_sequences tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_strip_ansi_csi() {
        let input = "\x1b[31mred\x1b[0m";
        assert_eq!(strip_ansi_sequences(input), "red");
    }

    #[test]
    fn test_strip_ansi_osc_bel() {
        let input = "\x1b]0;title\x07rest";
        assert_eq!(strip_ansi_sequences(input), "rest");
    }

    #[test]
    fn test_strip_ansi_osc_st() {
        let input = "\x1b]8;;link\x1b\\visible";
        assert_eq!(strip_ansi_sequences(input), "visible");
    }

    #[test]
    fn test_strip_ansi_no_sequences() {
        assert_eq!(strip_ansi_sequences("plain text"), "plain text");
    }

    // -----------------------------------------------------------------------
    // sha256_hex tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_sha256_hex_deterministic() {
        let a = sha256_hex(b"hello");
        let b = sha256_hex(b"hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64); // SHA-256 hex = 64 chars
    }

    #[test]
    fn test_sha256_hex_different_inputs() {
        assert_ne!(sha256_hex(b"hello"), sha256_hex(b"world"));
    }

    // -----------------------------------------------------------------------
    // validate_config tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_validate_config_ok() {
        let dir = tempdir().expect("tempdir");
        let cfg = make_config(dir.path());
        assert!(validate_config(&cfg).is_ok());
    }

    #[test]
    fn test_validate_config_zero_flush_threshold() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 0;
        let err = validate_config(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("flush_docs_threshold"));
    }

    #[test]
    fn test_validate_config_zero_flush_interval() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_interval_secs = 0;
        let err = validate_config(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("flush_interval_secs"));
    }

    #[test]
    fn test_validate_config_zero_max_docs_per_second() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.max_docs_per_second = 0;
        let err = validate_config(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("max_docs_per_second"));
    }

    #[test]
    fn test_validate_config_empty_index_dir() {
        let cfg = IndexingConfig {
            index_dir: PathBuf::from(""),
            ..make_config(Path::new("/tmp"))
        };
        let err = validate_config(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("index_dir"));
    }

    // -----------------------------------------------------------------------
    // IndexingConfig default tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_indexing_config_default() {
        let cfg = IndexingConfig::default();
        assert_eq!(cfg.max_index_size_bytes, 500 * 1024 * 1024);
        assert_eq!(cfg.ttl_days, 30);
        assert_eq!(cfg.flush_interval_secs, 5);
        assert_eq!(cfg.flush_docs_threshold, 50);
        assert_eq!(cfg.max_docs_per_second, 100);
    }

    // -----------------------------------------------------------------------
    // chunk_scrollback_lines edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_chunk_scrollback_empty() {
        assert!(chunk_scrollback_lines(&[], 5000).is_empty());
    }

    #[test]
    fn test_chunk_scrollback_single_line() {
        let lines = vec![ScrollbackLine::new("one line", 1000)];
        let chunks = chunk_scrollback_lines(&lines, 5000);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].source, SearchDocumentSource::Scrollback);
    }

    #[test]
    fn test_chunk_scrollback_gap_split() {
        let lines = vec![
            ScrollbackLine::new("before gap", 1000),
            ScrollbackLine::new("after gap", 7000),
        ];
        let chunks = chunk_scrollback_lines(&lines, 5000);
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn test_chunk_scrollback_blank_line_split() {
        let lines = vec![
            ScrollbackLine::new("first block", 1000),
            ScrollbackLine::new("", 1100),
            ScrollbackLine::new("second block", 1200),
        ];
        let chunks = chunk_scrollback_lines(&lines, 60_000);
        assert_eq!(chunks.len(), 2);
    }

    // -----------------------------------------------------------------------
    // extract_command_output_blocks tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_command_blocks_empty() {
        let config = CommandBlockExtractionConfig::default();
        assert!(extract_command_output_blocks(&[], &config).is_empty());
    }

    #[test]
    fn test_extract_command_blocks_osc133() {
        let lines = vec![
            ScrollbackLine::new("\x1b]133;A\x07user@host $", 1000),
            ScrollbackLine::new("ls output", 1100),
            ScrollbackLine::new("\x1b]133;A\x07user@host $", 1200),
            ScrollbackLine::new("pwd output", 1300),
        ];
        let config = CommandBlockExtractionConfig::default();
        let blocks = extract_command_output_blocks(&lines, &config);
        assert!(blocks.len() >= 2);
        for block in &blocks {
            assert_eq!(block.source, SearchDocumentSource::Command);
        }
    }

    #[test]
    fn test_extract_command_blocks_no_prompts_fallback() {
        let lines = vec![
            ScrollbackLine::new("output line 1", 1000),
            ScrollbackLine::new("output line 2", 1100),
        ];
        let config = CommandBlockExtractionConfig::default();
        let blocks = extract_command_output_blocks(&lines, &config);
        assert_eq!(blocks.len(), 1);
    }

    // -----------------------------------------------------------------------
    // extract_agent_artifacts tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_extract_artifacts_code_block() {
        let text = "Some text\n```\nfn main() {}\n```\nMore text";
        let artifacts = extract_agent_artifacts(text, 1000, Some(1), None);
        assert!(!artifacts.is_empty());
        let code_block = artifacts.iter().find(|a| {
            a.metadata.get("artifact_kind").and_then(|v| v.as_str()) == Some("code_block")
        });
        assert!(code_block.is_some());
    }

    #[test]
    fn test_extract_artifacts_error_line() {
        let text = "error: cannot find module `foo`";
        let artifacts = extract_agent_artifacts(text, 1000, None, None);
        assert!(!artifacts.is_empty());
        let err = artifacts
            .iter()
            .find(|a| a.metadata.get("artifact_kind").and_then(|v| v.as_str()) == Some("error"));
        assert!(err.is_some());
    }

    #[test]
    fn test_extract_artifacts_tool_trace() {
        let text = "tool call: read_file(\"/foo/bar.rs\")";
        let artifacts = extract_agent_artifacts(text, 1000, None, None);
        assert!(!artifacts.is_empty());
        let tool = artifacts
            .iter()
            .find(|a| a.metadata.get("artifact_kind").and_then(|v| v.as_str()) == Some("tool"));
        assert!(tool.is_some());
    }

    #[test]
    fn test_extract_artifacts_empty_text() {
        let artifacts = extract_agent_artifacts("", 1000, None, None);
        assert!(artifacts.is_empty());
    }

    #[test]
    fn test_extract_artifacts_dedup_identical_lines() {
        let text = "error: E0425\nerror: E0425";
        let artifacts = extract_agent_artifacts(text, 1000, None, None);
        // Dedup by content hash should keep only one
        assert_eq!(artifacts.len(), 1);
    }

    // -----------------------------------------------------------------------
    // SearchIndex::search edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_search_empty_query() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc("findable content", 1000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc], 1010, false, None)
            .expect("ingest");

        let hits = index.search("", 10, 1020);
        assert!(hits.is_empty());
    }

    #[test]
    fn test_search_zero_limit() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc("findable content", 1000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc], 1010, false, None)
            .expect("ingest");

        let hits = index.search("findable", 0, 1020);
        assert!(hits.is_empty());
    }

    #[test]
    fn test_search_ranks_by_occurrence_count() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc_one = make_doc("rust", 1000, SearchDocumentSource::Scrollback);
        let doc_many = make_doc("rust rust rust", 1100, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc_one, doc_many], 1200, false, None)
            .expect("ingest");

        let hits = index.search("rust", 10, 1300);
        assert_eq!(hits.len(), 2);
        // Higher occurrence count should appear first in ranking
        assert!(hits[0].text.matches("rust").count() >= hits[1].text.matches("rust").count());
    }

    // -----------------------------------------------------------------------
    // SearchIndex::stats tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_stats_empty_index() {
        let dir = tempdir().expect("tempdir");
        let cfg = make_config(dir.path());
        let index = SearchIndex::open(cfg).expect("open");
        let stats = index.stats(1000);
        assert_eq!(stats.doc_count, 0);
        assert_eq!(stats.pending_docs, 0);
        assert!(stats.newest_captured_at_ms.is_none());
        assert!(stats.oldest_captured_at_ms.is_none());
        assert!(stats.freshness_age_ms.is_none());
    }

    #[test]
    fn test_stats_freshness_calculation() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let doc = make_doc("fresh", 5000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[doc], 5010, false, None)
            .expect("ingest");

        let stats = index.stats(10_000);
        assert_eq!(stats.freshness_age_ms, Some(5000));
        assert_eq!(stats.newest_captured_at_ms, Some(5000));
        assert_eq!(stats.oldest_captured_at_ms, Some(5000));
    }

    // -----------------------------------------------------------------------
    // SearchIndex::reindex tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_reindex_clears_and_rebuilds() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        let mut index = SearchIndex::open(cfg).expect("open");

        let old = make_doc("old content", 1000, SearchDocumentSource::Scrollback);
        let _ = index
            .ingest_documents(&[old], 1010, false, None)
            .expect("ingest old");
        assert_eq!(index.documents().len(), 1);

        let new_docs = vec![
            make_doc("new alpha", 2000, SearchDocumentSource::Scrollback),
            make_doc("new beta", 2100, SearchDocumentSource::Scrollback),
        ];
        let report = index
            .reindex_documents(&new_docs, 2200, None)
            .expect("reindex");
        assert_eq!(report.accepted_docs, 2);
        assert_eq!(index.documents().len(), 2);
        // Old content should be gone
        let hits = index.search("old content", 10, 2300);
        assert!(hits.is_empty());
    }

    #[test]
    fn test_reindex_restores_state_and_rate_limit_after_ingest_error() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 1;
        cfg.max_docs_per_second = 2;
        let index_dir = cfg.index_dir.clone();
        let mut index = SearchIndex::open(cfg).expect("open");

        let original = make_doc("old content", 1000, SearchDocumentSource::Scrollback);
        index
            .ingest_documents(&[original], 1010, false, None)
            .expect("ingest original doc");

        std::fs::remove_dir_all(&index_dir).expect("remove index dir");
        std::fs::write(&index_dir, b"not-a-directory").expect("replace index dir with file");

        let err = index
            .reindex_documents(
                &[make_doc(
                    "new content",
                    2000,
                    SearchDocumentSource::Scrollback,
                )],
                2200,
                None,
            )
            .expect_err("reindex should fail when state path parent is not a directory");
        assert!(matches!(err, SearchIndexError::Io(_)));
        assert_eq!(index.config().max_docs_per_second, 2);
        assert_eq!(index.documents().len(), 1);
        let hits = index.search("old content", 10, 2300);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "old content");
    }

    // -----------------------------------------------------------------------
    // Rate limiting tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_rate_limiting() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 200;
        cfg.max_docs_per_second = 5;
        let mut index = SearchIndex::open(cfg).expect("open");

        let mut docs = Vec::new();
        for i in 0..10 {
            docs.push(make_doc(
                &format!("rate-doc-{i}"),
                1000,
                SearchDocumentSource::Scrollback,
            ));
        }
        // All submitted at the same timestamp within one rate window
        let report = index
            .ingest_documents(&docs, 1000, false, None)
            .expect("ingest");
        assert_eq!(report.accepted_docs, 5);
        assert_eq!(report.deferred_rate_limited_docs, 5);
    }

    #[test]
    fn test_rate_window_resets_after_1s() {
        let dir = tempdir().expect("tempdir");
        let mut cfg = make_config(dir.path());
        cfg.flush_docs_threshold = 200;
        cfg.max_docs_per_second = 2;
        let mut index = SearchIndex::open(cfg).expect("open");

        let docs_1 = vec![
            make_doc("a", 1000, SearchDocumentSource::Scrollback),
            make_doc("b", 1000, SearchDocumentSource::Scrollback),
            make_doc("c", 1000, SearchDocumentSource::Scrollback),
        ];
        let report_1 = index
            .ingest_documents(&docs_1, 1000, false, None)
            .expect("batch1");
        assert_eq!(report_1.accepted_docs, 2);
        assert_eq!(report_1.deferred_rate_limited_docs, 1);

        // After 1000ms, rate window resets
        let docs_2 = vec![make_doc("d", 2000, SearchDocumentSource::Scrollback)];
        let report_2 = index
            .ingest_documents(&docs_2, 2000, false, None)
            .expect("batch2");
        assert_eq!(report_2.accepted_docs, 1);
        assert_eq!(report_2.deferred_rate_limited_docs, 0);
    }

    // -----------------------------------------------------------------------
    // SearchIndexError display tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_io_display() {
        let err = SearchIndexError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file missing",
        ));
        let display = format!("{err}");
        assert!(display.contains("I/O error"));
    }

    #[test]
    fn test_error_invalid_config_display() {
        let err = SearchIndexError::InvalidConfig("bad value".to_string());
        let display = format!("{err}");
        assert!(display.contains("invalid indexing config"));
        assert!(display.contains("bad value"));
    }

    #[test]
    fn test_error_unsupported_version_display() {
        let err = SearchIndexError::UnsupportedStateVersion(99);
        let display = format!("{err}");
        assert!(display.contains("unsupported"));
        assert!(display.contains("99"));
    }

    // -----------------------------------------------------------------------
    // IndexingIngestReport / IndexingTickResult defaults
    // -----------------------------------------------------------------------

    #[test]
    fn test_ingest_report_default() {
        let report = IndexingIngestReport::default();
        assert_eq!(report.submitted_docs, 0);
        assert_eq!(report.accepted_docs, 0);
        assert_eq!(report.skipped_empty_docs, 0);
        assert!(report.flush_reason.is_none());
    }

    #[test]
    fn test_tick_result_default() {
        let result = IndexingTickResult::default();
        assert_eq!(result.flushed_docs, 0);
        assert_eq!(result.expired_docs, 0);
        assert_eq!(result.evicted_docs, 0);
        assert!(result.flush_reason.is_none());
    }

    // -----------------------------------------------------------------------
    // tick yields during resize
    // -----------------------------------------------------------------------

    #[test]
    fn test_tick_yields_during_resize() {
        let dir = tempdir().expect("tempdir");
        let cfg = make_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");
        let result = index.tick(1000, true).expect("tick");
        assert_eq!(result.flushed_docs, 0);
        assert!(result.flush_reason.is_none());
    }

    // -----------------------------------------------------------------------
    // flush_now with no pending
    // -----------------------------------------------------------------------

    #[test]
    fn test_flush_now_no_pending() {
        let dir = tempdir().expect("tempdir");
        let cfg = make_config(dir.path());
        let mut index = SearchIndex::open(cfg).expect("open");
        let result = index
            .flush_now(1000, IndexFlushReason::Manual)
            .expect("flush");
        assert_eq!(result.flushed_docs, 0);
        assert!(result.flush_reason.is_none());
    }

    // -----------------------------------------------------------------------
    // CassContentHashProvider trait impl
    // -----------------------------------------------------------------------

    #[test]
    fn test_cass_hash_provider_hashset() {
        let mut hashes = HashSet::new();
        hashes.insert("abc123".to_string());
        assert!(hashes.contains_content_hash("abc123"));
        assert!(!hashes.contains_content_hash("xyz789"));
    }

    // -----------------------------------------------------------------------
    // is_prompt_boundary tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_prompt_boundary_osc133_a() {
        assert!(is_prompt_boundary("\x1b]133;A\x07", "", None));
    }

    #[test]
    fn test_prompt_boundary_osc133_b() {
        assert!(is_prompt_boundary("\x1b]133;B\x07", "", None));
    }

    #[test]
    fn test_prompt_boundary_osc133_c() {
        assert!(is_prompt_boundary("\x1b]133;C\x07", "", None));
    }

    #[test]
    fn test_prompt_boundary_regex() {
        let regex = Regex::new(DEFAULT_PROMPT_PATTERN).unwrap();
        assert!(is_prompt_boundary("", "user@host $", Some(&regex)));
        assert!(!is_prompt_boundary("", "just some text", Some(&regex)));
    }
}
