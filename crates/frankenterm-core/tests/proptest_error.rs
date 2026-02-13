//! Property-based tests for `frankenterm_core::error` types.
//!
//! Validates:
//! 1. RemediationCommand serde roundtrip (with/without platform)
//! 2. Remediation serde roundtrip (various builder combos)
//! 3. Remediation builder chain preserves all fields
//! 4. Remediation render_plain formatting invariants
//! 5. Error::remediation() returns Some for every variant
//! 6. Error Display includes inner message
//! 7. WeztermError::is_circuit_breaker_trigger() classification
//! 8. WeztermError remediation() non-empty for all variants
//! 9. StorageError remediation() non-empty for all variants
//! 10. PatternError remediation() non-empty for all variants
//! 11. WorkflowError remediation() non-empty for all variants
//! 12. ConfigError remediation() non-empty for all variants
//! 13. format_error_with_remediation() output structure
//! 14. From conversions for sub-error types

use proptest::prelude::*;

use frankenterm_core::error::{
    ConfigError, PatternError, Remediation, RemediationCommand, StorageError, WeztermError,
    WorkflowError, format_error_with_remediation,
};
use frankenterm_core::Error as CoreError;

// =============================================================================
// Strategies
// =============================================================================

/// Arbitrary non-empty string for error messages, labels, etc.
fn arb_nonempty_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _/.-]{1,80}".prop_map(|s| s.trim().to_string())
        .prop_filter("must be non-empty", |s| !s.is_empty())
}

/// Arbitrary optional string for platform hints, learn_more links.
fn arb_opt_string() -> impl Strategy<Value = Option<String>> {
    prop_oneof![
        Just(None),
        arb_nonempty_string().prop_map(Some),
    ]
}

/// Arbitrary u64 for pane IDs, timeouts, retry_after_ms, sequence numbers.
fn arb_u64() -> impl Strategy<Value = u64> {
    any::<u64>()
}

/// Arbitrary i32 for schema versions.
fn arb_i32() -> impl Strategy<Value = i32> {
    any::<i32>()
}

/// Arbitrary RemediationCommand.
fn arb_remediation_command() -> impl Strategy<Value = RemediationCommand> {
    (arb_nonempty_string(), arb_nonempty_string(), arb_opt_string()).prop_map(
        |(label, command, platform)| RemediationCommand {
            label,
            command,
            platform,
        },
    )
}

/// Arbitrary Remediation with varying field population.
fn arb_remediation() -> impl Strategy<Value = Remediation> {
    (
        arb_nonempty_string(),
        proptest::collection::vec(arb_remediation_command(), 0..5),
        proptest::collection::vec(arb_nonempty_string(), 0..4),
        arb_opt_string(),
    )
        .prop_map(|(summary, commands, alternatives, learn_more)| Remediation {
            summary,
            commands,
            alternatives,
            learn_more,
        })
}

/// Arbitrary WeztermError variant.
fn arb_wezterm_error() -> impl Strategy<Value = WeztermError> {
    prop_oneof![
        (0..1u32).prop_map(|_| WeztermError::CliNotFound),
        (0..1u32).prop_map(|_| WeztermError::NotRunning),
        arb_u64().prop_map(WeztermError::PaneNotFound),
        arb_nonempty_string().prop_map(WeztermError::SocketNotFound),
        arb_nonempty_string().prop_map(WeztermError::CommandFailed),
        arb_nonempty_string().prop_map(WeztermError::ParseError),
        arb_u64().prop_map(WeztermError::Timeout),
        arb_u64().prop_map(|ms| WeztermError::CircuitOpen { retry_after_ms: ms }),
    ]
}

/// Arbitrary StorageError variant.
fn arb_storage_error() -> impl Strategy<Value = StorageError> {
    prop_oneof![
        arb_nonempty_string().prop_map(StorageError::Database),
        (arb_u64(), arb_u64()).prop_map(|(expected, actual)| StorageError::SequenceDiscontinuity {
            expected,
            actual,
        }),
        arb_nonempty_string().prop_map(StorageError::MigrationFailed),
        (arb_i32(), arb_i32()).prop_map(|(current, supported)| StorageError::SchemaTooNew {
            current,
            supported,
        }),
        (arb_nonempty_string(), arb_nonempty_string()).prop_map(|(current, min_compatible)| {
            StorageError::WaTooOld {
                current,
                min_compatible,
            }
        }),
        arb_nonempty_string().prop_map(StorageError::FtsQueryError),
        arb_nonempty_string().prop_map(|details| StorageError::Corruption { details }),
        arb_nonempty_string().prop_map(StorageError::NotFound),
    ]
}

/// Arbitrary PatternError variant.
fn arb_pattern_error() -> impl Strategy<Value = PatternError> {
    prop_oneof![
        arb_nonempty_string().prop_map(PatternError::InvalidRule),
        arb_nonempty_string().prop_map(PatternError::InvalidRegex),
        arb_nonempty_string().prop_map(PatternError::PackNotFound),
        (0..1u32).prop_map(|_| PatternError::MatchTimeout),
    ]
}

/// Arbitrary WorkflowError variant.
fn arb_workflow_error() -> impl Strategy<Value = WorkflowError> {
    prop_oneof![
        arb_nonempty_string().prop_map(WorkflowError::NotFound),
        arb_nonempty_string().prop_map(WorkflowError::Aborted),
        arb_nonempty_string().prop_map(WorkflowError::GuardFailed),
        (0..1u32).prop_map(|_| WorkflowError::PaneLocked),
    ]
}

/// Arbitrary ConfigError variant.
fn arb_config_error() -> impl Strategy<Value = ConfigError> {
    prop_oneof![
        arb_nonempty_string().prop_map(ConfigError::FileNotFound),
        (arb_nonempty_string(), arb_nonempty_string())
            .prop_map(|(path, msg)| ConfigError::ReadFailed(path, msg)),
        arb_nonempty_string().prop_map(ConfigError::ParseError),
        arb_nonempty_string().prop_map(ConfigError::ParseFailed),
        arb_nonempty_string().prop_map(ConfigError::SerializeFailed),
        arb_nonempty_string().prop_map(ConfigError::ValidationError),
    ]
}

/// Arbitrary CoreError variant (excluding Io and Json which are hard to generate arbitrarily).
fn arb_core_error() -> impl Strategy<Value = CoreError> {
    prop_oneof![
        arb_wezterm_error().prop_map(CoreError::Wezterm),
        arb_storage_error().prop_map(CoreError::Storage),
        arb_pattern_error().prop_map(CoreError::Pattern),
        arb_workflow_error().prop_map(CoreError::Workflow),
        arb_config_error().prop_map(CoreError::Config),
        arb_nonempty_string().prop_map(CoreError::Policy),
        arb_nonempty_string().prop_map(CoreError::Runtime),
        arb_nonempty_string().prop_map(CoreError::SetupError),
        arb_nonempty_string().prop_map(CoreError::Cancelled),
        arb_nonempty_string().prop_map(CoreError::Panicked),
    ]
}

// =============================================================================
// 1. RemediationCommand serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn remediation_command_serde_roundtrip_with_platform(
        label in arb_nonempty_string(),
        command in arb_nonempty_string(),
        platform in arb_nonempty_string(),
    ) {
        let cmd = RemediationCommand {
            label: label.clone(),
            command: command.clone(),
            platform: Some(platform.clone()),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: RemediationCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.label, &label, "label mismatch");
        prop_assert_eq!(&back.command, &command, "command mismatch");
        prop_assert_eq!(back.platform.as_deref(), Some(platform.as_str()), "platform mismatch");
    }

    #[test]
    fn remediation_command_serde_roundtrip_without_platform(
        label in arb_nonempty_string(),
        command in arb_nonempty_string(),
    ) {
        let cmd = RemediationCommand {
            label: label.clone(),
            command: command.clone(),
            platform: None,
        };
        let json = serde_json::to_string(&cmd).unwrap();
        let back: RemediationCommand = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.label, &label, "label mismatch");
        prop_assert_eq!(&back.command, &command, "command mismatch");
        prop_assert!(back.platform.is_none(), "platform should be None");
    }
}

// =============================================================================
// 2. Remediation serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn remediation_serde_roundtrip(rem in arb_remediation()) {
        let json = serde_json::to_string(&rem).unwrap();
        let back: Remediation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.summary, &rem.summary, "summary mismatch");
        prop_assert_eq!(back.commands.len(), rem.commands.len(), "commands length mismatch");
        prop_assert_eq!(back.alternatives.len(), rem.alternatives.len(), "alternatives length mismatch");
        let has_learn_more = back.learn_more.is_some();
        let expected_learn_more = rem.learn_more.is_some();
        prop_assert_eq!(has_learn_more, expected_learn_more, "learn_more presence mismatch");
    }

    #[test]
    fn remediation_serde_roundtrip_preserves_command_details(
        rem in arb_remediation(),
    ) {
        let json = serde_json::to_string(&rem).unwrap();
        let back: Remediation = serde_json::from_str(&json).unwrap();
        for (i, (orig, recovered)) in rem.commands.iter().zip(back.commands.iter()).enumerate() {
            prop_assert_eq!(&recovered.label, &orig.label, "command label mismatch at index {}", i);
            prop_assert_eq!(&recovered.command, &orig.command, "command text mismatch at index {}", i);
            let orig_plat = orig.platform.is_some();
            let recovered_plat = recovered.platform.is_some();
            prop_assert_eq!(recovered_plat, orig_plat, "platform presence mismatch at index {}", i);
        }
    }

    #[test]
    fn remediation_serde_roundtrip_preserves_alternatives(
        rem in arb_remediation(),
    ) {
        let json = serde_json::to_string(&rem).unwrap();
        let back: Remediation = serde_json::from_str(&json).unwrap();
        for (i, (orig, recovered)) in rem.alternatives.iter().zip(back.alternatives.iter()).enumerate() {
            prop_assert_eq!(recovered, orig, "alternative mismatch at index {}", i);
        }
    }
}

// =============================================================================
// 3. Remediation builder chain
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn remediation_builder_preserves_all_fields(
        summary in arb_nonempty_string(),
        cmd_label in arb_nonempty_string(),
        cmd_text in arb_nonempty_string(),
        plat_label in arb_nonempty_string(),
        plat_cmd in arb_nonempty_string(),
        plat_name in arb_nonempty_string(),
        alt in arb_nonempty_string(),
        link in arb_nonempty_string(),
    ) {
        let r = Remediation::new(summary.clone())
            .command(cmd_label.clone(), cmd_text.clone())
            .platform_command(plat_label.clone(), plat_cmd.clone(), plat_name.clone())
            .alternative(alt.clone())
            .learn_more(link.clone());

        prop_assert_eq!(&r.summary, &summary, "summary");
        prop_assert_eq!(r.commands.len(), 2, "should have 2 commands");
        prop_assert_eq!(&r.commands[0].label, &cmd_label, "first cmd label");
        prop_assert_eq!(&r.commands[0].command, &cmd_text, "first cmd text");
        prop_assert!(r.commands[0].platform.is_none(), "first cmd no platform");
        prop_assert_eq!(&r.commands[1].label, &plat_label, "second cmd label");
        prop_assert_eq!(&r.commands[1].command, &plat_cmd, "second cmd text");
        prop_assert_eq!(r.commands[1].platform.as_deref(), Some(plat_name.as_str()), "second cmd platform");
        prop_assert_eq!(r.alternatives.len(), 1, "one alternative");
        prop_assert_eq!(&r.alternatives[0], &alt, "alternative content");
        prop_assert_eq!(r.learn_more.as_deref(), Some(link.as_str()), "learn_more");
    }

    #[test]
    fn remediation_new_starts_empty(summary in arb_nonempty_string()) {
        let r = Remediation::new(summary.clone());
        prop_assert_eq!(&r.summary, &summary, "summary preserved");
        prop_assert!(r.commands.is_empty(), "commands start empty");
        prop_assert!(r.alternatives.is_empty(), "alternatives start empty");
        prop_assert!(r.learn_more.is_none(), "learn_more starts None");
    }

    #[test]
    fn remediation_builder_multiple_commands(
        summary in arb_nonempty_string(),
        labels in proptest::collection::vec(arb_nonempty_string(), 1..6),
        cmds in proptest::collection::vec(arb_nonempty_string(), 1..6),
    ) {
        let count = labels.len().min(cmds.len());
        let mut r = Remediation::new(summary);
        for i in 0..count {
            r = r.command(labels[i].clone(), cmds[i].clone());
        }
        prop_assert_eq!(r.commands.len(), count, "command count");
        for i in 0..count {
            prop_assert_eq!(&r.commands[i].label, &labels[i], "label at {}", i);
            prop_assert_eq!(&r.commands[i].command, &cmds[i], "cmd at {}", i);
        }
    }

    #[test]
    fn remediation_builder_multiple_alternatives(
        summary in arb_nonempty_string(),
        alts in proptest::collection::vec(arb_nonempty_string(), 1..6),
    ) {
        let mut r = Remediation::new(summary);
        for a in &alts {
            r = r.alternative(a.clone());
        }
        prop_assert_eq!(r.alternatives.len(), alts.len(), "alt count");
        for (i, a) in alts.iter().enumerate() {
            prop_assert_eq!(&r.alternatives[i], a, "alt at {}", i);
        }
    }
}

// =============================================================================
// 4. Remediation render_plain formatting invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn render_plain_always_contains_to_fix_and_summary(
        summary in arb_nonempty_string(),
    ) {
        let r = Remediation::new(summary.clone());
        let output = r.render_plain();
        prop_assert!(output.contains("To fix:"), "missing 'To fix:'");
        prop_assert!(output.contains(&summary), "missing summary text");
    }

    #[test]
    fn render_plain_shows_commands_section_when_present(
        summary in arb_nonempty_string(),
        label in arb_nonempty_string(),
        cmd in arb_nonempty_string(),
    ) {
        let r = Remediation::new(summary).command(label.clone(), cmd.clone());
        let output = r.render_plain();
        prop_assert!(output.contains("Commands:"), "missing Commands section");
        prop_assert!(output.contains(&label), "missing label in output");
        prop_assert!(output.contains(&cmd), "missing command in output");
    }

    #[test]
    fn render_plain_shows_platform_hint(
        summary in arb_nonempty_string(),
        label in arb_nonempty_string(),
        cmd in arb_nonempty_string(),
        platform in arb_nonempty_string(),
    ) {
        let r = Remediation::new(summary).platform_command(label.clone(), cmd.clone(), platform.clone());
        let output = r.render_plain();
        // Format is "label (platform): command"
        let expected_fragment = format!("{} ({})", label, platform);
        prop_assert!(output.contains(&expected_fragment), "missing platform label fragment: {}", expected_fragment);
        prop_assert!(output.contains(&cmd), "missing command");
    }

    #[test]
    fn render_plain_shows_alternatives_when_present(
        summary in arb_nonempty_string(),
        alt in arb_nonempty_string(),
    ) {
        let r = Remediation::new(summary).alternative(alt.clone());
        let output = r.render_plain();
        prop_assert!(output.contains("Alternatives:"), "missing Alternatives section");
        prop_assert!(output.contains(&alt), "missing alternative text");
    }

    #[test]
    fn render_plain_shows_learn_more_when_present(
        summary in arb_nonempty_string(),
        link in arb_nonempty_string(),
    ) {
        let r = Remediation::new(summary).learn_more(link.clone());
        let output = r.render_plain();
        prop_assert!(output.contains("Learn more:"), "missing Learn more section");
        prop_assert!(output.contains(&link), "missing link");
    }

    #[test]
    fn render_plain_omits_empty_sections(summary in arb_nonempty_string()) {
        let r = Remediation::new(summary);
        let output = r.render_plain();
        prop_assert!(!output.contains("Commands:"), "should not show Commands when empty");
        prop_assert!(!output.contains("Alternatives:"), "should not show Alternatives when empty");
        prop_assert!(!output.contains("Learn more:"), "should not show Learn more when empty");
    }

    #[test]
    fn render_plain_full_builder_contains_all_sections(
        summary in arb_nonempty_string(),
        label in arb_nonempty_string(),
        cmd in arb_nonempty_string(),
        alt in arb_nonempty_string(),
        link in arb_nonempty_string(),
    ) {
        let r = Remediation::new(summary.clone())
            .command(label, cmd)
            .alternative(alt)
            .learn_more(link);
        let output = r.render_plain();
        prop_assert!(output.contains("To fix:"), "missing To fix:");
        prop_assert!(output.contains(&summary), "missing summary");
        prop_assert!(output.contains("Commands:"), "missing Commands");
        prop_assert!(output.contains("Alternatives:"), "missing Alternatives");
        prop_assert!(output.contains("Learn more:"), "missing Learn more");
    }
}

// =============================================================================
// 5. Error::remediation() returns Some for every variant
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn error_remediation_always_some(err in arb_core_error()) {
        let remediation = err.remediation();
        let is_some = remediation.is_some();
        prop_assert!(is_some, "remediation() returned None for: {:?}", err);
    }

    #[test]
    fn error_remediation_has_nonempty_summary(err in arb_core_error()) {
        let remediation = err.remediation().unwrap();
        let summary_empty = remediation.summary.is_empty();
        prop_assert!(!summary_empty, "empty summary for: {:?}", err);
    }

    #[test]
    fn error_remediation_has_at_least_one_command(err in arb_core_error()) {
        let remediation = err.remediation().unwrap();
        let has_commands = !remediation.commands.is_empty();
        prop_assert!(has_commands, "no commands for: {:?}", err);
    }
}

// =============================================================================
// 6. Error Display includes inner message
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn error_display_contains_message_text_policy(msg in arb_nonempty_string()) {
        let err = CoreError::Policy(msg.clone());
        let display = err.to_string();
        prop_assert!(display.contains(&msg), "Policy display missing message: {}", display);
    }

    #[test]
    fn error_display_contains_message_text_runtime(msg in arb_nonempty_string()) {
        let err = CoreError::Runtime(msg.clone());
        let display = err.to_string();
        prop_assert!(display.contains(&msg), "Runtime display missing message: {}", display);
    }

    #[test]
    fn error_display_contains_message_text_setup(msg in arb_nonempty_string()) {
        let err = CoreError::SetupError(msg.clone());
        let display = err.to_string();
        prop_assert!(display.contains(&msg), "SetupError display missing message: {}", display);
    }

    #[test]
    fn error_display_contains_message_text_cancelled(msg in arb_nonempty_string()) {
        let err = CoreError::Cancelled(msg.clone());
        let display = err.to_string();
        prop_assert!(display.contains(&msg), "Cancelled display missing message: {}", display);
    }

    #[test]
    fn error_display_contains_message_text_panicked(msg in arb_nonempty_string()) {
        let err = CoreError::Panicked(msg.clone());
        let display = err.to_string();
        prop_assert!(display.contains(&msg), "Panicked display missing message: {}", display);
    }

    #[test]
    fn wezterm_pane_not_found_display_contains_id(id in arb_u64()) {
        let err = WeztermError::PaneNotFound(id);
        let display = err.to_string();
        let id_str = id.to_string();
        prop_assert!(display.contains(&id_str), "PaneNotFound display missing id {}: {}", id, display);
    }

    #[test]
    fn wezterm_timeout_display_contains_seconds(secs in arb_u64()) {
        let err = WeztermError::Timeout(secs);
        let display = err.to_string();
        let secs_str = secs.to_string();
        prop_assert!(display.contains(&secs_str), "Timeout display missing seconds {}: {}", secs, display);
    }

    #[test]
    fn wezterm_circuit_open_display_contains_ms(ms in arb_u64()) {
        let err = WeztermError::CircuitOpen { retry_after_ms: ms };
        let display = err.to_string();
        let ms_str = ms.to_string();
        prop_assert!(display.contains(&ms_str), "CircuitOpen display missing ms {}: {}", ms, display);
    }
}

// =============================================================================
// 7. WeztermError::is_circuit_breaker_trigger()
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn wezterm_circuit_breaker_trigger_classification(err in arb_wezterm_error()) {
        let is_trigger = err.is_circuit_breaker_trigger();
        let is_cli_not_found = matches!(err, WeztermError::CliNotFound);
        let is_not_running = matches!(err, WeztermError::NotRunning);
        let is_socket_not_found = matches!(err, WeztermError::SocketNotFound(_));
        let is_timeout = matches!(err, WeztermError::Timeout(_));
        let is_command_failed = matches!(err, WeztermError::CommandFailed(_));
        let should_be_trigger = is_cli_not_found || is_not_running || is_socket_not_found || is_timeout || is_command_failed;

        prop_assert_eq!(is_trigger, should_be_trigger,
            "circuit breaker mismatch for {:?}: got {}, expected {}", err, is_trigger, should_be_trigger);
    }

    #[test]
    fn wezterm_pane_not_found_not_trigger(id in arb_u64()) {
        let err = WeztermError::PaneNotFound(id);
        let trigger = err.is_circuit_breaker_trigger();
        prop_assert!(!trigger, "PaneNotFound should not be a circuit breaker trigger");
    }

    #[test]
    fn wezterm_parse_error_not_trigger(msg in arb_nonempty_string()) {
        let err = WeztermError::ParseError(msg);
        let trigger = err.is_circuit_breaker_trigger();
        prop_assert!(!trigger, "ParseError should not be a circuit breaker trigger");
    }

    #[test]
    fn wezterm_circuit_open_not_trigger(ms in arb_u64()) {
        let err = WeztermError::CircuitOpen { retry_after_ms: ms };
        let trigger = err.is_circuit_breaker_trigger();
        prop_assert!(!trigger, "CircuitOpen should not be a circuit breaker trigger");
    }
}

// =============================================================================
// 8. WeztermError remediation() non-empty for all variants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn wezterm_error_remediation_nonempty(err in arb_wezterm_error()) {
        let r = err.remediation();
        let summary_empty = r.summary.is_empty();
        prop_assert!(!summary_empty, "empty summary for WeztermError: {:?}", err);
        let cmds_empty = r.commands.is_empty();
        prop_assert!(!cmds_empty, "empty commands for WeztermError: {:?}", err);
    }

    #[test]
    fn wezterm_cli_not_found_has_learn_more(_dummy in 0..1u32) {
        let r = WeztermError::CliNotFound.remediation();
        let has_learn_more = r.learn_more.is_some();
        prop_assert!(has_learn_more, "CliNotFound should have learn_more link");
        let has_multiple_cmds = r.commands.len() >= 3;
        prop_assert!(has_multiple_cmds, "CliNotFound should have multiple install commands");
    }
}

// =============================================================================
// 9. StorageError remediation() non-empty for all variants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn storage_error_remediation_nonempty(err in arb_storage_error()) {
        let r = err.remediation();
        let summary_empty = r.summary.is_empty();
        prop_assert!(!summary_empty, "empty summary for StorageError: {:?}", err);
        let cmds_empty = r.commands.is_empty();
        prop_assert!(!cmds_empty, "empty commands for StorageError: {:?}", err);
    }

    #[test]
    fn storage_sequence_discontinuity_mentions_values(
        expected in arb_u64(),
        actual in arb_u64(),
    ) {
        let err = StorageError::SequenceDiscontinuity { expected, actual };
        let r = err.remediation();
        let expected_str = expected.to_string();
        let actual_str = actual.to_string();
        prop_assert!(
            r.summary.contains(&expected_str) && r.summary.contains(&actual_str),
            "SequenceDiscontinuity summary should contain expected ({}) and actual ({}): {}",
            expected, actual, r.summary
        );
    }

    #[test]
    fn storage_schema_too_new_mentions_current(
        current in arb_i32(),
        supported in arb_i32(),
    ) {
        let err = StorageError::SchemaTooNew { current, supported };
        let r = err.remediation();
        let current_str = current.to_string();
        prop_assert!(
            r.summary.contains(&current_str),
            "SchemaTooNew summary should contain current version {}: {}",
            current, r.summary
        );
    }
}

// =============================================================================
// 10. PatternError remediation() non-empty for all variants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn pattern_error_remediation_nonempty(err in arb_pattern_error()) {
        let r = err.remediation();
        let summary_empty = r.summary.is_empty();
        prop_assert!(!summary_empty, "empty summary for PatternError: {:?}", err);
        let cmds_empty = r.commands.is_empty();
        prop_assert!(!cmds_empty, "empty commands for PatternError: {:?}", err);
    }

    #[test]
    fn pattern_match_timeout_remediation(_dummy in 0..1u32) {
        let r = PatternError::MatchTimeout.remediation();
        let summary_mentions_timeout = r.summary.contains("timed out") || r.summary.contains("timeout");
        prop_assert!(summary_mentions_timeout, "MatchTimeout summary should mention timeout: {}", r.summary);
    }
}

// =============================================================================
// 11. WorkflowError remediation() non-empty for all variants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn workflow_error_remediation_nonempty(err in arb_workflow_error()) {
        let r = err.remediation();
        let summary_empty = r.summary.is_empty();
        prop_assert!(!summary_empty, "empty summary for WorkflowError: {:?}", err);
        let cmds_empty = r.commands.is_empty();
        prop_assert!(!cmds_empty, "empty commands for WorkflowError: {:?}", err);
    }

    #[test]
    fn workflow_pane_locked_remediation_mentions_locked(_dummy in 0..1u32) {
        let r = WorkflowError::PaneLocked.remediation();
        let mentions_locked = r.summary.contains("locked") || r.summary.contains("Lock");
        prop_assert!(mentions_locked, "PaneLocked summary should mention lock: {}", r.summary);
    }
}

// =============================================================================
// 12. ConfigError remediation() non-empty for all variants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    #[test]
    fn config_error_remediation_nonempty(err in arb_config_error()) {
        let r = err.remediation();
        let summary_empty = r.summary.is_empty();
        prop_assert!(!summary_empty, "empty summary for ConfigError: {:?}", err);
        let cmds_empty = r.commands.is_empty();
        prop_assert!(!cmds_empty, "empty commands for ConfigError: {:?}", err);
    }

    #[test]
    fn config_file_not_found_remediation_mentions_path(path in arb_nonempty_string()) {
        let err = ConfigError::FileNotFound(path.clone());
        let r = err.remediation();
        prop_assert!(
            r.summary.contains(&path),
            "FileNotFound summary should contain path '{}': {}", path, r.summary
        );
    }

    #[test]
    fn config_read_failed_remediation_mentions_path(
        path in arb_nonempty_string(),
        reason in arb_nonempty_string(),
    ) {
        let err = ConfigError::ReadFailed(path.clone(), reason);
        let r = err.remediation();
        prop_assert!(
            r.summary.contains(&path),
            "ReadFailed summary should contain path '{}': {}", path, r.summary
        );
    }
}

// =============================================================================
// 13. format_error_with_remediation() output structure
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn format_error_with_remediation_contains_error_prefix(err in arb_core_error()) {
        let output = format_error_with_remediation(&err);
        prop_assert!(output.starts_with("Error:"), "output should start with 'Error:': {}", output);
    }

    #[test]
    fn format_error_with_remediation_contains_error_message(err in arb_core_error()) {
        let output = format_error_with_remediation(&err);
        let display = err.to_string();
        prop_assert!(output.contains(&display), "output should contain error display text '{}': {}", display, output);
    }

    #[test]
    fn format_error_with_remediation_contains_to_fix(err in arb_core_error()) {
        let output = format_error_with_remediation(&err);
        // All variants return Some from remediation(), so "To fix:" should always be present
        prop_assert!(output.contains("To fix:"), "output should contain 'To fix:': {}", output);
    }

    #[test]
    fn format_error_with_remediation_policy(msg in arb_nonempty_string()) {
        let err = CoreError::Policy(msg.clone());
        let output = format_error_with_remediation(&err);
        prop_assert!(output.contains(&msg), "should contain policy message: {}", output);
        prop_assert!(output.contains("To fix:"), "should contain remediation: {}", output);
    }

    #[test]
    fn format_error_with_remediation_runtime(msg in arb_nonempty_string()) {
        let err = CoreError::Runtime(msg.clone());
        let output = format_error_with_remediation(&err);
        prop_assert!(output.contains(&msg), "should contain runtime message: {}", output);
    }
}

// =============================================================================
// 14. From conversions for sub-error types
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn from_wezterm_error_produces_wezterm_variant(we in arb_wezterm_error()) {
        let display_before = we.to_string();
        let err: CoreError = we.into();
        let is_wezterm = matches!(err, CoreError::Wezterm(_));
        prop_assert!(is_wezterm, "expected Wezterm variant, got {:?}", err);
        let display_after = err.to_string();
        prop_assert!(display_after.contains(&display_before), "display should contain inner: {}", display_after);
    }

    #[test]
    fn from_storage_error_produces_storage_variant(se in arb_storage_error()) {
        let display_before = se.to_string();
        let err: CoreError = se.into();
        let is_storage = matches!(err, CoreError::Storage(_));
        prop_assert!(is_storage, "expected Storage variant, got {:?}", err);
        let display_after = err.to_string();
        prop_assert!(display_after.contains(&display_before), "display should contain inner: {}", display_after);
    }

    #[test]
    fn from_pattern_error_produces_pattern_variant(pe in arb_pattern_error()) {
        let display_before = pe.to_string();
        let err: CoreError = pe.into();
        let is_pattern = matches!(err, CoreError::Pattern(_));
        prop_assert!(is_pattern, "expected Pattern variant, got {:?}", err);
        let display_after = err.to_string();
        prop_assert!(display_after.contains(&display_before), "display should contain inner: {}", display_after);
    }

    #[test]
    fn from_workflow_error_produces_workflow_variant(we in arb_workflow_error()) {
        let display_before = we.to_string();
        let err: CoreError = we.into();
        let is_workflow = matches!(err, CoreError::Workflow(_));
        prop_assert!(is_workflow, "expected Workflow variant, got {:?}", err);
        let display_after = err.to_string();
        prop_assert!(display_after.contains(&display_before), "display should contain inner: {}", display_after);
    }

    #[test]
    fn from_config_error_produces_config_variant(ce in arb_config_error()) {
        let display_before = ce.to_string();
        let err: CoreError = ce.into();
        let is_config = matches!(err, CoreError::Config(_));
        prop_assert!(is_config, "expected Config variant, got {:?}", err);
        let display_after = err.to_string();
        prop_assert!(display_after.contains(&display_before), "display should contain inner: {}", display_after);
    }

    #[test]
    fn from_io_error_produces_io_variant(msg in arb_nonempty_string()) {
        let io_err = std::io::Error::other(msg.clone());
        let err: CoreError = io_err.into();
        let is_io = matches!(err, CoreError::Io(_));
        prop_assert!(is_io, "expected Io variant, got {:?}", err);
    }
}

// =============================================================================
// Additional edge case properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn remediation_serde_json_roundtrip_empty_fields(
        summary in arb_nonempty_string(),
    ) {
        // Remediation with no commands, no alternatives, no learn_more
        let r = Remediation::new(summary.clone());
        let json = serde_json::to_string(&r).unwrap();
        let back: Remediation = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.summary, &summary, "summary roundtrip");
        prop_assert!(back.commands.is_empty(), "commands should be empty");
        prop_assert!(back.alternatives.is_empty(), "alternatives should be empty");
        prop_assert!(back.learn_more.is_none(), "learn_more should be None");
    }

    #[test]
    fn remediation_command_serde_json_is_valid_json(
        cmd in arb_remediation_command(),
    ) {
        let json = serde_json::to_string(&cmd).unwrap();
        // Should be valid JSON
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let is_object = parsed.is_object();
        prop_assert!(is_object, "serialized RemediationCommand should be a JSON object");
        let obj = parsed.as_object().unwrap();
        let has_label = obj.contains_key("label");
        prop_assert!(has_label, "should have 'label' field");
        let has_command = obj.contains_key("command");
        prop_assert!(has_command, "should have 'command' field");
    }

    #[test]
    fn error_display_is_nonempty(err in arb_core_error()) {
        let display = err.to_string();
        let is_empty = display.is_empty();
        prop_assert!(!is_empty, "Error display should never be empty");
    }

    #[test]
    fn error_debug_is_nonempty(err in arb_core_error()) {
        let debug = format!("{:?}", err);
        let is_empty = debug.is_empty();
        prop_assert!(!is_empty, "Error debug should never be empty");
    }

    #[test]
    fn wezterm_error_display_is_nonempty(err in arb_wezterm_error()) {
        let display = err.to_string();
        let is_empty = display.is_empty();
        prop_assert!(!is_empty, "WeztermError display should never be empty");
    }

    #[test]
    fn storage_error_display_is_nonempty(err in arb_storage_error()) {
        let display = err.to_string();
        let is_empty = display.is_empty();
        prop_assert!(!is_empty, "StorageError display should never be empty");
    }

    #[test]
    fn pattern_error_display_is_nonempty(err in arb_pattern_error()) {
        let display = err.to_string();
        let is_empty = display.is_empty();
        prop_assert!(!is_empty, "PatternError display should never be empty");
    }

    #[test]
    fn workflow_error_display_is_nonempty(err in arb_workflow_error()) {
        let display = err.to_string();
        let is_empty = display.is_empty();
        prop_assert!(!is_empty, "WorkflowError display should never be empty");
    }

    #[test]
    fn config_error_display_is_nonempty(err in arb_config_error()) {
        let display = err.to_string();
        let is_empty = display.is_empty();
        prop_assert!(!is_empty, "ConfigError display should never be empty");
    }
}
