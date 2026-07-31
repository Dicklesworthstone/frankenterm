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
//! Each timestamp names its producer and monotonic clock domain. Durations are
//! admitted only when every required stage is present, adjacent timestamps use
//! the same clock domain, and timestamps do not regress. Cross-domain latency
//! requires the trace-v2 calibration contract; this legacy proxy refuses to
//! guess it.
//!
//! # Design Principles
//!
//! - **Fail-closed evidence**: Empty, incomplete, ambiguous, or exhausted
//!   collectors cannot pass a budget.
//! - **Deterministic percentiles**: Uses the nearest-rank method (no interpolation).
//! - **Explicit provenance**: Every timestamp binds producer and clock-domain IDs.
//! - **Budget algebra**: Per-stage budgets compose to an aggregate ceiling.
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

/// Schema version for serialized legacy input-latency reports.
pub const INPUT_LATENCY_REPORT_SCHEMA_VERSION: u32 = 2;

/// Authority class carried by every report and budget verdict from this module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputLatencyEvidenceClass {
    /// Offline/synthetic regression proxy; never production input-to-present proof.
    ProxyOnly,
}

/// Producer identity within one evidence bundle.
///
/// The bundle producer registry binds this non-zero value to an exact host,
/// process, build, and boot/session identity. Reusing a value for a different
/// producer invalidates the surrounding bundle rather than this local DTO.
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

/// Monotonic clock-domain identity within one evidence bundle.
///
/// Equal IDs assert that timestamps share one subtraction-safe epoch and rate.
/// Different IDs are never subtracted by this legacy framework.
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

/// One producer-qualified timestamp in a monotonic clock domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputLatencyTimestamp {
    /// Timestamp in microseconds within `clock_domain_id`.
    pub timestamp_us: u64,
    /// Exact producer that observed this stage.
    pub producer_id: InputLatencyProducerId,
    /// Subtraction-safe monotonic clock domain.
    pub clock_domain_id: InputLatencyClockDomainId,
}

impl InputLatencyTimestamp {
    /// Construct a producer- and clock-qualified timestamp.
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

/// Stages on the input-to-display critical path.
///
/// Ordered by pipeline position: user keypress through to visible pixel update.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputLatencyStage {
    /// Key event received from the OS/window system.
    KeyEvent,
    /// Key event encoded and written to the PTY master fd.
    PtyWrite,
    /// Response bytes read from the PTY slave fd.
    PtyRead,
    /// Terminal state machine updated (cell grid, cursor, attributes).
    TermUpdate,
    /// Render command buffer submitted to GPU API (wgpu/Metal).
    RenderSubmit,
    /// GPU present completed (frame visible on screen).
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
/// Each stage retains producer and clock provenance. Partial measurements are
/// useful diagnostics, but are explicit non-pass evidence.
#[derive(Debug, Clone, Serialize)]
pub struct InputLatencyMeasurement {
    /// Monotonic measurement ID.
    pub id: u64,
    /// Qualified timestamp at each recorded stage.
    stages: BTreeMap<InputLatencyStage, InputLatencyTimestamp>,
    /// First recording fault. Once tainted, a measurement cannot become valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recording_fault: Option<InputLatencyMeasurementError>,
}

#[derive(Deserialize)]
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

    /// Reconstruct a measurement from serialized stage evidence.
    ///
    /// The result still passes through [`Self::validate_complete`] before any
    /// duration or percentile can be computed.
    #[must_use]
    pub fn from_stages(
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

    /// Validate completeness, clock comparability, and pipeline monotonicity.
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
            Self::InvalidMeasurement { .. } => "EVIDENCE_INVALID_MEASUREMENT",
        }
    }
}

/// Collects proxy latency measurements and computes aggregate statistics.
///
/// The retained sample ring is bounded. Percentile queries first validate the
/// entire retained window; they never filter invalid samples into a false pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencyCollector {
    /// Raw measurements in recording order.
    measurements: VecDeque<InputLatencyMeasurement>,
    /// Maximum measurements to retain (ring buffer semantics).
    capacity: usize,
    /// Next measurement ID.
    next_id: u64,
    /// Terminal fail-stop marker set before the reserved ID boundary.
    id_exhausted: bool,
}

impl InputLatencyCollector {
    /// Create a new collector with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            measurements: VecDeque::with_capacity(capacity.min(4096)),
            capacity: capacity.max(1),
            next_id: 1,
            id_exhausted: false,
        }
    }

    /// Start a new measurement and return its handle.
    ///
    /// ID zero and `u64::MAX` are reserved. Reaching the terminal boundary
    /// permanently fail-stops this collector so ignored allocation errors
    /// cannot leave an apparently authoritative report.
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
        if next_id == u64::MAX {
            self.id_exhausted = true;
        }
        Ok(InputLatencyMeasurement::new(id))
    }

    /// Record a completed measurement.
    pub fn record(&mut self, measurement: InputLatencyMeasurement) {
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

    /// Validate the complete retained evidence window.
    pub fn validate_evidence(&self) -> Result<(), InputLatencyEvidenceError> {
        if self.id_exhausted || self.next_id == 0 || self.next_id == u64::MAX {
            return Err(InputLatencyEvidenceError::MeasurementIdExhausted);
        }
        if self.measurements.is_empty() {
            return Err(InputLatencyEvidenceError::EmptyCollector);
        }

        let mut ids = BTreeSet::new();
        for measurement in &self.measurements {
            if measurement.id == 0 || measurement.id == u64::MAX {
                return Err(InputLatencyEvidenceError::ReservedMeasurementId {
                    id: measurement.id,
                });
            }
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
    pub regression_threshold: f64,
}

#[derive(Deserialize)]
struct InputLatencyBudgetWire {
    stages: Vec<StageBudget>,
    aggregate: DuplicateRejectingMap<Percentile, u64>,
    regression_threshold: f64,
}

impl<'de> Deserialize<'de> for InputLatencyBudget {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = InputLatencyBudgetWire::deserialize(deserializer)?;
        Ok(Self {
            stages: wire.stages,
            aggregate: wire.aggregate.0,
            regression_threshold: wire.regression_threshold,
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
    #[error("regression threshold {value} must be finite and greater than zero")]
    InvalidRegressionThreshold { value: f64 },
    /// Two entries for one stage make configuration precedence ambiguous.
    #[error("stage {stage} has more than one budget entry")]
    DuplicateStageBudget { stage: InputLatencyStage },
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
            Self::EmptyStageTargets { .. } => "BUDGET_CONFIG_EMPTY_STAGE",
            Self::StageHasNoPredecessor { .. } => "BUDGET_CONFIG_NO_PREDECESSOR",
            Self::EffectiveBudgetOverflow { .. } => "BUDGET_CONFIG_OVERFLOW",
        }
    }
}

/// Result of evaluating proxy measurements against a budget.
#[derive(Debug, Clone, Serialize)]
pub struct BudgetCheckResult {
    /// Permanent authority boundary for this legacy framework.
    pub evidence_class: InputLatencyEvidenceClass,
    /// Number of retained samples presented to the gate.
    pub sample_count: usize,
    /// Whether all budget checks passed.
    pub passed: bool,
    /// Per-percentile results.
    pub details: Vec<BudgetCheckDetail>,
    /// Evidence failure, if the collector was not admissible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_error: Option<InputLatencyEvidenceError>,
    /// Budget configuration failure, if the gate itself was invalid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_error: Option<InputLatencyBudgetError>,
    /// Overall reason code.
    pub reason_code: String,
}

/// Detail for a single percentile budget check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetCheckDetail {
    /// `None` means aggregate KeyEvent -> GpuPresent; `Some(stage)` means
    /// `stage.predecessor() -> stage`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<InputLatencyStage>,
    /// The percentile checked.
    pub percentile: Percentile,
    /// Budget target in microseconds.
    pub budget_us: u64,
    /// Measured value in microseconds.
    pub measured_us: u64,
    /// Whether this check passed.
    pub passed: bool,
    /// Ratio of measured/budget (1.0 = exactly at budget).
    ///
    /// A zero budget has no finite ratio and is represented as `None`.
    pub ratio: Option<f64>,
    /// Reason code.
    pub reason_code: String,
}

#[derive(Deserialize)]
struct BudgetCheckResultWire {
    evidence_class: InputLatencyEvidenceClass,
    sample_count: usize,
    passed: bool,
    details: Vec<BudgetCheckDetail>,
    #[serde(default)]
    evidence_error: Option<InputLatencyEvidenceError>,
    #[serde(default)]
    budget_error: Option<InputLatencyBudgetError>,
    reason_code: String,
}

impl BudgetCheckResult {
    fn validate_contract(&self) -> Result<(), &'static str> {
        match (&self.evidence_error, &self.budget_error) {
            (Some(_), Some(_)) => {
                return Err("budget verdict cannot carry evidence and budget errors together");
            }
            (Some(error), None) => {
                if self.passed || !self.details.is_empty() {
                    return Err("evidence-error verdict must fail without details");
                }
                if self.reason_code != error.reason_code() {
                    return Err("evidence-error verdict reason code does not match its error");
                }
                return Ok(());
            }
            (None, Some(error)) => {
                if self.passed || !self.details.is_empty() {
                    return Err("budget-error verdict must fail without details");
                }
                if self.reason_code != error.reason_code() {
                    return Err("budget-error verdict reason code does not match its error");
                }
                return Ok(());
            }
            (None, None) => {}
        }

        if self.sample_count == 0 {
            return Err("detail-bearing budget verdict requires at least one sample");
        }
        if self.details.is_empty() {
            return Err("error-free budget verdict requires at least one detail");
        }

        for detail in &self.details {
            let expected_ratio = (detail.budget_us > 0)
                .then(|| detail.measured_us as f64 / detail.budget_us as f64);
            if detail.ratio != expected_ratio {
                return Err("budget detail ratio does not match measured and target values");
            }

            let expected_reason = match (detail.stage, detail.passed) {
                (None, true) => format!("BUDGET_OK_AGGREGATE_{}", detail.percentile),
                (None, false) => {
                    format!("BUDGET_EXCEEDED_AGGREGATE_{}", detail.percentile)
                }
                (Some(stage), true) => {
                    format!("BUDGET_OK_{}_{}", stage.label(), detail.percentile)
                }
                (Some(stage), false) => {
                    format!("BUDGET_EXCEEDED_{}_{}", stage.label(), detail.percentile)
                }
            };
            if detail.reason_code != expected_reason {
                return Err("budget detail reason code does not match its outcome");
            }
        }

        let all_details_passed = self.details.iter().all(|detail| detail.passed);
        if self.passed != all_details_passed {
            return Err("budget verdict disagrees with its detail outcomes");
        }
        let expected_reason = if self.passed {
            "ALL_PROXY_BUDGETS_MET"
        } else {
            "PROXY_BUDGET_VIOLATION"
        };
        if self.reason_code != expected_reason {
            return Err("budget verdict reason code does not match its outcome");
        }

        Ok(())
    }
}

impl<'de> Deserialize<'de> for BudgetCheckResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = BudgetCheckResultWire::deserialize(deserializer)?;
        let result = Self {
            evidence_class: wire.evidence_class,
            sample_count: wire.sample_count,
            passed: wire.passed,
            details: wire.details,
            evidence_error: wire.evidence_error,
            budget_error: wire.budget_error,
            reason_code: wire.reason_code,
        };
        result.validate_contract().map_err(de::Error::custom)?;
        Ok(result)
    }
}

fn validate_budget(budget: &InputLatencyBudget) -> Result<(), InputLatencyBudgetError> {
    if budget.aggregate.is_empty() {
        return Err(InputLatencyBudgetError::EmptyAggregateTargets);
    }
    if !budget.regression_threshold.is_finite() || budget.regression_threshold <= 0.0 {
        return Err(InputLatencyBudgetError::InvalidRegressionThreshold {
            value: budget.regression_threshold,
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
    const U64_EXCLUSIVE_MAX_AS_F64: f64 = 18_446_744_073_709_551_616.0;

    let scaled = budget_us as f64 * threshold;
    if !scaled.is_finite() || scaled >= U64_EXCLUSIVE_MAX_AS_F64 {
        return Err(InputLatencyBudgetError::EffectiveBudgetOverflow {
            budget_us,
            threshold,
        });
    }
    Ok(scaled.floor() as u64)
}

fn failed_budget_check(
    collector: &InputLatencyCollector,
    evidence_error: Option<InputLatencyEvidenceError>,
    budget_error: Option<InputLatencyBudgetError>,
    reason_code: &str,
) -> BudgetCheckResult {
    BudgetCheckResult {
        evidence_class: InputLatencyEvidenceClass::ProxyOnly,
        sample_count: collector.count(),
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
        return failed_budget_check(collector, None, Some(error), reason_code);
    }
    if let Err(error) = collector.validate_evidence() {
        let reason_code = error.reason_code();
        return failed_budget_check(collector, Some(error), None, reason_code);
    }

    let mut details = Vec::new();
    let mut all_passed = true;

    for (&percentile, &budget_us) in &budget.aggregate {
        let measured_us = match collector.total_latency_percentile(percentile) {
            Ok(value) => value,
            Err(error) => {
                let reason_code = error.reason_code();
                return failed_budget_check(collector, Some(error), None, reason_code);
            }
        };
        let effective_budget = match effective_budget_us(budget_us, budget.regression_threshold) {
            Ok(value) => value,
            Err(error) => {
                let reason_code = error.reason_code();
                return failed_budget_check(collector, None, Some(error), reason_code);
            }
        };
        let passed = measured_us <= effective_budget;
        let ratio = (budget_us > 0).then(|| measured_us as f64 / budget_us as f64);

        if !passed {
            all_passed = false;
        }

        details.push(BudgetCheckDetail {
            stage: None,
            percentile,
            budget_us,
            measured_us,
            passed,
            ratio,
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
            return failed_budget_check(collector, None, Some(error), reason_code);
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
                    return failed_budget_check(collector, Some(error), None, reason_code);
                }
            };
            let effective_budget =
                match effective_budget_us(budget_us, budget.regression_threshold) {
                    Ok(value) => value,
                    Err(error) => {
                        let reason_code = error.reason_code();
                        return failed_budget_check(collector, None, Some(error), reason_code);
                    }
                };
            let passed = measured_us <= effective_budget;
            let ratio = (budget_us > 0).then(|| measured_us as f64 / budget_us as f64);

            if !passed {
                all_passed = false;
            }

            details.push(BudgetCheckDetail {
                stage: Some(stage_budget.stage),
                percentile,
                budget_us,
                measured_us,
                passed,
                ratio,
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
        evidence_class: InputLatencyEvidenceClass::ProxyOnly,
        sample_count: collector.count(),
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

/// Structured proxy latency report suitable for offline regression output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputLatencyReport {
    /// Serialized report schema version.
    pub schema_version: u32,
    /// Permanent authority boundary.
    pub evidence_class: InputLatencyEvidenceClass,
    /// Whether the full retained window was admitted.
    pub evidence_status: InputLatencyEvidenceStatus,
    /// Typed fail-closed diagnosis for an invalid window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_error: Option<InputLatencyEvidenceError>,
    /// Number of measurements in the sample.
    pub sample_count: usize,
    /// Samples admitted to percentile computation; either all or zero.
    pub admitted_sample_count: usize,
    /// Per-percentile end-to-end latency in microseconds.
    pub percentiles: BTreeMap<Percentile, u64>,
    /// Per-stage breakdown at p50.
    pub stage_breakdown_p50: BTreeMap<String, u64>,
    /// Budget evaluation result (None if no budget configured).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget_check: Option<BudgetCheckResult>,
}

/// Generate a latency report from a collector with optional budget evaluation.
#[must_use]
pub fn generate_report(
    collector: &InputLatencyCollector,
    budget: Option<&InputLatencyBudget>,
) -> InputLatencyReport {
    let mut evidence_error = collector.validate_evidence().err();
    let percentiles = if evidence_error.is_none() {
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
        collector.record(make_measurement(7, 1000, 100));
        collector.record(make_measurement(7, 2000, 100));
        assert_eq!(
            collector.validate_evidence(),
            Err(InputLatencyEvidenceError::DuplicateMeasurementId { id: 7 })
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
    fn collector_serde_roundtrip() {
        let mut collector = InputLatencyCollector::new(50);
        for i in 0..5u64 {
            record_measurement(&mut collector, 1000 + i, 200);
        }
        let json = serde_json::to_string(&collector).unwrap();
        let back: InputLatencyCollector = serde_json::from_str(&json).unwrap();
        assert_eq!(back.count(), 5);
    }

    #[test]
    fn budget_serde_roundtrip() {
        let budget = InputLatencyBudget::default();
        let json = serde_json::to_string(&budget).unwrap();
        let back: InputLatencyBudget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.aggregate.len(), budget.aggregate.len());
        assert!((back.regression_threshold - budget.regression_threshold).abs() < 1e-9);
    }

    #[test]
    fn report_serde_roundtrip() {
        let mut collector = InputLatencyCollector::new(100);
        for i in 0..10u64 {
            record_measurement(&mut collector, 1000 + i, 300);
        }
        let report = generate_report(&collector, Some(&InputLatencyBudget::default()));
        let json = serde_json::to_string(&report).unwrap();
        let back: InputLatencyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back.sample_count, report.sample_count);
    }

    #[test]
    fn budget_check_result_serde_roundtrip() {
        let result = BudgetCheckResult {
            evidence_class: InputLatencyEvidenceClass::ProxyOnly,
            sample_count: 1,
            passed: true,
            details: vec![BudgetCheckDetail {
                stage: None,
                percentile: Percentile::P50,
                budget_us: 2000,
                measured_us: 1500,
                passed: true,
                ratio: Some(0.75),
                reason_code: "BUDGET_OK_AGGREGATE_p50".to_string(),
            }],
            evidence_error: None,
            budget_error: None,
            reason_code: "ALL_PROXY_BUDGETS_MET".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: BudgetCheckResult = serde_json::from_str(&json).unwrap();
        assert!(back.passed);
        assert_eq!(back.details.len(), 1);
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
    fn provenance_ids_reserve_zero() {
        assert_eq!(InputLatencyProducerId::new(0), None);
        assert_eq!(InputLatencyClockDomainId::new(0), None);
        assert_eq!(InputLatencyProducerId::new(7).unwrap().get(), 7);
        assert_eq!(InputLatencyClockDomainId::new(9).unwrap().get(), 9);
    }
}
