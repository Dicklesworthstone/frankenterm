//! Property-based tests for storage pane metadata carriers.

use frankenterm_core::storage::PaneRecord;
use proptest::prelude::*;

fn arb_pane_record() -> impl Strategy<Value = PaneRecord> {
    (
        (
            0u64..10_000,
            prop::option::of("[0-9a-f]{32}"),
            "[a-z0-9_.-]{3,24}",
            prop::option::of(0u64..10_000),
            prop::option::of(0u64..10_000),
            prop::option::of("[A-Za-z0-9 _.:-]{0,40}"),
            prop::option::of("/[a-z]{1,8}(/[a-z]{1,8}){0,4}"),
        ),
        (
            prop::option::of("/dev/tty[a-z0-9]{0,6}"),
            0i64..9_999_999_999_999,
            0i64..9_999_999_999_999,
            any::<bool>(),
            prop::option::of("[A-Za-z0-9 _.:-]{3,40}"),
            prop::option::of(0i64..9_999_999_999_999),
        ),
    )
        .prop_map(
            |(
                (pane_id, pane_uuid, domain, window_id, tab_id, title, cwd),
                (tty_name, first_seen_at, last_seen_at, observed, ignore_reason, last_decision_at),
            )| PaneRecord {
                pane_id,
                pane_uuid,
                domain,
                window_id,
                tab_id,
                title,
                cwd,
                tty_name,
                first_seen_at,
                last_seen_at,
                observed,
                ignore_reason,
                last_decision_at,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_record_roundtrip(record in arb_pane_record()) {
        let json = serde_json::to_string(&record).unwrap();
        let back: PaneRecord = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(record.pane_id, back.pane_id);
        prop_assert_eq!(record.pane_uuid, back.pane_uuid);
        prop_assert_eq!(record.domain, back.domain);
        prop_assert_eq!(record.window_id, back.window_id);
        prop_assert_eq!(record.tab_id, back.tab_id);
        prop_assert_eq!(record.title, back.title);
        prop_assert_eq!(record.cwd, back.cwd);
        prop_assert_eq!(record.tty_name, back.tty_name);
        prop_assert_eq!(record.first_seen_at, back.first_seen_at);
        prop_assert_eq!(record.last_seen_at, back.last_seen_at);
        prop_assert_eq!(record.observed, back.observed);
        prop_assert_eq!(record.ignore_reason, back.ignore_reason);
        prop_assert_eq!(record.last_decision_at, back.last_decision_at);
    }

    #[test]
    fn pane_record_observation_fields_stay_coherent(record in arb_pane_record()) {
        if record.observed {
            prop_assert!(record.ignore_reason.is_none() || !record.ignore_reason.as_ref().unwrap().is_empty());
        } else {
            prop_assert!(record.ignore_reason.is_none() || !record.ignore_reason.as_ref().unwrap().is_empty());
        }
        if let Some(decision_at) = record.last_decision_at {
            prop_assert!(decision_at >= 0);
        }
    }

    #[test]
    fn pane_record_seen_window_can_roundtrip_json_shape(record in arb_pane_record()) {
        let value = serde_json::to_value(&record).unwrap();
        let obj = value.as_object().unwrap();

        prop_assert_eq!(obj.get("pane_id").unwrap().as_u64(), Some(record.pane_id));
        prop_assert_eq!(obj.get("domain").unwrap().as_str(), Some(record.domain.as_str()));
        prop_assert_eq!(obj.get("observed").unwrap().as_bool(), Some(record.observed));
        prop_assert_eq!(obj.get("first_seen_at").unwrap().as_i64(), Some(record.first_seen_at));
        prop_assert_eq!(obj.get("last_seen_at").unwrap().as_i64(), Some(record.last_seen_at));
    }
}
