# Comprehensive Analysis of cross_agent_session_resumer

> Analysis document for FrankenTerm bead `ft-2vuw7.27.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**cross_agent_session_resumer** (casr) is an 18.7K LOC single-crate Rust binary (edition 2024, MSRV 1.85) that converts and resumes AI agent sessions across 14 providers. It defines a canonical session IR, reads/writes native provider formats, and verifies conversion fidelity via read-back verification.

**Integration Value**: High — enables FrankenTerm to resume sessions across agent boundaries, a key multiplexer capability.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~18,700 |
| **Crate Count** | 1 (binary + library) |
| **Rust Edition** | 2024 (nightly) |
| **MSRV** | 1.85 |
| **License** | MIT + OpenAI/Anthropic Rider |
| **Unsafe Code** | `#![forbid(unsafe_code)]` |
| **Runtime** | Synchronous (no async) |

### Module Structure

```
casr/
├── src/
│   ├── lib.rs          # Module exports
│   ├── main.rs         # CLI entry (608 LOC)
│   ├── model.rs        # Canonical IR (267 LOC)
│   ├── error.rs        # Typed errors (246 LOC)
│   ├── discovery.rs    # Provider registry (527 LOC)
│   ├── pipeline.rs     # Conversion orchestration (536 LOC)
│   └── providers/      # 14 implementations
│       ├── claude_code.rs, codex.rs, gemini.rs, cursor.rs
│       ├── cline.rs, aider.rs, amp.rs, opencode.rs
│       ├── chatgpt.rs, clawdbot.rs, vibe.rs
│       ├── factory.rs, openclaw.rs, pi_agent.rs
│       └── mod.rs      # Provider trait
├── tests/              # 18 integration test files
```

---

## Core Architecture

### Canonical Session IR

```rust
CanonicalSession {
    session_id: String,
    provider_slug: String,
    workspace: Option<PathBuf>,
    messages: Vec<CanonicalMessage>,
    metadata: serde_json::Value,
    // ... timestamps, model_name, source_path
}

CanonicalMessage {
    idx: usize,
    role: MessageRole,  // User|Assistant|Tool|System|Other
    content: String,
    tool_calls: Vec<ToolCall>,
    tool_results: Vec<ToolResult>,
    extra: serde_json::Value,  // preserved for round-trip
}
```

### Provider Trait

```rust
pub trait Provider: Send + Sync {
    fn slug(&self) -> &str;
    fn detect(&self) -> DetectionResult;
    fn read_session(&self, path: &Path) -> Result<CanonicalSession>;
    fn write_session(&self, session: &CanonicalSession, opts: &WriteOptions) -> Result<WrittenSession>;
    fn resume_command(&self, session_id: &str) -> String;
    // ...
}
```

### Conversion Pipeline

```
Detect → Resolve Source → Read → Validate → [Enrich] → Write → Verify (read-back)
```

- **Validation**: Hard errors (no messages) vs warnings (no workspace) vs info (tool calls)
- **Enrichment**: Optional system messages about conversion source/target
- **Verification**: Read-back comparison of message counts/roles/content
- **Rollback**: On verify failure, attempt to delete written files

### CLI Commands

- `casr <target> resume <session-id>` — Convert and print resume command
- `casr list` — Enumerate sessions across providers
- `casr info <session-id>` — Show session details
- `casr providers` — List detected providers

---

## FrankenTerm Integration Assessment

### Integration Points

1. **Session portability**: FrankenTerm manages multiple agent panes; casr enables moving sessions between them
2. **Session discovery**: List all agent sessions across installed providers for unified view
3. **Resume automation**: Generate and execute resume commands in the right pane
4. **Canonical IR reuse**: Share CanonicalSession model for session indexing

### Integration Pattern

**Feature-gated library import** — casr exposes a public library API:

```rust
#[cfg(feature = "session-resume")]
fn resume_in_provider(session_id: &str, target: &str) -> Result<()> {
    let pipeline = casr::pipeline::ConversionPipeline {
        registry: casr::discovery::ProviderRegistry::default_registry(),
    };
    let result = pipeline.convert(target, session_id, ConvertOptions::default())?;
    let command = result.written.unwrap().resume_command;
    // Execute in FrankenTerm pane
}
```

### Subprocess Alternative

```bash
casr cc resume <session-id> --json
```

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Provider format changes | Medium | Medium | Maintained alongside franken_agent_detection |
| Synchronous blocking | Low | Low | Run in tokio spawn_blocking |
| Large session files | Low | Medium | Memory proportional to session size |

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | Single-crate session converter with 14 providers |
| **Key Innovation** | Canonical IR + read-back verification + rollback |
| **FrankenTerm Status** | No integration |
| **Integration Priority** | High — enables cross-agent session portability |
| **New Modules Needed** | 1 (session_resume.rs bridge) |
| **Dependencies** | casr library or subprocess CLI |

---

*Analysis complete.*
