//! Lindley-bounds diagnostic producer (br-ft-43x69 substrate-pass).
//!
//! Computes a model comparison; it does not run a benchmark or establish
//! measured provenance. Absent model/empirical inputs use HISTORICAL
//! documentation defaults, never a current release measurement.
//!
//! ## Telemetry input
//!
//! `FT_LINDLEY_STAGE_TELEMETRY_JSON` may contain a serialized
//! `LindleyTelemetryModel`. `FT_LINDLEY_STAGE_TELEMETRY_PATH` may point
//! at a file with the same JSON. If neither is set, the example uses
//! `LindleyTelemetryModel::documented_default()`. Explicit input must be
//! UTF-8 without NUL bytes and at most 64 KiB, including JSON whitespace.
//! File reads consume at most 64 KiB plus one byte to detect oversize input;
//! oversized input is rejected, never silently truncated.
//!
//! `FT_LINDLEY_EMPIRICAL_P99_MS` accepts only finite nonnegative numbers.
//! Only absence selects the historical 8.5ms reference. Empty, malformed,
//! negative, non-finite and non-Unicode values fail with exit 2.
//!
//! `FT_RELEASE_VERSION` defaults to `0.0.0-substrate`. Any other version
//! requires explicit model and empirical inputs plus matching
//! `FT_LINDLEY_INPUT_SHA256=sha256:<64 lowercase hex digits>`.
//! The digest covers the exact UTF-8 `input_provenance.payload_json` bytes:
//! compact serde_json serialization of `InputPayload`, in field order
//! `telemetry_model`, `empirical_p99_ms`, without a trailing newline. Model
//! fields use their declared serde order and parsed numeric values. The
//! encoding is `serde-json-lindley-inputs-v1`; it is not general canonical JSON.
//! Optional `FT_LINDLEY_INPUT_ORIGIN` records a caller-declared external origin.
//! A matching digest proves input binding only, not measurement authenticity,
//! source identity, performance coverage or release readiness.
//!
//! `FT_LINDLEY_BOUNDS_EMIT_JSON_MARKERS=1` opts into fixed stdout boundary
//! markers for the RCH wrapper. No caller-selected marker text is accepted.
//!
//! ## Usage
//!
//! ```text
//! cargo run --locked -j 1 --example lindley_bounds_build \
//!     -p frankenterm-core --no-default-features
//! ```
//!
//! The default development profile runs a diagnostic calculation; it provides
//! no performance measurement.
//!
//! Or via the wrapper:
//!
//! ```text
//! bash scripts/lindley-bounds-build.sh
//! ```
//!
//! ## Exit codes
//!
//! Exits 0 for a comparison within tolerance, 1 for a failed or undefined
//! comparison (with diagnostic JSON), and 2 for invalid input/encoding.
//! Exit 0 alone is not release evidence or proof of an upper bound: the
//! separate `exceeds_analytical_bound` field reports empirical exceedance.

use frankenterm_core::latency_stages::LindleyTelemetryModel;
use frankenterm_core::network_calculus_bound::{LindleyBoundsArtifact, pipeline_delay_bound};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::Read;
use std::process::ExitCode;

const HISTORICAL_VERSION: &str = "0.0.0-substrate";
const JSON_BEGIN: &str = "__FT_LINDLEY_BOUNDS_JSON_BEGIN__";
const JSON_END: &str = "__FT_LINDLEY_BOUNDS_JSON_END__";
const MAX_TELEMETRY_BYTES: usize = 64 * 1024;

#[derive(Serialize)]
struct InputPayload<'a> {
    telemetry_model: &'a LindleyTelemetryModel,
    empirical_p99_ms: f64,
}

fn main() -> ExitCode {
    match build_diagnostic() {
        Ok(within_tolerance) => {
            if within_tolerance {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(error) => {
            eprintln!("lindley_bounds_build: {error}");
            ExitCode::from(2)
        }
    }
}

fn build_diagnostic() -> Result<bool, String> {
    let emit_markers = match optional_env("FT_LINDLEY_BOUNDS_EMIT_JSON_MARKERS")?.as_deref() {
        None => false,
        Some("1") => true,
        Some(_) => return Err("FT_LINDLEY_BOUNDS_EMIT_JSON_MARKERS must be absent or 1".into()),
    };
    let (model, model_source) = load_lindley_model()?;
    let empirical_input = optional_env("FT_LINDLEY_EMPIRICAL_P99_MS")?;
    let empirical_p99_ms = parse_empirical(empirical_input.as_deref())?;
    let release_version =
        optional_env("FT_RELEASE_VERSION")?.unwrap_or_else(|| HISTORICAL_VERSION.to_string());
    if release_version.is_empty() || release_version.trim() != release_version {
        return Err("FT_RELEASE_VERSION must be nonempty without surrounding whitespace".into());
    }
    let declared_digest = optional_env("FT_LINDLEY_INPUT_SHA256")?;
    let declared_origin = optional_env("FT_LINDLEY_INPUT_ORIGIN")?;
    if declared_origin
        .as_deref()
        .is_some_and(|origin| origin.trim().is_empty())
    {
        return Err("FT_LINDLEY_INPUT_ORIGIN must be nonempty when supplied".into());
    }
    let (arrival, stages) = model
        .to_network_calculus_inputs()
        .map_err(|error| format!("invalid Lindley telemetry: {error}"))?;
    let payload_json = serde_json::to_string(&InputPayload {
        telemetry_model: &model,
        empirical_p99_ms,
    })
    .map_err(|error| format!("failed to serialize input payload: {error}"))?;
    let actual_digest = format!(
        "sha256:{}",
        hex::encode(Sha256::digest(payload_json.as_bytes()))
    );
    validate_input_binding(
        &release_version,
        model_source != "historical_documented_default",
        empirical_input.is_some(),
        declared_digest.as_deref(),
        &actual_digest,
    )?;
    let historical = model_source == "historical_documented_default" || empirical_input.is_none();
    if historical {
        eprintln!("lindley_bounds_build: HISTORICAL diagnostic inputs; not a release measurement");
    }

    // Compute the analytical bound from the substrate's
    // `pipeline_delay_bound` (Pay-Bursts-Only-Once composition + Lindley
    // delay bound). The historical default yields about 8.067ms;
    // supplied models can yield a different or unrepresentable bound.
    let analytical_bound_ms = pipeline_delay_bound(arrival, &stages).unwrap_or_else(|| {
        eprintln!(
            "lindley_bounds_build: pipeline_delay_bound returned None — \
             arrival/stages combination is unstable or its bound is unrepresentable"
        );
        f64::INFINITY
    });

    let artifact = LindleyBoundsArtifact {
        release_version,
        arrival,
        stages,
        analytical_bound_ms,
        empirical_p99_ms,
    };

    let mut diagnostic: serde_json::Value =
        serde_json::from_str(&artifact.render_attestation_json())
            .map_err(|error| format!("substrate emitted invalid diagnostic JSON: {error}"))?;
    diagnostic["exceeds_analytical_bound"] =
        serde_json::json!(artifact.comparison().exceeds_bound());
    diagnostic["input_provenance"] = serde_json::json!({
        "status": if historical { "historical_diagnostic" } else { "supplied_inputs_unverified" },
        "model_source": model_source,
        "empirical_source": if empirical_input.is_some() { "caller_supplied" } else { "historical_8_5_ms_reference" },
        "payload_encoding": "serde-json-lindley-inputs-v1",
        "payload_json": payload_json,
        "input_sha256": actual_digest,
        "declared_input_sha256": declared_digest,
        "input_binding_verified": declared_digest.is_some(),
        "declared_external_origin": declared_origin,
        "measurement_provenance_verified": false,
        "release_ready": false,
    });
    let json = serde_json::to_string_pretty(&diagnostic)
        .map_err(|error| format!("failed to encode diagnostic: {error}"))?;
    if emit_markers {
        println!("{JSON_BEGIN}");
    }
    println!("{json}");
    if emit_markers {
        println!("{JSON_END}");
    }

    let comparison = artifact.comparison();
    if comparison.within_tolerance() {
        Ok(true)
    } else {
        eprintln!(
            "lindley_bounds_build: tolerance check FAILED. \
             analytical_bound_ms={analytical} empirical_p99_ms={empirical} \
             deviation_pct={dev:.2} (substrate's TOLERANCE_PCT=20.0)",
            analytical = artifact.analytical_bound_ms,
            empirical = artifact.empirical_p99_ms,
            dev = comparison.deviation_pct().unwrap_or(f64::NAN),
        );
        Ok(false)
    }
}

fn optional_env(name: &str) -> Result<Option<String>, String> {
    decode_env(name, env::var(name))
}

fn decode_env(name: &str, value: Result<String, env::VarError>) -> Result<Option<String>, String> {
    match value {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(format!("{name} must be valid Unicode")),
    }
}

fn parse_empirical(input: Option<&str>) -> Result<f64, String> {
    let Some(input) = input else {
        return Ok(8.5);
    };
    input
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value >= 0.0)
        .ok_or_else(|| "FT_LINDLEY_EMPIRICAL_P99_MS must be a finite nonnegative number".into())
}

fn validate_input_binding(
    release_version: &str,
    explicit_model: bool,
    explicit_empirical: bool,
    declared_digest: Option<&str>,
    actual_digest: &str,
) -> Result<(), String> {
    if declared_digest.is_some_and(|digest| digest != actual_digest) {
        return Err(
            "FT_LINDLEY_INPUT_SHA256 does not match the serialized model and empirical input"
                .into(),
        );
    }
    if release_version != HISTORICAL_VERSION
        && (!explicit_model || !explicit_empirical || declared_digest.is_none())
    {
        return Err("a release version requires explicit telemetry, empirical p99 and matching FT_LINDLEY_INPUT_SHA256; defaults are HISTORICAL diagnostics".into());
    }
    Ok(())
}

fn load_lindley_model() -> Result<(LindleyTelemetryModel, &'static str), String> {
    let json = optional_env("FT_LINDLEY_STAGE_TELEMETRY_JSON")?;
    let path = optional_env("FT_LINDLEY_STAGE_TELEMETRY_PATH")?;
    if json.is_some() && path.is_some() {
        return Err("supply one telemetry input: JSON or PATH, not both".into());
    }
    if let Some(json) = json {
        return parse_lindley_model(json.as_bytes(), "FT_LINDLEY_STAGE_TELEMETRY_JSON")
            .map(|model| (model, "caller_supplied_json"));
    }
    if let Some(path) = path {
        let file = fs::File::open(&path)
            .map_err(|error| format!("failed to open telemetry file {path}: {error}"))?;
        return read_lindley_model(file, &format!("Lindley telemetry file {path}"))
            .map(|model| (model, "caller_supplied_file"));
    }

    Ok((
        LindleyTelemetryModel::documented_default(),
        "historical_documented_default",
    ))
}

fn read_lindley_model(reader: impl Read, source: &str) -> Result<LindleyTelemetryModel, String> {
    let mut bytes = Vec::with_capacity(MAX_TELEMETRY_BYTES + 1);
    reader
        .take((MAX_TELEMETRY_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read {source}: {error}"))?;
    parse_lindley_model(&bytes, source)
}

fn parse_lindley_model(bytes: &[u8], source: &str) -> Result<LindleyTelemetryModel, String> {
    if bytes.len() > MAX_TELEMETRY_BYTES {
        return Err(format!("{source}: telemetry exceeds 65536-byte limit"));
    }
    if bytes.contains(&0) {
        return Err(format!("{source}: telemetry must not contain NUL bytes"));
    }
    let json = std::str::from_utf8(bytes)
        .map_err(|_| format!("{source}: telemetry must be valid UTF-8"))?;
    serde_json::from_str(json).map_err(|error| format!("invalid {source}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absence_is_distinct_from_invalid_empirical_input() {
        assert_eq!(parse_empirical(None).unwrap().to_bits(), 8.5_f64.to_bits());
        assert_eq!(
            parse_empirical(Some("0")).unwrap().to_bits(),
            0.0_f64.to_bits()
        );
        for input in ["", "garbage", "-1", "NaN", "inf", "-inf", "1e999", " 8.5"] {
            assert!(parse_empirical(Some(input)).is_err(), "input={input:?}");
        }
        assert!(
            decode_env("EMPIRICAL", Err(env::VarError::NotPresent))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            decode_env("EMPIRICAL", Ok(String::new())).unwrap(),
            Some(String::new())
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_environment_is_not_an_absent_default() {
        use std::os::unix::ffi::OsStringExt;
        let invalid = std::ffi::OsString::from_vec(vec![0xff]);
        assert!(decode_env("EMPIRICAL", Err(env::VarError::NotUnicode(invalid))).is_err());
    }

    #[test]
    fn telemetry_limit_applies_before_parsing_without_truncation() {
        let model = LindleyTelemetryModel::documented_default();
        let mut bytes = serde_json::to_vec(&model).unwrap();
        bytes.resize(MAX_TELEMETRY_BYTES, b' ');
        assert_eq!(parse_lindley_model(&bytes, "inline").unwrap(), model);
        assert_eq!(
            read_lindley_model(bytes.as_slice(), "reader").unwrap(),
            model
        );

        // A valid JSON prefix plus whitespace must still fail above the cap.
        bytes.resize(MAX_TELEMETRY_BYTES * 2, b' ');
        assert!(
            parse_lindley_model(&bytes, "inline")
                .unwrap_err()
                .contains("exceeds 65536-byte limit")
        );
        let mut reader = std::io::Cursor::new(bytes);
        assert!(
            read_lindley_model(&mut reader, "reader")
                .unwrap_err()
                .contains("exceeds 65536-byte limit")
        );
        assert_eq!(reader.position(), (MAX_TELEMETRY_BYTES + 1) as u64);
    }

    #[test]
    fn telemetry_encoding_is_validated_before_deserialization() {
        for (bytes, expected) in [
            (b"\xff".as_slice(), "must be valid UTF-8"),
            (b"{}\0".as_slice(), "must not contain NUL bytes"),
        ] {
            assert!(
                parse_lindley_model(bytes, "inline")
                    .unwrap_err()
                    .contains(expected)
            );
            assert!(
                read_lindley_model(bytes, "reader")
                    .unwrap_err()
                    .contains(expected)
            );
        }
    }

    #[test]
    fn release_binding_checks_both_input_values() {
        let model = LindleyTelemetryModel::documented_default();
        let encode = |model: &LindleyTelemetryModel, empirical_p99_ms| {
            serde_json::to_string(&InputPayload {
                telemetry_model: model,
                empirical_p99_ms,
            })
            .unwrap()
        };
        let original = encode(&model, 8.5);
        let digest = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(original.as_bytes()))
        );
        assert!(validate_input_binding("1.2.3", true, true, Some(&digest), &digest).is_ok());
        for (model_present, empirical_present, declared) in [
            (false, true, Some(digest.as_str())),
            (true, false, Some(digest.as_str())),
            (true, true, None),
        ] {
            assert!(
                validate_input_binding(
                    "1.2.3",
                    model_present,
                    empirical_present,
                    declared,
                    &digest
                )
                .is_err()
            );
        }
        let mut altered = model.clone();
        altered.arrival_burst_events += 1.0;
        for changed in [encode(&altered, 8.5), encode(&model, 8.6)] {
            let changed_digest =
                format!("sha256:{}", hex::encode(Sha256::digest(changed.as_bytes())));
            assert!(
                validate_input_binding("1.2.3", true, true, Some(&digest), &changed_digest)
                    .is_err()
            );
        }
    }
}
