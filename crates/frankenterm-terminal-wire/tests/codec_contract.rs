use std::sync::atomic::{AtomicUsize, Ordering};

use frankenterm_terminal_wire::{
    BoundedVec, Capabilities, Capability, Cell, CellStyle, ClientFrame, ClientMessage, Color,
    Cursor, CursorShape, Delta, Dimensions, GridPatch, Hello, Id, Line, MAX_COLUMNS,
    MAX_FRAME_BYTES, MAX_PASTE_BYTES, Mutation, PaneIdentity, PasteRequest, Scope, ServerFrame,
    ServerMessage, Sha256, TerminalModes, Text, Underline, WireError, WireI64, WireU64,
    decode_client, decode_server, encode_client, encode_server,
};
use serde::{Deserialize, Deserializer};

const ASSET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

// These are independent wire fixtures, not output produced by our encoder.
// In particular, the opaque IDs exceed JavaScript's exact integer range.
const HELLO_GOLDEN: &[u8] = br#"{"protocol":"ft.terminal.v1","session":null,"message":{"type":"hello","payload":{"host":"host:9007199254740993","hello_id":"18446744073709551615","asset":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","required":["terminal_state"],"optional":["input","resize"]}}}"#;
const CLIENT_PING: &[u8] = br#"{"protocol":"ft.terminal.v1","session":"9007199254740993","message":{"type":"ping","payload":{"nonce":"18446744073709551615"}}}"#;
const SERVER_PONG: &[u8] = br#"{"protocol":"ft.terminal.v1","session":"9007199254740993","message":{"type":"pong","payload":{"nonce":"18446744073709551615"}}}"#;

fn hello_frame() -> ClientFrame {
    ClientFrame::new(
        None,
        ClientMessage::Hello(Hello {
            host: Id::new("host:9007199254740993").unwrap(),
            hello_id: Id::new("18446744073709551615").unwrap(),
            asset: Sha256::new(ASSET).unwrap(),
            required: Capabilities::try_from(vec![Capability::TerminalState]).unwrap(),
            optional: Capabilities::try_from(vec![Capability::Input, Capability::Resize]).unwrap(),
        }),
    )
    .unwrap()
}

fn scope() -> Scope {
    Scope {
        identity: PaneIdentity {
            host: Id::new("host-a").unwrap(),
            pane: Id::new("550e8400-e29b-41d4-a716-446655440000").unwrap(),
            incarnation: Id::new("incarnation-0001").unwrap(),
        },
        epoch: WireU64(9_007_199_254_740_993),
    }
}

fn ping_json(nonce_json: &str) -> String {
    let mut json = String::from(
        r#"{"protocol":"ft.terminal.v1","session":"9007199254740993","message":{"type":"ping","payload":{"nonce":"#,
    );
    json.push_str(nonce_json);
    json.push_str("}}}");
    json
}

fn paste_json(text_json: &str) -> String {
    let mut json = String::from(
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"paste","payload":{"effect":{"scope":{"identity":{"host":"host-a","pane":"pane-a","incarnation":"incarnation-a"},"epoch":"1"},"request_id":"paste-a","lease_fence":"1"},"text":"#,
    );
    json.push_str(text_json);
    json.push_str("}}}");
    json
}

#[test]
fn hello_golden_preserves_exact_ids_and_capability_spelling() {
    let decoded = decode_client(HELLO_GOLDEN).unwrap();
    assert_eq!(decoded.protocol.as_str(), "ft.terminal.v1");
    assert!(decoded.session.is_none());
    let ClientMessage::Hello(hello) = decoded.message else {
        panic!("the hello fixture decoded into a different message");
    };
    assert_eq!(hello.host.as_str(), "host:9007199254740993");
    assert_eq!(hello.hello_id.as_str(), "18446744073709551615");
    assert_eq!(hello.asset.as_str(), ASSET);
    assert_eq!(hello.required.as_slice(), &[Capability::TerminalState]);
    assert_eq!(
        hello.optional.as_slice(),
        &[Capability::Input, Capability::Resize]
    );

    // Construct independently rather than re-encoding the decoded fixture.
    assert_eq!(encode_client(&hello_frame()).unwrap(), HELLO_GOLDEN);
}

#[test]
fn integer_strings_preserve_the_full_unsigned_and_signed_ranges() {
    for (decimal, expected) in [
        ("0", 0),
        ("9007199254740993", 9_007_199_254_740_993),
        ("18446744073709551615", u64::MAX),
    ] {
        assert_eq!(WireU64::parse(decimal), Ok(WireU64(expected)));
        let json = format!("\"{decimal}\"");
        assert_eq!(
            serde_json::from_str::<WireU64>(&json).unwrap(),
            WireU64(expected)
        );
        assert_eq!(serde_json::to_string(&WireU64(expected)).unwrap(), json);
    }

    for (decimal, expected) in [
        ("0", 0),
        ("-1", -1),
        ("-9007199254740993", -9_007_199_254_740_993),
        ("9223372036854775807", i64::MAX),
        ("-9223372036854775808", i64::MIN),
    ] {
        assert_eq!(WireI64::parse(decimal), Ok(WireI64(expected)));
        let json = format!("\"{decimal}\"");
        assert_eq!(
            serde_json::from_str::<WireI64>(&json).unwrap(),
            WireI64(expected)
        );
        assert_eq!(serde_json::to_string(&WireI64(expected)).unwrap(), json);
    }

    let client_ping = decode_client(CLIENT_PING).unwrap();
    assert_eq!(
        client_ping.session.as_ref().unwrap().as_str(),
        "9007199254740993"
    );
    let ClientMessage::Ping(value) = client_ping.message else {
        panic!("the ping fixture decoded into a different message");
    };
    assert_eq!(value.nonce, WireU64(u64::MAX));

    let server_pong = decode_server(SERVER_PONG).unwrap();
    assert_eq!(
        server_pong.session.as_ref().unwrap().as_str(),
        "9007199254740993"
    );
    let ServerMessage::Pong(value) = server_pong.message else {
        panic!("the pong fixture decoded into a different message");
    };
    assert_eq!(value.nonce, WireU64(u64::MAX));
}

#[test]
fn noncanonical_decimal_strings_and_overflow_are_rejected() {
    for decimal in [
        "",
        "00",
        "01",
        "+0",
        "+1",
        "-0",
        "-1",
        "1.0",
        "1e3",
        " 1",
        "1 ",
        "1\n",
        "１",
        "١",
        "18446744073709551616",
        "999999999999999999999",
    ] {
        assert_eq!(
            WireU64::parse(decimal),
            Err(WireError::InvalidNumber),
            "unsigned lexeme {decimal:?}"
        );
        let quoted = serde_json::to_string(decimal).unwrap();
        assert!(serde_json::from_str::<WireU64>(&quoted).is_err());
        assert_eq!(
            decode_client(ping_json(&quoted).as_bytes()),
            Err(WireError::Malformed)
        );
    }

    for decimal in [
        "",
        "00",
        "01",
        "-00",
        "-01",
        "-0",
        "+0",
        "+1",
        "--1",
        "-+1",
        "1.0",
        "-1e3",
        " -1",
        "-1 ",
        "１",
        "9223372036854775808",
        "-9223372036854775809",
        "-18446744073709551616",
    ] {
        assert_eq!(
            WireI64::parse(decimal),
            Err(WireError::InvalidNumber),
            "signed lexeme {decimal:?}"
        );
        let quoted = serde_json::to_string(decimal).unwrap();
        assert!(serde_json::from_str::<WireI64>(&quoted).is_err());
    }
}

#[test]
fn json_numeric_tokens_never_substitute_for_exact_decimal_strings() {
    for json in [
        "0",
        "1",
        "9007199254740993",
        "18446744073709551615",
        "-9223372036854775808",
        "1.0",
        "1e3",
        "true",
        "null",
        "[]",
        "{}",
    ] {
        assert!(serde_json::from_str::<WireU64>(json).is_err());
        assert!(serde_json::from_str::<WireI64>(json).is_err());
        assert_eq!(
            decode_client(ping_json(json).as_bytes()),
            Err(WireError::Malformed)
        );
    }
}

#[test]
fn identifiers_are_exact_bounded_strings_not_coerced_numbers() {
    let numeric_id = "18446744073709551615";
    let id: Id = serde_json::from_str(r#""18446744073709551615""#).unwrap();
    assert_eq!(id.as_str(), numeric_id);
    assert!(serde_json::from_str::<Id>(numeric_id).is_err());

    let at_limit = "a".repeat(128);
    assert_eq!(Id::new(&at_limit).unwrap().as_str(), at_limit);
    assert_eq!(Id::new(&"a".repeat(129)), Err(WireError::InvalidIdentifier));
    for invalid in ["", "two words", "a/b", "a\nb", "é", "💻"] {
        assert_eq!(Id::new(invalid), Err(WireError::InvalidIdentifier));
        assert!(serde_json::from_str::<Id>(&serde_json::to_string(invalid).unwrap()).is_err());
    }
}

#[test]
fn text_limits_count_decoded_utf8_bytes_including_json_escapes() {
    for json in [r#""éé""#, r#""\u00e9\u00e9""#] {
        let text: Text<4> = serde_json::from_str(json).unwrap();
        assert_eq!(text.as_str(), "éé");
        assert_eq!(text.len(), 4);
        assert!(serde_json::from_str::<Text<3>>(json).is_err());
    }
    for json in [r#""🚀""#, r#""\ud83d\ude80""#] {
        let text: Text<4> = serde_json::from_str(json).unwrap();
        assert_eq!(text.as_str(), "🚀");
        assert_eq!(text.len(), 4);
        assert!(serde_json::from_str::<Text<3>>(json).is_err());
    }
    assert!(serde_json::from_str::<Text<4>>(r#""\u00e9\u00e9x""#).is_err());
    assert!(serde_json::from_str::<Text<4>>(r#""\ud83d""#).is_err());
    assert!(Text::<4>::new("ééx").is_err());
    assert!(Text::<0>::new("").unwrap().is_empty());
    assert!(Text::<0>::new("a").is_err());
}

#[test]
fn paste_frame_applies_the_decoded_byte_cap_before_exposing_text() {
    let escaped = "\\u00e9".repeat(MAX_PASTE_BYTES / 2);
    let at_limit = paste_json(&format!("\"{escaped}\""));
    assert!(at_limit.len() < MAX_FRAME_BYTES);
    let decoded = decode_client(at_limit.as_bytes()).unwrap();
    let ClientMessage::Paste(paste) = decoded.message else {
        panic!("the paste fixture decoded into a different message");
    };
    assert_eq!(paste.text.len(), MAX_PASTE_BYTES);
    assert_eq!(paste.text.as_str().chars().count(), MAX_PASTE_BYTES / 2);

    let one_byte_over = paste_json(&format!("\"{escaped}\\u0061\""));
    assert!(one_byte_over.len() < MAX_FRAME_BYTES);
    assert_eq!(
        decode_client(one_byte_over.as_bytes()),
        Err(WireError::Malformed)
    );
}

#[test]
fn unknown_and_duplicate_fields_are_rejected_at_each_envelope_layer() {
    let invalid_clients = [
        r#"{"protocol":"ft.terminal.v1","session":"s","unexpected":true,"message":{"type":"ping","payload":{"nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","protocol":"ft.terminal.v1","session":"s","message":{"type":"ping","payload":{"nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","session":"other","message":{"type":"ping","payload":{"nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"ping","unexpected":true,"payload":{"nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"ping","type":"ping","payload":{"nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"ping","payload":{"nonce":"1"},"payload":{"nonce":"2"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"ping","payload":{"nonce":"1","nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"ping","payload":{"nonce":"1","extra":"not negotiated"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"future_ping","payload":{"nonce":"1"}}}"#,
    ];
    for (index, json) in invalid_clients.iter().enumerate() {
        assert_eq!(
            decode_client(json.as_bytes()),
            Err(WireError::Malformed),
            "client fixture {index}"
        );
    }

    let invalid_servers = [
        r#"{"protocol":"ft.terminal.v1","session":"s","unexpected":true,"message":{"type":"pong","payload":{"nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","protocol":"ft.terminal.v1","session":"s","message":{"type":"pong","payload":{"nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"pong","type":"pong","payload":{"nonce":"1"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"pong","payload":{"nonce":"1","nonce":"2"}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"pong","payload":{"nonce":"1","extra":true}}}"#,
        r#"{"protocol":"ft.terminal.v1","session":"s","message":{"type":"future_pong","payload":{"nonce":"1"}}}"#,
    ];
    for (index, json) in invalid_servers.iter().enumerate() {
        assert_eq!(
            decode_server(json.as_bytes()),
            Err(WireError::Malformed),
            "server fixture {index}"
        );
    }
}

#[test]
fn only_one_complete_json_document_is_accepted() {
    for suffix in [
        b"{}".as_slice(),
        b"\nnull".as_slice(),
        b"garbage".as_slice(),
    ] {
        let mut client = CLIENT_PING.to_vec();
        client.extend_from_slice(suffix);
        assert_eq!(decode_client(&client), Err(WireError::Malformed));
        let mut server = SERVER_PONG.to_vec();
        server.extend_from_slice(suffix);
        assert_eq!(decode_server(&server), Err(WireError::Malformed));
    }

    for malformed in [
        b"".as_slice(),
        b"null".as_slice(),
        b"[]".as_slice(),
        b"{}".as_slice(),
        b"{".as_slice(),
        b"\xff".as_slice(),
    ] {
        assert_eq!(decode_client(malformed), Err(WireError::Malformed));
        assert_eq!(decode_server(malformed), Err(WireError::Malformed));
    }
}

#[test]
fn deeply_nested_payload_before_discriminator_is_rejected() {
    // Payload-before-type requires the adjacent-tag deserializer to inspect
    // the value before it can select a variant. Keep the normal JSON recursion
    // limit active instead of accepting arbitrarily nested hostile content.
    let nested = format!("{}0{}", "[".repeat(256), "]".repeat(256));
    for (kind, server) in [("ping", false), ("pong", true)] {
        let mut json =
            String::from(r#"{"protocol":"ft.terminal.v1","session":"s","message":{"payload":"#);
        json.push_str(&nested);
        json.push_str(",\"type\":\"");
        json.push_str(kind);
        json.push_str("\"}}");
        assert!(json.len() < MAX_FRAME_BYTES);
        if server {
            assert_eq!(decode_server(json.as_bytes()), Err(WireError::Malformed));
        } else {
            assert_eq!(decode_client(json.as_bytes()), Err(WireError::Malformed));
        }
    }
}

#[test]
fn frame_byte_admission_precedes_json_and_utf8_parsing() {
    for byte in [b'x', 0xff] {
        let oversized = vec![byte; MAX_FRAME_BYTES + 1];
        assert_eq!(decode_client(&oversized), Err(WireError::FrameTooLarge));
        assert_eq!(decode_server(&oversized), Err(WireError::FrameTooLarge));
        assert_eq!(
            decode_client(&oversized[..MAX_FRAME_BYTES]),
            Err(WireError::Malformed)
        );
        assert_eq!(
            decode_server(&oversized[..MAX_FRAME_BYTES]),
            Err(WireError::Malformed)
        );
    }
}

#[test]
fn exact_frame_limit_accepts_a_complete_document_and_counts_trailing_space() {
    let mut client = CLIENT_PING.to_vec();
    client.resize(MAX_FRAME_BYTES, b' ');
    assert!(decode_client(&client).is_ok());
    client.push(b' ');
    assert_eq!(decode_client(&client), Err(WireError::FrameTooLarge));

    let mut server = SERVER_PONG.to_vec();
    server.resize(MAX_FRAME_BYTES, b' ');
    assert!(decode_server(&server).is_ok());
    server.push(b' ');
    assert_eq!(decode_server(&server), Err(WireError::FrameTooLarge));
}

static CONSTRUCTED_ELEMENTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct CountedElement;

impl<'de> Deserialize<'de> for CountedElement {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let _ = u8::deserialize(deserializer)?;
        CONSTRUCTED_ELEMENTS.fetch_add(1, Ordering::SeqCst);
        Ok(Self)
    }
}

#[test]
fn excess_collection_elements_are_rejected_without_constructing_another_value() {
    CONSTRUCTED_ELEMENTS.store(0, Ordering::SeqCst);
    assert!(serde_json::from_str::<BoundedVec<CountedElement, 2>>("[1,2,3]").is_err());
    assert_eq!(CONSTRUCTED_ELEMENTS.load(Ordering::SeqCst), 2);

    let mut values = BoundedVec::<u8, 2>::new();
    values.try_push(1).unwrap();
    values.try_push(2).unwrap();
    assert_eq!(values.try_push(3), Err(WireError::LimitExceeded));
    assert_eq!(values.as_slice(), &[1, 2]);
    assert!(serde_json::from_str::<BoundedVec<u8, 0>>("[1]").is_err());
    assert!(
        serde_json::from_str::<BoundedVec<u8, 0>>("[]")
            .unwrap()
            .is_empty()
    );
}

fn oversized_delta() -> ServerFrame {
    // One grapheme per cell, with enough combining marks to exercise the
    // frame cap while every individual text and collection remains valid.
    let grapheme = format!("a{}", "\u{0301}".repeat(127));
    assert_eq!(grapheme.len(), 255);
    let text = Text::<256>::new(&grapheme).unwrap();
    let style = CellStyle {
        foreground: Color::Default,
        background: Color::Default,
        underline_color: Color::Default,
        underline: Underline::None,
        attributes: 0,
    };
    let mut lines = Vec::new();
    for row in 0_u16..2 {
        let mut cells = Vec::new();
        for column in 0..MAX_COLUMNS {
            cells.push(Cell {
                column,
                text: text.clone(),
                width: 1,
                style: style.clone(),
                hyperlink: None,
                image: None,
            });
        }
        lines.push(Line {
            row,
            stable_row: WireI64(-2 + i64::from(row)),
            cells: BoundedVec::try_from(cells).unwrap(),
        });
    }
    ServerFrame::new(
        Some(Id::new("session-a").unwrap()),
        ServerMessage::Delta(Delta {
            scope: scope(),
            base_sequence: WireU64(8),
            sequence: WireU64(9),
            modes: TerminalModes {
                alternate_screen: false,
                mouse_tracking: false,
                bracketed_paste: true,
            },
            patch: GridPatch {
                dimensions: Dimensions {
                    columns: MAX_COLUMNS,
                    rows: 2,
                    pixel_width: 0,
                    pixel_height: 0,
                },
                cursor: Cursor {
                    column: 0,
                    row: 0,
                    visible: true,
                    blink: false,
                    shape: CursorShape::Block,
                },
                lines: BoundedVec::try_from(lines).unwrap(),
                hyperlinks: BoundedVec::new(),
                images: BoundedVec::new(),
            },
        }),
    )
    .unwrap()
}

#[test]
fn oversized_encoding_returns_an_error_without_exposing_partial_frame_bytes() {
    let paste = ClientFrame::new(
        Some(Id::new("session-a").unwrap()),
        ClientMessage::Paste(PasteRequest {
            effect: Mutation {
                scope: scope(),
                request_id: Id::new("paste-a").unwrap(),
                lease_fence: WireU64(1),
            },
            // Escaping expands these otherwise bounded paste bytes sixfold.
            text: Text::new(&"\0".repeat(MAX_PASTE_BYTES)).unwrap(),
        }),
    )
    .unwrap();
    assert_eq!(paste.validate(), Ok(()));
    assert_eq!(encode_client(&paste), Err(WireError::FrameTooLarge));

    let delta = oversized_delta();
    assert_eq!(delta.validate(), Ok(()));
    assert_eq!(encode_server(&delta), Err(WireError::FrameTooLarge));

    // The API returns either a complete owned frame or an error. Verify an
    // overflow does not poison later ordinary encodes in either direction.
    assert_eq!(encode_client(&hello_frame()).unwrap(), HELLO_GOLDEN);
    let pong = decode_server(SERVER_PONG).unwrap();
    assert_eq!(encode_server(&pong).unwrap(), SERVER_PONG);
}
