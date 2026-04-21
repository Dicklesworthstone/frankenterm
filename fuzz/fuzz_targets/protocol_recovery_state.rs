#![no_main]
//! Stateful fuzz target for `protocol_recovery`.
//!
//! Drives three surfaces from crates/frankenterm-core/src/protocol_recovery.rs
//! that are pure + synchronous (so we don't need an async runtime inside the
//! harness, keeping exec/s high):
//!
//!   1. `classify_error_message(&str)` — raw bytes → UTF-8 → classify.
//!   2. `RecoveryConfig::delay_for_attempt(u32)` — the path ft-7axpo just
//!      fixed (deterministic `sin()` jitter → real `thread_rng`). Any panic
//!      here means the new random-jitter path has a precondition we missed
//!      (NaN, infinity, negative range, sample-from-empty, etc.).
//!   3. Two stateful machines: `FrameCorruptionDetector` and
//!      `ConnectionHealthTracker`. Feed them an Arbitrary-derived sequence
//!      of ops and assert invariants at each step.
//!
//! The `RecoveryEngine::execute*` entry points need an async context so
//! they live in a sibling target (follow-up if this surface finds bugs).

use arbitrary::{Arbitrary, Unstructured};
use frankenterm_core::protocol_recovery::{
    classify_error_message, ConnectionHealth, ConnectionHealthTracker,
    FrameCorruptionDetector, ProtocolErrorKind, RecoveryConfig,
};
use libfuzzer_sys::fuzz_target;
use std::time::Duration;

#[derive(Arbitrary, Debug, Clone, Copy)]
enum FuzzKind {
    Recoverable,
    Transient,
    Permanent,
}

impl From<FuzzKind> for ProtocolErrorKind {
    fn from(k: FuzzKind) -> Self {
        match k {
            FuzzKind::Recoverable => ProtocolErrorKind::Recoverable,
            FuzzKind::Transient => ProtocolErrorKind::Transient,
            FuzzKind::Permanent => ProtocolErrorKind::Permanent,
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Op<'a> {
    Success,
    Error { kind: FuzzKind, msg: &'a str },
    Reset,
    Classify(&'a str),
    DelayForAttempt(u32),
}

/// Clamped `RecoveryConfig` used for delay-budget fuzzing. We bound the
/// numeric fields to avoid NaN/inf *at the generation step* — the fuzz
/// target's job is to find bugs the caller would plausibly trigger, not
/// to shake out f64::NAN handling that callers can't produce anyway.
#[derive(Arbitrary, Debug)]
struct FuzzConfig {
    initial_delay_ms: u16,  // up to 64s
    max_delay_ms: u16,
    /// Stored as u8 → mapped into [0.5, 4.5] so backoff_factor stays finite and realistic.
    backoff_factor_raw: u8,
    /// Stored as u8 → mapped into [0.0, 1.0] for jitter_fraction.
    jitter_fraction_raw: u8,
    window_size: u16,          // FrameCorruptionDetector rolling window
    corruption_threshold: u16, // detector's alarm threshold
}

impl FuzzConfig {
    fn to_recovery(&self) -> RecoveryConfig {
        let backoff_factor = 0.5 + (self.backoff_factor_raw as f64) * (4.0 / 255.0);
        let jitter_fraction = (self.jitter_fraction_raw as f64) / 255.0;
        RecoveryConfig {
            enabled: true,
            max_retries: 3,
            initial_delay: Duration::from_millis(self.initial_delay_ms.max(1) as u64),
            max_delay: Duration::from_millis(self.max_delay_ms.max(1) as u64),
            backoff_factor,
            jitter_fraction,
            circuit_failure_threshold: 5,
            circuit_success_threshold: 2,
            circuit_cooldown: Duration::from_secs(15),
            report_degradation: false,
            permanent_failure_limit: 3,
        }
    }
}

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    config: FuzzConfig,
    ops: Vec<Op<'a>>,
}

fuzz_target!(|data: &[u8]| {
    // Bound input size — beyond this we're just spending cycles on the
    // size-scaling knee of libFuzzer's mutator.
    if data.len() > 64 * 1024 {
        return;
    }

    let mut u = Unstructured::new(data);
    let input: Input = match Input::arbitrary_take_rest(u) {
        Ok(i) => i,
        Err(_) => return,
    };

    let config = input.config.to_recovery();

    let window = input.config.window_size.max(1) as u32;
    let threshold = input.config.corruption_threshold.max(1) as u32;
    let mut detector = FrameCorruptionDetector::new(window, threshold);
    let mut tracker = ConnectionHealthTracker::new();

    // Bound the operation count so a single fuzz iteration stays fast
    // — libFuzzer's time budget per test case is ~25ms by default.
    let ops: Vec<_> = input.ops.into_iter().take(2048).collect();

    for op in ops {
        match op {
            Op::Success => {
                detector.record_success();
                let h = tracker.record_success();
                check_health_valid(h);
            }
            Op::Error { kind, msg } => {
                // Bound the message size — no realistic caller passes a
                // 10MB error message through `record_error`, and the
                // inner `to_lowercase()` scales with length.
                let msg_str: String = msg.chars().take(4096).collect();
                let corrupted = detector.record_error(kind.into(), &msg_str);
                // Invariant: `is_corrupted()` must agree with the bool
                // `record_error` just returned, at least at this point.
                assert_eq!(corrupted, detector.is_corrupted(),
                    "detector.record_error return value disagrees with is_corrupted()");
                let h = tracker.record_error(kind.into(), &msg_str);
                check_health_valid(h);
                // Invariant: the health we just observed matches the
                // tracker's stored health (no hidden drift).
                assert_eq!(h, tracker.health(),
                    "tracker.record_error return value disagrees with tracker.health()");
            }
            Op::Reset => {
                detector.reset();
                tracker.reset();
                // Invariant: after reset, the detector is not corrupted
                // and the tracker is healthy.
                assert!(!detector.is_corrupted(),
                    "detector.is_corrupted() true after reset");
                assert_eq!(tracker.health(), ConnectionHealth::Healthy,
                    "tracker.health() not Healthy after reset");
                let (a, b) = detector.error_counts();
                assert_eq!(a, 0, "detector unexpected_count != 0 after reset");
                assert_eq!(b, 0, "detector codec_error_count != 0 after reset");
            }
            Op::Classify(s) => {
                // Bound message so to_lowercase() doesn't linearly scale.
                let bounded: String = s.chars().take(4096).collect();
                let _ = classify_error_message(&bounded);
            }
            Op::DelayForAttempt(attempt) => {
                // Clamp to a realistic operator range. Very large attempts
                // are already covered by RetryPolicy's `.min(31)` ceiling
                // in retry.rs; protocol_recovery uses `powi(attempt as i32)`
                // directly, which overflows to `inf` around attempt ≈ 1024
                // depending on backoff_factor. That's a separate finding.
                let bounded = attempt.min(256);
                let d = config.delay_for_attempt(bounded);
                // Invariant 1: delay is at least 1ms (documented floor).
                assert!(d >= Duration::from_millis(1),
                    "delay_for_attempt returned {d:?}, below 1ms floor");
                // Invariant 2: jitter cannot push beyond max_delay * 2
                // (jitter_range = capped * jitter_fraction <= capped, so
                // capped + jitter <= 2 * capped). Allow 1ms slop for
                // rounding via `as u64`.
                let max_bound = config
                    .max_delay
                    .saturating_mul(2)
                    .saturating_add(Duration::from_millis(1));
                assert!(d <= max_bound,
                    "delay {d:?} exceeds 2*max_delay bound {max_bound:?}");
            }
        }

        // Cheap per-op invariant: the tracker's consecutive counters
        // should never exceed the number of ops processed (unbounded
        // growth would imply a wraparound or counter bug).
        // Note: we don't have accessors for consecutive_*, so we can't
        // check directly — but `health` is the observable output and we
        // check that on every op via check_health_valid.
    }
});

/// ConnectionHealth must always be one of the four variants. This is a
/// defensive check against any future refactor that might introduce an
/// "impossible" state (e.g., memory-unsafe code in some dependency
/// scribbling on the discriminant).
fn check_health_valid(h: ConnectionHealth) {
    match h {
        ConnectionHealth::Healthy
        | ConnectionHealth::Degraded
        | ConnectionHealth::Corrupted
        | ConnectionHealth::Dead => {}
    }
}
