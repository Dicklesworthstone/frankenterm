//! Headless ingest throughput bench for `frankenterm-term` (ft-yccm0.1.2).
//!
//! Shared by `benches/ingest_throughput.rs` (`cargo bench`), the
//! hyperfine-friendly `examples/ingest_throughput.rs`, and
//! `tests/ingest_throughput_smoke.rs`. It replaces the out-of-tree planning
//! prototype at `evidence/mac-render-perf/2026-10-05/ftbench/`.
//!
//! # Build profile
//!
//! Measure optimized builds only. The workspace `[profile.release]` is
//! size-optimized (`opt-level = "z"`), so never build this with `--release`:
//!
//! - `cargo build -p frankenterm-term --profile release-perf --example ingest_throughput`
//!   puts the binary at `$CARGO_TARGET_DIR/release-perf/examples/ingest_throughput`;
//! - `cargo bench -p frankenterm-term --bench ingest_throughput` uses the
//!   workspace `[profile.bench]`, which mirrors `release-perf`.
//!
//! An unoptimized build refuses to run unless given `--allow-debug`.
//!
//! # The operator's primary test
//!
//! The first corpus, `color_emoji_random`, replicates the operator's
//! `color-emoji-random.bin` (749,801,000 bytes), which this script generated:
//!
//! ```text
//! python3 -c '
//! import random, sys
//! emoji_ranges = [(0x1F600, 0x1F64F), (0x1F300, 0x1F5FF), (0x1F680, 0x1F6FF), (0x1F900, 0x1F9FF),
//! (0x1FA70, 0x1FAFF)]
//! pool = [chr(cp) for start, end in emoji_ranges for cp in range(start, end + 1)]
//! pool += list("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789!@#%^&*()")
//! sys.stdout.reconfigure(encoding="utf8")
//! for _ in range(30000000):
//!     fg = random.randint(0, 255); bg = random.randint(0, 255); char = random.choice(pool)
//!     sys.stdout.write(f"\033[38;5;{fg}m\033[48;5;{bg}m{char}")
//! ' > color-emoji-random.bin      # 749,801,000 bytes on the operator machine
//! time \cat color-emoji-random.bin
//! ```
//!
//! The generator emits the same frame format from the same pools in the same
//! draw order (fg, bg, character), but draws from a seeded xorshift64* rather
//! than Python's unseeded Mersenne Twister. Its bytes therefore differ while
//! every per-frame distribution matches, and 30M frames come to about
//! 749.8 MB, like the operator's file. The script's ASCII pool has 71
//! characters, not 72 (there is no `$`), so the pool holds 1,447 entries. To
//! measure the operator's actual file, pass `--from-file color-emoji-random.bin`.
//!
//! # Feeding another engine identical bytes
//!
//! `--gen-only` writes each corpus to the cache and prints its path and
//! SHA-256, so ghostty-bench and this bench can consume the same file.
//!
//! # Not measured
//!
//! Allocation counts. A counting global allocator needs an unsafe
//! `GlobalAlloc` impl, which this workspace does not admit without an audit,
//! so every record reports `"allocations": null`.

pub mod cache;
pub mod cli;
pub mod corpus;
pub mod lanes;
