# Plan to Deeply Integrate destructive_command_guard into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.13.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_destructive_command_guard.md (ft-2vuw7.13.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Pre-execution command validation**: Wire DCG's evaluator into FrankenTerm's command execution pipeline to block destructive commands before they reach the terminal
2. **MCP safety tool**: Expose DCG's `check_command` MCP tool through FrankenTerm's MCP server for agents to pre-validate commands
3. **Pattern pack extension**: Add FrankenTerm-specific pattern packs for terminal-related destructive operations (kill terminal sessions, destroy mux servers, corrupt recorder DBs)
4. **Allowlist integration**: Use DCG's layered allowlist system (project > user > system) for per-project FrankenTerm safety policies
5. **SARIF reporting**: Wire DCG's SARIF output into FrankenTerm's audit trail for GitHub Code Scanning integration

### Constraints

- **DCG already works as a Claude Code hook**: Integration deepens the existing PreToolUse hook relationship
- **Fail-open design preserved**: DCG's fail-open design must be maintained — never block the agent on DCG failure
- **<1ms quick-reject**: DCG's SIMD-accelerated keyword rejection must not be degraded
- **Feature-gated**: DCG integration behind `command-guard` feature flag
- **No ast-grep dependency in FrankenTerm**: Heredoc scanning stays in DCG; FrankenTerm sends plain commands

### Non-Goals

- **Replacing DCG**: FrankenTerm delegates to DCG, doesn't reimplement pattern matching
- **Importing DCG as a library**: 107K LOC monolith; use MCP server or subprocess
- **Building custom pattern packs**: Extend DCG's pack system, don't duplicate it
- **File scanning in FrankenTerm**: DCG handles Dockerfile/Makefile scanning independently

---

## P2: Evaluate Integration Patterns

### Option A: MCP Server Client (Chosen)

FrankenTerm calls DCG's MCP server (`check_command`, `scan_file`, `explain_pattern`) for command validation.

**Pros**: Zero compile-time deps, <10ms latency, DCG evolves independently, fail-open preserved
**Cons**: Requires DCG MCP server running, MCP call overhead
**Chosen**: DCG already offers MCP server with exactly the right tools

### Option B: CLI Subprocess

Shell out to `dcg test <command> --format json`.

**Pros**: Process isolation, works without MCP
**Cons**: ~50ms subprocess overhead per command, not suitable for real-time validation
**Rejected**: Too slow for pre-execution validation in terminal pipeline

### Option C: Library Import

Import DCG's evaluator directly.

**Pros**: Zero latency, type-safe
**Cons**: 107K LOC monolith, fancy-regex, ast-grep-core dependencies, nightly-only
**Rejected**: Way too heavy; MCP server provides the same functionality

### Decision: Option A — MCP server client

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── command_guard.rs          # NEW: DCG MCP client wrapper
│   └── ...existing modules...
├── Cargo.toml                    # No new dependencies
```

### Module Responsibilities

#### `command_guard.rs` — Destructive command guard integration

- `check_command(cmd: &str) -> GuardDecision` — Call DCG MCP `check_command` tool
- `GuardDecision` enum — Allow, Deny(reason, pattern_id), Warn(reason)
- `explain_pattern(pattern_id: &str) -> PatternExplanation` — Get pattern details
- Configurable: enable/disable, allowlist overrides, confidence threshold
- Fail-open: If DCG unavailable, return Allow with warning
- Integration point: Called before command execution in terminal pipeline

---

## P4: API Contracts

### Public API Contract

```rust
#[cfg(feature = "command-guard")]
pub mod command_guard {
    #[derive(Debug, Clone)]
    pub enum GuardDecision {
        Allow,
        Deny { reason: String, pattern_id: String, pack: String },
        Warn { reason: String, pattern_id: String },
    }

    pub struct CommandGuardConfig {
        pub enabled: bool,                // Default: true
        pub fail_open: bool,              // Default: true (MUST be true)
        pub confidence_threshold: f64,    // Default: 0.8
        pub timeout_ms: u64,             // Default: 100
    }

    pub struct CommandGuard {
        config: CommandGuardConfig,
        mcp_client: Option<McpClient>,
    }

    impl CommandGuard {
        pub fn new(config: CommandGuardConfig) -> Self;
        pub async fn check(&self, command: &str) -> GuardDecision;
        pub async fn explain(&self, pattern_id: &str) -> Option<PatternExplanation>;
        pub fn is_available(&self) -> bool;
    }
}
```

---

## P5-P6: Migration, Testing

**No migration needed** — new safety layer wrapping existing DCG MCP tools.

### Unit Tests (30+)

- `test_allow_safe_command` — `ls`, `pwd` allowed
- `test_deny_destructive_command` — `rm -rf /` denied
- `test_deny_git_force_push` — `git push --force` denied
- `test_warn_moderate_command` — commands below confidence threshold warn
- `test_fail_open_dcg_unavailable` — returns Allow when DCG down
- `test_fail_open_timeout` — returns Allow on timeout
- `test_config_disabled` — always Allow when disabled
- `test_confidence_threshold` — low-confidence matches filtered
- `test_explain_pattern` — pattern explanation returned
- Additional pattern and configuration tests for 30+ total

### Property-Based Tests

- `proptest_fail_open_always` — DCG unavailable never blocks
- `proptest_safe_commands_allowed` — known safe commands always allowed
- `proptest_check_never_panics` — any string input succeeds

---

## P7: Rollout

**Phase 1**: `command_guard.rs` behind `command-guard` feature (Week 1-2)
**Phase 2**: Wire into terminal command execution pipeline (Week 3)
**Phase 3**: Add FrankenTerm-specific pattern pack to DCG (Week 4, DCG codebase)
**Phase 4**: SARIF output to audit trail (Week 5)

### Rollback: Disable feature flag; commands execute without guard check.

---

## P8: Summary

**MCP client** wrapping DCG's `check_command` tool. Fail-open, <10ms, zero compile-time deps.

### One New Module
1. **`command_guard.rs`**: DCG MCP client with fail-open guard for terminal command validation

### Upstream Tweak Proposals (for destructive_command_guard)
1. **FrankenTerm pattern pack**: Terminal-specific destructive patterns (kill mux, corrupt DB)
2. **Batch check API**: `check_commands` for validating multiple commands atomically
3. **Confidence in MCP response**: Include confidence score in `check_command` response
4. **Custom pack directory**: Allow FrankenTerm to register custom pattern packs

### Beads: ft-2vuw7.13.1 (CLOSED), ft-2vuw7.13.2 (CLOSED), ft-2vuw7.13.3 (THIS DOCUMENT)

---

*Plan complete.*
