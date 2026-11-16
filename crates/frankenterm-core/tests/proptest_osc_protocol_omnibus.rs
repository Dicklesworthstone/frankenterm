use frankenterm_core::osc_protocol_integration::{
    Osc52WriteAuditOutcome, append_osc52_write_audit, sanitize_osc52_targets,
};
use frankenterm_core::osc_protocol_omnibus::{
    HyperlinkAllocOutcome, HyperlinkId, HyperlinkRegistry, HyperlinkScheme, HyperlinkUri,
    OmnibusOscTelemetry, Osc52AuditDecision, Osc52AuditEvent, Osc52Config, Osc52DenyReason,
    Osc52Direction, Osc52SizeCapDecision, Osc52Target, osc52_size_cap_decision,
    parse_osc52_targets,
};
use proptest::prelude::*;

#[derive(Debug, Clone, Copy)]
enum SchemeCase {
    Http,
    Https,
    Ftp,
    Mailto,
    File,
    Other,
}

impl SchemeCase {
    fn prefix(self) -> &'static str {
        match self {
            Self::Http => "HtTp://",
            Self::Https => "hTtPs://",
            Self::Ftp => "FtP://",
            Self::Mailto => "MaIlTo:",
            Self::File => "FiLe://",
            Self::Other => "custom:",
        }
    }

    const fn expected_scheme(self) -> HyperlinkScheme {
        match self {
            Self::Http => HyperlinkScheme::Http,
            Self::Https => HyperlinkScheme::Https,
            Self::Ftp => HyperlinkScheme::Ftp,
            Self::Mailto => HyperlinkScheme::Mailto,
            Self::File => HyperlinkScheme::File,
            Self::Other => HyperlinkScheme::Other,
        }
    }
}

fn scheme_case_strategy() -> impl Strategy<Value = SchemeCase> {
    prop::sample::select(vec![
        SchemeCase::Http,
        SchemeCase::Https,
        SchemeCase::Ftp,
        SchemeCase::Mailto,
        SchemeCase::File,
        SchemeCase::Other,
    ])
}

fn target_char_strategy() -> impl Strategy<Value = char> {
    prop::sample::select(vec![
        'c', 'p', 's', 'b', 'C', 'P', 'S', 'B', ',', ';', 'x', '0', '1', '2', '3', '4', '5', '6',
        '7', '8', '9', '\u{1b}', '\u{7}',
    ])
}

fn sanitized_target_char_strategy() -> impl Strategy<Value = char> {
    prop::sample::select(vec!['c', 'p', 's', '0', '1', '2', '3', '4', '5', '6', '7'])
}

fn canonical_osc52_targets() -> [(char, Osc52Target); 12] {
    [
        ('c', Osc52Target::Clipboard),
        ('p', Osc52Target::Primary),
        ('s', Osc52Target::Selection),
        ('b', Osc52Target::BufferCut),
        ('0', Osc52Target::CutBuffer0),
        ('1', Osc52Target::CutBuffer1),
        ('2', Osc52Target::CutBuffer2),
        ('3', Osc52Target::CutBuffer3),
        ('4', Osc52Target::CutBuffer4),
        ('5', Osc52Target::CutBuffer5),
        ('6', Osc52Target::CutBuffer6),
        ('7', Osc52Target::CutBuffer7),
    ]
}

fn direction_strategy() -> impl Strategy<Value = Osc52Direction> {
    prop_oneof![Just(Osc52Direction::Write), Just(Osc52Direction::Read)]
}

fn deny_reason_strategy() -> impl Strategy<Value = Osc52DenyReason> {
    prop_oneof![
        Just(Osc52DenyReason::OperatorPolicy),
        Just(Osc52DenyReason::Oversized),
        Just(Osc52DenyReason::ReadDefaultDeny),
        Just(Osc52DenyReason::InvalidBase64),
    ]
}

fn audit_decision_strategy() -> impl Strategy<Value = Osc52AuditDecision> {
    prop_oneof![
        Just(Osc52AuditDecision::Allowed),
        deny_reason_strategy().prop_map(|reason| Osc52AuditDecision::Denied { reason }),
        Just(Osc52AuditDecision::Prompted),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_osc_omnibus_scheme_classification_is_case_insensitive(
        case in scheme_case_strategy(),
        suffix in "[A-Za-z0-9_./?&=%+-]{0,64}",
    ) {
        let raw = format!("{}{}", case.prefix(), suffix);
        let uri = HyperlinkUri::new(raw.clone());
        let expected = case.expected_scheme();

        prop_assert_eq!(HyperlinkScheme::classify(&raw), expected);
        prop_assert_eq!(uri.raw, raw);
        prop_assert_eq!(uri.scheme, expected);
        prop_assert_eq!(expected.is_well_known(), matches!(expected, HyperlinkScheme::Http | HyperlinkScheme::Https | HyperlinkScheme::Mailto));
        prop_assert_eq!(expected.label(), match expected {
            HyperlinkScheme::Http => "http",
            HyperlinkScheme::Https => "https",
            HyperlinkScheme::Ftp => "ftp",
            HyperlinkScheme::Mailto => "mailto",
            HyperlinkScheme::File => "file",
            HyperlinkScheme::Other => "other",
        });
    }

    #[test]
    fn proptest_osc_omnibus_registry_caps_allocations_and_resolves_ids(
        cap in 0_usize..=16,
        attempts in 0_usize..=32,
    ) {
        let mut registry = HyperlinkRegistry::new(cap);
        let expected_allocations = cap.min(attempts);

        for idx in 0..attempts {
            let raw = format!("https://example.test/{idx}");
            let outcome = registry.allocate(HyperlinkUri::new(raw.clone()));
            if idx < cap {
                let id = match outcome {
                    HyperlinkAllocOutcome::Allocated(id) => id,
                    HyperlinkAllocOutcome::DeniedFull => panic!("allocation denied before cap"),
                };
                prop_assert_eq!(id, HyperlinkId((idx + 1) as u32));
                prop_assert_eq!(registry.resolve(id).map(|uri| uri.raw.as_str()), Some(raw.as_str()));
            } else {
                prop_assert_eq!(outcome, HyperlinkAllocOutcome::DeniedFull);
            }
        }

        prop_assert_eq!(registry.len(), expected_allocations);
        prop_assert_eq!(registry.is_empty(), expected_allocations == 0);
        prop_assert!(registry.resolve(HyperlinkId::NONE).is_none());
        prop_assert!(registry.resolve(HyperlinkId((expected_allocations + 1) as u32)).is_none());
    }

    #[test]
    fn proptest_osc_omnibus_parse_osc52_targets_is_stable_ordered_and_deduped(
        chars in prop::collection::vec(target_char_strategy(), 0..=80),
    ) {
        let field: String = chars.iter().collect();
        let parsed = parse_osc52_targets(&field);
        let expected: Vec<Osc52Target> = canonical_osc52_targets()
            .into_iter()
        .filter_map(|(letter, target)| field.contains(letter).then_some(target))
        .collect();

        prop_assert_eq!(&parsed, &expected);
        for target in &parsed {
            prop_assert_eq!(Osc52Target::from_letter(target.letter()), Some(*target));
        }
        for invalid in ['C', 'P', 'S', 'B', 'x', '8', '9', '\u{1b}', '\u{7}'] {
            prop_assert_eq!(Osc52Target::from_letter(invalid), None);
        }
    }

    #[test]
    fn proptest_osc_omnibus_numeric_cut_buffer_letters_roundtrip(index in 0_u8..=7) {
        let letter = char::from(b'0' + index);
        let target = Osc52Target::from_letter(letter).expect("numeric cut buffer is valid");

        prop_assert_eq!(target.letter(), letter);
        prop_assert_eq!(parse_osc52_targets(&letter.to_string()), vec![target]);
    }

    #[test]
    fn proptest_osc_omnibus_sanitized_targets_remain_auditable(
        chars in prop::collection::vec(target_char_strategy(), 0..=80),
    ) {
        let raw: String = chars.iter().collect();
        let sanitized = sanitize_osc52_targets(&raw);
        let parsed = parse_osc52_targets(&sanitized);
        let expected: Vec<Osc52Target> = canonical_osc52_targets()
            .into_iter()
            .filter(|(_, target)| *target != Osc52Target::BufferCut)
            .filter_map(|(letter, target)| sanitized.contains(letter).then_some(target))
            .collect();

        prop_assert_eq!(&parsed, &expected);
        prop_assert!(!parsed.is_empty());
        for target in &parsed {
            prop_assert!(sanitized.contains(target.letter()));
        }
    }

    #[test]
    fn proptest_osc_omnibus_write_audit_counts_sanitized_numeric_targets(
        chars in prop::collection::vec(sanitized_target_char_strategy(), 0..=40),
        decoded_bytes in 0_u64..=1_000_000,
        source_pane in 0_u64..=1_000,
        timestamp_ms in 0_u64..=1_000_000,
    ) {
        let targets: String = chars.iter().collect();
        let sanitized = sanitize_osc52_targets(&targets);
        let expected_count = parse_osc52_targets(&sanitized).len();
        let mut chain = frankenterm_core::policy_audit_chain::AuditChain::new(8);
        let entry = append_osc52_write_audit(
            &mut chain,
            &targets,
            decoded_bytes,
            Osc52WriteAuditOutcome::Allowed,
            source_pane,
            timestamp_ms,
        );

        let expected_description =
            format!("osc52 write targets={expected_count} bytes={decoded_bytes} decision=allowed");
        prop_assert_eq!(entry.description.as_str(), expected_description.as_str());
    }

    #[test]
    fn proptest_osc_omnibus_size_cap_gate_is_inclusive_at_cap(
        max_payload_bytes in 0_u64..=1_000_000,
        decoded_bytes in 0_u64..=1_000_000,
        refuse_invalid_base64 in any::<bool>(),
    ) {
        let config = Osc52Config::default()
            .with_max_payload_bytes(max_payload_bytes)
            .with_refuse_invalid_base64(refuse_invalid_base64);
        let expected = if decoded_bytes > max_payload_bytes {
            Osc52SizeCapDecision::RejectedOversized
        } else {
            Osc52SizeCapDecision::Approved
        };

        prop_assert_eq!(config.max_payload_bytes(), max_payload_bytes);
        prop_assert_eq!(config.refuse_invalid_base64(), refuse_invalid_base64);
        prop_assert_eq!(osc52_size_cap_decision(decoded_bytes, config), expected);
        prop_assert_eq!(osc52_size_cap_decision(max_payload_bytes, config), Osc52SizeCapDecision::Approved);
    }

    #[test]
    fn proptest_osc_omnibus_telemetry_routes_osc52_audit_events(
        events in prop::collection::vec((direction_strategy(), audit_decision_strategy(), 0_u64..=1_000_000), 0..=64),
    ) {
        let mut telemetry = OmnibusOscTelemetry::default();
        let mut writes_allowed = 0_u64;
        let mut writes_denied = 0_u64;
        let mut writes_prompted = 0_u64;
        let mut reads_attempted = 0_u64;
        let mut reads_allowed = 0_u64;
        let mut reads_denied = 0_u64;
        let mut oversized_rejections = 0_u64;

        for (idx, (direction, decision, decoded_bytes)) in events.iter().copied().enumerate() {
            let event = Osc52AuditEvent {
                direction,
                targets: vec![Osc52Target::Clipboard],
                decoded_bytes,
                decision,
                source_pane: idx as u64,
                timestamp_ms: idx as u64,
            };
            telemetry.record_osc52_audit(&event);

            match (direction, decision) {
                (Osc52Direction::Write, Osc52AuditDecision::Allowed) => writes_allowed += 1,
                (Osc52Direction::Write, Osc52AuditDecision::Denied { reason }) => {
                    writes_denied += 1;
                    if reason == Osc52DenyReason::Oversized {
                        oversized_rejections += 1;
                    }
                }
                (Osc52Direction::Write, Osc52AuditDecision::Prompted) => writes_prompted += 1,
                (Osc52Direction::Read, Osc52AuditDecision::Allowed) => {
                    reads_attempted += 1;
                    reads_allowed += 1;
                }
                (Osc52Direction::Read, Osc52AuditDecision::Denied { .. }) => {
                    reads_attempted += 1;
                    reads_denied += 1;
                }
                (Osc52Direction::Read, Osc52AuditDecision::Prompted) => {
                    reads_attempted += 1;
                }
            }
        }

        prop_assert_eq!(telemetry.osc52_writes_allowed, writes_allowed);
        prop_assert_eq!(telemetry.osc52_writes_denied, writes_denied);
        prop_assert_eq!(telemetry.osc52_writes_prompted, writes_prompted);
        prop_assert_eq!(telemetry.osc52_reads_attempted, reads_attempted);
        prop_assert_eq!(telemetry.osc52_reads_allowed, reads_allowed);
        prop_assert_eq!(telemetry.osc52_reads_denied, reads_denied);
        prop_assert_eq!(telemetry.osc52_oversized_rejections, oversized_rejections);
    }
}
