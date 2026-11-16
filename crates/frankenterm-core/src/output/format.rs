//! Output format detection and configuration
//!
//! Handles automatic detection of terminal capabilities and user-specified
//! output format preferences.

use std::io::IsTerminal;
use std::str::FromStr;

/// Output format for CLI commands
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputFormat {
    /// Automatic: rich if TTY, plain if not
    #[default]
    Auto,
    /// Plain text: no ANSI escape codes, stable for piping
    Plain,
    /// JSON: machine-readable structured output
    Json,
}

impl OutputFormat {
    /// Parse format from string argument.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::from_str(s).ok()
    }

    /// Check if this format should use colors/rich formatting
    ///
    /// Returns true only for Auto format when connected to a TTY.
    #[must_use]
    pub fn is_rich(&self) -> bool {
        match self {
            Self::Auto => std::io::stdout().is_terminal(),
            Self::Plain | Self::Json => false,
        }
    }

    /// Check if this format outputs JSON
    #[must_use]
    pub fn is_json(&self) -> bool {
        matches!(self, Self::Json)
    }

    /// Check if this format outputs plain text (no ANSI)
    #[must_use]
    pub fn is_plain(&self) -> bool {
        match self {
            Self::Auto => !std::io::stdout().is_terminal(),
            Self::Plain => true,
            Self::Json => false,
        }
    }

    /// Get the effective format (resolves Auto to Plain or Rich)
    #[must_use]
    pub fn effective(&self) -> EffectiveFormat {
        match self {
            Self::Auto => {
                if std::io::stdout().is_terminal() {
                    EffectiveFormat::Rich
                } else {
                    EffectiveFormat::Plain
                }
            }
            Self::Plain => EffectiveFormat::Plain,
            Self::Json => EffectiveFormat::Json,
        }
    }
}

impl FromStr for OutputFormat {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "plain" | "text" => Ok(Self::Plain),
            "json" => Ok(Self::Json),
            _ => Err(()),
        }
    }
}

impl std::fmt::Display for OutputFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Plain => write!(f, "plain"),
            Self::Json => write!(f, "json"),
        }
    }
}

/// Resolved output format (after TTY detection)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectiveFormat {
    /// Rich output with ANSI colors
    Rich,
    /// Plain text without ANSI
    Plain,
    /// JSON structured output
    Json,
}

/// Detect the appropriate output format based on environment
///
/// Checks (in order):
/// 1. `FT_OUTPUT_FORMAT` environment variable
/// 2. `NO_COLOR` environment variable (forces plain)
/// 3. TTY detection (rich if TTY, plain if not)
#[must_use]
pub fn detect_format() -> OutputFormat {
    // Check explicit format override
    if let Ok(format) = std::env::var("FT_OUTPUT_FORMAT") {
        if let Some(f) = OutputFormat::parse(&format) {
            return f;
        }
    }

    // Check NO_COLOR (https://no-color.org/)
    if std::env::var("NO_COLOR").is_ok() {
        return OutputFormat::Plain;
    }

    // Default to auto-detect
    OutputFormat::Auto
}

// =============================================================================
// ANSI Color Constants
// =============================================================================

/// ANSI escape codes for terminal colors
#[allow(dead_code)]
pub mod colors {
    /// Reset all formatting
    pub const RESET: &str = "\x1b[0m";
    /// Bold text
    pub const BOLD: &str = "\x1b[1m";
    /// Dim text
    pub const DIM: &str = "\x1b[2m";
    /// Italic text
    pub const ITALIC: &str = "\x1b[3m";
    /// Underline text
    pub const UNDERLINE: &str = "\x1b[4m";

    // Foreground colors
    /// Red foreground
    pub const RED: &str = "\x1b[31m";
    /// Green foreground
    pub const GREEN: &str = "\x1b[32m";
    /// Yellow foreground
    pub const YELLOW: &str = "\x1b[33m";
    /// Blue foreground
    pub const BLUE: &str = "\x1b[34m";
    /// Magenta foreground
    pub const MAGENTA: &str = "\x1b[35m";
    /// Cyan foreground
    pub const CYAN: &str = "\x1b[36m";
    /// White foreground
    pub const WHITE: &str = "\x1b[37m";
    /// Gray (bright black) foreground
    pub const GRAY: &str = "\x1b[90m";

    // Bright foreground colors
    /// Bright red foreground
    pub const BRIGHT_RED: &str = "\x1b[91m";
    /// Bright green foreground
    pub const BRIGHT_GREEN: &str = "\x1b[92m";
    /// Bright yellow foreground
    pub const BRIGHT_YELLOW: &str = "\x1b[93m";
    /// Bright blue foreground
    pub const BRIGHT_BLUE: &str = "\x1b[94m";
    /// Bright cyan foreground
    pub const BRIGHT_CYAN: &str = "\x1b[96m";
}

/// Style helper for conditional ANSI formatting
pub struct Style {
    enabled: bool,
}

impl Style {
    /// Create a new style helper
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Create style helper based on output format
    #[must_use]
    pub fn from_format(format: OutputFormat) -> Self {
        Self::new(format.is_rich())
    }

    /// Wrap text in the given ANSI code
    #[must_use]
    pub fn apply(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("{code}{text}{}", colors::RESET)
        } else {
            text.to_string()
        }
    }

    /// Make text bold
    #[must_use]
    pub fn bold(&self, text: &str) -> String {
        self.apply(colors::BOLD, text)
    }

    /// Make text dim
    #[must_use]
    pub fn dim(&self, text: &str) -> String {
        self.apply(colors::DIM, text)
    }

    /// Make text red
    #[must_use]
    pub fn red(&self, text: &str) -> String {
        self.apply(colors::RED, text)
    }

    /// Make text green
    #[must_use]
    pub fn green(&self, text: &str) -> String {
        self.apply(colors::GREEN, text)
    }

    /// Make text yellow
    #[must_use]
    pub fn yellow(&self, text: &str) -> String {
        self.apply(colors::YELLOW, text)
    }

    /// Make text blue
    #[allow(dead_code)]
    #[must_use]
    pub fn blue(&self, text: &str) -> String {
        self.apply(colors::BLUE, text)
    }

    /// Make text cyan
    #[must_use]
    pub fn cyan(&self, text: &str) -> String {
        self.apply(colors::CYAN, text)
    }

    /// Make text gray
    #[must_use]
    pub fn gray(&self, text: &str) -> String {
        self.apply(colors::GRAY, text)
    }

    /// Apply status color (green for success, red for failure, yellow for warning)
    #[allow(dead_code)]
    #[must_use]
    pub fn status(&self, text: &str, success: bool) -> String {
        if success {
            self.green(text)
        } else {
            self.red(text)
        }
    }

    /// Apply severity color
    #[must_use]
    pub fn severity(&self, text: &str, severity: &str) -> String {
        match severity.to_lowercase().as_str() {
            "critical" | "error" => self.red(text),
            "warning" | "warn" => self.yellow(text),
            "info" => self.cyan(text),
            _ => text.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_from_str() {
        assert_eq!(OutputFormat::parse("auto"), Some(OutputFormat::Auto));
        assert_eq!(OutputFormat::parse("plain"), Some(OutputFormat::Plain));
        assert_eq!(OutputFormat::parse("text"), Some(OutputFormat::Plain));
        assert_eq!(OutputFormat::parse("json"), Some(OutputFormat::Json));
        assert_eq!(OutputFormat::parse("JSON"), Some(OutputFormat::Json));
        assert_eq!(OutputFormat::parse("invalid"), None);
    }

    #[test]
    fn test_format_display() {
        assert_eq!(OutputFormat::Auto.to_string(), "auto");
        assert_eq!(OutputFormat::Plain.to_string(), "plain");
        assert_eq!(OutputFormat::Json.to_string(), "json");
    }

    #[test]
    fn test_style_disabled() {
        let style = Style::new(false);
        assert_eq!(style.bold("test"), "test");
        assert_eq!(style.red("error"), "error");
    }

    #[test]
    fn test_style_enabled() {
        let style = Style::new(true);
        assert!(style.bold("test").contains("\x1b[1m"));
        assert!(style.bold("test").contains("\x1b[0m"));
        assert!(style.red("error").contains("\x1b[31m"));
    }

    #[test]
    fn test_json_format_properties() {
        assert!(OutputFormat::Json.is_json());
        assert!(!OutputFormat::Json.is_rich());
        assert!(!OutputFormat::Json.is_plain());
    }

    #[test]
    fn test_plain_format_properties() {
        assert!(!OutputFormat::Plain.is_json());
        assert!(!OutputFormat::Plain.is_rich());
        assert!(OutputFormat::Plain.is_plain());
    }

    // =====================================================================
    // OutputFormat parsing edge cases
    // =====================================================================

    #[test]
    fn parse_case_insensitive() {
        assert_eq!(OutputFormat::parse("AUTO"), Some(OutputFormat::Auto));
        assert_eq!(OutputFormat::parse("Auto"), Some(OutputFormat::Auto));
        assert_eq!(OutputFormat::parse("PLAIN"), Some(OutputFormat::Plain));
        assert_eq!(OutputFormat::parse("Plain"), Some(OutputFormat::Plain));
        assert_eq!(OutputFormat::parse("TEXT"), Some(OutputFormat::Plain));
        assert_eq!(OutputFormat::parse("Text"), Some(OutputFormat::Plain));
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert_eq!(OutputFormat::parse(""), None);
        assert_eq!(OutputFormat::parse("xml"), None);
        assert_eq!(OutputFormat::parse("csv"), None);
        assert_eq!(OutputFormat::parse("yaml"), None);
        assert_eq!(OutputFormat::parse("rich"), None);
    }

    #[test]
    fn from_str_round_trip() {
        for format in [OutputFormat::Auto, OutputFormat::Plain, OutputFormat::Json] {
            let s = format.to_string();
            let parsed: OutputFormat = s.parse().expect("round-trip should succeed");
            assert_eq!(parsed, format);
        }
    }

    #[test]
    fn from_str_text_alias() {
        let parsed: OutputFormat = "text".parse().unwrap();
        assert_eq!(parsed, OutputFormat::Plain);
    }

    #[test]
    fn from_str_error_type() {
        let result: Result<OutputFormat, ()> = "nope".parse();
        assert!(result.is_err());
    }

    // =====================================================================
    // OutputFormat display
    // =====================================================================

    #[test]
    fn display_all_variants() {
        assert_eq!(format!("{}", OutputFormat::Auto), "auto");
        assert_eq!(format!("{}", OutputFormat::Plain), "plain");
        assert_eq!(format!("{}", OutputFormat::Json), "json");
    }

    // =====================================================================
    // OutputFormat boolean queries
    // =====================================================================

    #[test]
    fn is_json_only_for_json() {
        assert!(OutputFormat::Json.is_json());
        assert!(!OutputFormat::Auto.is_json());
        assert!(!OutputFormat::Plain.is_json());
    }

    #[test]
    fn plain_is_not_rich() {
        // Plain is never rich, regardless of TTY
        assert!(!OutputFormat::Plain.is_rich());
    }

    #[test]
    fn json_is_not_rich() {
        assert!(!OutputFormat::Json.is_rich());
    }

    #[test]
    fn json_is_not_plain() {
        assert!(!OutputFormat::Json.is_plain());
    }

    #[test]
    fn plain_is_plain() {
        assert!(OutputFormat::Plain.is_plain());
    }

    // =====================================================================
    // EffectiveFormat resolution
    // =====================================================================

    #[test]
    fn effective_plain() {
        assert_eq!(OutputFormat::Plain.effective(), EffectiveFormat::Plain);
    }

    #[test]
    fn effective_json() {
        assert_eq!(OutputFormat::Json.effective(), EffectiveFormat::Json);
    }

    #[test]
    fn effective_auto_resolves() {
        let eff = OutputFormat::Auto.effective();
        // In test harness (not a TTY), Auto should resolve to Plain
        assert!(
            matches!(eff, EffectiveFormat::Plain | EffectiveFormat::Rich),
            "Auto resolves to either Plain or Rich"
        );
    }

    // =====================================================================
    // Default trait
    // =====================================================================

    #[test]
    fn default_is_auto() {
        assert_eq!(OutputFormat::default(), OutputFormat::Auto);
    }

    // =====================================================================
    // Style tests
    // =====================================================================

    #[test]
    fn style_disabled_passes_through() {
        let style = Style::new(false);
        assert_eq!(style.bold("x"), "x");
        assert_eq!(style.dim("x"), "x");
        assert_eq!(style.red("x"), "x");
        assert_eq!(style.green("x"), "x");
        assert_eq!(style.yellow("x"), "x");
        assert_eq!(style.blue("x"), "x");
        assert_eq!(style.cyan("x"), "x");
        assert_eq!(style.gray("x"), "x");
    }

    #[test]
    fn style_enabled_wraps_with_ansi() {
        let style = Style::new(true);
        let bold = style.bold("test");
        assert!(bold.starts_with("\x1b[1m"));
        assert!(bold.ends_with("\x1b[0m"));
        assert!(bold.contains("test"));
    }

    #[test]
    fn style_enabled_each_color() {
        let style = Style::new(true);
        assert!(style.red("r").contains("\x1b[31m"));
        assert!(style.green("g").contains("\x1b[32m"));
        assert!(style.yellow("y").contains("\x1b[33m"));
        assert!(style.blue("b").contains("\x1b[34m"));
        assert!(style.cyan("c").contains("\x1b[36m"));
        assert!(style.gray("g").contains("\x1b[90m"));
        assert!(style.dim("d").contains("\x1b[2m"));
    }

    #[test]
    fn style_apply_custom_code() {
        let style = Style::new(true);
        let result = style.apply("\x1b[4m", "underlined");
        assert!(result.starts_with("\x1b[4m"));
        assert!(result.contains("underlined"));
        assert!(result.ends_with("\x1b[0m"));
    }

    #[test]
    fn style_apply_empty_text() {
        let style = Style::new(true);
        let result = style.bold("");
        assert_eq!(result, "\x1b[1m\x1b[0m");
    }

    #[test]
    fn style_from_format_plain_is_disabled() {
        let style = Style::from_format(OutputFormat::Plain);
        assert_eq!(style.bold("test"), "test");
    }

    #[test]
    fn style_from_format_json_is_disabled() {
        let style = Style::from_format(OutputFormat::Json);
        assert_eq!(style.red("test"), "test");
    }

    // =====================================================================
    // Style::severity tests
    // =====================================================================

    #[test]
    fn severity_critical_is_red() {
        let style = Style::new(true);
        let result = style.severity("alert", "critical");
        assert!(result.contains("\x1b[31m"));
    }

    #[test]
    fn severity_error_is_red() {
        let style = Style::new(true);
        let result = style.severity("msg", "error");
        assert!(result.contains("\x1b[31m"));
    }

    #[test]
    fn severity_warning_is_yellow() {
        let style = Style::new(true);
        let result = style.severity("msg", "warning");
        assert!(result.contains("\x1b[33m"));
    }

    #[test]
    fn severity_warn_is_yellow() {
        let style = Style::new(true);
        let result = style.severity("msg", "warn");
        assert!(result.contains("\x1b[33m"));
    }

    #[test]
    fn severity_info_is_cyan() {
        let style = Style::new(true);
        let result = style.severity("msg", "info");
        assert!(result.contains("\x1b[36m"));
    }

    #[test]
    fn severity_unknown_no_color() {
        let style = Style::new(true);
        let result = style.severity("msg", "debug");
        assert_eq!(result, "msg");
    }

    #[test]
    fn severity_case_insensitive() {
        let style = Style::new(true);
        assert!(style.severity("x", "CRITICAL").contains("\x1b[31m"));
        assert!(style.severity("x", "Warning").contains("\x1b[33m"));
        assert!(style.severity("x", "INFO").contains("\x1b[36m"));
    }

    #[test]
    fn severity_disabled_style() {
        let style = Style::new(false);
        assert_eq!(style.severity("msg", "critical"), "msg");
        assert_eq!(style.severity("msg", "error"), "msg");
    }

    // =====================================================================
    // Style::status tests
    // =====================================================================

    #[test]
    fn status_success_is_green() {
        let style = Style::new(true);
        let result = style.status("ok", true);
        assert!(result.contains("\x1b[32m"));
    }

    #[test]
    fn status_failure_is_red() {
        let style = Style::new(true);
        let result = style.status("fail", false);
        assert!(result.contains("\x1b[31m"));
    }

    // =====================================================================
    // detect_format tests
    // =====================================================================

    #[test]
    fn detect_format_default_is_auto() {
        // In test context (no env vars set), should get Auto
        let fmt = detect_format();
        // Could be Auto or Plain depending on NO_COLOR env
        assert!(matches!(fmt, OutputFormat::Auto | OutputFormat::Plain));
    }

    // =====================================================================
    // Clone, Copy, Debug, PartialEq, Eq
    // =====================================================================

    #[test]
    fn output_format_clone_and_eq() {
        let a = OutputFormat::Json;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn output_format_debug() {
        let dbg = format!("{:?}", OutputFormat::Auto);
        assert_eq!(dbg, "Auto");
    }

    #[test]
    fn effective_format_debug_and_eq() {
        assert_eq!(EffectiveFormat::Plain, EffectiveFormat::Plain);
        assert_ne!(EffectiveFormat::Plain, EffectiveFormat::Json);
        let dbg = format!("{:?}", EffectiveFormat::Rich);
        assert_eq!(dbg, "Rich");
    }
}
