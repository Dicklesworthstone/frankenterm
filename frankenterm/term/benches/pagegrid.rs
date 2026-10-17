//! PageGrid page micro-benches (ft-yccm0.3.3.2): print-run writes, palette
//! and rich style churn, rich repeats through the SGR cache hook, the T0
//! pattern (a random 256-colour fg x bg per wide glyph) and page reset.
//!
//! The palette and T0 benches also assert the ADR D2 claim they measure:
//! the rich-style table is never hashed and never grows.

use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};
use frankenterm_term::color::{ColorAttribute, SrgbaTuple};
use frankenterm_term::pagegrid::{CellWrite, Glyph, InlineStyle, Page, RichStyle, StyleSpec};
use std::hint::black_box;

const COLS: u16 = 120;
const EMOJI: [char; 8] = ['😀', '😂', '🥰', '😎', '🤖', '👾', '🎉', '🚀'];

fn xorshift(mut state: u64) -> u64 {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    state
}

fn full_page() -> Page {
    let mut page = Page::standard(COLS, 1);
    while page.grow(1).is_some() {}
    page
}

fn assert_table_untouched(page: &Page) {
    assert_eq!(page.styles().hash_count(), 0);
    assert_eq!(page.styles().live(), 0);
    assert_eq!(page.styles().index_slots(), 512);
}

/// Writes one row of the T0 pattern; returns the generator state.
fn t0_row(page: &mut Page, row: u32, mut rng: u64, seqno: usize) -> u64 {
    for x in (0..usize::from(COLS)).step_by(2) {
        rng = xorshift(rng);
        let fg = (rng & 0xFF) as u16 + 1;
        let bg = ((rng >> 8) & 0xFF) as u16 + 1;
        let glyph = Glyph::Char(EMOJI[(rng >> 16) as usize % EMOJI.len()]);
        let mut write = CellWrite::new(glyph, StyleSpec::Inline(InlineStyle::new(0, fg, bg)));
        write.wide = true;
        page.write(row, x, write, seqno);
    }
    rng
}

fn writes(c: &mut Criterion) {
    let mut group = c.benchmark_group("pagegrid_write");
    group.throughput(Throughput::Elements(u64::from(COLS)));
    let mut page = full_page();
    let rows = page.capacity();
    let mut row = 0;
    let mut seqno = 2;

    group.bench_function("print_run_row", |b| {
        b.iter(|| {
            for x in 0..usize::from(COLS) {
                let glyph = Glyph::Char(char::from(b'a' + (x % 26) as u8));
                let write = CellWrite::new(glyph, StyleSpec::Inline(InlineStyle::DEFAULT));
                page.write(row, x, black_box(write), seqno);
            }
            row = (row + 1) % rows;
            seqno += 1;
        })
    });

    let mut shift = 0;
    group.bench_function("palette_churn_256", |b| {
        b.iter(|| {
            for x in 0..usize::from(COLS) {
                let fg = ((x + shift) % 256) as u16 + 1;
                let style = InlineStyle::new(0, fg, 1);
                let write = CellWrite::new(Glyph::Char('x'), StyleSpec::Inline(style));
                page.write(row, x, black_box(write), seqno);
            }
            shift += 1;
            row = (row + 1) % rows;
            seqno += 1;
        })
    });
    assert_table_untouched(&page);

    let styles: Vec<RichStyle> = (0..256)
        .map(|n| RichStyle {
            attrs: 0,
            fg: ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
                n as f32 / 255.0,
                0.5,
                0.25,
                1.0,
            )),
            bg: ColorAttribute::Default,
            underline_color: ColorAttribute::Default,
        })
        .collect();
    group.bench_function("rich_churn_256", |b| {
        b.iter(|| {
            for x in 0..usize::from(COLS) {
                // A new SGR every cell: the pen's cache starts empty.
                let mut cache = None;
                let spec = StyleSpec::Rich {
                    style: &styles[(x + shift) % 256],
                    cache: &mut cache,
                };
                page.write(row, x, CellWrite::new(Glyph::Char('x'), spec), seqno);
            }
            shift += 1;
            row = (row + 1) % rows;
            seqno += 1;
        })
    });

    let mut cache = None;
    group.bench_function("rich_repeat_cached", |b| {
        b.iter(|| {
            for x in 0..usize::from(COLS) {
                let spec = StyleSpec::Rich {
                    style: &styles[7],
                    cache: &mut cache,
                };
                page.write(row, x, CellWrite::new(Glyph::Char('x'), spec), seqno);
            }
            row = (row + 1) % rows;
            seqno += 1;
        })
    });
    group.finish();

    let mut group = c.benchmark_group("pagegrid_t0");
    group.throughput(Throughput::Elements(u64::from(COLS / 2)));
    let mut page = full_page();
    let mut rng = 0x9E37_79B9_7F4A_7C15_u64;
    group.bench_function("random_fg_bg_wide_row", |b| {
        b.iter(|| {
            rng = t0_row(&mut page, row, rng, seqno);
            row = (row + 1) % rows;
            seqno += 1;
        })
    });
    assert_table_untouched(&page);
    group.finish();
}

fn reset(c: &mut Criterion) {
    let mut group = c.benchmark_group("pagegrid_reset");
    let mut serial = 2;
    group.bench_function("t0_filled_page", |b| {
        b.iter_batched(
            || {
                let mut page = full_page();
                let mut rng = 1;
                for row in 0..page.capacity() {
                    rng = t0_row(&mut page, row, rng, 2);
                }
                page
            },
            |mut page| {
                serial += 1;
                page.reset(serial);
                page
            },
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

criterion_group!(benches, writes, reset);
criterion_main!(benches);
