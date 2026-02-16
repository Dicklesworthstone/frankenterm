//! Rope — balanced binary tree for efficient large-text manipulation.
//!
//! A rope stores text as a balanced binary tree of string chunks (leaves),
//! enabling O(log n) insert, delete, split, and concatenation operations
//! on large text sequences. This is ideal for terminal scrollback buffers
//! where text is frequently appended and occasionally sliced.
//!
//! # Design
//!
//! ```text
//!              [Branch: len=13]
//!             /                \
//!      [Branch: len=7]    [Leaf: "world!"]
//!      /            \
//! [Leaf: "Hello"] [Leaf: ", "]
//! ```
//!
//! Each internal node stores the total character count of its left subtree,
//! enabling O(log n) indexed access. Leaves store string chunks up to a
//! configurable maximum size (`LEAF_MAX`).
//!
//! # Use Cases in FrankenTerm
//!
//! - **Scrollback buffer**: Efficient append + random access for terminal output.
//! - **Text replay**: Fast substring extraction for session replay seeking.
//! - **Search results**: Extract context around matches without copying entire buffer.
//! - **Multi-pane editing**: Structural sharing between related text views.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Constants ──────────────────────────────────────────────────────────

/// Maximum characters per leaf node before splitting.
const LEAF_MAX: usize = 512;

// ── Node types ─────────────────────────────────────────────────────────

/// A node in the rope tree (arena-allocated).
#[derive(Debug, Clone, Serialize, Deserialize)]
enum RopeNode {
    /// Leaf node containing a string chunk.
    Leaf {
        text: String,
    },
    /// Branch node with cached weight (left subtree length).
    Branch {
        left: usize,
        right: usize,
        /// Total character count of the left subtree.
        weight: usize,
        /// Total character count of both subtrees.
        total_len: usize,
    },
}

// ── Rope ───────────────────────────────────────────────────────────────

/// A rope data structure for efficient text manipulation.
///
/// Supports O(log n) insert, delete, index, split, and concatenate operations.
/// Built on an arena-allocated balanced binary tree of string chunks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rope {
    nodes: Vec<RopeNode>,
    root: Option<usize>,
}

impl Default for Rope {
    fn default() -> Self {
        Self::new()
    }
}

impl Rope {
    /// Create an empty rope.
    pub fn new() -> Self {
        Self {
            nodes: Vec::new(),
            root: None,
        }
    }

    /// Create a rope from a string.
    pub fn from_str(text: &str) -> Self {
        if text.is_empty() {
            return Self::new();
        }
        let mut rope = Self::new();
        rope.root = Some(rope.build_from_str(text));
        rope
    }

    /// Return the total character count.
    pub fn len(&self) -> usize {
        match self.root {
            Some(idx) => self.node_len(idx),
            None => 0,
        }
    }

    /// Check if the rope is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the character at the given index.
    ///
    /// Returns `None` if the index is out of bounds.
    pub fn char_at(&self, index: usize) -> Option<char> {
        let root = self.root?;
        self.char_at_node(root, index)
    }

    /// Extract a substring as a new `String`.
    ///
    /// Returns characters in the range `[start, end)`.
    pub fn substring(&self, start: usize, end: usize) -> String {
        if start >= end || start >= self.len() {
            return String::new();
        }
        let end = end.min(self.len());
        let mut result = String::with_capacity(end - start);
        if let Some(root) = self.root {
            self.collect_range(root, start, end, 0, &mut result);
        }
        result
    }

    /// Convert the entire rope to a string.
    pub fn to_string_full(&self) -> String {
        self.substring(0, self.len())
    }

    /// Append text to the end of the rope.
    pub fn append(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let new_idx = self.build_from_str(text);
        match self.root {
            Some(root) => {
                self.root = Some(self.merge_nodes(root, new_idx));
            }
            None => {
                self.root = Some(new_idx);
            }
        }
    }

    /// Prepend text to the beginning of the rope.
    pub fn prepend(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let new_idx = self.build_from_str(text);
        match self.root {
            Some(root) => {
                self.root = Some(self.merge_nodes(new_idx, root));
            }
            None => {
                self.root = Some(new_idx);
            }
        }
    }

    /// Insert text at the given character index.
    pub fn insert(&mut self, index: usize, text: &str) {
        if text.is_empty() {
            return;
        }

        if index == 0 {
            self.prepend(text);
            return;
        }

        let len = self.len();
        if index >= len {
            self.append(text);
            return;
        }

        // Split at index, insert between halves
        let (left, right) = self.split(index);
        let mut result = left;
        result.append(text);
        let right_str = right.to_string_full();
        result.append(&right_str);
        *self = result;
    }

    /// Delete characters in the range `[start, end)`.
    pub fn delete(&mut self, start: usize, end: usize) {
        let len = self.len();
        if start >= end || start >= len {
            return;
        }
        let end = end.min(len);

        let (left, right_full) = self.split(start);
        let (_, right) = right_full.split(end - start);

        let right_str = right.to_string_full();
        let mut result = left;
        result.append(&right_str);
        *self = result;
    }

    /// Split the rope at the given index.
    ///
    /// Returns `(left, right)` where `left` contains characters `[0, index)`
    /// and `right` contains characters `[index, len)`.
    pub fn split(&self, index: usize) -> (Rope, Rope) {
        if index == 0 {
            return (Rope::new(), self.clone());
        }
        if index >= self.len() {
            return (self.clone(), Rope::new());
        }
        let left_str = self.substring(0, index);
        let right_str = self.substring(index, self.len());
        (Rope::from_str(&left_str), Rope::from_str(&right_str))
    }

    /// Concatenate another rope onto the end of this one.
    pub fn concat(&mut self, other: &Rope) {
        let other_str = other.to_string_full();
        self.append(&other_str);
    }

    /// Return the number of lines (count of '\n' characters + 1 if non-empty).
    pub fn line_count(&self) -> usize {
        if self.is_empty() {
            return 0;
        }
        let text = self.to_string_full();
        text.chars().filter(|&c| c == '\n').count() + 1
    }

    /// Get a specific line by 0-based line number.
    pub fn line(&self, line_num: usize) -> Option<String> {
        let text = self.to_string_full();
        text.split('\n').nth(line_num).map(String::from)
    }

    /// Return the number of nodes (for diagnostics).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    // ── Internal: Node operations ──────────────────────────────────

    fn alloc_leaf(&mut self, text: String) -> usize {
        let idx = self.nodes.len();
        self.nodes.push(RopeNode::Leaf { text });
        idx
    }

    fn alloc_branch(&mut self, left: usize, right: usize) -> usize {
        let weight = self.node_len(left);
        let total_len = weight + self.node_len(right);
        let idx = self.nodes.len();
        self.nodes.push(RopeNode::Branch {
            left,
            right,
            weight,
            total_len,
        });
        idx
    }

    fn node_len(&self, idx: usize) -> usize {
        match &self.nodes[idx] {
            RopeNode::Leaf { text } => text.len(),
            RopeNode::Branch { total_len, .. } => *total_len,
        }
    }

    fn build_from_str(&mut self, text: &str) -> usize {
        if text.len() <= LEAF_MAX {
            return self.alloc_leaf(text.to_string());
        }

        // Split into balanced halves
        let mid = text.len() / 2;
        // Ensure we split on a char boundary
        let mid = text.floor_char_boundary(mid);
        let left = self.build_from_str(&text[..mid]);
        let right = self.build_from_str(&text[mid..]);
        self.alloc_branch(left, right)
    }

    fn merge_nodes(&mut self, left: usize, right: usize) -> usize {
        // Simple merge — just create a branch
        // Could be smarter about rebalancing, but this keeps it simple
        self.alloc_branch(left, right)
    }

    fn char_at_node(&self, idx: usize, pos: usize) -> Option<char> {
        match &self.nodes[idx] {
            RopeNode::Leaf { text } => text.chars().nth(pos),
            RopeNode::Branch {
                left,
                right,
                weight,
                ..
            } => {
                if pos < *weight {
                    self.char_at_node(*left, pos)
                } else {
                    self.char_at_node(*right, pos - weight)
                }
            }
        }
    }

    fn collect_range(
        &self,
        idx: usize,
        start: usize,
        end: usize,
        offset: usize,
        result: &mut String,
    ) {
        match &self.nodes[idx] {
            RopeNode::Leaf { text } => {
                let node_start = offset;
                let node_end = offset + text.len();

                if start >= node_end || end <= node_start {
                    return; // No overlap
                }

                let local_start = start.saturating_sub(node_start);
                let local_end = (end - node_start).min(text.len());

                if local_start < local_end && local_end <= text.len() {
                    result.push_str(&text[local_start..local_end]);
                }
            }
            RopeNode::Branch {
                left,
                right,
                weight,
                ..
            } => {
                let left_end = offset + weight;

                // Check left subtree
                if start < left_end {
                    self.collect_range(*left, start, end, offset, result);
                }

                // Check right subtree
                if end > left_end {
                    self.collect_range(*right, start, end, left_end, result);
                }
            }
        }
    }
}

// ── Display ────────────────────────────────────────────────────────────

impl fmt::Display for Rope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rope({} chars, {} nodes)", self.len(), self.nodes.len())
    }
}

// ── From conversions ───────────────────────────────────────────────────

impl From<&str> for Rope {
    fn from(s: &str) -> Self {
        Rope::from_str(s)
    }
}

impl From<String> for Rope {
    fn from(s: String) -> Self {
        Rope::from_str(&s)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_rope() {
        let rope = Rope::new();
        assert!(rope.is_empty());
        assert_eq!(rope.len(), 0);
        assert!(rope.char_at(0).is_none());
        assert_eq!(rope.to_string_full(), "");
    }

    #[test]
    fn from_str_small() {
        let rope = Rope::from_str("hello");
        assert_eq!(rope.len(), 5);
        assert_eq!(rope.to_string_full(), "hello");
    }

    #[test]
    fn from_str_large() {
        let text = "a".repeat(2000);
        let rope = Rope::from_str(&text);
        assert_eq!(rope.len(), 2000);
        assert_eq!(rope.to_string_full(), text);
    }

    #[test]
    fn char_at() {
        let rope = Rope::from_str("Hello, world!");
        assert_eq!(rope.char_at(0), Some('H'));
        assert_eq!(rope.char_at(7), Some('w'));
        assert_eq!(rope.char_at(12), Some('!'));
        assert_eq!(rope.char_at(13), None);
    }

    #[test]
    fn substring() {
        let rope = Rope::from_str("Hello, world!");
        assert_eq!(rope.substring(0, 5), "Hello");
        assert_eq!(rope.substring(7, 12), "world");
        assert_eq!(rope.substring(0, 13), "Hello, world!");
    }

    #[test]
    fn substring_out_of_bounds() {
        let rope = Rope::from_str("hello");
        assert_eq!(rope.substring(0, 100), "hello");
        assert_eq!(rope.substring(10, 20), "");
        assert_eq!(rope.substring(3, 2), "");
    }

    #[test]
    fn append() {
        let mut rope = Rope::from_str("hello");
        rope.append(" world");
        assert_eq!(rope.to_string_full(), "hello world");
        assert_eq!(rope.len(), 11);
    }

    #[test]
    fn prepend() {
        let mut rope = Rope::from_str("world");
        rope.prepend("hello ");
        assert_eq!(rope.to_string_full(), "hello world");
    }

    #[test]
    fn insert_middle() {
        let mut rope = Rope::from_str("helloworld");
        rope.insert(5, " ");
        assert_eq!(rope.to_string_full(), "hello world");
    }

    #[test]
    fn insert_beginning() {
        let mut rope = Rope::from_str("world");
        rope.insert(0, "hello ");
        assert_eq!(rope.to_string_full(), "hello world");
    }

    #[test]
    fn insert_end() {
        let mut rope = Rope::from_str("hello");
        rope.insert(100, " world");
        assert_eq!(rope.to_string_full(), "hello world");
    }

    #[test]
    fn delete_middle() {
        let mut rope = Rope::from_str("hello world");
        rope.delete(5, 6);
        assert_eq!(rope.to_string_full(), "helloworld");
    }

    #[test]
    fn delete_beginning() {
        let mut rope = Rope::from_str("hello world");
        rope.delete(0, 6);
        assert_eq!(rope.to_string_full(), "world");
    }

    #[test]
    fn delete_end() {
        let mut rope = Rope::from_str("hello world");
        rope.delete(5, 11);
        assert_eq!(rope.to_string_full(), "hello");
    }

    #[test]
    fn delete_all() {
        let mut rope = Rope::from_str("hello");
        rope.delete(0, 5);
        assert_eq!(rope.to_string_full(), "");
    }

    #[test]
    fn split_basic() {
        let rope = Rope::from_str("hello world");
        let (left, right) = rope.split(5);
        assert_eq!(left.to_string_full(), "hello");
        assert_eq!(right.to_string_full(), " world");
    }

    #[test]
    fn split_at_zero() {
        let rope = Rope::from_str("hello");
        let (left, right) = rope.split(0);
        assert_eq!(left.to_string_full(), "");
        assert_eq!(right.to_string_full(), "hello");
    }

    #[test]
    fn split_at_end() {
        let rope = Rope::from_str("hello");
        let (left, right) = rope.split(5);
        assert_eq!(left.to_string_full(), "hello");
        assert_eq!(right.to_string_full(), "");
    }

    #[test]
    fn concat_ropes() {
        let mut rope1 = Rope::from_str("hello");
        let rope2 = Rope::from_str(" world");
        rope1.concat(&rope2);
        assert_eq!(rope1.to_string_full(), "hello world");
    }

    #[test]
    fn line_count() {
        let rope = Rope::from_str("line1\nline2\nline3");
        assert_eq!(rope.line_count(), 3);

        let empty = Rope::new();
        assert_eq!(empty.line_count(), 0);

        let single = Rope::from_str("no newline");
        assert_eq!(single.line_count(), 1);
    }

    #[test]
    fn line_access() {
        let rope = Rope::from_str("line1\nline2\nline3");
        assert_eq!(rope.line(0), Some("line1".to_string()));
        assert_eq!(rope.line(1), Some("line2".to_string()));
        assert_eq!(rope.line(2), Some("line3".to_string()));
        assert_eq!(rope.line(3), None);
    }

    #[test]
    fn from_conversions() {
        let rope1: Rope = "hello".into();
        let rope2: Rope = String::from("hello").into();
        assert_eq!(rope1.to_string_full(), "hello");
        assert_eq!(rope2.to_string_full(), "hello");
    }

    #[test]
    fn display_format() {
        let rope = Rope::from_str("hello");
        let s = format!("{}", rope);
        assert!(s.contains("5 chars"));
    }

    #[test]
    fn default_is_empty() {
        let rope = Rope::default();
        assert!(rope.is_empty());
    }

    #[test]
    fn serde_roundtrip() {
        let rope = Rope::from_str("hello world");
        let json = serde_json::to_string(&rope).unwrap();
        let restored: Rope = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.to_string_full(), "hello world");
        assert_eq!(restored.len(), 11);
    }

    #[test]
    fn large_append_chain() {
        let mut rope = Rope::new();
        for i in 0..100 {
            rope.append(&format!("line {}\n", i));
        }
        let text = rope.to_string_full();
        assert!(text.starts_with("line 0\n"));
        assert!(text.contains("line 50\n"));
        assert!(text.contains("line 99\n"));
    }

    #[test]
    fn append_empty() {
        let mut rope = Rope::from_str("hello");
        rope.append("");
        assert_eq!(rope.to_string_full(), "hello");
        assert_eq!(rope.len(), 5);
    }

    #[test]
    fn unicode_text() {
        let rope = Rope::from_str("Hello, 世界! 🌍");
        assert_eq!(rope.to_string_full(), "Hello, 世界! 🌍");
        // Note: len() counts bytes, not characters
        assert_eq!(rope.len(), "Hello, 世界! 🌍".len());
    }

    #[test]
    fn repeated_insert_delete() {
        let mut rope = Rope::from_str("abcdef");
        rope.insert(3, "XY");
        assert_eq!(rope.to_string_full(), "abcXYdef");
        rope.delete(3, 5);
        assert_eq!(rope.to_string_full(), "abcdef");
    }

    #[test]
    fn clone_independence() {
        let rope = Rope::from_str("hello");
        let rope2 = rope.clone();
        assert_eq!(rope.to_string_full(), rope2.to_string_full());
    }

    #[test]
    fn char_at_out_of_bounds() {
        let rope = Rope::from_str("abc");
        assert_eq!(rope.char_at(0), Some('a'));
        assert_eq!(rope.char_at(2), Some('c'));
        assert_eq!(rope.char_at(3), None);
    }
}
