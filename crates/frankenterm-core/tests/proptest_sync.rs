//! Property-based tests for the sync module.
//!
//! Verifies path deny/allow rules, snapshot filename roundtrip, live DB
//! detection, SyncCategory serde, SyncItemAction serde, and the
//! sanitize-parse pipeline invariants.

use std::path::Path;

use proptest::prelude::*;

use frankenterm_core::sync::{
    SyncCategory, SyncItemAction, is_live_db_path, is_path_allowed, is_path_denied,
    parse_snapshot_filename, snapshot_filename,
};

// ── Strategies ────────────────────────────────────────────────────────

/// Generate a safe filename component (no special chars).
fn arb_safe_component() -> impl Strategy<Value = String> {
    "[a-z0-9]{1,12}"
}

/// Generate a relative file path like "dir/subdir/file.txt".
fn arb_relative_path() -> impl Strategy<Value = String> {
    prop::collection::vec(arb_safe_component(), 1..=4).prop_map(|parts| parts.join("/"))
}

/// Generate a path that contains a known denied pattern.
fn arb_denied_path() -> impl Strategy<Value = String> {
    let denied_patterns = vec![
        ".env",
        ".env.local",
        ".env.production",
        "tokens.json",
        "credentials.json",
        ".ssh",
        "id_rsa",
        "id_ed25519",
        ".gnupg",
        ".netrc",
        ".npmrc",
        ".pypirc",
    ];
    (prop::sample::select(denied_patterns), arb_safe_component())
        .prop_map(|(denied, prefix)| format!("{}/{}", prefix, denied))
}

/// Generate a path with a denied extension.
fn arb_denied_extension_path() -> impl Strategy<Value = String> {
    let denied_exts = vec!["key", "pem", "p12", "pfx"];
    (arb_safe_component(), prop::sample::select(denied_exts))
        .prop_map(|(name, ext)| format!("{}.{}", name, ext))
}

/// Generate a version string for snapshot filenames.
fn arb_version() -> impl Strategy<Value = String> {
    "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}"
}

/// Generate a UTC timestamp in compact ISO-8601 format.
fn arb_timestamp() -> impl Strategy<Value = String> {
    (
        2020_u32..2030,
        1_u32..=12,
        1_u32..=28,
        0_u32..24,
        0_u32..60,
        0_u32..60,
    )
        .prop_map(|(y, m, d, h, min, s)| format!("{y:04}{m:02}{d:02}_{h:02}{min:02}{s:02}"))
}

/// Generate a hostname.
fn arb_hostname() -> impl Strategy<Value = String> {
    "[a-z0-9\\-]{1,20}"
}

/// Generate a SyncCategory.
fn arb_category() -> impl Strategy<Value = SyncCategory> {
    prop_oneof![
        Just(SyncCategory::Binary),
        Just(SyncCategory::Config),
        Just(SyncCategory::Snapshots),
    ]
}

/// Generate a SyncItemAction.
fn arb_action() -> impl Strategy<Value = SyncItemAction> {
    prop_oneof![
        Just(SyncItemAction::Add),
        Just(SyncItemAction::Update),
        Just(SyncItemAction::Skip),
        Just(SyncItemAction::Conflict),
        Just(SyncItemAction::Denied),
    ]
}

// ── Path deny rules ───────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Paths containing denied patterns are always denied.
    #[test]
    fn denied_patterns_are_denied(path in arb_denied_path()) {
        prop_assert!(
            is_path_denied(&path, &[]),
            "path '{}' should be denied", path
        );
    }

    /// Paths with denied file extensions are always denied.
    #[test]
    fn denied_extensions_are_denied(path in arb_denied_extension_path()) {
        prop_assert!(
            is_path_denied(&path, &[]),
            "path '{}' with secret extension should be denied", path
        );
    }

    /// Paths that don't contain any denied pattern or extension pass.
    #[test]
    fn safe_paths_are_not_denied(name in "[a-z]{3,8}", dir in "[a-z]{3,8}") {
        let path = format!("{}/{}.txt", dir, name);
        prop_assert!(
            !is_path_denied(&path, &[]),
            "path '{}' should not be denied", path
        );
    }

    /// Extra deny patterns work alongside built-in ones.
    #[test]
    fn extra_deny_patterns_work(
        safe_path in arb_relative_path(),
        extra_name in arb_safe_component(),
    ) {
        let denied_path = format!("{}/{}", safe_path, extra_name);
        let extra_deny = vec![extra_name.clone()];
        prop_assert!(
            is_path_denied(&denied_path, &extra_deny),
            "path '{}' should be denied by extra pattern '{}'", denied_path, extra_name
        );
    }

    /// Deny check is independent for each path component.
    #[test]
    fn deny_checks_each_component(
        prefix in arb_safe_component(),
        suffix in arb_safe_component(),
    ) {
        // ".env" in the middle should trigger denial
        let path = format!("{prefix}/.env/{suffix}");
        prop_assert!(is_path_denied(&path, &[]), "path '{}' has .env component", path);
    }
}

// ── Path allow rules ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Empty allow list allows everything.
    #[test]
    fn empty_allow_list_allows_all(path in arb_relative_path()) {
        prop_assert!(
            is_path_allowed(&path, &[]),
            "empty allow list should allow '{}'", path
        );
    }

    /// Paths matching allow prefix are allowed.
    #[test]
    fn matching_prefix_is_allowed(
        prefix in arb_safe_component(),
        rest in arb_safe_component(),
    ) {
        let path = format!("{prefix}/{rest}");
        let allow = vec![prefix.clone()];
        prop_assert!(
            is_path_allowed(&path, &allow),
            "path '{}' should be allowed by prefix '{}'", path, prefix
        );
    }

    /// Paths NOT matching any allow prefix are denied.
    #[test]
    fn non_matching_prefix_is_denied(
        path_base in "[a-f]{3,6}",
        allow_base in "[g-z]{3,6}",
    ) {
        // Ensure no overlap between path and allow prefixes
        let path = format!("{path_base}/file.txt");
        let allow = vec![allow_base];
        prop_assert!(
            !is_path_allowed(&path, &allow),
            "path '{}' should NOT be allowed", path
        );
    }

    /// Deny takes precedence over allow (tested via is_path_denied).
    #[test]
    fn deny_overrides_allow(
        prefix in arb_safe_component(),
    ) {
        let path = format!("{prefix}/.env");
        let allow = vec![prefix.clone()];
        // Path is allowed by prefix but denied by pattern
        prop_assert!(is_path_allowed(&path, &allow));
        prop_assert!(is_path_denied(&path, &[]));
    }
}

// ── Snapshot filename roundtrip ───────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// snapshot_filename → parse_snapshot_filename recovers version.
    #[test]
    fn snapshot_filename_roundtrip_version(
        version in arb_version(),
        timestamp in arb_timestamp(),
        hostname in arb_hostname(),
    ) {
        let ws_root = Path::new("/tmp/test-workspace");
        let filename = snapshot_filename(&version, &timestamp, ws_root, &hostname);

        let parsed = parse_snapshot_filename(&filename);
        prop_assert!(parsed.is_some(), "failed to parse filename: {}", filename);

        let (parsed_version, _ts, _ws, _host) = parsed.unwrap();
        // Version is sanitized (non-alphanumeric → underscore) but should match
        let expected_version: String = version
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '.' { c } else { '_' })
            .collect();
        prop_assert_eq!(
            parsed_version, expected_version,
            "version mismatch in roundtrip"
        );
    }

    /// Snapshot filename always starts with "wa_snapshot_" and ends with ".db".
    #[test]
    fn snapshot_filename_format(
        version in arb_version(),
        timestamp in arb_timestamp(),
        hostname in arb_hostname(),
    ) {
        let ws_root = Path::new("/tmp/test");
        let filename = snapshot_filename(&version, &timestamp, ws_root, &hostname);
        prop_assert!(
            filename.starts_with("wa_snapshot_"),
            "filename '{}' doesn't start with wa_snapshot_", filename
        );
        let has_db_ext = std::path::Path::new(&*filename)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("db"));
        prop_assert!(
            has_db_ext,
            "filename '{}' doesn't end with .db", filename
        );
    }

    /// Different workspace roots produce different workspace keys.
    #[test]
    fn different_workspaces_different_keys(
        ws_a in "[a-z]{3,8}",
        ws_b in "[a-z]{3,8}",
        version in arb_version(),
        timestamp in arb_timestamp(),
        hostname in arb_hostname(),
    ) {
        prop_assume!(ws_a != ws_b);
        let fn_a = snapshot_filename(&version, &timestamp, Path::new(&format!("/tmp/{ws_a}")), &hostname);
        let fn_b = snapshot_filename(&version, &timestamp, Path::new(&format!("/tmp/{ws_b}")), &hostname);

        // Filenames should differ (different workspace hash)
        prop_assert_ne!(fn_a, fn_b);
    }

    /// Hostname is truncated to 16 characters in filename.
    #[test]
    fn hostname_truncated_to_16(
        long_host in "[a-z]{17,25}",
        version in arb_version(),
        timestamp in arb_timestamp(),
    ) {
        let ws_root = Path::new("/tmp/test");
        let filename = snapshot_filename(&version, &timestamp, ws_root, &long_host);
        let parsed = parse_snapshot_filename(&filename);
        prop_assert!(parsed.is_some(), "failed to parse: {}", filename);
        let (ref _v, ref _ts, ref _ws, ref host) = parsed.unwrap();
        let host_len = host.len();
        prop_assert!(
            host_len <= 16,
            "host '{}' should be truncated to 16 chars, got {}", host, host_len
        );
    }

    /// Snapshot filename is deterministic for same inputs.
    #[test]
    fn snapshot_filename_deterministic(
        version in arb_version(),
        timestamp in arb_timestamp(),
        hostname in arb_hostname(),
    ) {
        let ws_root = Path::new("/tmp/det-test");
        let a = snapshot_filename(&version, &timestamp, ws_root, &hostname);
        let b = snapshot_filename(&version, &timestamp, ws_root, &hostname);
        prop_assert_eq!(a, b);
    }

    /// Parsed filename always returns 4 non-empty components.
    #[test]
    fn parsed_filename_all_components_present(
        version in arb_version(),
        timestamp in arb_timestamp(),
        hostname in "[a-z]{1,15}",
    ) {
        let ws_root = Path::new("/tmp/comp-test");
        let filename = snapshot_filename(&version, &timestamp, ws_root, &hostname);
        let parsed = parse_snapshot_filename(&filename);
        prop_assert!(parsed.is_some(), "failed to parse: {}", filename);
        let (v, ts, ws, h): (String, String, String, String) = parsed.unwrap();
        prop_assert!(!v.is_empty(), "version should not be empty");
        prop_assert!(!ts.is_empty(), "timestamp should not be empty");
        prop_assert!(!ws.is_empty(), "workspace key should not be empty");
        prop_assert!(!h.is_empty(), "host should not be empty");
        // Workspace key should be exactly 8 hex chars
        prop_assert_eq!(ws.len(), 8, "workspace key should be 8 chars");
    }
}

// ── Live DB detection ─────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Files ending with .db, -wal, -shm, .sqlite are detected as live DB files.
    #[test]
    fn live_db_extensions_detected(
        name in arb_safe_component(),
        ext in prop_oneof![
            Just(".db"),
            Just("-wal"),
            Just("-shm"),
            Just(".db-wal"),
            Just(".db-shm"),
            Just(".sqlite"),
            Just(".sqlite-wal"),
            Just(".sqlite-shm"),
        ],
    ) {
        let path = format!("{name}{ext}");
        prop_assert!(
            is_live_db_path(&path),
            "path '{}' should be detected as live DB", path
        );
    }

    /// Non-DB files are not detected as live DB files.
    #[test]
    fn non_db_files_not_detected(
        name in arb_safe_component(),
        ext in prop_oneof![
            Just(".txt"),
            Just(".json"),
            Just(".toml"),
            Just(".rs"),
            Just(".yaml"),
            Just(".md"),
        ],
    ) {
        let path = format!("{name}{ext}");
        prop_assert!(
            !is_live_db_path(&path),
            "path '{}' should NOT be detected as live DB", path
        );
    }

    /// Live DB detection is case-insensitive.
    #[test]
    fn live_db_case_insensitive(name in arb_safe_component()) {
        // .DB should also match .db
        let upper = format!("{name}.DB");
        let lower = format!("{name}.db");
        prop_assert_eq!(
            is_live_db_path(&upper),
            is_live_db_path(&lower),
            "case sensitivity mismatch"
        );
    }
}

// ── Serde roundtrip for enums ─────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// SyncCategory serializes successfully (Serialize-only, no Deserialize).
    #[test]
    fn category_serializes_ok(category in arb_category()) {
        let json = serde_json::to_string(&category).expect("serialize");
        prop_assert!(!json.is_empty());
    }

    /// SyncItemAction serializes successfully.
    #[test]
    fn action_serializes_ok(action in arb_action()) {
        let json = serde_json::to_string(&action).expect("serialize");
        prop_assert!(!json.is_empty());
    }

    /// SyncCategory serializes to snake_case.
    #[test]
    fn category_is_snake_case(category in arb_category()) {
        let json = serde_json::to_string(&category).expect("serialize");
        let s = json.trim_matches('"');
        let all_snake = s.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_');
        prop_assert!(all_snake, "category '{}' is not snake_case", s);
    }

    /// SyncItemAction serializes to snake_case.
    #[test]
    fn action_is_snake_case(action in arb_action()) {
        let json = serde_json::to_string(&action).expect("serialize");
        let s = json.trim_matches('"');
        let all_snake = s.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_');
        prop_assert!(all_snake, "action '{}' is not snake_case", s);
    }
}

// ── Sanitize invariants ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Snapshot filenames only contain safe characters.
    #[test]
    fn snapshot_filename_only_safe_chars(
        version in arb_version(),
        timestamp in arb_timestamp(),
        hostname in arb_hostname(),
    ) {
        let ws_root = Path::new("/tmp/safe-test");
        let filename = snapshot_filename(&version, &timestamp, ws_root, &hostname);
        for ch in filename.chars() {
            let safe = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.';
            prop_assert!(
                safe,
                "filename '{}' contains unsafe char '{}'", filename, ch
            );
        }
    }

    /// Snapshot filenames don't contain path separators.
    #[test]
    fn snapshot_filename_no_path_separators(
        version in ".*",
        timestamp in ".*",
        hostname in ".*",
    ) {
        let ws_root = Path::new("/tmp/sep-test");
        let filename = snapshot_filename(&version, &timestamp, ws_root, &hostname);
        prop_assert!(
            !filename.contains('/') && !filename.contains('\\'),
            "filename '{}' contains path separator", filename
        );
    }
}

// ── Parse robustness ──────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Random strings don't cause parse_snapshot_filename to panic.
    #[test]
    fn parse_snapshot_random_input_no_panic(input in ".*") {
        // Should return None, not panic
        let _ = parse_snapshot_filename(&input);
    }

    /// Strings without the wa_snapshot_ prefix return None.
    #[test]
    fn parse_snapshot_wrong_prefix_returns_none(input in "[a-z]{5,30}\\.db") {
        let result = parse_snapshot_filename(&input);
        prop_assert!(
            result.is_none(),
            "non-snapshot '{}' should not parse", input
        );
    }

    /// Strings without .db suffix return None.
    #[test]
    fn parse_snapshot_no_db_suffix_returns_none(input in "wa_snapshot_[a-z_]{5,30}") {
        let result = parse_snapshot_filename(&input);
        prop_assert!(
            result.is_none(),
            "filename without .db suffix '{}' should not parse", input
        );
    }
}

// ── Unit tests ────────────────────────────────────────────────────────

#[test]
fn all_builtin_denied_patterns_work() {
    let patterns = [
        ".env",
        ".env.local",
        ".env.production",
        ".env.development",
        "tokens.json",
        "credentials.json",
        "keyring",
        "keychain",
        ".ssh",
        "id_rsa",
        "id_ed25519",
        ".gnupg",
        ".netrc",
        ".npmrc",
        ".pypirc",
    ];
    for pattern in patterns {
        assert!(
            is_path_denied(pattern, &[]),
            "pattern '{}' should be denied",
            pattern
        );
    }
}

#[test]
fn all_live_db_patterns_detected() {
    let patterns = [
        "data.db",
        "data-wal",
        "data-shm",
        "data.db-wal",
        "data.db-shm",
        "data.sqlite",
        "data.sqlite-wal",
        "data.sqlite-shm",
    ];
    for path in patterns {
        assert!(is_live_db_path(path), "'{}' should be live DB", path);
    }
}

#[test]
fn snapshot_filename_known_values() {
    let filename = snapshot_filename(
        "0.1.0",
        "20260101_120000",
        Path::new("/home/user/workspace"),
        "testhost",
    );
    assert!(filename.starts_with("wa_snapshot_0.1.0_20260101_120000_"));
    assert!(filename.ends_with("_testhost.db"));

    let parsed = parse_snapshot_filename(&filename);
    assert!(parsed.is_some());
    let (version, timestamp, ws_key, host): (String, String, String, String) = parsed.unwrap();
    assert_eq!(version, "0.1.0");
    assert_eq!(timestamp, "20260101_120000");
    assert_eq!(ws_key.len(), 8);
    assert_eq!(host, "testhost");
}
