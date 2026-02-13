use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

#[path = "../src/storage/mmap_store.rs"]
mod mmap_store;

use mmap_store::{build_offsets_from_lengths, page_align_down};

fn bench_offset_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap_scrollback/offset_build");

    for &size in &[1_000usize, 10_000usize, 100_000usize] {
        let lengths = vec![80u64; size];
        group.throughput(Throughput::Elements(size as u64));
        group.bench_with_input(BenchmarkId::from_parameter(size), &lengths, |b, lens| {
            b.iter(|| build_offsets_from_lengths(lens));
        });
    }

    group.finish();
}

fn bench_page_align(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap_scrollback/page_align");
    group.throughput(Throughput::Elements(1));

    for &page_size in &[4096u64, 16384u64, 65536u64] {
        group.bench_with_input(
            BenchmarkId::new("page_align_down", page_size),
            &page_size,
            |b, page| {
                b.iter(|| {
                    let mut acc = 0u64;
                    for i in 0..10_000u64 {
                        acc ^= page_align_down(i.saturating_mul(97), *page);
                    }
                    acc
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, bench_offset_build, bench_page_align);
criterion_main!(benches);
