//! Skeleton conformance harness for the schema-driven robot-family
//! contract infrastructure (`ft-hac7w.1`).
//!
//! This file is the **shared harness** that future family beads
//! (`ft-hac7w.2`…`ft-hac7w.6`) plug into. The methodology — validate the
//! contract itself, materialize a real `BoxedStrategy<serde_json::Value>`
//! from each `ProptestStrategyHint`, validate proptest-generated
//! requests against the contract's JSON Schema, run each declared
//! invariant — is general; only the per-family handler is family-specific.
//!
//! The `profile` family is the proof-of-concept this skeleton ships
//! against (acceptance criterion §4 of `ft-hac7w.1`).
//!
//! When a future family closes, it adds:
//!
//! 1. A `<family>_family_contract()` factory in
//!    `crates/frankenterm-core/src/robot_family_contract.rs`.
//! 2. A `<family>_handler` in this harness (or a sibling test file)
//!    that takes a request `serde_json::Value` and returns the response
//!    `serde_json::Value` — the conformance check is closed over it.
//! 3. The four required tests (`<family>_contract_self_validates`,
//!    `<family>_proptest_inputs_validate_against_schema`,
//!    `<family>_handler_passes_declared_invariants`,
//!    `<family>_mcp_descriptors_unique`).

#![allow(deprecated)]

use std::collections::{BTreeMap, BTreeSet};

use frankenterm_core::robot_family_contract::{
    ActionContract, ContractInvariant, FamilyContract, InvariantKind, ProptestField,
    ProptestStrategyHint, SchemaKind, profile_family_contract,
};
use jsonschema::{Draft, JSONSchema as Validator};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use serde_json::{Value, json};

// ============================================================================
// Strategy translator — `ProptestStrategyHint` → `BoxedStrategy<Value>`.
//
// The lib carries hints (declarative); the harness owns the actual
// proptest combinators (imperative). Adding a new hint variant requires
// extending exactly this function.
// ============================================================================

fn strategy_for(hint: &ProptestStrategyHint) -> BoxedStrategy<Value> {
    match hint {
        ProptestStrategyHint::AsciiString { max_len } => {
            let max = *max_len;
            // ASCII printable, plus the empty string. Bound the length
            // so the corpus stays small.
            proptest::collection::vec(
                any::<u8>().prop_map(|b| {
                    let c = (b % 95) + 32;
                    c as char
                }),
                0..=max,
            )
            .prop_map(|chars| Value::String(chars.into_iter().collect()))
            .boxed()
        }
        ProptestStrategyHint::U32Range { min, max } => {
            let lo = *min;
            let hi = *max;
            (lo..=hi)
                .prop_map(|n| Value::Number(serde_json::Number::from(n)))
                .boxed()
        }
        ProptestStrategyHint::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
        ProptestStrategyHint::OptionString { max_len } => {
            let max = *max_len;
            proptest::option::of(
                proptest::collection::vec(
                    any::<u8>().prop_map(|b| {
                        let c = (b % 95) + 32;
                        c as char
                    }),
                    0..=max,
                )
                .prop_map(|chars| chars.into_iter().collect::<String>()),
            )
            .prop_map(|opt| match opt {
                Some(s) => Value::String(s),
                None => Value::Null,
            })
            .boxed()
        }
        ProptestStrategyHint::StringMap { max_entries } => {
            let max = *max_entries;
            proptest::collection::btree_map("[a-z]{1,8}", "[A-Za-z0-9]{0,16}", 0..=max)
                .prop_map(|m| {
                    let obj: serde_json::Map<String, Value> =
                        m.into_iter().map(|(k, v)| (k, Value::String(v))).collect();
                    Value::Object(obj)
                })
                .boxed()
        }
    }
}

/// Build a strategy that produces a `(action, params)` request envelope
/// matching the family's JSON Schema.
///
/// Picks a uniformly-random action and assembles its `params` from the
/// per-field strategies declared on the action contract.
fn family_request_strategy(contract: &FamilyContract) -> BoxedStrategy<Value> {
    let actions: Vec<ActionContract> = contract.actions.clone();
    proptest::sample::select(actions)
        .prop_flat_map(|action| {
            let action_name = action.action.clone();
            // Build a `Vec<Strategy<(name, value)>>` then collapse.
            let field_strategies: Vec<BoxedStrategy<(String, Value)>> = action
                .request_proptest
                .iter()
                .map(|f: &ProptestField| {
                    let n = f.name.clone();
                    strategy_for(&f.strategy)
                        .prop_map(move |v| (n.clone(), v))
                        .boxed()
                })
                .collect();
            // We also want to honor the schema's required-fields constraint
            // on the produced params — drop fields that produced JSON Null
            // for OptionString hints and aren't required.
            let action_for_filter = action.clone();
            field_strategies
                .prop_map(move |pairs| {
                    let mut params = serde_json::Map::new();
                    for (name, value) in pairs {
                        let required = action_for_filter
                            .request_schema
                            .fields
                            .iter()
                            .find(|f| f.name == name)
                            .is_some_and(|f| f.required);
                        if value.is_null() && !required {
                            // Treat null as "absent" for non-required fields.
                            continue;
                        }
                        params.insert(name, value);
                    }
                    json!({
                        "action": action_name.clone(),
                        "params": Value::Object(params),
                    })
                })
                .boxed()
        })
        .boxed()
}

fn compile_schema(schema: &Value) -> Validator {
    Validator::options()
        .with_draft(Draft::Draft202012)
        .compile(schema)
        .expect("contract JSON Schema compiles under Draft 2020-12")
}

// ============================================================================
// Test 1 — the contract itself is internally consistent.
// ============================================================================

#[test]
fn profile_contract_self_validates() {
    let contract = profile_family_contract();
    let errs = contract.validate();
    assert!(errs.is_empty(), "contract violations: {errs:?}");
}

// ============================================================================
// Test 2 — JSON Schema compiles under Draft 2020-12 and accepts a hand
// crafted exemplar request for every action. This is the boundary check
// that downstream client codegen will rely on.
// ============================================================================

#[test]
fn profile_contract_json_schema_accepts_action_exemplars() {
    let contract = profile_family_contract();
    let schema_value = contract.json_schema();
    let validator = compile_schema(&schema_value);

    // One concrete request per action, hand-built to satisfy the
    // declared required-fields constraint exactly.
    let exemplars = vec![
        json!({ "action": "list",     "params": {} }),
        json!({ "action": "show",     "params": { "name": "release-pipeline" } }),
        json!({ "action": "apply",    "params": { "name": "release-pipeline" } }),
        json!({ "action": "validate", "params": { "name": "release-pipeline" } }),
    ];
    for ex in &exemplars {
        if let Err(errs) = validator.validate(ex) {
            let collected: Vec<String> = errs.map(|e| e.to_string()).collect();
            panic!("exemplar {ex} failed validation: {collected:?}");
        }
    }
}

#[test]
fn profile_contract_json_schema_rejects_unknown_action() {
    let contract = profile_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let bad = json!({ "action": "does_not_exist", "params": {} });
    assert!(
        validator.validate(&bad).is_err(),
        "unknown action must be rejected"
    );
}

// ============================================================================
// Test 3 — proptest-generated requests validate against the schema.
//
// This is the load-bearing piece: the schema-DSL declaration produces
// both the hints AND the schema, and they agree. If a future family
// breaks the agreement (e.g. declares a hint for a field absent from the
// schema), this test fires.
// ============================================================================

#[test]
fn profile_contract_proptest_inputs_validate_against_schema() {
    let contract = profile_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let strategy = family_request_strategy(&contract);

    let mut runner = TestRunner::default();
    for _ in 0..128 {
        let value = strategy.new_tree(&mut runner).unwrap().current();
        if let Err(errs) = validator.validate(&value) {
            let collected: Vec<String> = errs.map(|e| e.to_string()).collect();
            panic!("proptest-generated request {value} failed schema: {collected:?}");
        }
    }
}

// ============================================================================
// Test 4 — MCP descriptors are unique and well-formed.
// ============================================================================

#[test]
fn profile_contract_mcp_descriptors_are_unique_and_well_formed() {
    let contract = profile_family_contract();
    let descriptors = contract.mcp_tool_descriptors();
    assert_eq!(descriptors.len(), contract.action_count());

    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    for d in &descriptors {
        assert!(
            d.name.starts_with("ft."),
            "mcp tool name should be dotted: {}",
            d.name
        );
        assert!(
            !d.description.is_empty(),
            "{} has empty description",
            d.name
        );
        assert!(
            d.input_schema
                .pointer("/type")
                .and_then(|v| v.as_str())
                .is_some(),
            "{} input_schema missing top-level type",
            d.name
        );
        assert!(seen_names.insert(d.name.as_str()), "duplicate {}", d.name);
    }
}

// ============================================================================
// Test 5 — invariant enumeration produces stable, unique test names.
//
// The harness will eventually `#[test]` one function per
// `(family, action, invariant)` triple; the harness can rely on the
// triple being unique for `cargo test --list` to be deterministic.
// ============================================================================

#[test]
fn profile_contract_invariants_have_unique_action_invariant_pairs() {
    let contract = profile_family_contract();
    let mut pairs: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (action_name, invariant) in contract.invariants() {
        let key = (action_name.to_string(), invariant.name.clone());
        *pairs.entry(key).or_insert(0) += 1;
    }
    for ((action, name), count) in &pairs {
        assert_eq!(
            *count, 1,
            "(action={action}, invariant={name}) repeats {count}× — names must be unique"
        );
    }
}

// ============================================================================
// Test 6 — proof-of-concept invariant runner.
//
// Demonstrates how a real family handler plugs into the harness. This
// is a pure stub handler that the lib can fully predict; future family
// beads swap in a real `ft robot <family> <action>` handler invocation.
// ============================================================================

/// Stub handler for the `profile` family. Pure function, no side
/// effects — proves the harness can run `Determinism` and
/// `ResponseShape` invariants end-to-end.
fn stub_profile_handler(action: &str, params: &Value) -> Value {
    match action {
        "list" => json!({ "profiles": [] }),
        "show" => json!({
            "name": params.get("name").and_then(Value::as_str).unwrap_or(""),
            "role": "stub",
        }),
        "apply" => json!({
            "profile_name": params.get("name").and_then(Value::as_str).unwrap_or(""),
            "panes_spawned": [],
            "dry_run": params
                .get("dry_run")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }),
        "validate" => json!({
            "name": params.get("name").and_then(Value::as_str).unwrap_or(""),
            "valid": true,
            "issues": [],
        }),
        other => panic!("stub handler does not know about action `{other}`"),
    }
}

fn run_invariant(
    contract: &FamilyContract,
    action: &ActionContract,
    invariant: &ContractInvariant,
    handler: &dyn Fn(&str, &Value) -> Value,
) {
    match &invariant.kind {
        InvariantKind::Determinism => {
            // Hand-pick a stable input — the harness covers the random
            // case in the proptest test above.
            let input = build_default_request(action);
            let a = handler(&action.action, &input);
            let b = handler(&action.action, &input);
            assert_eq!(
                a, b,
                "{}.{} {} failed: handler is not deterministic",
                contract.family_name, action.action, invariant.name
            );
        }
        InvariantKind::ResponseShape => {
            let input = build_default_request(action);
            let response = handler(&action.action, &input);
            // Compile the response_schema and check shape.
            let response_schema_value = action.response_schema.to_json_schema();
            let validator = compile_schema(&response_schema_value);
            if let Err(errs) = validator.validate(&response) {
                let collected: Vec<String> = errs.map(|e| e.to_string()).collect();
                panic!(
                    "{}.{} {} failed: response {response} does not validate: {collected:?}",
                    contract.family_name, action.action, invariant.name
                );
            }
        }
        InvariantKind::Idempotence => {
            // Stubbed: future family beads with mutating actions will
            // observe the side-effect surface (events_emitted / storage
            // tables) before/after a duplicated request and assert no
            // delta on the second invocation.
        }
        InvariantKind::AtomicOnFailure => {
            // Stubbed: future family beads will inject a mid-flight
            // failure and verify no entry in `side_effects.storage_tables_mutated`
            // grew, no event in `events_emitted` fired.
        }
        InvariantKind::Commutativity => {
            // Stubbed: future family beads with `Commutative` actions
            // will run two distinct requests in both orders and assert
            // identical final observable state.
        }
        InvariantKind::Custom { name } => {
            // Family-specific predicates dispatch by name. The skeleton
            // does nothing; per-family conformance test files supply
            // implementations keyed off `name`.
            let _ = name;
        }
    }
}

/// Build a minimal request that satisfies each `required` field of an
/// action's request schema. Used only for the proof-of-concept
/// invariant runner — the proptest path uses richer inputs.
fn build_default_request(action: &ActionContract) -> Value {
    let mut params = serde_json::Map::new();
    for f in &action.request_schema.fields {
        if !f.required {
            continue;
        }
        let v = match f.kind {
            SchemaKind::String => Value::String("default".to_string()),
            SchemaKind::Integer => Value::Number(serde_json::Number::from(1u64)),
            SchemaKind::Boolean => Value::Bool(false),
            SchemaKind::Array => Value::Array(Vec::new()),
            SchemaKind::Object => Value::Object(serde_json::Map::new()),
            SchemaKind::Null => Value::Null,
        };
        params.insert(f.name.clone(), v);
    }
    Value::Object(params)
}

#[test]
fn profile_stub_handler_passes_declared_invariants() {
    let contract = profile_family_contract();
    for action in &contract.actions {
        for invariant in &action.invariants {
            run_invariant(&contract, action, invariant, &stub_profile_handler);
        }
    }
}
