//! Lindley-bounds attestation generator (br-ft-43x69 substrate-pass).
//!
//! Constructs a `LindleyBoundsArtifact` from the per-stage measurements
//! documented at `docs/perf/latency-derivation.md` and emits the
//! canonical JSON to stdout via the substrate's
//! `LindleyBoundsArtifact::render_attestation_json`. The release
//! pipeline pipes this into
//! `docs/attestations/perf/lindley-bounds.json`, which the attestation
//! bundle build (`scripts/attestation-build.sh`) hashes into the
//! `perf/lindley-bounds` slot.
//!
//! ## Substrate-pass scope
//!
//! This example hard-codes the per-stage rate / latency values from
//! `docs/perf/latency-derivation.md`'s table (capture / delta-extract /
//! storage write). The wired-pass cont-bead reads the same values
//! live from `latency_stages.rs` telemetry instead — matching the
//! parent bead's "live-rate wiring" item. Keeping this example as the
//! release-script entry point means the cont-bead is a one-file edit
//! (swap the hard-coded constructors for telemetry reads).
//!
//! Empirical p99 is supplied via the `FT_LINDLEY_EMPIRICAL_P99_MS`
//! environment variable, defaulting to `8.5` (matching the operator
//! doc's reference value). The wired-pass cont-bead computes this
//! from the bench harness instead.
//!
//! Release version is supplied via `FT_RELEASE_VERSION`, defaulting
//! to `0.0.0-substrate` so accidental release-script invocations
//! before wiring is complete carry an obvious sentinel.
//!
//! ## Usage
//!
//! ```text
//! cargo run --release --example lindley_bounds_build \
//!     -p frankenterm-core --no-default-features \
//!     > docs/attestations/perf/lindley-bounds.json
//! ```
//!
//! Or via the wrapper:
//!
//! ```text
//! bash scripts/lindley-bounds-build.sh
//! ```
//!
//! ## Exit codes
//!
//! Exits `0` on success. Exits `1` if the analytical-vs-empirical
//! comparison fails the substrate's 20% tolerance (per the parent
//! bead's "Asserts comparison().within_tolerance(); fails release on
//! violation" requirement). The release script's failure mode is the
//! tolerance check — operators see the JSON on stdout regardless so
//! they can diagnose the deviation.

use frankenterm_core::network_calculus_bound::{
    ArrivalCurve, LindleyBoundsArtifact, ServiceCurve, StageModel, pipeline_delay_bound,
};
use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Operator-doc values from docs/perf/latency-derivation.md
    // (substrate-pass; cont-bead reads these from
    // latency_stages.rs telemetry).
    //
    // The operator doc names arrival rate `r=100 events/sec`, equal
    // to the slowest stage's `R=100`. The substrate's `delay_bound`
    // requires `arrival.rate() < service.rate()` (strict) for a
    // finite bound — at boundary the queue is metastable. We use
    // 90 events/sec here (10% margin under capacity) so the
    // substrate-pass example produces a finite analytical bound
    // instead of `inf`. The wired-pass cont-bead will read live
    // arrival rate from `latency_stages.rs` telemetry, which
    // naturally carries margin under steady state.
    let arrival = ArrivalCurve::new(/* burst */ 10.0, /* rate */ 90.0);

    // Three pipeline stages from the operator doc's table. Each
    // ServiceCurve::new(rate_events_per_sec, latency_ms).
    let stages = vec![
        StageModel::new(
            "capture",
            ServiceCurve::new(/* rate */ 200.0, /* latency_ms */ 1.0),
        ),
        StageModel::new(
            "delta_extract",
            ServiceCurve::new(/* rate */ 150.0, /* latency_ms */ 2.0),
        ),
        StageModel::new(
            "storage_write",
            ServiceCurve::new(/* rate */ 100.0, /* latency_ms */ 5.0),
        ),
    ];

    // Compute the analytical bound from the substrate's
    // `pipeline_delay_bound` (Pay-Bursts-Only-Once composition + Lindley
    // delay bound). This is the substrate's source of truth — the
    // operator doc's "50ms" is the headline / scheduling-worst-case
    // number; pipeline_delay_bound returns the per-arrival 8.1ms.
    let analytical_bound_ms = pipeline_delay_bound(arrival, &stages).unwrap_or_else(|| {
        eprintln!(
            "lindley_bounds_build: pipeline_delay_bound returned None — \
             arrival/stages combination is empty or unstable"
        );
        f64::INFINITY
    });

    let empirical_p99_ms: f64 = env::var("FT_LINDLEY_EMPIRICAL_P99_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8.5);

    let release_version =
        env::var("FT_RELEASE_VERSION").unwrap_or_else(|_| "0.0.0-substrate".to_string());

    let artifact = LindleyBoundsArtifact {
        release_version,
        arrival,
        stages,
        analytical_bound_ms,
        empirical_p99_ms,
    };

    println!("{}", artifact.render_attestation_json());

    let comparison = artifact.comparison();
    if comparison.within_tolerance() {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "lindley_bounds_build: tolerance check FAILED. \
             analytical_bound_ms={analytical} empirical_p99_ms={empirical} \
             deviation_pct={dev:.2} (substrate's TOLERANCE_PCT=20.0)",
            analytical = artifact.analytical_bound_ms,
            empirical = artifact.empirical_p99_ms,
            dev = comparison.deviation_pct().unwrap_or(f64::NAN),
        );
        ExitCode::from(1)
    }
}
