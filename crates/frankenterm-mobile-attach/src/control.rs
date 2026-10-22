//! Private control messages from the phone app, embedded in its keyboard
//! byte stream as `ESC P ft-mobile;<verb>[;<arg>] ESC \`.
//!
//! [`ControlScanner`] removes complete messages from the input and passes
//! every other byte through in order, even when a message or an ordinary
//! escape sequence is split across reads.  A partial match is held back only
//! briefly ([`ControlScanner::on_idle`]): a lone ESC (the Escape key) for
//! [`LONE_ESC_RELEASE_AFTER`], an `ESC P`-prefixed partial match (Alt+Shift+P)
//! for [`PREFIX_RELEASE_AFTER`].  Malformed messages are dropped, never
//! forwarded as keystrokes, once their full prefix has been seen.

use std::time::Duration;

/// The exact byte prefix of every control message.
pub const CONTROL_PREFIX: &[u8] = b"\x1bPft-mobile;";
/// The longest payload (`verb;arg`) accepted; longer messages are dropped.
pub const MAX_CONTROL_PAYLOAD: usize = 256;
/// How long a lone trailing ESC is held waiting for a following byte.
pub const LONE_ESC_RELEASE_AFTER: Duration = Duration::from_millis(25);
/// How long a partial `ESC P ft-mobile;` prefix is held before it is
/// released as ordinary input.
pub const PREFIX_RELEASE_AFTER: Duration = Duration::from_millis(200);
/// How long an unterminated message payload waits for its `ESC \`.
pub const PAYLOAD_ABANDON_AFTER: Duration = Duration::from_secs(2);

/// One decoded control message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlMessage {
    pub verb: String,
    pub arg: String,
}

/// Scanner output, in input order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScanItem {
    /// Ordinary input bytes for the attached pane.
    Data(Vec<u8>),
    /// A complete, well-formed control message.
    Control(ControlMessage),
}

#[derive(Debug, Default)]
enum State {
    /// Passing bytes through.
    #[default]
    Ground,
    /// Holding a proper, non-empty prefix of [`CONTROL_PREFIX`].
    Prefix(Vec<u8>),
    /// Inside a message payload.
    Payload {
        payload: Vec<u8>,
        /// The previous byte was ESC; only `\` may follow.
        escape: bool,
        /// The payload exceeded [`MAX_CONTROL_PAYLOAD`]; drop it at its end.
        overflow: bool,
    },
}

/// Incremental splitter of control messages from ordinary input.
#[derive(Debug, Default)]
pub struct ControlScanner {
    state: State,
}

/// Collects pass-through bytes and flushes them before each control item.
struct Output {
    items: Vec<ScanItem>,
    data: Vec<u8>,
}

impl Output {
    fn control(&mut self, message: ControlMessage) {
        self.flush();
        self.items.push(ScanItem::Control(message));
    }

    fn flush(&mut self) {
        if !self.data.is_empty() {
            self.items
                .push(ScanItem::Data(std::mem::take(&mut self.data)));
        }
    }

    fn finish(mut self) -> Vec<ScanItem> {
        self.flush();
        self.items
    }
}

impl ControlScanner {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether bytes are held back waiting for more input.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        !matches!(self.state, State::Ground)
    }

    /// How long after the last input [`Self::on_idle`] will settle the held
    /// bytes, if any are held.
    #[must_use]
    pub fn release_after(&self) -> Option<Duration> {
        match &self.state {
            State::Ground => None,
            State::Prefix(held) if held.len() == 1 => Some(LONE_ESC_RELEASE_AFTER),
            State::Prefix(_) => Some(PREFIX_RELEASE_AFTER),
            State::Payload { .. } => Some(PAYLOAD_ABANDON_AFTER),
        }
    }

    /// Scan one chunk of input.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<ScanItem> {
        let mut out = Output {
            items: Vec::new(),
            data: Vec::with_capacity(bytes.len()),
        };
        for &byte in bytes {
            self.step(byte, &mut out);
        }
        out.finish()
    }

    /// Settle held bytes after `idle` without further input: release a
    /// partial prefix as ordinary input, or drop an unterminated payload.
    pub fn on_idle(&mut self, idle: Duration) -> Vec<ScanItem> {
        if self.release_after().is_none_or(|after| idle < after) {
            return Vec::new();
        }
        match std::mem::take(&mut self.state) {
            State::Prefix(held) => vec![ScanItem::Data(held)],
            // An unterminated payload is dropped, never typed.
            State::Ground | State::Payload { .. } => Vec::new(),
        }
    }

    fn step(&mut self, byte: u8, out: &mut Output) {
        match &mut self.state {
            State::Ground => {
                if byte == CONTROL_PREFIX[0] {
                    self.state = State::Prefix(vec![byte]);
                } else {
                    out.data.push(byte);
                }
            }
            State::Prefix(held) => {
                if byte == CONTROL_PREFIX[held.len()] {
                    held.push(byte);
                    if held.len() == CONTROL_PREFIX.len() {
                        self.state = State::Payload {
                            payload: Vec::new(),
                            escape: false,
                            overflow: false,
                        };
                    }
                    return;
                }
                // Not ours (an escape sequence, Alt+key, ...): release what
                // was held and rescan this byte from the ground state, since
                // it may itself start a new sequence.
                let State::Prefix(held) = std::mem::take(&mut self.state) else {
                    unreachable!("matched the prefix state above");
                };
                out.data.extend_from_slice(&held);
                self.step(byte, out);
            }
            State::Payload {
                payload,
                escape,
                overflow,
            } => {
                if *escape {
                    if byte == b'\\' {
                        let message = if *overflow {
                            None
                        } else {
                            parse_payload(payload)
                        };
                        self.state = State::Ground;
                        if let Some(message) = message {
                            out.control(message);
                        }
                        return;
                    }
                    // ESC inside a payload starts something else: drop the
                    // malformed message and rescan ESC and this byte.
                    self.state = State::Ground;
                    self.step(CONTROL_PREFIX[0], out);
                    self.step(byte, out);
                    return;
                }
                if byte == CONTROL_PREFIX[0] {
                    *escape = true;
                } else if byte < 0x20 || byte == 0x7f {
                    // A control character cannot be part of a message (and
                    // the app never splits one with a keystroke): drop the
                    // message and treat the byte as ordinary input.
                    self.state = State::Ground;
                    self.step(byte, out);
                } else if payload.len() >= MAX_CONTROL_PAYLOAD {
                    *overflow = true;
                } else {
                    payload.push(byte);
                }
            }
        }
    }
}

/// `verb[;arg]` of printable ASCII with a non-empty `[A-Za-z0-9_-]` verb.
fn parse_payload(payload: &[u8]) -> Option<ControlMessage> {
    if !payload.iter().all(|b| (0x20..0x7f).contains(b)) {
        return None;
    }
    let text = std::str::from_utf8(payload).ok()?;
    let (verb, arg) = text.split_once(';').unwrap_or((text, ""));
    if verb.is_empty()
        || !verb
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some(ControlMessage {
        verb: verb.to_string(),
        arg: arg.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(bytes: &[u8]) -> ScanItem {
        ScanItem::Data(bytes.to_vec())
    }

    fn control(verb: &str, arg: &str) -> ScanItem {
        ScanItem::Control(ControlMessage {
            verb: verb.to_string(),
            arg: arg.to_string(),
        })
    }

    /// Join adjacent data items so chunked and whole scans compare equal.
    fn merged(items: Vec<ScanItem>) -> Vec<ScanItem> {
        let mut out: Vec<ScanItem> = Vec::new();
        for item in items {
            match (out.last_mut(), item) {
                (Some(ScanItem::Data(prev)), ScanItem::Data(next)) => prev.extend(next),
                (_, item) => out.push(item),
            }
        }
        out
    }

    #[test]
    fn ordinary_input_passes_through_untouched() {
        let mut scanner = ControlScanner::new();
        assert_eq!(scanner.feed(b"ls -la\r"), vec![data(b"ls -la\r")]);
        assert_eq!(scanner.feed(b"\x1b[A\x1bOB"), vec![data(b"\x1b[A\x1bOB")]);
        assert!(!scanner.is_pending());
    }

    #[test]
    fn a_complete_message_is_extracted_in_order() {
        let mut scanner = ControlScanner::new();
        assert_eq!(
            scanner.feed(b"x\x1bPft-mobile;attach;7\x1b\\y"),
            vec![data(b"x"), control("attach", "7"), data(b"y")]
        );
        assert_eq!(
            scanner.feed(b"\x1bPft-mobile;panes\x1b\\\x1bPft-mobile;panes;\x1b\\"),
            vec![control("panes", ""), control("panes", "")]
        );
    }

    #[test]
    fn messages_split_at_every_byte_boundary_scan_identically() {
        let input: &[u8] =
            b"ab\x1b[Cx\x1bPft-mobile;attach;12\x1b\\\x1bPq\x1bPft-mobile;panes\x1b\\z";
        let mut whole = ControlScanner::new();
        let expected = merged(whole.feed(input));
        assert_eq!(
            expected,
            vec![
                data(b"ab\x1b[Cx"),
                control("attach", "12"),
                data(b"\x1bPq"),
                control("panes", ""),
                data(b"z"),
            ]
        );
        for split in 1..input.len() {
            let mut scanner = ControlScanner::new();
            let mut items = scanner.feed(&input[..split]);
            items.extend(scanner.feed(&input[split..]));
            assert_eq!(merged(items), expected, "split at byte {split}");
            assert!(!scanner.is_pending(), "split at byte {split}");
        }
        // One byte per read is the worst case for an SSH channel.
        let mut scanner = ControlScanner::new();
        let mut items = Vec::new();
        for byte in input {
            items.extend(scanner.feed(std::slice::from_ref(byte)));
        }
        assert_eq!(merged(items), expected);
    }

    #[test]
    fn a_lone_escape_is_released_after_a_short_idle() {
        let mut scanner = ControlScanner::new();
        assert_eq!(scanner.feed(b"\x1b"), Vec::new());
        assert!(scanner.is_pending());
        assert_eq!(scanner.on_idle(Duration::from_millis(5)), Vec::new());
        assert_eq!(scanner.on_idle(LONE_ESC_RELEASE_AFTER), vec![data(b"\x1b")]);
        assert!(!scanner.is_pending());
    }

    #[test]
    fn a_partial_prefix_waits_longer_then_passes_through() {
        let mut scanner = ControlScanner::new();
        assert_eq!(scanner.feed(b"\x1bPft-"), Vec::new());
        assert_eq!(scanner.on_idle(Duration::from_millis(50)), Vec::new());
        assert_eq!(
            scanner.on_idle(PREFIX_RELEASE_AFTER),
            vec![data(b"\x1bPft-")]
        );
        assert_eq!(scanner.feed(b"q"), vec![data(b"q")]);
    }

    #[test]
    fn an_escape_inside_a_payload_drops_the_message_and_keeps_the_sequence() {
        let mut scanner = ControlScanner::new();
        assert_eq!(
            scanner.feed(b"\x1bPft-mobile;attach;1\x1b[B"),
            vec![data(b"\x1b[B")]
        );
        // A nested message start restarts cleanly.
        assert_eq!(
            scanner.feed(b"\x1bPft-mobile;att\x1bPft-mobile;attach;2\x1b\\"),
            vec![control("attach", "2")]
        );
    }

    #[test]
    fn malformed_messages_are_dropped_never_typed() {
        let mut scanner = ControlScanner::new();
        // A control character ends the message; the byte itself is input.
        assert_eq!(scanner.feed(b"\x1bPft-mobile;att\rX"), vec![data(b"\rX")]);
        // Overlong payloads are swallowed to their terminator.
        let mut long = CONTROL_PREFIX.to_vec();
        long.extend(std::iter::repeat_n(b'a', MAX_CONTROL_PAYLOAD + 50));
        long.extend_from_slice(b"\x1b\\z");
        assert_eq!(scanner.feed(&long), vec![data(b"z")]);
        // Non-ASCII payloads and empty or invalid verbs are dropped.
        assert_eq!(
            scanner.feed("\x1bPft-mobile;attach;\u{e9}\x1b\\".as_bytes()),
            Vec::new()
        );
        assert_eq!(scanner.feed(b"\x1bPft-mobile;\x1b\\"), Vec::new());
        assert_eq!(scanner.feed(b"\x1bPft-mobile;at tach;1\x1b\\"), Vec::new());
        assert!(!scanner.is_pending());
    }

    #[test]
    fn an_unterminated_payload_is_abandoned_after_idle() {
        let mut scanner = ControlScanner::new();
        assert_eq!(scanner.feed(b"\x1bPft-mobile;atta"), Vec::new());
        assert_eq!(scanner.on_idle(Duration::from_millis(500)), Vec::new());
        assert!(scanner.is_pending());
        assert_eq!(scanner.on_idle(PAYLOAD_ABANDON_AFTER), Vec::new());
        assert!(!scanner.is_pending());
        assert_eq!(scanner.feed(b"q"), vec![data(b"q")]);
    }

    #[test]
    fn unknown_verbs_are_reported_for_the_caller_to_ignore() {
        let mut scanner = ControlScanner::new();
        assert_eq!(
            scanner.feed(b"\x1bPft-mobile;future-verb;a;b\x1b\\"),
            vec![control("future-verb", "a;b")]
        );
    }
}
