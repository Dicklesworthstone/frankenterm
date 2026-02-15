//! Error renderer for human-readable CLI error output
//!
//! Bridges the error_codes catalog with error rendering,
//! producing rich error messages with error codes, descriptions,
//! and actionable suggestions.

use super::format::{OutputFormat, Style};
use crate::error::{
    ConfigError, Error, PatternError, Remediation, StorageError, WeztermError, WorkflowError,
};
use crate::error_codes::{ErrorCodeDef, get_error_code};

/// Renderer for CLI error output
pub struct ErrorRenderer {
    format: OutputFormat,
}

impl Default for ErrorRenderer {
    fn default() -> Self {
        Self::new(OutputFormat::Auto)
    }
}

impl ErrorRenderer {
    /// Create a new error renderer with the specified format
    #[must_use]
    pub fn new(format: OutputFormat) -> Self {
        Self { format }
    }

    /// Map an Error to its error code
    #[must_use]
    pub fn error_code(error: &Error) -> &'static str {
        match error {
            Error::Wezterm(e) => match e {
                WeztermError::CliNotFound => "FT-1001",
                WeztermError::NotRunning => "FT-1002",
                WeztermError::PaneNotFound(_) => "FT-1010",
                WeztermError::SocketNotFound(_) => "FT-1003",
                WeztermError::CommandFailed(_) => "FT-1020",
                WeztermError::ParseError(_) => "FT-1021",
                WeztermError::Timeout(_) => "FT-1022",
                WeztermError::CircuitOpen { .. } => "FT-1030",
            },
            Error::Storage(e) => match e {
                StorageError::Database(_) => "FT-2001",
                StorageError::SequenceDiscontinuity { .. } => "FT-2010",
                StorageError::MigrationFailed(_) => "FT-2002",
                StorageError::SchemaTooNew { .. } => "FT-2003",
                StorageError::WaTooOld { .. } => "FT-2004",
                StorageError::FtsQueryError(_) => "FT-2020",
                StorageError::Corruption { .. } => "FT-2030",
                StorageError::NotFound(_) => "FT-2040",
            },
            Error::Pattern(e) => match e {
                PatternError::InvalidRule(_) => "FT-3001",
                PatternError::InvalidRegex(_) => "FT-3002",
                PatternError::PackNotFound(_) => "FT-3010",
                PatternError::MatchTimeout => "FT-3020",
            },
            Error::Workflow(e) => match e {
                WorkflowError::NotFound(_) => "FT-5001",
                WorkflowError::Aborted(_) => "FT-5010",
                WorkflowError::GuardFailed(_) => "FT-5020",
                WorkflowError::PaneLocked => "FT-5030",
            },
            Error::Config(e) => match e {
                ConfigError::FileNotFound(_) => "FT-7001",
                ConfigError::ReadFailed(_, _) => "FT-7002",
                ConfigError::ParseError(_) | ConfigError::ParseFailed(_) => "FT-7003",
                ConfigError::SerializeFailed(_) => "FT-7004",
                ConfigError::ValidationError(_) => "FT-7010",
            },
            Error::Policy(_) => "FT-4001",
            Error::Io(_) => "FT-9002",
            Error::Json(_) => "FT-9003",
            Error::Runtime(_) => "FT-9001",
            Error::SetupError(_) => "FT-6001",
            Error::Cancelled(_) => "FT-9004",
            Error::Panicked(_) => "FT-9005",
        }
    }

    /// Render an error for CLI output
    #[must_use]
    pub fn render(&self, error: &Error) -> String {
        if self.format.is_json() {
            return Self::render_json(error);
        }
        self.render_plain(error)
    }

    /// Render error as JSON
    fn render_json(error: &Error) -> String {
        let code = Self::error_code(error);
        let code_def = get_error_code(code);

        let mut obj = serde_json::json!({
            "ok": false,
            "error": error.to_string(),
            "code": code,
        });

        if let Some(def) = code_def {
            obj["title"] = serde_json::json!(def.title);
            obj["description"] = serde_json::json!(def.description);
            obj["category"] = serde_json::json!(format!("{:?}", def.category));
        }

        if let Some(remediation) = error.remediation() {
            obj["remediation"] = serde_json::json!({
                "summary": remediation.summary,
                "commands": remediation.commands.iter().map(|c| {
                    serde_json::json!({
                        "label": c.label,
                        "command": c.command,
                        "platform": c.platform,
                    })
                }).collect::<Vec<_>>(),
                "alternatives": remediation.alternatives,
                "learn_more": remediation.learn_more,
            });
        }

        serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string())
    }

    /// Render error as plain text
    fn render_plain(&self, error: &Error) -> String {
        let style = Style::from_format(self.format);
        let code = Self::error_code(error);
        let code_def = get_error_code(code);

        let mut output = String::new();

        // Error header with title
        let title = code_def.map_or_else(|| error.to_string(), |def| def.title.to_string());
        output.push_str(&format!("{} {}\n", style.red("Error:"), style.bold(&title)));

        // Error message (if different from title)
        let message = error.to_string();
        if code_def.is_none() || !message.contains(code_def.map_or("", |d| d.title)) {
            output.push_str(&format!("\n{message}\n"));
        }

        // Description from error code catalog
        if let Some(def) = code_def {
            output.push_str(&format!("\n{}\n", def.description));
        }

        // Suggestions from remediation
        if let Some(remediation) = error.remediation() {
            output.push_str(&Self::render_remediation(&remediation, &style));
        }

        // Error code footer
        output.push_str(&format!(
            "\n{}: {}\n",
            style.dim("Error code"),
            style.bold(code)
        ));
        output.push_str(&format!(
            "Run {} for more details.\n",
            style.cyan(&format!("`ft why {code}`"))
        ));

        output
    }

    /// Render remediation section
    fn render_remediation(remediation: &Remediation, style: &Style) -> String {
        let mut output = String::new();

        output.push_str(&format!("\n{}\n", style.bold("Suggestions:")));

        // Summary
        output.push_str(&format!("  {} {}\n", style.dim("•"), remediation.summary));

        // Commands
        for cmd in &remediation.commands {
            let label = cmd.platform.as_ref().map_or_else(
                || cmd.label.clone(),
                |platform| format!("{} ({platform})", cmd.label),
            );
            output.push_str(&format!(
                "  {} {}: {}\n",
                style.dim("→"),
                label,
                style.cyan(&format!("`{}`", cmd.command))
            ));
        }

        // Alternatives
        for alt in &remediation.alternatives {
            output.push_str(&format!("  {} {}\n", style.dim("•"), alt));
        }

        // Learn more link
        if let Some(link) = &remediation.learn_more {
            output.push_str(&format!("  {} Docs: {}\n", style.dim("•"), link));
        }

        output
    }

    /// Render an error code definition (for `ft why FT-XXXX`)
    #[must_use]
    pub fn render_error_code(&self, def: &ErrorCodeDef) -> String {
        if self.format.is_json() {
            return Self::render_error_code_json(def);
        }
        self.render_error_code_plain(def)
    }

    /// Render error code as JSON
    fn render_error_code_json(def: &ErrorCodeDef) -> String {
        let obj = serde_json::json!({
            "code": def.code,
            "title": def.title,
            "description": def.description,
            "category": format!("{:?}", def.category),
            "causes": def.causes,
            "recovery_steps": def.recovery_steps.iter().map(|s| {
                serde_json::json!({
                    "description": s.description,
                    "command": s.command,
                })
            }).collect::<Vec<_>>(),
            "doc_link": def.doc_link,
        });

        serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string())
    }

    /// Render error code as plain text
    fn render_error_code_plain(&self, def: &ErrorCodeDef) -> String {
        let style = Style::from_format(self.format);
        let mut output = String::new();

        // Header
        output.push_str(&format!(
            "{} {}\n",
            style.bold(def.code),
            style.dim(&format!("({:?})", def.category))
        ));
        output.push_str(&format!("{}\n", style.bold(def.title)));

        // Description
        output.push_str(&format!("\n{}\n", def.description));

        // Possible causes
        if !def.causes.is_empty() {
            output.push_str(&format!("\n{}\n", style.bold("Possible causes:")));
            for cause in def.causes {
                output.push_str(&format!("  {} {cause}\n", style.dim("•")));
            }
        }

        // Recovery steps
        if !def.recovery_steps.is_empty() {
            output.push_str(&format!("\n{}\n", style.bold("Recovery steps:")));
            for (i, step) in def.recovery_steps.iter().enumerate() {
                output.push_str(&format!("  {}. {}\n", i + 1, step.description));
                if let Some(cmd) = &step.command {
                    output.push_str(&format!("     {}\n", style.cyan(&format!("`{cmd}`"))));
                }
            }
        }

        // Doc link
        if let Some(link) = def.doc_link {
            output.push_str(&format!("\n{} {link}\n", style.dim("Learn more:")));
        }

        output
    }
}

/// Convenience function to render an error with rich formatting
#[must_use]
pub fn render_error(error: &Error, format: OutputFormat) -> String {
    ErrorRenderer::new(format).render(error)
}

/// Convenience function to get the error code for an error
#[must_use]
pub fn get_code_for_error(error: &Error) -> &'static str {
    ErrorRenderer::error_code(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_codes_mapped_correctly() {
        let test_cases = [
            (Error::Wezterm(WeztermError::CliNotFound), "FT-1001"),
            (Error::Wezterm(WeztermError::NotRunning), "FT-1002"),
            (Error::Wezterm(WeztermError::PaneNotFound(1)), "FT-1010"),
            (
                Error::Storage(StorageError::Database("test".into())),
                "FT-2001",
            ),
            (Error::Pattern(PatternError::MatchTimeout), "FT-3020"),
            (Error::Workflow(WorkflowError::PaneLocked), "FT-5030"),
            (
                Error::Config(ConfigError::FileNotFound("ft.toml".into())),
                "FT-7001",
            ),
            (Error::Policy("denied".into()), "FT-4001"),
        ];

        for (error, expected_code) in test_cases {
            assert_eq!(
                ErrorRenderer::error_code(&error),
                expected_code,
                "Wrong code for {:?}",
                error
            );
        }
    }

    #[test]
    fn render_plain_includes_code() {
        let error = Error::Wezterm(WeztermError::PaneNotFound(42));
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let output = renderer.render(&error);

        assert!(output.contains("FT-1010"), "Should include error code");
        assert!(
            output.contains("ft why FT-1010"),
            "Should include ft why hint"
        );
    }

    #[test]
    fn render_json_has_structure() {
        let error = Error::Wezterm(WeztermError::NotRunning);
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);

        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["code"], "FT-1002");
        assert!(parsed["error"].is_string());
    }

    #[test]
    fn render_plain_includes_code_and_title() {
        let error = Error::Wezterm(WeztermError::CliNotFound);
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let output = renderer.render(&error);

        assert!(output.contains("FT-1001"));
        assert!(output.contains("WezTerm CLI not found"));
        assert!(output.contains("ft why FT-1001"));
    }

    #[test]
    fn render_json_includes_code_and_category() {
        let error = Error::Config(ConfigError::FileNotFound("ft.toml".to_string()));
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);

        let json: serde_json::Value = serde_json::from_str(&output).expect("valid json output");
        assert_eq!(json["code"], "FT-7001");
        assert!(
            json["title"]
                .as_str()
                .unwrap_or_default()
                .contains("Config")
        );
        assert_eq!(json["category"], "Config");
    }

    #[test]
    fn renderer_codes_exist_in_catalog() {
        let io_error = Error::Io(std::io::Error::other("io failure"));
        let json_error =
            Error::Json(serde_json::from_str::<serde_json::Value>("not json").unwrap_err());

        let samples = vec![
            Error::Wezterm(WeztermError::CliNotFound),
            Error::Storage(StorageError::Database("db".to_string())),
            Error::Pattern(PatternError::InvalidRule("rule".to_string())),
            Error::Workflow(WorkflowError::NotFound("missing".to_string())),
            Error::Config(ConfigError::ValidationError("bad".to_string())),
            Error::Policy("denied".to_string()),
            Error::Runtime("boom".to_string()),
            io_error,
            json_error,
        ];

        for error in samples {
            let code = ErrorRenderer::error_code(&error);
            assert!(
                get_error_code(code).is_some(),
                "Missing catalog entry for {code}"
            );
        }
    }

    // =====================================================================
    // Exhaustive error_code mapping tests
    // =====================================================================

    #[test]
    fn error_code_wezterm_all_variants() {
        assert_eq!(
            ErrorRenderer::error_code(&Error::Wezterm(WeztermError::CliNotFound)),
            "FT-1001"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Wezterm(WeztermError::NotRunning)),
            "FT-1002"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Wezterm(WeztermError::PaneNotFound(0))),
            "FT-1010"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Wezterm(WeztermError::SocketNotFound(
                "/tmp/x".to_string()
            ))),
            "FT-1003"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Wezterm(WeztermError::CommandFailed(
                "err".to_string()
            ))),
            "FT-1020"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Wezterm(WeztermError::ParseError(
                "bad".to_string()
            ))),
            "FT-1021"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Wezterm(WeztermError::Timeout(30))),
            "FT-1022"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Wezterm(WeztermError::CircuitOpen {
                retry_after_ms: 5000
            })),
            "FT-1030"
        );
    }

    #[test]
    fn error_code_storage_all_variants() {
        assert_eq!(
            ErrorRenderer::error_code(&Error::Storage(StorageError::Database("x".into()))),
            "FT-2001"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Storage(StorageError::SequenceDiscontinuity {
                expected: 1,
                actual: 3,
            })),
            "FT-2010"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Storage(StorageError::MigrationFailed(
                "fail".into()
            ))),
            "FT-2002"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Storage(StorageError::SchemaTooNew {
                current: 5,
                supported: 3,
            })),
            "FT-2003"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Storage(StorageError::WaTooOld {
                current: "0.1".into(),
                min_compatible: "0.3".into(),
            })),
            "FT-2004"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Storage(StorageError::FtsQueryError("q".into()))),
            "FT-2020"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Storage(StorageError::Corruption {
                details: "bad".into()
            })),
            "FT-2030"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Storage(StorageError::NotFound("x".into()))),
            "FT-2040"
        );
    }

    #[test]
    fn error_code_pattern_all_variants() {
        assert_eq!(
            ErrorRenderer::error_code(&Error::Pattern(PatternError::InvalidRule("r".into()))),
            "FT-3001"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Pattern(PatternError::InvalidRegex("r".into()))),
            "FT-3002"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Pattern(PatternError::PackNotFound("p".into()))),
            "FT-3010"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Pattern(PatternError::MatchTimeout)),
            "FT-3020"
        );
    }

    #[test]
    fn error_code_workflow_all_variants() {
        assert_eq!(
            ErrorRenderer::error_code(&Error::Workflow(WorkflowError::NotFound("w".into()))),
            "FT-5001"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Workflow(WorkflowError::Aborted("a".into()))),
            "FT-5010"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Workflow(WorkflowError::GuardFailed("g".into()))),
            "FT-5020"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Workflow(WorkflowError::PaneLocked)),
            "FT-5030"
        );
    }

    #[test]
    fn error_code_config_all_variants() {
        assert_eq!(
            ErrorRenderer::error_code(&Error::Config(ConfigError::FileNotFound("f".into()))),
            "FT-7001"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Config(ConfigError::ReadFailed(
                "f".into(),
                "e".into()
            ))),
            "FT-7002"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Config(ConfigError::ParseError("p".into()))),
            "FT-7003"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Config(ConfigError::ParseFailed("p".into()))),
            "FT-7003"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Config(ConfigError::SerializeFailed("s".into()))),
            "FT-7004"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Config(ConfigError::ValidationError("v".into()))),
            "FT-7010"
        );
    }

    #[test]
    fn error_code_standalone_variants() {
        assert_eq!(
            ErrorRenderer::error_code(&Error::Policy("p".into())),
            "FT-4001"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Io(std::io::Error::other("e"))),
            "FT-9002"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Runtime("r".into())),
            "FT-9001"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::SetupError("s".into())),
            "FT-6001"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Cancelled("c".into())),
            "FT-9004"
        );
        assert_eq!(
            ErrorRenderer::error_code(&Error::Panicked("p".into())),
            "FT-9005"
        );
    }

    // =====================================================================
    // ErrorRenderer constructor / default tests
    // =====================================================================

    #[test]
    fn default_renderer_uses_auto() {
        let renderer = ErrorRenderer::default();
        // Just verify it doesn't panic and renders
        let error = Error::Runtime("test".into());
        let output = renderer.render(&error);
        assert!(!output.is_empty());
    }

    #[test]
    fn new_renderer_stores_format() {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let error = Error::Policy("denied".into());
        let output = renderer.render(&error);
        // JSON output should parse
        let _: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
    }

    // =====================================================================
    // render_plain comprehensive tests
    // =====================================================================

    #[test]
    fn render_plain_includes_ft_why_hint() {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let error = Error::Storage(StorageError::Database("connection failed".into()));
        let output = renderer.render(&error);
        assert!(output.contains("ft why FT-2001"));
    }

    #[test]
    fn render_plain_includes_error_message() {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let error = Error::Runtime("unexpected shutdown".into());
        let output = renderer.render(&error);
        assert!(output.contains("unexpected shutdown"));
    }

    #[test]
    fn render_plain_workflow_error() {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let error = Error::Workflow(WorkflowError::PaneLocked);
        let output = renderer.render(&error);
        assert!(output.contains("FT-5030"));
        assert!(output.contains("ft why FT-5030"));
    }

    #[test]
    fn render_plain_config_error_with_remediation() {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let error = Error::Config(ConfigError::FileNotFound("/etc/ft.toml".into()));
        let output = renderer.render(&error);
        assert!(output.contains("FT-7001"));
        // Config errors should have remediation suggestions
        assert!(output.contains("Suggestions:") || output.contains("ft why"));
    }

    #[test]
    fn render_plain_io_error() {
        let renderer = ErrorRenderer::new(OutputFormat::Plain);
        let error = Error::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "access denied",
        ));
        let output = renderer.render(&error);
        assert!(output.contains("FT-9002"));
    }

    // =====================================================================
    // render_json comprehensive tests
    // =====================================================================

    #[test]
    fn render_json_has_ok_false() {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let error = Error::Runtime("fail".into());
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["ok"], false);
    }

    #[test]
    fn render_json_has_error_code_and_message() {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let error = Error::Pattern(PatternError::MatchTimeout);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["code"], "FT-3020");
        assert!(parsed["error"].is_string());
    }

    #[test]
    fn render_json_includes_title_and_description() {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let error = Error::Wezterm(WeztermError::CliNotFound);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        // Should have catalog info since FT-1001 exists
        assert!(parsed["title"].is_string());
        assert!(parsed["description"].is_string());
    }

    #[test]
    fn render_json_includes_remediation() {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let error = Error::Wezterm(WeztermError::NotRunning);
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        // WeztermError provides remediation
        assert!(parsed.get("remediation").is_some());
        let rem = &parsed["remediation"];
        assert!(rem["summary"].is_string());
        assert!(rem["commands"].is_array());
    }

    #[test]
    fn render_json_cancelled_error() {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let error = Error::Cancelled("user pressed ctrl-c".into());
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["code"], "FT-9004");
        assert_eq!(parsed["ok"], false);
    }

    #[test]
    fn render_json_panicked_error() {
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let error = Error::Panicked("thread panicked at ...".into());
        let output = renderer.render(&error);
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["code"], "FT-9005");
    }

    // =====================================================================
    // Convenience function tests
    // =====================================================================

    #[test]
    fn render_error_convenience_plain() {
        let error = Error::Policy("blocked".into());
        let output = render_error(&error, OutputFormat::Plain);
        assert!(output.contains("FT-4001"));
    }

    #[test]
    fn render_error_convenience_json() {
        let error = Error::Policy("blocked".into());
        let output = render_error(&error, OutputFormat::Json);
        let _: serde_json::Value =
            serde_json::from_str(&output).expect("convenience fn produces valid JSON");
    }

    #[test]
    fn get_code_for_error_convenience() {
        let error = Error::SetupError("fail".into());
        assert_eq!(get_code_for_error(&error), "FT-6001");
    }

    // =====================================================================
    // Error code uniqueness and format invariants
    // =====================================================================

    #[test]
    fn all_error_codes_start_with_ft() {
        let errors: Vec<Error> = vec![
            Error::Wezterm(WeztermError::CliNotFound),
            Error::Storage(StorageError::Database("x".into())),
            Error::Pattern(PatternError::MatchTimeout),
            Error::Workflow(WorkflowError::PaneLocked),
            Error::Config(ConfigError::FileNotFound("x".into())),
            Error::Policy("x".into()),
            Error::Io(std::io::Error::other("x")),
            Error::Runtime("x".into()),
            Error::SetupError("x".into()),
            Error::Cancelled("x".into()),
            Error::Panicked("x".into()),
        ];

        for error in errors {
            let code = ErrorRenderer::error_code(&error);
            assert!(
                code.starts_with("FT-"),
                "Error code {code} should start with FT-"
            );
        }
    }

    #[test]
    fn error_code_format_is_ft_dash_digits() {
        let errors: Vec<Error> = vec![
            Error::Wezterm(WeztermError::CliNotFound),
            Error::Storage(StorageError::Database("x".into())),
            Error::Pattern(PatternError::MatchTimeout),
            Error::Workflow(WorkflowError::PaneLocked),
            Error::Config(ConfigError::FileNotFound("x".into())),
            Error::Policy("x".into()),
            Error::Io(std::io::Error::other("x")),
            Error::Runtime("x".into()),
        ];

        for error in errors {
            let code = ErrorRenderer::error_code(&error);
            let parts: Vec<&str> = code.splitn(2, '-').collect();
            assert_eq!(parts.len(), 2, "Code {code} should have FT-XXXX format");
            assert_eq!(parts[0], "FT");
            assert!(
                parts[1].chars().all(|c| c.is_ascii_digit()),
                "Code suffix {code} should be all digits"
            );
        }
    }
}
