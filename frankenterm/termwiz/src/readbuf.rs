use memmem::{Searcher, TwoWaySearcher};

/// This is a simple, small, read buffer that always has the buffer
/// contents available as a contiguous slice.
#[derive(Debug)]
pub struct ReadBuffer {
    storage: Vec<u8>,
}

impl ReadBuffer {
    pub fn new() -> Self {
        Self {
            storage: Vec::with_capacity(16),
        }
    }

    pub fn as_slice(&self) -> &[u8] {
        self.storage.as_slice()
    }

    pub fn is_empty(&self) -> bool {
        self.storage.is_empty()
    }

    pub fn len(&self) -> usize {
        self.storage.len()
    }

    /// Mark `len` bytes as consumed, discarding them and shunting
    /// the contents of the buffer such that the remainder of the
    /// bytes are available at the front of the buffer.
    ///
    /// If `len` exceeds the buffer's current length, the entire
    /// buffer is consumed instead of panicking. Without this
    /// saturation, `advance(buf_len + 1)` underflows
    /// `self.storage.len() - len` (panics in debug, wraps in
    /// release) and then panics in `rotate_left` — a foot-gun for
    /// callers who compute `len` from an external offset.
    pub fn advance(&mut self, len: usize) {
        let len = len.min(self.storage.len());
        let remain = self.storage.len() - len;
        self.storage.rotate_left(len);
        self.storage.truncate(remain);
    }

    /// Append the contents of the slice to the read buffer
    pub fn extend_with(&mut self, slice: &[u8]) {
        self.storage.extend_from_slice(slice);
    }

    /// Search for `needle` starting at `offset`.  Returns its offset
    /// into the buffer if found, else None.
    ///
    /// If `offset` is beyond the buffer's current length, returns
    /// `None` rather than panicking on the out-of-range slice. This
    /// matches the semantics a search on an empty suffix already has
    /// (no match) and removes the hidden precondition that callers
    /// must check the buffer size before calling.
    pub fn find_subsequence(&self, offset: usize, needle: &[u8]) -> Option<usize> {
        if offset > self.storage.len() {
            return None;
        }
        let needle = TwoWaySearcher::new(needle);
        let haystack = &self.storage[offset..];
        needle.search_in(haystack).map(|x| x + offset)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let buf = ReadBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);
        assert!(buf.as_slice().is_empty());
    }

    #[test]
    fn extend_with_adds_data() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"hello");
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), 5);
        assert_eq!(buf.as_slice(), b"hello");
    }

    #[test]
    fn extend_with_appends() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"hel");
        buf.extend_with(b"lo");
        assert_eq!(buf.as_slice(), b"hello");
    }

    #[test]
    fn advance_discards_prefix() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"hello world");
        buf.advance(6);
        assert_eq!(buf.as_slice(), b"world");
        assert_eq!(buf.len(), 5);
    }

    #[test]
    fn advance_entire_buffer() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"abc");
        buf.advance(3);
        assert!(buf.is_empty());
    }

    #[test]
    fn advance_zero_is_noop() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"data");
        buf.advance(0);
        assert_eq!(buf.as_slice(), b"data");
    }

    #[test]
    fn find_subsequence_at_start() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"hello world");
        assert_eq!(buf.find_subsequence(0, b"hello"), Some(0));
    }

    #[test]
    fn find_subsequence_in_middle() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"hello world");
        assert_eq!(buf.find_subsequence(0, b"world"), Some(6));
    }

    #[test]
    fn find_subsequence_not_found() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"hello world");
        assert_eq!(buf.find_subsequence(0, b"xyz"), None);
    }

    #[test]
    fn find_subsequence_with_offset() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"abcabc");
        // Starting at offset 1 should find the second "abc" at position 3
        assert_eq!(buf.find_subsequence(1, b"abc"), Some(3));
    }

    #[test]
    fn find_subsequence_offset_past_match() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"abc");
        // Start searching after the only occurrence
        assert_eq!(buf.find_subsequence(1, b"abc"), None);
    }

    #[test]
    fn advance_saturates_when_len_exceeds_buffer() {
        // Regression: advance(len) used to underflow
        // `self.storage.len() - len` and panic in rotate_left when
        // `len` exceeded the buffer size. Now it saturates.
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"abc");
        buf.advance(usize::MAX);
        assert!(buf.is_empty(), "over-advance must consume the buffer");
    }

    #[test]
    fn advance_saturates_just_past_buffer_end() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"xyz");
        buf.advance(4); // len + 1
        assert!(buf.is_empty());
    }

    #[test]
    fn advance_on_empty_buffer_is_noop() {
        let mut buf = ReadBuffer::new();
        buf.advance(0);
        buf.advance(usize::MAX);
        assert!(buf.is_empty());
    }

    #[test]
    fn find_subsequence_offset_past_end_returns_none() {
        // Regression: offset beyond buffer used to panic on
        // `&self.storage[offset..]`. Now it returns None.
        let buf_slice: Vec<u8> = b"abc".to_vec();
        let mut buf = ReadBuffer::new();
        buf.extend_with(&buf_slice);
        assert_eq!(buf.find_subsequence(100, b"abc"), None);
        assert_eq!(buf.find_subsequence(usize::MAX, b"abc"), None);
    }

    #[test]
    fn find_subsequence_offset_equal_to_len_returns_none_gracefully() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"abc");
        assert_eq!(buf.find_subsequence(3, b"abc"), None);
    }

    #[test]
    fn advance_then_extend_then_find() {
        let mut buf = ReadBuffer::new();
        buf.extend_with(b"prefix:data");
        buf.advance(7);
        assert_eq!(buf.as_slice(), b"data");
        buf.extend_with(b":more");
        assert_eq!(buf.as_slice(), b"data:more");
        assert_eq!(buf.find_subsequence(0, b"more"), Some(5));
    }
}
