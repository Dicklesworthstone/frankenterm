//! Lexical query service for Tantivy-indexed recorder events.
//!
//! Bead: wa-oegrb.4.5
//!
//! Provides a structured query API for lexical search over the recorder's
//! Tantivy index (`ft.recorder.lexical.v1`), including:
//!
//! - Multi-field text search with configurable boosts
//! - Typed filters (pane, session, event type, source, time range, direction)
//! - Deterministic ranking with score tie-breaking
//! - Cursor-based pagination
//! - Snippet/highlight extraction for terminal text
//! - Trait-based service interface for pluggable backends
//!
//! All types match the schema in `docs/flight-recorder/tantivy-schema-v1.md`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::tantivy_ingest::IndexDocumentFields;

// ---------------------------------------------------------------------------
// Query parameters
// ---------------------------------------------------------------------------

/// A structured lexical search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Free-text query string. Searched across `text` and `text_symbols` fields.
    pub text: String,
    /// Filters to narrow results (all must match — AND semantics).
    #[serde(default)]
    pub filters: Vec<SearchFilter>,
    /// Sort order for results.
    #[serde(default)]
    pub sort: SearchSortOrder,
    /// Pagination parameters.
    #[serde(default)]
    pub pagination: Pagination,
    /// Snippet/highlight configuration.
    #[serde(default)]
    pub snippet_config: SnippetConfig,
    /// Field boost overrides. Keys are field names, values are boost factors.
    /// Defaults: `text` = 1.0, `text_symbols` = 1.25.
    #[serde(default)]
    pub field_boosts: HashMap<String, f32>,
}

impl SearchQuery {
    /// Create a simple text search query with defaults.
    pub fn simple(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            filters: Vec::new(),
            sort: SearchSortOrder::default(),
            pagination: Pagination::default(),
            snippet_config: SnippetConfig::default(),
            field_boosts: HashMap::new(),
        }
    }

    /// Add a filter to the query.
    #[must_use]
    pub fn with_filter(mut self, filter: SearchFilter) -> Self {
        self.filters.push(filter);
        self
    }

    /// Set the page size.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.pagination.limit = limit;
        self
    }

    /// Set a cursor for pagination.
    #[must_use]
    pub fn with_cursor(mut self, cursor: PaginationCursor) -> Self {
        self.pagination.after = Some(cursor);
        self
    }

    /// Effective boost for the `text` field.
    pub fn text_boost(&self) -> f32 {
        *self.field_boosts.get("text").unwrap_or(&1.0)
    }

    /// Effective boost for the `text_symbols` field.
    pub fn text_symbols_boost(&self) -> f32 {
        *self.field_boosts.get("text_symbols").unwrap_or(&1.25)
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// A filter that narrows search results by an indexed field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SearchFilter {
    /// Filter by pane ID (exact match or set membership).
    PaneId { values: Vec<u64> },
    /// Filter by session ID.
    SessionId { value: String },
    /// Filter by workflow ID.
    WorkflowId { value: String },
    /// Filter by correlation ID.
    CorrelationId { value: String },
    /// Filter by event source (e.g. "robot_mode", "wezterm_mux").
    Source { values: Vec<String> },
    /// Filter by event type (e.g. "ingress_text", "egress_output").
    EventType { values: Vec<String> },
    /// Filter by ingress kind (e.g. "send_text", "paste").
    IngressKind { value: String },
    /// Filter by egress segment kind (e.g. "delta", "gap").
    SegmentKind { value: String },
    /// Filter by control marker type.
    ControlMarkerType { value: String },
    /// Filter by lifecycle phase.
    LifecyclePhase { value: String },
    /// Filter by gap status.
    IsGap { value: bool },
    /// Filter by redaction level.
    Redaction { value: String },
    /// Filter by occurred_at_ms time range (inclusive bounds).
    TimeRange {
        /// Minimum occurred_at_ms (inclusive). None = no lower bound.
        min_ms: Option<i64>,
        /// Maximum occurred_at_ms (inclusive). None = no upper bound.
        max_ms: Option<i64>,
    },
    /// Filter by recorded_at_ms time range.
    RecordedTimeRange {
        min_ms: Option<i64>,
        max_ms: Option<i64>,
    },
    /// Filter by sequence range within a pane.
    SequenceRange {
        min_seq: Option<u64>,
        max_seq: Option<u64>,
    },
    /// Filter by log offset range.
    LogOffsetRange {
        min_offset: Option<u64>,
        max_offset: Option<u64>,
    },
    /// Direction filter — shorthand for event_type-based ingress/egress selection.
    Direction { direction: EventDirection },
}

/// Direction filter for ingress vs egress events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventDirection {
    /// Ingress text events (input to terminal).
    Ingress,
    /// Egress output events (output from terminal).
    Egress,
    /// Both ingress and egress (text-bearing events only).
    Both,
}

impl SearchFilter {
    /// Check whether a document matches this filter.
    pub fn matches(&self, doc: &IndexDocumentFields) -> bool {
        match self {
            Self::PaneId { values } => values.contains(&doc.pane_id),
            Self::SessionId { value } => doc.session_id.as_deref() == Some(value.as_str()),
            Self::WorkflowId { value } => doc.workflow_id.as_deref() == Some(value.as_str()),
            Self::CorrelationId { value } => doc.correlation_id.as_deref() == Some(value.as_str()),
            Self::Source { values } => values.iter().any(|v| v == &doc.source),
            Self::EventType { values } => values.iter().any(|v| v == &doc.event_type),
            Self::IngressKind { value } => doc.ingress_kind.as_deref() == Some(value.as_str()),
            Self::SegmentKind { value } => doc.segment_kind.as_deref() == Some(value.as_str()),
            Self::ControlMarkerType { value } => {
                doc.control_marker_type.as_deref() == Some(value.as_str())
            }
            Self::LifecyclePhase { value } => {
                doc.lifecycle_phase.as_deref() == Some(value.as_str())
            }
            Self::IsGap { value } => doc.is_gap == *value,
            Self::Redaction { value } => doc.redaction.as_deref() == Some(value.as_str()),
            Self::TimeRange { min_ms, max_ms } => {
                if let Some(min) = min_ms {
                    if doc.occurred_at_ms < *min {
                        return false;
                    }
                }
                if let Some(max) = max_ms {
                    if doc.occurred_at_ms > *max {
                        return false;
                    }
                }
                true
            }
            Self::RecordedTimeRange { min_ms, max_ms } => {
                if let Some(min) = min_ms {
                    if doc.recorded_at_ms < *min {
                        return false;
                    }
                }
                if let Some(max) = max_ms {
                    if doc.recorded_at_ms > *max {
                        return false;
                    }
                }
                true
            }
            Self::SequenceRange { min_seq, max_seq } => {
                if let Some(min) = min_seq {
                    if doc.sequence < *min {
                        return false;
                    }
                }
                if let Some(max) = max_seq {
                    if doc.sequence > *max {
                        return false;
                    }
                }
                true
            }
            Self::LogOffsetRange {
                min_offset,
                max_offset,
            } => {
                if let Some(min) = min_offset {
                    if doc.log_offset < *min {
                        return false;
                    }
                }
                if let Some(max) = max_offset {
                    if doc.log_offset > *max {
                        return false;
                    }
                }
                true
            }
            Self::Direction { direction } => match direction {
                EventDirection::Ingress => doc.event_type == "ingress_text",
                EventDirection::Egress => doc.event_type == "egress_output",
                EventDirection::Both => {
                    doc.event_type == "ingress_text" || doc.event_type == "egress_output"
                }
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Sorting
// ---------------------------------------------------------------------------

/// Sort order for search results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchSortOrder {
    /// Primary sort field.
    pub primary: SortField,
    /// Whether primary sort is descending.
    pub descending: bool,
}

impl Default for SearchSortOrder {
    fn default() -> Self {
        Self {
            primary: SortField::Relevance,
            descending: true,
        }
    }
}

/// Fields available for sorting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SortField {
    /// Sort by relevance score (BM25).
    Relevance,
    /// Sort by occurred_at_ms timestamp.
    OccurredAt,
    /// Sort by recorded_at_ms timestamp.
    RecordedAt,
    /// Sort by sequence number.
    Sequence,
    /// Sort by log offset.
    LogOffset,
}

/// Tie-breaking key per schema spec:
/// `occurred_at_ms DESC → sequence DESC → log_offset DESC`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct TieBreakKey {
    /// Negated for descending sort (larger timestamp = smaller key).
    neg_occurred_at_ms: i64,
    neg_sequence: i64,
    neg_log_offset: i64,
}

impl TieBreakKey {
    pub fn from_doc(doc: &IndexDocumentFields) -> Self {
        Self {
            neg_occurred_at_ms: doc.occurred_at_ms.wrapping_neg(),
            neg_sequence: i64::try_from(doc.sequence).unwrap_or(i64::MAX).wrapping_neg(),
            neg_log_offset: i64::try_from(doc.log_offset).unwrap_or(i64::MAX).wrapping_neg(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pagination
// ---------------------------------------------------------------------------

/// Pagination parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pagination {
    /// Maximum results to return.
    pub limit: usize,
    /// Cursor to resume after (for cursor-based pagination).
    pub after: Option<PaginationCursor>,
}

impl Default for Pagination {
    fn default() -> Self {
        Self {
            limit: 20,
            after: None,
        }
    }
}

/// Opaque cursor for stable pagination across result pages.
///
/// Encodes the sort key of the last returned result so the next page
/// can resume from the correct position without offset drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaginationCursor {
    /// Score of the last result (scaled to integer for determinism).
    pub score_millis: i64,
    /// occurred_at_ms of the last result.
    pub occurred_at_ms: i64,
    /// sequence of the last result.
    pub sequence: u64,
    /// log_offset of the last result.
    pub log_offset: u64,
}

impl PaginationCursor {
    /// Create a cursor from a search hit.
    pub fn from_hit(hit: &SearchHit) -> Self {
        Self {
            score_millis: (hit.score * 1000.0) as i64,
            occurred_at_ms: hit.doc.occurred_at_ms,
            sequence: hit.doc.sequence,
            log_offset: hit.doc.log_offset,
        }
    }
}

// ---------------------------------------------------------------------------
// Snippet / highlight
// ---------------------------------------------------------------------------

/// Configuration for snippet/highlight extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetConfig {
    /// Maximum characters per snippet fragment.
    pub max_fragment_len: usize,
    /// Maximum number of fragments per hit.
    pub max_fragments: usize,
    /// Highlight tag for matched terms (before).
    pub highlight_pre: String,
    /// Highlight tag for matched terms (after).
    pub highlight_post: String,
    /// Whether to generate snippets at all.
    pub enabled: bool,
}

impl Default for SnippetConfig {
    fn default() -> Self {
        Self {
            max_fragment_len: 200,
            max_fragments: 3,
            highlight_pre: "«".to_string(),
            highlight_post: "»".to_string(),
            enabled: true,
        }
    }
}

/// A highlighted snippet from a matching document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    /// The text fragment with highlight markers inserted.
    pub fragment: String,
    /// Field the snippet was extracted from.
    pub field: String,
}

/// Extract simple keyword-match snippets from text.
///
/// This is a basic implementation suitable for terminal output. Real Tantivy
/// snippets use positional index data; this provides a compatible fallback.
pub fn extract_snippets(
    text: &str,
    query_terms: &[String],
    config: &SnippetConfig,
) -> Vec<Snippet> {
    if !config.enabled || text.is_empty() || query_terms.is_empty() {
        return Vec::new();
    }

    let text_lower = text.to_lowercase();
    let mut fragments = Vec::new();

    for term in query_terms {
        let term_lower = term.to_lowercase();
        if let Some(pos) = text_lower.find(&term_lower) {
            let half_window = config.max_fragment_len / 2;
            let start = pos.saturating_sub(half_window);
            // Find the end, clamped to text length
            let end = (pos + term.len() + half_window).min(text.len());

            // Ensure we're at valid char boundaries
            let start = {
                let mut j = start;
                while j > 0 && !text.is_char_boundary(j) {
                    j -= 1;
                }
                j
            };
            let end = {
                let mut j = end.min(text.len());
                while j < text.len() && !text.is_char_boundary(j) {
                    j += 1;
                }
                j
            };

            let raw_fragment = &text[start..end];

            // Insert highlight markers
            let highlighted = raw_fragment.replacen(
                &text[pos..pos + term.len()],
                &format!(
                    "{}{}{}",
                    config.highlight_pre,
                    &text[pos..pos + term.len()],
                    config.highlight_post
                ),
                1,
            );

            fragments.push(Snippet {
                fragment: highlighted,
                field: "text".to_string(),
            });

            if fragments.len() >= config.max_fragments {
                break;
            }
        }
    }

    fragments
}

/// Split a query string into individual search terms.
pub fn tokenize_query(query: &str) -> Vec<String> {
    // Match the ft_terminal_text_v1 tokenizer pattern: [A-Za-z0-9_./:-]+
    let mut terms = Vec::new();
    let mut current = String::new();

    for ch in query.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '.' || ch == '/' || ch == ':' || ch == '-' {
            current.push(ch);
        } else if !current.is_empty() {
            terms.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        terms.push(current);
    }

    terms
}

// ---------------------------------------------------------------------------
// Search results
// ---------------------------------------------------------------------------

/// Results from a lexical search query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResults {
    /// Matched documents with scores and snippets.
    pub hits: Vec<SearchHit>,
    /// Total number of matching documents (may exceed `hits.len()` due to pagination).
    pub total_hits: u64,
    /// Whether there are more results after this page.
    pub has_more: bool,
    /// Cursor for the next page (from the last hit).
    pub next_cursor: Option<PaginationCursor>,
    /// Query execution time in microseconds.
    pub elapsed_us: u64,
}

impl SearchResults {
    /// Create an empty result set.
    pub fn empty(elapsed_us: u64) -> Self {
        Self {
            hits: Vec::new(),
            total_hits: 0,
            has_more: false,
            next_cursor: None,
            elapsed_us,
        }
    }
}

/// A single search hit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    /// Relevance score (BM25 or 0.0 for non-relevance sorts).
    pub score: f32,
    /// The matched document fields.
    pub doc: IndexDocumentFields,
    /// Highlighted snippets.
    pub snippets: Vec<Snippet>,
}

// ---------------------------------------------------------------------------
// Search error
// ---------------------------------------------------------------------------

/// Error from a search operation.
#[derive(Debug)]
pub enum SearchError {
    /// Query syntax or validation error.
    InvalidQuery { reason: String },
    /// Internal index error.
    Internal { reason: String },
    /// Index is not available or not yet built.
    IndexUnavailable { reason: String },
}

impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidQuery { reason } => write!(f, "invalid query: {reason}"),
            Self::Internal { reason } => write!(f, "internal error: {reason}"),
            Self::IndexUnavailable { reason } => write!(f, "index unavailable: {reason}"),
        }
    }
}

impl std::error::Error for SearchError {}

// ---------------------------------------------------------------------------
// LexicalSearchService trait
// ---------------------------------------------------------------------------

/// Trait for lexical search over the recorder index.
///
/// Implementations wrap the actual Tantivy searcher or a test mock.
pub trait LexicalSearchService: Send + Sync {
    /// Execute a search query and return results.
    fn search(&self, query: &SearchQuery) -> Result<SearchResults, SearchError>;

    /// Count matching documents without fetching results.
    fn count(&self, query: &SearchQuery) -> Result<u64, SearchError>;

    /// Retrieve a single document by event_id.
    fn get_by_event_id(&self, event_id: &str) -> Result<Option<IndexDocumentFields>, SearchError>;

    /// Retrieve a single document by log_offset.
    fn get_by_log_offset(
        &self,
        log_offset: u64,
    ) -> Result<Option<IndexDocumentFields>, SearchError>;

    /// Check whether the index is ready for queries.
    fn is_ready(&self) -> bool;
}

// ---------------------------------------------------------------------------
// InMemorySearchService — reference implementation for tests
// ---------------------------------------------------------------------------

/// In-memory search service for testing and validation.
///
/// Stores documents in a Vec and performs linear scan with basic text matching.
/// Not suitable for production but validates the query contract.
#[derive(Default)]
pub struct InMemorySearchService {
    docs: Vec<IndexDocumentFields>,
}

impl InMemorySearchService {
    /// Create an empty service.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create from a pre-existing document set.
    pub fn from_docs(docs: Vec<IndexDocumentFields>) -> Self {
        Self { docs }
    }

    /// Add a document to the index.
    pub fn add(&mut self, doc: IndexDocumentFields) {
        self.docs.push(doc);
    }

    /// Number of indexed documents.
    pub fn len(&self) -> usize {
        self.docs.len()
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    /// Score a document against query terms using basic TF matching.
    fn score_doc(
        doc: &IndexDocumentFields,
        terms: &[String],
        text_boost: f32,
        symbols_boost: f32,
    ) -> f32 {
        let mut score = 0.0f32;
        let text_lower = doc.text.to_lowercase();
        let symbols_lower = doc.text_symbols.to_lowercase();

        for term in terms {
            let term_lower = term.to_lowercase();
            // Count occurrences in text field
            let text_count = text_lower.matches(&term_lower).count() as f32;
            score = text_count.mul_add(text_boost, score);

            // Count occurrences in text_symbols field
            let sym_count = symbols_lower.matches(&term_lower).count() as f32;
            score = sym_count.mul_add(symbols_boost, score);
        }

        score
    }

    /// Check if a document passes the cursor filter for pagination.
    fn passes_cursor(doc: &IndexDocumentFields, score: f32, cursor: &PaginationCursor) -> bool {
        let score_millis = (score * 1000.0) as i64;
        if score_millis < cursor.score_millis {
            return true;
        }
        if score_millis > cursor.score_millis {
            return false;
        }
        // Equal score: use tie-break ordering (desc)
        let tb = (doc.occurred_at_ms, doc.sequence, doc.log_offset);
        let cursor_tb = (cursor.occurred_at_ms, cursor.sequence, cursor.log_offset);
        tb < cursor_tb
    }
}

impl LexicalSearchService for InMemorySearchService {
    fn search(&self, query: &SearchQuery) -> Result<SearchResults, SearchError> {
        let start = std::time::Instant::now();

        if query.text.is_empty() && query.filters.is_empty() {
            return Err(SearchError::InvalidQuery {
                reason: "query must have text or at least one filter".to_string(),
            });
        }

        let terms = tokenize_query(&query.text);
        let text_boost = query.text_boost();
        let symbols_boost = query.text_symbols_boost();

        // Score and filter all documents
        let mut scored: Vec<(f32, &IndexDocumentFields)> = self
            .docs
            .iter()
            .filter(|doc| {
                // All filters must match
                query.filters.iter().all(|f| f.matches(doc))
            })
            .filter_map(|doc| {
                let score = if terms.is_empty() {
                    // Filter-only query: all matching docs get score 0
                    0.0
                } else {
                    Self::score_doc(doc, &terms, text_boost, symbols_boost)
                };

                // For text queries, require at least one term match
                if !terms.is_empty() && score.abs() < f32::EPSILON {
                    return None;
                }

                Some((score, doc))
            })
            .collect();

        let total_hits = scored.len() as u64;

        // Apply cursor filter
        if let Some(ref cursor) = query.pagination.after {
            scored.retain(|(score, doc)| Self::passes_cursor(doc, *score, cursor));
        }

        // Sort results
        match query.sort.primary {
            SortField::Relevance => {
                scored.sort_by(|(sa, da), (sb, db)| {
                    // Score descending, then tie-break
                    sb.partial_cmp(sa)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| TieBreakKey::from_doc(da).cmp(&TieBreakKey::from_doc(db)))
                });
            }
            SortField::OccurredAt => {
                if query.sort.descending {
                    scored.sort_by(|(_, da), (_, db)| {
                        db.occurred_at_ms
                            .cmp(&da.occurred_at_ms)
                            .then_with(|| TieBreakKey::from_doc(da).cmp(&TieBreakKey::from_doc(db)))
                    });
                } else {
                    scored.sort_by(|(_, da), (_, db)| {
                        da.occurred_at_ms
                            .cmp(&db.occurred_at_ms)
                            .then_with(|| TieBreakKey::from_doc(da).cmp(&TieBreakKey::from_doc(db)))
                    });
                }
            }
            SortField::RecordedAt => {
                if query.sort.descending {
                    scored.sort_by_key(|(_, d)| std::cmp::Reverse(d.recorded_at_ms));
                } else {
                    scored.sort_by_key(|(_, d)| d.recorded_at_ms);
                }
            }
            SortField::Sequence => {
                if query.sort.descending {
                    scored.sort_by_key(|(_, d)| std::cmp::Reverse(d.sequence));
                } else {
                    scored.sort_by_key(|(_, d)| d.sequence);
                }
            }
            SortField::LogOffset => {
                if query.sort.descending {
                    scored.sort_by_key(|(_, d)| std::cmp::Reverse(d.log_offset));
                } else {
                    scored.sort_by_key(|(_, d)| d.log_offset);
                }
            }
        }

        // Paginate
        let limit = query.pagination.limit;
        let has_more = scored.len() > limit;
        let page: Vec<_> = scored.into_iter().take(limit).collect();

        // Build hits with snippets
        let hits: Vec<SearchHit> = page
            .iter()
            .map(|(score, doc)| {
                let snippets = extract_snippets(&doc.text, &terms, &query.snippet_config);
                SearchHit {
                    score: *score,
                    doc: (*doc).clone(),
                    snippets,
                }
            })
            .collect();

        let next_cursor = hits.last().map(PaginationCursor::from_hit);

        let elapsed_us = start.elapsed().as_micros() as u64;

        Ok(SearchResults {
            hits,
            total_hits,
            has_more,
            next_cursor,
            elapsed_us,
        })
    }

    fn count(&self, query: &SearchQuery) -> Result<u64, SearchError> {
        let terms = tokenize_query(&query.text);

        let count = self
            .docs
            .iter()
            .filter(|doc| query.filters.iter().all(|f| f.matches(doc)))
            .filter(|doc| {
                if terms.is_empty() {
                    true
                } else {
                    let text_lower = doc.text.to_lowercase();
                    terms.iter().any(|t| text_lower.contains(&t.to_lowercase()))
                }
            })
            .count();

        Ok(count as u64)
    }

    fn get_by_event_id(&self, event_id: &str) -> Result<Option<IndexDocumentFields>, SearchError> {
        Ok(self.docs.iter().find(|d| d.event_id == event_id).cloned())
    }

    fn get_by_log_offset(
        &self,
        log_offset: u64,
    ) -> Result<Option<IndexDocumentFields>, SearchError> {
        Ok(self
            .docs
            .iter()
            .find(|d| d.log_offset == log_offset)
            .cloned())
    }

    fn is_ready(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// TantivySearchService — production implementation backed by a real index
// ---------------------------------------------------------------------------

/// Production implementation of [`LexicalSearchService`] backed by a live
/// Tantivy index managed by [`LexicalIndexer`](crate::recorder_lexical_ingest::LexicalIndexer).
///
/// Performs BM25 search using Tantivy's native query parser, then reconstructs
/// [`IndexDocumentFields`] from stored fields. Domain-specific filters from
/// [`SearchFilter`] are applied as Tantivy query clauses where possible and as
/// post-filters for complex predicates.
pub struct TantivySearchService {
    index: tantivy::Index,
    handles: crate::recorder_lexical_schema::LexicalFieldHandles,
    reader: tantivy::IndexReader,
}

impl TantivySearchService {
    /// Create a new search service from a [`LexicalIndexer`](crate::recorder_lexical_ingest::LexicalIndexer).
    pub fn from_indexer(
        indexer: &crate::recorder_lexical_ingest::LexicalIndexer,
    ) -> Result<Self, SearchError> {
        let index = indexer.index().clone();
        let reader = index.reader().map_err(|e| SearchError::Internal {
            reason: format!("failed to create index reader: {e}"),
        })?;
        Ok(Self {
            index,
            handles: indexer.handles(),
            reader,
        })
    }

    /// Create from raw Tantivy index and field handles.
    pub fn new(
        index: tantivy::Index,
        handles: crate::recorder_lexical_schema::LexicalFieldHandles,
    ) -> Result<Self, SearchError> {
        let reader = index.reader().map_err(|e| SearchError::Internal {
            reason: format!("failed to create index reader: {e}"),
        })?;
        Ok(Self {
            index,
            handles,
            reader,
        })
    }

    /// Reload the underlying reader to pick up newly committed segments.
    pub fn reload(&self) -> Result<(), SearchError> {
        self.reader.reload().map_err(|e| SearchError::Internal {
            reason: format!("reader reload failed: {e}"),
        })
    }

    /// Build a Tantivy query for the text portion of a [`SearchQuery`].
    fn build_text_query(
        &self,
        text: &str,
        text_boost: f32,
        symbols_boost: f32,
    ) -> Result<Box<dyn tantivy::query::Query>, SearchError> {
        use tantivy::query::{BooleanQuery, BoostQuery, Occur};

        let terms = tokenize_query(text);
        if terms.is_empty() {
            return Ok(Box::new(tantivy::query::AllQuery));
        }

        let _tokenizer_mgr = self.index.tokenizers();
        let mut all_clauses = Vec::new();

        for term in &terms {
            let term_lower = term.to_lowercase();
            let mut term_clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = Vec::new();

            // Text field with boost
            let text_term = tantivy::schema::Term::from_field_text(self.handles.text, &term_lower);
            let text_query = tantivy::query::TermQuery::new(
                text_term,
                tantivy::schema::IndexRecordOption::WithFreqsAndPositions,
            );
            term_clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(text_query), text_boost)),
            ));

            // text_symbols field with boost
            let sym_term =
                tantivy::schema::Term::from_field_text(self.handles.text_symbols, &term_lower);
            let sym_query = tantivy::query::TermQuery::new(
                sym_term,
                tantivy::schema::IndexRecordOption::WithFreqsAndPositions,
            );
            term_clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(sym_query), symbols_boost)),
            ));

            all_clauses.push((
                Occur::Should,
                Box::new(BooleanQuery::new(term_clauses)) as Box<dyn tantivy::query::Query>,
            ));
        }

        Ok(Box::new(BooleanQuery::new(all_clauses)))
    }

    /// Build Tantivy filter clauses from `SearchFilter` items.
    fn build_filter_query(
        &self,
        filters: &[SearchFilter],
    ) -> Vec<(tantivy::query::Occur, Box<dyn tantivy::query::Query>)> {
        use tantivy::query::{BooleanQuery, Occur, RangeQuery, TermQuery};
        use tantivy::schema::{IndexRecordOption, Term};

        let mut clauses = Vec::new();

        for filter in filters {
            match filter {
                SearchFilter::PaneId { values } => {
                    let sub: Vec<_> = values
                        .iter()
                        .map(|v| {
                            let term = Term::from_field_u64(self.handles.pane_id, *v);
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                                    as Box<dyn tantivy::query::Query>,
                            )
                        })
                        .collect();
                    if !sub.is_empty() {
                        clauses.push((
                            Occur::Must,
                            Box::new(BooleanQuery::new(sub)) as Box<dyn tantivy::query::Query>,
                        ));
                    }
                }
                SearchFilter::SessionId { value } => {
                    let term = Term::from_field_text(self.handles.session_id, value);
                    clauses.push((
                        Occur::Must,
                        Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                            as Box<dyn tantivy::query::Query>,
                    ));
                }
                SearchFilter::Source { values } => {
                    let sub: Vec<_> = values
                        .iter()
                        .map(|v| {
                            let term = Term::from_field_text(self.handles.source, v);
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                                    as Box<dyn tantivy::query::Query>,
                            )
                        })
                        .collect();
                    if !sub.is_empty() {
                        clauses.push((
                            Occur::Must,
                            Box::new(BooleanQuery::new(sub)) as Box<dyn tantivy::query::Query>,
                        ));
                    }
                }
                SearchFilter::EventType { values } => {
                    let sub: Vec<_> = values
                        .iter()
                        .map(|v| {
                            let term = Term::from_field_text(self.handles.event_type, v);
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                                    as Box<dyn tantivy::query::Query>,
                            )
                        })
                        .collect();
                    if !sub.is_empty() {
                        clauses.push((
                            Occur::Must,
                            Box::new(BooleanQuery::new(sub)) as Box<dyn tantivy::query::Query>,
                        ));
                    }
                }
                SearchFilter::Direction { direction } => match direction {
                    EventDirection::Ingress => {
                        let term = Term::from_field_text(self.handles.event_type, "ingress_text");
                        clauses.push((
                            Occur::Must,
                            Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                                as Box<dyn tantivy::query::Query>,
                        ));
                    }
                    EventDirection::Egress => {
                        let term = Term::from_field_text(self.handles.event_type, "egress_output");
                        clauses.push((
                            Occur::Must,
                            Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                                as Box<dyn tantivy::query::Query>,
                        ));
                    }
                    EventDirection::Both => {
                        let sub = vec![
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.handles.event_type, "ingress_text"),
                                    IndexRecordOption::Basic,
                                ))
                                    as Box<dyn tantivy::query::Query>,
                            ),
                            (
                                Occur::Should,
                                Box::new(TermQuery::new(
                                    Term::from_field_text(self.handles.event_type, "egress_output"),
                                    IndexRecordOption::Basic,
                                ))
                                    as Box<dyn tantivy::query::Query>,
                            ),
                        ];
                        clauses.push((
                            Occur::Must,
                            Box::new(BooleanQuery::new(sub)) as Box<dyn tantivy::query::Query>,
                        ));
                    }
                },
                SearchFilter::TimeRange { min_ms, max_ms } => {
                    let field = self.handles.occurred_at_ms;
                    let lower = min_ms
                        .map(|v| Term::from_field_i64(field, v))
                        .map(std::ops::Bound::Included)
                        .unwrap_or(std::ops::Bound::Unbounded);
                    let upper = max_ms
                        .map(|v| Term::from_field_i64(field, v))
                        .map(std::ops::Bound::Included)
                        .unwrap_or(std::ops::Bound::Unbounded);
                    let rq = RangeQuery::new_term_bounds(
                        "occurred_at_ms".to_string(),
                        tantivy::schema::Type::I64,
                        &lower,
                        &upper,
                    );
                    clauses.push((Occur::Must, Box::new(rq) as Box<dyn tantivy::query::Query>));
                }
                // Remaining filters applied post-query for simplicity
                _ => {}
            }
        }

        clauses
    }
}

impl LexicalSearchService for TantivySearchService {
    fn search(&self, query: &SearchQuery) -> Result<SearchResults, SearchError> {
        use tantivy::query::{BooleanQuery, Occur};

        let start = std::time::Instant::now();

        if query.text.is_empty() && query.filters.is_empty() {
            return Err(SearchError::InvalidQuery {
                reason: "query must have text or at least one filter".to_string(),
            });
        }

        let searcher = self.reader.searcher();

        // Build combined query: text + filters
        let text_query =
            self.build_text_query(&query.text, query.text_boost(), query.text_symbols_boost())?;
        let filter_clauses = self.build_filter_query(&query.filters);

        let final_query: Box<dyn tantivy::query::Query> = if filter_clauses.is_empty() {
            text_query
        } else {
            let mut clauses = vec![(Occur::Must, text_query)];
            clauses.extend(filter_clauses);
            Box::new(BooleanQuery::new(clauses))
        };

        // Fetch more than limit to account for post-filters
        let fetch_limit = (query.pagination.limit + 1) * 2;
        let top_docs = searcher
            .search(
                &final_query,
                &tantivy::collector::TopDocs::with_limit(fetch_limit),
            )
            .map_err(|e| SearchError::Internal {
                reason: format!("search failed: {e}"),
            })?;

        // Reconstruct IndexDocumentFields from stored fields
        let mut scored: Vec<(f32, IndexDocumentFields)> = Vec::with_capacity(top_docs.len());
        for (score, doc_addr) in &top_docs {
            let doc = searcher
                .doc::<tantivy::TantivyDocument>(*doc_addr)
                .map_err(|e| SearchError::Internal {
                    reason: format!("failed to retrieve doc: {e}"),
                })?;
            let fields = crate::recorder_lexical_schema::document_to_fields(&doc, &self.handles);
            scored.push((*score, fields));
        }

        // Apply remaining post-filters (those not handled as Tantivy queries)
        let has_post_filters = query.filters.iter().any(|f| {
            matches!(
                f,
                SearchFilter::WorkflowId { .. }
                    | SearchFilter::CorrelationId { .. }
                    | SearchFilter::IngressKind { .. }
                    | SearchFilter::SegmentKind { .. }
                    | SearchFilter::ControlMarkerType { .. }
                    | SearchFilter::LifecyclePhase { .. }
                    | SearchFilter::IsGap { .. }
                    | SearchFilter::Redaction { .. }
                    | SearchFilter::RecordedTimeRange { .. }
                    | SearchFilter::SequenceRange { .. }
                    | SearchFilter::LogOffsetRange { .. }
            )
        });
        if has_post_filters {
            scored.retain(|(_, doc)| query.filters.iter().all(|f| f.matches(doc)));
        }

        let total_hits = scored.len() as u64;

        // Apply cursor filter
        if let Some(ref cursor) = query.pagination.after {
            scored.retain(|(score, doc)| InMemorySearchService::passes_cursor(doc, *score, cursor));
        }

        // Sort results
        match query.sort.primary {
            SortField::Relevance => {
                scored.sort_by(|(sa, da), (sb, db)| {
                    sb.partial_cmp(sa)
                        .unwrap_or(std::cmp::Ordering::Equal)
                        .then_with(|| TieBreakKey::from_doc(da).cmp(&TieBreakKey::from_doc(db)))
                });
            }
            SortField::OccurredAt => {
                if query.sort.descending {
                    scored.sort_by(|(_, da), (_, db)| {
                        db.occurred_at_ms
                            .cmp(&da.occurred_at_ms)
                            .then_with(|| TieBreakKey::from_doc(da).cmp(&TieBreakKey::from_doc(db)))
                    });
                } else {
                    scored.sort_by(|(_, da), (_, db)| {
                        da.occurred_at_ms
                            .cmp(&db.occurred_at_ms)
                            .then_with(|| TieBreakKey::from_doc(da).cmp(&TieBreakKey::from_doc(db)))
                    });
                }
            }
            SortField::RecordedAt => {
                if query.sort.descending {
                    scored.sort_by_key(|(_, d)| std::cmp::Reverse(d.recorded_at_ms));
                } else {
                    scored.sort_by_key(|(_, d)| d.recorded_at_ms);
                }
            }
            SortField::Sequence => {
                if query.sort.descending {
                    scored.sort_by_key(|(_, d)| std::cmp::Reverse(d.sequence));
                } else {
                    scored.sort_by_key(|(_, d)| d.sequence);
                }
            }
            SortField::LogOffset => {
                if query.sort.descending {
                    scored.sort_by_key(|(_, d)| std::cmp::Reverse(d.log_offset));
                } else {
                    scored.sort_by_key(|(_, d)| d.log_offset);
                }
            }
        }

        // Paginate
        let limit = query.pagination.limit;
        let has_more = scored.len() > limit;
        let page: Vec<_> = scored.into_iter().take(limit).collect();

        // Build hits with snippets
        let terms = tokenize_query(&query.text);
        let hits: Vec<SearchHit> = page
            .iter()
            .map(|(score, doc)| {
                let snippets = extract_snippets(&doc.text, &terms, &query.snippet_config);
                SearchHit {
                    score: *score,
                    doc: doc.clone(),
                    snippets,
                }
            })
            .collect();

        let next_cursor = hits.last().map(PaginationCursor::from_hit);
        let elapsed_us = start.elapsed().as_micros() as u64;

        Ok(SearchResults {
            hits,
            total_hits,
            has_more,
            next_cursor,
            elapsed_us,
        })
    }

    fn count(&self, query: &SearchQuery) -> Result<u64, SearchError> {
        use tantivy::query::{BooleanQuery, Occur};

        let searcher = self.reader.searcher();

        let text_query =
            self.build_text_query(&query.text, query.text_boost(), query.text_symbols_boost())?;
        let filter_clauses = self.build_filter_query(&query.filters);

        let final_query: Box<dyn tantivy::query::Query> = if filter_clauses.is_empty() {
            text_query
        } else {
            let mut clauses = vec![(Occur::Must, text_query)];
            clauses.extend(filter_clauses);
            Box::new(BooleanQuery::new(clauses))
        };

        let count = searcher
            .search(&final_query, &tantivy::collector::Count)
            .map_err(|e| SearchError::Internal {
                reason: format!("count failed: {e}"),
            })?;

        Ok(count as u64)
    }

    fn get_by_event_id(&self, event_id: &str) -> Result<Option<IndexDocumentFields>, SearchError> {
        let searcher = self.reader.searcher();
        let term = tantivy::schema::Term::from_field_text(self.handles.event_id, event_id);
        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        let top = searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(1))
            .map_err(|e| SearchError::Internal {
                reason: format!("get_by_event_id failed: {e}"),
            })?;
        match top.first() {
            Some((_score, addr)) => {
                let doc = searcher
                    .doc::<tantivy::TantivyDocument>(*addr)
                    .map_err(|e| SearchError::Internal {
                        reason: format!("doc retrieval failed: {e}"),
                    })?;
                Ok(Some(crate::recorder_lexical_schema::document_to_fields(
                    &doc,
                    &self.handles,
                )))
            }
            None => Ok(None),
        }
    }

    fn get_by_log_offset(
        &self,
        log_offset: u64,
    ) -> Result<Option<IndexDocumentFields>, SearchError> {
        let searcher = self.reader.searcher();
        let term = tantivy::schema::Term::from_field_u64(self.handles.log_offset, log_offset);
        let query = tantivy::query::TermQuery::new(term, tantivy::schema::IndexRecordOption::Basic);
        let top = searcher
            .search(&query, &tantivy::collector::TopDocs::with_limit(1))
            .map_err(|e| SearchError::Internal {
                reason: format!("get_by_log_offset failed: {e}"),
            })?;
        match top.first() {
            Some((_score, addr)) => {
                let doc = searcher
                    .doc::<tantivy::TantivyDocument>(*addr)
                    .map_err(|e| SearchError::Internal {
                        reason: format!("doc retrieval failed: {e}"),
                    })?;
                Ok(Some(crate::recorder_lexical_schema::document_to_fields(
                    &doc,
                    &self.handles,
                )))
            }
            None => Ok(None),
        }
    }

    fn is_ready(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recording::{
        RECORDER_EVENT_SCHEMA_VERSION_V1, RecorderControlMarkerType, RecorderEvent,
        RecorderEventCausality, RecorderEventPayload, RecorderEventSource, RecorderIngressKind,
        RecorderRedactionLevel, RecorderSegmentKind, RecorderTextEncoding,
    };
    use crate::tantivy_ingest::{IndexWriter, map_event_to_document};

    fn make_doc(pane: u64, text: &str, seq: u64) -> IndexDocumentFields {
        make_ingress(&format!("doc-{pane}-{seq}"), pane, seq, text)
    }

    fn make_ingress(id: &str, pane: u64, seq: u64, text: &str) -> IndexDocumentFields {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: id.to_string(),
            pane_id: pane,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: Some("corr-1".to_string()),
            source: RecorderEventSource::RobotMode,
            occurred_at_ms: 1_700_000_000_000 + seq * 100,
            recorded_at_ms: 1_700_000_000_001 + seq * 100,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::IngressText {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                ingress_kind: RecorderIngressKind::SendText,
            },
        };
        map_event_to_document(&event, seq)
    }

    fn make_egress(id: &str, pane: u64, seq: u64, text: &str) -> IndexDocumentFields {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: id.to_string(),
            pane_id: pane,
            session_id: Some("sess-1".to_string()),
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1_700_000_000_000 + seq * 100,
            recorded_at_ms: 1_700_000_000_001 + seq * 100,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::EgressOutput {
                text: text.to_string(),
                encoding: RecorderTextEncoding::Utf8,
                redaction: RecorderRedactionLevel::None,
                segment_kind: RecorderSegmentKind::Delta,
                is_gap: false,
            },
        };
        map_event_to_document(&event, seq)
    }

    fn make_control(id: &str, pane: u64, seq: u64) -> IndexDocumentFields {
        let event = RecorderEvent {
            schema_version: RECORDER_EVENT_SCHEMA_VERSION_V1.to_string(),
            event_id: id.to_string(),
            pane_id: pane,
            session_id: None,
            workflow_id: None,
            correlation_id: None,
            source: RecorderEventSource::WeztermMux,
            occurred_at_ms: 1_700_000_000_000 + seq * 100,
            recorded_at_ms: 1_700_000_000_001 + seq * 100,
            sequence: seq,
            causality: RecorderEventCausality {
                parent_event_id: None,
                trigger_event_id: None,
                root_event_id: None,
            },
            payload: RecorderEventPayload::ControlMarker {
                control_marker_type: RecorderControlMarkerType::PromptBoundary,
                details: serde_json::json!({}),
            },
        };
        map_event_to_document(&event, seq)
    }

    fn test_service() -> InMemorySearchService {
        let mut svc = InMemorySearchService::new();
        svc.add(make_ingress("i1", 1, 0, "echo hello world"));
        svc.add(make_ingress("i2", 1, 1, "cargo test --release"));
        svc.add(make_ingress("i3", 2, 2, "git push origin main"));
        svc.add(make_egress("e1", 1, 3, "hello world\nfoo bar"));
        svc.add(make_egress("e2", 2, 4, "Compiling frankenterm v0.1.0"));
        svc.add(make_egress("e3", 2, 5, "error[E0308]: mismatched types"));
        svc.add(make_control("c1", 1, 6));
        svc
    }

    // =========================================================================
    // Simple text search
    // =========================================================================

    #[test]
    fn simple_text_search() {
        let svc = test_service();
        let q = SearchQuery::simple("hello");
        let results = svc.search(&q).unwrap();

        assert!(results.total_hits >= 1);
        assert!(results.hits.iter().any(|h| h.doc.event_id == "i1"));
        assert!(results.hits.iter().any(|h| h.doc.event_id == "e1"));
    }

    #[test]
    fn search_no_results() {
        let svc = test_service();
        let q = SearchQuery::simple("xyznonexistent");
        let results = svc.search(&q).unwrap();
        assert_eq!(results.total_hits, 0);
        assert!(results.hits.is_empty());
        assert!(!results.has_more);
    }

    #[test]
    fn search_empty_query_with_filter() {
        let svc = test_service();
        let q = SearchQuery {
            text: String::new(),
            filters: vec![SearchFilter::PaneId { values: vec![1] }],
            sort: SearchSortOrder::default(),
            pagination: Pagination::default(),
            snippet_config: SnippetConfig::default(),
            field_boosts: HashMap::new(),
        };
        let results = svc.search(&q).unwrap();
        assert!(results.total_hits > 0);
        assert!(results.hits.iter().all(|h| h.doc.pane_id == 1));
    }

    #[test]
    fn search_empty_query_no_filter_errors() {
        let svc = test_service();
        let q = SearchQuery {
            text: String::new(),
            filters: Vec::new(),
            sort: SearchSortOrder::default(),
            pagination: Pagination::default(),
            snippet_config: SnippetConfig::default(),
            field_boosts: HashMap::new(),
        };
        let err = svc.search(&q).unwrap_err();
        assert!(matches!(err, SearchError::InvalidQuery { .. }));
    }

    // =========================================================================
    // Filter tests
    // =========================================================================

    #[test]
    fn filter_by_pane_id() {
        let svc = test_service();
        let q = SearchQuery::simple("echo cargo git Compiling error")
            .with_filter(SearchFilter::PaneId { values: vec![2] });
        let results = svc.search(&q).unwrap();
        assert!(results.hits.iter().all(|h| h.doc.pane_id == 2));
    }

    #[test]
    fn filter_by_event_type() {
        let svc = test_service();
        let q = SearchQuery::simple("hello world echo Compiling error cargo git").with_filter(
            SearchFilter::EventType {
                values: vec!["ingress_text".to_string()],
            },
        );
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.event_type == "ingress_text")
        );
    }

    #[test]
    fn filter_by_direction_ingress() {
        let svc = test_service();
        let q = SearchQuery::simple("hello cargo git push echo Compiling error").with_filter(
            SearchFilter::Direction {
                direction: EventDirection::Ingress,
            },
        );
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.event_type == "ingress_text")
        );
    }

    #[test]
    fn filter_by_direction_egress() {
        let svc = test_service();
        let q = SearchQuery::simple("hello world Compiling error").with_filter(
            SearchFilter::Direction {
                direction: EventDirection::Egress,
            },
        );
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.event_type == "egress_output")
        );
    }

    #[test]
    fn filter_by_source() {
        let svc = test_service();
        let q = SearchQuery::simple("hello cargo echo git").with_filter(SearchFilter::Source {
            values: vec!["robot_mode".to_string()],
        });
        let results = svc.search(&q).unwrap();
        assert!(results.hits.iter().all(|h| h.doc.source == "robot_mode"));
    }

    #[test]
    fn filter_by_time_range() {
        let svc = test_service();
        // Events at seq 0 and 1 have occurred_at_ms 1_700_000_000_000 and 1_700_000_000_100
        let q = SearchQuery::simple("echo cargo").with_filter(SearchFilter::TimeRange {
            min_ms: Some(1_700_000_000_000),
            max_ms: Some(1_700_000_000_100),
        });
        let results = svc.search(&q).unwrap();
        assert!(results.total_hits >= 1);
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.occurred_at_ms >= 1_700_000_000_000
                    && h.doc.occurred_at_ms <= 1_700_000_000_100)
        );
    }

    #[test]
    fn filter_by_session_id() {
        let svc = test_service();
        let q = SearchQuery::simple("hello cargo git Compiling error echo").with_filter(
            SearchFilter::SessionId {
                value: "sess-1".to_string(),
            },
        );
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.session_id == Some("sess-1".to_string()))
        );
    }

    #[test]
    fn filter_by_correlation_id() {
        let svc = test_service();
        let q =
            SearchQuery::simple("echo cargo git hello").with_filter(SearchFilter::CorrelationId {
                value: "corr-1".to_string(),
            });
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.correlation_id == Some("corr-1".to_string()))
        );
    }

    #[test]
    fn multiple_filters_and_semantics() {
        let svc = test_service();
        let q = SearchQuery::simple("echo hello cargo git Compiling error")
            .with_filter(SearchFilter::PaneId { values: vec![1] })
            .with_filter(SearchFilter::Direction {
                direction: EventDirection::Ingress,
            });
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.pane_id == 1 && h.doc.event_type == "ingress_text")
        );
    }

    #[test]
    fn filter_sequence_range() {
        let svc = test_service();
        let q = SearchQuery::simple("hello cargo echo Compiling error git").with_filter(
            SearchFilter::SequenceRange {
                min_seq: Some(2),
                max_seq: Some(4),
            },
        );
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.sequence >= 2 && h.doc.sequence <= 4)
        );
    }

    // =========================================================================
    // Sorting tests
    // =========================================================================

    #[test]
    fn sort_by_occurred_at_descending() {
        let svc = test_service();
        let q = SearchQuery {
            text: "hello cargo echo git Compiling error".to_string(),
            sort: SearchSortOrder {
                primary: SortField::OccurredAt,
                descending: true,
            },
            ..SearchQuery::simple("")
        };
        let results = svc.search(&q).unwrap();
        for w in results.hits.windows(2) {
            assert!(
                w[0].doc.occurred_at_ms >= w[1].doc.occurred_at_ms,
                "results not sorted descending by occurred_at_ms"
            );
        }
    }

    #[test]
    fn sort_by_occurred_at_ascending() {
        let svc = test_service();
        let q = SearchQuery {
            text: "hello cargo echo git Compiling error".to_string(),
            sort: SearchSortOrder {
                primary: SortField::OccurredAt,
                descending: false,
            },
            ..SearchQuery::simple("")
        };
        let results = svc.search(&q).unwrap();
        for w in results.hits.windows(2) {
            assert!(
                w[0].doc.occurred_at_ms <= w[1].doc.occurred_at_ms,
                "results not sorted ascending by occurred_at_ms"
            );
        }
    }

    #[test]
    fn sort_by_sequence() {
        let svc = test_service();
        let q = SearchQuery {
            text: "hello cargo echo git Compiling error".to_string(),
            sort: SearchSortOrder {
                primary: SortField::Sequence,
                descending: false,
            },
            ..SearchQuery::simple("")
        };
        let results = svc.search(&q).unwrap();
        for w in results.hits.windows(2) {
            assert!(w[0].doc.sequence <= w[1].doc.sequence);
        }
    }

    // =========================================================================
    // Pagination tests
    // =========================================================================

    #[test]
    fn pagination_limits_results() {
        let svc = test_service();
        let q = SearchQuery::simple("hello cargo echo git Compiling error").with_limit(2);
        let results = svc.search(&q).unwrap();
        assert!(results.hits.len() <= 2);
        assert!(results.has_more);
        assert!(results.next_cursor.is_some());
    }

    #[test]
    fn pagination_cursor_advances() {
        let svc = test_service();

        // First page
        let q1 = SearchQuery::simple("hello cargo echo git Compiling error").with_limit(3);
        let r1 = svc.search(&q1).unwrap();
        assert_eq!(r1.hits.len(), 3);
        let cursor = r1.next_cursor.unwrap();

        // Second page
        let q2 = SearchQuery::simple("hello cargo echo git Compiling error")
            .with_limit(3)
            .with_cursor(cursor);
        let r2 = svc.search(&q2).unwrap();

        // Pages should not overlap
        let ids1: Vec<_> = r1.hits.iter().map(|h| &h.doc.event_id).collect();
        let ids2: Vec<_> = r2.hits.iter().map(|h| &h.doc.event_id).collect();
        for id in &ids2 {
            assert!(!ids1.contains(id), "page 2 overlaps with page 1");
        }
    }

    // =========================================================================
    // Snippet tests
    // =========================================================================

    #[test]
    fn snippet_extraction_basic() {
        let config = SnippetConfig::default();
        let snippets = extract_snippets("echo hello world", &["hello".to_string()], &config);
        assert_eq!(snippets.len(), 1);
        assert!(snippets[0].fragment.contains("«hello»"));
    }

    #[test]
    fn snippet_multiple_terms() {
        let config = SnippetConfig {
            max_fragments: 3,
            ..Default::default()
        };
        let snippets = extract_snippets(
            "cargo test --release && echo done",
            &["cargo".to_string(), "echo".to_string()],
            &config,
        );
        assert!(snippets.len() >= 2);
    }

    #[test]
    fn snippet_disabled() {
        let config = SnippetConfig {
            enabled: false,
            ..Default::default()
        };
        let snippets = extract_snippets("hello world", &["hello".to_string()], &config);
        assert!(snippets.is_empty());
    }

    #[test]
    fn snippet_empty_text() {
        let config = SnippetConfig::default();
        let snippets = extract_snippets("", &["hello".to_string()], &config);
        assert!(snippets.is_empty());
    }

    #[test]
    fn snippet_no_terms() {
        let config = SnippetConfig::default();
        let snippets = extract_snippets("hello world", &[], &config);
        assert!(snippets.is_empty());
    }

    #[test]
    fn snippet_custom_markers() {
        let config = SnippetConfig {
            highlight_pre: "<b>".to_string(),
            highlight_post: "</b>".to_string(),
            ..Default::default()
        };
        let snippets = extract_snippets("hello world", &["hello".to_string()], &config);
        assert!(snippets[0].fragment.contains("<b>hello</b>"));
    }

    // =========================================================================
    // Tokenizer tests
    // =========================================================================

    #[test]
    fn tokenize_simple() {
        let terms = tokenize_query("echo hello world");
        assert_eq!(terms, vec!["echo", "hello", "world"]);
    }

    #[test]
    fn tokenize_preserves_paths() {
        let terms = tokenize_query("src/main.rs:42");
        assert_eq!(terms, vec!["src/main.rs:42"]);
    }

    #[test]
    fn tokenize_preserves_flags() {
        let terms = tokenize_query("cargo test --release");
        assert_eq!(terms, vec!["cargo", "test", "--release"]);
    }

    #[test]
    fn tokenize_preserves_namespaces() {
        let terms = tokenize_query("std::io::Error");
        assert_eq!(terms, vec!["std::io::Error"]);
    }

    #[test]
    fn tokenize_empty() {
        assert!(tokenize_query("").is_empty());
    }

    // =========================================================================
    // get_by_event_id / get_by_log_offset
    // =========================================================================

    #[test]
    fn get_by_event_id_found() {
        let svc = test_service();
        let doc = svc.get_by_event_id("i1").unwrap().unwrap();
        assert_eq!(doc.event_id, "i1");
        assert_eq!(doc.text, "echo hello world");
    }

    #[test]
    fn get_by_event_id_not_found() {
        let svc = test_service();
        assert!(svc.get_by_event_id("nonexistent").unwrap().is_none());
    }

    #[test]
    fn get_by_log_offset() {
        let svc = test_service();
        let doc = svc.get_by_log_offset(0).unwrap().unwrap();
        assert_eq!(doc.event_id, "i1");
    }

    // =========================================================================
    // count
    // =========================================================================

    #[test]
    fn count_all_text_matches() {
        let svc = test_service();
        let q = SearchQuery::simple("hello");
        let count = svc.count(&q).unwrap();
        assert!(count >= 1);
    }

    #[test]
    fn count_with_filter() {
        let svc = test_service();
        let q = SearchQuery::simple("hello cargo echo git Compiling error")
            .with_filter(SearchFilter::PaneId { values: vec![1] });
        let count = svc.count(&q).unwrap();
        let results = svc.search(&q).unwrap();
        assert_eq!(count, results.total_hits);
    }

    // =========================================================================
    // Relevance scoring
    // =========================================================================

    #[test]
    fn higher_tf_scores_higher() {
        let mut svc = InMemorySearchService::new();
        svc.add(make_ingress("low", 1, 0, "hello"));
        svc.add(make_ingress("high", 1, 1, "hello hello hello"));

        let q = SearchQuery::simple("hello");
        let results = svc.search(&q).unwrap();
        assert_eq!(results.hits[0].doc.event_id, "high");
    }

    #[test]
    fn field_boosts_affect_ranking() {
        let mut svc = InMemorySearchService::new();
        svc.add(make_ingress("a", 1, 0, "test code"));
        svc.add(make_ingress("b", 1, 1, "test code"));

        let mut boosts = HashMap::new();
        boosts.insert("text".to_string(), 5.0);
        boosts.insert("text_symbols".to_string(), 0.1);

        let q = SearchQuery {
            field_boosts: boosts,
            ..SearchQuery::simple("test")
        };
        let results = svc.search(&q).unwrap();
        assert!(results.total_hits >= 1);
    }

    // =========================================================================
    // SearchQuery builder
    // =========================================================================

    #[test]
    fn query_builder() {
        let q = SearchQuery::simple("hello")
            .with_filter(SearchFilter::PaneId { values: vec![1, 2] })
            .with_filter(SearchFilter::TimeRange {
                min_ms: Some(1000),
                max_ms: None,
            })
            .with_limit(50);

        assert_eq!(q.text, "hello");
        assert_eq!(q.filters.len(), 2);
        assert_eq!(q.pagination.limit, 50);
    }

    #[test]
    fn default_boosts() {
        let q = SearchQuery::simple("test");
        assert!((q.text_boost() - 1.0).abs() < f32::EPSILON);
        assert!((q.text_symbols_boost() - 1.25).abs() < f32::EPSILON);
    }

    // =========================================================================
    // SearchResults
    // =========================================================================

    #[test]
    fn empty_results() {
        let r = SearchResults::empty(42);
        assert_eq!(r.total_hits, 0);
        assert_eq!(r.elapsed_us, 42);
        assert!(!r.has_more);
    }

    // =========================================================================
    // Error display
    // =========================================================================

    #[test]
    fn search_error_display() {
        let e1 = SearchError::InvalidQuery {
            reason: "bad".to_string(),
        };
        assert!(e1.to_string().contains("invalid query"));

        let e2 = SearchError::Internal {
            reason: "oops".to_string(),
        };
        assert!(e2.to_string().contains("internal"));

        let e3 = SearchError::IndexUnavailable {
            reason: "building".to_string(),
        };
        assert!(e3.to_string().contains("unavailable"));
    }

    // =========================================================================
    // is_ready
    // =========================================================================

    #[test]
    fn in_memory_service_always_ready() {
        let svc = InMemorySearchService::new();
        assert!(svc.is_ready());
    }

    // =========================================================================
    // Filter matches tests (unit)
    // =========================================================================

    #[test]
    fn filter_is_gap() {
        let doc = make_egress("e1", 1, 0, "text");
        assert!(SearchFilter::IsGap { value: false }.matches(&doc));
        assert!(!SearchFilter::IsGap { value: true }.matches(&doc));
    }

    #[test]
    fn filter_direction_both() {
        let ingress = make_ingress("i1", 1, 0, "text");
        let egress = make_egress("e1", 1, 1, "text");
        let control = make_control("c1", 1, 2);

        let f = SearchFilter::Direction {
            direction: EventDirection::Both,
        };
        assert!(f.matches(&ingress));
        assert!(f.matches(&egress));
        assert!(!f.matches(&control));
    }

    #[test]
    fn filter_log_offset_range() {
        let doc = make_ingress("i1", 1, 5, "text"); // log_offset = 5
        assert!(
            SearchFilter::LogOffsetRange {
                min_offset: Some(3),
                max_offset: Some(7),
            }
            .matches(&doc)
        );
        assert!(
            !SearchFilter::LogOffsetRange {
                min_offset: Some(6),
                max_offset: None,
            }
            .matches(&doc)
        );
    }

    #[test]
    fn filter_serialization_roundtrip() {
        let filter = SearchFilter::TimeRange {
            min_ms: Some(1000),
            max_ms: Some(2000),
        };
        let json = serde_json::to_string(&filter).unwrap();
        let deser: SearchFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(filter, deser);
    }

    // =========================================================================
    // SearchQuery serialization
    // =========================================================================

    #[test]
    fn search_query_serialization_roundtrip() {
        let q = SearchQuery::simple("hello")
            .with_filter(SearchFilter::PaneId { values: vec![1] })
            .with_limit(10);
        let json = serde_json::to_string(&q).unwrap();
        let deser: SearchQuery = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.text, "hello");
        assert_eq!(deser.pagination.limit, 10);
    }

    // -------------------------------------------------------------------
    // Batch: DarkBadger wa-1u90p.7.1
    // -------------------------------------------------------------------

    // -- SearchQuery trait coverage --

    #[test]
    fn search_query_debug() {
        let q = SearchQuery::simple("test");
        let dbg = format!("{:?}", q);
        assert!(dbg.contains("SearchQuery"), "got: {}", dbg);
    }

    #[test]
    fn search_query_clone() {
        let q = SearchQuery::simple("test").with_limit(5);
        let cloned = q.clone();
        assert_eq!(cloned.text, "test");
        assert_eq!(cloned.pagination.limit, 5);
    }

    #[test]
    fn search_query_text_boost_default() {
        let q = SearchQuery::simple("test");
        assert!((q.text_boost() - 1.0).abs() < f32::EPSILON);
        assert!((q.text_symbols_boost() - 1.25).abs() < f32::EPSILON);
    }

    #[test]
    fn search_query_with_cursor() {
        let cursor = PaginationCursor {
            score_millis: 500,
            occurred_at_ms: 1000,
            sequence: 1,
            log_offset: 0,
        };
        let q = SearchQuery::simple("test").with_cursor(cursor);
        assert!(q.pagination.after.is_some());
    }

    // -- SearchSortOrder --

    #[test]
    fn sort_order_debug_clone() {
        let s = SearchSortOrder::default();
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("SearchSortOrder"), "got: {}", dbg);
        let cloned = s.clone();
        assert_eq!(s, cloned);
    }

    #[test]
    fn sort_order_default_values() {
        let s = SearchSortOrder::default();
        assert_eq!(s.primary, SortField::Relevance);
        assert!(s.descending);
    }

    #[test]
    fn sort_order_serde_roundtrip() {
        let s = SearchSortOrder {
            primary: SortField::OccurredAt,
            descending: false,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: SearchSortOrder = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // -- SortField --

    #[test]
    fn sort_field_debug_clone_copy() {
        let f = SortField::Sequence;
        let cloned = f;
        let copied = f;
        assert_eq!(cloned, copied);
        let dbg = format!("{:?}", f);
        assert!(dbg.contains("Sequence"), "got: {}", dbg);
    }

    #[test]
    fn sort_field_serde_all_variants() {
        let fields = [
            SortField::Relevance,
            SortField::OccurredAt,
            SortField::RecordedAt,
            SortField::Sequence,
            SortField::LogOffset,
        ];
        for field in &fields {
            let json = serde_json::to_string(field).unwrap();
            let back: SortField = serde_json::from_str(&json).unwrap();
            assert_eq!(*field, back);
        }
    }

    // -- TieBreakKey --

    #[test]
    fn tie_break_key_debug_clone() {
        let key = TieBreakKey {
            neg_occurred_at_ms: -1000,
            neg_sequence: -5,
            neg_log_offset: -10,
        };
        let dbg = format!("{:?}", key);
        assert!(dbg.contains("TieBreakKey"), "got: {}", dbg);
        let cloned = key.clone();
        assert_eq!(key, cloned);
    }

    #[test]
    fn tie_break_key_ordering() {
        // Higher timestamps should sort first (smaller negated value)
        let a = TieBreakKey {
            neg_occurred_at_ms: -2000,
            neg_sequence: -1,
            neg_log_offset: -1,
        };
        let b = TieBreakKey {
            neg_occurred_at_ms: -1000,
            neg_sequence: -1,
            neg_log_offset: -1,
        };
        // -2000 < -1000, so a < b, meaning a (ts=2000) sorts before b (ts=1000)
        assert!(a < b);
    }

    #[test]
    fn tie_break_key_sequence_tiebreak() {
        let a = TieBreakKey {
            neg_occurred_at_ms: -1000,
            neg_sequence: -10,
            neg_log_offset: -1,
        };
        let b = TieBreakKey {
            neg_occurred_at_ms: -1000,
            neg_sequence: -5,
            neg_log_offset: -1,
        };
        // Same timestamp, seq 10 > seq 5, so -10 < -5, a < b
        assert!(a < b);
    }

    // -- Pagination --

    #[test]
    fn pagination_debug_clone() {
        let p = Pagination::default();
        let dbg = format!("{:?}", p);
        assert!(dbg.contains("Pagination"), "got: {}", dbg);
        let cloned = p.clone();
        assert_eq!(cloned.limit, p.limit);
    }

    #[test]
    fn pagination_default_values() {
        let p = Pagination::default();
        assert_eq!(p.limit, 20);
        assert!(p.after.is_none());
    }

    #[test]
    fn pagination_serde_roundtrip() {
        let p = Pagination {
            limit: 50,
            after: Some(PaginationCursor {
                score_millis: 100,
                occurred_at_ms: 5000,
                sequence: 42,
                log_offset: 99,
            }),
        };
        let json = serde_json::to_string(&p).unwrap();
        let back: Pagination = serde_json::from_str(&json).unwrap();
        assert_eq!(back.limit, 50);
        assert!(back.after.is_some());
    }

    // -- PaginationCursor --

    #[test]
    fn pagination_cursor_debug_clone_eq() {
        let cursor = PaginationCursor {
            score_millis: 500,
            occurred_at_ms: 1000,
            sequence: 1,
            log_offset: 0,
        };
        let dbg = format!("{:?}", cursor);
        assert!(dbg.contains("PaginationCursor"), "got: {}", dbg);
        let cloned = cursor.clone();
        assert_eq!(cursor, cloned);
    }

    #[test]
    fn pagination_cursor_serde_roundtrip() {
        let cursor = PaginationCursor {
            score_millis: 750,
            occurred_at_ms: 2000,
            sequence: 10,
            log_offset: 55,
        };
        let json = serde_json::to_string(&cursor).unwrap();
        let back: PaginationCursor = serde_json::from_str(&json).unwrap();
        assert_eq!(cursor, back);
    }

    // -- SnippetConfig --

    #[test]
    fn snippet_config_debug_clone() {
        let config = SnippetConfig::default();
        let dbg = format!("{:?}", config);
        assert!(dbg.contains("SnippetConfig"), "got: {}", dbg);
        let _cloned = config.clone();
    }

    #[test]
    fn snippet_config_default_values() {
        let config = SnippetConfig::default();
        assert_eq!(config.max_fragment_len, 200);
        assert_eq!(config.max_fragments, 3);
        assert_eq!(config.highlight_pre, "\u{ab}"); // «
        assert_eq!(config.highlight_post, "\u{bb}"); // »
        assert!(config.enabled);
    }

    #[test]
    fn snippet_config_serde_roundtrip() {
        let config = SnippetConfig {
            max_fragment_len: 100,
            max_fragments: 5,
            highlight_pre: "<em>".to_string(),
            highlight_post: "</em>".to_string(),
            enabled: false,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: SnippetConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.max_fragment_len, 100);
        assert!(!back.enabled);
    }

    // -- Snippet --

    #[test]
    fn snippet_debug_clone_eq() {
        let s = Snippet {
            fragment: "hello «world»".to_string(),
            field: "text".to_string(),
        };
        let dbg = format!("{:?}", s);
        assert!(dbg.contains("Snippet"), "got: {}", dbg);
        let cloned = s.clone();
        assert_eq!(s, cloned);
    }

    #[test]
    fn snippet_serde_roundtrip() {
        let s = Snippet {
            fragment: "test «match»".to_string(),
            field: "text_symbols".to_string(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Snippet = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    // -- SearchResults --

    #[test]
    fn search_results_debug_clone() {
        let r = SearchResults::empty(42);
        let dbg = format!("{:?}", r);
        assert!(dbg.contains("SearchResults"), "got: {}", dbg);
        let cloned = r.clone();
        assert_eq!(cloned.total_hits, 0);
        assert_eq!(cloned.elapsed_us, 42);
    }

    #[test]
    fn search_results_empty_fields() {
        let r = SearchResults::empty(100);
        assert!(r.hits.is_empty());
        assert_eq!(r.total_hits, 0);
        assert!(!r.has_more);
        assert!(r.next_cursor.is_none());
    }

    #[test]
    fn search_results_serde_roundtrip() {
        let r = SearchResults::empty(55);
        let json = serde_json::to_string(&r).unwrap();
        let back: SearchResults = serde_json::from_str(&json).unwrap();
        assert_eq!(back.total_hits, 0);
        assert_eq!(back.elapsed_us, 55);
    }

    // -- SearchError --

    #[test]
    fn search_error_debug() {
        let e = SearchError::InvalidQuery {
            reason: "bad syntax".to_string(),
        };
        let dbg = format!("{:?}", e);
        assert!(dbg.contains("InvalidQuery"), "got: {}", dbg);
    }

    #[test]
    fn search_error_display_all() {
        let errors = [
            SearchError::InvalidQuery {
                reason: "bad".to_string(),
            },
            SearchError::Internal {
                reason: "oops".to_string(),
            },
            SearchError::IndexUnavailable {
                reason: "down".to_string(),
            },
        ];
        let expected = [
            "invalid query: bad",
            "internal error: oops",
            "index unavailable: down",
        ];
        for (e, exp) in errors.iter().zip(expected.iter()) {
            assert_eq!(e.to_string(), *exp);
        }
    }

    #[test]
    fn search_error_is_error_trait() {
        let e = SearchError::Internal {
            reason: "test".to_string(),
        };
        let err: &dyn std::error::Error = &e;
        assert!(!err.to_string().is_empty());
    }

    // -- InMemorySearchService --

    #[test]
    fn in_memory_service_default() {
        let svc = InMemorySearchService::default();
        assert!(svc.is_empty());
        assert_eq!(svc.len(), 0);
    }

    #[test]
    fn in_memory_service_add() {
        let mut svc = InMemorySearchService::new();
        svc.add(make_doc(1, "hello world", 1000));
        assert_eq!(svc.len(), 1);
        assert!(!svc.is_empty());
    }

    // -- EventDirection --

    #[test]
    fn event_direction_debug_clone_copy() {
        let d = EventDirection::Ingress;
        let cloned = d;
        let copied = d;
        assert_eq!(cloned, copied);
        let dbg = format!("{:?}", d);
        assert!(dbg.contains("Ingress"), "got: {}", dbg);
    }

    #[test]
    fn event_direction_serde_all() {
        let dirs = [
            EventDirection::Ingress,
            EventDirection::Egress,
            EventDirection::Both,
        ];
        for d in &dirs {
            let json = serde_json::to_string(d).unwrap();
            let back: EventDirection = serde_json::from_str(&json).unwrap();
            assert_eq!(*d, back);
        }
    }

    // =========================================================================
    // TantivySearchService tests
    // =========================================================================

    fn tantivy_service() -> (TantivySearchService, tempfile::TempDir) {
        use crate::recorder_lexical_ingest::{LexicalIndexer, LexicalIndexerConfig};

        let tmp = tempfile::tempdir().unwrap();
        let config = LexicalIndexerConfig {
            index_dir: tmp.path().to_path_buf(),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let mut writer = indexer.create_writer().unwrap();

        // Index same docs as test_service()
        let docs = vec![
            make_ingress("i1", 1, 0, "echo hello world"),
            make_ingress("i2", 1, 1, "cargo test --release"),
            make_ingress("i3", 2, 2, "git push origin main"),
            make_egress("e1", 1, 3, "hello world\nfoo bar"),
            make_egress("e2", 2, 4, "Compiling frankenterm v0.1.0"),
            make_egress("e3", 2, 5, "error[E0308]: mismatched types"),
            make_control("c1", 1, 6),
        ];
        for doc in &docs {
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();

        let svc = TantivySearchService::from_indexer(&indexer).unwrap();
        (svc, tmp)
    }

    #[test]
    fn tantivy_simple_text_search() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery::simple("hello");
        let results = svc.search(&q).unwrap();
        assert!(
            results.total_hits >= 1,
            "expected at least 1 hit for 'hello'"
        );
        let ids: Vec<_> = results
            .hits
            .iter()
            .map(|h| h.doc.event_id.as_str())
            .collect();
        assert!(ids.contains(&"i1"), "missing i1 in results: {:?}", ids);
    }

    #[test]
    fn tantivy_search_no_results() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery::simple("xyznonexistent");
        let results = svc.search(&q).unwrap();
        assert_eq!(results.total_hits, 0);
        assert!(results.hits.is_empty());
    }

    #[test]
    fn tantivy_filter_by_pane_id() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery::simple("hello echo cargo git Compiling error")
            .with_filter(SearchFilter::PaneId { values: vec![2] });
        let results = svc.search(&q).unwrap();
        assert!(results.hits.iter().all(|h| h.doc.pane_id == 2));
    }

    #[test]
    fn tantivy_filter_by_event_type() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery::simple("hello echo cargo git Compiling error").with_filter(
            SearchFilter::EventType {
                values: vec!["ingress_text".to_string()],
            },
        );
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.event_type == "ingress_text")
        );
    }

    #[test]
    fn tantivy_get_by_event_id() {
        let (svc, _tmp) = tantivy_service();
        let doc = svc.get_by_event_id("i2").unwrap();
        assert!(doc.is_some());
        let doc = doc.unwrap();
        assert_eq!(doc.event_id, "i2");
        assert_eq!(doc.pane_id, 1);
    }

    #[test]
    fn tantivy_get_by_event_id_missing() {
        let (svc, _tmp) = tantivy_service();
        let doc = svc.get_by_event_id("nonexistent").unwrap();
        assert!(doc.is_none());
    }

    #[test]
    fn tantivy_get_by_log_offset() {
        let (svc, _tmp) = tantivy_service();
        let doc = svc.get_by_log_offset(3).unwrap();
        assert!(doc.is_some());
        let doc = doc.unwrap();
        assert_eq!(doc.event_id, "e1");
        assert_eq!(doc.log_offset, 3);
    }

    #[test]
    fn tantivy_count() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery::simple("hello");
        let count = svc.count(&q).unwrap();
        assert!(count >= 1, "expected at least 1 for 'hello', got {count}");
    }

    #[test]
    fn tantivy_is_ready() {
        let (svc, _tmp) = tantivy_service();
        assert!(svc.is_ready());
    }

    #[test]
    fn tantivy_empty_query_with_filter() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery {
            text: String::new(),
            filters: vec![SearchFilter::PaneId { values: vec![1] }],
            sort: SearchSortOrder::default(),
            pagination: Pagination::default(),
            snippet_config: SnippetConfig::default(),
            field_boosts: HashMap::new(),
        };
        let results = svc.search(&q).unwrap();
        assert!(results.total_hits > 0);
        assert!(results.hits.iter().all(|h| h.doc.pane_id == 1));
    }

    #[test]
    fn tantivy_empty_query_no_filter_errors() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery {
            text: String::new(),
            filters: Vec::new(),
            sort: SearchSortOrder::default(),
            pagination: Pagination::default(),
            snippet_config: SnippetConfig::default(),
            field_boosts: HashMap::new(),
        };
        let err = svc.search(&q).unwrap_err();
        assert!(matches!(err, SearchError::InvalidQuery { .. }));
    }

    #[test]
    fn tantivy_pagination_limit() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery::simple("hello echo cargo git Compiling error").with_limit(2);
        let results = svc.search(&q).unwrap();
        assert!(results.hits.len() <= 2);
    }

    #[test]
    fn tantivy_direction_filter_ingress() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery::simple("hello echo cargo git Compiling error").with_filter(
            SearchFilter::Direction {
                direction: EventDirection::Ingress,
            },
        );
        let results = svc.search(&q).unwrap();
        assert!(
            results
                .hits
                .iter()
                .all(|h| h.doc.event_type == "ingress_text")
        );
    }

    #[test]
    fn tantivy_snippets_generated() {
        let (svc, _tmp) = tantivy_service();
        let q = SearchQuery::simple("hello");
        let results = svc.search(&q).unwrap();
        let has_snippets = results.hits.iter().any(|h| !h.snippets.is_empty());
        assert!(has_snippets, "expected at least one hit with snippets");
    }

    #[test]
    fn tantivy_document_round_trip_fidelity() {
        let (svc, _tmp) = tantivy_service();
        let doc = svc.get_by_event_id("i1").unwrap().unwrap();
        assert_eq!(doc.event_id, "i1");
        assert_eq!(doc.pane_id, 1);
        assert_eq!(doc.source, "robot_mode");
        assert_eq!(doc.event_type, "ingress_text");
        assert!(doc.text.contains("echo hello world"));
        assert!(doc.session_id.is_some());
        assert_eq!(doc.sequence, 0);
        assert_eq!(doc.log_offset, 0);
    }

    #[test]
    fn tantivy_reload_picks_up_new_docs() {
        use crate::recorder_lexical_ingest::{LexicalIndexer, LexicalIndexerConfig};

        let tmp = tempfile::tempdir().unwrap();
        let config = LexicalIndexerConfig {
            index_dir: tmp.path().to_path_buf(),
            writer_memory_bytes: 15_000_000,
        };
        let indexer = LexicalIndexer::open(config).unwrap();
        let mut writer = indexer.create_writer().unwrap();
        writer
            .add_document(&make_ingress("r1", 1, 0, "initial doc"))
            .unwrap();
        writer.commit().unwrap();
        drop(writer); // Must drop before creating a new writer

        let svc = TantivySearchService::from_indexer(&indexer).unwrap();
        let count_before = svc.count(&SearchQuery::simple("initial")).unwrap();
        assert!(count_before >= 1);

        // Add more docs and commit (previous writer must be dropped first)
        let mut writer2 = indexer.create_writer().unwrap();
        writer2
            .add_document(&make_ingress("r2", 1, 1, "second doc"))
            .unwrap();
        writer2.commit().unwrap();

        svc.reload().unwrap();
        let doc = svc.get_by_event_id("r2").unwrap();
        assert!(
            doc.is_some(),
            "newly committed doc should be visible after reload"
        );
    }
}
