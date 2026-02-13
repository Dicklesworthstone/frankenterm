//! Property-based tests for API schema types.
//!
//! Covers serde roundtrips, version parsing/display/compatibility,
//! registry invariants, and schema diff semantics.

use proptest::prelude::*;
use std::collections::HashSet;

use frankenterm_core::VERSION;
use frankenterm_core::api_schema::{
    ApiVersion, ChangeKind, EndpointMeta, SchemaChange, SchemaDiffResult, SchemaRegistry,
    VersionCompatibility,
};

// =============================================================================
// Strategies
// =============================================================================

/// Generate an arbitrary ApiVersion with tractable component ranges.
fn arb_api_version() -> impl Strategy<Value = ApiVersion> {
    (0..=10u32, 0..=10u32, 0..=10u32).prop_map(|(major, minor, patch)| ApiVersion {
        major,
        minor,
        patch,
    })
}

/// Generate a pre-1.0 ApiVersion (major == 0).
fn arb_pre1_version() -> impl Strategy<Value = ApiVersion> {
    (0..=10u32, 0..=10u32).prop_map(|(minor, patch)| ApiVersion {
        major: 0,
        minor,
        patch,
    })
}

/// Generate a post-1.0 ApiVersion (major >= 1).
fn arb_post1_version() -> impl Strategy<Value = ApiVersion> {
    (1..=10u32, 0..=10u32, 0..=10u32).prop_map(|(major, minor, patch)| ApiVersion {
        major,
        minor,
        patch,
    })
}

/// Generate an identifier-like string.
fn arb_id() -> impl Strategy<Value = String> {
    "[a-zA-Z][a-zA-Z0-9_]{0,29}"
}

/// Generate a short descriptive string.
fn arb_desc() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _.-]{1,30}"
}

/// Generate a schema filename.
fn arb_schema_file() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9-]{0,20}\\.json"
}

/// Generate an arbitrary EndpointMeta.
fn arb_endpoint_meta() -> impl Strategy<Value = EndpointMeta> {
    (
        arb_id(),
        arb_desc(),
        arb_desc(),
        prop::option::of(arb_desc()),
        prop::option::of(arb_desc()),
        arb_schema_file(),
        any::<bool>(),
        "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}",
    )
        .prop_map(
            |(id, title, description, robot_command, mcp_tool, schema_file, stable, since)| {
                EndpointMeta {
                    id,
                    title,
                    description,
                    robot_command,
                    mcp_tool,
                    schema_file,
                    stable,
                    since,
                }
            },
        )
}

/// Generate an arbitrary ChangeKind (uniform over all 7 variants).
fn arb_change_kind() -> impl Strategy<Value = ChangeKind> {
    prop_oneof![
        Just(ChangeKind::Added),
        Just(ChangeKind::Removed),
        Just(ChangeKind::RequiredFieldAdded),
        Just(ChangeKind::OptionalFieldAdded),
        Just(ChangeKind::FieldRemoved),
        Just(ChangeKind::TypeChanged),
        Just(ChangeKind::Cosmetic),
    ]
}

/// Generate an arbitrary SchemaChange.
fn arb_schema_change() -> impl Strategy<Value = SchemaChange> {
    (arb_schema_file(), arb_change_kind(), arb_desc()).prop_map(
        |(schema_file, kind, description)| SchemaChange {
            schema_file,
            kind,
            description,
        },
    )
}

/// Generate an arbitrary SchemaDiffResult with 0..=8 changes.
fn arb_schema_diff() -> impl Strategy<Value = SchemaDiffResult> {
    (
        "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}",
        "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}",
        prop::collection::vec(arb_schema_change(), 0..=8),
    )
        .prop_map(|(from_version, to_version, changes)| SchemaDiffResult {
            from_version,
            to_version,
            changes,
        })
}

// =============================================================================
// 1. ApiVersion serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn api_version_serde_roundtrip(v in arb_api_version()) {
        let json = serde_json::to_string(&v).expect("serialize");
        let parsed: ApiVersion = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&parsed, &v, "serde roundtrip failed for {}", v);
    }
}

// =============================================================================
// 2. ApiVersion parse/Display roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn api_version_parse_display_roundtrip(v in arb_api_version()) {
        let display = v.to_string();
        let parsed = ApiVersion::parse(&display);
        prop_assert!(parsed.is_some(), "parse failed for display string: {}", display);
        prop_assert_eq!(&parsed.unwrap(), &v, "roundtrip mismatch for {}", display);
    }
}

// =============================================================================
// 3. ApiVersion parse invalid strings
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Strings with fewer than 3 dot-separated parts should fail to parse.
    #[test]
    fn api_version_parse_too_few_parts(s in "[a-zA-Z0-9]{1,10}") {
        // Single token, no dots
        let result = ApiVersion::parse(&s);
        let has_three = s.split('.').count() >= 3;
        if !has_three {
            prop_assert!(result.is_none(), "expected None for '{}'", s);
        }
    }

    /// Two-part strings should fail.
    #[test]
    fn api_version_parse_two_parts(a in 0..100u32, b in 0..100u32) {
        let s = format!("{}.{}", a, b);
        prop_assert!(ApiVersion::parse(&s).is_none(), "expected None for '{}'", s);
    }

    /// Strings with non-numeric parts should fail.
    #[test]
    fn api_version_parse_nonnumeric(
        prefix in "[a-zA-Z]{1,5}",
        b in 0..10u32,
        c in 0..10u32,
    ) {
        let s = format!("{}.{}.{}", prefix, b, c);
        prop_assert!(ApiVersion::parse(&s).is_none(), "expected None for '{}'", s);
    }
}

// =============================================================================
// 4. ApiVersion::current() matches crate VERSION
// =============================================================================

#[test]
fn api_version_current_matches_crate_version() {
    let current = ApiVersion::current();
    assert_eq!(current.to_string(), VERSION);
    let parsed = ApiVersion::parse(VERSION).expect("VERSION should be valid semver");
    assert_eq!(current, parsed);
}

// =============================================================================
// 5. Pre-1.0 compatibility rules
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Same pre-1.0 version yields Exact.
    #[test]
    fn pre1_same_version_is_exact(v in arb_pre1_version()) {
        let compat = v.compatibility(&v);
        prop_assert_eq!(compat, VersionCompatibility::Exact);
    }

    /// Same major.minor, different patch yields Compatible (pre-1.0).
    #[test]
    fn pre1_same_minor_diff_patch_is_compatible(
        minor in 0..=10u32,
        p1 in 0..=10u32,
        p2 in 0..=10u32,
    ) {
        prop_assume!(p1 != p2);
        let reader = ApiVersion { major: 0, minor, patch: p1 };
        let wire = ApiVersion { major: 0, minor, patch: p2 };
        let compat = reader.compatibility(&wire);
        prop_assert_eq!(compat, VersionCompatibility::Compatible,
            "expected Compatible for reader={} wire={}", reader, wire);
    }

    /// Different minor with major=0 yields Incompatible.
    #[test]
    fn pre1_diff_minor_is_incompatible(
        m1 in 0..=10u32,
        m2 in 0..=10u32,
        p1 in 0..=10u32,
        p2 in 0..=10u32,
    ) {
        prop_assume!(m1 != m2);
        let reader = ApiVersion { major: 0, minor: m1, patch: p1 };
        let wire = ApiVersion { major: 0, minor: m2, patch: p2 };
        let compat = reader.compatibility(&wire);
        prop_assert_eq!(compat, VersionCompatibility::Incompatible,
            "expected Incompatible for reader={} wire={}", reader, wire);
    }

    /// Pre-1.0 reader vs different major yields Incompatible.
    #[test]
    fn pre1_diff_major_is_incompatible(
        wire_major in 1..=10u32,
        minor in 0..=10u32,
        p1 in 0..=10u32,
        p2 in 0..=10u32,
    ) {
        let reader = ApiVersion { major: 0, minor, patch: p1 };
        let wire = ApiVersion { major: wire_major, minor, patch: p2 };
        let compat = reader.compatibility(&wire);
        prop_assert_eq!(compat, VersionCompatibility::Incompatible,
            "expected Incompatible for reader={} wire={}", reader, wire);
    }
}

// =============================================================================
// 6. Post-1.0 compatibility rules
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Same post-1.0 version yields Exact.
    #[test]
    fn post1_same_version_is_exact(v in arb_post1_version()) {
        let compat = v.compatibility(&v);
        prop_assert_eq!(compat, VersionCompatibility::Exact);
    }

    /// Same major, wire.minor <= reader.minor, different version yields Compatible.
    #[test]
    fn post1_same_major_older_minor_is_compatible(
        major in 1..=10u32,
        reader_minor in 1..=10u32,
        wire_minor in 0..=10u32,
        p1 in 0..=10u32,
        p2 in 0..=10u32,
    ) {
        prop_assume!(wire_minor <= reader_minor);
        let reader = ApiVersion { major, minor: reader_minor, patch: p1 };
        let wire = ApiVersion { major, minor: wire_minor, patch: p2 };
        prop_assume!(reader != wire);
        let compat = reader.compatibility(&wire);
        prop_assert_eq!(compat, VersionCompatibility::Compatible,
            "expected Compatible for reader={} wire={}", reader, wire);
    }

    /// Same major, wire.minor > reader.minor yields NewerMinor.
    #[test]
    fn post1_newer_wire_minor_is_newer_minor(
        major in 1..=10u32,
        reader_minor in 0..=9u32,
        delta in 1..=10u32,
        p1 in 0..=10u32,
        p2 in 0..=10u32,
    ) {
        let wire_minor = reader_minor + delta;
        let reader = ApiVersion { major, minor: reader_minor, patch: p1 };
        let wire = ApiVersion { major, minor: wire_minor, patch: p2 };
        let compat = reader.compatibility(&wire);
        prop_assert_eq!(compat, VersionCompatibility::NewerMinor,
            "expected NewerMinor for reader={} wire={}", reader, wire);
    }

    /// Different major (post-1.0 reader) yields Incompatible.
    #[test]
    fn post1_diff_major_is_incompatible(
        reader_major in 1..=10u32,
        wire_major in 1..=10u32,
        m1 in 0..=10u32,
        m2 in 0..=10u32,
        p1 in 0..=10u32,
        p2 in 0..=10u32,
    ) {
        prop_assume!(reader_major != wire_major);
        let reader = ApiVersion { major: reader_major, minor: m1, patch: p1 };
        let wire = ApiVersion { major: wire_major, minor: m2, patch: p2 };
        let compat = reader.compatibility(&wire);
        prop_assert_eq!(compat, VersionCompatibility::Incompatible,
            "expected Incompatible for reader={} wire={}", reader, wire);
    }
}

// =============================================================================
// 7. is_compatible_with consistency with compatibility()
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// is_compatible_with returns true iff compatibility is Exact, Compatible, or NewerMinor.
    #[test]
    fn is_compatible_with_matches_compatibility(
        reader in arb_api_version(),
        wire in arb_api_version(),
    ) {
        let compat = reader.compatibility(&wire);
        let is_compat = reader.is_compatible_with(&wire);

        let expected_compat = compat == VersionCompatibility::Exact
            || compat == VersionCompatibility::Compatible
            || compat == VersionCompatibility::NewerMinor;

        prop_assert_eq!(is_compat, expected_compat,
            "is_compatible_with mismatch for reader={} wire={} compat={:?}",
            reader, wire, compat);
    }
}

// =============================================================================
// 8. EndpointMeta serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn endpoint_meta_serde_roundtrip(ep in arb_endpoint_meta()) {
        let json = serde_json::to_string(&ep).expect("serialize");
        let parsed: EndpointMeta = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&parsed.id, &ep.id, "id mismatch");
        prop_assert_eq!(&parsed.title, &ep.title, "title mismatch");
        prop_assert_eq!(&parsed.description, &ep.description, "description mismatch");
        prop_assert_eq!(&parsed.robot_command, &ep.robot_command, "robot_command mismatch");
        prop_assert_eq!(&parsed.mcp_tool, &ep.mcp_tool, "mcp_tool mismatch");
        prop_assert_eq!(&parsed.schema_file, &ep.schema_file, "schema_file mismatch");
        prop_assert_eq!(parsed.stable, ep.stable, "stable mismatch");
        prop_assert_eq!(&parsed.since, &ep.since, "since mismatch");
    }
}

// =============================================================================
// 9. SchemaRegistry serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn schema_registry_serde_roundtrip(
        version in "[0-9]{1,2}\\.[0-9]{1,2}\\.[0-9]{1,2}",
        endpoints in prop::collection::vec(arb_endpoint_meta(), 0..=5),
    ) {
        let reg = SchemaRegistry { version: version.clone(), endpoints };
        let json = serde_json::to_string(&reg).expect("serialize");
        let parsed: SchemaRegistry = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&parsed.version, &reg.version, "version mismatch");
        prop_assert_eq!(parsed.endpoints.len(), reg.endpoints.len(), "endpoint count mismatch");
        for (i, (a, b)) in parsed.endpoints.iter().zip(reg.endpoints.iter()).enumerate() {
            prop_assert_eq!(&a.id, &b.id, "endpoint {} id mismatch", i);
        }
    }
}

// =============================================================================
// 10. SchemaRegistry canonical invariants
// =============================================================================

#[test]
fn canonical_registry_invariants() {
    let reg = SchemaRegistry::canonical();

    // Non-empty
    assert!(
        !reg.endpoints.is_empty(),
        "canonical registry should have endpoints"
    );

    // Version matches crate VERSION
    assert_eq!(
        reg.version, VERSION,
        "canonical registry version must match VERSION"
    );

    // Unique ids
    let ids: Vec<&str> = reg.ids().collect();
    let unique_ids: HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(
        ids.len(),
        unique_ids.len(),
        "canonical registry has duplicate endpoint ids"
    );

    // Every endpoint has a non-empty id and schema_file
    for ep in &reg.endpoints {
        assert!(!ep.id.is_empty(), "endpoint id must not be empty");
        assert!(
            !ep.schema_file.is_empty(),
            "endpoint schema_file must not be empty"
        );
        assert!(
            ep.schema_file.ends_with(".json"),
            "schema_file must end with .json, got: {}",
            ep.schema_file,
        );
    }
}

// =============================================================================
// 11. SchemaRegistry get
// =============================================================================

#[test]
fn canonical_registry_get_known_ids() {
    let reg = SchemaRegistry::canonical();
    let ids: Vec<&str> = reg.ids().collect();
    for id in &ids {
        let ep = reg.get(id);
        assert!(ep.is_some(), "get should return Some for known id '{}'", id);
        assert_eq!(ep.unwrap().id, *id);
    }
}

#[test]
fn canonical_registry_get_unknown_returns_none() {
    let reg = SchemaRegistry::canonical();
    assert!(reg.get("__nonexistent_endpoint__").is_none());
    assert!(reg.get("").is_none());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Arbitrary registries: get returns the correct endpoint when present.
    #[test]
    fn registry_get_finds_inserted(
        version in "[0-9]\\.[0-9]\\.[0-9]",
        ep in arb_endpoint_meta(),
    ) {
        let reg = SchemaRegistry {
            version,
            endpoints: vec![ep.clone()],
        };
        let found = reg.get(&ep.id);
        prop_assert!(found.is_some(), "get should find the endpoint with id={}", ep.id);
        prop_assert_eq!(&found.unwrap().id, &ep.id);
    }

    /// get returns None for ids not in the registry.
    #[test]
    fn registry_get_returns_none_for_missing(
        version in "[0-9]\\.[0-9]\\.[0-9]",
        ep in arb_endpoint_meta(),
        query in arb_id(),
    ) {
        prop_assume!(query != ep.id);
        let reg = SchemaRegistry {
            version,
            endpoints: vec![ep],
        };
        let found = reg.get(&query);
        prop_assert!(found.is_none(), "get should return None for missing id={}", query);
    }
}

// =============================================================================
// 12. SchemaRegistry dual_surface / robot_only
// =============================================================================

#[test]
fn canonical_dual_surface_all_have_both() {
    let reg = SchemaRegistry::canonical();
    for ep in reg.dual_surface() {
        assert!(
            ep.robot_command.is_some(),
            "dual_surface endpoint '{}' missing robot_command",
            ep.id,
        );
        assert!(
            ep.mcp_tool.is_some(),
            "dual_surface endpoint '{}' missing mcp_tool",
            ep.id,
        );
    }
}

#[test]
fn canonical_robot_only_none_have_mcp() {
    let reg = SchemaRegistry::canonical();
    for ep in reg.robot_only() {
        assert!(
            ep.robot_command.is_some(),
            "robot_only endpoint '{}' missing robot_command",
            ep.id,
        );
        assert!(
            ep.mcp_tool.is_none(),
            "robot_only endpoint '{}' should not have mcp_tool",
            ep.id,
        );
    }
}

#[test]
fn canonical_dual_plus_robot_only_covers_all_robot() {
    let reg = SchemaRegistry::canonical();
    let dual_count = reg.dual_surface().count();
    let robot_only_count = reg.robot_only().count();
    let total_with_robot = reg
        .endpoints
        .iter()
        .filter(|e| e.robot_command.is_some())
        .count();
    assert_eq!(
        dual_count + robot_only_count,
        total_with_robot,
        "dual_surface + robot_only should equal all endpoints with robot_command"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// For arbitrary registries, dual_surface only returns endpoints with both fields.
    #[test]
    fn arbitrary_registry_dual_surface_property(
        endpoints in prop::collection::vec(arb_endpoint_meta(), 1..=6),
    ) {
        let reg = SchemaRegistry { version: "1.0.0".into(), endpoints };
        for ep in reg.dual_surface() {
            let has_robot = ep.robot_command.is_some();
            let has_mcp = ep.mcp_tool.is_some();
            prop_assert!(has_robot && has_mcp,
                "dual_surface returned ep '{}' without both surfaces", ep.id);
        }
        for ep in reg.robot_only() {
            let has_robot = ep.robot_command.is_some();
            let no_mcp = ep.mcp_tool.is_none();
            prop_assert!(has_robot && no_mcp,
                "robot_only returned ep '{}' with mcp_tool present", ep.id);
        }
    }
}

// =============================================================================
// 13. SchemaRegistry schema_files — sorted, no duplicates
// =============================================================================

#[test]
fn canonical_schema_files_sorted_and_deduped() {
    let reg = SchemaRegistry::canonical();
    let files = reg.schema_files();
    assert!(!files.is_empty());

    // Sorted
    let mut sorted = files.clone();
    sorted.sort();
    assert_eq!(files, sorted, "schema_files should be sorted");

    // No duplicates
    let unique: HashSet<&str> = files.iter().copied().collect();
    assert_eq!(
        files.len(),
        unique.len(),
        "schema_files should have no duplicates"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// schema_files is always sorted and deduplicated for arbitrary registries.
    #[test]
    fn arbitrary_schema_files_sorted_and_deduped(
        endpoints in prop::collection::vec(arb_endpoint_meta(), 0..=8),
    ) {
        let reg = SchemaRegistry { version: "0.1.0".into(), endpoints };
        let files = reg.schema_files();

        // Check sorted
        for window in files.windows(2) {
            prop_assert!(window[0] <= window[1],
                "schema_files not sorted: '{}' > '{}'", window[0], window[1]);
        }

        // Check no duplicates
        let unique: HashSet<&str> = files.iter().copied().collect();
        prop_assert_eq!(files.len(), unique.len(), "schema_files has duplicates");
    }
}

// =============================================================================
// 14. SchemaRegistry uncovered_schemas
// =============================================================================

#[test]
fn uncovered_schemas_detects_extra_files() {
    let reg = SchemaRegistry::canonical();
    let known: Vec<String> = reg.schema_files().iter().map(|s| s.to_string()).collect();
    let mut on_disk = known.clone();
    on_disk.push("extra-unknown.json".to_string());

    let uncovered = reg.uncovered_schemas(&on_disk);
    assert_eq!(uncovered, vec!["extra-unknown.json"]);
}

#[test]
fn uncovered_schemas_empty_when_all_covered() {
    let reg = SchemaRegistry::canonical();
    let on_disk: Vec<String> = reg.schema_files().iter().map(|s| s.to_string()).collect();
    let uncovered = reg.uncovered_schemas(&on_disk);
    assert!(
        uncovered.is_empty(),
        "should be empty when all files are in registry"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// Any file on disk that is NOT in any endpoint schema_file must be uncovered.
    #[test]
    fn uncovered_schemas_returns_unregistered(
        endpoints in prop::collection::vec(arb_endpoint_meta(), 1..=4),
        extra_files in prop::collection::vec(arb_schema_file(), 1..=3),
    ) {
        let reg = SchemaRegistry { version: "1.0.0".into(), endpoints };
        let registered: HashSet<String> = reg.endpoints.iter().map(|e| e.schema_file.clone()).collect();

        // On-disk includes both registered and extra
        let mut on_disk: Vec<String> = reg.schema_files().iter().map(|s| s.to_string()).collect();
        for f in &extra_files {
            on_disk.push(f.clone());
        }

        let uncovered = reg.uncovered_schemas(&on_disk);

        // Every uncovered file must NOT be in the registered set
        for f in &uncovered {
            let not_registered = !registered.contains(f.as_str());
            prop_assert!(not_registered,
                "uncovered file '{}' should not be in registered set", f);
        }

        // Every extra file that is not registered must appear in uncovered
        for f in &extra_files {
            if !registered.contains(f.as_str()) {
                let found = uncovered.contains(f);
                prop_assert!(found,
                    "extra unregistered file '{}' not found in uncovered list", f);
            }
        }
    }
}

// =============================================================================
// 15. SchemaChange serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn schema_change_serde_roundtrip(change in arb_schema_change()) {
        let json = serde_json::to_string(&change).expect("serialize");
        let parsed: SchemaChange = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&parsed, &change, "serde roundtrip mismatch");
    }
}

// =============================================================================
// 16. ChangeKind serde roundtrip (all 7 variants serialize to snake_case)
// =============================================================================

#[test]
fn change_kind_serde_all_variants() {
    let variants = [
        (ChangeKind::Added, "\"added\""),
        (ChangeKind::Removed, "\"removed\""),
        (ChangeKind::RequiredFieldAdded, "\"required_field_added\""),
        (ChangeKind::OptionalFieldAdded, "\"optional_field_added\""),
        (ChangeKind::FieldRemoved, "\"field_removed\""),
        (ChangeKind::TypeChanged, "\"type_changed\""),
        (ChangeKind::Cosmetic, "\"cosmetic\""),
    ];

    for (variant, expected_json) in &variants {
        let json = serde_json::to_string(variant).expect("serialize");
        assert_eq!(
            &json, expected_json,
            "ChangeKind {:?} serialized to '{}', expected '{}'",
            variant, json, expected_json,
        );
        let parsed: ChangeKind = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(&parsed, variant, "roundtrip mismatch for {:?}", variant);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Every ChangeKind variant roundtrips through serde.
    #[test]
    fn change_kind_serde_roundtrip(kind in arb_change_kind()) {
        let json = serde_json::to_string(&kind).expect("serialize");
        let parsed: ChangeKind = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(parsed, kind, "roundtrip mismatch for {:?}", kind);
    }
}

// =============================================================================
// 17. ChangeKind is_breaking — property test
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// is_breaking matches the expected set of breaking variants.
    #[test]
    fn change_kind_is_breaking_property(kind in arb_change_kind()) {
        let expected_breaking = kind == ChangeKind::Removed
            || kind == ChangeKind::RequiredFieldAdded
            || kind == ChangeKind::FieldRemoved
            || kind == ChangeKind::TypeChanged;
        let actual = kind.is_breaking();
        prop_assert_eq!(actual, expected_breaking,
            "is_breaking mismatch for {:?}: expected={}, got={}", kind, expected_breaking, actual);
    }
}

// =============================================================================
// 18. SchemaDiffResult serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(25))]

    #[test]
    fn schema_diff_result_serde_roundtrip(diff in arb_schema_diff()) {
        let json = serde_json::to_string(&diff).expect("serialize");
        let parsed: SchemaDiffResult = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&parsed.from_version, &diff.from_version, "from_version mismatch");
        prop_assert_eq!(&parsed.to_version, &diff.to_version, "to_version mismatch");
        prop_assert_eq!(parsed.changes.len(), diff.changes.len(), "changes count mismatch");
        for (i, (a, b)) in parsed.changes.iter().zip(diff.changes.iter()).enumerate() {
            prop_assert_eq!(a, b, "change {} mismatch", i);
        }
    }
}

// =============================================================================
// 19. SchemaDiffResult has_breaking_changes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// has_breaking_changes is true iff at least one change is_breaking.
    #[test]
    fn schema_diff_has_breaking_matches_changes(diff in arb_schema_diff()) {
        let any_breaking = diff.changes.iter().any(|c| c.kind.is_breaking());
        prop_assert_eq!(diff.has_breaking_changes(), any_breaking,
            "has_breaking_changes mismatch: computed={}, expected={}", diff.has_breaking_changes(), any_breaking);
    }
}

// =============================================================================
// 20. SchemaDiffResult breaking_changes count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// breaking_changes().count() equals the number of changes where is_breaking is true.
    #[test]
    fn schema_diff_breaking_count_matches_filter(diff in arb_schema_diff()) {
        let manual_count = diff.changes.iter().filter(|c| c.kind.is_breaking()).count();
        let method_count = diff.breaking_changes().count();
        prop_assert_eq!(method_count, manual_count,
            "breaking_changes count mismatch: method={}, manual={}", method_count, manual_count);
    }
}

// =============================================================================
// Additional: Compatibility reflexivity and symmetry properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// Compatibility with self is always Exact.
    #[test]
    fn compatibility_reflexive(v in arb_api_version()) {
        prop_assert_eq!(v.compatibility(&v), VersionCompatibility::Exact,
            "compatibility with self should be Exact for {}", v);
    }

    /// is_compatible_with self is always true.
    #[test]
    fn is_compatible_with_self(v in arb_api_version()) {
        prop_assert!(v.is_compatible_with(&v),
            "version {} should be compatible with itself", v);
    }

    /// If compatibility is Incompatible, is_compatible_with must be false.
    #[test]
    fn incompatible_implies_not_compatible(
        reader in arb_api_version(),
        wire in arb_api_version(),
    ) {
        let compat = reader.compatibility(&wire);
        if compat == VersionCompatibility::Incompatible {
            prop_assert!(!reader.is_compatible_with(&wire),
                "Incompatible should mean is_compatible_with=false for reader={} wire={}", reader, wire);
        }
    }
}

// =============================================================================
// Additional: SchemaDiffResult Default
// =============================================================================

#[test]
fn schema_diff_result_default_has_no_breaking() {
    let diff = SchemaDiffResult::default();
    assert!(!diff.has_breaking_changes());
    assert_eq!(diff.breaking_changes().count(), 0);
    assert!(diff.from_version.is_empty());
    assert!(diff.to_version.is_empty());
    assert!(diff.changes.is_empty());
}

// =============================================================================
// Additional: SchemaRegistry Default
// =============================================================================

#[test]
fn schema_registry_default_is_empty() {
    let reg = SchemaRegistry::default();
    assert!(reg.endpoints.is_empty());
    assert!(reg.version.is_empty());
    assert_eq!(reg.schema_files().len(), 0);
    assert!(reg.get("anything").is_none());
    assert_eq!(reg.dual_surface().count(), 0);
    assert_eq!(reg.robot_only().count(), 0);
}

// =============================================================================
// Additional: ids() returns all endpoint ids
// =============================================================================

#[test]
fn canonical_ids_count_matches_endpoints() {
    let reg = SchemaRegistry::canonical();
    let ids: Vec<&str> = reg.ids().collect();
    assert_eq!(ids.len(), reg.endpoints.len());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    /// ids() iterator length matches endpoints length for arbitrary registries.
    #[test]
    fn arbitrary_ids_count_matches_endpoints(
        endpoints in prop::collection::vec(arb_endpoint_meta(), 0..=6),
    ) {
        let reg = SchemaRegistry { version: "0.1.0".into(), endpoints };
        let ids: Vec<&str> = reg.ids().collect();
        prop_assert_eq!(ids.len(), reg.endpoints.len(),
            "ids count ({}) != endpoints count ({})", ids.len(), reg.endpoints.len());
    }
}
