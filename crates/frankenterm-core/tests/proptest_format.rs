//! Property-based tests for `frankenterm_core::output::format`.
//!
//! Validates:
//!  1. OutputFormat::parse succeeds for known format strings
//!  2. OutputFormat::parse returns None for unknown strings
//!  3. FromStr/Display round-trip: parse(display(f)) == f for all variants
//!  4. Display produces lowercase output for all variants
//!  5. Display output is non-empty for all variants
//!  6. Json.is_json() is always true
//!  7. Plain/Auto.is_json() is always false
//!  8. Plain.is_plain() is always true
//!  9. Json.is_plain() is always false
//! 10. Plain.is_rich() is always false
//! 11. Json.is_rich() is always false
//! 12. OutputFormat::default() is Auto
//! 13. EffectiveFormat for Plain is EffectiveFormat::Plain
//! 14. EffectiveFormat for Json is EffectiveFormat::Json
//! 15. EffectiveFormat for Auto is either Rich or Plain (never Json)
//! 16. Clone/Copy semantics: cloned value equals original
//! 17. Style disabled: apply returns text unchanged for any code+text
//! 18. Style enabled: apply wraps text with given code and RESET suffix
//! 19. Style enabled: bold contains BOLD code
//! 20. Style enabled: red contains RED code
//! 21. Style enabled: green contains GREEN code
//! 22. Style enabled: yellow contains YELLOW code
//! 23. Style enabled: cyan contains CYAN code
//! 24. Style enabled: dim contains DIM code
//! 25. Style enabled: gray contains GRAY code
//! 26. Style disabled: all color methods return original text
//! 27. Style::severity maps critical/error to red
//! 28. Style::severity maps warning/warn to yellow
//! 29. Style::severity maps info to cyan
//! 30. Style::severity returns unmodified text for unknown severity
//! 31. Style::status success → green, failure → red
//! 32. Style::from_format(Plain) produces disabled style
//! 33. Style::from_format(Json) produces disabled style
//! 34. parse is case-insensitive: parse(s) == parse(s.to_uppercase())
//! 35. "text" is an alias for Plain

use proptest::prelude::*;

use frankenterm_core::output::{EffectiveFormat, OutputFormat, Style, colors};

// =============================================================================
// Strategies
// =============================================================================

fn arb_output_format() -> impl Strategy<Value = OutputFormat> {
    prop_oneof![
        Just(OutputFormat::Auto),
        Just(OutputFormat::Plain),
        Just(OutputFormat::Json),
    ]
}

fn arb_known_format_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("auto".to_string()),
        Just("plain".to_string()),
        Just("text".to_string()),
        Just("json".to_string()),
        Just("AUTO".to_string()),
        Just("PLAIN".to_string()),
        Just("TEXT".to_string()),
        Just("JSON".to_string()),
        Just("Auto".to_string()),
        Just("Plain".to_string()),
        Just("Json".to_string()),
    ]
}

fn arb_unknown_format_string() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("xml".to_string()),
        Just("csv".to_string()),
        Just("yaml".to_string()),
        Just("rich".to_string()),
        Just("binary".to_string()),
        Just("markdown".to_string()),
        Just("".to_string()),
        Just("   ".to_string()),
        Just("jsons".to_string()),
        Just("plaintxt".to_string()),
    ]
}

fn arb_nonempty_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _/.-]{1,80}"
        .prop_map(|s| s.trim().to_string())
        .prop_filter("must be non-empty", |s| !s.is_empty())
}

fn arb_ansi_code() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(colors::BOLD),
        Just(colors::DIM),
        Just(colors::ITALIC),
        Just(colors::UNDERLINE),
        Just(colors::RED),
        Just(colors::GREEN),
        Just(colors::YELLOW),
        Just(colors::BLUE),
        Just(colors::MAGENTA),
        Just(colors::CYAN),
        Just(colors::WHITE),
        Just(colors::GRAY),
        Just(colors::BRIGHT_RED),
        Just(colors::BRIGHT_GREEN),
        Just(colors::BRIGHT_YELLOW),
        Just(colors::BRIGHT_BLUE),
        Just(colors::BRIGHT_CYAN),
    ]
}

fn arb_severity_red() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("critical".to_string()),
        Just("error".to_string()),
        Just("CRITICAL".to_string()),
        Just("ERROR".to_string()),
        Just("Critical".to_string()),
        Just("Error".to_string()),
    ]
}

fn arb_severity_yellow() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("warning".to_string()),
        Just("warn".to_string()),
        Just("WARNING".to_string()),
        Just("WARN".to_string()),
        Just("Warning".to_string()),
        Just("Warn".to_string()),
    ]
}

fn arb_severity_cyan() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("info".to_string()),
        Just("INFO".to_string()),
        Just("Info".to_string()),
    ]
}

fn arb_severity_unknown() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("debug".to_string()),
        Just("trace".to_string()),
        Just("notice".to_string()),
        Just("verbose".to_string()),
        Just("unknown".to_string()),
    ]
}

// =============================================================================
// 1. parse succeeds for known format strings
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn parse_succeeds_for_known_strings(s in arb_known_format_string()) {
        let result = OutputFormat::parse(&s);
        prop_assert!(result.is_some(), "parse({}) should succeed", s);
    }
}

// =============================================================================
// 2. parse returns None for unknown strings
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn parse_returns_none_for_unknown_strings(s in arb_unknown_format_string()) {
        let result = OutputFormat::parse(&s);
        prop_assert!(result.is_none(), "parse({}) should return None", s);
    }
}

// =============================================================================
// 3. FromStr/Display round-trip
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn display_parse_roundtrip(format in arb_output_format()) {
        let displayed = format.to_string();
        let parsed = OutputFormat::parse(&displayed);
        prop_assert_eq!(parsed, Some(format), "round-trip failed for {:?}", format);
    }
}

// =============================================================================
// 4. Display produces lowercase
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn display_is_lowercase(format in arb_output_format()) {
        let displayed = format.to_string();
        let lower = displayed.to_lowercase();
        prop_assert_eq!(
            displayed, lower,
            "Display should produce lowercase"
        );
    }
}

// =============================================================================
// 5. Display output is non-empty
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn display_is_non_empty(format in arb_output_format()) {
        let displayed = format.to_string();
        prop_assert!(!displayed.is_empty(), "Display should be non-empty");
    }
}

// =============================================================================
// 6. Json.is_json() is always true
// =============================================================================
#[test]
fn json_is_json_always_true() {
    assert!(OutputFormat::Json.is_json());
}

// =============================================================================
// 7. Plain/Auto.is_json() is always false
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn non_json_formats_not_json(format in prop_oneof![Just(OutputFormat::Auto), Just(OutputFormat::Plain)]) {
        prop_assert!(!format.is_json(), "{:?}.is_json() should be false", format);
    }
}

// =============================================================================
// 8. Plain.is_plain() is always true
// =============================================================================
#[test]
fn plain_is_plain_always_true() {
    assert!(OutputFormat::Plain.is_plain());
}

// =============================================================================
// 9. Json.is_plain() is always false
// =============================================================================
#[test]
fn json_is_not_plain() {
    assert!(!OutputFormat::Json.is_plain());
}

// =============================================================================
// 10. Plain.is_rich() is always false
// =============================================================================
#[test]
fn plain_is_not_rich() {
    assert!(!OutputFormat::Plain.is_rich());
}

// =============================================================================
// 11. Json.is_rich() is always false
// =============================================================================
#[test]
fn json_is_not_rich() {
    assert!(!OutputFormat::Json.is_rich());
}

// =============================================================================
// 12. Default is Auto
// =============================================================================
#[test]
fn default_is_auto() {
    assert_eq!(OutputFormat::default(), OutputFormat::Auto);
}

// =============================================================================
// 13. EffectiveFormat for Plain
// =============================================================================
#[test]
fn effective_plain_is_plain() {
    assert_eq!(OutputFormat::Plain.effective(), EffectiveFormat::Plain);
}

// =============================================================================
// 14. EffectiveFormat for Json
// =============================================================================
#[test]
fn effective_json_is_json() {
    assert_eq!(OutputFormat::Json.effective(), EffectiveFormat::Json);
}

// =============================================================================
// 15. EffectiveFormat for Auto is not Json
// =============================================================================
#[test]
fn effective_auto_is_never_json() {
    let eff = OutputFormat::Auto.effective();
    assert_ne!(
        eff,
        EffectiveFormat::Json,
        "Auto should never resolve to Json"
    );
}

// =============================================================================
// 16. Clone/Copy semantics
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn clone_copy_eq(format in arb_output_format()) {
        let cloned = format;
        prop_assert_eq!(format, cloned, "Copy should preserve value");
    }
}

// =============================================================================
// 17. Style disabled: apply returns text unchanged
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn style_disabled_apply_passthrough(
        code in arb_ansi_code(),
        text in arb_nonempty_string()
    ) {
        let style = Style::new(false);
        let result = style.apply(code, &text);
        prop_assert_eq!(result, text, "disabled style should pass through text");
    }
}

// =============================================================================
// 18. Style enabled: apply wraps with code and RESET
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn style_enabled_apply_wraps(
        code in arb_ansi_code(),
        text in arb_nonempty_string()
    ) {
        let style = Style::new(true);
        let result = style.apply(code, &text);
        prop_assert!(
            result.starts_with(code),
            "enabled apply should start with code"
        );
        prop_assert!(
            result.ends_with(colors::RESET),
            "enabled apply should end with RESET"
        );
        prop_assert!(
            result.contains(&text),
            "enabled apply should contain original text"
        );
    }
}

// =============================================================================
// 19. Style enabled: bold contains BOLD code
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn style_enabled_bold_has_bold_code(text in arb_nonempty_string()) {
        let style = Style::new(true);
        let result = style.bold(&text);
        prop_assert!(result.contains(colors::BOLD), "bold should contain BOLD code");
    }
}

// =============================================================================
// 20. Style enabled: red contains RED code
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn style_enabled_red_has_red_code(text in arb_nonempty_string()) {
        let style = Style::new(true);
        let result = style.red(&text);
        prop_assert!(result.contains(colors::RED), "red should contain RED code");
    }
}

// =============================================================================
// 21. Style enabled: green contains GREEN code
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn style_enabled_green_has_green_code(text in arb_nonempty_string()) {
        let style = Style::new(true);
        let result = style.green(&text);
        prop_assert!(result.contains(colors::GREEN), "green should contain GREEN code");
    }
}

// =============================================================================
// 22. Style enabled: yellow contains YELLOW code
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn style_enabled_yellow_has_yellow_code(text in arb_nonempty_string()) {
        let style = Style::new(true);
        let result = style.yellow(&text);
        prop_assert!(result.contains(colors::YELLOW), "yellow should contain YELLOW code");
    }
}

// =============================================================================
// 23. Style enabled: cyan contains CYAN code
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn style_enabled_cyan_has_cyan_code(text in arb_nonempty_string()) {
        let style = Style::new(true);
        let result = style.cyan(&text);
        prop_assert!(result.contains(colors::CYAN), "cyan should contain CYAN code");
    }
}

// =============================================================================
// 24. Style enabled: dim contains DIM code
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn style_enabled_dim_has_dim_code(text in arb_nonempty_string()) {
        let style = Style::new(true);
        let result = style.dim(&text);
        prop_assert!(result.contains(colors::DIM), "dim should contain DIM code");
    }
}

// =============================================================================
// 25. Style enabled: gray contains GRAY code
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn style_enabled_gray_has_gray_code(text in arb_nonempty_string()) {
        let style = Style::new(true);
        let result = style.gray(&text);
        prop_assert!(result.contains(colors::GRAY), "gray should contain GRAY code");
    }
}

// =============================================================================
// 26. Style disabled: all color methods return original text
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn style_disabled_all_colors_passthrough(text in arb_nonempty_string()) {
        let style = Style::new(false);
        prop_assert_eq!(style.bold(&text), text.as_str());
        prop_assert_eq!(style.dim(&text), text.as_str());
        prop_assert_eq!(style.red(&text), text.as_str());
        prop_assert_eq!(style.green(&text), text.as_str());
        prop_assert_eq!(style.yellow(&text), text.as_str());
        prop_assert_eq!(style.blue(&text), text.as_str());
        prop_assert_eq!(style.cyan(&text), text.as_str());
        prop_assert_eq!(style.gray(&text), text.as_str());
    }
}

// =============================================================================
// 27. severity maps critical/error to red
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn severity_red_levels(
        text in arb_nonempty_string(),
        severity in arb_severity_red()
    ) {
        let style = Style::new(true);
        let result = style.severity(&text, &severity);
        prop_assert!(
            result.contains(colors::RED),
            "severity '{}' should produce red output", severity
        );
    }
}

// =============================================================================
// 28. severity maps warning/warn to yellow
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn severity_yellow_levels(
        text in arb_nonempty_string(),
        severity in arb_severity_yellow()
    ) {
        let style = Style::new(true);
        let result = style.severity(&text, &severity);
        prop_assert!(
            result.contains(colors::YELLOW),
            "severity '{}' should produce yellow output", severity
        );
    }
}

// =============================================================================
// 29. severity maps info to cyan
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn severity_info_is_cyan(
        text in arb_nonempty_string(),
        severity in arb_severity_cyan()
    ) {
        let style = Style::new(true);
        let result = style.severity(&text, &severity);
        prop_assert!(
            result.contains(colors::CYAN),
            "severity '{}' should produce cyan output", severity
        );
    }
}

// =============================================================================
// 30. severity returns unmodified text for unknown severity
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn severity_unknown_returns_text(
        text in arb_nonempty_string(),
        severity in arb_severity_unknown()
    ) {
        let style = Style::new(true);
        let result = style.severity(&text, &severity);
        prop_assert_eq!(
            result, text,
            "unknown severity '{}' should return text unchanged", severity
        );
    }
}

// =============================================================================
// 31. status: success → green, failure → red
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn status_colors(text in arb_nonempty_string(), success in proptest::bool::ANY) {
        let style = Style::new(true);
        let result = style.status(&text, success);
        if success {
            prop_assert!(
                result.contains(colors::GREEN),
                "success status should be green"
            );
        } else {
            prop_assert!(
                result.contains(colors::RED),
                "failure status should be red"
            );
        }
    }
}

// =============================================================================
// 32. from_format(Plain) produces disabled style
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn from_format_plain_is_disabled(text in arb_nonempty_string()) {
        let style = Style::from_format(OutputFormat::Plain);
        prop_assert_eq!(
            style.bold(&text), text,
            "Style::from_format(Plain) should be disabled"
        );
    }
}

// =============================================================================
// 33. from_format(Json) produces disabled style
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn from_format_json_is_disabled(text in arb_nonempty_string()) {
        let style = Style::from_format(OutputFormat::Json);
        prop_assert_eq!(
            style.red(&text), text,
            "Style::from_format(Json) should be disabled"
        );
    }
}

// =============================================================================
// 34. parse is case-insensitive
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn parse_case_insensitive(s in arb_known_format_string()) {
        let lower = OutputFormat::parse(&s.to_lowercase());
        let upper = OutputFormat::parse(&s.to_uppercase());
        prop_assert_eq!(
            lower, upper,
            "parse should be case-insensitive for '{}'", s
        );
    }
}

// =============================================================================
// 35. "text" is alias for Plain
// =============================================================================
#[test]
fn text_alias_for_plain() {
    assert_eq!(OutputFormat::parse("text"), Some(OutputFormat::Plain));
    assert_eq!(OutputFormat::parse("TEXT"), Some(OutputFormat::Plain));
    assert_eq!(OutputFormat::parse("Text"), Some(OutputFormat::Plain));
}
