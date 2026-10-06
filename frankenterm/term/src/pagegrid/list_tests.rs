//! B3.3 tests: stable-index mapping across recycling, per-row admission and
//! the over-cap reasons, sealing with copy-on-write, footer inserts and
//! rotations against a `VecDeque` model, and the change floors.

use super::*;
use crate::color::{ColorAttribute, SrgbaTuple};
use crate::StableRowIndex;
use std::collections::VecDeque;
use std::sync::Arc;

const COLS: u16 = 8;

fn rich(id: u32) -> RichStyle {
    RichStyle {
        attrs: (id % 5) as u16,
        fg: ColorAttribute::TrueColorWithDefaultFallback(SrgbaTuple(
            id as f32 / 512.0,
            0.25,
            0.5,
            1.0,
        )),
        bg: ColorAttribute::Default,
        underline_color: ColorAttribute::Default,
    }
}

fn glyph(id: u32) -> char {
    char::from_u32(0x100 + id).expect("a narrow Latin Extended scalar")
}

/// Writes content `id` at column 0; every third id uses a rich style, so
/// rows that cross a page boundary re-intern a style-table entry.
fn put(list: &mut PageList, stable: StableRowIndex, id: u32, seqno: usize) {
    let (page, row) = list.row_mut(stable).expect("a retained row");
    let style = rich(id);
    let mut cache = None;
    let spec = if id.is_multiple_of(3) {
        StyleSpec::Rich {
            style: &style,
            cache: &mut cache,
        }
    } else {
        StyleSpec::Inline(InlineStyle::DEFAULT)
    };
    assert!(page.write(row, 0, CellWrite::new(Glyph::Char(glyph(id)), spec), seqno));
}

/// The content id at column 0, checking a rich row kept its style.
fn content(list: &PageList, stable: StableRowIndex) -> Option<u32> {
    let row = list.row(stable).expect("a retained row");
    match row.page.glyph(row.row, 0) {
        Glyph::Blank => None,
        Glyph::Char(ch) => {
            let id = ch as u32 - 0x100;
            let cell = row.page.cell(row.row, 0);
            match cell.style() {
                CellStyle::Rich(style_id) => {
                    assert!(id.is_multiple_of(3), "row {} gained a rich style", stable);
                    assert_eq!(row.page.styles().get(style_id), Some(&rich(id)));
                }
                CellStyle::Inline(_) => {
                    assert!(!id.is_multiple_of(3), "row {} lost its style", stable)
                }
            }
            Some(id)
        }
        Glyph::Cluster(text) => panic!("unexpected cluster {:?}", text),
    }
}

fn check(list: &PageList) {
    if let Err(err) = list.check_invariants() {
        panic!("{:?}: {}", list, err);
    }
}

fn admit_all(_: RowRef<'_>) -> bool {
    true
}

#[test]
fn stable_indices_map_onto_page_rows_across_recycling() {
    let mut list = PageList::with_page_rows(COLS, 4, 3, 6, 1);
    let mut admitted = Vec::new();
    for stable in 0..40 {
        let scrolled = list.scroll(1, stable as usize + 1, false, |row| {
            assert_eq!(
                row.page.glyph(row.row, 0),
                Glyph::Char(glyph(row.stable as u32))
            );
            admitted.push(row.stable);
            true
        });
        assert_eq!(scrolled.first_new, stable);
        put(&mut list, stable, stable as u32, stable as usize + 1);
        check(&list);
        assert_eq!(list.retained_rows(), (stable as usize + 1).min(6));
        let first = list.first_stable();
        assert_eq!(first, (stable - 5).max(0));
        for s in first..list.end_stable() {
            assert_eq!(content(&list, s), Some(s as u32));
            let (_, row) = list.locate(s).expect("retained");
            assert!(row < 4);
        }
        assert!(list.locate(first - 1).is_none());
        assert!(list.locate(list.end_stable()).is_none());
        // Pages in use plus pooled pages stay bounded: recycling, not churn.
        assert!(list.page_count() + list.pooled_page_count() <= 4);
    }
    // Every trimmed row was offered once, oldest first.
    assert_eq!(admitted, (0..34).collect::<Vec<StableRowIndex>>());
}

#[test]
fn refusal_hold_and_cap_changes_keep_legacy_eviction_counts() {
    let mut list = PageList::with_page_rows(COLS, 3, 2, 4, 1);
    list.scroll(4, 1, false, admit_all);
    assert_eq!((list.retained_rows(), list.over_cap()), (4, None));

    // A refused row stays resident and the list says why.
    let scrolled = list.scroll(1, 2, false, |_| false);
    assert_eq!((scrolled.trimmed, scrolled.refused), (0, true));
    assert_eq!(list.retained_rows(), 5);
    assert_eq!(list.over_cap(), Some(OverCap::SpillRefused));
    check(&list);

    // The next scroll trims the whole excess: 5 + 1 - 4.
    let scrolled = list.scroll(1, 3, false, admit_all);
    assert_eq!(scrolled.trimmed, 2);
    assert_eq!((list.retained_rows(), list.over_cap()), (4, None));

    // A recovery hold trims nothing.
    let scrolled = list.scroll(2, 4, true, |_| panic!("held rows are not offered"));
    assert_eq!(scrolled.trimmed, 0);
    assert_eq!(list.over_cap(), Some(OverCap::RecoveryHold));
    check(&list);
    list.scroll(0, 5, false, admit_all);
    assert_eq!((list.retained_rows(), list.over_cap()), (4, None));

    // Lowering the cap does not trim; the next evicting scroll does.
    list.set_hot_cap(2);
    assert_eq!(
        (list.retained_rows(), list.over_cap()),
        (4, Some(OverCap::CapLowered))
    );
    check(&list);
    let scrolled = list.scroll(1, 6, false, admit_all);
    assert_eq!(scrolled.trimmed, 3);
    assert_eq!((list.retained_rows(), list.over_cap()), (2, None));
    list.set_hot_cap(10);
    assert_eq!(list.over_cap(), None);
    check(&list);
}

#[test]
fn sealed_pages_are_shared_and_written_by_copy_on_write() {
    let mut list = PageList::with_page_rows(COLS, 2, 2, 100, 1);
    for stable in 0..10 {
        list.scroll(1, 1, false, admit_all);
        put(&mut list, stable, stable as u32, 1);
    }
    check(&list);
    // Viewport top 8, so pages ending at or before row 6 are sealed: rows
    // 0-1, 2-3 and 4-5. Rows 6-7 hold the seam row; 8-9 is the tail.
    assert_eq!(list.viewport_top(), 8);
    assert_eq!(list.sealed_page_count(), 3);
    assert!(list.row(5).unwrap().sealed.is_some());
    assert!(list.row(6).unwrap().sealed.is_none());

    // A reader holds the oldest sealed page; a write forks it.
    let (_, held) = list.sealed_pages().next().expect("a sealed page");
    let reader = Arc::clone(held);
    let serial = reader.serial();
    put(&mut list, 0, 30, 2);
    assert_eq!(reader.serial(), serial, "the reader keeps its page");
    assert_eq!(reader.glyph(0, 0), Glyph::Char(glyph(0)));
    assert_ne!(list.row(0).unwrap().page.serial(), serial);
    assert_eq!(content(&list, 0), Some(30));
    drop(reader);

    // Unshared, a sealed page is written in place.
    let serial = list.row(2).unwrap().page.serial();
    put(&mut list, 2, 31, 3);
    assert_eq!(list.row(2).unwrap().page.serial(), serial);
    check(&list);

    // A taller viewport pulls rows back; their pages unseal (I15).
    list.set_viewport_rows(6);
    assert_eq!(list.viewport_top(), 4);
    assert_eq!(list.sealed_page_count(), 1);
    check(&list);
    list.set_viewport_rows(2);
    assert_eq!(list.sealed_page_count(), 3);
    check(&list);
}

/// The legacy model: a deque of rows, content `None` for a blank row.
struct Model {
    rows: VecDeque<Option<u32>>,
    first: StableRowIndex,
}

impl Model {
    fn compare(&self, list: &PageList) {
        assert_eq!(list.first_stable(), self.first);
        assert_eq!(list.retained_rows(), self.rows.len());
        for (i, expected) in self.rows.iter().enumerate() {
            let stable = self.first + i as StableRowIndex;
            assert_eq!(content(list, stable), *expected, "row {}", stable);
        }
    }
}

#[test]
fn footer_inserts_and_rotations_match_a_deque_model_across_pages() {
    let (viewport, cap) = (5_usize, 9_usize);
    let mut list = PageList::with_page_rows(COLS, 3, viewport, cap, 1);
    let mut model = Model {
        rows: VecDeque::new(),
        first: 0,
    };
    let mut next_id = 0_u32;
    let mut seqno = 0_usize;
    let mut new_row = |list: &mut PageList, stable: StableRowIndex, seqno: usize| -> u32 {
        let id = next_id;
        next_id += 1;
        put(list, stable, id, seqno);
        id
    };
    for step in 0..60_usize {
        seqno += 1;
        match step % 5 {
            // Full-screen line feed.
            0 | 1 => {
                let scrolled = list.scroll(1, seqno, false, admit_all);
                let id = new_row(&mut list, scrolled.first_new, seqno);
                model.rows.push_back(Some(id));
            }
            // Region 0..bottom with a footer of 1 or 2 rows.
            2 => {
                let footer = 1 + step % 2;
                list.scroll_above_footer(1, footer, seqno, false, admit_all);
                let at = list.end_stable() - 1 - footer as StableRowIndex;
                let id = new_row(&mut list, at, seqno);
                let pos = model.rows.len() - footer;
                model.rows.insert(pos, Some(id));
            }
            // A region with a top margin scrolls within the viewport.
            3 => {
                let top = list.viewport_top() + 1;
                let range = top..list.end_stable() - 1;
                list.rotate_up(range.clone(), 1, seqno);
                let start = (range.start - model.first) as usize;
                let end = (range.end - model.first) as usize;
                if start < end {
                    model.rows.remove(start);
                    model.rows.insert(end - 1, None);
                }
            }
            _ => {
                let top = list.viewport_top();
                let range = top..list.end_stable();
                list.rotate_down(range.clone(), 2, seqno);
                let start = (range.start - model.first) as usize;
                let end = (range.end - model.first) as usize;
                for _ in 0..2.min(end - start) {
                    model.rows.remove(end - 1);
                    model.rows.insert(start, None);
                }
            }
        }
        // Legacy evicts past the cap before appending; the model trims after.
        while model.rows.len() > cap {
            model.rows.pop_front();
            model.first += 1;
        }
        check(&list);
        model.compare(&list);
    }
    // 36 rows went through 3-row pages: boundaries were crossed and the
    // front pages recycled.
    assert_eq!(list.end_stable(), 36);
    assert_eq!(list.first_stable(), 27);
}

#[test]
fn change_floors_raise_effective_seqnos_without_writing_sealed_pages() {
    let mut list = PageList::with_page_rows(COLS, 2, 2, 100, 1);
    for seqno in 1..=10 {
        list.scroll(1, seqno, false, admit_all);
    }
    assert_eq!(list.sealed_page_count(), 3);
    let serial = list.row(1).unwrap().page.serial();

    // Q2: rows 1..7 straddle sealed pages (rows up to 5) and active ones.
    list.mark_rows_changed(1..7, 20);
    assert_eq!(
        list.range_floors(),
        &[RangeFloor {
            start: 1,
            end: 6,
            seqno: 20
        }]
    );
    assert_eq!(list.row(1).unwrap().page.serial(), serial, "no page write");
    assert_eq!(list.row(1).unwrap().page.row_seqno(1), 2);
    assert_eq!(list.effective_seqno(1), Some(20));
    assert_eq!(list.effective_seqno(6), Some(20), "active rows are touched");
    assert_eq!(list.effective_seqno(0), Some(1));
    assert_eq!(list.effective_seqno(7), Some(8));
    let mut changed = Vec::new();
    list.changed_rows(0..10, 10, &mut changed);
    assert_eq!(changed, (1..7).collect::<Vec<StableRowIndex>>());

    // A palette change raises every row.
    list.mark_all_changed(30);
    changed.clear();
    list.changed_rows(0..10, 25, &mut changed);
    assert_eq!(changed, (0..10).collect::<Vec<StableRowIndex>>());

    // Q5: the numeric maximum comes first; only an all-zero result means
    // "always changed".
    let mut zero = PageList::with_page_rows(COLS, 2, 2, 100, 1);
    zero.scroll(1, 0, false, admit_all);
    assert_eq!(zero.effective_seqno(0), Some(0));
    assert_eq!(zero.changed_since(0, 1_000), Some(true));
    zero.mark_all_changed(5);
    assert_eq!(zero.effective_seqno(0), Some(5));
    assert_eq!(zero.changed_since(0, 5), Some(false));

    // Floors are dropped once trimming passes them.
    list.set_hot_cap(3);
    list.scroll(1, 40, false, admit_all);
    assert!(list.range_floors().is_empty());
    check(&list);
}

#[test]
fn range_floors_travel_with_rows_that_rotate() {
    let mut list = PageList::with_page_rows(COLS, 2, 2, 100, 1);
    for stable in 0..10 {
        list.scroll(1, stable as usize + 1, false, admit_all);
        put(&mut list, stable, stable as u32, stable as usize + 1);
    }
    // Rows 4 and 5 sit in a sealed page, so the bump becomes a floor.
    assert_eq!(list.sealed_page_count(), 3);
    list.mark_rows_changed(4..6, 20);
    assert_eq!(list.range_floors().len(), 1);

    // A taller viewport brings them into a scroll region, which moves row
    // 5's content down to row 6, outside the floor. The bump goes with it.
    list.set_viewport_rows(8);
    assert_eq!(list.sealed_page_count(), 0);
    list.rotate_down(4..8, 1, 30);
    assert_eq!(content(&list, 6), Some(5));
    assert!(list.effective_seqno(6).unwrap() >= 20);
    assert_eq!(list.changed_since(6, 10), Some(true));
    assert_eq!(content(&list, 4), None);
    check(&list);
}

// --- Random operation sequences against a `VecDeque` model ---

use proptest::prelude::*;

#[derive(Clone, Copy, Debug, PartialEq)]
struct ModelRow {
    content: Option<u32>,
    seqno: usize,
}

impl ModelRow {
    fn blank(seqno: usize) -> Self {
        ModelRow {
            content: None,
            seqno,
        }
    }
}

/// Legacy's row deque with the list's documented semantics: per-row
/// admission at the cap, refusal, recovery hold, region rotations, and the
/// over-cap reasons (ADR Q4).
struct DequeModel {
    rows: VecDeque<ModelRow>,
    first: StableRowIndex,
    cap: usize,
    viewport: usize,
    over: Option<OverCap>,
}

impl DequeModel {
    fn end(&self) -> StableRowIndex {
        self.first + self.rows.len() as StableRowIndex
    }

    fn index(&self, stable: StableRowIndex) -> usize {
        (stable - self.first) as usize
    }

    /// Trims from the front until `trimmed` reaches `target`; true when a row
    /// was refused.
    fn trim(
        &mut self,
        target: usize,
        trimmed: &mut usize,
        admit: &mut impl FnMut(StableRowIndex, Option<u32>) -> bool,
    ) -> bool {
        while *trimmed < target {
            let Some(front) = self.rows.front() else {
                break;
            };
            if !admit(self.first, front.content) {
                return true;
            }
            self.rows.pop_front();
            self.first += 1;
            *trimmed += 1;
        }
        false
    }

    fn scroll(
        &mut self,
        n: usize,
        seqno: usize,
        hold: bool,
        mut admit: impl FnMut(StableRowIndex, Option<u32>) -> bool,
    ) -> (usize, bool) {
        let excess = (self.rows.len() + n).saturating_sub(self.cap);
        let mut trimmed = 0;
        let mut refused = false;
        if !hold {
            refused = self.trim(excess, &mut trimmed, &mut admit);
        }
        for _ in 0..n {
            self.rows.push_back(ModelRow::blank(seqno));
        }
        if !hold && !refused {
            refused = self.trim(excess, &mut trimmed, &mut admit);
        }
        self.over = if self.rows.len() <= self.cap {
            None
        } else if hold {
            Some(OverCap::RecoveryHold)
        } else {
            Some(OverCap::SpillRefused)
        };
        (trimmed, refused)
    }

    fn clamp(&self, range: std::ops::Range<StableRowIndex>) -> (usize, usize) {
        let start = range.start.max(self.first);
        let end = range.end.min(self.end()).max(start);
        (self.index(start), self.index(end))
    }

    fn rotate_up(&mut self, range: std::ops::Range<StableRowIndex>, n: usize, seqno: usize) {
        let (start, end) = self.clamp(range);
        for _ in 0..n.min(end - start) {
            self.rows.remove(start);
            self.rows.insert(end - 1, ModelRow::blank(seqno));
        }
    }

    fn rotate_down(&mut self, range: std::ops::Range<StableRowIndex>, n: usize, seqno: usize) {
        let (start, end) = self.clamp(range);
        for _ in 0..n.min(end - start) {
            self.rows.remove(end - 1);
            self.rows.insert(start, ModelRow::blank(seqno));
        }
    }

    fn set_cap(&mut self, cap: usize) {
        self.cap = cap;
        self.over = if self.rows.len() <= cap {
            None
        } else {
            Some(self.over.unwrap_or(OverCap::CapLowered))
        };
    }

    fn viewport_top(&self) -> StableRowIndex {
        self.end() - self.viewport.min(self.rows.len()) as StableRowIndex
    }
}

/// The content id at column 0 of a page row, read straight from the page.
fn page_content(page: &Page, row: u32) -> Option<u32> {
    match page.glyph(row, 0) {
        Glyph::Blank => None,
        Glyph::Char(ch) => Some(ch as u32 - 0x100),
        Glyph::Cluster(text) => panic!("unexpected cluster {:?}", text),
    }
}

/// A reader holding a sealed page (the durability writer or the warm-tier
/// compressor), with what the page held when it was taken.
struct Reader {
    page: Arc<Page>,
    serial: u64,
    rows: Vec<(Option<u32>, usize)>,
}

impl Reader {
    fn new(page: Arc<Page>) -> Self {
        let rows = Self::snapshot(&page);
        Reader {
            serial: page.serial(),
            page,
            rows,
        }
    }

    fn snapshot(page: &Page) -> Vec<(Option<u32>, usize)> {
        (0..page.used())
            .map(|row| (page_content(page, row), page.row_seqno(row)))
            .collect()
    }

    fn check(&self) {
        assert_eq!(
            self.page.serial(),
            self.serial,
            "a reader's page changed serial"
        );
        assert_eq!(
            Self::snapshot(&self.page),
            self.rows,
            "a reader's sealed page was written"
        );
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// Full-screen line feeds. The sink admits `refuse_after` rows and then
    /// refuses (`None`: admits all), and may keep sealed pages it admits.
    Scroll {
        n: usize,
        refuse_after: Option<usize>,
        hold: bool,
        keep_sealed: bool,
    },
    /// A region `0..bottom` with `footer` rows below it.
    Footer {
        n: usize,
        footer: usize,
    },
    /// A region starting `top` rows below the viewport top.
    RotateUp {
        top: usize,
        height: usize,
        n: usize,
    },
    RotateDown {
        top: usize,
        height: usize,
        n: usize,
    },
    /// Writes the next content id into any retained row, sealed or not.
    Write {
        pick: usize,
    },
    HoldReader {
        pick: usize,
    },
    DropReader {
        pick: usize,
    },
    Viewport {
        rows: usize,
    },
    Cap {
        cap: usize,
    },
    MarkRows {
        pick: usize,
        len: usize,
    },
    MarkAll,
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        6 => (
            0..4_usize,
            proptest::option::weighted(0.2, 0..3_usize),
            proptest::bool::weighted(0.1),
            any::<bool>(),
        )
            .prop_map(|(n, refuse_after, hold, keep_sealed)| Op::Scroll {
                n,
                refuse_after,
                hold,
                keep_sealed,
            }),
        2 => (1..3_usize, 1..3_usize).prop_map(|(n, footer)| Op::Footer { n, footer }),
        2 => (0..6_usize, 0..6_usize, 1..3_usize)
            .prop_map(|(top, height, n)| Op::RotateUp { top, height, n }),
        2 => (0..6_usize, 0..6_usize, 1..3_usize)
            .prop_map(|(top, height, n)| Op::RotateDown { top, height, n }),
        4 => any::<usize>().prop_map(|pick| Op::Write { pick }),
        1 => any::<usize>().prop_map(|pick| Op::HoldReader { pick }),
        1 => any::<usize>().prop_map(|pick| Op::DropReader { pick }),
        1 => (1..7_usize).prop_map(|rows| Op::Viewport { rows }),
        1 => (1..16_usize).prop_map(|cap| Op::Cap { cap }),
        1 => (any::<usize>(), 0..6_usize).prop_map(|(pick, len)| Op::MarkRows { pick, len }),
        1 => Just(Op::MarkAll),
    ]
}

/// Readers kept at once; older ones are dropped.
const MAX_READERS: usize = 6;

fn run_ops(page_rows: u32, viewport: usize, cap: usize, ops: &[Op]) {
    let mut list = PageList::with_page_rows(COLS, page_rows, viewport, cap, 1);
    let mut model = DequeModel {
        rows: VecDeque::new(),
        first: 0,
        cap,
        viewport,
        over: None,
    };
    let mut readers: Vec<Reader> = Vec::new();
    let mut next_id = 0_u32;
    let mut seqno = 0_usize;

    for (step, op) in ops.iter().enumerate() {
        seqno += 1;
        match *op {
            Op::Scroll {
                n,
                refuse_after,
                hold,
                keep_sealed,
            } => {
                let mut offered = Vec::new();
                let mut kept = Vec::new();
                let mut budget = refuse_after;
                let scrolled = list.scroll(n, seqno, hold, |row| {
                    offered.push((row.stable, page_content(row.page, row.row)));
                    if let Some(left) = budget.as_mut() {
                        if *left == 0 {
                            return false;
                        }
                        *left -= 1;
                    }
                    if keep_sealed {
                        if let Some(page) = row.sealed {
                            kept.push(Arc::clone(page));
                        }
                    }
                    true
                });
                let mut model_offered = Vec::new();
                let mut budget = refuse_after;
                let (trimmed, refused) = model.scroll(n, seqno, hold, |stable, content| {
                    model_offered.push((stable, content));
                    match budget.as_mut() {
                        Some(0) => false,
                        Some(left) => {
                            *left -= 1;
                            true
                        }
                        None => true,
                    }
                });
                assert_eq!(offered, model_offered, "rows offered for eviction");
                assert_eq!((scrolled.trimmed, scrolled.refused), (trimmed, refused));
                assert_eq!(scrolled.first_new, model.end() - n as StableRowIndex);
                readers.extend(kept.into_iter().map(Reader::new));
            }
            Op::Footer { n, footer } => {
                list.scroll_above_footer(n, footer, seqno, false, admit_all);
                model.scroll(n, seqno, false, |_, _| true);
                let end = model.end();
                let start = (end - (footer + n) as StableRowIndex).max(model.first);
                model.rotate_down(start..end, n, seqno);
            }
            Op::RotateUp { top, height, n } => {
                let start = list.viewport_top() + top as StableRowIndex;
                let range = start..start + height as StableRowIndex;
                list.rotate_up(range.clone(), n, seqno);
                model.rotate_up(range, n, seqno);
            }
            Op::RotateDown { top, height, n } => {
                let start = list.viewport_top() + top as StableRowIndex;
                let range = start..start + height as StableRowIndex;
                list.rotate_down(range.clone(), n, seqno);
                model.rotate_down(range, n, seqno);
            }
            Op::Write { pick } => {
                if !model.rows.is_empty() {
                    let index = pick % model.rows.len();
                    let stable = model.first + index as StableRowIndex;
                    put(&mut list, stable, next_id, seqno);
                    let row = &mut model.rows[index];
                    *row = ModelRow {
                        content: Some(next_id),
                        seqno: row.seqno.max(seqno),
                    };
                    next_id += 1;
                }
            }
            Op::HoldReader { pick } => {
                let sealed: Vec<Arc<Page>> = list
                    .sealed_pages()
                    .map(|(_, page)| Arc::clone(page))
                    .collect();
                if !sealed.is_empty() {
                    readers.push(Reader::new(Arc::clone(&sealed[pick % sealed.len()])));
                }
            }
            Op::DropReader { pick } => {
                if !readers.is_empty() {
                    let index = pick % readers.len();
                    readers.remove(index).check();
                }
            }
            Op::Viewport { rows } => {
                list.set_viewport_rows(rows);
                model.viewport = rows;
            }
            Op::Cap { cap } => {
                list.set_hot_cap(cap);
                model.set_cap(cap);
            }
            Op::MarkRows { pick, len } => {
                if !model.rows.is_empty() {
                    let start = model.first + (pick % model.rows.len()) as StableRowIndex;
                    let range = start..start + len as StableRowIndex;
                    list.mark_rows_changed(range.clone(), seqno);
                    let (from, to) = model.clamp(range);
                    for row in model.rows.range_mut(from..to) {
                        row.seqno = row.seqno.max(seqno);
                    }
                }
            }
            Op::MarkAll => {
                list.mark_all_changed(seqno);
                for row in model.rows.iter_mut() {
                    row.seqno = row.seqno.max(seqno);
                }
            }
        }
        while readers.len() > MAX_READERS {
            readers.remove(0).check();
        }

        // Structure: I12 serials, I13 counts and over-cap reasons, I15
        // sealing, and every page's own invariants.
        check(&list);
        assert_eq!(list.first_stable(), model.first, "step {} {:?}", step, op);
        assert_eq!(list.end_stable(), model.end(), "step {} {:?}", step, op);
        assert_eq!(list.retained_rows(), model.rows.len());
        assert_eq!(list.over_cap(), model.over, "step {} {:?}", step, op);
        assert_eq!(list.viewport_top(), model.viewport_top());
        assert!(list.locate(model.first - 1).is_none() || model.first == 0);
        assert!(list.locate(model.end()).is_none());

        // Content (styles included), and I10: a row never reports an
        // effective seqno below the model's.
        for (index, expected) in model.rows.iter().enumerate() {
            let stable = model.first + index as StableRowIndex;
            assert_eq!(
                content(&list, stable),
                expected.content,
                "row {} after step {} {:?}",
                stable,
                step,
                op
            );
            let effective = list.effective_seqno(stable).expect("a retained row");
            assert!(
                effective >= expected.seqno,
                "I10: row {} effective seqno {} below the model's {}",
                stable,
                effective,
                expected.seqno
            );
        }

        // The page-skipping scan agrees with a row-by-row one.
        let since = seqno.saturating_sub(step % 5);
        let mut changed = Vec::new();
        list.changed_rows(model.first..model.end(), since, &mut changed);
        let brute: Vec<StableRowIndex> = (model.first..model.end())
            .filter(|&stable| list.changed_since(stable, since) == Some(true))
            .collect();
        assert_eq!(changed, brute, "changed_rows since {}", since);

        // Readers' sealed pages never change underneath them (D8).
        for reader in &readers {
            reader.check();
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 200,
        ..ProptestConfig::default()
    })]

    #[test]
    fn op_sequences_match_a_deque_model_and_keep_readers_pages_intact(
        page_rows in 1..5_u32,
        viewport in 1..6_usize,
        cap in 1..13_usize,
        ops in proptest::collection::vec(op_strategy(), 1..80),
    ) {
        run_ops(page_rows, viewport, cap, &ops);
    }
}
