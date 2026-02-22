# Plan to Deeply Integrate cross_agent_session_resumer into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.27.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_cross_agent_session_resumer.md (ft-2vuw7.27.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **FrankenTerm as a CASR data provider**: Implement a `FrankenTermProvider` (either upstream in casr or as an adapter in frankenterm-core) that reads flight recorder sessions and converts them into `CanonicalSession` IR. This enables any casr-supported agent (Claude Code, Codex, Gemini, Cursor, etc.) to resume a conversation that was originally observed through FrankenTerm's terminal capture pipeline. The provider maps recorder events (output frames, input frames, detection events) to `CanonicalMessage` entries with appropriate role inference (`User` for input, `Assistant` for agent output, `Tool` for tool-use detections).

2. **Session resume from FrankenTerm UI**: Allow FrankenTerm users (human or robot-mode agents) to select a recorded terminal session and resume it in a target agent. The workflow: `ft resume <session-id> --target claude-code` discovers the session in FrankenTerm's recorder storage, converts it to CanonicalSession, writes it to the target provider's native format via casr, and optionally executes the resume command in a designated pane. This closes the loop between FrankenTerm's observability layer and agent session continuity.

3. **CanonicalSession type adoption for session interchange**: Vendor-adapt casr's `CanonicalSession`, `CanonicalMessage`, `MessageRole`, `ToolCall`, and `ToolResult` types into frankenterm-core as the canonical session exchange format. These types become the bridge between FrankenTerm's internal `PaneStateSnapshot`/`RecorderEvent` representations and external agent session formats. All session import/export flows through this IR.

4. **Unified multi-provider session discovery**: Expose casr's `ProviderRegistry::list_sessions()` through FrankenTerm's CLI and robot-mode API, enriched with FrankenTerm's own recorder sessions. Users see a single unified session inventory across all installed coding agents plus FrankenTerm's flight recorder. Sessions include metadata: provider, workspace, message count, start/end time, model name.

5. **Agent-pane session correlation**: Connect FrankenTerm's existing `agent_correlator.rs` and `session_correlation.rs` modules with casr's session discovery. When FrankenTerm detects an agent running in a pane (via pattern detection), it can cross-reference casr's session list to identify the exact external session ID, enabling precise resume targeting. This enriches the `AgentMetadata` captured in pane state snapshots with the external session ID from the original provider.

### Constraints

- **Subprocess-first integration**: casr is a synchronous CLI tool with no async runtime. FrankenTerm's async architecture requires wrapping casr calls in `tokio::task::spawn_blocking` or invoking the `casr` binary as a subprocess. The subprocess path is preferred for Phase 1 because it avoids pulling casr's dependency tree (including `rusqlite` with `bundled` feature, which would conflict with FrankenTerm's existing rusqlite dependency version).

- **Feature-gated behind `session-resume`**: All new modules compile only when `--features session-resume` is active. The feature flag controls both the vendored types and the subprocess wrapper. This prevents adding any compile-time cost to default builds and allows the integration to ship independently of FrankenTerm's core release cadence.

- **`#![forbid(unsafe_code)]` in frankenterm-core**: All new code must be safe Rust. The subprocess approach naturally satisfies this since `std::process::Command` is safe. If library integration is later pursued, casr itself enforces `#![forbid(unsafe_code)]`, so no conflict arises.

- **Rust 2024 edition compatibility**: casr uses Rust 2024 edition with nightly. FrankenTerm also uses nightly (`#![feature(stmt_expr_attributes)]`). Edition compatibility is not a blocker, but vendored types must be adapted to FrankenTerm's edition conventions if they differ.

- **rusqlite version alignment**: FrankenTerm already depends on `rusqlite` (workspace). casr uses `rusqlite = "0.33"` with `bundled` feature. If library integration is pursued, the `bundled` feature must be reconciled to avoid duplicate SQLite compilations. The subprocess approach sidesteps this entirely.

- **No casr modification required for Phase 1**: The initial integration works with casr as-is. Upstream tweaks (FrankenTerm provider, feature flags for SQLite providers) are proposed for later phases.

- **30+ tests per new module**: Per FrankenTerm project convention, every new module requires at least 30 unit tests. Property-based tests with proptest are expected for any data transformation logic.

### Non-Goals

- **Reimplementing provider parsers**: FrankenTerm does not reimplement casr's 14 provider read/write implementations. All format conversion delegates to casr.

- **Real-time session synchronization**: casr operates on session snapshots (point-in-time reads). FrankenTerm does not attempt live-streaming session updates between providers.

- **Modifying casr's provider implementations**: Bug fixes or new providers in casr are upstream concerns. FrankenTerm consumes casr's output without patching its internals.

- **Replacing FrankenTerm's flight recorder**: The recorder captures terminal I/O at the frame level. casr operates at the conversation-message level. These are complementary, not competing, representations.

- **Agent session state transfer**: Resuming a session in a different agent gives that agent conversation history but does not transfer in-memory state (tool caches, context window, pending operations). This is a fundamental limitation of all cross-agent resume workflows.

---

## P2: Evaluate Integration Patterns

### Option A: Library Import (casr as Cargo dependency)

Add casr as an optional workspace dependency:

```toml
# Cargo.toml
[dependencies]
casr = { path = "/dp/cross_agent_session_resumer", optional = true }

[features]
session-resume = ["dep:casr"]
```

**Pros**:
- Type-safe API: `casr::model::CanonicalSession`, `casr::pipeline::ConversionPipeline` used directly
- No subprocess overhead (eliminate 100-1000ms spawn latency per operation)
- Compile-time guarantees: API changes caught immediately
- Access to internal helpers: `flatten_content()`, `parse_timestamp()`, `normalize_role()`
- FrankenTerm can implement `casr::providers::Provider` trait directly for upstream registration

**Cons**:
- Pulls in casr's full dependency tree: `rusqlite` (bundled, compiles SQLite from C), `walkdir`, `sha2`, `glob`, `which`, `uuid`, `clap`, `colored`
- rusqlite `bundled` feature compiles SQLite from C source (~1.5MB, ~30s compile time), potentially conflicting with FrankenTerm's existing rusqlite workspace dep
- casr's synchronous API requires `spawn_blocking` wrappers for every call in FrankenTerm's async context
- Tight coupling: casr API changes break FrankenTerm builds
- casr requires nightly (Rust 2024 let-chains, `if let` guards) -- compatible but pins toolchain

**Verdict**: Viable for Phase 2+, after subprocess approach proves the integration value.

### Option B: Subprocess CLI (shell out to `casr`)

Invoke `casr` as an external process with `--json` flag for machine-readable output:

```rust
let output = Command::new("casr")
    .args(["claude-code", "resume", session_id, "--json", "--source", source])
    .output()?;
let result: CasrResumeOutput = serde_json::from_slice(&output.stdout)?;
```

**Pros**:
- Zero compile-time dependencies: no impact on FrankenTerm's build graph
- Process isolation: casr crashes do not affect FrankenTerm
- Version independence: casr can update without rebuilding FrankenTerm
- Natural async boundary: subprocess I/O is inherently non-blocking
- Graceful degradation: `which::which("casr")` / `Command::new("casr").arg("--version")` detects availability
- Follows FrankenTerm's existing patterns: `cass.rs` already wraps an external CLI (`cass`) with typed parsing

**Cons**:
- Subprocess spawn overhead: 50-200ms per invocation (acceptable for user-initiated operations)
- JSON contract coupling: changes to casr's `--json` output schema can break parsing (mitigated by `#[serde(default)]` and version checking)
- No access to internal types at compile time: vendored types must be maintained separately
- Error messages from casr require stderr parsing for structured error reporting
- Requires casr binary installed in PATH (documented as optional dependency)

**Verdict**: Chosen for Phase 1. Matches FrankenTerm's existing `cass.rs` subprocess pattern.

### Option C: Type Vendoring (copy CanonicalSession types)

Copy casr's `model.rs` types into frankenterm-core, adapting them to FrankenTerm's conventions:

```rust
// frankenterm-core/src/casr_types.rs (vendored from casr model.rs)
pub struct CanonicalSession { ... }
pub struct CanonicalMessage { ... }
pub enum MessageRole { ... }
```

**Pros**:
- Types available without any external dependency
- Can be adapted to FrankenTerm's serde conventions and error types
- Follows casr's own CASS-independence pattern (casr vendored types from CASS)
- Stable even if casr's actual types evolve

**Cons**:
- Manual sync burden: changes to casr's IR require manual type updates
- Risk of drift: vendored types may fall behind casr's canonical definitions
- Duplication of ~200 LOC with semantic implications for compatibility

**Verdict**: Used in combination with Option B. The subprocess wrapper parses JSON into vendored types, giving both process isolation and compile-time type safety.

### Decision: Option B + C -- Subprocess CLI with Vendored Types

Phase 1 uses subprocess invocation (`casr --json`) with vendored CanonicalSession types for parsing. This matches FrankenTerm's established pattern in `cass.rs` (subprocess wrapper for `coding_agent_session_search`) and avoids dependency-tree complications. Phase 2 may upgrade to library import (Option A) if subprocess latency proves problematic for interactive workflows.

---

## P3: Target Placement Within FrankenTerm Subsystems

### Module Layout

```
frankenterm-core/
  src/
    casr_types.rs           # NEW: Vendored CanonicalSession IR types
    session_resume.rs       # NEW: CASR subprocess wrapper + resume orchestrator
    recorder_casr_export.rs # NEW: Flight recorder -> CanonicalSession converter
```

### Module Responsibilities

#### `casr_types.rs` -- Vendored Canonical IR Types

Vendor-adapted copies of casr's core model types. These are the serialization targets for JSON output from the `casr` subprocess and the interchange format between FrankenTerm's recorder and external agent session formats.

**Types vendored from casr `model.rs`**:
- `CanonicalSession` -- Top-level session container (session_id, provider_slug, workspace, title, timestamps, messages, metadata, source_path, model_name)
- `CanonicalMessage` -- Individual conversation turn (idx, role, content, timestamp, author, tool_calls, tool_results, extra)
- `MessageRole` -- `User | Assistant | Tool | System | Other(String)`
- `ToolCall` -- Tool invocation record (id, name, arguments)
- `ToolResult` -- Tool execution result (call_id, content, is_error)

**Types vendored from casr `pipeline.rs`**:
- `ConversionResultJson` -- JSON output schema for `casr resume --json`
- `WrittenSessionJson` -- Written file paths and resume command
- `ValidationResultJson` -- Validation errors/warnings/info

**Types vendored from casr `discovery.rs`**:
- `SessionListEntryJson` -- JSON output schema for `casr list --json`
- `ProviderStatusJson` -- JSON output schema for `casr providers --json`
- `DetectionResultJson` -- Provider installation status

**Adaptation rules**:
- All types derive `Debug, Clone, Serialize, Deserialize`
- All optional fields use `#[serde(default, skip_serializing_if = "Option::is_none")]`
- Unknown fields tolerated via `#[serde(flatten)] pub extra: HashMap<String, serde_json::Value>` on leaf types
- No `#[serde(deny_unknown_fields)]` to maintain forward compatibility with newer casr versions

#### `session_resume.rs` -- CASR Subprocess Wrapper and Resume Orchestrator

The primary integration surface. Wraps `casr` CLI invocations and orchestrates the resume workflow from FrankenTerm's perspective.

**Core type**: `SessionResumer` -- stateless orchestrator that invokes `casr` subprocess commands.

**Responsibilities**:
1. **Availability detection**: Check if `casr` binary exists in PATH and verify minimum version compatibility
2. **Session listing**: Invoke `casr list --json` and parse into `Vec<SessionListEntry>`, optionally filtered by provider or workspace
3. **Session info**: Invoke `casr info <session-id> --json` for detailed single-session metadata
4. **Session resume**: Invoke `casr <target> resume <session-id> --json [--source <hint>] [--enrich]` and parse the conversion result
5. **Provider inventory**: Invoke `casr providers --json` to enumerate installed providers and their detection status
6. **Pane resume orchestration**: Given a resume command string from casr, send it to a target FrankenTerm pane via `WeztermHandle::send_text()`
7. **Error mapping**: Convert casr's `CasrError` JSON structure into FrankenTerm's `Error::Runtime(String)` with actionable remediation suggestions

**Design patterns**:
- All subprocess calls wrapped in `tokio::task::spawn_blocking` for async compatibility
- Hard timeout on subprocess execution (configurable, default 30 seconds)
- Output size cap (16 MB) to prevent OOM from pathological casr output
- Stderr captured for diagnostic logging via `tracing::warn!`

#### `recorder_casr_export.rs` -- Flight Recorder to CanonicalSession Converter

Converts FrankenTerm's internal flight recorder data into the CanonicalSession IR for export to external agents.

**Core type**: `RecorderCasrExporter` -- reads recorder events and produces `CanonicalSession` instances.

**Responsibilities**:
1. **Event-to-message mapping**: Convert `RecorderEvent` sequences into `CanonicalMessage` entries:
   - `FrameType::Input` events -> `MessageRole::User` messages (user-typed commands)
   - `FrameType::Output` events from agent panes -> `MessageRole::Assistant` messages (agent responses)
   - `FrameType::Event` with `Detection` payloads containing tool-use patterns -> `MessageRole::Tool` messages
   - `FrameType::Marker` events -> `MessageRole::System` messages (annotations)
2. **Session boundary detection**: Group recorder events into logical sessions using:
   - Agent detection boundaries (when `AgentType` changes in a pane)
   - Time gaps exceeding a configurable threshold (default: 30 minutes)
   - Explicit session markers if present
3. **Metadata enrichment**: Populate `CanonicalSession` fields from FrankenTerm's state:
   - `provider_slug`: `"frankenterm"`
   - `workspace`: From `PaneStateSnapshot.cwd`
   - `title`: From first user command or pane title
   - `started_at` / `ended_at`: From first/last event timestamps
   - `model_name`: From `AgentMetadata.model` if detected
   - `metadata`: Include pane dimensions, shell type, agent type, FrankenTerm version
4. **Content coalescing**: Merge consecutive output frames into single messages to avoid one-message-per-frame fragmentation. Use configurable time-window coalescing (default: 500ms).
5. **Sensitivity filtering**: Respect recorder retention tiers. T3 (restricted) events are excluded from export unless the caller has A3+ access tier. Redacted events export with `[REDACTED]` content placeholder.

---

## P4: API Contracts

### `casr_types.rs` -- Vendored Types

```rust
//! Vendored types from cross_agent_session_resumer (casr) model.rs.
//!
//! These types mirror casr's CanonicalSession IR for JSON deserialization
//! of `casr --json` subprocess output. Maintained independently of casr's
//! source to avoid compile-time dependency on casr's crate.
//!
//! Vendor source: /dp/cross_agent_session_resumer/src/model.rs
//! Last synced: 2026-02-22

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Canonical intermediate representation for an AI agent session.
///
/// This is the "Rosetta Stone" type: every agent provider reads INTO this
/// format and writes FROM it. FrankenTerm uses it as the bridge between
/// its flight recorder and external agent session formats.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalSession {
    pub session_id: String,
    pub provider_slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
    pub messages: Vec<CanonicalMessage>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub source_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
}

/// A single message in a canonical session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalMessage {
    pub idx: usize,
    pub role: MessageRole,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_results: Vec<ToolResult>,
    #[serde(default)]
    pub extra: serde_json::Value,
}

/// Message role in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
    Other(String),
}

/// A tool invocation within a message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub arguments: serde_json::Value,
}

/// The result of a tool invocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<String>,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

// -- Subprocess JSON output types --

/// JSON output from `casr <target> resume <id> --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasrResumeOutput {
    #[serde(default)]
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_path: Option<PathBuf>,
    #[serde(default)]
    pub warnings: Vec<String>,
    // Error fields (when ok=false)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// JSON output from `casr list --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasrListEntry {
    pub session_id: String,
    pub provider: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// JSON output from `casr providers --json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CasrProviderStatus {
    pub name: String,
    pub slug: String,
    pub cli_alias: String,
    pub installed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}
```

### `session_resume.rs` -- Subprocess Wrapper API

```rust
//! Cross-agent session resume via casr subprocess.
//!
//! Wraps the `casr` CLI binary for session discovery, conversion, and
//! resume command generation. All operations are async-safe via
//! spawn_blocking.

use crate::casr_types::*;
use crate::error::Result;
use crate::wezterm::WeztermHandle;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Configuration for the session resume subsystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionResumeConfig {
    /// Path to casr binary (default: search PATH).
    pub casr_binary: Option<PathBuf>,
    /// Maximum time to wait for casr subprocess (ms).
    pub timeout_ms: u64,
    /// Maximum stdout size from casr (bytes).
    pub max_output_bytes: usize,
    /// Whether to pass --enrich flag to casr resume.
    pub enrich_by_default: bool,
}

impl Default for SessionResumeConfig {
    fn default() -> Self {
        Self {
            casr_binary: None,
            timeout_ms: 30_000,
            max_output_bytes: 16 * 1024 * 1024,
            enrich_by_default: false,
        }
    }
}

/// Result of a session resume operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResumeResult {
    /// Shell command to resume the session in the target agent.
    pub resume_command: String,
    /// Session ID in the target provider's format.
    pub target_session_id: String,
    /// Source provider slug (e.g., "claude-code").
    pub source_provider: String,
    /// Target provider slug.
    pub target_provider: String,
    /// Path(s) where the converted session was written.
    pub written_paths: Vec<PathBuf>,
    /// Warnings from the conversion pipeline.
    pub warnings: Vec<String>,
}

/// Summary of a discoverable session across all providers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub session_id: String,
    pub provider: String,
    pub workspace: Option<String>,
    pub message_count: usize,
    pub started_at: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
}

/// Installation status of a provider detected by casr.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderInfo {
    pub name: String,
    pub slug: String,
    pub cli_alias: String,
    pub installed: bool,
    pub version: Option<String>,
}

/// Orchestrates cross-agent session resume operations.
pub struct SessionResumer {
    config: SessionResumeConfig,
}

impl SessionResumer {
    pub fn new(config: SessionResumeConfig) -> Self;

    /// Check if casr is installed and reachable.
    pub async fn is_available(&self) -> bool;

    /// Get casr version string (e.g., "casr 0.3.0 (abc1234)").
    pub async fn version(&self) -> Result<String>;

    /// List all discoverable sessions across installed providers.
    ///
    /// Optionally filter by provider slug and/or workspace path.
    pub async fn list_sessions(
        &self,
        provider: Option<&str>,
        workspace: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionSummary>>;

    /// Get detailed info for a single session.
    pub async fn session_info(
        &self,
        session_id: &str,
    ) -> Result<CanonicalSession>;

    /// Convert a session from one provider to another.
    ///
    /// Returns the resume command and metadata about the written session.
    pub async fn resume(
        &self,
        session_id: &str,
        target_provider: &str,
        source_hint: Option<&str>,
        force: bool,
    ) -> Result<ResumeResult>;

    /// List installed providers and their detection status.
    pub async fn list_providers(&self) -> Result<Vec<ProviderInfo>>;

    /// Resume a session AND execute the resume command in a pane.
    ///
    /// Combines `resume()` with `WeztermHandle::send_text()` to
    /// actually launch the resumed session in a terminal pane.
    pub async fn resume_in_pane(
        &self,
        session_id: &str,
        target_provider: &str,
        source_hint: Option<&str>,
        pane_id: u64,
        handle: &WeztermHandle,
    ) -> Result<ResumeResult>;
}
```

### `recorder_casr_export.rs` -- Flight Recorder Export API

```rust
//! Export flight recorder sessions as CanonicalSession IR.
//!
//! Converts FrankenTerm's internal recorder event streams into
//! casr-compatible CanonicalSession format for cross-agent resume.

use crate::casr_types::{
    CanonicalMessage, CanonicalSession, MessageRole, ToolCall, ToolResult,
};
use crate::error::Result;
use crate::patterns::AgentType;
use crate::recorder_audit::AccessTier;
use crate::recorder_query::{RecorderEventReader, RecorderQueryRequest};
use crate::recording::RecorderEvent;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// Configuration for recorder-to-CanonicalSession export.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RecorderExportConfig {
    /// Time gap (ms) between events that triggers a session boundary.
    pub session_gap_ms: u64,
    /// Time window (ms) for coalescing consecutive output frames
    /// into a single message.
    pub coalesce_window_ms: u64,
    /// Maximum content length per message (chars). Longer content
    /// is truncated with a "[truncated]" suffix.
    pub max_message_content_len: usize,
    /// Whether to include terminal control sequences in exported
    /// content, or strip them to plain text.
    pub strip_ansi: bool,
    /// Minimum access tier required for export (default: A2).
    pub min_access_tier: AccessTier,
}

impl Default for RecorderExportConfig {
    fn default() -> Self {
        Self {
            session_gap_ms: 30 * 60 * 1000, // 30 minutes
            coalesce_window_ms: 500,
            max_message_content_len: 100_000,
            strip_ansi: true,
            min_access_tier: AccessTier::A2,
        }
    }
}

/// Metadata about a recorder session boundary for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecorderSessionEntry {
    /// Synthetic session ID (pane_id + start_timestamp).
    pub session_id: String,
    /// Pane ID where the session was recorded.
    pub pane_id: u64,
    /// Agent type detected in this session (if any).
    pub agent_type: Option<AgentType>,
    /// Working directory at session start.
    pub workspace: Option<PathBuf>,
    /// Epoch ms of first event.
    pub started_at: i64,
    /// Epoch ms of last event.
    pub ended_at: i64,
    /// Number of messages after coalescing.
    pub message_count: usize,
}

/// Converts recorder events into CanonicalSession format.
pub struct RecorderCasrExporter {
    config: RecorderExportConfig,
}

impl RecorderCasrExporter {
    pub fn new(config: RecorderExportConfig) -> Self;

    /// List all exportable sessions from the recorder.
    ///
    /// Scans recorder events, detects session boundaries, and returns
    /// summary entries for each logical session.
    pub fn list_sessions(
        &self,
        reader: &dyn RecorderEventReader,
        query: &RecorderQueryRequest,
    ) -> Result<Vec<RecorderSessionEntry>>;

    /// Export a single recorder session as a CanonicalSession.
    ///
    /// Reads events for the specified pane and time range, coalesces
    /// them into messages, and produces a CanonicalSession ready for
    /// casr consumption or direct JSON serialization.
    pub fn export_session(
        &self,
        reader: &dyn RecorderEventReader,
        pane_id: u64,
        start_ms: i64,
        end_ms: i64,
        agent_type: Option<AgentType>,
        workspace: Option<&str>,
    ) -> Result<CanonicalSession>;

    /// Export a session and write it as JSON to a file.
    ///
    /// Convenience method combining export_session() with atomic
    /// file write (temp-then-rename pattern).
    pub fn export_to_file(
        &self,
        reader: &dyn RecorderEventReader,
        pane_id: u64,
        start_ms: i64,
        end_ms: i64,
        output_path: &std::path::Path,
    ) -> Result<PathBuf>;

    // -- Internal helpers --

    /// Map a RecorderEvent to a CanonicalMessage role.
    fn event_to_role(event: &RecorderEvent) -> MessageRole;

    /// Coalesce consecutive same-role events within the time window.
    fn coalesce_messages(
        events: Vec<(RecorderEvent, MessageRole)>,
        window_ms: u64,
    ) -> Vec<CanonicalMessage>;

    /// Strip ANSI escape sequences from text content.
    fn strip_ansi_codes(text: &str) -> String;

    /// Generate a synthetic session ID from pane ID and timestamp.
    fn synthetic_session_id(pane_id: u64, start_ms: i64) -> String;
}

/// Map FrankenTerm AgentType to casr provider_slug.
pub fn agent_type_to_provider_slug(agent: &AgentType) -> &'static str {
    match agent {
        AgentType::ClaudeCode => "claude-code",
        AgentType::Codex => "codex",
        AgentType::Gemini => "gemini",
        AgentType::Wezterm => "frankenterm",
        AgentType::Unknown => "unknown",
    }
}
```

---

## P5-P6: Migration, Testing

### Migration

No migration is required. All new functionality is additive behind the `session-resume` feature flag. No existing database schemas, configuration files, or APIs are modified.

### Unit Tests

#### `casr_types.rs` -- 30+ tests

| # | Test | Description |
|---|------|-------------|
| 1-5 | `canonical_session_roundtrip_*` | Serialize/deserialize CanonicalSession with all field combinations (full, minimal, empty messages, with metadata, with source_path) |
| 6-9 | `canonical_message_roundtrip_*` | Message with tool_calls, tool_results, extra fields, empty content |
| 10-13 | `message_role_serde_*` | Each MessageRole variant serializes to expected string and deserializes back |
| 14 | `message_role_other_variant` | `Other("custom")` round-trips correctly |
| 15-17 | `tool_call_serde_*` | ToolCall with/without id, with complex arguments JSON |
| 18-20 | `tool_result_serde_*` | ToolResult with/without call_id, is_error true/false |
| 21-23 | `casr_resume_output_*` | Parse success JSON, error JSON, partial/unknown fields |
| 24-26 | `casr_list_entry_*` | Parse list entries with varying field presence |
| 27-29 | `casr_provider_status_*` | Parse provider status with/without version, evidence |
| 30-32 | `forward_compat_*` | Unknown fields in JSON are silently ignored (no deserialization failure) |
| 33 | `empty_session_valid` | Session with zero messages deserializes without error |
| 34 | `large_message_content` | 1MB content string roundtrips without truncation |

#### `session_resume.rs` -- 35+ tests

| # | Test | Description |
|---|------|-------------|
| 1 | `is_available_when_casr_missing` | Returns false when `casr` not in PATH |
| 2 | `is_available_when_casr_present` | Returns true with mock binary |
| 3 | `version_parse` | Parses version string from `casr --version` output |
| 4-7 | `list_sessions_*` | Empty list, single provider filter, workspace filter, limit |
| 8-10 | `list_sessions_json_parse_*` | Valid JSON, malformed JSON, empty stdout |
| 11-14 | `resume_success_*` | Successful resume with various provider combinations |
| 15-16 | `resume_error_*` | SessionNotFound error, AmbiguousSessionId error |
| 17 | `resume_with_source_hint` | `--source` flag passed correctly |
| 18 | `resume_with_force` | `--force` flag passed correctly |
| 19 | `resume_with_enrich` | `--enrich` flag from config |
| 20-22 | `list_providers_*` | All installed, none installed, mixed |
| 23 | `timeout_exceeded` | Subprocess killed after timeout_ms |
| 24 | `output_size_exceeded` | Large stdout truncated and error returned |
| 25 | `stderr_captured_as_warning` | Non-empty stderr logged via tracing |
| 26-28 | `config_*` | Default config, custom binary path, custom timeout |
| 29 | `session_info_success` | Parses full CanonicalSession from `casr info --json` |
| 30 | `session_info_not_found` | Returns appropriate error |
| 31-33 | `resume_in_pane_*` | Sends resume command to pane, handles pane not found, handles send_text failure |
| 34 | `concurrent_resume_calls` | Multiple async resume calls complete without interference |
| 35 | `casr_exit_code_nonzero` | Non-zero exit code mapped to error with stderr context |

#### `recorder_casr_export.rs` -- 35+ tests

| # | Test | Description |
|---|------|-------------|
| 1-3 | `export_empty_*` | No events -> empty session, single event -> single message, no matching pane -> error |
| 4-6 | `event_to_role_*` | Input->User, Output->Assistant, Marker->System |
| 7-9 | `coalesce_same_role_*` | Two consecutive User messages within window merge, across window stay separate, different roles never merge |
| 10 | `coalesce_mixed_roles` | User-Assistant-User produces 3 messages |
| 11-13 | `session_boundary_*` | Gap > threshold splits sessions, gap < threshold merges, exact boundary edge case |
| 14 | `list_sessions_multiple_panes` | Events from 3 panes produce 3 separate sessions |
| 15-16 | `strip_ansi_*` | ANSI codes removed, plain text unchanged |
| 17 | `strip_ansi_disabled` | Config `strip_ansi: false` preserves codes |
| 18-19 | `max_content_truncation_*` | Content exceeding limit truncated with suffix, content under limit unchanged |
| 20 | `synthetic_session_id_format` | ID format is `ft-{pane_id}-{start_ms}` |
| 21 | `synthetic_session_id_deterministic` | Same inputs produce same ID |
| 22-23 | `metadata_enrichment_*` | Workspace, agent_type populated in session metadata |
| 24 | `provider_slug_frankenterm` | Exported sessions have `provider_slug = "frankenterm"` |
| 25 | `agent_type_to_provider_slug_mapping` | All AgentType variants map to correct slugs |
| 26-27 | `timestamp_ordering_*` | Messages ordered by timestamp, started_at < ended_at |
| 28 | `reindex_after_coalesce` | Message idx values are sequential 0..n after coalescing |
| 29 | `export_to_file_atomic` | File written atomically (temp-then-rename) |
| 30 | `export_to_file_parent_dir_created` | Parent directories created if missing |
| 31 | `sensitivity_filtering_t3_excluded` | T3 events excluded at default A2 access tier |
| 32 | `sensitivity_filtering_t3_included` | T3 events included at A3 access tier |
| 33 | `redacted_events_placeholder` | Redacted events export as `[REDACTED]` |
| 34 | `tool_detection_to_tool_call` | Pattern detection with tool-use rule mapped to ToolCall |
| 35 | `large_session_performance` | 10,000 events export in < 1 second |

### Property-Based Tests

#### `tests/proptest_casr_types.rs` -- 30+ properties

| # | Property | Description |
|---|----------|-------------|
| 1-3 | `canonical_session_roundtrip` | Any CanonicalSession survives JSON serialize/deserialize |
| 4-6 | `canonical_message_roundtrip` | Any CanonicalMessage survives roundtrip |
| 7 | `message_role_roundtrip` | Any MessageRole variant survives roundtrip |
| 8 | `tool_call_roundtrip` | Any ToolCall survives roundtrip |
| 9 | `tool_result_roundtrip` | Any ToolResult survives roundtrip |
| 10-11 | `resume_output_forward_compat` | Extra JSON fields do not cause deserialization failure |
| 12-13 | `list_entry_forward_compat` | Extra JSON fields silently ignored |
| 14-15 | `message_count_preserved` | Session message count unchanged across roundtrip |
| 16-18 | `role_ordering_preserved` | Message roles maintain original sequence |
| 19-21 | `timestamp_monotonicity` | If input timestamps monotonic, output timestamps monotonic |
| 22-24 | `content_integrity` | Message content bytes unchanged across roundtrip |
| 25-27 | `session_id_nonempty` | Deserialized session_id is never empty |
| 28-30 | `metadata_preserved` | Arbitrary JSON metadata survives roundtrip |

#### `tests/proptest_recorder_casr_export.rs` -- 30+ properties

| # | Property | Description |
|---|----------|-------------|
| 1-3 | `coalesce_reduces_count` | Coalescing never increases message count |
| 4-5 | `coalesce_preserves_content` | Concatenated coalesced content equals original |
| 6-8 | `session_boundary_monotone` | Session start times are strictly increasing |
| 9-10 | `export_message_count_bounded` | Message count <= event count |
| 11-13 | `role_distribution_valid` | Every message has a valid MessageRole |
| 14-15 | `idx_sequential` | Message indices are 0, 1, 2, ... n-1 |
| 16-18 | `timestamp_range_valid` | started_at <= ended_at for every session |
| 19-20 | `strip_ansi_idempotent` | Stripping twice equals stripping once |
| 21-23 | `truncation_respects_limit` | Truncated content length <= limit + suffix length |
| 24-25 | `synthetic_id_unique` | Different (pane_id, start_ms) pairs produce different IDs |
| 26-28 | `sensitivity_filter_monotone` | Higher access tier sees >= events as lower tier |
| 29-30 | `provider_slug_always_frankenterm` | Exported sessions always have provider_slug "frankenterm" |

---

## P7: Rollout

### Phase 1: Foundation (Week 1-2)

**Deliverables**:
- `casr_types.rs` with all vendored types and 30+ tests
- `session_resume.rs` with subprocess wrapper and 35+ tests
- `recorder_casr_export.rs` with recorder-to-IR converter and 35+ tests
- Feature flag `session-resume` in Cargo.toml
- `proptest_casr_types.rs` and `proptest_recorder_casr_export.rs`

**Feature flag addition**:
```toml
[features]
session-resume = []  # No external deps; subprocess + vendored types
```

**Acceptance criteria**:
- `cargo test --features session-resume` passes all 130+ new tests
- `cargo test` (without feature) compiles cleanly -- no regressions
- `cargo clippy --features session-resume` clean

### Phase 2: CLI Integration (Week 3-4)

**Deliverables**:
- `ft resume <session-id> --target <provider>` CLI command
- `ft sessions list [--provider <slug>]` CLI command (combines casr list + recorder sessions)
- `ft sessions info <session-id>` CLI command
- Robot-mode JSON output for all new commands
- Wire `resume_in_pane` into pane management (send resume command to selected pane)

**Acceptance criteria**:
- Manual end-to-end test: record an agent session in FrankenTerm, export it, resume in a different provider
- Robot-mode JSON output matches documented schema

### Phase 3: Agent Correlation (Week 5-6)

**Deliverables**:
- Connect `agent_correlator.rs` with casr's session discovery to populate external session IDs in `AgentMetadata`
- Enhance `session_correlation.rs` to use casr session list as a correlation source alongside existing CASS correlation
- Agent inventory (`ft robot agents`) enriched with casr provider info

**Acceptance criteria**:
- When an agent is detected in a pane AND casr finds a matching session, the `AgentMetadata.session_id` field is populated
- `ft robot agents --json` includes `external_session_id` when available

### Phase 4: MCP and Upstream (Week 7-8)

**Deliverables**:
- MCP tool `ft_resume_session(session_id, target_provider, pane_id)` for agent-initiated resume
- MCP resource `ft://sessions/list` for session discovery
- Upstream proposal: `FrankenTermProvider` implementation in casr (reads exported JSON from FrankenTerm)
- Upstream proposal: feature flags for casr's SQLite providers (`sqlite-providers` feature)

**Acceptance criteria**:
- Agent (Claude Code, Codex) can invoke `ft_resume_session` via MCP to resume a session from another provider
- Upstream PR opened for FrankenTermProvider with tests

### Phase 5: Library Upgrade (Future, conditional)

**Trigger**: If subprocess latency (50-200ms per invocation) proves problematic for interactive workflows (e.g., real-time session search in TUI).

**Deliverables**:
- Replace subprocess calls in `session_resume.rs` with direct casr library calls
- Feature flag change: `session-resume = ["dep:casr"]`
- Benchmark: library vs subprocess latency comparison

**Decision criteria**:
- Subprocess p99 latency > 500ms in production monitoring
- User feedback requesting faster session listing

### Rollback Strategy

Each phase is independently reversible:

- **Phase 1**: Remove `session-resume` feature flag and associated source files. No existing functionality affected.
- **Phase 2**: Remove CLI command registrations. Existing CLI unchanged.
- **Phase 3**: Revert agent_correlator changes. Existing correlation (CASS-based) continues working.
- **Phase 4**: Remove MCP tool registrations. Other MCP tools unaffected.

Global rollback: disable `session-resume` feature flag. All new code becomes dead (compile-excluded). Zero impact on FrankenTerm's existing functionality.

---

## P8: Summary

### New Modules (3)

| Module | LOC (est.) | Tests | Purpose |
|--------|-----------|-------|---------|
| `casr_types.rs` | ~250 | 34+ unit, 30+ proptest | Vendored CanonicalSession IR types |
| `session_resume.rs` | ~400 | 35+ unit | CASR subprocess wrapper + resume orchestrator |
| `recorder_casr_export.rs` | ~450 | 35+ unit, 30+ proptest | Flight recorder -> CanonicalSession converter |

**Total**: ~1,100 LOC new code, ~160+ tests.

### Upstream Tweak Proposals (for cross_agent_session_resumer)

1. **FrankenTermProvider**: Implement `Provider` trait for FrankenTerm's exported JSON format. `detect()` checks for `ft` binary and exported session directories. `read_session()` parses exported CanonicalSession JSON files. `write_session()` writes JSON for FrankenTerm import. `resume_command()` returns `ft resume <id>`. This makes FrankenTerm a first-class casr provider alongside the existing 14.

2. **Feature flags for SQLite providers**: Add `sqlite-providers` feature gating `rusqlite` dependency for Cursor and OpenCode providers. Consumers that don't need SQLite-backed providers can opt out, reducing compile time and dependency footprint. Proposed in analysis R8.3.

3. **Provider registration API**: Add `ProviderRegistry::with_custom_provider(Box<dyn Provider>)` to allow external crates to register providers without forking. This enables FrankenTerm to register its provider at runtime when using library integration (Phase 5).

4. **CanonicalSession builder**: Add a builder pattern for `CanonicalSession` to simplify programmatic construction in `recorder_casr_export.rs`. Currently requires manual struct construction with 10 fields.

5. **Stable JSON output schema versioning**: Add a `schema_version` field to `--json` output so consumers can detect breaking changes. FrankenTerm's vendored types can check this field and warn/error on unsupported versions.

### Beads

- `ft-2vuw7.27.1` -- Research pass (CLOSED)
- `ft-2vuw7.27.2` -- Comprehensive analysis (CLOSED)
- `ft-2vuw7.27.3` -- This integration plan

### Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| Subprocess over library | Avoids rusqlite conflict, matches existing cass.rs pattern, zero compile-time cost |
| Vendored types over direct import | Forward-compatible JSON parsing, no casr build dependency |
| Feature-gated | No impact on default builds, independent release cadence |
| Recorder export as separate module | Clean separation between FrankenTerm internals (recorder events) and external format (CanonicalSession) |
| Three modules not one | Single-responsibility: types (casr_types), CLI wrapper (session_resume), data conversion (recorder_casr_export) |

---

*Plan complete.*
