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
    pub fn advance(&mut self, len: usize) {
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
    pub fn find_subsequence(&self, offset: usize, needle: &[u8]) -> Option<usize> {
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
        assert_eq!(buf.as_slice(), &[]);
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
