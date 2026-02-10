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
}
