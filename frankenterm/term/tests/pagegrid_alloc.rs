//! Zero allocations per scroll in steady state (ft-yccm0.3.3.3, ADR D8).
//!
//! A `PageList` at its hot cap scrolls by taking the next row of the tail
//! page and recycling the oldest page through its pool, so once warmed up a
//! scroll allocates nothing. jemalloc's per-thread cumulative counter
//! (`thread.allocatedp`) measures this thread exactly: any allocation, however
//! small, raises it.
//!
//! Debug builds included: `Page::reset` and the per-row checks are
//! allocation-free on their success paths.
#![cfg(not(windows))]

use frankenterm_term::pagegrid::{CellWrite, Glyph, InlineStyle, PageList, RowRef, StyleSpec};
use tikv_jemalloc_ctl::thread;

#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

const COLS: u16 = 80;
const VIEWPORT: usize = 24;
/// Viewport plus 1000 rows of hot scrollback: about three standard pages.
const HOT_CAP: usize = VIEWPORT + 1000;
const STEADY_SCROLLS: usize = 10_000;
const EMOJI: [char; 4] = ['\u{1f600}', '\u{1f680}', '\u{1f916}', '\u{1f389}'];

/// This thread's cumulative allocated bytes. The pointer is looked up once;
/// each read is a plain load, so measuring does not itself call jemalloc.
#[derive(Clone, Copy)]
struct Counter(thread::ThreadLocal<u64>);

impl Counter {
    fn new() -> Self {
        Counter(
            thread::allocatedp::mib()
                .and_then(|mib| mib.read())
                .expect("jemalloc thread.allocatedp"),
        )
    }

    fn bytes(self) -> u64 {
        self.0.get()
    }
}

fn admit_all(_: RowRef<'_>) -> bool {
    true
}

#[derive(Clone, Copy)]
enum Rows {
    /// ASCII text in the default style, as `seq` prints.
    Seq,
    /// A wide emoji per cell pair with a fresh palette fg and bg, as T0 prints.
    T0,
}

fn fill(list: &mut PageList, rows: Rows, n: usize) {
    let stable = list.end_stable() - 1;
    let (page, row) = list.row_mut(stable).expect("the new row");
    match rows {
        Rows::Seq => {
            for x in 0..usize::from(COLS) {
                let glyph = Glyph::Char(char::from(b'0' + ((n + x) % 10) as u8));
                let write = CellWrite::new(glyph, StyleSpec::Inline(InlineStyle::DEFAULT));
                assert!(page.write(row, x, write, n));
            }
        }
        Rows::T0 => {
            for x in (0..usize::from(COLS)).step_by(2) {
                let fg = ((n * 7 + x) % 256) as u16 + 1;
                let bg = ((n * 13 + x * 3) % 256) as u16 + 1;
                let glyph = Glyph::Char(EMOJI[(n + x) % EMOJI.len()]);
                let mut write =
                    CellWrite::new(glyph, StyleSpec::Inline(InlineStyle::new(0, fg, bg)));
                write.wide = true;
                assert!(page.write(row, x, write, n));
            }
        }
    }
}

/// Scrolls one filled row at a time. Returns the bytes allocated inside the
/// `scroll` calls, and the bytes allocated across whole scroll-and-fill
/// cycles.
fn scroll_rows(list: &mut PageList, rows: Rows, count: usize, seqno: &mut usize) -> (u64, u64) {
    let counter = Counter::new();
    let mut in_scroll = 0;
    let cycles_start = counter.bytes();
    for _ in 0..count {
        *seqno += 1;
        let before = counter.bytes();
        let scrolled = list.scroll(1, *seqno, false, admit_all);
        in_scroll += counter.bytes() - before;
        assert!(!scrolled.refused);
        fill(list, rows, *seqno);
    }
    (in_scroll, counter.bytes() - cycles_start)
}

fn steady_state(rows: Rows) {
    let mut list = PageList::new(COLS, VIEWPORT, HOT_CAP, 1);
    let mut seqno = 0;
    // Warm-up: fill to the cap, then turn every page over a few times so the
    // pool, the page deque and the lazily sized side tables reach their
    // steady sizes.
    scroll_rows(&mut list, rows, 4 * HOT_CAP, &mut seqno);
    assert_eq!(list.retained_rows(), HOT_CAP);
    assert!(
        list.pooled_page_count() > 0,
        "pages recycle through the pool"
    );

    let (in_scroll, in_cycles) = scroll_rows(&mut list, rows, STEADY_SCROLLS, &mut seqno);
    assert_eq!(
        in_scroll, 0,
        "{} steady-state scrolls allocated {} bytes",
        STEADY_SCROLLS, in_scroll
    );
    assert_eq!(
        in_cycles, 0,
        "{} steady-state scroll-and-fill cycles allocated {} bytes",
        STEADY_SCROLLS, in_cycles
    );
    assert_eq!(list.retained_rows(), HOT_CAP);
    list.check_invariants().expect("list invariants");
}

#[test]
fn the_counter_sees_a_single_small_allocation() {
    // The counter is live: without this, a zero delta proves nothing.
    let counter = Counter::new();
    let before = counter.bytes();
    let boxed = std::hint::black_box(Box::new(7_u8));
    assert!(counter.bytes() > before);
    drop(boxed);
}

#[test]
fn seq_like_scrolling_at_the_cap_allocates_nothing() {
    steady_state(Rows::Seq);
}

#[test]
fn t0_like_scrolling_at_the_cap_allocates_nothing() {
    steady_state(Rows::T0);
}
