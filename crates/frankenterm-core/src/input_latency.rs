//! Legacy proxy-only input-latency measurement framework (ft-1memj.25).
//!
//! This module records synthetic or explicitly instrumented timestamps shaped
//! like the GUI input pipeline:
//!
//! ```text
//! KeyEvent → PtyWrite → PtyRead → TermUpdate → RenderSubmit → GpuPresent
//! ```
//!
//! It is not wired into the production AppKit/mux/PTY/presentation path and
//! therefore cannot establish production input-to-present latency. Reports
//! emitted here are permanently classified as [`InputLatencyEvidenceClass::ProxyOnly`].
//!
//! Each timestamp carries caller-supplied producer and monotonic-clock labels.
//! Durations are admitted only when every required stage is present, adjacent
//! timestamps assert the same clock domain, and timestamps do not regress.
//! Cross-domain latency requires the trace-v2 calibration contract; this legacy
//! proxy refuses to guess it.
//!
//! # Design Principles
//!
//! - **Fail-closed evidence**: Empty, incomplete, ambiguous, or exhausted
//!   collectors cannot pass a budget.
//! - **Deterministic percentiles**: Uses the nearest-rank method (no interpolation).
//! - **Explicit labels**: Every timestamp carries caller-supplied producer and
//!   clock-domain IDs; retained bundles must bind them externally.
//! - **Budget algebra**: Per-stage and aggregate budgets are enforced together.
//! - **Honest overhead**: The legacy `BTreeMap` representation may allocate;
//!   its benchmark measures proxy-framework overhead, not the production path.

use serde::{Deserialize, Deserializer, Serialize, de};
use std::collections::{BTreeMap, BTreeSet, VecDeque, btree_map::Entry};
use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroU64;

/// A map decoder that rejects duplicate wire keys instead of silently keeping
/// the last value. `BTreeMap`'s ordinary `Deserialize` implementation cannot
/// preserve evidence that a duplicate key was present, so authority-bearing
/// maps must cross this adapter before becoming ordinary maps.
struct DuplicateRejectingMap<K, V>(BTreeMap<K, V>);

impl<'de, K, V> Deserialize<'de> for DuplicateRejectingMap<K, V>
where
    K: Deserialize<'de> + Ord + fmt::Display,
    V: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct DuplicateRejectingMapVisitor<K, V>(PhantomData<(K, V)>);

        impl<'de, K, V> de::Visitor<'de> for DuplicateRejectingMapVisitor<K, V>
        where
            K: Deserialize<'de> + Ord + fmt::Display,
            V: Deserialize<'de>,
        {
            type Value = DuplicateRejectingMap<K, V>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a map with unique keys")
            }

            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut values = BTreeMap::new();
                while let Some((key, value)) = access.next_entry()? {
                    match values.entry(key) {
                        Entry::Vacant(entry) => {
                            entry.insert(value);
                        }
                        Entry::Occupied(entry) => {
                            return Err(de::Error::custom(format_args!(
                                "duplicate map key {}",
                                entry.key()
                            )));
                        }
                    }
                }
                Ok(DuplicateRejectingMap(values))
            }
        }

        deserializer.deserialize_map(DuplicateRejectingMapVisitor(PhantomData))
    }
}

/// A sequence decoder that never allocates or retains more than `MAX` items.
struct BoundedVec<T, const MAX: usize>(Vec<T>);

impl<'de, T, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX>
where
    T: Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BoundedVecVisitor<T, const MAX: usize>(PhantomData<T>);

        impl<'de, T, const MAX: usize> de::Visitor<'de> for BoundedVecVisitor<T, MAX>
        where
            T: Deserialize<'de>,
        {
            type Value = BoundedVec<T, MAX>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "a sequence containing at most {MAX} items")
            }

            fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                if let Some(length) = access.size_hint()
                    && length > MAX
                {
                    return Err(de::Error::invalid_length(length, &self));
                }

                let initial_capacity = access.size_hint().unwrap_or(0).min(MAX);
                let mut values = Vec::with_capacity(initial_capacity);
                while values.len() < MAX {
                    let Some(value) = access.next_element()? else {
                        return Ok(BoundedVec(values));
                    };
                    values.push(value);
                }

                // Probe for one additional element without deserializing it as
                // `T`; an adversarial oversize element may itself own an
                // allocation much larger than this container's bound.
                if access.next_element::<de::IgnoredAny>()?.is_some() {
                    return Err(de::Error::invalid_length(
                        MAX.saturating_add(1),
                        &self,
                    ));
                }
                Ok(BoundedVec(values))
            }
        }

        deserializer.deserialize_seq(BoundedVecVisitor(PhantomData))
    }
}

/// Schema version for serialized legacy input-latency reports.
pub const INPUT_LATENCY_REPORT_SCHEMA_VERSION: u32 = 4;

/// Schema version for serialized legacy input-latency collectors.
pub const INPUT_LATENCY_COLLECTOR_SCHEMA_VERSION: u32 = 1;

/// Schema version for standalone serialized legacy budget verdicts.
pub const INPUT_LATENCY_BUDGET_CHECK_SCHEMA_VERSION: u32 = 2;

/// Maximum samples retained or decoded by the legacy proxy collector.
pub const MAX_INPUT_LATENCY_EVIDENCE_WINDOW: usize = 65_536;

/// Maximum adjacent stage intervals that can have distinct budgets.
pub const MAX_INPUT_LATENCY_STAGE_BUDGETS: usize = InputLatencyStage::ALL.len() - 1;

/// Authority class carried by every report and budget verdict from this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputLatencyEvidenceClass {
    /// Offline/synthetic regression proxy; never production input-to-present proof.
    ProxyOnly,
}

/// Caller-supplied producer label within one synthetic evidence bundle.
///
/// This module does not own a producer registry and cannot independently bind
/// the label to a host, process, build, or boot/session. A retained bundle must
/// provide that external registry; without it the label remains unverified
/// proxy metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputLatencyProducerId(NonZeroU64);

impl InputLatencyProducerId {
    /// Construct a producer ID, reserving zero for missing/invalid provenance.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Return the encoded non-zero producer ID.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Caller-supplied clock-domain label within one synthetic evidence bundle.
///
/// Equal labels assert that timestamps share one subtraction-safe epoch and
/// rate; this module does not calibrate or independently prove that assertion.
/// Different labels are never subtracted by this legacy framework.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct InputLatencyClockDomainId(NonZeroU64);

impl InputLatencyClockDomainId {
    /// Construct a clock-domain ID, reserving zero for missing/invalid identity.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Return the encoded non-zero clock-domain ID.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// One producer- and clock-labelled synthetic timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputLatencyTimestamp {
    /// Timestamp in microseconds within `clock_domain_id`.
    pub timestamp_us: u64,
    /// Caller label for the producer; an external registry must bind it.
    pub producer_id: InputLatencyProducerId,
    /// Caller assertion of a subtraction-safe monotonic clock domain.
    pub clock_domain_id: InputLatencyClockDomainId,
}

impl InputLatencyTimestamp {
    /// Construct a producer- and clock-labelled timestamp.
    #[must_use]
    pub const fn new(
        timestamp_us: u64,
        producer_id: InputLatencyProducerId,
        clock_domain_id: InputLatencyClockDomainId,
    ) -> Self {
        Self {
            timestamp_us,
            producer_id,
            clock_domain_id,
        }
    }
}

// ── Stage Definitions ────────────────────────────────────────────────────────

/// Stages in the legacy proxy model, shaped like an input-to-presentation path.
///
/// They are ordered by synthetic pipeline position. The final marker is not a
/// measured display scanout or photon boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputLatencyStage {
    /// Key event received from the OS/window system.
    KeyEvent,
    /// Key event encoded and written to the PTY master fd.
    PtyWrite,
    /// Response bytes read from the PTY master/reader side.
    PtyRead,
    /// Terminal state machine updated (cell grid, cursor, attributes).
    TermUpdate,
    /// Render command buffer submitted to GPU API (wgpu/Metal).
    RenderSubmit,
    /// Caller-recorded completion marker for a GPU present operation.
    GpuPresent,
}

impl InputLatencyStage {
    /// All stages in pipeline order.
    pub const ALL: &'static [Self] = &[
        Self::KeyEvent,
        Self::PtyWrite,
        Self::PtyRead,
        Self::TermUpdate,
        Self::RenderSubmit,
        Self::GpuPresent,
    ];

    /// Human-readable label for this stage.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::KeyEvent => "key_event",
            Self::PtyWrite => "pty_write",
            Self::PtyRead => "pty_read",
            Self::TermUpdate => "term_update",
            Self::RenderSubmit => "render_submit",
            Self::GpuPresent => "gpu_present",
        }
    }

    /// Previous stage whose duration feeds this stage's per-stage budget.
    #[must_use]
    pub const fn predecessor(self) -> Option<Self> {
        match self {
            Self::KeyEvent => None,
            Self::PtyWrite => Some(Self::KeyEvent),
            Self::PtyRead => Some(Self::PtyWrite),
            Self::TermUpdate => Some(Self::PtyRead),
            Self::RenderSubmit => Some(Self::TermUpdate),
            Self::GpuPresent => Some(Self::RenderSubmit),
        }
    }
}

impl std::fmt::Display for InputLatencyStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ── Single Measurement ──────────────────────────────────────────────────────

/// Why a single measurement cannot be admitted as latency evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputLatencyMeasurementError {
    /// A stage write attempted to replace already-recorded evidence.
    #[error("stage {stage} was recorded more than once")]
    DuplicateStage { stage: InputLatencyStage },
    /// Complete proxy evidence requires all six declared stages.
    #[error("required stage {stage} is missing")]
    MissingStage { stage: InputLatencyStage },
    /// Bare subtraction across clock domains would fabricate latency.
    #[error("clock domain changes between {from} and {to}")]
    ClockDomainMismatch {
        from: InputLatencyStage,
        to: InputLatencyStage,
        from_clock_domain_id: InputLatencyClockDomainId,
        to_clock_domain_id: InputLatencyClockDomainId,
    },
    /// A later pipeline stage cannot precede an earlier stage in one domain.
    #[error("timestamp regresses between {from} and {to}")]
    TimestampRegression {
        from: InputLatencyStage,
        to: InputLatencyStage,
        from_timestamp_us: u64,
        to_timestamp_us: u64,
    },
}

/// A single proxy input-latency measurement.
///
/// Each stage retains caller-supplied producer and clock labels. Partial
/// measurements are useful diagnostics, but are explicit non-pass evidence.
#[derive(Debug, Clone, Serialize)]
pub struct InputLatencyMeasurement {
    /// Monotonic measurement ID.
    pub id: u64,
    /// Caller-labelled timestamp at each recorded stage.
    stages: BTreeMap<InputLatencyStage, InputLatencyTimestamp>,
    /// First recording fault. Once tainted, a measurement cannot become valid.
    #[serde(skip_serializing_if = "Option::is_none")]
    recording_fault: Option<InputLatencyMeasurementError>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputLatencyMeasurementWire {
    id: u64,
    stages: DuplicateRejectingMap<InputLatencyStage, InputLatencyTimestamp>,
    #[serde(default)]
    recording_fault: Option<InputLatencyMeasurementError>,
}

impl<'de> Deserialize<'de> for InputLatencyMeasurement {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InputLatencyMeasurementWire::deserialize(deserializer)?;
        Ok(Self {
            id: wire.id,
            stages: wire.stages.0,
            recording_fault: wire.recording_fault,
        })
    }
}

impl InputLatencyMeasurement {
    /// Create a new measurement with the given ID.
    #[must_use]
    pub fn new(id: u64) -> Self {
        Self {
            id,
            stages: BTreeMap::new(),
            recording_fault: None,
        }
    }

    /// Construct a measurement from an already-unique in-memory stage map.
    ///
    /// Wire callers must use `Deserialize`, whose duplicate-rejecting map sees
    /// duplicate keys before a `BTreeMap` could collapse them. The result still
    /// passes through [`Self::validate_complete`] before any duration or
    /// percentile can be computed.
    #[cfg(test)]
    #[must_use]
    fn from_stages(
        id: u64,
        stages: BTreeMap<InputLatencyStage, InputLatencyTimestamp>,
    ) -> Self {
        Self {
            id,
            stages,
            recording_fault: None,
        }
    }

    /// Record a stage exactly once.
    ///
    /// A duplicate does not overwrite the original timestamp and permanently
    /// taints the measurement so ignoring the returned error cannot mint a pass.
    pub fn record_stage(
        &mut self,
        stage: InputLatencyStage,
        timestamp: InputLatencyTimestamp,
    ) -> Result<(), InputLatencyMeasurementError> {
        if let Some(error) = &self.recording_fault {
            return Err(error.clone());
        }
        match self.stages.entry(stage) {
            Entry::Vacant(entry) => {
                entry.insert(timestamp);
                Ok(())
            }
            Entry::Occupied(_) => {
                let error = InputLatencyMeasurementError::DuplicateStage { stage };
                self.recording_fault = Some(error.clone());
                Err(error)
            }
        }
    }

    /// Read the recorded stage map without permitting replacement.
    #[must_use]
    pub fn stages(&self) -> &BTreeMap<InputLatencyStage, InputLatencyTimestamp> {
        &self.stages
    }

    /// Return one stage timestamp, if recorded.
    #[must_use]
    pub fn stage_timestamp(&self, stage: InputLatencyStage) -> Option<InputLatencyTimestamp> {
        self.stages.get(&stage).copied()
    }

    fn required_timestamp(
        &self,
        stage: InputLatencyStage,
    ) -> Result<&InputLatencyTimestamp, InputLatencyMeasurementError> {
        self.stages
            .get(&stage)
            .ok_or(InputLatencyMeasurementError::MissingStage { stage })
    }

    /// Validate completeness, asserted clock-label consistency, and numeric
    /// pipeline monotonicity.
    pub fn validate_complete(&self) -> Result<(), InputLatencyMeasurementError> {
        if let Some(error) = &self.recording_fault {
            return Err(error.clone());
        }

        for pair in InputLatencyStage::ALL.windows(2) {
            let from = pair[0];
            let to = pair[1];
            let from_timestamp = self.required_timestamp(from)?;
            let to_timestamp = self.required_timestamp(to)?;

            if from_timestamp.clock_domain_id != to_timestamp.clock_domain_id {
                return Err(InputLatencyMeasurementError::ClockDomainMismatch {
                    from,
                    to,
                    from_clock_domain_id: from_timestamp.clock_domain_id,
                    to_clock_domain_id: to_timestamp.clock_domain_id,
                });
            }
            if to_timestamp.timestamp_us < from_timestamp.timestamp_us {
                return Err(InputLatencyMeasurementError::TimestampRegression {
                    from,
                    to,
                    from_timestamp_us: from_timestamp.timestamp_us,
                    to_timestamp_us: to_timestamp.timestamp_us,
                });
            }
        }

        Ok(())
    }

    /// Total end-to-end latency in microseconds (first stage to last stage).
    ///
    /// All stages must be present and comparable. Zero microseconds is valid at
    /// this clock's resolution; a regression is not.
    pub fn total_latency_us(&self) -> Result<u64, InputLatencyMeasurementError> {
        self.validate_complete()?;
        let first = self.required_timestamp(InputLatencyStage::KeyEvent)?;
        let last = self.required_timestamp(InputLatencyStage::GpuPresent)?;
        last.timestamp_us.checked_sub(first.timestamp_us).ok_or(
            InputLatencyMeasurementError::TimestampRegression {
                from: InputLatencyStage::KeyEvent,
                to: InputLatencyStage::GpuPresent,
                from_timestamp_us: first.timestamp_us,
                to_timestamp_us: last.timestamp_us,
            },
        )
    }

    /// Latency between two specific stages in microseconds.
    ///
    /// The entire measurement must be complete and monotonic so callers cannot
    /// silently compute a passing percentile from a partial sample.
    pub fn stage_latency_us(
        &self,
        from: InputLatencyStage,
        to: InputLatencyStage,
    ) -> Result<u64, InputLatencyMeasurementError> {
        self.validate_complete()?;
        let from_timestamp = self.required_timestamp(from)?;
        let to_timestamp = self.required_timestamp(to)?;

        if from_timestamp.clock_domain_id != to_timestamp.clock_domain_id {
            return Err(InputLatencyMeasurementError::ClockDomainMismatch {
                from,
                to,
                from_clock_domain_id: from_timestamp.clock_domain_id,
                to_clock_domain_id: to_timestamp.clock_domain_id,
            });
        }
        to_timestamp.timestamp_us.checked_sub(from_timestamp.timestamp_us).ok_or(
            InputLatencyMeasurementError::TimestampRegression {
                from,
                to,
                from_timestamp_us: from_timestamp.timestamp_us,
                to_timestamp_us: to_timestamp.timestamp_us,
            },
        )
    }

    /// Number of stages recorded.
    #[must_use]
    pub fn stage_count(&self) -> usize {
        self.stages.len()
    }
}

// ── Percentile Computation ──────────────────────────────────────────────────

/// Percentile targets for latency reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Percentile {
    P50,
    P95,
    P99,
    P999,
}

impl Percentile {
    /// The fraction this percentile represents (0.0–1.0).
    #[must_use]
    pub fn fraction(self) -> f64 {
        match self {
            Self::P50 => 0.50,
            Self::P95 => 0.95,
            Self::P99 => 0.99,
            Self::P999 => 0.999,
        }
    }

    /// All standard percentiles.
    pub const ALL: &'static [Self] = &[Self::P50, Self::P95, Self::P99, Self::P999];
}

impl std::fmt::Display for Percentile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::P50 => f.write_str("p50"),
            Self::P95 => f.write_str("p95"),
            Self::P99 => f.write_str("p99"),
            Self::P999 => f.write_str("p999"),
        }
    }
}

/// Compute the percentile value from a sorted slice using nearest-rank method.
///
/// Returns `None` if the slice is empty.
#[must_use]
pub fn percentile_nearest_rank(sorted_values: &[u64], percentile: Percentile) -> Option<u64> {
    if sorted_values.is_empty() {
        return None;
    }
    let n = sorted_values.len();
    let rank = (percentile.fraction() * n as f64).ceil() as usize;
    let idx = rank.min(n).saturating_sub(1);
    Some(sorted_values[idx])
}

// ── Latency Collector ───────────────────────────────────────────────────────

/// Failure to allocate a fresh measurement identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputLatencyCollectorError {
    /// Zero and `u64::MAX` are reserved; a new collector/run is required.
    #[error("measurement ID space is exhausted; start a new collector identity")]
    MeasurementIdExhausted,
}

/// Why a collector cannot serve as budget authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputLatencyEvidenceError {
    /// No samples is not a zero-latency sample set.
    #[error("collector contains no measurements")]
    EmptyCollector,
    /// Allocation crossed the reserved terminal identity boundary.
    #[error("measurement ID space was exhausted")]
    MeasurementIdExhausted,
    /// Zero and `u64::MAX` cannot identify evidence samples.
    #[error("measurement uses reserved ID {id}")]
    ReservedMeasurementId { id: u64 },
    /// IDs must remain unique within the retained evidence window.
    #[error("measurement ID {id} appears more than once")]
    DuplicateMeasurementId { id: u64 },
    /// Serialized collectors are admitted only at the exact supported schema.
    #[error("collector schema version {actual} is unsupported; expected {expected}")]
    UnsupportedCollectorSchemaVersion { expected: u32, actual: u32 },
    /// Collector capacity must remain within the bounded evidence envelope.
    #[error("collector capacity {capacity} is outside the supported evidence window")]
    InvalidCapacity { capacity: usize },
    /// A retained ring cannot contain more elements than its declared bound.
    #[error("collector retains {sample_count} samples beyond capacity {capacity}")]
    RetainedWindowExceedsCapacity {
        capacity: usize,
        sample_count: usize,
    },
    /// Allocator state must be reachable from the constructor and allocation API.
    #[error(
        "collector allocator state is invalid (next_id={next_id}, id_exhausted={id_exhausted})"
    )]
    InvalidAllocatorState { next_id: u64, id_exhausted: bool },
    /// Every retained ID must have been allocated before the current frontier.
    #[error("measurement ID {id} was not allocated before next ID {next_id}")]
    UnallocatedMeasurementId { id: u64, next_id: u64 },
    /// One retained sample is incomplete, ambiguous, or non-monotonic.
    #[error("measurement {id} is invalid: {error}")]
    InvalidMeasurement {
        id: u64,
        error: InputLatencyMeasurementError,
    },
}

impl InputLatencyEvidenceError {
    /// Stable machine-readable reason code for reports and gates.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::EmptyCollector => "EVIDENCE_EMPTY",
            Self::MeasurementIdExhausted => "EVIDENCE_ID_EXHAUSTED",
            Self::ReservedMeasurementId { .. } => "EVIDENCE_RESERVED_ID",
            Self::DuplicateMeasurementId { .. } => "EVIDENCE_DUPLICATE_ID",
            Self::UnsupportedCollectorSchemaVersion { .. } => {
                "EVIDENCE_UNSUPPORTED_COLLECTOR_SCHEMA"
            }
            Self::InvalidCapacity { .. } => "EVIDENCE_INVALID_CAPACITY",
            Self::RetainedWindowExceedsCapacity { .. } => "EVIDENCE_CAPACITY_EXCEEDED",
            Self::InvalidAllocatorState { .. } => "EVIDENCE_INVALID_ALLOCATOR_STATE",
            Self::UnallocatedMeasurementId { .. } => "EVIDENCE_UNALLOCATED_ID",
            Self::InvalidMeasurement { .. } => "EVIDENCE_INVALID_MEASUREMENT",
        }
    }
}

/// Collects proxy latency measurements and computes aggregate statistics.
///
/// The retained sample ring is bounded. Percentile queries first validate the
/// entire retained window; they never filter invalid samples into a false pass.
#[derive(Debug, Clone, Serialize)]
pub struct InputLatencyCollector {
    /// Exact wire schema for retained proxy collectors.
    schema_version: u32,
    /// Raw measurements in recording order.
    measurements: VecDeque<InputLatencyMeasurement>,
    /// Maximum measurements to retain (ring buffer semantics).
    capacity: usize,
    /// Next measurement ID.
    next_id: u64,
    /// Sticky fail-stop marker set by an allocation attempt at `u64::MAX`.
    id_exhausted: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputLatencyCollectorWire {
    schema_version: u32,
    measurements: BoundedVec<InputLatencyMeasurement, MAX_INPUT_LATENCY_EVIDENCE_WINDOW>,
    capacity: usize,
    next_id: u64,
    id_exhausted: bool,
}

impl<'de> Deserialize<'de> for InputLatencyCollector {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InputLatencyCollectorWire::deserialize(deserializer)?;
        let collector = Self {
            schema_version: wire.schema_version,
            measurements: wire.measurements.0.into_iter().collect(),
            capacity: wire.capacity,
            next_id: wire.next_id,
            id_exhausted: wire.id_exhausted,
        };
        collector
            .validate_structure()
            .map_err(de::Error::custom)?;
        Ok(collector)
    }
}

impl InputLatencyCollector {
    /// Create a new collector with the given capacity.
    ///
    /// Zero or a value above [`MAX_INPUT_LATENCY_EVIDENCE_WINDOW`] is preserved
    /// as an invalid configuration rather than normalized; every gate/report
    /// from such a collector fails with
    /// [`InputLatencyEvidenceError::InvalidCapacity`].
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            schema_version: INPUT_LATENCY_COLLECTOR_SCHEMA_VERSION,
            measurements: VecDeque::with_capacity(capacity.min(4096)),
            capacity,
            next_id: 1,
            id_exhausted: false,
        }
    }

    /// Start a new measurement and return its handle.
    ///
    /// ID zero and `u64::MAX` are reserved. `u64::MAX - 1` is the last usable
    /// identity; a subsequent allocation attempt at `u64::MAX` permanently
    /// fail-stops this collector so ignoring the error cannot leave an
    /// apparently authoritative report.
    pub fn begin_measurement(
        &mut self,
    ) -> Result<InputLatencyMeasurement, InputLatencyCollectorError> {
        if self.id_exhausted || self.next_id == 0 || self.next_id == u64::MAX {
            self.id_exhausted = true;
            return Err(InputLatencyCollectorError::MeasurementIdExhausted);
        }
        let id = self.next_id;
        let Some(next_id) = id.checked_add(1) else {
            self.id_exhausted = true;
            return Err(InputLatencyCollectorError::MeasurementIdExhausted);
        };
        self.next_id = next_id;
        Ok(InputLatencyMeasurement::new(id))
    }

    /// Record a completed or externally constructed measurement.
    ///
    /// A non-reserved imported ID at or beyond the local frontier advances the
    /// frontier before retention. This keeps subsequent locally allocated IDs
    /// strictly newer while allowing begun measurements to complete out of
    /// order. Duplicate, reserved, and invalid samples remain visible to the
    /// fail-closed retained-window validator.
    pub fn record(&mut self, measurement: InputLatencyMeasurement) {
        if !self.id_exhausted
            && measurement.id >= self.next_id
            && measurement.id != u64::MAX
            && let Some(next_id) = measurement.id.checked_add(1)
        {
            self.next_id = next_id;
        }
        if self.capacity == 0 || self.capacity > MAX_INPUT_LATENCY_EVIDENCE_WINDOW {
            return;
        }
        if self.measurements.len() >= self.capacity {
            self.measurements.pop_front();
        }
        self.measurements.push_back(measurement);
    }

    /// Number of recorded measurements.
    #[must_use]
    pub fn count(&self) -> usize {
        self.measurements.len()
    }

    /// Return the exact serialized collector schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    fn validate_structure(&self) -> Result<(), InputLatencyEvidenceError> {
        if self.schema_version != INPUT_LATENCY_COLLECTOR_SCHEMA_VERSION {
            return Err(
                InputLatencyEvidenceError::UnsupportedCollectorSchemaVersion {
                    expected: INPUT_LATENCY_COLLECTOR_SCHEMA_VERSION,
                    actual: self.schema_version,
                },
            );
        }
        if self.capacity == 0 || self.capacity > MAX_INPUT_LATENCY_EVIDENCE_WINDOW {
            return Err(InputLatencyEvidenceError::InvalidCapacity {
                capacity: self.capacity,
            });
        }
        if self.measurements.len() > self.capacity {
            return Err(InputLatencyEvidenceError::RetainedWindowExceedsCapacity {
                capacity: self.capacity,
                sample_count: self.measurements.len(),
            });
        }
        if self.next_id == 0 || (self.id_exhausted && self.next_id != u64::MAX) {
            return Err(InputLatencyEvidenceError::InvalidAllocatorState {
                next_id: self.next_id,
                id_exhausted: self.id_exhausted,
            });
        }

        for measurement in &self.measurements {
            if measurement.id == 0 || measurement.id == u64::MAX {
                return Err(InputLatencyEvidenceError::ReservedMeasurementId {
                    id: measurement.id,
                });
            }
            if measurement.id >= self.next_id {
                return Err(InputLatencyEvidenceError::UnallocatedMeasurementId {
                    id: measurement.id,
                    next_id: self.next_id,
                });
            }
        }

        Ok(())
    }

    /// Validate the complete retained evidence window.
    pub fn validate_evidence(&self) -> Result<(), InputLatencyEvidenceError> {
        self.validate_structure()?;
        if self.id_exhausted {
            return Err(InputLatencyEvidenceError::MeasurementIdExhausted);
        }
        if self.measurements.is_empty() {
            return Err(InputLatencyEvidenceError::EmptyCollector);
        }

        let mut ids = BTreeSet::new();
        for measurement in &self.measurements {
            if !ids.insert(measurement.id) {
                return Err(InputLatencyEvidenceError::DuplicateMeasurementId {
                    id: measurement.id,
                });
            }
            if let Err(error) = measurement.validate_complete() {
                return Err(InputLatencyEvidenceError::InvalidMeasurement {
                    id: measurement.id,
                    error,
                });
            }
        }

        Ok(())
    }

    /// Compute the percentile for end-to-end latency across all measurements.
    pub fn total_latency_percentile(
        &self,
        percentile: Percentile,
    ) -> Result<u64, InputLatencyEvidenceError> {
        self.validate_evidence()?;
        let mut values = Vec::with_capacity(self.measurements.len());
        for measurement in &self.measurements {
            let value = measurement.total_latency_us().map_err(|error| {
                InputLatencyEvidenceError::InvalidMeasurement {
                    id: measurement.id,
                    error,
                }
            })?;
            values.push(value);
        }
        values.sort_unstable();
        percentile_nearest_rank(&values, percentile)
            .ok_or(InputLatencyEvidenceError::EmptyCollector)
    }

    /// Compute the percentile for a specific stage-to-stage latency.
    pub fn stage_latency_percentile(
        &self,
        from: InputLatencyStage,
        to: InputLatencyStage,
        percentile: Percentile,
    ) -> Result<u64, InputLatencyEvidenceError> {
        self.validate_evidence()?;
        let mut values = Vec::with_capacity(self.measurements.len());
        for measurement in &self.measurements {
            let value = measurement.stage_latency_us(from, to).map_err(|error| {
                InputLatencyEvidenceError::InvalidMeasurement {
                    id: measurement.id,
                    error,
                }
            })?;
            values.push(value);
        }
        values.sort_unstable();
        percentile_nearest_rank(&values, percentile)
            .ok_or(InputLatencyEvidenceError::EmptyCollector)
    }

    /// Compute all standard percentiles for end-to-end latency.
    pub fn total_latency_summary(
        &self,
    ) -> Result<BTreeMap<Percentile, u64>, InputLatencyEvidenceError> {
        self.validate_evidence()?;
        let mut values = Vec::with_capacity(self.measurements.len());
        for measurement in &self.measurements {
            let value = measurement.total_latency_us().map_err(|error| {
                InputLatencyEvidenceError::InvalidMeasurement {
                    id: measurement.id,
                    error,
                }
            })?;
            values.push(value);
        }
        values.sort_unstable();
        Ok(Percentile::ALL
            .iter()
            .filter_map(|&p| percentile_nearest_rank(&values, p).map(|v| (p, v)))
            .collect())
    }

    /// Clear all recorded measurements.
    pub fn clear(&mut self) {
        self.measurements.clear();
    }
}

// ── Budget Configuration ────────────────────────────────────────────────────

/// Per-stage latency budget in microseconds at each percentile.
///
/// `stage` names the end of the interval; for example, `PtyWrite` budgets
/// `KeyEvent -> PtyWrite`. `KeyEvent` has no predecessor and is rejected.
#[derive(Debug, Clone, Serialize)]
pub struct StageBudget {
    /// End stage of the adjacent interval this budget applies to.
    pub stage: InputLatencyStage,
    /// Budget targets: percentile → maximum allowed microseconds.
    pub targets: BTreeMap<Percentile, u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StageBudgetWire {
    stage: InputLatencyStage,
    targets: DuplicateRejectingMap<Percentile, u64>,
}

impl<'de> Deserialize<'de> for StageBudget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = StageBudgetWire::deserialize(deserializer)?;
        Ok(Self {
            stage: wire.stage,
            targets: wire.targets.0,
        })
    }
}

/// Complete latency budget configuration.
#[derive(Debug, Clone, Serialize)]
pub struct InputLatencyBudget {
    /// Per-stage budgets.
    pub stages: Vec<StageBudget>,
    /// Aggregate end-to-end budget (KeyEvent → GpuPresent).
    pub aggregate: BTreeMap<Percentile, u64>,
    /// Regression threshold: if measured latency exceeds budget by this fraction,
    /// the check fails. 1.0 = exactly at budget, 1.1 = 10% over budget.
    ///
    /// The wire field is the canonical `0x`-prefixed, 16-lowercase-hex-digit
    /// IEEE-754 payload `regression_threshold_bits`. A decimal JSON number can
    /// move by one ULP in a decoder and thereby change an exact gate boundary.
    #[serde(
        rename = "regression_threshold_bits",
        serialize_with = "serialize_regression_threshold_bits"
    )]
    pub regression_threshold: f64,
}

// Serde's `serialize_with` field adapter contract requires the value as `&T`.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn serialize_regression_threshold_bits<S>(
    value: &f64,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.collect_str(&format_args!("0x{:016x}", value.to_bits()))
}

#[derive(Debug, Clone, Copy)]
struct RegressionThresholdBits(u64);

impl<'de> Deserialize<'de> for RegressionThresholdBits {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RegressionThresholdBitsVisitor;

        impl de::Visitor<'_> for RegressionThresholdBitsVisitor {
            type Value = RegressionThresholdBits;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(
                    "an IEEE-754 payload encoded as 0x followed by 16 lowercase hex digits",
                )
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                let Some(hex) = value.strip_prefix("0x") else {
                    return Err(E::custom("regression threshold bits lack canonical prefix"));
                };
                if hex.len() != 16
                    || !hex
                        .bytes()
                        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
                {
                    return Err(E::custom(
                        "regression threshold bits are not 16 lowercase hex digits",
                    ));
                }
                u64::from_str_radix(hex, 16)
                    .map(RegressionThresholdBits)
                    .map_err(E::custom)
            }
        }

        deserializer.deserialize_str(RegressionThresholdBitsVisitor)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InputLatencyBudgetWire {
    stages: BoundedVec<StageBudget, MAX_INPUT_LATENCY_STAGE_BUDGETS>,
    aggregate: DuplicateRejectingMap<Percentile, u64>,
    regression_threshold_bits: RegressionThresholdBits,
}

impl<'de> Deserialize<'de> for InputLatencyBudget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InputLatencyBudgetWire::deserialize(deserializer)?;
        Ok(Self {
            stages: wire.stages.0,
            aggregate: wire.aggregate.0,
            regression_threshold: f64::from_bits(wire.regression_threshold_bits.0),
        })
    }
}

impl Default for InputLatencyBudget {
    fn default() -> Self {
        Self {
            stages: Vec::new(),
            aggregate: [
                (Percentile::P50, 2000),   // 2ms p50
                (Percentile::P95, 4000),   // 4ms p95
                (Percentile::P99, 8000),   // 8ms p99
                (Percentile::P999, 16000), // 16ms p999
            ]
            .into_iter()
            .collect(),
            regression_threshold: 1.0,
        }
    }
}

// ── Budget Evaluation ───────────────────────────────────────────────────────

/// Invalid budget configuration that must fail closed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputLatencyBudgetError {
    /// An empty aggregate map otherwise passes vacuously.
    #[error("aggregate latency budget has no percentile targets")]
    EmptyAggregateTargets,
    /// NaN, infinity, zero, and negative thresholds are not admissible.
    ///
    /// Exact IEEE-754 bits keep every diagnosis serializable without coercing
    /// NaN or infinity into a misleading JSON value.
    #[error(
        "regression threshold bits 0x{value_bits:016x} must encode a finite value greater than zero"
    )]
    InvalidRegressionThreshold { value_bits: u64 },
    /// Two entries for one stage make configuration precedence ambiguous.
    #[error("stage {stage} has more than one budget entry")]
    DuplicateStageBudget { stage: InputLatencyStage },
    /// Only the five adjacent pipeline intervals can carry stage budgets.
    #[error("budget contains {count} stage entries; maximum is {maximum}")]
    TooManyStageBudgets { count: usize, maximum: usize },
    /// A configured stage budget must actually contain a target.
    #[error("stage {stage} has no percentile targets")]
    EmptyStageTargets { stage: InputLatencyStage },
    /// KeyEvent is an observation boundary, not a stage duration.
    #[error("stage {stage} has no predecessor interval")]
    StageHasNoPredecessor { stage: InputLatencyStage },
    /// Scaling a target must remain representable without saturation.
    #[error("scaled budget {budget_us} * {threshold} exceeds u64 microseconds")]
    EffectiveBudgetOverflow { budget_us: u64, threshold: f64 },
}

impl InputLatencyBudgetError {
    /// Stable machine-readable reason code for reports and gates.
    #[must_use]
    pub const fn reason_code(&self) -> &'static str {
        match self {
            Self::EmptyAggregateTargets => "BUDGET_CONFIG_EMPTY_AGGREGATE",
            Self::InvalidRegressionThreshold { .. } => "BUDGET_CONFIG_INVALID_THRESHOLD",
            Self::DuplicateStageBudget { .. } => "BUDGET_CONFIG_DUPLICATE_STAGE",
            Self::TooManyStageBudgets { .. } => "BUDGET_CONFIG_TOO_MANY_STAGES",
            Self::EmptyStageTargets { .. } => "BUDGET_CONFIG_EMPTY_STAGE",
            Self::StageHasNoPredecessor { .. } => "BUDGET_CONFIG_NO_PREDECESSOR",
            Self::EffectiveBudgetOverflow { .. } => "BUDGET_CONFIG_OVERFLOW",
        }
    }
}

/// Serialize-only result of evaluating proxy measurements against a budget.
///
/// The finite configured threshold and successful per-detail gate boundaries
/// are diagnostic fields, but the exact source collector, full budget, and
/// content-bound external producer/clock registry are not retained here. This
/// derived verdict deliberately does not implement `Deserialize`; consumers
/// must replay that complete bundle rather than trust decoded `passed` fields.
///
/// ```compile_fail
/// use frankenterm_core::input_latency::BudgetCheckResult;
/// let _: BudgetCheckResult = serde_json::from_str("{}").unwrap();
/// ```
///
/// ```compile_fail
/// use frankenterm_core::input_latency::{
///     InputLatencyBudget, InputLatencyCollector, evaluate_budget,
/// };
/// let mut verdict = evaluate_budget(
///     &InputLatencyCollector::new(1),
///     &InputLatencyBudget::default(),
/// );
/// verdict.passed = true;
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct BudgetCheckResult {
    /// Exact wire schema for this standalone derived verdict.
    schema_version: u32,
    /// Permanent authority boundary for this legacy framework.
    evidence_class: InputLatencyEvidenceClass,
    /// Number of retained samples presented to the gate.
    sample_count: usize,
    /// Finite configured threshold, whether valid or invalid.
    ///
    /// `None` is retained for non-finite invalid configurations so the result
    /// remains serializable; the typed `budget_error` remains authoritative.
    #[serde(skip_serializing_if = "Option::is_none")]
    regression_threshold: Option<f64>,
    /// Whether all budget checks passed.
    passed: bool,
    /// Per-percentile results.
    details: Vec<BudgetCheckDetail>,
    /// Evidence failure, if the collector was not admissible.
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_error: Option<InputLatencyEvidenceError>,
    /// Budget configuration failure, if the gate itself was invalid.
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_error: Option<InputLatencyBudgetError>,
    /// Overall reason code.
    reason_code: String,
}

impl BudgetCheckResult {
    /// Return the exact serialized verdict schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Return the permanent authority boundary for this verdict.
    #[must_use]
    pub const fn evidence_class(&self) -> InputLatencyEvidenceClass {
        self.evidence_class
    }

    /// Return the number of retained samples presented to the gate.
    #[must_use]
    pub const fn sample_count(&self) -> usize {
        self.sample_count
    }

    /// Return the finite configured threshold; inspect `budget_error()` before
    /// treating it as an admitted gate parameter.
    #[must_use]
    pub const fn regression_threshold(&self) -> Option<f64> {
        self.regression_threshold
    }

    /// Return whether every configured budget check passed.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.passed
    }

    /// Return the derived per-percentile verdicts.
    #[must_use]
    pub fn details(&self) -> &[BudgetCheckDetail] {
        &self.details
    }

    /// Return the evidence failure when the collector was inadmissible.
    #[must_use]
    pub const fn evidence_error(&self) -> Option<&InputLatencyEvidenceError> {
        self.evidence_error.as_ref()
    }

    /// Return the budget failure when the configuration was inadmissible.
    #[must_use]
    pub const fn budget_error(&self) -> Option<&InputLatencyBudgetError> {
        self.budget_error.as_ref()
    }

    /// Return the stable overall reason code.
    #[must_use]
    pub fn reason_code(&self) -> &str {
        &self.reason_code
    }
}

/// Detail for a single percentile budget check.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetCheckDetail {
    /// `None` means aggregate KeyEvent -> GpuPresent; `Some(stage)` means
    /// `stage.predecessor() -> stage`.
    #[serde(skip_serializing_if = "Option::is_none")]
    stage: Option<InputLatencyStage>,
    /// The percentile checked.
    percentile: Percentile,
    /// Raw, unscaled budget target in microseconds.
    budget_us: u64,
    /// Exact floored result of scaling `budget_us` by the threshold.
    effective_budget_us: u64,
    /// Measured value in microseconds.
    measured_us: u64,
    /// Whether this check passed.
    passed: bool,
    /// Reason code.
    reason_code: String,
}

impl BudgetCheckDetail {
    /// Return the end stage for a per-stage check, or `None` for aggregate.
    #[must_use]
    pub const fn stage(&self) -> Option<InputLatencyStage> {
        self.stage
    }

    /// Return the percentile checked.
    #[must_use]
    pub const fn percentile(&self) -> Percentile {
        self.percentile
    }

    /// Return the configured, unscaled budget in microseconds.
    #[must_use]
    pub const fn budget_us(&self) -> u64 {
        self.budget_us
    }

    /// Return the exact scaled-and-floored gate boundary in microseconds.
    #[must_use]
    pub const fn effective_budget_us(&self) -> u64 {
        self.effective_budget_us
    }

    /// Return the measured percentile in microseconds.
    #[must_use]
    pub const fn measured_us(&self) -> u64 {
        self.measured_us
    }

    /// Return whether this individual check passed.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.passed
    }

    /// Return the stable detail reason code.
    #[must_use]
    pub fn reason_code(&self) -> &str {
        &self.reason_code
    }
}

fn validate_budget(budget: &InputLatencyBudget) -> Result<(), InputLatencyBudgetError> {
    if budget.aggregate.is_empty() {
        return Err(InputLatencyBudgetError::EmptyAggregateTargets);
    }
    if !budget.regression_threshold.is_finite() || budget.regression_threshold <= 0.0 {
        return Err(InputLatencyBudgetError::InvalidRegressionThreshold {
            value_bits: budget.regression_threshold.to_bits(),
        });
    }
    if budget.stages.len() > MAX_INPUT_LATENCY_STAGE_BUDGETS {
        return Err(InputLatencyBudgetError::TooManyStageBudgets {
            count: budget.stages.len(),
            maximum: MAX_INPUT_LATENCY_STAGE_BUDGETS,
        });
    }

    let mut stages = BTreeSet::new();
    for stage_budget in &budget.stages {
        if !stages.insert(stage_budget.stage) {
            return Err(InputLatencyBudgetError::DuplicateStageBudget {
                stage: stage_budget.stage,
            });
        }
        if stage_budget.stage.predecessor().is_none() {
            return Err(InputLatencyBudgetError::StageHasNoPredecessor {
                stage: stage_budget.stage,
            });
        }
        if stage_budget.targets.is_empty() {
            return Err(InputLatencyBudgetError::EmptyStageTargets {
                stage: stage_budget.stage,
            });
        }
    }

    for &budget_us in budget
        .aggregate
        .values()
        .chain(budget.stages.iter().flat_map(|stage| stage.targets.values()))
    {
        effective_budget_us(budget_us, budget.regression_threshold)?;
    }

    Ok(())
}

fn effective_budget_us(
    budget_us: u64,
    threshold: f64,
) -> Result<u64, InputLatencyBudgetError> {
    if !threshold.is_finite() || threshold <= 0.0 {
        return Err(InputLatencyBudgetError::InvalidRegressionThreshold {
            value_bits: threshold.to_bits(),
        });
    }
    if budget_us == 0 {
        return Ok(0);
    }

    // Decode the positive finite f64 exactly as `significand * 2^exponent`.
    // Multiplying `budget_us as f64` first can round upward above 2^53 and mint
    // a pass one microsecond beyond the mathematical budget. The 53-bit
    // significand times u64 fits in u128, so integer shifts can compute the
    // exact floor of the represented threshold without a second rounding.
    const FRACTION_BITS: u32 = 52;
    const EXPONENT_BIAS: i32 = 1023;
    const FRACTION_MASK: u64 = (1_u64 << FRACTION_BITS) - 1;

    let bits = threshold.to_bits();
    let encoded_exponent = ((bits >> FRACTION_BITS) & 0x7ff) as i32;
    let fraction = bits & FRACTION_MASK;
    let (significand, exponent) = if encoded_exponent == 0 {
        (u128::from(fraction), 1 - EXPONENT_BIAS - FRACTION_BITS as i32)
    } else {
        (
            u128::from((1_u64 << FRACTION_BITS) | fraction),
            encoded_exponent - EXPONENT_BIAS - FRACTION_BITS as i32,
        )
    };
    let product = u128::from(budget_us) * significand;
    let scaled = if exponent >= 0 {
        let shift = exponent as u32;
        if shift >= u64::BITS || product > (u128::from(u64::MAX) >> shift) {
            return Err(InputLatencyBudgetError::EffectiveBudgetOverflow {
                budget_us,
                threshold,
            });
        }
        product << shift
    } else {
        let shift = exponent.unsigned_abs();
        if shift >= u128::BITS {
            0
        } else {
            product >> shift
        }
    };

    u64::try_from(scaled).map_err(|_| InputLatencyBudgetError::EffectiveBudgetOverflow {
        budget_us,
        threshold,
    })
}

fn failed_budget_check(
    collector: &InputLatencyCollector,
    budget: &InputLatencyBudget,
    evidence_error: Option<InputLatencyEvidenceError>,
    budget_error: Option<InputLatencyBudgetError>,
    reason_code: &str,
) -> BudgetCheckResult {
    BudgetCheckResult {
        schema_version: INPUT_LATENCY_BUDGET_CHECK_SCHEMA_VERSION,
        evidence_class: InputLatencyEvidenceClass::ProxyOnly,
        sample_count: collector.count(),
        regression_threshold: budget
            .regression_threshold
            .is_finite()
            .then_some(budget.regression_threshold),
        passed: false,
        details: Vec::new(),
        evidence_error,
        budget_error,
        reason_code: reason_code.to_string(),
    }
}

/// Evaluate a collector's measurements against a budget.
#[must_use]
pub fn evaluate_budget(
    collector: &InputLatencyCollector,
    budget: &InputLatencyBudget,
) -> BudgetCheckResult {
    if let Err(error) = validate_budget(budget) {
        let reason_code = error.reason_code();
        return failed_budget_check(collector, budget, None, Some(error), reason_code);
    }
    if let Err(error) = collector.validate_evidence() {
        let reason_code = error.reason_code();
        return failed_budget_check(collector, budget, Some(error), None, reason_code);
    }

    let mut details = Vec::new();
    let mut all_passed = true;

    for (&percentile, &budget_us) in &budget.aggregate {
        let measured_us = match collector.total_latency_percentile(percentile) {
            Ok(value) => value,
            Err(error) => {
                let reason_code = error.reason_code();
                return failed_budget_check(collector, budget, Some(error), None, reason_code);
            }
        };
        let effective_budget = match effective_budget_us(budget_us, budget.regression_threshold) {
            Ok(value) => value,
            Err(error) => {
                let reason_code = error.reason_code();
                return failed_budget_check(collector, budget, None, Some(error), reason_code);
            }
        };
        let passed = measured_us <= effective_budget;
        if !passed {
            all_passed = false;
        }

        details.push(BudgetCheckDetail {
            stage: None,
            percentile,
            budget_us,
            effective_budget_us: effective_budget,
            measured_us,
            passed,
            reason_code: if passed {
                format!("BUDGET_OK_AGGREGATE_{percentile}")
            } else {
                format!("BUDGET_EXCEEDED_AGGREGATE_{percentile}")
            },
        });
    }

    for stage_budget in &budget.stages {
        let Some(from) = stage_budget.stage.predecessor() else {
            let error = InputLatencyBudgetError::StageHasNoPredecessor {
                stage: stage_budget.stage,
            };
            let reason_code = error.reason_code();
            return failed_budget_check(collector, budget, None, Some(error), reason_code);
        };
        for (&percentile, &budget_us) in &stage_budget.targets {
            let measured_us = match collector.stage_latency_percentile(
                from,
                stage_budget.stage,
                percentile,
            ) {
                Ok(value) => value,
                Err(error) => {
                    let reason_code = error.reason_code();
                    return failed_budget_check(collector, budget, Some(error), None, reason_code);
                }
            };
            let effective_budget =
                match effective_budget_us(budget_us, budget.regression_threshold) {
                    Ok(value) => value,
                    Err(error) => {
                        let reason_code = error.reason_code();
                        return failed_budget_check(collector, budget, None, Some(error), reason_code);
                    }
                };
            let passed = measured_us <= effective_budget;
            if !passed {
                all_passed = false;
            }

            details.push(BudgetCheckDetail {
                stage: Some(stage_budget.stage),
                percentile,
                budget_us,
                effective_budget_us: effective_budget,
                measured_us,
                passed,
                reason_code: if passed {
                    format!("BUDGET_OK_{}_{percentile}", stage_budget.stage.label())
                } else {
                    format!(
                        "BUDGET_EXCEEDED_{}_{percentile}",
                        stage_budget.stage.label()
                    )
                },
            });
        }
    }

    BudgetCheckResult {
        schema_version: INPUT_LATENCY_BUDGET_CHECK_SCHEMA_VERSION,
        evidence_class: InputLatencyEvidenceClass::ProxyOnly,
        sample_count: collector.count(),
        regression_threshold: Some(budget.regression_threshold),
        passed: all_passed,
        details,
        evidence_error: None,
        budget_error: None,
        reason_code: if all_passed {
            "ALL_PROXY_BUDGETS_MET".to_string()
        } else {
            "PROXY_BUDGET_VIOLATION".to_string()
        },
    }
}

// ── Latency Report ──────────────────────────────────────────────────────────

/// Admission state for one legacy report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputLatencyEvidenceStatus {
    /// Complete and internally valid, but still proxy-only.
    ValidProxy,
    /// Empty, incomplete, ambiguous, duplicate, regressing, or exhausted.
    Invalid,
}

/// Derived proxy latency summary for immediate regression diagnostics.
///
/// This DTO is intentionally serialize-only. It omits the labelled source
/// timestamps, exact budget, and external producer/clock registry, so decoding
/// a report alone could not replay or verify its verdict. Retained proxy
/// evidence must bundle the serialized [`InputLatencyCollector`], exact budget,
/// and external registry; production authority belongs to trace v2, not this
/// summary.
///
/// ```compile_fail
/// use frankenterm_core::input_latency::InputLatencyReport;
/// let _: InputLatencyReport = serde_json::from_str("{}").unwrap();
/// ```
///
/// ```compile_fail
/// use frankenterm_core::input_latency::{InputLatencyCollector, generate_report};
/// let mut report = generate_report(&InputLatencyCollector::new(1), None);
/// report.admitted_sample_count = report.sample_count;
/// ```
#[derive(Debug, Clone, Serialize)]
pub struct InputLatencyReport {
    /// Serialized report schema version.
    schema_version: u32,
    /// Permanent authority boundary.
    evidence_class: InputLatencyEvidenceClass,
    /// Whether the full retained window was admitted.
    evidence_status: InputLatencyEvidenceStatus,
    /// Typed fail-closed diagnosis for an invalid window.
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_error: Option<InputLatencyEvidenceError>,
    /// Number of measurements in the sample.
    sample_count: usize,
    /// Samples admitted to percentile computation; either all or zero.
    admitted_sample_count: usize,
    /// Per-percentile end-to-end latency in microseconds.
    percentiles: BTreeMap<Percentile, u64>,
    /// Per-stage breakdown at p50.
    stage_breakdown_p50: BTreeMap<String, u64>,
    /// Budget evaluation result (None if no budget configured).
    #[serde(skip_serializing_if = "Option::is_none")]
    budget_check: Option<BudgetCheckResult>,
}

impl InputLatencyReport {
    /// Return the serialized report schema version.
    #[must_use]
    pub const fn schema_version(&self) -> u32 {
        self.schema_version
    }

    /// Return the permanent authority boundary for this report.
    #[must_use]
    pub const fn evidence_class(&self) -> InputLatencyEvidenceClass {
        self.evidence_class
    }

    /// Return whether the entire retained window was admitted.
    #[must_use]
    pub const fn evidence_status(&self) -> InputLatencyEvidenceStatus {
        self.evidence_status
    }

    /// Return the typed evidence failure for an invalid retained window.
    #[must_use]
    pub const fn evidence_error(&self) -> Option<&InputLatencyEvidenceError> {
        self.evidence_error.as_ref()
    }

    /// Return the number of measurements presented to the report generator.
    #[must_use]
    pub const fn sample_count(&self) -> usize {
        self.sample_count
    }

    /// Return the number admitted to percentile computation: all or zero.
    #[must_use]
    pub const fn admitted_sample_count(&self) -> usize {
        self.admitted_sample_count
    }

    /// Return the derived aggregate percentiles.
    #[must_use]
    pub fn percentiles(&self) -> &BTreeMap<Percentile, u64> {
        &self.percentiles
    }

    /// Return the derived adjacent-stage p50 breakdown.
    #[must_use]
    pub fn stage_breakdown_p50(&self) -> &BTreeMap<String, u64> {
        &self.stage_breakdown_p50
    }

    /// Return the optional derived budget verdict.
    #[must_use]
    pub const fn budget_check(&self) -> Option<&BudgetCheckResult> {
        self.budget_check.as_ref()
    }
}

/// Generate a latency report from a collector with optional budget evaluation.
#[must_use]
pub fn generate_report(
    collector: &InputLatencyCollector,
    budget: Option<&InputLatencyBudget>,
) -> InputLatencyReport {
    let mut evidence_error = collector.validate_evidence().err();
    let mut percentiles = if evidence_error.is_none() {
        match collector.total_latency_summary() {
            Ok(summary) => summary,
            Err(error) => {
                evidence_error = Some(error);
                BTreeMap::new()
            }
        }
    } else {
        BTreeMap::new()
    };

    // Stage breakdown at p50
    let mut stage_breakdown_p50 = BTreeMap::new();
    if evidence_error.is_none() {
        for window in InputLatencyStage::ALL.windows(2) {
            let from = window[0];
            let to = window[1];
            match collector.stage_latency_percentile(from, to, Percentile::P50) {
                Ok(latency_us) => {
                    let label = format!("{}_to_{}", from.label(), to.label());
                    stage_breakdown_p50.insert(label, latency_us);
                }
                Err(error) => {
                    evidence_error = Some(error);
                    percentiles.clear();
                    stage_breakdown_p50.clear();
                    break;
                }
            }
        }
    }

    let budget_check = budget.map(|b| evaluate_budget(collector, b));
    let admitted_sample_count = if evidence_error.is_none() {
        collector.count()
    } else {
        0
    };
    let evidence_status = if evidence_error.is_none() {
        InputLatencyEvidenceStatus::ValidProxy
    } else {
        InputLatencyEvidenceStatus::Invalid
    };

    InputLatencyReport {
        schema_version: INPUT_LATENCY_REPORT_SCHEMA_VERSION,
        evidence_class: InputLatencyEvidenceClass::ProxyOnly,
        evidence_status,
        evidence_error,
        sample_count: collector.count(),
        admitted_sample_count,
        percentiles,
        stage_breakdown_p50,
        budget_check,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn producer(value: u64) -> InputLatencyProducerId {
        InputLatencyProducerId::new(value).expect("test producer IDs are non-zero")
    }

    fn clock_domain(value: u64) -> InputLatencyClockDomainId {
        InputLatencyClockDomainId::new(value).expect("test clock-domain IDs are non-zero")
    }

    fn timestamp(timestamp_us: u64) -> InputLatencyTimestamp {
        InputLatencyTimestamp::new(timestamp_us, producer(1), clock_domain(1))
    }

    fn timestamp_from(
        timestamp_us: u64,
        producer_id: u64,
        clock_domain_id: u64,
    ) -> InputLatencyTimestamp {
        InputLatencyTimestamp::new(
            timestamp_us,
            producer(producer_id),
            clock_domain(clock_domain_id),
        )
    }

    fn make_measurement(id: u64, base: u64, step: u64) -> InputLatencyMeasurement {
        let mut m = InputLatencyMeasurement::new(id);
        for (i, &stage) in InputLatencyStage::ALL.iter().enumerate() {
            m.record_stage(stage, timestamp(base + step * i as u64))
                .expect("fixture stages are unique");
        }
        m
    }

    fn make_total_measurement(id: u64, total_us: u64) -> InputLatencyMeasurement {
        let mut measurement = InputLatencyMeasurement::new(id);
        for &stage in InputLatencyStage::ALL {
            let timestamp_us = if stage == InputLatencyStage::GpuPresent {
                total_us
            } else {
                0
            };
            measurement
                .record_stage(stage, timestamp(timestamp_us))
                .expect("fixture stages are unique");
        }
        measurement
    }

    fn record_measurement(collector: &mut InputLatencyCollector, base: u64, step: u64) {
        let id = collector
            .begin_measurement()
            .expect("fixture collector has ID capacity")
            .id;
        collector.record(make_measurement(id, base, step));
    }

    #[test]
    fn measurement_total_latency() {
        let m = make_measurement(1, 1000, 500);
        // KeyEvent=1000, PtyWrite=1500, ..., GpuPresent=3500
        assert_eq!(m.total_latency_us(), Ok(2500)); // 3500 - 1000
    }

    #[test]
    fn measurement_stage_latency() {
        let m = make_measurement(1, 1000, 500);
        assert_eq!(
            m.stage_latency_us(InputLatencyStage::KeyEvent, InputLatencyStage::PtyWrite),
            Ok(500)
        );
        assert_eq!(
            m.stage_latency_us(InputLatencyStage::KeyEvent, InputLatencyStage::GpuPresent),
            Ok(2500)
        );
    }

    #[test]
    fn measurement_missing_stage_is_explicit_error() {
        let mut m = InputLatencyMeasurement::new(1);
        m.record_stage(InputLatencyStage::KeyEvent, timestamp(1000))
            .unwrap();
        assert!(matches!(
            m.stage_latency_us(InputLatencyStage::KeyEvent, InputLatencyStage::PtyWrite),
            Err(InputLatencyMeasurementError::MissingStage {
                stage: InputLatencyStage::PtyWrite
            })
        ));
    }

    #[test]
    fn duplicate_stage_taints_measurement_without_overwrite() {
        let mut m = InputLatencyMeasurement::new(1);
        m.record_stage(InputLatencyStage::KeyEvent, timestamp(1000))
            .unwrap();
        let error = m
            .record_stage(InputLatencyStage::KeyEvent, timestamp(2000))
            .unwrap_err();
        assert_eq!(
            error,
            InputLatencyMeasurementError::DuplicateStage {
                stage: InputLatencyStage::KeyEvent
            }
        );
        assert_eq!(
            m.stage_timestamp(InputLatencyStage::KeyEvent)
                .unwrap()
                .timestamp_us,
            1000
        );
        assert_eq!(m.validate_complete(), Err(error));
    }

    #[test]
    fn unrelated_clock_domains_fail_closed() {
        let mut m = make_measurement(1, 1000, 100);
        let stages = m.stages().clone();
        m = InputLatencyMeasurement::from_stages(
            1,
            stages
                .into_iter()
                .map(|(stage, value)| {
                    let value = if stage == InputLatencyStage::PtyRead {
                        timestamp_from(value.timestamp_us, 2, 2)
                    } else {
                        value
                    };
                    (stage, value)
                })
                .collect(),
        );
        assert!(matches!(
            m.validate_complete(),
            Err(InputLatencyMeasurementError::ClockDomainMismatch { .. })
        ));
    }

    #[test]
    fn regressing_timestamp_fails_closed() {
        let mut stages = make_measurement(1, 1000, 100).stages().clone();
        stages.insert(InputLatencyStage::TermUpdate, timestamp(100));
        let m = InputLatencyMeasurement::from_stages(1, stages);
        assert!(matches!(
            m.validate_complete(),
            Err(InputLatencyMeasurementError::TimestampRegression { .. })
        ));
    }

    #[test]
    fn different_producers_in_shared_clock_domain_are_comparable() {
        let mut stages = BTreeMap::new();
        for (index, &stage) in InputLatencyStage::ALL.iter().enumerate() {
            stages.insert(
                stage,
                timestamp_from(1000 + index as u64 * 100, index as u64 + 1, 9),
            );
        }
        let m = InputLatencyMeasurement::from_stages(1, stages);
        assert_eq!(m.total_latency_us(), Ok(500));
        assert_eq!(
            m.stage_timestamp(InputLatencyStage::GpuPresent)
                .unwrap()
                .producer_id
                .get(),
            6
        );
    }

    #[test]
    fn zero_duration_is_valid_at_clock_resolution() {
        let m = make_measurement(1, 1000, 0);
        assert_eq!(m.total_latency_us(), Ok(0));
    }

    #[test]
    fn collector_basic_operations() {
        let mut collector = InputLatencyCollector::new(100);
        assert_eq!(collector.count(), 0);

        record_measurement(&mut collector, 1000, 500);
        assert_eq!(collector.count(), 1);
        assert_eq!(collector.validate_evidence(), Ok(()));
    }

    #[test]
    fn collector_capacity_eviction() {
        let mut collector = InputLatencyCollector::new(3);
        for i in 0..5u64 {
            record_measurement(&mut collector, 1000 + i, 500);
        }
        assert_eq!(collector.count(), 3);
        assert_eq!(collector.validate_evidence(), Ok(()));
    }

    #[test]
    fn collector_percentile_computation() {
        let mut collector = InputLatencyCollector::new(100);
        // Add measurements with increasing latency
        for i in 0..100u64 {
            record_measurement(&mut collector, 1000, 100 + i * 10);
        }

        let p50 = collector.total_latency_percentile(Percentile::P50).unwrap();
        let p99 = collector.total_latency_percentile(Percentile::P99).unwrap();
        assert!(p99 >= p50, "p99 >= p50");
    }

    #[test]
    fn collector_empty_percentile_fails_closed() {
        let collector = InputLatencyCollector::new(100);
        assert_eq!(
            collector.total_latency_percentile(Percentile::P50),
            Err(InputLatencyEvidenceError::EmptyCollector)
        );
    }

    #[test]
    fn collector_duplicate_measurement_id_fails_closed() {
        let mut collector = InputLatencyCollector::new(100);
        let id = collector.begin_measurement().unwrap().id;
        collector.record(make_measurement(id, 1000, 100));
        collector.record(make_measurement(id, 2000, 100));
        assert_eq!(
            collector.validate_evidence(),
            Err(InputLatencyEvidenceError::DuplicateMeasurementId { id })
        );
    }

    #[test]
    fn collector_reserved_measurement_ids_fail_closed() {
        for id in [0, u64::MAX] {
            let mut collector = InputLatencyCollector::new(1);
            collector.record(make_measurement(id, 1000, 100));
            assert_eq!(
                collector.validate_evidence(),
                Err(InputLatencyEvidenceError::ReservedMeasurementId { id })
            );
        }
    }

    #[test]
    fn measurement_id_exhaustion_is_terminal() {
        let mut collector = InputLatencyCollector::new(10);
        record_measurement(&mut collector, 1000, 100);
        collector.next_id = u64::MAX;
        assert!(matches!(
            collector.begin_measurement(),
            Err(InputLatencyCollectorError::MeasurementIdExhausted)
        ));
        assert_eq!(
            collector.validate_evidence(),
            Err(InputLatencyEvidenceError::MeasurementIdExhausted)
        );
        collector.clear();
        assert!(matches!(
            collector.begin_measurement(),
            Err(InputLatencyCollectorError::MeasurementIdExhausted)
        ));
    }

    #[test]
    fn percentile_nearest_rank_basic() {
        let values = vec![100, 200, 300, 400, 500, 600, 700, 800, 900, 1000];
        assert_eq!(percentile_nearest_rank(&values, Percentile::P50), Some(500));
        assert_eq!(
            percentile_nearest_rank(&values, Percentile::P95),
            Some(1000)
        );
    }

    #[test]
    fn percentile_nearest_rank_single_element() {
        let values = vec![42];
        assert_eq!(percentile_nearest_rank(&values, Percentile::P50), Some(42));
        assert_eq!(percentile_nearest_rank(&values, Percentile::P99), Some(42));
    }

    #[test]
    fn percentile_nearest_rank_empty() {
        let values: Vec<u64> = vec![];
        assert_eq!(percentile_nearest_rank(&values, Percentile::P50), None);
    }

    #[test]
    fn budget_check_all_passing() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..50u64 {
            record_measurement(&mut collector, 1000 + i, 100); // total = 500us
        }

        let budget = InputLatencyBudget::default(); // p50=2000, p95=4000
        let result = evaluate_budget(&collector, &budget);

        assert!(result.passed);
        assert_eq!(result.reason_code, "ALL_PROXY_BUDGETS_MET");
        assert_eq!(result.evidence_class, InputLatencyEvidenceClass::ProxyOnly);
        assert!(result.evidence_error.is_none());
        assert!(result.details.iter().all(|d| d.passed));
    }

    #[test]
    fn budget_check_violation() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..50u64 {
            // total = 50000us (50ms) — way over budget
            record_measurement(&mut collector, 1000 + i, 10000);
        }

        let budget = InputLatencyBudget::default();
        let result = evaluate_budget(&collector, &budget);

        assert!(!result.passed);
        assert_eq!(result.reason_code, "PROXY_BUDGET_VIOLATION");
    }

    #[test]
    fn budget_check_with_regression_threshold() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..50u64 {
            record_measurement(&mut collector, 1000 + i, 500); // total = 2500us
        }

        let budget = InputLatencyBudget {
            regression_threshold: 1.5, // allow 50% over
            ..Default::default()
        };
        let result = evaluate_budget(&collector, &budget);

        // 2500 < 2000*1.5=3000, so should pass
        assert!(result.passed);
        assert_eq!(result.regression_threshold, Some(1.5));
        let p50 = result
            .details
            .iter()
            .find(|detail| detail.percentile == Percentile::P50)
            .unwrap();
        assert_eq!(p50.budget_us, 2000);
        assert_eq!(p50.effective_budget_us, 3000);
        assert_eq!(p50.measured_us, 2500);
        assert!(p50.passed);
    }

    #[test]
    fn effective_budget_scaling_is_exact_above_f64_integer_range() {
        let budget_us = (1_u64 << 53) + 3;
        let measured_us = budget_us + 1;
        let mut collector = InputLatencyCollector::new(1);
        let id = collector.begin_measurement().unwrap().id;
        collector.record(make_total_measurement(id, measured_us));
        let budget = InputLatencyBudget {
            stages: Vec::new(),
            aggregate: [(Percentile::P50, budget_us)].into_iter().collect(),
            regression_threshold: 1.0,
        };

        let result = evaluate_budget(&collector, &budget);
        assert!(!result.passed);
        let detail = result.details.first().unwrap();
        assert_eq!(detail.budget_us, budget_us);
        assert_eq!(detail.effective_budget_us, budget_us);
        assert_eq!(detail.measured_us, measured_us);
        assert!(!detail.passed);
    }

    #[test]
    fn exact_scaling_handles_binary_fractions_limits_and_invalid_inputs() {
        assert_eq!(effective_budget_us(3, 0.5), Ok(1));
        assert_eq!(effective_budget_us(3, 1.5), Ok(4));
        assert_eq!(effective_budget_us(u64::MAX, f64::from_bits(1)), Ok(0));
        assert_eq!(effective_budget_us(u64::MAX, 1.0), Ok(u64::MAX));
        assert!(matches!(
            effective_budget_us(u64::MAX, 2.0),
            Err(InputLatencyBudgetError::EffectiveBudgetOverflow { .. })
        ));
        for threshold in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            -0.0,
            0.0,
        ] {
            assert!(matches!(
                effective_budget_us(1, threshold),
                Err(InputLatencyBudgetError::InvalidRegressionThreshold { value_bits })
                    if value_bits == threshold.to_bits()
            ));
        }
    }

    #[test]
    fn invalid_threshold_verdicts_remain_json_serializable() {
        let mut collector = InputLatencyCollector::new(1);
        record_measurement(&mut collector, 1000, 100);

        for threshold in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            -0.0,
            0.0,
        ] {
            let budget = InputLatencyBudget {
                regression_threshold: threshold,
                ..Default::default()
            };
            let result = evaluate_budget(&collector, &budget);
            assert!(!result.passed);
            assert!(matches!(
                &result.budget_error,
                Some(InputLatencyBudgetError::InvalidRegressionThreshold { value_bits })
                    if *value_bits == threshold.to_bits()
            ));
            let value = serde_json::to_value(&result).unwrap();
            assert_eq!(
                value["schema_version"],
                serde_json::json!(INPUT_LATENCY_BUDGET_CHECK_SCHEMA_VERSION)
            );
            assert_eq!(
                value["budget_error"]["value_bits"],
                serde_json::json!(threshold.to_bits())
            );
            if threshold.is_finite() {
                assert_eq!(value["regression_threshold"], serde_json::json!(threshold));
            } else {
                assert!(value.get("regression_threshold").is_none());
            }
        }
    }

    #[test]
    fn empty_collector_budget_is_explicit_non_pass() {
        let collector = InputLatencyCollector::new(100);
        let result = evaluate_budget(&collector, &InputLatencyBudget::default());
        assert!(!result.passed);
        assert_eq!(result.reason_code, "EVIDENCE_EMPTY");
        assert!(result.details.is_empty());
        assert_eq!(
            result.evidence_error,
            Some(InputLatencyEvidenceError::EmptyCollector)
        );
    }

    #[test]
    fn incomplete_measurement_budget_is_explicit_non_pass() {
        let mut collector = InputLatencyCollector::new(100);
        let mut measurement = InputLatencyMeasurement::new(1);
        measurement
            .record_stage(InputLatencyStage::KeyEvent, timestamp(1000))
            .unwrap();
        collector.record(measurement);
        let result = evaluate_budget(&collector, &InputLatencyBudget::default());
        assert!(!result.passed);
        assert_eq!(result.reason_code, "EVIDENCE_INVALID_MEASUREMENT");
        assert!(matches!(
            result.evidence_error,
            Some(InputLatencyEvidenceError::InvalidMeasurement {
                error: InputLatencyMeasurementError::MissingStage { .. },
                ..
            })
        ));
    }

    #[test]
    fn configured_stage_budget_is_enforced() {
        let mut collector = InputLatencyCollector::new(10);
        record_measurement(&mut collector, 1000, 100);
        let budget = InputLatencyBudget {
            stages: vec![StageBudget {
                stage: InputLatencyStage::PtyWrite,
                targets: [(Percentile::P50, 99)].into_iter().collect(),
            }],
            aggregate: [(Percentile::P50, 1000)].into_iter().collect(),
            regression_threshold: 1.0,
        };
        let result = evaluate_budget(&collector, &budget);
        assert!(!result.passed);
        assert!(result.details.iter().any(|detail| {
            detail.stage == Some(InputLatencyStage::PtyWrite) && !detail.passed
        }));
    }

    #[test]
    fn invalid_budget_configurations_fail_closed() {
        let mut collector = InputLatencyCollector::new(10);
        record_measurement(&mut collector, 1000, 100);

        let cases = [
            InputLatencyBudget {
                aggregate: BTreeMap::new(),
                ..Default::default()
            },
            InputLatencyBudget {
                regression_threshold: 0.0,
                ..Default::default()
            },
            InputLatencyBudget {
                stages: vec![StageBudget {
                    stage: InputLatencyStage::KeyEvent,
                    targets: [(Percentile::P50, 100)].into_iter().collect(),
                }],
                ..Default::default()
            },
            InputLatencyBudget {
                stages: vec![
                    StageBudget {
                        stage: InputLatencyStage::PtyWrite,
                        targets: [(Percentile::P50, 100)].into_iter().collect(),
                    },
                    StageBudget {
                        stage: InputLatencyStage::PtyWrite,
                        targets: [(Percentile::P95, 100)].into_iter().collect(),
                    },
                ],
                ..Default::default()
            },
        ];

        for budget in cases {
            let result = evaluate_budget(&collector, &budget);
            assert!(!result.passed);
            assert!(result.budget_error.is_some());
            assert!(result.details.is_empty());
        }
    }

    #[test]
    fn report_generation() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..20u64 {
            record_measurement(&mut collector, 1000 + i, 200);
        }

        let report = generate_report(&collector, Some(&InputLatencyBudget::default()));

        assert_eq!(report.sample_count, 20);
        assert_eq!(report.admitted_sample_count, 20);
        assert_eq!(report.schema_version, INPUT_LATENCY_REPORT_SCHEMA_VERSION);
        assert_eq!(report.evidence_class, InputLatencyEvidenceClass::ProxyOnly);
        assert_eq!(report.evidence_status, InputLatencyEvidenceStatus::ValidProxy);
        assert!(report.evidence_error.is_none());
        assert!(!report.percentiles.is_empty());
        assert!(!report.stage_breakdown_p50.is_empty());
        assert!(report.budget_check.is_some());
    }

    #[test]
    fn report_without_budget() {
        let mut collector = InputLatencyCollector::new(100);
        record_measurement(&mut collector, 1000, 200);

        let report = generate_report(&collector, None);
        assert!(report.budget_check.is_none());
    }

    #[test]
    fn invalid_report_admits_zero_samples() {
        let collector = InputLatencyCollector::new(100);
        let report = generate_report(&collector, Some(&InputLatencyBudget::default()));
        assert_eq!(report.evidence_status, InputLatencyEvidenceStatus::Invalid);
        assert_eq!(report.sample_count, 0);
        assert_eq!(report.admitted_sample_count, 0);
        assert!(report.percentiles.is_empty());
        assert!(report.stage_breakdown_p50.is_empty());
        assert!(!report.budget_check.unwrap().passed);
    }

    #[test]
    fn stage_display_and_label() {
        for stage in InputLatencyStage::ALL {
            let label = stage.label();
            let display = format!("{stage}");
            assert_eq!(label, display);
            assert!(!label.is_empty());
        }
    }

    #[test]
    fn percentile_display() {
        assert_eq!(format!("{}", Percentile::P50), "p50");
        assert_eq!(format!("{}", Percentile::P95), "p95");
        assert_eq!(format!("{}", Percentile::P99), "p99");
        assert_eq!(format!("{}", Percentile::P999), "p999");
    }

    #[test]
    fn percentile_fraction_ordering() {
        assert!(Percentile::P50.fraction() < Percentile::P95.fraction());
        assert!(Percentile::P95.fraction() < Percentile::P99.fraction());
        assert!(Percentile::P99.fraction() < Percentile::P999.fraction());
    }

    #[test]
    fn stage_all_has_correct_count() {
        assert_eq!(InputLatencyStage::ALL.len(), 6);
        assert_eq!(InputLatencyStage::KeyEvent.predecessor(), None);
        assert_eq!(
            InputLatencyStage::GpuPresent.predecessor(),
            Some(InputLatencyStage::RenderSubmit)
        );
    }

    #[test]
    fn collector_clear_resets() {
        let mut collector = InputLatencyCollector::new(100);
        record_measurement(&mut collector, 1000, 100);
        assert_eq!(collector.count(), 1);
        collector.clear();
        assert_eq!(collector.count(), 0);
    }

    #[test]
    fn measurement_serde_roundtrip() {
        let m = make_measurement(42, 1000, 500);
        let json = serde_json::to_string(&m).unwrap();
        let back: InputLatencyMeasurement = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, 42);
        assert_eq!(back.stages().len(), 6);
        assert_eq!(back.total_latency_us(), m.total_latency_us());
    }

    #[test]
    fn serialized_duplicate_stage_key_is_rejected_before_overwrite() {
        let json = r#"{
            "id": 1,
            "stages": {
                "key_event": {
                    "timestamp_us": 100,
                    "producer_id": 1,
                    "clock_domain_id": 1
                },
                "key_event": {
                    "timestamp_us": 200,
                    "producer_id": 1,
                    "clock_domain_id": 1
                }
            }
        }"#;
        let error = serde_json::from_str::<InputLatencyMeasurement>(json).unwrap_err();
        assert!(
            error.to_string().contains("duplicate map key key_event"),
            "unexpected duplicate-stage error: {error}"
        );
    }

    #[test]
    fn evidence_input_wires_reject_unknown_fields_and_zero_labels() {
        let mut timestamp_value = serde_json::to_value(timestamp(100)).unwrap();
        timestamp_value["future_clock_authority"] = serde_json::json!(true);
        assert!(serde_json::from_value::<InputLatencyTimestamp>(timestamp_value).is_err());

        let measurement = make_measurement(1, 1000, 100);
        let mut measurement_value = serde_json::to_value(&measurement).unwrap();
        measurement_value["recording_faults"] = serde_json::json!([]);
        assert!(
            serde_json::from_value::<InputLatencyMeasurement>(measurement_value.clone()).is_err()
        );

        measurement_value
            .as_object_mut()
            .unwrap()
            .remove("recording_faults");
        measurement_value["stages"]["key_event"]["future_clock_authority"] =
            serde_json::json!(true);
        assert!(
            serde_json::from_value::<InputLatencyMeasurement>(measurement_value).is_err()
        );

        for label in ["producer_id", "clock_domain_id"] {
            let mut zero_label = serde_json::to_value(&measurement).unwrap();
            zero_label["stages"]["key_event"][label] = serde_json::json!(0);
            assert!(serde_json::from_value::<InputLatencyMeasurement>(zero_label).is_err());
        }

        let mut collector_value = serde_json::to_value(InputLatencyCollector::new(1)).unwrap();
        collector_value["future_allocator_authority"] = serde_json::json!(true);
        assert!(serde_json::from_value::<InputLatencyCollector>(collector_value).is_err());

        let mut stage_budget_value = serde_json::to_value(StageBudget {
            stage: InputLatencyStage::PtyWrite,
            targets: [(Percentile::P50, 100)].into_iter().collect(),
        })
        .unwrap();
        stage_budget_value["future_stage_authority"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StageBudget>(stage_budget_value).is_err());

        let mut budget_value = serde_json::to_value(InputLatencyBudget::default()).unwrap();
        budget_value["future_budget_authority"] = serde_json::json!(true);
        assert!(serde_json::from_value::<InputLatencyBudget>(budget_value).is_err());
    }

    #[test]
    fn collector_serde_roundtrip() {
        let mut collector = InputLatencyCollector::new(50);
        for i in 0..5u64 {
            record_measurement(&mut collector, 1000 + i, 200);
        }
        let json = serde_json::to_string(&collector).unwrap();
        let mut back: InputLatencyCollector = serde_json::from_str(&json).unwrap();
        assert_eq!(back.count(), 5);
        assert_eq!(back.schema_version(), INPUT_LATENCY_COLLECTOR_SCHEMA_VERSION);
        assert_eq!(back.begin_measurement().unwrap().id, 6);
    }

    #[test]
    fn collector_serde_rejects_unallocated_id_and_invalid_capacity() {
        let mut collector = InputLatencyCollector::new(2);
        record_measurement(&mut collector, 1000, 100);
        let mut encoded = serde_json::to_value(&collector).unwrap();
        encoded["next_id"] = serde_json::json!(1);
        assert!(serde_json::from_value::<InputLatencyCollector>(encoded.clone()).is_err());

        encoded["next_id"] = serde_json::json!(2);
        encoded["capacity"] = serde_json::json!(0);
        assert!(serde_json::from_value::<InputLatencyCollector>(encoded).is_err());
    }

    #[test]
    fn collector_serde_rejects_schema_capacity_and_retained_window_incoherence() {
        let mut collector = InputLatencyCollector::new(2);
        record_measurement(&mut collector, 1000, 100);
        record_measurement(&mut collector, 2000, 100);
        let encoded = serde_json::to_value(&collector).unwrap();

        let mut unsupported_schema = encoded.clone();
        unsupported_schema["schema_version"] =
            serde_json::json!(INPUT_LATENCY_COLLECTOR_SCHEMA_VERSION + 1);
        assert!(serde_json::from_value::<InputLatencyCollector>(unsupported_schema).is_err());

        let mut retained_overflow = encoded.clone();
        retained_overflow["capacity"] = serde_json::json!(1);
        assert!(serde_json::from_value::<InputLatencyCollector>(retained_overflow).is_err());

        let mut incoherent_exhaustion = encoded;
        incoherent_exhaustion["id_exhausted"] = serde_json::json!(true);
        assert!(serde_json::from_value::<InputLatencyCollector>(incoherent_exhaustion).is_err());

        let oversized = InputLatencyCollector::new(MAX_INPUT_LATENCY_EVIDENCE_WINDOW + 1);
        assert_eq!(
            oversized.validate_evidence(),
            Err(InputLatencyEvidenceError::InvalidCapacity {
                capacity: MAX_INPUT_LATENCY_EVIDENCE_WINDOW + 1
            })
        );
        assert!(serde_json::from_str::<BoundedVec<u8, 2>>("[1,2,3]").is_err());
    }

    #[test]
    fn budget_serde_roundtrip() {
        // The first non-default value is a retained regression for a one-ULP
        // drift exposed by the property suite. Invalid values are included
        // because their exact bits must survive replay until validation emits
        // the authoritative typed failure.
        for regression_threshold in [
            1.0,
            0.908_841_146_302_401_9,
            0.1,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            f64::MAX,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::from_bits(0x7ff8_0000_0000_1234),
        ] {
            let mut budget = InputLatencyBudget::default();
            budget.regression_threshold = regression_threshold;
            let json = serde_json::to_string(&budget).unwrap();
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(
                value["regression_threshold_bits"],
                serde_json::json!(format!(
                    "0x{:016x}",
                    regression_threshold.to_bits()
                ))
            );
            assert!(value.get("regression_threshold").is_none());
            let back: InputLatencyBudget = serde_json::from_str(&json).unwrap();
            assert_eq!(back.aggregate, budget.aggregate);
            assert_eq!(
                back.regression_threshold.to_bits(),
                budget.regression_threshold.to_bits(),
                "regression threshold changed across JSON: {json}"
            );
        }
    }

    #[test]
    fn serialized_duplicate_budget_targets_are_rejected() {
        let aggregate = r#"{
            "stages": [],
            "aggregate": {"p50": 100, "p50": 200},
            "regression_threshold_bits": "0x3ff0000000000000"
        }"#;
        assert!(serde_json::from_str::<InputLatencyBudget>(aggregate).is_err());

        let stage = r#"{
            "stage": "pty_write",
            "targets": {"p95": 100, "p95": 200}
        }"#;
        assert!(serde_json::from_str::<StageBudget>(stage).is_err());

        let stage_value = serde_json::json!({
            "stage": "pty_write",
            "targets": {"p50": 100}
        });
        let oversized_stages = serde_json::json!({
            "stages": vec![stage_value; MAX_INPUT_LATENCY_STAGE_BUDGETS + 1],
            "aggregate": {"p50": 1000},
            "regression_threshold_bits": "0x3ff0000000000000"
        });
        assert!(serde_json::from_value::<InputLatencyBudget>(oversized_stages).is_err());
    }

    #[test]
    fn budget_serde_rejects_ambiguous_or_noncanonical_threshold_wire_values() {
        for invalid_field in [
            serde_json::json!({"regression_threshold": 1.0}),
            serde_json::json!({"regression_threshold_bits": 4_607_182_418_800_017_408_u64}),
            serde_json::json!({"regression_threshold_bits": "3ff0000000000000"}),
            serde_json::json!({"regression_threshold_bits": "0X3ff0000000000000"}),
            serde_json::json!({"regression_threshold_bits": "0x3FF0000000000000"}),
            serde_json::json!({"regression_threshold_bits": "0x3ff000000000000"}),
            serde_json::json!({"regression_threshold_bits": "0x3ff00000000000000"}),
            serde_json::json!({"regression_threshold_bits": "0x3fg0000000000000"}),
        ] {
            let mut value = serde_json::json!({
                "stages": [],
                "aggregate": {"p50": 1000}
            });
            value.as_object_mut().unwrap().extend(
                invalid_field
                    .as_object()
                    .unwrap()
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone())),
            );
            assert!(
                serde_json::from_value::<InputLatencyBudget>(value).is_err(),
                "ambiguous threshold wire value must fail closed: {invalid_field}"
            );
        }
    }

    #[test]
    fn report_is_serialize_only_derived_summary() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..10u64 {
            record_measurement(&mut collector, 1000 + i, 300);
        }
        let report = generate_report(&collector, Some(&InputLatencyBudget::default()));
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(INPUT_LATENCY_REPORT_SCHEMA_VERSION)
        );
        assert_eq!(value["sample_count"], serde_json::json!(10));
        assert_eq!(value["evidence_class"], serde_json::json!("proxy_only"));
    }

    #[test]
    fn budget_check_is_serialize_only_derived_verdict() {
        let mut collector = InputLatencyCollector::new(1);
        record_measurement(&mut collector, 1000, 100);
        let result = evaluate_budget(&collector, &InputLatencyBudget::default());
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(
            value["schema_version"],
            serde_json::json!(INPUT_LATENCY_BUDGET_CHECK_SCHEMA_VERSION)
        );
        assert_eq!(value["passed"], serde_json::json!(true));
        assert_eq!(value["sample_count"], serde_json::json!(1));
        assert_eq!(
            value["reason_code"],
            serde_json::json!("ALL_PROXY_BUDGETS_MET")
        );
    }

    #[test]
    fn stage_breakdown_labels_follow_convention() {
        let mut collector = InputLatencyCollector::new(100);
        record_measurement(&mut collector, 1000, 200);
        let report = generate_report(&collector, None);

        for key in report.stage_breakdown_p50.keys() {
            assert!(key.contains("_to_"), "Stage key must contain '_to_': {key}");
        }
    }

    #[test]
    fn begin_measurement_assigns_incrementing_ids() {
        let mut collector = InputLatencyCollector::new(100);
        let m1 = collector.begin_measurement().unwrap();
        let m2 = collector.begin_measurement().unwrap();
        let m3 = collector.begin_measurement().unwrap();
        assert_eq!(m1.id, 1);
        assert_eq!(m2.id, 2);
        assert_eq!(m3.id, 3);
    }

    #[test]
    fn imported_measurement_advances_allocator_frontier_without_reordering() {
        let mut collector = InputLatencyCollector::new(4);
        collector.record(make_measurement(999, 1000, 100));
        assert_eq!(collector.validate_evidence(), Ok(()));
        assert_eq!(collector.begin_measurement().unwrap().id, 1000);
    }

    #[test]
    fn max_minus_one_is_last_usable_id_then_exhaustion_taints() {
        let mut collector = InputLatencyCollector::new(2);
        collector.next_id = u64::MAX - 1;
        let measurement = collector.begin_measurement().unwrap();
        assert_eq!(measurement.id, u64::MAX - 1);
        collector.record(make_measurement(measurement.id, 1000, 100));
        assert_eq!(collector.validate_evidence(), Ok(()));

        assert!(matches!(
            collector.begin_measurement(),
            Err(InputLatencyCollectorError::MeasurementIdExhausted)
        ));
        assert_eq!(
            collector.validate_evidence(),
            Err(InputLatencyEvidenceError::MeasurementIdExhausted)
        );
    }

    #[test]
    fn provenance_ids_reserve_zero() {
        assert_eq!(InputLatencyProducerId::new(0), None);
        assert_eq!(InputLatencyClockDomainId::new(0), None);
        assert_eq!(InputLatencyProducerId::new(7).unwrap().get(), 7);
        assert_eq!(InputLatencyClockDomainId::new(9).unwrap().get(), 9);
    }
}
