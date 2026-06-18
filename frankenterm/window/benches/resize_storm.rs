//! Resize-storm bench (RQ-S1) for ft-c9arc / ft-mpc9b.7.
//!
//! Pins the cost of the atlas under a worst-case "user dragging the
//! window edge at 60Hz while typing" workload: each frame allocates
//! a few new sprites (newly-rasterized cells at a different scale),
//! some old sprites become unreachable, and the version cursor must
//! correctly identify which sprites were touched in the current
//! frame.
//!
//! The full RQ-S1 SLO ("60 FPS sustained on 200-pane fleet")
//! requires real-GPU integration — out of reach for this CPU-only
//! bench. What this bench does prove is that the atlas data
//! structure can sustain a 60Hz allocate-and-version-bump cadence
//! with comfortable margin: each `allocate` call is O(1) amortized
//! and the version increment is a single `fetch_add`.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Terminal, TerminalConfiguration, TerminalSize};
use std::hint::black_box;
use std::io::Write;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use window::bitmaps::atlas::Atlas;
use window::bitmaps::ImageTexture;
use window::bitmaps::Texture2d;
use window::BitmapImage;
use window::Image;

#[derive(Debug)]
struct ReflowBenchConfig;

impl TerminalConfiguration for ReflowBenchConfig {
    fn scrollback_size(&self) -> usize {
        8192
    }

    fn color_palette(&self) -> ColorPalette {
        ColorPalette::default()
    }
}

fn fresh_atlas(side: usize) -> Atlas {
    let texture: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(side, side));
    Atlas::new(&texture).expect("atlas construction")
}

fn cell(width: usize, height: usize, byte: u8) -> Image {
    let mut image = Image::new(width, height);
    let pixel = (u32::from(byte) << 24) | (u32::from(byte) << 16) | (u32::from(byte) << 8) | 0xff;
    for y in 0..height {
        for x in 0..width {
            *image.pixel_mut(x, y) = pixel;
        }
    }
    image
}

fn term_size(rows: usize, cols: usize) -> TerminalSize {
    TerminalSize {
        rows,
        cols,
        pixel_width: cols * 8,
        pixel_height: rows * 16,
        dpi: 96,
    }
}

fn reflow_payload() -> &'static [u8] {
    static PAYLOAD: OnceLock<Vec<u8>> = OnceLock::new();
    PAYLOAD
        .get_or_init(|| {
            let mut payload = Vec::with_capacity(512 * 256);
            for idx in 0..512 {
                writeln!(
                    payload,
                    "pane-{idx:04} {} {} {}",
                    "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
                    "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
                    "diagnostics=warning,error,rate-limit,context-window cwd=/tmp/frankenterm/term/src/screen.rs"
                )
                .expect("write reflow payload");
            }
            payload
        })
        .as_slice()
}

fn fresh_reflow_terminal() -> Terminal {
    let mut term = Terminal::new(
        term_size(48, 120),
        Arc::new(ReflowBenchConfig),
        "frankenterm-resize-storm",
        env!("CARGO_PKG_VERSION"),
        Box::new(Vec::new()),
    );
    term.advance_bytes(reflow_payload());
    term
}

fn allocate_one_per_frame(c: &mut Criterion) {
    let mut group = c.benchmark_group("resize_storm_rq_s1");
    group.throughput(Throughput::Elements(1));
    group.bench_function("allocate_one", |b| {
        let mut atlas = fresh_atlas(2048);
        let mut byte = 0u8;
        b.iter(|| {
            // One new glyph per frame — the typical typing cadence
            // during a resize gesture.
            byte = byte.wrapping_add(1);
            let sprite = atlas
                .allocate(&cell(8, 16, byte))
                .expect("allocate (atlas large enough)");
            black_box(sprite.version());
        })
    });
    group.finish();
}

fn allocate_burst_per_frame(c: &mut Criterion) {
    // Burst case: a paste / scroll across many panes triggers
    // dozens of simultaneous allocates in a single frame.
    let mut group = c.benchmark_group("resize_storm_rq_s1");
    group.throughput(Throughput::Elements(32));
    group.bench_function("allocate_burst_32", |b| {
        let mut atlas = fresh_atlas(4096);
        let mut byte = 0u8;
        b.iter(|| {
            for _ in 0..32 {
                byte = byte.wrapping_add(1);
                let sprite = atlas
                    .allocate(&cell(8, 16, byte))
                    .expect("allocate (atlas large enough)");
                black_box(sprite.version());
            }
        })
    });
    group.finish();
}

fn version_bump_atomic_cost(c: &mut Criterion) {
    // The minimum-bound "atlas allocate cost" — even if guillotiere
    // returned an answer instantly, the atomic version bump on
    // success is unavoidable. Pinning its cost separately helps a
    // future regression localize "is the slowness in the allocator
    // or in the version path?".
    let atlas = fresh_atlas(64);
    let mut group = c.benchmark_group("resize_storm_rq_s1");
    group.throughput(Throughput::Elements(1));
    group.bench_function("version_load", |b| {
        b.iter(|| {
            black_box(atlas.version());
        })
    });
    group.finish();
}

fn term_reflow_wrap_cache(c: &mut Criterion) {
    let mut group = c.benchmark_group("resize_storm_rq_s1");
    group.throughput(Throughput::Elements(6));
    group.bench_function("term_reflow_wrap_cache", |b| {
        b.iter_batched(
            fresh_reflow_terminal,
            |mut term| {
                for size in [
                    term_size(48, 80),
                    term_size(48, 132),
                    term_size(48, 96),
                    term_size(48, 80),
                    term_size(48, 132),
                    term_size(48, 120),
                ] {
                    term.resize(size);
                    black_box(term.screen().last_viewport_first_reflow_us());
                }
                black_box(term.screen().scrollback_rows());
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(
    resize_storm,
    allocate_one_per_frame,
    allocate_burst_per_frame,
    version_bump_atomic_cost,
    term_reflow_wrap_cache,
);
criterion_main!(resize_storm);
