//! Metamorphic oracles for `frankenterm_core::robot_envelope::canonicalize_json`.
//!
//! The canonicalizer is the gatekeeper between runtime-variable bridge output
//! (wall-clock timestamps, HashMap iteration order, platform-specific float
//! noise) and the byte-exact golden JSON fixtures under
//! `tests/golden_robot_envelope/`. If its behaviour drifts, golden tests
//! either start flaking or quietly stop catching real regressions.
//!
//! Metamorphic testing lets us pin invariants without hand-writing per-case
//! expectations: we generate arbitrary JSON, apply a structure-preserving
//! transformation, and assert that canonicalization either absorbs the
//! transformation (MR1-MR3, MR5) or propagates it deterministically (MR4).
//!
//! Every relation uses proptest so each CI run reshapes the JSON under test.

use frankenterm_core::robot_envelope::canonicalize_json;
use proptest::prelude::*;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Arbitrary JSON strategies
// ---------------------------------------------------------------------------

/// Bounded-depth JSON value strategy. Depth + breadth caps keep shrinking
/// cheap and prevent recursive structures from exhausting the 256 MiB
/// default proptest memory budget.
fn arb_json() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<f64>()
            .prop_filter("finite float", |f| f.is_finite())
            .prop_map(|f| Value::Number(
                serde_json::Number::from_f64(f).unwrap_or_else(|| 0.into())
            )),
        "[\\x20-\\x7e]{0,24}".prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 16, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::vec(("[a-zA-Z_][a-zA-Z0-9_]{0,10}", inner), 0..6).prop_map(|pairs| {
                let mut map = serde_json::Map::new();
                for (k, v) in pairs {
                    map.insert(k, v);
                }
                Value::Object(map)
            }),
        ]
    })
}

fn arb_json_array() -> impl Strategy<Value = Vec<Value>> {
    prop::collection::vec(arb_json(), 0..8)
}

/// Render a JSON Value to a canonical form by running canonicalize_json on
/// a clone, then pretty-printing. All MR assertions compare canonical
/// rendered strings rather than raw Values so object-field ordering from
/// serde_json::to_string is deterministic (serde_json preserves insertion
/// order for `Map`).
fn canon_text(value: &Value) -> String {
    let mut clone = value.clone();
    canonicalize_json(&mut clone, None);
    serde_json::to_string(&clone).expect("serialize canonical")
}

// ---------------------------------------------------------------------------
// MR1 — Idempotence: canon(canon(x)) == canon(x)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn mr1_canonicalize_is_idempotent(value in arb_json()) {
        let mut once = value.clone();
        canonicalize_json(&mut once, None);

        let mut twice = once.clone();
        canonicalize_json(&mut twice, None);

        prop_assert_eq!(
            serde_json::to_string(&once).unwrap(),
            serde_json::to_string(&twice).unwrap(),
            "canonicalization must be a fixed-point operation"
        );
    }
}

// ---------------------------------------------------------------------------
// MR2 — Timestamp erasure: replacing the VALUE at any top-level-or-nested
//       `timestamp` string field with a different string must not change
//       the canonical output. The rule is "value ignored, key present."
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mr2_timestamp_value_is_erased_across_substitutions(
        inner in arb_json(),
        ts1 in "[A-Za-z0-9:+-]{1,32}",
        ts2 in "[A-Za-z0-9:+-]{1,32}",
    ) {
        // Build two envelopes that differ ONLY in the string value of a
        // top-level `timestamp` field. Canonicalization must flatten both.
        let with_ts_a = json!({
            "timestamp": ts1,
            "source": "mr2",
            "data": inner.clone(),
            "degraded": false,
        });
        let with_ts_b = json!({
            "timestamp": ts2,
            "source": "mr2",
            "data": inner,
            "degraded": false,
        });

        prop_assert_eq!(
            canon_text(&with_ts_a),
            canon_text(&with_ts_b),
            "timestamp value must not leak into canonical output"
        );
    }
}

// ---------------------------------------------------------------------------
// MR3 — Sort-invariance under magic keys: arrays whose parent key is
//       `agents`, `tags`, or `pane_ids` must produce the same canonical
//       output no matter how the input is permuted.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mr3_magic_key_arrays_are_permutation_invariant(
        items in arb_json_array(),
        perm_seed in any::<u64>(),
        magic_key in prop_oneof![Just("agents"), Just("tags"), Just("pane_ids")],
    ) {
        // Permute deterministically using a splitmix-style shuffle.
        let mut shuffled = items.clone();
        let mut state = perm_seed | 1; // avoid 0
        for i in (1..shuffled.len()).rev() {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (state as usize) % (i + 1);
            shuffled.swap(i, j);
        }

        let original = json!({ magic_key: items });
        let permuted = json!({ magic_key: shuffled });

        prop_assert_eq!(
            canon_text(&original),
            canon_text(&permuted),
            "magic key {} must sort its array during canonicalization", magic_key
        );
    }
}

// ---------------------------------------------------------------------------
// MR4 — Order preservation under non-magic keys: arrays under any OTHER
//       key must keep their original order. This is the dual of MR3: the
//       sort is applied selectively, never globally.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn mr4_non_magic_key_arrays_preserve_order(
        items in arb_json_array(),
        non_magic in "[a-z][a-z0-9_]{0,10}"
            .prop_filter(
                "non-magic",
                |k: &String| !matches!(k.as_str(), "agents" | "tags" | "pane_ids"),
            ),
    ) {
        prop_assume!(items.len() >= 2);
        // Build an array that has a distinguishable ordering: append a
        // sentinel that is strictly less-than any item already in the
        // list so the "sorted" form and the "reversed" form differ.
        let mut reversed = items.clone();
        reversed.reverse();
        prop_assume!(items != reversed);

        let original = json!({ &non_magic: items });
        let flipped = json!({ &non_magic: reversed });

        prop_assert_ne!(
            canon_text(&original),
            canon_text(&flipped),
            "non-magic key {} must NOT sort; reversing should change canonical output",
            non_magic
        );
    }
}

// ---------------------------------------------------------------------------
// MR5 — Float precision clamp: any two floats that round to the same
//       4-decimal-place value must canonicalize to the same JSON number.
//       This absorbs libc / fma / cross-platform rounding noise without
//       dropping semantically meaningful precision.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn mr5_floats_rounding_to_same_4dp_value_canonicalize_equally(
        base in -10_000.0f64..10_000.0,
        // Epsilon small enough that (base + epsilon) rounds to the same
        // 4-decimal value as `base` itself: |epsilon| < 5e-5. Clamp hard
        // so proptest's shrinking can't widen us into a different bucket.
        epsilon_millionths in -49_000i64..49_000,
    ) {
        let epsilon = epsilon_millionths as f64 / 1_000_000_000.0; // |e| < 4.9e-5
        let a = base;
        let b = base + epsilon;

        // Skip degenerate floats that fail to round-trip through JSON.
        prop_assume!(a.is_finite() && b.is_finite());
        let round_a = (a * 10_000.0).round() / 10_000.0;
        let round_b = (b * 10_000.0).round() / 10_000.0;
        prop_assume!((round_a - round_b).abs() < f64::EPSILON);

        let payload_a = json!({ "score": a });
        let payload_b = json!({ "score": b });

        prop_assert_eq!(
            canon_text(&payload_a),
            canon_text(&payload_b),
            "floats rounding to the same 4dp value must canon equally: a={} b={}", a, b
        );
    }
}

// ---------------------------------------------------------------------------
// MR6 — Integer passthrough: integer JSON numbers (i64/u64) must be
//       byte-identical before and after canonicalization. The float
//       rounding path is guarded by `fract() != 0.0`, so integer values
//       must not round-trip through f64 and lose precision.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn mr6_integer_numbers_pass_through_unchanged(
        value in any::<i64>(),
    ) {
        let before = json!({ "count": value });
        let after = canon_text(&before);

        // Parse back and assert the i64 is EXACTLY preserved. Going via
        // f64 would lose precision for |value| > 2^53.
        let parsed: Value = serde_json::from_str(&after).expect("roundtrip");
        let got = parsed.get("count").and_then(Value::as_i64);
        prop_assert_eq!(
            got,
            Some(value),
            "integer must pass through canon without f64 round-trip; got {:?}", got
        );
    }
}
