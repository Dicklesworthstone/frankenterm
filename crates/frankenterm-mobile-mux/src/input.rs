//! The phone terminal's input, turned into what the desktop app sends.
//!
//! Keys whose encoding depends on the remote program's modes go to the server
//! as key presses. The mirror enables bracketed paste locally; pasted text goes
//! to the server as a paste, which it brackets only when the program asked.
//! Other bytes retain their order. The opening paste marker and each encoded
//! key must arrive in one write; this is not an arbitrary ANSI stream parser.
//! Once a paste starts, its text and closing marker may span arbitrary writes.

use std::collections::{HashMap, HashSet};
use termwiz::input::{KeyCode, KeyEvent, Modifiers};

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// Long pastes are sent in bounded pieces, never through the middle of a valid
/// UTF-8 scalar or a possible closing delimiter.
const MAX_PASTE_BYTES: usize = 1 << 20;
const MAX_PASTE_BUFFER: usize = MAX_PASTE_BYTES + PASTE_END.len();
const MAX_PENDING_PANES: usize = 16;
const MAX_PENDING_CAPACITY: usize = 2 * MAX_PASTE_BUFFER;
const MAX_ABANDONED_PANES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Bytes(Vec<u8>),
    Key(KeyEvent),
    Paste(String),
}

/// One destination's input stream. An Escape key is deliberately not held for
/// lookahead: decoding arbitrary fragments of key/opening-marker sequences
/// would require an explicit semantic operation boundary from the caller.
#[derive(Default)]
pub struct InputSplitter {
    paste: Option<Vec<u8>>,
}

impl InputSplitter {
    pub fn split(&mut self, mut rest: &[u8]) -> Vec<Piece> {
        let mut pieces = Vec::new();
        let mut text = Vec::new();
        while !rest.is_empty() {
            if let Some(paste) = &mut self.paste {
                // Consume incrementally. Copying all of `rest` first can exceed
                // the cap, and recursively processing its suffix can overflow
                // the stack on many adjacent pastes in a single write.
                if paste.len() == paste.capacity() {
                    let capacity = paste.capacity().max(1024);
                    let next = capacity.saturating_mul(2).min(MAX_PASTE_BUFFER);
                    paste.reserve_exact(next - paste.len());
                }
                paste.push(rest[0]);
                rest = &rest[1..];
                if paste.ends_with(PASTE_END) {
                    paste.truncate(paste.len() - PASTE_END.len());
                    pieces.push(Piece::Paste(String::from_utf8_lossy(paste).into_owned()));
                    self.paste = None;
                } else if paste.len() == MAX_PASTE_BUFFER {
                    // Six bytes of lookbehind keep every proper delimiter
                    // prefix. Moving back by up to three further bytes keeps a
                    // valid scalar whole when its continuation crosses the cap.
                    let mut end = MAX_PASTE_BYTES;
                    while end > MAX_PASTE_BYTES - 3 && paste[end] & 0xc0 == 0x80 {
                        end -= 1;
                    }
                    pieces.push(Piece::Paste(
                        String::from_utf8_lossy(&paste[..end]).into_owned(),
                    ));
                    paste.drain(..end);
                }
                continue;
            }
            if let Some(after) = rest.strip_prefix(PASTE_START) {
                flush(&mut pieces, &mut text);
                self.paste = Some(Vec::new());
                rest = after;
                continue;
            }
            if let Some((event, length)) = key_at(rest) {
                flush(&mut pieces, &mut text);
                pieces.push(Piece::Key(event));
                rest = &rest[length..];
                continue;
            }
            text.push(rest[0]);
            rest = &rest[1..];
        }
        flush(&mut pieces, &mut text);
        pieces
    }

    pub fn in_paste(&self) -> bool {
        self.paste.is_some()
    }

    fn retained_capacity(&self) -> usize {
        self.paste.as_ref().map_or(0, Vec::capacity)
    }
}

impl std::fmt::Debug for InputSplitter {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("InputSplitter")
            .field("in_paste", &self.in_paste())
            .field("retained_capacity", &self.retained_capacity())
            .finish()
    }
}

/// Separate unfinished pastes for separate panes. Switching focus is not an
/// end-of-paste marker. Removing a pane or closing the connection discards its
/// unfinished local fragment; neither action sends more commands.
///
/// The server never gives a removed pane's number to another pane (its pane
/// ids come from one counter that only goes up), so input that arrives later
/// for a pane removed mid-paste is the rest of that paste. Sent as typing, its
/// Returns would run the pasted text, so it is refused and the caller must
/// fail the connection.
#[derive(Default)]
pub struct PaneInput {
    pending: HashMap<usize, InputSplitter>,
    retained_capacity: usize,
    abandoned: HashSet<usize>,
}

impl std::fmt::Debug for PaneInput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PaneInput")
            .field("pending_panes", &self.pending.len())
            .field("retained_capacity", &self.retained_capacity)
            .field("abandoned_panes", &self.abandoned.len())
            .finish()
    }
}

impl PaneInput {
    pub fn in_paste(&self, pane: usize) -> bool {
        self.pending.contains_key(&pane)
    }

    pub fn split(&mut self, pane: usize, data: &[u8]) -> Result<Vec<Piece>, &'static str> {
        if self.abandoned.contains(&pane) {
            self.clear();
            return Err("input arrived for a pane that closed in the middle of a paste");
        }
        let mut input = self.pending.remove(&pane).unwrap_or_default();
        self.retained_capacity -= input.retained_capacity();
        let pieces = input.split(data);
        if input.in_paste() {
            let charge = input.retained_capacity();
            if self.pending.len() >= MAX_PENDING_PANES
                || charge > MAX_PENDING_CAPACITY.saturating_sub(self.retained_capacity)
            {
                // No pieces from this call have escaped to the session. Its
                // caller must fail the connection, never retry this write.
                self.clear();
                return Err("unfinished mux paste capacity exceeded");
            }
            self.retained_capacity += charge;
            self.pending.insert(pane, input);
        }
        Ok(pieces)
    }

    /// Forgets a removed pane. An error means the caller must fail the
    /// connection: too many panes closed mid-paste to remember them all.
    pub fn remove(&mut self, pane: usize) -> Result<(), &'static str> {
        if let Some(input) = self.pending.remove(&pane) {
            self.retained_capacity -= input.retained_capacity();
            self.abandon(pane)?;
        }
        Ok(())
    }

    /// Forgets every pane the inventory no longer lists, as [`Self::remove`].
    pub fn retain(&mut self, mut is_live: impl FnMut(usize) -> bool) -> Result<(), &'static str> {
        let mut released = 0;
        let mut gone = Vec::new();
        self.pending.retain(|pane, input| {
            if is_live(*pane) {
                true
            } else {
                released += input.retained_capacity();
                gone.push(*pane);
                false
            }
        });
        self.retained_capacity -= released;
        for pane in gone {
            self.abandon(pane)?;
        }
        Ok(())
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.retained_capacity = 0;
        self.abandoned.clear();
    }

    fn abandon(&mut self, pane: usize) -> Result<(), &'static str> {
        if self.abandoned.len() >= MAX_ABANDONED_PANES {
            self.clear();
            return Err("too many panes closed in the middle of a paste");
        }
        self.abandoned.insert(pane);
        Ok(())
    }
}

fn flush(pieces: &mut Vec<Piece>, text: &mut Vec<u8>) {
    if !text.is_empty() {
        pieces.push(Piece::Bytes(std::mem::take(text)));
    }
}

fn modifiers(parameter: u32) -> Option<Modifiers> {
    let bits = parameter.checked_sub(1)?;
    if bits > 7 {
        return None;
    }
    let mut mods = Modifiers::NONE;
    for (bit, modifier) in [
        (1, Modifiers::SHIFT),
        (2, Modifiers::ALT),
        (4, Modifiers::CTRL),
    ] {
        if bits & bit != 0 {
            mods |= modifier;
        }
    }
    Some(mods)
}

fn cursor_key(final_byte: u8) -> Option<KeyCode> {
    Some(match final_byte {
        b'A' => KeyCode::UpArrow,
        b'B' => KeyCode::DownArrow,
        b'C' => KeyCode::RightArrow,
        b'D' => KeyCode::LeftArrow,
        b'H' => KeyCode::Home,
        b'F' => KeyCode::End,
        _ => return None,
    })
}

fn low_function_key(final_byte: u8) -> Option<KeyCode> {
    Some(KeyCode::Function(match final_byte {
        b'P' => 1,
        b'Q' => 2,
        b'R' => 3,
        b'S' => 4,
        _ => return None,
    }))
}

fn tilde_key(number: u32) -> Option<KeyCode> {
    Some(match number {
        1 | 7 => KeyCode::Home,
        2 => KeyCode::Insert,
        3 => KeyCode::Delete,
        4 | 8 => KeyCode::End,
        5 => KeyCode::PageUp,
        6 => KeyCode::PageDown,
        15 => KeyCode::Function(5),
        17 => KeyCode::Function(6),
        18 => KeyCode::Function(7),
        19 => KeyCode::Function(8),
        20 => KeyCode::Function(9),
        21 => KeyCode::Function(10),
        23 => KeyCode::Function(11),
        24 => KeyCode::Function(12),
        _ => return None,
    })
}

fn key_at(bytes: &[u8]) -> Option<(KeyEvent, usize)> {
    let event = |key, modifiers| KeyEvent { key, modifiers };
    match bytes.get(..2)? {
        b"\x1bO" => {
            let final_byte = *bytes.get(2)?;
            let key = cursor_key(final_byte).or_else(|| low_function_key(final_byte))?;
            Some((event(key, Modifiers::NONE), 3))
        }
        b"\x1b[" => {
            let body = &bytes[2..];
            let end = body
                .iter()
                .take(8)
                .position(|byte| !(byte.is_ascii_digit() || *byte == b';'))?;
            let final_byte = body[end];
            let mut numbers = Vec::new();
            if end > 0 {
                for part in body[..end].split(|byte| *byte == b';') {
                    numbers.push(std::str::from_utf8(part).ok()?.parse::<u32>().ok()?);
                }
            }
            let length = 2 + end + 1;
            let key = match (final_byte, numbers.as_slice()) {
                // Back-tab: the Z itself means shift.
                (b'Z', []) => event(KeyCode::Tab, Modifiers::SHIFT),
                (b'Z', [1, mods]) => event(KeyCode::Tab, modifiers(*mods)? | Modifiers::SHIFT),
                (_, []) => event(cursor_key(final_byte)?, Modifiers::NONE),
                (_, [1, mods]) if final_byte != b'~' => {
                    let key = cursor_key(final_byte).or_else(|| low_function_key(final_byte))?;
                    event(key, modifiers(*mods)?)
                }
                (b'~', [number]) => event(tilde_key(*number)?, Modifiers::NONE),
                (b'~', [number, mods]) => event(tilde_key(*number)?, modifiers(*mods)?),
                _ => return None,
            };
            Some((key, length))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: KeyCode, modifiers: Modifiers) -> Piece {
        Piece::Key(KeyEvent { key, modifiers })
    }

    fn pasted(pieces: Vec<Piece>) -> String {
        pieces
            .into_iter()
            .map(|piece| match piece {
                Piece::Paste(text) => text,
                other => panic!("unexpected non-paste piece: {other:?}"),
            })
            .collect()
    }

    #[test]
    fn keys_whose_encoding_depends_on_modes_become_key_presses() {
        let mut input = InputSplitter::default();
        assert_eq!(
            input.split(b"\x1b[A\x1b[1;5D\x1bOP\x1b[5~\x1b[3;2~\x1b[Z\x1b[24~\x1b[1;3H"),
            [
                key(KeyCode::UpArrow, Modifiers::NONE),
                key(KeyCode::LeftArrow, Modifiers::CTRL),
                key(KeyCode::Function(1), Modifiers::NONE),
                key(KeyCode::PageUp, Modifiers::NONE),
                key(KeyCode::Delete, Modifiers::SHIFT),
                key(KeyCode::Tab, Modifiers::SHIFT),
                key(KeyCode::Function(12), Modifiers::NONE),
                key(KeyCode::Home, Modifiers::ALT),
            ]
        );
    }

    #[test]
    fn everything_else_is_sent_as_is_in_order() {
        let mut input = InputSplitter::default();
        assert_eq!(
            input.split(b"ls\r\x1b\x1bb\x03\x1b[9;5u\x1b[Bq\x1b[99~\x1b["),
            [
                Piece::Bytes(b"ls\r\x1b\x1bb\x03\x1b[9;5u".to_vec()),
                key(KeyCode::DownArrow, Modifiers::NONE),
                Piece::Bytes(b"q\x1b[99~\x1b[".to_vec()),
            ]
        );
    }

    #[test]
    fn a_bracketed_paste_becomes_one_paste_even_across_writes() {
        let mut input = InputSplitter::default();
        assert_eq!(
            input.split(b"x\x1b[200~one\rtwo\x1b[2"),
            [Piece::Bytes(b"x".to_vec())]
        );
        assert_eq!(
            input.split(b"01~\x1b[A"),
            [
                Piece::Paste("one\rtwo".into()),
                key(KeyCode::UpArrow, Modifiers::NONE),
            ]
        );
        assert_eq!(
            input.split(b"\x1b[200~\x1b[A\x1b[201~"),
            [Piece::Paste("\x1b[A".into())]
        );
    }

    #[test]
    fn end_marker_survives_every_chunk_boundary_near_the_paste_cap() {
        for offset in 0..=PASTE_END.len() + 3 {
            for split in 0..=PASTE_END.len() {
                let mut input = InputSplitter::default();
                let text = "x".repeat(MAX_PASTE_BYTES + offset);
                let mut expected = input.split(PASTE_START);
                expected.extend(input.split(text.as_bytes()));
                expected.extend(input.split(&PASTE_END[..split]));
                expected.extend(input.split(&PASTE_END[split..]));
                assert_eq!(pasted(expected), text, "offset {offset}, split {split}");
                assert!(!input.in_paste());
            }
        }
    }

    #[test]
    fn valid_unicode_is_never_replaced_at_the_chunk_cap() {
        for scalar in ["é", "中", "🙂"] {
            for offset in 0..4 {
                let text = "x".repeat(MAX_PASTE_BYTES - offset) + &scalar.repeat(16);
                for chunk in [1, 5, 16_384, MAX_PASTE_BUFFER + 1] {
                    let mut input = InputSplitter::default();
                    let mut output = input.split(PASTE_START);
                    for bytes in text.as_bytes().chunks(chunk) {
                        output.extend(input.split(bytes));
                    }
                    output.extend(input.split(PASTE_END));
                    assert_eq!(
                        pasted(output),
                        text,
                        "{scalar:?}, offset {offset}, chunk {chunk}"
                    );
                    assert!(!input.in_paste());
                }
            }
        }
    }

    #[test]
    fn many_adjacent_pastes_do_not_recurse_or_reorder_plain_text() {
        let mut bytes = Vec::new();
        for _ in 0..10_000 {
            bytes.extend_from_slice(b"x\x1b[200~y\x1b[201~");
        }
        let output = InputSplitter::default().split(&bytes);
        assert_eq!(output.len(), 20_000);
        for pair in output.chunks_exact(2) {
            assert_eq!(
                pair,
                [Piece::Bytes(b"x".to_vec()), Piece::Paste("y".into())]
            );
        }
    }

    #[test]
    fn retained_allocation_and_each_large_paste_piece_are_bounded() {
        let mut input = InputSplitter::default();
        input.split(PASTE_START);
        let output = input.split(&vec![b'x'; MAX_PASTE_BYTES * 4]);
        assert!(input.retained_capacity() <= MAX_PASTE_BUFFER);
        for piece in &output {
            let Piece::Paste(text) = piece else {
                panic!("expected paste")
            };
            assert!(text.len() <= MAX_PASTE_BYTES);
        }
        let mut complete = output;
        complete.extend(input.split(PASTE_END));
        assert_eq!(pasted(complete), "x".repeat(MAX_PASTE_BYTES * 4));
        assert_eq!(input.retained_capacity(), 0);
    }

    #[test]
    fn marker_like_text_is_not_consumed_as_a_delimiter() {
        let text = "a\u{1b}[201x\u{1b}[20\u{1b}[201\u{1b}[200~b";
        let mut input = InputSplitter::default();
        input.split(PASTE_START);
        let mut output = Vec::new();
        for byte in text.as_bytes() {
            output.extend(input.split(&[*byte]));
        }
        output.extend(input.split(PASTE_END));
        assert_eq!(pasted(output), text);
    }

    #[test]
    fn switching_panes_does_not_finish_or_retarget_an_unfinished_paste() {
        let mut input = PaneInput::default();
        assert!(input.split(10, b"\x1b[200~first ").unwrap().is_empty());
        assert_eq!(
            input.split(20, b"command\r").unwrap(),
            [Piece::Bytes(b"command\r".to_vec())]
        );
        assert!(input.split(20, b"\x1b[200~second ").unwrap().is_empty());
        assert_eq!(
            input.split(10, b"pane\x1b[201~").unwrap(),
            [Piece::Paste("first pane".into())]
        );
        assert_eq!(
            input.split(20, b"pane\x1b[201~").unwrap(),
            [Piece::Paste("second pane".into())]
        );
        assert!(input.pending.is_empty());
        assert_eq!(input.retained_capacity, 0);
    }

    #[test]
    fn pane_removal_inventory_removal_and_close_discard_without_flush() {
        let mut input = PaneInput::default();
        input.split(1, b"\x1b[200~discard").unwrap();
        input.split(2, b"\x1b[200~retain").unwrap();
        input.split(3, b"\x1b[200~unlisted").unwrap();
        input.remove(1).unwrap();
        input.retain(|pane| pane != 3).unwrap();
        assert_eq!(
            input.split(2, PASTE_END).unwrap(),
            [Piece::Paste("retain".into())]
        );
        assert!(input.pending.is_empty());
        assert_eq!(input.retained_capacity, 0);
        // A removed pane without a paste in progress is simply forgotten.
        input.remove(4).unwrap();
        assert_eq!(input.split(4, b"x").unwrap(), [Piece::Bytes(b"x".to_vec())]);
        input.split(5, b"\x1b[200~old").unwrap();
        input.clear();
        assert!(input.pending.is_empty());
        assert!(input.abandoned.is_empty());
        assert_eq!(input.retained_capacity, 0);
    }

    #[test]
    fn the_rest_of_a_removed_panes_paste_is_refused_not_typed() {
        for unlisted in [false, true] {
            let mut input = PaneInput::default();
            input.split(1, b"\x1b[200~make ").unwrap();
            input.split(2, b"\x1b[200~other").unwrap();
            if unlisted {
                input.retain(|pane| pane != 1).unwrap();
            } else {
                input.remove(1).unwrap();
            }
            assert!(input.split(1, b"install\r\x1b[201~").is_err());
            // The refusal discards everything; the caller ends the connection.
            assert!(input.pending.is_empty());
            assert!(input.abandoned.is_empty());
            assert_eq!(input.retained_capacity, 0);
        }
    }

    #[test]
    fn panes_abandoned_mid_paste_are_bounded() {
        let mut input = PaneInput::default();
        for pane in 0..MAX_ABANDONED_PANES {
            input.split(pane, PASTE_START).unwrap();
            input.remove(pane).unwrap();
        }
        input.split(1000, PASTE_START).unwrap();
        assert!(input.remove(1000).is_err());
        assert!(input.abandoned.is_empty());
        assert!(input.pending.is_empty());
    }

    #[test]
    fn pending_pane_limit_does_not_block_other_panes_plain_input() {
        let mut input = PaneInput::default();
        for pane in 0..MAX_PENDING_PANES {
            input.split(pane, PASTE_START).unwrap();
        }
        assert_eq!(
            input.split(100, b"x").unwrap(),
            [Piece::Bytes(b"x".to_vec())]
        );
        assert!(input.split(100, PASTE_START).is_err());
        assert!(input.pending.is_empty());
        assert_eq!(input.retained_capacity, 0);
    }

    #[test]
    fn pending_capacity_counts_allocations_and_is_released_with_the_pane() {
        let mut input = PaneInput::default();
        for pane in 0..2 {
            input.split(pane, PASTE_START).unwrap();
            input.split(pane, &vec![b'x'; MAX_PASTE_BYTES]).unwrap();
        }
        assert!(input.retained_capacity <= MAX_PENDING_CAPACITY);
        input.split(2, PASTE_START).unwrap();
        assert!(input.split(2, &vec![b'x'; MAX_PASTE_BYTES]).is_err());
        assert_eq!(input.retained_capacity, 0);
        input.split(3, b"\x1b[200~x").unwrap();
        assert!(input.retained_capacity > 0);
        input.remove(3);
        assert_eq!(input.retained_capacity, 0);
    }

    #[test]
    fn debug_output_never_contains_pending_input() {
        let secret = b"\x1b[200~private-command-123";
        let mut splitter = InputSplitter::default();
        splitter.split(secret);
        let mut panes = PaneInput::default();
        panes.split(1, secret).unwrap();
        for text in [format!("{splitter:?}"), format!("{panes:?}")] {
            assert!(!text.contains("private-command"));
            assert!(text.contains("retained_capacity"));
        }
    }
}
