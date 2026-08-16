//! Bounded, content-free interaction flight-recorder manifest contract.
//!
//! This module freezes recorder-wide identity, capacity, sampling, accounting,
//! lifecycle, export, and certification semantics.  It is deliberately a DTO
//! and pure-validation layer: the operational recorder lives in a lower-level
//! runtime crate, and platform marker emission remains a separate loss domain.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::interaction_trace_v2::{
    InteractionTraceId, InteractionTracePath, InteractionTraceTimestamp,
    MAX_INTERACTION_TRACE_EVENTS,
};

/// Exact numeric schema version for [`RecorderEpochManifestV1`].
pub const RECORDER_EPOCH_MANIFEST_SCHEMA_VERSION: u16 = 1;
/// Exact numeric schema version for [`SampledTraceContextV1`].
pub const SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION: u16 = 1;
/// Maximum number of queues across one recorder epoch.
pub const MAX_SHARDS: u16 = 256;
/// Maximum global slot count across all queues in one recorder epoch.
pub const MAX_TOTAL_SLOTS: u32 = 1_048_576;
/// Maximum semantic in-memory size admitted for the operational raw event.
pub const MAX_RAW_EVENT_BYTES: u16 = 512;
/// Maximum total memory reservation admitted for one recorder epoch (1 GiB).
pub const MAX_RESERVED_BYTES: u64 = 1 << 30;
/// One trace conversion workspace is bounded by the trace-v2 event ceiling.
pub const CONVERSION_WORKSPACE_EVENTS: u16 = MAX_INTERACTION_TRACE_EVENTS as u16;

/// Opaque identity of one immutable local recorder configuration epoch.
///
/// This is intentionally distinct from an originating
/// [`crate::interaction_trace_v2::InteractionTraceRunId`]. A local recorder
/// can retain remote fragments from multiple originating runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderEpochId {
    pub nonce_hi: u64,
    pub nonce_lo: u64,
}

impl RecorderEpochId {
    #[must_use]
    pub const fn new(nonce_hi: u64, nonce_lo: u64) -> Option<Self> {
        if nonce_hi == 0 && nonce_lo == 0 {
            None
        } else {
            Some(Self { nonce_hi, nonce_lo })
        }
    }

    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.nonce_hi != 0 || self.nonce_lo != 0
    }
}

/// Immutable recorder operating mode within one epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderMode {
    /// No clock, ID, sampler, counter, queue, marker, or export side effect.
    Off,
    /// Deterministically sampled diagnostic recording.
    Low,
    /// Complete internal recording eligible for certification.
    Certification,
    /// Complete internal recording plus independent platform-marker attempts.
    ///
    /// Marker delivery remains a separate loss domain and cannot strengthen
    /// the internal recorder authority.
    CertificationWithMarkers,
}

/// Closed, versioned whole-trace sampler vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderSamplerAlgorithm {
    SplitMix64V1,
}

/// Immutable whole-trace sampler configuration for one recorder epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderSamplerConfigV1 {
    pub algorithm: RecorderSamplerAlgorithm,
    pub numerator: u64,
    pub denominator: u64,
    pub seed_hi: u64,
    pub seed_lo: u64,
}

impl RecorderSamplerConfigV1 {
    /// Canonical sampler for a recorder that must remain observationally off.
    #[must_use]
    pub const fn off() -> Self {
        Self {
            algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
            numerator: 0,
            denominator: 1,
            seed_hi: 0,
            seed_lo: 0,
        }
    }

    /// Canonical sampler for complete certification recording.
    #[must_use]
    pub const fn certification() -> Self {
        Self {
            algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
            numerator: 1,
            denominator: 1,
            seed_hi: 0,
            seed_lo: 0,
        }
    }

    #[must_use]
    pub const fn is_full_sampling(self) -> bool {
        self.denominator != 0 && self.numerator == self.denominator
    }

    #[must_use]
    pub const fn is_canonical_off(self) -> bool {
        self.numerator == 0 && self.denominator == 1 && self.seed_hi == 0 && self.seed_lo == 0
    }

    pub fn validate(self) -> Result<(), RecorderContractError> {
        if self.denominator == 0 || self.numerator > self.denominator {
            return Err(RecorderContractError::InvalidSamplerRatio {
                numerator: self.numerator,
                denominator: self.denominator,
            });
        }
        Ok(())
    }

    pub fn validate_for_mode(self, mode: RecorderMode) -> Result<(), RecorderContractError> {
        self.validate()?;
        let valid = match mode {
            RecorderMode::Off => self.is_canonical_off(),
            RecorderMode::Low => true,
            RecorderMode::Certification | RecorderMode::CertificationWithMarkers => {
                self.is_full_sampling()
            }
        };
        if valid {
            Ok(())
        } else {
            Err(RecorderContractError::InvalidSamplerForMode { mode })
        }
    }

    /// Return the deterministic SplitMix64V1 hash for an originating trace.
    pub fn hash(self, trace_id: InteractionTraceId) -> Result<u64, RecorderContractError> {
        self.validate()?;
        match self.algorithm {
            RecorderSamplerAlgorithm::SplitMix64V1 => {
                splitmix64_v1_hash(trace_id, self.seed_hi, self.seed_lo)
            }
        }
    }

    /// Map the trace hash uniformly into `[0, denominator)` using multiply-high.
    pub fn bucket(self, trace_id: InteractionTraceId) -> Result<u64, RecorderContractError> {
        let hash = self.hash(trace_id)?;
        Ok(multiply_high_u64(hash, self.denominator))
    }

    /// Make the one whole-trace sampling decision for an enabled recorder.
    pub fn samples(self, trace_id: InteractionTraceId) -> Result<bool, RecorderContractError> {
        Ok(self.bucket(trace_id)? < self.numerator)
    }
}

/// Integer-only SplitMix64V1 semantic hash.
pub fn splitmix64_v1_hash(
    trace_id: InteractionTraceId,
    seed_hi: u64,
    seed_lo: u64,
) -> Result<u64, RecorderContractError> {
    if !trace_id.is_valid() {
        return Err(RecorderContractError::InvalidTraceId);
    }
    let input = trace_id.run_id.epoch_nonce_hi
        ^ trace_id.run_id.epoch_nonce_lo.rotate_left(17)
        ^ trace_id.sequence.rotate_left(31)
        ^ seed_hi
        ^ seed_lo.rotate_left(47);
    Ok(splitmix64_v1_mix(input))
}

#[must_use]
pub const fn splitmix64_v1_mix(input: u64) -> u64 {
    let mut value = input.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[must_use]
pub const fn multiply_high_u64(left: u64, right: u64) -> u64 {
    (((left as u128) * (right as u128)) >> 64) as u64
}

/// Why a new immutable recorder epoch began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderEpochStartReason {
    ProcessStart,
    ModeChanged,
    ConfigurationChanged,
    Recovery,
}

/// Why admission into an immutable recorder epoch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderEpochCloseReason {
    ModeChanged,
    ConfigurationChanged,
    NormalShutdown,
    CrashAdjacentShutdown,
}

/// Linear recorder lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderLifecycleState {
    Active,
    Closing,
    Closed,
}

/// Deterministic division of one global slot budget across bounded shards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderShardDistributionV1 {
    pub base_slots_per_shard: u32,
    pub remainder_shards: u16,
}

/// Conservative memory inputs for one fixed-total-memory recorder epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderCapacityV1 {
    pub shard_count: u16,
    pub total_slots: u32,
    pub raw_event_bytes: u16,
    pub queue_slot_overhead_bytes: u16,
    pub queue_header_bytes_per_shard: u32,
    pub padded_counter_bytes_per_shard: u32,
    pub shard_metadata_bytes_per_shard: u32,
    pub frozen_export_slot_bytes: u16,
    pub conversion_event_bytes: u32,
    pub serialization_workspace_bytes: u64,
    pub configured_byte_ceiling: u64,
}

impl RecorderCapacityV1 {
    pub fn checked_shard_distribution(
        self,
    ) -> Result<RecorderShardDistributionV1, RecorderContractError> {
        if self.shard_count == 0 || self.shard_count > MAX_SHARDS {
            return Err(RecorderContractError::CapacityOutOfRange {
                field: "shard_count",
                actual: u64::from(self.shard_count),
                maximum: u64::from(MAX_SHARDS),
            });
        }
        if self.total_slots == 0 || self.total_slots > MAX_TOTAL_SLOTS {
            return Err(RecorderContractError::CapacityOutOfRange {
                field: "total_slots",
                actual: u64::from(self.total_slots),
                maximum: u64::from(MAX_TOTAL_SLOTS),
            });
        }
        if u32::from(self.shard_count) > self.total_slots {
            return Err(RecorderContractError::InvalidShardDistribution);
        }

        let shard_count = u32::from(self.shard_count);
        let base_slots_per_shard = self.total_slots / shard_count;
        let remainder = self.total_slots % shard_count;
        let remainder_shards = u16::try_from(remainder)
            .map_err(|_| RecorderContractError::InvalidShardDistribution)?;
        let reconstructed = base_slots_per_shard
            .checked_mul(shard_count)
            .and_then(|base| base.checked_add(u32::from(remainder_shards)))
            .ok_or(RecorderContractError::CapacityArithmeticOverflow)?;
        if reconstructed != self.total_slots || remainder_shards >= self.shard_count {
            return Err(RecorderContractError::InvalidShardDistribution);
        }
        Ok(RecorderShardDistributionV1 {
            base_slots_per_shard,
            remainder_shards,
        })
    }

    /// Compute the complete conservative reservation with checked arithmetic.
    pub fn checked_reserved_bytes(self) -> Result<u64, RecorderContractError> {
        let slot_reservation = u64::from(self.raw_event_bytes)
            .checked_add(u64::from(self.queue_slot_overhead_bytes))
            .and_then(|bytes| bytes.checked_add(u64::from(self.frozen_export_slot_bytes)))
            .ok_or(RecorderContractError::CapacityArithmeticOverflow)?;
        let all_slots = u64::from(self.total_slots)
            .checked_mul(slot_reservation)
            .ok_or(RecorderContractError::CapacityArithmeticOverflow)?;

        let per_shard_reservation = u64::from(self.queue_header_bytes_per_shard)
            .checked_add(u64::from(self.padded_counter_bytes_per_shard))
            .and_then(|bytes| bytes.checked_add(u64::from(self.shard_metadata_bytes_per_shard)))
            .ok_or(RecorderContractError::CapacityArithmeticOverflow)?;
        let all_shards = u64::from(self.shard_count)
            .checked_mul(per_shard_reservation)
            .ok_or(RecorderContractError::CapacityArithmeticOverflow)?;

        let conversion_workspace = u64::from(CONVERSION_WORKSPACE_EVENTS)
            .checked_mul(u64::from(self.conversion_event_bytes))
            .ok_or(RecorderContractError::CapacityArithmeticOverflow)?;

        all_slots
            .checked_add(all_shards)
            .and_then(|bytes| bytes.checked_add(conversion_workspace))
            .and_then(|bytes| bytes.checked_add(self.serialization_workspace_bytes))
            .ok_or(RecorderContractError::CapacityArithmeticOverflow)
    }

    pub fn validate(self) -> Result<RecorderShardDistributionV1, RecorderContractError> {
        let distribution = self.checked_shard_distribution()?;
        if self.raw_event_bytes == 0 || self.raw_event_bytes > MAX_RAW_EVENT_BYTES {
            return Err(RecorderContractError::CapacityOutOfRange {
                field: "raw_event_bytes",
                actual: u64::from(self.raw_event_bytes),
                maximum: u64::from(MAX_RAW_EVENT_BYTES),
            });
        }
        for (field, actual) in [
            (
                "queue_slot_overhead_bytes",
                u64::from(self.queue_slot_overhead_bytes),
            ),
            (
                "queue_header_bytes_per_shard",
                u64::from(self.queue_header_bytes_per_shard),
            ),
            (
                "padded_counter_bytes_per_shard",
                u64::from(self.padded_counter_bytes_per_shard),
            ),
            (
                "shard_metadata_bytes_per_shard",
                u64::from(self.shard_metadata_bytes_per_shard),
            ),
        ] {
            if actual == 0 {
                return Err(RecorderContractError::CapacityComponentTooSmall {
                    field,
                    actual,
                    minimum: 1,
                });
            }
        }
        if self.frozen_export_slot_bytes < self.raw_event_bytes {
            return Err(RecorderContractError::CapacityComponentTooSmall {
                field: "frozen_export_slot_bytes",
                actual: u64::from(self.frozen_export_slot_bytes),
                minimum: u64::from(self.raw_event_bytes),
            });
        }
        if self.conversion_event_bytes == 0 {
            return Err(RecorderContractError::CapacityComponentTooSmall {
                field: "conversion_event_bytes",
                actual: 0,
                minimum: 1,
            });
        }
        if self.serialization_workspace_bytes == 0 {
            return Err(RecorderContractError::CapacityComponentTooSmall {
                field: "serialization_workspace_bytes",
                actual: 0,
                minimum: 1,
            });
        }
        if self.configured_byte_ceiling == 0 || self.configured_byte_ceiling > MAX_RESERVED_BYTES {
            return Err(RecorderContractError::CapacityOutOfRange {
                field: "configured_byte_ceiling",
                actual: self.configured_byte_ceiling,
                maximum: MAX_RESERVED_BYTES,
            });
        }
        let reserved = self.checked_reserved_bytes()?;
        if reserved > self.configured_byte_ceiling {
            return Err(RecorderContractError::CapacityReservationExceeded {
                reserved,
                ceiling: self.configured_byte_ceiling,
            });
        }
        Ok(distribution)
    }
}

/// Whole-trace admission accounting for enabled recorder calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderTraceAccountingV1 {
    pub sampled_in: u64,
    pub sampled_out: u64,
    pub trace_id_exhausted: u64,
}

impl RecorderTraceAccountingV1 {
    pub fn checked_enabled_trace_attempts(self) -> Result<u64, RecorderContractError> {
        self.sampled_in
            .checked_add(self.sampled_out)
            .and_then(|total| total.checked_add(self.trace_id_exhausted))
            .ok_or(RecorderContractError::AccountingOverflow { domain: "trace" })
    }
}

/// Event-publication accounting for traces admitted by whole-trace sampling.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderEventAccountingV1 {
    pub recorded: u64,
    pub queue_full: u64,
    pub closing: u64,
    pub clock_invalid: u64,
    pub epoch_mismatch: u64,
}

impl RecorderEventAccountingV1 {
    pub fn checked_sampled_event_attempts(self) -> Result<u64, RecorderContractError> {
        self.recorded
            .checked_add(self.queue_full)
            .and_then(|total| total.checked_add(self.closing))
            .and_then(|total| total.checked_add(self.clock_invalid))
            .and_then(|total| total.checked_add(self.epoch_mismatch))
            .ok_or(RecorderContractError::AccountingOverflow { domain: "event" })
    }

    #[must_use]
    pub const fn is_lossless(self) -> bool {
        self.queue_full == 0
            && self.closing == 0
            && self.clock_invalid == 0
            && self.epoch_mismatch == 0
    }
}

/// Authority of the retained aggregate counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderAccountingAuthority {
    Exact,
    Exhausted,
}

impl RecorderAccountingAuthority {
    /// Counter exhaustion is sticky and cannot be restored to exact authority.
    #[must_use]
    pub const fn after_exhaustion(self) -> Self {
        let _ = self;
        Self::Exhausted
    }
}

/// Result of bounded recorder shutdown and queue freezing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecorderShutdownStatusV1 {
    NotStarted,
    InProgress,
    Completed {
        frozen_events: u64,
    },
    Incomplete {
        frozen_events: u64,
        in_flight_operations: u64,
    },
    /// Best effort after recoverable unwind/cancellation or supervisor control.
    /// This does not claim SIGKILL, abort, segfault, OOM, or signal-handler I/O.
    CrashAdjacentIncomplete {
        frozen_events: u64,
    },
}

impl RecorderShutdownStatusV1 {
    #[must_use]
    pub const fn frozen_events(self) -> Option<u64> {
        match self {
            Self::NotStarted | Self::InProgress => None,
            Self::Completed { frozen_events }
            | Self::Incomplete { frozen_events, .. }
            | Self::CrashAdjacentIncomplete { frozen_events } => Some(frozen_events),
        }
    }
}

/// Result of deterministic off-path export of one frozen epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecorderExportStatusV1 {
    NotAttempted,
    Completed {
        exported_events: u64,
    },
    Incomplete {
        exported_events: u64,
        retained_events: u64,
    },
}

/// Independent platform-marker evidence authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformMarkerAuthorityV1 {
    NotRequested,
    /// Exactly one platform marker was eligible for every recorded event.
    ExactEveryRecordedEvent,
    Inexact,
}

/// Independent platform-marker outcome accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformMarkerAccountingV1 {
    pub attempted: u64,
    pub emitted: u64,
    pub unavailable: u64,
    pub dropped: u64,
    pub loss_unknown: bool,
}

impl PlatformMarkerAccountingV1 {
    pub fn checked_classified(self) -> Result<u64, RecorderContractError> {
        self.emitted
            .checked_add(self.unavailable)
            .and_then(|total| total.checked_add(self.dropped))
            .ok_or(RecorderContractError::MarkerAccountingOverflow)
    }

    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.attempted == 0
            && self.emitted == 0
            && self.unavailable == 0
            && self.dropped == 0
            && !self.loss_unknown
    }
}

/// Certification question being asked of one recorder epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderCertificationClass {
    InternalRecorderCertification,
    MarkerAssistedCertification,
}

/// Fail-closed result for one structurally valid certification question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecorderCertificationVerdict {
    Qualifying,
    NonQualifying,
}

/// One bounded, content-free immutable recorder epoch manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecorderEpochManifestV1 {
    pub schema_version: u16,
    pub epoch_id: RecorderEpochId,
    pub previous_epoch_id: Option<RecorderEpochId>,
    pub mode: RecorderMode,
    pub sampler: RecorderSamplerConfigV1,
    pub start_reason: RecorderEpochStartReason,
    pub close_reason: Option<RecorderEpochCloseReason>,
    pub lifecycle: RecorderLifecycleState,
    pub started_at: InteractionTraceTimestamp,
    pub closed_at: Option<InteractionTraceTimestamp>,
    pub capacity: RecorderCapacityV1,
    pub trace_accounting: RecorderTraceAccountingV1,
    pub event_accounting: RecorderEventAccountingV1,
    pub accounting_authority: RecorderAccountingAuthority,
    pub shutdown: RecorderShutdownStatusV1,
    pub export: RecorderExportStatusV1,
    pub marker_authority: PlatformMarkerAuthorityV1,
    pub marker_accounting: PlatformMarkerAccountingV1,
}

impl RecorderEpochManifestV1 {
    /// Validate only the internal recorder authority domain.
    ///
    /// Marker ambiguity or malformed marker evidence is intentionally not read
    /// here, so it cannot strengthen or invalidate an internal-only claim.
    pub fn validate_internal_contract(self) -> Result<(), RecorderContractError> {
        if self.schema_version != RECORDER_EPOCH_MANIFEST_SCHEMA_VERSION {
            return Err(RecorderContractError::UnsupportedSchemaVersion {
                expected: RECORDER_EPOCH_MANIFEST_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }
        if !self.epoch_id.is_valid() {
            return Err(RecorderContractError::InvalidEpochId);
        }
        if self
            .previous_epoch_id
            .is_some_and(|previous_epoch_id| !previous_epoch_id.is_valid())
        {
            return Err(RecorderContractError::InvalidEpochId);
        }
        if self.previous_epoch_id == Some(self.epoch_id) {
            return Err(RecorderContractError::InvalidEpochTransition {
                detail: "epoch cannot name itself as predecessor",
            });
        }
        match self.start_reason {
            RecorderEpochStartReason::ProcessStart | RecorderEpochStartReason::Recovery
                if self.previous_epoch_id.is_some() =>
            {
                return Err(RecorderContractError::InvalidEpochTransition {
                    detail: "process start or recovery unexpectedly names a predecessor",
                });
            }
            RecorderEpochStartReason::ModeChanged
            | RecorderEpochStartReason::ConfigurationChanged
                if self.previous_epoch_id.is_none() =>
            {
                return Err(RecorderContractError::InvalidEpochTransition {
                    detail: "mode or configuration change lacks a predecessor",
                });
            }
            _ => {}
        }
        self.sampler.validate_for_mode(self.mode)?;
        self.capacity.validate()?;
        let trace_attempts = self.trace_accounting.checked_enabled_trace_attempts()?;
        let event_attempts = self.event_accounting.checked_sampled_event_attempts()?;
        let maximum_event_attempts = self
            .trace_accounting
            .sampled_in
            .checked_mul(MAX_INTERACTION_TRACE_EVENTS as u64)
            .ok_or(RecorderContractError::AccountingOverflow {
                domain: "event ceiling",
            })?;
        validate_epoch_timestamp(self.started_at)?;
        self.validate_lifecycle()?;

        if self.trace_accounting.sampled_in == 0 && event_attempts != 0 {
            return Err(RecorderContractError::InvalidAccounting {
                detail: "event attempts require an admitted trace",
            });
        }
        if event_attempts > maximum_event_attempts {
            return Err(RecorderContractError::InvalidAccounting {
                detail: "event attempts exceed the trace-schema event ceiling",
            });
        }
        if self.mode == RecorderMode::Off {
            if trace_attempts != 0
                || event_attempts != 0
                || self.accounting_authority != RecorderAccountingAuthority::Exact
                || self.marker_authority != PlatformMarkerAuthorityV1::NotRequested
                || !self.marker_accounting.is_zero()
            {
                return Err(RecorderContractError::OffModeHadSideEffects);
            }
        }
        Ok(())
    }

    /// Validate only the independent platform-marker accounting domain.
    pub fn validate_marker_contract(self) -> Result<(), RecorderContractError> {
        let classified = self.marker_accounting.checked_classified()?;
        if self.mode != RecorderMode::CertificationWithMarkers {
            if self.marker_authority != PlatformMarkerAuthorityV1::NotRequested
                || !self.marker_accounting.is_zero()
            {
                return Err(RecorderContractError::InvalidMarkerAccounting {
                    detail: "platform markers require certification-with-markers mode",
                });
            }
            return Ok(());
        }
        match self.marker_authority {
            PlatformMarkerAuthorityV1::NotRequested => {
                return Err(RecorderContractError::InvalidMarkerAccounting {
                    detail: "marker-enabled mode lacks platform-marker authority",
                });
            }
            PlatformMarkerAuthorityV1::ExactEveryRecordedEvent => {
                if self.marker_accounting.loss_unknown
                    || classified != self.marker_accounting.attempted
                    || self.marker_accounting.attempted != self.event_accounting.recorded
                {
                    return Err(RecorderContractError::InvalidMarkerAccounting {
                        detail: "exact marker authority is incomplete or miscounted",
                    });
                }
            }
            PlatformMarkerAuthorityV1::Inexact => {
                if classified > self.marker_accounting.attempted
                    || (!self.marker_accounting.loss_unknown
                        && classified != self.marker_accounting.attempted)
                {
                    return Err(RecorderContractError::InvalidMarkerAccounting {
                        detail: "inexact marker outcomes exceed or contradict attempts",
                    });
                }
            }
        }
        Ok(())
    }

    /// Return a class-specific verdict without allowing marker ambiguity to
    /// affect internal evidence.
    pub fn certification_verdict(
        self,
        class: RecorderCertificationClass,
    ) -> Result<RecorderCertificationVerdict, RecorderContractError> {
        self.validate_internal_contract()?;
        let internal_qualifies = matches!(
            self.mode,
            RecorderMode::Certification | RecorderMode::CertificationWithMarkers
        ) && self.sampler.is_full_sampling()
            && self.accounting_authority == RecorderAccountingAuthority::Exact
            && self.trace_accounting.sampled_in != 0
            && self.trace_accounting.sampled_out == 0
            && self.trace_accounting.trace_id_exhausted == 0
            && self.event_accounting.recorded != 0
            && self.event_accounting.is_lossless()
            && self.lifecycle == RecorderLifecycleState::Closed
            && matches!(
                self.shutdown,
                RecorderShutdownStatusV1::Completed { frozen_events }
                    if frozen_events == self.event_accounting.recorded
            )
            && matches!(
                self.export,
                RecorderExportStatusV1::Completed { exported_events }
                    if exported_events == self.event_accounting.recorded
            );
        if !internal_qualifies {
            return Ok(RecorderCertificationVerdict::NonQualifying);
        }
        if class == RecorderCertificationClass::InternalRecorderCertification {
            return Ok(RecorderCertificationVerdict::Qualifying);
        }

        self.validate_marker_contract()?;
        let markers_qualify = self.marker_authority
            == PlatformMarkerAuthorityV1::ExactEveryRecordedEvent
            && !self.marker_accounting.loss_unknown
            && self.marker_accounting.unavailable == 0
            && self.marker_accounting.dropped == 0
            && self.marker_accounting.emitted == self.event_accounting.recorded;
        Ok(if markers_qualify {
            RecorderCertificationVerdict::Qualifying
        } else {
            RecorderCertificationVerdict::NonQualifying
        })
    }

    fn validate_lifecycle(self) -> Result<(), RecorderContractError> {
        match self.lifecycle {
            RecorderLifecycleState::Active => {
                if self.close_reason.is_some()
                    || self.closed_at.is_some()
                    || self.shutdown != RecorderShutdownStatusV1::NotStarted
                    || self.export != RecorderExportStatusV1::NotAttempted
                {
                    return Err(RecorderContractError::InvalidLifecycle {
                        detail: "active epoch carries close, shutdown, or export state",
                    });
                }
            }
            RecorderLifecycleState::Closing => {
                if self.close_reason.is_none()
                    || self.closed_at.is_some()
                    || self.shutdown != RecorderShutdownStatusV1::InProgress
                    || self.export != RecorderExportStatusV1::NotAttempted
                {
                    return Err(RecorderContractError::InvalidLifecycle {
                        detail: "closing epoch lacks the canonical in-progress state",
                    });
                }
            }
            RecorderLifecycleState::Closed => {
                let closed_at = self
                    .closed_at
                    .ok_or(RecorderContractError::InvalidLifecycle {
                        detail: "closed epoch lacks close timestamp",
                    })?;
                if self.close_reason.is_none() {
                    return Err(RecorderContractError::InvalidLifecycle {
                        detail: "closed epoch lacks close reason",
                    });
                }
                match (self.close_reason, self.shutdown) {
                    (
                        Some(RecorderEpochCloseReason::CrashAdjacentShutdown),
                        RecorderShutdownStatusV1::CrashAdjacentIncomplete { .. },
                    ) => {}
                    (Some(RecorderEpochCloseReason::CrashAdjacentShutdown), _)
                    | (
                        Some(
                            RecorderEpochCloseReason::ModeChanged
                            | RecorderEpochCloseReason::ConfigurationChanged
                            | RecorderEpochCloseReason::NormalShutdown,
                        ),
                        RecorderShutdownStatusV1::CrashAdjacentIncomplete { .. },
                    ) => {
                        return Err(RecorderContractError::InvalidShutdownStatus {
                            detail: "crash-adjacent close reason and shutdown status disagree",
                        });
                    }
                    _ => {}
                }
                validate_epoch_timestamp(closed_at)?;
                if self.started_at.clock_domain != closed_at.clock_domain
                    || closed_at.monotonic_ns < self.started_at.monotonic_ns
                {
                    return Err(RecorderContractError::InvalidEpochClock);
                }
                self.validate_terminal_shutdown_and_export()?;
            }
        }
        Ok(())
    }

    fn validate_terminal_shutdown_and_export(self) -> Result<(), RecorderContractError> {
        let frozen_events =
            self.shutdown
                .frozen_events()
                .ok_or(RecorderContractError::InvalidShutdownStatus {
                    detail: "closed epoch has nonterminal shutdown status",
                })?;
        if frozen_events > self.event_accounting.recorded {
            return Err(RecorderContractError::InvalidShutdownStatus {
                detail: "frozen event count exceeds recorded events",
            });
        }
        if matches!(self.shutdown, RecorderShutdownStatusV1::Completed { .. })
            && frozen_events != self.event_accounting.recorded
        {
            return Err(RecorderContractError::InvalidShutdownStatus {
                detail: "completed shutdown did not freeze every recorded event",
            });
        }
        if matches!(
            self.shutdown,
            RecorderShutdownStatusV1::Incomplete {
                in_flight_operations: 0,
                ..
            }
        ) && frozen_events == self.event_accounting.recorded
        {
            return Err(RecorderContractError::InvalidShutdownStatus {
                detail: "incomplete shutdown has no incomplete work",
            });
        }

        match self.export {
            RecorderExportStatusV1::NotAttempted => {}
            RecorderExportStatusV1::Completed { exported_events } => {
                if exported_events != frozen_events {
                    return Err(RecorderContractError::InvalidExportStatus {
                        detail: "completed export count differs from frozen batch",
                    });
                }
            }
            RecorderExportStatusV1::Incomplete {
                exported_events,
                retained_events,
            } => {
                if retained_events == 0 {
                    return Err(RecorderContractError::InvalidExportStatus {
                        detail: "incomplete export retains no unexported events",
                    });
                }
                let accounted = exported_events
                    .checked_add(retained_events)
                    .ok_or(RecorderContractError::AccountingOverflow { domain: "export" })?;
                if accounted != frozen_events {
                    return Err(RecorderContractError::InvalidExportStatus {
                        detail: "incomplete export does not account for frozen batch",
                    });
                }
            }
        }
        Ok(())
    }
}

/// Validate one explicit same-process immutable-epoch transition.
pub fn validate_epoch_transition(
    previous: RecorderEpochManifestV1,
    next: RecorderEpochManifestV1,
) -> Result<(), RecorderContractError> {
    previous.validate_internal_contract()?;
    next.validate_internal_contract()?;
    if previous.lifecycle != RecorderLifecycleState::Closed {
        return Err(RecorderContractError::InvalidEpochTransition {
            detail: "predecessor is not closed",
        });
    }
    if previous.epoch_id == next.epoch_id || next.previous_epoch_id != Some(previous.epoch_id) {
        return Err(RecorderContractError::InvalidEpochTransition {
            detail: "successor does not name a distinct predecessor",
        });
    }

    match (previous.close_reason, next.start_reason) {
        (Some(RecorderEpochCloseReason::ModeChanged), RecorderEpochStartReason::ModeChanged)
            if previous.mode != next.mode => {}
        (
            Some(RecorderEpochCloseReason::ConfigurationChanged),
            RecorderEpochStartReason::ConfigurationChanged,
        ) if previous.mode == next.mode
            && (previous.sampler != next.sampler || previous.capacity != next.capacity) => {}
        _ => {
            return Err(RecorderContractError::InvalidEpochTransition {
                detail: "close/start reasons or immutable configuration do not match",
            });
        }
    }

    let previous_closed_at =
        previous
            .closed_at
            .ok_or(RecorderContractError::InvalidEpochTransition {
                detail: "predecessor lacks close timestamp",
            })?;
    if previous_closed_at.clock_domain != next.started_at.clock_domain
        || next.started_at.monotonic_ns < previous_closed_at.monotonic_ns
    {
        return Err(RecorderContractError::InvalidEpochTransition {
            detail: "same-process epoch transition regresses or changes clock domain",
        });
    }
    Ok(())
}

/// Bounded context propagated only for a whole trace already sampled in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampledTraceContextV1 {
    pub schema_version: u16,
    pub trace_id: InteractionTraceId,
    pub path: InteractionTracePath,
    pub origin_recorder_epoch_id: RecorderEpochId,
    pub sampler_algorithm: RecorderSamplerAlgorithm,
}

impl SampledTraceContextV1 {
    pub fn validate(self) -> Result<(), RecorderContractError> {
        if self.schema_version != SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION {
            return Err(RecorderContractError::UnsupportedSchemaVersion {
                expected: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
                actual: self.schema_version,
            });
        }
        if !self.trace_id.is_valid() {
            return Err(RecorderContractError::InvalidTraceId);
        }
        if !self.origin_recorder_epoch_id.is_valid() {
            return Err(RecorderContractError::InvalidEpochId);
        }
        Ok(())
    }
}

fn validate_epoch_timestamp(
    timestamp: InteractionTraceTimestamp,
) -> Result<(), RecorderContractError> {
    if timestamp.clock_domain.host_id == 0
        || timestamp.clock_domain.process_generation == 0
        || timestamp.clock_domain.clock_id == 0
    {
        return Err(RecorderContractError::InvalidEpochClock);
    }
    Ok(())
}

/// Fail-closed recorder manifest and validation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecorderContractError {
    UnsupportedSchemaVersion {
        expected: u16,
        actual: u16,
    },
    InvalidEpochId,
    InvalidTraceId,
    InvalidSamplerRatio {
        numerator: u64,
        denominator: u64,
    },
    InvalidSamplerForMode {
        mode: RecorderMode,
    },
    CapacityOutOfRange {
        field: &'static str,
        actual: u64,
        maximum: u64,
    },
    CapacityComponentTooSmall {
        field: &'static str,
        actual: u64,
        minimum: u64,
    },
    CapacityArithmeticOverflow,
    CapacityReservationExceeded {
        reserved: u64,
        ceiling: u64,
    },
    InvalidShardDistribution,
    AccountingOverflow {
        domain: &'static str,
    },
    InvalidAccounting {
        detail: &'static str,
    },
    MarkerAccountingOverflow,
    InvalidEpochClock,
    InvalidLifecycle {
        detail: &'static str,
    },
    InvalidEpochTransition {
        detail: &'static str,
    },
    InvalidShutdownStatus {
        detail: &'static str,
    },
    InvalidExportStatus {
        detail: &'static str,
    },
    InvalidMarkerAccounting {
        detail: &'static str,
    },
    OffModeHadSideEffects,
}

impl fmt::Display for RecorderContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "flight recorder v1 contract violation: {self:?}")
    }
}

impl std::error::Error for RecorderContractError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interaction_trace_v2::{InteractionTraceClockDomain, InteractionTraceRunId};
    use proptest::prelude::*;

    fn epoch_id(value: u64) -> RecorderEpochId {
        RecorderEpochId::new(0xfeed, value).expect("test epoch is non-zero")
    }

    fn trace_id(run_lo: u64, sequence: u64) -> InteractionTraceId {
        trace_id_for_run(0xfeed, run_lo, sequence)
    }

    fn trace_id_for_run(run_hi: u64, run_lo: u64, sequence: u64) -> InteractionTraceId {
        InteractionTraceId::new(
            InteractionTraceRunId::new(run_hi, run_lo).expect("test run is non-zero"),
            sequence,
        )
        .expect("test trace ID is admissible")
    }

    fn timestamp(monotonic_ns: u64) -> InteractionTraceTimestamp {
        InteractionTraceTimestamp {
            clock_domain: InteractionTraceClockDomain {
                host_id: 1,
                process_generation: 2,
                clock_id: 3,
            },
            monotonic_ns,
            wall_time_unix_ns: None,
        }
    }

    fn capacity() -> RecorderCapacityV1 {
        RecorderCapacityV1 {
            shard_count: 2,
            total_slots: 5,
            raw_event_bytes: 64,
            queue_slot_overhead_bytes: 8,
            queue_header_bytes_per_shard: 32,
            padded_counter_bytes_per_shard: 128,
            shard_metadata_bytes_per_shard: 16,
            frozen_export_slot_bytes: 64,
            conversion_event_bytes: 128,
            serialization_workspace_bytes: 4_096,
            configured_byte_ceiling: 65_536,
        }
    }

    fn qualifying_manifest() -> RecorderEpochManifestV1 {
        RecorderEpochManifestV1 {
            schema_version: RECORDER_EPOCH_MANIFEST_SCHEMA_VERSION,
            epoch_id: epoch_id(1),
            previous_epoch_id: None,
            mode: RecorderMode::CertificationWithMarkers,
            sampler: RecorderSamplerConfigV1::certification(),
            start_reason: RecorderEpochStartReason::ProcessStart,
            close_reason: Some(RecorderEpochCloseReason::NormalShutdown),
            lifecycle: RecorderLifecycleState::Closed,
            started_at: timestamp(100),
            closed_at: Some(timestamp(200)),
            capacity: capacity(),
            trace_accounting: RecorderTraceAccountingV1 {
                sampled_in: 1,
                sampled_out: 0,
                trace_id_exhausted: 0,
            },
            event_accounting: RecorderEventAccountingV1 {
                recorded: 14,
                queue_full: 0,
                closing: 0,
                clock_invalid: 0,
                epoch_mismatch: 0,
            },
            accounting_authority: RecorderAccountingAuthority::Exact,
            shutdown: RecorderShutdownStatusV1::Completed { frozen_events: 14 },
            export: RecorderExportStatusV1::Completed {
                exported_events: 14,
            },
            marker_authority: PlatformMarkerAuthorityV1::ExactEveryRecordedEvent,
            marker_accounting: PlatformMarkerAccountingV1 {
                attempted: 14,
                emitted: 14,
                unavailable: 0,
                dropped: 0,
                loss_unknown: false,
            },
        }
    }

    fn active_manifest(
        id: RecorderEpochId,
        mode: RecorderMode,
        sampler: RecorderSamplerConfigV1,
        started_at_ns: u64,
    ) -> RecorderEpochManifestV1 {
        RecorderEpochManifestV1 {
            schema_version: RECORDER_EPOCH_MANIFEST_SCHEMA_VERSION,
            epoch_id: id,
            previous_epoch_id: None,
            mode,
            sampler,
            start_reason: RecorderEpochStartReason::ProcessStart,
            close_reason: None,
            lifecycle: RecorderLifecycleState::Active,
            started_at: timestamp(started_at_ns),
            closed_at: None,
            capacity: capacity(),
            trace_accounting: RecorderTraceAccountingV1::default(),
            event_accounting: RecorderEventAccountingV1::default(),
            accounting_authority: RecorderAccountingAuthority::Exact,
            shutdown: RecorderShutdownStatusV1::NotStarted,
            export: RecorderExportStatusV1::NotAttempted,
            marker_authority: PlatformMarkerAuthorityV1::NotRequested,
            marker_accounting: PlatformMarkerAccountingV1::default(),
        }
    }

    #[test]
    fn splitmix64_v1_golden_vectors_are_stable() {
        let half = RecorderSamplerConfigV1 {
            algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
            numerator: 1,
            denominator: 2,
            seed_hi: 0,
            seed_lo: 0,
        };
        assert_eq!(
            half.hash(trace_id_for_run(1, 2, 1)),
            Ok(0xa10a_fed3_c9e0_bd73)
        );
        assert_eq!(half.bucket(trace_id_for_run(1, 2, 1)), Ok(1));
        assert_eq!(
            half.hash(trace_id_for_run(1, 2, 2)),
            Ok(0x9e75_9c08_0cb9_c871)
        );
        assert_eq!(half.bucket(trace_id_for_run(1, 2, 2)), Ok(1));

        let percent = RecorderSamplerConfigV1 {
            algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
            numerator: 10,
            denominator: 100,
            seed_hi: 0x1234,
            seed_lo: 0x5678,
        };
        assert_eq!(percent.hash(trace_id(0xbeef, 1)), Ok(0x1df3_4b47_14d0_3542));
        assert_eq!(percent.bucket(trace_id(0xbeef, 1)), Ok(11));
        assert_eq!(percent.hash(trace_id(0xbeef, 2)), Ok(0xde76_60c6_28c0_62f3));
        assert_eq!(percent.bucket(trace_id(0xbeef, 2)), Ok(86));
    }

    proptest! {
        #[test]
        fn splitmix_sampler_is_deterministic_and_bucketed(
            run_hi in any::<u64>(),
            run_lo in any::<u64>(),
            sequence in 0_u64..u64::MAX,
            seed_hi in any::<u64>(),
            seed_lo in any::<u64>(),
            denominator in 1_u64..=u64::MAX,
        ) {
            prop_assume!(run_hi != 0 || run_lo != 0);
            let trace_id = InteractionTraceId::new(
                InteractionTraceRunId::new(run_hi, run_lo)
                    .expect("assumption guarantees a valid run ID"),
                sequence,
            )
            .expect("strategy excludes the reserved sequence");
            let sampler = RecorderSamplerConfigV1 {
                algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
                numerator: denominator / 2,
                denominator,
                seed_hi,
                seed_lo,
            };
            let first_hash = sampler.hash(trace_id).expect("generated sampler is valid");
            let second_hash = sampler.hash(trace_id).expect("generated sampler is valid");
            let bucket = sampler.bucket(trace_id).expect("generated sampler is valid");
            prop_assert_eq!(first_hash, second_hash);
            prop_assert!(bucket < denominator);
            prop_assert_eq!(
                sampler.samples(trace_id).expect("generated sampler is valid"),
                bucket < sampler.numerator,
            );
        }
    }

    #[test]
    fn sampler_ratio_edges_and_reserved_trace_ids_fail_closed() {
        let none = RecorderSamplerConfigV1::off();
        let all = RecorderSamplerConfigV1::certification();
        for sequence in 1..=32 {
            let id = trace_id(2, sequence);
            assert_eq!(none.samples(id), Ok(false));
            assert_eq!(all.samples(id), Ok(true));
        }
        let invalid = RecorderSamplerConfigV1 {
            numerator: 2,
            denominator: 1,
            ..all
        };
        assert!(matches!(
            invalid.validate(),
            Err(RecorderContractError::InvalidSamplerRatio { .. })
        ));
        let zero_denominator = RecorderSamplerConfigV1 {
            numerator: 0,
            denominator: 0,
            ..all
        };
        assert!(zero_denominator.validate().is_err());
        let invalid_id = InteractionTraceId {
            run_id: InteractionTraceRunId {
                epoch_nonce_hi: 1,
                epoch_nonce_lo: 2,
            },
            sequence: u64::MAX,
        };
        assert_eq!(
            all.samples(invalid_id),
            Err(RecorderContractError::InvalidTraceId)
        );
        assert_eq!(
            all.samples(InteractionTraceId {
                sequence: 0,
                ..invalid_id
            }),
            Err(RecorderContractError::InvalidTraceId)
        );
        assert_eq!(
            all.samples(InteractionTraceId {
                run_id: InteractionTraceRunId {
                    epoch_nonce_hi: 0,
                    epoch_nonce_lo: 0,
                },
                sequence: 1,
            }),
            Err(RecorderContractError::InvalidTraceId)
        );
    }

    #[test]
    fn capacity_formula_and_distribution_are_exact() {
        let capacity = capacity();
        assert_eq!(
            capacity.checked_shard_distribution(),
            Ok(RecorderShardDistributionV1 {
                base_slots_per_shard: 2,
                remainder_shards: 1,
            })
        );
        assert_eq!(capacity.checked_reserved_bytes(), Ok(37_896));
        assert!(capacity.validate().is_ok());

        let mut one = capacity;
        one.shard_count = 1;
        one.total_slots = 1;
        assert_eq!(
            one.checked_shard_distribution(),
            Ok(RecorderShardDistributionV1 {
                base_slots_per_shard: 1,
                remainder_shards: 0,
            })
        );
        let mut two = one;
        two.shard_count = 2;
        two.total_slots = 2;
        assert!(two.validate().is_ok());
    }

    #[test]
    fn invalid_capacity_and_arithmetic_fail_closed() {
        let mut invalid = capacity();
        invalid.shard_count = 0;
        assert!(invalid.validate().is_err());
        invalid = capacity();
        invalid.shard_count = 3;
        invalid.total_slots = 2;
        assert_eq!(
            invalid.validate(),
            Err(RecorderContractError::InvalidShardDistribution)
        );
        invalid = capacity();
        invalid.raw_event_bytes = MAX_RAW_EVENT_BYTES + 1;
        assert!(invalid.validate().is_err());
        invalid = capacity();
        invalid.frozen_export_slot_bytes = invalid.raw_event_bytes - 1;
        assert!(matches!(
            invalid.validate(),
            Err(RecorderContractError::CapacityComponentTooSmall { .. })
        ));
        invalid = capacity();
        invalid.configured_byte_ceiling = invalid.checked_reserved_bytes().unwrap() - 1;
        assert!(matches!(
            invalid.validate(),
            Err(RecorderContractError::CapacityReservationExceeded { .. })
        ));
        invalid = capacity();
        invalid.serialization_workspace_bytes = u64::MAX;
        assert_eq!(
            invalid.checked_reserved_bytes(),
            Err(RecorderContractError::CapacityArithmeticOverflow)
        );

        let zero_component: [fn(&mut RecorderCapacityV1); 6] = [
            |value| value.queue_slot_overhead_bytes = 0,
            |value| value.queue_header_bytes_per_shard = 0,
            |value| value.padded_counter_bytes_per_shard = 0,
            |value| value.shard_metadata_bytes_per_shard = 0,
            |value| value.conversion_event_bytes = 0,
            |value| value.serialization_workspace_bytes = 0,
        ];
        for zero in zero_component {
            let mut omitted = capacity();
            zero(&mut omitted);
            assert!(matches!(
                omitted.validate(),
                Err(RecorderContractError::CapacityComponentTooSmall { minimum: 1, .. })
            ));
        }
    }

    #[test]
    fn every_capacity_hard_limit_accepts_its_edge_and_rejects_edge_plus_one() {
        let mut at_limit = capacity();
        at_limit.shard_count = MAX_SHARDS;
        at_limit.total_slots = u32::from(MAX_SHARDS);
        at_limit.configured_byte_ceiling = MAX_RESERVED_BYTES;
        assert!(at_limit.validate().is_ok());
        at_limit.shard_count = MAX_SHARDS + 1;
        assert!(matches!(
            at_limit.validate(),
            Err(RecorderContractError::CapacityOutOfRange {
                field: "shard_count",
                ..
            })
        ));

        at_limit = capacity();
        at_limit.shard_count = 1;
        at_limit.total_slots = MAX_TOTAL_SLOTS;
        at_limit.configured_byte_ceiling = MAX_RESERVED_BYTES;
        assert!(at_limit.validate().is_ok());
        at_limit.total_slots = MAX_TOTAL_SLOTS + 1;
        assert!(matches!(
            at_limit.validate(),
            Err(RecorderContractError::CapacityOutOfRange {
                field: "total_slots",
                ..
            })
        ));

        at_limit = capacity();
        at_limit.raw_event_bytes = MAX_RAW_EVENT_BYTES;
        at_limit.frozen_export_slot_bytes = MAX_RAW_EVENT_BYTES;
        at_limit.configured_byte_ceiling = MAX_RESERVED_BYTES;
        assert!(at_limit.validate().is_ok());
        at_limit.raw_event_bytes = MAX_RAW_EVENT_BYTES + 1;
        assert!(matches!(
            at_limit.validate(),
            Err(RecorderContractError::CapacityOutOfRange {
                field: "raw_event_bytes",
                ..
            })
        ));

        at_limit = capacity();
        at_limit.configured_byte_ceiling = MAX_RESERVED_BYTES;
        assert!(at_limit.validate().is_ok());
        at_limit.configured_byte_ceiling = MAX_RESERVED_BYTES + 1;
        assert!(matches!(
            at_limit.validate(),
            Err(RecorderContractError::CapacityOutOfRange {
                field: "configured_byte_ceiling",
                ..
            })
        ));
        assert_eq!(
            usize::from(CONVERSION_WORKSPACE_EVENTS),
            MAX_INTERACTION_TRACE_EVENTS
        );
    }

    #[test]
    fn accounting_totals_are_derived_checked_and_exhaustion_is_sticky() {
        let traces = RecorderTraceAccountingV1 {
            sampled_in: 2,
            sampled_out: 3,
            trace_id_exhausted: 4,
        };
        assert_eq!(traces.checked_enabled_trace_attempts(), Ok(9));
        let events = RecorderEventAccountingV1 {
            recorded: 1,
            queue_full: 2,
            closing: 3,
            clock_invalid: 4,
            epoch_mismatch: 5,
        };
        assert_eq!(events.checked_sampled_event_attempts(), Ok(15));

        let overflow = RecorderTraceAccountingV1 {
            sampled_in: u64::MAX,
            sampled_out: 1,
            trace_id_exhausted: 0,
        };
        assert!(matches!(
            overflow.checked_enabled_trace_attempts(),
            Err(RecorderContractError::AccountingOverflow { domain: "trace" })
        ));
        let event_overflow = RecorderEventAccountingV1 {
            recorded: u64::MAX,
            queue_full: 1,
            closing: 0,
            clock_invalid: 0,
            epoch_mismatch: 0,
        };
        assert!(matches!(
            event_overflow.checked_sampled_event_attempts(),
            Err(RecorderContractError::AccountingOverflow { domain: "event" })
        ));
        assert_eq!(
            RecorderAccountingAuthority::Exact
                .after_exhaustion()
                .after_exhaustion(),
            RecorderAccountingAuthority::Exhausted
        );

        let mut impossible_event_total = qualifying_manifest();
        impossible_event_total.event_accounting.recorded = MAX_INTERACTION_TRACE_EVENTS as u64 + 1;
        impossible_event_total.shutdown = RecorderShutdownStatusV1::Completed {
            frozen_events: MAX_INTERACTION_TRACE_EVENTS as u64 + 1,
        };
        impossible_event_total.export = RecorderExportStatusV1::Completed {
            exported_events: MAX_INTERACTION_TRACE_EVENTS as u64 + 1,
        };
        assert!(matches!(
            impossible_event_total.validate_internal_contract(),
            Err(RecorderContractError::InvalidAccounting { .. })
        ));
    }

    #[test]
    fn off_mode_is_canonical_and_observationally_empty() {
        let manifest = active_manifest(
            epoch_id(1),
            RecorderMode::Off,
            RecorderSamplerConfigV1::off(),
            100,
        );
        assert!(manifest.validate_internal_contract().is_ok());
        assert_eq!(
            manifest
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::NonQualifying)
        );

        let mut side_effect = manifest;
        side_effect.event_accounting.recorded = 1;
        side_effect.trace_accounting.sampled_in = 1;
        assert_eq!(
            side_effect.validate_internal_contract(),
            Err(RecorderContractError::OffModeHadSideEffects)
        );
        let mut noncanonical = manifest;
        noncanonical.sampler.seed_hi = 1;
        assert!(matches!(
            noncanonical.validate_internal_contract(),
            Err(RecorderContractError::InvalidSamplerForMode {
                mode: RecorderMode::Off
            })
        ));
    }

    #[test]
    fn lifecycle_shutdown_and_export_counts_fail_closed() {
        let manifest = qualifying_manifest();
        assert!(manifest.validate_internal_contract().is_ok());

        let mut closing = active_manifest(
            epoch_id(2),
            RecorderMode::Low,
            RecorderSamplerConfigV1::certification(),
            100,
        );
        closing.lifecycle = RecorderLifecycleState::Closing;
        closing.close_reason = Some(RecorderEpochCloseReason::NormalShutdown);
        closing.shutdown = RecorderShutdownStatusV1::InProgress;
        assert!(closing.validate_internal_contract().is_ok());

        let mut active_with_close = active_manifest(
            epoch_id(3),
            RecorderMode::Low,
            RecorderSamplerConfigV1::certification(),
            100,
        );
        active_with_close.close_reason = Some(RecorderEpochCloseReason::NormalShutdown);
        assert!(matches!(
            active_with_close.validate_internal_contract(),
            Err(RecorderContractError::InvalidLifecycle { .. })
        ));

        let mut incomplete_freeze = manifest;
        incomplete_freeze.shutdown = RecorderShutdownStatusV1::Completed { frozen_events: 13 };
        assert!(matches!(
            incomplete_freeze.validate_internal_contract(),
            Err(RecorderContractError::InvalidShutdownStatus { .. })
        ));
        let mut incomplete_export = manifest;
        incomplete_export.export = RecorderExportStatusV1::Incomplete {
            exported_events: 5,
            retained_events: 8,
        };
        assert!(matches!(
            incomplete_export.validate_internal_contract(),
            Err(RecorderContractError::InvalidExportStatus { .. })
        ));

        let mut falsely_incomplete_shutdown = manifest;
        falsely_incomplete_shutdown.shutdown = RecorderShutdownStatusV1::Incomplete {
            frozen_events: 14,
            in_flight_operations: 0,
        };
        assert!(matches!(
            falsely_incomplete_shutdown.validate_internal_contract(),
            Err(RecorderContractError::InvalidShutdownStatus { .. })
        ));

        let mut falsely_incomplete_export = manifest;
        falsely_incomplete_export.export = RecorderExportStatusV1::Incomplete {
            exported_events: 14,
            retained_events: 0,
        };
        assert!(matches!(
            falsely_incomplete_export.validate_internal_contract(),
            Err(RecorderContractError::InvalidExportStatus { .. })
        ));

        let mut regressing_clock = manifest;
        regressing_clock.closed_at = Some(timestamp(99));
        assert_eq!(
            regressing_clock.validate_internal_contract(),
            Err(RecorderContractError::InvalidEpochClock)
        );

        let mut crash_adjacent = manifest;
        crash_adjacent.close_reason = Some(RecorderEpochCloseReason::CrashAdjacentShutdown);
        crash_adjacent.shutdown =
            RecorderShutdownStatusV1::CrashAdjacentIncomplete { frozen_events: 13 };
        crash_adjacent.export = RecorderExportStatusV1::NotAttempted;
        assert!(crash_adjacent.validate_internal_contract().is_ok());
        assert_eq!(
            crash_adjacent
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::NonQualifying)
        );

        let mut mismatched_crash_reason = manifest;
        mismatched_crash_reason.close_reason =
            Some(RecorderEpochCloseReason::CrashAdjacentShutdown);
        assert!(matches!(
            mismatched_crash_reason.validate_internal_contract(),
            Err(RecorderContractError::InvalidShutdownStatus { .. })
        ));
        let mut mismatched_crash_status = manifest;
        mismatched_crash_status.shutdown =
            RecorderShutdownStatusV1::CrashAdjacentIncomplete { frozen_events: 13 };
        mismatched_crash_status.export = RecorderExportStatusV1::NotAttempted;
        assert!(matches!(
            mismatched_crash_status.validate_internal_contract(),
            Err(RecorderContractError::InvalidShutdownStatus { .. })
        ));
    }

    #[test]
    fn certification_classes_keep_marker_authority_separate() {
        let manifest = qualifying_manifest();
        assert_eq!(
            manifest
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );
        assert_eq!(
            manifest.certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );

        let mut ambiguous = manifest;
        ambiguous.marker_authority = PlatformMarkerAuthorityV1::Inexact;
        ambiguous.marker_accounting = PlatformMarkerAccountingV1 {
            attempted: 14,
            emitted: 13,
            unavailable: 0,
            dropped: 0,
            loss_unknown: true,
        };
        assert_eq!(
            ambiguous
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );
        assert_eq!(
            ambiguous
                .certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
            Ok(RecorderCertificationVerdict::NonQualifying)
        );

        let mut dropped = manifest;
        dropped.marker_accounting.emitted = 13;
        dropped.marker_accounting.dropped = 1;
        assert_eq!(
            dropped
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );
        assert_eq!(
            dropped.certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
            Ok(RecorderCertificationVerdict::NonQualifying)
        );

        let mut unavailable = manifest;
        unavailable.marker_accounting.emitted = 13;
        unavailable.marker_accounting.unavailable = 1;
        assert_eq!(
            unavailable
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );
        assert_eq!(
            unavailable
                .certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
            Ok(RecorderCertificationVerdict::NonQualifying)
        );

        let mut contradictory = manifest;
        contradictory.marker_accounting.attempted = 13;
        assert_eq!(
            contradictory
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );
        assert!(matches!(
            contradictory
                .certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
            Err(RecorderContractError::InvalidMarkerAccounting { .. })
        ));

        let mut overflowing = manifest;
        overflowing.marker_authority = PlatformMarkerAuthorityV1::Inexact;
        overflowing.marker_accounting = PlatformMarkerAccountingV1 {
            attempted: u64::MAX,
            emitted: u64::MAX,
            unavailable: 1,
            dropped: 0,
            loss_unknown: true,
        };
        assert_eq!(
            overflowing
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );
        assert_eq!(
            overflowing
                .certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
            Err(RecorderContractError::MarkerAccountingOverflow)
        );
    }

    #[test]
    fn platform_markers_require_the_explicit_marker_mode() {
        assert_eq!(
            serde_json::to_value(RecorderMode::CertificationWithMarkers)
                .expect("marker mode serializes"),
            serde_json::json!("certification_with_markers")
        );
        let marker_manifest = qualifying_manifest();
        assert!(marker_manifest.validate_marker_contract().is_ok());

        let mut ordinary_certification = marker_manifest;
        ordinary_certification.mode = RecorderMode::Certification;
        assert!(ordinary_certification.validate_internal_contract().is_ok());
        assert_eq!(
            ordinary_certification
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );
        assert!(matches!(
            ordinary_certification
                .certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
            Err(RecorderContractError::InvalidMarkerAccounting { .. })
        ));

        ordinary_certification.marker_authority = PlatformMarkerAuthorityV1::NotRequested;
        ordinary_certification.marker_accounting = PlatformMarkerAccountingV1::default();
        assert!(ordinary_certification.validate_marker_contract().is_ok());
        assert_eq!(
            ordinary_certification
                .certification_verdict(RecorderCertificationClass::MarkerAssistedCertification),
            Ok(RecorderCertificationVerdict::NonQualifying)
        );

        let mut marker_mode_without_authority = marker_manifest;
        marker_mode_without_authority.marker_authority = PlatformMarkerAuthorityV1::NotRequested;
        marker_mode_without_authority.marker_accounting = PlatformMarkerAccountingV1::default();
        assert!(matches!(
            marker_mode_without_authority.validate_marker_contract(),
            Err(RecorderContractError::InvalidMarkerAccounting { .. })
        ));
        assert_eq!(
            marker_mode_without_authority
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::Qualifying)
        );
    }

    #[test]
    fn every_internal_loss_or_authority_gap_is_nonqualifying() {
        let manifest = qualifying_manifest();
        for mutate in [
            |value: &mut RecorderEpochManifestV1| value.trace_accounting.sampled_out = 1,
            |value: &mut RecorderEpochManifestV1| value.trace_accounting.trace_id_exhausted = 1,
            |value: &mut RecorderEpochManifestV1| value.event_accounting.queue_full = 1,
            |value: &mut RecorderEpochManifestV1| value.event_accounting.closing = 1,
            |value: &mut RecorderEpochManifestV1| value.event_accounting.clock_invalid = 1,
            |value: &mut RecorderEpochManifestV1| value.event_accounting.epoch_mismatch = 1,
        ] {
            let mut candidate = manifest;
            mutate(&mut candidate);
            assert_eq!(
                candidate.certification_verdict(
                    RecorderCertificationClass::InternalRecorderCertification
                ),
                Ok(RecorderCertificationVerdict::NonQualifying)
            );
        }
        let mut exhausted = manifest;
        exhausted.accounting_authority = RecorderAccountingAuthority::Exhausted;
        assert_eq!(
            exhausted
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::NonQualifying)
        );

        let mut vacuous = manifest;
        vacuous.trace_accounting.sampled_in = 0;
        vacuous.event_accounting.recorded = 0;
        vacuous.shutdown = RecorderShutdownStatusV1::Completed { frozen_events: 0 };
        vacuous.export = RecorderExportStatusV1::Completed { exported_events: 0 };
        assert_eq!(
            vacuous
                .certification_verdict(RecorderCertificationClass::InternalRecorderCertification),
            Ok(RecorderCertificationVerdict::NonQualifying)
        );
    }

    #[test]
    fn mode_and_configuration_changes_require_new_linked_epochs() {
        let mut previous = qualifying_manifest();
        previous.mode = RecorderMode::Low;
        previous.sampler = RecorderSamplerConfigV1 {
            numerator: 1,
            denominator: 2,
            ..RecorderSamplerConfigV1::certification()
        };
        previous.close_reason = Some(RecorderEpochCloseReason::ModeChanged);

        let mut next = active_manifest(
            epoch_id(2),
            RecorderMode::Certification,
            RecorderSamplerConfigV1::certification(),
            300,
        );
        next.previous_epoch_id = Some(previous.epoch_id);
        next.start_reason = RecorderEpochStartReason::ModeChanged;
        assert_eq!(validate_epoch_transition(previous, next), Ok(()));

        let mut unlinked = next;
        unlinked.previous_epoch_id = None;
        assert!(matches!(
            validate_epoch_transition(previous, unlinked),
            Err(RecorderContractError::InvalidEpochTransition { .. })
        ));

        let mut invalid_predecessor = next;
        invalid_predecessor.previous_epoch_id = Some(RecorderEpochId {
            nonce_hi: 0,
            nonce_lo: 0,
        });
        assert_eq!(
            invalid_predecessor.validate_internal_contract(),
            Err(RecorderContractError::InvalidEpochId)
        );

        let mut configuration_previous = previous;
        configuration_previous.mode = RecorderMode::Certification;
        configuration_previous.sampler = RecorderSamplerConfigV1::certification();
        configuration_previous.close_reason = Some(RecorderEpochCloseReason::ConfigurationChanged);
        let mut configuration_next = next;
        configuration_next.mode = RecorderMode::Certification;
        configuration_next.start_reason = RecorderEpochStartReason::ConfigurationChanged;
        configuration_next.capacity.serialization_workspace_bytes += 1;
        assert_eq!(
            validate_epoch_transition(configuration_previous, configuration_next),
            Ok(())
        );
    }

    #[test]
    fn sampled_context_preserves_remote_origin_independent_of_local_epoch() {
        let local_epoch = epoch_id(99);
        let first = SampledTraceContextV1 {
            schema_version: SAMPLED_TRACE_CONTEXT_SCHEMA_VERSION,
            trace_id: trace_id(0xbeef, 1),
            path: InteractionTracePath::Keypress,
            origin_recorder_epoch_id: epoch_id(1),
            sampler_algorithm: RecorderSamplerAlgorithm::SplitMix64V1,
        };
        let second = SampledTraceContextV1 {
            trace_id: trace_id(0xcafe, 1),
            origin_recorder_epoch_id: epoch_id(2),
            path: InteractionTracePath::ResizeZoom,
            ..first
        };
        assert!(first.validate().is_ok());
        assert!(second.validate().is_ok());
        assert_ne!(first.trace_id.run_id, second.trace_id.run_id);
        assert_ne!(first.origin_recorder_epoch_id, local_epoch);
        assert_ne!(second.origin_recorder_epoch_id, local_epoch);

        let mut wrong_schema = first;
        wrong_schema.schema_version = 0;
        assert!(matches!(
            wrong_schema.validate(),
            Err(RecorderContractError::UnsupportedSchemaVersion { .. })
        ));
        let mut invalid_origin = first;
        invalid_origin.origin_recorder_epoch_id = RecorderEpochId {
            nonce_hi: 0,
            nonce_lo: 0,
        };
        assert_eq!(
            invalid_origin.validate(),
            Err(RecorderContractError::InvalidEpochId)
        );
        let mut reserved_trace = first;
        reserved_trace.trace_id.sequence = u64::MAX;
        assert_eq!(
            reserved_trace.validate(),
            Err(RecorderContractError::InvalidTraceId)
        );
    }

    #[test]
    fn serde_rejects_missing_unknown_and_old_version_fields() {
        let manifest = qualifying_manifest();
        let encoded = serde_json::to_value(manifest).expect("manifest serializes");
        let decoded: RecorderEpochManifestV1 =
            serde_json::from_value(encoded.clone()).expect("manifest deserializes");
        assert_eq!(decoded, manifest);

        let mut missing = encoded.clone();
        missing.as_object_mut().unwrap().remove("capacity");
        assert!(serde_json::from_value::<RecorderEpochManifestV1>(missing).is_err());

        let mut unknown = encoded.clone();
        unknown
            .as_object_mut()
            .unwrap()
            .insert("raw_key".to_owned(), serde_json::json!("secret"));
        assert!(serde_json::from_value::<RecorderEpochManifestV1>(unknown).is_err());

        let mut nested_unknown = encoded;
        nested_unknown["sampler"]["pane_text"] = serde_json::json!("secret");
        assert!(serde_json::from_value::<RecorderEpochManifestV1>(nested_unknown).is_err());

        let mut old = manifest;
        old.schema_version = 0;
        assert!(matches!(
            old.validate_internal_contract(),
            Err(RecorderContractError::UnsupportedSchemaVersion { .. })
        ));
    }

    #[test]
    fn manifest_wire_field_inventory_is_closed() {
        let encoded = serde_json::to_value(qualifying_manifest()).expect("manifest serializes");
        let actual: std::collections::BTreeSet<_> = encoded
            .as_object()
            .expect("manifest is an object")
            .keys()
            .map(String::as_str)
            .collect();
        let expected = std::collections::BTreeSet::from([
            "schema_version",
            "epoch_id",
            "previous_epoch_id",
            "mode",
            "sampler",
            "start_reason",
            "close_reason",
            "lifecycle",
            "started_at",
            "closed_at",
            "capacity",
            "trace_accounting",
            "event_accounting",
            "accounting_authority",
            "shutdown",
            "export",
            "marker_authority",
            "marker_accounting",
        ]);
        assert_eq!(actual, expected);
    }

    #[test]
    fn privacy_planted_negatives_have_no_typed_entry_point() {
        let encoded = serde_json::to_value(qualifying_manifest()).expect("manifest serializes");
        for forbidden in [
            "raw_key",
            "text",
            "pane_text",
            "title",
            "command",
            "cwd",
            "hostname",
            "reason",
        ] {
            let mut planted = encoded.clone();
            planted
                .as_object_mut()
                .unwrap()
                .insert(forbidden.to_owned(), serde_json::json!("seeded-secret"));
            assert!(
                serde_json::from_value::<RecorderEpochManifestV1>(planted).is_err(),
                "forbidden field {forbidden} was accepted"
            );
        }

        let serialized = serde_json::to_string(&qualifying_manifest()).unwrap();
        for secret in ["seeded-secret", "pane_text", "raw_key", "hostname"] {
            assert!(!serialized.contains(secret));
        }
    }
}
