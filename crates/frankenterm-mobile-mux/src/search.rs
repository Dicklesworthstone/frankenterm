//! Searching a pane's scrollback where it lives, on the mux server, newest
//! rows first.
//!
//! The server walks a requested row range oldest first and stops at the
//! limit, so one request over the whole scrollback would report the oldest
//! matches. A search therefore asks for blocks of rows walking back from the
//! newest, stopping once it has enough matches, and then fetches the text of
//! the matched rows.

use crate::wire::{Pattern, SearchResult, StableRowIndex};
use serde::Serialize;
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
/// cells), with the text of the rows it spans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SearchMatch {
    pub row: StableRowIndex,
    pub start_column: usize,
    pub end_row: StableRowIndex,
    pub end_column: usize,
    pub text: String,
}

/// One row of a pane, as text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ContextLine {
    pub row: StableRowIndex,
    pub text: String,
}

/// Rows one search request covers.
pub const BLOCK_ROWS: StableRowIndex = 5000;
/// Blocks one search walks back through at most.
pub const BLOCKS: u32 = 10;
/// The newest matches one pane reports.
pub const WANTED: usize = 200;
/// Rows of one match whose text is fetched.
const MATCH_ROWS: StableRowIndex = 4;

#[derive(Debug)]
pub(crate) struct SearchJob {
    pub pane_id: usize,
    pub pattern: Pattern,
    next_end: StableRowIndex,
    blocks_left: u32,
    found: Vec<SearchResult>,
    pub searched_rows: usize,
}

impl SearchJob {
    /// A search of the rows before `end` (one past the newest row).
    pub fn new(pane_id: usize, pattern: Pattern, end: StableRowIndex) -> Self {
        Self {
            pane_id,
            pattern,
            next_end: end.max(0),
            blocks_left: BLOCKS,
            found: Vec::new(),
            searched_rows: 0,
        }
    }

    /// The next block to ask for, newest first; None once enough matches
    /// are in or the walk reached the pane's first row.
    pub fn next_block(&mut self) -> Option<Range<StableRowIndex>> {
        if self.blocks_left == 0 || self.found.len() >= WANTED || self.next_end <= 0 {
            return None;
        }
        let end = self.next_end;
        let start = end.saturating_sub(BLOCK_ROWS).max(0);
        self.next_end = start;
        self.blocks_left -= 1;
        self.searched_rows += usize::try_from(end - start).unwrap_or(0);
        Some(start..end)
    }

    pub fn add(&mut self, results: Vec<SearchResult>) {
        self.found.extend(results);
    }

    /// The newest `WANTED` matches, newest first.
    pub fn best(&self) -> Vec<SearchResult> {
        let mut best = self.found.clone();
        best.sort_by_key(|hit| std::cmp::Reverse((hit.start_y, hit.start_x)));
        best.dedup();
        best.truncate(WANTED);
        best
    }

    /// The rows whose text the best matches need, as GetLines ranges.
    pub fn rows(best: &[SearchResult]) -> Vec<Range<StableRowIndex>> {
        let mut rows: Vec<StableRowIndex> = best
            .iter()
            .flat_map(|hit| hit.start_y..=hit.end_y.min(hit.start_y + MATCH_ROWS - 1))
            .collect();
        rows.sort_unstable();
        rows.dedup();
        let mut ranges: Vec<Range<StableRowIndex>> = Vec::new();
        for row in rows {
            match ranges.last_mut() {
                Some(last) if last.end == row => last.end = row + 1,
                _ => ranges.push(row..row + 1),
            }
        }
        ranges
    }

    /// Each match with the text of its rows (`text` maps row to text).
    pub fn matches(
        best: &[SearchResult],
        text: &impl Fn(StableRowIndex) -> Option<String>,
    ) -> Vec<SearchMatch> {
        best.iter()
            .map(|hit| {
                let last = hit.end_y.min(hit.start_y + MATCH_ROWS - 1);
                let rows: Vec<String> = (hit.start_y..=last).filter_map(text).collect();
                SearchMatch {
                    row: hit.start_y,
                    start_column: hit.start_x,
                    end_row: hit.end_y,
                    end_column: hit.end_x,
                    text: rows.join("\n"),
                }
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

    #[test]
    fn blocks_walk_back_from_the_newest_row_to_the_first() {
        let mut job = SearchJob::new(1, Pattern::Regex("x".into()), 12_000);
        assert_eq!(job.next_block(), Some(7000..12_000));
        assert_eq!(job.next_block(), Some(2000..7000));
        assert_eq!(job.next_block(), Some(0..2000));
        assert_eq!(job.next_block(), None, "nothing is older than row 0");
        assert_eq!(job.searched_rows, 12_000);
    }

    #[test]
    fn enough_matches_stop_the_walk_and_the_newest_are_kept() {
        let mut job = SearchJob::new(1, Pattern::Regex("x".into()), 1_000_000);
        let first = job.next_block().unwrap();
        job.add(
            (0..WANTED as isize)
                .map(|n| hit(first.start + n, 0))
                .collect(),
        );
        assert_eq!(job.next_block(), None);
        let mut job = SearchJob::new(1, Pattern::Regex("x".into()), 100);
        job.next_block();
        job.add(vec![hit(5, 1), hit(90, 2), hit(40, 0), hit(90, 2)]);
        let best = job.best();
        assert_eq!(
            best.iter().map(|h| h.start_y).collect::<Vec<_>>(),
            [90, 40, 5]
        );
        assert_eq!(SearchJob::rows(&best), [5..6, 40..41, 90..91]);
        let text = |row: StableRowIndex| Some(format!("row {row}"));
        let matches = SearchJob::matches(&best, &text);
        assert_eq!(matches[0].text, "row 90");
        assert_eq!((matches[2].row, matches[2].start_column), (5, 1));
    }
}
