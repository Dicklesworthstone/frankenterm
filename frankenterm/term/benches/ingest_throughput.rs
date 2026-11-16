//! Headless ingest throughput bench (ft-yccm0.1.2): lanes `parse`, `term`,
//! `mux_two_stage` and `prod_config` over deterministic corpora, the first of
//! which replicates the operator's `color-emoji-random.bin` test.
//!
//! Build profile: `cargo bench` uses the workspace `[profile.bench]`, which
//! mirrors `release-perf` (opt-level 3, thin LTO). Never measure a `--release`
//! build: `[profile.release]` is size-optimized (`opt-level = "z"`).
//!
//! `cargo bench -p frankenterm-term --bench ingest_throughput` runs every lane
//! over 64 MiB of `color_emoji_random`; pass options after `--` (see `--help`).
//! For hyperfine, use the `ingest_throughput` example, which has a stable path.
//! Under `cargo test --benches` (no `--bench` flag) it runs a quick 64 KiB check.
//! See `benches/ingest/mod.rs` for the corpus and lane documentation.

#[path = "ingest/mod.rs"]
#[allow(dead_code)]
mod ingest;

fn main() {
    let args = std::env::args().skip(1).collect();
    std::process::exit(ingest::cli::main_with_args(
        args,
        ingest::cli::Entry::BenchTarget,
    ));
}
