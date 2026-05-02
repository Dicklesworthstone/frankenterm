//! Lindley-bounds attestation generator (br-ft-43x69 substrate-pass).
//!
//! Constructs a `LindleyBoundsArtifact` from live
//! `latency_stages.rs` telemetry, or from the documented fallback model
//! when release jobs do not pass telemetry, and emits the canonical JSON
//! to stdout via the substrate's
//! `LindleyBoundsArtifact::render_attestation_json`. The release
//! pipeline pipes this into
//! `docs/attestations/perf/lindley-bounds.json`, which the attestation
//! bundle build (`scripts/attestation-build.sh`) hashes into the
//! `perf/lindley-bounds` slot.
//!
//! ## Telemetry input
//!
//! `FT_LINDLEY_STAGE_TELEMETRY_JSON` may contain a serialized
//! `LindleyTelemetryModel`. `FT_LINDLEY_STAGE_TELEMETRY_PATH` may point
//! at a file with the same JSON. If neither is set, the example uses
//! `LindleyTelemetryModel::documented_default()`.
//!
//! Empirical p99 is supplied via the `FT_LINDLEY_EMPIRICAL_P99_MS`
//! environment variable, defaulting to `8.5` (matching the operator
//! doc's reference value). Release wrappers should set this from the
//! bench harness output.
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

use frankenterm_core::latency_stages::LindleyTelemetryModel;
use frankenterm_core::network_calculus_bound::{pipeline_delay_bound, LindleyBoundsArtifact};
use std::env;
use std::fs;
use std::process::ExitCode;

fn main() -> ExitCode {
    let model = match load_lindley_model() {
        Ok(model) => model,
        Err(error) => {
            eprintln!("lindley_bounds_build: {error}");
            return ExitCode::from(2);
        }
    };

    let (arrival, stages) = match model.to_network_calculus_inputs() {
        Ok(inputs) => inputs,
        Err(error) => {
            eprintln!("lindley_bounds_build: invalid Lindley telemetry: {error}");
            return ExitCode::from(2);
        }
    };

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

fn load_lindley_model() -> Result<LindleyTelemetryModel, String> {
    if let Ok(json) = env::var("FT_LINDLEY_STAGE_TELEMETRY_JSON") {
        return serde_json::from_str(&json)
            .map_err(|error| format!("invalid FT_LINDLEY_STAGE_TELEMETRY_JSON: {error}"));
    }

    if let Ok(path) = env::var("FT_LINDLEY_STAGE_TELEMETRY_PATH") {
        let json =
            fs::read_to_string(&path).map_err(|error| format!("failed to read {path}: {error}"))?;
        return serde_json::from_str(&json)
            .map_err(|error| format!("invalid Lindley telemetry file {path}: {error}"));
    }

    Ok(LindleyTelemetryModel::documented_default())
}
