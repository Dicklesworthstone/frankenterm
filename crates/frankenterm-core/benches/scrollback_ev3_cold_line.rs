//! Round-6 A4b bench: EV3 single-line cold scrollback retrieval.
//!
//! The benchmark constructs through `TieredScrollback::new` so
//! `FT_MOONSHOT_SCROLLBACK_BLOCKED_PAGE_INDEX` is the runtime A/B gate. Default
//! OFF retrieves a whole cold page for one line; gate ON retains the rank/select
//! block sidecar and calls `retrieve_line_block` for only the target block.

use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};

use criterion::{Criterion, criterion_group, criterion_main};
use frankenterm_core::scrollback_tiers::{
    ColdLineBlockData, ColdPageData, ColdRetrievalError, ColdTierRetriever, ScrollbackConfig,
    ScrollbackLocationHint, TieredScrollback,
};

mod bench_common;

const LINE_COUNT: usize = 4_096;
const HOT_LINES: usize = 32;
const PAGE_SIZE: usize = 64;
const OFFSET_PROBES: usize = 1_024;

const BUDGETS: &[bench_common::BenchBudget] = &[bench_common::BenchBudget {
    name: "scrollback_ev3_cold_line/single_line_from_cold",
    budget: "A/B env gate FT_MOONSHOT_SCROLLBACK_BLOCKED_PAGE_INDEX should make cold_line fetch/decode only the selected line block instead of cloning a full cold page",
}];

#[derive(Debug)]
struct BenchColdRetriever {
    pages: Vec<Vec<String>>,
    full_page_calls: AtomicUsize,
    block_calls: AtomicUsize,
}

impl BenchColdRetriever {
    fn new(pages: Vec<Vec<String>>) -> Self {
        Self {
            pages,
            full_page_calls: AtomicUsize::new(0),
            block_calls: AtomicUsize::new(0),
        }
    }

    fn call_counts(&self) -> (usize, usize) {
        (
            self.full_page_calls.load(Ordering::Relaxed),
            self.block_calls.load(Ordering::Relaxed),
        )
    }
}

impl ColdTierRetriever for BenchColdRetriever {
    fn retrieve_page(&self, page_index: u64) -> Result<ColdPageData, ColdRetrievalError> {
        self.full_page_calls.fetch_add(1, Ordering::Relaxed);
        let page_index_usize = usize::try_from(page_index)
            .map_err(|_| ColdRetrievalError::PageNotFound { page_index })?;
        self.pages
            .get(page_index_usize)
            .cloned()
            .map(|lines| ColdPageData { page_index, lines })
            .ok_or(ColdRetrievalError::PageNotFound { page_index })
    }

    fn retrieve_line_block(
        &self,
        page_index: u64,
        block_index: usize,
        first_line: usize,
        line_count: usize,
    ) -> Result<ColdLineBlockData, ColdRetrievalError> {
        self.block_calls.fetch_add(1, Ordering::Relaxed);
        let page_index_usize = usize::try_from(page_index)
            .map_err(|_| ColdRetrievalError::PageNotFound { page_index })?;
        let page = self
            .pages
            .get(page_index_usize)
            .ok_or(ColdRetrievalError::PageNotFound { page_index })?;
        let end = first_line
            .checked_add(line_count)
            .ok_or(ColdRetrievalError::PageNotFound { page_index })?;
        let lines = page
            .get(first_line..end)
            .ok_or(ColdRetrievalError::PageNotFound { page_index })?
            .to_vec();
        Ok(ColdLineBlockData {
            page_index,
            block_index,
            first_line,
            lines,
        })
    }
}

fn scrollback_config() -> ScrollbackConfig {
    ScrollbackConfig {
        hot_lines: HOT_LINES,
        page_size: PAGE_SIZE,
        warm_max_bytes: 1,
        compression: Default::default(),
        cold_eviction_enabled: true,
    }
}

fn corpus_line(i: usize) -> String {
    match i % 7 {
        0 => format!("ascii cold scrollback line {i:05}"),
        1 => format!("utf8 café 漢字 line {i:05}"),
        2 => format!("embedded\nnewline payload {i:05}"),
        3 => "x".repeat(384 + i % 23),
        4 => format!("prompt redraw slot {} :: {}", i % 9, "stable ".repeat(48)),
        5 => String::new(),
        _ => format!("json-ish {{\"line\":{i},\"ok\":true}}"),
    }
}

fn page_fixtures(lines: &[String], cold_pages: u64) -> Vec<Vec<String>> {
    let Ok(cold_pages) = usize::try_from(cold_pages) else {
        return Vec::new();
    };
    (0..cold_pages)
        .map(|page| {
            let start = page * PAGE_SIZE;
            let end = start.saturating_add(PAGE_SIZE).min(lines.len());
            lines.get(start..end).unwrap_or_default().to_vec()
        })
        .collect()
}

fn cold_hints(scrollback: &TieredScrollback) -> Vec<ScrollbackLocationHint> {
    let total = scrollback.total_line_count() as usize;
    let mut hints = Vec::with_capacity(OFFSET_PROBES);
    for probe in 0..OFFSET_PROBES {
        let offset = probe.wrapping_mul(37).wrapping_add(11) % total;
        if let Some(hint @ ScrollbackLocationHint::Cold { .. }) = scrollback.locate_offset(offset) {
            hints.push(hint);
        }
    }
    assert!(!hints.is_empty(), "fixture must produce cold line hints");
    hints
}

fn build_fixture() -> (
    TieredScrollback,
    BenchColdRetriever,
    Vec<ScrollbackLocationHint>,
) {
    let lines: Vec<String> = (0..LINE_COUNT).map(corpus_line).collect();
    let mut scrollback = TieredScrollback::new(scrollback_config());
    scrollback.push_lines(lines.iter().cloned());
    assert!(scrollback.cold_page_count() > 0);
    let retriever = BenchColdRetriever::new(page_fixtures(&lines, scrollback.cold_page_count()));
    let hints = cold_hints(&scrollback);
    (scrollback, retriever, hints)
}

fn bench_single_line_from_cold(c: &mut Criterion) {
    let (scrollback, retriever, hints) = build_fixture();
    let mut group = c.benchmark_group("scrollback_ev3_cold_line");

    group.bench_function("single_line_from_cold", |b| {
        b.iter(|| {
            let mut bytes = 0usize;
            for hint in &hints {
                let line = scrollback
                    .cold_line(black_box(hint), &retriever)
                    .expect("fixture cold line should resolve");
                bytes = bytes.saturating_add(line.len());
            }
            black_box((
                bytes,
                retriever.call_counts(),
                scrollback.blocked_page_index_active(),
            ));
        });
    });

    group.finish();
}

fn bench_suite(c: &mut Criterion) {
    bench_single_line_from_cold(c);
    bench_common::emit_bench_artifacts("scrollback_ev3_cold_line", BUDGETS);
}

criterion_group!(benches, bench_suite);
criterion_main!(benches);
