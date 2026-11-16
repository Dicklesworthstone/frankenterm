#![no_main]

#[path = "../contract_fuzz_common.rs"]
mod contract_fuzz_common;

use arbitrary::Arbitrary;
use contract_fuzz_common::{
    MAX_ITEMS, assert_no_raw_content_flags, assert_schema_valid, bounded_ms, compile_schema,
    limited_text, roundtrip_value, stable_fragment,
};
use frankenterm_core::blocker_radar::{
    blocker_radar_input_from_coordination_snapshot, build_blocker_radar_report,
};
use jsonschema::Validator;
use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};
use std::sync::OnceLock;

static SCHEMA: OnceLock<Validator> = OnceLock::new();

fn schema() -> &'static Validator {
    SCHEMA.get_or_init(|| {
        compile_schema(include_bytes!(
            "../../docs/json-schema/ft-blocker-radar.json"
        ))
    })
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    generated_at_ms: u64,
    source: String,
    mail_unavailable: bool,
    ready: Vec<RawBead>,
    active_agents: Vec<RawActiveAgent>,
    stale_candidates: Vec<RawStaleCandidate>,
    dirty_paths: Vec<RawDirtyPath>,
}

#[derive(Arbitrary, Debug)]
struct RawBead {
    id: String,
}

#[derive(Arbitrary, Debug)]
struct RawActiveAgent {
    assignee: String,
    beads: Vec<RawAgentBead>,
}

#[derive(Arbitrary, Debug)]
struct RawAgentBead {
    id: String,
    stale_over_2h: bool,
}

#[derive(Arbitrary, Debug)]
struct RawStaleCandidate {
    id: String,
    assignee: String,
    age_seconds: u32,
    reason: String,
}

#[derive(Arbitrary, Debug)]
struct RawDirtyPath {
    path: String,
    status: String,
    category: String,
    raw: String,
}

impl RawBead {
    fn into_json(self) -> Value {
        json!({ "id": stable_fragment(self.id, "ft-fuzz") })
    }
}

impl RawAgentBead {
    fn into_json(self) -> Value {
        json!({
            "id": stable_fragment(self.id, "ft-active"),
            "stale_over_2h": self.stale_over_2h
        })
    }
}

impl RawActiveAgent {
    fn into_json(self) -> Value {
        json!({
            "assignee": stable_fragment(self.assignee, "FuzzAgent"),
            "beads": self
                .beads
                .into_iter()
                .take(MAX_ITEMS)
                .map(RawAgentBead::into_json)
                .collect::<Vec<_>>()
        })
    }
}

impl RawStaleCandidate {
    fn into_json(self) -> Value {
        json!({
            "id": stable_fragment(self.id, "ft-stale"),
            "assignee": stable_fragment(self.assignee, "FuzzAgent"),
            "age_seconds": self.age_seconds,
            "reason": stable_fragment(self.reason, "beads.status_check")
        })
    }
}

impl RawDirtyPath {
    fn into_json(self) -> Value {
        json!({
            "path": limited_text(self.path, "crates/frankenterm-core/src/lib.rs"),
            "status": stable_fragment(self.status, "M"),
            "category": stable_fragment(self.category, "dirty_tree"),
            "raw": stable_fragment(self.raw, "M crates/frankenterm-core/src/lib.rs")
        })
    }
}

impl FuzzInput {
    fn into_snapshot(self) -> (u64, String, Value) {
        let generated_at_ms = bounded_ms(self.generated_at_ms);
        let ready = self
            .ready
            .into_iter()
            .take(MAX_ITEMS)
            .map(RawBead::into_json)
            .collect::<Vec<_>>();
        let active_agents = self
            .active_agents
            .into_iter()
            .take(MAX_ITEMS)
            .map(RawActiveAgent::into_json)
            .collect::<Vec<_>>();
        let stale_candidates = self
            .stale_candidates
            .into_iter()
            .take(MAX_ITEMS)
            .map(RawStaleCandidate::into_json)
            .collect::<Vec<_>>();
        let dirty_paths = self
            .dirty_paths
            .into_iter()
            .take(MAX_ITEMS)
            .map(RawDirtyPath::into_json)
            .collect::<Vec<_>>();

        let snapshot = json!({
            "agent_mail": {
                "status": if self.mail_unavailable { "unavailable" } else { "available" },
                "marker": "fuzzed coordination snapshot"
            },
            "beads": {
                "ready": ready,
                "active_agents": active_agents,
                "stale_reopen": {
                    "candidates": stale_candidates
                }
            },
            "git": {
                "dirty_paths": dirty_paths
            }
        });
        (
            generated_at_ms,
            stable_fragment(self.source, "cargo_fuzz.contract_blocker_radar"),
            snapshot,
        )
    }
}

fuzz_target!(|input: FuzzInput| {
    let (generated_at_ms, source, snapshot) = input.into_snapshot();
    let radar_input =
        blocker_radar_input_from_coordination_snapshot(&snapshot, generated_at_ms, source);
    let report = build_blocker_radar_report(&radar_input);
    let value = roundtrip_value(&report);
    assert_schema_valid(schema(), &value);
    assert_no_raw_content_flags(&value);
});
