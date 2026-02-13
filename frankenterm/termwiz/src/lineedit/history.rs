use std::borrow::Cow;
use std::collections::VecDeque;

/// Represents a position within the history.
/// Smaller numbers are assumed to be before larger numbers,
/// and the indices are assumed to be contiguous.
pub type HistoryIndex = usize;

/// Defines the history interface for the line editor.
pub trait History {
    /// Lookup the line corresponding to an index.
    fn get(&self, idx: HistoryIndex) -> Option<Cow<'_, str>>;
    /// Return the index for the most recently added entry.
    fn last(&self) -> Option<HistoryIndex>;
    /// Add an entry.
    /// Note that the LineEditor will not automatically call
    /// the add method.
    fn add(&mut self, line: &str);

    /// Search for a matching entry relative to the specified history index.
    fn search(
        &self,
        idx: HistoryIndex,
        style: SearchStyle,
        direction: SearchDirection,
        pattern: &str,
    ) -> Option<SearchResult<'_>>;
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct SearchResult<'a> {
    pub line: Cow<'a, str>,
    pub idx: HistoryIndex,
    pub cursor: usize,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SearchStyle {
    Substring,
}

impl SearchStyle {
    /// Matches pattern against line, returning the byte index of the
    /// first matching character
    pub fn match_against(&self, pattern: &str, line: &str) -> Option<usize> {
        match self {
            Self::Substring => line.find(pattern),
        }
    }
}

/// Encodes the direction the search should take, relative to the
/// current HistoryIndex.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SearchDirection {
    /// The search goes backwards towards the smaller HistoryIndex values
    /// at the beginning of history.
    Backwards,
    /// The search goes forwards towarrds the larger HistoryIndex values
    /// at the end of history.
    Forwards,
}

impl SearchDirection {
    /// Given a history index, compute the next value in the
    /// encoded search directory.
    /// Returns `None` if the search would overflow.
    pub fn next(self, idx: HistoryIndex) -> Option<HistoryIndex> {
        let (next, overflow) = match self {
            Self::Backwards => idx.overflowing_sub(1),
            Self::Forwards => idx.overflowing_add(1),
        };
        if overflow {
            None
        } else {
            Some(next)
        }
    }
}

/// A simple history implementation that holds entries in memory.
#[derive(Default)]
pub struct BasicHistory {
    entries: VecDeque<String>,
}

impl History for BasicHistory {
    fn get(&self, idx: HistoryIndex) -> Option<Cow<'_, str>> {
        self.entries.get(idx).map(|s| Cow::Borrowed(s.as_str()))
    }

    fn last(&self) -> Option<HistoryIndex> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.entries.len() - 1)
        }
    }

    fn add(&mut self, line: &str) {
        if self.entries.back().map(String::as_str) == Some(line) {
            // Ignore duplicates
            return;
        }
        self.entries.push_back(line.to_owned());
    }

    fn search(
        &self,
        idx: HistoryIndex,
        style: SearchStyle,
        direction: SearchDirection,
        pattern: &str,
    ) -> Option<SearchResult<'_>> {
        let mut idx = idx;

        loop {
            let line = match self.entries.get(idx) {
                Some(line) => line,
                None => return None,
            };

            if let Some(cursor) = style.match_against(pattern, line) {
                return Some(SearchResult {
                    line: Cow::Borrowed(line.as_str()),
                    idx,
                    cursor,
                });
            }

            idx = match direction.next(idx) {
                None => return None,
                Some(idx) => idx,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── SearchStyle ─────────────────────────────────────────

    #[test]
    fn search_style_substring_finds_match() {
        assert_eq!(SearchStyle::Substring.match_against("ll", "hello"), Some(2));
    }

    #[test]
    fn search_style_substring_no_match() {
        assert_eq!(SearchStyle::Substring.match_against("xyz", "hello"), None);
    }

    #[test]
    fn search_style_substring_at_start() {
        assert_eq!(
            SearchStyle::Substring.match_against("hel", "hello"),
            Some(0)
        );
    }

    #[test]
    fn search_style_substring_empty_pattern() {
        assert_eq!(SearchStyle::Substring.match_against("", "hello"), Some(0));
    }

    #[test]
    fn search_style_clone_and_eq() {
        let a = SearchStyle::Substring;
        let b = a;
        assert_eq!(a, b);
    }

    // ── SearchDirection ─────────────────────────────────────

    #[test]
    fn search_direction_backwards_decrements() {
        assert_eq!(SearchDirection::Backwards.next(5), Some(4));
        assert_eq!(SearchDirection::Backwards.next(1), Some(0));
    }

    #[test]
    fn search_direction_backwards_at_zero_is_none() {
        assert_eq!(SearchDirection::Backwards.next(0), None);
    }

    #[test]
    fn search_direction_forwards_increments() {
        assert_eq!(SearchDirection::Forwards.next(0), Some(1));
        assert_eq!(SearchDirection::Forwards.next(5), Some(6));
    }

    #[test]
    fn search_direction_forwards_at_max_is_none() {
        assert_eq!(SearchDirection::Forwards.next(usize::MAX), None);
    }

    #[test]
    fn search_direction_clone_and_eq() {
        let a = SearchDirection::Backwards;
        let b = a;
        assert_eq!(a, b);
        assert_ne!(SearchDirection::Backwards, SearchDirection::Forwards);
    }

    // ── BasicHistory ────────────────────────────────────────

    #[test]
    fn empty_history_last_is_none() {
        let hist = BasicHistory::default();
        assert_eq!(hist.last(), None);
    }

    #[test]
    fn empty_history_get_is_none() {
        let hist = BasicHistory::default();
        assert_eq!(hist.get(0), None);
    }

    #[test]
    fn add_and_get() {
        let mut hist = BasicHistory::default();
        hist.add("first");
        hist.add("second");
        assert_eq!(hist.get(0).unwrap(), "first");
        assert_eq!(hist.get(1).unwrap(), "second");
        assert_eq!(hist.get(2), None);
    }

    #[test]
    fn last_returns_most_recent_index() {
        let mut hist = BasicHistory::default();
        hist.add("a");
        assert_eq!(hist.last(), Some(0));
        hist.add("b");
        assert_eq!(hist.last(), Some(1));
        hist.add("c");
        assert_eq!(hist.last(), Some(2));
    }

    #[test]
    fn add_deduplicates_consecutive() {
        let mut hist = BasicHistory::default();
        hist.add("same");
        hist.add("same");
        hist.add("same");
        assert_eq!(hist.last(), Some(0));
        assert_eq!(hist.get(1), None);
    }

    #[test]
    fn add_allows_non_consecutive_duplicates() {
        let mut hist = BasicHistory::default();
        hist.add("a");
        hist.add("b");
        hist.add("a");
        assert_eq!(hist.last(), Some(2));
        assert_eq!(hist.get(0).unwrap(), "a");
        assert_eq!(hist.get(2).unwrap(), "a");
    }

    // ── BasicHistory search ─────────────────────────────────

    #[test]
    fn search_backwards_finds_match() {
        let mut hist = BasicHistory::default();
        hist.add("apple");
        hist.add("banana");
        hist.add("apricot");

        let result = hist
            .search(2, SearchStyle::Substring, SearchDirection::Backwards, "ap")
            .unwrap();
        assert_eq!(result.idx, 2);
        assert_eq!(result.line, "apricot");
        assert_eq!(result.cursor, 0);
    }

    #[test]
    fn search_backwards_skips_non_matches() {
        let mut hist = BasicHistory::default();
        hist.add("apple");
        hist.add("banana");
        hist.add("cherry");

        let result = hist
            .search(2, SearchStyle::Substring, SearchDirection::Backwards, "app")
            .unwrap();
        assert_eq!(result.idx, 0);
        assert_eq!(result.line, "apple");
    }

    #[test]
    fn search_backwards_no_match_returns_none() {
        let mut hist = BasicHistory::default();
        hist.add("hello");
        hist.add("world");

        assert!(hist
            .search(1, SearchStyle::Substring, SearchDirection::Backwards, "xyz")
            .is_none());
    }

    #[test]
    fn search_forwards_finds_match() {
        let mut hist = BasicHistory::default();
        hist.add("alpha");
        hist.add("beta");
        hist.add("gamma");

        let result = hist
            .search(0, SearchStyle::Substring, SearchDirection::Forwards, "bet")
            .unwrap();
        assert_eq!(result.idx, 1);
        assert_eq!(result.line, "beta");
    }

    #[test]
    fn search_from_out_of_bounds_returns_none() {
        let mut hist = BasicHistory::default();
        hist.add("hello");

        assert!(hist
            .search(5, SearchStyle::Substring, SearchDirection::Forwards, "hel")
            .is_none());
    }

    // ── SearchResult ────────────────────────────────────────

    #[test]
    fn search_result_clone_and_eq() {
        let a = SearchResult {
            line: Cow::Borrowed("test"),
            idx: 0,
            cursor: 2,
        };
        let b = a.clone();
        assert_eq!(a, b);
    }
}
