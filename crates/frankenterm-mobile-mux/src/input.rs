//! The phone terminal's input, turned into what the desktop app sends.
//!
//! The phone encodes keys in its own terminal's modes, which never follow
//! the remote program's (application cursor keys, keypad, kitty keyboard):
//! the v46 protocol does not report them. So the sequences of keys whose
//! encoding depends on those modes go to the server as key presses, which
//! it encodes for the pane exactly as for the desktop. The mirror turns on
//! bracketed paste in the phone's terminal; a bracketed paste goes to the
//! server as a paste, which it brackets again only if the program asked.
//! Everything else is sent as the bytes it is.

use termwiz::input::{KeyCode, KeyEvent, Modifiers};

const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
/// A paste longer than this goes to the server in parts as it arrives.
const MAX_PASTE_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    Bytes(Vec<u8>),
    Key(KeyEvent),
    Paste(String),
}

/// Splits input as it is written; a paste may span several writes.
#[derive(Debug, Default)]
pub struct InputSplitter {
    paste: Option<Vec<u8>>,
}

impl InputSplitter {
    pub fn split(&mut self, mut rest: &[u8]) -> Vec<Piece> {
        let mut pieces = Vec::new();
        let mut text = Vec::new();
        while !rest.is_empty() {
            if let Some(paste) = &mut self.paste {
                // Search from just before the new bytes: the end marker may
                // have been split between two writes.
                let from = paste.len().saturating_sub(PASTE_END.len() - 1);
                paste.extend_from_slice(rest);
                rest = &[];
                if let Some(at) = find(&paste[from..], PASTE_END).map(|at| from + at) {
                    let after = paste.split_off(at + PASTE_END.len());
                    paste.truncate(at);
                    let done = std::mem::take(paste);
                    self.paste = None;
                    pieces.push(Piece::Paste(String::from_utf8_lossy(&done).into_owned()));
                    // The rest is processed after the paste, in order.
                    let mut after_pieces = self.split(&after);
                    pieces.append(&mut after_pieces);
                    return pieces;
                }
                if paste.len() > MAX_PASTE_BYTES {
                    let part = std::mem::take(paste);
                    pieces.push(Piece::Paste(String::from_utf8_lossy(&part).into_owned()));
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
}

fn flush(pieces: &mut Vec<Piece>, text: &mut Vec<u8>) {
    if !text.is_empty() {
        pieces.push(Piece::Bytes(std::mem::take(text)));
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The xterm modifier parameter (1 + shift 1, alt 2, ctrl 4).
fn modifiers(parameter: u32) -> Option<Modifiers> {
    let bits = parameter.checked_sub(1)?;
    if bits > 7 {
        return None;
    }
    let mut mods = Modifiers::NONE;
    for (bit, modifier) in [(1, Modifiers::SHIFT), (2, Modifiers::ALT), (4, Modifiers::CTRL)] {
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

/// The key whose sequence starts `bytes`, as the phone's terminal encodes
/// it in its default modes, and the sequence's length.
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
}
