# Plan to Deeply Integrate automated_plan_reviser_pro into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.29.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_automated_plan_reviser_pro.md (ft-2vuw7.29.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Minimal**: APR is a Bash spec refinement tool with limited terminal multiplexer relevance

### Constraints

- **Bash tool**: No Rust library interface; subprocess-only
- **Oracle dependency**: Requires browser automation for GPT Pro; heavy external requirement
- **Limited overlap**: Spec refinement is orthogonal to terminal multiplexing

### Non-Goals

- **Deep integration**: APR's functionality doesn't align with FrankenTerm's core
- **Reimplementation**: APR's convergence detection is domain-specific
- **Library import**: Not possible (Bash)

---

## P2: Evaluate Integration Patterns

### Option A: Skip Integration (Chosen)

No integration — APR is a development workflow tool, not a terminal multiplexer component.

**Pros**: Zero maintenance burden, no unnecessary coupling
**Cons**: None significant
**Chosen**: Cost/benefit strongly favors skipping

### Decision: Option A — Skip integration

---

## P3-P8: Not Applicable

No modules, no API, no tests, no rollout needed.

### Summary

**No integration** — automated_plan_reviser_pro is a Bash-based spec refinement tool using GPT Pro browser automation. Its functionality (iterative spec revision, convergence detection) is orthogonal to FrankenTerm's terminal multiplexer role.

---

*Plan complete.*
