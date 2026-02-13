use unicode_segmentation::GraphemeCursor;

use super::actions::Movement;

pub struct LineEditBuffer {
    line: String,
    /// byte index into the UTF-8 string data of the insertion
    /// point.  This is NOT the number of graphemes!
    cursor: usize,
}

impl Default for LineEditBuffer {
    fn default() -> Self {
        Self {
            line: String::new(),
            cursor: 0,
        }
    }
}

impl LineEditBuffer {
    pub fn new(line: &str, cursor: usize) -> Self {
        let mut buffer = Self::default();
        buffer.set_line_and_cursor(line, cursor);
        buffer
    }

    pub fn get_line(&self) -> &str {
        &self.line
    }

    pub fn get_cursor(&self) -> usize {
        self.cursor
    }

    pub fn insert_char(&mut self, c: char) {
        self.line.insert(self.cursor, c);
        let mut cursor = GraphemeCursor::new(self.cursor, self.line.len(), false);
        if let Ok(Some(pos)) = cursor.next_boundary(&self.line, 0) {
            self.cursor = pos;
        }
    }

    pub fn insert_text(&mut self, text: &str) {
        self.line.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// The cursor position is the byte index into the line UTF-8 bytes.
    /// Panics: the cursor must be the first byte in a UTF-8 code point
    /// sequence or the end of the provided line.
    pub fn set_line_and_cursor(&mut self, line: &str, cursor: usize) {
        assert!(
            line.is_char_boundary(cursor),
            "cursor {} is not a char boundary of the new line {}",
            cursor,
            line
        );
        self.line = line.to_string();
        self.cursor = cursor;
    }

    pub fn kill_text(&mut self, kill_movement: Movement, move_movement: Movement) {
        let kill_pos = self.eval_movement(kill_movement);
        let new_cursor = self.eval_movement(move_movement);

        let (lower, upper) = if kill_pos < self.cursor {
            (kill_pos, self.cursor)
        } else {
            (self.cursor, kill_pos)
        };

        self.line.replace_range(lower..upper, "");

        // Clamp to the line length, otherwise a kill to end of line
        // command will leave the cursor way off beyond the end of
        // the line.
        self.cursor = new_cursor.min(self.line.len());
    }

    pub fn clear(&mut self) {
        self.line.clear();
        self.cursor = 0;
    }

    pub fn exec_movement(&mut self, movement: Movement) {
        self.cursor = self.eval_movement(movement);
    }

    /// Compute the cursor position after applying movement
    fn eval_movement(&self, movement: Movement) -> usize {
        match movement {
            Movement::BackwardChar(rep) => {
                let mut position = self.cursor;
                for _ in 0..rep {
                    let mut cursor = GraphemeCursor::new(position, self.line.len(), false);
                    if let Ok(Some(pos)) = cursor.prev_boundary(&self.line, 0) {
                        position = pos;
                    } else {
                        break;
                    }
                }
                position
            }
            Movement::BackwardWord(rep) => {
                let char_indices: Vec<(usize, char)> = self.line.char_indices().collect();
                if char_indices.is_empty() {
                    return self.cursor;
                }
                let mut char_position = char_indices
                    .iter()
                    .position(|(idx, _)| *idx == self.cursor)
                    .unwrap_or(char_indices.len() - 1);

                for _ in 0..rep {
                    if char_position == 0 {
                        break;
                    }

                    let mut found = None;
                    for prev in (0..char_position - 1).rev() {
                        if char_indices[prev].1.is_whitespace() {
                            found = Some(prev + 1);
                            break;
                        }
                    }

                    char_position = found.unwrap_or(0);
                }
                char_indices[char_position].0
            }
            Movement::ForwardWord(rep) => {
                let char_indices: Vec<(usize, char)> = self.line.char_indices().collect();
                if char_indices.is_empty() {
                    return self.cursor;
                }
                let mut char_position = char_indices
                    .iter()
                    .position(|(idx, _)| *idx == self.cursor)
                    .unwrap_or_else(|| char_indices.len());

                for _ in 0..rep {
                    // Skip any non-whitespace characters
                    while char_position < char_indices.len()
                        && !char_indices[char_position].1.is_whitespace()
                    {
                        char_position += 1;
                    }

                    // Skip any whitespace characters
                    while char_position < char_indices.len()
                        && char_indices[char_position].1.is_whitespace()
                    {
                        char_position += 1;
                    }

                    // We are now on the start of the next word
                }
                char_indices
                    .get(char_position)
                    .map(|(i, _)| *i)
                    .unwrap_or_else(|| self.line.len())
            }
            Movement::ForwardChar(rep) => {
                let mut position = self.cursor;
                for _ in 0..rep {
                    let mut cursor = GraphemeCursor::new(position, self.line.len(), false);
                    if let Ok(Some(pos)) = cursor.next_boundary(&self.line, 0) {
                        position = pos;
                    } else {
                        break;
                    }
                }
                position
            }
            Movement::StartOfLine => 0,
            Movement::EndOfLine => {
                let mut cursor =
                    GraphemeCursor::new(self.line.len().saturating_sub(1), self.line.len(), false);
                if let Ok(Some(pos)) = cursor.next_boundary(&self.line, 0) {
                    pos
                } else {
                    self.cursor
                }
            }
            Movement::None => self.cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Construction ────────────────────────────────────────

    #[test]
    fn default_is_empty() {
        let buf = LineEditBuffer::default();
        assert_eq!(buf.get_line(), "");
        assert_eq!(buf.get_cursor(), 0);
    }

    #[test]
    fn new_sets_line_and_cursor() {
        let buf = LineEditBuffer::new("hello", 3);
        assert_eq!(buf.get_line(), "hello");
        assert_eq!(buf.get_cursor(), 3);
    }

    #[test]
    fn new_cursor_at_end() {
        let buf = LineEditBuffer::new("abc", 3);
        assert_eq!(buf.get_cursor(), 3);
    }

    #[test]
    fn new_cursor_at_start() {
        let buf = LineEditBuffer::new("abc", 0);
        assert_eq!(buf.get_cursor(), 0);
    }

    // ── set_line_and_cursor ─────────────────────────────────

    #[test]
    fn set_line_and_cursor_replaces_content() {
        let mut buf = LineEditBuffer::new("old", 1);
        buf.set_line_and_cursor("new text", 4);
        assert_eq!(buf.get_line(), "new text");
        assert_eq!(buf.get_cursor(), 4);
    }

    #[test]
    #[should_panic(expected = "cursor")]
    fn set_line_and_cursor_panics_on_non_boundary() {
        let mut buf = LineEditBuffer::default();
        // "é" is 2 bytes, so byte index 1 is mid-char
        buf.set_line_and_cursor("é", 1);
    }

    // ── insert_char ─────────────────────────────────────────

    #[test]
    fn insert_char_at_start() {
        let mut buf = LineEditBuffer::new("bc", 0);
        buf.insert_char('a');
        assert_eq!(buf.get_line(), "abc");
        assert_eq!(buf.get_cursor(), 1);
    }

    #[test]
    fn insert_char_at_end() {
        let mut buf = LineEditBuffer::new("ab", 2);
        buf.insert_char('c');
        assert_eq!(buf.get_line(), "abc");
        assert_eq!(buf.get_cursor(), 3);
    }

    #[test]
    fn insert_char_in_middle() {
        let mut buf = LineEditBuffer::new("ac", 1);
        buf.insert_char('b');
        assert_eq!(buf.get_line(), "abc");
        assert_eq!(buf.get_cursor(), 2);
    }

    #[test]
    fn insert_multibyte_char() {
        let mut buf = LineEditBuffer::new("", 0);
        buf.insert_char('é');
        assert_eq!(buf.get_line(), "é");
        assert_eq!(buf.get_cursor(), 2); // é is 2 bytes in UTF-8
    }

    // ── insert_text ─────────────────────────────────────────

    #[test]
    fn insert_text_at_start() {
        let mut buf = LineEditBuffer::new("world", 0);
        buf.insert_text("hello ");
        assert_eq!(buf.get_line(), "hello world");
        assert_eq!(buf.get_cursor(), 6);
    }

    #[test]
    fn insert_text_at_end() {
        let mut buf = LineEditBuffer::new("hello", 5);
        buf.insert_text(" world");
        assert_eq!(buf.get_line(), "hello world");
        assert_eq!(buf.get_cursor(), 11);
    }

    #[test]
    fn insert_empty_text() {
        let mut buf = LineEditBuffer::new("hello", 3);
        buf.insert_text("");
        assert_eq!(buf.get_line(), "hello");
        assert_eq!(buf.get_cursor(), 3);
    }

    // ── clear ───────────────────────────────────────────────

    #[test]
    fn clear_empties_buffer() {
        let mut buf = LineEditBuffer::new("hello world", 5);
        buf.clear();
        assert_eq!(buf.get_line(), "");
        assert_eq!(buf.get_cursor(), 0);
    }

    // ── Movement: ForwardChar / BackwardChar ────────────────

    #[test]
    fn forward_char_moves_one() {
        let mut buf = LineEditBuffer::new("abc", 0);
        buf.exec_movement(Movement::ForwardChar(1));
        assert_eq!(buf.get_cursor(), 1);
    }

    #[test]
    fn forward_char_moves_multiple() {
        let mut buf = LineEditBuffer::new("abcdef", 0);
        buf.exec_movement(Movement::ForwardChar(3));
        assert_eq!(buf.get_cursor(), 3);
    }

    #[test]
    fn forward_char_stops_at_end() {
        let mut buf = LineEditBuffer::new("ab", 1);
        buf.exec_movement(Movement::ForwardChar(5));
        assert_eq!(buf.get_cursor(), 2);
    }

    #[test]
    fn backward_char_moves_one() {
        let mut buf = LineEditBuffer::new("abc", 2);
        buf.exec_movement(Movement::BackwardChar(1));
        assert_eq!(buf.get_cursor(), 1);
    }

    #[test]
    fn backward_char_stops_at_start() {
        let mut buf = LineEditBuffer::new("abc", 1);
        buf.exec_movement(Movement::BackwardChar(5));
        assert_eq!(buf.get_cursor(), 0);
    }

    // ── Movement: StartOfLine / EndOfLine ───────────────────

    #[test]
    fn start_of_line_moves_to_zero() {
        let mut buf = LineEditBuffer::new("hello", 3);
        buf.exec_movement(Movement::StartOfLine);
        assert_eq!(buf.get_cursor(), 0);
    }

    #[test]
    fn end_of_line_moves_to_end() {
        let mut buf = LineEditBuffer::new("hello", 0);
        buf.exec_movement(Movement::EndOfLine);
        assert_eq!(buf.get_cursor(), 5);
    }

    // ── Movement: ForwardWord / BackwardWord ────────────────

    #[test]
    fn forward_word_skips_to_next_word() {
        let mut buf = LineEditBuffer::new("hello world", 0);
        buf.exec_movement(Movement::ForwardWord(1));
        assert_eq!(buf.get_cursor(), 6); // start of "world"
    }

    #[test]
    fn forward_word_at_end_stays() {
        let mut buf = LineEditBuffer::new("hello", 5);
        buf.exec_movement(Movement::ForwardWord(1));
        assert_eq!(buf.get_cursor(), 5);
    }

    #[test]
    fn backward_word_skips_to_prev_word() {
        let mut buf = LineEditBuffer::new("hello world", 6);
        buf.exec_movement(Movement::BackwardWord(1));
        assert_eq!(buf.get_cursor(), 0); // start of "hello"
    }

    #[test]
    fn backward_word_at_start_stays() {
        let mut buf = LineEditBuffer::new("hello world", 0);
        buf.exec_movement(Movement::BackwardWord(1));
        assert_eq!(buf.get_cursor(), 0);
    }

    // ── Movement: None ──────────────────────────────────────

    #[test]
    fn movement_none_does_not_move() {
        let mut buf = LineEditBuffer::new("hello", 3);
        buf.exec_movement(Movement::None);
        assert_eq!(buf.get_cursor(), 3);
    }

    // ── kill_text ───────────────────────────────────────────

    #[test]
    fn kill_to_end_of_line() {
        let mut buf = LineEditBuffer::new("hello world", 5);
        buf.kill_text(Movement::EndOfLine, Movement::None);
        assert_eq!(buf.get_line(), "hello");
        assert_eq!(buf.get_cursor(), 5);
    }

    #[test]
    fn kill_to_start_of_line() {
        let mut buf = LineEditBuffer::new("hello world", 5);
        buf.kill_text(Movement::StartOfLine, Movement::StartOfLine);
        assert_eq!(buf.get_line(), " world");
        assert_eq!(buf.get_cursor(), 0);
    }

    #[test]
    fn kill_backward_char() {
        let mut buf = LineEditBuffer::new("abc", 2);
        buf.kill_text(Movement::BackwardChar(1), Movement::BackwardChar(1));
        assert_eq!(buf.get_line(), "ac");
        assert_eq!(buf.get_cursor(), 1);
    }

    #[test]
    fn kill_forward_char() {
        let mut buf = LineEditBuffer::new("abc", 1);
        buf.kill_text(Movement::ForwardChar(1), Movement::None);
        assert_eq!(buf.get_line(), "ac");
        assert_eq!(buf.get_cursor(), 1);
    }

    // ── Unicode / multibyte ─────────────────────────────────

    #[test]
    fn movement_with_multibyte_chars() {
        // "café" has é at byte offset 3-4
        let mut buf = LineEditBuffer::new("café", 0);
        buf.exec_movement(Movement::ForwardChar(4));
        assert_eq!(buf.get_cursor(), 5); // past "café" (5 bytes)
    }

    #[test]
    fn insert_char_into_multibyte_string() {
        let mut buf = LineEditBuffer::new("café", 5);
        buf.insert_char('!');
        assert_eq!(buf.get_line(), "café!");
    }

    // ── empty line edge cases ───────────────────────────────

    #[test]
    fn forward_word_on_empty_line() {
        let mut buf = LineEditBuffer::new("", 0);
        buf.exec_movement(Movement::ForwardWord(1));
        assert_eq!(buf.get_cursor(), 0);
    }

    #[test]
    fn backward_word_on_empty_line() {
        let mut buf = LineEditBuffer::new("", 0);
        buf.exec_movement(Movement::BackwardWord(1));
        assert_eq!(buf.get_cursor(), 0);
    }

    #[test]
    fn kill_on_empty_line() {
        let mut buf = LineEditBuffer::new("", 0);
        buf.kill_text(Movement::EndOfLine, Movement::None);
        assert_eq!(buf.get_line(), "");
    }
}
