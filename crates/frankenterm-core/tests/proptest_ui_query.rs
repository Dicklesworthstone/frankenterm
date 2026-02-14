//! Property-based tests for ui_query module.
//!
//! Verifies UI query helper invariants:
//! - PaneBookmarkView::from preserves pane_id, alias, description, timestamps
//! - PaneBookmarkView::from: None tags -> empty vec, Some tags preserved exactly
//! - PaneBookmarkView::from ignores the record `id` field (not in view)
//! - SavedSearchView::from preserves all 14 fields faithfully
//! - SavedSearchView field identity: every field matches its source record
//! - RulesetProfileState::default invariants (active="default", single implicit profile)
//! - PaneBookmarkView serialization never panics, produces valid JSON with expected keys
//! - SavedSearchView serialization never panics, produces valid JSON with expected keys
//! - RulesetProfileState serialization never panics
//! - Profile resolution tie-breaking: same timestamp -> lexicographically first name wins
//! - Profile resolution: strictly greater timestamp always wins
//! - Profile resolution: no timestamps -> stays "default"
//! - Serde roundtrip for bookmark records and saved search records

use proptest::prelude::*;

use frankenterm_core::rulesets::RulesetProfileSummary;
use frankenterm_core::storage::{PaneBookmarkRecord, SavedSearchRecord};
use frankenterm_core::ui_query::{PaneBookmarkView, RulesetProfileState, SavedSearchView};

// ────────────────────────────────────────────────────────────────────
// Strategies
// ────────────────────────────────────────────────────────────────────

/// Strategy for non-empty printable strings (aliases, names, queries).
fn arb_name() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_-]{1,32}"
}

/// Strategy for arbitrary tag strings.
fn arb_tag() -> impl Strategy<Value = String> {
    "[a-z]{1,12}"
}

/// Strategy for a tag vector (0..8 tags).
fn arb_tags() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_tag(), 0..8)
}

/// Strategy for optional description.
fn arb_opt_description() -> impl Strategy<Value = Option<String>> {
    prop::option::of("[a-zA-Z0-9 .,!?]{0,64}")
}

/// Strategy for optional string (last_error, etc).
fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
    prop::option::of("[a-zA-Z0-9_ ]{0,32}")
}

/// Strategy for since_mode values.
fn arb_since_mode() -> impl Strategy<Value = String> {
    prop_oneof![Just("last_run".to_string()), Just("fixed".to_string()),]
}

/// Strategy for a PaneBookmarkRecord.
fn arb_bookmark_record() -> impl Strategy<Value = PaneBookmarkRecord> {
    (
        any::<i64>(),                 // id
        any::<u64>(),                 // pane_id
        arb_name(),                   // alias
        prop::option::of(arb_tags()), // tags
        arb_opt_description(),        // description
        any::<i64>(),                 // created_at
        any::<i64>(),                 // updated_at
    )
        .prop_map(
            |(id, pane_id, alias, tags, description, created_at, updated_at)| PaneBookmarkRecord {
                id,
                pane_id,
                alias,
                tags,
                description,
                created_at,
                updated_at,
            },
        )
}

/// Strategy for a SavedSearchRecord — split into nested tuples to stay under
/// the 12-element proptest Strategy limit.
fn arb_saved_search_record() -> impl Strategy<Value = SavedSearchRecord> {
    // Group 1: id, name, query, pane_id, limit, since_mode, since_ms (7 fields)
    let group1 = (
        arb_name(),                       // id
        arb_name(),                       // name
        "[a-zA-Z0-9 *]{1,32}",            // query
        prop::option::of(any::<u64>()),   // pane_id
        1i64..10000i64,                   // limit
        arb_since_mode(),                 // since_mode
        prop::option::of(0i64..i64::MAX), // since_ms
    );

    // Group 2: schedule_interval_ms, enabled, last_run_at, last_result_count,
    //          last_error, created_at, updated_at (7 fields)
    let group2 = (
        prop::option::of(1000i64..3_600_000i64), // schedule_interval_ms
        any::<bool>(),                           // enabled
        prop::option::of(0i64..i64::MAX),        // last_run_at
        prop::option::of(0i64..10000i64),        // last_result_count
        arb_opt_string(),                        // last_error
        any::<i64>(),                            // created_at
        any::<i64>(),                            // updated_at
    );

    (group1, group2).prop_map(
        |(
            (id, name, query, pane_id, limit, since_mode, since_ms),
            (
                schedule_interval_ms,
                enabled,
                last_run_at,
                last_result_count,
                last_error,
                created_at,
                updated_at,
            ),
        )| {
            SavedSearchRecord {
                id,
                name,
                query,
                pane_id,
                limit,
                since_mode,
                since_ms,
                schedule_interval_ms,
                enabled,
                last_run_at,
                last_result_count,
                last_error,
                created_at,
                updated_at,
            }
        },
    )
}

/// Strategy for a RulesetProfileSummary.
fn arb_profile_summary() -> impl Strategy<Value = RulesetProfileSummary> {
    (
        arb_name(),
        arb_opt_description(),
        prop::option::of("[a-z/]{1,32}"),
        prop::option::of(any::<u64>()),
        any::<bool>(),
    )
        .prop_map(|(name, description, path, last_applied_at, implicit)| {
            RulesetProfileSummary {
                name,
                description,
                path,
                last_applied_at,
                implicit,
            }
        })
}

/// Strategy for a RulesetProfileState with 1..6 profiles.
fn arb_profile_state() -> impl Strategy<Value = RulesetProfileState> {
    (
        arb_name(),
        prop::option::of(any::<u64>()),
        prop::collection::vec(arb_profile_summary(), 1..6),
    )
        .prop_map(
            |(active_profile, active_last_applied_at, profiles)| RulesetProfileState {
                active_profile,
                active_last_applied_at,
                profiles,
            },
        )
}

// ────────────────────────────────────────────────────────────────────
// Property tests: PaneBookmarkView::from
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 1. from() preserves pane_id exactly
    #[test]
    fn bookmark_from_preserves_pane_id(record in arb_bookmark_record()) {
        let expected = record.pane_id;
        let view = PaneBookmarkView::from(record);
        prop_assert_eq!(view.pane_id, expected);
    }

    // 2. from() preserves alias exactly
    #[test]
    fn bookmark_from_preserves_alias(record in arb_bookmark_record()) {
        let expected = record.alias.clone();
        let view = PaneBookmarkView::from(record);
        prop_assert_eq!(view.alias, expected);
    }

    // 3. from() preserves description exactly
    #[test]
    fn bookmark_from_preserves_description(record in arb_bookmark_record()) {
        let expected = record.description.clone();
        let view = PaneBookmarkView::from(record);
        prop_assert_eq!(view.description, expected);
    }

    // 4. from() preserves created_at exactly
    #[test]
    fn bookmark_from_preserves_created_at(record in arb_bookmark_record()) {
        let expected = record.created_at;
        let view = PaneBookmarkView::from(record);
        prop_assert_eq!(view.created_at, expected);
    }

    // 5. from() preserves updated_at exactly
    #[test]
    fn bookmark_from_preserves_updated_at(record in arb_bookmark_record()) {
        let expected = record.updated_at;
        let view = PaneBookmarkView::from(record);
        prop_assert_eq!(view.updated_at, expected);
    }

    // 6. None tags → empty vec
    #[test]
    fn bookmark_from_none_tags_yields_empty_vec(
        id in any::<i64>(),
        pane_id in any::<u64>(),
        alias in arb_name(),
        description in arb_opt_description(),
        created_at in any::<i64>(),
        updated_at in any::<i64>(),
    ) {
        let record = PaneBookmarkRecord {
            id,
            pane_id,
            alias,
            tags: None,
            description,
            created_at,
            updated_at,
        };
        let view = PaneBookmarkView::from(record);
        prop_assert!(view.tags.is_empty(), "None tags should become empty vec");
    }

    // 7. Some tags preserved exactly in order
    #[test]
    fn bookmark_from_some_tags_preserved(
        id in any::<i64>(),
        pane_id in any::<u64>(),
        alias in arb_name(),
        tags in arb_tags(),
        created_at in any::<i64>(),
        updated_at in any::<i64>(),
    ) {
        let expected = tags.clone();
        let record = PaneBookmarkRecord {
            id,
            pane_id,
            alias,
            tags: Some(tags),
            description: None,
            created_at,
            updated_at,
        };
        let view = PaneBookmarkView::from(record);
        prop_assert_eq!(view.tags.len(), expected.len(), "tag count mismatch");
        for (i, (got, want)) in view.tags.iter().zip(expected.iter()).enumerate() {
            prop_assert_eq!(got, want, "tag mismatch at index {}", i);
        }
    }

    // 8. from() drops the record `id` field — PaneBookmarkView has no `id`
    #[test]
    fn bookmark_from_drops_record_id(record in arb_bookmark_record()) {
        // PaneBookmarkView does not have an `id` field; we verify the JSON
        // output does not contain an "id" key at the top level.
        let view = PaneBookmarkView::from(record);
        let json = serde_json::to_value(&view).unwrap();
        prop_assert!(json.get("id").is_none(), "PaneBookmarkView should not expose record id");
    }

    // 9. Serialization never panics and produces an object with expected keys
    #[test]
    fn bookmark_serialization_valid_json(record in arb_bookmark_record()) {
        let view = PaneBookmarkView::from(record);
        let json = serde_json::to_value(&view).unwrap();
        let obj = json.as_object().unwrap();
        prop_assert!(obj.contains_key("pane_id"), "missing pane_id key");
        prop_assert!(obj.contains_key("alias"), "missing alias key");
        prop_assert!(obj.contains_key("tags"), "missing tags key");
        prop_assert!(obj.contains_key("description"), "missing description key");
        prop_assert!(obj.contains_key("created_at"), "missing created_at key");
        prop_assert!(obj.contains_key("updated_at"), "missing updated_at key");
        // Exactly 6 keys
        prop_assert_eq!(obj.len(), 6, "PaneBookmarkView should have exactly 6 JSON keys");
    }

    // 10. tags is always a JSON array
    #[test]
    fn bookmark_tags_always_json_array(record in arb_bookmark_record()) {
        let view = PaneBookmarkView::from(record);
        let json = serde_json::to_value(&view).unwrap();
        prop_assert!(json["tags"].is_array(), "tags should be a JSON array");
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: SavedSearchView::from
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 11. from() preserves all 14 fields
    #[test]
    fn saved_search_from_preserves_all_fields(record in arb_saved_search_record()) {
        let expected_id = record.id.clone();
        let expected_name = record.name.clone();
        let expected_query = record.query.clone();
        let expected_pane_id = record.pane_id;
        let expected_limit = record.limit;
        let expected_since_mode = record.since_mode.clone();
        let expected_since_ms = record.since_ms;
        let expected_schedule = record.schedule_interval_ms;
        let expected_enabled = record.enabled;
        let expected_last_run = record.last_run_at;
        let expected_last_count = record.last_result_count;
        let expected_last_error = record.last_error.clone();
        let expected_created = record.created_at;
        let expected_updated = record.updated_at;

        let view = SavedSearchView::from(record);

        prop_assert_eq!(&view.id, &expected_id, "id mismatch");
        prop_assert_eq!(&view.name, &expected_name, "name mismatch");
        prop_assert_eq!(&view.query, &expected_query, "query mismatch");
        prop_assert_eq!(view.pane_id, expected_pane_id, "pane_id mismatch");
        prop_assert_eq!(view.limit, expected_limit, "limit mismatch");
        prop_assert_eq!(&view.since_mode, &expected_since_mode, "since_mode mismatch");
        prop_assert_eq!(view.since_ms, expected_since_ms, "since_ms mismatch");
        prop_assert_eq!(view.schedule_interval_ms, expected_schedule, "schedule_interval_ms mismatch");
        prop_assert_eq!(view.enabled, expected_enabled, "enabled mismatch");
        prop_assert_eq!(view.last_run_at, expected_last_run, "last_run_at mismatch");
        prop_assert_eq!(view.last_result_count, expected_last_count, "last_result_count mismatch");
        prop_assert_eq!(view.last_error, expected_last_error, "last_error mismatch");
        prop_assert_eq!(view.created_at, expected_created, "created_at mismatch");
        prop_assert_eq!(view.updated_at, expected_updated, "updated_at mismatch");
    }

    // 12. Serialization never panics
    #[test]
    fn saved_search_serialization_valid_json(record in arb_saved_search_record()) {
        let view = SavedSearchView::from(record);
        let json = serde_json::to_value(&view).unwrap();
        let obj = json.as_object().unwrap();
        prop_assert_eq!(obj.len(), 14, "SavedSearchView should have exactly 14 JSON keys");
    }

    // 13. JSON keys match struct field names
    #[test]
    fn saved_search_json_has_expected_keys(record in arb_saved_search_record()) {
        let view = SavedSearchView::from(record);
        let json = serde_json::to_value(&view).unwrap();
        let obj = json.as_object().unwrap();

        let expected_keys = [
            "id", "name", "query", "pane_id", "limit", "since_mode",
            "since_ms", "schedule_interval_ms", "enabled", "last_run_at",
            "last_result_count", "last_error", "created_at", "updated_at",
        ];
        for key in &expected_keys {
            prop_assert!(obj.contains_key(*key), "missing key: {}", key);
        }
    }

    // 14. enabled field serializes as JSON boolean
    #[test]
    fn saved_search_enabled_is_json_bool(record in arb_saved_search_record()) {
        let view = SavedSearchView::from(record);
        let json = serde_json::to_value(&view).unwrap();
        prop_assert!(json["enabled"].is_boolean(), "enabled should serialize as boolean");
    }

    // 15. limit field serializes as JSON number
    #[test]
    fn saved_search_limit_is_json_number(record in arb_saved_search_record()) {
        let view = SavedSearchView::from(record);
        let json = serde_json::to_value(&view).unwrap();
        prop_assert!(json["limit"].is_number(), "limit should serialize as number");
    }

    // 16. since_mode is one of known values from strategy
    #[test]
    fn saved_search_since_mode_is_known(record in arb_saved_search_record()) {
        let view = SavedSearchView::from(record);
        let valid_modes = ["last_run", "fixed"];
        prop_assert!(
            valid_modes.contains(&view.since_mode.as_str()),
            "since_mode should be a known value, got: {}",
            view.since_mode
        );
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: RulesetProfileState
// ────────────────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 17. Default invariants: active_profile is "default"
    #[test]
    fn profile_state_default_active_is_default(_dummy in 0u8..1u8) {
        let state = RulesetProfileState::default();
        prop_assert_eq!(&state.active_profile, "default");
    }

    // 18. Default invariants: active_last_applied_at is None
    #[test]
    fn profile_state_default_no_last_applied(_dummy in 0u8..1u8) {
        let state = RulesetProfileState::default();
        prop_assert!(state.active_last_applied_at.is_none());
    }

    // 19. Default invariants: exactly one profile, named "default", implicit=true
    #[test]
    fn profile_state_default_single_implicit_profile(_dummy in 0u8..1u8) {
        let state = RulesetProfileState::default();
        prop_assert_eq!(state.profiles.len(), 1, "default state should have 1 profile");
        prop_assert_eq!(&state.profiles[0].name, "default");
        prop_assert!(state.profiles[0].implicit, "default profile should be implicit");
        prop_assert!(state.profiles[0].path.is_none(), "default profile should have no path");
    }

    // 20. Serialization of arbitrary RulesetProfileState never panics
    #[test]
    fn profile_state_serialization_valid_json(state in arb_profile_state()) {
        let json = serde_json::to_value(&state).unwrap();
        let obj = json.as_object().unwrap();
        prop_assert!(obj.contains_key("active_profile"), "missing active_profile");
        prop_assert!(obj.contains_key("active_last_applied_at"), "missing active_last_applied_at");
        prop_assert!(obj.contains_key("profiles"), "missing profiles");
        prop_assert!(json["profiles"].is_array(), "profiles should be an array");
    }

    // 21. profiles array in JSON has same length as struct profiles vec
    #[test]
    fn profile_state_json_profiles_length_matches(state in arb_profile_state()) {
        let expected_len = state.profiles.len();
        let json = serde_json::to_value(&state).unwrap();
        let arr = json["profiles"].as_array().unwrap();
        prop_assert_eq!(arr.len(), expected_len, "JSON profiles array length should match struct");
    }
}

// ────────────────────────────────────────────────────────────────────
// Property tests: Profile resolution tie-breaking logic
// ────────────────────────────────────────────────────────────────────

/// Simulate the active-profile resolution algorithm from ui_query.rs
/// using RulesetProfileSummary structs directly (no filesystem needed).
fn resolve_active_profile(profiles: &[RulesetProfileSummary]) -> (String, Option<u64>) {
    let mut active_profile = "default".to_string();
    let mut active_last_applied_at: Option<u64> = None;

    for profile in profiles {
        let Some(ts) = profile.last_applied_at else {
            continue;
        };
        match active_last_applied_at {
            None => {
                active_last_applied_at = Some(ts);
                active_profile.clone_from(&profile.name);
            }
            Some(current) if ts > current => {
                active_last_applied_at = Some(ts);
                active_profile.clone_from(&profile.name);
            }
            Some(current) if ts == current && profile.name < active_profile => {
                active_last_applied_at = Some(ts);
                active_profile.clone_from(&profile.name);
            }
            Some(_) => {}
        }
    }

    (active_profile, active_last_applied_at)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // 22. Tie-breaking: when two profiles share the same timestamp, lexicographically
    // first name wins.
    #[test]
    fn profile_tie_breaks_lexicographically(
        ts in any::<u64>(),
        name_a in "[a-z]{1,8}",
        name_b in "[a-z]{1,8}",
    ) {
        // Ensure distinct names to make the test meaningful.
        prop_assume!(name_a != name_b);

        let profiles = vec![
            RulesetProfileSummary {
                name: name_a.clone(),
                description: None,
                path: None,
                last_applied_at: Some(ts),
                implicit: false,
            },
            RulesetProfileSummary {
                name: name_b.clone(),
                description: None,
                path: None,
                last_applied_at: Some(ts),
                implicit: false,
            },
        ];

        let (active, _) = resolve_active_profile(&profiles);
        let expected = std::cmp::min_by(name_a, name_b, |a: &String, b: &String| a.cmp(b));
        prop_assert_eq!(active, expected, "tie should be broken lexicographically");
    }

    // 23. Strictly greater timestamp always wins regardless of name ordering.
    #[test]
    fn profile_greater_timestamp_wins(
        ts_low in 0u64..u64::MAX / 2,
        delta in 1u64..1_000_000u64,
        name_first in "[a-z]{1,8}",
        name_second in "[a-z]{1,8}",
    ) {
        let ts_high = ts_low + delta;
        // Even if name_first is lexicographically before name_second,
        // the higher timestamp wins.
        let profiles = vec![
            RulesetProfileSummary {
                name: name_first,
                description: None,
                path: None,
                last_applied_at: Some(ts_low),
                implicit: false,
            },
            RulesetProfileSummary {
                name: name_second.clone(),
                description: None,
                path: None,
                last_applied_at: Some(ts_high),
                implicit: false,
            },
        ];

        let (active, active_ts) = resolve_active_profile(&profiles);
        prop_assert_eq!(&active, &name_second, "higher timestamp should win");
        prop_assert_eq!(active_ts, Some(ts_high));
    }

    // 24. No timestamps at all → stays "default"
    #[test]
    fn profile_no_timestamps_stays_default(
        profiles in prop::collection::vec(
            arb_name().prop_map(|name| RulesetProfileSummary {
                name,
                description: None,
                path: None,
                last_applied_at: None,
                implicit: false,
            }),
            0..8,
        )
    ) {
        let (active, ts) = resolve_active_profile(&profiles);
        prop_assert_eq!(&active, "default", "with no timestamps, active should be default");
        prop_assert!(ts.is_none(), "with no timestamps, active_last_applied_at should be None");
    }

    // 25. Resolution is idempotent: running it twice gives the same result
    #[test]
    fn profile_resolution_idempotent(
        profiles in prop::collection::vec(arb_profile_summary(), 0..8)
    ) {
        let (active1, ts1) = resolve_active_profile(&profiles);
        let (active2, ts2) = resolve_active_profile(&profiles);
        prop_assert_eq!(active1, active2, "resolution should be idempotent for active_profile");
        prop_assert_eq!(ts1, ts2, "resolution should be idempotent for timestamp");
    }
}
