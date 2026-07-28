use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use std::hint::black_box;
#[cfg(feature = "succinct_attrs")]
use termwiz::cell::AttributeRuns;
use termwiz::cell::{Cell, CellAttributes};

const ATTR_COLUMNS: usize = 240;

pub fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("Cell::blank", |b| b.iter(|| black_box(Cell::blank())));
    c.bench_function("Cell::new", |b| {
        b.iter(|| Cell::new(black_box('a'), CellAttributes::default()))
    });
    c.bench_function("Cell::new_grapheme", |b| {
        b.iter(|| Cell::new_grapheme(black_box("a"), CellAttributes::default(), None))
    });
    c.bench_function("Cell::new_grapheme_with_width", |b| {
        b.iter(|| Cell::new_grapheme_with_width(black_box("a"), 1, CellAttributes::default()))
    });

    c.bench_function("CellAttributes::blank", |b| {
        b.iter(|| black_box(CellAttributes::blank()))
    });

    let attrs = CellAttributes::default();
    let mut group = c.benchmark_group("attribute_storage");
    group.throughput(Throughput::Elements(ATTR_COLUMNS as u64));

    group.bench_function("per_cell_uniform_build", |b| {
        b.iter(|| {
            let per_cell = (0..ATTR_COLUMNS)
                .map(|_| black_box(attrs.clone()))
                .collect::<Vec<_>>();
            black_box(per_cell);
        });
    });

    group.bench_function("per_cell_uniform_lookup", |b| {
        let per_cell = vec![attrs.clone(); ATTR_COLUMNS];
        b.iter(|| {
            let mut seen = 0usize;
            for idx in 0..ATTR_COLUMNS {
                seen += usize::from(black_box(per_cell.get(idx)).is_some());
            }
            black_box(seen);
        });
    });

    #[cfg(feature = "succinct_attrs")]
    {
        group.bench_function("succinct_runs_uniform_build", |b| {
            b.iter(|| {
                let mut runs = AttributeRuns::new();
                runs.push(black_box(&attrs), ATTR_COLUMNS as u32);
                black_box(runs);
            });
        });

        group.bench_function("succinct_runs_uniform_lookup", |b| {
            let mut runs = AttributeRuns::new();
            runs.push(&attrs, ATTR_COLUMNS as u32);
            b.iter(|| {
                let mut seen = 0usize;
                for idx in 0..ATTR_COLUMNS {
                    seen += usize::from(black_box(runs.get(idx)).is_some());
                }
                black_box(seen);
            });
        });

        group.bench_function("succinct_runs_roundtrip_expand", |b| {
            let mut runs = AttributeRuns::new();
            runs.push(&attrs, ATTR_COLUMNS as u32);
            b.iter(|| black_box(runs.to_per_cell()));
        });
    }

    group.finish();
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
