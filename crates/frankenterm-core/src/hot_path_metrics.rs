//! Per-frame self-time instrumentation for the hottest loops (gauntlet FND-002).
//!
//! The performance pillar's MT8 keep-gate requires that any kept perf win name a
//! specific profile frame with >=0.1% self-time. The hot loops — `extract_delta`,
//! `ScanPipeline::process`, `Redactor::redact` — previously had only coarse
//! cumulative call-counts (`PatternTelemetry`, `IngestTelemetry`), never per-frame
//! self-time, so MT8 attribution was impossible and no `.bench-history` baseline
//! could be captured meaningfully.
//!
//! This module provides a [`HotPathTimer`] RAII guard. It is gated behind the
//! **non-default** `hot-path-metrics` Cargo feature:
//!
//! * **feature OFF (default build):** [`HotPathTimer`] is a zero-sized type and
//!   [`HotPathTimer::start`] is an inlined no-op, so the hot paths pay *nothing* —
//!   the instrumentation compiles away entirely.
//! * **feature ON:** each named frame accumulates `count` / `total_nanos` /
//!   `max_nanos` into a process-global registry; a bench or diagnostic reads them
//!   via [`snapshot`] (and clears with [`reset`]) to attribute self-time.
//!
//! No `unsafe`, asupersync-agnostic (pure `std::time::Instant` + `std::sync`).
//!
//! ```ignore
//! pub fn extract_delta(prev: &str, cur: &str, overlap: usize) -> DeltaResult {
//!     let _hpt = crate::hot_path_metrics::HotPathTimer::start("ingest.extract_delta");
//!     // ... body ...; the guard records elapsed self-time on every return path.
//! }
//! ```

#[cfg(feature = "hot-path-metrics")]
mod enabled {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    use std::time::Instant;

    /// Accumulated self-time stats for one named frame.
    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub struct FrameStats {
        /// Number of times the frame was entered.
        pub count: u64,
        /// Sum of elapsed self-time across all entries, in nanoseconds.
        pub total_nanos: u128,
        /// Largest single-entry self-time, in nanoseconds.
        pub max_nanos: u128,
    }

    impl FrameStats {
        /// Mean self-time per entry in nanoseconds (0 when `count == 0`).
        #[must_use]
        pub fn mean_nanos(&self) -> u128 {
            if self.count == 0 {
                0
            } else {
                self.total_nanos / u128::from(self.count)
            }
        }
    }

    static REGISTRY: OnceLock<Mutex<HashMap<&'static str, FrameStats>>> = OnceLock::new();

    fn registry() -> &'static Mutex<HashMap<&'static str, FrameStats>> {
        REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
    }

    /// Record one self-time sample for `name`. Lock-poisoning is treated as a
    /// drop (telemetry must never panic a hot path).
    pub fn record(name: &'static str, nanos: u128) {
        if let Ok(mut guard) = registry().lock() {
            let entry = guard.entry(name).or_default();
            entry.count += 1;
            entry.total_nanos = entry.total_nanos.saturating_add(nanos);
            if nanos > entry.max_nanos {
                entry.max_nanos = nanos;
            }
        }
    }

    /// Snapshot all recorded frames (for benches / diagnostics).
    #[must_use]
    pub fn snapshot() -> HashMap<&'static str, FrameStats> {
        registry().lock().map(|g| g.clone()).unwrap_or_default()
    }

    /// Clear all recorded frames (call before a measured run).
    pub fn reset() {
        if let Ok(mut guard) = registry().lock() {
            guard.clear();
        }
    }

    /// RAII guard: records elapsed self-time for its frame when dropped.
    #[must_use = "bind the timer to a variable so it lives for the measured scope"]
    pub struct HotPathTimer {
        name: &'static str,
        start: Instant,
    }

    impl HotPathTimer {
        /// Start timing the named frame.
        #[inline]
        pub fn start(name: &'static str) -> Self {
            Self {
                name,
                start: Instant::now(),
            }
        }
    }

    impl Drop for HotPathTimer {
        fn drop(&mut self) {
            record(self.name, self.start.elapsed().as_nanos());
        }
    }
}

#[cfg(not(feature = "hot-path-metrics"))]
mod disabled {
    /// Zero-sized no-op timer; the entire `hot-path-metrics` surface compiles away
    /// in the default build.
    #[must_use]
    pub struct HotPathTimer;

    impl HotPathTimer {
        /// No-op when the `hot-path-metrics` feature is disabled.
        #[inline(always)]
        pub fn start(_name: &'static str) -> Self {
            Self
        }
    }

    // Empty `Drop` so call sites (`let _hpt = HotPathTimer::start(...)`) read as a
    // genuine RAII guard rather than a no-effect underscore binding (keeps
    // `clippy::no_effect_underscore_binding` quiet under `-D warnings`). The drop
    // glue is empty and is elided in optimized builds — still zero cost.
    impl Drop for HotPathTimer {
        #[inline(always)]
        fn drop(&mut self) {}
    }
}

#[cfg(feature = "hot-path-metrics")]
pub use enabled::{FrameStats, HotPathTimer, record, reset, snapshot};

#[cfg(not(feature = "hot-path-metrics"))]
pub use disabled::HotPathTimer;
