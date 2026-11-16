# Plan to Deeply Integrate rano into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.23.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_rano.md (ft-2vuw7.23.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Per-pane network attribution**: Query rano for network connections attributable to each agent pane's process
2. **Provider cost correlation**: Supplement caut's cost tracking with network volume data
3. **Anomaly alerting**: Surface rano's alerts (unusual providers, unexpected destinations) in pane status

### Constraints

- **rano is a separate daemon**: Integration via subprocess CLI only
- **Privilege requirements**: rano needs elevated access on some platforms (macOS: lsof, Linux: /proc/net)
- **Feature-gated**: Behind `network-observer` feature flag
- **Graceful degradation**: Return empty data if rano not installed

### Non-Goals

- **Replacing rano**: FrankenTerm delegates; doesn't reimplement network observation
- **Running rano as a library**: rano is a standalone binary
- **Real-time packet capture**: rano polls, not streams

---

## P2: Evaluate Integration Patterns

### Option A: Subprocess CLI (Chosen)

FrankenTerm calls `rano report --pid <pid> --json` for per-pane network data.

**Pros**: Zero compile-time deps, process isolation, rano evolves independently
**Cons**: Subprocess latency, requires rano installed
**Chosen**: Only viable option for a separate daemon

### Decision: Option A — Subprocess CLI

---

## P3: Target Placement

```
frankenterm-core/
├── src/
│   ├── network_observer.rs    # NEW: rano subprocess wrapper
```

### Module Responsibilities

#### `network_observer.rs`

- `query_pane_network(pid: u32) -> NetworkAttribution` — Query rano for process connections
- `NetworkAttribution` — Provider, connection count, bytes transferred
- `is_rano_available() -> bool` — Check if rano binary exists
- Graceful degradation: Empty result if rano not installed

---

## P4: API Contract

```rust
#[cfg(feature = "network-observer")]
pub mod network_observer {
    pub struct NetworkAttribution {
        pub provider: Option<String>,
        pub connections: usize,
        pub bytes_sent: u64,
        pub bytes_received: u64,
    }

    pub struct NetworkObserver;

    impl NetworkObserver {
        pub async fn query_pid(&self, pid: u32) -> Result<Vec<NetworkAttribution>>;
        pub fn is_available(&self) -> bool;
    }
}
```

---

## P5-P8: Testing, Rollout

**No migration needed.**

**Tests**: 30+ unit tests (JSON parsing, empty results, unavailable rano, mock subprocess output).

**Rollout**: Phase 1 (network_observer.rs behind feature) → Phase 2 (wire into pane status) → Phase 3 (alert integration).

**Rollback**: Disable feature flag.

### Summary

**Subprocess CLI** wrapping `rano report --json`. One new module: `network_observer.rs`. Zero compile-time deps. Feature-gated behind `network-observer`.

---

*Plan complete.*
