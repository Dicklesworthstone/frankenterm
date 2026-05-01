//! Tmux control-mode protocol wire format.
//!
//! **Bead:** [BR-TERM-EMULATOR-UPLIFT-2.5.3] / `ft-hs5f6`.
//! **Design doc:** [`docs/design/crash-safe-scrollback.md`]
//! (../../../../docs/design/crash-safe-scrollback.md) — section
//! "tmux compatibility matrix".
//! **Parent epic:** ft-2okh0.5.
//!
//! # What this module ships
//!
//! The pure wire-format substrate for parsing + serializing the
//! tmux *control-mode* protocol that drop-in tmux tooling speaks.
//! Tier-1 RPC subset per the parent bead's compatibility matrix:
//!
//! - `tmux send-keys`
//! - `tmux list-windows`
//! - `tmux list-sessions`
//! - `tmux capture-pane`
//! - `tmux split-window`
//! - `tmux new-session`
//! - `tmux attach-session`
//! - `tmux detach`
//!
//! What this module is: the line-oriented codec — parse a
//! command line into a [`TmuxCommand`], format a [`TmuxResponse`]
//! back into the tmux block-line wire shape (`%begin`/`%end`/`%error`).
//!
//! What this module is NOT (filed as `ft-hs5f6.cont` follow-up):
//!
//! - The actual socket listener at `$TMUX_TMPDIR/tmux-<uid>/default`.
//! - Handler dispatch — translating a parsed [`TmuxCommand`] into
//!   ft's mux state (the dispatcher hooks the parsed command into
//!   the existing `wezterm.rs` surface).
//! - Notification stream (the asynchronous `%output`,
//!   `%window-add`, etc. lines tmux clients subscribe to).
//!
//! # Wire format
//!
//! Tmux's control mode is line-oriented over a UNIX socket. A
//! client sends one command per line. The server replies with a
//! block:
//!
//! ```text
//! %begin <unix-time> <command-id> <flags>
//! <command output, possibly empty, possibly multi-line>
//! %end <unix-time> <command-id> <flags>
//! ```
//!
//! On error, the server uses `%error` instead of `%end`. The
//! `<unix-time>` is seconds since epoch when the server processed
//! the command; the `<command-id>` is a monotonically increasing
//! integer the server assigns; `<flags>` is reserved (zero in
//! practice).
//!
//! The command line itself uses tmux's argument syntax:
//!
//! - Arguments are space-separated.
//! - Single-quoted strings preserve everything literally
//!   (including spaces).
//! - Double-quoted strings honor `\\` and `\"` escapes.
//! - Bare arguments use the same word-boundary rules as a POSIX
//!   shell *minus* glob expansion.
//!
//! This module implements parsing + serialization for both halves
//! of the conversation.
//!
//! # Why a typed [`TmuxCommand`] enum
//!
//! Each Tier-1 command has a well-defined argument shape. Pinning
//! the shape in a typed enum means downstream handlers (filed as
//! `ft-hs5f6.cont`) can pattern-match exhaustively, and a future
//! Tier-2 command (e.g. `pipe-pane`) is a new variant the
//! compiler forces every match site to handle.

use std::fmt;

/// A parsed tmux control-mode command. Variants cover the Tier-1
/// subset; everything else parses into [`Self::Unknown`] so the
/// server can return a graceful "unsupported" response rather than
/// a parser error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxCommand {
    /// `tmux send-keys -t <target> <key>...` — inject input into
    /// a pane.
    SendKeys {
        target: Option<String>,
        keys: Vec<String>,
    },
    /// `tmux list-windows` — enumerate windows in a session.
    ListWindows { target_session: Option<String> },
    /// `tmux list-sessions` — enumerate sessions on the server.
    ListSessions,
    /// `tmux capture-pane -t <target>` — read pane content into
    /// the buffer.
    CapturePane {
        target: Option<String>,
        /// `-p`: print to stdout instead of buffering.
        print: bool,
    },
    /// `tmux split-window -t <target> [-h | -v]` — spawn a pane.
    SplitWindow {
        target: Option<String>,
        /// `-h` (horizontal split) → `Some(Direction::Horizontal)`;
        /// `-v` (vertical split) → `Some(Direction::Vertical)`;
        /// neither → `None` (tmux default: vertical).
        direction: Option<SplitDirection>,
    },
    /// `tmux new-session [-s <name>]` — spawn a session.
    NewSession { name: Option<String> },
    /// `tmux attach-session [-t <target>]` — reattach.
    AttachSession { target: Option<String> },
    /// `tmux detach` — detach the current client.
    Detach,
    /// Anything else — the verb + raw args. Server returns a
    /// graceful "unsupported" `%error` response.
    Unknown { verb: String, args: Vec<String> },
}

/// Split-window direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Errors during command parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Empty input (no verb).
    Empty,
    /// Unterminated quoted string (single or double quote).
    UnterminatedQuote {
        /// Position (byte offset) where the quote opened.
        opened_at: usize,
        kind: QuoteKind,
    },
    /// Bad escape in a double-quoted string (e.g. `"\x"`).
    BadEscape { at: usize, observed: char },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteKind {
    Single,
    Double,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "empty command line"),
            Self::UnterminatedQuote { opened_at, kind } => {
                let kc = match kind {
                    QuoteKind::Single => '\'',
                    QuoteKind::Double => '"',
                };
                write!(f, "unterminated {kc}-quote opened at offset {opened_at}")
            }
            Self::BadEscape { at, observed } => {
                write!(f, "bad escape `\\{observed}` at offset {at}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// Parse a single tmux command line into a [`TmuxCommand`].
///
/// Trailing newlines are tolerated (the line-reader strips them
/// before this function is called, but we accept them defensively).
pub fn parse_command(line: &str) -> Result<TmuxCommand, ParseError> {
    let line = line.trim_end_matches(['\r', '\n']);
    let tokens = tokenize(line)?;
    let mut iter = tokens.into_iter();
    let verb = iter.next().ok_or(ParseError::Empty)?;
    let args: Vec<String> = iter.collect();
    Ok(match verb.as_str() {
        "send-keys" => parse_send_keys(args),
        "list-windows" => parse_list_windows(args),
        "list-sessions" => TmuxCommand::ListSessions,
        "capture-pane" => parse_capture_pane(args),
        "split-window" => parse_split_window(args),
        "new-session" => parse_new_session(args),
        "attach-session" => parse_attach_session(args),
        "detach" | "detach-client" => TmuxCommand::Detach,
        _ => TmuxCommand::Unknown { verb, args },
    })
}

/// Tokenize a tmux command line into argv-shaped tokens. Honors
/// single quotes (literal), double quotes (with `\\` / `\"` /
/// `\n` / `\t` escapes), and shell-style word splitting on
/// unquoted whitespace.
fn tokenize(line: &str) -> Result<Vec<String>, ParseError> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if !in_token && c.is_whitespace() {
            i += 1;
            continue;
        }
        match c {
            '\'' => {
                let opened_at = i;
                i += 1;
                let mut found = false;
                while i < bytes.len() {
                    let cc = bytes[i] as char;
                    if cc == '\'' {
                        i += 1;
                        found = true;
                        break;
                    }
                    current.push(cc);
                    i += 1;
                }
                if !found {
                    return Err(ParseError::UnterminatedQuote {
                        opened_at,
                        kind: QuoteKind::Single,
                    });
                }
                in_token = true;
            }
            '"' => {
                let opened_at = i;
                i += 1;
                let mut found = false;
                while i < bytes.len() {
                    let cc = bytes[i] as char;
                    if cc == '"' {
                        i += 1;
                        found = true;
                        break;
                    }
                    if cc == '\\' && i + 1 < bytes.len() {
                        let next = bytes[i + 1] as char;
                        match next {
                            '\\' => current.push('\\'),
                            '"' => current.push('"'),
                            'n' => current.push('\n'),
                            't' => current.push('\t'),
                            other => {
                                return Err(ParseError::BadEscape {
                                    at: i,
                                    observed: other,
                                });
                            }
                        }
                        i += 2;
                        continue;
                    }
                    current.push(cc);
                    i += 1;
                }
                if !found {
                    return Err(ParseError::UnterminatedQuote {
                        opened_at,
                        kind: QuoteKind::Double,
                    });
                }
                in_token = true;
            }
            c if c.is_whitespace() => {
                if in_token {
                    tokens.push(std::mem::take(&mut current));
                    in_token = false;
                }
                i += 1;
            }
            c => {
                current.push(c);
                in_token = true;
                i += 1;
            }
        }
    }
    if in_token {
        tokens.push(current);
    }
    Ok(tokens)
}

fn parse_send_keys(args: Vec<String>) -> TmuxCommand {
    let mut target: Option<String> = None;
    let mut keys: Vec<String> = Vec::new();
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "-t" => target = iter.next(),
            _ => keys.push(a),
        }
    }
    TmuxCommand::SendKeys { target, keys }
}

fn parse_list_windows(args: Vec<String>) -> TmuxCommand {
    let mut target_session: Option<String> = None;
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        if a == "-t" {
            target_session = iter.next();
        }
    }
    TmuxCommand::ListWindows { target_session }
}

fn parse_capture_pane(args: Vec<String>) -> TmuxCommand {
    let mut target: Option<String> = None;
    let mut print = false;
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "-t" => target = iter.next(),
            "-p" => print = true,
            _ => {} // ignore unknown flags for now
        }
    }
    TmuxCommand::CapturePane { target, print }
}

fn parse_split_window(args: Vec<String>) -> TmuxCommand {
    let mut target: Option<String> = None;
    let mut direction: Option<SplitDirection> = None;
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "-t" => target = iter.next(),
            "-h" => direction = Some(SplitDirection::Horizontal),
            "-v" => direction = Some(SplitDirection::Vertical),
            _ => {}
        }
    }
    TmuxCommand::SplitWindow { target, direction }
}

fn parse_new_session(args: Vec<String>) -> TmuxCommand {
    let mut name: Option<String> = None;
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        if a == "-s" {
            name = iter.next();
        }
    }
    TmuxCommand::NewSession { name }
}

fn parse_attach_session(args: Vec<String>) -> TmuxCommand {
    let mut target: Option<String> = None;
    let mut iter = args.into_iter();
    while let Some(a) = iter.next() {
        if a == "-t" {
            target = iter.next();
        }
    }
    TmuxCommand::AttachSession { target }
}

/// A control-mode response. Wire shape:
///
/// ```text
/// %begin <unix-time> <command-id> <flags>
/// <output_lines>
/// %end <unix-time> <command-id> <flags>          (success)
///   OR
/// %error <unix-time> <command-id> <flags>        (failure)
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxResponse {
    /// Unix time (seconds since epoch) when the server processed
    /// the command.
    pub timestamp_secs: u64,
    /// Server-assigned monotonic command ID.
    pub command_id: u64,
    /// Reserved; tmux uses zero in practice.
    pub flags: u32,
    /// Output lines produced by the command.
    pub output: Vec<String>,
    /// `Ok(())` → emit `%end`; `Err(reason)` → emit `%error`. The
    /// reason string is logged but not part of the wire frame
    /// (tmux's `%error` is an unadorned marker; the reason lives
    /// in the preceding `output` lines).
    pub outcome: Result<(), String>,
}

impl TmuxResponse {
    /// Encode the response into the wire format. Trailing newline
    /// included so the caller can write to a `Write` directly.
    #[must_use]
    pub fn encode(&self) -> String {
        let trailer_marker = if self.outcome.is_ok() {
            "%end"
        } else {
            "%error"
        };
        let mut out = String::new();
        out.push_str(&format!(
            "%begin {} {} {}\n",
            self.timestamp_secs, self.command_id, self.flags
        ));
        for line in &self.output {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str(&format!(
            "{trailer_marker} {} {} {}\n",
            self.timestamp_secs, self.command_id, self.flags
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_send_keys_with_target_and_payload() {
        let cmd = parse_command("send-keys -t mysession ls Enter").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::SendKeys {
                target: Some("mysession".to_string()),
                keys: vec!["ls".to_string(), "Enter".to_string()],
            }
        );
    }

    #[test]
    fn parse_send_keys_without_target_uses_none() {
        let cmd = parse_command("send-keys Enter").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::SendKeys {
                target: None,
                keys: vec!["Enter".to_string()],
            }
        );
    }

    #[test]
    fn parse_list_sessions_takes_no_args() {
        assert_eq!(
            parse_command("list-sessions").unwrap(),
            TmuxCommand::ListSessions
        );
    }

    #[test]
    fn parse_list_windows_with_session_target() {
        let cmd = parse_command("list-windows -t backend").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::ListWindows {
                target_session: Some("backend".to_string()),
            }
        );
    }

    #[test]
    fn parse_capture_pane_print_flag() {
        let cmd = parse_command("capture-pane -t %1 -p").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::CapturePane {
                target: Some("%1".to_string()),
                print: true,
            }
        );
    }

    #[test]
    fn parse_split_window_horizontal() {
        let cmd = parse_command("split-window -t main -h").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::SplitWindow {
                target: Some("main".to_string()),
                direction: Some(SplitDirection::Horizontal),
            }
        );
    }

    #[test]
    fn parse_split_window_vertical() {
        let cmd = parse_command("split-window -v").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::SplitWindow {
                target: None,
                direction: Some(SplitDirection::Vertical),
            }
        );
    }

    #[test]
    fn parse_new_session_with_name() {
        let cmd = parse_command("new-session -s scratch").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::NewSession {
                name: Some("scratch".to_string()),
            }
        );
    }

    #[test]
    fn parse_attach_session_with_target() {
        let cmd = parse_command("attach-session -t main").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::AttachSession {
                target: Some("main".to_string()),
            }
        );
    }

    #[test]
    fn parse_detach_and_detach_client_aliases() {
        assert_eq!(parse_command("detach").unwrap(), TmuxCommand::Detach);
        assert_eq!(parse_command("detach-client").unwrap(), TmuxCommand::Detach);
    }

    #[test]
    fn parse_unknown_verb_preserves_raw_args() {
        let cmd = parse_command("kill-server foo bar").unwrap();
        assert_eq!(
            cmd,
            TmuxCommand::Unknown {
                verb: "kill-server".to_string(),
                args: vec!["foo".to_string(), "bar".to_string()],
            }
        );
    }

    #[test]
    fn parse_empty_line_errors() {
        assert_eq!(parse_command(""), Err(ParseError::Empty));
        assert_eq!(parse_command("   "), Err(ParseError::Empty));
    }

    #[test]
    fn parse_tolerates_trailing_newline() {
        assert_eq!(parse_command("detach\n").unwrap(), TmuxCommand::Detach);
        assert_eq!(parse_command("detach\r\n").unwrap(), TmuxCommand::Detach);
    }

    #[test]
    fn tokenize_single_quoted_arg_preserves_spaces() {
        let cmd = parse_command("send-keys -t main 'echo hello world'").unwrap();
        if let TmuxCommand::SendKeys { keys, .. } = cmd {
            assert_eq!(keys, vec!["echo hello world".to_string()]);
        } else {
            panic!("expected SendKeys");
        }
    }

    #[test]
    fn tokenize_double_quoted_arg_honors_escapes() {
        let cmd = parse_command(r#"send-keys "line1\nline2""#).unwrap();
        if let TmuxCommand::SendKeys { keys, .. } = cmd {
            assert_eq!(keys, vec!["line1\nline2".to_string()]);
        } else {
            panic!("expected SendKeys");
        }
    }

    #[test]
    fn tokenize_unterminated_single_quote_errors() {
        let err = parse_command("send-keys 'oops").unwrap_err();
        assert!(matches!(
            err,
            ParseError::UnterminatedQuote {
                kind: QuoteKind::Single,
                ..
            }
        ));
    }

    #[test]
    fn tokenize_unterminated_double_quote_errors() {
        let err = parse_command(r#"send-keys "oops"#).unwrap_err();
        assert!(matches!(
            err,
            ParseError::UnterminatedQuote {
                kind: QuoteKind::Double,
                ..
            }
        ));
    }

    #[test]
    fn tokenize_bad_escape_errors() {
        let err = parse_command(r#"send-keys "\x""#).unwrap_err();
        assert!(matches!(err, ParseError::BadEscape { observed: 'x', .. }));
    }

    #[test]
    fn response_encode_success_uses_end_trailer() {
        let resp = TmuxResponse {
            timestamp_secs: 1_700_000_000,
            command_id: 7,
            flags: 0,
            output: vec!["window 0".to_string(), "window 1".to_string()],
            outcome: Ok(()),
        };
        let wire = resp.encode();
        assert_eq!(
            wire,
            "%begin 1700000000 7 0\nwindow 0\nwindow 1\n%end 1700000000 7 0\n"
        );
    }

    #[test]
    fn response_encode_error_uses_error_trailer() {
        let resp = TmuxResponse {
            timestamp_secs: 1_700_000_001,
            command_id: 8,
            flags: 0,
            output: vec!["unsupported command: kill-server".to_string()],
            outcome: Err("unsupported".to_string()),
        };
        let wire = resp.encode();
        assert_eq!(
            wire,
            "%begin 1700000001 8 0\nunsupported command: kill-server\n%error 1700000001 8 0\n"
        );
    }

    #[test]
    fn response_encode_with_empty_output_is_well_formed() {
        let resp = TmuxResponse {
            timestamp_secs: 1,
            command_id: 1,
            flags: 0,
            output: vec![],
            outcome: Ok(()),
        };
        assert_eq!(resp.encode(), "%begin 1 1 0\n%end 1 1 0\n");
    }
}
