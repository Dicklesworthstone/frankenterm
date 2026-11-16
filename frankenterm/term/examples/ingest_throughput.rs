//! Single-run CLI of the headless ingest throughput bench (ft-yccm0.1.2), with
//! a stable path for hyperfine:
//!
//! ```text
//! cargo build -p frankenterm-term --profile release-perf --example ingest_throughput
//! hyperfine -w 1 -r 10 "$CARGO_TARGET_DIR/release-perf/examples/ingest_throughput --lane term"
//! ```
//!
//! Build profile: `release-perf` (opt-level 3, thin LTO). Never measure a
//! `--release` build: `[profile.release]` is size-optimized (`opt-level = "z"`).
//! See `benches/ingest/mod.rs` for the corpus and lane documentation.

#[path = "../benches/ingest/mod.rs"]
#[allow(dead_code)]
mod ingest;

fn main() {
    let args = std::env::args().skip(1).collect();
    std::process::exit(ingest::cli::main_with_args(
        args,
        ingest::cli::Entry::Example,
    ));
}
