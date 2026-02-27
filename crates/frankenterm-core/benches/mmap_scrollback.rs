use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use frankenterm_core::storage::mmap_store::{
    build_offsets_from_lengths, page_align_down, MmapScrollbackStore, MmapStoreConfig,
};
use rusqlite::{params, Connection};
use std::hint::black_box;
use std::path::Path;

struct SqliteScrollbackBenchStore {
    conn: Connection,
}

impl SqliteScrollbackBenchStore {
    fn new(db_path: &Path) -> Self {
        let conn = Connection::open(db_path).expect("open sqlite");
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS bench_scrollback (
                 pane_id INTEGER NOT NULL,
                 seq INTEGER NOT NULL,
                 content TEXT NOT NULL,
                 PRIMARY KEY (pane_id, seq)
             );
             CREATE INDEX IF NOT EXISTS idx_bench_scrollback_pane_seq
                 ON bench_scrollback(pane_id, seq DESC);",
        )
        .expect("create sqlite schema");
        Self { conn }
    }

    fn append_line(&self, pane_id: u64, line: &str) {
        let pane_id_i64 = i64::try_from(pane_id).expect("pane_id fits in i64");
        let next_seq: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM bench_scrollback WHERE pane_id = ?1",
                [pane_id_i64],
                |row| row.get(0),
            )
            .expect("query next seq");

        self.conn
            .execute(
                "INSERT INTO bench_scrollback (pane_id, seq, content) VALUES (?1, ?2, ?3)",
                params![pane_id_i64, next_seq, line],
            )
            .expect("insert sqlite line");
    }

    fn tail_lines(&self, pane_id: u64, n: usize) -> Vec<String> {
        if n == 0 {
            return Vec::new();
        }

        let pane_id_i64 = i64::try_from(pane_id).expect("pane_id fits in i64");
        let limit_i64 = i64::try_from(n).expect("limit fits in i64");

        let mut stmt = self
            .conn
            .prepare(
                "SELECT content
                 FROM bench_scrollback
                 WHERE pane_id = ?1
                 ORDER BY seq DESC
                 LIMIT ?2",
            )
            .expect("prepare tail query");
        let mut lines: Vec<String> = stmt
            .query_map(params![pane_id_i64, limit_i64], |row| {
                row.get::<_, String>(0)
            })
            .expect("run tail query")
            .collect::<std::result::Result<Vec<_>, _>>()
            .expect("collect tail rows");
        lines.reverse();
        lines
    }
}

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

fn bench_store_append(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap_scrollback/store_append_compare");
    let pane_id = 9u64;

    for &line_count in &[1_000usize, 10_000usize] {
        let lines: Vec<String> = (0..line_count)
            .map(|i| format!("line-{i:06}-abcdefghijklmnopqrstuvwxyz0123456789"))
            .collect();
        let bytes = lines.iter().map(|line| line.len() + 1).sum::<usize>() as u64;
        group.throughput(Throughput::Bytes(bytes));
        group.bench_with_input(
            BenchmarkId::new("mmap_append_batch", line_count),
            &line_count,
            |b, _| {
                b.iter_batched(
                    || tempfile::tempdir().expect("tempdir"),
                    |dir| {
                        let config = MmapStoreConfig::new(dir.path().to_path_buf());
                        let mut store = MmapScrollbackStore::new(config).expect("store");
                        for line in &lines {
                            store.append_line(pane_id, line).expect("append");
                        }
                        black_box(store.line_count(pane_id));
                    },
                    BatchSize::SmallInput,
                );
            },
        );
        group.bench_with_input(
            BenchmarkId::new("sqlite_append_batch", line_count),
            &line_count,
            |b, _| {
                b.iter_batched(
                    || tempfile::tempdir().expect("tempdir"),
                    |dir| {
                        let db_path = dir.path().join("append.sqlite3");
                        let sqlite = SqliteScrollbackBenchStore::new(&db_path);
                        for line in &lines {
                            sqlite.append_line(pane_id, line);
                        }
                        black_box(sqlite.tail_lines(pane_id, 1).len());
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_store_tail(c: &mut Criterion) {
    let mut group = c.benchmark_group("mmap_scrollback/store_tail_compare");
    let pane_id = 11u64;

    for &total_lines in &[1_000usize, 10_000usize, 100_000usize] {
        let lines: Vec<String> = (0..total_lines)
            .map(|i| format!("payload-{i:06}-abcdefghijklmnopqrstuvwxyz0123456789"))
            .collect();

        let _mmap_dir = tempfile::tempdir().expect("tempdir");
        let config = MmapStoreConfig::new(_mmap_dir.path().to_path_buf());
        let mut store = MmapScrollbackStore::new(config).expect("store");

        for line in &lines {
            store.append_line(pane_id, line).expect("append");
        }

        let _sqlite_dir = tempfile::tempdir().expect("tempdir");
        let sqlite_path = _sqlite_dir.path().join("tail.sqlite3");
        let sqlite = SqliteScrollbackBenchStore::new(&sqlite_path);
        for line in &lines {
            sqlite.append_line(pane_id, line);
        }

        for &tail in &[10usize, 100usize, 1_000usize] {
            group.throughput(Throughput::Elements(tail as u64));
            group.bench_with_input(
                BenchmarkId::new(format!("mmap_tail_from_{total_lines}"), tail),
                &tail,
                |b, requested_tail| {
                    b.iter(|| {
                        let lines = store.tail_lines(pane_id, *requested_tail).expect("tail");
                        black_box(lines.len());
                    });
                },
            );
            group.bench_with_input(
                BenchmarkId::new(format!("sqlite_tail_from_{total_lines}"), tail),
                &tail,
                |b, requested_tail| {
                    b.iter(|| {
                        let lines = sqlite.tail_lines(pane_id, *requested_tail);
                        black_box(lines.len());
                    });
                },
            );
        }
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_offset_build,
    bench_page_align,
    bench_store_append,
    bench_store_tail
);
criterion_main!(benches);
