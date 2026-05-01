//! Atlas-stability bench (RQ-S10) for ft-c9arc / ft-mpc9b.7.
//!
//! Pins the **steady-state** cost of the atlas's version cursor: a
//! window-resize loop that does NO sprite allocations should produce
//! zero atlas rebuilds and zero version bumps. Pre-`ft-mpc9b.1.1`,
//! every resize triggered a wholesale `recreate_texture_atlas` and a
//! corresponding `clear()`; post-fix, the atlas's version field stays
//! constant and the per-frame `last_synced_version` cursor produces
//! a no-op compare.
//!
//! The bench reports the cost of N "did anything change?" probes
//! against a quiescent atlas. The RQ-S10 SLO catalog target is "0
//! rebuilds on pure resize" — this bench can't enforce the SLO
//! directly (that's a real-GPU integration concern), but it does
//! prove the version cursor is O(1) and lock-free for the steady-
//! state hot path.

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::rc::Rc;
use window::bitmaps::atlas::Atlas;
use window::bitmaps::ImageTexture;
use window::bitmaps::Texture2d;

fn fresh_atlas(side: usize) -> Atlas {
    let texture: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(side, side));
    Atlas::new(&texture).expect("atlas construction")
}

fn version_cursor_quiescent_probe(c: &mut Criterion) {
    let atlas = fresh_atlas(1024);
    let last_synced_version = atlas.version();

    let mut group = c.benchmark_group("atlas_stability_rq_s10");
    group.throughput(Throughput::Elements(1));
    group.bench_function("quiescent_probe", |b| {
        b.iter(|| {
            // Steady-state per-frame work: snapshot, compare, no-op.
            let snap = atlas.version();
            let drifted = snap > last_synced_version;
            criterion::black_box(drifted);
        })
    });
    group.finish();
}

fn version_cursor_cost_per_resize_event(c: &mut Criterion) {
    // Models the resize storm: 60Hz × 1s of "resize, no allocate"
    // events. The whole loop must be much faster than the 16.6ms
    // per-frame budget on its own (real frame work pile on top).
    let atlas = fresh_atlas(1024);
    let last_synced_version = atlas.version();

    let mut group = c.benchmark_group("atlas_stability_rq_s10");
    group.throughput(Throughput::Elements(60));
    group.bench_function("60_resize_events", |b| {
        b.iter(|| {
            let mut drifted = false;
            for _ in 0..60 {
                let snap = atlas.version();
                drifted |= snap > last_synced_version;
            }
            criterion::black_box(drifted);
        })
    });
    group.finish();
}

criterion_group!(
    atlas_stability,
    version_cursor_quiescent_probe,
    version_cursor_cost_per_resize_event,
);
criterion_main!(atlas_stability);
