//! Property-based tests for `frankenterm_core::session_pane_state` types.
//!
//! Validates:
//! 1.  PaneStateSnapshot serde roundtrip (minimal -- terminal only)
//! 2.  PaneStateSnapshot serde roundtrip (all fields populated)
//! 3.  PaneStateSnapshot serde roundtrip (optional fields None)
//! 4.  PaneStateSnapshot::new() sets schema_version = 1, optionals = None
//! 5.  Builder with_cwd sets cwd field
//! 6.  Builder with_process sets foreground_process field
//! 7.  Builder with_shell sets shell field
//! 8.  Builder with_scrollback sets scrollback_ref field
//! 9.  Builder with_agent sets agent field
//! 10. Builder full chain sets all fields correctly
//! 11. ProcessInfo serde roundtrip (with pid and argv)
//! 12. ProcessInfo serde roundtrip (without pid)
//! 13. ProcessInfo serde roundtrip (without argv)
//! 14. ProcessInfo serde roundtrip (pid and argv both None)
//! 15. TerminalState serde roundtrip (all fields set)
//! 16. TerminalState deserialized defaults (cursor_row/col=0, is_alt_screen=false, title="")
//! 17. ScrollbackRef serde roundtrip
//! 18. AgentMetadata serde roundtrip (all fields populated)
//! 19. AgentMetadata serde roundtrip (session_id None)
//! 20. AgentMetadata serde roundtrip (state None)
//! 21. AgentMetadata serde roundtrip (both optional fields None)
//! 22. CapturedEnv serde roundtrip
//! 23. Env capture -- safe vars captured via with_env_from_iter
//! 24. Env capture -- all 17 safe var names individually captured
//! 25. Env capture -- non-safe vars excluded
//! 26. Env capture -- sensitive vars with SECRET pattern redacted
//! 27. Env capture -- sensitive vars with TOKEN pattern redacted
//! 28. Env capture -- sensitive vars with KEY pattern redacted
//! 29. Env capture -- sensitive vars with PASSWORD pattern redacted
//! 30. Env capture -- sensitive vars with CREDENTIAL pattern redacted
//! 31. Env capture -- sensitive vars with AUTH pattern redacted
//! 32. Env capture -- sensitive vars with API_KEY pattern redacted
//! 33. Env capture -- sensitive vars with PRIVATE pattern redacted
//! 34. Env capture -- sensitive vars with PASSWD pattern redacted
//! 35. Env capture -- case insensitive sensitivity check
//! 36. Env capture -- redacted_count matches number of sensitive vars
//! 37. Size budget -- small snapshot not truncated
//! 38. Size budget -- large env triggers truncation and env removed
//! 39. Size budget -- truncated output <= PANE_STATE_SIZE_BUDGET
//! 40. Size budget -- large argv also truncated when env removal insufficient
//! 41. Schema version -- new() always returns PANE_STATE_SCHEMA_VERSION (1)
//! 42. Forward compat -- JSON with schema_version=2 and unknown fields parses
//! 43. Forward compat -- future schema version preserved in roundtrip
//! 44. PartialEq -- identical snapshots are equal
//! 45. PartialEq -- different pane_id produces inequality
//! 46. PartialEq -- different terminal produces inequality
//! 47. Clone -- clone produces equal snapshot
//! 48. Clone -- mutating clone does not affect original
//! 49. from_json rejects invalid JSON
//! 50. from_json rejects empty string
//!
//! Pure property tests only -- no async, no I/O.

use proptest::prelude::*;
use std::collections::HashMap;

use frankenterm_core::session_pane_state::{
    AgentMetadata, CapturedEnv, PANE_STATE_SCHEMA_VERSION, PANE_STATE_SIZE_BUDGET,
    PaneStateSnapshot, ProcessInfo, ScrollbackRef, TerminalState,
};

// =============================================================================
// Safe env var names (mirrored from source for test assertions)
// =============================================================================

const SAFE_ENV_VARS: &[&str] = &[
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

const SENSITIVE_PATTERNS: &[&str] = &[
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

// =============================================================================
// Strategies
// =============================================================================

/// Arbitrary short alphanumeric string suitable for names, paths, etc.
fn arb_short_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_/.-]{0,50}"
}

/// Arbitrary non-empty short string.
fn arb_nonempty_string() -> impl Strategy<Value = String> {
    "[a-zA-Z0-9_/.-]{1,50}"
}

/// Arbitrary pane ID.
fn arb_pane_id() -> impl Strategy<Value = u64> {
    any::<u64>()
}

/// Arbitrary timestamp in reasonable epoch-ms range.
fn arb_timestamp() -> impl Strategy<Value = u64> {
    1_000_000u64..2_000_000_000_000u64
}

/// Arbitrary TerminalState with sensible ranges.
fn arb_terminal_state() -> impl Strategy<Value = TerminalState> {
    (
        1u16..=500,
        1u16..=500,
        0u16..=499,
        0u16..=499,
        any::<bool>(),
        arb_short_string(),
    )
        .prop_map(
            |(rows, cols, cursor_row, cursor_col, is_alt_screen, title)| TerminalState {
                rows,
                cols,
                cursor_row: cursor_row.min(rows.saturating_sub(1)),
                cursor_col: cursor_col.min(cols.saturating_sub(1)),
                is_alt_screen,
                title,
            },
        )
}

/// Arbitrary ProcessInfo with all fields populated.
fn arb_process_info_full() -> impl Strategy<Value = ProcessInfo> {
    (
        arb_nonempty_string(),
        any::<u32>(),
        proptest::collection::vec(arb_short_string(), 0..6),
    )
        .prop_map(|(name, pid, argv)| ProcessInfo {
            name,
            pid: Some(pid),
            argv: Some(argv),
        })
}

/// Arbitrary ProcessInfo with optional fields.
fn arb_process_info() -> impl Strategy<Value = ProcessInfo> {
    (
        arb_nonempty_string(),
        proptest::option::of(any::<u32>()),
        proptest::option::of(proptest::collection::vec(arb_short_string(), 0..6)),
    )
        .prop_map(|(name, pid, argv)| ProcessInfo { name, pid, argv })
}

/// Arbitrary ScrollbackRef.
fn arb_scrollback_ref() -> impl Strategy<Value = ScrollbackRef> {
    (any::<i64>(), any::<u64>(), arb_timestamp()).prop_map(
        |(output_segments_seq, total_lines_captured, last_capture_at)| ScrollbackRef {
            output_segments_seq,
            total_lines_captured,
            last_capture_at,
        },
    )
}

/// Arbitrary AgentMetadata with all fields populated.
fn arb_agent_metadata_full() -> impl Strategy<Value = AgentMetadata> {
    (
        arb_nonempty_string(),
        arb_nonempty_string(),
        arb_nonempty_string(),
    )
        .prop_map(|(agent_type, session_id, state)| AgentMetadata {
            agent_type,
            session_id: Some(session_id),
            state: Some(state),
        })
}

/// Arbitrary AgentMetadata with optional fields.
fn arb_agent_metadata() -> impl Strategy<Value = AgentMetadata> {
    (
        arb_nonempty_string(),
        proptest::option::of(arb_nonempty_string()),
        proptest::option::of(arb_nonempty_string()),
    )
        .prop_map(|(agent_type, session_id, state)| AgentMetadata {
            agent_type,
            session_id,
            state,
        })
}

/// Arbitrary CapturedEnv.
fn arb_captured_env() -> impl Strategy<Value = CapturedEnv> {
    (
        proptest::collection::hash_map(arb_nonempty_string(), arb_short_string(), 0..10),
        0usize..20,
    )
        .prop_map(|(vars, redacted_count)| CapturedEnv {
            vars,
            redacted_count,
        })
}

/// Arbitrary PaneStateSnapshot with only required fields (minimal).
fn arb_snapshot_minimal() -> impl Strategy<Value = PaneStateSnapshot> {
    (arb_pane_id(), arb_timestamp(), arb_terminal_state()).prop_map(
        |(pane_id, captured_at, terminal)| PaneStateSnapshot::new(pane_id, captured_at, terminal),
    )
}

/// Arbitrary PaneStateSnapshot with all fields populated.
fn arb_snapshot_full() -> impl Strategy<Value = PaneStateSnapshot> {
    (
        arb_pane_id(),
        arb_timestamp(),
        arb_terminal_state(),
        arb_nonempty_string(),
        arb_process_info_full(),
        arb_nonempty_string(),
        arb_scrollback_ref(),
        arb_agent_metadata_full(),
        arb_captured_env(),
    )
        .prop_map(
            |(pane_id, captured_at, terminal, cwd, process, shell, scrollback, agent, env)| {
                PaneStateSnapshot::new(pane_id, captured_at, terminal)
                    .with_cwd(cwd)
                    .with_process(process)
                    .with_shell(shell)
                    .with_scrollback(scrollback)
                    .with_agent(agent)
                    .with_env_from_iter(std::iter::empty())
                    // Override env directly since with_env_from_iter filters
                    .tap_env(env)
            },
        )
}

/// Helper trait to set env directly on a snapshot (bypassing capture logic).
trait TapEnv {
    fn tap_env(self, env: CapturedEnv) -> Self;
}

impl TapEnv for PaneStateSnapshot {
    fn tap_env(mut self, env: CapturedEnv) -> Self {
        self.env = Some(env);
        self
    }
}

// =============================================================================
// 1-3. PaneStateSnapshot serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_state_roundtrip_minimal(snap in arb_snapshot_minimal()) {
        let json = snap.to_json().expect("serialization should succeed");
        let restored = PaneStateSnapshot::from_json(&json).expect("deserialization should succeed");
        prop_assert_eq!(&snap, &restored, "minimal roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_state_roundtrip_full(snap in arb_snapshot_full()) {
        let json = snap.to_json().expect("serialization should succeed");
        let restored = PaneStateSnapshot::from_json(&json).expect("deserialization should succeed");
        prop_assert_eq!(&snap, &restored, "full roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn pane_state_roundtrip_optional_fields_none(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal);
        // All optional fields should be None
        prop_assert!(snap.cwd.is_none(), "cwd should be None");
        prop_assert!(snap.foreground_process.is_none(), "foreground_process should be None");
        prop_assert!(snap.shell.is_none(), "shell should be None");
        prop_assert!(snap.scrollback_ref.is_none(), "scrollback_ref should be None");
        prop_assert!(snap.agent.is_none(), "agent should be None");
        prop_assert!(snap.env.is_none(), "env should be None");

        let json = snap.to_json().expect("serialization should succeed");
        let restored = PaneStateSnapshot::from_json(&json).expect("deserialization should succeed");
        prop_assert_eq!(&snap, &restored, "optional-none roundtrip mismatch");
    }
}

// =============================================================================
// 4. PaneStateSnapshot::new() invariants
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn new_sets_schema_version_and_none_optionals(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal.clone());
        prop_assert_eq!(snap.schema_version, PANE_STATE_SCHEMA_VERSION, "schema_version should be {}", PANE_STATE_SCHEMA_VERSION);
        prop_assert_eq!(snap.pane_id, pane_id, "pane_id mismatch");
        prop_assert_eq!(snap.captured_at, captured_at, "captured_at mismatch");
        prop_assert_eq!(&snap.terminal, &terminal, "terminal mismatch");
        prop_assert!(snap.cwd.is_none(), "cwd should be None");
        prop_assert!(snap.foreground_process.is_none(), "foreground_process should be None");
        prop_assert!(snap.shell.is_none(), "shell should be None");
        prop_assert!(snap.scrollback_ref.is_none(), "scrollback_ref should be None");
        prop_assert!(snap.agent.is_none(), "agent should be None");
        prop_assert!(snap.env.is_none(), "env should be None");
    }
}

// =============================================================================
// 5-10. Builder methods
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn builder_with_cwd(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
        cwd in arb_nonempty_string(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_cwd(cwd.clone());
        prop_assert_eq!(snap.cwd.as_deref(), Some(cwd.as_str()), "cwd should match");
        // Other optionals remain None
        prop_assert!(snap.foreground_process.is_none(), "foreground_process should still be None");
        prop_assert!(snap.shell.is_none(), "shell should still be None");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn builder_with_process(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
        process in arb_process_info(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_process(process.clone());
        prop_assert_eq!(&snap.foreground_process, &Some(process), "process should match");
        prop_assert!(snap.cwd.is_none(), "cwd should still be None");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn builder_with_shell(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
        shell in arb_nonempty_string(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_shell(shell.clone());
        prop_assert_eq!(snap.shell.as_deref(), Some(shell.as_str()), "shell should match");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn builder_with_scrollback(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
        scrollback in arb_scrollback_ref(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_scrollback(scrollback.clone());
        prop_assert_eq!(&snap.scrollback_ref, &Some(scrollback), "scrollback_ref should match");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn builder_with_agent(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
        agent in arb_agent_metadata(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_agent(agent.clone());
        prop_assert_eq!(&snap.agent, &Some(agent), "agent should match");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn builder_full_chain(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
        cwd in arb_nonempty_string(),
        process in arb_process_info_full(),
        shell in arb_nonempty_string(),
        scrollback in arb_scrollback_ref(),
        agent in arb_agent_metadata_full(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal.clone())
            .with_cwd(cwd.clone())
            .with_process(process.clone())
            .with_shell(shell.clone())
            .with_scrollback(scrollback.clone())
            .with_agent(agent.clone());

        prop_assert_eq!(snap.pane_id, pane_id, "pane_id mismatch");
        prop_assert_eq!(snap.captured_at, captured_at, "captured_at mismatch");
        prop_assert_eq!(&snap.terminal, &terminal, "terminal mismatch");
        prop_assert_eq!(snap.cwd.as_deref(), Some(cwd.as_str()), "cwd mismatch");
        prop_assert_eq!(&snap.foreground_process, &Some(process), "process mismatch");
        prop_assert_eq!(snap.shell.as_deref(), Some(shell.as_str()), "shell mismatch");
        prop_assert_eq!(&snap.scrollback_ref, &Some(scrollback), "scrollback mismatch");
        prop_assert_eq!(&snap.agent, &Some(agent), "agent mismatch");
    }
}

// =============================================================================
// 11-14. ProcessInfo serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn process_info_roundtrip_full(info in arb_process_info_full()) {
        let json = serde_json::to_string(&info).expect("serialize ProcessInfo");
        let restored: ProcessInfo = serde_json::from_str(&json).expect("deserialize ProcessInfo");
        prop_assert_eq!(&info, &restored, "ProcessInfo full roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn process_info_roundtrip_no_pid(
        name in arb_nonempty_string(),
        argv in proptest::collection::vec(arb_short_string(), 0..5),
    ) {
        let info = ProcessInfo {
            name,
            pid: None,
            argv: Some(argv),
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let restored: ProcessInfo = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&info, &restored, "ProcessInfo no-pid roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn process_info_roundtrip_no_argv(
        name in arb_nonempty_string(),
        pid in any::<u32>(),
    ) {
        let info = ProcessInfo {
            name,
            pid: Some(pid),
            argv: None,
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let restored: ProcessInfo = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&info, &restored, "ProcessInfo no-argv roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn process_info_roundtrip_minimal(name in arb_nonempty_string()) {
        let info = ProcessInfo {
            name,
            pid: None,
            argv: None,
        };
        let json = serde_json::to_string(&info).expect("serialize");
        let restored: ProcessInfo = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&info, &restored, "ProcessInfo minimal roundtrip mismatch");
    }
}

// =============================================================================
// 15-16. TerminalState serde roundtrip and defaults
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn terminal_state_roundtrip(ts in arb_terminal_state()) {
        let json = serde_json::to_string(&ts).expect("serialize TerminalState");
        let restored: TerminalState = serde_json::from_str(&json).expect("deserialize TerminalState");
        prop_assert_eq!(&ts, &restored, "TerminalState roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn terminal_state_defaults_from_minimal_json(
        rows in 1u16..=500,
        cols in 1u16..=500,
    ) {
        // JSON with only rows and cols -- defaults should fill cursor_row/col, is_alt_screen, title
        let json = format!(r#"{{"rows":{},"cols":{}}}"#, rows, cols);
        let ts: TerminalState = serde_json::from_str(&json).expect("deserialize with defaults");
        prop_assert_eq!(ts.rows, rows, "rows mismatch");
        prop_assert_eq!(ts.cols, cols, "cols mismatch");
        prop_assert_eq!(ts.cursor_row, 0, "cursor_row default should be 0");
        prop_assert_eq!(ts.cursor_col, 0, "cursor_col default should be 0");
        prop_assert!(!ts.is_alt_screen, "is_alt_screen default should be false");
        prop_assert_eq!(ts.title.as_str(), "", "title default should be empty");
    }
}

// =============================================================================
// 17. ScrollbackRef serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn scrollback_ref_roundtrip(sr in arb_scrollback_ref()) {
        let json = serde_json::to_string(&sr).expect("serialize ScrollbackRef");
        let restored: ScrollbackRef = serde_json::from_str(&json).expect("deserialize ScrollbackRef");
        prop_assert_eq!(&sr, &restored, "ScrollbackRef roundtrip mismatch");
    }
}

// =============================================================================
// 18-21. AgentMetadata serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn agent_metadata_roundtrip_full(am in arb_agent_metadata_full()) {
        let json = serde_json::to_string(&am).expect("serialize AgentMetadata");
        let restored: AgentMetadata = serde_json::from_str(&json).expect("deserialize AgentMetadata");
        prop_assert_eq!(&am, &restored, "AgentMetadata full roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn agent_metadata_roundtrip_no_session_id(
        agent_type in arb_nonempty_string(),
        state in arb_nonempty_string(),
    ) {
        let am = AgentMetadata {
            agent_type,
            session_id: None,
            state: Some(state),
        };
        let json = serde_json::to_string(&am).expect("serialize");
        let restored: AgentMetadata = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&am, &restored, "AgentMetadata no-session roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn agent_metadata_roundtrip_no_state(
        agent_type in arb_nonempty_string(),
        session_id in arb_nonempty_string(),
    ) {
        let am = AgentMetadata {
            agent_type,
            session_id: Some(session_id),
            state: None,
        };
        let json = serde_json::to_string(&am).expect("serialize");
        let restored: AgentMetadata = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&am, &restored, "AgentMetadata no-state roundtrip mismatch");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn agent_metadata_roundtrip_minimal(agent_type in arb_nonempty_string()) {
        let am = AgentMetadata {
            agent_type,
            session_id: None,
            state: None,
        };
        let json = serde_json::to_string(&am).expect("serialize");
        let restored: AgentMetadata = serde_json::from_str(&json).expect("deserialize");
        prop_assert_eq!(&am, &restored, "AgentMetadata minimal roundtrip mismatch");
    }
}

// =============================================================================
// 22. CapturedEnv serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn captured_env_roundtrip(env in arb_captured_env()) {
        let json = serde_json::to_string(&env).expect("serialize CapturedEnv");
        let restored: CapturedEnv = serde_json::from_str(&json).expect("deserialize CapturedEnv");
        prop_assert_eq!(&env, &restored, "CapturedEnv roundtrip mismatch");
    }
}

// =============================================================================
// 23-24. Env capture -- safe vars captured
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_safe_vars_captured(
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        // All safe env vars should be captured when present
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(
                SAFE_ENV_VARS
                    .iter()
                    .map(|&name| (name.to_string(), value.clone())),
            );

        let env = snap.env.as_ref().expect("env should be Some");
        for &safe_name in SAFE_ENV_VARS {
            prop_assert!(
                env.vars.contains_key(safe_name),
                "safe var {} should be captured", safe_name
            );
            prop_assert_eq!(
                env.vars.get(safe_name).map(|s| s.as_str()),
                Some(value.as_str()),
                "safe var {} value mismatch", safe_name
            );
        }
        prop_assert_eq!(env.redacted_count, 0, "no sensitive vars, redacted_count should be 0");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_each_safe_var_individually(
        idx in 0..SAFE_ENV_VARS.len(),
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = SAFE_ENV_VARS[idx];
        let vars = vec![(var_name.to_string(), value.clone())];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(
            env.vars.contains_key(var_name),
            "safe var {} should be captured individually", var_name
        );
        prop_assert_eq!(
            env.vars.get(var_name).map(|s| s.as_str()),
            Some(value.as_str()),
            "value mismatch for {}", var_name
        );
    }
}

// =============================================================================
// 25. Env capture -- non-safe vars excluded
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn env_capture_non_safe_vars_excluded(
        var_name in "[A-Z]{3,10}_[A-Z]{3,10}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        // Skip if the generated name happens to be in the safe list
        // or happens to match a sensitive pattern
        let is_safe = SAFE_ENV_VARS.iter().any(|&s| s == var_name);
        let upper = var_name.to_uppercase();
        let is_sensitive = SENSITIVE_PATTERNS.iter().any(|pat| upper.contains(pat));

        prop_assume!(!is_safe && !is_sensitive);

        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(
            !env.vars.contains_key(&var_name),
            "non-safe var {} should be excluded", var_name
        );
        prop_assert_eq!(env.redacted_count, 0, "non-sensitive non-safe var should not increment redacted_count");
    }
}

// =============================================================================
// 26-34. Env capture -- sensitive var patterns redacted
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_secret_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = format!("{}_SECRET", prefix);
        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "SECRET var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_token_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = format!("{}_TOKEN", prefix);
        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "TOKEN var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_key_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        // KEY pattern: avoid generating names that are in SAFE_ENV_VARS
        let var_name = format!("{}_KEY", prefix);
        let is_safe = SAFE_ENV_VARS.iter().any(|&s| s == var_name);
        prop_assume!(!is_safe);

        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "KEY var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_password_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = format!("{}_PASSWORD", prefix);
        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "PASSWORD var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_credential_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = format!("{}_CREDENTIAL", prefix);
        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "CREDENTIAL var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_auth_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = format!("{}_AUTH", prefix);
        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "AUTH var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_api_key_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = format!("{}_API_KEY", prefix);
        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "API_KEY var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_private_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = format!("{}_PRIVATE", prefix);
        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "PRIVATE var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_sensitive_passwd_redacted(
        prefix in "[A-Z]{2,8}",
        value in arb_nonempty_string(),
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let var_name = format!("{}_PASSWD", prefix);
        let vars = vec![(var_name.clone(), value)];
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(!env.vars.contains_key(&var_name), "PASSWD var should be redacted");
        prop_assert_eq!(env.redacted_count, 1, "redacted_count should be 1");
    }
}

// =============================================================================
// 35. Env capture -- case-insensitive sensitivity check
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_case_insensitive_sensitivity(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        // "secret" in various cases should all be caught
        let vars = vec![
            ("my_secret".to_string(), "v1".to_string()),
            ("MY_SECRET".to_string(), "v2".to_string()),
            ("My_Secret".to_string(), "v3".to_string()),
            ("mY_sEcReT_VALUE".to_string(), "v4".to_string()),
        ];

        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert!(env.vars.is_empty(), "all sensitive vars should be excluded from vars");
        prop_assert_eq!(env.redacted_count, 4, "all 4 variants should be redacted");
    }
}

// =============================================================================
// 36. Env capture -- redacted_count matches sensitive var count
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn env_capture_redacted_count_matches_sensitive_count(
        num_safe in 0usize..5,
        num_sensitive in 0usize..10,
        num_nonsafe in 0usize..5,
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let mut vars: Vec<(String, String)> = Vec::new();

        // Add some safe vars
        for (i, &name) in SAFE_ENV_VARS.iter().enumerate().take(num_safe) {
            vars.push((name.to_string(), format!("val_{}", i)));
        }

        // Add sensitive vars
        for i in 0..num_sensitive {
            vars.push((format!("XSECRET_{}", i), format!("secret_{}", i)));
        }

        // Add non-safe, non-sensitive vars
        for i in 0..num_nonsafe {
            vars.push((format!("CUSTOMXYZ_{}", i), format!("custom_{}", i)));
        }

        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_env_from_iter(vars.into_iter());

        let env = snap.env.as_ref().expect("env should be Some");
        prop_assert_eq!(
            env.redacted_count, num_sensitive,
            "redacted_count should equal number of sensitive vars"
        );
        prop_assert_eq!(
            env.vars.len(), num_safe.min(SAFE_ENV_VARS.len()),
            "captured count should equal number of safe vars provided"
        );
    }
}

// =============================================================================
// 37. Size budget -- small snapshot not truncated
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn size_budget_small_not_truncated(snap in arb_snapshot_minimal()) {
        let (json, truncated) = snap.to_json_budgeted().expect("budgeted serialization should succeed");
        prop_assert!(!truncated, "small snapshot should not be truncated");
        prop_assert!(
            json.len() <= PANE_STATE_SIZE_BUDGET,
            "small snapshot should be within budget, got {} bytes", json.len()
        );
    }
}

// =============================================================================
// 38-39. Size budget -- large env triggers truncation, output <= budget
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn size_budget_large_env_triggers_truncation(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
        num_vars in 800usize..1200,
    ) {
        let mut vars = HashMap::new();
        for i in 0..num_vars {
            vars.insert(format!("VAR_{}", i), "x".repeat(100));
        }

        let mut snap = PaneStateSnapshot::new(pane_id, captured_at, terminal);
        snap.env = Some(CapturedEnv {
            vars,
            redacted_count: 0,
        });

        let full_json = snap
            .to_json()
            .expect("full serialization should succeed");
        prop_assume!(
            full_json.len() > PANE_STATE_SIZE_BUDGET,
            "test input must exceed size budget to assert truncation"
        );

        let (json, truncated) = snap.to_json_budgeted().expect("budgeted serialization should succeed");
        prop_assert!(truncated, "large env should trigger truncation");
        prop_assert!(
            json.len() <= PANE_STATE_SIZE_BUDGET,
            "truncated output should be within budget, got {} bytes", json.len()
        );

        // After truncation, env should be removed
        let restored = PaneStateSnapshot::from_json(&json).expect("truncated JSON should be valid");
        prop_assert!(restored.env.is_none(), "env should be removed after truncation");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn size_budget_truncated_always_within_budget(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        rows in 1u16..=500,
        cols in 1u16..=500,
        num_env_vars in 200usize..800,
        value_len in 50usize..200,
    ) {
        let terminal = TerminalState {
            rows,
            cols,
            cursor_row: 0,
            cursor_col: 0,
            is_alt_screen: false,
            title: String::new(),
        };

        let mut vars = HashMap::new();
        for i in 0..num_env_vars {
            vars.insert(format!("VAR_{}", i), "a".repeat(value_len));
        }

        let mut snap = PaneStateSnapshot::new(pane_id, captured_at, terminal);
        snap.env = Some(CapturedEnv {
            vars,
            redacted_count: 0,
        });

        let (json, _truncated) = snap.to_json_budgeted().expect("budgeted serialization should succeed");
        prop_assert!(
            json.len() <= PANE_STATE_SIZE_BUDGET,
            "output must always be within budget, got {} bytes", json.len()
        );
    }
}

// =============================================================================
// 40. Size budget -- large argv also truncated
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn size_budget_large_argv_also_truncated(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        // Build a snapshot with huge env AND huge argv
        let mut vars = HashMap::new();
        for i in 0..800 {
            vars.insert(format!("VAR_{}", i), "x".repeat(100));
        }

        let huge_argv: Vec<String> = (0..5000).map(|i| format!("arg_{}", "y".repeat(100 + i % 50))).collect();

        let mut snap = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_process(ProcessInfo {
                name: "test_proc".to_string(),
                pid: Some(999),
                argv: Some(huge_argv),
            });
        snap.env = Some(CapturedEnv {
            vars,
            redacted_count: 0,
        });

        let (json, truncated) = snap.to_json_budgeted().expect("budgeted serialization should succeed");
        prop_assert!(truncated, "oversized snapshot should be truncated");

        // After truncation, argv should also be removed if env removal was insufficient
        let restored = PaneStateSnapshot::from_json(&json).expect("truncated JSON should be valid");
        prop_assert!(restored.env.is_none(), "env should be removed");
        // Process should still exist but argv may be None
        let has_process = restored.foreground_process.is_some();
        prop_assert!(has_process, "foreground_process should still be present");
        let argv_is_none = restored
            .foreground_process
            .as_ref()
            .map(|p| p.argv.is_none())
            .unwrap_or(false);
        prop_assert!(argv_is_none, "argv should be truncated to None");
    }
}

// =============================================================================
// 41. Schema version always matches constant
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn schema_version_always_current(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let snap = PaneStateSnapshot::new(pane_id, captured_at, terminal);
        prop_assert_eq!(
            snap.schema_version, 1u32,
            "schema_version should be 1"
        );
        prop_assert_eq!(
            snap.schema_version, PANE_STATE_SCHEMA_VERSION,
            "schema_version should match PANE_STATE_SCHEMA_VERSION"
        );
    }
}

// =============================================================================
// 42-43. Forward compatibility
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn forward_compat_unknown_fields_ignored(
        schema_version in 2u32..100,
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        rows in 1u16..=500,
        cols in 1u16..=500,
        extra_field_val in arb_short_string(),
    ) {
        let json = format!(
            r#"{{"schema_version":{},"pane_id":{},"captured_at":{},"terminal":{{"rows":{},"cols":{}}},"future_field":"{}","another_new_thing":42}}"#,
            schema_version, pane_id, captured_at, rows, cols, extra_field_val
        );

        let snap = PaneStateSnapshot::from_json(&json).expect("should parse JSON with unknown fields");
        prop_assert_eq!(snap.schema_version, schema_version, "schema_version should be preserved");
        prop_assert_eq!(snap.pane_id, pane_id, "pane_id should be preserved");
        prop_assert_eq!(snap.captured_at, captured_at, "captured_at should be preserved");
        prop_assert_eq!(snap.terminal.rows, rows, "rows should be preserved");
        prop_assert_eq!(snap.terminal.cols, cols, "cols should be preserved");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn forward_compat_future_schema_version_roundtrip(
        future_version in 2u32..1000,
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        let mut snap = PaneStateSnapshot::new(pane_id, captured_at, terminal);
        snap.schema_version = future_version;

        let json = snap.to_json().expect("serialization should succeed");
        let restored = PaneStateSnapshot::from_json(&json).expect("deserialization should succeed");
        prop_assert_eq!(
            restored.schema_version, future_version,
            "future schema version should survive roundtrip"
        );
    }
}

// =============================================================================
// 44-46. PartialEq
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn partial_eq_identical_snapshots(snap in arb_snapshot_full()) {
        let other = snap.clone();
        prop_assert_eq!(&snap, &other, "identical snapshots should be equal");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn partial_eq_different_pane_id(
        pane_id_a in arb_pane_id(),
        pane_id_b in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
    ) {
        prop_assume!(pane_id_a != pane_id_b);
        let a = PaneStateSnapshot::new(pane_id_a, captured_at, terminal.clone());
        let b = PaneStateSnapshot::new(pane_id_b, captured_at, terminal);
        prop_assert_ne!(&a, &b, "snapshots with different pane_id should not be equal");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn partial_eq_different_terminal(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal_a in arb_terminal_state(),
        terminal_b in arb_terminal_state(),
    ) {
        prop_assume!(terminal_a != terminal_b);
        let a = PaneStateSnapshot::new(pane_id, captured_at, terminal_a);
        let b = PaneStateSnapshot::new(pane_id, captured_at, terminal_b);
        prop_assert_ne!(&a, &b, "snapshots with different terminal should not be equal");
    }
}

// =============================================================================
// 47-48. Clone
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn clone_produces_equal_snapshot(snap in arb_snapshot_full()) {
        let cloned = snap.clone();
        prop_assert_eq!(&snap, &cloned, "clone should produce equal snapshot");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn clone_mutation_independent(
        pane_id in arb_pane_id(),
        captured_at in arb_timestamp(),
        terminal in arb_terminal_state(),
        cwd in arb_nonempty_string(),
    ) {
        let original = PaneStateSnapshot::new(pane_id, captured_at, terminal)
            .with_cwd(cwd.clone());
        let mut cloned = original.clone();
        cloned.cwd = Some("mutated_path".to_string());

        // Original should be unaffected
        prop_assert_eq!(
            original.cwd.as_deref(),
            Some(cwd.as_str()),
            "original should not be affected by clone mutation"
        );
        prop_assert_eq!(
            cloned.cwd.as_deref(),
            Some("mutated_path"),
            "cloned should reflect mutation"
        );
        prop_assert_ne!(&original, &cloned, "original and mutated clone should differ");
    }
}

// =============================================================================
// 49-50. from_json error cases
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn from_json_rejects_invalid_json(garbage in "[^{}\\[\\]\"]{1,100}") {
        let result = PaneStateSnapshot::from_json(&garbage);
        let is_err = result.is_err();
        prop_assert!(is_err, "from_json should reject invalid JSON");
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn from_json_rejects_empty_string(_dummy in 0..1u8) {
        let result = PaneStateSnapshot::from_json("");
        let is_err = result.is_err();
        prop_assert!(is_err, "from_json should reject empty string");
    }
}
