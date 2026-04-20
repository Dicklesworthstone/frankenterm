//! Benchmarks for `content_dedup` hot-path: SHA-256 hashing and dedup engine.
//!
//! Performance budgets:
//! - content_hash (1 KB): **< 5µs**
//! - process_segment new insert: **< 10µs**
//! - process_segment dedup hit: **< 6µs** (hash only, no copy)
//! - inline short segment: **< 1µs**

use std::collections::HashMap;
use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use frankenterm_core::content_dedup::{
    ContentBlock, ContentStore, DedupConfig, DedupEngine, DedupStats, StoreResult, content_hash,
};

mod bench_common;

const BUDGETS: &[bench_common::BenchBudget] = &[
    bench_common::BenchBudget {
        name: "content_dedup/sha256_hash",
        budget: "< 5µs per 1 KB hash",
    },
    bench_common::BenchBudget {
        name: "content_dedup/process_new",
        budget: "< 10µs per new segment insert",
    },
    bench_common::BenchBudget {
        name: "content_dedup/process_dedup_hit",
        budget: "< 6µs per duplicate segment (hash + lookup only)",
    },
    bench_common::BenchBudget {
        name: "content_dedup/process_inline",
        budget: "< 1µs per inline small segment",
    },
];

// ---------------------------------------------------------------------------
// In-memory ContentStore (mirrors the one in content_dedup::tests)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MemStore {
    blocks: HashMap<String, (Vec<u8>, ContentBlock)>,
}

impl ContentStore for MemStore {
    fn store(
        &mut self,
        hash: &str,
        content: &[u8],
        timestamp_ms: u64,
    ) -> Result<StoreResult, String> {
        if let Some((_, block)) = self.blocks.get_mut(hash) {
            block.ref_count += 1;
            block.last_seen_ms = timestamp_ms;
            Ok(StoreResult::Deduplicated)
        } else {
            self.blocks.insert(
                hash.to_string(),
                (
                    content.to_vec(),
                    ContentBlock {
                        hash: hash.to_string(),
                        byte_size: content.len(),
                        ref_count: 1,
                        first_seen_ms: timestamp_ms,
                        last_seen_ms: timestamp_ms,
                    },
                ),
            );
            Ok(StoreResult::Inserted)
        }
    }

    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>, String> {
        Ok(self.blocks.get(hash).map(|(d, _)| d.clone()))
    }

    fn decrement_ref(&mut self, hash: &str) -> Result<u64, String> {
        if let Some((_, block)) = self.blocks.get_mut(hash) {
            block.ref_count = block.ref_count.saturating_sub(1);
            Ok(block.ref_count)
        } else {
            Err(format!("not found: {hash}"))
        }
    }

    fn gc(&mut self) -> Result<usize, String> {
        let before = self.blocks.len();
        self.blocks.retain(|_, (_, b)| b.ref_count > 0);
        Ok(before - self.blocks.len())
    }

    fn stats(&self) -> Result<DedupStats, String> {
        Ok(DedupStats::default())
    }

    fn contains(&self, hash: &str) -> Result<bool, String> {
        Ok(self.blocks.contains_key(hash))
    }
}

fn make_engine() -> DedupEngine<MemStore> {
    DedupEngine::new(DedupConfig::default(), MemStore::default())
}

// ---------------------------------------------------------------------------
// Payload generators
// ---------------------------------------------------------------------------

/// Realistic AI agent terminal output line.
fn agent_output(size: usize) -> Vec<u8> {
    let line = b"[2026-04-19T10:00:00Z] INFO agent: processing step 42, tokens=1500, cost=$0.03\n";
    let mut buf = Vec::with_capacity(size);
    while buf.len() < size {
        buf.extend_from_slice(line);
    }
    buf.truncate(size);
    buf
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

/// Bench: raw SHA-256 hashing at varying payload sizes.
fn bench_sha256_hash(c: &mut Criterion) {
    let mut group = c.benchmark_group("content_dedup");

    for &size in &[64_usize, 256, 1024, 4096, 16384] {
        let data = agent_output(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(
            BenchmarkId::new("sha256_hash", size),
            &data,
            |b, data| {
                b.iter(|| black_box(content_hash(data)));
            },
        );
    }

    group.finish();
}

/// Bench: process_segment for new (unique) content at varying sizes.
fn bench_process_new(c: &mut Criterion) {
    let mut group = c.benchmark_group("content_dedup");

    for &size in &[256_usize, 1024, 4096] {
        group.throughput(Throughput::Bytes(size as u64));
        // Each iteration processes a unique segment (different content each time)
        let segments: Vec<Vec<u8>> = (0..200)
            .map(|i| {
                let mut s = agent_output(size);
                // Make each segment unique by stamping the index
                let tag = format!("__unique_{i:04}__");
                let start = s.len().saturating_sub(tag.len());
                s[start..].copy_from_slice(tag.as_bytes());
                s
            })
            .collect();

        group.bench_with_input(
            BenchmarkId::new("process_new", size),
            &segments,
            |b, segments| {
                b.iter(|| {
                    let mut engine = make_engine();
                    for seg in segments {
                        black_box(engine.process_segment(seg, 1000).unwrap());
                    }
                });
            },
        );
    }

    group.finish();
}

/// Bench: process_segment for duplicate content (dedup hit path).
fn bench_process_dedup_hit(c: &mut Criterion) {
    let mut group = c.benchmark_group("content_dedup");

    let content = agent_output(1024);
    group.throughput(Throughput::Elements(1));

    group.bench_function("process_dedup_hit", |b| {
        b.iter(|| {
            let mut engine = make_engine();
            // First call: insert
            engine.process_segment(&content, 1000).unwrap();
            // Second call: dedup hit — this is what we measure
            black_box(engine.process_segment(&content, 2000).unwrap());
        });
    });

    group.finish();
}

/// Bench: process_segment for inline (below min_dedup_size) content.
fn bench_process_inline(c: &mut Criterion) {
    let mut group = c.benchmark_group("content_dedup");

    // 16 bytes is below default min_dedup_size (32)
    let tiny = b"short output ok!";
    group.throughput(Throughput::Elements(1));

    group.bench_function("process_inline", |b| {
        let mut engine = make_engine();
        b.iter(|| {
            black_box(engine.process_segment(tiny, 1000).unwrap());
        });
    });

    group.finish();
}

fn bench_config() -> Criterion {
    bench_common::emit_bench_artifacts("content_dedup", BUDGETS);
    Criterion::default().configure_from_args()
}

criterion_group!(
    name = benches;
    config = bench_config();
    targets = bench_sha256_hash,
        bench_process_new,
        bench_process_dedup_hit,
        bench_process_inline
);
criterion_main!(benches);
