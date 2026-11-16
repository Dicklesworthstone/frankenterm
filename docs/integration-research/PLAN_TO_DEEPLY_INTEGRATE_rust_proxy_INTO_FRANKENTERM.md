# Plan to Deeply Integrate rust_proxy into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.22.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_rust_proxy.md (ft-2vuw7.22.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Minimal status bridge**: If agents route through rust_proxy, display upstream health in FrankenTerm dashboards

### Constraints

- **Linux-only**: rust_proxy uses iptables/ipset; FrankenTerm is cross-platform (macOS primary)
- **Limited overlap**: FrankenTerm already has circuit breaker, health checking, and Prometheus metrics
- **Standalone proxy**: No shared types or runtime

### Non-Goals

- **Deep integration**: rust_proxy's functionality doesn't align with terminal multiplexing
- **Cross-platform port**: iptables is fundamentally Linux-only
- **Algorithm extraction**: FrankenTerm already has circuit breaker and LB implementations

---

## P2: Evaluate Integration Patterns

### Option A: Skip Integration (Chosen)

No integration — the overlap is too small and the platform constraint too large.

**Pros**: Zero maintenance burden, no unnecessary coupling
**Cons**: Loses potential proxy health visibility
**Chosen**: Cost/benefit strongly favors skipping

### Option B: Optional Status Reader

Subprocess call to `rust_proxy status --json` for health data.

**Pros**: Minimal code, optional
**Cons**: Linux-only, proxy may not be deployed
**Rejected**: Too niche for the maintenance cost

### Decision: Option A — Skip integration

---

## P3-P8: Not Applicable

No modules, no API, no tests, no rollout needed.

### Summary

**No integration** — rust_proxy is Linux-only and its functionality (transparent HTTP proxy, iptables) has minimal relevance to FrankenTerm's cross-platform terminal multiplexer role. FrankenTerm already implements circuit breaker and health checking patterns independently.

---

*Plan complete.*
