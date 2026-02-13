//! Property-based tests for the desktop_notify module.
//!
//! Tests cover: Urgency enum (serde, Display), NotifyBackend enum (serde, Display),
//! DesktopNotifyConfig (serde, Default), build_command dispatch and escaping safety,
//! severity_to_urgency mapping, DesktopNotifier is_available logic, and
//! cross-module integration properties.

use proptest::prelude::*;

use frankenterm_core::desktop_notify::{
    DesktopNotifier, DesktopNotifyConfig, NotifyBackend, Urgency, build_command,
    severity_to_urgency,
};
use frankenterm_core::patterns::Severity;

// ============================================================================
// Strategies
// ============================================================================

fn arb_urgency() -> impl Strategy<Value = Urgency> {
    prop_oneof![
        Just(Urgency::Low),
        Just(Urgency::Normal),
        Just(Urgency::Critical),
    ]
}

fn arb_notify_backend() -> impl Strategy<Value = NotifyBackend> {
    prop_oneof![
        Just(NotifyBackend::MacOs),
        Just(NotifyBackend::Linux),
        Just(NotifyBackend::Windows),
        Just(NotifyBackend::None),
    ]
}

fn arb_severity() -> impl Strategy<Value = Severity> {
    prop_oneof![
        Just(Severity::Info),
        Just(Severity::Warning),
        Just(Severity::Critical),
    ]
}

fn arb_desktop_notify_config() -> impl Strategy<Value = DesktopNotifyConfig> {
    (any::<bool>(), any::<bool>())
        .prop_map(|(enabled, sound)| DesktopNotifyConfig { enabled, sound })
}

// ============================================================================
// Urgency properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Urgency serde roundtrip
    #[test]
    fn prop_urgency_serde_roundtrip(u in arb_urgency()) {
        let json = serde_json::to_string(&u).unwrap();
        let decoded: Urgency = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(u, decoded);
    }

    /// Urgency serializes to lowercase
    #[test]
    fn prop_urgency_serde_lowercase(u in arb_urgency()) {
        let json = serde_json::to_string(&u).unwrap();
        let s = json.trim_matches('"');
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase()),
            "Expected lowercase, got: {}", s
        );
    }

    /// Urgency Display produces lowercase
    #[test]
    fn prop_urgency_display_lowercase(u in arb_urgency()) {
        let display = format!("{}", u);
        prop_assert!(
            display.chars().all(|c| c.is_ascii_lowercase()),
            "Expected lowercase, got: {}", display
        );
    }

    /// Urgency Display matches serde serialization (without quotes)
    #[test]
    fn prop_urgency_display_matches_serde(u in arb_urgency()) {
        let display = format!("{}", u);
        let json = serde_json::to_string(&u).unwrap();
        let serde_str = json.trim_matches('"');
        prop_assert_eq!(display, serde_str);
    }

    /// All 3 Urgency variants have distinct Display values
    #[test]
    fn prop_urgency_display_distinct(_dummy in 0..1u8) {
        let displays: Vec<String> = [Urgency::Low, Urgency::Normal, Urgency::Critical]
            .iter()
            .map(|u| format!("{}", u))
            .collect();
        let mut uniq = displays.clone();
        uniq.sort();
        uniq.dedup();
        prop_assert_eq!(displays.len(), uniq.len());
    }
}

// ============================================================================
// NotifyBackend properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// NotifyBackend serde roundtrip
    #[test]
    fn prop_notify_backend_serde_roundtrip(b in arb_notify_backend()) {
        let json = serde_json::to_string(&b).unwrap();
        let decoded: NotifyBackend = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(b, decoded);
    }

    /// NotifyBackend serializes to snake_case
    #[test]
    fn prop_notify_backend_serde_snake_case(b in arb_notify_backend()) {
        let json = serde_json::to_string(&b).unwrap();
        let s = json.trim_matches('"');
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "Expected snake_case, got: {}", s
        );
    }

    /// NotifyBackend Display is non-empty
    #[test]
    fn prop_notify_backend_display_nonempty(b in arb_notify_backend()) {
        let display = format!("{}", b);
        prop_assert!(!display.is_empty());
    }

    /// All 4 NotifyBackend variants have distinct Display values
    #[test]
    fn prop_notify_backend_display_distinct(_dummy in 0..1u8) {
        let displays: Vec<String> = [
            NotifyBackend::MacOs,
            NotifyBackend::Linux,
            NotifyBackend::Windows,
            NotifyBackend::None,
        ]
            .iter()
            .map(|b| format!("{}", b))
            .collect();
        let mut uniq = displays.clone();
        uniq.sort();
        uniq.dedup();
        prop_assert_eq!(displays.len(), uniq.len());
    }

    /// All 4 NotifyBackend variants have distinct serde values
    #[test]
    fn prop_notify_backend_serde_distinct(_dummy in 0..1u8) {
        let jsons: Vec<String> = [
            NotifyBackend::MacOs,
            NotifyBackend::Linux,
            NotifyBackend::Windows,
            NotifyBackend::None,
        ]
            .iter()
            .map(|b| serde_json::to_string(b).unwrap())
            .collect();
        let mut uniq = jsons.clone();
        uniq.sort();
        uniq.dedup();
        prop_assert_eq!(jsons.len(), uniq.len());
    }

    /// detect() returns a non-None on known platforms
    #[test]
    fn prop_notify_backend_detect(_dummy in 0..1u8) {
        let detected = NotifyBackend::detect();
        // On macOS, Linux, Windows it should be non-None
        if cfg!(target_os = "macos") || cfg!(target_os = "linux") || cfg!(target_os = "windows") {
            prop_assert_ne!(detected, NotifyBackend::None);
        }
    }
}

// ============================================================================
// DesktopNotifyConfig properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// DesktopNotifyConfig serde roundtrip
    #[test]
    fn prop_config_serde_roundtrip(config in arb_desktop_notify_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let decoded: DesktopNotifyConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.enabled, decoded.enabled);
        prop_assert_eq!(config.sound, decoded.sound);
    }

    /// DesktopNotifyConfig default is disabled, no sound
    #[test]
    fn prop_config_default(_dummy in 0..1u8) {
        let config = DesktopNotifyConfig::default();
        prop_assert!(!config.enabled);
        prop_assert!(!config.sound);
    }

    /// Empty JSON object deserializes to defaults (serde(default))
    #[test]
    fn prop_config_empty_json_defaults(_dummy in 0..1u8) {
        let config: DesktopNotifyConfig = serde_json::from_str("{}").unwrap();
        prop_assert!(!config.enabled);
        prop_assert!(!config.sound);
    }

    /// Partial JSON fills missing fields from defaults
    #[test]
    fn prop_config_partial_json(enabled in any::<bool>()) {
        let json = format!(r#"{{"enabled":{}}}"#, enabled);
        let config: DesktopNotifyConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.enabled, enabled);
        prop_assert!(!config.sound); // default
    }
}

// ============================================================================
// build_command properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// None backend always returns None
    #[test]
    fn prop_build_command_none_backend(
        title in ".*",
        body in ".*",
        urgency in arb_urgency(),
        sound in any::<bool>(),
    ) {
        let result = build_command(NotifyBackend::None, &title, &body, urgency, sound);
        prop_assert!(result.is_none());
    }

    /// MacOs backend always returns Some with osascript program
    #[test]
    fn prop_build_command_macos(
        title in "[a-zA-Z0-9 ]{1,30}",
        body in "[a-zA-Z0-9 ]{1,30}",
        urgency in arb_urgency(),
        sound in any::<bool>(),
    ) {
        let cmd = build_command(NotifyBackend::MacOs, &title, &body, urgency, sound).unwrap();
        prop_assert_eq!(&cmd.program, "osascript");
        prop_assert_eq!(cmd.args.len(), 2);
        prop_assert_eq!(&cmd.args[0], "-e");
        prop_assert!(cmd.args[1].contains("display notification"));
    }

    /// Linux backend always returns Some with notify-send program
    #[test]
    fn prop_build_command_linux(
        title in "[a-zA-Z0-9 ]{1,30}",
        body in "[a-zA-Z0-9 ]{1,30}",
        urgency in arb_urgency(),
    ) {
        let cmd = build_command(NotifyBackend::Linux, &title, &body, urgency, false).unwrap();
        prop_assert_eq!(&cmd.program, "notify-send");
        prop_assert!(cmd.args.contains(&"--app-name=wa".to_string()));
    }

    /// Linux backend urgency flag matches the Urgency value
    #[test]
    fn prop_build_command_linux_urgency(urgency in arb_urgency()) {
        let cmd = build_command(NotifyBackend::Linux, "t", "b", urgency, false).unwrap();
        let expected_flag = format!("--urgency={}", urgency);
        prop_assert!(
            cmd.args.contains(&expected_flag),
            "Expected urgency flag '{}' in args", expected_flag
        );
    }

    /// Windows backend always returns Some with powershell program
    #[test]
    fn prop_build_command_windows(
        title in "[a-zA-Z0-9 ]{1,30}",
        body in "[a-zA-Z0-9 ]{1,30}",
        urgency in arb_urgency(),
    ) {
        let cmd = build_command(NotifyBackend::Windows, &title, &body, urgency, false).unwrap();
        prop_assert_eq!(&cmd.program, "powershell");
        prop_assert_eq!(&cmd.args[0], "-Command");
        prop_assert!(cmd.args[1].contains("ToastNotification"));
    }

    /// macOS build_command with sound includes "sound name"
    #[test]
    fn prop_build_command_macos_sound(
        title in "[a-zA-Z0-9]{1,10}",
        body in "[a-zA-Z0-9]{1,10}",
    ) {
        let cmd_sound = build_command(NotifyBackend::MacOs, &title, &body, Urgency::Normal, true).unwrap();
        prop_assert!(cmd_sound.args[1].contains("sound name"));

        let cmd_no_sound = build_command(NotifyBackend::MacOs, &title, &body, Urgency::Normal, false).unwrap();
        prop_assert!(!cmd_no_sound.args[1].contains("sound name"));
    }

    /// macOS command escapes double quotes in title/body
    #[test]
    fn prop_build_command_macos_escapes_quotes(
        prefix in "[a-z]{1,5}",
        suffix in "[a-z]{1,5}",
    ) {
        let title = format!("{}\"{}\"", prefix, suffix);
        let body = format!("{}\"test\"", prefix);
        let cmd = build_command(NotifyBackend::MacOs, &title, &body, Urgency::Normal, false).unwrap();
        let script = &cmd.args[1];
        // The script should not contain unescaped double quotes from input
        // (escaped quotes become \")
        // Count quotes in script: display notification "BODY" with title "TITLE"
        // The structural quotes are there, but input quotes should be escaped
        prop_assert!(
            !script.contains(&format!("\"{}\"{}\"", prefix, suffix)),
            "Raw unescaped quotes found in script"
        );
    }

    /// macOS command escapes backslashes
    #[test]
    fn prop_build_command_macos_escapes_backslash(
        prefix in "[a-z]{1,5}",
    ) {
        let title = format!("{}\\test", prefix);
        let cmd = build_command(NotifyBackend::MacOs, &title, "body", Urgency::Normal, false).unwrap();
        let script = &cmd.args[1];
        // Backslashes should be doubled in the script
        prop_assert!(
            script.contains("\\\\test"),
            "Backslash not escaped in script: {}", script
        );
    }

    /// Windows command escapes single quotes
    #[test]
    fn prop_build_command_windows_escapes_single_quotes(
        prefix in "[a-z]{1,5}",
    ) {
        let title = format!("{}'s test", prefix);
        let cmd = build_command(NotifyBackend::Windows, &title, "body", Urgency::Normal, false).unwrap();
        let script = &cmd.args[1];
        // Single quotes should be doubled
        prop_assert!(
            script.contains("''s"),
            "Single quote not escaped: {}", script
        );
    }
}

// ============================================================================
// severity_to_urgency properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// severity_to_urgency is deterministic
    #[test]
    fn prop_severity_to_urgency_deterministic(sev in arb_severity()) {
        let a = severity_to_urgency(sev);
        let b = severity_to_urgency(sev);
        prop_assert_eq!(a, b);
    }

    /// Info maps to Low, Warning to Normal, Critical to Critical
    #[test]
    fn prop_severity_to_urgency_mapping(_dummy in 0..1u8) {
        prop_assert_eq!(severity_to_urgency(Severity::Info), Urgency::Low);
        prop_assert_eq!(severity_to_urgency(Severity::Warning), Urgency::Normal);
        prop_assert_eq!(severity_to_urgency(Severity::Critical), Urgency::Critical);
    }

    /// All severities produce valid urgencies
    #[test]
    fn prop_severity_to_urgency_valid(sev in arb_severity()) {
        let u = severity_to_urgency(sev);
        // Urgency serde should succeed
        let json = serde_json::to_string(&u).unwrap();
        let _decoded: Urgency = serde_json::from_str(&json).unwrap();
    }
}

// ============================================================================
// DesktopNotifier properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// is_available requires both enabled=true and backend != None
    #[test]
    fn prop_notifier_is_available(
        backend in arb_notify_backend(),
        config in arb_desktop_notify_config(),
    ) {
        let notifier = DesktopNotifier::with_backend(backend, config.clone());
        let expected = config.enabled && backend != NotifyBackend::None;
        prop_assert_eq!(notifier.is_available(), expected);
    }

    /// backend() returns the backend passed to with_backend
    #[test]
    fn prop_notifier_backend_accessor(backend in arb_notify_backend()) {
        let notifier = DesktopNotifier::with_backend(backend, DesktopNotifyConfig::default());
        prop_assert_eq!(notifier.backend(), backend);
    }

    /// Disabled notifier's notify_message returns success=false
    #[test]
    fn prop_notifier_disabled_returns_error(
        backend in arb_notify_backend(),
        title in "[a-z]{1,10}",
        body in "[a-z]{1,10}",
        urgency in arb_urgency(),
    ) {
        let config = DesktopNotifyConfig {
            enabled: false,
            sound: false,
        };
        let notifier = DesktopNotifier::with_backend(backend, config);
        let result = notifier.notify_message(&title, &body, urgency);
        prop_assert!(!result.success);
        prop_assert!(result.error.is_some());
    }

    /// None backend with enabled config returns error about no backend
    #[test]
    fn prop_notifier_none_backend_error(
        title in "[a-z]{1,10}",
        body in "[a-z]{1,10}",
        urgency in arb_urgency(),
    ) {
        let config = DesktopNotifyConfig {
            enabled: true,
            sound: false,
        };
        let notifier = DesktopNotifier::with_backend(NotifyBackend::None, config);
        let result = notifier.notify_message(&title, &body, urgency);
        prop_assert!(!result.success);
        prop_assert!(
            result.error.as_ref().unwrap().contains("no notification backend"),
            "Expected 'no notification backend' error, got: {:?}", result.error
        );
    }
}

// ============================================================================
// Cross-module / integration properties
// ============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// build_command for all non-None backends produces a command with non-empty program
    #[test]
    fn prop_build_command_nonempty_program(
        title in "[a-z]{1,10}",
        body in "[a-z]{1,10}",
        urgency in arb_urgency(),
        sound in any::<bool>(),
    ) {
        for backend in &[NotifyBackend::MacOs, NotifyBackend::Linux, NotifyBackend::Windows] {
            let cmd = build_command(*backend, &title, &body, urgency, sound).unwrap();
            prop_assert!(!cmd.program.is_empty());
            prop_assert!(!cmd.args.is_empty());
        }
    }

    /// build_command for all non-None backends includes title and body in output
    #[test]
    fn prop_build_command_includes_content(
        title in "[a-z]{3,10}",
        body in "[a-z]{3,10}",
    ) {
        for backend in &[NotifyBackend::MacOs, NotifyBackend::Linux, NotifyBackend::Windows] {
            let cmd = build_command(*backend, &title, &body, Urgency::Normal, false).unwrap();
            let all_args = cmd.args.join(" ");
            prop_assert!(
                all_args.contains(&title),
                "Title '{}' not found in args for {:?}: {}", title, backend, all_args
            );
            prop_assert!(
                all_args.contains(&body),
                "Body '{}' not found in args for {:?}: {}", body, backend, all_args
            );
        }
    }

    /// severity_to_urgency -> serde -> deserialize roundtrip works
    #[test]
    fn prop_severity_urgency_serde_pipeline(sev in arb_severity()) {
        let urgency = severity_to_urgency(sev);
        let json = serde_json::to_string(&urgency).unwrap();
        let decoded: Urgency = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(urgency, decoded);
    }
}
