#![no_main]

use arbitrary::{Arbitrary, Unstructured};
use frankenterm_core::tmux_control_protocol::{
    ParseError, QuoteKind, SplitDirection, TmuxCommand, parse_command,
};
use libfuzzer_sys::fuzz_target;

const MAX_DATA_BYTES: usize = 4 * 1024;
const MAX_LINE_CHARS: usize = 1_024;
const MAX_ATOM_CHARS: usize = 64;
const MAX_ITEMS: usize = 16;

#[derive(Arbitrary, Debug)]
enum FuzzInput<'a> {
    Raw(&'a [u8]),
    Structured(StructuredCommand<'a>),
}

#[derive(Arbitrary, Debug)]
enum StructuredCommand<'a> {
    SendKeys {
        alias: bool,
        target: OptionalAtom<'a>,
        keys: Vec<AtomInput<'a>>,
    },
    ListWindows {
        alias: bool,
        target: OptionalAtom<'a>,
    },
    ListWindowsUnsupported {
        alias: bool,
    },
    ListSessions {
        alias: bool,
    },
    ListSessionsUnsupported {
        alias: bool,
    },
    CapturePane {
        alias: bool,
        target: OptionalAtom<'a>,
        print: bool,
    },
    SplitWindow {
        alias: bool,
        target: OptionalAtom<'a>,
        direction: DirectionInput,
    },
    NewSession {
        alias: bool,
        name: OptionalAtom<'a>,
    },
    AttachSession {
        alias: bool,
        target: OptionalAtom<'a>,
    },
    Detach {
        alias: bool,
        trailing: Vec<AtomInput<'a>>,
    },
    PipePane {
        alias: bool,
        target: OptionalAtom<'a>,
        only_if_not_piped: bool,
        command: Vec<AtomInput<'a>>,
    },
    CopyMode {
        target: OptionalAtom<'a>,
        args: Vec<AtomInput<'a>>,
    },
    Unknown {
        verb: AtomInput<'a>,
        args: Vec<AtomInput<'a>>,
    },
    MissingOptionValue(MissingOptionCommand),
    UnterminatedQuote {
        kind: QuoteKindInput,
        payload: &'a [u8],
    },
    BadEscape {
        observed: EscapeInput,
    },
}

#[derive(Arbitrary, Debug)]
enum OptionalAtom<'a> {
    None,
    Some(AtomInput<'a>),
}

#[derive(Arbitrary, Debug)]
struct AtomInput<'a> {
    style: AtomStyle,
    bytes: &'a [u8],
}

#[derive(Arbitrary, Debug)]
enum AtomStyle {
    Bare,
    SingleQuoted,
    DoubleQuoted,
    DoubleEscaped,
}

#[derive(Arbitrary, Debug)]
enum DirectionInput {
    None,
    Horizontal,
    Vertical,
    HorizontalThenVertical,
    VerticalThenHorizontal,
}

#[derive(Arbitrary, Debug)]
enum MissingOptionCommand {
    SendKeysTarget,
    ListWindowsTarget,
    CapturePaneTarget,
    SplitWindowTarget,
    NewSessionName,
    AttachSessionTarget,
    PipePaneTarget,
    CopyModeTarget,
}

#[derive(Arbitrary, Debug)]
enum QuoteKindInput {
    Single,
    Double,
}

#[derive(Arbitrary, Debug)]
enum EscapeInput {
    X,
    R,
    Zero,
    Unicode,
    Bell,
}

#[derive(Debug)]
struct TokenValue {
    token: String,
    value: String,
}

#[derive(Debug)]
struct Case {
    line: String,
    expected: ExpectedOutcome,
}

#[derive(Debug)]
enum ExpectedOutcome {
    Command(TmuxCommand),
    Error(ParseError),
    UnterminatedQuote(QuoteKind),
    BadEscape(char),
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_DATA_BYTES {
        return;
    }

    run_raw_case(data);

    let mut unstructured = Unstructured::new(data);
    if let Ok(input) = FuzzInput::arbitrary(&mut unstructured) {
        input.run();
    }
});

impl<'a> FuzzInput<'a> {
    fn run(self) {
        match self {
            Self::Raw(bytes) => run_raw_case(bytes),
            Self::Structured(command) => command.run(),
        }
    }
}

impl<'a> StructuredCommand<'a> {
    fn run(self) {
        let case = self.into_case();
        let result = parse_command(&case.line);
        assert_result_invariants(&case.line, &result);
        assert_expected(&case.line, result, case.expected);
    }

    fn into_case(self) -> Case {
        match self {
            Self::SendKeys {
                alias,
                target,
                keys,
            } => {
                let mut parts = vec![verb(alias, "send-keys", "send")];
                let target = push_optional_option(&mut parts, "-t", target, "pane");
                let mut expected_keys = Vec::new();
                for (index, key) in keys.into_iter().take(MAX_ITEMS).enumerate() {
                    let value = key.token_value(&format!("key{index}"));
                    parts.push(value.token);
                    expected_keys.push(value.value);
                }
                command_case(
                    parts,
                    TmuxCommand::SendKeys {
                        target,
                        keys: expected_keys,
                    },
                )
            }
            Self::ListWindows { alias, target } => {
                let mut parts = vec![verb(alias, "list-windows", "lsw")];
                let target_session = push_optional_option(&mut parts, "-t", target, "session");
                command_case(parts, TmuxCommand::ListWindows { target_session })
            }
            Self::ListWindowsUnsupported { alias } => Case {
                line: join_parts([verb(alias, "list-windows", "lsw"), "-F".to_string()]),
                expected: ExpectedOutcome::Error(ParseError::UnsupportedOption {
                    command: "list-windows".to_string(),
                    option: "-F".to_string(),
                }),
            },
            Self::ListSessions { alias } => command_case(
                [verb(alias, "list-sessions", "ls")],
                TmuxCommand::ListSessions,
            ),
            Self::ListSessionsUnsupported { alias } => Case {
                line: join_parts([verb(alias, "list-sessions", "ls"), "-F".to_string()]),
                expected: ExpectedOutcome::Error(ParseError::UnsupportedOption {
                    command: "list-sessions".to_string(),
                    option: "-F".to_string(),
                }),
            },
            Self::CapturePane {
                alias,
                target,
                print,
            } => {
                let mut parts = vec![verb(alias, "capture-pane", "capturep")];
                let target = push_optional_option(&mut parts, "-t", target, "pane");
                if print {
                    parts.push("-p".to_string());
                }
                command_case(parts, TmuxCommand::CapturePane { target, print })
            }
            Self::SplitWindow {
                alias,
                target,
                direction,
            } => {
                let mut parts = vec![verb(alias, "split-window", "splitw")];
                let target = push_optional_option(&mut parts, "-t", target, "pane");
                let expected_direction = direction.push_to(&mut parts);
                command_case(
                    parts,
                    TmuxCommand::SplitWindow {
                        target,
                        direction: expected_direction,
                    },
                )
            }
            Self::NewSession { alias, name } => {
                let mut parts = vec![verb(alias, "new-session", "new")];
                let name = push_optional_option(&mut parts, "-s", name, "session");
                command_case(parts, TmuxCommand::NewSession { name })
            }
            Self::AttachSession { alias, target } => {
                let mut parts = vec![verb(alias, "attach-session", "attach")];
                let target = push_optional_option(&mut parts, "-t", target, "session");
                command_case(parts, TmuxCommand::AttachSession { target })
            }
            Self::Detach { alias, trailing } => {
                let mut parts = vec![verb(alias, "detach", "detach-client")];
                for (index, token) in trailing.into_iter().take(MAX_ITEMS).enumerate() {
                    parts.push(token.token_value(&format!("ignored{index}")).token);
                }
                command_case(parts, TmuxCommand::Detach)
            }
            Self::PipePane {
                alias,
                target,
                only_if_not_piped,
                command,
            } => {
                let mut parts = vec![verb(alias, "pipe-pane", "pipep")];
                let target = push_optional_option(&mut parts, "-t", target, "pane");
                if only_if_not_piped {
                    parts.push("-o".to_string());
                }
                let mut expected_command = Vec::new();
                for (index, token) in command.into_iter().take(MAX_ITEMS).enumerate() {
                    let value = token.token_value(&format!("cmd{index}"));
                    parts.push(value.token);
                    expected_command.push(value.value);
                }
                command_case(
                    parts,
                    TmuxCommand::PipePane {
                        target,
                        only_if_not_piped,
                        command: expected_command,
                    },
                )
            }
            Self::CopyMode { target, args } => {
                let mut parts = vec!["copy-mode".to_string()];
                let target = push_optional_option(&mut parts, "-t", target, "pane");
                let mut expected_args = Vec::new();
                for (index, token) in args.into_iter().take(MAX_ITEMS).enumerate() {
                    let value = token.token_value(&format!("arg{index}"));
                    parts.push(value.token);
                    expected_args.push(value.value);
                }
                command_case(
                    parts,
                    TmuxCommand::CopyMode {
                        target,
                        args: expected_args,
                    },
                )
            }
            Self::Unknown { verb, args } => {
                let mut expected_args = Vec::new();
                let unknown_verb = format!("unknown-{}", verb.safe_value("verb"));
                let mut parts = vec![unknown_verb.clone()];
                for (index, token) in args.into_iter().take(MAX_ITEMS).enumerate() {
                    let value = token.token_value(&format!("arg{index}"));
                    parts.push(value.token);
                    expected_args.push(value.value);
                }
                command_case(
                    parts,
                    TmuxCommand::Unknown {
                        verb: unknown_verb,
                        args: expected_args,
                    },
                )
            }
            Self::MissingOptionValue(command) => {
                let (line, option) = command.line_and_option();
                Case {
                    line: line.to_string(),
                    expected: ExpectedOutcome::Error(ParseError::MissingOptionValue {
                        option: option.to_string(),
                    }),
                }
            }
            Self::UnterminatedQuote { kind, payload } => {
                let quote_kind = kind.quote_kind();
                let payload = safe_value(payload, "payload");
                let line = match quote_kind {
                    QuoteKind::Single => format!("send-keys '{payload}"),
                    QuoteKind::Double => format!("send-keys \"{payload}"),
                };
                Case {
                    line,
                    expected: ExpectedOutcome::UnterminatedQuote(quote_kind),
                }
            }
            Self::BadEscape { observed } => {
                let observed = observed.char();
                Case {
                    line: format!("send-keys \"bad\\{observed}\""),
                    expected: ExpectedOutcome::BadEscape(observed),
                }
            }
        }
    }
}

impl<'a> OptionalAtom<'a> {
    fn token_value(self, fallback: &str) -> Option<TokenValue> {
        match self {
            Self::None => None,
            Self::Some(atom) => Some(atom.token_value(fallback)),
        }
    }
}

impl<'a> AtomInput<'a> {
    fn safe_value(&self, fallback: &str) -> String {
        safe_value(self.bytes, fallback)
    }

    fn token_value(self, fallback: &str) -> TokenValue {
        let base = safe_value(self.bytes, fallback);
        match self.style {
            AtomStyle::Bare => TokenValue {
                token: base.clone(),
                value: base,
            },
            AtomStyle::SingleQuoted => TokenValue {
                token: format!("'{base}'"),
                value: base,
            },
            AtomStyle::DoubleQuoted => TokenValue {
                token: format!("\"{}\"", escape_double_quoted(&base)),
                value: base,
            },
            AtomStyle::DoubleEscaped => {
                let value = format!("{base}\n\t\"\\");
                TokenValue {
                    token: format!("\"{}\\n\\t\\\"\\\\\"", escape_double_quoted(&base)),
                    value,
                }
            }
        }
    }
}

impl DirectionInput {
    fn push_to(self, parts: &mut Vec<String>) -> Option<SplitDirection> {
        match self {
            Self::None => None,
            Self::Horizontal => {
                parts.push("-h".to_string());
                Some(SplitDirection::Horizontal)
            }
            Self::Vertical => {
                parts.push("-v".to_string());
                Some(SplitDirection::Vertical)
            }
            Self::HorizontalThenVertical => {
                parts.push("-h".to_string());
                parts.push("-v".to_string());
                Some(SplitDirection::Vertical)
            }
            Self::VerticalThenHorizontal => {
                parts.push("-v".to_string());
                parts.push("-h".to_string());
                Some(SplitDirection::Horizontal)
            }
        }
    }
}

impl MissingOptionCommand {
    fn line_and_option(self) -> (&'static str, &'static str) {
        match self {
            Self::SendKeysTarget => ("send-keys -t", "-t"),
            Self::ListWindowsTarget => ("list-windows -t", "-t"),
            Self::CapturePaneTarget => ("capture-pane -t", "-t"),
            Self::SplitWindowTarget => ("split-window -t", "-t"),
            Self::NewSessionName => ("new-session -s", "-s"),
            Self::AttachSessionTarget => ("attach-session -t", "-t"),
            Self::PipePaneTarget => ("pipe-pane -t", "-t"),
            Self::CopyModeTarget => ("copy-mode -t", "-t"),
        }
    }
}

impl QuoteKindInput {
    fn quote_kind(self) -> QuoteKind {
        match self {
            Self::Single => QuoteKind::Single,
            Self::Double => QuoteKind::Double,
        }
    }
}

impl EscapeInput {
    fn char(self) -> char {
        match self {
            Self::X => 'x',
            Self::R => 'r',
            Self::Zero => '0',
            Self::Unicode => 'u',
            Self::Bell => 'a',
        }
    }
}

fn run_raw_case(bytes: &[u8]) {
    let line = limited_lossy(bytes, MAX_LINE_CHARS);
    let result = parse_command(&line);
    assert_result_invariants(&line, &result);
}

fn push_optional_option(
    parts: &mut Vec<String>,
    option: &str,
    value: OptionalAtom<'_>,
    fallback: &str,
) -> Option<String> {
    let value = value.token_value(fallback)?;
    parts.push(option.to_string());
    parts.push(value.token);
    Some(value.value)
}

fn command_case(parts: impl IntoIterator<Item = String>, command: TmuxCommand) -> Case {
    Case {
        line: join_parts(parts),
        expected: ExpectedOutcome::Command(command),
    }
}

fn verb(alias: bool, canonical: &str, alias_name: &str) -> String {
    if alias {
        alias_name.to_string()
    } else {
        canonical.to_string()
    }
}

fn join_parts(parts: impl IntoIterator<Item = String>) -> String {
    parts.into_iter().collect::<Vec<_>>().join(" ")
}

fn assert_expected(line: &str, result: Result<TmuxCommand, ParseError>, expected: ExpectedOutcome) {
    match expected {
        ExpectedOutcome::Command(command) => {
            assert_eq!(result, Ok(command), "line: {line:?}");
        }
        ExpectedOutcome::Error(error) => {
            assert_eq!(result, Err(error), "line: {line:?}");
        }
        ExpectedOutcome::UnterminatedQuote(kind) => match result {
            Err(ParseError::UnterminatedQuote { kind: actual, .. }) => {
                assert_eq!(actual, kind, "line: {line:?}");
            }
            other => panic!("expected unterminated quote {kind:?} for {line:?}, got {other:?}"),
        },
        ExpectedOutcome::BadEscape(observed) => match result {
            Err(ParseError::BadEscape {
                observed: actual, ..
            }) => {
                assert_eq!(actual, observed, "line: {line:?}");
            }
            other => panic!("expected bad escape {observed:?} for {line:?}, got {other:?}"),
        },
    }
}

fn assert_result_invariants(line: &str, result: &Result<TmuxCommand, ParseError>) {
    match result {
        Ok(command) => assert_command_invariants(command),
        Err(error) => assert_error_invariants(line, error),
    }
}

fn assert_command_invariants(command: &TmuxCommand) {
    match command {
        TmuxCommand::SendKeys { target, keys } => {
            assert_optional_string_bound(target);
            assert_string_vec_bound(keys);
        }
        TmuxCommand::ListWindows { target_session } => {
            assert_optional_string_bound(target_session);
        }
        TmuxCommand::ListSessions | TmuxCommand::Detach => {}
        TmuxCommand::CapturePane { target, .. }
        | TmuxCommand::SplitWindow { target, .. }
        | TmuxCommand::AttachSession { target }
        | TmuxCommand::PipePane { target, .. }
        | TmuxCommand::CopyMode { target, .. } => {
            assert_optional_string_bound(target);
        }
        TmuxCommand::NewSession { name } => {
            assert_optional_string_bound(name);
        }
        TmuxCommand::Unknown { verb, args } => {
            assert!(!verb.is_empty());
            assert_string_bound(verb);
            assert_string_vec_bound(args);
        }
    }

    match command {
        TmuxCommand::SplitWindow { direction, .. } => match direction {
            Some(SplitDirection::Horizontal) | Some(SplitDirection::Vertical) | None => {}
        },
        TmuxCommand::PipePane { command, .. } | TmuxCommand::CopyMode { args: command, .. } => {
            assert_string_vec_bound(command);
        }
        _ => {}
    }
}

fn assert_error_invariants(line: &str, error: &ParseError) {
    match error {
        ParseError::Empty => {}
        ParseError::UnterminatedQuote { opened_at, .. } => {
            assert!(
                *opened_at <= line.len(),
                "quote offset {opened_at} outside line {line:?}"
            );
        }
        ParseError::BadEscape { at, observed } => {
            assert!(
                *at <= line.len(),
                "escape offset {at} outside line {line:?}"
            );
            assert!(
                !matches!(observed, '\\' | '"' | 'n' | 't'),
                "supported escape rejected in {line:?}"
            );
        }
        ParseError::MissingOptionValue { option } => {
            assert!(!option.is_empty(), "empty missing-value option");
            assert!(option.starts_with('-'), "missing-value option {option:?}");
        }
        ParseError::UnsupportedOption { command, option } => {
            assert!(!command.is_empty(), "empty unsupported-option command");
            assert!(!option.is_empty(), "empty unsupported option");
            assert!(option.starts_with('-'), "unsupported option {option:?}");
        }
    }
}

fn assert_optional_string_bound(value: &Option<String>) {
    if let Some(value) = value {
        assert_string_bound(value);
    }
}

fn assert_string_vec_bound(values: &[String]) {
    assert!(values.len() <= MAX_ITEMS || values.len() <= MAX_LINE_CHARS);
    for value in values {
        assert_string_bound(value);
    }
}

fn assert_string_bound(value: &str) {
    assert!(
        value.len() <= MAX_DATA_BYTES,
        "parsed token exceeded input cap: {} bytes",
        value.len()
    );
}

fn limited_lossy(bytes: &[u8], max_chars: usize) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(max_chars)
        .collect()
}

fn safe_value(bytes: &[u8], fallback: &str) -> String {
    let mut output = String::new();
    for ch in limited_lossy(bytes, MAX_ATOM_CHARS).chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' | ':' | '/' => output.push(ch),
            _ => output.push('_'),
        }
    }

    let output = output.trim_matches('_');
    let mut output = if output.is_empty() {
        fallback.to_string()
    } else {
        output.to_string()
    };
    if output.starts_with('-') {
        output.insert(0, 'v');
    }
    output
}

fn escape_double_quoted(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            other => escaped.push(other),
        }
    }
    escaped
}
