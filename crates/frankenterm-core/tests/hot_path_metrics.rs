//! FND-002 / MT8: under the `hot-path-metrics` feature, the hot loops record
//! per-frame self-time so a bench can attribute >=0.1% self-time and a
//! `.bench-history` baseline becomes meaningful. The whole file is gated on the
//! feature; the default-build (feature-off) no-op path is covered by the workspace
//! compiling at all with the zero-sized timer.
//!
//! Single test fn on purpose: the registry is process-global and cargo runs tests
//! within a binary concurrently, so splitting record/reset assertions across tests
//! would race. Everything here is sequential.
#![cfg(feature = "hot-path-metrics")]

use frankenterm_core::hot_path_metrics::{reset, snapshot};
use frankenterm_core::ingest::extract_delta;
use frankenterm_core::redactor::Redactor;

#[test]
fn hot_path_timers_record_self_time_and_reset_clears() {
    reset();

    // Exercise extract_delta (append + overlap + first-capture cases).
    let _ = extract_delta("abc", "abcdef", 4096);
    let _ = extract_delta("hello", "hello world", 4096);
    let _ = extract_delta("", "fresh", 4096);

    // Exercise the redactor (clean + secret-bearing).
    let r = Redactor::new();
    let _ = r.redact("no secrets here");
    let _ = r.redact("leaked sk-proj-abcdefghijklmnopqrstuvwxyz012345");

    let snap = snapshot();

    let delta = snap
        .get("ingest.extract_delta")
        .expect("extract_delta frame must be recorded");
    assert!(delta.count >= 3, "extract_delta count was {}", delta.count);
    assert!(delta.total_nanos > 0, "extract_delta total_nanos must be > 0");
    // max is the largest single sample; it can never be below the mean.
    assert!(
        delta.max_nanos >= delta.mean_nanos(),
        "max_nanos {} < mean_nanos {}",
        delta.max_nanos,
        delta.mean_nanos()
    );

    let red = snap
        .get("redactor.redact")
        .expect("redactor.redact frame must be recorded");
    assert!(red.count >= 2, "redactor.redact count was {}", red.count);
    assert!(red.total_nanos > 0, "redactor.redact total_nanos must be > 0");

    // reset() clears the registry.
    reset();
    let cleared = snapshot();
    assert!(
        !cleared.contains_key("ingest.extract_delta")
            && !cleared.contains_key("redactor.redact"),
        "reset() must clear the registry, got {} frames",
        cleared.len()
    );
}
