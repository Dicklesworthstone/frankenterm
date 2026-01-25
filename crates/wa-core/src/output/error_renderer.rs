//! Error renderer for human-readable CLI error output
//!
//! Bridges the error_codes catalog with error rendering,
//! producing rich error messages with error codes, descriptions,
//! and actionable suggestions.

use crate::error::{
    ConfigError, Error, PatternError, Remediation, StorageError, WeztermError, WorkflowError,
};
use crate::error_codes::{get_error_code, ErrorCodeDef};
use super::format::{OutputFormat, Style};

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
                WeztermError::CliNotFound => "WA-1001",
                WeztermError::NotRunning => "WA-1002",
                WeztermError::PaneNotFound(_) => "WA-1010",
                WeztermError::SocketNotFound(_) => "WA-1003",
                WeztermError::CommandFailed(_) => "WA-1020",
                WeztermError::ParseError(_) => "WA-1021",
                WeztermError::Timeout(_) => "WA-1022",
            },
            Error::Storage(e) => match e {
                StorageError::Database(_) => "WA-2001",
                StorageError::SequenceDiscontinuity { .. } => "WA-2010",
                StorageError::MigrationFailed(_) => "WA-2002",
                StorageError::FtsQueryError(_) => "WA-2020",
            },
            Error::Pattern(e) => match e {
                PatternError::InvalidRule(_) => "WA-3001",
                PatternError::InvalidRegex(_) => "WA-3002",
                PatternError::PackNotFound(_) => "WA-3010",
                PatternError::MatchTimeout => "WA-3020",
            },
            Error::Workflow(e) => match e {
                WorkflowError::NotFound(_) => "WA-5001",
                WorkflowError::Aborted(_) => "WA-5010",
                WorkflowError::GuardFailed(_) => "WA-5020",
                WorkflowError::PaneLocked => "WA-5030",
            },
            Error::Config(e) => match e {
                ConfigError::FileNotFound(_) => "WA-7001",
                ConfigError::ReadFailed(_, _) => "WA-7002",
                ConfigError::ParseError(_) | ConfigError::ParseFailed(_) => "WA-7003",
                ConfigError::SerializeFailed(_) => "WA-7004",
                ConfigError::ValidationError(_) => "WA-7010",
            },
            Error::Policy(_) => "WA-4001",
            Error::Io(_) => "WA-9002",
            Error::Json(_) => "WA-9003",
            Error::Runtime(_) => "WA-9001",
        }
    }

    /// Render an error for CLI output
    #[must_use]
    pub fn render(&self, error: &Error) -> String {
        if self.format.is_json() {
            return self.render_json(error);
        }
        self.render_plain(error)
    }

    /// Render error as JSON
    fn render_json(&self, error: &Error) -> String {
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
        let title = code_def.map_or_else(
            || error.to_string(),
            |def| def.title.to_string(),
        );
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
            output.push_str(&self.render_remediation(&remediation, &style));
        }

        // Error code footer
        output.push_str(&format!(
            "\n{}: {}\n",
            style.dim("Error code"),
            style.bold(code)
        ));
        output.push_str(&format!(
            "Run {} for more details.\n",
            style.cyan(&format!("`wa why {code}`"))
        ));

        output
    }

    /// Render remediation section
    fn render_remediation(&self, remediation: &Remediation, style: &Style) -> String {
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

    /// Render an error code definition (for `wa why WA-XXXX`)
    #[must_use]
    pub fn render_error_code(&self, def: &ErrorCodeDef) -> String {
        if self.format.is_json() {
            return self.render_error_code_json(def);
        }
        self.render_error_code_plain(def)
    }

    /// Render error code as JSON
    fn render_error_code_json(&self, def: &ErrorCodeDef) -> String {
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
                output.push_str(&format!(
                    "  {}. {}\n",
                    i + 1,
                    step.description
                ));
                if let Some(cmd) = step.command {
                    output.push_str(&format!(
                        "     {}\n",
                        style.cyan(&format!("`{cmd}`"))
                    ));
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
            (Error::Wezterm(WeztermError::CliNotFound), "WA-1001"),
            (Error::Wezterm(WeztermError::NotRunning), "WA-1002"),
            (Error::Wezterm(WeztermError::PaneNotFound(1)), "WA-1010"),
            (Error::Storage(StorageError::Database("test".into())), "WA-2001"),
            (Error::Pattern(PatternError::MatchTimeout), "WA-3020"),
            (Error::Workflow(WorkflowError::PaneLocked), "WA-5030"),
            (Error::Config(ConfigError::FileNotFound("wa.toml".into())), "WA-7001"),
            (Error::Policy("denied".into()), "WA-4001"),
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

        assert!(output.contains("WA-1010"), "Should include error code");
        assert!(output.contains("wa why WA-1010"), "Should include wa why hint");
    }

    #[test]
    fn render_json_has_structure() {
        let error = Error::Wezterm(WeztermError::NotRunning);
        let renderer = ErrorRenderer::new(OutputFormat::Json);
        let output = renderer.render(&error);

        let parsed: serde_json::Value = serde_json::from_str(&output).expect("valid JSON");
        assert_eq!(parsed["ok"], false);
        assert_eq!(parsed["code"], "WA-1002");
        assert!(parsed["error"].is_string());
    }
}
