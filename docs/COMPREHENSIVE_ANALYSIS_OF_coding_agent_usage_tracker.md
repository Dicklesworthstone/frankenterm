# Comprehensive Analysis of coding_agent_usage_tracker

> Analysis document for FrankenTerm bead `ft-2vuw7.28.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**coding_agent_usage_tracker** (caut) is a 39.7K LOC single-crate Rust binary (edition 2024) that tracks rate limits, cost attribution, and credential health across 16 AI coding agent providers. It features a multi-strategy fetch pipeline (CLI/Web/OAuth/API), SQLite history, keyring credential storage, TUI dashboard via ratatui, and rich terminal output.

**Integration Value**: High — FrankenTerm agents need rate limit awareness for scheduling and backpressure.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~39,700 |
| **Crate Count** | 1 (single binary) |
| **Rust Edition** | 2024 |
| **MSRV** | 1.88+ |
| **License** | MIT + OpenAI/Anthropic Rider |
| **Unsafe Code** | `#![deny(unsafe_code)]` (tests allow for env manipulation) |
| **Runtime** | tokio (async) |

### Module Structure

```
caut/
├── src/
│   ├── main.rs           # CLI entry (tokio::main)
│   ├── lib.rs            # Library root
│   ├── cli/              # Command dispatch (8 modules)
│   ├── core/             # Provider logic, fetch pipeline (19 modules)
│   ├── providers/        # Codex, Claude implementations
│   ├── render/           # Human/JSON/Markdown output (4 modules)
│   ├── rich/             # Terminal rich text
│   ├── storage/          # Config, cache, history (8 modules)
│   ├── tui/              # Ratatui dashboard (4 modules)
│   ├── util/             # Formatting, env, time helpers
│   └── error/            # Error taxonomy + fix suggestions
├── tests/                # 15 integration test files
├── migrations/           # SQLite schema migrations
├── schemas/              # JSON schema definitions
```

---

## Core Architecture

### Multi-Strategy Fetch Pipeline

```
fetch_provider(provider, mode)
  → Get FetchPlan (ordered strategy list)
  → Try strategies in fallback order:
      CLI → Web → OAuth → API → Local
  → First success → FetchOutcome
  → All fail → Aggregate errors
```

### Supported Providers (16)

Codex, Claude, Gemini, Cursor, Copilot, Zai, MiniMax, Kimi, KimiK2, Kiro, VertexAI, JetBrains, Antigravity, OpenCode, Factory, Amp

### Key Data Models

```rust
RateWindow {
    used_percent: f64,
    window_minutes: Option<i32>,
    resets_at: Option<DateTime<Utc>>,
}

UsageSnapshot {
    primary: Option<RateWindow>,
    secondary: Option<RateWindow>,
    tertiary: Option<RateWindow>,
    identity: Option<ProviderIdentity>,
}

ProviderPayload {
    provider: String,
    account: String,
    usage: UsageSnapshot,
    credits: Option<CreditsSnapshot>,
    status: Option<StatusPayload>,
}
```

### Error Taxonomy

6 categories with stable error codes:
- **Authentication** (CAUT-A*): expired, not configured, invalid
- **Network** (CAUT-N*): timeout, DNS, SSL
- **Configuration** (CAUT-C*): invalid config, unknown provider
- **Provider** (CAUT-P*): not found, unavailable, rate limited
- **Environment** (CAUT-E*): permission denied, missing env var
- **Internal** (CAUT-X*): parse error, unexpected None

### CLI Commands

```bash
caut usage [--provider <name>] [--format json]  # Rate limit status
caut cost [--provider <name>]                    # Local cost from JSONL logs
caut session                                     # Session cost attribution
caut dashboard                                   # TUI dashboard (ratatui)
caut history show|prune|stats|export             # Usage history
caut doctor                                      # Diagnose setup health
caut prompt                                      # Shell prompt integration
```

---

## FrankenTerm Integration Assessment

### Integration Points

1. **Rate limit awareness**: FrankenTerm's backpressure system can import provider rate limits
2. **Cost attribution per pane**: Map agent pane activity to provider costs
3. **Credential health monitoring**: Surface credential expiry warnings in pane status
4. **Dashboard embedding**: Display usage data in FrankenTerm's monitoring views
5. **Multi-account switching**: Coordinate account selection across agent panes

### Integration Pattern

**Subprocess CLI** — FrankenTerm calls `caut usage --json`:

```rust
pub async fn query_usage(provider: &str) -> Result<UsageSnapshot> {
    let output = Command::new("caut")
        .args(&["usage", "--provider", provider, "--json"])
        .output().await?;
    let robot: RobotOutput = serde_json::from_slice(&output.stdout)?;
    // Extract usage data
}
```

### Backpressure Integration

Map caut's rate windows to FrankenTerm's backpressure tiers:
- `used_percent < 50%` → Green
- `used_percent < 80%` → Yellow
- `used_percent < 95%` → Red
- `used_percent >= 95%` → Black (circuit break)

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Provider API changes | Medium | Medium | Multi-strategy fallback chain |
| Credential storage portability | Low | Low | Keyring + JSON fallback |
| Stale cached data | Low | Medium | TTL-based cache invalidation |

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | Single-crate usage tracker with 16 providers |
| **Key Innovation** | Multi-strategy fetch pipeline with fallback chain |
| **FrankenTerm Status** | No integration |
| **Integration Priority** | High — rate limit awareness for backpressure |
| **New Modules Needed** | 1 (usage_bridge.rs subprocess wrapper) |
| **Dependencies** | Subprocess CLI (caut binary) |

---

*Analysis complete.*
