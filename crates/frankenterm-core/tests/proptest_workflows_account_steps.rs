// Property-based tests for workflows/account_steps public result and error carriers.

use frankenterm_core::accounts::{
    AccountQuotaAdvisory, AccountRecord, CandidateAccount, FilteredAccount, QuotaAvailability,
    SelectionExplanation,
};
use frankenterm_core::caut::CautError;
use frankenterm_core::workflows::{
    AccountSelectionStepError, AccountSelectionStepResult, DeviceCodeParseError,
};
use proptest::prelude::*;

fn arb_account_record() -> impl Strategy<Value = AccountRecord> {
    (
        (
            0i64..10_000,
            "[a-z0-9_-]{3,20}",
            "[a-z_]{3,12}",
            prop::option::of("[a-z ]{3,20}"),
            0.0f64..100.0,
            prop::option::of("[0-9T: -]{8,24}"),
            prop::option::of(0i64..1_000_000),
        ),
        (
            prop::option::of(0i64..1_000_000),
            prop::option::of(0i64..1_000_000),
            0i64..9_999_999_999_999,
            prop::option::of(0i64..9_999_999_999_999),
            0i64..9_999_999_999_999,
            0i64..9_999_999_999_999,
        ),
    )
        .prop_map(
            |(
                (
                    id,
                    account_id,
                    service,
                    name,
                    percent_remaining,
                    reset_at,
                    tokens_used,
                ),
                (
                    tokens_remaining,
                    tokens_limit,
                    last_refreshed_at,
                    last_used_at,
                    created_at,
                    updated_at,
                ),
            )| AccountRecord {
                id,
                account_id,
                service,
                name,
                percent_remaining,
                reset_at,
                tokens_used,
                tokens_remaining,
                tokens_limit,
                last_refreshed_at,
                last_used_at,
                created_at,
                updated_at,
            },
        )
}

fn arb_selection_explanation() -> impl Strategy<Value = SelectionExplanation> {
    (
        0usize..12,
        prop::collection::vec(
            (
                "[a-z0-9_-]{3,20}",
                prop::option::of("[a-z ]{3,20}"),
                0.0f64..100.0,
                "[A-Za-z0-9 %.()_-]{3,40}",
            )
                .prop_map(|(account_id, name, percent_remaining, reason)| FilteredAccount {
                    account_id,
                    name,
                    percent_remaining,
                    reason,
                }),
            0..4,
        ),
        prop::collection::vec(
            (
                "[a-z0-9_-]{3,20}",
                prop::option::of("[a-z ]{3,20}"),
                0.0f64..100.0,
                prop::option::of(0i64..9_999_999_999_999),
            )
                .prop_map(|(account_id, name, percent_remaining, last_used_at)| CandidateAccount {
                    account_id,
                    name,
                    percent_remaining,
                    last_used_at,
                }),
            0..4,
        ),
        "[A-Za-z0-9 ,.()_%:-]{3,80}",
    )
        .prop_map(
            |(total_considered, filtered_out, candidates, selection_reason)| SelectionExplanation {
                total_considered,
                filtered_out,
                candidates,
                selection_reason,
            },
        )
}

fn arb_quota_advisory() -> impl Strategy<Value = AccountQuotaAdvisory> {
    prop_oneof![
        (0.0f64..100.0, prop::option::of(0.0f64..100.0), prop::option::of("[A-Za-z0-9 ,.()_%:-]{3,60}"))
            .prop_map(|(low_quota_threshold_percent, selected_percent_remaining, warning)| AccountQuotaAdvisory {
                availability: QuotaAvailability::Available,
                low_quota_threshold_percent,
                selected_percent_remaining,
                warning,
            }),
        (0.0f64..100.0, prop::option::of(0.0f64..100.0), prop::option::of("[A-Za-z0-9 ,.()_%:-]{3,60}"))
            .prop_map(|(low_quota_threshold_percent, selected_percent_remaining, warning)| AccountQuotaAdvisory {
                availability: QuotaAvailability::Low,
                low_quota_threshold_percent,
                selected_percent_remaining,
                warning,
            }),
        (0.0f64..100.0, Just(None::<f64>), prop::option::of("[A-Za-z0-9 ,.()_%:-]{3,60}"))
            .prop_map(|(low_quota_threshold_percent, selected_percent_remaining, warning)| AccountQuotaAdvisory {
                availability: QuotaAvailability::Exhausted,
                low_quota_threshold_percent,
                selected_percent_remaining,
                warning,
            }),
    ]
}

fn arb_account_selection_step_result() -> impl Strategy<Value = AccountSelectionStepResult> {
    (
        prop::option::of(arb_account_record()),
        arb_selection_explanation(),
        arb_quota_advisory(),
        0usize..32,
    )
        .prop_map(
            |(selected, explanation, quota_advisory, accounts_refreshed)| AccountSelectionStepResult {
                selected,
                explanation,
                quota_advisory,
                accounts_refreshed,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn account_selection_step_result_json_preserves_shape(result in arb_account_selection_step_result()) {
        let json = serde_json::to_value(&result).unwrap();
        let obj = json.as_object().unwrap();

        prop_assert_eq!(obj.get("accounts_refreshed").unwrap().as_u64(), Some(result.accounts_refreshed as u64));

        let selected = obj.get("selected").unwrap();
        if let Some(account) = &result.selected {
            prop_assert!(selected.is_object());
            prop_assert_eq!(selected.get("account_id").and_then(serde_json::Value::as_str), Some(account.account_id.as_str()));
        } else {
            prop_assert!(selected.is_null());
        }

        let advisory = obj.get("quota_advisory").unwrap();
        let availability = advisory.get("availability").and_then(serde_json::Value::as_str).unwrap();
        match result.quota_advisory.availability {
            QuotaAvailability::Available => prop_assert_eq!(availability, "available"),
            QuotaAvailability::Low => prop_assert_eq!(availability, "low"),
            QuotaAvailability::Exhausted => prop_assert_eq!(availability, "exhausted"),
        }
    }

    #[test]
    fn account_selection_step_error_display_preserves_context(
        storage_message in "[A-Za-z0-9 ,.()_:-]{3,40}",
        io_message in "[A-Za-z0-9 ,.()_:-]{3,40}",
    ) {
        let storage = AccountSelectionStepError::Storage(storage_message.clone());
        let storage_display = storage.to_string();
        prop_assert!(storage_display.contains("storage error"));
        prop_assert!(storage_display.contains(&storage_message));

        let caut = AccountSelectionStepError::Caut(CautError::Io { message: io_message.clone() });
        let caut_display = caut.to_string();
        prop_assert!(caut_display.contains("caut error"));
        prop_assert!(caut_display.contains(&io_message));
    }

    #[test]
    fn device_code_parse_error_display_keeps_expected_and_hash(
        expected in "[a-z_ ]{3,30}",
        tail_hash in any::<u64>(),
        tail_len in 0usize..8192,
    ) {
        let err = DeviceCodeParseError {
            expected: Box::leak(expected.into_boxed_str()),
            tail_hash,
            tail_len,
        };
        let display = err.to_string();
        let tail_hash_hex = format!("{tail_hash:016x}");
        let tail_len_marker = format!("tail_len={tail_len}");

        prop_assert!(display.contains("Device code not found"));
        prop_assert!(display.contains(err.expected));
        prop_assert!(display.contains(&tail_hash_hex));
        prop_assert!(display.contains(&tail_len_marker));
    }
}
