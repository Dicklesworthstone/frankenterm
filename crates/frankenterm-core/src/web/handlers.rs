//! HTTP handler extraction surface for Wave 4B migration.
//!
//! These handlers are extracted from `web.rs` in a strangler-fig migration to
//! keep behavior stable while reducing monolith size.

#[allow(clippy::wildcard_imports)]
use super::*;
use serde::Serialize;

// =============================================================================
// /health
// =============================================================================

#[derive(Serialize)]
pub(super) struct HealthResponse {
    pub(super) ok: bool,
    pub(super) version: &'static str,
}

pub(super) fn health_response() -> Response {
    let payload = HealthResponse {
        ok: true,
        version: VERSION,
    };
    Response::json(&payload).unwrap_or_else(|_| Response::internal_error())
}

// =============================================================================
// /panes
// =============================================================================

#[derive(Serialize)]
pub(super) struct PanesResponse {
    pub(super) panes: Vec<PaneView>,
    pub(super) total: usize,
}

#[derive(Serialize)]
pub(super) struct PaneView {
    pub(super) pane_id: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pane_uuid: Option<String>,
    pub(super) domain: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) window_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) tab_id: Option<u64>,
    pub(super) first_seen_at: i64,
    pub(super) last_seen_at: i64,
}

impl PaneView {
    pub(super) fn from_record(r: PaneRecord, redactor: &Redactor) -> Self {
        Self {
            pane_id: r.pane_id,
            pane_uuid: r.pane_uuid,
            domain: r.domain,
            title: r.title.map(|t| redactor.redact(&t)),
            cwd: r.cwd.map(|c| redactor.redact(&c)),
            window_id: r.window_id,
            tab_id: r.tab_id,
            first_seen_at: r.first_seen_at,
            last_seen_at: r.last_seen_at,
        }
    }
}

pub(super) fn handle_panes(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    Box::pin(async move {
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match storage.get_panes().await {
            Ok(panes) => {
                let total = panes.len();
                let views: Vec<PaneView> = panes
                    .into_iter()
                    .map(|p| PaneView::from_record(p, &redactor))
                    .collect();
                json_ok(PanesResponse {
                    panes: views,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Failed to query panes: {e}"),
            ),
        }
    })
}

// =============================================================================
// /events
// =============================================================================

#[derive(Serialize)]
pub(super) struct EventsResponse {
    pub(super) events: Vec<EventView>,
    pub(super) total: usize,
}

#[derive(Serialize)]
pub(super) struct EventView {
    pub(super) id: i64,
    pub(super) pane_id: u64,
    pub(super) rule_id: String,
    pub(super) event_type: String,
    pub(super) severity: String,
    pub(super) confidence: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) extracted: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) matched_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) annotations: Option<EventAnnotationsView>,
    pub(super) detected_at: i64,
}

#[derive(Serialize)]
pub(super) struct EventAnnotationsView {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) triage_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) note: Option<String>,
    pub(super) labels: Vec<String>,
}

impl EventAnnotationsView {
    pub(super) fn from_stored(
        annotations: crate::storage::EventAnnotations,
        redactor: &Redactor,
    ) -> Self {
        Self {
            triage_state: annotations.triage_state.map(|v| redactor.redact(&v)),
            note: annotations.note.map(|v| redactor.redact(&v)),
            labels: annotations
                .labels
                .into_iter()
                .map(|label| redactor.redact(&label))
                .collect(),
        }
    }
}

impl EventView {
    pub(super) fn from_stored(
        e: crate::storage::StoredEvent,
        redactor: &Redactor,
        annotations: Option<EventAnnotationsView>,
    ) -> Self {
        Self {
            id: e.id,
            pane_id: e.pane_id,
            rule_id: e.rule_id,
            event_type: e.event_type,
            severity: e.severity,
            confidence: e.confidence,
            extracted: e.extracted,
            matched_text: e.matched_text.map(|t| redactor.redact(&t)),
            annotations,
            detected_at: e.detected_at,
        }
    }
}

pub(super) fn handle_events(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);

    let query = EventQuery {
        limit: Some(parse_limit(&qs)),
        pane_id: parse_u64(&qs, "pane_id"),
        rule_id: qs.get("rule_id").map(String::from),
        event_type: qs.get("event_type").map(String::from),
        triage_state: qs.get("triage_state").map(String::from),
        label: qs.get("label").map(String::from),
        unhandled_only: parse_bool(&qs, "unhandled"),
        since: parse_i64(&qs, "since"),
        until: parse_i64(&qs, "until"),
    };

    Box::pin(async move {
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match storage.get_events(query).await {
            Ok(events) => {
                let total = events.len();
                let mut views: Vec<EventView> = Vec::with_capacity(total);
                for event in events {
                    let annotations = match storage.get_event_annotations(event.id).await {
                        Ok(Some(annotations)) => {
                            Some(EventAnnotationsView::from_stored(annotations, &redactor))
                        }
                        Ok(None) => None,
                        Err(err) => {
                            tracing::warn!(
                                target: "wa.web",
                                error = %err,
                                event_id = event.id,
                                "failed to load event annotations"
                            );
                            None
                        }
                    };
                    views.push(EventView::from_stored(event, &redactor, annotations));
                }
                json_ok(EventsResponse {
                    events: views,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Failed to query events: {e}"),
            ),
        }
    })
}

// =============================================================================
// /search
// =============================================================================

#[derive(Serialize)]
pub(super) struct SearchResponse {
    pub(super) results: Vec<SearchHit>,
    pub(super) total: usize,
}

#[derive(Serialize)]
pub(super) struct SearchHit {
    pub(super) segment_id: i64,
    pub(super) pane_id: u64,
    pub(super) score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) snippet: Option<String>,
    pub(super) captured_at: i64,
    pub(super) content_len: usize,
}

impl SearchHit {
    pub(super) fn from_result(r: SearchResult, redactor: &Redactor) -> Self {
        Self {
            segment_id: r.segment.id,
            pane_id: r.segment.pane_id,
            score: r.score,
            snippet: r.snippet.map(|s| redactor.redact(&s)),
            captured_at: r.segment.captured_at,
            content_len: r.segment.content_len,
        }
    }
}

pub(super) fn handle_search(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    let qs_raw = req.query().unwrap_or("").to_string();
    let qs = QueryString::parse(&qs_raw);

    let query_str = qs.get("q").map(String::from);
    let options = SearchOptions {
        limit: Some(parse_limit(&qs)),
        pane_id: parse_u64(&qs, "pane_id"),
        since: parse_i64(&qs, "since"),
        until: parse_i64(&qs, "until"),
        include_snippets: Some(true),
        snippet_max_tokens: Some(64),
        highlight_prefix: None,
        highlight_suffix: None,
    };

    Box::pin(async move {
        let query = match query_str {
            Some(q) if !q.is_empty() => q,
            _ => {
                return json_err(
                    StatusCode::BAD_REQUEST,
                    "missing_query",
                    "Query parameter 'q' is required",
                );
            }
        };
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match storage.search_with_results(&query, options).await {
            Ok(results) => {
                let total = results.len();
                let hits: Vec<SearchHit> = results
                    .into_iter()
                    .map(|r| SearchHit::from_result(r, &redactor))
                    .collect();
                json_ok(SearchResponse {
                    results: hits,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Search failed: {e}"),
            ),
        }
    })
}

// =============================================================================
// /bookmarks
// =============================================================================

#[derive(Serialize)]
pub(super) struct BookmarksResponse {
    pub(super) bookmarks: Vec<BookmarkView>,
    pub(super) total: usize,
}

#[derive(Serialize)]
pub(super) struct BookmarkView {
    pub(super) pane_id: u64,
    pub(super) alias: String,
    pub(super) tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    pub(super) created_at: i64,
    pub(super) updated_at: i64,
}

impl BookmarkView {
    pub(super) fn from_query(bookmark: ui_query::PaneBookmarkView, redactor: &Redactor) -> Self {
        Self {
            pane_id: bookmark.pane_id,
            alias: redactor.redact(&bookmark.alias),
            tags: bookmark
                .tags
                .iter()
                .map(|tag| redactor.redact(tag))
                .collect(),
            description: bookmark.description.map(|desc| redactor.redact(&desc)),
            created_at: bookmark.created_at,
            updated_at: bookmark.updated_at,
        }
    }
}

pub(super) fn handle_bookmarks(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    Box::pin(async move {
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match ui_query::list_pane_bookmarks(&storage).await {
            Ok(bookmarks) => {
                let total = bookmarks.len();
                let views: Vec<BookmarkView> = bookmarks
                    .into_iter()
                    .map(|bookmark| BookmarkView::from_query(bookmark, &redactor))
                    .collect();
                json_ok(BookmarksResponse {
                    bookmarks: views,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Failed to query bookmarks: {e}"),
            ),
        }
    })
}

// =============================================================================
// /ruleset-profile
// =============================================================================

#[derive(Serialize)]
pub(super) struct RulesetProfileResponse {
    pub(super) active_profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) active_last_applied_at: Option<u64>,
    pub(super) profiles: Vec<RulesetProfileView>,
    pub(super) total: usize,
}

#[derive(Serialize)]
pub(super) struct RulesetProfileView {
    pub(super) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_applied_at: Option<u64>,
    pub(super) implicit: bool,
}

impl RulesetProfileView {
    pub(super) fn from_summary(
        summary: crate::rulesets::RulesetProfileSummary,
        redactor: &Redactor,
    ) -> Self {
        Self {
            name: summary.name,
            description: summary.description.map(|d| redactor.redact(&d)),
            path: summary.path.map(|p| redactor.redact(&p)),
            last_applied_at: summary.last_applied_at,
            implicit: summary.implicit,
        }
    }
}

pub(super) fn handle_ruleset_profile(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let redactor = req
        .get_extension::<AppState>()
        .map(|s| Arc::clone(&s.redactor))
        .unwrap_or_else(|| Arc::new(Redactor::new()));
    Box::pin(async move {
        let config_path = crate::config::resolve_config_path(None);
        match ui_query::resolve_ruleset_profile_state(config_path.as_deref()) {
            Ok(state) => {
                let total = state.profiles.len();
                let profiles = state
                    .profiles
                    .into_iter()
                    .map(|profile| RulesetProfileView::from_summary(profile, &redactor))
                    .collect();
                json_ok(RulesetProfileResponse {
                    active_profile: state.active_profile,
                    active_last_applied_at: state.active_last_applied_at,
                    profiles,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "ruleset_profile_error",
                format!("Failed to resolve ruleset profile state: {e}"),
            ),
        }
    })
}

// =============================================================================
// /saved-searches
// =============================================================================

#[derive(Serialize)]
pub(super) struct SavedSearchesResponse {
    pub(super) saved_searches: Vec<SavedSearchView>,
    pub(super) total: usize,
}

#[derive(Serialize)]
pub(super) struct SavedSearchView {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pane_id: Option<u64>,
    pub(super) limit: i64,
    pub(super) since_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) since_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) schedule_interval_ms: Option<i64>,
    pub(super) enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_run_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_result_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) last_error: Option<String>,
    pub(super) created_at: i64,
    pub(super) updated_at: i64,
}

impl SavedSearchView {
    pub(super) fn from_query(saved: ui_query::SavedSearchView, redactor: &Redactor) -> Self {
        Self {
            id: saved.id,
            name: redactor.redact(&saved.name),
            query: redactor.redact(&saved.query),
            pane_id: saved.pane_id,
            limit: saved.limit,
            since_mode: saved.since_mode,
            since_ms: saved.since_ms,
            schedule_interval_ms: saved.schedule_interval_ms,
            enabled: saved.enabled,
            last_run_at: saved.last_run_at,
            last_result_count: saved.last_result_count,
            last_error: saved.last_error.map(|e| redactor.redact(&e)),
            created_at: saved.created_at,
            updated_at: saved.updated_at,
        }
    }
}

pub(super) fn handle_saved_searches(
    req: &Request,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    let result = require_storage(req);
    Box::pin(async move {
        let (storage, redactor) = match result {
            Ok(s) => s,
            Err(resp) => return resp,
        };
        match ui_query::list_saved_searches(&storage).await {
            Ok(saved_searches) => {
                let total = saved_searches.len();
                let views = saved_searches
                    .into_iter()
                    .map(|saved| SavedSearchView::from_query(saved, &redactor))
                    .collect();
                json_ok(SavedSearchesResponse {
                    saved_searches: views,
                    total,
                })
            }
            Err(e) => json_err(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                format!("Failed to query saved searches: {e}"),
            ),
        }
    })
}
