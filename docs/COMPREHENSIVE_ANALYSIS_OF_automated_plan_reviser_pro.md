# Comprehensive Analysis of automated_plan_reviser_pro

> Analysis document for FrankenTerm bead `ft-2vuw7.29.2`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22

---

## Executive Summary

**automated_plan_reviser_pro** (APR) is a 6.9K LOC single-file Bash 4+ tool for iterative specification refinement via GPT Pro 5.2 Extended Reasoning. It orchestrates revision rounds through an Oracle browser automation backend, tracks convergence metrics, and provides a robot-mode JSON API. Not a Rust project.

**Integration Value**: Low — Bash tool for spec refinement; limited direct integration with FrankenTerm's Rust codebase.

---

## Repository Topology

| Metric | Value |
|--------|-------|
| **Total LOC** | ~6,900 |
| **Language** | Bash 4+ |
| **Crate Count** | N/A (not Rust) |
| **Version** | 1.2.2 |
| **License** | MIT |
| **Platform** | Cross-platform (macOS/Linux) |

### Key Components

- `apr` — Main executable (6.9K LOC single bash script)
- `install.sh` — Secure installer with checksum verification
- `tests/` — BATS test suite (unit + integration + e2e)
- `.apr/` — Per-project workflow directories

---

## Core Architecture

### Revision Workflow

```
README + Spec + [Implementation] → Bundle → Oracle (GPT Pro) → Round Output → Metrics
```

### Key Features

- **Iterative rounds**: Sequential numbered revisions with diff tracking
- **Convergence detection**: Exponential convergence scoring (0.0-1.0 confidence)
- **Oracle integration**: Browser automation via Node.js Oracle CLI
- **Robot mode**: JSON API for automation (`apr robot status`, `apr robot run`)
- **Metrics engine**: Document metrics, change tracking, convergence analytics
- **Session management**: Background execution with PID tracking, reattachment

### CLI Commands

```bash
apr run <N>        # Execute revision round
apr setup          # Interactive workflow configuration
apr status         # Oracle session status
apr list           # List workflows
apr history        # Round history
apr stats          # Convergence analytics
apr dashboard      # Interactive TUI dashboard
apr robot <cmd>    # Machine-readable JSON API
```

---

## FrankenTerm Integration Assessment

### Relevance

| Factor | Assessment |
|--------|-----------|
| **Language** | Bash (not Rust) — no library import possible |
| **Use Case** | Spec refinement; orthogonal to terminal multiplexing |
| **Runtime** | Synchronous bash + Oracle subprocess |
| **Data Format** | JSON robot mode output |

### Potential (Limited) Integration

1. **Spec revision automation**: FrankenTerm agents could invoke `apr robot run` to refine specs
2. **Convergence metrics**: Import APR's convergence data into project dashboards
3. **Session tracking**: APR's background sessions could be tracked as FrankenTerm panes

### Recommendation

**Minimal integration** — APR is a standalone workflow tool. If used, integration would be purely subprocess-based via `apr robot` JSON API.

---

## Summary

| Aspect | Details |
|--------|---------|
| **Architecture** | Single-file Bash spec revision tool |
| **Key Innovation** | Convergence detection for iterative refinement |
| **FrankenTerm Status** | No integration |
| **Integration Priority** | Low |
| **New Modules Needed** | 0 (subprocess only if needed) |
| **Recommendation** | Skip deep integration; use subprocess if needed |

---

*Analysis complete.*
