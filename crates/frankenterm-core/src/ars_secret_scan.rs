//! ARS pipeline secret & PII scanner using Aho-Corasick multi-pattern matching.
//!
//! When the MDL extractor produces a minimal recovery sequence, this module
//! scans the commands for leaked secrets, API tokens, and high-entropy strings.
//! If contamination is found, the ARS pipeline **permanently aborts** for
//! that cluster — we refuse to learn a potentially unsafe reflex.
//!
//! # Architecture
//!
//! 1. **Aho-Corasick automaton**: precompiled multi-pattern matcher for known
//!    secret prefixes (AWS, OpenAI, GitHub, Stripe, etc.)
//! 2. **Shannon entropy detector**: catches unknown high-entropy tokens that
//!    may be secrets not matching any known prefix pattern
//! 3. **Verdict**: `Clean` or `Contaminated` with detailed findings
//!
//! # Performance
//!
//! Aho-Corasick runs in O(n + m) where n = text length, m = number of matches.
//! Shannon entropy is O(n) per token. Combined: < 100μs for typical sequences.

use std::collections::HashMap;

use aho_corasick::AhoCorasick;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::mdl_extraction::CommandBlock;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the ARS secret scanner.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ArsScanConfig {
    /// Minimum Shannon entropy (bits/char) to flag a token as high-entropy.
    /// Typical range: 3.5–4.5. Higher = fewer false positives, more misses.
    pub entropy_threshold: f64,
    /// Minimum token length to evaluate for entropy (short tokens are noisy).
    pub min_entropy_token_len: usize,
    /// Maximum token length to evaluate (skip very long tokens as they're
    /// likely base64-encoded data, not secrets).
    pub max_entropy_token_len: usize,
    /// Whether to scan command output previews (not just command text).
    pub scan_output: bool,
    /// Whether to enable entropy-based detection (in addition to pattern matching).
    pub entropy_detection_enabled: bool,
    /// Additional literal patterns to scan for (beyond built-in).
    pub extra_patterns: Vec<String>,
}

impl Default for ArsScanConfig {
    fn default() -> Self {
        Self {
            entropy_threshold: 4.0,
            min_entropy_token_len: 16,
            max_entropy_token_len: 256,
            scan_output: true,
            entropy_detection_enabled: true,
            extra_patterns: Vec::new(),
        }
    }
}

// =============================================================================
// Built-in secret prefix patterns
// =============================================================================

/// Known secret token prefixes for Aho-Corasick matching.
/// These are literal string prefixes, not regexes — fast O(1) per character.
const BUILTIN_PATTERNS: &[(&str, &str)] = &[
    // OpenAI
    ("openai_key", "sk-"),
    ("openai_proj", "sk-proj-"),
    // Anthropic
    ("anthropic_key", "sk-ant-"),
    // GitHub
    ("github_pat", "ghp_"),
    ("github_oauth", "gho_"),
    ("github_app", "ghs_"),
    ("github_refresh", "ghr_"),
    // AWS
    ("aws_access_key", "AKIA"),
    ("aws_secret_key", "aws_secret_access_key"),
    ("aws_session", "FwoGZX"),
    // Slack
    ("slack_bot", "xoxb-"),
    ("slack_user", "xoxp-"),
    ("slack_app", "xapp-"),
    // Stripe
    ("stripe_live", "sk_live_"),
    ("stripe_test", "sk_test_"),
    ("stripe_pub", "pk_live_"),
    // Google
    ("google_api", "AIza"),
    // Twilio
    ("twilio_key", "SK"),
    // SendGrid
    ("sendgrid_key", "SG."),
    // Database URLs with credentials
    ("postgres_url", "postgres://"),
    ("mysql_url", "mysql://"),
    ("mongodb_url", "mongodb+srv://"),
    ("redis_url", "redis://"),
    // Generic patterns
    ("bearer_token", "Bearer "),
    ("basic_auth", "Basic "),
    ("password_eq", "password="),
    ("passwd_eq", "passwd="),
    ("secret_eq", "secret="),
    ("token_eq", "token="),
    ("api_key_eq", "api_key="),
    ("apikey_eq", "apikey="),
    // SSH/PGP
    ("ssh_private", "-----BEGIN"),
    ("pgp_private", "-----BEGIN PGP PRIVATE"),
    // npm/pypi tokens
    ("npm_token", "npm_"),
    ("pypi_token", "pypi-"),
    // Vercel
    ("vercel_token", "vercel_"),
    // Supabase
    ("supabase_key", "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9"),
    // Datadog
    ("datadog_key", "dd-api-"),
    // Cloudflare
    ("cloudflare_key", "cf-"),
];

// =============================================================================
// Scanner
// =============================================================================

/// ARS secret/PII scanner using Aho-Corasick multi-pattern matching.
pub struct ArsSecretScanner {
    config: ArsScanConfig,
    automaton: AhoCorasick,
    /// Maps automaton pattern index → (pattern_name, pattern_text).
    pattern_names: Vec<(&'static str, &'static str)>,
    /// Extra patterns start at this index in the automaton.
    extra_pattern_offset: usize,
}

impl ArsSecretScanner {
    /// Create a scanner with the given configuration.
    #[must_use]
    pub fn new(config: ArsScanConfig) -> Self {
        let mut patterns: Vec<&str> = BUILTIN_PATTERNS.iter().map(|(_, pat)| *pat).collect();
        let extra_offset = patterns.len();

        for extra in &config.extra_patterns {
            patterns.push(extra.as_str());
        }

        let automaton = AhoCorasick::new(&patterns).expect("valid Aho-Corasick patterns");

        let pattern_names: Vec<(&'static str, &'static str)> = BUILTIN_PATTERNS.to_vec();

        Self {
            config,
            automaton,
            pattern_names,
            extra_pattern_offset: extra_offset,
        }
    }

    /// Create a scanner with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(ArsScanConfig::default())
    }

    /// Scan a sequence of MDL-extracted commands for secrets.
    ///
    /// Returns `ScanVerdict::Clean` if no secrets found, or
    /// `ScanVerdict::Contaminated` with details about what was found.
    #[must_use]
    pub fn scan_commands(&self, commands: &[CommandBlock]) -> ScanVerdict {
        let mut findings = Vec::new();

        for cmd in commands {
            // Scan command text.
            self.scan_text(&cmd.command, cmd.index, "command", &mut findings);

            // Optionally scan output preview.
            if self.config.scan_output {
                if let Some(output) = &cmd.output_preview {
                    self.scan_text(output, cmd.index, "output", &mut findings);
                }
            }
        }

        if findings.is_empty() {
            debug!(commands = commands.len(), "ARS secret scan: clean");
            ScanVerdict::Clean
        } else {
            warn!(
                commands = commands.len(),
                findings = findings.len(),
                "ARS secret scan: CONTAMINATED — pipeline aborted"
            );
            ScanVerdict::Contaminated(ScanContamination {
                findings,
                abort_reason: "Secret or PII detected in MDL-extracted commands. \
                    ARS pipeline permanently aborted for this cluster."
                    .to_string(),
            })
        }
    }

    /// Scan a single text string for secrets.
    ///
    /// Returns findings (empty if clean).
    #[must_use]
    pub fn scan_text_standalone(&self, text: &str) -> Vec<ScanFinding> {
        let mut findings = Vec::new();
        self.scan_text(text, 0, "standalone", &mut findings);
        findings
    }

    /// Internal: scan a text for patterns and entropy.
    fn scan_text(
        &self,
        text: &str,
        block_index: u32,
        source: &str,
        findings: &mut Vec<ScanFinding>,
    ) {
        // Phase 1: Aho-Corasick pattern matching.
        for mat in self.automaton.find_iter(text) {
            let pattern_idx = mat.pattern().as_usize();
            let (pattern_name, detection_method) = if pattern_idx < self.extra_pattern_offset {
                (
                    self.pattern_names[pattern_idx].0,
                    DetectionMethod::PatternMatch,
                )
            } else {
                ("custom_pattern", DetectionMethod::PatternMatch)
            };

            // Extract surrounding context (up to 10 chars before, 20 after).
            let ctx_start = mat.start().saturating_sub(10);
            let ctx_end = (mat.end() + 20).min(text.len());
            let context = &text[ctx_start..ctx_end];

            findings.push(ScanFinding {
                pattern_name: pattern_name.to_string(),
                block_index,
                source: source.to_string(),
                byte_offset: mat.start(),
                match_len: mat.end() - mat.start(),
                context_redacted: redact_context(context),
                detection_method,
                entropy: None,
            });
        }

        // Phase 2: Shannon entropy detection for unknown tokens.
        if self.config.entropy_detection_enabled {
            self.scan_entropy(text, block_index, source, findings);
        }
    }

    /// Scan for high-entropy tokens using Shannon entropy.
    fn scan_entropy(
        &self,
        text: &str,
        block_index: u32,
        source: &str,
        findings: &mut Vec<ScanFinding>,
    ) {
        // Split on whitespace and common delimiters.
        for token in
            text.split(|c: char| c.is_whitespace() || c == '=' || c == ':' || c == '"' || c == '\'')
        {
            let len = token.len();
            if len < self.config.min_entropy_token_len || len > self.config.max_entropy_token_len {
                continue;
            }

            // Skip tokens that are clearly not secrets (all same char, all digits, etc.)
            if is_trivially_low_entropy(token) {
                continue;
            }

            let entropy = shannon_entropy(token);
            if entropy >= self.config.entropy_threshold {
                // Check if this token was already caught by pattern matching.
                let already_found = findings.iter().any(|f| {
                    f.block_index == block_index
                        && f.source == source
                        && f.detection_method == DetectionMethod::PatternMatch
                        && text[f.byte_offset..f.byte_offset + f.match_len].contains(token)
                });

                if !already_found {
                    findings.push(ScanFinding {
                        pattern_name: "high_entropy".to_string(),
                        block_index,
                        source: source.to_string(),
                        byte_offset: 0, // Approximate; not byte-exact for tokens.
                        match_len: len,
                        context_redacted: format!(
                            "[HIGH_ENTROPY len={} entropy={:.2}]",
                            len, entropy
                        ),
                        detection_method: DetectionMethod::EntropyThreshold,
                        entropy: Some(entropy),
                    });
                }
            }
        }
    }
}

// =============================================================================
// Scan verdict & findings
// =============================================================================

/// Result of scanning MDL-extracted commands.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ScanVerdict {
    /// No secrets or PII found — safe to continue ARS pipeline.
    Clean,
    /// Secrets detected — ARS pipeline MUST be permanently aborted.
    Contaminated(ScanContamination),
}

impl ScanVerdict {
    /// Whether the scan found contamination.
    #[must_use]
    pub fn is_contaminated(&self) -> bool {
        matches!(self, ScanVerdict::Contaminated(_))
    }

    /// Whether the scan is clean.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        matches!(self, ScanVerdict::Clean)
    }
}

/// Details of contamination found during scanning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanContamination {
    /// Individual findings.
    pub findings: Vec<ScanFinding>,
    /// Human-readable abort reason.
    pub abort_reason: String,
}

/// A single secret/PII finding.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScanFinding {
    /// Pattern name that matched (e.g., "openai_key", "high_entropy").
    pub pattern_name: String,
    /// Index of the CommandBlock where the finding occurred.
    pub block_index: u32,
    /// Source within the block ("command" or "output").
    pub source: String,
    /// Byte offset within the source text.
    pub byte_offset: usize,
    /// Length of the match in bytes.
    pub match_len: usize,
    /// Redacted context around the match (safe to log).
    pub context_redacted: String,
    /// How this finding was detected.
    pub detection_method: DetectionMethod,
    /// Shannon entropy (only for entropy-based detections).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entropy: Option<f64>,
}

/// How a secret was detected.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DetectionMethod {
    /// Aho-Corasick pattern match.
    PatternMatch,
    /// Shannon entropy threshold exceeded.
    EntropyThreshold,
}

// =============================================================================
// Scan statistics
// =============================================================================

/// Aggregate scan statistics across multiple ARS pipeline runs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScanStats {
    /// Total scans performed.
    pub total_scans: u64,
    /// Scans that were clean.
    pub clean_count: u64,
    /// Scans that found contamination.
    pub contaminated_count: u64,
    /// Total findings across all scans.
    pub total_findings: u64,
    /// Findings per pattern name.
    pub findings_by_pattern: HashMap<String, u64>,
    /// Findings per detection method.
    pub findings_by_method: HashMap<String, u64>,
}

impl ScanStats {
    /// Create empty stats.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a scan verdict.
    pub fn record(&mut self, verdict: &ScanVerdict) {
        self.total_scans += 1;
        match verdict {
            ScanVerdict::Clean => self.clean_count += 1,
            ScanVerdict::Contaminated(contamination) => {
                self.contaminated_count += 1;
                for finding in &contamination.findings {
                    self.total_findings += 1;
                    *self
                        .findings_by_pattern
                        .entry(finding.pattern_name.clone())
                        .or_insert(0) += 1;
                    let method = format!("{:?}", finding.detection_method);
                    *self.findings_by_method.entry(method).or_insert(0) += 1;
                }
            }
        }
    }
}

// =============================================================================
// Shannon entropy
// =============================================================================

/// Compute Shannon entropy in bits per character for a string.
///
/// Returns 0.0 for empty strings. Maximum is log2(alphabet_size).
#[must_use]
pub fn shannon_entropy(s: &str) -> f64 {
    if s.is_empty() {
        return 0.0;
    }

    let mut freq = [0u32; 256];
    let len = s.len() as f64;

    for byte in s.bytes() {
        freq[byte as usize] += 1;
    }

    let mut entropy = 0.0f64;
    for &count in &freq {
        if count > 0 {
            let p = count as f64 / len;
            entropy -= p * p.log2();
        }
    }

    entropy
}

/// Check if a token is trivially low-entropy (all same char, all digits, etc.)
fn is_trivially_low_entropy(token: &str) -> bool {
    if token.is_empty() {
        return true;
    }

    let bytes = token.as_bytes();

    // All same character.
    if bytes.iter().all(|&b| b == bytes[0]) {
        return true;
    }

    // All ASCII digits.
    if bytes.iter().all(|b| b.is_ascii_digit()) {
        return true;
    }

    // All ASCII lowercase.
    if bytes.iter().all(|b| b.is_ascii_lowercase()) {
        return true;
    }

    // Known benign paths.
    if token.starts_with('/') || token.starts_with("./") || token.starts_with("../") {
        return true;
    }

    false
}

/// Redact the middle of a context string, keeping only the edges.
fn redact_context(context: &str) -> String {
    if context.len() <= 8 {
        return "[REDACTED]".to_string();
    }
    let prefix = &context[..4];
    let suffix_start = context.len().saturating_sub(4);
    let suffix = &context[suffix_start..];
    format!("{}...{}", prefix, suffix)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdl_extraction::CommandBlock;

    fn make_cmd(index: u32, command: &str, exit_code: Option<i32>) -> CommandBlock {
        CommandBlock {
            index,
            command: command.to_string(),
            exit_code,
            duration_us: Some(1000),
            output_preview: None,
            timestamp_us: (index as u64 + 1) * 1_000_000,
        }
    }

    fn make_cmd_with_output(
        index: u32,
        command: &str,
        exit_code: Option<i32>,
        output: &str,
    ) -> CommandBlock {
        CommandBlock {
            index,
            command: command.to_string(),
            exit_code,
            duration_us: Some(1000),
            output_preview: Some(output.to_string()),
            timestamp_us: (index as u64 + 1) * 1_000_000,
        }
    }

    fn default_scanner() -> ArsSecretScanner {
        ArsSecretScanner::with_defaults()
    }

    // -------------------------------------------------------------------------
    // ArsScanConfig
    // -------------------------------------------------------------------------

    #[test]
    fn config_defaults() {
        let cfg = ArsScanConfig::default();
        assert!((cfg.entropy_threshold - 4.0).abs() < f64::EPSILON);
        assert_eq!(cfg.min_entropy_token_len, 16);
        assert_eq!(cfg.max_entropy_token_len, 256);
        assert!(cfg.scan_output);
        assert!(cfg.entropy_detection_enabled);
        assert!(cfg.extra_patterns.is_empty());
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = ArsScanConfig {
            entropy_threshold: 3.5,
            min_entropy_token_len: 20,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: ArsScanConfig = serde_json::from_str(&json).unwrap();
        assert!((decoded.entropy_threshold - 3.5).abs() < 1e-10);
        assert_eq!(decoded.min_entropy_token_len, 20);
    }

    // -------------------------------------------------------------------------
    // Shannon entropy
    // -------------------------------------------------------------------------

    #[test]
    fn entropy_empty_string() {
        assert!((shannon_entropy("") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn entropy_single_char() {
        assert!((shannon_entropy("a") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn entropy_uniform_binary() {
        // Equal distribution of two chars → 1 bit.
        let e = shannon_entropy("abababababababab");
        assert!((e - 1.0).abs() < 0.01, "entropy should be ~1.0, got {}", e);
    }

    #[test]
    fn entropy_all_same() {
        assert!((shannon_entropy("aaaaaaaaaa") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn entropy_high_for_random_looking() {
        // A string with many distinct characters should have high entropy.
        let e = shannon_entropy("aB3$xY7!mN9@kL2&");
        assert!(e > 3.5, "entropy should be > 3.5, got {}", e);
    }

    #[test]
    fn entropy_moderate_for_english() {
        let e = shannon_entropy("the quick brown fox jumps over");
        assert!(
            e > 2.0 && e < 4.5,
            "english entropy should be 2-4.5, got {}",
            e
        );
    }

    // -------------------------------------------------------------------------
    // Trivially low entropy
    // -------------------------------------------------------------------------

    #[test]
    fn trivial_all_same() {
        assert!(is_trivially_low_entropy("aaaaaaaaaa"));
    }

    #[test]
    fn trivial_all_digits() {
        assert!(is_trivially_low_entropy("1234567890"));
    }

    #[test]
    fn trivial_all_lowercase() {
        assert!(is_trivially_low_entropy("abcdefghij"));
    }

    #[test]
    fn trivial_path() {
        assert!(is_trivially_low_entropy("/usr/local/bin"));
    }

    #[test]
    fn not_trivial_mixed() {
        assert!(!is_trivially_low_entropy("aB3$xY7!"));
    }

    #[test]
    fn trivial_empty() {
        assert!(is_trivially_low_entropy(""));
    }

    // -------------------------------------------------------------------------
    // Redact context
    // -------------------------------------------------------------------------

    #[test]
    fn redact_short_context() {
        assert_eq!(redact_context("ab"), "[REDACTED]");
    }

    #[test]
    fn redact_long_context() {
        let result = redact_context("hello world, this is a test");
        assert!(result.starts_with("hell"));
        assert!(result.ends_with("test"));
        assert!(result.contains("..."));
    }

    // -------------------------------------------------------------------------
    // Pattern matching — known tokens
    // -------------------------------------------------------------------------

    #[test]
    fn detects_openai_key_in_command() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(
            0,
            "export OPENAI_API_KEY=sk-abc123456789",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_github_token_in_command() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(
            0,
            "git clone https://ghp_token123456@github.com/repo",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_aws_key_in_command() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(
            0,
            "export AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_stripe_key() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(
            0,
            "curl -H 'Authorization: Bearer sk_live_abc123'",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_database_url() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(
            0,
            "export DATABASE_URL=postgres://admin:secret@db:5432/prod",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_password_assignment() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(0, "mysql -u root password=hunter2", Some(0))];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_bearer_token() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(
            0,
            "curl -H 'Authorization: Bearer eyJhbGciOi...'",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_ssh_private_key() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(
            0,
            "echo '-----BEGIN RSA PRIVATE KEY-----' > key.pem",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_slack_token() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(
            0,
            "export SLACK_TOKEN=xoxb-1234567890-abc",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    #[test]
    fn detects_anthropic_key() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd(0, "ANTHROPIC_API_KEY=sk-ant-api03-XXXXX", Some(0))];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    // -------------------------------------------------------------------------
    // Clean commands
    // -------------------------------------------------------------------------

    #[test]
    fn clean_normal_commands() {
        let scanner = default_scanner();
        let cmds = vec![
            make_cmd(0, "cd /project", Some(0)),
            make_cmd(1, "cargo build", Some(0)),
            make_cmd(2, "cargo test", Some(0)),
        ];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_clean());
    }

    #[test]
    fn clean_empty_commands() {
        let scanner = default_scanner();
        let verdict = scanner.scan_commands(&[]);
        assert!(verdict.is_clean());
    }

    #[test]
    fn clean_common_shell_commands() {
        let scanner = default_scanner();
        let cmds = vec![
            make_cmd(0, "ls -la /tmp", Some(0)),
            make_cmd(1, "grep -r 'TODO' src/", Some(0)),
            make_cmd(2, "cat README.md", Some(0)),
            make_cmd(3, "git status", Some(0)),
        ];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_clean());
    }

    // -------------------------------------------------------------------------
    // Output scanning
    // -------------------------------------------------------------------------

    #[test]
    fn detects_secret_in_output() {
        let scanner = default_scanner();
        let cmds = vec![make_cmd_with_output(
            0,
            "cat .env",
            Some(0),
            "OPENAI_API_KEY=sk-secret12345678",
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
        if let ScanVerdict::Contaminated(c) = &verdict {
            assert!(c.findings.iter().any(|f| f.source == "output"));
        }
    }

    #[test]
    fn output_scanning_disabled() {
        let config = ArsScanConfig {
            scan_output: false,
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let cmds = vec![make_cmd_with_output(
            0,
            "cat .env",
            Some(0),
            "OPENAI_API_KEY=sk-secret12345678",
        )];
        let verdict = scanner.scan_commands(&cmds);
        // Command "cat .env" is clean; output has the secret but scanning is disabled.
        assert!(verdict.is_clean());
    }

    // -------------------------------------------------------------------------
    // Entropy detection
    // -------------------------------------------------------------------------

    #[test]
    fn entropy_detects_unknown_high_entropy_token() {
        let scanner = default_scanner();
        // This looks like a random API key but doesn't match any known prefix.
        let cmds = vec![make_cmd(
            0,
            "export CUSTOM_KEY=Zx9kQ3mW7bRt5Yp8Cn2FvJ6dLs4A",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
        if let ScanVerdict::Contaminated(c) = &verdict {
            let has_entropy = c
                .findings
                .iter()
                .any(|f| f.detection_method == DetectionMethod::EntropyThreshold);
            assert!(has_entropy, "should have entropy-based detection");
        }
    }

    #[test]
    fn entropy_disabled_skips_detection() {
        let config = ArsScanConfig {
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        // High-entropy token but no known prefix.
        let cmds = vec![make_cmd(
            0,
            "export FOO=Zx9kQ3mW7bRt5Yp8Cn2FvJ6dLs4A",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        // Pattern match may or may not catch this; but entropy won't.
        // Check there are no entropy findings.
        match &verdict {
            ScanVerdict::Clean => {} // fine
            ScanVerdict::Contaminated(c) => {
                let has_entropy = c
                    .findings
                    .iter()
                    .any(|f| f.detection_method == DetectionMethod::EntropyThreshold);
                assert!(!has_entropy, "entropy detection should be disabled");
            }
        }
    }

    // -------------------------------------------------------------------------
    // Multiple findings
    // -------------------------------------------------------------------------

    #[test]
    fn multiple_secrets_in_sequence() {
        let scanner = default_scanner();
        let cmds = vec![
            make_cmd(0, "export OPENAI_API_KEY=sk-key123456", Some(0)),
            make_cmd(1, "git clone https://ghp_token@github.com/repo", Some(0)),
            make_cmd(2, "export DB=postgres://root:pass@db:5432/prod", Some(0)),
        ];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
        if let ScanVerdict::Contaminated(c) = &verdict {
            assert!(
                c.findings.len() >= 3,
                "should find at least 3 findings, got {}",
                c.findings.len()
            );
        }
    }

    // -------------------------------------------------------------------------
    // ScanVerdict
    // -------------------------------------------------------------------------

    #[test]
    fn verdict_clean_methods() {
        let v = ScanVerdict::Clean;
        assert!(v.is_clean());
        assert!(!v.is_contaminated());
    }

    #[test]
    fn verdict_contaminated_methods() {
        let v = ScanVerdict::Contaminated(ScanContamination {
            findings: vec![],
            abort_reason: "test".to_string(),
        });
        assert!(v.is_contaminated());
        assert!(!v.is_clean());
    }

    #[test]
    fn verdict_serde_roundtrip_clean() {
        let v = ScanVerdict::Clean;
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ScanVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn verdict_serde_roundtrip_contaminated() {
        let v = ScanVerdict::Contaminated(ScanContamination {
            findings: vec![ScanFinding {
                pattern_name: "openai_key".to_string(),
                block_index: 0,
                source: "command".to_string(),
                byte_offset: 10,
                match_len: 3,
                context_redacted: "...sk-...".to_string(),
                detection_method: DetectionMethod::PatternMatch,
                entropy: None,
            }],
            abort_reason: "test".to_string(),
        });
        let json = serde_json::to_string(&v).unwrap();
        let decoded: ScanVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, v);
    }

    // -------------------------------------------------------------------------
    // ScanStats
    // -------------------------------------------------------------------------

    #[test]
    fn stats_empty() {
        let stats = ScanStats::new();
        assert_eq!(stats.total_scans, 0);
        assert_eq!(stats.clean_count, 0);
        assert_eq!(stats.contaminated_count, 0);
    }

    #[test]
    fn stats_record_clean() {
        let mut stats = ScanStats::new();
        stats.record(&ScanVerdict::Clean);
        assert_eq!(stats.total_scans, 1);
        assert_eq!(stats.clean_count, 1);
        assert_eq!(stats.contaminated_count, 0);
    }

    #[test]
    fn stats_record_contaminated() {
        let mut stats = ScanStats::new();
        let verdict = ScanVerdict::Contaminated(ScanContamination {
            findings: vec![ScanFinding {
                pattern_name: "openai_key".to_string(),
                block_index: 0,
                source: "command".to_string(),
                byte_offset: 0,
                match_len: 3,
                context_redacted: "...".to_string(),
                detection_method: DetectionMethod::PatternMatch,
                entropy: None,
            }],
            abort_reason: "test".to_string(),
        });
        stats.record(&verdict);
        assert_eq!(stats.total_scans, 1);
        assert_eq!(stats.contaminated_count, 1);
        assert_eq!(stats.total_findings, 1);
        assert_eq!(*stats.findings_by_pattern.get("openai_key").unwrap(), 1);
    }

    #[test]
    fn stats_accumulate() {
        let mut stats = ScanStats::new();
        stats.record(&ScanVerdict::Clean);
        stats.record(&ScanVerdict::Clean);
        let contaminated = ScanVerdict::Contaminated(ScanContamination {
            findings: vec![
                ScanFinding {
                    pattern_name: "aws_access_key".to_string(),
                    block_index: 0,
                    source: "command".to_string(),
                    byte_offset: 0,
                    match_len: 4,
                    context_redacted: "...".to_string(),
                    detection_method: DetectionMethod::PatternMatch,
                    entropy: None,
                },
                ScanFinding {
                    pattern_name: "high_entropy".to_string(),
                    block_index: 1,
                    source: "output".to_string(),
                    byte_offset: 0,
                    match_len: 20,
                    context_redacted: "...".to_string(),
                    detection_method: DetectionMethod::EntropyThreshold,
                    entropy: Some(4.2),
                },
            ],
            abort_reason: "test".to_string(),
        });
        stats.record(&contaminated);

        assert_eq!(stats.total_scans, 3);
        assert_eq!(stats.clean_count, 2);
        assert_eq!(stats.contaminated_count, 1);
        assert_eq!(stats.total_findings, 2);
    }

    // -------------------------------------------------------------------------
    // Extra patterns
    // -------------------------------------------------------------------------

    #[test]
    fn extra_patterns_detected() {
        let config = ArsScanConfig {
            extra_patterns: vec!["CUSTOM_SECRET_PREFIX_".to_string()],
            entropy_detection_enabled: false,
            ..Default::default()
        };
        let scanner = ArsSecretScanner::new(config);
        let cmds = vec![make_cmd(
            0,
            "export KEY=CUSTOM_SECRET_PREFIX_abc123",
            Some(0),
        )];
        let verdict = scanner.scan_commands(&cmds);
        assert!(verdict.is_contaminated());
    }

    // -------------------------------------------------------------------------
    // scan_text_standalone
    // -------------------------------------------------------------------------

    #[test]
    fn standalone_scan_finds_secrets() {
        let scanner = default_scanner();
        let findings = scanner.scan_text_standalone("my key is sk-abc123 and ghp_token");
        assert!(findings.len() >= 2, "should find at least 2 patterns");
    }

    #[test]
    fn standalone_scan_clean() {
        let scanner = default_scanner();
        let findings = scanner.scan_text_standalone("just normal text here");
        // Entropy might trigger on some tokens, so check pattern matches specifically.
        assert!(
            findings
                .iter()
                .all(|f| f.detection_method != DetectionMethod::PatternMatch)
        );
    }

    // -------------------------------------------------------------------------
    // Detection method serde
    // -------------------------------------------------------------------------

    #[test]
    fn detection_method_serde() {
        for method in [
            DetectionMethod::PatternMatch,
            DetectionMethod::EntropyThreshold,
        ] {
            let json = serde_json::to_string(&method).unwrap();
            let decoded: DetectionMethod = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, method);
        }
    }

    // -------------------------------------------------------------------------
    // Context redaction in findings
    // -------------------------------------------------------------------------

    #[test]
    fn findings_never_contain_full_secret() {
        let scanner = default_scanner();
        let secret = "sk-abc1234567890abcdef1234567890abcdef12345678";
        let cmds = vec![make_cmd(0, &format!("export KEY={secret}"), Some(0))];
        let verdict = scanner.scan_commands(&cmds);
        if let ScanVerdict::Contaminated(c) = &verdict {
            for finding in &c.findings {
                assert!(
                    !finding.context_redacted.contains(secret),
                    "context should not contain full secret"
                );
            }
        }
    }
}
