//! HTTP handler extraction surface for Wave 4B migration.
//!
//! These handlers are extracted from `web.rs` in a strangler-fig migration to
//! keep behavior stable while reducing monolith size.

use super::error::{json_err, json_ok};
use super::extractors::{parse_bool, parse_i64, parse_limit, parse_u64, require_storage};
use super::middleware::AppState;
use crate::VERSION;
use crate::policy::Redactor;
use crate::storage::{EventQuery, PaneRecord, SearchOptions, SearchResult};
use crate::ui_query;
use crate::web_framework::{QueryString, Request, Response, StatusCode};
use serde::Serialize;
use std::sync::Arc;

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
        // ft-xbnl0.2.3 tick 257: cx-first web events handler.
        let events_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        match storage.get_events_with_cx(&events_cx, query).await {
            Ok(events) => {
                let total = events.len();
                let mut views: Vec<EventView> = Vec::with_capacity(total);
                for event in events {
                    let annotations = match storage
                        .get_event_annotations_with_cx(&events_cx, event.id)
                        .await
                    {
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
        // ft-xbnl0.2.3 tick 257: cx-first web search handler.
        let search_cx = crate::cx::Cx::current().unwrap_or_else(crate::cx::for_request);
        match storage
            .search_with_results_with_cx(&search_cx, &query, options)
            .await
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rulesets::RulesetProfileSummary;
    use crate::storage::{
        EventAnnotations, PaneBookmarkRecord, SavedSearchRecord, SearchResult, Segment,
        StorageHandle, StoredEvent,
    };
    use crate::web_framework::{Method, ResponseBody};
    use serde_json::Value;
    use std::sync::Arc;

    fn run_async_test<F>(future: F)
    where
        F: std::future::Future<Output = ()>,
    {
        use crate::runtime_compat::CompatRuntime;
        let runtime = crate::runtime_compat::RuntimeBuilder::current_thread()
            .enable_all()
            .build()
            .expect("failed to build web handlers test runtime");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(future);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(runtime);
        }));
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            crate::runtime_compat::clear_runtime_handle();
        }));
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    }

    async fn temp_storage() -> (tempfile::TempDir, StorageHandle) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db_path = dir.path().join("ft.db");
        let storage = StorageHandle::new(&db_path.to_string_lossy())
            .await
            .expect("storage handle");
        (dir, storage)
    }

    fn make_request(path: &str, query: Option<&str>, storage: Option<StorageHandle>) -> Request {
        let mut req = Request::new(Method::GET, path);
        if let Some(query) = query {
            req.set_query(Some(query.to_string()));
        }
        req.insert_extension(super::super::middleware::AppState {
            storage,
            event_bus: None,
            redactor: Arc::new(Redactor::new()),
        });
        req
    }

    fn response_json(response: Response) -> Value {
        let (_, _, body) = response.into_parts();
        match body {
            ResponseBody::Bytes(bytes) => serde_json::from_slice(&bytes).expect("json body"),
            ResponseBody::Empty => panic!("expected JSON response body"),
            ResponseBody::Stream(_) => panic!("expected non-streaming response body"),
        }
    }

    fn sample_pane(pane_id: u64) -> PaneRecord {
        PaneRecord {
            pane_id,
            pane_uuid: Some(format!("pane-{pane_id}-uuid")),
            domain: "local".to_string(),
            window_id: Some(7),
            tab_id: Some(3),
            title: Some(format!("pane-{pane_id}")),
            cwd: Some(format!("/tmp/pane-{pane_id}")),
            tty_name: None,
            first_seen_at: 1_700_000_000_000,
            last_seen_at: 1_700_000_000_123,
            observed: true,
            ignore_reason: None,
            last_decision_at: Some(1_700_000_000_100),
        }
    }

    fn sample_event(pane_id: u64) -> StoredEvent {
        StoredEvent {
            id: 0,
            pane_id,
            rule_id: "claude_code.usage.reached".to_string(),
            agent_type: "claude_code".to_string(),
            event_type: "usage".to_string(),
            severity: "warning".to_string(),
            confidence: 0.91,
            extracted: Some(serde_json::json!({"remaining": 1})),
            matched_text: Some("usage warning".to_string()),
            segment_id: None,
            detected_at: 1_700_000_000_500,
            dedupe_key: Some("dedupe-1".to_string()),
            handled_at: None,
            handled_by_workflow_id: None,
            handled_status: None,
        }
    }

    #[test]
    fn health_response_serializes_ok_and_version() {
        let response = health_response();
        assert_eq!(response.status(), StatusCode::OK);
        let json = response_json(response);
        assert_eq!(json["ok"], true);
        assert_eq!(json["version"], VERSION);
    }

    #[test]
    fn pane_view_from_record_maps_optional_fields() {
        let pane = PaneView::from_record(sample_pane(42), &Redactor::new());
        assert_eq!(pane.pane_id, 42);
        assert_eq!(pane.pane_uuid.as_deref(), Some("pane-42-uuid"));
        assert_eq!(pane.domain, "local");
        assert_eq!(pane.title.as_deref(), Some("pane-42"));
        assert_eq!(pane.cwd.as_deref(), Some("/tmp/pane-42"));
        assert_eq!(pane.window_id, Some(7));
        assert_eq!(pane.tab_id, Some(3));
    }

    #[test]
    fn event_annotations_view_from_stored_maps_labels_and_note() {
        let annotations = EventAnnotationsView::from_stored(
            EventAnnotations {
                triage_state: Some("needs_review".to_string()),
                triage_updated_at: Some(10),
                triage_updated_by: Some("agent".to_string()),
                note: Some("operator note".to_string()),
                note_updated_at: Some(20),
                note_updated_by: Some("agent".to_string()),
                labels: vec!["alpha".to_string(), "beta".to_string()],
            },
            &Redactor::new(),
        );
        assert_eq!(annotations.triage_state.as_deref(), Some("needs_review"));
        assert_eq!(annotations.note.as_deref(), Some("operator note"));
        assert_eq!(annotations.labels, vec!["alpha", "beta"]);
    }

    #[test]
    fn event_view_from_stored_carries_annotations_and_text() {
        let view = EventView::from_stored(
            sample_event(7),
            &Redactor::new(),
            Some(EventAnnotationsView {
                triage_state: Some("open".to_string()),
                note: None,
                labels: vec!["triage".to_string()],
            }),
        );
        assert_eq!(view.pane_id, 7);
        assert_eq!(view.rule_id, "claude_code.usage.reached");
        assert_eq!(view.matched_text.as_deref(), Some("usage warning"));
        assert_eq!(
            view.annotations.as_ref().map(|a| a.labels.clone()),
            Some(vec!["triage".to_string()])
        );
    }

    #[test]
    fn search_hit_from_result_uses_segment_metadata() {
        let hit = SearchHit::from_result(
            SearchResult {
                segment: Segment {
                    id: 9,
                    pane_id: 4,
                    seq: 2,
                    content: "needle in haystack".to_string(),
                    content_len: 18,
                    content_hash: None,
                    captured_at: 1_700_000_000_777,
                },
                snippet: Some("needle".to_string()),
                highlight: None,
                score: 0.42,
            },
            &Redactor::new(),
        );
        assert_eq!(hit.segment_id, 9);
        assert_eq!(hit.pane_id, 4);
        assert_eq!(hit.snippet.as_deref(), Some("needle"));
        assert_eq!(hit.content_len, 18);
        assert_eq!(hit.score, 0.42);
    }

    #[test]
    fn bookmark_view_from_query_maps_fields() {
        let view = BookmarkView::from_query(
            crate::ui_query::PaneBookmarkView {
                pane_id: 11,
                alias: "ops".to_string(),
                tags: vec!["prod".to_string(), "ssh".to_string()],
                description: Some("primary pane".to_string()),
                created_at: 100,
                updated_at: 200,
            },
            &Redactor::new(),
        );
        assert_eq!(view.pane_id, 11);
        assert_eq!(view.alias, "ops");
        assert_eq!(view.tags, vec!["prod", "ssh"]);
        assert_eq!(view.description.as_deref(), Some("primary pane"));
        assert_eq!(view.created_at, 100);
        assert_eq!(view.updated_at, 200);
    }

    #[test]
    fn ruleset_profile_view_from_summary_maps_fields() {
        let view = RulesetProfileView::from_summary(
            RulesetProfileSummary {
                name: "default".to_string(),
                description: Some("Default profile".to_string()),
                path: Some("/tmp/rulesets/default.toml".to_string()),
                last_applied_at: Some(123),
                implicit: false,
            },
            &Redactor::new(),
        );
        assert_eq!(view.name, "default");
        assert_eq!(view.description.as_deref(), Some("Default profile"));
        assert_eq!(view.path.as_deref(), Some("/tmp/rulesets/default.toml"));
        assert_eq!(view.last_applied_at, Some(123));
        assert!(!view.implicit);
    }

    #[test]
    fn saved_search_view_from_query_maps_fields() {
        let view = SavedSearchView::from_query(
            crate::ui_query::SavedSearchView {
                id: "ss-1".to_string(),
                name: "Failures".to_string(),
                query: "error".to_string(),
                pane_id: Some(5),
                limit: 25,
                since_mode: "last_run".to_string(),
                since_ms: None,
                schedule_interval_ms: Some(60_000),
                enabled: true,
                last_run_at: Some(200),
                last_result_count: Some(3),
                last_error: Some("boom".to_string()),
                created_at: 100,
                updated_at: 200,
            },
            &Redactor::new(),
        );
        assert_eq!(view.id, "ss-1");
        assert_eq!(view.name, "Failures");
        assert_eq!(view.query, "error");
        assert_eq!(view.pane_id, Some(5));
        assert_eq!(view.schedule_interval_ms, Some(60_000));
        assert_eq!(view.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn handle_panes_without_state_returns_internal_error() {
        run_async_test(async {
            let req = Request::new(Method::GET, "/panes");
            let response = handle_panes(&req).await;
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let json = response_json(response);
            assert_eq!(json["ok"], false);
            assert_eq!(json["error_code"], "internal_error");
        });
    }

    #[test]
    fn handle_search_without_query_returns_bad_request() {
        run_async_test(async {
            let req = Request::new(Method::GET, "/search");
            let response = handle_search(&req).await;
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let json = response_json(response);
            assert_eq!(json["ok"], false);
            assert_eq!(json["error_code"], "missing_query");
        });
    }

    #[test]
    fn handle_panes_with_storage_returns_serialized_panes() {
        run_async_test(async {
            let (_dir, storage) = temp_storage().await;
            storage
                .upsert_pane(sample_pane(21))
                .await
                .expect("upsert pane");

            let req = make_request("/panes", None, Some(storage.clone()));
            let response = handle_panes(&req).await;
            assert_eq!(response.status(), StatusCode::OK);

            let json = response_json(response);
            assert_eq!(json["ok"], true);
            assert_eq!(json["data"]["total"], 1);
            assert_eq!(json["data"]["panes"][0]["pane_id"], 21);
            assert_eq!(json["data"]["panes"][0]["cwd"], "/tmp/pane-21");

            storage.shutdown().await.expect("shutdown storage");
        });
    }

    #[test]
    fn handle_events_with_storage_returns_annotations() {
        run_async_test(async {
            let (_dir, storage) = temp_storage().await;
            storage
                .upsert_pane(sample_pane(8))
                .await
                .expect("upsert pane");
            let event_id = storage
                .record_event(sample_event(8))
                .await
                .expect("record event");
            storage
                .set_event_triage_state(
                    event_id,
                    Some("needs_review".to_string()),
                    Some("tester".to_string()),
                )
                .await
                .expect("triage state");
            storage
                .set_event_note(
                    event_id,
                    Some("operator note".to_string()),
                    Some("tester".to_string()),
                )
                .await
                .expect("event note");
            storage
                .add_event_label(event_id, "urgent".to_string(), Some("tester".to_string()))
                .await
                .expect("event label");

            let req = make_request(
                "/events",
                Some("pane_id=not-a-number&since=oops&until=nah&unhandled=yes"),
                Some(storage.clone()),
            );
            let response = handle_events(&req).await;
            assert_eq!(response.status(), StatusCode::OK);

            let json = response_json(response);
            assert_eq!(json["data"]["total"], 1);
            assert_eq!(json["data"]["events"][0]["pane_id"], 8);
            assert_eq!(
                json["data"]["events"][0]["annotations"]["triage_state"],
                "needs_review"
            );
            assert_eq!(
                json["data"]["events"][0]["annotations"]["note"],
                "operator note"
            );
            assert_eq!(
                json["data"]["events"][0]["annotations"]["labels"][0],
                "urgent"
            );

            storage.shutdown().await.expect("shutdown storage");
        });
    }

    #[test]
    fn handle_search_with_storage_returns_search_hits() {
        run_async_test(async {
            let (_dir, storage) = temp_storage().await;
            storage
                .upsert_pane(sample_pane(5))
                .await
                .expect("upsert pane");
            storage
                .append_segment(5, "needle in haystack", None)
                .await
                .expect("append segment");

            let req = make_request("/search", Some("q=needle"), Some(storage.clone()));
            let response = handle_search(&req).await;
            assert_eq!(response.status(), StatusCode::OK);

            let json = response_json(response);
            assert_eq!(json["data"]["total"], 1);
            assert_eq!(json["data"]["results"][0]["pane_id"], 5);
            assert_eq!(json["data"]["results"][0]["content_len"], 18);

            storage.shutdown().await.expect("shutdown storage");
        });
    }

    #[test]
    fn handle_bookmarks_with_storage_returns_serialized_bookmarks() {
        run_async_test(async {
            let (_dir, storage) = temp_storage().await;
            storage
                .upsert_pane(sample_pane(3))
                .await
                .expect("upsert pane");
            storage
                .insert_pane_bookmark(PaneBookmarkRecord {
                    id: 0,
                    pane_id: 3,
                    alias: "ops".to_string(),
                    tags: Some(vec!["prod".to_string(), "ssh".to_string()]),
                    description: Some("primary".to_string()),
                    created_at: 10,
                    updated_at: 20,
                })
                .await
                .expect("insert bookmark");

            let req = make_request("/bookmarks", None, Some(storage.clone()));
            let response = handle_bookmarks(&req).await;
            assert_eq!(response.status(), StatusCode::OK);

            let json = response_json(response);
            assert_eq!(json["data"]["total"], 1);
            assert_eq!(json["data"]["bookmarks"][0]["alias"], "ops");
            assert_eq!(json["data"]["bookmarks"][0]["tags"][0], "prod");
            assert_eq!(json["data"]["bookmarks"][0]["description"], "primary");

            storage.shutdown().await.expect("shutdown storage");
        });
    }

    #[test]
    fn handle_saved_searches_with_storage_returns_serialized_saved_searches() {
        run_async_test(async {
            let (_dir, storage) = temp_storage().await;
            storage
                .insert_saved_search(SavedSearchRecord::new(
                    "Failures".to_string(),
                    "error".to_string(),
                    Some(9),
                    25,
                    "last_run".to_string(),
                    None,
                ))
                .await
                .expect("insert saved search");

            let req = make_request("/saved-searches", None, Some(storage.clone()));
            let response = handle_saved_searches(&req).await;
            assert_eq!(response.status(), StatusCode::OK);

            let json = response_json(response);
            assert_eq!(json["data"]["total"], 1);
            assert_eq!(json["data"]["saved_searches"][0]["name"], "Failures");
            assert_eq!(json["data"]["saved_searches"][0]["query"], "error");
            assert_eq!(json["data"]["saved_searches"][0]["pane_id"], 9);

            storage.shutdown().await.expect("shutdown storage");
        });
    }
}
