//! Property-based tests for `frankenterm_core::output::error_renderer`.
//!
//! Validates:
//!  1. error_code returns FT-XXXX format for all Error variants
//!  2. error_code digit suffix is within valid category range
//!  3. error_code is deterministic (same error → same code)
//!  4. JSON render always produces valid JSON
//!  5. JSON render always has ok:false field
//!  6. JSON render always has code field matching error_code()
//!  7. JSON render always has error string field
//!  8. JSON render includes title when catalog entry exists
//!  9. JSON render includes description when catalog entry exists
//! 10. JSON render remediation has required fields when present
//! 11. Plain render always contains the error code
//! 12. Plain render always contains ft why hint
//! 13. Plain render always contains Error: header
//! 14. Plain render is non-empty for all errors
//! 15. render dispatches to JSON for Json format
//! 16. render dispatches to plain for Plain format
//! 17. render_error convenience matches ErrorRenderer::new().render()
//! 18. get_code_for_error convenience matches ErrorRenderer::error_code()
//! 19. all error codes have catalog entries
//! 20. render_error_code JSON produces valid JSON for all catalog entries
//! 21. render_error_code plain is non-empty for all catalog entries
//! 22. render_error_code JSON has code field
//! 23. render_error_code JSON has title field
//! 24. render_error_code JSON has category field
//! 25. render_error_code JSON recovery_steps is array
//! 26. render_error_code JSON causes is array
//! 27. Default renderer produces non-empty output
//! 28. ErrorCategory::from_code roundtrips for rendered codes
//! 29. JSON render output length is bounded
//! 30. Plain render output length is bounded
//! 31. Wezterm error codes are in 1000-1999 range
//! 32. Storage error codes are in 2000-2999 range
//! 33. Pattern error codes are in 3000-3999 range
//! 34. Workflow error codes are in 5000-5999 range
//! 35. Config error codes are in 7000-7999 range

use proptest::prelude::*;

use frankenterm_core::Error as CoreError;
use frankenterm_core::error::{
    ConfigError, PatternError, StorageError, WeztermError, WorkflowError,
};
use frankenterm_core::error_codes::{ErrorCategory, get_error_code};
use frankenterm_core::output::error_renderer::{ErrorRenderer, get_code_for_error, render_error};
use frankenterm_core::output::format::OutputFormat;

// =============================================================================
// Strategies
// =============================================================================

fn arb_nonempty_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9 _/.-]{1,80}"
        .prop_map(|s| s.trim().to_string())
        .prop_filter("must be non-empty", |s| !s.is_empty())
}

fn arb_u64() -> impl Strategy<Value = u64> {
    0u64..=1_000_000
}

fn arb_i32() -> impl Strategy<Value = i32> {
    -1000i32..=1000
}

fn arb_wezterm_error() -> impl Strategy<Value = WeztermError> {
    prop_oneof![
        Just(WeztermError::CliNotFound),
        Just(WeztermError::NotRunning),
        arb_u64().prop_map(WeztermError::PaneNotFound),
        arb_nonempty_string().prop_map(WeztermError::SocketNotFound),
        arb_nonempty_string().prop_map(WeztermError::CommandFailed),
        arb_nonempty_string().prop_map(WeztermError::ParseError),
        arb_u64().prop_map(WeztermError::Timeout),
        arb_u64().prop_map(|ms| WeztermError::CircuitOpen { retry_after_ms: ms }),
    ]
}

fn arb_storage_error() -> impl Strategy<Value = StorageError> {
    prop_oneof![
        arb_nonempty_string().prop_map(StorageError::Database),
        (arb_u64(), arb_u64()).prop_map(|(expected, actual)| StorageError::SequenceDiscontinuity {
            expected,
            actual,
        }),
        arb_nonempty_string().prop_map(StorageError::MigrationFailed),
        (arb_i32(), arb_i32())
            .prop_map(|(current, supported)| StorageError::SchemaTooNew { current, supported }),
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

fn arb_pattern_error() -> impl Strategy<Value = PatternError> {
    prop_oneof![
        arb_nonempty_string().prop_map(PatternError::InvalidRule),
        arb_nonempty_string().prop_map(PatternError::InvalidRegex),
        arb_nonempty_string().prop_map(PatternError::PackNotFound),
        Just(PatternError::MatchTimeout),
    ]
}

fn arb_workflow_error() -> impl Strategy<Value = WorkflowError> {
    prop_oneof![
        arb_nonempty_string().prop_map(WorkflowError::NotFound),
        arb_nonempty_string().prop_map(WorkflowError::Aborted),
        arb_nonempty_string().prop_map(WorkflowError::GuardFailed),
        Just(WorkflowError::PaneLocked),
    ]
}

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

/// All constructible CoreError variants (excluding Io and Json which
/// require special constructors not easily generated).
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

/// Non-JSON output formats for plain rendering tests.
fn arb_plain_format() -> impl Strategy<Value = OutputFormat> {
    prop_oneof![Just(OutputFormat::Auto), Just(OutputFormat::Plain),]
}

/// All output formats.
fn arb_output_format() -> impl Strategy<Value = OutputFormat> {
    prop_oneof![
        Just(OutputFormat::Auto),
        Just(OutputFormat::Plain),
        Just(OutputFormat::Json),
    ]
}

/// Helper: parse the numeric suffix from an FT-XXXX code.
fn parse_code_number(code: &str) -> u16 {
    code.strip_prefix("FT-")
        .expect("code should start with FT-")
        .parse::<u16>()
        .expect("code suffix should be numeric")
}

// =============================================================================
// 1. error_code returns FT-XXXX format for all Error variants
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn error_code_starts_with_ft_prefix(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        prop_assert!(
            code.starts_with("FT-"),
            "Error code '{}' should start with FT-", code
        );
    }
}

// =============================================================================
// 2. error_code digit suffix is all digits
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn error_code_suffix_is_all_digits(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        let suffix = code.strip_prefix("FT-").unwrap();
        prop_assert!(
            suffix.chars().all(|c| c.is_ascii_digit()),
            "Code suffix '{}' should be all digits", suffix
        );
    }
}

// =============================================================================
// 3. error_code is deterministic
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn error_code_is_deterministic(error in arb_core_error()) {
        let code1 = ErrorRenderer::error_code(&error);
        let code2 = ErrorRenderer::error_code(&error);
        prop_assert_eq!(code1, code2, "error_code should be deterministic");
    }
}

// =============================================================================
// 4. JSON render always produces valid JSON
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn json_render_is_valid_json(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&output);
        prop_assert!(parsed.is_ok(), "JSON render should produce valid JSON, got: {}", output);
    }
}

// =============================================================================
// 5. JSON render always has ok:false
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn json_render_has_ok_false(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        prop_assert_eq!(parsed["ok"], serde_json::Value::Bool(false));
    }
}

// =============================================================================
// 6. JSON render code matches error_code()
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn json_render_code_matches_error_code(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        prop_assert_eq!(
            parsed["code"].as_str().unwrap_or(""),
            code,
            "JSON code field should match error_code()"
        );
    }
}

// =============================================================================
// 7. JSON render has error string
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn json_render_has_error_string(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        prop_assert!(parsed["error"].is_string(), "JSON should have error string field");
    }
}

// =============================================================================
// 8. JSON render includes title when catalog entry exists
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn json_render_has_title_when_catalog_exists(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        if get_error_code(code).is_some() {
            prop_assert!(parsed["title"].is_string(), "Should have title for catalog code {}", code);
        }
    }
}

// =============================================================================
// 9. JSON render includes description when catalog entry exists
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn json_render_has_description_when_catalog_exists(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        if get_error_code(code).is_some() {
            prop_assert!(
                parsed["description"].is_string(),
                "Should have description for catalog code {}", code
            );
        }
    }
}

// =============================================================================
// 10. JSON render remediation has required structure when present
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn json_remediation_has_required_fields(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        if let Some(rem) = parsed.get("remediation") {
            prop_assert!(rem["summary"].is_string(), "remediation.summary should be string");
            prop_assert!(rem["commands"].is_array(), "remediation.commands should be array");
            prop_assert!(rem["alternatives"].is_array(), "remediation.alternatives should be array");
        }
    }
}

// =============================================================================
// 11. Plain render always contains the error code
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn plain_render_contains_error_code(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let output = renderer.render(&error);
        prop_assert!(
            output.contains(code),
            "Plain render should contain error code {}, got: {}", code, output
        );
    }
}

// =============================================================================
// 12. Plain render always contains ft why hint
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn plain_render_contains_ft_why_hint(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let output = renderer.render(&error);
        let expected_hint = format!("ft why {}", code);
        prop_assert!(
            output.contains(&expected_hint),
            "Plain render should contain '{}', got: {}", expected_hint, output
        );
    }
}

// =============================================================================
// 13. Plain render always contains Error: header
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn plain_render_contains_error_header(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let output = renderer.render(&error);
        prop_assert!(
            output.contains("Error:"),
            "Plain render should contain 'Error:' header, got: {}", output
        );
    }
}

// =============================================================================
// 14. Plain render is non-empty
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn plain_render_is_non_empty(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let output = renderer.render(&error);
        prop_assert!(!output.is_empty(), "Plain render should be non-empty");
    }
}

// =============================================================================
// 15. render dispatches to JSON for Json format
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn render_dispatches_json_for_json_format(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        // JSON output must parse as JSON
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&output);
        prop_assert!(parsed.is_ok(), "Json format should produce parseable JSON");
    }
}

// =============================================================================
// 16. render dispatches to plain for Plain format
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn render_dispatches_plain_for_plain_format(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let output = renderer.render(&error);
        // Plain output should NOT parse as JSON (it contains human-readable formatting)
        let parsed: Result<serde_json::Value, _> = serde_json::from_str(&output);
        prop_assert!(parsed.is_err(), "Plain format should not produce valid JSON");
    }
}

// =============================================================================
// 17. render_error convenience matches ErrorRenderer::new().render()
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn render_error_convenience_matches_renderer(
        error in arb_core_error(),
        format in arb_output_format()
    ) {
        let convenience = render_error(&error, format);
        let explicit = ErrorRenderer::new(format).render(&error);
        prop_assert_eq!(
            convenience, explicit,
            "render_error convenience should match ErrorRenderer::new().render()"
        );
    }
}

// =============================================================================
// 18. get_code_for_error convenience matches ErrorRenderer::error_code()
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn get_code_convenience_matches_error_code(error in arb_core_error()) {
        let convenience = get_code_for_error(&error);
        let explicit = ErrorRenderer::error_code(&error);
        prop_assert_eq!(
            convenience, explicit,
            "get_code_for_error should match ErrorRenderer::error_code()"
        );
    }
}

// =============================================================================
// 19. All error codes have catalog entries
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn all_error_codes_in_catalog(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        let entry = get_error_code(code);
        prop_assert!(
            entry.is_some(),
            "Error code '{}' should have a catalog entry", code
        );
    }
}

// =============================================================================
// 20. render_error_code JSON produces valid JSON for all catalog codes
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn render_error_code_json_is_valid(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        if let Some(def) = get_error_code(code) {
            let renderer = ErrorRenderer::new(OutputFormat::Json);
            let output = renderer.render_error_code(def);
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&output);
            prop_assert!(parsed.is_ok(), "render_error_code JSON should be valid for {}", code);
        }
    }
}

// =============================================================================
// 21. render_error_code plain is non-empty
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn render_error_code_plain_is_non_empty(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        if let Some(def) = get_error_code(code) {
            let renderer = ErrorRenderer::new(OutputFormat::Plain);
            let output = renderer.render_error_code(def);
            prop_assert!(!output.is_empty(), "render_error_code plain should be non-empty for {}", code);
        }
    }
}

// =============================================================================
// 22. render_error_code JSON has code field
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn render_error_code_json_has_code_field(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        if let Some(def) = get_error_code(code) {
            let renderer = ErrorRenderer::new(OutputFormat::Json);
            let output = renderer.render_error_code(def);
            let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
            prop_assert_eq!(
                parsed["code"].as_str().unwrap_or(""),
                code,
                "render_error_code JSON should have matching code field"
            );
        }
    }
}

// =============================================================================
// 23. render_error_code JSON has title field
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn render_error_code_json_has_title(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        if let Some(def) = get_error_code(code) {
            let renderer = ErrorRenderer::new(OutputFormat::Json);
            let output = renderer.render_error_code(def);
            let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
            prop_assert!(
                parsed["title"].is_string(),
                "render_error_code JSON should have title for {}", code
            );
        }
    }
}

// =============================================================================
// 24. render_error_code JSON has category field
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn render_error_code_json_has_category(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        if let Some(def) = get_error_code(code) {
            let renderer = ErrorRenderer::new(OutputFormat::Json);
            let output = renderer.render_error_code(def);
            let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
            prop_assert!(
                parsed["category"].is_string(),
                "render_error_code JSON should have category for {}", code
            );
        }
    }
}

// =============================================================================
// 25. render_error_code JSON recovery_steps is array
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn render_error_code_json_recovery_steps_is_array(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        if let Some(def) = get_error_code(code) {
            let renderer = ErrorRenderer::new(OutputFormat::Json);
            let output = renderer.render_error_code(def);
            let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
            prop_assert!(
                parsed["recovery_steps"].is_array(),
                "recovery_steps should be array for {}", code
            );
        }
    }
}

// =============================================================================
// 26. render_error_code JSON causes is array
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn render_error_code_json_causes_is_array(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        if let Some(def) = get_error_code(code) {
            let renderer = ErrorRenderer::new(OutputFormat::Json);
            let output = renderer.render_error_code(def);
            let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
            prop_assert!(
                parsed["causes"].is_array(),
                "causes should be array for {}", code
            );
        }
    }
}

// =============================================================================
// 27. Default renderer produces non-empty output
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn default_renderer_produces_output(error in arb_core_error()) {
        let renderer = ErrorRenderer::default();
        let output = renderer.render(&error);
        prop_assert!(!output.is_empty(), "Default renderer should produce non-empty output");
    }
}

// =============================================================================
// 28. ErrorCategory::from_code roundtrips for rendered codes
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn error_code_has_valid_category(error in arb_core_error()) {
        let code = ErrorRenderer::error_code(&error);
        let category = ErrorCategory::from_code(code);
        prop_assert!(
            category.is_some(),
            "Error code '{}' should map to a valid ErrorCategory", code
        );
    }
}

// =============================================================================
// 29. JSON render output length is bounded
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn json_render_length_is_bounded(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);
        // JSON output should be reasonable size (not megabytes of output)
        prop_assert!(
            output.len() < 10_000,
            "JSON render should be under 10KB, got {} bytes", output.len()
        );
    }
}

// =============================================================================
// 30. Plain render output length is bounded
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    #[test]
    fn plain_render_length_is_bounded(error in arb_core_error()) {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let output = renderer.render(&error);
        prop_assert!(
            output.len() < 10_000,
            "Plain render should be under 10KB, got {} bytes", output.len()
        );
    }
}

// =============================================================================
// 31. Wezterm error codes are in 1000-1999 range
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn wezterm_error_codes_in_range(error in arb_wezterm_error()) {
        let core_error = CoreError::Wezterm(error);
        let code = ErrorRenderer::error_code(&core_error);
        let num = parse_code_number(code);
        prop_assert!(
            (1000..=1999).contains(&num),
            "Wezterm error code {} ({}) should be in 1000-1999 range", code, num
        );
    }
}

// =============================================================================
// 32. Storage error codes are in 2000-2999 range
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn storage_error_codes_in_range(error in arb_storage_error()) {
        let core_error = CoreError::Storage(error);
        let code = ErrorRenderer::error_code(&core_error);
        let num = parse_code_number(code);
        prop_assert!(
            (2000..=2999).contains(&num),
            "Storage error code {} ({}) should be in 2000-2999 range", code, num
        );
    }
}

// =============================================================================
// 33. Pattern error codes are in 3000-3999 range
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn pattern_error_codes_in_range(error in arb_pattern_error()) {
        let core_error = CoreError::Pattern(error);
        let code = ErrorRenderer::error_code(&core_error);
        let num = parse_code_number(code);
        prop_assert!(
            (3000..=3999).contains(&num),
            "Pattern error code {} ({}) should be in 3000-3999 range", code, num
        );
    }
}

// =============================================================================
// 34. Workflow error codes are in 5000-5999 range
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn workflow_error_codes_in_range(error in arb_workflow_error()) {
        let core_error = CoreError::Workflow(error);
        let code = ErrorRenderer::error_code(&core_error);
        let num = parse_code_number(code);
        prop_assert!(
            (5000..=5999).contains(&num),
            "Workflow error code {} ({}) should be in 5000-5999 range", code, num
        );
    }
}

// =============================================================================
// 35. Config error codes are in 7000-7999 range
// =============================================================================
proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn config_error_codes_in_range(error in arb_config_error()) {
        let core_error = CoreError::Config(error);
        let code = ErrorRenderer::error_code(&core_error);
        let num = parse_code_number(code);
        prop_assert!(
            (7000..=7999).contains(&num),
            "Config error code {} ({}) should be in 7000-7999 range", code, num
        );
    }
}
