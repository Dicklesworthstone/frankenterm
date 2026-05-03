use frankenterm_core::osc_protocol_integration::{
    CellCoord, CursorShapeSlug, Decoded, HyperlinkInteraction, HyperlinkSpan,
    Osc22PerPaneCursorMap, Osc52PolicyGated, Osc52PolicySlug, Osc52ReadResponse,
    OscIntegrationHealth, dispatch_click, sanitize_osc52_targets,
};
use proptest::prelude::*;
use std::collections::BTreeMap;

fn target_chars() -> impl Strategy<Value = Vec<char>> {
    prop::collection::vec(
        prop::sample::select(vec![
            'c', 'p', 's', '0', '1', '2', '3', '4', '5', '6', '7', ';', '\\', '\u{1b}', '\u{7}',
            'x', 'C', 'P', '\u{2603}',
        ]),
        0..=96,
    )
}

fn cursor_shape_strategy() -> impl Strategy<Value = CursorShapeSlug> {
    prop::sample::select(vec![
        CursorShapeSlug::Default,
        CursorShapeSlug::BlockBlinking,
        CursorShapeSlug::BlockSteady,
        CursorShapeSlug::UnderlineBlinking,
        CursorShapeSlug::UnderlineSteady,
        CursorShapeSlug::BarBlinking,
        CursorShapeSlug::BarSteady,
    ])
}

fn policy_strategy() -> impl Strategy<Value = Osc52PolicySlug> {
    prop::sample::select(vec![
        Osc52PolicySlug::Allow,
        Osc52PolicySlug::Prompt,
        Osc52PolicySlug::Deny,
    ])
}

fn expected_safe_targets(raw: &str) -> String {
    let filtered: String = raw
        .chars()
        .filter(|ch| matches!(ch, 'c' | 'p' | 's' | '0'..='7'))
        .collect();
    if filtered.is_empty() {
        "c".to_string()
    } else {
        filtered
    }
}

fn osc52_envelope(targets: &str, payload: &[u8]) -> Vec<u8> {
    let mut expected = Vec::from(&b"\x1b]52;"[..]);
    expected.extend_from_slice(targets.as_bytes());
    expected.push(b';');
    expected.extend_from_slice(payload);
    expected.extend_from_slice(&b"\x1b\\"[..]);
    expected
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_osc_protocol_integration_sanitize_targets_filters_to_protocol_alphabet(chars in target_chars()) {
        let raw: String = chars.into_iter().collect();
        let sanitized = sanitize_osc52_targets(&raw);

        let expected_targets = expected_safe_targets(&raw);
        prop_assert_eq!(&sanitized, &expected_targets);
        prop_assert!(!sanitized.is_empty());
        prop_assert!(sanitized.chars().all(|ch| matches!(ch, 'c' | 'p' | 's' | '0'..='7')));
        prop_assert!(!sanitized.contains(';'));
        let contains_esc = sanitized.contains('\u{1b}');
        let contains_bel = sanitized.contains('\u{7}');
        prop_assert!(!contains_esc);
        prop_assert!(!contains_bel);
    }

    #[test]
    fn proptest_osc_protocol_integration_hyperlink_spans_are_start_inclusive_end_exclusive(
        line in 0_u32..=64,
        start_col in 0_u32..=512,
        len in 1_u32..=256,
        probe_col in 0_u32..=900,
        selection_modifier_held in any::<bool>(),
        uri in "[A-Za-z0-9_./?&=%:+-]{0,80}",
        id in any::<u32>(),
    ) {
        let end_col = start_col.saturating_add(len);
        let span = HyperlinkSpan {
            id,
            start: CellCoord { line, col: start_col },
            end_exclusive: CellCoord { line, col: end_col },
            uri: uri.clone(),
        };
        let probe = CellCoord { line, col: probe_col };

        prop_assert_eq!(span.contains(probe), probe_col >= start_col && probe_col < end_col);
        prop_assert_eq!(span.intra_line_cell_count(), Some(len));
        prop_assert!(!span.is_multi_line());

        let click = dispatch_click(Some(&span), selection_modifier_held);
        match (selection_modifier_held, click) {
            (true, HyperlinkInteraction::SelectInstead { id: selected_id }) => {
                prop_assert_eq!(selected_id, id);
            }
            (false, HyperlinkInteraction::OpenUrl { id: opened_id, uri: opened_uri }) => {
                prop_assert_eq!(opened_id, id);
                prop_assert_eq!(opened_uri, uri);
            }
            _ => prop_assert!(false, "unexpected hyperlink click dispatch result"),
        }
        prop_assert_eq!(dispatch_click(None, selection_modifier_held), HyperlinkInteraction::NotOverHyperlink);
    }

    #[test]
    fn proptest_osc_protocol_integration_cursor_map_matches_btreemap_model(
        ops in prop::collection::vec((0_u64..=16, cursor_shape_strategy()), 0..=128),
        lookup_pane in 0_u64..=16,
    ) {
        let mut map = Osc22PerPaneCursorMap::new();
        let mut model = BTreeMap::new();
        let mut changes_total = 0_u64;

        for (pane_id, shape) in ops {
            let expected_prior = model.insert(pane_id, shape);
            let actual_prior = map.set(pane_id, shape);
            if expected_prior != Some(shape) {
                changes_total += 1;
            }

            prop_assert_eq!(actual_prior, expected_prior);
            prop_assert_eq!(map.get(pane_id), shape);
            prop_assert_eq!(map.changes_total, changes_total);
        }

        prop_assert_eq!(
            map.get(lookup_pane),
            model.get(&lookup_pane).copied().unwrap_or(CursorShapeSlug::Default)
        );
        let forgotten = map.forget(lookup_pane);
        prop_assert_eq!(forgotten, model.remove(&lookup_pane));
        prop_assert_eq!(map.get(lookup_pane), CursorShapeSlug::Default);
    }

    #[test]
    fn proptest_osc_protocol_integration_osc52_policy_gate_emits_expected_wire_shape(
        payload in prop::collection::vec(any::<u8>(), 0..=96),
        targets in target_chars().prop_map(|chars| chars.into_iter().collect::<String>()),
        policy in policy_strategy(),
    ) {
        let expected_targets = expected_safe_targets(&targets);
        let gated = Osc52ReadResponse::<Decoded>::from_clipboard(payload.clone()).policy_gate(policy);

        match (policy, gated) {
            (Osc52PolicySlug::Allow, Osc52PolicyGated::Allowed(allowed)) => {
                let emitted = allowed.emit_with_base64(&targets, |bytes| bytes.to_vec());
                prop_assert_eq!(emitted, osc52_envelope(&expected_targets, &payload));
            }
            (Osc52PolicySlug::Deny, Osc52PolicyGated::Denied(denied)) => {
                let emitted = denied.emit_empty(&targets);
                prop_assert_eq!(emitted, osc52_envelope(&expected_targets, &[]));
            }
            (Osc52PolicySlug::Prompt, Osc52PolicyGated::Prompted(prompted)) => {
                let allowed = prompted.clone().confirmed_by_operator();
                let denied = prompted.denied_by_operator();

                prop_assert_eq!(
                    allowed.emit_with_base64(&targets, |bytes| bytes.to_vec()),
                    osc52_envelope(&expected_targets, &payload)
                );
                prop_assert_eq!(denied.emit_empty(&targets), osc52_envelope(&expected_targets, &[]));
            }
            _ => prop_assert!(false, "policy gate returned the wrong typed state"),
        }
    }

    #[test]
    fn proptest_osc_protocol_integration_health_safety_tracks_a11y_coverage(
        interactions in prop::collection::vec(any::<bool>(), 0..=128),
        announcements in 0_usize..=160,
    ) {
        let mut health = OscIntegrationHealth::baseline();
        let mut required_announcements = 0_u64;

        for select_instead in interactions {
            let interaction = if select_instead {
                HyperlinkInteraction::SelectInstead { id: 7 }
            } else {
                HyperlinkInteraction::OpenUrl {
                    id: 7,
                    uri: "https://example.test".to_string(),
                }
            };
            health.record_hyperlink_interaction(&interaction);
            required_announcements += 1;
        }
        for _ in 0..announcements {
            health.record_a11y_announcement();
        }

        prop_assert_eq!(health.osc8_clicks_dispatched_total + health.osc8_select_instead_total, required_announcements);
        prop_assert_eq!(health.is_safe(), health.a11y_announcements_total >= required_announcements);
    }
}
