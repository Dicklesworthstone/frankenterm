# Comprehensive Analysis of rano

> Analysis document for FrankenTerm bead `ft-2vuw7.23.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**rano** (Rust Agent Network Observer) is a 12.6K LOC single-crate Rust binary that monitors outbound network connections from AI CLI processes, attributing traffic to providers (Anthropic, OpenAI, Google, etc.). It uses SQLite for persistence, supports alerts, and includes provider presets. Standalone tool with no FrankenTerm dependency.

**Integration Value**: Medium — network observability data is useful for FrankenTerm's agent monitoring but rano is a separate daemon.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~12,600 |
| **Crate Count** | 1 (single binary) |
| **Rust Edition** | 2024 |
| **MSRV** | 1.85 |
| **License** | MIT |
| **Platform** | Cross-platform (macOS lsof, Linux /proc/net) |

### Key Modules

- **observer.rs**: Process network connection scanner
- **attribution.rs**: Map connections to AI providers (Anthropic, OpenAI, Google, Cohere, etc.)
- **storage.rs**: SQLite connection history + aggregation
- **alerts.rs**: Threshold-based alerting (connection count, bandwidth, new providers)
- **presets.rs**: Known AI CLI process names (claude, codex, gemini, cursor, etc.)
- **cli.rs**: clap-based CLI (watch, report, export, alert)

---

## Core Architecture

### Observation Pipeline

```
[Process Scanner] → [Connection Extractor] → [Provider Attribution]
     → [SQLite Storage] → [Alert Evaluation] → [Dashboard/Export]
```

### Provider Attribution

Maps destination IPs/hostnames to providers:
- `api.anthropic.com` → Anthropic
- `api.openai.com` → OpenAI
- `generativelanguage.googleapis.com` → Google
- DNS reverse lookup + WHOIS fallback for unknown hosts

### Agent Process Detection

Presets for 15+ AI CLIs: claude, codex, gemini, cursor, cline, aider, copilot, amp, etc. Uses process name matching via `ps` / `/proc`.

---

## FrankenTerm Integration Assessment

### Relevant Integration Points

1. **Agent network attribution**: FrankenTerm manages agent panes; rano can attribute their network traffic to providers
2. **Cost estimation**: Network volume correlates with API usage; supplement caut's cost tracking
3. **Anomaly detection**: Unusual connection patterns (new providers, unexpected destinations) as security signal
4. **Dashboard widget**: Display per-pane network attribution in FrankenTerm's monitoring

### Integration Pattern

**Subprocess CLI** — FrankenTerm calls `rano report --json` for network data.

```rust
// Potential integration
pub async fn query_network_attribution(pane_pid: u32) -> Result<Vec<ConnectionInfo>> {
    let output = Command::new("rano")
        .args(&["report", "--pid", &pane_pid.to_string(), "--json"])
        .output().await?;
    // Parse JSON output
}
```

### Limitations

- rano requires elevated privileges on some platforms (macOS: lsof, Linux: /proc/net)
- Daemon mode adds another running process
- Attribution accuracy depends on DNS resolution

---

## Risks

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| Privilege requirements | Medium | Medium | Graceful degradation if unavailable |
| Attribution accuracy | Low | Low | Best-effort, not critical path |
| Daemon overhead | Low | Low | On-demand queries, not always-on |

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | Single-crate network observer daemon |
| **Key Innovation** | AI provider attribution for CLI agent traffic |
| **FrankenTerm Status** | No integration |
| **Integration Priority** | Medium |
| **New Modules Needed** | 1 (network_observer.rs subprocess wrapper) |
| **Recommendation** | Feature-gated subprocess integration for agent monitoring |

---

*Analysis complete.*
