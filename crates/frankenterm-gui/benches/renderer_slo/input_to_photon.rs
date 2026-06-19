//! Input-to-photon renderer SLO Criterion substrate.
//!
//! This bench intentionally writes a structured evidence row before Criterion
//! starts sampling. Criterion output proves timing regressions; the JSONL row
//! tells the release-attestation consumer whether the SLO was measured or
//! degraded because GPU/photon instrumentation was unavailable.

use std::fs::{self, OpenOptions};
use std::hint::black_box;
use std::io::Write;
use std::mem::size_of;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_gui::glyph_quad_staging::{
    GlyphQuadSoaBuffers, GlyphQuadStagingInstance, GlyphQuadStagingVertex, VERTICES_PER_GLYPH_QUAD,
    aos_glyph_quad_vertices, moonshot_instanced_glyph_quads_enabled,
};
use frankenterm_gui::glyph_run_interning::{
    GlyphRunProbeGlyph, glyph_run_interning_enabled, glyph_run_probe_iteration,
};
use frankenterm_gui::headless_render::render_headless;
use frankenterm_gui::renderer_slo::headless::{
    known_key_headless_input, trace_from_headless_frame,
};
use frankenterm_gui::renderer_slo::{
    INPUT_TO_PHOTON_CLAIM_ID, InputToPhotonState, summarize_input_to_photon_traces,
    unavailable_evidence,
};
use serde_json::json;

fn bench_known_key_input_to_headless_frame(c: &mut Criterion) {
    let input = known_key_headless_input();
    if render_headless(&input).is_err() {
        c.bench_function("input_to_photon/headless_unavailable_noop", |b| {
            b.iter(|| black_box(()));
        });
        return;
    }

    c.bench_function("input_to_photon/known_key_headless_frame", |b| {
        b.iter(|| {
            let frame = render_headless(black_box(&input))
                .expect("headless renderer must be available for measured Criterion run");
            black_box(frame.rgba.len());
        });
    });
}

fn bench_ft_3r0yk_soa_quad_staging_toggle(c: &mut Criterion) {
    let fixture = SoaQuadBenchFixture::glyph_dense_frame(160, 72);
    c.bench_function("ft_3r0yk/soa_quad_staging_toggle", |b| {
        b.iter(|| {
            let prepared_bytes = if moonshot_instanced_glyph_quads_enabled() {
                fixture.soa_instance_upload_bytes()
            } else {
                fixture.expand_aos_baseline_bytes()
            };
            black_box(prepared_bytes);
        });
    });
}

fn bench_ft_egok5_glyph_run_interning_toggle(c: &mut Criterion) {
    let glyphs = glyph_run_probe_fixture(64);
    c.bench_function("ft_egok5/glyph_run_interning_toggle", |b| {
        b.iter(|| {
            let retained = if glyph_run_interning_enabled() {
                glyph_run_probe_iteration(black_box(&glyphs), 16)
            } else {
                glyph_run_probe_disabled_iteration(black_box(&glyphs), 16)
            };
            black_box(retained);
        });
    });
}

struct SoaQuadBenchFixture {
    instances: Vec<GlyphQuadStagingInstance>,
    positions: Vec<[f32; 4]>,
    tex_rects: Vec<[f32; 4]>,
    fg_colors: Vec<[f32; 4]>,
    alt_colors: Vec<[f32; 4]>,
    hsv: Vec<[f32; 3]>,
    has_color: Vec<f32>,
    mix_values: Vec<f32>,
}

impl SoaQuadBenchFixture {
    fn glyph_dense_frame(cols: usize, rows: usize) -> Self {
        Self::new(cols.saturating_mul(rows).max(1), cols.max(1))
    }

    fn new(len: usize, cols: usize) -> Self {
        let mut instances = Vec::with_capacity(len);
        let mut positions = Vec::with_capacity(len);
        let mut tex_rects = Vec::with_capacity(len);
        let mut fg_colors = Vec::with_capacity(len);
        let mut alt_colors = Vec::with_capacity(len);
        let mut hsv = Vec::with_capacity(len);
        let mut has_color = Vec::with_capacity(len);
        let mut mix_values = Vec::with_capacity(len);

        for idx in 0..len {
            let col = (idx % cols) as f32;
            let row = (idx / cols) as f32;
            let left = -384.0 + col * 8.0;
            let top = -220.0 + row * 16.0;
            let tex_left = ((idx % 32) as f32) / 64.0;
            let tex_top = ((idx / 32) as f32) / 64.0;
            let instance = GlyphQuadStagingInstance::new(
                [left, top, left + 8.0, top + 16.0],
                [tex_left, tex_left + 0.015625, tex_top, tex_top + 0.03125],
                [
                    0.20 + (idx % 5) as f32 * 0.03,
                    0.40 + (idx % 7) as f32 * 0.02,
                    0.72,
                    1.0,
                ],
                [0.88, 0.18 + (idx % 3) as f32 * 0.08, 0.12, 0.70],
                [1.0, 1.0 - (idx % 4) as f32 * 0.05, 0.90],
                idx % 11 == 0,
                (idx % 8) as f32 / 8.0,
            );
            positions.push(instance.position);
            tex_rects.push(instance.tex);
            fg_colors.push(instance.fg_color);
            alt_colors.push(instance.alt_color);
            hsv.push(instance.hsv);
            has_color.push(instance.has_color);
            mix_values.push(instance.mix_value);
            instances.push(instance);
        }

        Self {
            instances,
            positions,
            tex_rects,
            fg_colors,
            alt_colors,
            hsv,
            has_color,
            mix_values,
        }
    }

    fn buffers(&self) -> GlyphQuadSoaBuffers<'_> {
        GlyphQuadSoaBuffers {
            positions: &self.positions,
            tex_rects: &self.tex_rects,
            fg_colors: &self.fg_colors,
            alt_colors: &self.alt_colors,
            hsv: &self.hsv,
            has_color: &self.has_color,
            mix_values: &self.mix_values,
        }
    }

    fn expand_aos_baseline_bytes(&self) -> usize {
        let mut vertices = Vec::with_capacity(self.instances.len() * VERTICES_PER_GLYPH_QUAD);
        for instance in &self.instances {
            vertices.extend_from_slice(&aos_glyph_quad_vertices(*instance));
        }
        let bytes = vertices.len() * size_of::<GlyphQuadStagingVertex>();
        black_box(vertices);
        bytes
    }

    fn soa_instance_upload_bytes(&self) -> usize {
        let buffers = self.buffers();
        buffers.assert_consistent_lengths();

        let mut checksum = 0.0f32;
        for rect in buffers.positions {
            checksum += rect.iter().copied().sum::<f32>();
        }
        for rect in buffers.tex_rects {
            checksum += rect.iter().copied().sum::<f32>();
        }
        for color in buffers.fg_colors {
            checksum += color.iter().copied().sum::<f32>();
        }
        for color in buffers.alt_colors {
            checksum += color.iter().copied().sum::<f32>();
        }
        for hsv in buffers.hsv {
            checksum += hsv.iter().copied().sum::<f32>();
        }
        for value in buffers.has_color {
            checksum += *value;
        }
        for value in buffers.mix_values {
            checksum += *value;
        }

        black_box(checksum);
        self.positions.len()
            * (size_of::<[f32; 4]>() * 4 + size_of::<[f32; 3]>() + size_of::<f32>() * 2)
    }
}

fn glyph_run_probe_fixture(len: u32) -> Vec<GlyphRunProbeGlyph> {
    (0..len)
        .map(|idx| GlyphRunProbeGlyph {
            glyph_pos: 400 + idx,
            cluster: idx,
            font_idx: (idx % 3) as usize,
            x_advance_bits: (8.0f64 + f64::from(idx % 7) / 16.0).to_bits(),
            x_offset_bits: (f64::from(idx % 5) / 32.0).to_bits(),
            glyph_ptr: 0x1000 + idx as usize * 64,
            bitmap_pixel_width: 8 + idx % 11,
            bearing_x_bits: (1.0f64 + f64::from(idx % 9) / 64.0).to_bits(),
        })
        .collect()
}

fn glyph_run_probe_disabled_iteration(glyphs: &[GlyphRunProbeGlyph], repeats: usize) -> usize {
    let mut retained = 0usize;
    for _ in 0..repeats {
        let run = glyphs.to_vec();
        retained = retained
            .wrapping_add(run.len())
            .wrapping_add(run.capacity());
        black_box(run);
    }
    retained
}

fn bench_config() -> Criterion {
    emit_evidence_row();
    Criterion::default().configure_from_args()
}

fn emit_evidence_row() {
    let platform = std::env::consts::OS.to_string();
    let input = known_key_headless_input();
    let evidence = match render_headless(&input) {
        Ok(frame) => {
            let marker_started = Instant::now();
            let _ = trace_from_headless_frame(0, "a", platform.clone(), &frame, 0);
            let marker_overhead_us = u64::try_from(marker_started.elapsed().as_micros())
                .unwrap_or(u64::MAX)
                .max(1);
            let trace =
                trace_from_headless_frame(0, "a", platform.clone(), &frame, marker_overhead_us);
            summarize_input_to_photon_traces(platform.clone(), &[trace])
        }
        Err(error) if error.is_gpu_init_failed() => unavailable_evidence(
            platform.clone(),
            InputToPhotonState::PhotonDetectionUnavailable,
            error.to_string(),
        ),
        Err(error) => unavailable_evidence(
            platform.clone(),
            InputToPhotonState::InstrumentationUnavailable,
            error.to_string(),
        ),
    };

    let row = json!({
        "schema_version": "ft.perf.evidence-sample.v1",
        "ts_ms": now_ms(),
        "claim_id": INPUT_TO_PHOTON_CLAIM_ID,
        "metric_value": evidence.p95_us.map(|value| value as f64 / 1_000.0).unwrap_or(0.0),
        "metric_unit": "ms",
        "sample_size": evidence.sample_count.max(1),
        "commit_sha": option_env!("VERGEN_GIT_SHA"),
        "hardware_fingerprint": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "runner_sku": std::env::var("RUNNER_OS").unwrap_or_else(|_| std::env::consts::OS.to_string()),
        "workload_class": "known-key-headless-render",
        "tags": {
            "frankenterm_version": env!("CARGO_PKG_VERSION"),
            "renderer_slo_state": state_tag(evidence.state),
            "within_target": evidence.within_target.map(|value| value.to_string()).unwrap_or_else(|| "unknown".to_string())
        }
    });

    let evidence_path = evidence_path(&platform);
    if let Some(parent) = evidence_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&evidence_path)
    {
        let _ = writeln!(file, "{row}");
    }
    println!(
        "[BENCH] input_to_photon_evidence={}",
        evidence_path.display()
    );
}

fn evidence_path(platform: &str) -> PathBuf {
    let suffix = match platform {
        "macos" => "macos",
        "linux" => "wayland",
        other => other,
    };
    PathBuf::from(format!(
        "target/criterion/slo-input_to_photon_{suffix}.jsonl"
    ))
}

fn state_tag(state: InputToPhotonState) -> &'static str {
    match state {
        InputToPhotonState::Measured => "measured",
        InputToPhotonState::InstrumentationUnavailable => "instrumentation_unavailable",
        InputToPhotonState::PhotonDetectionUnavailable => "photon_detection_unavailable",
        InputToPhotonState::InstrumentationOverheadExceeded => "instrumentation_overhead_exceeded",
        InputToPhotonState::InvalidTrace => "invalid_trace",
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets =
        bench_known_key_input_to_headless_frame,
        bench_ft_3r0yk_soa_quad_staging_toggle,
        bench_ft_egok5_glyph_run_interning_toggle,
);
criterion_main!(benches);
