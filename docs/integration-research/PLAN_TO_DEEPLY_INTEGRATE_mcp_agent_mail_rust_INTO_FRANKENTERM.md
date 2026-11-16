# Plan to Deeply Integrate mcp_agent_mail_rust into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.11.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_mcp_agent_mail_rust.md (ft-2vuw7.11.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Auto-reserve files on pane activity**: When a FrankenTerm-managed agent edits files, automatically reserve those paths via Agent Mail to prevent conflicts with other agents
2. **Pane-level agent identity**: Embed Agent Mail identity (agent name, ID) in FrankenTerm's PaneInfo metadata for tracking which agent operates in which pane
3. **Message-driven pane events**: Broadcast pane lifecycle events (open, close, resize, process change) as Agent Mail messages for cross-agent awareness
4. **Backpressure signal import**: Import Agent Mail's 3-level backpressure (Green/Yellow/Red) as an additional signal in FrankenTerm's own backpressure system
5. **TOON encoding adoption**: Use Agent Mail's TOON encoding for token-efficient MCP responses from FrankenTerm's MCP tools

### Constraints

- **Agent Mail is the coordination backbone**: FrankenTerm agents already register and coordinate via Agent Mail MCP tools; deepening integration, not introducing it
- **MCP tool calls only**: FrankenTerm uses Agent Mail's MCP tools (send_message, reserve_paths, etc.); no library import needed
- **No additional runtime**: Agent Mail runs as a separate MCP server; FrankenTerm is a client
- **No unsafe code**: Both projects enforce `#![forbid(unsafe_code)]`
- **Backward compatible**: Existing Agent Mail workflows must continue unchanged

### Non-Goals

- **Replacing Agent Mail**: FrankenTerm deepens integration, doesn't replace any Agent Mail functionality
- **Importing Agent Mail library crates**: Use MCP tool calls, not Rust library imports
- **Building a custom Agent Mail TUI**: Agent Mail has its own 15-screen TUI
- **Forking Agent Mail**: All changes via upstream tweak proposals

---

## P2: Evaluate Integration Patterns

### Option A: MCP Tool Client (Chosen)

FrankenTerm calls Agent Mail's 34 MCP tools via JSON-RPC 2.0 for all coordination actions.

**Pros**: Already working, zero compile-time deps, standard MCP protocol, Agent Mail evolves independently
**Cons**: MCP call overhead (~1ms per tool call), requires Agent Mail server running
**Chosen**: This is the existing pattern; deepening it is the lowest-risk path

### Option B: Library Import (core crate)

Import `mcp-agent-mail-core` (25K LOC) for direct type access.

**Pros**: Type-safe, no serialization overhead
**Cons**: Pulls in 25K LOC + transitive deps; compile time increase
**Rejected**: MCP tools provide the same functionality without compile-time coupling

### Option C: Shared Types Crate

Extract shared types (Agent, Message, Reservation) to a micro-crate.

**Pros**: Type-safe without full core import
**Cons**: Upstream coordination; types already serialized as JSON in MCP responses
**Considered for Phase 3**: If MCP serialization overhead becomes measurable

### Decision: Option A — MCP tool client (deepened)

---

## P3: Target Placement Within FrankenTerm Subsystems

### Architecture Placement

```
frankenterm-core/
├── src/
│   ├── agent_mail_bridge.rs      # NEW: Agent Mail MCP client wrapper
│   ├── toon_encoding.rs          # NEW: TOON encoder/decoder for token-efficient MCP
│   └── ...existing modules...
├── Cargo.toml                    # No new dependencies
```

### Module Responsibilities

#### `agent_mail_bridge.rs` — Agent Mail integration layer

Wraps Agent Mail MCP tool calls with FrankenTerm-specific logic:
- `auto_reserve_on_edit(pane_id: u64, files: &[PathBuf])` — Reserve paths when agent starts editing
- `auto_release_on_close(pane_id: u64)` — Release reservations when pane closes
- `broadcast_pane_event(event: PaneEvent)` — Send pane lifecycle events as messages
- `embed_identity(pane_info: &mut PaneInfo, agent: &AgentIdentity)` — Add agent identity to pane metadata
- `import_backpressure() -> BackpressureLevel` — Read Agent Mail's backpressure level
- `check_inbox(agent_name: &str) -> Vec<Message>` — Check for incoming messages

#### `toon_encoding.rs` — Token-efficient encoding

TOON (Token-Optimized Object Notation) encoder/decoder:
- `toon_encode(value: &serde_json::Value) -> String` — Compress JSON to TOON
- `toon_decode(toon: &str) -> serde_json::Value` — Decompress TOON to JSON
- Removes quotes from keys, uses compact separators, strips null values
- Used by FrankenTerm's MCP tools for ~40% token reduction

### Dependency Wiring

```toml
# In crates/frankenterm-core/Cargo.toml

[features]
agent-mail = []   # No new deps — uses existing serde_json + MCP client
# TOON encoding has no feature gate — useful for all MCP output
```

---

## P4: API Contracts and Crate/Subcrate Extraction Roadmap

### Public API Contract: AgentMailBridge

```rust
#[cfg(feature = "agent-mail")]
pub mod agent_mail_bridge {
    pub struct AgentMailBridge {
        project_key: String,
        agent_name: String,
        mcp_client: McpClient,
    }

    impl AgentMailBridge {
        pub fn new(project_key: String, agent_name: String, mcp_client: McpClient) -> Self;
        pub async fn auto_reserve(&self, files: &[PathBuf], ttl_secs: u64) -> Result<ReservationResult>;
        pub async fn auto_release(&self) -> Result<()>;
        pub async fn broadcast_event(&self, event: &PaneEvent, thread_id: Option<&str>) -> Result<()>;
        pub async fn check_inbox(&self, limit: usize) -> Result<Vec<InboxMessage>>;
        pub async fn import_backpressure(&self) -> Result<BackpressureLevel>;
        pub fn agent_name(&self) -> &str;
    }

    pub struct ReservationResult {
        pub granted: Vec<String>,
        pub conflicts: Vec<ReservationConflict>,
    }

    pub struct ReservationConflict {
        pub path: String,
        pub holder: String,
        pub expires_at: Option<String>,
    }

    pub struct InboxMessage {
        pub id: String,
        pub from: String,
        pub subject: String,
        pub body: String,
        pub importance: String,
        pub timestamp: String,
    }
}
```

### Public API Contract: ToonEncoding

```rust
pub mod toon_encoding {
    pub fn encode(value: &serde_json::Value) -> String;
    pub fn decode(toon: &str) -> Result<serde_json::Value>;
    pub fn encode_with_options(value: &serde_json::Value, opts: &ToonOptions) -> String;

    pub struct ToonOptions {
        pub strip_nulls: bool,
        pub compact_arrays: bool,
        pub unquote_safe_keys: bool,
    }
}
```

### Crate Extraction Roadmap

**Phase 1**: Implement in frankenterm-core.
**Phase 2**: Extract `ft-toon` if multiple projects need TOON encoding.
**Phase 3**: If Agent Mail publishes a client SDK, switch to it.

---

## P5: Data Migration/State Synchronization and Compatibility Posture

### Migration Strategy

**No migration needed** — deepening existing MCP tool usage.

### State Synchronization

- **Agent Mail is source of truth** for reservations, messages, identities
- **FrankenTerm caches** agent identity locally in PaneInfo.extra
- **Auto-reserve follows pane lifecycle**: reserve on edit, release on close
- **Backpressure import polls**: Check Agent Mail health on configurable interval

### Compatibility Posture

- **Additive only**: All new capabilities layered on existing MCP tools
- **Graceful degradation**: If Agent Mail not running, bridge methods return Ok(default)
- **No breaking changes**: Existing `macro_start_session` workflow unchanged

---

## P6: Testing Strategy and Detailed Logging/Observability

### Unit Tests (per module)

#### `agent_mail_bridge.rs` tests (target: 30+)

- `test_auto_reserve_single_file` — reserves one path correctly
- `test_auto_reserve_multiple_files` — reserves multiple paths
- `test_auto_reserve_conflict_handling` — conflicts returned, not panic
- `test_auto_release_succeeds` — release works
- `test_auto_release_no_reservations` — no-op when nothing reserved
- `test_broadcast_pane_open` — pane open event broadcast correctly
- `test_broadcast_pane_close` — pane close event broadcast
- `test_check_inbox_empty` — empty inbox returns empty vec
- `test_check_inbox_with_messages` — messages parsed correctly
- `test_import_backpressure_green` — green level imported
- `test_import_backpressure_red` — red level imported
- `test_graceful_degradation_no_server` — returns defaults when AM down
- Additional MCP call and error handling tests for 30+ total

#### `toon_encoding.rs` tests (target: 30+)

- `test_encode_simple_object` — basic key-value encoding
- `test_encode_nested_object` — nested objects encoded
- `test_encode_array` — arrays encoded compactly
- `test_encode_null_stripped` — null values removed
- `test_encode_unquoted_keys` — safe keys unquoted
- `test_encode_special_char_keys` — keys with special chars quoted
- `test_decode_roundtrip` — encode then decode preserves data
- `test_decode_invalid_toon` — error on invalid input
- `test_token_reduction` — TOON is 30%+ shorter than JSON
- Additional encoding edge cases for 30+ total

### Integration Tests

- `test_pane_lifecycle_reservation` — open pane, edit file, reserve, close pane, release
- `test_multi_agent_conflict` — two agents edit same file, conflict detected
- `test_backpressure_import_drives_capture` — AM Red level reduces FrankenTerm capture rate
- `test_toon_mcp_response` — MCP tool returns TOON-encoded response

### Property-Based Tests

- `proptest_toon_roundtrip` — any JSON value round-trips through TOON
- `proptest_toon_shorter_than_json` — TOON length <= JSON length
- `proptest_reservation_idempotent` — double-reserve same files succeeds
- `proptest_release_safe` — release never panics

### Logging Requirements

```rust
tracing::debug!(
    agent = self.agent_name,
    paths = ?files,
    granted = result.granted.len(),
    conflicts = result.conflicts.len(),
    "agent_mail_bridge.auto_reserve"
);
```

---

## P7: Rollout, Rollback, Risk Mitigation, and Acceptance Gates

### Rollout Plan

**Phase 1: TOON Encoding (Week 1)**
1. Implement `toon_encoding.rs` (no feature gate)
2. Write 30+ unit tests
3. Gate: `cargo check --workspace`

**Phase 2: Agent Mail Bridge (Week 2-3)**
1. Implement `agent_mail_bridge.rs` behind `agent-mail` feature
2. Auto-reserve on file edit, auto-release on pane close
3. Write 30+ unit tests
4. Gate: Reservation lifecycle works end-to-end

**Phase 3: Pane Events and Backpressure (Week 4-5)**
1. Broadcast pane lifecycle events as messages
2. Import Agent Mail backpressure
3. Embed agent identity in PaneInfo.extra
4. Integration tests

**Phase 4: MCP TOON Responses (Week 6)**
1. Wire TOON encoding into FrankenTerm's MCP tool responses
2. Gate: Token reduction 30%+

### Rollback Plan

- **Phase 1**: Remove toon_encoding.rs
- **Phase 2**: Disable `agent-mail` feature
- **Phase 3**: Stop broadcasting events
- **Phase 4**: Remove TOON option; JSON remains default

### Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Agent Mail server unavailable | Medium | Low | Graceful degradation |
| MCP call latency | Low | Low | Batch calls, cache results |
| Reservation TTL expiry | Medium | Medium | Auto-renew |
| TOON parsing errors | Low | Medium | Fallback to JSON |

### Acceptance Gates

1. `cargo check --workspace --all-targets` (with and without feature)
2. `cargo test --workspace`
3. 60+ new unit tests across 2 modules
4. 4+ integration tests, 4+ proptest scenarios
5. TOON token reduction 30%+ in benchmark

---

## P8: Summary and Action Items

### Chosen Architecture

**MCP tool client** deepening via `agent_mail_bridge.rs` + **TOON encoding** via `toon_encoding.rs`.

### Two New Modules

1. **`agent_mail_bridge.rs`**: Auto-reserve/release, event broadcasting, backpressure import
2. **`toon_encoding.rs`**: TOON encoder/decoder for ~40% token reduction

### Upstream Tweak Proposals (for mcp_agent_mail_rust)

1. **Bulk reservation API**: `reserve_paths_batch` for atomic multi-path reservation
2. **Reservation change notifications**: Push notification on reservation changes
3. **TOON as default MCP format**: Default response format for all tools
4. **Backpressure export**: MCP resource for efficient polling

### Beads Created/Updated

- `ft-2vuw7.11.1` (CLOSED): Research complete
- `ft-2vuw7.11.2` (CLOSED): Analysis document complete
- `ft-2vuw7.11.3` (THIS DOCUMENT): Integration plan complete

---

*Plan complete. Ready for review and implementation bead creation.*
