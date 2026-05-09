use std::path::PathBuf;

use frankenterm_core::scrollback_mmap_format::{
    FormatVersion, HeaderDecodeError, HeaderFlags, ScrollbackHeader,
};
use frankenterm_core::scrollback_mmap_recovery::{
    OrphanCandidate, OrphanPickerBadge, OrphanPickerKey, OrphanPickerOutcome, OrphanPickerState,
    OrphanState, RecoveryAction, RecoveryDecision,
};
use proptest::prelude::*;

fn action_strategy() -> impl Strategy<Value = RecoveryAction> {
    prop_oneof![Just(RecoveryAction::Recover), Just(RecoveryAction::Discard),]
}

fn decode_error_strategy() -> impl Strategy<Value = HeaderDecodeError> {
    prop_oneof![
        (0_usize..256, 0_usize..256)
            .prop_filter(
                "actual is shorter than expected",
                |(expected, actual)| actual < expected
            )
            .prop_map(|(expected, actual)| HeaderDecodeError::Truncated { expected, actual }),
        any::<[u8; 4]>().prop_map(|observed| HeaderDecodeError::BadMagic { observed }),
        any::<u16>().prop_map(|observed| HeaderDecodeError::UnknownVersion { observed }),
        (0_u64..1_000_000, 0_u64..1_000_000)
            .prop_filter("cursor exceeds capacity", |(cursor, capacity)| cursor
                > capacity)
            .prop_map(
                |(cursor, capacity)| HeaderDecodeError::CursorBeyondCapacity { cursor, capacity }
            ),
    ]
}

fn path_for(label: impl AsRef<str>) -> PathBuf {
    PathBuf::from(format!("/tmp/{}.bin", label.as_ref()))
}

fn header(
    uuid_byte: u8,
    created_at_epoch_ms: u64,
    last_msync_at_epoch_ms: u64,
    bytes: u64,
) -> ScrollbackHeader {
    ScrollbackHeader {
        version: FormatVersion::V1,
        flags: HeaderFlags::empty(),
        capacity_bytes: 4096,
        write_cursor_bytes: 0,
        pane_uuid: [uuid_byte; 32],
        created_at_epoch_ms,
        last_msync_at_epoch_ms,
        redactions_applied: 0,
        total_bytes_written: bytes,
    }
}

fn orphan_candidate(
    label: impl AsRef<str>,
    uuid_byte: u8,
    created: u64,
    last_msync: u64,
    bytes: u64,
) -> OrphanCandidate {
    OrphanCandidate {
        path: path_for(label),
        state: OrphanState::Orphaned,
        header: Some(Ok(header(uuid_byte, created, last_msync, bytes))),
    }
}

fn locked_candidate(
    label: impl AsRef<str>,
    uuid_byte: u8,
    created: u64,
    last_msync: u64,
    bytes: u64,
) -> OrphanCandidate {
    OrphanCandidate {
        path: path_for(label),
        state: OrphanState::Locked,
        header: Some(Ok(header(uuid_byte, created, last_msync, bytes))),
    }
}

fn corrupt_candidate(label: impl AsRef<str>, err: HeaderDecodeError) -> OrphanCandidate {
    OrphanCandidate {
        path: path_for(label),
        state: OrphanState::Corrupt,
        header: Some(Err(err)),
    }
}

fn wrong_shape_candidate(label: impl AsRef<str>) -> OrphanCandidate {
    OrphanCandidate {
        path: PathBuf::from(format!("/tmp/{}.txt", label.as_ref())),
        state: OrphanState::WrongShape,
        header: None,
    }
}

fn expected_uuid_short(uuid_byte: u8) -> String {
    format!("{uuid_byte:02x}").repeat(8)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_scrollback_recovery_candidate_helpers_are_state_sensitive(
        uuid_byte in any::<u8>(),
        created in any::<u64>(),
        last_msync in any::<u64>(),
        bytes in any::<u64>(),
        err in decode_error_strategy(),
    ) {
        let orphan = orphan_candidate("orphan", uuid_byte, created, last_msync, bytes);
        let locked = locked_candidate("locked", uuid_byte, created, last_msync, bytes);
        let corrupt = corrupt_candidate("corrupt", err.clone());
        let wrong_shape = wrong_shape_candidate("wrong_shape");

        prop_assert_eq!(orphan.header_ok(), Some(&header(uuid_byte, created, last_msync, bytes)));
        prop_assert_eq!(locked.header_ok(), Some(&header(uuid_byte, created, last_msync, bytes)));
        prop_assert_eq!(orphan.corrupt_reason(), None);
        prop_assert_eq!(locked.corrupt_reason(), None);
        prop_assert_eq!(corrupt.header_ok(), None);
        prop_assert_eq!(corrupt.corrupt_reason(), Some(&err));
        prop_assert_eq!(wrong_shape.header_ok(), None);
        prop_assert_eq!(wrong_shape.corrupt_reason(), None);

        prop_assert_eq!(OrphanPickerBadge::Orphaned.as_str(), "orphaned");
        prop_assert_eq!(OrphanPickerBadge::Locked.as_str(), "locked");
        prop_assert_eq!(OrphanPickerBadge::Corrupt.as_str(), "corrupt");
    }

    #[test]
    fn proptest_scrollback_recovery_picker_filters_wrong_shape_and_marks_selectability(
        action in action_strategy(),
        now in any::<u64>(),
        created in any::<u64>(),
        last_msync in any::<u64>(),
        bytes in any::<u64>(),
        uuid_byte in any::<u8>(),
        err in decode_error_strategy(),
    ) {
        let candidates = vec![
            orphan_candidate("orphan", uuid_byte, created, last_msync, bytes),
            wrong_shape_candidate("ignored"),
            locked_candidate("locked", uuid_byte, created, last_msync, bytes),
            corrupt_candidate("corrupt-file", err),
        ];
        let state = OrphanPickerState::new(&candidates, action, now);
        let rows = state.rows();

        prop_assert_eq!(state.action(), action);
        prop_assert_eq!(state.highlighted(), Some(0));
        prop_assert_eq!(state.highlighted_row(), rows.first());
        prop_assert_eq!(rows.len(), 3);
        prop_assert!(!rows.iter().any(|row| row.path.ends_with("ignored.txt")));

        prop_assert_eq!(rows[0].badge, OrphanPickerBadge::Orphaned);
        prop_assert!(rows[0].selectable);
        prop_assert!(!rows[0].selected);
        let expected_short_uuid = expected_uuid_short(uuid_byte);
        prop_assert_eq!(&rows[0].pane_uuid_short, &expected_short_uuid);
        prop_assert_eq!(rows[0].created_at_epoch_ms, Some(created));
        prop_assert_eq!(rows[0].bytes_written, Some(bytes));
        prop_assert_eq!(rows[0].last_msync_age_ms, now.checked_sub(last_msync));
        prop_assert!(rows[0].accessibility_label.contains("orphaned"));

        prop_assert_eq!(rows[1].badge, OrphanPickerBadge::Locked);
        prop_assert!(!rows[1].selectable);
        prop_assert!(rows[1].accessibility_label.contains("locked"));
        prop_assert_eq!(&rows[1].pane_uuid_short, &expected_short_uuid);

        prop_assert_eq!(rows[2].badge, OrphanPickerBadge::Corrupt);
        prop_assert_eq!(rows[2].selectable, action == RecoveryAction::Discard);
        prop_assert!(rows[2].corrupt_reason.is_some());
        prop_assert!(rows[2].accessibility_label.contains("corrupt"));
    }

    #[test]
    fn proptest_scrollback_recovery_picker_navigation_saturates(
        row_count in 1_usize..=12,
        down_steps in 0_usize..=24,
        up_steps in 0_usize..=24,
    ) {
        let candidates: Vec<_> = (0..row_count)
            .map(|idx| orphan_candidate(format!("row_{idx}"), idx as u8, idx as u64, idx as u64, idx as u64))
            .collect();
        let mut state = OrphanPickerState::new(&candidates, RecoveryAction::Recover, 100);

        for _ in 0..down_steps {
            state.move_down();
        }
        let after_down = down_steps.min(row_count - 1);
        prop_assert_eq!(state.highlighted(), Some(after_down));

        for _ in 0..up_steps {
            state.move_up();
        }
        prop_assert_eq!(state.highlighted(), Some(after_down.saturating_sub(up_steps)));

        let mut empty = OrphanPickerState::new(&[wrong_shape_candidate("hidden")], RecoveryAction::Recover, 100);
        prop_assert!(empty.rows().is_empty());
        prop_assert_eq!(empty.highlighted(), None);
        empty.move_down();
        empty.move_up();
        empty.toggle_highlighted();
        prop_assert_eq!(empty.highlighted(), None);
        prop_assert_eq!(empty.confirm(), Vec::<RecoveryDecision>::new());
    }

    #[test]
    fn proptest_scrollback_recovery_toggle_and_confirm_emit_one_decision_per_displayed_row(
        action in action_strategy(),
        selections in prop::collection::vec(any::<bool>(), 1..=12),
    ) {
        let candidates: Vec<_> = selections
            .iter()
            .enumerate()
            .map(|(idx, _)| orphan_candidate(format!("row_{idx}"), idx as u8, 1, 2, 3))
            .collect();
        let mut state = OrphanPickerState::new(&candidates, action, 10);

        for should_select in &selections {
            if *should_select {
                state.toggle_highlighted();
            }
            state.move_down();
        }

        let decisions = state.confirm();
        prop_assert_eq!(decisions.len(), selections.len());
        for (idx, should_select) in selections.iter().enumerate() {
            let path = path_for(format!("row_{idx}"));
            let expected = if *should_select {
                match action {
                    RecoveryAction::Recover => RecoveryDecision::Recover(path),
                    RecoveryAction::Discard => RecoveryDecision::Discard(path),
                }
            } else {
                RecoveryDecision::Skip(path)
            };
            prop_assert_eq!(&decisions[idx], &expected);
        }
    }

    #[test]
    fn proptest_scrollback_recovery_handle_key_maps_to_pending_confirm_and_cancel(
        action in action_strategy(),
        uuid_byte in any::<u8>(),
        err in decode_error_strategy(),
    ) {
        let candidates = vec![
            orphan_candidate("orphan", uuid_byte, 1, 2, 3),
            locked_candidate("locked", uuid_byte, 4, 5, 6),
            corrupt_candidate("corrupt", err),
        ];
        let mut state = OrphanPickerState::new(&candidates, action, 10);

        prop_assert_eq!(state.handle_key(OrphanPickerKey::Down), OrphanPickerOutcome::Pending);
        prop_assert_eq!(state.highlighted(), Some(1));
        prop_assert_eq!(state.handle_key(OrphanPickerKey::Toggle), OrphanPickerOutcome::Pending);
        prop_assert!(!state.rows()[1].selected);
        prop_assert_eq!(state.handle_key(OrphanPickerKey::Up), OrphanPickerOutcome::Pending);
        prop_assert_eq!(state.highlighted(), Some(0));
        prop_assert_eq!(state.handle_key(OrphanPickerKey::Toggle), OrphanPickerOutcome::Pending);

        let confirmed = state.handle_key(OrphanPickerKey::Confirm);
        prop_assert_eq!(confirmed, OrphanPickerOutcome::Confirmed(state.confirm()));

        let mut cancelled = OrphanPickerState::new(&candidates, action, 10);
        prop_assert_eq!(cancelled.handle_key(OrphanPickerKey::Cancel), OrphanPickerOutcome::Cancelled);
    }
}
