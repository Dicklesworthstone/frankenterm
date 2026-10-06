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
