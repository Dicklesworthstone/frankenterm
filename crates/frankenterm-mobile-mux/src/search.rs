//! Searching a pane's scrollback where it lives, on the mux server, newest
//! rows first.
//!
//! The server walks a requested row range oldest first and stops at the
//! limit, so one request over the whole scrollback would report the oldest
//! matches. A search therefore asks for blocks of rows walking back from the
//! newest. A block that hits the limit may hold newer matches than the ones
//! returned, so it is split and its newer half searched first; blocks are
//! sized from the matches seen so far. The walk stops once the rows searched
//! to the end hold enough matches, and then fetches the text of the matched
//! rows.

use crate::session::RowText;
use crate::wire::{Pattern, SearchResult, StableRowIndex};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::ops::Range;

/// What to look for in a pane's rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub text: String,
    pub case_sensitive: bool,
    /// `text` is a regular expression (the server's regex dialect).
    pub regex: bool,
}

impl SearchQuery {
    pub(crate) fn pattern(&self) -> Pattern {
        if self.regex {
            Pattern::Regex(self.text.clone())
        } else if self.case_sensitive {
            Pattern::CaseSensitiveString(self.text.clone())
        } else {
            Pattern::CaseInSensitiveString(self.text.clone())
        }
    }
}

/// One match, from `start_column` of `row` to `end_column` of `end_row` (in
/// cells), with the text of the rows it spans (at most four, joined by
/// newlines) and where in that text the match is (UTF-8 byte offsets, so
/// wide characters before it do not shift it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchMatch {
    pub row: StableRowIndex,
    pub start_column: usize,
    pub end_row: StableRowIndex,
    pub end_column: usize,
    pub text: String,
    pub text_start: usize,
    pub text_end: usize,
}

/// One row of a pane, as text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextLine {
    pub row: StableRowIndex,
    pub text: String,
}

/// The newest matches one pane reports, and the limit of one request.
pub const WANTED: usize = 200;
/// The most rows one search walks back through.
pub const MAX_ROWS: usize = 50_000;
/// The rows of the first block, and the bounds of later ones.
const FIRST_BLOCK: usize = 1000;
const MIN_BLOCK: usize = 100;
const MAX_BLOCK: usize = 20_000;
/// The most requests one search sends.
const MAX_REQUESTS: u32 = 64;
/// Rows of one match whose text is fetched.
const MATCH_ROWS: StableRowIndex = 4;

fn rows_between(start: StableRowIndex, end: StableRowIndex) -> usize {
    usize::try_from(end.saturating_sub(start)).unwrap_or(0)
}

fn match_key(hit: &SearchResult) -> (StableRowIndex, usize, StableRowIndex, usize) {
    (hit.start_y, hit.start_x, hit.end_y, hit.end_x)
}

/// Only representable, ordered coordinates can become line requests. The
/// exclusive end of a row at `MAX` is not representable, so it cannot be read.
/// A match may still end there after starting on an earlier, readable row.
fn text_rows(hit: &SearchResult) -> Option<Range<StableRowIndex>> {
    if hit.start_y < 0
        || hit.start_y == StableRowIndex::MAX
        || (hit.end_y, hit.end_x) < (hit.start_y, hit.start_x)
    {
        return None;
    }
    let end = hit
        .end_y
        .saturating_add(1)
        .min(hit.start_y.saturating_add(MATCH_ROWS));
    Some(hit.start_y..end)
}

#[derive(Debug)]
pub(crate) struct SearchJob {
    pub pane_id: usize,
    pub pattern: Pattern,
    /// One past the newest row the walk has not covered; `None` until the
    /// pane's end is known.
    next_end: Option<StableRowIndex>,
    /// Parts of a block still to search, the newest last.
    queue: Vec<Range<StableRowIndex>>,
    in_flight: Option<Range<StableRowIndex>>,
    rows_left: usize,
    block: usize,
    requests_left: u32,
    /// At most `WANTED` distinct matches, newest first. Split responses may
    /// repeat candidates, so they must not accumulate across the whole walk.
    found: Vec<SearchResult>,
    /// Matches in the rows searched to their end: always the newest rows.
    complete: usize,
    pub searched_rows: usize,
}

impl SearchJob {
    pub fn new(pane_id: usize, pattern: Pattern) -> Self {
        Self {
            pane_id,
            pattern,
            next_end: None,
            queue: Vec::new(),
            in_flight: None,
            rows_left: MAX_ROWS,
            block: FIRST_BLOCK,
            requests_left: MAX_REQUESTS,
            found: Vec::new(),
            complete: 0,
            searched_rows: 0,
        }
    }

    /// Searches the rows before `end`, one past the pane's newest row.
    pub fn start(&mut self, end: StableRowIndex) {
        self.next_end = Some(end.max(0));
    }

    /// The next rows to search, newest first; None once the newest rows
    /// hold enough matches, or the walk reached the pane's first row or its
    /// budget.
    pub fn next_request(&mut self) -> Option<Range<StableRowIndex>> {
        if self.complete >= WANTED || self.requests_left == 0 {
            return None;
        }
        let range = match self.queue.pop() {
            Some(range) => range,
            None => {
                let end = self.next_end?;
                if end <= 0 || self.rows_left == 0 {
                    return None;
                }
                let rows = self.block.min(self.rows_left);
                let start = end
                    .saturating_sub(StableRowIndex::try_from(rows).unwrap_or(StableRowIndex::MAX))
                    .max(0);
                self.next_end = Some(start);
                self.rows_left = self.rows_left.saturating_sub(rows_between(start, end));
                start..end
            }
        };
        self.requests_left -= 1;
        self.in_flight = Some(range.clone());
        Some(range)
    }

    /// The server's matches for the rows last asked for.
    pub fn answer(&mut self, results: Vec<SearchResult>) {
        let Some(range) = self.in_flight.take() else {
            return;
        };
        let rows = rows_between(range.start, range.end);
        // At the limit, the server stopped early: newer matches may follow
        // the ones it sent. Search the newer half first, then the older.
        let stopped = results.len() >= WANTED && rows > 1;
        let mut unique = BTreeSet::new();
        for hit in results {
            if text_rows(&hit).is_none()
                || hit.start_y >= range.end
                || hit.end_y < range.start
                || (hit.start_y < range.start && hit.end_y == range.start && hit.end_x == 0)
            {
                continue;
            }
            // Legacy servers can repeat a match spanning a wrapped line at
            // a request boundary. Preserve it, but count only unique starts
            // inside this block toward the completed newest-match budget.
            let key = match_key(&hit);
            if range.contains(&hit.start_y) && unique.len() < WANTED {
                unique.insert(key);
            }
            if let Err(index) = self
                .found
                .binary_search_by_key(&Reverse(key), |found| Reverse(match_key(found)))
                && index < WANTED
            {
                self.found.insert(index, hit);
                self.found.truncate(WANTED);
            }
        }
        let hits = unique.len();
        if stopped {
            let middle = range.start + (range.end - range.start) / 2;
            self.queue.push(range.start..middle);
            self.queue.push(middle..range.end);
            return;
        }
        self.complete += hits;
        self.searched_rows += rows;
        // The next block should hold about half a request's matches.
        self.block = if hits == 0 {
            self.block.saturating_mul(4).min(MAX_BLOCK)
        } else {
            (rows.saturating_mul(WANTED) / (2 * hits)).clamp(MIN_BLOCK, MAX_BLOCK)
        };
    }

    /// The newest `WANTED` matches, newest first, each once (the server
    /// searches a line that straddles two requests in both).
    pub fn best(&self) -> Vec<SearchResult> {
        self.found.clone()
    }

    /// The rows whose text the best matches need, as GetLines ranges.
    pub fn rows(best: &[SearchResult]) -> Vec<Range<StableRowIndex>> {
        let mut rows: Vec<StableRowIndex> = best.iter().filter_map(text_rows).flatten().collect();
        rows.sort_unstable();
        rows.dedup();
        let mut ranges: Vec<Range<StableRowIndex>> = Vec::new();
        for row in rows {
            match ranges.last_mut() {
                Some(last) if last.end == row => last.end = row.saturating_add(1),
                _ => ranges.push(row..row.saturating_add(1)),
            }
        }
        ranges
    }

    /// Each match with the text of its rows (`text` maps a row to it).
    pub fn matches<'a>(
        best: &[SearchResult],
        text: &impl Fn(StableRowIndex) -> Option<&'a RowText>,
    ) -> Vec<SearchMatch> {
        best.iter()
            .filter_map(|hit| {
                let rows = text_rows(hit)?;
                let mut joined = String::new();
                let (mut text_start, mut text_end) = (None, None);
                for row in rows {
                    let Some(line) = text(row) else { continue };
                    if !joined.is_empty() {
                        joined.push('\n');
                    }
                    if row == hit.start_y {
                        text_start = Some(joined.len() + line.offset(hit.start_x));
                    }
                    if row == hit.end_y {
                        text_end = Some(joined.len() + line.offset(hit.end_x));
                    }
                    joined.push_str(&line.text);
                }
                let text_start = text_start.unwrap_or(0).min(joined.len());
                let text_end = text_end
                    .unwrap_or(joined.len())
                    .clamp(text_start, joined.len());
                Some(SearchMatch {
                    row: hit.start_y,
                    start_column: hit.start_x,
                    end_row: hit.end_y,
                    end_column: hit.end_x,
                    text: joined,
                    text_start,
                    text_end,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(row: StableRowIndex, column: usize) -> SearchResult {
        SearchResult {
            start_y: row,
            start_x: column,
            end_y: row,
            end_x: column + 3,
            match_id: 0,
        }
    }

    fn walk(end: StableRowIndex) -> SearchJob {
        let mut job = SearchJob::new(1, Pattern::Regex("x".into()));
        assert_eq!(
            job.next_request(),
            None,
            "nothing is asked before the end is known"
        );
        job.start(end);
        job
    }

    #[test]
    fn blocks_walk_back_from_the_newest_row_to_the_first_growing_while_empty() {
        let mut job = walk(10_000);
        assert_eq!(job.next_request(), Some(9000..10_000));
        job.answer(Vec::new());
        assert_eq!(job.next_request(), Some(5000..9000));
        job.answer(Vec::new());
        assert_eq!(job.next_request(), Some(0..5000));
        job.answer(Vec::new());
        assert_eq!(job.next_request(), None, "nothing is older than row 0");
        assert_eq!(job.searched_rows, 10_000);
    }

    #[test]
    fn a_block_at_the_limit_is_split_and_its_newest_matches_win() {
        // Rows 0..1000 hold a match on every row; the server returns the
        // oldest 200 of any range.
        let server = |range: Range<StableRowIndex>| -> Vec<SearchResult> {
            range.take(WANTED).map(|row| hit(row, 0)).collect()
        };
        let mut job = walk(1000);
        let mut asked = Vec::new();
        while let Some(range) = job.next_request() {
            asked.push(range.clone());
            job.answer(server(range));
        }
        assert_eq!(asked[0], 0..1000);
        assert_eq!(asked[1], 500..1000, "the newer half goes first");
        let best = job.best();
        assert_eq!(best.len(), WANTED);
        assert_eq!(best[0].start_y, 999, "the newest match leads");
        assert_eq!(best[WANTED - 1].start_y, 800);
        assert!(asked.len() < 12, "{asked:?}");
    }

    #[test]
    fn enough_matches_stop_the_walk_and_the_newest_are_kept_once() {
        let mut job = walk(1_000_000);
        let first = job.next_request().unwrap();
        job.answer(
            (0..WANTED as isize - 1)
                .map(|n| hit(first.start + n, 0))
                .collect(),
        );
        let second = job.next_request().unwrap();
        assert_eq!(second.end, first.start);
        job.answer(vec![hit(second.end - 1, 0)]);
        assert_eq!(job.next_request(), None);
        let mut job = walk(100);
        job.next_request();
        job.answer(vec![hit(5, 1), hit(90, 2), hit(40, 0), hit(90, 2)]);
        let best = job.best();
        assert_eq!(
            best.iter().map(|h| h.start_y).collect::<Vec<_>>(),
            [90, 40, 5]
        );
        assert_eq!(SearchJob::rows(&best), [5..6, 40..41, 90..91]);
        let rows: std::collections::HashMap<StableRowIndex, RowText> = [5, 40, 90]
            .map(|row| (row, RowText::plain(&format!("row {row}"))))
            .into_iter()
            .collect();
        let matches = SearchJob::matches(&best, &|row| rows.get(&row));
        assert_eq!(matches[0].text, "row 90");
        assert_eq!(
            &matches[0].text[matches[0].text_start..matches[0].text_end],
            "w 9"
        );
        assert_eq!((matches[2].row, matches[2].start_column), (5, 1));
    }

    #[test]
    fn repeated_coordinates_do_not_complete_the_search_early() {
        let mut job = walk(1_000_000);
        let first = job.next_request().unwrap();
        let repeated = hit(first.end - 1, 0);
        job.answer(
            (0..WANTED - 1)
                .map(|match_id| SearchResult {
                    match_id,
                    ..repeated
                })
                .collect(),
        );
        assert_eq!(job.complete, 1);
        assert_eq!(job.best(), [repeated]);
        let second = job.next_request().unwrap();
        job.answer(vec![hit(second.end - 1, 0)]);
        assert_eq!(job.complete, 2);
        assert!(job.next_request().is_some(), "198 distinct matches remain");
    }

    #[test]
    fn only_matches_overlapping_the_requested_rows_are_retained() {
        let mut job = walk(2000);
        assert_eq!(job.next_request(), Some(1000..2000));
        let straddling = SearchResult {
            start_y: 999,
            end_y: 1001,
            ..hit(999, 0)
        };
        let ends_before_range = SearchResult {
            end_y: 1000,
            end_x: 0,
            ..straddling
        };
        job.answer(vec![
            hit(1999, 0),
            straddling,
            ends_before_range,
            hit(998, 0),
            hit(2000, 0),
            hit(-1, 0),
            SearchResult {
                end_y: 1499,
                ..hit(1500, 0)
            },
            SearchResult {
                end_x: 1,
                ..hit(1500, 2)
            },
        ]);
        assert_eq!(job.best(), [hit(1999, 0), straddling]);
        assert_eq!(job.complete, 1, "a straddling start belongs to older rows");
        assert_eq!(SearchJob::rows(&job.best()), [999..1002, 1999..2000]);
    }

    #[test]
    fn split_responses_retain_only_the_newest_bounded_candidates() {
        let mut job = walk(50_000);
        let mut requests = 0;
        while let Some(range) = job.next_request() {
            requests += 1;
            job.answer(range.take(WANTED).map(|row| hit(row, 0)).collect());
            assert!(job.found.len() <= WANTED, "response {requests}");
        }
        assert!(requests > 1, "a full block must be subdivided");
        let best = job.best();
        assert_eq!(best.len(), WANTED);
        assert_eq!(best.first().unwrap().start_y, 49_999);
        assert_eq!(best.last().unwrap().start_y, 49_800);

        let mut one_row = walk(1);
        one_row.next_request();
        one_row.answer((0..WANTED * 10).map(|column| hit(0, column)).collect());
        assert_eq!(one_row.found.len(), WANTED);
        assert_eq!(one_row.complete, WANTED);
        assert_eq!(one_row.best()[0].start_x, WANTED * 10 - 1);
    }

    #[test]
    fn extreme_row_coordinates_never_overflow_line_requests_or_match_text() {
        let maximum = StableRowIndex::MAX;
        let malformed = [
            SearchResult {
                start_y: StableRowIndex::MIN,
                end_y: maximum,
                ..hit(0, 0)
            },
            hit(maximum, 0),
            SearchResult {
                end_y: 0,
                ..hit(1, 0)
            },
            SearchResult {
                end_x: 0,
                ..hit(1, 1)
            },
        ];
        assert!(SearchJob::rows(&malformed).is_empty());
        assert!(SearchJob::matches(&malformed, &|_| None).is_empty());

        let edge = SearchResult {
            start_y: maximum - 2,
            start_x: 0,
            end_y: maximum,
            end_x: 0,
            match_id: 0,
        };
        let mut job = walk(maximum);
        job.next_request();
        job.answer(vec![edge, hit(maximum, 0)]);
        assert_eq!(job.best(), [edge]);
        assert_eq!(SearchJob::rows(&job.best()), [maximum - 2..maximum]);
        let rows: std::collections::HashMap<_, _> = [
            (maximum - 2, RowText::plain("first")),
            (maximum - 1, RowText::plain("second")),
        ]
        .into();
        let found = SearchJob::matches(&job.best(), &|row| rows.get(&row));
        assert_eq!(found[0].text, "first\nsecond");
        assert_eq!((found[0].text_start, found[0].text_end), (0, 12));

        let long = SearchResult {
            end_y: maximum,
            ..hit(1, 0)
        };
        assert_eq!(SearchJob::rows(&[long]), [1..5]);
    }

    #[test]
    fn a_match_after_wide_characters_is_highlighted_where_it_is() {
        // "界界ab": the wide characters take columns 0-3, "a" is column 4.
        let row = RowText::wide(&[("界", 0), ("界", 2), ("a", 4), ("b", 5)]);
        let rows: std::collections::HashMap<StableRowIndex, RowText> = [(7, row)].into();
        let hit = SearchResult {
            start_y: 7,
            start_x: 4,
            end_y: 7,
            end_x: 6,
            match_id: 0,
        };
        let found = SearchJob::matches(&[hit], &|row| rows.get(&row));
        assert_eq!(&found[0].text[found[0].text_start..found[0].text_end], "ab");
    }
}
