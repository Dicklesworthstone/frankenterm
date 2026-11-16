# Plan to Deeply Integrate coding_agent_usage_tracker into FrankenTerm

> Integration plan for FrankenTerm bead `ft-2vuw7.28.3`
> Author: DarkMill (claude-code, opus-4.6)
> Date: 2026-02-22
> Prerequisites: COMPREHENSIVE_ANALYSIS_OF_coding_agent_usage_tracker.md (ft-2vuw7.28.2)

---

## P1: Integration Objectives, Constraints, and Non-Goals

### Objectives

1. **Rate limit awareness**: Import provider rate limits into FrankenTerm's backpressure system
2. **Cost attribution per pane**: Map agent pane activity to provider costs via caut's session tracking
3. **Credential health**: Surface credential expiry warnings in pane status indicators
4. **Usage dashboard widget**: Display provider usage data in FrankenTerm's monitoring views

### Constraints

- **caut is a separate binary**: Integration via subprocess CLI (`caut usage --json`)
- **Feature-gated**: Behind `usage-tracking` feature flag
- **Graceful degradation**: Return default data if caut not installed
- **Stable JSON schema**: caut uses `caut.v1` versioned output

### Non-Goals

- **Reimplementing usage tracking**: Delegate entirely to caut
- **Importing caut as library**: 39.7K LOC with tokio, reqwest, keyring — too heavy
- **Managing credentials**: caut handles credential storage independently
- **Real-time streaming**: caut is polled, not streaming

---

## P2: Evaluate Integration Patterns

### Option A: Subprocess CLI (Chosen)

FrankenTerm calls `caut usage --json` and `caut cost --json` for usage data.

**Pros**: Zero compile-time deps, process isolation, stable JSON schema, caut evolves independently
**Cons**: Subprocess latency (20-500ms), requires caut installed
**Chosen**: Only viable option — caut is a heavyweight binary

### Decision: Option A — Subprocess CLI

---

## P3: Target Placement

```
frankenterm-core/
├── src/
│   ├── usage_bridge.rs        # NEW: caut subprocess wrapper
```

### Module Responsibilities

#### `usage_bridge.rs`

- `query_usage(provider: &str) -> UsageSnapshot` — Rate limit status
- `query_cost(provider: &str) -> CostSnapshot` — Local cost data
- `BackpressureLevel` mapping — caut `used_percent` → Green/Yellow/Red/Black
- Graceful degradation: Return unknown level if caut not installed

### Backpressure Mapping

```
used_percent < 50%  → Green  (normal operation)
used_percent < 80%  → Yellow (reduce non-essential requests)
used_percent < 95%  → Red    (critical requests only)
used_percent >= 95% → Black  (circuit break)
```

---

## P4: API Contract

```rust
#[cfg(feature = "usage-tracking")]
pub mod usage_bridge {
    pub struct UsageSnapshot {
        pub provider: String,
        pub used_percent: f64,
        pub window_minutes: Option<i32>,
        pub resets_at: Option<String>,
    }

    pub struct CostSnapshot {
        pub provider: String,
        pub total_cost_usd: f64,
        pub period: String,
    }

    pub struct UsageBridge;

    impl UsageBridge {
        pub async fn query_usage(&self, provider: &str) -> Result<UsageSnapshot>;
        pub async fn query_cost(&self, provider: &str) -> Result<CostSnapshot>;
        pub async fn backpressure_level(&self, provider: &str) -> BackpressureLevel;
        pub fn is_available(&self) -> bool;
    }
}
```

---

## P5-P8: Testing, Rollout

**No migration needed.**

**Tests**: 30+ unit tests (JSON parsing, backpressure mapping, unavailable caut, threshold boundaries).

**Rollout**: Phase 1 (usage_bridge.rs behind feature) → Phase 2 (wire into backpressure system) → Phase 3 (dashboard widget) → Phase 4 (credential health alerts).

**Rollback**: Disable feature flag.

### Summary

**Subprocess CLI** wrapping `caut usage --json`. One new module: `usage_bridge.rs`. Zero compile-time deps. Backpressure tier mapping from rate limits. Feature-gated behind `usage-tracking`.

---

*Plan complete.*
