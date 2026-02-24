//! Fault-injection DSL for replay resilience testing (ft-og6q6.4.2).
//!
//! Provides:
//! - [`FaultSpec`] — Declarative fault specification (TOML-based).
//! - [`FaultInjector`] — Applies fault specs to event streams with seeded PRNG.
//! - [`FaultLog`] — Records all injected faults for traceability.
//! - Built-in presets: rate_limit_storm, pane_death, clock_skew, network_partition.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

// ============================================================================
// Seeded PRNG — SplitMix64 (no external deps)
// ============================================================================

/// Deterministic PRNG (SplitMix64) for reproducible fault injection.
#[derive(Debug, Clone)]
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Returns a value in [0.0, 1.0).
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ============================================================================
// Event filter
// ============================================================================

/// Predicate for matching events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFilter {
    /// Match events from this pane ID (if set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane_id: Option<String>,
    /// Match events of this kind (if set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_kind: Option<String>,
    /// Match events in this time range [start_ms, end_ms] (if set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_range_start_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_range_end_ms: Option<u64>,
    /// Match events in this sequence range [start, end] (if set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_start: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence_end: Option<u64>,
}

impl EventFilter {
    /// Create a match-all filter.
    #[must_use]
    pub fn match_all() -> Self {
        Self {
            pane_id: None,
            event_kind: None,
            time_range_start_ms: None,
            time_range_end_ms: None,
            sequence_start: None,
            sequence_end: None,
        }
    }

    /// Check if an event matches this filter.
    #[must_use]
    pub fn matches(
        &self,
        pane_id: &str,
        event_kind: &str,
        timestamp_ms: u64,
        sequence: u64,
    ) -> bool {
        if let Some(ref pid) = self.pane_id {
            if pid != pane_id {
                return false;
            }
        }
        if let Some(ref ek) = self.event_kind {
            if ek != event_kind {
                return false;
            }
        }
        if let Some(start) = self.time_range_start_ms {
            if timestamp_ms < start {
                return false;
            }
        }
        if let Some(end) = self.time_range_end_ms {
            if timestamp_ms > end {
                return false;
            }
        }
        if let Some(start) = self.sequence_start {
            if sequence < start {
                return false;
            }
        }
        if let Some(end) = self.sequence_end {
            if sequence > end {
                return false;
            }
        }
        true
    }
}

// ============================================================================
// Fault types
// ============================================================================

/// A fault to inject into the event stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FaultType {
    /// Inject delay before matching events.
    Delay {
        filter: EventFilter,
        duration_ms: u64,
    },
    /// Drop matching events with given probability.
    Drop {
        filter: EventFilter,
        probability: f64,
    },
    /// Corrupt a specific field of matching events.
    Corrupt {
        filter: EventFilter,
        field: String,
        mutation: String,
    },
    /// Shuffle events within a window.
    Reorder {
        filter: EventFilter,
        window_size: usize,
    },
    /// Duplicate matching events.
    Duplicate {
        filter: EventFilter,
        count: usize,
    },
}

impl FaultType {
    /// Get the filter for this fault.
    #[must_use]
    pub fn filter(&self) -> &EventFilter {
        match self {
            Self::Delay { filter, .. }
            | Self::Drop { filter, .. }
            | Self::Corrupt { filter, .. }
            | Self::Reorder { filter, .. }
            | Self::Duplicate { filter, .. } => filter,
        }
    }

    /// Human-readable fault type name.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Delay { .. } => "delay",
            Self::Drop { .. } => "drop",
            Self::Corrupt { .. } => "corrupt",
            Self::Reorder { .. } => "reorder",
            Self::Duplicate { .. } => "duplicate",
        }
    }
}

// ============================================================================
// FaultSpec — TOML-based fault specification
// ============================================================================

/// Complete fault specification (deserializable from TOML).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultSpec {
    /// Human-readable name.
    pub name: String,
    /// Description.
    #[serde(default)]
    pub description: String,
    /// PRNG seed for deterministic injection.
    pub seed: u64,
    /// Faults to inject.
    pub faults: Vec<FaultType>,
}

impl FaultSpec {
    /// Load from TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self, String> {
        toml::from_str(toml_str).map_err(|e| format!("fault spec parse error: {e}"))
    }

    /// Number of faults.
    #[must_use]
    pub fn fault_count(&self) -> usize {
        self.faults.len()
    }
}

// ============================================================================
// Built-in presets
// ============================================================================

/// Built-in high-value failure mode presets.
pub struct FaultPresets;

impl FaultPresets {
    /// rate_limit_storm: burst of events in tight window to test backpressure.
    #[must_use]
    pub fn rate_limit_storm(pane_id: &str, burst_count: usize, seed: u64) -> FaultSpec {
        FaultSpec {
            name: "rate_limit_storm".to_string(),
            description: format!("Inject burst of {burst_count} events for pane {pane_id}"),
            seed,
            faults: vec![FaultType::Duplicate {
                filter: EventFilter {
                    pane_id: Some(pane_id.to_string()),
                    ..EventFilter::match_all()
                },
                count: burst_count,
            }],
        }
    }

    /// pane_death: drop all events for a pane after timestamp T.
    #[must_use]
    pub fn pane_death(pane_id: &str, after_ms: u64, seed: u64) -> FaultSpec {
        FaultSpec {
            name: "pane_death".to_string(),
            description: format!("Drop all events for pane {pane_id} after {after_ms}ms"),
            seed,
            faults: vec![FaultType::Drop {
                filter: EventFilter {
                    pane_id: Some(pane_id.to_string()),
                    time_range_start_ms: Some(after_ms),
                    ..EventFilter::match_all()
                },
                probability: 1.0,
            }],
        }
    }

    /// clock_skew: inject monotonicity violation by adding negative delay.
    #[must_use]
    pub fn clock_skew(pane_id: &str, at_sequence: u64, skew_ms: u64, seed: u64) -> FaultSpec {
        FaultSpec {
            name: "clock_skew".to_string(),
            description: format!("Inject clock skew of -{skew_ms}ms at sequence {at_sequence}"),
            seed,
            faults: vec![FaultType::Corrupt {
                filter: EventFilter {
                    pane_id: Some(pane_id.to_string()),
                    sequence_start: Some(at_sequence),
                    sequence_end: Some(at_sequence),
                    ..EventFilter::match_all()
                },
                field: "timestamp_ms".to_string(),
                mutation: format!("-{skew_ms}"),
            }],
        }
    }

    /// network_partition: delay all events for a duration window.
    #[must_use]
    pub fn network_partition(
        start_ms: u64,
        end_ms: u64,
        delay_ms: u64,
        seed: u64,
    ) -> FaultSpec {
        FaultSpec {
            name: "network_partition".to_string(),
            description: format!("Delay all events by {delay_ms}ms between {start_ms}-{end_ms}ms"),
            seed,
            faults: vec![FaultType::Delay {
                filter: EventFilter {
                    time_range_start_ms: Some(start_ms),
                    time_range_end_ms: Some(end_ms),
                    ..EventFilter::match_all()
                },
                duration_ms: delay_ms,
            }],
        }
    }
}

// ============================================================================
// FaultLogEntry — records each injected fault
// ============================================================================

/// A single fault injection record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultLogEntry {
    /// Type of fault injected.
    pub fault_type: String,
    /// Event ID affected.
    pub event_id: String,
    /// Position in original stream.
    pub original_position: u64,
    /// Description of what was injected.
    pub description: String,
}

/// Accumulated fault injection log.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FaultLog {
    entries: Vec<FaultLogEntry>,
}

impl FaultLog {
    /// Create empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a fault injection.
    pub fn record(
        &mut self,
        fault_type: &str,
        event_id: &str,
        position: u64,
        description: &str,
    ) {
        self.entries.push(FaultLogEntry {
            fault_type: fault_type.to_string(),
            event_id: event_id.to_string(),
            original_position: position,
            description: description.to_string(),
        });
    }

    /// Number of faults injected.
    #[must_use]
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// Get all entries.
    #[must_use]
    pub fn entries(&self) -> &[FaultLogEntry] {
        &self.entries
    }

    /// Count by fault type.
    #[must_use]
    pub fn count_by_type(&self) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        for entry in &self.entries {
            *counts.entry(entry.fault_type.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// Export as JSONL.
    #[must_use]
    pub fn to_jsonl(&self) -> String {
        self.entries
            .iter()
            .filter_map(|e| serde_json::to_string(e).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

// ============================================================================
// Simulated event for testing injection
// ============================================================================

/// A minimal event representation for fault injection processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimEvent {
    /// Event ID.
    pub event_id: String,
    /// Pane ID.
    pub pane_id: String,
    /// Event kind.
    pub event_kind: String,
    /// Timestamp in ms.
    pub timestamp_ms: u64,
    /// Sequence number.
    pub sequence: u64,
    /// Payload data.
    pub payload: String,
}

// ============================================================================
// FaultInjector — applies fault specs to event stream
// ============================================================================

/// Applies fault specifications to an event stream with deterministic PRNG.
pub struct FaultInjector {
    spec: FaultSpec,
    rng: SplitMix64,
    log: FaultLog,
}

impl FaultInjector {
    /// Create from a fault spec.
    #[must_use]
    pub fn new(spec: FaultSpec) -> Self {
        let rng = SplitMix64::new(spec.seed);
        Self {
            spec,
            rng,
            log: FaultLog::new(),
        }
    }

    /// Process a single event through all fault specs.
    ///
    /// Returns the (possibly modified) events to emit. An empty Vec means the event was dropped.
    pub fn process(&mut self, event: SimEvent) -> Vec<SimEvent> {
        let mut results = vec![event.clone()];

        for fault in &self.spec.faults {
            if !fault.filter().matches(
                &event.pane_id,
                &event.event_kind,
                event.timestamp_ms,
                event.sequence,
            ) {
                continue;
            }

            match fault {
                FaultType::Delay { duration_ms, .. } => {
                    // Inject delay: modify timestamp.
                    for evt in &mut results {
                        evt.timestamp_ms = evt.timestamp_ms.saturating_add(*duration_ms);
                    }
                    self.log.record(
                        "delay",
                        &event.event_id,
                        event.sequence,
                        &format!("delayed by {duration_ms}ms"),
                    );
                }
                FaultType::Drop { probability, .. } => {
                    let roll = self.rng.next_f64();
                    if roll < *probability {
                        results.clear();
                        self.log.record(
                            "drop",
                            &event.event_id,
                            event.sequence,
                            &format!("dropped (p={probability}, roll={roll:.4})"),
                        );
                        return results;
                    }
                }
                FaultType::Corrupt {
                    field, mutation, ..
                } => {
                    for evt in &mut results {
                        if field == "timestamp_ms" {
                            if let Some(stripped) = mutation.strip_prefix('-') {
                                if let Ok(delta) = stripped.parse::<u64>() {
                                    evt.timestamp_ms = evt.timestamp_ms.saturating_sub(delta);
                                }
                            } else if let Some(stripped) = mutation.strip_prefix('+') {
                                if let Ok(delta) = stripped.parse::<u64>() {
                                    evt.timestamp_ms = evt.timestamp_ms.saturating_add(delta);
                                }
                            }
                        } else if field == "payload" {
                            evt.payload = mutation.clone();
                        }
                    }
                    self.log.record(
                        "corrupt",
                        &event.event_id,
                        event.sequence,
                        &format!("corrupted {field} with {mutation}"),
                    );
                }
                FaultType::Reorder { window_size, .. } => {
                    // For single-event processing, reorder is a no-op.
                    // In batch mode, the caller would use process_batch.
                    self.log.record(
                        "reorder",
                        &event.event_id,
                        event.sequence,
                        &format!("reorder window={window_size} (single event no-op)"),
                    );
                }
                FaultType::Duplicate { count, .. } => {
                    let mut copies = Vec::new();
                    for i in 0..*count {
                        let mut copy = event.clone();
                        copy.event_id = format!("{}_dup_{i}", event.event_id);
                        copies.push(copy);
                    }
                    self.log.record(
                        "duplicate",
                        &event.event_id,
                        event.sequence,
                        &format!("duplicated {count} times"),
                    );
                    results.extend(copies);
                }
            }
        }

        results
    }

    /// Process a batch of events, applying reorder faults.
    pub fn process_batch(&mut self, events: Vec<SimEvent>) -> Vec<SimEvent> {
        let mut output = Vec::new();

        // First, check for reorder faults.
        let reorder_window = self.spec.faults.iter().find_map(|f| match f {
            FaultType::Reorder {
                filter,
                window_size,
            } => Some((filter.clone(), *window_size)),
            _ => None,
        });

        if let Some((filter, window_size)) = reorder_window {
            let mut buffer = Vec::new();
            for event in events {
                let matches = filter.matches(
                    &event.pane_id,
                    &event.event_kind,
                    event.timestamp_ms,
                    event.sequence,
                );
                if matches {
                    buffer.push(event);
                    if buffer.len() >= window_size {
                        self.shuffle_buffer(&mut buffer);
                        for evt in buffer.drain(..) {
                            let processed = self.process(evt);
                            output.extend(processed);
                        }
                    }
                } else {
                    // Flush buffer before non-matching event.
                    if !buffer.is_empty() {
                        self.shuffle_buffer(&mut buffer);
                        for evt in buffer.drain(..) {
                            let processed = self.process(evt);
                            output.extend(processed);
                        }
                    }
                    let processed = self.process(event);
                    output.extend(processed);
                }
            }
            // Flush remaining buffer.
            if !buffer.is_empty() {
                self.shuffle_buffer(&mut buffer);
                for evt in buffer.drain(..) {
                    let processed = self.process(evt);
                    output.extend(processed);
                }
            }
        } else {
            // No reorder: process events individually.
            for event in events {
                let processed = self.process(event);
                output.extend(processed);
            }
        }

        output
    }

    /// Fisher-Yates shuffle using seeded PRNG.
    fn shuffle_buffer(&mut self, buffer: &mut [SimEvent]) {
        let n = buffer.len();
        for i in (1..n).rev() {
            let j = (self.rng.next_u64() as usize) % (i + 1);
            buffer.swap(i, j);
        }
    }

    /// Get the fault log.
    #[must_use]
    pub fn log(&self) -> &FaultLog {
        &self.log
    }

    /// Consume and return the fault log.
    #[must_use]
    pub fn into_log(self) -> FaultLog {
        self.log
    }

    /// Get the spec name.
    #[must_use]
    pub fn spec_name(&self) -> &str {
        &self.spec.name
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(id: &str, pane: &str, kind: &str, ts: u64, seq: u64) -> SimEvent {
        SimEvent {
            event_id: id.to_string(),
            pane_id: pane.to_string(),
            event_kind: kind.to_string(),
            timestamp_ms: ts,
            sequence: seq,
            payload: format!("payload_{id}"),
        }
    }

    // ── EventFilter ─────────────────────────────────────────────────────

    #[test]
    fn filter_match_all() {
        let f = EventFilter::match_all();
        assert!(f.matches("p1", "data", 100, 0));
        assert!(f.matches("p2", "ctrl", 0, 999));
    }

    #[test]
    fn filter_pane_id() {
        let f = EventFilter {
            pane_id: Some("p1".into()),
            ..EventFilter::match_all()
        };
        assert!(f.matches("p1", "data", 100, 0));
        assert!(!f.matches("p2", "data", 100, 0));
    }

    #[test]
    fn filter_event_kind() {
        let f = EventFilter {
            event_kind: Some("data".into()),
            ..EventFilter::match_all()
        };
        assert!(f.matches("p1", "data", 100, 0));
        assert!(!f.matches("p1", "ctrl", 100, 0));
    }

    #[test]
    fn filter_time_range() {
        let f = EventFilter {
            time_range_start_ms: Some(100),
            time_range_end_ms: Some(200),
            ..EventFilter::match_all()
        };
        assert!(f.matches("p1", "d", 100, 0));
        assert!(f.matches("p1", "d", 150, 0));
        assert!(f.matches("p1", "d", 200, 0));
        assert!(!f.matches("p1", "d", 99, 0));
        assert!(!f.matches("p1", "d", 201, 0));
    }

    #[test]
    fn filter_sequence_range() {
        let f = EventFilter {
            sequence_start: Some(5),
            sequence_end: Some(10),
            ..EventFilter::match_all()
        };
        assert!(f.matches("p1", "d", 0, 5));
        assert!(f.matches("p1", "d", 0, 10));
        assert!(!f.matches("p1", "d", 0, 4));
        assert!(!f.matches("p1", "d", 0, 11));
    }

    #[test]
    fn filter_combined() {
        let f = EventFilter {
            pane_id: Some("p1".into()),
            event_kind: Some("data".into()),
            time_range_start_ms: Some(100),
            time_range_end_ms: Some(200),
            sequence_start: None,
            sequence_end: None,
        };
        assert!(f.matches("p1", "data", 150, 0));
        assert!(!f.matches("p2", "data", 150, 0));
        assert!(!f.matches("p1", "ctrl", 150, 0));
        assert!(!f.matches("p1", "data", 50, 0));
    }

    // ── FaultSpec parsing ───────────────────────────────────────────────

    #[test]
    fn parse_delay_fault() {
        let toml = r#"
name = "delay-test"
seed = 42

[[faults]]
type = "delay"
duration_ms = 500
[faults.filter]
pane_id = "p1"
"#;
        let spec = FaultSpec::from_toml(toml).unwrap();
        assert_eq!(spec.name, "delay-test");
        assert_eq!(spec.seed, 42);
        assert_eq!(spec.fault_count(), 1);
        assert_eq!(spec.faults[0].type_name(), "delay");
    }

    #[test]
    fn parse_drop_fault() {
        let toml = r#"
name = "drop-test"
seed = 123

[[faults]]
type = "drop"
probability = 0.5
[faults.filter]
event_kind = "data"
"#;
        let spec = FaultSpec::from_toml(toml).unwrap();
        assert_eq!(spec.faults[0].type_name(), "drop");
    }

    #[test]
    fn parse_corrupt_fault() {
        let toml = r#"
name = "corrupt-test"
seed = 0

[[faults]]
type = "corrupt"
field = "timestamp_ms"
mutation = "-1000"
[faults.filter]
"#;
        let spec = FaultSpec::from_toml(toml).unwrap();
        assert_eq!(spec.faults[0].type_name(), "corrupt");
    }

    #[test]
    fn parse_reorder_fault() {
        let toml = r#"
name = "reorder-test"
seed = 7

[[faults]]
type = "reorder"
window_size = 5
[faults.filter]
"#;
        let spec = FaultSpec::from_toml(toml).unwrap();
        assert_eq!(spec.faults[0].type_name(), "reorder");
    }

    #[test]
    fn parse_duplicate_fault() {
        let toml = r#"
name = "dup-test"
seed = 99

[[faults]]
type = "duplicate"
count = 3
[faults.filter]
pane_id = "p1"
"#;
        let spec = FaultSpec::from_toml(toml).unwrap();
        assert_eq!(spec.faults[0].type_name(), "duplicate");
    }

    #[test]
    fn parse_invalid_toml() {
        let result = FaultSpec::from_toml("not valid {{{");
        assert!(result.is_err());
    }

    // ── FaultInjector ───────────────────────────────────────────────────

    #[test]
    fn delay_injects_duration() {
        let spec = FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 42,
            faults: vec![FaultType::Delay {
                filter: EventFilter::match_all(),
                duration_ms: 500,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", 1000, 0);
        let result = inj.process(evt);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].timestamp_ms, 1500);
        assert_eq!(inj.log().count(), 1);
    }

    #[test]
    fn drop_probabilistic() {
        let spec = FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 42,
            faults: vec![FaultType::Drop {
                filter: EventFilter::match_all(),
                probability: 0.5,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let mut dropped = 0;
        let n = 1000;
        for i in 0..n {
            let evt = make_event(&format!("e{i}"), "p1", "data", i * 10, i);
            let result = inj.process(evt);
            if result.is_empty() {
                dropped += 1;
            }
        }
        // With p=0.5 and 1000 events, expect ~500 drops (allow wide margin).
        assert!(dropped > 300, "expected >300 drops, got {dropped}");
        assert!(dropped < 700, "expected <700 drops, got {dropped}");
    }

    #[test]
    fn drop_probability_zero_keeps_all() {
        let spec = FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 42,
            faults: vec![FaultType::Drop {
                filter: EventFilter::match_all(),
                probability: 0.0,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        for i in 0..100u64 {
            let evt = make_event(&format!("e{i}"), "p1", "data", i * 10, i);
            let result = inj.process(evt);
            assert_eq!(result.len(), 1);
        }
    }

    #[test]
    fn drop_probability_one_drops_all() {
        let spec = FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 42,
            faults: vec![FaultType::Drop {
                filter: EventFilter::match_all(),
                probability: 1.0,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        for i in 0..50u64 {
            let evt = make_event(&format!("e{i}"), "p1", "data", i * 10, i);
            let result = inj.process(evt);
            assert!(result.is_empty());
        }
    }

    #[test]
    fn corrupt_timestamp_subtract() {
        let spec = FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Corrupt {
                filter: EventFilter::match_all(),
                field: "timestamp_ms".into(),
                mutation: "-500".into(),
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", 1000, 0);
        let result = inj.process(evt);
        assert_eq!(result[0].timestamp_ms, 500);
    }

    #[test]
    fn corrupt_payload() {
        let spec = FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Corrupt {
                filter: EventFilter::match_all(),
                field: "payload".into(),
                mutation: "corrupted!".into(),
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", 100, 0);
        let result = inj.process(evt);
        assert_eq!(result[0].payload, "corrupted!");
    }

    #[test]
    fn duplicate_creates_copies() {
        let spec = FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Duplicate {
                filter: EventFilter::match_all(),
                count: 3,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", 100, 0);
        let result = inj.process(evt);
        assert_eq!(result.len(), 4); // original + 3 copies
        assert_eq!(result[1].event_id, "e1_dup_0");
        assert_eq!(result[2].event_id, "e1_dup_1");
        assert_eq!(result[3].event_id, "e1_dup_2");
    }

    #[test]
    fn filter_restricts_injection() {
        let spec = FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 0,
            faults: vec![FaultType::Delay {
                filter: EventFilter {
                    pane_id: Some("target".into()),
                    ..EventFilter::match_all()
                },
                duration_ms: 500,
            }],
        };
        let mut inj = FaultInjector::new(spec);
        // Non-matching event unchanged.
        let evt1 = make_event("e1", "other", "data", 1000, 0);
        let r1 = inj.process(evt1);
        assert_eq!(r1[0].timestamp_ms, 1000);
        // Matching event delayed.
        let evt2 = make_event("e2", "target", "data", 1000, 1);
        let r2 = inj.process(evt2);
        assert_eq!(r2[0].timestamp_ms, 1500);
    }

    #[test]
    fn seeded_prng_deterministic() {
        let make_spec = || FaultSpec {
            name: "test".into(),
            description: String::new(),
            seed: 12345,
            faults: vec![FaultType::Drop {
                filter: EventFilter::match_all(),
                probability: 0.5,
            }],
        };

        let events: Vec<_> = (0..100u64)
            .map(|i| make_event(&format!("e{i}"), "p1", "data", i * 10, i))
            .collect();

        let mut inj1 = FaultInjector::new(make_spec());
        let mut inj2 = FaultInjector::new(make_spec());

        for evt in &events {
            let r1 = inj1.process(evt.clone());
            let r2 = inj2.process(evt.clone());
            assert_eq!(r1.len(), r2.len(), "determinism broken at {}", evt.event_id);
        }
    }

    #[test]
    fn empty_spec_no_modifications() {
        let spec = FaultSpec {
            name: "empty".into(),
            description: String::new(),
            seed: 0,
            faults: vec![],
        };
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", 100, 0);
        let result = inj.process(evt.clone());
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].timestamp_ms, 100);
        assert_eq!(inj.log().count(), 0);
    }

    // ── Presets ──────────────────────────────────────────────────────────

    #[test]
    fn preset_rate_limit_storm() {
        let spec = FaultPresets::rate_limit_storm("p1", 5, 42);
        assert_eq!(spec.name, "rate_limit_storm");
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", 100, 0);
        let result = inj.process(evt);
        assert_eq!(result.len(), 6); // 1 original + 5 copies
    }

    #[test]
    fn preset_pane_death() {
        let spec = FaultPresets::pane_death("p1", 500, 42);
        assert_eq!(spec.name, "pane_death");
        let mut inj = FaultInjector::new(spec);
        // Before cutoff: kept.
        let e1 = make_event("e1", "p1", "data", 400, 0);
        assert_eq!(inj.process(e1).len(), 1);
        // After cutoff: dropped.
        let e2 = make_event("e2", "p1", "data", 600, 1);
        assert!(inj.process(e2).is_empty());
        // Other pane: kept regardless.
        let e3 = make_event("e3", "p2", "data", 600, 2);
        assert_eq!(inj.process(e3).len(), 1);
    }

    #[test]
    fn preset_clock_skew() {
        let spec = FaultPresets::clock_skew("p1", 5, 200, 42);
        let mut inj = FaultInjector::new(spec);
        let evt = make_event("e1", "p1", "data", 1000, 5);
        let result = inj.process(evt);
        assert_eq!(result[0].timestamp_ms, 800); // 1000 - 200
    }

    #[test]
    fn preset_network_partition() {
        let spec = FaultPresets::network_partition(100, 500, 5000, 42);
        let mut inj = FaultInjector::new(spec);
        // In window: delayed.
        let e1 = make_event("e1", "p1", "data", 200, 0);
        assert_eq!(inj.process(e1)[0].timestamp_ms, 5200);
        // Out of window: not delayed.
        let e2 = make_event("e2", "p1", "data", 600, 1);
        assert_eq!(inj.process(e2)[0].timestamp_ms, 600);
    }

    // ── FaultLog ────────────────────────────────────────────────────────

    #[test]
    fn fault_log_tracks() {
        let mut log = FaultLog::new();
        log.record("delay", "e1", 0, "delayed 500ms");
        log.record("drop", "e2", 1, "dropped");
        assert_eq!(log.count(), 2);
        let counts = log.count_by_type();
        assert_eq!(counts.get("delay"), Some(&1));
        assert_eq!(counts.get("drop"), Some(&1));
    }

    #[test]
    fn fault_log_jsonl() {
        let mut log = FaultLog::new();
        log.record("delay", "e1", 0, "test");
        let jsonl = log.to_jsonl();
        assert!(jsonl.contains("delay"));
        assert!(jsonl.contains("e1"));
    }

    // ── Batch processing (reorder) ──────────────────────────────────────

    #[test]
    fn batch_reorder_shuffles() {
        let spec = FaultSpec {
            name: "reorder".into(),
            description: String::new(),
            seed: 42,
            faults: vec![FaultType::Reorder {
                filter: EventFilter::match_all(),
                window_size: 5,
            }],
        };
        let events: Vec<_> = (0..10u64)
            .map(|i| make_event(&format!("e{i}"), "p1", "data", i * 100, i))
            .collect();
        let mut inj = FaultInjector::new(spec);
        let result = inj.process_batch(events.clone());
        // Same count.
        assert_eq!(result.len(), events.len());
        // Order may differ (with high probability for seed=42).
        let original_ids: Vec<_> = events.iter().map(|e| e.event_id.clone()).collect();
        let result_ids: Vec<_> = result.iter().map(|e| e.event_id.clone()).collect();
        // At least one element should be out of order (probabilistic, but very likely).
        let same_order = original_ids == result_ids;
        // This could theoretically pass if shuffle is identity, but probability is ~1/120 per window.
        if same_order {
            // Re-run with different seed to confirm.
            let spec2 = FaultSpec {
                name: "reorder2".into(),
                description: String::new(),
                seed: 999,
                faults: vec![FaultType::Reorder {
                    filter: EventFilter::match_all(),
                    window_size: 5,
                }],
            };
            let mut inj2 = FaultInjector::new(spec2);
            let result2 = inj2.process_batch(events);
            let result_ids2: Vec<_> = result2.iter().map(|e| e.event_id.clone()).collect();
            assert_ne!(original_ids, result_ids2, "reorder should shuffle events");
        }
    }

    // ── Serde roundtrips ────────────────────────────────────────────────

    #[test]
    fn fault_spec_serde() {
        let spec = FaultPresets::rate_limit_storm("p1", 10, 42);
        let json = serde_json::to_string(&spec).unwrap();
        let restored: FaultSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, spec.name);
        assert_eq!(restored.seed, spec.seed);
    }

    #[test]
    fn event_filter_serde() {
        let f = EventFilter {
            pane_id: Some("p1".into()),
            event_kind: Some("data".into()),
            time_range_start_ms: Some(100),
            time_range_end_ms: Some(200),
            sequence_start: None,
            sequence_end: None,
        };
        let json = serde_json::to_string(&f).unwrap();
        let restored: EventFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.pane_id, f.pane_id);
    }

    #[test]
    fn fault_log_entry_serde() {
        let entry = FaultLogEntry {
            fault_type: "delay".into(),
            event_id: "e1".into(),
            original_position: 42,
            description: "test".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let restored: FaultLogEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.fault_type, "delay");
    }

    #[test]
    fn sim_event_serde() {
        let evt = make_event("e1", "p1", "data", 100, 0);
        let json = serde_json::to_string(&evt).unwrap();
        let restored: SimEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.event_id, "e1");
    }
}
