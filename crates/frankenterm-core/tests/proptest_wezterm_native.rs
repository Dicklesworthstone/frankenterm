//! Property-based tests for `wezterm_native` carrier types and trait contract.

use frankenterm_core::wezterm_native::{WaEventSink, WaPaneState};
use proptest::prelude::*;
use std::sync::Mutex;

fn arb_pane_state() -> impl Strategy<Value = WaPaneState> {
    (
        "[A-Za-z0-9 _.-]{0,40}",
        1u16..=500,
        1u16..=500,
        any::<bool>(),
        0u32..=10_000,
        0u32..=10_000,
    )
        .prop_map(
            |(title, rows, cols, is_alt_screen, cursor_row, cursor_col)| WaPaneState {
                title,
                rows,
                cols,
                is_alt_screen,
                cursor_row,
                cursor_col,
            },
        )
}

#[derive(Default)]
struct RecordingSink {
    events: Mutex<Vec<String>>,
}

impl WaEventSink for RecordingSink {
    fn on_pane_output(&self, pane_id: u64, data: &[u8]) {
        self.events
            .lock()
            .unwrap()
            .push(format!("output:{pane_id}:{}", data.len()));
    }

    fn on_pane_state_change(&self, pane_id: u64, state: &WaPaneState) {
        self.events
            .lock()
            .unwrap()
            .push(format!("state:{pane_id}:{}x{}", state.rows, state.cols));
    }

    fn on_user_var_changed(&self, pane_id: u64, name: &str, value: &str) {
        self.events
            .lock()
            .unwrap()
            .push(format!("var:{pane_id}:{name}={value}"));
    }

    fn on_pane_created(&self, pane_id: u64, domain: &str, cwd: Option<&str>) {
        self.events.lock().unwrap().push(format!(
            "created:{pane_id}:{domain}:{}",
            cwd.unwrap_or("")
        ));
    }

    fn on_pane_destroyed(&self, pane_id: u64) {
        self.events.lock().unwrap().push(format!("destroyed:{pane_id}"));
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn pane_state_clone_roundtrip(state in arb_pane_state()) {
        let cloned = state.clone();
        prop_assert_eq!(cloned, state);
    }

    #[test]
    fn pane_state_debug_contains_core_fields(state in arb_pane_state()) {
        let debug = format!("{:?}", state);
        prop_assert!(debug.contains("WaPaneState"));
        prop_assert!(debug.contains(&state.title));
        prop_assert!(debug.contains(&state.rows.to_string()));
        prop_assert!(debug.contains(&state.cols.to_string()));
    }

    #[test]
    fn recording_sink_accepts_callback_sequence(
        pane_id in 0u64..=10_000,
        state in arb_pane_state(),
        domain in "[a-z0-9_-]{1,20}",
        cwd in prop::option::of("/[a-z0-9/_-]{1,40}"),
        var_name in "[A-Z_]{1,12}",
        var_value in "[A-Za-z0-9_-]{0,20}",
        output in prop::collection::vec(any::<u8>(), 0..64),
    ) {
        let sink = RecordingSink::default();
        sink.on_pane_created(pane_id, &domain, cwd.as_deref());
        sink.on_pane_output(pane_id, &output);
        sink.on_pane_state_change(pane_id, &state);
        sink.on_user_var_changed(pane_id, &var_name, &var_value);
        sink.on_pane_destroyed(pane_id);

        let events = sink.events.lock().unwrap();
        let created_prefix = format!("created:{}:", pane_id);
        prop_assert_eq!(events.len(), 5);
        prop_assert!(events[0].starts_with(&created_prefix));
        prop_assert_eq!(&events[1], &format!("output:{pane_id}:{}", output.len()));
        prop_assert_eq!(&events[2], &format!("state:{pane_id}:{}x{}", state.rows, state.cols));
        prop_assert_eq!(&events[3], &format!("var:{pane_id}:{var_name}={var_value}"));
        prop_assert_eq!(&events[4], &format!("destroyed:{pane_id}"));
    }
}
