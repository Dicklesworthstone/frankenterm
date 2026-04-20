//! Structure-aware adversarial deserialization fuzz harness for mux wire types.
//!
//! The existing `proptest_tab_wire_serde`, `proptest_pane_serde`, and
//! `proptest_client_tab_serde` suites roundtrip WELL-FORMED values through
//! serde and assert equality. They do not exercise the *deserialization*
//! boundary that the mux sees at runtime: a peer client or an in-memory
//! snapshot can hand us JSON bytes that are syntactically well-formed but
//! semantically adversarial — bad URLs, out-of-range terminal sizes,
//! wrong types, missing required fields, extra/unknown fields, truncated
//! shapes, or deeply-nested split trees.
//!
//! This harness targets the public serde deserialization surface for the
//! three wire types most directly consumed from untrusted IPC frames:
//! `PaneEntry`, `PaneNode`, `SwapLayout`. The contract under test:
//!
//! 1. **Crash-freedom**: `serde_json::from_str::<T>(bytes)` must return
//!    `Result<T, serde_json::Error>` for any byte slice, including
//!    structurally-adversarial JSON, deeply-nested arrays/objects, and
//!    payloads whose field types are intentionally wrong. It must never
//!    panic, assertion-trip, or overflow the stack during deserialization.
//!
//! 2. **URL-validation contract**: `SerdeUrl` uses `#[serde(try_from =
//!    "String")]` with `Url::parse` as the validator. Any arbitrary string
//!    must either parse to `Ok(SerdeUrl)` or return `Err` — never panic.
//!
//! 3. **Self-roundtrip stability**: for any payload that successfully
//!    deserializes to `T`, `serde_json::to_string(&value)` must succeed
//!    and re-deserialize back to an equal `T`. (Regression guard: a Debug
//!    equality check ensures we don't silently lose information across
//!    the serialize→deserialize cycle in a way that roundtrip-of-
//!    constructed-values proptests would miss.)
//!
//! Previously zero adversarial-deserialization coverage for these three
//! types — verified by grep across `frankenterm/mux/tests/`.

use mux::layout::{LayoutArrangement, SwapLayout};
use mux::tab::{PaneEntry, PaneNode, SerdeUrl};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────

/// Fully arbitrary bytes presented as a UTF-8 string. Many of these won't
/// be valid JSON at all — that's fine, serde returns Err and we assert
/// no-panic.
fn arb_any_json_string() -> impl Strategy<Value = String> {
    proptest::collection::vec(any::<char>(), 0..256).prop_map(|chars| chars.into_iter().collect())
}

/// Structurally-plausible JSON values: nested objects/arrays/strings/
/// numbers/bools/nulls up to a bounded depth. This is the main
/// structure-aware fuzzing corpus: it exercises the JSON shape the
/// deserializer actually navigates.
fn arb_json_value() -> impl Strategy<Value = serde_json::Value> {
    let leaf = prop_oneof![
        Just(serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        any::<i64>().prop_map(|n| serde_json::Value::Number(n.into())),
        (-1_000_000.0f64..1_000_000.0).prop_filter("finite", |v| v.is_finite()).prop_map(|f| {
            serde_json::Number::from_f64(f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null)
        }),
        "[a-zA-Z0-9_./:?=&+ \\-]{0,32}".prop_map(serde_json::Value::String),
    ];

    leaf.prop_recursive(
        4,  // depth
        32, // size budget
        8,  // branching factor
        |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..6).prop_map(serde_json::Value::Array),
                proptest::collection::hash_map("[a-z_]{1,12}", inner, 0..6).prop_map(|m| {
                    let object: serde_json::Map<String, serde_json::Value> = m.into_iter().collect();
                    serde_json::Value::Object(object)
                }),
            ]
        },
    )
}

/// Arbitrary URL-shaped strings: mostly well-formed URLs, occasionally
/// malformed candidates (missing scheme, bad hostnames, control bytes).
fn arb_url_candidate() -> impl Strategy<Value = String> {
    prop_oneof![
        // Well-formed: scheme + host + path
        ("[a-z]{2,8}", "[a-z0-9.-]{1,32}", "[a-zA-Z0-9/_-]{0,24}")
            .prop_map(|(scheme, host, path)| format!("{scheme}://{host}/{path}")),
        // Missing scheme
        "[a-z0-9./-]{1,64}".prop_map(String::from),
        // Invalid scheme characters
        "[!@#$%^&*()]{1,8}://example.com".prop_map(String::from),
        // Control bytes in URL path
        (0u32..=0x1F)
            .prop_filter_map("valid char", char::from_u32)
            .prop_map(|c| format!("https://example.com/{c}"))
            .boxed(),
        // Any bytes (expect most to fail)
        arb_any_json_string(),
    ]
}

// ── Properties ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    // ── SerdeUrl: URL validation contract ──────────────────────────

    /// `SerdeUrl` deserialization through JSON must never panic on any
    /// string, including malformed URLs and control-byte-laden inputs.
    #[test]
    fn serde_url_from_arbitrary_string_never_panics(candidate in arb_url_candidate()) {
        // Wrap the candidate in a JSON string literal so serde sees a
        // valid JSON *document* and the only decision point is the
        // try_from<String> validator.
        let json = serde_json::to_string(&candidate).unwrap();
        let _ = serde_json::from_str::<SerdeUrl>(&json);
    }

    /// When `SerdeUrl` deserialization from a candidate succeeds, the
    /// value must round-trip to an equal `SerdeUrl` through JSON.
    #[test]
    fn serde_url_roundtrip_stability(candidate in arb_url_candidate()) {
        let json = serde_json::to_string(&candidate).unwrap();
        if let Ok(url) = serde_json::from_str::<SerdeUrl>(&json) {
            let reserialized = serde_json::to_string(&url).unwrap();
            let reparsed: SerdeUrl = serde_json::from_str(&reserialized)
                .expect("re-serialized SerdeUrl must re-deserialize");
            prop_assert_eq!(url, reparsed, "SerdeUrl roundtrip must be stable");
        }
    }

    // ── PaneEntry: wire deserialization crash-freedom ──────────────

    /// `serde_json::from_str::<PaneEntry>(bytes)` must never panic on
    /// arbitrary byte input.
    #[test]
    fn pane_entry_from_arbitrary_string_never_panics(bytes in arb_any_json_string()) {
        let _ = serde_json::from_str::<PaneEntry>(&bytes);
    }

    /// Structured JSON values must deserialize into `PaneEntry` without
    /// panic. Most will fail (missing required fields), but failure
    /// must be graceful.
    #[test]
    fn pane_entry_from_structured_json_never_panics(value in arb_json_value()) {
        let json = serde_json::to_string(&value).unwrap();
        let _ = serde_json::from_str::<PaneEntry>(&json);
    }

    // ── PaneNode: recursive wire deserialization crash-freedom ─────

    /// `PaneNode` is a recursive wire type; adversarial JSON could try
    /// to send deeply nested arrays/objects to stack-exhaust the
    /// deserializer. serde_json is expected to bound depth, but we
    /// still need to verify no panic.
    #[test]
    fn pane_node_from_arbitrary_string_never_panics(bytes in arb_any_json_string()) {
        let _ = serde_json::from_str::<PaneNode>(&bytes);
    }

    #[test]
    fn pane_node_from_structured_json_never_panics(value in arb_json_value()) {
        let json = serde_json::to_string(&value).unwrap();
        let _ = serde_json::from_str::<PaneNode>(&json);
    }

    // ── SwapLayout + LayoutArrangement ─────────────────────────────

    /// `SwapLayout` wraps a recursive `LayoutArrangement`. Crash-freedom
    /// applies to the outer wrapper too.
    #[test]
    fn swap_layout_from_arbitrary_string_never_panics(bytes in arb_any_json_string()) {
        let _ = serde_json::from_str::<SwapLayout>(&bytes);
    }

    #[test]
    fn swap_layout_from_structured_json_never_panics(value in arb_json_value()) {
        let json = serde_json::to_string(&value).unwrap();
        let _ = serde_json::from_str::<SwapLayout>(&json);
    }

    #[test]
    fn layout_arrangement_from_arbitrary_string_never_panics(bytes in arb_any_json_string()) {
        let _ = serde_json::from_str::<LayoutArrangement>(&bytes);
    }

    #[test]
    fn layout_arrangement_from_structured_json_never_panics(value in arb_json_value()) {
        let json = serde_json::to_string(&value).unwrap();
        let _ = serde_json::from_str::<LayoutArrangement>(&json);
    }

    // ── Self-roundtrip stability on accepted payloads ──────────────

    /// For any structured JSON that successfully deserializes as
    /// `LayoutArrangement`, re-serializing and re-deserializing must
    /// produce an equal value.
    #[test]
    fn layout_arrangement_accepted_payload_roundtrips(value in arb_json_value()) {
        let json = serde_json::to_string(&value).unwrap();
        if let Ok(original) = serde_json::from_str::<LayoutArrangement>(&json) {
            let reserialized =
                serde_json::to_string(&original).expect("accepted LayoutArrangement must serialize");
            let reparsed: LayoutArrangement = serde_json::from_str(&reserialized)
                .expect("re-serialized LayoutArrangement must re-deserialize");
            prop_assert_eq!(original, reparsed, "LayoutArrangement roundtrip must be stable");
        }
    }

    /// Same contract for `SwapLayout`.
    #[test]
    fn swap_layout_accepted_payload_roundtrips(value in arb_json_value()) {
        let json = serde_json::to_string(&value).unwrap();
        if let Ok(original) = serde_json::from_str::<SwapLayout>(&json) {
            let reserialized =
                serde_json::to_string(&original).expect("accepted SwapLayout must serialize");
            let reparsed: SwapLayout = serde_json::from_str(&reserialized)
                .expect("re-serialized SwapLayout must re-deserialize");
            prop_assert_eq!(original, reparsed, "SwapLayout roundtrip must be stable");
        }
    }
}

// ── Hand-rolled regressions for specific adversarial shapes ─────────────

#[test]
fn pane_entry_rejects_empty_object() {
    assert!(serde_json::from_str::<PaneEntry>("{}").is_err());
}

#[test]
fn pane_entry_rejects_array() {
    assert!(serde_json::from_str::<PaneEntry>("[]").is_err());
}

#[test]
fn pane_node_accepts_unit_empty_variant() {
    // Untagged or explicit "Empty" is a real PaneNode variant; this
    // pins the accepted serialization shape so a future refactor that
    // breaks it is caught here.
    let payload = serde_json::to_string(&PaneNode::Empty).unwrap();
    let parsed: PaneNode = serde_json::from_str(&payload).unwrap();
    assert_eq!(parsed, PaneNode::Empty);
}

#[test]
fn serde_url_rejects_empty_string() {
    assert!(serde_json::from_str::<SerdeUrl>("\"\"").is_err());
}

#[test]
fn serde_url_rejects_scheme_only() {
    assert!(serde_json::from_str::<SerdeUrl>("\"http://\"").is_err());
}

#[test]
fn serde_url_accepts_file_scheme_with_path() {
    let ok: SerdeUrl = serde_json::from_str("\"file:///tmp/foo.txt\"").unwrap();
    assert_eq!(ok.url.scheme(), "file");
}

#[test]
fn swap_layout_rejects_split_without_children() {
    let bad = r#"{"name":"x","description":null,"arrangement":{"Split":{"direction":"Horizontal","ratio":0.5}}}"#;
    assert!(serde_json::from_str::<SwapLayout>(bad).is_err());
}

#[test]
fn swap_layout_accepts_minimal_slot() {
    let ok = r#"{"name":"x","description":null,"arrangement":{"Slot":{"is_main":true}}}"#;
    let parsed: SwapLayout = serde_json::from_str(ok).unwrap();
    assert_eq!(parsed.name, "x");
}
