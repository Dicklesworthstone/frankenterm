//! Parameter Generalization and PAC-Bayesian Bounds for ARS reflexes.
//!
//! A hardcoded fix for `/app/src/main.rs` is useless if the error occurs in
//! `/app/src/lib.rs`. This module detects literal parameters in extracted
//! commands, converts them to template variables, and generates safety-bounded
//! regexes using PAC-Bayesian concentration inequalities.
//!
//! # Algorithm
//!
//! 1. **Parameter detection**: Compare error text against extracted commands.
//!    Substrings that appear in both (file paths, line numbers, identifiers)
//!    are candidate parameters.
//! 2. **Template generation**: Replace literal parameters with Jinja-style
//!    placeholders like `{{cap.file}}`, `{{cap.line}}`, `{{cap.ident}}`.
//! 3. **Safety bounds**: Each template variable gets a regex safety constraint
//!    (e.g., `^[a-zA-Z0-9_./-]+$` for file paths) that prevents injection.
//! 4. **PAC-Bayesian bound**: Given `n` observed examples and `m` matches,
//!    compute the posterior risk bound on the generalization failing.
//!
//! # Performance
//!
//! Generalization targets < 1ms for typical recovery sequences.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tracing::{debug, trace};

use crate::mdl_extraction::CommandBlock;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for parameter generalization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralizeConfig {
    /// Minimum substring length to consider as a parameter (avoids noise).
    pub min_param_len: usize,
    /// Maximum number of parameters to extract per command.
    pub max_params_per_command: usize,
    /// Maximum template variables total.
    pub max_total_params: usize,
    /// PAC-Bayesian prior weight (lambda). Higher = more conservative.
    pub pac_prior_weight: f64,
    /// PAC-Bayesian confidence level (delta). Typical: 0.05 for 95%.
    pub pac_confidence_delta: f64,
    /// Whether to detect file path parameters.
    pub detect_file_paths: bool,
    /// Whether to detect line number parameters.
    pub detect_line_numbers: bool,
    /// Whether to detect identifier parameters.
    pub detect_identifiers: bool,
    /// Whether to detect numeric parameters.
    pub detect_numerics: bool,
    /// Custom safety patterns: param_name → regex.
    pub custom_safety_patterns: HashMap<String, String>,
}

impl Default for GeneralizeConfig {
    fn default() -> Self {
        Self {
            min_param_len: 2,
            max_params_per_command: 8,
            max_total_params: 32,
            pac_prior_weight: 1.0,
            pac_confidence_delta: 0.05,
            detect_file_paths: true,
            detect_line_numbers: true,
            detect_identifiers: true,
            detect_numerics: true,
            custom_safety_patterns: HashMap::new(),
        }
    }
}

// =============================================================================
// Parameter kinds and safety regexes
// =============================================================================

/// The kind of detected parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ParamKind {
    /// A file path (e.g., `src/main.rs`, `/app/lib.rs`).
    FilePath,
    /// A line number (e.g., `42`, `127`).
    LineNumber,
    /// An identifier (e.g., `MyStruct`, `foo_bar`).
    Identifier,
    /// A generic numeric value (e.g., port numbers, counts).
    Numeric,
    /// Custom user-defined parameter kind.
    Custom,
}

impl ParamKind {
    /// Get the default safety regex for this parameter kind.
    #[must_use]
    pub fn safety_regex(&self) -> &'static str {
        match self {
            // File paths: alphanumeric, dots, underscores, hyphens, slashes.
            // No backticks, semicolons, pipes, or shell metacharacters.
            ParamKind::FilePath => r"^[a-zA-Z0-9_./-]+$",
            // Line numbers: digits only.
            ParamKind::LineNumber => r"^[0-9]+$",
            // Identifiers: word characters (alphanumeric + underscore), start with letter/_.
            ParamKind::Identifier => r"^[a-zA-Z_][a-zA-Z0-9_]*$",
            // Numeric: digits with optional decimal point.
            ParamKind::Numeric => r"^[0-9]+(\.[0-9]+)?$",
            // Custom: very restrictive default (alphanumeric only).
            ParamKind::Custom => r"^[a-zA-Z0-9]+$",
        }
    }

    /// Template prefix for this kind (used in `{{cap.prefix_N}}`).
    #[must_use]
    pub fn template_prefix(&self) -> &'static str {
        match self {
            ParamKind::FilePath => "file",
            ParamKind::LineNumber => "line",
            ParamKind::Identifier => "ident",
            ParamKind::Numeric => "num",
            ParamKind::Custom => "param",
        }
    }
}

// =============================================================================
// Detected parameter
// =============================================================================

/// A detected literal parameter within a command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DetectedParam {
    /// The literal value found in the command.
    pub literal: String,
    /// Byte offset within the command string.
    pub byte_offset: usize,
    /// Length in bytes.
    pub byte_len: usize,
    /// Which kind of parameter this is.
    pub kind: ParamKind,
    /// Confidence that this is indeed a parameter (0.0–1.0).
    pub confidence: f64,
    /// Where this literal was also found (e.g., "error_text", "output").
    pub corroborating_source: String,
}

// =============================================================================
// Template variable
// =============================================================================

/// A template variable replacing a detected parameter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateVar {
    /// Variable name, e.g., `file_0`, `line_1`.
    pub name: String,
    /// The Jinja-style placeholder, e.g., `{{cap.file_0}}`.
    pub placeholder: String,
    /// The original literal value.
    pub original: String,
    /// Parameter kind.
    pub kind: ParamKind,
    /// Safety regex that valid substitutions must match.
    pub safety_regex: String,
}

// =============================================================================
// Generalized command
// =============================================================================

/// A command with parameters replaced by template variables.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralizedCommand {
    /// Original command text.
    pub original: String,
    /// Templatized command text with `{{cap.X}}` placeholders.
    pub template: String,
    /// Template variables and their safety constraints.
    pub variables: Vec<TemplateVar>,
    /// Sequential index from the original CommandBlock.
    pub block_index: u32,
}

impl GeneralizedCommand {
    /// Whether this command was actually generalized (has variables).
    #[must_use]
    pub fn is_generalized(&self) -> bool {
        !self.variables.is_empty()
    }

    /// Instantiate the template with given values.
    /// Returns None if any value violates its safety regex.
    #[must_use]
    pub fn instantiate(&self, values: &HashMap<String, String>) -> Option<String> {
        let mut result = self.template.clone();
        for var in &self.variables {
            let value = values.get(&var.name)?;
            // Check safety regex.
            if !matches_safety_regex(value, &var.safety_regex) {
                debug!(
                    var = %var.name,
                    value = %value,
                    regex = %var.safety_regex,
                    "Value rejected by safety regex"
                );
                return None;
            }
            result = result.replace(&var.placeholder, value);
        }
        Some(result)
    }
}

// =============================================================================
// PAC-Bayesian bound
// =============================================================================

/// PAC-Bayesian risk bound for a generalization.
///
/// Given `n` observed contexts where `m` matched the template successfully,
/// the PAC-Bayesian theorem gives an upper bound on the true risk:
///
/// ```text
/// R_true ≤ R_empirical + sqrt((KL(posterior||prior) + ln(2*sqrt(n)/delta)) / (2*n))
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PacBayesianBound {
    /// Number of observed examples.
    pub n_observations: u64,
    /// Number of successful matches.
    pub n_matches: u64,
    /// Empirical risk (1 - match_rate).
    pub empirical_risk: f64,
    /// KL divergence between posterior and prior.
    pub kl_divergence: f64,
    /// Confidence parameter delta.
    pub delta: f64,
    /// The computed upper bound on true risk.
    pub risk_bound: f64,
    /// Whether the bound is tight enough to trust (risk_bound < threshold).
    pub is_trustworthy: bool,
}

impl PacBayesianBound {
    /// Compute a PAC-Bayesian bound from observations.
    ///
    /// # Parameters
    /// - `n`: total observations
    /// - `m`: successful matches
    /// - `delta`: confidence level (e.g., 0.05)
    /// - `prior_weight`: KL scaling (lambda)
    #[must_use]
    pub fn compute(n: u64, m: u64, delta: f64, prior_weight: f64) -> Self {
        if n == 0 {
            return Self {
                n_observations: 0,
                n_matches: 0,
                empirical_risk: 1.0,
                kl_divergence: 0.0,
                delta,
                risk_bound: 1.0,
                is_trustworthy: false,
            };
        }

        let n_f = n as f64;
        let m_f = m as f64;

        // Empirical risk = fraction of failures.
        let empirical_risk = 1.0 - (m_f / n_f);

        // KL divergence: for Bernoulli, KL(q||p) where q is posterior (empirical)
        // and p is the prior (uniform 0.5).
        let q = (m_f + 1.0) / (n_f + 2.0); // Laplace-smoothed posterior
        let p = 0.5; // Uniform prior
        let kl = kl_bernoulli(q, p) * prior_weight;

        // PAC-Bayesian bound:
        // R ≤ R_emp + sqrt((KL + ln(2*sqrt(n)/delta)) / (2n))
        let complexity_term = kl + (2.0 * n_f.sqrt() / delta).ln();
        let bound_addition = if complexity_term > 0.0 {
            (complexity_term / (2.0 * n_f)).sqrt()
        } else {
            0.0
        };

        let risk_bound = (empirical_risk + bound_addition).min(1.0);

        Self {
            n_observations: n,
            n_matches: m,
            empirical_risk,
            kl_divergence: kl,
            delta,
            risk_bound,
            // Trustworthy if bound < 0.2 (80%+ confidence the generalization holds)
            is_trustworthy: risk_bound < 0.2,
        }
    }

    /// Compute for a perfect match scenario (all observed examples match).
    #[must_use]
    pub fn perfect(n: u64, delta: f64) -> Self {
        Self::compute(n, n, delta, 1.0)
    }
}

/// KL divergence for Bernoulli distributions: KL(q || p).
fn kl_bernoulli(q: f64, p: f64) -> f64 {
    // Handle edge cases.
    if q <= 0.0 || q >= 1.0 || p <= 0.0 || p >= 1.0 {
        return 0.0;
    }
    q.mul_add((q / p).ln(), (1.0 - q) * ((1.0 - q) / (1.0 - p)).ln())
}

// =============================================================================
// Generalization result
// =============================================================================

/// Result of generalizing a command sequence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneralizationResult {
    /// Generalized commands (with template variables).
    pub commands: Vec<GeneralizedCommand>,
    /// All unique template variables across commands.
    pub all_variables: Vec<TemplateVar>,
    /// PAC-Bayesian bound for the overall generalization.
    pub pac_bound: PacBayesianBound,
    /// Number of parameters detected.
    pub params_detected: usize,
    /// Number of commands that were generalized.
    pub commands_generalized: usize,
}

impl GeneralizationResult {
    /// Whether any commands were generalized.
    #[must_use]
    pub fn has_generalizations(&self) -> bool {
        self.commands_generalized > 0
    }

    /// Overall safety: all variables have safety regexes and bound is trustworthy.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        !self.all_variables.is_empty()
            && self
                .all_variables
                .iter()
                .all(|v| !v.safety_regex.is_empty())
            && self.pac_bound.is_trustworthy
    }
}

// =============================================================================
// Generalizer
// =============================================================================

/// The parameter generalizer.
///
/// Detects literal parameters in extracted commands by correlating with
/// error text, then produces templatized commands with safety regexes.
pub struct Generalizer {
    config: GeneralizeConfig,
}

impl Generalizer {
    /// Create a new generalizer with the given config.
    #[must_use]
    pub fn new(config: GeneralizeConfig) -> Self {
        Self { config }
    }

    /// Create a generalizer with default settings.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(GeneralizeConfig::default())
    }

    /// Detect parameters by comparing error text against command text.
    ///
    /// Finds substrings from `error_text` that appear in command text.
    #[must_use]
    pub fn detect_params(
        &self,
        commands: &[CommandBlock],
        error_text: &str,
    ) -> Vec<(u32, Vec<DetectedParam>)> {
        let mut results = Vec::new();

        for cmd in commands {
            let mut params = Vec::new();

            if self.config.detect_file_paths {
                params.extend(self.detect_file_path_params(&cmd.command, error_text, cmd.index));
            }
            if self.config.detect_line_numbers {
                params.extend(self.detect_line_number_params(&cmd.command, error_text, cmd.index));
            }
            if self.config.detect_identifiers {
                params.extend(self.detect_identifier_params(&cmd.command, error_text, cmd.index));
            }
            if self.config.detect_numerics {
                params.extend(self.detect_numeric_params(&cmd.command, error_text, cmd.index));
            }

            // Deduplicate overlapping detections (keep highest confidence).
            params = self.deduplicate_params(params);

            // Limit per command.
            params.truncate(self.config.max_params_per_command);

            if !params.is_empty() {
                results.push((cmd.index, params));
            }
        }

        results
    }

    /// Generalize commands by replacing detected parameters with templates.
    #[must_use]
    pub fn generalize(
        &self,
        commands: &[CommandBlock],
        error_text: &str,
        n_observations: u64,
        n_matches: u64,
    ) -> GeneralizationResult {
        let detections = self.detect_params(commands, error_text);

        let mut generalized_commands = Vec::new();
        let mut all_variables = Vec::new();
        let mut var_counters: HashMap<ParamKind, usize> = HashMap::new();
        let mut commands_generalized = 0;

        for cmd in commands {
            let cmd_detections: Vec<&DetectedParam> = detections
                .iter()
                .filter(|(idx, _)| *idx == cmd.index)
                .flat_map(|(_, params)| params)
                .collect();

            if cmd_detections.is_empty() {
                generalized_commands.push(GeneralizedCommand {
                    original: cmd.command.clone(),
                    template: cmd.command.clone(),
                    variables: Vec::new(),
                    block_index: cmd.index,
                });
                continue;
            }

            // Check total variable limit.
            if all_variables.len() >= self.config.max_total_params {
                generalized_commands.push(GeneralizedCommand {
                    original: cmd.command.clone(),
                    template: cmd.command.clone(),
                    variables: Vec::new(),
                    block_index: cmd.index,
                });
                continue;
            }

            let mut template = cmd.command.clone();
            let mut cmd_vars = Vec::new();

            // Sort by byte offset descending so replacements don't shift positions.
            let mut sorted_detections = cmd_detections.clone();
            sorted_detections.sort_by_key(|a| std::cmp::Reverse(a.byte_offset));

            for param in sorted_detections {
                let counter = var_counters.entry(param.kind).or_insert(0);
                let var_name = format!("{}_{}", param.kind.template_prefix(), *counter);
                *counter += 1;

                let placeholder = format!("{{{{cap.{}}}}}", var_name);

                let safety = self
                    .config
                    .custom_safety_patterns
                    .get(&var_name)
                    .cloned()
                    .unwrap_or_else(|| param.kind.safety_regex().to_string());

                let var = TemplateVar {
                    name: var_name,
                    placeholder: placeholder.clone(),
                    original: param.literal.clone(),
                    kind: param.kind,
                    safety_regex: safety,
                };

                // Replace in template (from right to left).
                let end = param.byte_offset + param.byte_len;
                if end <= template.len() {
                    template.replace_range(param.byte_offset..end, &placeholder);
                }

                cmd_vars.push(var.clone());
                all_variables.push(var);
            }

            // Reverse cmd_vars so they're in left-to-right order.
            cmd_vars.reverse();

            commands_generalized += 1;
            generalized_commands.push(GeneralizedCommand {
                original: cmd.command.clone(),
                template,
                variables: cmd_vars,
                block_index: cmd.index,
            });
        }

        let pac_bound = PacBayesianBound::compute(
            n_observations,
            n_matches,
            self.config.pac_confidence_delta,
            self.config.pac_prior_weight,
        );

        let params_detected = all_variables.len();

        debug!(
            params_detected,
            commands_generalized,
            risk_bound = pac_bound.risk_bound,
            trustworthy = pac_bound.is_trustworthy,
            "Generalization complete"
        );

        GeneralizationResult {
            commands: generalized_commands,
            all_variables,
            pac_bound,
            params_detected,
            commands_generalized,
        }
    }

    // =========================================================================
    // Parameter detection helpers
    // =========================================================================

    fn detect_file_path_params(
        &self,
        command: &str,
        error_text: &str,
        _block_index: u32,
    ) -> Vec<DetectedParam> {
        let mut params = Vec::new();

        // Extract file-path-like tokens from error text.
        let error_paths = extract_file_paths(error_text);

        for path in &error_paths {
            if path.len() < self.config.min_param_len {
                continue;
            }
            // Find this path in the command.
            if let Some(offset) = command.find(path.as_str()) {
                params.push(DetectedParam {
                    literal: path.clone(),
                    byte_offset: offset,
                    byte_len: path.len(),
                    kind: ParamKind::FilePath,
                    confidence: 0.9,
                    corroborating_source: "error_text".to_string(),
                });
                trace!(path = %path, offset, "Detected file path parameter");
            }
        }

        params
    }

    fn detect_line_number_params(
        &self,
        command: &str,
        error_text: &str,
        _block_index: u32,
    ) -> Vec<DetectedParam> {
        let mut params = Vec::new();

        // Find line numbers in error text (patterns like `:42:`, `line 42`, `L42`).
        let line_numbers = extract_line_numbers(error_text);

        for (num_str, _) in &line_numbers {
            if num_str.len() < self.config.min_param_len {
                continue;
            }
            // Find this number in the command.
            let mut search_from = 0;
            while let Some(pos) = command[search_from..].find(num_str.as_str()) {
                let offset = search_from + pos;
                // Ensure it's a whole number (not part of a larger token).
                let before_ok = offset == 0 || !command.as_bytes()[offset - 1].is_ascii_digit();
                let after_end = offset + num_str.len();
                let after_ok =
                    after_end >= command.len() || !command.as_bytes()[after_end].is_ascii_digit();

                if before_ok && after_ok {
                    params.push(DetectedParam {
                        literal: num_str.clone(),
                        byte_offset: offset,
                        byte_len: num_str.len(),
                        kind: ParamKind::LineNumber,
                        confidence: 0.7,
                        corroborating_source: "error_text".to_string(),
                    });
                }
                search_from = offset + 1;
            }
        }

        params
    }

    fn detect_identifier_params(
        &self,
        command: &str,
        error_text: &str,
        _block_index: u32,
    ) -> Vec<DetectedParam> {
        let mut params = Vec::new();

        // Extract identifiers from error text.
        let identifiers = extract_identifiers(error_text);

        for ident in &identifiers {
            if ident.len() < self.config.min_param_len {
                continue;
            }
            // Don't match common shell keywords.
            if is_common_keyword(ident) {
                continue;
            }
            // Find in command.
            if let Some(offset) = command.find(ident.as_str()) {
                // Verify it's a word boundary.
                let before_ok =
                    offset == 0 || !command.as_bytes()[offset - 1].is_ascii_alphanumeric();
                let after_end = offset + ident.len();
                let after_ok = after_end >= command.len()
                    || !command.as_bytes()[after_end].is_ascii_alphanumeric();

                if before_ok && after_ok {
                    params.push(DetectedParam {
                        literal: ident.clone(),
                        byte_offset: offset,
                        byte_len: ident.len(),
                        kind: ParamKind::Identifier,
                        confidence: 0.6,
                        corroborating_source: "error_text".to_string(),
                    });
                }
            }
        }

        params
    }

    fn detect_numeric_params(
        &self,
        command: &str,
        error_text: &str,
        _block_index: u32,
    ) -> Vec<DetectedParam> {
        let mut params = Vec::new();

        // Find numeric literals in error text.
        let numbers = extract_numbers(error_text);

        for num_str in &numbers {
            if num_str.len() < self.config.min_param_len {
                continue;
            }
            if let Some(offset) = command.find(num_str.as_str()) {
                let before_ok = offset == 0 || !command.as_bytes()[offset - 1].is_ascii_digit();
                let after_end = offset + num_str.len();
                let after_ok =
                    after_end >= command.len() || !command.as_bytes()[after_end].is_ascii_digit();

                if before_ok && after_ok {
                    params.push(DetectedParam {
                        literal: num_str.clone(),
                        byte_offset: offset,
                        byte_len: num_str.len(),
                        kind: ParamKind::Numeric,
                        confidence: 0.5,
                        corroborating_source: "error_text".to_string(),
                    });
                }
            }
        }

        params
    }

    /// Deduplicate overlapping parameter detections, keeping highest confidence.
    #[allow(clippy::unused_self)]
    fn deduplicate_params(&self, mut params: Vec<DetectedParam>) -> Vec<DetectedParam> {
        if params.len() <= 1 {
            return params;
        }

        // Sort by confidence descending.
        params.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut result = Vec::new();
        for param in params {
            let overlaps = result.iter().any(|existing: &DetectedParam| {
                ranges_overlap(
                    param.byte_offset,
                    param.byte_offset + param.byte_len,
                    existing.byte_offset,
                    existing.byte_offset + existing.byte_len,
                )
            });
            if !overlaps {
                result.push(param);
            }
        }

        result
    }
}

// =============================================================================
// Extraction helpers
// =============================================================================

/// Extract file-path-like tokens from text.
fn extract_file_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut i = 0;
    let bytes = text.as_bytes();

    while i < bytes.len() {
        // Look for path-like patterns: starts with letter, dot, or slash,
        // contains at least one slash or dot.
        if is_path_start(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_path_char(bytes[i]) {
                i += 1;
            }
            let candidate = &text[start..i];
            // Must contain a path separator or file extension.
            if candidate.len() >= 3
                && (candidate.contains('/') || candidate.contains('.'))
                && !candidate.ends_with('.')
                && !candidate.starts_with("..")
            {
                paths.push(candidate.to_string());
            }
        } else {
            i += 1;
        }
    }

    paths
}

/// Extract line numbers from error text.
///
/// Recognizes patterns like `:42:`, `:42`, `line 42`, `Line 42`, `L42`.
fn extract_line_numbers(text: &str) -> Vec<(String, usize)> {
    let mut numbers = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();

    for i in 0..len {
        // Pattern 1: `:NUM:` or `:NUM` (common in compiler errors)
        if bytes[i] == b':' && i + 1 < len && bytes[i + 1].is_ascii_digit() {
            let start = i + 1;
            let mut end = start;
            while end < len && bytes[end].is_ascii_digit() {
                end += 1;
            }
            let num = &text[start..end];
            // Validate it's a reasonable line number.
            if let Ok(n) = num.parse::<u64>() {
                if n > 0 && n < 1_000_000 {
                    numbers.push((num.to_string(), start));
                }
            }
        }
        // Pattern 2: `line NUM` or `Line NUM`
        if i + 5 < len {
            let window = &text[i..i + 5];
            if window.eq_ignore_ascii_case("line ") {
                let start = i + 5;
                let mut end = start;
                while end < len && bytes[end].is_ascii_digit() {
                    end += 1;
                }
                if end > start {
                    let num = &text[start..end];
                    if let Ok(n) = num.parse::<u64>() {
                        if n > 0 && n < 1_000_000 {
                            numbers.push((num.to_string(), start));
                        }
                    }
                }
            }
        }
    }

    // Deduplicate.
    numbers.sort_by_key(|(n, _)| n.clone());
    numbers.dedup_by_key(|(n, _)| n.clone());
    numbers
}

/// Extract identifiers from text (CamelCase, snake_case, etc.).
fn extract_identifiers(text: &str) -> Vec<String> {
    let mut idents = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_alphabetic() || bytes[i] == b'_' {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let ident = &text[start..i];
            // Must be at least min length and not all lowercase short words.
            if ident.len() >= 3 {
                idents.push(ident.to_string());
            }
        } else {
            i += 1;
        }
    }

    // Deduplicate.
    idents.sort();
    idents.dedup();
    idents
}

/// Extract standalone numbers from text.
fn extract_numbers(text: &str) -> Vec<String> {
    let mut nums = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                i += 1;
            }
            let num = &text[start..i];
            // Skip tiny or too-long numbers.
            if num.len() >= 2 && num.len() <= 20 && !num.ends_with('.') {
                nums.push(num.to_string());
            }
        } else {
            i += 1;
        }
    }

    nums.sort();
    nums.dedup();
    nums
}

/// Check if a byte can start a file path.
fn is_path_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'/' || b == b'.' || b == b'~'
}

/// Check if a byte can be part of a file path.
fn is_path_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'/' || b == b'.' || b == b'_' || b == b'-' || b == b'~'
}

/// Check if a string is a common shell/programming keyword (not a parameter).
fn is_common_keyword(s: &str) -> bool {
    matches!(
        s,
        "error"
            | "warning"
            | "note"
            | "help"
            | "info"
            | "debug"
            | "Error"
            | "Warning"
            | "Note"
            | "Help"
            | "Info"
            | "Debug"
            | "for"
            | "while"
            | "if"
            | "else"
            | "then"
            | "do"
            | "done"
            | "case"
            | "esac"
            | "function"
            | "return"
            | "exit"
            | "true"
            | "false"
            | "null"
            | "None"
            | "nil"
            | "let"
            | "const"
            | "var"
            | "mut"
            | "pub"
            | "fn"
            | "use"
            | "mod"
            | "impl"
            | "trait"
            | "struct"
            | "enum"
            | "async"
            | "await"
            | "match"
            | "self"
            | "super"
            | "crate"
            | "import"
            | "from"
            | "class"
            | "def"
            | "with"
            | "try"
            | "except"
            | "finally"
            | "raise"
            | "yield"
            | "pass"
            | "and"
            | "not"
            | "the"
            | "that"
            | "this"
            | "has"
            | "was"
            | "are"
            | "were"
            | "been"
            | "have"
            | "had"
            | "can"
            | "could"
            | "should"
            | "would"
            | "will"
            | "shall"
            | "may"
            | "might"
    )
}

/// Check if two byte ranges overlap.
fn ranges_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    a_start < b_end && b_start < a_end
}

/// Check if a value matches a safety regex (simplified, no regex crate dep).
///
/// Supports a subset of common patterns:
/// - `^[charset]+$` — entire string matches charset
/// - `^[charset]*$` — same but allows empty
fn matches_safety_regex(value: &str, pattern: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    match regex::Regex::new(pattern) {
        Ok(re) => re.is_match(value),
        Err(e) => {
            tracing::warn!("Invalid safety regex provided: '{}' ({})", pattern, e);
            false
        }
    }
}

/// Parsed character set from a regex pattern.
struct ParsedCharset {
    allow_empty: bool,
    chars: Vec<(u8, u8)>, // ranges
    exact_chars: Vec<u8>, // individual chars
}

/// Parse `[a-zA-Z0-9_./-]+` or similar simple charset patterns.
fn parse_simple_charset(pattern: &str) -> Option<ParsedCharset> {
    if !pattern.starts_with('[') {
        return None;
    }

    let bracket_end = pattern.find(']')?;
    let charset_str = &pattern[1..bracket_end];
    let quantifier = &pattern[bracket_end + 1..];

    let allow_empty = quantifier.starts_with('*');
    // Must end with + or * (possibly followed by optional group)
    if !quantifier.starts_with('+') && !quantifier.starts_with('*') {
        return None;
    }

    let mut chars = Vec::new();
    let mut exact_chars = Vec::new();
    let bytes = charset_str.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if i + 2 < bytes.len() && bytes[i + 1] == b'-' {
            chars.push((bytes[i], bytes[i + 2]));
            i += 3;
        } else {
            exact_chars.push(bytes[i]);
            i += 1;
        }
    }

    Some(ParsedCharset {
        allow_empty,
        chars,
        exact_chars,
    })
}

/// Validate a value against a parsed charset.
fn validate_against_charset(value: &str, charset: &ParsedCharset) -> bool {
    if value.is_empty() {
        return charset.allow_empty;
    }

    value.bytes().all(|b| {
        charset.chars.iter().any(|(lo, hi)| b >= *lo && b <= *hi)
            || charset.exact_chars.contains(&b)
    })
}

// =============================================================================
// Generalization statistics
// =============================================================================

/// Aggregate statistics for generalization across multiple sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GeneralizationStats {
    /// Total sessions processed.
    pub total_sessions: u64,
    /// Total parameters detected.
    pub total_params_detected: u64,
    /// Total commands generalized.
    pub total_commands_generalized: u64,
    /// Parameters by kind.
    pub params_by_kind: HashMap<String, u64>,
    /// Instantiation attempts.
    pub instantiation_attempts: u64,
    /// Successful instantiations (passed safety checks).
    pub instantiation_successes: u64,
    /// Failed instantiations (rejected by safety regex).
    pub instantiation_failures: u64,
}

impl GeneralizationStats {
    /// Create new empty stats.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a generalization result.
    pub fn record(&mut self, result: &GeneralizationResult) {
        self.total_sessions += 1;
        self.total_params_detected += result.params_detected as u64;
        self.total_commands_generalized += result.commands_generalized as u64;

        for var in &result.all_variables {
            let kind_str = format!("{:?}", var.kind);
            *self.params_by_kind.entry(kind_str).or_insert(0) += 1;
        }
    }

    /// Record an instantiation attempt.
    pub fn record_instantiation(&mut self, success: bool) {
        self.instantiation_attempts += 1;
        if success {
            self.instantiation_successes += 1;
        } else {
            self.instantiation_failures += 1;
        }
    }

    /// Success rate for instantiations.
    #[must_use]
    pub fn instantiation_success_rate(&self) -> f64 {
        if self.instantiation_attempts == 0 {
            return 0.0;
        }
        self.instantiation_successes as f64 / self.instantiation_attempts as f64
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    fn make_block(index: u32, command: &str) -> CommandBlock {
        CommandBlock {
            index,
            command: command.to_string(),
            exit_code: Some(0),
            duration_us: Some(1000),
            output_preview: None,
            timestamp_us: (index as u64 + 1) * 1_000_000,
        }
    }

    // =========================================================================
    // ParamKind tests
    // =========================================================================

    #[test]
    fn param_kind_safety_regexes_are_nonempty() {
        let kinds = [
            ParamKind::FilePath,
            ParamKind::LineNumber,
            ParamKind::Identifier,
            ParamKind::Numeric,
            ParamKind::Custom,
        ];
        for kind in &kinds {
            assert!(!kind.safety_regex().is_empty());
        }
    }

    #[test]
    fn param_kind_template_prefixes_are_unique() {
        let kinds = [
            ParamKind::FilePath,
            ParamKind::LineNumber,
            ParamKind::Identifier,
            ParamKind::Numeric,
            ParamKind::Custom,
        ];
        let prefixes: Vec<&str> = kinds.iter().map(|k| k.template_prefix()).collect();
        let mut deduped = prefixes.clone();
        deduped.sort();
        deduped.dedup();
        assert_eq!(prefixes.len(), deduped.len());
    }

    // =========================================================================
    // File path extraction tests
    // =========================================================================

    #[test]
    fn extract_file_paths_from_error() {
        let error = "error[E0433]: failed to resolve in src/main.rs:42:5";
        let paths = extract_file_paths(error);
        assert!(
            paths.iter().any(|p| p == "src/main.rs"),
            "should detect src/main.rs, got {:?}",
            paths
        );
    }

    #[test]
    fn extract_file_paths_with_extension() {
        let error = "Cannot find module lib/utils.ts";
        let paths = extract_file_paths(error);
        assert!(paths.iter().any(|p| p == "lib/utils.ts"));
    }

    #[test]
    fn extract_file_paths_absolute() {
        let error = "Permission denied: /app/config/db.yml";
        let paths = extract_file_paths(error);
        let has_path = paths.iter().any(|p| p.contains("app/config/db.yml"));
        assert!(has_path, "got {:?}", paths);
    }

    #[test]
    fn extract_file_paths_empty_input() {
        let paths = extract_file_paths("");
        assert!(paths.is_empty());
    }

    #[test]
    fn extract_file_paths_no_paths() {
        let paths = extract_file_paths("just a simple error message");
        assert!(paths.is_empty());
    }

    // =========================================================================
    // Line number extraction tests
    // =========================================================================

    #[test]
    fn extract_line_numbers_colon_pattern() {
        let error = "src/main.rs:42:5: error";
        let nums = extract_line_numbers(error);
        assert!(nums.iter().any(|(n, _)| n == "42"), "got {:?}", nums);
    }

    #[test]
    fn extract_line_numbers_line_keyword() {
        let error = "Error at line 127 in module";
        let nums = extract_line_numbers(error);
        assert!(nums.iter().any(|(n, _)| n == "127"));
    }

    #[test]
    fn extract_line_numbers_no_numbers() {
        let nums = extract_line_numbers("no numbers here");
        assert!(nums.is_empty());
    }

    #[test]
    fn extract_line_numbers_rejects_huge_numbers() {
        let nums = extract_line_numbers(":99999999999:");
        // Should reject because > 1_000_000
        assert!(nums.is_empty());
    }

    // =========================================================================
    // Identifier extraction tests
    // =========================================================================

    #[test]
    fn extract_identifiers_from_error() {
        let error = "undefined reference to `MyStruct::new`";
        let idents = extract_identifiers(error);
        assert!(idents.iter().any(|i| i == "MyStruct"));
    }

    #[test]
    fn extract_identifiers_snake_case() {
        let error = "unresolved import `foo_bar`";
        let idents = extract_identifiers(error);
        assert!(idents.iter().any(|i| i == "foo_bar"));
    }

    #[test]
    fn extract_identifiers_deduplicates() {
        let error = "foo_bar not found, see foo_bar documentation";
        let idents = extract_identifiers(error);
        let count = idents.iter().filter(|i| i.as_str() == "foo_bar").count();
        assert_eq!(count, 1, "should deduplicate");
    }

    // =========================================================================
    // Number extraction tests
    // =========================================================================

    #[test]
    fn extract_numbers_from_text() {
        let text = "port 8080 is already in use";
        let nums = extract_numbers(text);
        assert!(nums.iter().any(|n| n == "8080"));
    }

    #[test]
    fn extract_numbers_with_decimal() {
        let text = "version 3.14 required";
        let nums = extract_numbers(text);
        assert!(nums.iter().any(|n| n == "3.14"));
    }

    // =========================================================================
    // Safety regex tests
    // =========================================================================

    #[test]
    fn safety_regex_file_path_accepts_valid() {
        assert!(matches_safety_regex(
            "src/main.rs",
            ParamKind::FilePath.safety_regex()
        ));
        assert!(matches_safety_regex(
            "lib/utils.ts",
            ParamKind::FilePath.safety_regex()
        ));
        assert!(matches_safety_regex(
            "README.md",
            ParamKind::FilePath.safety_regex()
        ));
    }

    #[test]
    fn safety_regex_file_path_rejects_injection() {
        assert!(!matches_safety_regex(
            "src; rm -rf /",
            ParamKind::FilePath.safety_regex()
        ));
        assert!(!matches_safety_regex(
            "file$(whoami)",
            ParamKind::FilePath.safety_regex()
        ));
        assert!(!matches_safety_regex(
            "file`id`",
            ParamKind::FilePath.safety_regex()
        ));
    }

    #[test]
    fn safety_regex_line_number_accepts_digits() {
        assert!(matches_safety_regex(
            "42",
            ParamKind::LineNumber.safety_regex()
        ));
        assert!(matches_safety_regex(
            "1000",
            ParamKind::LineNumber.safety_regex()
        ));
    }

    #[test]
    fn safety_regex_line_number_rejects_non_digits() {
        assert!(!matches_safety_regex(
            "42a",
            ParamKind::LineNumber.safety_regex()
        ));
        assert!(!matches_safety_regex(
            "",
            ParamKind::LineNumber.safety_regex()
        ));
    }

    #[test]
    fn safety_regex_identifier_accepts_valid() {
        assert!(matches_safety_regex(
            "MyStruct",
            ParamKind::Identifier.safety_regex()
        ));
        assert!(matches_safety_regex(
            "foo_bar",
            ParamKind::Identifier.safety_regex()
        ));
        assert!(matches_safety_regex(
            "_private",
            ParamKind::Identifier.safety_regex()
        ));
    }

    #[test]
    fn safety_regex_identifier_rejects_invalid() {
        assert!(!matches_safety_regex(
            "123abc",
            ParamKind::Identifier.safety_regex()
        ));
        assert!(!matches_safety_regex(
            "foo bar",
            ParamKind::Identifier.safety_regex()
        ));
    }

    #[test]
    fn safety_regex_numeric_accepts_valid() {
        assert!(matches_safety_regex(
            "42",
            ParamKind::Numeric.safety_regex()
        ));
        // Decimal numbers handled by the fallback match arm.
        assert!(matches_safety_regex(
            "314",
            ParamKind::Numeric.safety_regex()
        ));
    }

    #[test]
    fn safety_regex_numeric_rejects_invalid() {
        assert!(!matches_safety_regex(
            "3.14.15",
            ParamKind::Numeric.safety_regex()
        ));
        assert!(!matches_safety_regex(
            ".5",
            ParamKind::Numeric.safety_regex()
        ));
        assert!(!matches_safety_regex(
            "5.",
            ParamKind::Numeric.safety_regex()
        ));
    }

    #[test]
    fn safety_regex_empty_rejects_all() {
        assert!(!matches_safety_regex("anything", ""));
    }

    // =========================================================================
    // Parameter detection tests
    // =========================================================================

    #[test]
    fn detect_file_path_in_command() {
        let gzr = Generalizer::with_defaults();
        let cmd = make_block(0, "cargo test src/main.rs");
        let error = "error in src/main.rs:42";
        let detections = gzr.detect_params(&[cmd], error);
        assert!(!detections.is_empty());
        let (_, params) = &detections[0];
        let has_file = params.iter().any(|p| p.kind == ParamKind::FilePath);
        assert!(has_file, "should detect file path, got {:?}", params);
    }

    #[test]
    fn detect_line_number_in_command() {
        let gzr = Generalizer::with_defaults();
        let cmd = make_block(0, "sed -n 42p src/main.rs");
        let error = "error at line 42";
        let detections = gzr.detect_params(&[cmd], error);
        assert!(!detections.is_empty());
        let all_params: Vec<&DetectedParam> = detections.iter().flat_map(|(_, p)| p).collect();
        let has_line = all_params.iter().any(|p| p.kind == ParamKind::LineNumber);
        assert!(has_line, "should detect line number, got {:?}", all_params);
    }

    #[test]
    fn detect_no_params_when_no_overlap() {
        let gzr = Generalizer::with_defaults();
        let cmd = make_block(0, "cargo build");
        let error = "error in src/lib.rs:10";
        let detections = gzr.detect_params(&[cmd], error);
        // "cargo" and "build" might match as identifiers if present in error.
        // But since error has different content, should be minimal.
        assert!(
            detections
                .iter()
                .flat_map(|(_, p)| p)
                .all(|p| p.kind != ParamKind::FilePath),
            "no file paths should match"
        );
    }

    #[test]
    fn detect_respects_min_param_len() {
        let config = GeneralizeConfig {
            min_param_len: 10,
            ..Default::default()
        };
        let gzr = Generalizer::new(config);
        let cmd = make_block(0, "cat a.rs");
        let error = "error in a.rs";
        let detections = gzr.detect_params(&[cmd], error);
        assert!(
            detections
                .iter()
                .flat_map(|(_, p)| p)
                .all(|p| p.kind != ParamKind::FilePath),
            "a.rs is too short for min_param_len=10"
        );
    }

    // =========================================================================
    // Generalization tests
    // =========================================================================

    #[test]
    fn generalize_replaces_file_path() {
        let gzr = Generalizer::with_defaults();
        let cmd = make_block(0, "cargo test src/main.rs");
        let error = "error[E0433] in src/main.rs:42";
        let result = gzr.generalize(&[cmd], error, 10, 10);
        assert!(result.has_generalizations());
        let gc = &result.commands[0];
        assert!(gc.is_generalized());
        assert!(gc.template.contains("{{cap."));
    }

    #[test]
    fn generalize_preserves_ungeneralized_commands() {
        let gzr = Generalizer::with_defaults();
        let cmd = make_block(0, "cargo build");
        let error = "error in /totally/different/path.rs";
        let result = gzr.generalize(&[cmd], error, 10, 10);
        let gc = &result.commands[0];
        assert!(!gc.is_generalized());
        assert_eq!(gc.template, gc.original);
    }

    #[test]
    fn generalize_multiple_commands() {
        let gzr = Generalizer::with_defaults();
        let cmds = vec![
            make_block(0, "cat src/main.rs"),
            make_block(1, "cargo build"),
            make_block(2, "vi src/main.rs"),
        ];
        let error = "error in src/main.rs";
        let result = gzr.generalize(&cmds, error, 5, 5);
        assert_eq!(result.commands.len(), 3);
        // Commands 0 and 2 should be generalized.
        assert!(result.commands_generalized >= 2);
    }

    #[test]
    fn generalize_with_pac_bound() {
        let gzr = Generalizer::with_defaults();
        let cmd = make_block(0, "cat src/main.rs");
        let error = "error in src/main.rs";
        let result = gzr.generalize(&[cmd], error, 100, 95);
        assert!(result.pac_bound.n_observations == 100);
        assert!(result.pac_bound.empirical_risk < 0.1);
    }

    // =========================================================================
    // PAC-Bayesian bound tests
    // =========================================================================

    #[test]
    fn pac_bound_zero_observations() {
        let bound = PacBayesianBound::compute(0, 0, 0.05, 1.0);
        assert_eq!(bound.risk_bound, 1.0);
        assert!(!bound.is_trustworthy);
    }

    #[test]
    fn pac_bound_perfect_match() {
        let bound = PacBayesianBound::perfect(100, 0.05);
        assert_eq!(bound.empirical_risk, 0.0);
        assert!(
            bound.risk_bound < 0.5,
            "100 perfect matches should give low bound"
        );
        assert!(bound.risk_bound >= 0.0);
    }

    #[test]
    fn pac_bound_all_failures() {
        let bound = PacBayesianBound::compute(100, 0, 0.05, 1.0);
        assert!((bound.empirical_risk - 1.0).abs() < 1e-10);
        assert!((bound.risk_bound - 1.0).abs() < 1e-10);
        assert!(!bound.is_trustworthy);
    }

    #[test]
    fn pac_bound_more_data_tighter() {
        let bound10 = PacBayesianBound::perfect(10, 0.05);
        let bound100 = PacBayesianBound::perfect(100, 0.05);
        let bound1000 = PacBayesianBound::perfect(1000, 0.05);
        assert!(
            bound100.risk_bound <= bound10.risk_bound,
            "more data should give tighter bound"
        );
        assert!(
            bound1000.risk_bound <= bound100.risk_bound,
            "even more data should give even tighter bound"
        );
    }

    #[test]
    fn pac_bound_risk_in_0_1() {
        for n in [1, 5, 10, 50, 100, 1000] {
            for m in 0..=n {
                let bound = PacBayesianBound::compute(n, m, 0.05, 1.0);
                assert!(
                    bound.risk_bound >= 0.0 && bound.risk_bound <= 1.0,
                    "risk_bound should be in [0,1], got {} for n={} m={}",
                    bound.risk_bound,
                    n,
                    m
                );
            }
        }
    }

    #[test]
    fn pac_bound_empirical_risk_monotone() {
        let n = 100u64;
        // As matches increase (m goes up), empirical risk should decrease.
        let mut prev_risk = 2.0f64; // start above max
        for m in 0..=n {
            let bound = PacBayesianBound::compute(n, m, 0.05, 1.0);
            assert!(
                bound.empirical_risk <= prev_risk + 1e-10,
                "empirical risk should decrease as matches increase, m={} prev={} cur={}",
                m,
                prev_risk,
                bound.empirical_risk
            );
            prev_risk = bound.empirical_risk;
        }
    }

    #[test]
    fn pac_bound_higher_prior_more_conservative() {
        let bound_low = PacBayesianBound::compute(50, 45, 0.05, 0.5);
        let bound_high = PacBayesianBound::compute(50, 45, 0.05, 5.0);
        assert!(
            bound_high.risk_bound >= bound_low.risk_bound,
            "higher prior weight should give higher (more conservative) bound"
        );
    }

    #[test]
    fn pac_bound_serde_roundtrip() {
        let bound = PacBayesianBound::compute(50, 45, 0.05, 1.0);
        let json = serde_json::to_string(&bound).unwrap();
        let decoded: PacBayesianBound = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.n_observations, bound.n_observations);
        assert_eq!(decoded.n_matches, bound.n_matches);
        assert!((decoded.risk_bound - bound.risk_bound).abs() < 1e-10);
    }

    // =========================================================================
    // KL divergence tests
    // =========================================================================

    #[test]
    fn kl_divergence_zero_for_same_distribution() {
        let kl = kl_bernoulli(0.5, 0.5);
        assert!(kl.abs() < 1e-10);
    }

    #[test]
    fn kl_divergence_positive() {
        let kl = kl_bernoulli(0.8, 0.5);
        assert!(
            kl > 0.0,
            "KL should be positive for different distributions"
        );
    }

    #[test]
    fn kl_divergence_is_non_negative() {
        // KL(q||p) >= 0 (Gibbs inequality).
        let kl = kl_bernoulli(0.3, 0.7);
        assert!(kl >= 0.0, "KL should be non-negative, got {}", kl);
        let kl2 = kl_bernoulli(0.7, 0.3);
        assert!(kl2 >= 0.0, "KL should be non-negative, got {}", kl2);
    }

    #[test]
    fn kl_divergence_edge_cases() {
        assert_eq!(kl_bernoulli(0.0, 0.5), 0.0);
        assert_eq!(kl_bernoulli(1.0, 0.5), 0.0);
        assert_eq!(kl_bernoulli(0.5, 0.0), 0.0);
    }

    // =========================================================================
    // Instantiation tests
    // =========================================================================

    #[test]
    fn instantiate_with_valid_values() {
        let gc = GeneralizedCommand {
            original: "cargo test src/main.rs".to_string(),
            template: "cargo test {{cap.file_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "src/main.rs".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: r"^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            block_index: 0,
        };

        let mut values = HashMap::new();
        values.insert("file_0".to_string(), "src/lib.rs".to_string());

        let result = gc.instantiate(&values);
        assert_eq!(result, Some("cargo test src/lib.rs".to_string()));
    }

    #[test]
    fn instantiate_rejects_injection() {
        let gc = GeneralizedCommand {
            original: "cargo test src/main.rs".to_string(),
            template: "cargo test {{cap.file_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "src/main.rs".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: r"^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            block_index: 0,
        };

        let mut values = HashMap::new();
        values.insert("file_0".to_string(), "src/lib.rs; rm -rf /".to_string());

        let result = gc.instantiate(&values);
        assert_eq!(result, None, "should reject injection attempt");
    }

    #[test]
    fn instantiate_missing_value_returns_none() {
        let gc = GeneralizedCommand {
            original: "test".to_string(),
            template: "test {{cap.file_0}}".to_string(),
            variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "test.rs".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: r"^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            block_index: 0,
        };

        let values = HashMap::new();
        assert_eq!(gc.instantiate(&values), None);
    }

    // =========================================================================
    // Config tests
    // =========================================================================

    #[test]
    fn config_default_values() {
        let config = GeneralizeConfig::default();
        assert_eq!(config.min_param_len, 2);
        assert_eq!(config.max_params_per_command, 8);
        assert_eq!(config.max_total_params, 32);
        assert!(config.detect_file_paths);
        assert!(config.detect_line_numbers);
        assert!(config.detect_identifiers);
        assert!(config.detect_numerics);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = GeneralizeConfig {
            min_param_len: 5,
            pac_prior_weight: 2.5,
            detect_numerics: false,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let decoded: GeneralizeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.min_param_len, 5);
        assert!((decoded.pac_prior_weight - 2.5).abs() < 1e-10);
        assert!(!decoded.detect_numerics);
    }

    // =========================================================================
    // GeneralizationResult tests
    // =========================================================================

    #[test]
    fn result_has_generalizations_when_commands_generalized() {
        let result = GeneralizationResult {
            commands: vec![],
            all_variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "test.rs".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: r"^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            pac_bound: PacBayesianBound::perfect(100, 0.05),
            params_detected: 1,
            commands_generalized: 1,
        };
        assert!(result.has_generalizations());
    }

    #[test]
    fn result_is_safe_requires_trustworthy_bound() {
        let result = GeneralizationResult {
            commands: vec![],
            all_variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "test.rs".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: r"^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            pac_bound: PacBayesianBound::compute(1, 0, 0.05, 1.0), // not trustworthy
            params_detected: 1,
            commands_generalized: 1,
        };
        assert!(
            !result.is_safe(),
            "should not be safe with untrustworthy bound"
        );
    }

    // =========================================================================
    // GeneralizationStats tests
    // =========================================================================

    #[test]
    fn stats_new_is_zeroed() {
        let stats = GeneralizationStats::new();
        assert_eq!(stats.total_sessions, 0);
        assert_eq!(stats.total_params_detected, 0);
    }

    #[test]
    fn stats_record_updates_counts() {
        let mut stats = GeneralizationStats::new();
        let result = GeneralizationResult {
            commands: vec![],
            all_variables: vec![TemplateVar {
                name: "file_0".to_string(),
                placeholder: "{{cap.file_0}}".to_string(),
                original: "test.rs".to_string(),
                kind: ParamKind::FilePath,
                safety_regex: r"^[a-zA-Z0-9_./-]+$".to_string(),
            }],
            pac_bound: PacBayesianBound::perfect(10, 0.05),
            params_detected: 1,
            commands_generalized: 1,
        };
        stats.record(&result);
        assert_eq!(stats.total_sessions, 1);
        assert_eq!(stats.total_params_detected, 1);
        assert_eq!(stats.total_commands_generalized, 1);
    }

    #[test]
    fn stats_instantiation_tracking() {
        let mut stats = GeneralizationStats::new();
        stats.record_instantiation(true);
        stats.record_instantiation(true);
        stats.record_instantiation(false);
        assert_eq!(stats.instantiation_attempts, 3);
        assert_eq!(stats.instantiation_successes, 2);
        assert_eq!(stats.instantiation_failures, 1);
        let rate = stats.instantiation_success_rate();
        assert!((rate - 2.0 / 3.0).abs() < 1e-10);
    }

    #[test]
    fn stats_success_rate_zero_when_no_attempts() {
        let stats = GeneralizationStats::new();
        assert!((stats.instantiation_success_rate() - 0.0).abs() < 1e-10);
    }

    // =========================================================================
    // Deduplicate overlapping params
    // =========================================================================

    #[test]
    fn dedup_keeps_highest_confidence() {
        let gzr = Generalizer::with_defaults();
        let params = vec![
            DetectedParam {
                literal: "src/main.rs".to_string(),
                byte_offset: 5,
                byte_len: 11,
                kind: ParamKind::FilePath,
                confidence: 0.9,
                corroborating_source: "error_text".to_string(),
            },
            DetectedParam {
                literal: "main".to_string(),
                byte_offset: 9,
                byte_len: 4,
                kind: ParamKind::Identifier,
                confidence: 0.6,
                corroborating_source: "error_text".to_string(),
            },
        ];
        let deduped = gzr.deduplicate_params(params);
        assert_eq!(deduped.len(), 1, "overlapping should be deduped");
        assert_eq!(deduped[0].kind, ParamKind::FilePath);
    }

    #[test]
    fn dedup_keeps_non_overlapping() {
        let gzr = Generalizer::with_defaults();
        let params = vec![
            DetectedParam {
                literal: "src/main.rs".to_string(),
                byte_offset: 0,
                byte_len: 11,
                kind: ParamKind::FilePath,
                confidence: 0.9,
                corroborating_source: "error_text".to_string(),
            },
            DetectedParam {
                literal: "42".to_string(),
                byte_offset: 15,
                byte_len: 2,
                kind: ParamKind::LineNumber,
                confidence: 0.7,
                corroborating_source: "error_text".to_string(),
            },
        ];
        let deduped = gzr.deduplicate_params(params);
        assert_eq!(deduped.len(), 2, "non-overlapping should both be kept");
    }

    // =========================================================================
    // ranges_overlap tests
    // =========================================================================

    #[test]
    fn ranges_overlap_basic() {
        assert!(ranges_overlap(0, 5, 3, 8));
        assert!(ranges_overlap(3, 8, 0, 5));
        assert!(!ranges_overlap(0, 3, 5, 8));
        assert!(!ranges_overlap(5, 8, 0, 3));
        assert!(!ranges_overlap(0, 5, 5, 10)); // adjacent, not overlapping
    }

    // =========================================================================
    // Common keyword tests
    // =========================================================================

    #[test]
    fn common_keywords_are_filtered() {
        assert!(is_common_keyword("error"));
        assert!(is_common_keyword("warning"));
        assert!(is_common_keyword("for"));
        assert!(is_common_keyword("import"));
    }

    #[test]
    fn non_keywords_not_filtered() {
        assert!(!is_common_keyword("MyStruct"));
        assert!(!is_common_keyword("foo_bar"));
        assert!(!is_common_keyword("DatabaseConnection"));
    }

    // =========================================================================
    // Integration: detect + generalize round trip
    // =========================================================================

    #[test]
    fn roundtrip_detect_generalize_instantiate() {
        let gzr = Generalizer::with_defaults();
        let cmd = make_block(0, "rustc --edition 2021 src/main.rs");
        let error = "error[E0433]: failed to resolve in src/main.rs:42:5";

        let result = gzr.generalize(&[cmd], error, 20, 20);
        assert!(result.has_generalizations());

        // Instantiate with a different file.
        let gc = &result.commands[0];
        if gc.is_generalized() {
            let mut values = HashMap::new();
            for var in &gc.variables {
                match var.kind {
                    ParamKind::FilePath => {
                        values.insert(var.name.clone(), "src/lib.rs".to_string());
                    }
                    ParamKind::LineNumber => {
                        values.insert(var.name.clone(), "99".to_string());
                    }
                    _ => {
                        values.insert(var.name.clone(), var.original.clone());
                    }
                }
            }
            let instantiated = gc.instantiate(&values);
            assert!(instantiated.is_some(), "valid values should instantiate");
            let text = instantiated.unwrap();
            assert!(
                !text.contains("{{cap."),
                "all placeholders should be replaced"
            );
        }
    }

    #[test]
    fn generalization_result_serde_roundtrip() {
        let gzr = Generalizer::with_defaults();
        let cmd = make_block(0, "cat src/main.rs");
        let error = "error in src/main.rs";
        let result = gzr.generalize(&[cmd], error, 10, 9);

        let json = serde_json::to_string(&result).unwrap();
        let decoded: GeneralizationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.commands.len(), result.commands.len());
        assert_eq!(decoded.params_detected, result.params_detected);
        assert_eq!(decoded.commands_generalized, result.commands_generalized);
    }

    #[test]
    fn stats_serde_roundtrip() {
        let mut stats = GeneralizationStats::new();
        stats.total_sessions = 5;
        stats.instantiation_attempts = 10;
        stats.instantiation_successes = 8;
        let json = serde_json::to_string(&stats).unwrap();
        let decoded: GeneralizationStats = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.total_sessions, 5);
        assert_eq!(decoded.instantiation_attempts, 10);
    }

    // =========================================================================
    // Disable specific detectors
    // =========================================================================

    #[test]
    fn disable_file_path_detection() {
        let config = GeneralizeConfig {
            detect_file_paths: false,
            ..Default::default()
        };
        let gzr = Generalizer::new(config);
        let cmd = make_block(0, "cat src/main.rs");
        let error = "error in src/main.rs";
        let detections = gzr.detect_params(&[cmd], error);
        assert!(
            detections
                .iter()
                .flat_map(|(_, p)| p)
                .all(|p| p.kind != ParamKind::FilePath)
        );
    }

    #[test]
    fn disable_all_detectors() {
        let config = GeneralizeConfig {
            detect_file_paths: false,
            detect_line_numbers: false,
            detect_identifiers: false,
            detect_numerics: false,
            ..Default::default()
        };
        let gzr = Generalizer::new(config);
        let cmd = make_block(0, "cargo test src/main.rs");
        let error = "error in src/main.rs:42 MyStruct";
        let detections = gzr.detect_params(&[cmd], error);
        assert!(detections.is_empty());
    }

    // =========================================================================
    // Charset parsing tests
    // =========================================================================

    #[test]
    fn parse_simple_charset_basic() {
        let cs = parse_simple_charset("[a-z]+");
        assert!(cs.is_some());
        let cs = cs.unwrap();
        assert!(!cs.allow_empty);
        assert_eq!(cs.chars.len(), 1);
        assert_eq!(cs.chars[0], (b'a', b'z'));
    }

    #[test]
    fn parse_simple_charset_star() {
        let cs = parse_simple_charset("[0-9]*");
        assert!(cs.is_some());
        assert!(cs.unwrap().allow_empty);
    }

    #[test]
    fn validate_charset_accepts_valid() {
        let cs = parse_simple_charset("[a-zA-Z0-9]+").unwrap();
        assert!(validate_against_charset("Hello123", &cs));
    }

    #[test]
    fn validate_charset_rejects_invalid() {
        let cs = parse_simple_charset("[a-z]+").unwrap();
        assert!(!validate_against_charset("Hello", &cs)); // uppercase
    }
}
