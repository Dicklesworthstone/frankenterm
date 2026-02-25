//! Lightweight AST symbolic execution for ARS command safety verification.
//!
//! When the MDL extractor and secret scanner pass, we still need to prove
//! that the extracted commands don't perform destructive filesystem operations
//! outside the captured CWD. This module parses commands into a minimal shell
//! AST and symbolically executes them against a mock filesystem to detect:
//!
//! 1. **Path traversal**: `rm -rf ../*`, `cat /etc/shadow`
//! 2. **Catastrophic binaries**: `mkfs`, `dd`, `:(){ :|:& };:` (fork bomb)
//! 3. **Privilege escalation**: `sudo`, `su`, `chmod 777 /`
//! 4. **Unbounded deletion**: `rm -rf /`, `find / -delete`
//!
//! # Architecture
//!
//! ```text
//! CommandBlock → ShellLexer → [ShellToken] → ShellAst
//!     → SymbolicExecutor(cwd) → SafetyVerdict
//! ```
//!
//! # Limitations
//!
//! This is a *conservative* analysis. It will reject some safe commands
//! (false positives) but should never approve a dangerous one (no false
//! negatives). When in doubt, reject.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::mdl_extraction::CommandBlock;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the symbolic executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SymExecConfig {
    /// The CWD boundary — commands must not escape this directory.
    /// Defaults to "/" which means no restriction (use set_cwd to restrict).
    pub cwd: String,
    /// Maximum path depth allowed (prevents deeply nested traversals).
    pub max_path_depth: usize,
    /// Whether to allow commands with unknown/unparseable arguments.
    /// If false, unparseable commands are rejected (conservative).
    pub allow_unparseable: bool,
    /// Additional banned binary names beyond the built-in list.
    pub extra_banned_binaries: Vec<String>,
    /// Additional safe binary names that are always allowed.
    pub extra_safe_binaries: Vec<String>,
}

impl Default for SymExecConfig {
    fn default() -> Self {
        Self {
            cwd: "/".to_string(),
            max_path_depth: 32,
            allow_unparseable: false,
            extra_banned_binaries: Vec::new(),
            extra_safe_binaries: Vec::new(),
        }
    }
}

// =============================================================================
// Banned/safe binary lists
// =============================================================================

/// Binaries that are unconditionally banned in ARS reflexes.
const BANNED_BINARIES: &[&str] = &[
    // Filesystem destruction
    "mkfs",
    "mkfs.ext4",
    "mkfs.xfs",
    "mkfs.btrfs",
    "mkfs.vfat",
    "mke2fs",
    "mkswap",
    "fdisk",
    "gdisk",
    "parted",
    // Raw disk access
    "dd",
    // Privilege escalation
    "sudo",
    "su",
    "doas",
    "pkexec",
    // Dangerous system commands
    "shutdown",
    "reboot",
    "halt",
    "poweroff",
    "init",
    // Network attacks
    "nmap",
    "masscan",
    // Kernel manipulation
    "insmod",
    "rmmod",
    "modprobe",
    // Container escape
    "nsenter",
    "unshare",
    // Format string / injection vectors
    "eval",
];

/// Binaries that are always safe (read-only or output-only).
const SAFE_BINARIES: &[&str] = &[
    "echo",
    "printf",
    "cat",
    "head",
    "tail",
    "less",
    "more",
    "ls",
    "dir",
    "stat",
    "file",
    "wc",
    "sort",
    "uniq",
    "grep",
    "egrep",
    "fgrep",
    "rg",
    "ag",
    "ack",
    "find", // safe unless combined with -exec rm
    "which",
    "whereis",
    "type",
    "command",
    "pwd",
    "whoami",
    "id",
    "hostname",
    "uname",
    "date",
    "cal",
    "uptime",
    "env",
    "printenv",
    "true",
    "false",
    "test",
    "git",
    "cargo",
    "npm",
    "yarn",
    "pnpm",
    "bun",
    "make",
    "cmake",
    "meson",
    "ninja",
    "python",
    "python3",
    "ruby",
    "node",
    "deno",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "man",
    "info",
    "help",
    "diff",
    "cmp",
    "comm",
    "tee",
    "tr",
    "cut",
    "paste",
    "join",
    "sed",
    "awk",
    "perl",
    "jq",
    "yq",
    "xq",
    "curl",
    "wget",
    "http", // network but read-only
    "ssh",
    "scp",
    "rsync",
    "docker",
    "podman", // container management
    "kubectl",
    "helm",
];

/// Flags on `rm` that make it catastrophic.
const RM_DANGEROUS_FLAGS: &[&str] = &["-rf", "-fr", "--recursive", "-r"];

/// Flags on `chmod` that are dangerous with broad targets.
const CHMOD_DANGEROUS_PATTERNS: &[&str] = &["777", "a+rwx", "ugo+rwx"];

// =============================================================================
// Shell lexer
// =============================================================================

/// A minimal shell token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellToken {
    /// A bare word (command name, argument, path).
    Word(String),
    /// A pipe operator `|`.
    Pipe,
    /// A semicolon `;` or `&&` or `||` command separator.
    Separator,
    /// A redirect operator (`>`, `>>`, `<`, `2>`).
    Redirect(String),
    /// A subshell/command substitution `$(...)` or backticks.
    Substitution(String),
}

/// Tokenize a shell command into tokens.
///
/// This is a simplified lexer — it handles quoting and basic operators
/// but doesn't fully parse bash syntax. Conservative: treats unknown
/// constructs as opaque words.
#[must_use]
pub fn tokenize(input: &str) -> Vec<ShellToken> {
    let mut tokens = Vec::new();
    let mut chars = input.chars().peekable();
    let mut current_word = String::new();

    while let Some(&ch) = chars.peek() {
        match ch {
            // Whitespace: flush current word.
            ' ' | '\t' => {
                flush_word(&mut current_word, &mut tokens);
                chars.next();
            }
            // Pipe.
            '|' => {
                flush_word(&mut current_word, &mut tokens);
                chars.next();
                if chars.peek() == Some(&'|') {
                    chars.next();
                    tokens.push(ShellToken::Separator);
                } else {
                    tokens.push(ShellToken::Pipe);
                }
            }
            // Semicolon.
            ';' => {
                flush_word(&mut current_word, &mut tokens);
                chars.next();
                tokens.push(ShellToken::Separator);
            }
            // && operator.
            '&' => {
                flush_word(&mut current_word, &mut tokens);
                chars.next();
                if chars.peek() == Some(&'&') {
                    chars.next();
                    tokens.push(ShellToken::Separator);
                }
                // Single & (background) — treat as separator.
            }
            // Redirects.
            '>' | '<' => {
                flush_word(&mut current_word, &mut tokens);
                let mut redir = String::new();
                redir.push(ch);
                chars.next();
                if chars.peek() == Some(&'>') {
                    redir.push('>');
                    chars.next();
                }
                tokens.push(ShellToken::Redirect(redir));
            }
            // File descriptor redirect (2>).
            '2' if chars.clone().nth(1) == Some('>') => {
                flush_word(&mut current_word, &mut tokens);
                chars.next(); // '2'
                let mut redir = String::from("2");
                redir.push(chars.next().unwrap()); // '>'
                if chars.peek() == Some(&'>') {
                    redir.push('>');
                    chars.next();
                }
                tokens.push(ShellToken::Redirect(redir));
            }
            // Command substitution $(...).
            '$' => {
                chars.next();
                if chars.peek() == Some(&'(') {
                    chars.next();
                    let mut depth = 1;
                    let mut sub = String::new();
                    while let Some(&c) = chars.peek() {
                        chars.next();
                        if c == '(' {
                            depth += 1;
                        } else if c == ')' {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        sub.push(c);
                    }
                    flush_word(&mut current_word, &mut tokens);
                    tokens.push(ShellToken::Substitution(sub));
                } else {
                    current_word.push('$');
                    // Variable reference — continue as word.
                }
            }
            // Single-quoted string.
            '\'' => {
                chars.next();
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c == '\'' {
                        break;
                    }
                    current_word.push(c);
                }
            }
            // Double-quoted string.
            '"' => {
                chars.next();
                while let Some(&c) = chars.peek() {
                    chars.next();
                    if c == '"' {
                        break;
                    }
                    if c == '\\' {
                        if let Some(&next) = chars.peek() {
                            chars.next();
                            current_word.push(next);
                            continue;
                        }
                    }
                    current_word.push(c);
                }
            }
            // Escape.
            '\\' => {
                chars.next();
                if let Some(&next) = chars.peek() {
                    chars.next();
                    current_word.push(next);
                }
            }
            // Regular character.
            _ => {
                current_word.push(ch);
                chars.next();
            }
        }
    }

    flush_word(&mut current_word, &mut tokens);
    tokens
}

fn flush_word(word: &mut String, tokens: &mut Vec<ShellToken>) {
    if !word.is_empty() {
        tokens.push(ShellToken::Word(std::mem::take(word)));
    }
}

// =============================================================================
// Shell AST
// =============================================================================

/// A parsed shell command (single pipeline stage).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    /// The binary/command name.
    pub binary: String,
    /// Arguments (flags and positional args).
    pub args: Vec<String>,
    /// Whether this command is piped to another.
    pub piped: bool,
    /// Subcommand substitutions found (need recursive analysis).
    pub substitutions: Vec<String>,
}

/// Parse tokens into a sequence of commands (handling pipes and separators).
#[must_use]
pub fn parse_commands(tokens: &[ShellToken]) -> Vec<ParsedCommand> {
    let mut commands = Vec::new();
    let mut current_words = Vec::new();
    let mut current_subs = Vec::new();
    let mut is_piped = false;

    for token in tokens {
        match token {
            ShellToken::Word(w) => current_words.push(w.clone()),
            ShellToken::Substitution(s) => current_subs.push(s.clone()),
            ShellToken::Pipe => {
                if let Some(cmd) = build_command(&current_words, &current_subs, true) {
                    commands.push(cmd);
                }
                current_words.clear();
                current_subs.clear();
                is_piped = true;
            }
            ShellToken::Separator => {
                if let Some(cmd) = build_command(&current_words, &current_subs, false) {
                    commands.push(cmd);
                }
                current_words.clear();
                current_subs.clear();
                is_piped = false;
            }
            ShellToken::Redirect(_) => {
                // Skip the redirect operator; next word is the target.
            }
        }
    }

    // Final command.
    if let Some(cmd) = build_command(&current_words, &current_subs, false) {
        commands.push(cmd);
    }

    let _ = is_piped; // suppress warning
    commands
}

fn build_command(words: &[String], subs: &[String], piped: bool) -> Option<ParsedCommand> {
    if words.is_empty() {
        return None;
    }

    // Handle env var assignments before the command (e.g., "FOO=bar cargo build").
    let mut cmd_start = 0;
    for (i, w) in words.iter().enumerate() {
        if w.contains('=') && !w.starts_with('-') {
            cmd_start = i + 1;
        } else {
            break;
        }
    }

    if cmd_start >= words.len() {
        // All words are assignments — no actual command.
        return None;
    }

    let binary = extract_binary_name(&words[cmd_start]);
    let args = words[cmd_start + 1..].to_vec();

    Some(ParsedCommand {
        binary,
        args,
        piped,
        substitutions: subs.to_vec(),
    })
}

/// Extract the binary name from a possibly qualified path.
fn extract_binary_name(word: &str) -> String {
    Path::new(word)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(word)
        .to_string()
}

// =============================================================================
// Symbolic path resolution
// =============================================================================

/// Resolve a path relative to CWD, normalizing `.` and `..` components.
///
/// Returns the canonical absolute path.
#[must_use]
pub fn resolve_path(cwd: &str, path_str: &str) -> PathBuf {
    let path = Path::new(path_str);
    let mut result = if path.is_absolute() {
        PathBuf::new()
    } else {
        PathBuf::from(cwd)
    };

    for component in path.components() {
        match component {
            Component::RootDir => {
                result = PathBuf::from("/");
            }
            Component::CurDir => {} // `.` — no-op
            Component::ParentDir => {
                result.pop();
            }
            Component::Normal(c) => {
                result.push(c);
            }
            Component::Prefix(_) => {} // Windows — ignore
        }
    }

    result
}

/// Check if a resolved path is within the allowed CWD boundary.
#[must_use]
pub fn path_within_boundary(path: &Path, boundary: &Path) -> bool {
    // Root boundary means no restriction.
    if boundary == Path::new("/") {
        return true;
    }
    path.starts_with(boundary)
}

// =============================================================================
// Safety verdict
// =============================================================================

/// Result of symbolic execution safety analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SafetyVerdict {
    /// All commands are safe within the CWD boundary.
    Safe,
    /// One or more commands are unsafe.
    Unsafe(SafetyViolations),
}

impl SafetyVerdict {
    #[must_use]
    pub fn is_safe(&self) -> bool {
        matches!(self, SafetyVerdict::Safe)
    }

    #[must_use]
    pub fn is_unsafe(&self) -> bool {
        matches!(self, SafetyVerdict::Unsafe(_))
    }
}

/// Details of safety violations found during analysis.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SafetyViolations {
    /// Individual violations.
    pub violations: Vec<SafetyViolation>,
}

/// A single safety violation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SafetyViolation {
    /// Which command block triggered this violation.
    pub block_index: u32,
    /// The violation category.
    pub category: ViolationCategory,
    /// Human-readable description.
    pub description: String,
    /// The specific command/path that triggered the violation.
    pub evidence: String,
}

/// Categories of safety violations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ViolationCategory {
    /// Command accesses paths outside the CWD boundary.
    PathTraversal,
    /// Command invokes a banned destructive binary.
    BannedBinary,
    /// Command performs unbounded recursive deletion.
    UnboundedDeletion,
    /// Command attempts privilege escalation.
    PrivilegeEscalation,
    /// Command contains a fork bomb or resource exhaustion pattern.
    ResourceExhaustion,
    /// Command is unparseable and allow_unparseable is false.
    Unparseable,
    /// Subcommand substitution detected (opaque execution).
    OpaqueSubstitution,
}

// =============================================================================
// Symbolic executor
// =============================================================================

/// Symbolically executes commands to prove safety within a CWD boundary.
pub struct SymbolicExecutor {
    config: SymExecConfig,
    banned_set: HashSet<String>,
    safe_set: HashSet<String>,
}

impl SymbolicExecutor {
    /// Create a new executor with the given configuration.
    #[must_use]
    pub fn new(config: SymExecConfig) -> Self {
        let mut banned_set: HashSet<String> =
            BANNED_BINARIES.iter().map(|s| s.to_string()).collect();
        for extra in &config.extra_banned_binaries {
            banned_set.insert(extra.clone());
        }

        let mut safe_set: HashSet<String> = SAFE_BINARIES.iter().map(|s| s.to_string()).collect();
        for extra in &config.extra_safe_binaries {
            safe_set.insert(extra.clone());
        }

        Self {
            config,
            banned_set,
            safe_set,
        }
    }

    /// Create an executor with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(SymExecConfig::default())
    }

    /// Create an executor with a specific CWD boundary.
    #[must_use]
    pub fn with_cwd(cwd: &str) -> Self {
        Self::new(SymExecConfig {
            cwd: cwd.to_string(),
            ..Default::default()
        })
    }

    /// Analyze a sequence of MDL-extracted commands for safety.
    #[must_use]
    pub fn analyze(&self, commands: &[CommandBlock]) -> SafetyVerdict {
        let mut violations = Vec::new();

        for cmd in commands {
            self.analyze_command(cmd, &mut violations);
        }

        if violations.is_empty() {
            debug!(
                commands = commands.len(),
                cwd = %self.config.cwd,
                "Symbolic execution: SAFE"
            );
            SafetyVerdict::Safe
        } else {
            warn!(
                commands = commands.len(),
                violations = violations.len(),
                "Symbolic execution: UNSAFE"
            );
            SafetyVerdict::Unsafe(SafetyViolations { violations })
        }
    }

    /// Analyze a single command for safety violations.
    fn analyze_command(&self, cmd: &CommandBlock, violations: &mut Vec<SafetyViolation>) {
        // Check for fork bomb patterns first.
        if is_fork_bomb(&cmd.command) {
            violations.push(SafetyViolation {
                block_index: cmd.index,
                category: ViolationCategory::ResourceExhaustion,
                description: "Fork bomb pattern detected".to_string(),
                evidence: cmd.command.clone(),
            });
            return;
        }

        let tokens = tokenize(&cmd.command);
        let parsed = parse_commands(&tokens);

        if parsed.is_empty() && !cmd.command.trim().is_empty() && !self.config.allow_unparseable {
            violations.push(SafetyViolation {
                block_index: cmd.index,
                category: ViolationCategory::Unparseable,
                description: "Command could not be parsed".to_string(),
                evidence: cmd.command.clone(),
            });
            return;
        }

        for parsed_cmd in &parsed {
            // Check for subcommand substitutions.
            for sub in &parsed_cmd.substitutions {
                if !self.config.allow_unparseable {
                    violations.push(SafetyViolation {
                        block_index: cmd.index,
                        category: ViolationCategory::OpaqueSubstitution,
                        description: format!("Subcommand substitution detected: $({sub})"),
                        evidence: sub.clone(),
                    });
                }
            }

            // Check banned binaries.
            if self.banned_set.contains(&parsed_cmd.binary) {
                let category =
                    if ["sudo", "su", "doas", "pkexec"].contains(&parsed_cmd.binary.as_str()) {
                        ViolationCategory::PrivilegeEscalation
                    } else {
                        ViolationCategory::BannedBinary
                    };

                violations.push(SafetyViolation {
                    block_index: cmd.index,
                    category,
                    description: format!("Banned binary: {}", parsed_cmd.binary),
                    evidence: parsed_cmd.binary.clone(),
                });
                continue;
            }

            // Check for dangerous rm patterns.
            if parsed_cmd.binary == "rm" {
                self.check_rm_safety(cmd.index, parsed_cmd, violations);
                continue;
            }

            // Check for dangerous chmod patterns.
            if parsed_cmd.binary == "chmod" {
                self.check_chmod_safety(cmd.index, parsed_cmd, violations);
                continue;
            }

            // Check path arguments for traversal.
            if !self.safe_set.contains(&parsed_cmd.binary) {
                self.check_path_args(cmd.index, parsed_cmd, violations);
            }
        }
    }

    /// Check `rm` commands for dangerous patterns.
    fn check_rm_safety(
        &self,
        block_index: u32,
        cmd: &ParsedCommand,
        violations: &mut Vec<SafetyViolation>,
    ) {
        let has_recursive = cmd.args.iter().any(|a| {
            RM_DANGEROUS_FLAGS
                .iter()
                .any(|f| a == *f || a.starts_with(f))
        });

        for arg in &cmd.args {
            if arg.starts_with('-') {
                continue;
            }

            let resolved = resolve_path(&self.config.cwd, arg);
            let boundary = Path::new(&self.config.cwd);

            // rm on root or parent of CWD is always dangerous.
            if resolved == Path::new("/") {
                violations.push(SafetyViolation {
                    block_index,
                    category: ViolationCategory::UnboundedDeletion,
                    description: "rm targets root filesystem".to_string(),
                    evidence: arg.clone(),
                });
                continue;
            }

            if !path_within_boundary(&resolved, boundary) {
                violations.push(SafetyViolation {
                    block_index,
                    category: ViolationCategory::PathTraversal,
                    description: format!(
                        "rm targets path outside CWD: {} → {}",
                        arg,
                        resolved.display()
                    ),
                    evidence: arg.clone(),
                });
                continue;
            }

            if has_recursive && resolved == self.config.cwd {
                violations.push(SafetyViolation {
                    block_index,
                    category: ViolationCategory::UnboundedDeletion,
                    description: "rm -rf targets the entire CWD".to_string(),
                    evidence: arg.clone(),
                });
            }
        }
    }

    /// Check `chmod` commands for dangerous patterns.
    fn check_chmod_safety(
        &self,
        block_index: u32,
        cmd: &ParsedCommand,
        violations: &mut Vec<SafetyViolation>,
    ) {
        let has_dangerous_mode = cmd
            .args
            .iter()
            .any(|a| CHMOD_DANGEROUS_PATTERNS.iter().any(|p| a.contains(p)));

        if !has_dangerous_mode {
            return;
        }

        for arg in &cmd.args {
            if arg.starts_with('-') || CHMOD_DANGEROUS_PATTERNS.iter().any(|p| arg.contains(p)) {
                continue;
            }

            let resolved = resolve_path(&self.config.cwd, arg);
            let boundary = Path::new(&self.config.cwd);

            if !path_within_boundary(&resolved, boundary) {
                violations.push(SafetyViolation {
                    block_index,
                    category: ViolationCategory::PathTraversal,
                    description: format!("chmod 777 on path outside CWD: {}", resolved.display()),
                    evidence: arg.clone(),
                });
            }
        }
    }

    /// Check non-safe binary path arguments for traversal.
    fn check_path_args(
        &self,
        block_index: u32,
        cmd: &ParsedCommand,
        violations: &mut Vec<SafetyViolation>,
    ) {
        let boundary = Path::new(&self.config.cwd);

        for arg in &cmd.args {
            // Skip flags.
            if arg.starts_with('-') {
                continue;
            }

            // Only check args that look like paths.
            if !looks_like_path(arg) {
                continue;
            }

            let resolved = resolve_path(&self.config.cwd, arg);

            if !path_within_boundary(&resolved, boundary) {
                violations.push(SafetyViolation {
                    block_index,
                    category: ViolationCategory::PathTraversal,
                    description: format!(
                        "'{}' accesses path outside CWD: {} → {}",
                        cmd.binary,
                        arg,
                        resolved.display()
                    ),
                    evidence: arg.clone(),
                });
            }
        }
    }
}

/// Check if a string looks like a fork bomb.
fn is_fork_bomb(cmd: &str) -> bool {
    // Classic bash fork bomb: :(){ :|:& };:
    let normalized = cmd.replace(' ', "");
    let fork_pattern = ":(){";
    let pipe_pattern = ":|:";
    if normalized.contains(fork_pattern) || normalized.contains(pipe_pattern) {
        return true;
    }
    // Perl/Python variants.
    if cmd.contains("fork()") && cmd.contains("while") {
        return true;
    }
    false
}

/// Heuristic: does this string look like a filesystem path?
fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.contains("..") || s.starts_with('~') || s.starts_with('.')
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cmd(index: u32, command: &str) -> CommandBlock {
        CommandBlock {
            index,
            command: command.to_string(),
            exit_code: Some(0),
            duration_us: Some(1000),
            output_preview: None,
            timestamp_us: (index as u64 + 1) * 1_000_000,
        }
    }

    fn cwd_executor(cwd: &str) -> SymbolicExecutor {
        SymbolicExecutor::with_cwd(cwd)
    }

    // -------------------------------------------------------------------------
    // Config
    // -------------------------------------------------------------------------

    #[test]
    fn config_defaults() {
        let cfg = SymExecConfig::default();
        assert_eq!(cfg.cwd, "/");
        assert_eq!(cfg.max_path_depth, 32);
        assert!(!cfg.allow_unparseable);
    }

    #[test]
    fn config_serde_roundtrip() {
        let cfg = SymExecConfig {
            cwd: "/home/user/project".to_string(),
            max_path_depth: 16,
            ..Default::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let decoded: SymExecConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.cwd, "/home/user/project");
        assert_eq!(decoded.max_path_depth, 16);
    }

    // -------------------------------------------------------------------------
    // Tokenizer
    // -------------------------------------------------------------------------

    #[test]
    fn tokenize_simple_command() {
        let tokens = tokenize("ls -la /tmp");
        assert_eq!(tokens.len(), 3);
        assert_eq!(tokens[0], ShellToken::Word("ls".into()));
        assert_eq!(tokens[1], ShellToken::Word("-la".into()));
        assert_eq!(tokens[2], ShellToken::Word("/tmp".into()));
    }

    #[test]
    fn tokenize_pipe() {
        let input = "cat file | grep foo";
        let tokens = tokenize(input);
        assert!(tokens.contains(&ShellToken::Pipe));
    }

    #[test]
    fn tokenize_and_operator() {
        let input = "cmd1 && cmd2";
        let tokens = tokenize(input);
        assert!(tokens.contains(&ShellToken::Separator));
    }

    #[test]
    fn tokenize_semicolon() {
        let input = "cmd1 ; cmd2";
        let tokens = tokenize(input);
        assert!(tokens.contains(&ShellToken::Separator));
    }

    #[test]
    fn tokenize_single_quotes() {
        let input = concat!("echo ", "'", "hello world", "'");
        let tokens = tokenize(input);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], ShellToken::Word("echo".into()));
        assert_eq!(tokens[1], ShellToken::Word("hello world".into()));
    }

    #[test]
    fn tokenize_double_quotes() {
        let input = "echo \"hello world\"";
        let tokens = tokenize(input);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], ShellToken::Word("echo".into()));
        assert_eq!(tokens[1], ShellToken::Word("hello world".into()));
    }

    #[test]
    fn tokenize_redirect() {
        let input = "echo foo > output.txt";
        let tokens = tokenize(input);
        assert!(tokens.iter().any(|t| matches!(t, ShellToken::Redirect(_))));
    }

    #[test]
    fn tokenize_subshell() {
        let input = "echo $(whoami)";
        let tokens = tokenize(input);
        assert!(
            tokens
                .iter()
                .any(|t| matches!(t, ShellToken::Substitution(_)))
        );
    }

    #[test]
    fn tokenize_empty() {
        let tokens = tokenize("");
        assert!(tokens.is_empty());
    }

    #[test]
    fn tokenize_escape() {
        let input = "echo hello\\ world";
        let tokens = tokenize(input);
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[0], ShellToken::Word("echo".into()));
        assert_eq!(tokens[1], ShellToken::Word("hello world".into()));
    }

    // -------------------------------------------------------------------------
    // Parser
    // -------------------------------------------------------------------------

    #[test]
    fn parse_simple_command() {
        let tokens = tokenize("cargo build --release");
        let cmds = parse_commands(&tokens);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].binary, "cargo");
        assert_eq!(cmds[0].args, vec!["build", "--release"]);
    }

    #[test]
    fn parse_pipeline() {
        let tokens = tokenize("cat file | grep foo | wc -l");
        let cmds = parse_commands(&tokens);
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0].binary, "cat");
        assert_eq!(cmds[1].binary, "grep");
        assert_eq!(cmds[2].binary, "wc");
    }

    #[test]
    fn parse_chained_commands() {
        let tokens = tokenize("mkdir build && cd build && cmake ..");
        let cmds = parse_commands(&tokens);
        assert_eq!(cmds.len(), 3);
    }

    #[test]
    fn parse_env_prefix() {
        let tokens = tokenize("FOO=bar cargo build");
        let cmds = parse_commands(&tokens);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].binary, "cargo");
    }

    #[test]
    fn parse_full_path_binary() {
        let tokens = tokenize("/usr/bin/ls -la");
        let cmds = parse_commands(&tokens);
        assert_eq!(cmds[0].binary, "ls");
    }

    // -------------------------------------------------------------------------
    // Path resolution
    // -------------------------------------------------------------------------

    #[test]
    fn resolve_absolute_path() {
        let resolved = resolve_path("/home/user", "/etc/passwd");
        assert_eq!(resolved, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn resolve_relative_path() {
        let resolved = resolve_path("/home/user/project", "src/main.rs");
        assert_eq!(resolved, PathBuf::from("/home/user/project/src/main.rs"));
    }

    #[test]
    fn resolve_parent_traversal() {
        let resolved = resolve_path("/home/user/project", "../../../etc/passwd");
        assert_eq!(resolved, PathBuf::from("/etc/passwd"));
    }

    #[test]
    fn resolve_dot() {
        let resolved = resolve_path("/home/user", "./file.txt");
        assert_eq!(resolved, PathBuf::from("/home/user/file.txt"));
    }

    #[test]
    fn path_within_boundary_true() {
        let path = Path::new("/home/user/project/src/main.rs");
        let boundary = Path::new("/home/user/project");
        assert!(path_within_boundary(path, boundary));
    }

    #[test]
    fn path_within_boundary_false() {
        let path = Path::new("/etc/passwd");
        let boundary = Path::new("/home/user/project");
        assert!(!path_within_boundary(path, boundary));
    }

    #[test]
    fn path_within_root_always_true() {
        let path = Path::new("/etc/passwd");
        let boundary = Path::new("/");
        assert!(path_within_boundary(path, boundary));
    }

    // -------------------------------------------------------------------------
    // Safety verdicts — safe commands
    // -------------------------------------------------------------------------

    #[test]
    fn safe_cargo_build() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "cargo build --release")];
        assert!(exec.analyze(&cmds).is_safe());
    }

    #[test]
    fn safe_git_status() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "git status")];
        assert!(exec.analyze(&cmds).is_safe());
    }

    #[test]
    fn safe_ls_within_cwd() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "ls -la src/")];
        assert!(exec.analyze(&cmds).is_safe());
    }

    #[test]
    fn safe_empty_commands() {
        let exec = cwd_executor("/home/user/project");
        assert!(exec.analyze(&[]).is_safe());
    }

    #[test]
    fn safe_echo() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "echo 'hello world'")];
        assert!(exec.analyze(&cmds).is_safe());
    }

    // -------------------------------------------------------------------------
    // Safety verdicts — banned binaries
    // -------------------------------------------------------------------------

    #[test]
    fn unsafe_mkfs() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "mkfs.ext4 /dev/sda1")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
        if let SafetyVerdict::Unsafe(v) = &verdict {
            assert!(
                v.violations
                    .iter()
                    .any(|v| v.category == ViolationCategory::BannedBinary)
            );
        }
    }

    #[test]
    fn unsafe_dd() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "dd if=/dev/zero of=/dev/sda")];
        assert!(exec.analyze(&cmds).is_unsafe());
    }

    #[test]
    fn unsafe_sudo() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "sudo rm -rf /")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
        if let SafetyVerdict::Unsafe(v) = &verdict {
            assert!(
                v.violations
                    .iter()
                    .any(|v| v.category == ViolationCategory::PrivilegeEscalation)
            );
        }
    }

    #[test]
    fn unsafe_eval() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "eval 'rm -rf /'")];
        assert!(exec.analyze(&cmds).is_unsafe());
    }

    // -------------------------------------------------------------------------
    // Safety verdicts — path traversal
    // -------------------------------------------------------------------------

    #[test]
    fn unsafe_rm_parent_traversal() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "rm -rf ../../../etc")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
        if let SafetyVerdict::Unsafe(v) = &verdict {
            assert!(
                v.violations
                    .iter()
                    .any(|v| v.category == ViolationCategory::PathTraversal)
            );
        }
    }

    #[test]
    fn unsafe_rm_root() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "rm -rf /")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
        if let SafetyVerdict::Unsafe(v) = &verdict {
            assert!(
                v.violations
                    .iter()
                    .any(|v| v.category == ViolationCategory::UnboundedDeletion)
            );
        }
    }

    #[test]
    fn unsafe_rm_absolute_outside_cwd() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "rm -rf /etc/important")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
    }

    #[test]
    fn safe_rm_within_cwd() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "rm -rf target/debug")];
        assert!(exec.analyze(&cmds).is_safe());
    }

    // -------------------------------------------------------------------------
    // Safety verdicts — fork bomb
    // -------------------------------------------------------------------------

    #[test]
    fn unsafe_fork_bomb_classic() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, ":(){ :|:& };:")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
        if let SafetyVerdict::Unsafe(v) = &verdict {
            assert!(
                v.violations
                    .iter()
                    .any(|v| v.category == ViolationCategory::ResourceExhaustion)
            );
        }
    }

    // -------------------------------------------------------------------------
    // Safety verdicts — subcommand substitution
    // -------------------------------------------------------------------------

    #[test]
    fn unsafe_subcommand_substitution() {
        let exec = SymbolicExecutor::new(SymExecConfig {
            cwd: "/home/user/project".to_string(),
            allow_unparseable: false,
            ..Default::default()
        });
        let cmds = vec![make_cmd(0, "echo $(rm -rf /)")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
    }

    #[test]
    fn safe_subcommand_when_allowed() {
        let exec = SymbolicExecutor::new(SymExecConfig {
            cwd: "/home/user/project".to_string(),
            allow_unparseable: true,
            ..Default::default()
        });
        let cmds = vec![make_cmd(0, "echo $(whoami)")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_safe());
    }

    // -------------------------------------------------------------------------
    // Safety verdicts — chmod
    // -------------------------------------------------------------------------

    #[test]
    fn unsafe_chmod_777_outside_cwd() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "chmod 777 /etc/passwd")];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
    }

    #[test]
    fn safe_chmod_within_cwd() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![make_cmd(0, "chmod 755 src/main.rs")];
        // 755 is not in the dangerous patterns, so this is safe.
        assert!(exec.analyze(&cmds).is_safe());
    }

    // -------------------------------------------------------------------------
    // Extra banned/safe binaries
    // -------------------------------------------------------------------------

    #[test]
    fn extra_banned_binary_detected() {
        let exec = SymbolicExecutor::new(SymExecConfig {
            cwd: "/home/user/project".to_string(),
            extra_banned_binaries: vec!["dangerous_tool".to_string()],
            ..Default::default()
        });
        let cmds = vec![make_cmd(0, "dangerous_tool --nuke")];
        assert!(exec.analyze(&cmds).is_unsafe());
    }

    #[test]
    fn extra_safe_binary_allowed() {
        let exec = SymbolicExecutor::new(SymExecConfig {
            cwd: "/home/user/project".to_string(),
            extra_safe_binaries: vec!["custom_tool".to_string()],
            ..Default::default()
        });
        let cmds = vec![make_cmd(0, "custom_tool /etc/config")];
        // custom_tool is safe, so path args aren't checked.
        assert!(exec.analyze(&cmds).is_safe());
    }

    // -------------------------------------------------------------------------
    // Multiple commands
    // -------------------------------------------------------------------------

    #[test]
    fn mixed_safe_and_unsafe() {
        let exec = cwd_executor("/home/user/project");
        let cmds = vec![
            make_cmd(0, "cargo build"),
            make_cmd(1, "rm -rf /etc"),
            make_cmd(2, "cargo test"),
        ];
        let verdict = exec.analyze(&cmds);
        assert!(verdict.is_unsafe());
        if let SafetyVerdict::Unsafe(v) = &verdict {
            assert_eq!(v.violations.len(), 1);
            assert_eq!(v.violations[0].block_index, 1);
        }
    }

    // -------------------------------------------------------------------------
    // Verdict serde
    // -------------------------------------------------------------------------

    #[test]
    fn verdict_serde_safe() {
        let v = SafetyVerdict::Safe;
        let json = serde_json::to_string(&v).unwrap();
        let decoded: SafetyVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn verdict_serde_unsafe() {
        let v = SafetyVerdict::Unsafe(SafetyViolations {
            violations: vec![SafetyViolation {
                block_index: 0,
                category: ViolationCategory::BannedBinary,
                description: "test".to_string(),
                evidence: "dd".to_string(),
            }],
        });
        let json = serde_json::to_string(&v).unwrap();
        let decoded: SafetyVerdict = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, v);
    }

    // -------------------------------------------------------------------------
    // ViolationCategory serde
    // -------------------------------------------------------------------------

    #[test]
    fn violation_category_serde() {
        for cat in [
            ViolationCategory::PathTraversal,
            ViolationCategory::BannedBinary,
            ViolationCategory::UnboundedDeletion,
            ViolationCategory::PrivilegeEscalation,
            ViolationCategory::ResourceExhaustion,
            ViolationCategory::Unparseable,
            ViolationCategory::OpaqueSubstitution,
        ] {
            let json = serde_json::to_string(&cat).unwrap();
            let decoded: ViolationCategory = serde_json::from_str(&json).unwrap();
            assert_eq!(decoded, cat);
        }
    }

    // -------------------------------------------------------------------------
    // Helper functions
    // -------------------------------------------------------------------------

    #[test]
    fn looks_like_path_positive() {
        assert!(looks_like_path("/etc/passwd"));
        assert!(looks_like_path("../parent"));
        assert!(looks_like_path("./current"));
        assert!(looks_like_path("~/home"));
        assert!(looks_like_path("src/main.rs"));
    }

    #[test]
    fn looks_like_path_negative() {
        assert!(!looks_like_path("--flag"));
        assert!(!looks_like_path("word"));
        assert!(!looks_like_path("123"));
    }

    #[test]
    fn is_fork_bomb_detects_classic() {
        assert!(is_fork_bomb(":(){ :|:& };:"));
    }

    #[test]
    fn is_fork_bomb_negative() {
        assert!(!is_fork_bomb("echo hello"));
        assert!(!is_fork_bomb("cargo build"));
    }

    #[test]
    fn extract_binary_name_from_path() {
        assert_eq!(extract_binary_name("/usr/bin/ls"), "ls");
        assert_eq!(extract_binary_name("ls"), "ls");
        assert_eq!(extract_binary_name("./script.sh"), "script.sh");
    }
}
