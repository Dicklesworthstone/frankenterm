//! Per-pane terminal state snapshot for session persistence.
//!
//! Models terminal state (cursor, alt-screen, scrollback ref, optional process
//! and agent observations, and curated env vars) for each pane. The production
//! snapshot writer projects a bounded subset into
//! `mux_pane_state.terminal_state_json` and related columns.
//!
//! # Size budget
//!
//! [`PaneStateSnapshot::to_json_budgeted`] never returns more than 64 KiB. It
//! progressively removes optional observations if necessary. Production row
//! persistence has an independent whole-row budget at the actual SQLite
//! projection boundary.

use std::io::{self, Write};

use serde::ser::SerializeMap;
use serde::{Deserialize, Serialize, Serializer};
use tracing::{debug, trace};

/// Maximum serialized size per pane state (64KB).
pub const PANE_STATE_SIZE_BUDGET: usize = 65_536;

/// Current schema version for pane state snapshots.
///
/// Version 2 gives the scrollback cardinality its exact retained-segment
/// meaning. Version-1 payloads named the same database row count first as
/// `total_lines_captured` and later as `total_segments_captured`; neither key
/// is accepted as the version-2 field, so legacy values cannot silently cross
/// the semantic boundary.
pub const PANE_STATE_SCHEMA_VERSION: u32 = 2;

/// Environment variable names that are safe to capture.
pub(crate) const SAFE_ENV_VARS: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "TERM",
    "LANG",
    "EDITOR",
    "FT_WORKSPACE",
    "FT_OUTPUT_FORMAT",
    "VISUAL",
    "USER",
    "HOSTNAME",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
];

/// Patterns that indicate a sensitive env var name.
const SENSITIVE_VAR_PATTERNS: &[&str] = &[
    "SECRET",
    "TOKEN",
    "KEY",
    "PASSWORD",
    "CREDENTIAL",
    "AUTH",
    "API_KEY",
    "PRIVATE",
    "PASSWD",
];

/// Maximum variable-name size admitted to case-insensitive sensitive-name
/// classification. Real environment names fit comfortably within this bound;
/// adversarial iterator inputs cannot force an unbounded Unicode case-fold
/// allocation merely to update redaction telemetry.
const MAX_ENV_NAME_CLASSIFICATION_BYTES: usize = 256;

// =============================================================================
// Core types
// =============================================================================

/// Complete per-pane state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PaneStateSnapshot {
    /// Schema version for forward compatibility.
    pub schema_version: u32,
    /// WezTerm pane ID at capture time.
    pub pane_id: u64,
    /// When this snapshot was captured (epoch ms).
    pub captured_at: u64,

    /// Process info (best-effort).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreground_process: Option<ProcessInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,

    /// Terminal state.
    pub terminal: TerminalState,

    /// Scrollback linkage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_ref: Option<ScrollbackRef>,

    /// Agent context (populated by agent detection if active).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentMetadata>,

    /// Curated environment variables (redacted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<CapturedEnv>,
}

/// Best-effort foreground process information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProcessInfo {
    /// Process name (e.g., "claude-code", "bash").
    pub name: String,
    /// Process ID if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// First 5 arguments (if available).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
}

/// Terminal display state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalState {
    pub rows: u16,
    pub cols: u16,
    #[serde(default)]
    pub cursor_row: u16,
    #[serde(default)]
    pub cursor_col: u16,
    #[serde(default)]
    pub is_alt_screen: bool,
    #[serde(default)]
    pub title: String,
}

/// Reference to scrollback data in output_segments.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScrollbackRef {
    /// Highest sequence number among currently retained `output_segments`.
    pub output_segments_seq: i64,
    /// Number of `output_segments` rows currently retained for this pane.
    ///
    /// A segment is an arbitrary stream fragment, not a logical terminal line.
    pub retained_segment_count: u64,
    /// Greatest capture timestamp among currently retained segments (epoch ms).
    pub last_capture_at: u64,
}

/// Agent metadata for AI coding agent sessions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentMetadata {
    /// Agent type (e.g., "claude_code", "codex", "gemini").
    pub agent_type: String,
    /// Agent session ID if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Current agent state (e.g., "idle", "working", "rate_limited").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Curated set of environment variables (with redaction).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapturedEnv {
    /// Safe environment variables captured.
    #[serde(serialize_with = "serialize_env_vars_sorted")]
    pub vars: std::collections::HashMap<String, String>,
    /// Number of variables that were redacted.
    pub redacted_count: usize,
}

/// Serialize captured environment entries in lexical key order.
///
/// `HashMap` remains the in-memory representation because capture and restore
/// callers perform keyed lookups, but its randomized iteration order must not
/// leak into persisted checkpoint bytes or witness inputs. The bounded pane
/// serializer rejects impossible cardinalities before reaching this helper;
/// the ordinary serializer deliberately preserves its historical unbounded
/// API while making its output reproducible.
fn serialize_env_vars_sorted<S>(
    vars: &std::collections::HashMap<String, String>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut entries: Vec<_> = vars.iter().collect();
    entries.sort_unstable_by_key(|(key, _)| *key);

    let mut map = serializer.serialize_map(Some(entries.len()))?;
    for (key, value) in entries {
        map.serialize_entry(key, value)?;
    }
    map.end()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneStateRetention {
    env: bool,
    argv: bool,
    agent: bool,
    process_context: bool,
    title: bool,
}

impl PaneStateRetention {
    const FULL: Self = Self {
        env: true,
        argv: true,
        agent: true,
        process_context: true,
        title: true,
    };

    fn effective_for(self, snapshot: &PaneStateSnapshot) -> Self {
        let process = snapshot.foreground_process.as_ref();
        let has_process_context =
            snapshot.cwd.is_some() || snapshot.shell.is_some() || process.is_some();
        Self {
            env: self.env && snapshot.env.is_some(),
            argv: self.process_context
                && self.argv
                && process.is_some_and(|process| process.argv.is_some()),
            agent: self.agent && snapshot.agent.is_some(),
            process_context: self.process_context && has_process_context,
            title: self.title && !snapshot.terminal.title.is_empty(),
        }
    }
}

#[derive(Serialize)]
struct ProcessInfoProjection<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    argv: Option<&'a [String]>,
}

#[derive(Serialize)]
struct TerminalStateProjection<'a> {
    rows: u16,
    cols: u16,
    cursor_row: u16,
    cursor_col: u16,
    is_alt_screen: bool,
    title: &'a str,
}

#[derive(Serialize)]
struct PaneStateSnapshotProjection<'a> {
    schema_version: u32,
    pane_id: u64,
    captured_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    foreground_process: Option<ProcessInfoProjection<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    shell: Option<&'a str>,
    terminal: TerminalStateProjection<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    scrollback_ref: Option<&'a ScrollbackRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a AgentMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<&'a CapturedEnv>,
}

impl<'a> PaneStateSnapshotProjection<'a> {
    fn new(snapshot: &'a PaneStateSnapshot, retain: PaneStateRetention) -> Self {
        let foreground_process = if retain.process_context {
            snapshot
                .foreground_process
                .as_ref()
                .map(|process| ProcessInfoProjection {
                    name: &process.name,
                    pid: process.pid,
                    argv: if retain.argv {
                        process.argv.as_deref()
                    } else {
                        None
                    },
                })
        } else {
            None
        };

        Self {
            schema_version: snapshot.schema_version,
            pane_id: snapshot.pane_id,
            captured_at: snapshot.captured_at,
            cwd: if retain.process_context {
                snapshot.cwd.as_deref()
            } else {
                None
            },
            foreground_process,
            shell: if retain.process_context {
                snapshot.shell.as_deref()
            } else {
                None
            },
            terminal: TerminalStateProjection {
                rows: snapshot.terminal.rows,
                cols: snapshot.terminal.cols,
                cursor_row: snapshot.terminal.cursor_row,
                cursor_col: snapshot.terminal.cursor_col,
                is_alt_screen: snapshot.terminal.is_alt_screen,
                title: if retain.title {
                    &snapshot.terminal.title
                } else {
                    ""
                },
            },
            scrollback_ref: snapshot.scrollback_ref.as_ref(),
            agent: if retain.agent {
                snapshot.agent.as_ref()
            } else {
                None
            },
            env: if retain.env {
                snapshot.env.as_ref()
            } else {
                None
            },
        }
    }
}

fn projection_raw_text_exceeds_budget(
    snapshot: &PaneStateSnapshot,
    retain: PaneStateRetention,
) -> bool {
    let mut raw_bytes = 0usize;
    let mut exceeds = |additional: usize| match raw_bytes.checked_add(additional) {
        Some(total) if total <= PANE_STATE_SIZE_BUDGET => {
            raw_bytes = total;
            false
        }
        _ => true,
    };

    if retain.process_context {
        if exceeds(snapshot.cwd.as_ref().map_or(0, String::len))
            || exceeds(snapshot.shell.as_ref().map_or(0, String::len))
        {
            return true;
        }
        if let Some(process) = &snapshot.foreground_process {
            if exceeds(process.name.len()) {
                return true;
            }
            if retain.argv
                && let Some(argv) = &process.argv
            {
                // Each JSON string contributes at least two quotes and all
                // but one element contributes a comma. This makes even a
                // caller-supplied vector of empty strings bounded before
                // we iterate its elements.
                if exceeds(argv.len().saturating_mul(3)) {
                    return true;
                }
                for argument in argv {
                    if exceeds(argument.len()) {
                        return true;
                    }
                }
            }
        }
    }

    if retain.title && exceeds(snapshot.terminal.title.len()) {
        return true;
    }
    if retain.agent
        && let Some(agent) = &snapshot.agent
    {
        if exceeds(agent.agent_type.len())
            || exceeds(agent.session_id.as_ref().map_or(0, String::len))
            || exceeds(agent.state.as_ref().map_or(0, String::len))
        {
            return true;
        }
    }
    if retain.env
        && let Some(env) = &snapshot.env
    {
        // Four quotes and a colon are unavoidable for every key/value
        // pair. Bound an adversarial map of empty strings before walking
        // it, then count the retained raw bytes with early exit.
        if exceeds(env.vars.len().saturating_mul(5)) {
            return true;
        }
        for (key, value) in &env.vars {
            if exceeds(key.len()) || exceeds(value.len()) {
                return true;
            }
        }
    }

    false
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl BoundedJsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(limit.min(1024)),
            limit,
            overflowed: false,
        }
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let admitted = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .is_some_and(|length| length <= self.limit);
        if !admitted {
            self.overflowed = true;
            return Err(io::Error::other("pane state JSON size budget exceeded"));
        }
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialize_projection_to_budget(
    projection: &PaneStateSnapshotProjection<'_>,
) -> Result<Option<String>, serde_json::Error> {
    let mut writer = BoundedJsonWriter::new(PANE_STATE_SIZE_BUDGET);
    if let Err(error) = serde_json::to_writer(&mut writer, projection) {
        if writer.overflowed {
            return Ok(None);
        }
        return Err(error);
    }

    let json = String::from_utf8(writer.bytes).map_err(|error| {
        serde_json::Error::io(io::Error::new(io::ErrorKind::InvalidData, error))
    })?;
    Ok(Some(json))
}

// =============================================================================
// Construction
// =============================================================================

impl PaneStateSnapshot {
    /// Create a new pane state snapshot from available data.
    #[must_use]
    pub fn new(pane_id: u64, captured_at: u64, terminal: TerminalState) -> Self {
        Self {
            schema_version: PANE_STATE_SCHEMA_VERSION,
            pane_id,
            captured_at,
            cwd: None,
            foreground_process: None,
            shell: None,
            terminal,
            scrollback_ref: None,
            agent: None,
            env: None,
        }
    }

    /// Set the current working directory.
    #[must_use]
    pub fn with_cwd(mut self, cwd: String) -> Self {
        self.cwd = Some(cwd);
        self
    }

    /// Set foreground process info.
    #[must_use]
    pub fn with_process(mut self, process: ProcessInfo) -> Self {
        self.foreground_process = Some(process);
        self
    }

    /// Set shell name.
    #[must_use]
    pub fn with_shell(mut self, shell: String) -> Self {
        self.shell = Some(shell);
        self
    }

    /// Set scrollback reference.
    #[must_use]
    pub fn with_scrollback(mut self, scrollback: ScrollbackRef) -> Self {
        trace!(
            pane_id = self.pane_id,
            seq = scrollback.output_segments_seq,
            retained_segments = scrollback.retained_segment_count,
            "Scrollback ref for pane"
        );
        self.scrollback_ref = Some(scrollback);
        self
    }

    /// Set agent metadata.
    #[must_use]
    pub fn with_agent(mut self, agent: AgentMetadata) -> Self {
        self.agent = Some(agent);
        self
    }

    /// Capture and set environment variables from the current process environment.
    ///
    /// Only captures variables from the safe-list and redacts sensitive ones.
    #[must_use]
    pub fn with_env_from_current(mut self) -> Self {
        // `std::env::vars()` panics when the process environment contains a
        // non-UTF-8 key or value. Environment observation is best-effort, so
        // skip those entries without exposing their content instead of taking
        // down snapshot capture.
        let mut non_utf8_entries = 0usize;
        let utf8_vars = std::env::vars_os().filter_map(|(key, value)| {
            match (key.into_string(), value.into_string()) {
                (Ok(key), Ok(value)) => Some((key, value)),
                _ => {
                    non_utf8_entries = non_utf8_entries.saturating_add(1);
                    None
                }
            }
        });
        let env = capture_env_from_iter(utf8_vars);
        trace!(
            pane_id = self.pane_id,
            var_count = env.vars.len(),
            redacted_count = env.redacted_count,
            non_utf8_entries = non_utf8_entries,
            "Environment capture for pane"
        );
        self.env = Some(env);
        self
    }

    /// Capture environment from an explicit iterator (for testing).
    #[must_use]
    pub fn with_env_from_iter(mut self, vars: impl Iterator<Item = (String, String)>) -> Self {
        let env = capture_env_from_iter(vars);
        trace!(
            pane_id = self.pane_id,
            var_count = env.vars.len(),
            redacted_count = env.redacted_count,
            "Environment capture for pane"
        );
        self.env = Some(env);
        self
    }

    /// Serialize to JSON.
    ///
    /// # Errors
    /// Returns error if serialization fails.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Serialize to JSON, enforcing the size budget.
    ///
    /// If the serialized form exceeds `PANE_STATE_SIZE_BUDGET`, optional
    /// observations are progressively removed in a deterministic order. The
    /// returned JSON is always within the budget. Each attempt writes through
    /// a bounded buffer, so an oversized caller-controlled string is never
    /// cloned into a correspondingly oversized temporary JSON allocation.
    pub fn to_json_budgeted(&self) -> Result<(String, bool), serde_json::Error> {
        let stages = [
            (PaneStateRetention::FULL, "none"),
            (
                PaneStateRetention {
                    env: false,
                    ..PaneStateRetention::FULL
                },
                "environment",
            ),
            (
                PaneStateRetention {
                    env: false,
                    argv: false,
                    ..PaneStateRetention::FULL
                },
                "process_argv",
            ),
            (
                PaneStateRetention {
                    env: false,
                    argv: false,
                    agent: false,
                    ..PaneStateRetention::FULL
                },
                "agent_metadata",
            ),
            (
                PaneStateRetention {
                    env: false,
                    argv: false,
                    agent: false,
                    process_context: false,
                    ..PaneStateRetention::FULL
                },
                "process_context",
            ),
            (
                PaneStateRetention {
                    env: false,
                    argv: false,
                    agent: false,
                    process_context: false,
                    title: false,
                },
                "terminal_title",
            ),
        ];

        let mut previous_effective_retention = None;
        for (index, (retain, truncation_tier)) in stages.into_iter().enumerate() {
            let effective_retention = retain.effective_for(self);
            if previous_effective_retention == Some(effective_retention) {
                continue;
            }
            previous_effective_retention = Some(effective_retention);

            let json = if projection_raw_text_exceeds_budget(self, retain) {
                None
            } else {
                let projection = PaneStateSnapshotProjection::new(self, retain);
                serialize_projection_to_budget(&projection)?
            };
            if let Some(json) = json {
                let truncated = index != 0;
                if truncated {
                    debug!(
                        pane_id = self.pane_id,
                        truncation_tier = truncation_tier,
                        retained_bytes = json.len(),
                        budget = PANE_STATE_SIZE_BUDGET,
                        "Pane state truncated to size budget"
                    );
                }
                return Ok((json, truncated));
            }
        }

        Err(serde_json::Error::io(io::Error::other(
            "fixed pane state projection exceeds size budget",
        )))
    }

    /// Deserialize from JSON.
    ///
    /// # Errors
    /// Returns error if the JSON is invalid.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }
}

// =============================================================================
// Environment capture
// =============================================================================

/// Capture environment variables from an iterator, applying the safe-list
/// and redacting sensitive names.
fn capture_env_from_iter(vars: impl Iterator<Item = (String, String)>) -> CapturedEnv {
    let mut captured = std::collections::HashMap::new();
    let mut redacted_count = 0usize;
    let max_safe_name_bytes = SAFE_ENV_VARS
        .iter()
        .map(|name| name.len())
        .max()
        .unwrap_or(0);

    for (key, value) in vars {
        // Preserve redaction telemetry for ordinary sensitive names even when
        // they are longer than every allow-listed name. Keep the case-fold
        // bounded so an arbitrary caller key cannot force an equally large
        // Unicode allocation/scan.
        if key.len() <= MAX_ENV_NAME_CLASSIFICATION_BYTES && is_sensitive_var(&key) {
            redacted_count = redacted_count.saturating_add(1);
            continue;
        }

        if key.len() > max_safe_name_bytes {
            continue;
        }

        if SAFE_ENV_VARS.iter().any(|&safe| safe == key) {
            captured.insert(key, value);
        }
    }

    CapturedEnv {
        vars: captured,
        redacted_count,
    }
}

/// Check if a variable name matches sensitive patterns.
fn is_sensitive_var(name: &str) -> bool {
    let upper = name.to_uppercase();
    SENSITIVE_VAR_PATTERNS.iter().any(|pat| upper.contains(pat))
}

// =============================================================================
// PaneInfo integration
// =============================================================================

impl PaneStateSnapshot {
    /// Build a snapshot from a `PaneInfo` (from wezterm cli list).
    ///
    /// Extracts terminal size, cursor position, title, cwd, and alt-screen
    /// status (from the provided tracker state).
    #[must_use]
    pub fn from_pane_info(
        pane: &crate::wezterm::PaneInfo,
        captured_at: u64,
        is_alt_screen: bool,
    ) -> Self {
        let terminal = TerminalState {
            rows: terminal_size_dimension(pane.effective_rows()),
            cols: terminal_size_dimension(pane.effective_cols()),
            cursor_row: terminal_dimension(pane.cursor_y.unwrap_or(0)),
            cursor_col: terminal_dimension(pane.cursor_x.unwrap_or(0)),
            is_alt_screen,
            title: pane.title.clone().unwrap_or_default(),
        };

        let mut snapshot = Self::new(pane.pane_id, captured_at, terminal);
        if let Some(ref cwd) = pane.cwd {
            let parsed = crate::wezterm::CwdInfo::parse(cwd);
            snapshot.cwd = Some(parsed.path);
        }

        trace!(
            pane_id = pane.pane_id,
            has_cwd = snapshot.cwd.is_some(),
            alt_screen = is_alt_screen,
            "Captured state for pane"
        );

        snapshot
    }
}

fn terminal_size_dimension(value: u32) -> u16 {
    terminal_dimension(value.max(1))
}

fn terminal_dimension(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_terminal() -> TerminalState {
        TerminalState {
            rows: 24,
            cols: 80,
            cursor_row: 10,
            cursor_col: 5,
            is_alt_screen: false,
            title: "bash".to_string(),
        }
    }

    // ---- Roundtrip ----

    #[test]
    fn pane_state_roundtrip_minimal() {
        let snapshot = PaneStateSnapshot::new(0, 1000, make_terminal());
        let json = snapshot.to_json().unwrap();
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
        let (budgeted_json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(!truncated);
        assert_eq!(budgeted_json, json);
    }

    #[test]
    fn pane_state_roundtrip_full() {
        let snapshot = PaneStateSnapshot::new(5, 2000, make_terminal())
            .with_cwd("/home/user/project".to_string())
            .with_process(ProcessInfo {
                name: "claude-code".to_string(),
                pid: Some(12345),
                argv: Some(vec![
                    "claude-code".to_string(),
                    "--model".to_string(),
                    "opus".to_string(),
                ]),
            })
            .with_shell("zsh".to_string())
            .with_scrollback(ScrollbackRef {
                output_segments_seq: 42,
                retained_segment_count: 1000,
                last_capture_at: 1999,
            })
            .with_agent(AgentMetadata {
                agent_type: "claude_code".to_string(),
                session_id: Some("sess-123".to_string()),
                state: Some("working".to_string()),
            })
            .with_env_from_iter(std::iter::once(("PATH".to_owned(), "/usr/bin".to_owned())));

        let json = snapshot.to_json().unwrap();
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert_eq!(snapshot, restored);
        let (budgeted_json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(!truncated);
        assert_eq!(budgeted_json, json);
    }

    #[test]
    fn captured_environment_serialization_is_canonical() {
        let forward = PaneStateSnapshot::new(5, 2000, make_terminal()).with_env_from_iter(
            [
                ("TERM".to_owned(), "xterm-256color".to_owned()),
                ("HOME".to_owned(), "/home/operator".to_owned()),
                ("EDITOR".to_owned(), "hx".to_owned()),
            ]
            .into_iter(),
        );
        let reverse = PaneStateSnapshot::new(5, 2000, make_terminal()).with_env_from_iter(
            [
                ("EDITOR".to_owned(), "hx".to_owned()),
                ("HOME".to_owned(), "/home/operator".to_owned()),
                ("TERM".to_owned(), "xterm-256color".to_owned()),
            ]
            .into_iter(),
        );

        assert_eq!(forward, reverse);
        let canonical_json = forward.to_json().unwrap();
        assert_eq!(canonical_json, reverse.to_json().unwrap());
        let editor = canonical_json.find("\"EDITOR\"").unwrap();
        let home = canonical_json.find("\"HOME\"").unwrap();
        let term = canonical_json.find("\"TERM\"").unwrap();
        assert!(editor < home && home < term);
        assert_eq!(
            forward.to_json_budgeted().unwrap(),
            reverse.to_json_budgeted().unwrap()
        );
    }

    // ---- Alt-screen ----

    #[test]
    fn alt_screen_detection_captured() {
        let terminal = TerminalState {
            rows: 24,
            cols: 80,
            cursor_row: 0,
            cursor_col: 0,
            is_alt_screen: true,
            title: "vim".to_string(),
        };
        let snapshot = PaneStateSnapshot::new(0, 1000, terminal);
        assert!(snapshot.terminal.is_alt_screen);

        let json = snapshot.to_json().unwrap();
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert!(restored.terminal.is_alt_screen);
    }

    // ---- Env redaction ----

    #[test]
    fn env_redaction_secret_vars_removed() {
        let vars = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/home/user".to_string()),
            ("AWS_SECRET_KEY".to_string(), "super-secret".to_string()),
            ("API_TOKEN".to_string(), "tok-12345".to_string()),
            ("SHELL".to_string(), "/bin/bash".to_string()),
            ("MY_PASSWORD".to_string(), "hunter2".to_string()),
        ];

        let env = capture_env_from_iter(vars.into_iter());

        assert_eq!(env.vars.get("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(env.vars.get("HOME"), Some(&"/home/user".to_string()));
        assert_eq!(env.vars.get("SHELL"), Some(&"/bin/bash".to_string()));
        assert!(!env.vars.contains_key("AWS_SECRET_KEY"));
        assert!(!env.vars.contains_key("API_TOKEN"));
        assert!(!env.vars.contains_key("MY_PASSWORD"));
        assert_eq!(env.redacted_count, 3);
    }

    #[test]
    fn env_only_captures_safe_vars() {
        let vars = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("RANDOM_VAR".to_string(), "foo".to_string()),
            ("CUSTOM_THING".to_string(), "bar".to_string()),
            ("TERM".to_string(), "xterm-256color".to_string()),
        ];

        let env = capture_env_from_iter(vars.into_iter());

        assert!(env.vars.contains_key("PATH"));
        assert!(env.vars.contains_key("TERM"));
        assert!(!env.vars.contains_key("RANDOM_VAR"));
        assert!(!env.vars.contains_key("CUSTOM_THING"));
        assert_eq!(env.redacted_count, 0);
    }

    // ---- Size budget ----

    #[test]
    fn size_budget_small_snapshot_not_truncated() {
        let snapshot = PaneStateSnapshot::new(0, 1000, make_terminal());
        let (json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(!truncated);
        assert!(json.len() < PANE_STATE_SIZE_BUDGET);
    }

    #[test]
    fn size_budget_large_env_truncated() {
        let mut vars = std::collections::HashMap::new();
        // Create a large env that will push us over budget
        for i in 0..1000 {
            vars.insert(format!("VAR_{i}"), "x".repeat(100));
        }

        let mut snapshot = PaneStateSnapshot::new(0, 1000, make_terminal());
        snapshot.env = Some(CapturedEnv {
            vars,
            redacted_count: 0,
        });

        let (json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(truncated);
        assert!(json.len() <= PANE_STATE_SIZE_BUDGET);
    }

    #[test]
    fn size_budget_huge_title_cwd_process_and_agent_is_hard_bounded() {
        let hostile = "\u{1}".repeat(PANE_STATE_SIZE_BUDGET * 2);
        let mut snapshot = PaneStateSnapshot::new(
            7,
            1000,
            TerminalState {
                title: hostile.clone(),
                ..make_terminal()
            },
        )
        .with_cwd(hostile.clone())
        .with_shell(hostile.clone())
        .with_process(ProcessInfo {
            name: hostile.clone(),
            pid: Some(1),
            argv: Some(vec![hostile.clone()]),
        })
        .with_agent(AgentMetadata {
            agent_type: hostile.clone(),
            session_id: Some(hostile.clone()),
            state: Some(hostile),
        });
        snapshot.env = Some(CapturedEnv {
            vars: std::collections::HashMap::from([(
                "PATH".to_owned(),
                "x".repeat(PANE_STATE_SIZE_BUDGET * 2),
            )]),
            redacted_count: 0,
        });

        let (json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(truncated);
        assert!(json.len() <= PANE_STATE_SIZE_BUDGET);
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert!(restored.env.is_none());
        assert!(restored.foreground_process.is_none());
        assert!(restored.cwd.is_none());
        assert!(restored.shell.is_none());
        assert!(restored.agent.is_none());
        assert_eq!(restored.terminal.title, "");
    }

    #[test]
    fn size_budget_accounts_for_json_escape_expansion() {
        let snapshot = PaneStateSnapshot::new(
            8,
            1000,
            TerminalState {
                title: "\u{1}".repeat(PANE_STATE_SIZE_BUDGET / 2),
                ..make_terminal()
            },
        );

        let (json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(truncated);
        assert!(json.len() <= PANE_STATE_SIZE_BUDGET);
        assert_eq!(
            PaneStateSnapshot::from_json(&json).unwrap().terminal.title,
            ""
        );
    }

    // ---- Schema version forward compat ----

    #[test]
    fn schema_version_forward_compat() {
        let json = r#"{
            "schema_version": 2,
            "pane_id": 0,
            "captured_at": 1000,
            "terminal": {"rows": 24, "cols": 80, "cursor_row": 0, "cursor_col": 0, "is_alt_screen": false, "title": ""},
            "future_field": "ignored"
        }"#;
        let snapshot: PaneStateSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(snapshot.schema_version, 2);
        assert_eq!(snapshot.pane_id, 0);
    }

    // ---- PaneInfo integration ----

    #[test]
    fn from_pane_info_extracts_state() {
        let pane = crate::wezterm::PaneInfo {
            pane_id: 7,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: Some(crate::wezterm::PaneSize {
                rows: 30,
                cols: 120,
                pixel_width: None,
                pixel_height: None,
                dpi: None,
            }),
            rows: None,
            cols: None,
            title: Some("claude-code".to_string()),
            cwd: Some("file:///home/user/project".to_string()),
            tty_name: None,
            cursor_x: Some(15),
            cursor_y: Some(20),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: true,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };

        let snapshot = PaneStateSnapshot::from_pane_info(&pane, 5000, false);

        assert_eq!(snapshot.pane_id, 7);
        assert_eq!(snapshot.terminal.rows, 30);
        assert_eq!(snapshot.terminal.cols, 120);
        assert_eq!(snapshot.terminal.cursor_row, 20);
        assert_eq!(snapshot.terminal.cursor_col, 15);
        assert!(!snapshot.terminal.is_alt_screen);
        assert_eq!(snapshot.terminal.title, "claude-code");
        assert_eq!(snapshot.cwd, Some("/home/user/project".to_string()));
    }

    #[test]
    fn terminal_dimensions_saturate_instead_of_wrapping() {
        assert_eq!(terminal_size_dimension(0), 1);
        assert_eq!(terminal_dimension(u32::from(u16::MAX)), u16::MAX);
        assert_eq!(terminal_dimension(u32::from(u16::MAX) + 1), u16::MAX);
        assert_eq!(terminal_dimension(u32::MAX), u16::MAX);
    }

    // ---- Sensitive var detection ----

    #[test]
    fn is_sensitive_detects_patterns() {
        assert!(is_sensitive_var("AWS_SECRET_KEY"));
        assert!(is_sensitive_var("my_api_token"));
        assert!(is_sensitive_var("DB_PASSWORD"));
        assert!(is_sensitive_var("GITHUB_AUTH"));
        assert!(is_sensitive_var("Private_key_path"));

        assert!(!is_sensitive_var("PATH"));
        assert!(!is_sensitive_var("HOME"));
        assert!(!is_sensitive_var("SHELL"));
        assert!(!is_sensitive_var("TERM"));
    }

    // -----------------------------------------------------------------------
    // Batch 14 — PearlHeron wa-1u90p.7.1
    // -----------------------------------------------------------------------

    // ---- Builder chain ----

    #[test]
    fn builder_chain_sets_all_optional_fields() {
        let snapshot = PaneStateSnapshot::new(1, 5000, make_terminal())
            .with_cwd("/home/user".to_string())
            .with_shell("zsh".to_string())
            .with_process(ProcessInfo {
                name: "cargo".to_string(),
                pid: Some(999),
                argv: Some(vec!["cargo".to_string(), "test".to_string()]),
            })
            .with_scrollback(ScrollbackRef {
                output_segments_seq: 10,
                retained_segment_count: 500,
                last_capture_at: 4999,
            })
            .with_agent(AgentMetadata {
                agent_type: "codex".to_string(),
                session_id: Some("s-1".to_string()),
                state: Some("idle".to_string()),
            });

        assert_eq!(snapshot.cwd.as_deref(), Some("/home/user"));
        assert_eq!(snapshot.shell.as_deref(), Some("zsh"));
        assert_eq!(snapshot.foreground_process.as_ref().unwrap().name, "cargo");
        assert_eq!(
            snapshot
                .scrollback_ref
                .as_ref()
                .unwrap()
                .retained_segment_count,
            500
        );
        assert_eq!(snapshot.agent.as_ref().unwrap().agent_type, "codex");
    }

    // ---- Schema version ----

    #[test]
    fn new_snapshot_uses_current_schema_version() {
        let snapshot = PaneStateSnapshot::new(0, 0, make_terminal());
        assert_eq!(snapshot.schema_version, PANE_STATE_SCHEMA_VERSION);
    }

    // ---- Env capture from iterator: empty input ----

    #[test]
    fn env_capture_empty_iterator() {
        let env = capture_env_from_iter(std::iter::empty());
        assert!(env.vars.is_empty());
        assert_eq!(env.redacted_count, 0);
    }

    // ---- Env capture: sensitive patterns are case-insensitive ----

    #[test]
    fn is_sensitive_case_insensitive() {
        assert!(is_sensitive_var("aws_secret_key"));
        assert!(is_sensitive_var("Api_Key_Value"));
        assert!(is_sensitive_var("credential_store"));
        assert!(is_sensitive_var("PASSWD_FILE"));
    }

    // ---- Env capture: all safe vars captured when present ----

    #[test]
    fn env_captures_all_safe_vars() {
        let env = capture_env_from_iter(
            SAFE_ENV_VARS
                .iter()
                .map(|&name| (name.to_string(), format!("val_{name}"))),
        );
        for &name in SAFE_ENV_VARS {
            assert!(
                env.vars.contains_key(name),
                "Safe var {name} should be captured"
            );
        }
        assert_eq!(env.redacted_count, 0);
    }

    // ---- Env capture: non-safe non-sensitive vars are silently dropped ----

    #[test]
    fn env_drops_unknown_vars_without_counting_as_redacted() {
        let vars = vec![
            ("COMPLETELY_CUSTOM".to_string(), "value".to_string()),
            ("MY_APP_FLAG".to_string(), "true".to_string()),
        ];
        let env = capture_env_from_iter(vars.into_iter());
        assert!(env.vars.is_empty());
        assert_eq!(env.redacted_count, 0);
    }

    #[test]
    fn env_drops_oversized_names_before_sensitive_classification() {
        let oversized_sensitive_name = format!("{}TOKEN", "X".repeat(1_000_000));
        let env = capture_env_from_iter(
            vec![(oversized_sensitive_name, "must-not-be-captured".to_string())].into_iter(),
        );
        assert!(env.vars.is_empty());
        assert_eq!(env.redacted_count, 0);
    }

    #[test]
    fn env_counts_common_sensitive_name_longer_than_allowlist_names() {
        let env = capture_env_from_iter(
            [(
                "AWS_SECRET_ACCESS_KEY".to_owned(),
                "must-not-be-captured".to_owned(),
            )]
            .into_iter(),
        );
        assert!(env.vars.is_empty());
        assert_eq!(env.redacted_count, 1);
    }

    #[test]
    fn env_sensitive_classification_cap_is_exact() {
        let suffix = "_TOKEN";
        let at_cap = format!(
            "{}{}",
            "X".repeat(MAX_ENV_NAME_CLASSIFICATION_BYTES - suffix.len()),
            suffix
        );
        let over_cap = format!("X{at_cap}");
        let env = capture_env_from_iter(
            [
                (at_cap, "at-cap".to_owned()),
                (over_cap, "over-cap".to_owned()),
            ]
            .into_iter(),
        );
        assert!(env.vars.is_empty());
        assert_eq!(env.redacted_count, 1);
    }

    // ---- with_env_from_iter via builder ----

    #[test]
    fn with_env_from_iter_captures_safe_vars() {
        let vars = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("TERM".to_string(), "xterm".to_string()),
            ("NOT_SAFE".to_string(), "ignored".to_string()),
        ];
        let snapshot =
            PaneStateSnapshot::new(0, 0, make_terminal()).with_env_from_iter(vars.into_iter());
        let env = snapshot.env.unwrap();
        assert_eq!(env.vars.len(), 2);
        assert_eq!(env.vars.get("PATH"), Some(&"/usr/bin".to_string()));
        assert_eq!(env.vars.get("TERM"), Some(&"xterm".to_string()));
    }

    // ---- to_json_budgeted: large argv truncated when env already removed ----

    #[test]
    fn size_budget_large_argv_truncated() {
        let mut snapshot =
            PaneStateSnapshot::new(0, 1000, make_terminal()).with_process(ProcessInfo {
                name: "test".to_string(),
                pid: Some(1),
                argv: Some(vec!["x".repeat(80_000)]),
            });
        // Also add large env so we trigger the two-stage truncation
        let mut vars = std::collections::HashMap::new();
        for i in 0..500 {
            vars.insert(format!("VAR_{i}"), "x".repeat(100));
        }
        snapshot.env = Some(CapturedEnv {
            vars,
            redacted_count: 0,
        });

        let (json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(truncated);
        let restored: PaneStateSnapshot = serde_json::from_str(&json).unwrap();
        assert!(restored.env.is_none());
        assert!(restored.foreground_process.as_ref().unwrap().argv.is_none());
        assert!(json.len() <= PANE_STATE_SIZE_BUDGET);
    }

    // ---- ProcessInfo fields ----

    #[test]
    fn process_info_minimal() {
        let p = ProcessInfo {
            name: "bash".to_string(),
            pid: None,
            argv: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let restored: ProcessInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "bash");
        assert!(restored.pid.is_none());
        assert!(restored.argv.is_none());
    }

    // ---- TerminalState defaults ----

    #[test]
    fn terminal_state_serde_defaults_on_missing_fields() {
        let json = r#"{"rows":24,"cols":80}"#;
        let terminal: TerminalState = serde_json::from_str(json).unwrap();
        assert_eq!(terminal.rows, 24);
        assert_eq!(terminal.cols, 80);
        assert_eq!(terminal.cursor_row, 0);
        assert_eq!(terminal.cursor_col, 0);
        assert!(!terminal.is_alt_screen);
        assert_eq!(terminal.title, "");
    }

    // ---- Constants ----

    #[test]
    fn pane_state_size_budget_is_64kb() {
        assert_eq!(PANE_STATE_SIZE_BUDGET, 65_536);
    }

    #[test]
    fn safe_env_vars_contains_expected_entries() {
        assert!(SAFE_ENV_VARS.contains(&"PATH"));
        assert!(SAFE_ENV_VARS.contains(&"HOME"));
        assert!(SAFE_ENV_VARS.contains(&"SHELL"));
        assert!(SAFE_ENV_VARS.contains(&"TERM"));
        assert!(SAFE_ENV_VARS.contains(&"EDITOR"));
        assert!(SAFE_ENV_VARS.contains(&"FT_WORKSPACE"));
    }

    #[test]
    fn sensitive_patterns_contains_expected_entries() {
        assert!(SENSITIVE_VAR_PATTERNS.contains(&"SECRET"));
        assert!(SENSITIVE_VAR_PATTERNS.contains(&"TOKEN"));
        assert!(SENSITIVE_VAR_PATTERNS.contains(&"PASSWORD"));
        assert!(SENSITIVE_VAR_PATTERNS.contains(&"API_KEY"));
    }

    // -----------------------------------------------------------------------
    // Batch — RubyBeaver wa-1u90p.7.1
    // -----------------------------------------------------------------------

    // ---- Sub-type serde roundtrips ----

    #[test]
    fn scrollback_ref_serde_roundtrip() {
        let sr = ScrollbackRef {
            output_segments_seq: -5,
            retained_segment_count: 999_999,
            last_capture_at: u64::MAX,
        };
        let json = serde_json::to_string(&sr).unwrap();
        let restored: ScrollbackRef = serde_json::from_str(&json).unwrap();
        assert_eq!(sr, restored);
    }

    #[test]
    fn pane_state_v2_scrollback_wire_rejects_legacy_cardinality_names() {
        let snapshot =
            PaneStateSnapshot::new(7, 100, make_terminal()).with_scrollback(ScrollbackRef {
                output_segments_seq: 9,
                retained_segment_count: 3,
                last_capture_at: 99,
            });
        let json = snapshot.to_json().unwrap();
        assert!(json.contains("\"schema_version\":2"));
        assert!(json.contains("\"retained_segment_count\":3"));
        assert!(!json.contains("total_lines_captured"));
        assert!(!json.contains("total_segments_captured"));
        assert_eq!(PaneStateSnapshot::from_json(&json).unwrap(), snapshot);

        for legacy_name in ["total_lines_captured", "total_segments_captured"] {
            let legacy = format!(
                "{{\"schema_version\":1,\"pane_id\":7,\"captured_at\":100,\
                 \"terminal\":{{\"rows\":24,\"cols\":80}},\
                 \"scrollback_ref\":{{\"output_segments_seq\":9,\
                 \"{legacy_name}\":3,\"last_capture_at\":99}}}}"
            );
            assert!(
                PaneStateSnapshot::from_json(&legacy).is_err(),
                "legacy cardinality key {legacy_name} must not cross the v2 boundary"
            );
        }
    }

    #[test]
    fn agent_metadata_minimal_serde_roundtrip() {
        let am = AgentMetadata {
            agent_type: "gemini".to_string(),
            session_id: None,
            state: None,
        };
        let json = serde_json::to_string(&am).unwrap();
        let restored: AgentMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(am, restored);
        // Optional fields should be absent in JSON
        assert!(!json.contains("session_id"));
        assert!(!json.contains("state"));
    }

    #[test]
    fn captured_env_serde_roundtrip() {
        let mut vars = std::collections::HashMap::new();
        vars.insert("PATH".to_string(), "/usr/bin:/usr/local/bin".to_string());
        vars.insert("HOME".to_string(), "/home/test".to_string());
        let ce = CapturedEnv {
            vars,
            redacted_count: 7,
        };
        let json = serde_json::to_string(&ce).unwrap();
        let restored: CapturedEnv = serde_json::from_str(&json).unwrap();
        assert_eq!(ce, restored);
    }

    #[test]
    fn process_info_full_serde_roundtrip() {
        let p = ProcessInfo {
            name: "claude-code".to_string(),
            pid: Some(u32::MAX),
            argv: Some(vec![
                "claude-code".to_string(),
                "--flag".to_string(),
                String::new(),
            ]),
        };
        let json = serde_json::to_string(&p).unwrap();
        let restored: ProcessInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(p, restored);
    }

    #[test]
    fn terminal_state_full_roundtrip() {
        let ts = TerminalState {
            rows: u16::MAX,
            cols: u16::MAX,
            cursor_row: u16::MAX,
            cursor_col: u16::MAX,
            is_alt_screen: true,
            title: "a".repeat(500),
        };
        let json = serde_json::to_string(&ts).unwrap();
        let restored: TerminalState = serde_json::from_str(&json).unwrap();
        assert_eq!(ts, restored);
    }

    // ---- from_pane_info edge cases ----

    #[test]
    fn from_pane_info_no_cwd_leaves_cwd_none() {
        let pane = crate::wezterm::PaneInfo {
            pane_id: 1,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: Some(40),
            cols: Some(100),
            title: Some("test".to_string()),
            cwd: None,
            tty_name: None,
            cursor_x: Some(5),
            cursor_y: Some(10),
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        let snapshot = PaneStateSnapshot::from_pane_info(&pane, 1000, false);
        assert!(snapshot.cwd.is_none());
        // Falls back to legacy rows/cols
        assert_eq!(snapshot.terminal.rows, 40);
        assert_eq!(snapshot.terminal.cols, 100);
    }

    #[test]
    fn from_pane_info_no_title_defaults_to_empty() {
        let pane = crate::wezterm::PaneInfo {
            pane_id: 2,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        let snapshot = PaneStateSnapshot::from_pane_info(&pane, 2000, true);
        assert_eq!(snapshot.terminal.title, "");
        // Cursor defaults to 0 when not present
        assert_eq!(snapshot.terminal.cursor_row, 0);
        assert_eq!(snapshot.terminal.cursor_col, 0);
        // No size or legacy rows/cols => defaults 24x80
        assert_eq!(snapshot.terminal.rows, 24);
        assert_eq!(snapshot.terminal.cols, 80);
        // Alt-screen flag is passed through
        assert!(snapshot.terminal.is_alt_screen);
    }

    #[test]
    fn from_pane_info_uses_legacy_rows_cols_fallback() {
        let pane = crate::wezterm::PaneInfo {
            pane_id: 3,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None, // no nested size
            rows: Some(50),
            cols: Some(200),
            title: Some("legacy".to_string()),
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        let snapshot = PaneStateSnapshot::from_pane_info(&pane, 3000, false);
        assert_eq!(snapshot.terminal.rows, 50);
        assert_eq!(snapshot.terminal.cols, 200);
    }

    // ---- Builder edge cases ----

    #[test]
    fn with_cwd_overwrites_previous_value() {
        let snapshot = PaneStateSnapshot::new(0, 0, make_terminal())
            .with_cwd("/first".to_string())
            .with_cwd("/second".to_string());
        assert_eq!(snapshot.cwd.as_deref(), Some("/second"));
    }

    #[test]
    fn builder_order_does_not_matter() {
        let a = PaneStateSnapshot::new(1, 100, make_terminal())
            .with_cwd("/tmp".to_string())
            .with_shell("bash".to_string());

        let b = PaneStateSnapshot::new(1, 100, make_terminal())
            .with_shell("bash".to_string())
            .with_cwd("/tmp".to_string());

        assert_eq!(a, b);
    }

    // ---- Size budget boundary ----

    #[test]
    fn size_budget_at_exact_boundary_not_truncated() {
        let mut empty_value_snapshot = PaneStateSnapshot::new(0, 1000, make_terminal());
        empty_value_snapshot.env = Some(CapturedEnv {
            vars: std::collections::HashMap::from([("X".to_owned(), String::new())]),
            redacted_count: 0,
        });
        let empty_value_bytes = empty_value_snapshot.to_json().unwrap().len();
        let padding_needed = PANE_STATE_SIZE_BUDGET
            .checked_sub(empty_value_bytes)
            .expect("fixed snapshot must fit below the pane-state budget");

        let mut exact_snapshot = empty_value_snapshot;
        exact_snapshot
            .env
            .as_mut()
            .unwrap()
            .vars
            .insert("X".to_owned(), "y".repeat(padding_needed));
        let exact_json = exact_snapshot.to_json().unwrap();
        assert_eq!(exact_json.len(), PANE_STATE_SIZE_BUDGET);

        let (budgeted_json, truncated) = exact_snapshot.to_json_budgeted().unwrap();
        assert!(!truncated);
        assert_eq!(budgeted_json, exact_json);

        exact_snapshot
            .env
            .as_mut()
            .unwrap()
            .vars
            .insert("X".to_owned(), "y".repeat(padding_needed + 1));
        let (budgeted_json, truncated) = exact_snapshot.to_json_budgeted().unwrap();
        assert!(truncated);
        assert!(budgeted_json.len() <= PANE_STATE_SIZE_BUDGET);
        assert!(
            PaneStateSnapshot::from_json(&budgeted_json)
                .unwrap()
                .env
                .is_none()
        );
    }

    #[test]
    fn size_budget_small_argv_without_env_is_not_truncated() {
        let snapshot = PaneStateSnapshot::new(0, 1000, make_terminal()).with_process(ProcessInfo {
            name: "test".to_string(),
            pid: Some(1),
            argv: Some(vec!["arg".to_string(); 10]),
        });
        let (_, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(!truncated);
    }

    #[test]
    fn size_budget_only_argv_oversized_no_env_to_remove() {
        // Snapshot with no env but argv so large it exceeds budget
        let snapshot = PaneStateSnapshot::new(0, 1000, make_terminal()).with_process(ProcessInfo {
            name: "test".to_string(),
            pid: Some(1),
            argv: Some(vec!["x".repeat(70_000)]),
        });
        let (json, truncated) = snapshot.to_json_budgeted().unwrap();
        assert!(truncated);
        // argv should be removed in the result
        let restored: PaneStateSnapshot = serde_json::from_str(&json).unwrap();
        assert!(restored.foreground_process.as_ref().unwrap().argv.is_none());
        // process name is preserved
        assert_eq!(restored.foreground_process.as_ref().unwrap().name, "test");
    }

    // ---- Sensitive variable detection edge cases ----

    #[test]
    fn is_sensitive_detects_passwd_pattern() {
        assert!(is_sensitive_var("MYSQL_PASSWD"));
        assert!(is_sensitive_var("passwd_file"));
        assert!(is_sensitive_var("MyPasswdStore"));
    }

    #[test]
    fn is_sensitive_detects_credential_pattern() {
        assert!(is_sensitive_var("GOOGLE_CREDENTIAL"));
        assert!(is_sensitive_var("credential_helper"));
        assert!(is_sensitive_var("SERVICE_CREDENTIALS"));
    }

    #[test]
    fn is_sensitive_detects_private_pattern() {
        assert!(is_sensitive_var("SSH_PRIVATE_KEY"));
        assert!(is_sensitive_var("private_key_path"));
        assert!(is_sensitive_var("TLS_PRIVATE"));
    }

    #[test]
    fn is_sensitive_partial_match_on_key_substring() {
        // "KEY" pattern matches within variable names containing "key"
        assert!(is_sensitive_var("ENCRYPTION_KEY_ID"));
        assert!(is_sensitive_var("ssh_key"));
        assert!(is_sensitive_var("MONKEY")); // contains "KEY"
    }

    #[test]
    fn is_sensitive_does_not_match_unrelated_words() {
        assert!(!is_sensitive_var("PATH"));
        assert!(!is_sensitive_var("DISPLAY"));
        assert!(!is_sensitive_var("LANG"));
        assert!(!is_sensitive_var("COLORTERM"));
        assert!(!is_sensitive_var("SHLVL"));
        assert!(!is_sensitive_var("XDG_RUNTIME_DIR"));
    }

    // ---- Env capture: mixed safe + sensitive + unknown ----

    #[test]
    fn env_capture_mixed_safe_sensitive_unknown() {
        let vars = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("AWS_SECRET_KEY".to_string(), "secret!".to_string()),
            ("RANDOM_CUSTOM".to_string(), "value".to_string()),
            ("TERM".to_string(), "xterm".to_string()),
            ("DB_PASSWORD".to_string(), "pw123".to_string()),
            ("EDITOR".to_string(), "vim".to_string()),
            ("NOT_LISTED".to_string(), "nope".to_string()),
        ];
        let env = capture_env_from_iter(vars.into_iter());

        // Safe vars captured
        assert_eq!(env.vars.len(), 3);
        assert!(env.vars.contains_key("PATH"));
        assert!(env.vars.contains_key("TERM"));
        assert!(env.vars.contains_key("EDITOR"));
        // Sensitive vars counted as redacted
        assert_eq!(env.redacted_count, 2);
        // Unknown vars neither captured nor counted
        assert!(!env.vars.contains_key("RANDOM_CUSTOM"));
        assert!(!env.vars.contains_key("NOT_LISTED"));
    }

    // ---- Empty and null field handling ----

    #[test]
    fn snapshot_all_none_optionals_serializes_compactly() {
        let snapshot = PaneStateSnapshot::new(0, 0, make_terminal());
        let json = snapshot.to_json().unwrap();

        // skip_serializing_if = "Option::is_none" means these keys are absent
        assert!(!json.contains("\"cwd\""));
        assert!(!json.contains("\"foreground_process\""));
        assert!(!json.contains("\"shell\""));
        assert!(!json.contains("\"scrollback_ref\""));
        assert!(!json.contains("\"agent\""));
        assert!(!json.contains("\"env\""));
    }

    #[test]
    fn from_json_with_empty_string_fails() {
        let result = PaneStateSnapshot::from_json("");
        assert!(result.is_err());
    }

    #[test]
    fn from_json_with_invalid_json_fails() {
        let result = PaneStateSnapshot::from_json("{not valid json}");
        assert!(result.is_err());
    }

    #[test]
    fn from_json_missing_required_field_fails() {
        // Missing "terminal" field which is required
        let json = r#"{"schema_version":1,"pane_id":0,"captured_at":0}"#;
        let result = PaneStateSnapshot::from_json(json);
        assert!(result.is_err());
    }

    // ---- ProcessInfo edge cases ----

    #[test]
    fn process_info_empty_argv_vec_roundtrip() {
        let p = ProcessInfo {
            name: "shell".to_string(),
            pid: Some(1),
            argv: Some(vec![]),
        };
        let json = serde_json::to_string(&p).unwrap();
        let restored: ProcessInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.argv, Some(vec![]));
    }

    #[test]
    fn process_info_empty_name_roundtrip() {
        let p = ProcessInfo {
            name: String::new(),
            pid: None,
            argv: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        let restored: ProcessInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "");
    }

    // ---- AgentMetadata deserializes with unknown fields ----

    #[test]
    fn agent_metadata_ignores_unknown_json_fields() {
        let json = r#"{"agent_type":"codex","session_id":"s1","state":"idle","future_field":"ok"}"#;
        let am: AgentMetadata = serde_json::from_str(json).unwrap();
        assert_eq!(am.agent_type, "codex");
        assert_eq!(am.session_id, Some("s1".to_string()));
        assert_eq!(am.state, Some("idle".to_string()));
    }

    // ---- Safe env vars: FT_ prefixed vars ----

    #[test]
    fn safe_env_vars_include_ft_prefixed() {
        assert!(SAFE_ENV_VARS.contains(&"FT_WORKSPACE"));
        assert!(SAFE_ENV_VARS.contains(&"FT_OUTPUT_FORMAT"));

        let vars = vec![
            ("FT_WORKSPACE".to_string(), "/workspace".to_string()),
            ("FT_OUTPUT_FORMAT".to_string(), "json".to_string()),
            ("FT_CUSTOM".to_string(), "not_safe".to_string()),
        ];
        let env = capture_env_from_iter(vars.into_iter());
        assert!(env.vars.contains_key("FT_WORKSPACE"));
        assert!(env.vars.contains_key("FT_OUTPUT_FORMAT"));
        // FT_CUSTOM is not in the safe list
        assert!(!env.vars.contains_key("FT_CUSTOM"));
    }

    // ---- Captured env with empty vars HashMap ----

    #[test]
    fn captured_env_empty_roundtrip() {
        let ce = CapturedEnv {
            vars: std::collections::HashMap::new(),
            redacted_count: 0,
        };
        let json = serde_json::to_string(&ce).unwrap();
        let restored: CapturedEnv = serde_json::from_str(&json).unwrap();
        assert!(restored.vars.is_empty());
        assert_eq!(restored.redacted_count, 0);
    }

    // ---- Schema version constant ----

    #[test]
    fn schema_version_is_two() {
        assert_eq!(PANE_STATE_SCHEMA_VERSION, 2);
    }

    // ---- Pane state with zero pane_id and captured_at ----

    #[test]
    fn snapshot_zero_ids_roundtrip() {
        let snapshot = PaneStateSnapshot::new(
            0,
            0,
            TerminalState {
                rows: 1,
                cols: 1,
                cursor_row: 0,
                cursor_col: 0,
                is_alt_screen: false,
                title: String::new(),
            },
        );
        let json = snapshot.to_json().unwrap();
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert_eq!(restored.pane_id, 0);
        assert_eq!(restored.captured_at, 0);
        assert_eq!(restored.terminal.rows, 1);
        assert_eq!(restored.terminal.cols, 1);
    }

    // ---- Pane state with u64::MAX pane_id ----

    #[test]
    fn snapshot_max_pane_id_roundtrip() {
        let snapshot = PaneStateSnapshot::new(u64::MAX, u64::MAX, make_terminal());
        let json = snapshot.to_json().unwrap();
        let restored = PaneStateSnapshot::from_json(&json).unwrap();
        assert_eq!(restored.pane_id, u64::MAX);
        assert_eq!(restored.captured_at, u64::MAX);
    }
}
