# Plan to Deeply Integrate agentic_coding_flywheel_setup into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.30.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_agentic_coding_flywheel_setup.md (ft-2vuw7.30.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Minimal**: ACFS is a VPS provisioning tool with limited runtime relevance to FrankenTerm

### Constraints

- **TypeScript + Bash**: No Rust interface
- **One-time provisioning**: ACFS runs during setup, not at runtime
- **Ubuntu-only target**: VPS provisioning; FrankenTerm is cross-platform

### Non-Goals

- **Deep integration**: ACFS provisions environments; FrankenTerm operates in them
- **Tool management**: ACFS handles tool installation; FrankenTerm assumes tools exist
- **Manifest parsing**: Not needed at runtime

---

## P2: Evaluate Integration Patterns

### Option A: Skip Integration (Chosen)

No integration — ACFS provisions the environment before FrankenTerm runs.

**Pros**: Zero maintenance burden, clean separation of concerns
**Cons**: None significant
**Chosen**: ACFS and FrankenTerm operate at different lifecycle phases

### Decision: Option A — Skip integration

---

## P3-P8: Not Applicable

No modules, no API, no tests, no rollout needed.

### Summary

**No integration** — agentic_coding_flywheel_setup is a VPS provisioning tool (TypeScript + Bash) that runs before FrankenTerm. It installs ~60 tools including agent CLIs that FrankenTerm then manages. The two tools operate at different lifecycle phases: ACFS provisions, FrankenTerm operates.

---

*Plan complete.*
