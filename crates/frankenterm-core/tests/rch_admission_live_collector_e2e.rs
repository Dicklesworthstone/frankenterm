//! E2E tests for the rch_admission live collector + doctor surface
//! (ft-69gwh.9 collector, ft-69gwh.5 surface; tracked by ft-ux9kb).
//!
//! These prove the admission diagnosis is backed by REAL probed state rather
//! than stub/canned data:
//!   * `collect_live_rch_admission_input` runs real read-only probes (disk
//!     write probes, `df`, `git status`, `.beads` writeability, agent-mail
//!     bounded-skip) and populates a non-stub `RchAdmissionCollectorInput`.
//!   * the doctor surface renders the advisory not-proof banner and is
//!     data-driven — a synthetic ENOSPC input surfaces a real reason code and
//!     changes the output, so the surface cannot be emitting canned text.
//!
//! ZERO-RCH: probes are status queries / self-cleaning write probes only. `rch`
//! itself may be absent (recorded as an observation, never required).

use std::path::{Path, PathBuf};

use frankenterm_core::rch_admission::{
    build_rch_admission_report, collect_live_rch_admission_input, RchAdmissionCommandDiagnostic,
    RchAdmissionCollectorInput, RchAdmissionLocalDiskDiagnostic, RchAdmissionProbeDiagnostic,
    RchAdmissionProbeStatus,
};
use frankenterm_core::rch_admission_surface::{RchAdmissionSurface, RchAdmissionSurfaceKind};

/// Repository root = two levels above this crate's manifest dir
/// (`crates/frankenterm-core` -> repo root).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above the crate manifest dir")
        .to_path_buf()
}

const PROOF_CMD: &str = "cargo test -p frankenterm-core --lib";

#[test]
fn live_collector_populates_from_real_probes_not_stub() {
    let root = repo_root();
    let input = collect_live_rch_admission_input(&root, PROOF_CMD, 1_700_000_000_000);

    // Source is stamped by the live collector entry point.
    assert_eq!(input.source, "ft doctor --rch-admission");

    // The disk write probes actually RAN (Ok or Failed) — not the
    // `not_checked()` stub default. This is the core "no fake data" signal:
    // the collector executed a real self-cleaning write against the repo and
    // the temp dir.
    assert_ne!(
        input.local_disk.repo_write_probe.status,
        RchAdmissionProbeStatus::NotChecked,
        "repo write probe must run, got stub NotChecked: {:?}",
        input.local_disk.repo_write_probe
    );
    assert_ne!(
        input.local_disk.rch_cache_write_probe.status,
        RchAdmissionProbeStatus::NotChecked,
        "cache write probe must run, got stub NotChecked"
    );

    // The whole local-disk diagnostic must differ from the unprobed stub.
    assert_ne!(
        input.local_disk,
        RchAdmissionLocalDiskDiagnostic::not_checked(),
        "local_disk must be populated by real probes, not the not_checked() stub"
    );

    // `df` produced a real free-byte figure for the repo volume.
    assert!(
        input.local_disk.system_data_free_bytes.is_some(),
        "df probe must populate system_data_free_bytes"
    );

    // The collector always runs to completion past agent-mail, recording it as a
    // bounded skip rather than hanging on the shared singleton.
    assert!(
        input
            .collector_observations
            .iter()
            .any(|obs| obs.source_id == "agent_mail"),
        "agent-mail must be recorded as a bounded skip"
    );

    // A real report is buildable from the live input (the ft-69gwh.9 gap was
    // that no production caller ever produced a live input to report on).
    let report = build_rch_admission_report(&input);
    assert!(!report.source.is_empty(), "report must carry a source");
}

#[test]
fn doctor_surface_reflects_live_input_with_advisory_banner() {
    let root = repo_root();
    let input = collect_live_rch_admission_input(&root, PROOF_CMD, 1_700_000_000_000);
    let report = build_rch_admission_report(&input);
    let surface = RchAdmissionSurface::from_report(RchAdmissionSurfaceKind::Doctor, report);
    let lines = surface.doctor_lines();

    // The advisory not-proof banner is ALWAYS the first line — the surface must
    // never let an operator mistake admission advice for a green build proof.
    assert!(surface.advisory_only, "doctor surface must be advisory-only");
    assert_eq!(
        lines.first().map(String::as_str),
        Some(surface.not_proof_banner.as_str()),
        "first doctor line must be the not-proof banner"
    );

    let text = lines.join("\n");
    assert!(text.contains("surface: doctor"), "surface kind rendered");
    assert!(text.contains("proof_status:"), "proof status rendered");
    assert!(text.contains("citations:"), "citation count rendered");
    // The citation count is derived from the report's retained evidence, not a
    // fixed literal — it must agree with the surface's own field.
    assert!(
        text.contains(&format!("citations: {}", surface.citation_count)),
        "citation count must reflect the report's real evidence rows"
    );
}

#[test]
fn doctor_surface_is_data_driven_not_canned() {
    // Two inputs that differ only in disk health must produce different doctor
    // output, and the ENOSPC one must surface a real reason code. If the
    // surface emitted canned text this would fail.
    fn input_with_disk(disk: RchAdmissionLocalDiskDiagnostic) -> RchAdmissionCollectorInput {
        RchAdmissionCollectorInput::new(
            1_700_000_000_000,
            "ft doctor --rch-admission",
            RchAdmissionCommandDiagnostic::new(PROOF_CMD),
        )
        .with_local_disk(disk)
    }

    let healthy_disk = RchAdmissionLocalDiskDiagnostic {
        system_data_free_bytes: Some(200 * 1024 * 1024 * 1024),
        private_tmp_free_bytes: Some(200 * 1024 * 1024 * 1024),
        repo_write_probe: RchAdmissionProbeDiagnostic::ok("repo writeable"),
        rch_cache_write_probe: RchAdmissionProbeDiagnostic::ok("cache writeable"),
    };
    let enospc_disk = RchAdmissionLocalDiskDiagnostic {
        repo_write_probe: RchAdmissionProbeDiagnostic::failed(
            "ENOSPC",
            "repo write failed: No space left on device (os error 28)",
        ),
        ..healthy_disk.clone()
    };

    let healthy_surface = RchAdmissionSurface::from_report(
        RchAdmissionSurfaceKind::Doctor,
        build_rch_admission_report(&input_with_disk(healthy_disk)),
    );
    let enospc_surface = RchAdmissionSurface::from_report(
        RchAdmissionSurfaceKind::Doctor,
        build_rch_admission_report(&input_with_disk(enospc_disk)),
    );

    let healthy_lines = healthy_surface.doctor_lines();
    let enospc_lines = enospc_surface.doctor_lines();

    // Data-driven: the rendered output changes with the input.
    assert_ne!(
        healthy_lines, enospc_lines,
        "doctor output must reflect the input (not canned)"
    );

    // The ENOSPC input must surface at least one real reason code that the
    // healthy input does not.
    assert!(
        enospc_surface.reasons.len() > healthy_surface.reasons.len(),
        "ENOSPC disk failure must surface an extra reason code (healthy={}, enospc={})",
        healthy_surface.reasons.len(),
        enospc_surface.reasons.len()
    );

    // ...and that reason must be visible in the human doctor block, mentioning
    // the disk-space condition rather than generic boilerplate.
    let enospc_text = enospc_lines.join("\n").to_lowercase();
    assert!(
        enospc_text.contains("space") || enospc_text.contains("enospc") || enospc_text.contains("disk"),
        "ENOSPC doctor output must name the disk-space condition; got:\n{enospc_text}"
    );

    // Both surfaces still lead with the advisory banner.
    assert_eq!(
        healthy_lines.first(),
        Some(&healthy_surface.not_proof_banner)
    );
    assert_eq!(enospc_lines.first(), Some(&enospc_surface.not_proof_banner));
}
