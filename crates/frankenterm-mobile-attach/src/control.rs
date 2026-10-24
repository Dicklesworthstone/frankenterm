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
//! Bracketed paste is forwarded byte for byte, including any private-looking
//! messages in its contents. Only its exact end marker resumes control parsing.

use std::time::Duration;

/// The exact byte prefix of every control message.
pub const CONTROL_PREFIX: &[u8] = b"\x1bPft-mobile;";
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";
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
    /// Holding a proper, non-empty prefix of a control or paste-start marker.
    Prefix(Vec<u8>),
    /// Paste bytes are already forwarded, including any matched end prefix.
    /// No payload is retained here and idle cannot end the paste.
    Paste { end_matched: usize },
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
        matches!(self.state, State::Prefix(_) | State::Payload { .. })
    }

    /// How long after the last input [`Self::on_idle`] will settle the held
    /// bytes, if any are held.
    #[must_use]
    pub fn release_after(&self) -> Option<Duration> {
        match &self.state {
            State::Ground | State::Paste { .. } => None,
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
            State::Ground | State::Paste { .. } | State::Payload { .. } => Vec::new(),
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
                if (CONTROL_PREFIX.starts_with(held)
                    && CONTROL_PREFIX.get(held.len()) == Some(&byte))
                    || (PASTE_START.starts_with(held)
                        && PASTE_START.get(held.len()) == Some(&byte))
                {
                    held.push(byte);
                    if held.as_slice() == CONTROL_PREFIX {
                        self.state = State::Payload {
                            payload: Vec::new(),
                            escape: false,
                            overflow: false,
                        };
                    } else if held.as_slice() == PASTE_START {
                        out.data.extend_from_slice(held);
                        self.state = State::Paste { end_matched: 0 };
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
            State::Paste { end_matched } => {
                out.data.push(byte);
                if byte == PASTE_END[*end_matched] {
                    *end_matched += 1;
                    if *end_matched == PASTE_END.len() {
                        self.state = State::Ground;
                    }
                } else {
                    // ESC occurs only at the marker's start, so it is the
                    // only possible overlapping prefix after a mismatch.
                    *end_matched = usize::from(byte == PASTE_END[0]);
                }
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
    use termwiz::input::{InputEvent, InputParser};

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

    #[derive(Debug, PartialEq)]
    enum ParsedItem {
        Input(InputEvent),
        Control(ControlMessage),
    }

    /// Use the same scanner-then-parser ordering as Session::on_input.
    fn parse_chunks(chunks: &[&[u8]]) -> Vec<ParsedItem> {
        let mut scanner = ControlScanner::new();
        let mut parser = InputParser::new();
        let mut out = Vec::new();
        for chunk in chunks {
            for item in scanner.feed(chunk) {
                match item {
                    ScanItem::Data(bytes) => out.extend(
                        parser
                            .parse_as_vec(&bytes, true)
                            .into_iter()
                            .map(ParsedItem::Input),
                    ),
                    ScanItem::Control(message) => out.push(ParsedItem::Control(message)),
                }
            }
        }
        out.extend(
            parser
                .parse_as_vec(&[], false)
                .into_iter()
                .map(ParsedItem::Input),
        );
        out
    }

    fn parsed_control(verb: &str, arg: &str) -> ParsedItem {
        ParsedItem::Control(ControlMessage {
            verb: verb.to_string(),
            arg: arg.to_string(),
        })
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

    #[test]
    fn paste_contents_are_forwarded_byte_for_byte_without_extracting_controls() {
        let mut input = PASTE_START.to_vec();
        input.extend_from_slice(b"\x1bPft-mobile;attach;99\x1b\\\x1bPft-mobile;panes\x1b\\");
        input.extend_from_slice(b"\x1bPft-mobile;");
        input.extend(std::iter::repeat_n(b'a', MAX_CONTROL_PAYLOAD + 50));
        input.extend_from_slice(b"\x1b\\\x00\xff\x7f\r\n");
        input.extend_from_slice(PASTE_END);
        let mut scanner = ControlScanner::new();
        assert_eq!(scanner.feed(&input), vec![data(&input)]);
        assert!(!scanner.is_pending());
        assert_eq!(scanner.release_after(), None);
    }

    #[test]
    fn pasted_commands_cannot_redirect_input_at_any_read_boundary() {
        let payload = "before\x1bPft-mobile;attach;99\x1b\\\x1bPft-mobile;panes\x1b\\é after";
        let mut input = b"\x1bPft-mobile;attach;7\x1b\\".to_vec();
        input.extend_from_slice(PASTE_START);
        input.extend_from_slice(payload.as_bytes());
        input.extend_from_slice(PASTE_END);
        input.extend_from_slice(b"\x1bPft-mobile;attach;9\x1b\\");
        input.extend_from_slice(b"\x1b[200~second\x1b[201~");
        let expected = vec![
            parsed_control("attach", "7"),
            ParsedItem::Input(InputEvent::Paste(payload.to_string())),
            parsed_control("attach", "9"),
            ParsedItem::Input(InputEvent::Paste("second".to_string())),
        ];
        assert_eq!(parse_chunks(&[&input]), expected);
        for split in 1..input.len() {
            assert_eq!(
                parse_chunks(&[&input[..split], &input[split..]]),
                expected,
                "split at byte {split}"
            );
        }
        assert_eq!(parse_chunks(&input.chunks(1).collect::<Vec<_>>()), expected);
    }

    #[test]
    fn idle_during_paste_or_its_end_marker_cannot_reenable_controls() {
        let payload = b"\x1bPft-mobile;attach;99\x1b\\";
        for split in 0..PASTE_END.len() {
            let mut scanner = ControlScanner::new();
            let mut parser = InputParser::new();
            let mut head = PASTE_START.to_vec();
            head.extend_from_slice(payload);
            head.extend_from_slice(&PASTE_END[..split]);
            assert_eq!(scanner.feed(&head), vec![data(&head)]);
            assert_eq!(parser.parse_as_vec(&head, true), Vec::new());
            assert!(!scanner.is_pending(), "paste bytes have already been forwarded");
            assert_eq!(scanner.release_after(), None);
            assert!(scanner.on_idle(Duration::from_secs(60)).is_empty());
            assert!(parser.parse_as_vec(&[], false).is_empty());

            let after_idle = b"\x1bPft-mobile;attach;88\x1b\\";
            let mut continued = after_idle.to_vec();
            continued.extend_from_slice(PASTE_END);
            let mut tail = continued.clone();
            tail.extend_from_slice(b"\x1bPft-mobile;attach;9\x1b\\");
            let items = scanner.feed(&tail);
            assert_eq!(
                items,
                vec![data(&continued), control("attach", "9")],
                "end marker split at byte {split}"
            );
            let ScanItem::Data(bytes) = &items[0] else {
                panic!("the paste must settle before the following control");
            };
            let mut expected = payload.to_vec();
            expected.extend_from_slice(&PASTE_END[..split]);
            expected.extend_from_slice(after_idle);
            assert_eq!(
                parser.parse_as_vec(bytes, true),
                vec![InputEvent::Paste(String::from_utf8(expected).unwrap())]
            );
        }
    }

    #[test]
    fn nested_starts_and_false_end_prefixes_stay_inside_the_same_paste() {
        let payload = "nested\x1b[200~\x1b[201x\x1b[20\x1bPft-mobile;attach;99\x1b\\\x1b";
        let mut input = PASTE_START.to_vec();
        input.extend_from_slice(payload.as_bytes());
        input.extend_from_slice(PASTE_END);
        input.extend_from_slice(b"\x1bPft-mobile;panes\x1b\\");
        let expected = vec![
            ParsedItem::Input(InputEvent::Paste(payload.to_string())),
            parsed_control("panes", ""),
        ];
        assert_eq!(parse_chunks(&[&input]), expected);
        assert_eq!(parse_chunks(&input.chunks(1).collect::<Vec<_>>()), expected);
    }
}
