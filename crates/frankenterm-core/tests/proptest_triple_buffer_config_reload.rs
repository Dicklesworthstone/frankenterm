use proptest::prelude::*;

use frankenterm_core::triple_buffer_config_reload::{
    MAX_FORCE_RECYCLE_MS, TripleBufferConfigError, TripleBufferConfigSection,
    build_reconfigure_event, parse_render_triple_buffer_section,
};
use frankenterm_core::triple_buffer_fleet_health::{PaneId, ReconfigureSource};
use frankenterm_core::triple_buffer_watchdog::WatchdogConfig;

fn arb_valid_section() -> impl Strategy<Value = TripleBufferConfigSection> {
    (2u64..=MAX_FORCE_RECYCLE_MS).prop_flat_map(|force_recycle_after_ms| {
        (1u64..force_recycle_after_ms).prop_map(move |warn_after_ms| TripleBufferConfigSection {
            warn_after_ms,
            force_recycle_after_ms,
        })
    })
}

fn arb_source() -> impl Strategy<Value = ReconfigureSource> {
    prop_oneof![
        Just(ReconfigureSource::OperatorReload),
        Just(ReconfigureSource::TestOverride),
        Just(ReconfigureSource::AutomaticDegradation),
    ]
}

fn section_toml(section: TripleBufferConfigSection) -> String {
    format!(
        "\
[render.triple_buffer]
warn_after_ms = {}
force_recycle_after_ms = {}
",
        section.warn_after_ms, section.force_recycle_after_ms
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn proptest_triple_buffer_valid_sections_parse_and_roundtrip_watchdog(
        section in arb_valid_section(),
    ) {
        let parsed = parse_render_triple_buffer_section(&section_toml(section))
            .expect("valid section parses")
            .expect("section is present");

        prop_assert_eq!(parsed, section);
        prop_assert_eq!(
            TripleBufferConfigSection::from_watchdog_config(&section.to_watchdog_config()),
            section,
        );
    }

    #[test]
    fn proptest_triple_buffer_parse_matches_validate_for_numeric_sections(
        warn_after_ms in 0u64..=(MAX_FORCE_RECYCLE_MS + 10_000),
        force_recycle_after_ms in 0u64..=(MAX_FORCE_RECYCLE_MS + 10_000),
    ) {
        let section = TripleBufferConfigSection {
            warn_after_ms,
            force_recycle_after_ms,
        };
        let parsed = parse_render_triple_buffer_section(&section_toml(section));
        let expected = section.validate().map(|()| Some(section));

        prop_assert_eq!(parsed, expected);
    }

    #[test]
    fn proptest_triple_buffer_section_serde_preserves_public_fields(
        warn_after_ms in any::<u64>(),
        force_recycle_after_ms in any::<u64>(),
    ) {
        let section = TripleBufferConfigSection {
            warn_after_ms,
            force_recycle_after_ms,
        };

        let json = serde_json::to_string(&section).expect("serialize section");
        let back: TripleBufferConfigSection =
            serde_json::from_str(&json).expect("deserialize section");

        prop_assert_eq!(back, section);
    }

    #[test]
    fn proptest_triple_buffer_event_builder_preserves_public_audit_fields(
        prev in arb_valid_section(),
        next in arb_valid_section(),
        pane_id in any::<u64>(),
        timestamp_ms in any::<u64>(),
        source in arb_source(),
    ) {
        let event = build_reconfigure_event(
            PaneId(pane_id),
            prev.to_watchdog_config(),
            next,
            timestamp_ms,
            source,
        );

        prop_assert_eq!(event.pane_id, PaneId(pane_id));
        prop_assert_eq!(event.prev_warn_ms, prev.warn_after_ms);
        prop_assert_eq!(event.prev_force_ms, prev.force_recycle_after_ms);
        prop_assert_eq!(event.new_warn_ms, next.warn_after_ms);
        prop_assert_eq!(event.new_force_ms, next.force_recycle_after_ms);
        prop_assert_eq!(event.timestamp_ms, timestamp_ms);
        prop_assert_eq!(event.source, source);
        prop_assert_eq!(event.is_no_op(), prev == next);
        prop_assert_eq!(
            event.is_relaxed(),
            next.warn_after_ms > prev.warn_after_ms
                || next.force_recycle_after_ms > prev.force_recycle_after_ms,
        );
    }

    #[test]
    fn proptest_triple_buffer_missing_section_stays_absent(
        unrelated_value in 0u64..=MAX_FORCE_RECYCLE_MS,
    ) {
        let render_without_triple_buffer = format!(
            "\
[render]
unrelated_value = {unrelated_value}
"
        );
        let other_section = format!(
            "\
[other]
warn_after_ms = {unrelated_value}
force_recycle_after_ms = {}
",
            unrelated_value.saturating_add(1)
        );

        prop_assert_eq!(
            parse_render_triple_buffer_section(&render_without_triple_buffer),
            Ok(None),
        );
        prop_assert_eq!(parse_render_triple_buffer_section(&other_section), Ok(None));
    }

    #[test]
    fn proptest_triple_buffer_validation_errors_report_boundary_values(
        warn_after_ms in 1u64..=MAX_FORCE_RECYCLE_MS,
        force_recycle_after_ms in (MAX_FORCE_RECYCLE_MS + 1)..=(MAX_FORCE_RECYCLE_MS + 10_000),
    ) {
        let section = TripleBufferConfigSection {
            warn_after_ms,
            force_recycle_after_ms,
        };

        prop_assert_eq!(
            section.validate(),
            Err(TripleBufferConfigError::ForceExceedsCap {
                force_ms: force_recycle_after_ms,
                cap_ms: MAX_FORCE_RECYCLE_MS,
            }),
        );
    }
}

#[test]
fn watchdog_default_snapshot_is_parseable_when_rendered_as_section() {
    let section = TripleBufferConfigSection::from_watchdog_config(&WatchdogConfig::default());
    assert_eq!(
        parse_render_triple_buffer_section(&section_toml(section)),
        Ok(Some(section)),
    );
}
