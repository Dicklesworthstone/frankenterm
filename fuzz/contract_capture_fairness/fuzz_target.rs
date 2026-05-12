#![no_main]

#[path = "../contract_fuzz_common.rs"]
mod contract_fuzz_common;

use arbitrary::Arbitrary;
use contract_fuzz_common::{
    MAX_ITEMS, assert_no_raw_content_flags, assert_schema_valid, bounded_ms, compile_schema,
    roundtrip_value,
};
use frankenterm_core::config::CaptureBudgetConfig;
use frankenterm_core::tailer::CaptureScheduler;
use jsonschema::Validator;
use libfuzzer_sys::fuzz_target;
use serde_json::{Value, json};
use std::sync::OnceLock;

static SCHEMA: OnceLock<Validator> = OnceLock::new();

fn schema() -> &'static Validator {
    SCHEMA.get_or_init(|| {
        compile_schema(include_bytes!(
            "../../docs/json-schema/ft-capture-fairness.json"
        ))
    })
}

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    generated_at_ms: u64,
    max_captures_per_sec: u16,
    max_bytes_per_sec: u32,
    available_permits: u8,
    panes: Vec<RawPane>,
    record_bytes: Vec<u32>,
}

#[derive(Arbitrary, Debug)]
struct RawPane {
    pane_id: u64,
    priority: u32,
}

impl FuzzInput {
    fn into_report(self) -> Value {
        let mut ready_panes = self
            .panes
            .into_iter()
            .take(MAX_ITEMS)
            .map(|pane| (pane.pane_id % 1_000_000, pane.priority % 101))
            .collect::<Vec<_>>();
        if ready_panes.is_empty() {
            ready_panes.push((0, 50));
        }
        ready_panes.sort_by_key(|(pane_id, priority)| (*priority, *pane_id));

        let budget = CaptureBudgetConfig {
            max_captures_per_sec: u32::from(self.max_captures_per_sec % 512),
            max_bytes_per_sec: u64::from(self.max_bytes_per_sec),
        };
        let mut scheduler = CaptureScheduler::new(budget.clone());
        let available_permits = usize::from((self.available_permits % 32).max(1));
        let selected = scheduler.select_panes(&ready_panes, available_permits);
        for (pane_id, bytes) in selected.iter().zip(self.record_bytes.into_iter()) {
            scheduler.record_capture(*pane_id, u64::from(bytes));
        }
        let snapshot = scheduler.snapshot();
        let selected_value = selected
            .iter()
            .map(|pane_id| json!(pane_id))
            .collect::<Vec<_>>();
        let selected_within_permits = selected.len() <= available_permits;
        let selected_within_ready = selected.iter().all(|pane_id| {
            ready_panes
                .iter()
                .any(|(ready_pane_id, _)| ready_pane_id == pane_id)
        });

        json!({
            "schema_version": 1,
            "contract_id": "ft.capture_fairness.v1",
            "generated_at_ms": bounded_ms(self.generated_at_ms),
            "source": "cargo_fuzz.contract_capture_fairness",
            "budget": {
                "max_captures_per_sec": budget.max_captures_per_sec,
                "max_bytes_per_sec": budget.max_bytes_per_sec
            },
            "ready_panes_total": ready_panes.len(),
            "available_permits": available_permits,
            "selected_panes": selected_value,
            "scheduler_snapshot": roundtrip_value(&snapshot),
            "pass_fail": {
                "selected_within_permits": selected_within_permits,
                "selected_within_ready_set": selected_within_ready,
                "no_raw_content": true
            },
            "redaction_policy": {
                "raw_pane_content_allowed": false,
                "bounded_counters_only": true,
                "secret_redaction_required": true
            },
            "raw_pane_content_stored": false,
            "artifact_paths": []
        })
    }
}

fuzz_target!(|input: FuzzInput| {
    let value = input.into_report();
    assert_schema_valid(schema(), &value);
    assert_no_raw_content_flags(&value);
    assert_eq!(
        value["pass_fail"]["selected_within_permits"],
        Value::Bool(true),
        "capture scheduler selected more panes than available permits"
    );
    assert_eq!(
        value["pass_fail"]["selected_within_ready_set"],
        Value::Bool(true),
        "capture scheduler selected a pane outside the ready set"
    );
});
