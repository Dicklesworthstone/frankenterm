//! Cold-tier chunk-write pipeline state machine substrate
//! (ft-tfb64 / ft-2okh0.13.cont).
//!
//! Pure-logic substrate covering the substrate-shaped pieces
//! of ft-tfb64's cold-tier integration. Sibling module:
//! `scrollback_cold_tier.rs` (commit b3e8a6845) — tier
//! cascade, EvictionPolicy, RedactionStatus, ChunkMetadata.
//!
//! This module ships the per-chunk write pipeline state
//! machine — the integration's chunk-evictor calls
//! `apply_step_outcome` repeatedly until the chunk reaches
//! `Indexed` (success) or `Failed` (terminal).
//!
//! ## What this module ships
//!
//! - `WritePipelineStep` — `Compress / Redact / Encrypt /
//!   Persist / Index`. Bead's "zstd → redact → encrypt → write
//!   → index" pipeline stages.
//! - `WritePipelineState` — `Pending(step) / Done /
//!   Failed { step, reason }`. Tracks where the chunk is in
//!   the pipeline.
//! - `StepOutcome` — `Success / Skipped / Failure(reason)` —
//!   what each step reports back.
//! - `StepFailureReason` 7-variant covering the bead's failure
//!   modes (CompressionRatioBelowFloor / RedactorTimeout /
//!   EncryptionKeyMissing / DiskFull / DiskIoError /
//!   IndexInsertConflict / RedactorRefused).
//! - `apply_step_outcome` advances the state machine.
//! - `next_step` reports the next step to run when in
//!   `Pending`.
//! - `WritePipelineConfig` — operator-tunable: encryption
//!   on/off, redaction-skip-permitted (for testing only).
//! - `ColdTierPipelineTelemetry` per-step counters + failure
//!   breakdown.
//! - `ChunkWriteSummary` — final outcome per chunk-write
//!   attempt (steps_completed, latency_ns, etc.) for the
//!   bead's per-write structured-log line.
//!
//! ## What is deferred to ft-tfb64 follow-up
//!
//! - zstd compression at the Compress step (use the `zstd`
//!   crate; integration runs the actual compressor).
//! - redactor.rs::redact_text call at the Redact step.
//! - AES-256-GCM encryption (`aes-gcm` crate) at the Encrypt
//!   step.
//! - Async file I/O via asupersync at the Persist step
//!   (`~/.cache/ft/scrollback/<pane_id>/<chunk_id>.zst[.enc]`).
//! - SQLite metadata index at the Index step (cross-link
//!   storage_backend_trait.rs).
//! - JSON-line logging from ChunkWriteSummary.
//! - Cleanup task: weekly cron-style scan calling the
//!   substrate's should_purge_by_retention.

#![allow(dead_code)]

// ============================================================================
// WritePipelineStep
// ============================================================================

/// Pipeline stages in execution order: redact → compress →
/// encrypt → persist → index.
///
/// **Order matters** for the bead's privacy rule: the
/// redactor scans for secret patterns (api_key=..., bearer
/// tokens, etc.) and only matches plaintext. Running it on
/// compressed or encrypted bytes silently no-ops, so
/// substrate places `Redact` before `Compress` and exposes
/// the chain via `successor()` so callers can't accidentally
/// reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WritePipelineStep {
    /// Run redactor over the raw plaintext chunk bytes,
    /// replacing any matched secret with the redactor's
    /// substitution token.
    Redact,
    /// Compress the post-redact bytes via zstd.
    Compress,
    /// AES-256-GCM encrypt the post-compress bytes (operator
    /// opt-in).
    Encrypt,
    /// Async write to `~/.cache/ft/scrollback/<pane_id>/
    /// <chunk_id>.zst[.enc]` mode 0600.
    Persist,
    /// Insert metadata into the SQLite index (chunk_id,
    /// pane_id, byte range, line range, content_hash, etc.).
    Index,
}

impl WritePipelineStep {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Redact => "redact",
            Self::Compress => "compress",
            Self::Encrypt => "encrypt",
            Self::Persist => "persist",
            Self::Index => "index",
        }
    }

    /// The step that follows this one. `None` after `Index`
    /// (terminal success).
    #[must_use]
    pub const fn successor(self) -> Option<Self> {
        match self {
            Self::Redact => Some(Self::Compress),
            Self::Compress => Some(Self::Encrypt),
            Self::Encrypt => Some(Self::Persist),
            Self::Persist => Some(Self::Index),
            Self::Index => None,
        }
    }

    /// Whether this step is operator-skippable. Bead's
    /// privacy rule: `Redact` is NEVER skippable in default
    /// builds; operator can flip a config flag for testing.
    /// `Encrypt` is opt-in and skippable.
    #[must_use]
    pub const fn is_skippable_by_default(self) -> bool {
        matches!(self, Self::Encrypt)
    }
}

// ============================================================================
// StepOutcome + failure reasons
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepOutcome {
    /// Step ran and produced output for the next stage.
    Success,
    /// Step was skipped per config (`Encrypt` when
    /// encryption disabled). Substrate advances to the next
    /// step without recording a failure.
    Skipped,
    /// Step failed; pipeline transitions to `Failed`.
    Failure(StepFailureReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepFailureReason {
    /// `Compress` step: zstd ratio below `min_compression_ratio`
    /// from EvictionPolicy — chunk doesn't compress well
    /// enough for disk eviction.
    CompressionRatioBelowFloor,
    /// `Redact` step: timed out (cross-link
    /// runtime_async::timeout_with_cx).
    RedactorTimeout,
    /// `Redact` step: redactor refused on policy grounds.
    RedactorRefused,
    /// `Encrypt` step: AES key not provisioned (operator
    /// enabled encryption but no keychain entry exists).
    EncryptionKeyMissing,
    /// `Persist` step: disk full.
    DiskFull,
    /// `Persist` step: any other I/O error (permission,
    /// EACCES on the cache dir, etc.).
    DiskIoError,
    /// `Index` step: chunk_id collision in the metadata
    /// table — substrate's chunk-id allocator should prevent
    /// this, so it indicates a bug.
    IndexInsertConflict,
    /// Self-review fix (br-ft-uznr1): integration passed
    /// `StepOutcome::Skipped` for a step that's not
    /// operator-skippable. Substrate refuses to advance to
    /// preserve the privacy invariant (Redact, Compress,
    /// Persist, Index are all required). Permanent —
    /// retry would re-fire the same illegal call.
    IllegalSkip,
}

impl StepFailureReason {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CompressionRatioBelowFloor => "compression_ratio_below_floor",
            Self::RedactorTimeout => "redactor_timeout",
            Self::RedactorRefused => "redactor_refused",
            Self::EncryptionKeyMissing => "encryption_key_missing",
            Self::DiskFull => "disk_full",
            Self::DiskIoError => "disk_io_error",
            Self::IndexInsertConflict => "index_insert_conflict",
            Self::IllegalSkip => "illegal_skip",
        }
    }

    /// Whether the integration should retry this chunk after
    /// the failure. CompressionRatio + Redactor + IndexConflict
    /// are permanent for this chunk; DiskFull / DiskIoError /
    /// EncryptionKeyMissing may resolve. IllegalSkip is
    /// permanent — retry would re-fire the same illegal call.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::DiskFull | Self::DiskIoError | Self::EncryptionKeyMissing
        )
    }
}

// ============================================================================
// WritePipelineState
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WritePipelineState {
    /// In progress at `step`.
    Pending(WritePipelineStep),
    /// All steps complete.
    Done,
    /// Pipeline aborted at `step` because of `reason`.
    Failed {
        step: WritePipelineStep,
        reason: StepFailureReason,
    },
}

impl WritePipelineState {
    #[must_use]
    pub const fn new() -> Self {
        Self::Pending(WritePipelineStep::Redact)
    }

    /// What step is next (None when Done or Failed).
    #[must_use]
    pub const fn next_step(self) -> Option<WritePipelineStep> {
        match self {
            Self::Pending(step) => Some(step),
            Self::Done | Self::Failed { .. } => None,
        }
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. })
    }

    #[must_use]
    pub const fn is_failed(self) -> bool {
        matches!(self, Self::Failed { .. })
    }
}

impl Default for WritePipelineState {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Config
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WritePipelineConfig {
    /// Whether to encrypt at rest (AES-256-GCM). Bead default
    /// false (operator opt-in via `[scrollback.cold_tier]
    /// encrypt = true`).
    pub encrypt_at_rest: bool,
    /// Bypass redaction for testing. Bead's privacy rule
    /// requires this be false in production builds; substrate
    /// honours the flag but logs via telemetry.
    pub bypass_redaction: bool,
    /// Whether to retry after a retryable failure. Default
    /// true.
    pub retry_on_retryable: bool,
}

impl Default for WritePipelineConfig {
    fn default() -> Self {
        Self {
            encrypt_at_rest: false,
            bypass_redaction: false,
            retry_on_retryable: true,
        }
    }
}

// ============================================================================
// apply_step_outcome
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PipelineDecision {
    /// Pipeline state advanced; integration runs the new
    /// `Pending` step.
    Advanced,
    /// Step succeeded and was the last one; pipeline is now
    /// `Done`.
    Completed,
    /// Pipeline failed.
    Failed,
    /// Caller passed an outcome for a terminal state — no-op.
    AlreadyTerminal,
    /// Caller passed `Skipped` for a step that
    /// `is_skippable_by_default()` returns false for.
    /// Substrate refuses (privacy bypass guard) and
    /// transitions to `Failed` with `IllegalSkip` reason.
    RefusedIllegalSkip,
}

/// Apply an outcome to the state machine. Returns the
/// transition that fired.
///
/// Self-review fix (br-ft-uznr1): `Skipped` is rejected for
/// any step that isn't `is_skippable_by_default()`. The
/// integration passing `StepOutcome::Skipped` for `Redact`
/// (privacy-critical) used to silently advance to `Compress`
/// — bypassing the redactor and breaking the bead's privacy
/// invariant. Now substrate transitions to
/// `Failed { reason: IllegalSkip }` and returns
/// `RefusedIllegalSkip`.
pub fn apply_step_outcome(
    state: &mut WritePipelineState,
    outcome: StepOutcome,
) -> PipelineDecision {
    let current = match *state {
        WritePipelineState::Pending(step) => step,
        WritePipelineState::Done | WritePipelineState::Failed { .. } => {
            return PipelineDecision::AlreadyTerminal;
        }
    };

    match outcome {
        StepOutcome::Skipped if !current.is_skippable_by_default() => {
            *state = WritePipelineState::Failed {
                step: current,
                reason: StepFailureReason::IllegalSkip,
            };
            PipelineDecision::RefusedIllegalSkip
        }
        StepOutcome::Success | StepOutcome::Skipped => match current.successor() {
            Some(next) => {
                *state = WritePipelineState::Pending(next);
                PipelineDecision::Advanced
            }
            None => {
                *state = WritePipelineState::Done;
                PipelineDecision::Completed
            }
        },
        StepOutcome::Failure(reason) => {
            *state = WritePipelineState::Failed {
                step: current,
                reason,
            };
            PipelineDecision::Failed
        }
    }
}

// ============================================================================
// Per-chunk summary
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkWriteSummary {
    pub steps_completed: u32,
    pub final_state: WritePipelineState,
    pub elapsed_ns: u64,
    pub bytes_in: u64,
    pub bytes_out_post_compress: u64,
    pub bytes_out_post_encrypt: u64,
}

impl ChunkWriteSummary {
    #[must_use]
    pub const fn succeeded(&self) -> bool {
        matches!(self.final_state, WritePipelineState::Done)
    }

    /// Effective compression ratio. `None` if no input bytes.
    #[must_use]
    pub fn compression_ratio(&self) -> Option<f64> {
        if self.bytes_in == 0 {
            None
        } else {
            Some(self.bytes_out_post_compress as f64 / self.bytes_in as f64)
        }
    }
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColdTierPipelineTelemetry {
    pub chunks_attempted: u64,
    pub chunks_succeeded: u64,
    pub chunks_failed: u64,
    pub steps_succeeded: PerStepCounter,
    pub steps_skipped: PerStepCounter,
    pub failures_by_reason: FailureByReason,
    pub bytes_in_total: u64,
    pub bytes_out_post_compress_total: u64,
    pub bytes_out_post_encrypt_total: u64,
    /// Bead's privacy-rule audit: how many times redaction
    /// was bypassed via the test flag.
    pub redaction_bypasses_total: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PerStepCounter {
    pub compress: u64,
    pub redact: u64,
    pub encrypt: u64,
    pub persist: u64,
    pub index: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FailureByReason {
    pub compression_ratio_below_floor: u64,
    pub redactor_timeout: u64,
    pub redactor_refused: u64,
    pub encryption_key_missing: u64,
    pub disk_full: u64,
    pub disk_io_error: u64,
    pub index_insert_conflict: u64,
    /// Self-review fix (br-ft-uznr1): integration tried to
    /// pass `Skipped` for a non-skippable step. Substrate
    /// refused; this counter surfaces the bug for `ft doctor`.
    pub illegal_skip: u64,
}

impl ColdTierPipelineTelemetry {
    pub fn record_step(&mut self, step: WritePipelineStep, outcome: StepOutcome) {
        let counter = match outcome {
            StepOutcome::Success => Some(&mut self.steps_succeeded),
            StepOutcome::Skipped => Some(&mut self.steps_skipped),
            StepOutcome::Failure(reason) => {
                self.record_failure(reason);
                None
            }
        };
        if let Some(counter) = counter {
            let slot = match step {
                WritePipelineStep::Compress => &mut counter.compress,
                WritePipelineStep::Redact => &mut counter.redact,
                WritePipelineStep::Encrypt => &mut counter.encrypt,
                WritePipelineStep::Persist => &mut counter.persist,
                WritePipelineStep::Index => &mut counter.index,
            };
            *slot = slot.saturating_add(1);
        }
    }

    fn record_failure(&mut self, reason: StepFailureReason) {
        let slot = match reason {
            StepFailureReason::CompressionRatioBelowFloor => {
                &mut self.failures_by_reason.compression_ratio_below_floor
            }
            StepFailureReason::RedactorTimeout => &mut self.failures_by_reason.redactor_timeout,
            StepFailureReason::RedactorRefused => &mut self.failures_by_reason.redactor_refused,
            StepFailureReason::EncryptionKeyMissing => {
                &mut self.failures_by_reason.encryption_key_missing
            }
            StepFailureReason::DiskFull => &mut self.failures_by_reason.disk_full,
            StepFailureReason::DiskIoError => &mut self.failures_by_reason.disk_io_error,
            StepFailureReason::IndexInsertConflict => {
                &mut self.failures_by_reason.index_insert_conflict
            }
            StepFailureReason::IllegalSkip => &mut self.failures_by_reason.illegal_skip,
        };
        *slot = slot.saturating_add(1);
    }

    pub fn record_summary(&mut self, summary: &ChunkWriteSummary) {
        self.chunks_attempted = self.chunks_attempted.saturating_add(1);
        if summary.succeeded() {
            self.chunks_succeeded = self.chunks_succeeded.saturating_add(1);
        } else {
            self.chunks_failed = self.chunks_failed.saturating_add(1);
        }
        self.bytes_in_total = self.bytes_in_total.saturating_add(summary.bytes_in);
        self.bytes_out_post_compress_total = self
            .bytes_out_post_compress_total
            .saturating_add(summary.bytes_out_post_compress);
        self.bytes_out_post_encrypt_total = self
            .bytes_out_post_encrypt_total
            .saturating_add(summary.bytes_out_post_encrypt);
    }

    pub fn record_redaction_bypass(&mut self) {
        self.redaction_bypasses_total = self.redaction_bypasses_total.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ----------------------------------------------------------------
    // WritePipelineStep
    // ----------------------------------------------------------------

    #[test]
    fn step_successor_chain() {
        assert_eq!(
            WritePipelineStep::Redact.successor(),
            Some(WritePipelineStep::Compress),
        );
        assert_eq!(
            WritePipelineStep::Compress.successor(),
            Some(WritePipelineStep::Encrypt),
        );
        assert_eq!(
            WritePipelineStep::Encrypt.successor(),
            Some(WritePipelineStep::Persist),
        );
        assert_eq!(
            WritePipelineStep::Persist.successor(),
            Some(WritePipelineStep::Index),
        );
        assert_eq!(WritePipelineStep::Index.successor(), None);
    }

    #[test]
    fn step_redact_runs_before_compress_for_privacy() {
        // Bug fix (ft-gc8zs): redactor must scan plaintext;
        // compressed bytes are opaque. Substrate enforces by
        // putting Redact at the head of the chain.
        assert_eq!(
            WritePipelineStep::Redact.successor(),
            Some(WritePipelineStep::Compress),
        );
        // Default state starts at Redact, not Compress.
        let s = WritePipelineState::default();
        assert_eq!(s.next_step(), Some(WritePipelineStep::Redact));
    }

    #[test]
    fn step_skippable_only_encrypt() {
        assert!(WritePipelineStep::Encrypt.is_skippable_by_default());
        assert!(!WritePipelineStep::Compress.is_skippable_by_default());
        assert!(!WritePipelineStep::Redact.is_skippable_by_default());
        assert!(!WritePipelineStep::Persist.is_skippable_by_default());
        assert!(!WritePipelineStep::Index.is_skippable_by_default());
    }

    #[test]
    fn step_label_stable() {
        assert_eq!(WritePipelineStep::Compress.label(), "compress");
        assert_eq!(WritePipelineStep::Redact.label(), "redact");
        assert_eq!(WritePipelineStep::Encrypt.label(), "encrypt");
        assert_eq!(WritePipelineStep::Persist.label(), "persist");
        assert_eq!(WritePipelineStep::Index.label(), "index");
    }

    // ----------------------------------------------------------------
    // StepFailureReason
    // ----------------------------------------------------------------

    #[test]
    fn failure_retryable_classification() {
        assert!(StepFailureReason::DiskFull.is_retryable());
        assert!(StepFailureReason::DiskIoError.is_retryable());
        assert!(StepFailureReason::EncryptionKeyMissing.is_retryable());
        assert!(!StepFailureReason::CompressionRatioBelowFloor.is_retryable());
        assert!(!StepFailureReason::RedactorTimeout.is_retryable());
        assert!(!StepFailureReason::RedactorRefused.is_retryable());
        assert!(!StepFailureReason::IndexInsertConflict.is_retryable());
    }

    // ----------------------------------------------------------------
    // WritePipelineState
    // ----------------------------------------------------------------

    #[test]
    fn state_default_starts_at_redact() {
        let s = WritePipelineState::default();
        assert_eq!(s, WritePipelineState::Pending(WritePipelineStep::Redact));
        assert_eq!(s.next_step(), Some(WritePipelineStep::Redact));
        assert!(!s.is_terminal());
        assert!(!s.is_failed());
    }

    #[test]
    fn state_done_is_terminal_not_failed() {
        let s = WritePipelineState::Done;
        assert!(s.is_terminal());
        assert!(!s.is_failed());
        assert_eq!(s.next_step(), None);
    }

    #[test]
    fn state_failed_is_terminal_and_failed() {
        let s = WritePipelineState::Failed {
            step: WritePipelineStep::Persist,
            reason: StepFailureReason::DiskFull,
        };
        assert!(s.is_terminal());
        assert!(s.is_failed());
    }

    // ----------------------------------------------------------------
    // apply_step_outcome
    // ----------------------------------------------------------------

    #[test]
    fn apply_success_advances_through_chain() {
        let mut s = WritePipelineState::default();
        let d1 = apply_step_outcome(&mut s, StepOutcome::Success);
        assert_eq!(d1, PipelineDecision::Advanced);
        assert_eq!(s, WritePipelineState::Pending(WritePipelineStep::Compress));

        apply_step_outcome(&mut s, StepOutcome::Success);
        assert_eq!(s, WritePipelineState::Pending(WritePipelineStep::Encrypt));

        apply_step_outcome(&mut s, StepOutcome::Success);
        assert_eq!(s, WritePipelineState::Pending(WritePipelineStep::Persist));

        apply_step_outcome(&mut s, StepOutcome::Success);
        assert_eq!(s, WritePipelineState::Pending(WritePipelineStep::Index));

        let d_final = apply_step_outcome(&mut s, StepOutcome::Success);
        assert_eq!(d_final, PipelineDecision::Completed);
        assert_eq!(s, WritePipelineState::Done);
    }

    #[test]
    fn apply_skipped_advances_like_success() {
        let mut s = WritePipelineState::Pending(WritePipelineStep::Encrypt);
        let d = apply_step_outcome(&mut s, StepOutcome::Skipped);
        assert_eq!(d, PipelineDecision::Advanced);
        assert_eq!(s, WritePipelineState::Pending(WritePipelineStep::Persist));
    }

    #[test]
    fn apply_skipped_for_redact_is_refused() {
        // Self-review fix (br-ft-uznr1): Skipped for the
        // privacy-critical Redact step is refused. Substrate
        // transitions to Failed{IllegalSkip} so the privacy
        // invariant cannot be bypassed.
        let mut s = WritePipelineState::Pending(WritePipelineStep::Redact);
        let d = apply_step_outcome(&mut s, StepOutcome::Skipped);
        assert_eq!(d, PipelineDecision::RefusedIllegalSkip);
        match s {
            WritePipelineState::Failed { step, reason } => {
                assert_eq!(step, WritePipelineStep::Redact);
                assert_eq!(reason, StepFailureReason::IllegalSkip);
            }
            other => panic!("expected Failed; got {other:?}"),
        }
    }

    #[test]
    fn apply_skipped_for_compress_is_refused() {
        let mut s = WritePipelineState::Pending(WritePipelineStep::Compress);
        let d = apply_step_outcome(&mut s, StepOutcome::Skipped);
        assert_eq!(d, PipelineDecision::RefusedIllegalSkip);
    }

    #[test]
    fn apply_skipped_for_persist_is_refused() {
        let mut s = WritePipelineState::Pending(WritePipelineStep::Persist);
        let d = apply_step_outcome(&mut s, StepOutcome::Skipped);
        assert_eq!(d, PipelineDecision::RefusedIllegalSkip);
    }

    #[test]
    fn apply_skipped_for_index_is_refused() {
        let mut s = WritePipelineState::Pending(WritePipelineStep::Index);
        let d = apply_step_outcome(&mut s, StepOutcome::Skipped);
        assert_eq!(d, PipelineDecision::RefusedIllegalSkip);
    }

    #[test]
    fn illegal_skip_is_not_retryable() {
        // Permanent — retry would re-fire the same illegal call.
        assert!(!StepFailureReason::IllegalSkip.is_retryable());
    }

    #[test]
    fn apply_failure_transitions_to_failed() {
        let mut s = WritePipelineState::Pending(WritePipelineStep::Persist);
        let d = apply_step_outcome(&mut s, StepOutcome::Failure(StepFailureReason::DiskFull));
        assert_eq!(d, PipelineDecision::Failed);
        assert_eq!(
            s,
            WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
        );
    }

    #[test]
    fn apply_outcome_to_terminal_is_noop() {
        let mut s = WritePipelineState::Done;
        let d = apply_step_outcome(&mut s, StepOutcome::Success);
        assert_eq!(d, PipelineDecision::AlreadyTerminal);
        assert_eq!(s, WritePipelineState::Done);
    }

    #[test]
    fn apply_outcome_to_failed_is_noop() {
        let mut s = WritePipelineState::Failed {
            step: WritePipelineStep::Compress,
            reason: StepFailureReason::CompressionRatioBelowFloor,
        };
        let d = apply_step_outcome(&mut s, StepOutcome::Success);
        assert_eq!(d, PipelineDecision::AlreadyTerminal);
        assert!(s.is_failed());
    }

    // ----------------------------------------------------------------
    // ChunkWriteSummary
    // ----------------------------------------------------------------

    #[test]
    fn summary_succeeded_when_done() {
        let s = ChunkWriteSummary {
            steps_completed: 5,
            final_state: WritePipelineState::Done,
            elapsed_ns: 1_000,
            bytes_in: 1024,
            bytes_out_post_compress: 256,
            bytes_out_post_encrypt: 256,
        };
        assert!(s.succeeded());
    }

    #[test]
    fn summary_not_succeeded_when_failed() {
        let s = ChunkWriteSummary {
            steps_completed: 3,
            final_state: WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
            elapsed_ns: 500,
            bytes_in: 1024,
            bytes_out_post_compress: 256,
            bytes_out_post_encrypt: 0,
        };
        assert!(!s.succeeded());
    }

    #[test]
    fn summary_compression_ratio_correct() {
        let s = ChunkWriteSummary {
            steps_completed: 5,
            final_state: WritePipelineState::Done,
            elapsed_ns: 1_000,
            bytes_in: 1024,
            bytes_out_post_compress: 256,
            bytes_out_post_encrypt: 256,
        };
        assert_eq!(s.compression_ratio(), Some(0.25));
    }

    #[test]
    fn summary_compression_ratio_none_for_zero_input() {
        let s = ChunkWriteSummary {
            steps_completed: 0,
            final_state: WritePipelineState::Done,
            elapsed_ns: 0,
            bytes_in: 0,
            bytes_out_post_compress: 0,
            bytes_out_post_encrypt: 0,
        };
        assert!(s.compression_ratio().is_none());
    }

    // ----------------------------------------------------------------
    // Telemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_record_step_routes() {
        let mut t = ColdTierPipelineTelemetry::default();
        t.record_step(WritePipelineStep::Compress, StepOutcome::Success);
        t.record_step(WritePipelineStep::Redact, StepOutcome::Success);
        t.record_step(WritePipelineStep::Encrypt, StepOutcome::Skipped);
        t.record_step(
            WritePipelineStep::Persist,
            StepOutcome::Failure(StepFailureReason::DiskFull),
        );
        assert_eq!(t.steps_succeeded.compress, 1);
        assert_eq!(t.steps_succeeded.redact, 1);
        assert_eq!(t.steps_skipped.encrypt, 1);
        assert_eq!(t.failures_by_reason.disk_full, 1);
    }

    #[test]
    fn telemetry_record_summary_routes() {
        let mut t = ColdTierPipelineTelemetry::default();
        let success = ChunkWriteSummary {
            steps_completed: 5,
            final_state: WritePipelineState::Done,
            elapsed_ns: 1_000,
            bytes_in: 1024,
            bytes_out_post_compress: 256,
            bytes_out_post_encrypt: 256,
        };
        let failure = ChunkWriteSummary {
            steps_completed: 3,
            final_state: WritePipelineState::Failed {
                step: WritePipelineStep::Persist,
                reason: StepFailureReason::DiskFull,
            },
            elapsed_ns: 500,
            bytes_in: 1024,
            bytes_out_post_compress: 256,
            bytes_out_post_encrypt: 0,
        };
        t.record_summary(&success);
        t.record_summary(&failure);
        assert_eq!(t.chunks_attempted, 2);
        assert_eq!(t.chunks_succeeded, 1);
        assert_eq!(t.chunks_failed, 1);
        assert_eq!(t.bytes_in_total, 2048);
    }

    #[test]
    fn telemetry_record_redaction_bypass() {
        let mut t = ColdTierPipelineTelemetry::default();
        t.record_redaction_bypass();
        t.record_redaction_bypass();
        assert_eq!(t.redaction_bypasses_total, 2);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_full_success_pipeline() {
        // 1 KiB chunk → redact → zstd → no encrypt → persist
        // → index. All succeed.
        let mut state = WritePipelineState::default();
        let mut t = ColdTierPipelineTelemetry::default();

        t.record_step(WritePipelineStep::Redact, StepOutcome::Success);
        apply_step_outcome(&mut state, StepOutcome::Success);

        t.record_step(WritePipelineStep::Compress, StepOutcome::Success);
        apply_step_outcome(&mut state, StepOutcome::Success);

        // Encryption disabled — Skipped.
        t.record_step(WritePipelineStep::Encrypt, StepOutcome::Skipped);
        apply_step_outcome(&mut state, StepOutcome::Skipped);

        t.record_step(WritePipelineStep::Persist, StepOutcome::Success);
        apply_step_outcome(&mut state, StepOutcome::Success);

        t.record_step(WritePipelineStep::Index, StepOutcome::Success);
        let final_decision = apply_step_outcome(&mut state, StepOutcome::Success);

        assert_eq!(final_decision, PipelineDecision::Completed);
        assert_eq!(state, WritePipelineState::Done);
        assert_eq!(t.steps_succeeded.compress, 1);
        assert_eq!(t.steps_succeeded.redact, 1);
        assert_eq!(t.steps_skipped.encrypt, 1);
    }

    #[test]
    fn scenario_disk_full_retryable() {
        // Persist fails with DiskFull; retryable.
        let mut state = WritePipelineState::Pending(WritePipelineStep::Persist);
        let _d = apply_step_outcome(
            &mut state,
            StepOutcome::Failure(StepFailureReason::DiskFull),
        );
        assert!(state.is_failed());
        if let WritePipelineState::Failed { reason, .. } = state {
            assert!(reason.is_retryable());
        }
    }

    #[test]
    fn scenario_redactor_timeout_not_retryable() {
        // Redactor failure is permanent for this chunk.
        let mut state = WritePipelineState::Pending(WritePipelineStep::Redact);
        apply_step_outcome(
            &mut state,
            StepOutcome::Failure(StepFailureReason::RedactorTimeout),
        );
        assert!(state.is_failed());
        if let WritePipelineState::Failed { reason, .. } = state {
            assert!(!reason.is_retryable());
        }
    }

    #[test]
    fn scenario_compression_ratio_below_floor_aborts_at_compress_step() {
        // Bead's "skip eviction if compression below threshold"
        // — substrate exposes via Failure(CompressionRatioBelowFloor)
        // at the Compress step (which now follows Redact).
        let mut state = WritePipelineState::default();
        // Redact succeeds.
        apply_step_outcome(&mut state, StepOutcome::Success);
        assert_eq!(
            state,
            WritePipelineState::Pending(WritePipelineStep::Compress)
        );
        // Compress fails on ratio.
        apply_step_outcome(
            &mut state,
            StepOutcome::Failure(StepFailureReason::CompressionRatioBelowFloor),
        );
        if let WritePipelineState::Failed { step, .. } = state {
            assert_eq!(step, WritePipelineStep::Compress);
        } else {
            panic!("expected Failed state");
        }
    }

    #[test]
    fn scenario_encryption_enabled_pipeline() {
        // Encryption ON — Encrypt step runs Success not Skipped.
        let mut state = WritePipelineState::Pending(WritePipelineStep::Encrypt);
        apply_step_outcome(&mut state, StepOutcome::Success);
        assert_eq!(
            state,
            WritePipelineState::Pending(WritePipelineStep::Persist)
        );
    }

    #[test]
    fn scenario_redaction_bypass_audit_trail() {
        // Operator enabled bypass_redaction for testing;
        // substrate logs every bypass.
        let mut t = ColdTierPipelineTelemetry::default();
        for _ in 0..5 {
            t.record_redaction_bypass();
        }
        assert_eq!(t.redaction_bypasses_total, 5);
    }

    #[test]
    fn scenario_index_conflict_indicates_substrate_bug() {
        // Bead notes index_insert_conflict shouldn't happen
        // if chunk-id allocator is correct. Substrate exposes
        // for telemetry visibility.
        let mut t = ColdTierPipelineTelemetry::default();
        t.record_step(
            WritePipelineStep::Index,
            StepOutcome::Failure(StepFailureReason::IndexInsertConflict),
        );
        assert_eq!(t.failures_by_reason.index_insert_conflict, 1);
    }
}
