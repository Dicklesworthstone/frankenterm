use std::collections::HashMap;

use frankenterm_core::agent_profiles::{
    AGENT_PROFILES_ROLE_INDEX, AGENT_PROFILES_SCHEMA, AgentProfile,
};
use frankenterm_core::robot_family_contract::{
    ActionContract, ProptestField, ProptestStrategyHint, SchemaKind, profile_family_contract,
};
use frankenterm_core::robot_ntm_differential::{DifferentialHarness, NtmInvoker};
use frankenterm_core::robot_profile_handler::handle_profile_command;
use frankenterm_core::storage::agent_profiles_sql::insert_agent_profile;
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use rusqlite::Connection;
use serde_json::{Value, json};

const PROFILE_DIFFERENTIAL_CASES: usize = 1_000;

struct ProfileNtmMirror;

impl NtmInvoker for ProfileNtmMirror {
    fn invoke(&self, family: &str, action: &str, request: &Value) -> Result<Value, String> {
        if family != "profile" {
            return Err(format!(
                "ProfileNtmMirror only handles profile, got {family}"
            ));
        }
        Ok(profile_response_envelope(action, request))
    }
}

fn profile_response_envelope(action: &str, request: &Value) -> Value {
    let conn = seeded_profile_conn();
    match handle_profile_command(action, request, &conn) {
        Ok(data) => json!({
            "ok": true,
            "data": data,
        }),
        Err(err) => json!({
            "ok": false,
            "error_code": err.error_code(),
            "message": err.to_string(),
        }),
    }
}

fn seeded_profile_conn() -> Connection {
    let conn = Connection::open_in_memory().expect("open in-memory profile DB");
    conn.execute_batch(&format!(
        "{AGENT_PROFILES_SCHEMA};\n{AGENT_PROFILES_ROLE_INDEX};"
    ))
    .expect("agent_profiles schema");

    for profile in [
        profile("default", "worker", &["stable", "default"]),
        profile("ops", "operator", &["stable", "ops"]),
        profile("empty-tags", "worker", &[]),
    ] {
        insert_agent_profile(&conn, &profile).expect("seed profile");
    }

    conn
}

fn profile(name: &str, role: &str, tags: &[&str]) -> AgentProfile {
    let mut metadata = HashMap::new();
    metadata.insert("description".to_string(), format!("{name} profile"));
    AgentProfile {
        name: name.to_string(),
        role: role.to_string(),
        tags: tags.iter().copied().map(str::to_string).collect(),
        shell: "/bin/sh".to_string(),
        command: None,
        env: HashMap::new(),
        metadata,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

fn strategy_for(hint: &ProptestStrategyHint) -> BoxedStrategy<Value> {
    match hint {
        ProptestStrategyHint::AsciiString { max_len } => {
            let max = *max_len;
            prop::collection::vec("[A-Za-z0-9_-]", 0..=max)
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
            prop::option::of(prop::collection::vec("[A-Za-z0-9_-]", 0..=max))
                .prop_map(|maybe_chars| match maybe_chars {
                    Some(chars) => Value::String(chars.into_iter().collect()),
                    None => Value::Null,
                })
                .boxed()
        }
        ProptestStrategyHint::StringMap { max_entries } => {
            let max = *max_entries;
            prop::collection::btree_map(
                "[A-Za-z_][A-Za-z0-9_]{0,12}",
                "[A-Za-z0-9_ ./-]{0,24}",
                0..=max,
            )
            .prop_map(|map| {
                Value::Object(
                    map.into_iter()
                        .map(|(key, value)| (key, Value::String(value)))
                        .collect(),
                )
            })
            .boxed()
        }
    }
}

fn request_strategy(action: &ActionContract) -> BoxedStrategy<Value> {
    let action_for_filter = action.clone();
    let field_strategies: Vec<BoxedStrategy<(String, Value)>> = action
        .request_proptest
        .iter()
        .map(|field: &ProptestField| {
            let name = field.name.clone();
            strategy_for(&field.strategy)
                .prop_map(move |value| (name.clone(), value))
                .boxed()
        })
        .collect();

    field_strategies
        .prop_map(move |pairs| {
            let mut params = serde_json::Map::new();
            for (name, value) in pairs {
                let required = action_for_filter
                    .request_schema
                    .fields
                    .iter()
                    .find(|field| field.name == name)
                    .is_some_and(|field| field.required);
                if value.is_null() && !required {
                    continue;
                }
                params.insert(name, value);
            }
            Value::Object(params)
        })
        .boxed()
}

fn required_default_params(action: &ActionContract) -> Value {
    let mut params = serde_json::Map::new();
    for field in &action.request_schema.fields {
        if !field.required {
            continue;
        }
        let value = match field.kind {
            SchemaKind::String => Value::String("default".to_string()),
            SchemaKind::Integer => Value::Number(serde_json::Number::from(1_u32)),
            SchemaKind::Boolean => Value::Bool(false),
            SchemaKind::Array => Value::Array(Vec::new()),
            SchemaKind::Object => Value::Object(serde_json::Map::new()),
            SchemaKind::Null => Value::Null,
        };
        params.insert(field.name.clone(), value);
    }
    Value::Object(params)
}

#[test]
fn profile_family_differential_matches_ntm_mirror_for_1000_contract_requests() {
    let contract = profile_family_contract();
    let cases_per_action = PROFILE_DIFFERENTIAL_CASES / contract.actions.len();
    let remainder = PROFILE_DIFFERENTIAL_CASES % contract.actions.len();
    let ntm = ProfileNtmMirror;
    let mut runner = TestRunner::default();
    let mut compared = 0_usize;

    for (action_index, action) in contract.actions.iter().enumerate() {
        let action_name = action.action.clone();
        let native_action = action_name.clone();
        let harness = DifferentialHarness::new(
            "profile",
            action_name.as_str(),
            move |request| Ok(profile_response_envelope(&native_action, request)),
            &ntm,
        );

        let default_request = required_default_params(action);
        let default_report = harness
            .compare(&default_request)
            .unwrap_or_else(|err| panic!("profile.{action_name} default compare failed: {err}"));
        assert!(
            default_report.is_match(),
            "profile.{action_name} default request diverged: {default_report:?}"
        );
        compared += 1;

        let target_for_action = cases_per_action + usize::from(action_index < remainder);
        let generated_for_action = target_for_action.saturating_sub(1);
        let strategy = request_strategy(action);
        for case_index in 0..generated_for_action {
            let request = strategy
                .new_tree(&mut runner)
                .unwrap_or_else(|err| {
                    panic!("profile.{action_name} case {case_index} generation failed: {err}")
                })
                .current();
            let report = harness.compare(&request).unwrap_or_else(|err| {
                panic!("profile.{action_name} case {case_index} compare failed: {err}")
            });
            assert!(
                report.is_match(),
                "profile.{action_name} case {case_index} diverged for request {request}: {report:?}"
            );
            compared += 1;
        }
    }

    assert_eq!(compared, PROFILE_DIFFERENTIAL_CASES);
}
