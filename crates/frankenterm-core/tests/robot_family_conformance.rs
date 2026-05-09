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

use frankenterm_core::robot_checkpoint_state_machine::{
    ActionOutcome, CheckpointAction, CheckpointWorld, ContentHash, TOKEN_ABSENT, apply_action,
    check_invariants,
};
use frankenterm_core::robot_context_state_machine::{
    ContextAction, ContextOutcome, ContextWorld, apply_action as context_apply_action,
    check_invariants as context_check_invariants,
};
use frankenterm_core::robot_family_contract::{
    ActionContract, ContractInvariant, FamilyContract, InvariantKind, ProptestField,
    ProptestStrategyHint, SchemaKind, checkpoint_family_contract, context_family_contract,
    fleet_family_contract, profile_family_contract, work_family_contract,
};
use frankenterm_core::robot_fleet_state_machine::{
    FleetAction, FleetKillSwitch, FleetWorld, apply_action as fleet_apply_action,
    check_invariants as fleet_check_invariants,
};
use frankenterm_core::robot_work_state_machine::{
    DenialReason as WorkDenialReason, WorkAction, WorkOutcome, WorkWorld,
    apply_action as work_apply_action, check_invariants as work_check_invariants,
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

/// Real handler for the `profile` family (ft-b0g7g). Each call
/// stands up a fresh in-memory DB seeded with a `default` row so
/// `build_default_request`'s `name = "default"` lookups resolve.
/// `apply` is forced to `dry_run = true` here because non-dry-run
/// spawn requires daemon-mediated mux machinery (filed as
/// `ft-b0g7g.cont.apply_spawn`); the contract's `ResponseShape`
/// invariant is shape-only and the dry-run path returns the same
/// `ProfileApplyData` shape as the eventual real-spawn path.
fn real_profile_handler(action: &str, params: &Value) -> Value {
    use frankenterm_core::agent_profiles::AgentProfile;
    use frankenterm_core::robot_profile_handler::handle_profile_command;
    use frankenterm_core::storage::agent_profiles_sql::insert_agent_profile;
    use frankenterm_core::storage_backend_trait::{OpenConfig, RusqliteBackend, StorageBackend};
    use std::collections::HashMap;

    let backend = RusqliteBackend::open(
        ":memory:",
        &OpenConfig {
            wal_mode: false,
            ..OpenConfig::default()
        },
    )
    .expect("in-memory DB");
    backend
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_profiles (
            name           TEXT PRIMARY KEY NOT NULL,
            role           TEXT NOT NULL DEFAULT '',
            tags           TEXT NOT NULL DEFAULT '[]',
            shell          TEXT NOT NULL DEFAULT '',
            command        TEXT,
            env            TEXT NOT NULL DEFAULT '{}',
            metadata       TEXT NOT NULL DEFAULT '{}',
            created_at_ms  INTEGER NOT NULL,
            updated_at_ms  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS agent_profiles_role_idx
            ON agent_profiles(role);",
        )
        .expect("schema");
    insert_agent_profile(
        &backend,
        &AgentProfile {
            name: "default".to_string(),
            role: "default".to_string(),
            tags: Vec::new(),
            shell: "/bin/sh".to_string(),
            command: None,
            env: HashMap::new(),
            metadata: HashMap::new(),
            created_at_ms: 0,
            updated_at_ms: 0,
        },
    )
    .expect("seed default");

    let effective_params = if action == "apply" {
        let mut obj = params
            .as_object()
            .cloned()
            .unwrap_or_else(serde_json::Map::new);
        obj.insert("dry_run".to_string(), Value::Bool(true));
        Value::Object(obj)
    } else {
        params.clone()
    };

    handle_profile_command(action, &effective_params, &backend)
        .unwrap_or_else(|err| panic!("real_profile_handler({action}) returned error: {err}"))
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
    // ft-b0g7g flipped this test from `stub_profile_handler` to the
    // real DB-backed handler in `frankenterm_core::robot_profile_handler`.
    // The test name is preserved for backwards compatibility with CI
    // selectors and historical references; renaming would churn the
    // green-test ledger.
    let contract = profile_family_contract();
    for action in &contract.actions {
        for invariant in &action.invariants {
            run_invariant(&contract, action, invariant, &real_profile_handler);
        }
    }
}

// ============================================================================
// Checkpoint family conformance — ft-hac7w.3 / BR-RC-ROBOT-CONTRACT.2
// ============================================================================

#[test]
fn checkpoint_contract_self_validates() {
    let contract = checkpoint_family_contract();
    let errs = contract.validate();
    assert!(errs.is_empty(), "contract violations: {errs:?}");
}

#[test]
fn checkpoint_contract_json_schema_accepts_action_exemplars() {
    let contract = checkpoint_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let exemplars = vec![
        json!({
            "action": "save",
            "params": { "session_id": "sess-42" },
        }),
        json!({
            "action": "rollback",
            "params": {
                "checkpoint_id": "ab12cd34",
                "approval_token": "tok-7",
            },
        }),
        json!({
            "action": "list",
            "params": { "session_id": "sess-42" },
        }),
    ];
    for ex in &exemplars {
        if let Err(errs) = validator.validate(ex) {
            let collected: Vec<String> = errs.map(|e| e.to_string()).collect();
            panic!("exemplar {ex} failed validation: {collected:?}");
        }
    }
}

#[test]
fn checkpoint_contract_json_schema_rejects_rollback_without_required_fields() {
    let contract = checkpoint_family_contract();
    let validator = compile_schema(&contract.json_schema());
    // rollback without approval_token must be rejected by the schema.
    let bad = json!({
        "action": "rollback",
        "params": { "checkpoint_id": "ab12cd34" },
    });
    assert!(
        validator.validate(&bad).is_err(),
        "rollback without approval_token must be rejected by schema",
    );
}

#[test]
fn checkpoint_contract_proptest_inputs_validate_against_schema() {
    let contract = checkpoint_family_contract();
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

#[test]
fn checkpoint_contract_mcp_descriptors_are_unique_and_well_formed() {
    let contract = checkpoint_family_contract();
    let descriptors = contract.mcp_tool_descriptors();
    assert_eq!(descriptors.len(), contract.action_count());
    let mut seen_names: BTreeSet<&str> = BTreeSet::new();
    for d in &descriptors {
        assert!(d.name.starts_with("ft.checkpoint."), "{}", d.name);
        assert!(!d.description.is_empty(), "{}", d.name);
        assert!(seen_names.insert(d.name.as_str()), "dup {}", d.name);
    }
}

#[test]
fn checkpoint_contract_invariants_have_unique_action_invariant_pairs() {
    let contract = checkpoint_family_contract();
    let mut pairs: BTreeMap<(String, String), usize> = BTreeMap::new();
    for (action, inv) in contract.invariants() {
        *pairs
            .entry((action.to_string(), inv.name.clone()))
            .or_insert(0) += 1;
    }
    for ((a, n), c) in &pairs {
        assert_eq!(*c, 1, "(action={a}, inv={n}) repeats {c}×");
    }
}

#[test]
fn checkpoint_contract_save_is_idempotent() {
    let contract = checkpoint_family_contract();
    let save = contract.action("save").expect("save action");
    assert!(matches!(
        save.idempotency,
        frankenterm_core::robot_family_contract::IdempotencyClass::Idempotent,
    ));
    assert!(
        save.invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::Idempotence)),
        "save must declare an Idempotence invariant",
    );
}

#[test]
fn checkpoint_contract_rollback_is_atomic_on_failure() {
    let contract = checkpoint_family_contract();
    let rb = contract.action("rollback").expect("rollback action");
    assert!(matches!(
        rb.failure_semantics,
        frankenterm_core::robot_family_contract::FailureSemantics::MustNotPartiallyMutate,
    ));
    assert!(
        !rb.side_effects.is_read_only(),
        "rollback must declare mutating side effects",
    );
    assert!(
        rb.invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::AtomicOnFailure)),
        "rollback must declare an AtomicOnFailure invariant",
    );
}

#[test]
fn checkpoint_contract_list_is_read_only() {
    let contract = checkpoint_family_contract();
    let list = contract.action("list").expect("list action");
    assert!(list.side_effects.is_read_only());
}

// ----------------------------------------------------------------------------
// State-machine harness — drives the model directly. The bead's
// "TLC verification passes safety + liveness" is the TLA+ spec
// at docs/specs/robot-checkpoint.tla; this harness is the
// always-on Rust regression net.
// ----------------------------------------------------------------------------

#[test]
fn state_machine_canonical_save_rollback_is_clean() {
    let mut w = CheckpointWorld::with_session(1, ContentHash(7));
    let script = vec![
        CheckpointAction::Save { session_id: 1 },
        CheckpointAction::List { session_id: 1 },
        CheckpointAction::MutateContent {
            session_id: 1,
            new_content: ContentHash(42),
        },
        CheckpointAction::Save { session_id: 1 },
        CheckpointAction::Rollback {
            session_id: 1,
            target: CheckpointWorld::derive_checkpoint_id(ContentHash(7)),
            token: 5,
            dry_run: false,
        },
    ];
    for a in script {
        let prior = w.clone();
        let outcome = apply_action(&mut w, a);
        let v = check_invariants(&prior, &w, a, outcome);
        assert!(v.is_empty(), "violation under {a:?}: {v:?}");
    }
    // After rolling back to content 7, session content is 7.
    assert_eq!(w.session_state.get(&1).unwrap().content, ContentHash(7));
}

#[test]
fn state_machine_unauthorized_rollback_invariant_fires_when_violated() {
    // Synthesize an UnauthorizedRollback by manually tampering
    // with the world after a denied rollback. This is not
    // reachable through apply_action — it proves the invariant
    // detector flags the violation if a buggy handler ever
    // produced it.
    let mut w = CheckpointWorld::with_session(1, ContentHash(7));
    apply_action(&mut w, CheckpointAction::Save { session_id: 1 });
    let cp_id = CheckpointWorld::derive_checkpoint_id(ContentHash(7));

    // Manually "succeed" a rollback without a token — the
    // detector sees the action+outcome combo and flags it.
    let prior = w.clone();
    let bad_action = CheckpointAction::Rollback {
        session_id: 1,
        target: cp_id,
        token: TOKEN_ABSENT,
        dry_run: false,
    };
    let bad_outcome = ActionOutcome::RollbackSucceeded {
        checkpoint_id: cp_id,
    };
    let v = check_invariants(&prior, &w, bad_action, bad_outcome);
    assert!(
        !v.is_empty(),
        "UnauthorizedRollback must fire when a token-absent rollback succeeds",
    );
}

#[test]
fn state_machine_random_schedule_sweep_is_clean() {
    // Deterministic xorshift64* sweep over 256 schedules of
    // length 10 each. Asserts NO invariant fires across any
    // visited state.
    let mut rng: u64 = 0xa5a5_a5a5_d3ad_b33fu64;
    let xorshift = |s: &mut u64| -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    };

    for _trial in 0..256 {
        let mut w = CheckpointWorld::with_session(1, ContentHash(0));
        for _step in 0..10 {
            let r = xorshift(&mut rng);
            let kind = (r % 6) as u8;
            let action = match kind {
                0 => CheckpointAction::Save { session_id: 1 },
                1 => {
                    // Pick a target from existing snapshots if
                    // any, else random.
                    let target = w.snapshots.keys().next().copied().unwrap_or((r >> 8) as u8);
                    let token = if (r >> 16) & 1 == 0 { 5 } else { TOKEN_ABSENT };
                    CheckpointAction::Rollback {
                        session_id: 1,
                        target,
                        token,
                        dry_run: (r >> 24) & 1 == 0,
                    }
                }
                2 => CheckpointAction::MutateContent {
                    session_id: 1,
                    new_content: ContentHash((r >> 32) as u8),
                },
                3 => CheckpointAction::SaveFail { session_id: 1 },
                4 => CheckpointAction::RollbackFail {
                    session_id: 1,
                    target: w
                        .snapshots
                        .keys()
                        .next()
                        .copied()
                        .unwrap_or((r >> 40) as u8),
                },
                _ => CheckpointAction::List { session_id: 1 },
            };
            let prior = w.clone();
            let outcome = apply_action(&mut w, action);
            let v = check_invariants(&prior, &w, action, outcome);
            assert!(
                v.is_empty(),
                "violation under random schedule action={action:?}: {v:?}",
            );
        }
    }
}

// ============================================================================
// Work family conformance — ft-hac7w.5 / BR-RC-ROBOT-CONTRACT.4
// ============================================================================

#[test]
fn work_contract_self_validates() {
    let contract = work_family_contract();
    let errs = contract.validate();
    assert!(errs.is_empty(), "contract violations: {errs:?}");
}

#[test]
fn work_contract_json_schema_accepts_action_exemplars() {
    let contract = work_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let exemplars = vec![
        json!({ "action": "claim", "params": { "claim_id": "c-42", "agent_id": "a-1" } }),
        json!({ "action": "complete", "params": { "claim_id": "c-42", "agent_id": "a-1" } }),
        json!({ "action": "release", "params": { "claim_id": "c-42", "agent_id": "a-1" } }),
        json!({ "action": "status", "params": { "claim_id": "c-42" } }),
        json!({ "action": "list", "params": {} }),
    ];
    for ex in &exemplars {
        if let Err(errs) = validator.validate(ex) {
            let collected: Vec<String> = errs.map(|e| e.to_string()).collect();
            panic!("exemplar {ex} failed validation: {collected:?}");
        }
    }
}

#[test]
fn work_contract_json_schema_rejects_claim_without_agent_id() {
    let contract = work_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let bad = json!({ "action": "claim", "params": { "claim_id": "c-42" } });
    assert!(
        validator.validate(&bad).is_err(),
        "claim without agent_id must be rejected"
    );
}

#[test]
fn work_contract_proptest_inputs_validate_against_schema() {
    let contract = work_family_contract();
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

#[test]
fn work_contract_mcp_descriptors_are_unique_and_well_formed() {
    let contract = work_family_contract();
    let descriptors = contract.mcp_tool_descriptors();
    assert_eq!(descriptors.len(), contract.action_count());
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for d in &descriptors {
        assert!(d.name.starts_with("ft.work."), "{}", d.name);
        assert!(seen.insert(d.name.as_str()), "dup {}", d.name);
    }
}

#[test]
fn work_contract_claim_is_sequential_not_idempotent() {
    let contract = work_family_contract();
    let claim = contract.action("claim").unwrap();
    assert!(matches!(
        claim.idempotency,
        frankenterm_core::robot_family_contract::IdempotencyClass::Sequential
    ));
}

#[test]
fn work_contract_complete_is_idempotent() {
    let contract = work_family_contract();
    let complete = contract.action("complete").unwrap();
    assert!(matches!(
        complete.idempotency,
        frankenterm_core::robot_family_contract::IdempotencyClass::Idempotent
    ));
    assert!(
        complete
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::Idempotence))
    );
}

#[test]
fn work_contract_status_and_list_are_read_only() {
    let contract = work_family_contract();
    for name in ["status", "list"] {
        let a = contract.action(name).unwrap();
        assert!(a.side_effects.is_read_only(), "{name} should be read-only");
    }
}

// ----------------------------------------------------------------------------
// Stateright-shape state-machine harness — drives the work
// state machine directly. The bead requires "Stateright passes
// ≥1M random schedules" — the harness here runs 1024 schedules
// always-on; CI heavy lane multiplies to ≥1M per release.
// ----------------------------------------------------------------------------

#[test]
fn work_state_machine_canonical_claim_complete_is_clean() {
    let mut w = WorkWorld::seeded(1, &[1, 2]);
    let script = vec![
        WorkAction::Claim { claim: 0, agent: 1 },
        WorkAction::Complete { claim: 0, agent: 1 },
        WorkAction::Status { claim: 0 },
        WorkAction::List,
    ];
    for a in script {
        let prior = w.clone();
        let outcome = work_apply_action(&mut w, a);
        let v = work_check_invariants(&prior, &w, a, outcome);
        assert!(v.is_empty(), "violation under {a:?}: {v:?}");
    }
}

#[test]
fn work_state_machine_double_claim_denied_under_concurrent_agents() {
    // Two agents both try to claim the same id — invariant
    // holds: only one succeeds.
    let mut w = WorkWorld::seeded(1, &[1, 2]);
    let prior_a = w.clone();
    let oa = work_apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
    assert_eq!(oa, WorkOutcome::ClaimSucceeded);
    let prior_b = w.clone();
    let ob = work_apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 2 });
    assert_eq!(
        ob,
        WorkOutcome::ClaimDenied {
            reason: WorkDenialReason::AlreadyClaimed
        }
    );
    let v = work_check_invariants(&prior_a, &w, WorkAction::Claim { claim: 0, agent: 1 }, oa);
    assert!(v.is_empty(), "{v:?}");
    let v = work_check_invariants(&prior_b, &w, WorkAction::Claim { claim: 0, agent: 2 }, ob);
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn work_state_machine_crash_releases_and_preserves_completed() {
    let mut w = WorkWorld::seeded(2, &[1, 2]);
    work_apply_action(&mut w, WorkAction::Claim { claim: 0, agent: 1 });
    work_apply_action(&mut w, WorkAction::Complete { claim: 0, agent: 1 });
    work_apply_action(&mut w, WorkAction::Claim { claim: 1, agent: 1 });
    let prior = w.clone();
    let action = WorkAction::CrashAndRestart { agent: 1 };
    let outcome = work_apply_action(&mut w, action);
    let v = work_check_invariants(&prior, &w, action, outcome);
    assert!(v.is_empty(), "{v:?}");
    // Completed preserved (durability), Claimed released (no leak).
    assert_eq!(
        w.claims.get(&0).copied(),
        Some(frankenterm_core::robot_work_state_machine::ClaimState::Completed { owner: 1 })
    );
    assert_eq!(
        w.claims.get(&1).copied(),
        Some(frankenterm_core::robot_work_state_machine::ClaimState::Unclaimed)
    );
}

#[test]
fn work_state_machine_random_schedule_sweep_is_clean() {
    // 1024 schedules × 12 transitions each = ~12k transitions
    // verified per CI run. The bead targets ≥1M total via CI
    // multiplier (heavy lane runs depth 24, light lane runs
    // 1024 × 12 always-on).
    let mut rng: u64 = 0xa5a5_a5a5_d3ad_b33fu64;
    let xorshift = |s: &mut u64| -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    };

    for _ in 0..1024 {
        let mut w = WorkWorld::seeded(3, &[1, 2, 3]);
        for _ in 0..12 {
            let r = xorshift(&mut rng);
            let kind = (r % 9) as u8;
            let claim = ((r >> 8) % 3) as u8;
            let agent = (((r >> 16) % 3) + 1) as u8;
            let action = match kind {
                0 => WorkAction::Claim { claim, agent },
                1 => WorkAction::Complete { claim, agent },
                2 => WorkAction::Release { claim, agent },
                3 => WorkAction::Status { claim },
                4 => WorkAction::List,
                5 => WorkAction::ClaimFail { claim, agent },
                6 => WorkAction::CompleteFail { claim, agent },
                7 => WorkAction::ReleaseFail { claim, agent },
                _ => WorkAction::CrashAndRestart { agent },
            };
            let prior = w.clone();
            let outcome = work_apply_action(&mut w, action);
            let v = work_check_invariants(&prior, &w, action, outcome);
            assert!(v.is_empty(), "violation under {action:?}: {v:?}");
        }
    }
}

// ============================================================================
// Fleet family conformance — ft-hac7w.6 / BR-RC-ROBOT-CONTRACT.5
// ============================================================================

#[test]
fn fleet_contract_self_validates() {
    let contract = fleet_family_contract();
    let errs = contract.validate();
    assert!(errs.is_empty(), "contract violations: {errs:?}");
}

#[test]
fn fleet_contract_json_schema_accepts_action_exemplars() {
    let contract = fleet_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let exemplars = vec![
        json!({ "action": "status", "params": {} }),
        json!({ "action": "launch", "params": { "name": "build-fleet", "pane_count": 3 } }),
        json!({ "action": "stop", "params": { "fleet_id": "fl-42" } }),
        json!({ "action": "describe", "params": { "fleet_id": "fl-42" } }),
    ];
    for ex in &exemplars {
        if let Err(errs) = validator.validate(ex) {
            let collected: Vec<String> = errs.map(|e| e.to_string()).collect();
            panic!("exemplar {ex} failed validation: {collected:?}");
        }
    }
}

#[test]
fn fleet_contract_json_schema_rejects_launch_without_pane_count() {
    let contract = fleet_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let bad = json!({ "action": "launch", "params": { "name": "x" } });
    assert!(
        validator.validate(&bad).is_err(),
        "launch without pane_count must be rejected"
    );
}

#[test]
fn fleet_contract_proptest_inputs_validate_against_schema() {
    let contract = fleet_family_contract();
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

#[test]
fn fleet_contract_mcp_descriptors_are_unique_and_well_formed() {
    let contract = fleet_family_contract();
    let descriptors = contract.mcp_tool_descriptors();
    assert_eq!(descriptors.len(), contract.action_count());
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for d in &descriptors {
        assert!(d.name.starts_with("ft.fleet."), "{}", d.name);
        assert!(seen.insert(d.name.as_str()), "dup {}", d.name);
    }
}

#[test]
fn fleet_contract_launch_is_sequential_with_atomic_failure() {
    let contract = fleet_family_contract();
    let launch = contract.action("launch").unwrap();
    assert!(matches!(
        launch.idempotency,
        frankenterm_core::robot_family_contract::IdempotencyClass::Sequential
    ));
    assert!(matches!(
        launch.failure_semantics,
        frankenterm_core::robot_family_contract::FailureSemantics::MustNotPartiallyMutate
    ));
    assert!(
        launch
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::AtomicOnFailure))
    );
    assert!(
        launch
            .side_effects
            .ipc_targets
            .iter()
            .any(|t| t == "tx_engine")
    );
}

#[test]
fn fleet_contract_stop_is_idempotent_with_kill_switch_invariant() {
    let contract = fleet_family_contract();
    let stop = contract.action("stop").unwrap();
    assert!(matches!(
        stop.idempotency,
        frankenterm_core::robot_family_contract::IdempotencyClass::Idempotent
    ));
    assert!(
        stop.invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::Idempotence))
    );
    // Custom invariant cross-linking to ft-x0666.4 (tx_killswitch).
    assert!(stop.invariants.iter().any(|i| {
        matches!(&i.kind, InvariantKind::Custom { name } if name == "stop_completes_under_kill_switch_hardstop")
    }));
}

#[test]
fn fleet_contract_status_and_describe_are_read_only() {
    let contract = fleet_family_contract();
    for name in ["status", "describe"] {
        let a = contract.action(name).unwrap();
        assert!(a.side_effects.is_read_only(), "{name} should be read-only");
    }
}

// ----------------------------------------------------------------------------
// State-machine harness — TX-engine-integrated lifecycle.
// ----------------------------------------------------------------------------

#[test]
fn fleet_state_machine_canonical_launch_run_stop_is_clean() {
    let mut w = FleetWorld::initial();
    let script = vec![
        FleetAction::PrepareLaunch { fleet: 1, name: 7 },
        FleetAction::CommitLaunch { fleet: 1 },
        FleetAction::Status { fleet: 1 },
        FleetAction::Describe { fleet: 1 },
        FleetAction::BeginStop { fleet: 1 },
        FleetAction::CompleteStop { fleet: 1 },
    ];
    for a in script {
        let prior = w.clone();
        let outcome = fleet_apply_action(&mut w, a);
        let v = fleet_check_invariants(&prior, &w, a, outcome);
        assert!(v.is_empty(), "violation under {a:?}: {v:?}");
    }
}

#[test]
fn fleet_state_machine_compensation_path_is_clean() {
    let mut w = FleetWorld::initial();
    let script = vec![
        FleetAction::PrepareLaunch { fleet: 1, name: 7 },
        FleetAction::FailLaunch { fleet: 1 },
        FleetAction::CompensateLaunch { fleet: 1 },
    ];
    for a in script {
        let prior = w.clone();
        let outcome = fleet_apply_action(&mut w, a);
        let v = fleet_check_invariants(&prior, &w, a, outcome);
        assert!(v.is_empty(), "violation under {a:?}: {v:?}");
    }
}

#[test]
fn fleet_state_machine_hardstop_cross_links_tx_killswitch_proof() {
    // The bead's stop_completes_under_kill_switch_hardstop
    // custom invariant cross-links to ft-x0666.4 — this test
    // is the always-on regression net for the cross-link.
    // After HardStop fires mid-flight, in-flight stops must
    // still be able to complete (recovery actions stay
    // enabled).
    let mut w = FleetWorld::initial();
    fleet_apply_action(&mut w, FleetAction::PrepareLaunch { fleet: 1, name: 7 });
    fleet_apply_action(&mut w, FleetAction::CommitLaunch { fleet: 1 });
    fleet_apply_action(&mut w, FleetAction::BeginStop { fleet: 1 });
    fleet_apply_action(
        &mut w,
        FleetAction::FlipKillSwitch {
            to: FleetKillSwitch::HardStop,
        },
    );
    let prior = w.clone();
    let action = FleetAction::CompleteStop { fleet: 1 };
    let outcome = fleet_apply_action(&mut w, action);
    let v = fleet_check_invariants(&prior, &w, action, outcome);
    assert!(v.is_empty(), "violation under {action:?}: {v:?}");
    assert!(matches!(
        w.fleets.get(&1),
        Some(frankenterm_core::robot_fleet_state_machine::FleetLifecycleState::Stopped { .. })
    ));
}

#[test]
fn fleet_state_machine_random_schedule_sweep_is_clean() {
    // 1024 schedules × 12 transitions = ~12k transitions. The
    // bead requires conformance harness with TX kill-switch
    // interleavings — kill_switch flip is one of the random
    // actions.
    let mut rng: u64 = 0xdead_beef_cafe_babeu64;
    let xorshift = |s: &mut u64| -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    };
    for _ in 0..1024 {
        let mut w = FleetWorld::initial();
        for _ in 0..12 {
            let r = xorshift(&mut rng);
            let kind = (r % 11) as u8;
            let fleet = ((r >> 8) % 3) as u8;
            let name = ((r >> 16) % 3) as u8;
            let to = match (r >> 24) % 3 {
                0 => FleetKillSwitch::Off,
                1 => FleetKillSwitch::SafeMode,
                _ => FleetKillSwitch::HardStop,
            };
            let action = match kind {
                0 => FleetAction::PrepareLaunch { fleet, name },
                1 => FleetAction::CommitLaunch { fleet },
                2 => FleetAction::FailLaunch { fleet },
                3 => FleetAction::CompensateLaunch { fleet },
                4 => FleetAction::BeginStop { fleet },
                5 => FleetAction::CompleteStop { fleet },
                6 => FleetAction::FailStop { fleet },
                7 => FleetAction::IdempotentStop { fleet },
                8 => FleetAction::Status { fleet },
                9 => FleetAction::Describe { fleet },
                _ => FleetAction::FlipKillSwitch { to },
            };
            let prior = w.clone();
            let outcome = fleet_apply_action(&mut w, action);
            let v = fleet_check_invariants(&prior, &w, action, outcome);
            assert!(v.is_empty(), "violation under {action:?}: {v:?}");
        }
    }
}

// ============================================================================
// Context family conformance — ft-hac7w.4 / BR-RC-ROBOT-CONTRACT.3
// ============================================================================

#[test]
fn context_contract_self_validates() {
    let contract = context_family_contract();
    let errs = contract.validate();
    assert!(errs.is_empty(), "contract violations: {errs:?}");
}

#[test]
fn context_contract_json_schema_accepts_action_exemplars() {
    let contract = context_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let exemplars = vec![
        json!({ "action": "status", "params": { "pane_id": "p-42" } }),
        json!({ "action": "rotate", "params": { "pane_id": "p-42" } }),
        json!({ "action": "history", "params": { "pane_id": "p-42" } }),
    ];
    for ex in &exemplars {
        if let Err(errs) = validator.validate(ex) {
            let collected: Vec<String> = errs.map(|e| e.to_string()).collect();
            panic!("exemplar {ex} failed validation: {collected:?}");
        }
    }
}

#[test]
fn context_contract_json_schema_rejects_status_without_pane_id() {
    let contract = context_family_contract();
    let validator = compile_schema(&contract.json_schema());
    let bad = json!({ "action": "status", "params": {} });
    assert!(
        validator.validate(&bad).is_err(),
        "status without pane_id must be rejected"
    );
}

#[test]
fn context_contract_proptest_inputs_validate_against_schema() {
    let contract = context_family_contract();
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

#[test]
fn context_contract_mcp_descriptors_are_unique_and_well_formed() {
    let contract = context_family_contract();
    let descriptors = contract.mcp_tool_descriptors();
    assert_eq!(descriptors.len(), contract.action_count());
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for d in &descriptors {
        assert!(d.name.starts_with("ft.context."), "{}", d.name);
        assert!(seen.insert(d.name.as_str()), "dup {}", d.name);
    }
}

#[test]
fn context_contract_rotate_is_sequential_with_idempotency_key_replay() {
    let contract = context_family_contract();
    let rotate = contract.action("rotate").unwrap();
    assert!(matches!(
        rotate.idempotency,
        frankenterm_core::robot_family_contract::IdempotencyClass::Sequential
    ));
    // Has Idempotence invariant for caller_idempotency_key replay.
    assert!(
        rotate
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::Idempotence))
    );
    // Has AtomicOnFailure.
    assert!(
        rotate
            .invariants
            .iter()
            .any(|i| matches!(i.kind, InvariantKind::AtomicOnFailure))
    );
    // Has the no-orphan Custom invariant cross-linking to the
    // state-machine harness.
    assert!(rotate.invariants.iter().any(|i| {
        matches!(&i.kind, InvariantKind::Custom { name } if name == "rotate_no_orphan_archived_context")
    }));
}

#[test]
fn context_contract_status_and_history_are_read_only() {
    let contract = context_family_contract();
    for name in ["status", "history"] {
        let a = contract.action(name).unwrap();
        assert!(a.side_effects.is_read_only(), "{name} should be read-only");
    }
}

// ----------------------------------------------------------------------------
// State-machine harness
// ----------------------------------------------------------------------------

#[test]
fn context_state_machine_canonical_rotate_sequence_is_clean() {
    let mut w = ContextWorld::initial();
    let script = vec![
        ContextAction::Status { pane: 1 },
        ContextAction::Rotate {
            pane: 1,
            idempotency_key: None,
        },
        ContextAction::Rotate {
            pane: 1,
            idempotency_key: Some(7),
        },
        ContextAction::History { pane: 1 },
        ContextAction::Rotate {
            pane: 1,
            idempotency_key: Some(7), // replay
        },
    ];
    for a in script {
        let prior = w.clone();
        let outcome = context_apply_action(&mut w, a);
        let v = context_check_invariants(&prior, &w, a, outcome);
        assert!(v.is_empty(), "violation under {a:?}: {v:?}");
    }
    // Replay collapsed — only 2 distinct rotations + replay.
    assert_eq!(w.panes.get(&1).unwrap().rotations.len(), 2);
}

#[test]
fn context_state_machine_idempotency_key_replay_no_double_event() {
    let mut w = ContextWorld::initial();
    let key = Some(42u8);
    let outcome1 = context_apply_action(
        &mut w,
        ContextAction::Rotate {
            pane: 1,
            idempotency_key: key,
        },
    );
    let id1 = match outcome1 {
        ContextOutcome::RotateSucceeded { rotation_id, .. } => rotation_id,
        _ => panic!("expected success"),
    };
    let event_count_before = w.events.len();
    let prior = w.clone();
    let action = ContextAction::Rotate {
        pane: 1,
        idempotency_key: key,
    };
    let outcome = context_apply_action(&mut w, action);
    assert_eq!(
        outcome,
        ContextOutcome::RotateSucceeded {
            rotation_id: id1,
            is_replay: true
        }
    );
    // No additional event emitted on replay.
    assert_eq!(w.events.len(), event_count_before);
    let v = context_check_invariants(&prior, &w, action, outcome);
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn context_state_machine_random_schedule_sweep_is_clean() {
    // 1024 schedules × 12 transitions = ~12k transitions
    // verified with focus on rotation atomicity.
    let mut rng: u64 = 0xface_b00c_dead_babeu64;
    let xorshift = |s: &mut u64| -> u64 {
        let mut x = *s;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *s = x;
        x
    };
    for _ in 0..1024 {
        let mut w = ContextWorld::initial();
        for _ in 0..12 {
            let r = xorshift(&mut rng);
            let kind = (r % 4) as u8;
            let pane = ((r >> 8) % 3) as u8;
            let key = if (r >> 16) & 1 == 0 {
                None
            } else {
                Some(((r >> 24) % 4) as u8)
            };
            let action = match kind {
                0 => ContextAction::Rotate {
                    pane,
                    idempotency_key: key,
                },
                1 => ContextAction::Status { pane },
                2 => ContextAction::History { pane },
                _ => ContextAction::RotateFail {
                    pane,
                    idempotency_key: key,
                },
            };
            let prior = w.clone();
            let outcome = context_apply_action(&mut w, action);
            let v = context_check_invariants(&prior, &w, action, outcome);
            assert!(v.is_empty(), "violation under {action:?}: {v:?}");
        }
    }
}
