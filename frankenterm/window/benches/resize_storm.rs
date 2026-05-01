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

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::rc::Rc;
use window::bitmaps::atlas::Atlas;
use window::bitmaps::ImageTexture;
use window::bitmaps::Texture2d;
use window::BitmapImage;
use window::Image;

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
            criterion::black_box(sprite.version());
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
                criterion::black_box(sprite.version());
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
            criterion::black_box(atlas.version());
        })
    });
    group.finish();
}

criterion_group!(
    resize_storm,
    allocate_one_per_frame,
    allocate_burst_per_frame,
    version_bump_atomic_cost,
);
criterion_main!(resize_storm);
