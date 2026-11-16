//! Property-based tests for suggestions module
//!
//! Tests invariants for levenshtein_distance (identity, symmetry, triangle inequality,
//! bounds), suggest_closest (exact/case/prefix/empty), format_available (truncation),
//! PaneInfo (Display, builder), StateChange (time_ago), UserHistory (record/query),
//! Priority (ordering), SuggestionType (Display), SuggestionId (Display/as_str),
//! Suggestion (builder), DismissedStore (permanent/temporary), SuggestionEngine
//! (disabled/max/dismissed/priority), SuggestionContext (suggest_pane/workflow/rule).

use frankenterm_core::suggestions::*;
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

/// Generate arbitrary non-empty strings for levenshtein tests
fn arb_word() -> impl Strategy<Value = String> {
    "[a-z]{1,15}"
}

/// Generate arbitrary Platform variant
fn arb_platform() -> impl Strategy<Value = Platform> {
    prop_oneof![
        Just(Platform::MacOS),
        Just(Platform::Linux),
        Just(Platform::Windows),
        Just(Platform::Container),
        Just(Platform::Unknown),
    ]
}

/// Generate arbitrary Priority variant
fn arb_priority() -> impl Strategy<Value = Priority> {
    prop_oneof![
        Just(Priority::Low),
        Just(Priority::Medium),
        Just(Priority::High),
        Just(Priority::Critical),
    ]
}

/// Generate arbitrary SuggestionType variant
fn arb_suggestion_type() -> impl Strategy<Value = SuggestionType> {
    prop_oneof![
        Just(SuggestionType::NextStep),
        Just(SuggestionType::Optimization),
        Just(SuggestionType::Warning),
        Just(SuggestionType::Tip),
        Just(SuggestionType::Recovery),
    ]
}

/// Generate a PaneInfo with optional fields
fn arb_pane_info() -> impl Strategy<Value = PaneInfo> {
    (
        0..1000u64,
        proptest::option::of("[a-zA-Z ]{1,20}"),
        proptest::option::of("[a-z]{1,10}"),
        proptest::bool::ANY,
    )
        .prop_map(|(id, title, domain, is_alt)| {
            let mut p = PaneInfo::new(id);
            if let Some(t) = title {
                p = p.with_title(t);
            }
            if let Some(d) = domain {
                p = p.with_domain(d);
            }
            p = p.with_alt_screen(is_alt);
            p
        })
}

// ============================================================================
// Property Tests: levenshtein_distance
// ============================================================================

proptest! {
    /// Property 1: Identity — distance from a string to itself is 0.
    #[test]
    fn prop_levenshtein_identity(s in arb_word()) {
        prop_assert_eq!(levenshtein_distance(&s, &s), 0,
            "distance from '{}' to itself should be 0", s);
    }

    /// Property 2: Symmetry — d(a, b) == d(b, a).
    #[test]
    fn prop_levenshtein_symmetry(a in arb_word(), b in arb_word()) {
        let d_ab = levenshtein_distance(&a, &b);
        let d_ba = levenshtein_distance(&b, &a);
        prop_assert_eq!(d_ab, d_ba,
            "d('{}','{}')={} != d('{}','{}')={}", a, b, d_ab, b, a, d_ba);
    }

    /// Property 3: Lower bound — d(a, b) >= |len(a) - len(b)|.
    #[test]
    fn prop_levenshtein_lower_bound(a in arb_word(), b in arb_word()) {
        let d = levenshtein_distance(&a, &b);
        let len_diff = a.len().abs_diff(b.len());
        prop_assert!(d >= len_diff,
            "d('{}','{}')={} < |{}-{}|={}", a, b, d, a.len(), b.len(), len_diff);
    }

    /// Property 4: Upper bound — d(a, b) <= max(len(a), len(b)).
    #[test]
    fn prop_levenshtein_upper_bound(a in arb_word(), b in arb_word()) {
        let d = levenshtein_distance(&a, &b);
        let upper = a.len().max(b.len());
        prop_assert!(d <= upper,
            "d('{}','{}')={} > max({},{})={}", a, b, d, a.len(), b.len(), upper);
    }

    /// Property 5: Empty string — d("", b) = len(b) and d(a, "") = len(a).
    #[test]
    fn prop_levenshtein_empty(s in arb_word()) {
        prop_assert_eq!(levenshtein_distance("", &s), s.len(),
            "d('', '{}') should be {}", s, s.len());
        prop_assert_eq!(levenshtein_distance(&s, ""), s.len(),
            "d('{}', '') should be {}", s, s.len());
    }

    /// Property 6: Triangle inequality — d(a, c) <= d(a, b) + d(b, c).
    #[test]
    fn prop_levenshtein_triangle_inequality(
        a in arb_word(),
        b in arb_word(),
        c in arb_word(),
    ) {
        let d_ac = levenshtein_distance(&a, &c);
        let d_ab = levenshtein_distance(&a, &b);
        let d_bc = levenshtein_distance(&b, &c);
        prop_assert!(d_ac <= d_ab + d_bc,
            "d('{}','{}')={} > d('{}','{}')={} + d('{}','{}')={}",
            a, c, d_ac, a, b, d_ab, b, c, d_bc);
    }

    /// Property 7: Single character difference — adjacent strings have distance 1.
    #[test]
    fn prop_levenshtein_single_char_append(s in "[a-z]{1,10}", c in "[a-z]") {
        let extended = format!("{}{}", s, c);
        let d = levenshtein_distance(&s, &extended);
        prop_assert_eq!(d, 1,
            "appending one char should give distance 1: d('{}','{}')={}", s, extended, d);
    }

    // ========================================================================
    // Property Tests: suggest_closest
    // ========================================================================

    /// Property 8: Exact match always returns the match.
    #[test]
    fn prop_suggest_closest_exact(
        target in "[a-z]{3,10}",
        others in proptest::collection::vec("[a-z]{3,10}", 1..5),
    ) {
        let mut candidates: Vec<String> = others;
        candidates.push(target.clone());
        let result = suggest_closest(&target, &candidates);
        prop_assert_eq!(result, Some(target.as_str()),
            "exact match should be returned");
    }

    /// Property 9: Empty candidates returns None.
    #[test]
    fn prop_suggest_closest_empty(input in "[a-z]{1,10}") {
        let candidates: Vec<String> = vec![];
        prop_assert_eq!(suggest_closest(&input, &candidates), None,
            "empty candidates should return None");
    }

    /// Property 10: Case-insensitive exact match returns the candidate.
    #[test]
    fn prop_suggest_closest_case_insensitive(target in "[a-z]{3,10}") {
        let upper = target.to_uppercase();
        let candidates = vec![upper.clone()];
        let result = suggest_closest(&target, &candidates);
        prop_assert_eq!(result, Some(upper.as_str()),
            "case-insensitive match should work for '{}' vs '{}'", target, upper);
    }

    /// Property 11: Prefix match is returned when available.
    #[test]
    fn prop_suggest_closest_prefix(
        prefix in "[a-z]{3,6}",
        suffix in "[a-z]{3,6}",
    ) {
        let full = format!("{}{}", prefix, suffix);
        let candidates = vec![full.clone()];
        let result = suggest_closest(&prefix, &candidates);
        prop_assert_eq!(result, Some(full.as_str()),
            "prefix '{}' should match '{}'", prefix, full);
    }

    // ========================================================================
    // Property Tests: format_available
    // ========================================================================

    /// Property 12: Empty list returns empty string.
    #[test]
    fn prop_format_available_empty(_dummy in Just(())) {
        let items: Vec<&str> = vec![];
        prop_assert_eq!(format_available(&items), "");
    }

    /// Property 13: Single item returns just that item.
    #[test]
    fn prop_format_available_single(item in "[a-z]{1,10}") {
        let items = vec![item.clone()];
        prop_assert_eq!(format_available(&items), item);
    }

    /// Property 14: Lists of <= 10 items don't have "more".
    #[test]
    fn prop_format_available_short_no_more(n in 1..10usize) {
        let items: Vec<String> = (0..n).map(|i| format!("item_{}", i)).collect();
        let result = format_available(&items);
        prop_assert!(!result.contains("more"),
            "list of {} items should not have 'more': {}", n, result);
    }

    /// Property 15: Lists of > 10 items contain "more".
    #[test]
    fn prop_format_available_long_has_more(extra in 1..10usize) {
        let n = 10 + extra;
        let items: Vec<String> = (0..n).map(|i| format!("item_{}", i)).collect();
        let result = format_available(&items);
        prop_assert!(result.contains("more"),
            "list of {} items should contain 'more': {}", n, result);
        let expected_remaining = format!("{} more", extra);
        prop_assert!(result.contains(&expected_remaining),
            "should say '{}': {}", expected_remaining, result);
    }

    // ========================================================================
    // Property Tests: PaneInfo
    // ========================================================================

    /// Property 16: PaneInfo Display always contains the pane ID.
    #[test]
    fn prop_pane_info_display_contains_id(pane in arb_pane_info()) {
        let display = format!("{}", pane);
        prop_assert!(display.contains(&pane.id.to_string()),
            "display '{}' should contain id {}", display, pane.id);
    }

    /// Property 17: PaneInfo with title shows title in parentheses.
    #[test]
    fn prop_pane_info_display_title(
        id in 0..1000u64,
        title in "[a-zA-Z]{1,10}",
    ) {
        let pane = PaneInfo::new(id).with_title(&title);
        let display = format!("{}", pane);
        prop_assert!(display.contains(&format!("({})", title)),
            "display '{}' should contain '({})'", display, title);
    }

    /// Property 18: PaneInfo builder chain preserves all fields.
    #[test]
    fn prop_pane_info_builder(
        id in 0..1000u64,
        title in "[a-z]{1,10}",
        domain in "[a-z]{1,10}",
        is_alt in proptest::bool::ANY,
    ) {
        let pane = PaneInfo::new(id)
            .with_title(&title)
            .with_domain(&domain)
            .with_alt_screen(is_alt);
        prop_assert_eq!(pane.id, id);
        prop_assert_eq!(pane.title.as_deref(), Some(title.as_str()));
        prop_assert_eq!(pane.domain.as_deref(), Some(domain.as_str()));
        prop_assert_eq!(pane.is_alt_screen, is_alt);
    }

    // ========================================================================
    // Property Tests: StateChange
    // ========================================================================

    /// Property 19: StateChange time_ago() returns non-empty string.
    #[test]
    fn prop_state_change_time_ago_nonempty(
        pane_id in 0..1000u64,
        desc in "[a-z ]{1,20}",
    ) {
        let change = StateChange::new(pane_id, &desc);
        let ago = change.time_ago();
        prop_assert!(!ago.is_empty(), "time_ago should not be empty");
    }

    /// Property 20: StateChange preserves fields.
    #[test]
    fn prop_state_change_fields(
        pane_id in 0..1000u64,
        desc in "[a-z ]{1,20}",
    ) {
        let change = StateChange::new(pane_id, &desc);
        prop_assert_eq!(change.pane_id, pane_id);
        prop_assert_eq!(&change.description, &desc);
    }

    // ========================================================================
    // Property Tests: UserHistory
    // ========================================================================

    /// Property 21: has_used_command returns true after record_command.
    #[test]
    fn prop_user_history_command(cmd in "[a-z ]{1,20}") {
        let mut history = UserHistory::default();
        prop_assert!(!history.has_used_command(&cmd),
            "should not have command before recording");
        history.record_command(&cmd);
        prop_assert!(history.has_used_command(&cmd),
            "should have command after recording");
    }

    /// Property 22: has_used_feature returns true after record_feature.
    #[test]
    fn prop_user_history_feature(feature in "[a-z_]{1,15}") {
        let mut history = UserHistory::default();
        prop_assert!(!history.has_used_feature(&feature),
            "should not have feature before recording");
        history.record_feature(&feature);
        prop_assert!(history.has_used_feature(&feature),
            "should have feature after recording");
    }

    /// Property 23: Empty history returns false for any query.
    #[test]
    fn prop_user_history_empty(
        cmd in "[a-z]{1,10}",
        feature in "[a-z]{1,10}",
    ) {
        let history = UserHistory::default();
        prop_assert!(!history.has_used_command(&cmd));
        prop_assert!(!history.has_used_feature(&feature));
    }

    // ========================================================================
    // Property Tests: FeatureHint
    // ========================================================================

    /// Property 24: FeatureHint builder preserves all fields.
    #[test]
    fn prop_feature_hint_builder(
        feature in "[a-z]{1,10}",
        message in "[a-z ]{1,30}",
        command in "ft [a-z]{1,10}",
        used in proptest::bool::ANY,
    ) {
        let hint = FeatureHint::new(&feature, &message, &command)
            .with_used(used);
        prop_assert_eq!(&hint.feature, &feature);
        prop_assert_eq!(&hint.message, &message);
        prop_assert_eq!(&hint.command, &command);
        prop_assert_eq!(hint.used, used);
        prop_assert!(hint.learn_more.is_none());
    }

    /// Property 25: FeatureHint with_learn_more preserves URL.
    #[test]
    fn prop_feature_hint_learn_more(url in "https://[a-z.]{5,20}") {
        let hint = FeatureHint::new("f", "m", "c")
            .with_learn_more(&url);
        prop_assert_eq!(hint.learn_more.as_deref(), Some(url.as_str()));
    }

    // ========================================================================
    // Property Tests: SystemMetrics
    // ========================================================================

    /// Property 26: SystemMetrics builder preserves fields.
    #[test]
    fn prop_system_metrics_builder(
        poll_ms in 10..5000u64,
        storage_bytes in 0..10_000_000_000u64,
    ) {
        let metrics = SystemMetrics::default()
            .with_poll_interval_ms(poll_ms)
            .with_storage_size_bytes(storage_bytes);
        prop_assert_eq!(metrics.poll_interval_ms, Some(poll_ms));
        prop_assert_eq!(metrics.storage_size_bytes, Some(storage_bytes));
    }

    // ========================================================================
    // Property Tests: Platform
    // ========================================================================

    /// Property 27: Platform Display is non-empty.
    #[test]
    fn prop_platform_display_nonempty(p in arb_platform()) {
        let display = format!("{}", p);
        prop_assert!(!display.is_empty(), "platform display should not be empty");
    }

    /// Property 28: All Platform variants have distinct Display values.
    #[test]
    fn prop_platform_display_distinct(a in arb_platform(), b in arb_platform()) {
        if a != b {
            prop_assert_ne!(format!("{}", a), format!("{}", b),
                "different platforms should have different display");
        }
    }

    /// Property 29: Platform::MacOS install command uses brew.
    #[test]
    fn prop_platform_macos_brew(pkg in "[a-z]{1,10}") {
        let cmd = Platform::MacOS.install_command(&pkg);
        prop_assert!(cmd.is_some(), "macOS should have install command");
        prop_assert!(cmd.unwrap().contains("brew"),
            "macOS install should use brew");
    }

    /// Property 30: Platform::Windows install command uses winget.
    #[test]
    fn prop_platform_windows_winget(pkg in "[a-z]{1,10}") {
        let cmd = Platform::Windows.install_command(&pkg);
        prop_assert!(cmd.is_some(), "Windows should have install command");
        prop_assert!(cmd.unwrap().contains("winget"),
            "Windows install should use winget");
    }

    /// Property 31: Platform install command always contains package name.
    #[test]
    fn prop_platform_install_has_package(p in arb_platform(), pkg in "[a-z]{3,10}") {
        if let Some(cmd) = p.install_command(&pkg) {
            prop_assert!(cmd.contains(&pkg),
                "install command '{}' should contain package '{}'", cmd, pkg);
        }
    }

    // ========================================================================
    // Property Tests: Priority
    // ========================================================================

    /// Property 32: Priority ordering is strict — Critical > High > Medium > Low.
    #[test]
    fn prop_priority_ordering(_dummy in Just(())) {
        prop_assert!(Priority::Critical > Priority::High);
        prop_assert!(Priority::High > Priority::Medium);
        prop_assert!(Priority::Medium > Priority::Low);
    }

    /// Property 33: Priority default is Medium.
    #[test]
    fn prop_priority_default(_dummy in Just(())) {
        prop_assert_eq!(Priority::default(), Priority::Medium);
    }

    /// Property 34: Priority self-comparison is equal.
    #[test]
    fn prop_priority_self_equal(p in arb_priority()) {
        prop_assert!(p == p, "priority should equal itself");
        prop_assert!(!(p < p), "priority should not be less than itself");
    }

    // ========================================================================
    // Property Tests: SuggestionType
    // ========================================================================

    /// Property 35: SuggestionType Display is non-empty.
    #[test]
    fn prop_suggestion_type_display_nonempty(st in arb_suggestion_type()) {
        let display = format!("{}", st);
        prop_assert!(!display.is_empty(), "suggestion type display should not be empty");
    }

    /// Property 36: All SuggestionType variants have distinct Display values.
    #[test]
    fn prop_suggestion_type_display_distinct(a in arb_suggestion_type(), b in arb_suggestion_type()) {
        if a != b {
            prop_assert_ne!(format!("{}", a), format!("{}", b),
                "different suggestion types should have different display");
        }
    }

    // ========================================================================
    // Property Tests: SuggestionId
    // ========================================================================

    /// Property 37: SuggestionId as_str matches Display.
    #[test]
    fn prop_suggestion_id_as_str_matches_display(s in "[a-z._]{1,20}") {
        let id = SuggestionId::new(&s);
        prop_assert_eq!(id.as_str(), &s);
        prop_assert_eq!(format!("{}", id), s);
    }

    /// Property 38: SuggestionId clone preserves equality.
    #[test]
    fn prop_suggestion_id_clone_eq(s in "[a-z._]{1,20}") {
        let id = SuggestionId::new(&s);
        let cloned = id.clone();
        prop_assert_eq!(id, cloned);
    }

    // ========================================================================
    // Property Tests: Suggestion
    // ========================================================================

    /// Property 39: Suggestion builder preserves fields.
    #[test]
    fn prop_suggestion_builder(
        id in "[a-z._]{1,15}",
        st in arb_suggestion_type(),
        msg in "[a-zA-Z ]{1,30}",
        rule_id in "[a-z._]{1,15}",
        priority in arb_priority(),
        dismissable in proptest::bool::ANY,
    ) {
        let suggestion = Suggestion::new(&id, st, &msg, &rule_id)
            .with_priority(priority)
            .with_dismissable(dismissable);
        prop_assert_eq!(suggestion.id.as_str(), id.as_str());
        prop_assert_eq!(suggestion.suggestion_type, st);
        prop_assert_eq!(&suggestion.message, &msg);
        prop_assert_eq!(&suggestion.rule_id, &rule_id);
        prop_assert_eq!(suggestion.priority, priority);
        prop_assert_eq!(suggestion.dismissable, dismissable);
    }

    /// Property 40: Suggestion default is dismissable.
    #[test]
    fn prop_suggestion_default_dismissable(st in arb_suggestion_type()) {
        let s = Suggestion::new("test", st, "msg", "rule");
        prop_assert!(s.dismissable, "default should be dismissable");
    }

    /// Property 41: Suggestion with_action preserves action.
    #[test]
    fn prop_suggestion_with_action(
        label in "[a-zA-Z ]{1,15}",
        command in "ft [a-z]{1,10}",
    ) {
        let s = Suggestion::new("test", SuggestionType::Tip, "msg", "rule")
            .with_action(SuggestedAction::new(&label, &command));
        prop_assert!(s.action.is_some());
        let action = s.action.unwrap();
        prop_assert_eq!(&action.label, &label);
        prop_assert_eq!(&action.command, &command);
    }

    /// Property 42: Suggestion with_learn_more preserves URL.
    #[test]
    fn prop_suggestion_with_learn_more(url in "https://[a-z.]{5,20}") {
        let s = Suggestion::new("test", SuggestionType::Tip, "msg", "rule")
            .with_learn_more(&url);
        prop_assert_eq!(s.learn_more.as_deref(), Some(url.as_str()));
    }

    // ========================================================================
    // Property Tests: DismissedStore
    // ========================================================================

    /// Property 43: Permanent dismissal persists.
    #[test]
    fn prop_dismissed_store_permanent(s in "[a-z._]{1,15}") {
        let mut store = DismissedStore::new();
        let id = SuggestionId::new(&s);
        prop_assert!(!store.is_dismissed(&id));
        store.dismiss_permanent(&id);
        prop_assert!(store.is_dismissed(&id));
    }

    /// Property 44: is_dismissed returns false for unknown IDs.
    #[test]
    fn prop_dismissed_store_unknown(s in "[a-z._]{1,15}") {
        let store = DismissedStore::new();
        let id = SuggestionId::new(&s);
        prop_assert!(!store.is_dismissed(&id));
    }

    /// Property 45: count increases with dismissals.
    #[test]
    fn prop_dismissed_store_count(n in 1..10usize) {
        let mut store = DismissedStore::new();
        for i in 0..n {
            let id = SuggestionId::new(format!("id_{}", i));
            store.dismiss_permanent(&id);
        }
        prop_assert_eq!(store.count(), n,
            "count should be {} after {} dismissals", n, n);
    }

    /// Property 46: Temporary dismissal is active immediately.
    #[test]
    fn prop_dismissed_store_temporary(s in "[a-z._]{1,15}") {
        let mut store = DismissedStore::new();
        let id = SuggestionId::new(&s);
        store.dismiss_temporary(&id, std::time::Duration::from_secs(3600));
        prop_assert!(store.is_dismissed(&id),
            "temporary dismissal should be active immediately");
    }

    /// Property 47: Permanent dismissal overrides temporary.
    #[test]
    fn prop_dismissed_store_permanent_overrides_temp(s in "[a-z._]{1,15}") {
        let mut store = DismissedStore::new();
        let id = SuggestionId::new(&s);
        store.dismiss_temporary(&id, std::time::Duration::from_millis(1));
        store.dismiss_permanent(&id);
        // Even after cleanup (which removes expired temps), permanent stays
        store.cleanup_expired();
        prop_assert!(store.is_dismissed(&id),
            "permanent should persist after cleanup");
    }

    // ========================================================================
    // Property Tests: SuggestionContext
    // ========================================================================

    /// Property 48: suggest_pane finds exact match.
    #[test]
    fn prop_context_suggest_pane_exact(id in 0..1000u64) {
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(id));
        let result = ctx.suggest_pane(id);
        prop_assert!(result.is_some());
        prop_assert_eq!(result.unwrap().id, id);
    }

    /// Property 49: suggest_pane finds closest by numeric distance.
    #[test]
    fn prop_context_suggest_pane_closest(
        target in 100..200u64,
        offset in 1..10u64,
    ) {
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1)); // far away
        ctx.add_pane(PaneInfo::new(target)); // close
        ctx.add_pane(PaneInfo::new(999)); // far away
        let requested = target + offset;
        let result = ctx.suggest_pane(requested);
        prop_assert!(result.is_some());
        // The closest pane should be the target (distance = offset)
        // vs pane 1 (distance = target + offset - 1) or pane 999 (distance = 999 - target - offset)
        prop_assert_eq!(result.unwrap().id, target,
            "suggest_pane({}) should find closest pane {}", requested, target);
    }

    /// Property 50: suggest_pane with empty panes returns None.
    #[test]
    fn prop_context_suggest_pane_empty(id in 0..1000u64) {
        let ctx = SuggestionContext::new();
        prop_assert!(ctx.suggest_pane(id).is_none());
    }

    /// Property 51: suggest_workflow finds exact case-insensitive match.
    #[test]
    fn prop_context_suggest_workflow_exact(name in "[a-z_]{3,15}") {
        let mut ctx = SuggestionContext::new();
        ctx.add_workflow(&name);
        let result = ctx.suggest_workflow(&name);
        prop_assert_eq!(result, Some(name.as_str()));
    }

    /// Property 52: suggest_rule finds exact match.
    #[test]
    fn prop_context_suggest_rule_exact(rule in "[a-z.]{3,15}") {
        let mut ctx = SuggestionContext::new();
        ctx.add_rule(&rule);
        let result = ctx.suggest_rule(&rule);
        prop_assert_eq!(result, Some(rule.as_str()));
    }

    /// Property 53: format_available_panes with no panes returns "No panes available".
    #[test]
    fn prop_context_format_no_panes(_dummy in Just(())) {
        let ctx = SuggestionContext::new();
        prop_assert_eq!(ctx.format_available_panes(), "No panes available");
    }

    /// Property 54: format_available_workflows with no workflows returns "No workflows available".
    #[test]
    fn prop_context_format_no_workflows(_dummy in Just(())) {
        let ctx = SuggestionContext::new();
        prop_assert_eq!(ctx.format_available_workflows(), "No workflows available");
    }

    /// Property 55: format_available_rules with no rules returns "No rules available".
    #[test]
    fn prop_context_format_no_rules(_dummy in Just(())) {
        let ctx = SuggestionContext::new();
        prop_assert_eq!(ctx.format_available_rules(), "No rules available");
    }

    /// Property 56: recent_state_for_pane filters by pane_id.
    #[test]
    fn prop_context_recent_state_filters(
        target_pane in 0..100u64,
        other_pane in 100..200u64,
    ) {
        let mut ctx = SuggestionContext::new();
        ctx.add_state_change(StateChange::new(target_pane, "target event"));
        ctx.add_state_change(StateChange::new(other_pane, "other event"));
        let recent = ctx.recent_state_for_pane(target_pane);
        prop_assert_eq!(recent.len(), 1,
            "should find exactly 1 state change for pane {}", target_pane);
        prop_assert_eq!(recent[0].pane_id, target_pane);
    }

    // ========================================================================
    // Property Tests: SuggestionEngine
    // ========================================================================

    /// Property 57: Disabled engine returns empty suggestions.
    #[test]
    fn prop_engine_disabled_empty(_dummy in Just(())) {
        let config = SuggestionConfig {
            enabled: false,
            ..Default::default()
        };
        let engine = SuggestionEngine::new(config);
        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);
        prop_assert!(suggestions.is_empty(), "disabled engine should return empty");
    }

    /// Property 58: max_suggestions is respected.
    #[test]
    fn prop_engine_max_suggestions(max in 1..5usize) {
        let config = SuggestionConfig {
            max_suggestions: max,
            ..Default::default()
        };
        let engine = SuggestionEngine::new(config);
        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);
        prop_assert!(suggestions.len() <= max,
            "should return at most {} suggestions, got {}", max, suggestions.len());
    }

    /// Property 59: Suggestions are sorted by priority (highest first).
    #[test]
    fn prop_engine_priority_sorted(_dummy in Just(())) {
        let config = SuggestionConfig {
            max_suggestions: 10,
            ..Default::default()
        };
        let engine = SuggestionEngine::new(config);
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(1).with_alt_screen(true));
        let suggestions = engine.suggest(&ctx);
        for window in suggestions.windows(2) {
            prop_assert!(window[0].priority >= window[1].priority,
                "suggestions should be sorted by priority (highest first)");
        }
    }

    /// Property 60: Dismissed suggestions are excluded from results.
    #[test]
    fn prop_engine_dismissed_excluded(_dummy in Just(())) {
        let config = SuggestionConfig::default();
        let mut engine = SuggestionEngine::new(config);
        let ctx = SuggestionContext::new();

        let suggestions = engine.suggest(&ctx);
        if !suggestions.is_empty() {
            let first_id = suggestions[0].id.clone();
            engine.dismiss(&first_id);
            let after = engine.suggest(&ctx);
            prop_assert!(after.iter().all(|s| s.id != first_id),
                "dismissed suggestion should not appear");
        }
    }

    /// Property 61: min_priority filter excludes low priority suggestions.
    #[test]
    fn prop_engine_min_priority(min_p in arb_priority()) {
        let config = SuggestionConfig {
            min_priority: min_p,
            max_suggestions: 20,
            ..Default::default()
        };
        let engine = SuggestionEngine::new(config);
        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);
        for s in &suggestions {
            prop_assert!(s.priority >= min_p,
                "suggestion priority {:?} should be >= min {:?}", s.priority, min_p);
        }
    }

    /// Property 62: Empty engine returns no suggestions.
    #[test]
    fn prop_engine_empty_no_suggestions(_dummy in Just(())) {
        let engine = SuggestionEngine::empty(SuggestionConfig::default());
        let ctx = SuggestionContext::new();
        let suggestions = engine.suggest(&ctx);
        prop_assert!(suggestions.is_empty(), "empty engine should return no suggestions");
    }

    // ========================================================================
    // Property Tests: pane_not_found_suggestion
    // ========================================================================

    /// Property 63: pane_not_found_suggestion returns None for empty context.
    #[test]
    fn prop_pane_not_found_empty(id in 0..1000u64) {
        let ctx = SuggestionContext::new();
        prop_assert!(pane_not_found_suggestion(id, &ctx).is_none());
    }

    /// Property 64: pane_not_found_suggestion includes "Did you mean" for non-empty context.
    #[test]
    fn prop_pane_not_found_has_did_you_mean(
        requested in 0..1000u64,
        pane_id in 0..1000u64,
    ) {
        let mut ctx = SuggestionContext::new();
        ctx.add_pane(PaneInfo::new(pane_id));
        let result = pane_not_found_suggestion(requested, &ctx);
        prop_assert!(result.is_some());
        let text = result.unwrap();
        prop_assert!(text.contains("Did you mean"),
            "should contain 'Did you mean': {}", text);
    }

    // ========================================================================
    // Property Tests: workflow_not_found_suggestion
    // ========================================================================

    /// Property 65: workflow_not_found_suggestion returns None for empty context.
    #[test]
    fn prop_workflow_not_found_empty(name in "[a-z]{3,10}") {
        let ctx = SuggestionContext::new();
        prop_assert!(workflow_not_found_suggestion(&name, &ctx).is_none());
    }

    /// Property 66: workflow_not_found_suggestion includes "Available workflows" when available.
    #[test]
    fn prop_workflow_not_found_lists_available(name in "[a-z]{3,10}") {
        let mut ctx = SuggestionContext::new();
        ctx.add_workflow("handle_compaction");
        let result = workflow_not_found_suggestion(&name, &ctx);
        prop_assert!(result.is_some());
        let text = result.unwrap();
        prop_assert!(text.contains("Available workflows"),
            "should contain 'Available workflows': {}", text);
    }

    // ========================================================================
    // Property Tests: SuggestedAction
    // ========================================================================

    /// Property 67: SuggestedAction preserves fields.
    #[test]
    fn prop_suggested_action_fields(
        label in "[a-zA-Z ]{1,15}",
        command in "ft [a-z ]{1,20}",
    ) {
        let action = SuggestedAction::new(&label, &command);
        prop_assert_eq!(&action.label, &label);
        prop_assert_eq!(&action.command, &command);
    }

    // ========================================================================
    // Property Tests: SuggestionConfig
    // ========================================================================

    /// Property 68: SuggestionConfig default has sensible values.
    #[test]
    fn prop_suggestion_config_default(_dummy in Just(())) {
        let config = SuggestionConfig::default();
        prop_assert!(config.enabled, "default should be enabled");
        prop_assert!(config.max_suggestions > 0, "max_suggestions should be > 0");
        prop_assert_eq!(config.min_priority, Priority::Low);
    }
}
