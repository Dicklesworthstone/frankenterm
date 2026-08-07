use crate::selection::{SelectionCoordinate, SelectionRange};
use crate::termwindow::{TermWindow, TermWindowNotif};
use config::ConfigHandle;
use config::keyassignment::{ClipboardCopyDestination, QuickSelectArguments, ScrollbackEraseMode};
use futures::channel::oneshot;
use futures::future::{AbortHandle, Abortable};
use mux::domain::DomainId;
use mux::pane::{
    CachePolicy, ForEachPaneLogicalLine, LogicalLine, Pane, PaneId, Pattern, SearchResult,
    WithPaneLines,
};
use mux::renderable::*;
use parking_lot::{MappedMutexGuard, Mutex};
use promise::spawn::sleep;
use rayon::prelude::*;
use rangeset::RangeSet;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use termwiz::cell::{Cell, CellAttributes};
use termwiz::color::AnsiColor;
use termwiz::surface::{SEQ_ZERO, SequenceNo};
use url::Url;
use wezterm_term::color::ColorPalette;
use wezterm_term::{
    Clipboard, Intensity, KeyCode, KeyModifiers, Line, MouseEvent, StableRowIndex, TerminalSize,
};
use window::WindowOps;

const MAX_QUICK_SELECT_SCOPE_RADIUS_ROWS: usize = 100_000;
const MAX_QUICK_SELECT_TOTAL_SEARCH_ROWS: usize = 200_000;
const SEARCH_RESULT_REQUEST_LIMIT: u32 = 100_000;
const MAX_SEARCH_RESULTS: usize = 100_000;
const MAX_EXPANDED_SEARCH_ROWS: usize = 200_000;
const PARALLEL_SORT_MIN_RESULTS: usize = 4096;
const SEARCH_RETRY_BASE_MILLIS: u64 = 50;
const SEARCH_RETRY_MAX_MILLIS: u64 = 1000;

const PATTERNS: [&str; 14] = [
    // markdown_url
    r"\[[^]]*\]\(([^)]+)\)",
    // url
    r"(?:https?://|git@|git://|ssh://|ftp://|file://)\S+",
    // diff_a
    r"--- a/(\S+)",
    // diff_b
    r"\+\+\+ b/(\S+)",
    // docker
    r"sha256:([0-9a-f]{64})",
    // path
    r"(?:[.\w\-@~]+)?(?:/+[.\w\-@]+)+",
    // color
    r"#[0-9a-fA-F]{6}",
    // uuid
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    // ipfs
    r"Qm[0-9a-zA-Z]{44}",
    // sha
    r"[0-9a-f]{7,40}",
    // ip
    r"\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}",
    // ipv6
    r"[A-f0-9:]+:+[A-f0-9:]+[%\w\d]+",
    // address
    r"0x[0-9a-fA-F]+",
    // number
    r"[0-9]{4,}",
];

/// This function computes a set of labels for a given alphabet.
/// It is derived from https://github.com/fcsonline/tmux-thumbs/blob/ae91d5f7c0d989933e86409833c46a1eca521b6a/src/alphabets.rs
/// which is Copyright (c) 2019 Ferran Basora and provided under the MIT license
pub fn compute_labels_for_alphabet(alphabet: &str, num_matches: usize) -> Vec<String> {
    compute_labels_for_alphabet_impl(alphabet, num_matches, true)
}

pub fn compute_labels_for_alphabet_with_preserved_case(
    alphabet: &str,
    num_matches: usize,
) -> Vec<String> {
    compute_labels_for_alphabet_impl(alphabet, num_matches, false)
}

fn compute_labels_for_alphabet_impl(
    alphabet: &str,
    num_matches: usize,
    make_lowercase: bool,
) -> Vec<String> {
    let alphabet = if make_lowercase {
        alphabet
            .chars()
            .map(|c| c.to_lowercase().to_string())
            .collect::<Vec<String>>()
    } else {
        alphabet
            .chars()
            .map(|c| c.to_string())
            .collect::<Vec<String>>()
    };
    // Prefer to use single character matches to represent everything
    let mut primary = alphabet.clone();
    let mut secondary = vec![];

    loop {
        let represented = primary.len().saturating_add(secondary.len());
        if represented >= num_matches {
            break;
        }

        // We have more matches than can be represented by alphabet,
        // so steal one of the single character options from the end
        // of the alphabet and use it to generate a two character
        // label
        let prefix = match primary.pop() {
            Some(p) => p,
            None => break,
        };

        // Generate a two character label for each of the alphabet
        // characters.  This ignores later alphabet characters;
        // since we popped our prefix from the end of alphabet,
        // length limiting this iteration ensures that we don't
        // end up with a duplicate letters in the result.
        let prefixed: Vec<String> = alphabet
            .iter()
            .take(
                num_matches
                    .saturating_sub(primary.len())
                    .saturating_sub(secondary.len()),
            )
            .map(|s| format!("{}{}", prefix, s))
            .collect();

        secondary.splice(0..0, prefixed);
    }

    let len = secondary.len();

    primary
        .drain(0..)
        .take(num_matches.saturating_sub(len))
        .chain(secondary.drain(0..))
        .collect()
}

fn merge_dirty_results(
    lines: Range<StableRowIndex>,
    mut delegate_dirty: RangeSet<StableRowIndex>,
    overlay_dirty: &RangeSet<StableRowIndex>,
) -> RangeSet<StableRowIndex> {
    delegate_dirty.add_set(overlay_dirty);
    delegate_dirty.intersection_with_range(lines)
}

/// Claim overlay damage only at the atomic source-fence discovery boundary.
/// `get_lines` and decoration reads are intentionally non-destructive because
/// they are not presentation acknowledgements. This remains a single-renderer
/// DamageGeneration handoff; a per-window GPU-present ACK needs a wider API.
fn take_dirty_results(
    lines: Range<StableRowIndex>,
    mut delegate_dirty: RangeSet<StableRowIndex>,
    overlay_dirty: &mut RangeSet<StableRowIndex>,
) -> RangeSet<StableRowIndex> {
    let claimed = overlay_dirty.intersection_with_range(lines.clone());
    delegate_dirty.add_set(&claimed);
    let mut retained = RangeSet::default();
    for range in overlay_dirty.iter() {
        if range.start < lines.start {
            retained.add_range(range.start..range.end.min(lines.start));
        }
        if range.end > lines.end {
            retained.add_range(range.start.max(lines.end)..range.end);
        }
    }
    *overlay_dirty = retained;
    delegate_dirty.intersection_with_range(lines)
}

fn prune_dirty_results(
    dirty: &mut RangeSet<StableRowIndex>,
    retained: Option<Range<StableRowIndex>>,
) {
    *dirty = retained
        .map(|range| dirty.intersection_with_range(range))
        .unwrap_or_default();
}

fn retained_row_range(dims: RenderableDimensions) -> Option<Range<StableRowIndex>> {
    let rows = StableRowIndex::try_from(dims.scrollback_rows).ok()?;
    let end = dims.scrollback_top.checked_add(rows)?;
    Some(dims.scrollback_top..end)
}

fn dirty_rows_for_search_refresh(
    previous_match_rows: impl IntoIterator<Item = StableRowIndex>,
    last_bar_pos: Option<StableRowIndex>,
    next_bar_pos: StableRowIndex,
) -> RangeSet<StableRowIndex> {
    let mut dirty_rows = RangeSet::default();

    for idx in previous_match_rows {
        dirty_rows.add(idx);
    }
    if let Some(idx) = last_bar_pos {
        dirty_rows.add(idx);
    }
    dirty_rows.add(next_bar_pos);

    dirty_rows
}

fn compute_search_row_from_viewport(
    viewport: Option<StableRowIndex>,
    dims: RenderableDimensions,
) -> StableRowIndex {
    // `RangeSet::add(row)` represents a scalar as `row..row + 1`, so MAX is
    // not a representable dirty row even though it is a valid scalar.
    let max_dirty_row = StableRowIndex::MAX.saturating_sub(1);
    let top = viewport.unwrap_or(dims.physical_top).min(max_dirty_row);
    let Some(last_row_offset) = dims
        .viewport_rows
        .checked_sub(1)
        .and_then(|offset| StableRowIndex::try_from(offset).ok())
    else {
        // A zero-height or unrepresentably tall viewport has no trustworthy
        // bottom row. Keep the overlay anchored at the known-representable
        // top instead of wrapping or inventing a row outside the viewport.
        return top;
    };

    top.checked_add(last_row_offset)
        .filter(|row| *row <= max_dirty_row)
        .unwrap_or(top)
}

fn checked_page_up_boundary(
    top: StableRowIndex,
    viewport_rows: usize,
) -> Option<StableRowIndex> {
    let viewport_rows = StableRowIndex::try_from(viewport_rows).ok()?;
    top.checked_sub(viewport_rows)
}

fn checked_page_down_boundary(
    top: StableRowIndex,
    viewport_rows: usize,
) -> Option<StableRowIndex> {
    let viewport_rows = StableRowIndex::try_from(viewport_rows).ok()?;
    top.checked_add(viewport_rows)
}

fn checked_search_range(
    top: StableRowIndex,
    viewport_rows: usize,
    scope: usize,
) -> Option<Range<StableRowIndex>> {
    let scope_rows = StableRowIndex::try_from(scope).ok()?;
    let upper_rows = viewport_rows.checked_add(scope)?;
    let upper_rows = StableRowIndex::try_from(upper_rows).ok()?;
    let end = top.checked_add(upper_rows)?;
    Some(top.saturating_sub(scope_rows)..end)
}

fn bounded_scope_rows(viewport_rows: usize, requested_scope: usize) -> Option<usize> {
    let remaining = MAX_QUICK_SELECT_TOTAL_SEARCH_ROWS.checked_sub(viewport_rows)?;
    Some(
        requested_scope
            .min(MAX_QUICK_SELECT_SCOPE_RADIUS_ROWS)
            .min(remaining / 2),
    )
}

fn intersect_row_ranges(
    left: Range<StableRowIndex>,
    right: Range<StableRowIndex>,
) -> Option<Range<StableRowIndex>> {
    let start = left.start.max(right.start);
    let end = left.end.min(right.end);
    (start < end).then_some(start..end)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SearchRunIdentity {
    id: usize,
    source_seqno: SequenceNo,
    cols: usize,
    viewport_rows: usize,
    scrollback_rows: usize,
    physical_top: StableRowIndex,
    scrollback_top: StableRowIndex,
    effective_top: StableRowIndex,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingSearch {
    run: SearchRunIdentity,
    range: Range<StableRowIndex>,
    retry_attempt: u8,
    is_initial_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SearchResultAnchor {
    start_x: usize,
    start_y: StableRowIndex,
    end_x: usize,
    end_y: StableRowIndex,
    match_id: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SearchCompletionStatus {
    Accepted,
    Superseded,
    SourceChanged,
}

fn allocate_search_run_id(next: &mut usize) -> Option<usize> {
    let id = *next;
    *next = id.checked_add(1)?;
    Some(id)
}

fn search_retry_delay(attempt: u8) -> Duration {
    let multiplier = 1u64
        .checked_shl(u32::from(attempt.min(10)))
        .unwrap_or(u64::MAX);
    Duration::from_millis(
        SEARCH_RETRY_BASE_MILLIS
            .saturating_mul(multiplier)
            .min(SEARCH_RETRY_MAX_MILLIS),
    )
}

fn search_result_anchor(result: &SearchResult) -> SearchResultAnchor {
    SearchResultAnchor {
        start_x: result.start_x,
        start_y: result.start_y,
        end_x: result.end_x,
        end_y: result.end_y,
        match_id: result.match_id,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ValidatedSearchGeometry {
    start_x: usize,
    start_y: StableRowIndex,
    end_x: usize,
    end_y: StableRowIndex,
    row_span: usize,
}

fn validate_search_geometry(
    start_x: usize,
    start_y: StableRowIndex,
    end_x: usize,
    end_y: StableRowIndex,
    searched: &Range<StableRowIndex>,
    cols: usize,
) -> Option<ValidatedSearchGeometry> {
    if cols == 0
        || searched.start >= searched.end
        || start_y > end_y
        || start_y < searched.start
        || start_x > cols
        || end_x > cols
    {
        return None;
    }
    let searched_last = searched.end.checked_sub(1)?;
    if end_y > searched_last {
        return None;
    }
    let row_span = end_y
        .checked_sub(start_y)?
        .checked_add(1)
        .and_then(|span| usize::try_from(span).ok())?;
    if (start_y == end_y && start_x >= end_x)
        || (row_span == 2 && start_x == cols && end_x == 0)
    {
        return None;
    }
    Some(ValidatedSearchGeometry {
        start_x,
        start_y,
        end_x,
        end_y,
        row_span,
    })
}

fn inclusive_search_result_end(
    result: &SearchResult,
    cols: usize,
) -> Option<(usize, StableRowIndex)> {
    if result.end_x > 0 {
        Some((result.end_x.checked_sub(1)?, result.end_y))
    } else if result.end_y > result.start_y && cols > 0 {
        Some((cols.checked_sub(1)?, result.end_y.checked_sub(1)?))
    } else {
        None
    }
}

fn sanitize_search_results(
    results: Vec<SearchResult>,
    searched: &Range<StableRowIndex>,
    cols: usize,
    cancel: &AtomicBool,
) -> Option<Vec<SearchResult>> {
    let mut sanitized = Vec::with_capacity(results.len().min(MAX_SEARCH_RESULTS));
    let mut expanded_rows = 0usize;
    for (index, mut result) in results.into_iter().take(MAX_SEARCH_RESULTS).enumerate() {
        if index.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let Some(geometry) = validate_search_geometry(
            result.start_x,
            result.start_y,
            result.end_x,
            result.end_y,
            searched,
            cols,
        ) else {
            continue;
        };
        let Some(next_expanded_rows) = expanded_rows.checked_add(geometry.row_span) else {
            break;
        };
        if next_expanded_rows > MAX_EXPANDED_SEARCH_ROWS {
            break;
        }
        expanded_rows = next_expanded_rows;
        result.start_x = geometry.start_x;
        result.start_y = geometry.start_y;
        result.end_x = geometry.end_x;
        result.end_y = geometry.end_y;
        sanitized.push(result);
    }
    (!cancel.load(Ordering::Relaxed)).then_some(sanitized)
}

fn search_run_identity(
    id: usize,
    source_seqno: SequenceNo,
    dims: RenderableDimensions,
    effective_top: StableRowIndex,
) -> SearchRunIdentity {
    SearchRunIdentity {
        id,
        source_seqno,
        cols: dims.cols,
        viewport_rows: dims.viewport_rows,
        scrollback_rows: dims.scrollback_rows,
        physical_top: dims.physical_top,
        scrollback_top: dims.scrollback_top,
        effective_top,
    }
}

fn classify_search_completion(
    pending: Option<&PendingSearch>,
    run: SearchRunIdentity,
    range: &Range<StableRowIndex>,
    current_source: SearchRunIdentity,
    source_range_changed: bool,
) -> SearchCompletionStatus {
    let Some(pending) = pending else {
        return SearchCompletionStatus::Superseded;
    };
    if pending.run != run || pending.range != *range {
        return SearchCompletionStatus::Superseded;
    }
    let current_retained_end = StableRowIndex::try_from(current_source.scrollback_rows)
        .ok()
        .and_then(|rows| current_source.scrollback_top.checked_add(rows));
    if run.cols != current_source.cols
        || run.viewport_rows != current_source.viewport_rows
        || run.effective_top != current_source.effective_top
        || current_source.scrollback_top > range.start
        || current_retained_end.is_none_or(|end| range.end > end)
        || source_range_changed
    {
        return SearchCompletionStatus::SourceChanged;
    }
    SearchCompletionStatus::Accepted
}

#[cfg(test)]
mod alphabet_test {
    use super::*;
    use std::ops::Range;

    #[test]
    fn simple_alphabet() {
        assert_eq!(compute_labels_for_alphabet("abcd", 3), vec!["a", "b", "c"]);
    }

    #[test]
    fn more_matches_than_alphabet_can_represent() {
        assert_eq!(
            compute_labels_for_alphabet("asdfqwerzxcvjklmiuopghtybn", 792).len(),
            676
        );
    }

    #[test]
    fn composed_single() {
        assert_eq!(
            compute_labels_for_alphabet("abcd", 6),
            vec!["a", "b", "c", "da", "db", "dc"]
        );
    }

    #[test]
    fn composed_multiple() {
        assert_eq!(
            compute_labels_for_alphabet("abcd", 8),
            vec!["a", "b", "ca", "cb", "da", "db", "dc", "dd"]
        );
    }

    #[test]
    fn composed_max() {
        // The number of chars in the alphabet limits the potential matches to fewer
        // than the number of matches that we requested
        assert_eq!(
            compute_labels_for_alphabet("ab", 5),
            vec!["aa", "ab", "ba", "bb"]
        );
    }

    #[test]
    fn alphabet_remaining_count_saturates_for_unrepresentable_request() {
        assert_eq!(
            compute_labels_for_alphabet("ab", usize::MAX),
            vec!["aa", "ab", "ba", "bb"]
        );
    }

    #[test]
    fn composed_capital() {
        assert_eq!(
            compute_labels_for_alphabet_with_preserved_case("AB", 4),
            vec!["AA", "AB", "BA", "BB"]
        );
    }

    #[test]
    fn composed_mixed() {
        assert_eq!(
            compute_labels_for_alphabet_with_preserved_case("aA", 4),
            vec!["aa", "aA", "Aa", "AA"]
        );
    }

    #[test]
    fn lowercase_alphabet_equal() {
        assert_eq!(
            compute_labels_for_alphabet_with_preserved_case("abc123", 12),
            compute_labels_for_alphabet("abc123", 12)
        );
    }

    fn make_dirty_ranges(
        ranges: impl IntoIterator<Item = Range<StableRowIndex>>,
    ) -> RangeSet<StableRowIndex> {
        let mut dirty = RangeSet::default();
        for range in ranges {
            dirty.add_range(range);
        }
        dirty
    }

    fn collect_ranges(set: &RangeSet<StableRowIndex>) -> Vec<Range<StableRowIndex>> {
        set.iter().cloned().collect()
    }

    #[test]
    fn test_dirty_rect_merge() {
        let visible = 9..15;
        let delegate_dirty = make_dirty_ranges(std::iter::once(10..12));
        let overlay_dirty = make_dirty_ranges(std::iter::once(12..14));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![10..14]);
    }

    #[test]
    fn dirty_rect_non_adjacent_ranges_stay_separate() {
        // [10..12] and [14..16] share no boundary — there's a gap at row 12,13.
        // The merge MUST NOT collapse them into one rect, otherwise rows 12-13
        // would be needlessly repainted on every overlay refresh.
        let visible = 0..30;
        let delegate_dirty = make_dirty_ranges(std::iter::once(10..12));
        let overlay_dirty = make_dirty_ranges(std::iter::once(14..16));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![10..12, 14..16]);
    }

    #[test]
    fn dirty_rect_overlapping_ranges_collapse_to_one() {
        // delegate [10..14] and overlay [12..16] overlap at 12,13.
        // RangeSet semantics: overlap collapses into [10..16].
        let visible = 0..30;
        let delegate_dirty = make_dirty_ranges(std::iter::once(10..14));
        let overlay_dirty = make_dirty_ranges(std::iter::once(12..16));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![10..16]);
    }

    #[test]
    fn dirty_rect_clips_to_visible_viewport() {
        // delegate touches rows 5..8 (entirely above the visible window) and
        // 12..14 (inside). overlay touches 18..22 which spills past the
        // bottom edge. After clipping to visible 10..20, only the inside
        // portions survive. This is the invariant that prevents the
        // renderer from invalidating off-screen geometry.
        let visible = 10..20;
        let delegate_dirty = make_dirty_ranges([5..8, 12..14]);
        let overlay_dirty = make_dirty_ranges(std::iter::once(18..22));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![12..14, 18..20]);
    }

    #[test]
    fn dirty_rect_empty_overlay_leaves_delegate_clipped() {
        // Common case: overlay is dormant and contributes nothing. The
        // result should be exactly the delegate, clipped to the viewport.
        let visible = 0..10;
        let delegate_dirty = make_dirty_ranges([2..4, 7..15]);
        let overlay_dirty = RangeSet::default();

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![2..4, 7..10]);
    }

    #[test]
    fn dirty_rect_both_empty_produces_empty_set() {
        let visible = 0..10;
        let merged = merge_dirty_results(visible, RangeSet::default(), &RangeSet::default());
        assert!(collect_ranges(&merged).is_empty());
    }

    #[test]
    fn dirty_rect_back_to_back_single_rows_merge_into_one() {
        // Adjacent SINGLE-row dirty marks (e.g. cursor moved one row, then
        // the next was also dirtied) must collapse into one rect. RangeSet
        // treats {[5..6], [6..7], [7..8]} as the contiguous range [5..8].
        let visible = 0..20;
        let delegate_dirty = make_dirty_ranges([5..6, 6..7]);
        let overlay_dirty = make_dirty_ranges(std::iter::once(7..8));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert_eq!(collect_ranges(&merged), vec![5..8]);
    }

    #[test]
    fn dirty_rect_out_of_viewport_ranges_drop_completely() {
        // If every dirty range sits entirely outside the visible window,
        // the merge result is empty — nothing to paint.
        let visible = 100..110;
        let delegate_dirty = make_dirty_ranges(std::iter::once(5..8));
        let overlay_dirty = make_dirty_ranges(std::iter::once(200..205));

        let merged = merge_dirty_results(visible, delegate_dirty, &overlay_dirty);

        assert!(collect_ranges(&merged).is_empty());
    }

    #[test]
    fn search_refresh_marks_previous_match_rows_and_new_search_row_dirty() {
        let dirty = dirty_rows_for_search_refresh([20, 21], Some(24), 30);

        assert_eq!(collect_ranges(&dirty), vec![20..22, 24..25, 30..31]);
    }

    #[test]
    fn compute_search_row_tracks_viewport_bottom_edge() {
        let dims = RenderableDimensions {
            cols: 80,
            viewport_rows: 5,
            scrollback_rows: 100,
            physical_top: 40,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 80,
            reverse_video: false,
        };

        assert_eq!(compute_search_row_from_viewport(None, dims), 44);
        assert_eq!(compute_search_row_from_viewport(Some(12), dims), 16);
    }

    #[test]
    fn compute_search_row_fails_closed_when_bottom_would_overflow() {
        let dims = RenderableDimensions {
            cols: 80,
            viewport_rows: 2,
            scrollback_rows: 100,
            physical_top: StableRowIndex::MAX,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 80,
            reverse_video: false,
        };

        let row = compute_search_row_from_viewport(Some(StableRowIndex::MAX), dims);
        assert_eq!(row, StableRowIndex::MAX.saturating_sub(1));
        let mut dirty = RangeSet::default();
        dirty.add(row);
        assert_eq!(collect_ranges(&dirty), vec![row..StableRowIndex::MAX]);
    }

    #[test]
    fn compute_search_row_fails_closed_for_oversized_or_empty_viewport() {
        let mut dims = RenderableDimensions {
            cols: 80,
            viewport_rows: usize::MAX,
            scrollback_rows: 100,
            physical_top: 40,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 640,
            pixel_height: 80,
            reverse_video: false,
        };

        assert_eq!(compute_search_row_from_viewport(Some(17), dims), 17);
        dims.viewport_rows = 0;
        assert_eq!(compute_search_row_from_viewport(Some(17), dims), 17);
    }

    #[test]
    fn page_navigation_boundaries_fail_closed_at_stable_row_limits() {
        assert_eq!(checked_page_up_boundary(StableRowIndex::MIN, 1), None);
        assert_eq!(
            checked_page_down_boundary(StableRowIndex::MAX, 1),
            None
        );
    }

    #[test]
    fn page_navigation_boundaries_reject_oversized_viewports() {
        assert_eq!(checked_page_up_boundary(17, usize::MAX), None);
        assert_eq!(checked_page_down_boundary(17, usize::MAX), None);
    }

    #[test]
    fn search_range_rejects_overflow_and_oversized_counts() {
        assert_eq!(checked_search_range(StableRowIndex::MAX, 1, 1), None);
        assert_eq!(checked_search_range(17, usize::MAX, 1), None);
        assert_eq!(checked_search_range(17, 1, usize::MAX), None);
    }

    #[test]
    fn search_range_handles_zero_dimensions_without_inventing_rows() {
        assert_eq!(checked_search_range(7, 0, 0), Some(7..7));
    }

    #[test]
    fn quick_select_scope_has_a_hard_total_row_envelope() {
        assert_eq!(bounded_scope_rows(20, usize::MAX), Some(99_990));
        assert_eq!(bounded_scope_rows(200_000, 1000), Some(0));
        assert_eq!(bounded_scope_rows(200_001, 0), None);
        let scope = bounded_scope_rows(20, usize::MAX).expect("bounded scope");
        let range = checked_search_range(0, 20, scope).expect("representable range");
        assert_eq!(range.end.saturating_sub(range.start), 200_000);
    }

    #[test]
    fn search_range_is_clipped_to_retained_scrollback() {
        assert_eq!(intersect_row_ranges(-20..20, -5..10), Some(-5..10));
        assert_eq!(intersect_row_ranges(-20..-10, -5..10), None);
    }

    fn search_identity(id: usize, seqno: SequenceNo, cols: usize) -> SearchRunIdentity {
        search_run_identity(
            id,
            seqno,
            RenderableDimensions {
                cols,
                viewport_rows: 5,
                scrollback_rows: 100,
                physical_top: 40,
                scrollback_top: 0,
                dpi: 96,
                pixel_width: 640,
                pixel_height: 80,
                reverse_video: false,
            },
            40,
        )
    }

    fn pending(run: SearchRunIdentity, range: Range<StableRowIndex>) -> PendingSearch {
        PendingSearch {
            run,
            range,
            retry_attempt: 0,
            is_initial_run: true,
        }
    }

    #[test]
    fn quick_select_rejects_same_pattern_from_superseded_run() {
        let old = search_identity(1, 10, 80);
        let current = search_identity(2, 10, 80);
        let pending = pending(current, 0..10);

        assert_eq!(
            classify_search_completion(Some(&pending), old, &(0..10), current, false),
            SearchCompletionStatus::Superseded
        );
    }

    #[test]
    fn quick_select_accepts_only_exact_run_range_and_source() {
        let run = search_identity(2, 10, 80);
        let pending = pending(run, 0..10);

        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), run, false),
            SearchCompletionStatus::Accepted
        );
    }

    #[test]
    fn quick_select_rejects_out_of_order_range_completion() {
        let run = search_identity(2, 10, 80);
        let pending = pending(run, 0..10);

        assert_eq!(
            classify_search_completion(Some(&pending), run, &(10..20), run, false),
            SearchCompletionStatus::Superseded
        );
    }

    #[test]
    fn quick_select_ignores_unrelated_source_change_but_rejects_dirty_range_resize_or_scroll() {
        let run = search_identity(2, 10, 80);
        let pending = pending(run, 0..10);
        let changed_source = search_identity(99, 11, 80);
        let resized = search_identity(99, 10, 120);
        let mut height_resized = search_identity(99, 10, 80);
        height_resized.viewport_rows = 6;
        let mut scrolled = search_identity(99, 10, 80);
        scrolled.effective_top = 41;

        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), changed_source, false),
            SearchCompletionStatus::Accepted
        );
        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), changed_source, true),
            SearchCompletionStatus::SourceChanged
        );
        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), resized, false),
            SearchCompletionStatus::SourceChanged
        );
        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), height_resized, false),
            SearchCompletionStatus::SourceChanged
        );
        assert_eq!(
            classify_search_completion(Some(&pending), run, &(0..10), scrolled, false),
            SearchCompletionStatus::SourceChanged
        );
    }

    #[test]
    fn quick_select_search_run_id_exhaustion_never_wraps_or_reuses_an_id() {
        let mut next = usize::MAX;
        assert_eq!(allocate_search_run_id(&mut next), None);
        assert_eq!(next, usize::MAX);
    }

    #[test]
    fn quick_select_instance_tokens_do_not_alias_across_reopen() {
        let first = Arc::new(());
        let reopened = Arc::new(());
        assert!(!Arc::ptr_eq(&first, &reopened));
        assert!(Arc::ptr_eq(&first, &Arc::clone(&first)));
    }

    #[test]
    fn quick_select_damage_transfer_preserves_unclaimed_rows() {
        let mut overlay = make_dirty_ranges([1..4, 8..12]);
        let claimed = take_dirty_results(2..10, RangeSet::default(), &mut overlay);
        assert_eq!(collect_ranges(&claimed), vec![2..4, 8..10]);
        assert_eq!(collect_ranges(&overlay), vec![1..2, 10..12]);
    }

    #[test]
    fn quick_select_damage_pruning_handles_negative_scrollback() {
        let mut dirty = make_dirty_ranges([-20..-10, -5..3, 8..12]);
        prune_dirty_results(&mut dirty, Some(-10..10));
        assert_eq!(collect_ranges(&dirty), vec![-5..3, 8..10]);
    }

    #[test]
    fn quick_select_search_geometry_is_strictly_validated_and_bounded() {
        assert_eq!(validate_search_geometry(7, -20, 90, 20, &(-5..6), 80), None);
        assert_eq!(validate_search_geometry(9, 2, 9, 2, &(0..4), 80), None);
        assert_eq!(validate_search_geometry(0, 9, 1, 2, &(0..10), 80), None);
        assert_eq!(
            validate_search_geometry(7, -5, 80, 5, &(-5..6), 80),
            Some(ValidatedSearchGeometry {
                start_x: 7,
                start_y: -5,
                end_x: 80,
                end_y: 5,
                row_span: 11,
            })
        );
    }

    #[test]
    fn quick_select_multiline_exclusive_zero_end_selects_previous_row() {
        let result = SearchResult {
            start_y: 2,
            start_x: 7,
            end_y: 3,
            end_x: 0,
            match_id: 1,
        };
        assert_eq!(inclusive_search_result_end(&result, 80), Some((79, 2)));
    }

    #[test]
    fn quick_select_preparation_builds_labels_and_rows_off_ui_path() {
        let cancel = AtomicBool::new(false);
        let prepared = prepare_quick_select_results(
            vec![SearchResult {
                start_y: 3,
                start_x: 4,
                end_y: 3,
                end_x: 8,
                match_id: 9,
            }],
            &(0..10),
            80,
            "asdf",
            &cancel,
        )
        .expect("uncancelled preparation completes");

        assert_eq!(prepared.results.len(), 1);
        assert_eq!(prepared.by_label.len(), 1);
        assert_eq!(prepared.by_line.get(&3).map(Vec::len), Some(1));
        assert_eq!(collect_ranges(&prepared.dirty_rows), vec![3..4]);
    }

    #[test]
    fn quick_select_preparation_honors_latest_wins_cancellation() {
        let cancel = AtomicBool::new(true);
        assert!(
            prepare_quick_select_results(Vec::new(), &(0..10), 80, "asdf", &cancel).is_none()
        );
    }

    #[test]
    fn quick_select_preparation_gate_rejects_overlap_without_queueing() {
        let gate = Mutex::new(());
        let _running = gate.lock();
        assert!(gate.try_lock().is_none());
    }

    #[test]
    fn quick_select_retry_delay_is_bounded() {
        assert_eq!(search_retry_delay(0), Duration::from_millis(50));
        assert_eq!(search_retry_delay(u8::MAX), Duration::from_millis(1000));
    }
}

pub struct QuickSelectOverlay {
    renderer: Mutex<QuickSelectRenderable>,
    delegate: Arc<dyn Pane>,
}

fn quick_select_action_is_current(
    term_window: &TermWindow,
    pane_id: PaneId,
    instance_token: &Arc<()>,
    accepted_run_id: usize,
) -> bool {
    term_window
        .pane_state(pane_id)
        .overlay
        .as_ref()
        .and_then(|overlay| overlay.pane.downcast_ref::<QuickSelectOverlay>())
        .is_some_and(|search_overlay| {
            let renderer = search_overlay.renderer.lock();
            Arc::ptr_eq(&renderer.instance_token, instance_token)
                && renderer.accepted_run_id == Some(accepted_run_id)
                && renderer.action_pending
        })
}

fn close_quick_select_overlay_if_current(
    term_window: &TermWindow,
    pane_id: PaneId,
    instance_token: &Arc<()>,
) {
    let removed = {
        let mut state = term_window.pane_state(pane_id);
        let is_current = state
            .overlay
            .as_ref()
            .and_then(|overlay| overlay.pane.downcast_ref::<QuickSelectOverlay>())
            .is_some_and(|search_overlay| {
                Arc::ptr_eq(
                    &search_overlay.renderer.lock().instance_token,
                    instance_token,
                )
            });
        if is_current {
            state.overlay.take();
            true
        } else {
            false
        }
    };
    if removed {
        if let Some(window) = term_window.window.as_ref() {
            window.invalidate();
        }
    }
}

fn restart_quick_select_after_stale_action(
    term_window: &TermWindow,
    pane_id: PaneId,
    instance_token: &Arc<()>,
) {
    let state = term_window.pane_state(pane_id);
    if let Some(overlay) = state.overlay.as_ref() {
        if let Some(search_overlay) = overlay.pane.downcast_ref::<QuickSelectOverlay>() {
            let mut renderer = search_overlay.renderer.lock();
            if Arc::ptr_eq(&renderer.instance_token, instance_token) {
                renderer.action_pending = false;
                renderer.selection.clear();
                renderer.restart_search(false, true, 0);
            }
        }
    }
}

#[derive(Debug)]
struct MatchResult {
    range: Range<usize>,
    label: String,
}

struct PreparedQuickSelectResults {
    results: Vec<SearchResult>,
    by_line: HashMap<StableRowIndex, Vec<MatchResult>>,
    by_label: HashMap<String, usize>,
    dirty_rows: RangeSet<StableRowIndex>,
}

/// Validate, sort, label, and expand matches away from the window callback.
/// The UI-side install is then a bounded collection swap rather than up to
/// hundreds of thousands of allocations while input and rendering wait on
/// the renderer mutex.
fn prepare_quick_select_results(
    results: Vec<SearchResult>,
    searched: &Range<StableRowIndex>,
    cols: usize,
    alphabet: &str,
    cancel: &AtomicBool,
) -> Option<PreparedQuickSelectResults> {
    let mut results = sanitize_search_results(results, searched, cols, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    if results.len() >= PARALLEL_SORT_MIN_RESULTS {
        results.par_sort_unstable();
    } else {
        results.sort_unstable();
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }

    let mut uniq_results: Vec<usize> = results.iter().map(|result| result.match_id).collect();
    if uniq_results.len() >= PARALLEL_SORT_MIN_RESULTS {
        uniq_results.par_sort_unstable();
    } else {
        uniq_results.sort_unstable();
    }
    if cancel.load(Ordering::Relaxed) {
        return None;
    }
    uniq_results.dedup();
    let labels = compute_labels_for_alphabet(alphabet, uniq_results.len());

    let mut assigned_labels: HashMap<usize, usize> = HashMap::new();
    let mut by_line: HashMap<StableRowIndex, Vec<MatchResult>> = HashMap::new();
    let mut by_label = HashMap::new();
    let mut dirty_rows = RangeSet::default();

    // Reverse traversal preserves the established label priority: the
    // bottom-right-most distinct match receives the first label.
    for (ordinal, (result_index, result)) in results.iter().enumerate().rev().enumerate() {
        if ordinal.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
            return None;
        }
        let label_index = match assigned_labels.get(&result.match_id).copied() {
            Some(index) => index,
            None => {
                let index = assigned_labels.len();
                assigned_labels.insert(result.match_id, index);
                index
            }
        };
        let Some(label) = labels.get(label_index) else {
            continue;
        };
        by_label.entry(label.clone()).or_insert(result_index);

        for (row_offset, row) in (result.start_y..=result.end_y).enumerate() {
            if row_offset.is_multiple_of(256) && cancel.load(Ordering::Relaxed) {
                return None;
            }
            let range = if row == result.start_y && row == result.end_y {
                result.start_x..result.end_x
            } else if row == result.end_y {
                0..result.end_x
            } else if row == result.start_y {
                result.start_x..cols
            } else {
                0..cols
            };
            if range.start >= range.end {
                continue;
            }
            by_line.entry(row).or_default().push(MatchResult {
                range,
                label: label.clone(),
            });
            dirty_rows.add(row);
        }
    }

    Some(PreparedQuickSelectResults {
        results,
        by_line,
        by_label,
        dirty_rows,
    })
}

async fn prepare_quick_select_results_off_thread(
    results: Vec<SearchResult>,
    searched: Range<StableRowIndex>,
    cols: usize,
    alphabet: String,
    cancel: Arc<AtomicBool>,
    gate: Arc<Mutex<()>>,
) -> Result<PreparedQuickSelectResults, String> {
    let (sender, receiver) = oneshot::channel();
    rayon::spawn(move || {
        let Some(_gate_guard) = gate.try_lock() else {
            let _ = sender.send(Err("quick-select preparation lane busy".to_string()));
            return;
        };
        let prepared =
            prepare_quick_select_results(results, &searched, cols, &alphabet, &cancel);
        if let Some(prepared) = prepared {
            let _ = sender.send(Ok(prepared));
        } else {
            let _ = sender.send(Err("quick-select preparation cancelled".to_string()));
        }
    });
    receiver
        .await
        .map_err(|_| "quick-select result preparation worker stopped".to_string())?
}

struct QuickSelectRenderable {
    /// Allocation identity for this exact overlay. It prevents a delayed
    /// callback from attaching to a later overlay that reuses the PaneId and
    /// starts its local run counter at the same value.
    instance_token: Arc<()>,
    delegate: Arc<dyn Pane>,
    /// The text that the user entered
    pattern: Pattern,
    /// The most recently queried set of matches
    results: Vec<SearchResult>,
    by_line: HashMap<StableRowIndex, Vec<MatchResult>>,
    by_label: HashMap<String, usize>,
    selection: String,
    action_pending: bool,

    viewport: Option<StableRowIndex>,
    last_bar_pos: Option<StableRowIndex>,

    dirty_results: RangeSet<StableRowIndex>,
    result_pos: Option<usize>,
    width: usize,
    height: usize,
    next_search_run_id: usize,
    searching: Option<PendingSearch>,
    search_abort: Option<AbortHandle>,
    search_preparation_cancel: Option<Arc<AtomicBool>>,
    search_preparation_gate: Arc<Mutex<()>>,
    retry_abort: Option<AbortHandle>,
    retry_token: Option<Arc<()>>,
    desired_result: Option<SearchResultAnchor>,
    desired_result_ordinal: Option<usize>,
    accepted_source_end: Option<SequenceNo>,
    accepted_range: Option<Range<StableRowIndex>>,
    accepted_run_id: Option<usize>,

    /// We use this to cancel ourselves later
    window: ::window::Window,

    config: ConfigHandle,
    args: QuickSelectArguments,
}

impl QuickSelectOverlay {
    pub fn with_pane(
        term_window: &TermWindow,
        pane: &Arc<dyn Pane>,
        args: &QuickSelectArguments,
    ) -> anyhow::Result<Arc<dyn Pane>> {
        let viewport = term_window.get_viewport(pane.pane_id());
        let dims = pane.get_dimensions();

        let config = term_window.config.clone();

        let mut pattern = "(?m)(".to_string();
        let mut have_patterns = false;
        if !args.patterns.is_empty() {
            for p in &args.patterns {
                if have_patterns {
                    pattern.push('|');
                }
                pattern.push_str(p);
                have_patterns = true;
            }
        } else {
            // User-provided patterns take precedence over built-ins
            for p in &config.quick_select_patterns {
                if have_patterns {
                    pattern.push('|');
                }
                pattern.push_str(p);
                have_patterns = true;
            }
            if !config.disable_default_quick_select_patterns {
                for p in &PATTERNS {
                    if have_patterns {
                        pattern.push('|');
                    }
                    pattern.push_str(p);
                    have_patterns = true;
                }
            }
        }
        pattern.push(')');

        let pattern = Pattern::Regex(pattern);

        let window = term_window.window.clone().ok_or_else(|| {
            anyhow::anyhow!("cannot start quick-select overlay without a GUI window")
        })?;
        let mut renderer = QuickSelectRenderable {
            instance_token: Arc::new(()),
            delegate: Arc::clone(pane),
            pattern,
            selection: "".to_string(),
            action_pending: false,
            results: vec![],
            by_line: HashMap::new(),
            by_label: HashMap::new(),
            dirty_results: RangeSet::default(),
            viewport,
            last_bar_pos: None,
            window,
            result_pos: None,
            width: dims.cols,
            height: dims.viewport_rows,
            next_search_run_id: 0,
            searching: None,
            search_abort: None,
            search_preparation_cancel: None,
            search_preparation_gate: Arc::new(Mutex::new(())),
            retry_abort: None,
            retry_token: None,
            desired_result: None,
            desired_result_ordinal: None,
            accepted_source_end: None,
            accepted_range: None,
            accepted_run_id: None,
            config,
            args: args.clone(),
        };

        let search_row = renderer.compute_search_row();
        renderer.dirty_results.add(search_row);
        renderer.update_search(true);

        Ok(Arc::new(QuickSelectOverlay {
            renderer: Mutex::new(renderer),
            delegate: Arc::clone(pane),
        }))
    }

    pub fn viewport_changed(&self, viewport: Option<StableRowIndex>) {
        let mut render = self.renderer.lock();
        if render.viewport != viewport {
            if let Some(last) = render.last_bar_pos.take() {
                render.dirty_results.add(last);
            }
            render.viewport = viewport;
            // The quick-select query range is centered on the effective
            // viewport top. Treat a viewport change as a new initial search so
            // an old in-flight range cannot install stale labels, while also
            // avoiding an automatic match activation that would move the
            // viewport again and create a search loop.
            render.restart_search(true, true, 0);
        }
    }
}

impl Drop for QuickSelectRenderable {
    fn drop(&mut self) {
        if let Some(cancel) = self.search_preparation_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(abort) = self.search_abort.take() {
            abort.abort();
        }
        if let Some(abort) = self.retry_abort.take() {
            abort.abort();
        }
    }
}

impl QuickSelectOverlay {
    fn with_decorated_lines(
        &self,
        lines: Range<StableRowIndex>,
        hyperlink_rules: Option<&[termwiz::hyperlink::Rule]>,
        with_lines: &mut dyn WithPaneLines,
    ) {
        let mut renderer = self.renderer.lock();
        // Keep one delegate snapshot under the overlay decoration pass. The
        // combined path must reach the delegate before this overlay clones the
        // rows, otherwise copy-backed hyperlink mutations are discarded.
        renderer.prepare_for_render();
        let dims = self.get_dimensions();
        let search_row = renderer.compute_search_row();

        struct OverlayLines<'a> {
            with_lines: &'a mut dyn WithPaneLines,
            dims: RenderableDimensions,
            search_row: StableRowIndex,
            renderer: &'a mut QuickSelectRenderable,
        }

        impl WithPaneLines for OverlayLines<'_> {
            fn with_lines_mut(&mut self, first_row: StableRowIndex, lines: &mut [&mut Line]) {
                let mut overlay_lines = vec![];

                let config = &self.renderer.config;
                let colors = config.resolved_palette.clone();
                let disable_attr = config.quick_select_remove_styling;

                for (idx, line) in lines.iter_mut().enumerate() {
                    let mut line: Line = line.clone();
                    if disable_attr {
                        line.cells_mut_for_attr_changes_only()
                            .iter_mut()
                            .for_each(|cell| cell.attrs_mut().clear());
                        line.clear_appdata();
                    }
                    let Some(stable_idx) = StableRowIndex::try_from(idx)
                        .ok()
                        .and_then(|offset| first_row.checked_add(offset))
                    else {
                        break;
                    };
                    if stable_idx == self.search_row {
                        let rev = CellAttributes::default().set_reverse(true).clone();
                        line.fill_range(0..self.dims.cols, &Cell::new(' ', rev.clone()), SEQ_ZERO);
                        line.overlay_text_with_attribute(
                            0,
                            &format!(
                                "Select: {}  (type highlighted prefix to {}, uppercase pastes, ESC to cancel)",
                                self.renderer.selection,
                                if self.renderer.args.label.is_empty() {
                                    "copy"
                                } else {
                                    &self.renderer.args.label
                                },
                            ),
                            rev,
                            SEQ_ZERO,
                        );
                        self.renderer.last_bar_pos = Some(self.search_row);
                        line.clear_appdata();
                    } else if let Some(matches) = self.renderer.by_line.get(&stable_idx) {
                        for m in matches {
                            for cell_idx in m.range.clone() {
                                if let Some(cell) =
                                    line.cells_mut_for_attr_changes_only().get_mut(cell_idx)
                                {
                                    cell.attrs_mut()
                                        .set_background(
                                            colors
                                                .quick_select_match_bg
                                                .unwrap_or(AnsiColor::Black.into()),
                                        )
                                        .set_foreground(
                                            colors
                                                .quick_select_match_fg
                                                .unwrap_or(AnsiColor::Green.into()),
                                        )
                                        .set_reverse(false)
                                        .set_intensity(Intensity::Bold);
                                }
                            }
                            for (idx, c) in m.label.chars().enumerate() {
                                let Some(label_col) = m.range.start.checked_add(idx) else {
                                    break;
                                };
                                if label_col >= self.dims.cols || label_col >= line.len() {
                                    break;
                                }
                                let mut attr = line
                                    .get_cell(label_col)
                                    .map(|cell| cell.attrs().clone())
                                    .unwrap_or_else(CellAttributes::default);
                                attr.set_background(
                                    colors
                                        .quick_select_label_bg
                                        .unwrap_or(AnsiColor::Black.into()),
                                )
                                .set_foreground(
                                    colors
                                        .quick_select_label_fg
                                        .unwrap_or(AnsiColor::Olive.into()),
                                )
                                .set_reverse(false)
                                .set_intensity(Intensity::Bold);
                                line.set_cell(label_col, Cell::new(c, attr), SEQ_ZERO);
                            }
                        }
                        line.clear_appdata();
                    }
                    overlay_lines.push(line);
                }

                // Decorated clones cannot safely return renderer appdata to
                // delegate lines whose cells are different. Cross-frame shape
                // reuse requires a bounded overlay-owned cache keyed by source
                // fence and overlay revision; silently poisoning delegate
                // appdata here would produce incorrect glyph runs.
                let mut overlay_refs: Vec<&mut Line> = overlay_lines.iter_mut().collect();
                self.with_lines.with_lines_mut(first_row, &mut overlay_refs);
            }
        }

        let mut overlay = OverlayLines {
            with_lines,
            dims,
            search_row,
            renderer: &mut renderer,
        };
        if let Some(rules) = hyperlink_rules {
            self.delegate
                .with_lines_mut_and_apply_hyperlinks(lines, rules, &mut overlay);
        } else {
            self.delegate.with_lines_mut(lines, &mut overlay);
        }
    }
}

impl Pane for QuickSelectOverlay {
    fn pane_id(&self) -> PaneId {
        self.delegate.pane_id()
    }

    fn mux_registration_slot(&self) -> &Arc<mux::PaneRegistrationSlot> {
        self.delegate.mux_registration_slot()
    }

    fn get_title(&self) -> String {
        self.delegate.get_title()
    }

    fn send_paste(&self, _text: &str) -> anyhow::Result<()> {
        // Ignore
        Ok(())
    }

    fn reader(&self) -> anyhow::Result<Option<Box<dyn std::io::Read + Send>>> {
        Ok(None)
    }

    fn writer(&self) -> MappedMutexGuard<'_, dyn std::io::Write> {
        self.delegate.writer()
    }

    fn resize(&self, size: TerminalSize) -> anyhow::Result<()> {
        self.delegate.resize(size)
    }

    fn key_up(&self, _key: KeyCode, _mods: KeyModifiers) -> anyhow::Result<()> {
        Ok(())
    }

    fn key_down(&self, key: KeyCode, mods: KeyModifiers) -> anyhow::Result<()> {
        let mods = mods.remove_positional_mods();
        if !matches!((key, mods), (KeyCode::Escape, KeyModifiers::NONE)) {
            // Close the acceptance-to-action race: a label may only act on the
            // source range whose fence authorized it. A detected change clears
            // the maps and starts one bounded refresh before dispatch below.
            self.renderer.lock().prepare_for_render();
        }
        match (key, mods) {
            (KeyCode::Escape, KeyModifiers::NONE) => self.renderer.lock().close(),
            (KeyCode::UpArrow, KeyModifiers::NONE)
            | (KeyCode::Enter, KeyModifiers::NONE)
            | (KeyCode::Char('p'), KeyModifiers::CTRL) => {
                // Move to prior match
                let mut r = self.renderer.lock();
                if let Some(cur) = r.result_pos.as_ref() {
                    let prior = if *cur > 0 {
                        cur.saturating_sub(1)
                    } else {
                        r.results.len().saturating_sub(1)
                    };
                    r.activate_match_number(prior);
                }
            }
            (KeyCode::PageUp, KeyModifiers::NONE) => {
                // Skip this page of matches and move up to the first match from
                // the prior page.
                let dims = self.delegate.get_dimensions();
                let mut r = self.renderer.lock();
                if let Some(cur) = r.result_pos {
                    let top = r.viewport.unwrap_or(dims.physical_top);
                    let Some(prior) = checked_page_up_boundary(top, dims.viewport_rows) else {
                        return Ok(());
                    };
                    if let Some(pos) = r
                        .results
                        .iter()
                        .position(|res| res.start_y > prior && res.start_y < top)
                    {
                        r.activate_match_number(pos);
                    } else {
                        r.activate_match_number(cur.saturating_sub(1));
                    }
                }
            }
            (KeyCode::PageDown, KeyModifiers::NONE) => {
                // Skip this page of matches and move down to the first match from
                // the next page.
                let dims = self.delegate.get_dimensions();
                let mut r = self.renderer.lock();
                if let Some(cur) = r.result_pos {
                    let top = r.viewport.unwrap_or(dims.physical_top);
                    let Some(bottom) = checked_page_down_boundary(top, dims.viewport_rows) else {
                        return Ok(());
                    };
                    if let Some(pos) = r.results.iter().position(|res| res.start_y >= bottom) {
                        r.activate_match_number(pos);
                    } else {
                        let len = r.results.len().saturating_sub(1);
                        r.activate_match_number(cur.min(len));
                    }
                }
            }
            (KeyCode::DownArrow, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CTRL) => {
                // Move to next match
                let mut r = self.renderer.lock();
                if let Some(cur) = r.result_pos.as_ref() {
                    let next = if cur
                        .checked_add(1)
                        .is_none_or(|next| next >= r.results.len())
                    {
                        0
                    } else {
                        cur.saturating_add(1)
                    };
                    r.activate_match_number(next);
                }
            }
            (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
                // Type to add to the selection
                let mut r = self.renderer.lock();
                // Labels are authoritative only after the current search has
                // installed. Never reinterpret input typed against an old or
                // not-yet-visible label generation.
                if r.searching.is_none() && !r.by_label.is_empty() && !r.action_pending {
                    r.selection.push(c);
                    r.dispatch_complete_selection();
                }
                r.mark_search_ui_dirty();
            }
            (KeyCode::Backspace, KeyModifiers::NONE) => {
                // Backspace to edit the selection
                let mut r = self.renderer.lock();
                r.selection.pop();
                r.mark_search_ui_dirty();
            }
            (KeyCode::Char('u'), KeyModifiers::CTRL) => {
                // CTRL-u to clear the selection
                let mut r = self.renderer.lock();
                r.selection.clear();
                r.mark_search_ui_dirty();
            }
            _ => {}
        }
        Ok(())
    }

    fn mouse_event(&self, event: MouseEvent) -> anyhow::Result<()> {
        self.delegate.mouse_event(event)
    }

    fn perform_actions(&self, actions: Vec<termwiz::escape::Action>) {
        self.delegate.perform_actions(actions)
    }

    fn is_dead(&self) -> bool {
        self.delegate.is_dead()
    }

    fn palette(&self) -> ColorPalette {
        self.delegate.palette()
    }
    fn domain_id(&self) -> DomainId {
        self.delegate.domain_id()
    }

    fn erase_scrollback(&self, erase_mode: ScrollbackEraseMode) {
        self.delegate.erase_scrollback(erase_mode)
    }

    fn is_mouse_grabbed(&self) -> bool {
        // Force grabbing off while we're searching
        false
    }

    fn is_alt_screen_active(&self) -> bool {
        false
    }

    fn set_clipboard(&self, clipboard: &Arc<dyn Clipboard>) {
        self.delegate.set_clipboard(clipboard)
    }

    fn get_current_working_dir(&self, policy: CachePolicy) -> Option<Url> {
        self.delegate.get_current_working_dir(policy)
    }

    fn get_cursor_position(&self) -> StableCursorPosition {
        // move to the search box
        let renderer = self.renderer.lock();
        StableCursorPosition {
            x: 8usize.saturating_add(wezterm_term::unicode_column_width(
                &renderer.selection,
                None,
            )),
            y: renderer.compute_search_row(),
            shape: termwiz::surface::CursorShape::SteadyBlock,
            visibility: termwiz::surface::CursorVisibility::Visible,
        }
    }

    fn get_current_seqno(&self) -> SequenceNo {
        self.delegate.get_current_seqno()
    }

    fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let dirty = self.delegate.get_changed_since(lines.clone(), seqno);
        merge_dirty_results(lines, dirty, &self.renderer.lock().dirty_results)
    }

    fn get_changed_since_with_source_fence(
        &self,
        lines: Range<StableRowIndex>,
        last_observed_source_end: SequenceNo,
    ) -> (SequenceNo, RangeSet<StableRowIndex>) {
        let (source_end, dirty) = self.delegate.get_changed_since_with_source_fence(
            lines.clone(),
            last_observed_source_end,
        );
        let mut renderer = self.renderer.lock();
        let retained = retained_row_range(renderer.delegate.get_dimensions());
        prune_dirty_results(&mut renderer.dirty_results, retained);
        (
            source_end,
            take_dirty_results(lines, dirty, &mut renderer.dirty_results),
        )
    }

    fn for_each_logical_line_in_stable_range_mut(
        &self,
        lines: Range<StableRowIndex>,
        for_line: &mut dyn ForEachPaneLogicalLine,
    ) {
        self.delegate
            .for_each_logical_line_in_stable_range_mut(lines, for_line);
    }

    fn get_logical_lines(&self, lines: Range<StableRowIndex>) -> Vec<LogicalLine> {
        self.delegate.get_logical_lines(lines)
    }

    fn with_lines_mut(&self, lines: Range<StableRowIndex>, with_lines: &mut dyn WithPaneLines) {
        self.with_decorated_lines(lines, None, with_lines);
    }

    fn with_lines_mut_and_apply_hyperlinks(
        &self,
        lines: Range<StableRowIndex>,
        rules: &[termwiz::hyperlink::Rule],
        with_lines: &mut dyn WithPaneLines,
    ) {
        self.with_decorated_lines(lines, Some(rules), with_lines);
    }

    fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut renderer = self.renderer.lock();
        renderer.prepare_for_render();
        let dims = self.get_dimensions();

        let (top, mut lines) = self.delegate.get_lines(lines);
        let colors = renderer.config.resolved_palette.clone();
        let disable_attr = renderer.config.quick_select_remove_styling;

        // Process the lines; for the search row we want to render instead
        // the search UI.
        // For rows with search results, we want to highlight the matching ranges
        let search_row = renderer.compute_search_row();
        for (idx, line) in lines.iter_mut().enumerate() {
            if disable_attr {
                line.cells_mut_for_attr_changes_only()
                    .iter_mut()
                    .for_each(|cell| cell.attrs_mut().clear());
            }
            let Some(stable_idx) = StableRowIndex::try_from(idx)
                .ok()
                .and_then(|offset| top.checked_add(offset))
            else {
                break;
            };
            if stable_idx == search_row {
                // Replace with search UI
                let rev = CellAttributes::default().set_reverse(true).clone();
                line.fill_range(0..dims.cols, &Cell::new(' ', rev.clone()), SEQ_ZERO);
                line.overlay_text_with_attribute(
                    0,
                    &format!(
                        "Select: {}  (type highlighted prefix to {}, uppercase pastes, ESC to cancel)",
                        renderer.selection,
                        if renderer.args.label.is_empty() {
                            "copy"
                        } else {
                            &renderer.args.label
                        },
                    ),
                    rev,
                    SEQ_ZERO,
                );
                renderer.last_bar_pos = Some(search_row);
            } else if let Some(matches) = renderer.by_line.get(&stable_idx) {
                for m in matches {
                    // highlight
                    for cell_idx in m.range.clone() {
                        if let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(cell_idx)
                        {
                            cell.attrs_mut()
                                .set_background(
                                    colors
                                        .quick_select_match_bg
                                        .unwrap_or(AnsiColor::Black.into()),
                                )
                                .set_foreground(
                                    colors
                                        .quick_select_match_fg
                                        .unwrap_or(AnsiColor::Green.into()),
                                )
                                .set_reverse(false)
                                .set_intensity(Intensity::Bold);
                        }
                    }
                    for (idx, c) in m.label.chars().enumerate() {
                        let Some(label_col) = m.range.start.checked_add(idx) else {
                            break;
                        };
                        if label_col >= dims.cols || label_col >= line.len() {
                            break;
                        }
                        let mut attr = line
                            .get_cell(label_col)
                            .map(|cell| cell.attrs().clone())
                            .unwrap_or_else(|| CellAttributes::default());
                        attr.set_background(
                            colors
                                .quick_select_label_bg
                                .unwrap_or(AnsiColor::Black.into()),
                        )
                        .set_foreground(
                            colors
                                .quick_select_label_fg
                                .unwrap_or(AnsiColor::Olive.into()),
                        )
                        .set_reverse(false)
                        .set_intensity(Intensity::Bold);
                        line.set_cell(label_col, Cell::new(c, attr), SEQ_ZERO);
                    }
                }
            }
        }

        (top, lines)
    }

    fn get_dimensions(&self) -> RenderableDimensions {
        self.delegate.get_dimensions()
    }
}

impl QuickSelectRenderable {
    fn compute_search_row(&self) -> StableRowIndex {
        compute_search_row_from_viewport(self.viewport, self.delegate.get_dimensions())
    }

    fn close(&self) {
        let pane_id = self.delegate.pane_id();
        let instance_token = Arc::clone(&self.instance_token);
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                close_quick_select_overlay_if_current(term_window, pane_id, &instance_token);
            })));
    }

    fn set_viewport(&self, row: Option<StableRowIndex>) {
        let dims = self.delegate.get_dimensions();
        let pane_id = self.delegate.pane_id();
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.set_viewport(pane_id, row, dims);
            })));
    }

    fn update_dimensions(&mut self) -> bool {
        let dims = self.delegate.get_dimensions();
        if dims.cols == self.width && dims.viewport_rows == self.height {
            return false;
        }

        self.width = dims.cols;
        self.height = dims.viewport_rows;
        true
    }

    fn mark_search_ui_dirty(&mut self) {
        if let Some(last) = self.last_bar_pos {
            self.dirty_results.add(last);
        }
        self.dirty_results.add(self.compute_search_row());
        let retained = retained_row_range(self.delegate.get_dimensions());
        prune_dirty_results(&mut self.dirty_results, retained);
        self.window.invalidate();
    }

    fn cancel_search_task(&mut self) {
        if let Some(cancel) = self.search_preparation_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        if let Some(abort) = self.search_abort.take() {
            abort.abort();
        }
    }

    fn cancel_retry(&mut self) {
        if let Some(abort) = self.retry_abort.take() {
            abort.abort();
        }
        self.retry_token.take();
    }

    fn prepare_for_render(&mut self) {
        let resized = self.update_dimensions();
        let source_changed = if resized || self.searching.is_some() {
            false
        } else if let (Some(source_end), Some(range)) =
            (self.accepted_source_end, self.accepted_range.clone())
        {
            let (next_source_end, dirty) = self
                .delegate
                .get_changed_since_with_source_fence(range, source_end);
            if next_source_end >= source_end && dirty.iter().next().is_none() {
                self.accepted_source_end = Some(next_source_end);
                false
            } else {
                true
            }
        } else {
            false
        };
        if resized || source_changed {
            self.restart_search(false, true, 0);
        }
    }

    fn install_prepared_results(&mut self, prepared: PreparedQuickSelectResults) {
        for row in self.by_line.keys() {
            self.dirty_results.add(*row);
        }
        self.dirty_results.add_set(&prepared.dirty_rows);
        self.results = prepared.results;
        self.by_line = prepared.by_line;
        self.by_label = prepared.by_label;
    }

    fn update_search(&mut self, is_initial_run: bool) {
        self.restart_search(is_initial_run, false, 0);
    }

    fn restart_search(
        &mut self,
        is_initial_run: bool,
        preserve_result: bool,
        retry_attempt: u8,
    ) {
        self.cancel_search_task();
        self.cancel_retry();
        // Label prefixes are meaningful only for one installed result map.
        // A restart may reassign every label, so retaining or replaying input
        // across generations could act on a target the user never saw.
        self.selection.clear();
        self.action_pending = false;
        if preserve_result && self.desired_result.is_none() {
            self.desired_result_ordinal = self.result_pos;
            self.desired_result = self
                .result_pos
                .and_then(|position| self.results.get(position))
                .map(search_result_anchor);
        } else if !preserve_result {
            self.desired_result.take();
            self.desired_result_ordinal.take();
        }
        let bar_pos = self.compute_search_row();
        let dirty_rows =
            dirty_rows_for_search_refresh(self.by_line.keys().copied(), self.last_bar_pos, bar_pos);
        self.dirty_results.add_set(&dirty_rows);

        self.results.clear();
        self.by_line.clear();
        self.by_label.clear();
        self.result_pos.take();
        self.searching.take();
        self.accepted_source_end.take();
        self.accepted_range.take();
        self.accepted_run_id.take();

        let dims = self.delegate.get_dimensions();
        self.width = dims.cols;
        self.height = dims.viewport_rows;

        if !self.pattern.is_empty() {
            let pane: Arc<dyn Pane> = self.delegate.clone();
            let window = self.window.clone();
            let pattern = self.pattern.clone();
            let requested_scope = self
                .args
                .scope_lines
                .unwrap_or(1000)
                .max(dims.viewport_rows);
            let Some(scope) = bounded_scope_rows(dims.viewport_rows, requested_scope) else {
                log::warn!(
                    "quick-select viewport exceeds bounded search envelope: viewport_rows={}, max_rows={MAX_QUICK_SELECT_TOTAL_SEARCH_ROWS}",
                    dims.viewport_rows
                );
                self.clear_selection();
                self.mark_search_ui_dirty();
                return;
            };
            let top = self.viewport.unwrap_or(dims.physical_top);
            let Some(requested_range) = checked_search_range(top, dims.viewport_rows, scope) else {
                log::warn!(
                    "quick-select search range is not representable: top={top}, viewport_rows={}, scope={scope}",
                    dims.viewport_rows
                );
                self.clear_selection();
                self.mark_search_ui_dirty();
                return;
            };
            let Some(range) = retained_row_range(dims)
                .and_then(|retained| intersect_row_ranges(requested_range, retained))
            else {
                self.clear_selection();
                self.mark_search_ui_dirty();
                return;
            };
            let Some(run_id) = allocate_search_run_id(&mut self.next_search_run_id) else {
                self.clear_selection();
                self.mark_search_ui_dirty();
                return;
            };
            let run = search_run_identity(run_id, pane.get_current_seqno(), dims, top);
            self.searching.replace(PendingSearch {
                run,
                range: range.clone(),
                retry_attempt,
                is_initial_run,
            });
            let instance_token = Arc::clone(&self.instance_token);
            let label_alphabet = if self.args.alphabet.is_empty() {
                self.config.quick_select_alphabet.clone()
            } else {
                self.args.alphabet.clone()
            };
            let preparation_cancel = Arc::new(AtomicBool::new(false));
            self.search_preparation_cancel = Some(Arc::clone(&preparation_cancel));
            let preparation_gate = Arc::clone(&self.search_preparation_gate);
            let (abort, registration) = AbortHandle::new_pair();
            self.search_abort = Some(abort);
            promise::spawn::spawn(async move {
                let limit = Some(SEARCH_RESULT_REQUEST_LIMIT);
                let preparation_range = range.clone();
                let completion = Abortable::new(
                    async {
                        let results = pane
                            .search(pattern, range.clone(), limit)
                            .await
                            .map_err(|err| format!("{err:#}"))?;
                        prepare_quick_select_results_off_thread(
                            results,
                            preparation_range,
                            run.cols,
                            label_alphabet,
                            preparation_cancel,
                            preparation_gate,
                        )
                        .await
                    },
                    registration,
                )
                .await;
                let outcome = match completion {
                    Err(_) => return anyhow::Result::<()>::Ok(()),
                    Ok(outcome) => outcome,
                };

                let pane_id = pane.pane_id();
                let mut outcome = Some(outcome);
                window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                    let state = term_window.pane_state(pane_id);
                    if let Some(overlay) = state.overlay.as_ref() {
                        if let Some(search_overlay) =
                            overlay.pane.downcast_ref::<QuickSelectOverlay>()
                        {
                            let mut r = search_overlay.renderer.lock();
                            if !Arc::ptr_eq(&r.instance_token, &instance_token) {
                                return;
                            }
                            let Some(outcome) = outcome.take() else {
                                log::warn!(
                                    "quick-select search completion already consumed for pane {pane_id}"
                                );
                                return;
                            };
                            let Some(pending) = r.searching.as_ref() else {
                                return;
                            };
                            if pending.run != run || pending.range != range {
                                return;
                            }
                            let retry_attempt = pending.retry_attempt;
                            let pending_initial_run = pending.is_initial_run;
                            let prepared_results = match outcome {
                                Ok(results) => results,
                                Err(error) => {
                                    r.search_abort.take();
                                    r.search_preparation_cancel.take();
                                    if error.starts_with("quick-select preparation ") {
                                        log::debug!(
                                            "quick-select preparation deferred for {range:?}: {error}"
                                        );
                                    } else {
                                        log::warn!(
                                            "quick-select search failed for {range:?}: {error}"
                                        );
                                    }
                                    r.schedule_search_retry(
                                        run,
                                        retry_attempt,
                                        pending_initial_run,
                                    );
                                    return;
                                }
                            };
                            let dims = r.delegate.get_dimensions();
                            let (source_end, source_dirty) = r
                                .delegate
                                .get_changed_since_with_source_fence(
                                    range.clone(),
                                    run.source_seqno,
                                );
                            let current_source = search_run_identity(
                                run.id,
                                source_end,
                                dims,
                                r.viewport.unwrap_or(dims.physical_top),
                            );
                            match classify_search_completion(
                                r.searching.as_ref(),
                                run,
                                &range,
                                current_source,
                                source_end < run.source_seqno
                                    || source_dirty.iter().next().is_some(),
                            ) {
                                SearchCompletionStatus::Accepted => {}
                                SearchCompletionStatus::Superseded => return,
                                SearchCompletionStatus::SourceChanged => {
                                    r.search_abort.take();
                                    r.schedule_search_retry(
                                        run,
                                        retry_attempt,
                                        pending_initial_run,
                                    );
                                    return;
                                }
                            }
                            r.search_abort.take();
                            r.search_preparation_cancel.take();
                            r.searching.take();
                            r.accepted_source_end = Some(source_end);
                            r.accepted_range = Some(range.clone());
                            r.accepted_run_id = Some(run.id);
                            r.install_prepared_results(prepared_results);
                            let num_results = r.results.len();

                            if !r.results.is_empty() {
                                if let Some(desired) = r.desired_result.take() {
                                    if let Some(position) = r
                                        .results
                                        .iter()
                                        .position(|result| search_result_anchor(result) == desired)
                                    {
                                        r.desired_result_ordinal.take();
                                        r.result_pos = Some(position);
                                    } else if let Some(ordinal) =
                                        r.desired_result_ordinal.take()
                                    {
                                        r.activate_match_number(
                                            ordinal.min(num_results.saturating_sub(1)),
                                        );
                                    } else if !pending_initial_run {
                                        r.activate_match_number(num_results.saturating_sub(1));
                                    }
                                } else {
                                    match &r.viewport {
                                        Some(y) if pending_initial_run => {
                                            r.result_pos = r
                                                .results
                                                .iter()
                                                .position(|result| result.start_y >= *y);
                                        }
                                        _ => {
                                            r.activate_match_number(
                                                num_results.saturating_sub(1),
                                            );
                                        }
                                    }
                                }
                            } else {
                                r.desired_result.take();
                                r.desired_result_ordinal.take();
                                if !pending_initial_run {
                                    r.set_viewport(None);
                                }
                                r.clear_selection();
                            }
                            r.mark_search_ui_dirty();
                        }
                    }
                })));
                anyhow::Result::<()>::Ok(())
            })
            .detach();
        } else {
            if !is_initial_run {
                self.set_viewport(None);
            }
            self.clear_selection();
        }
        self.mark_search_ui_dirty();
    }

    fn schedule_search_retry(
        &mut self,
        run: SearchRunIdentity,
        retry_attempt: u8,
        is_initial_run: bool,
    ) {
        self.cancel_search_task();
        self.cancel_retry();
        let delay = search_retry_delay(retry_attempt);
        let next_attempt = retry_attempt.saturating_add(1);
        let pane_id = self.delegate.pane_id();
        let window = self.window.clone();
        let instance_token = Arc::clone(&self.instance_token);
        let retry_token = Arc::new(());
        self.retry_token = Some(Arc::clone(&retry_token));
        let (abort, registration) = AbortHandle::new_pair();
        self.retry_abort = Some(abort);
        self.mark_search_ui_dirty();
        promise::spawn::spawn(async move {
            if Abortable::new(sleep(delay), registration).await.is_err() {
                return anyhow::Result::<()>::Ok(());
            }
            window.notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                let state = term_window.pane_state(pane_id);
                if let Some(overlay) = state.overlay.as_ref() {
                    if let Some(search_overlay) =
                        overlay.pane.downcast_ref::<QuickSelectOverlay>()
                    {
                        let mut renderer = search_overlay.renderer.lock();
                        if Arc::ptr_eq(&renderer.instance_token, &instance_token)
                            && renderer.searching.as_ref().is_some_and(|pending| pending.run == run)
                            && renderer.retry_token
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &retry_token))
                        {
                            renderer.retry_abort.take();
                            renderer.retry_token.take();
                            renderer.restart_search(is_initial_run, true, next_attempt);
                        }
                    }
                }
            })));
            anyhow::Result::<()>::Ok(())
        })
        .detach();
    }

    fn clear_selection(&mut self) {
        let pane = Arc::clone(&self.delegate);
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                term_window.clear_selection(&pane);
            })));
    }

    fn dispatch_complete_selection(&mut self) {
        if self.action_pending {
            return;
        }
        let lowered = self.selection.to_lowercase();
        let paste = lowered != self.selection;
        let Some(result_index) = self.by_label.get(&lowered).copied() else {
            return;
        };
        self.action_pending = true;
        self.select_and_copy_match_number(result_index, paste);
    }

    fn select_and_copy_match_number(&mut self, n: usize, paste: bool) {
        let Some(result) = self.results.get(n).cloned() else {
            return;
        };
        let (Some(source_end), Some(source_range)) =
            (self.accepted_source_end, self.accepted_range.clone())
        else {
            return;
        };
        let Some(accepted_run_id) = self.accepted_run_id else {
            return;
        };

        let pane_id = self.delegate.pane_id();
        let pane = Arc::clone(&self.delegate);
        let accepted_cols = self.width;
        let instance_token = Arc::clone(&self.instance_token);
        let action = self.args.action.clone();
        let skip_action_on_paste = self.args.skip_action_on_paste;
        self.window
            .notify(TermWindowNotif::Apply(Box::new(move |term_window| {
                if !quick_select_action_is_current(
                    term_window,
                    pane_id,
                    &instance_token,
                    accepted_run_id,
                ) {
                    return;
                }
                if pane.is_dead() {
                    log::debug!("quick-select pane {pane_id} closed before action");
                    close_quick_select_overlay_if_current(
                        term_window,
                        pane_id,
                        &instance_token,
                    );
                    return;
                }
                // The key handler fenced the accepted result before looking
                // up its label. Fence the exact captured pane once more at
                // execution time; resolving by numeric PaneId here would
                // reintroduce an ABA window if a pane were replaced.
                let dims = pane.get_dimensions();
                let chunk_is_retained = retained_row_range(dims).as_ref().is_some_and(|retained| {
                    retained.start <= source_range.start && source_range.end <= retained.end
                });
                let inclusive_end = inclusive_search_result_end(&result, accepted_cols);
                let geometry_changed = dims.cols != accepted_cols
                    || !chunk_is_retained
                    || inclusive_end.is_none();
                let validated_source_end = if geometry_changed {
                    None
                } else {
                    let (next_source_end, dirty) = pane
                        .get_changed_since_with_source_fence(source_range.clone(), source_end);
                    (next_source_end >= source_end && dirty.iter().next().is_none())
                        .then_some(next_source_end)
                };
                let Some(validated_source_end) = validated_source_end else {
                    log::debug!("quick-select result for pane {pane_id} changed before action");
                    restart_quick_select_after_stale_action(
                        term_window,
                        pane_id,
                        &instance_token,
                    );
                    return;
                };
                let Some((inclusive_end_x, inclusive_end_y)) = inclusive_end else {
                    return;
                };
                let start = SelectionCoordinate::x_y(result.start_x, result.start_y);
                term_window.update_selection(&pane, |selection| {
                    selection.origin = Some(start);
                    selection.range = Some(SelectionRange {
                        start,
                        // inclusive range for selection, but the result
                        // range is exclusive
                        end: SelectionCoordinate::x_y(inclusive_end_x, inclusive_end_y),
                    });
                    selection.rectangular = false;
                });

                let text = term_window.selection_text(&pane);
                let extracted_dims = pane.get_dimensions();
                let extracted_range_is_retained = retained_row_range(extracted_dims)
                    .as_ref()
                    .is_some_and(|retained| {
                        retained.start <= source_range.start && source_range.end <= retained.end
                    });
                let (extracted_source_end, dirty) = pane
                    .get_changed_since_with_source_fence(source_range, validated_source_end);
                if pane.is_dead()
                    || extracted_dims.cols != accepted_cols
                    || !extracted_range_is_retained
                    || extracted_source_end < validated_source_end
                    || dirty.iter().next().is_some()
                {
                    log::debug!(
                        "quick-select result for pane {pane_id} changed or was trimmed during extraction"
                    );
                    restart_quick_select_after_stale_action(
                        term_window,
                        pane_id,
                        &instance_token,
                    );
                    return;
                }
                if !text.is_empty() {
                    if paste {
                        let _ = pane.send_paste(&text);
                    }
                    if let Some(action) = action {
                        if !paste || !skip_action_on_paste {
                            let _ = term_window.perform_key_assignment(&pane, &action);
                        }
                    } else {
                        term_window.copy_to_clipboard(
                            ClipboardCopyDestination::ClipboardAndPrimarySelection,
                            text,
                        );
                    }
                }
                close_quick_select_overlay_if_current(
                    term_window,
                    pane_id,
                    &instance_token,
                );
            })));
    }

    fn activate_match_number(&mut self, n: usize) {
        if let Some(result) = self.results.get(n) {
            self.result_pos.replace(n);
            let start_y = result.start_y;
            self.set_viewport(Some(start_y));
        }
    }
}
