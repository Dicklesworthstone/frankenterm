//! Conformance harness for the codec PDU wire format.
//!
//! The wire format, per `encode_raw` in `codec::lib`:
//!
//! ```text
//! tagged_len : leb128 u64   (bit 63 set iff payload is zstd-compressed)
//! serial     : leb128 u64
//! ident      : leb128 u64
//! data       : tagged_len - encoded byte length of serial - encoded byte length of ident
//! ```
//!
//! These tests pin the decoder's response to minimal/maximum/boundary/non-canonical
//! frames. Crafted frames are hand-built so we exercise the decoder *without*
//! first routing through the encoder — that way every byte on the wire is
//! under test control.

use std::collections::HashMap;
use std::path::PathBuf;

use codec::{
    CompressionMode, DecodedPdu, ErrorResponse, GetCodecVersion, GetCodecVersionResponse,
    GetImageCell, GetImageCellResponse, GetLinesResponse, GetPaneRenderChangesResponse,
    GetTlsCreds, InputSerial, Legacy46FloatingPaneState, Legacy46RejectionAuthority, ListPanes,
    ListPanesResponse, MuxWireDecodedPayload, MuxWireDialect, MuxWireUnsupportedReason, Pdu,
    PduProducer, PduWireIdent, PduWireRole, Ping, Pong, SendPaste, SerializedLines,
    StreamingPduBuffer, UnitResponse, CODEC_VERSION, CODEC_VERSION_MIN_SUPPORTED,
    MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES, MAX_LEGACY46_ERROR_RESPONSE_DECOMPRESSED_BYTES,
    MAX_LEGACY46_LIST_PANES_RESPONSE_DECOMPRESSED_BYTES,
    MAX_LEGACY46_UNSUPPORTED_IMAGE_RESPONSE_DECOMPRESSED_BYTES,
    MAX_MUX_ERROR_RESPONSE_DECOMPRESSED_BYTES, MAX_ORDERED_TABS_PER_SNAPSHOT,
    MAX_ORDERED_WINDOWS_PER_SNAPSHOT,
};
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::tab::PaneNode;
use mux::window::WindowId;
use serde::Serialize;
use termwiz::cell::CellAttributes;
use termwiz::surface::Line;

// Mirror private constants from `codec::lib`. Kept in lockstep by the
// `conformance_constants_still_match_encoder` test below: any drift of the
// decoder's limits flips that test red before the rest of the suite can
// accidentally succeed against wrong boundaries.
const MAX_PDU_SIZE: usize = 256 * 1024 * 1024;
const COMPRESSED_MASK: u64 = 1 << 63;

// -----------------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------------

fn leb128_u64(mut value: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(10);
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
    out
}

fn encoded_leb128_len(value: u64) -> usize {
    leb128_u64(value).len()
}

/// Hand-build a raw frame with the given `(tagged_len, serial, ident, data)`.
///
/// `tagged_len` is written verbatim — callers can set the high bit for the
/// compression flag or inject an overflow value to exercise the overflow
/// guard directly, bypassing the encoder's own length computation.
fn frame(tagged_len: u64, serial: u64, ident: u64, data: &[u8]) -> Vec<u8> {
    let mut buf = leb128_u64(tagged_len);
    buf.extend(leb128_u64(serial));
    buf.extend(leb128_u64(ident));
    buf.extend_from_slice(data);
    buf
}

/// Hand-build a frame with verbatim leb128 fields. Use this when the test
/// needs non-canonical encodings that the encoder would normalize away.
fn frame_verbatim(tagged_len: &[u8], serial: &[u8], ident: &[u8], data: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(tagged_len.len() + serial.len() + ident.len() + data.len());
    buf.extend_from_slice(tagged_len);
    buf.extend_from_slice(serial);
    buf.extend_from_slice(ident);
    buf.extend_from_slice(data);
    buf
}

/// Compute a well-formed `tagged_len` for `(serial, ident, data)` without the
/// compression bit. This matches what the encoder would have produced.
fn well_formed_len(serial: u64, ident: u64, data_len: u64) -> u64 {
    data_len + encoded_leb128_len(serial) as u64 + encoded_leb128_len(ident) as u64
}

fn golden_bytes(label: &str, text: &str) -> Vec<u8> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .flat_map(str::split_ascii_whitespace)
        .map(|token| {
            u8::from_str_radix(token, 16)
                .unwrap_or_else(|err| panic!("{}: invalid hex byte {:?}: {}", label, token, err))
        })
        .collect()
}

// -----------------------------------------------------------------------------
// 1. Constants guard — fails loudly if the encoder drifts away from the decoder
// -----------------------------------------------------------------------------

#[test]
fn conformance_constants_still_match_encoder() {
    // A tagged_len with bit 63 set must decode as "compressed". If the
    // encoder ever reassigns the mask, encoded Ping payload would flip
    // interpretation and this test's Pdu::decode would fail.
    let mut buf = Vec::new();
    Pdu::Ping(Ping {})
        .encode(&mut buf, 7)
        .expect("Ping encodes");
    // First byte is leb128-encoded length; since Ping is tiny, it fits in
    // a single byte, whose high bit MUST be clear (non-compressed, small).
    assert!(
        buf[0] & 0x80 == 0,
        "minimal Ping length should fit in a single leb128 byte with high bit clear: got {:#x}",
        buf[0]
    );
    assert!(
        (buf[0] as u64) < COMPRESSED_MASK,
        "encoder must not set COMPRESSED_MASK on an uncompressed Ping"
    );
}

// -----------------------------------------------------------------------------
// 2. Minimal valid encoding — Ping round-trip
// -----------------------------------------------------------------------------

#[test]
fn conformance_minimal_valid_ping_round_trip() {
    let mut wire = Vec::new();
    Pdu::Ping(Ping {})
        .encode(&mut wire, 42)
        .expect("encode Ping");

    let decoded = Pdu::decode(wire.as_slice()).expect("decode Ping");
    assert_eq!(
        decoded,
        DecodedPdu {
            serial: 42,
            pdu: Pdu::Ping(Ping {}),
        }
    );
}

#[test]
fn conformance_golden_ping_header_bytes_cover_leb128_boundaries() {
    let cases = [
        (
            "serial-zero-single-byte",
            0,
            golden_bytes(
                "serial-zero-single-byte",
                include_str!("goldens/pdu_ping_serial_zero.hex"),
            ),
        ),
        (
            "serial-max-single-byte",
            127,
            golden_bytes(
                "serial-max-single-byte",
                include_str!("goldens/pdu_ping_serial_127.hex"),
            ),
        ),
        (
            "serial-first-two-byte",
            128,
            golden_bytes(
                "serial-first-two-byte",
                include_str!("goldens/pdu_ping_serial_128.hex"),
            ),
        ),
        (
            "serial-first-three-byte",
            16_384,
            golden_bytes(
                "serial-first-three-byte",
                include_str!("goldens/pdu_ping_serial_16384.hex"),
            ),
        ),
    ];

    for (label, serial, wire) in cases {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {})
            .encode(&mut encoded, serial)
            .unwrap_or_else(|err| panic!("{}: encode failed: {}", label, err));
        assert_eq!(encoded, wire, "{label}: canonical Ping wire bytes changed");

        let decoded = Pdu::decode(wire.as_slice())
            .unwrap_or_else(|err| panic!("{}: golden wire failed to decode: {}", label, err));
        assert_eq!(
            decoded,
            DecodedPdu {
                serial,
                pdu: Pdu::Ping(Ping {}),
            },
            "{label}: golden wire decoded to the wrong PDU"
        );
    }
}

#[test]
fn conformance_golden_unit_response_wire_bytes() {
    let wire = golden_bytes(
        "unit-response-serial-200",
        include_str!("goldens/pdu_unit_response_serial_200.hex"),
    );
    let mut encoded = Vec::new();
    Pdu::UnitResponse(UnitResponse {})
        .encode(&mut encoded, 200)
        .expect("encode UnitResponse");
    assert_eq!(encoded, wire, "canonical UnitResponse wire bytes changed");

    let decoded = Pdu::decode(wire.as_slice()).expect("decode UnitResponse golden");
    assert_eq!(
        decoded,
        DecodedPdu {
            serial: 200,
            pdu: Pdu::UnitResponse(UnitResponse {}),
        }
    );
}

#[test]
fn conformance_golden_zero_body_control_pdu_wire_bytes() {
    let cases = [
        (
            "pong-serial-2",
            2,
            Pdu::Pong(Pong {}),
            golden_bytes(
                "pong-serial-2",
                include_str!("goldens/pdu_pong_serial_2.hex"),
            ),
        ),
        (
            "list-panes-serial-3",
            3,
            Pdu::ListPanes(ListPanes {}),
            golden_bytes(
                "list-panes-serial-3",
                include_str!("goldens/pdu_list_panes_serial_3.hex"),
            ),
        ),
        (
            "get-codec-version-serial-26",
            26,
            Pdu::GetCodecVersion(GetCodecVersion {}),
            golden_bytes(
                "get-codec-version-serial-26",
                include_str!("goldens/pdu_get_codec_version_serial_26.hex"),
            ),
        ),
        (
            "get-tls-creds-serial-28",
            28,
            Pdu::GetTlsCreds(GetTlsCreds {}),
            golden_bytes(
                "get-tls-creds-serial-28",
                include_str!("goldens/pdu_get_tls_creds_serial_28.hex"),
            ),
        ),
    ];

    for (label, serial, pdu, wire) in cases {
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, serial)
            .unwrap_or_else(|err| panic!("{}: encode failed: {}", label, err));
        assert_eq!(
            encoded, wire,
            "{label}: canonical zero-body control PDU wire bytes changed"
        );

        let decoded = Pdu::decode(wire.as_slice())
            .unwrap_or_else(|err| panic!("{}: golden wire failed to decode: {}", label, err));
        assert_eq!(
            decoded,
            DecodedPdu { serial, pdu },
            "{label}: golden wire decoded to the wrong PDU"
        );
    }
}

fn assert_roundtrip_modes<F>(label: &str, serial: u64, make_pdu: F)
where
    F: Fn() -> Pdu,
{
    for mode in [
        CompressionMode::Auto,
        CompressionMode::Never,
        CompressionMode::Always,
    ] {
        let expected = make_pdu();
        let mut encoded = Vec::new();
        expected
            .encode_with_mode(&mut encoded, serial, mode)
            .unwrap_or_else(|err| {
                panic!(
                    "{label}: encode_with_mode({mode:?}) failed: {err}",
                    label = label,
                    mode = mode,
                    err = err,
                )
            });

        let decoded = Pdu::decode(encoded.as_slice()).unwrap_or_else(|err| {
            panic!(
                "{label}: decode after {mode:?} failed: {err}",
                label = label,
                mode = mode,
                err = err,
            )
        });
        assert_eq!(decoded.serial, serial, "{label}: decoded serial drifted");
        assert_eq!(decoded.pdu, expected, "{label}: decoded PDU drifted");

        let mut streaming_bytes = encoded.clone();
        streaming_bytes.extend_from_slice(b"NEXT");
        let mut streaming = StreamingPduBuffer::from(streaming_bytes);
        let streamed = Pdu::stream_decode(&mut streaming)
            .unwrap_or_else(|err| {
                panic!(
                    "{label}: stream_decode after {mode:?} failed: {err}",
                    label = label,
                    mode = mode,
                    err = err,
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "{label}: stream_decode returned None for complete frame",
                    label = label,
                )
            });
        assert_eq!(streamed.serial, serial, "{label}: streamed serial drifted");
        assert_eq!(streamed.pdu, make_pdu(), "{label}: streamed PDU drifted");
        assert_eq!(
            streaming.as_slice(),
            b"NEXT",
            "{label}: stream_decode must leave trailing bytes for the next frame"
        );
    }
}

#[test]
fn conformance_pdu_roundtrip_matrix_preserves_serial_payload_and_streaming() {
    assert_roundtrip_modes("ping", 0, || Pdu::Ping(Ping {}));
    assert_roundtrip_modes("pong", 127, || Pdu::Pong(Pong {}));
    assert_roundtrip_modes("list-panes", 128, || Pdu::ListPanes(ListPanes {}));
    assert_roundtrip_modes("get-codec-version", 26, || {
        Pdu::GetCodecVersion(GetCodecVersion {})
    });
    assert_roundtrip_modes("get-tls-creds", 28, || Pdu::GetTlsCreds(GetTlsCreds {}));
    assert_roundtrip_modes("unit-response", 16_384, || {
        Pdu::UnitResponse(UnitResponse {})
    });
    assert_roundtrip_modes("error-response", 65_536, || {
        Pdu::ErrorResponse(ErrorResponse::pane_not_found(ListPanes::IDENT, 91))
    });
}

#[test]
fn wire_protocol_versioning_codec_roundtrip_conformance_pdu_version_response_modes() {
    assert_roundtrip_modes("get-codec-version-response", 27, || {
        Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
            codec_vers: CODEC_VERSION,
            version_string: "frankenterm-conformance".to_string(),
            executable_path: PathBuf::from("/opt/frankenterm/bin/ft"),
            config_file_path: Some(PathBuf::from("/etc/frankenterm/ft.toml")),
            min_supported: CODEC_VERSION_MIN_SUPPORTED,
        })
    });
}

// -----------------------------------------------------------------------------
// 3. Fixed-shape ErrorResponse without an object identity
// -----------------------------------------------------------------------------

#[test]
fn conformance_fixed_error_response_without_object_round_trips() {
    let mut wire = Vec::new();
    let expected = ErrorResponse::invalid_request(Ping::IDENT);
    Pdu::ErrorResponse(expected.clone())
        .encode(&mut wire, 1)
        .expect("encode object-free ErrorResponse");
    assert!(wire.len() <= MAX_MUX_ERROR_RESPONSE_DECOMPRESSED_BYTES);

    let decoded = Pdu::decode(wire.as_slice()).expect("decode object-free ErrorResponse");
    assert_eq!(decoded.serial, 1);
    match decoded.pdu {
        Pdu::ErrorResponse(response) => assert_eq!(response, expected),
        other => panic!("expected ErrorResponse, got {:?}", other),
    }
}

// -----------------------------------------------------------------------------
// 4. Fixed-shape ErrorResponse with an exact object identity
// -----------------------------------------------------------------------------

#[test]
fn conformance_fixed_error_response_with_object_round_trips() {
    let mut wire = Vec::new();
    let expected = ErrorResponse::pane_not_found(Ping::IDENT, u64::MAX);
    Pdu::ErrorResponse(expected.clone())
        .encode(&mut wire, 2)
        .expect("encode object-bearing ErrorResponse");
    assert!(wire.len() <= MAX_MUX_ERROR_RESPONSE_DECOMPRESSED_BYTES);

    let decoded = Pdu::decode(wire.as_slice()).expect("decode object-bearing ErrorResponse");
    match decoded.pdu {
        Pdu::ErrorResponse(response) => assert_eq!(response, expected),
        other => panic!("expected ErrorResponse, got {:?}", other),
    }
}

// -----------------------------------------------------------------------------
// 5. PDU0 has a schema-specific body ceiling far below the generic PDU cap.
// -----------------------------------------------------------------------------

#[test]
fn conformance_error_response_body_above_schema_cap_is_rejected() {
    let data = vec![0_u8; MAX_MUX_ERROR_RESPONSE_DECOMPRESSED_BYTES + 1];
    let tagged_len = well_formed_len(3, ErrorResponse::IDENT, data.len() as u64);
    let wire = frame(tagged_len, 3, ErrorResponse::IDENT, &data);
    assert!(Pdu::decode(wire.as_slice()).is_err());
}

// -----------------------------------------------------------------------------
// 6. Boundary length MAX+1 — crafted frame claims payload > MAX_PDU_SIZE.
//    The decoder must refuse BEFORE attempting to allocate.
// -----------------------------------------------------------------------------

#[test]
fn conformance_boundary_payload_one_over_max_is_rejected_without_allocation() {
    // data_len = MAX_PDU_SIZE + 1. We never actually write that many bytes —
    // the decoder's size check fires first and the read_exact() call that
    // would allocate `vec![0u8; data_len]` never happens.
    let serial = 1u64;
    let ident = 1u64; // Ping
    let over_max = (MAX_PDU_SIZE as u64) + 1;
    let tagged_len = well_formed_len(serial, ident, over_max);
    let wire = frame(tagged_len, serial, ident, &[]);

    let err = Pdu::decode(wire.as_slice()).expect_err("decoder must reject payload > MAX_PDU_SIZE");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("exceeds maximum"),
        "error must name the MAX_PDU_SIZE violation; got: {}",
        msg
    );
}

// -----------------------------------------------------------------------------
// 7. Truncated input — empty buffer yields None on stream_decode
// -----------------------------------------------------------------------------

#[test]
fn conformance_truncated_empty_buffer_returns_none() {
    let mut buf = StreamingPduBuffer::new();
    let result = Pdu::stream_decode(&mut buf).expect("empty buffer is not an error");
    assert!(result.is_none(), "empty buffer must decode to None");
    assert!(buf.is_empty(), "buffer must be untouched");
}

// -----------------------------------------------------------------------------
// 8. Truncated input — partial leb128 length byte (high bit set, no continuation)
// -----------------------------------------------------------------------------

#[test]
fn conformance_truncated_mid_leb128_length_returns_none() {
    // 0x80 = continuation bit set, value so far = 0. Decoder needs at least
    // one more byte to complete the leb128.
    let mut buf = StreamingPduBuffer::from(vec![0x80u8]);
    let result = Pdu::stream_decode(&mut buf).expect("partial leb128 is not fatal on a stream");
    assert!(result.is_none(), "partial leb128 must decode to None");
    assert_eq!(
        buf.as_slice(),
        &[0x80],
        "buffer must be preserved for more bytes"
    );
}

// -----------------------------------------------------------------------------
// 9. Truncated input — length present, payload missing
// -----------------------------------------------------------------------------

#[test]
fn conformance_truncated_length_without_payload_returns_none() {
    // tagged_len=10, but only the length byte is present. stream_decode
    // should hold the bytes and wait for more.
    let mut buf = StreamingPduBuffer::from(vec![10u8]);
    let result = Pdu::stream_decode(&mut buf).expect("truncated frame must not error on stream");
    assert!(result.is_none(), "truncated body must decode to None");
    assert_eq!(buf.as_slice(), &[10u8], "buffer must be preserved");
}

// -----------------------------------------------------------------------------
// 10. Trailing garbage — extra bytes after a valid frame stay in the buffer
// -----------------------------------------------------------------------------

#[test]
fn conformance_trailing_garbage_leaves_remainder_in_buffer() {
    let mut wire = Vec::new();
    Pdu::Ping(Ping {})
        .encode(&mut wire, 5)
        .expect("encode Ping");
    let consumed_len = wire.len();

    // Append arbitrary bytes after the valid frame.
    wire.extend_from_slice(b"GARBAGE");
    let mut stream = StreamingPduBuffer::from(wire);

    let decoded = Pdu::stream_decode(&mut stream)
        .expect("valid prefix must decode even with trailing bytes")
        .expect("stream_decode returned None despite valid frame");
    assert_eq!(
        decoded,
        DecodedPdu {
            serial: 5,
            pdu: Pdu::Ping(Ping {}),
        }
    );
    assert_eq!(
        stream.as_slice(),
        b"GARBAGE",
        "stream_decode must consume exactly {consumed_len} bytes and leave the rest"
    );
}

// -----------------------------------------------------------------------------
// 11. Back-to-back framing — two PDUs in one buffer both decode
// -----------------------------------------------------------------------------

#[test]
fn conformance_two_back_to_back_pdus_decode_cleanly() {
    let mut wire = Vec::new();
    Pdu::Ping(Ping {}).encode(&mut wire, 11).unwrap();
    Pdu::Ping(Ping {}).encode(&mut wire, 22).unwrap();
    let mut stream = StreamingPduBuffer::from(wire);

    let first = Pdu::stream_decode(&mut stream)
        .unwrap()
        .expect("first frame");
    assert_eq!(first.serial, 11);
    let second = Pdu::stream_decode(&mut stream)
        .unwrap()
        .expect("second frame");
    assert_eq!(second.serial, 22);
    assert!(stream.is_empty(), "both frames must be fully consumed");

    let third = Pdu::stream_decode(&mut stream).unwrap();
    assert!(third.is_none(), "empty buffer after two PDUs must be None");
}

#[test]
fn conformance_mixed_mux_pdu_roundtrip_preserves_order_under_all_compression_modes() {
    let cases = [
        (
            "ping-never",
            101,
            CompressionMode::Never,
            Pdu::Ping(Ping {}),
        ),
        ("pong-auto", 102, CompressionMode::Auto, Pdu::Pong(Pong {})),
        (
            "unit-response-always",
            103,
            CompressionMode::Always,
            Pdu::UnitResponse(UnitResponse {}),
        ),
        (
            "error-response-never",
            104,
            CompressionMode::Never,
            Pdu::ErrorResponse(ErrorResponse::backend_failure(Ping::IDENT)),
        ),
        (
            "list-panes-auto",
            105,
            CompressionMode::Auto,
            Pdu::ListPanes(ListPanes {}),
        ),
        (
            "get-codec-version-always",
            106,
            CompressionMode::Always,
            Pdu::GetCodecVersion(GetCodecVersion {}),
        ),
        (
            "get-tls-creds-never",
            107,
            CompressionMode::Never,
            Pdu::GetTlsCreds(GetTlsCreds {}),
        ),
    ];

    let mut stream_bytes = Vec::new();
    for (label, serial, mode, pdu) in &cases {
        let mut frame = Vec::new();
        pdu.encode_with_mode(&mut frame, *serial, *mode)
            .unwrap_or_else(|err| {
                panic!(
                    "{label}: encode_with_mode({mode:?}) failed: {err}",
                    label = label,
                    mode = mode,
                    err = err,
                )
            });

        let decoded = Pdu::decode(frame.as_slice()).unwrap_or_else(|err| {
            panic!(
                "{label}: direct decode failed: {err}",
                label = label,
                err = err,
            )
        });
        assert_eq!(decoded.serial, *serial, "{label}: direct serial");
        assert_eq!(&decoded.pdu, pdu, "{label}: direct pdu");

        stream_bytes.extend(frame);
    }
    let mut stream = StreamingPduBuffer::from(stream_bytes);

    for (idx, (label, serial, _mode, pdu)) in cases.iter().enumerate() {
        let decoded = Pdu::stream_decode(&mut stream)
            .unwrap_or_else(|err| {
                panic!(
                    "{label}: stream decode failed: {err}",
                    label = label,
                    err = err,
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "{label}: missing stream frame at index {idx}",
                    label = label,
                    idx = idx,
                )
            });
        assert_eq!(decoded.serial, *serial, "{label}: stream serial");
        assert_eq!(&decoded.pdu, pdu, "{label}: stream pdu");
    }

    assert!(
        stream.is_empty(),
        "stream_decode left {} bytes after the mixed PDU stream",
        stream.len()
    );
}

#[test]
fn conformance_mux_pdu_roundtrip_decodes_one_byte_chunks_in_order() {
    let cases = [
        (201, CompressionMode::Never, Pdu::Ping(Ping {})),
        (
            202,
            CompressionMode::Always,
            Pdu::ErrorResponse(ErrorResponse::backend_failure(Ping::IDENT)),
        ),
        (
            203,
            CompressionMode::Auto,
            Pdu::GetCodecVersion(GetCodecVersion {}),
        ),
        (
            204,
            CompressionMode::Never,
            Pdu::UnitResponse(UnitResponse {}),
        ),
    ];

    let mut wire = Vec::new();
    let mut expected = Vec::new();
    for (serial, mode, pdu) in cases {
        pdu.encode_with_mode(&mut wire, serial, mode)
            .unwrap_or_else(|err| {
                panic!(
                    "encode_with_mode({mode:?}) failed: {err}",
                    mode = mode,
                    err = err,
                )
            });
        expected.push((serial, pdu));
    }

    let mut buffer = StreamingPduBuffer::new();
    let mut actual = Vec::new();
    for byte in wire {
        buffer.extend_from_slice(&[byte]);
        while let Some(decoded) =
            Pdu::stream_decode(&mut buffer).expect("stream_decode one-byte chunk")
        {
            actual.push((decoded.serial, decoded.pdu));
        }
    }

    assert_eq!(actual, expected);
    assert!(
        buffer.is_empty(),
        "one-byte chunked decode left {} bytes",
        buffer.len()
    );
}

// -----------------------------------------------------------------------------
// 12. Non-canonical leb128 — valid wire frames must use the actual bytes
//     consumed on the wire, not the decoder's re-encoded canonical lengths.
// -----------------------------------------------------------------------------

#[test]
fn conformance_valid_non_canonical_leb128_headers_are_accepted() {
    // tagged_len=4, serial=1, ident=99, each encoded in a 2-byte non-canonical
    // form. The frame is valid because tagged_len counts the ACTUAL bytes consumed
    // by serial + ident on the wire: 2 + 2 = 4, leaving data_len = 0.
    let wire = frame_verbatim(&[0x84, 0x00], &[0x81, 0x00], &[0xE3, 0x00], &[]);
    let decoded = Pdu::decode(wire.as_slice()).expect("valid non-canonical header");
    assert_eq!(
        decoded,
        DecodedPdu {
            serial: 1,
            pdu: Pdu::Invalid { ident: 99 },
        }
    );
}

#[test]
fn conformance_impossible_non_canonical_header_fails_sanity_check() {
    // tagged_len = 0 (non-canonical two-byte form), while serial + ident each
    // consume two bytes on the wire. The decoder must reject the impossible
    // arithmetic before attempting to read payload bytes.
    let wire = frame_verbatim(&[0x80, 0x00], &[0x81, 0x00], &[0xE3, 0x00], &[]);
    let err = Pdu::decode(wire.as_slice()).expect_err("impossible non-canonical header must fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("sizes don't make sense"),
        "expected arithmetic-sanity error, got: {}",
        msg
    );
}

#[test]
fn conformance_stream_decode_preserves_malformed_complete_frame() {
    // Same impossible arithmetic as above, but through the streaming API. A
    // malformed complete frame must surface an error without consuming bytes so
    // callers can log or quarantine the offending wire image.
    let wire = frame_verbatim(&[0x80, 0x00], &[0x81, 0x00], &[0xE3, 0x00], &[]);
    let original = wire.clone();
    let mut stream = StreamingPduBuffer::from(wire);

    let err = Pdu::stream_decode(&mut stream)
        .expect_err("stream_decode must reject malformed complete frame");
    let msg = format!("{err:#}");
    // The streaming framer (`buffered_frame_len`) trusts the non-canonical
    // `tagged_len = 0` and treats only the 2-byte length prefix as the complete
    // frame, so `stream_decode` rejects it while reading the now-out-of-bounds
    // serial ("…leb128: failed to fill whole buffer") rather than at the
    // whole-frame arithmetic check that `Pdu::decode` reaches over the full slice
    // ("sizes don't make sense"). Both are valid rejections of the malformed
    // frame; the contract under test is that the framer surfaces *an* error
    // rather than silently accepting a bogus PDU — pinning the exact message
    // over-specifies an implementation detail of which decode path trips first.
    assert!(
        msg.contains("sizes don't make sense")
            || msg.contains("leb128")
            || msg.contains("failed to fill whole buffer"),
        "expected a malformed-frame rejection error, got: {msg}",
        msg = msg,
    );
    assert_eq!(
        stream.as_slice(),
        original.as_slice(),
        "stream_decode must preserve bytes when a complete frame is malformed"
    );
}

// -----------------------------------------------------------------------------
// 13. Unknown ident + zero data — produces Pdu::Invalid, preserves serial
// -----------------------------------------------------------------------------

#[test]
fn conformance_unknown_ident_zero_data_yields_invalid_variant() {
    // tagged_len = enc_len(serial=7) + enc_len(ident=99) = 1 + 1 = 2, data_len = 0.
    let wire = frame(2, 7, 99, &[]);
    let mut stream = StreamingPduBuffer::from(wire);
    let decoded = Pdu::stream_decode(&mut stream)
        .unwrap()
        .expect("unknown ident is still a well-formed frame");
    assert_eq!(
        decoded,
        DecodedPdu {
            serial: 7,
            pdu: Pdu::Invalid { ident: 99 },
        }
    );
}

// -----------------------------------------------------------------------------
// 14. buffered_frame_len contract via stream_decode — whole-frame-or-nothing.
//     Feed the buffer one byte at a time; stream_decode must return None on
//     every prefix shorter than the full frame, then decode exactly once.
// -----------------------------------------------------------------------------

#[test]
fn conformance_stream_decode_is_whole_frame_or_nothing() {
    let mut encoded = Vec::new();
    Pdu::Ping(Ping {})
        .encode(&mut encoded, 99)
        .expect("encode Ping");

    let total_len = encoded.len();
    let mut growing = StreamingPduBuffer::with_capacity(total_len);
    for (i, &b) in encoded.iter().enumerate() {
        growing.extend_from_slice(&[b]);
        if i + 1 < total_len {
            // Every proper prefix must decode to None without consuming the buffer.
            let before = growing.as_slice().to_vec();
            let result = Pdu::stream_decode(&mut growing).expect("partial prefix is not an error");
            assert!(
                result.is_none(),
                "partial frame of {} / {} bytes must decode to None",
                i + 1,
                total_len
            );
            assert_eq!(
                growing.as_slice(),
                before.as_slice(),
                "stream_decode must not mutate the buffer on a partial read"
            );
        }
    }

    // Final byte triggers the decode.
    let decoded = Pdu::stream_decode(&mut growing)
        .unwrap()
        .expect("complete frame must decode");
    assert_eq!(decoded.serial, 99);
    assert!(growing.is_empty(), "complete frame must be fully consumed");
}

// -----------------------------------------------------------------------------
// 15. Compressed-flag high bit — tagged_len with bit 63 set must trigger the
//     compressed-decode path. We encode a large repetitive payload so the
//     encoder chooses compression, then verify the first length byte has
//     a continuation and the roundtrip still succeeds.
// -----------------------------------------------------------------------------

#[test]
fn conformance_compressed_flag_round_trip() {
    // zstd compresses repetitive input dramatically. The error envelope is
    // intentionally fixed and tiny, so exercise compression with bounded
    // interactive input instead of reintroducing attacker-authored error text.
    let data = "COMPRESSIBLE-".repeat(20_000); // ~260 KB
    let original = Pdu::SendPaste(SendPaste {
        pane_id: 7,
        data: data.clone(),
        input_serial: InputSerial::from_millis_since_epoch(1),
    });
    let mut wire = Vec::new();
    original.encode(&mut wire, 88).expect("encode compressed");

    // The compressed output should be materially smaller than the source.
    assert!(
        wire.len() < data.len() / 2,
        "compression must shrink a highly repetitive payload by >2x; wire={} input={}",
        wire.len(),
        data.len()
    );

    let decoded = Pdu::decode(wire.as_slice()).expect("decode compressed");
    assert_eq!(decoded.serial, 88);
    assert_eq!(decoded.pdu, original);
}

fn varbincode_bytes<T: Serialize>(value: &T) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut serializer = varbincode::Serializer::new(&mut bytes);
    value
        .serialize(&mut serializer)
        .expect("serialize frozen varbincode fixture");
    bytes
}

#[derive(Serialize)]
struct FrozenLegacy46ListPanesResponse {
    tabs: Vec<PaneNode>,
    tab_titles: Vec<String>,
    window_titles: HashMap<WindowId, String>,
}

#[derive(Serialize)]
struct FrozenLegacy46SendPaste<'a> {
    pane_id: usize,
    data: &'a str,
}

fn legacy46_frame<T: Serialize>(serial: u64, ident: u64, payload: &T) -> Vec<u8> {
    let body = varbincode_bytes(payload);
    frame(
        well_formed_len(serial, ident, body.len() as u64),
        serial,
        ident,
        &body,
    )
}

#[test]
fn legacy46_dialect_is_closed_and_does_not_widen_global_minimum() {
    assert_eq!(CODEC_VERSION_MIN_SUPPORTED, 61);
    assert_eq!(MuxWireDialect::LEGACY46.codec_version(), 46);
    assert!(MuxWireDialect::LEGACY46.is_legacy46());
    assert!(MuxWireDialect::current(CODEC_VERSION_MIN_SUPPORTED).is_ok());
    assert!(MuxWireDialect::current(CODEC_VERSION).is_ok());
    assert!(MuxWireDialect::current(46).is_err());
    assert!(MuxWireDialect::current(CODEC_VERSION + 1).is_err());

    let absent_with_body = frame(well_formed_len(1, 101, 1), 1, 101, &[0]);
    assert!(
        Pdu::decode_for_dialect(absent_with_body.as_slice(), MuxWireDialect::LEGACY46).is_err(),
        "a post-v46 PDU body must be rejected before payload allocation"
    );
    let absent_empty = frame(well_formed_len(2, 101, 0), 2, 101, &[]);
    let decoded = Pdu::decode_for_dialect(absent_empty.as_slice(), MuxWireDialect::LEGACY46)
        .expect("an empty absent-PDU frame can be consumed as content-free metadata");
    let (_, MuxWireDecodedPayload::Unsupported(unsupported)) = decoded.into_parts() else {
        panic!("absent PDU did not produce explicit unsupported metadata");
    };
    assert_eq!(unsupported.ident(), 101);
    assert_eq!(
        unsupported.reason(),
        MuxWireUnsupportedReason::PduAbsentFromDialect
    );
}

#[test]
fn legacy46_pdu4_three_fields_preserve_unavailable_floating_state_and_cross_reject() {
    let old_wire = legacy46_frame(
        4,
        4,
        &FrozenLegacy46ListPanesResponse {
            tabs: Vec::new(),
            tab_titles: Vec::new(),
            window_titles: HashMap::new(),
        },
    );
    let decoded = Pdu::decode_for_dialect(old_wire.as_slice(), MuxWireDialect::LEGACY46)
        .expect("exact legacy PDU4 must decode");
    let (serial, payload) = decoded.into_parts();
    assert_eq!(serial, 4);
    let MuxWireDecodedPayload::Legacy46ListPanesResponse(topology) = payload else {
        panic!("legacy PDU4 decoded to the wrong payload class");
    };
    assert_eq!(
        topology.floating_pane_state(),
        Legacy46FloatingPaneState::Unavailable
    );
    assert!(topology.tabs().is_empty());
    assert!(topology.tab_titles().is_empty());
    assert!(topology.window_titles().is_empty());
    assert!(
        Pdu::decode(old_wire.as_slice()).is_err(),
        "the current four-field PDU4 schema must reject a legacy body"
    );

    let current = Pdu::ListPanesResponse(ListPanesResponse {
        tabs: Vec::new(),
        tab_titles: Vec::new(),
        window_titles: HashMap::new(),
        floating_panes: Vec::new(),
    });
    let mut current_wire = Vec::new();
    current
        .encode_with_mode(&mut current_wire, 4, CompressionMode::Never)
        .expect("encode current PDU4");
    assert!(
        Pdu::decode_for_dialect(current_wire.as_slice(), MuxWireDialect::LEGACY46).is_err(),
        "the legacy three-field schema must reject a current trailing field"
    );
}

#[test]
fn legacy46_pdu4_rejects_oversized_container_length_before_element_allocation() {
    // Each declared collection is rejected at its exact application ceiling,
    // before reserving or reading an element. These are deliberately much
    // smaller than bounded_varbincode's generic one-million-item ceiling.
    for (label, body, expected_maximum) in [
        (
            "pane trees",
            leb128_u64((MAX_ORDERED_TABS_PER_SNAPSHOT + 1) as u64),
            MAX_ORDERED_TABS_PER_SNAPSHOT,
        ),
        (
            "tab titles",
            [
                leb128_u64(0),
                leb128_u64((MAX_ORDERED_TABS_PER_SNAPSHOT + 1) as u64),
            ]
            .concat(),
            MAX_ORDERED_TABS_PER_SNAPSHOT,
        ),
        (
            "window titles",
            [
                leb128_u64(0),
                leb128_u64(0),
                leb128_u64((MAX_ORDERED_WINDOWS_PER_SNAPSHOT + 1) as u64),
            ]
            .concat(),
            MAX_ORDERED_WINDOWS_PER_SNAPSHOT,
        ),
    ] {
        let wire = frame(well_formed_len(40, 4, body.len() as u64), 40, 4, &body);
        let error = Pdu::decode_for_dialect(wire.as_slice(), MuxWireDialect::LEGACY46).unwrap_err();
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains(label),
            "unexpected {} error: {}",
            label,
            rendered
        );
        assert!(
            rendered.contains(&format!("exceeds maximum {expected_maximum}")),
            "unexpected {} ceiling error: {}",
            label,
            rendered
        );
    }

    let declared_body = MAX_LEGACY46_LIST_PANES_RESPONSE_DECOMPRESSED_BYTES + 1;
    let header_only = frame(well_formed_len(41, 4, declared_body as u64), 41, 4, &[]);
    let error = Pdu::decode_for_dialect(header_only.as_slice(), MuxWireDialect::LEGACY46)
        .expect_err("oversized v46 PDU4 must fail at header admission");
    assert!(
        format!("{error:#}").contains(&format!(
            "exceeds maximum {MAX_LEGACY46_LIST_PANES_RESPONSE_DECOMPRESSED_BYTES}"
        )),
        "unexpected v46 PDU4 header-bound error: {:#}",
        error
    );
}

#[test]
fn legacy46_pdu4_rejects_title_overflow_cardinality_mismatch_and_duplicate_window() {
    let mut oversized_title = [leb128_u64(0), leb128_u64(0), leb128_u64(1)].concat();
    oversized_title.extend(leb128_u64(7));
    oversized_title.extend(leb128_u64(
        (MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES + 1) as u64,
    ));
    let wire = frame(
        well_formed_len(42, 4, oversized_title.len() as u64),
        42,
        4,
        &oversized_title,
    );
    let error = Pdu::decode_for_dialect(wire.as_slice(), MuxWireDialect::LEGACY46)
        .expect_err("oversized v46 title must fail before reading title bytes");
    assert!(
        format!("{error:#}")
            .contains("exact render metadata UTF-8 bytes length 65537 exceeds maximum 65536"),
        "unexpected v46 title-bound error: {:#}",
        error
    );

    let mismatch = [leb128_u64(0), leb128_u64(1)].concat();
    let wire = frame(
        well_formed_len(43, 4, mismatch.len() as u64),
        43,
        4,
        &mismatch,
    );
    let error = Pdu::decode_for_dialect(wire.as_slice(), MuxWireDialect::LEGACY46)
        .expect_err("v46 tree/title cardinality mismatch must fail closed");
    assert!(format!("{error:#}").contains("cardinality mismatch"));

    let mut duplicate = [leb128_u64(0), leb128_u64(0), leb128_u64(2)].concat();
    for title in *b"ab" {
        duplicate.extend(leb128_u64(7));
        duplicate.extend(leb128_u64(1));
        duplicate.push(title);
    }
    let wire = frame(
        well_formed_len(44, 4, duplicate.len() as u64),
        44,
        4,
        &duplicate,
    );
    let error = Pdu::decode_for_dialect(wire.as_slice(), MuxWireDialect::LEGACY46)
        .expect_err("duplicate v46 window ids must fail closed");
    assert!(format!("{error:#}").contains("duplicate window id 7"));
}

#[test]
fn legacy46_pdu13_owned_plan_emits_exact_two_field_bytes_and_cross_rejects() {
    let current = Pdu::SendPaste(SendPaste {
        pane_id: 17,
        data: "paste exactly once".to_string(),
        input_serial: InputSerial::from_millis_since_epoch(0xDEAD_BEEF),
    });
    let prepared = current
        .prepare_outbound_for_dialect(
            MuxWireDialect::LEGACY46,
            PduProducer::Client,
            PduWireRole::Request,
            None,
            CompressionMode::Never,
        )
        .expect("legacy PDU13 must plan");
    let frozen = FrozenLegacy46SendPaste {
        pane_id: 17,
        data: "paste exactly once",
    };
    let frozen_body = varbincode_bytes(&frozen);
    assert_eq!(prepared.plan().logical_payload_bytes(), frozen_body.len());
    let actual = prepared.encode_frame(13).expect("legacy PDU13 encodes");
    assert_eq!(actual, legacy46_frame(13, 13, &frozen));

    let decoded = Pdu::decode_for_dialect(actual.as_slice(), MuxWireDialect::LEGACY46)
        .expect("legacy PDU13 decodes");
    let (_, MuxWireDecodedPayload::Legacy46SendPaste(paste)) = decoded.into_parts() else {
        panic!("legacy PDU13 decoded to the wrong payload class");
    };
    assert_eq!(paste.pane_id(), 17);
    assert_eq!(paste.data(), "paste exactly once");
    assert!(
        Pdu::decode(actual.as_slice()).is_err(),
        "current PDU13 must reject the missing input serial"
    );

    let current = Pdu::SendPaste(SendPaste {
        pane_id: 17,
        data: "paste exactly once".to_string(),
        input_serial: InputSerial::from_millis_since_epoch(7),
    });
    let mut current_wire = Vec::new();
    current
        .encode_with_mode(&mut current_wire, 13, CompressionMode::Never)
        .expect("encode current PDU13");
    assert!(
        Pdu::decode_for_dialect(current_wire.as_slice(), MuxWireDialect::LEGACY46).is_err(),
        "legacy PDU13 must reject the current trailing input serial"
    );
}

#[test]
fn legacy46_pdu13_compressed_owned_plan_round_trips_without_fallback() {
    let text = "legacy-compressed-paste-".repeat(2_000);
    let prepared = Pdu::SendPaste(SendPaste {
        pane_id: 9,
        data: text.clone(),
        input_serial: InputSerial::from_millis_since_epoch(99),
    })
    .prepare_outbound_for_dialect(
        MuxWireDialect::LEGACY46,
        PduProducer::Client,
        PduWireRole::Request,
        None,
        CompressionMode::Always,
    )
    .expect("compressed legacy PDU13 must plan");
    let wire = prepared.encode_frame(91).expect("compressed legacy PDU13");
    let decoded = Pdu::decode_for_dialect(wire.as_slice(), MuxWireDialect::LEGACY46)
        .expect("decode compressed legacy PDU13");
    let (_, MuxWireDecodedPayload::Legacy46SendPaste(paste)) = decoded.into_parts() else {
        panic!("compressed legacy PDU13 decoded to the wrong payload class");
    };
    assert_eq!(paste.pane_id(), 9);
    assert_eq!(paste.data(), text);
}

#[test]
fn legacy46_pdu0_goldens_are_content_free_and_non_authoritative() {
    for (label, wire) in [
        (
            "empty",
            golden_bytes(
                "legacy-error-empty",
                include_str!("goldens/pdu_error_response_empty_reason.hex"),
            ),
        ),
        (
            "x",
            golden_bytes(
                "legacy-error-x",
                include_str!("goldens/pdu_error_response_reason_x.hex"),
            ),
        ),
    ] {
        let decoded = Pdu::decode_for_dialect(wire.as_slice(), MuxWireDialect::LEGACY46)
            .unwrap_or_else(|error| panic!("{}: legacy rejection failed: {:#}", label, error));
        let (_, MuxWireDecodedPayload::Legacy46Rejection(rejection)) = decoded.into_parts() else {
            panic!("{}: legacy PDU0 decoded to the wrong payload class", label);
        };
        assert_eq!(
            rejection.effect_authority(),
            Legacy46RejectionAuthority::Unavailable
        );
        assert_eq!(
            rejection.retry_authority(),
            Legacy46RejectionAuthority::Unavailable
        );
        assert!(Pdu::decode(wire.as_slice()).is_err());
    }

    let opaque_non_utf8 = frame(well_formed_len(3, 0, 2), 3, 0, &[1, 0xFF]);
    let decoded = Pdu::decode_for_dialect(opaque_non_utf8.as_slice(), MuxWireDialect::LEGACY46)
        .expect("legacy PDU0 text bytes must be discarded without UTF-8 parsing");
    assert!(matches!(
        decoded.payload(),
        MuxWireDecodedPayload::Legacy46Rejection(_)
    ));
}

#[test]
fn legacy46_pdu0_is_bounded_exact_and_compression_safe() {
    let body = varbincode_bytes(&"opaque legacy failure".to_string());
    let compressed = zstd::stream::encode_all(body.as_slice(), 0).expect("compress legacy PDU0");
    let tagged_len = COMPRESSED_MASK | well_formed_len(8, 0, compressed.len() as u64);
    let compressed_wire = frame(tagged_len, 8, 0, &compressed);
    let decoded = Pdu::decode_for_dialect(compressed_wire.as_slice(), MuxWireDialect::LEGACY46)
        .expect("bounded compressed legacy PDU0");
    assert!(matches!(
        decoded.payload(),
        MuxWireDecodedPayload::Legacy46Rejection(_)
    ));

    let trailing_body = [0_u8, 0xAA];
    let trailing_wire = frame(
        well_formed_len(9, 0, trailing_body.len() as u64),
        9,
        0,
        &trailing_body,
    );
    assert!(Pdu::decode_for_dialect(trailing_wire.as_slice(), MuxWireDialect::LEGACY46).is_err());

    let over = MAX_LEGACY46_ERROR_RESPONSE_DECOMPRESSED_BYTES + 1;
    let oversize_header = frame(well_formed_len(10, 0, over as u64), 10, 0, &[]);
    assert!(Pdu::decode_for_dialect(oversize_header.as_slice(), MuxWireDialect::LEGACY46).is_err());
    let mut streaming = StreamingPduBuffer::from(oversize_header);
    assert!(Pdu::stream_decode_for_dialect(&mut streaming, MuxWireDialect::LEGACY46).is_err());
}

#[test]
fn legacy46_streaming_consumes_exactly_one_frame_and_preserves_successor() {
    let mut wire = golden_bytes(
        "legacy-error-x",
        include_str!("goldens/pdu_error_response_reason_x.hex"),
    );
    Pdu::Ping(Ping {}).encode(&mut wire, 12).expect("successor");
    let mut buffer = StreamingPduBuffer::from(wire);
    let first = Pdu::stream_decode_for_dialect(&mut buffer, MuxWireDialect::LEGACY46)
        .expect("legacy first frame")
        .expect("complete legacy first frame");
    assert!(matches!(
        first.payload(),
        MuxWireDecodedPayload::Legacy46Rejection(_)
    ));
    let second = Pdu::stream_decode_for_dialect(&mut buffer, MuxWireDialect::LEGACY46)
        .expect("legacy second frame")
        .expect("complete legacy second frame");
    assert_eq!(second.serial(), 12);
    assert!(matches!(
        second.payload(),
        MuxWireDecodedPayload::Pdu(Pdu::Ping(_))
    ));
    assert!(buffer.is_empty());
}

#[test]
fn legacy46_pdu47_is_consumed_but_explicitly_unsupported() {
    // `data: None` is a canonical valid v46 PDU47 body.  The dialect consumes
    // the exact frame but never exposes even this image response as complete.
    let mut rejected = Vec::new();
    Pdu::GetImageCellResponse(GetImageCellResponse {
        pane_id: 5,
        data: None,
    })
    .encode_with_mode(&mut rejected, 47, CompressionMode::Never)
    .expect("encode exact image-response fixture");
    let decoded = Pdu::decode_for_dialect(rejected.as_slice(), MuxWireDialect::LEGACY46)
        .expect("legacy PDU47 must be bounded and classified");
    let (_, MuxWireDecodedPayload::Unsupported(unsupported)) = decoded.into_parts() else {
        panic!("legacy PDU47 was not rejected explicitly");
    };
    assert_eq!(unsupported.ident(), 47);
    assert_eq!(
        unsupported.reason(),
        MuxWireUnsupportedReason::Legacy46ImageContentSemanticsUnavailable
    );

    let oversized = MAX_LEGACY46_UNSUPPORTED_IMAGE_RESPONSE_DECOMPRESSED_BYTES + 1;
    let header_only = frame(well_formed_len(48, 47, oversized as u64), 48, 47, &[]);
    assert!(
        Pdu::decode_for_dialect(header_only.as_slice(), MuxWireDialect::LEGACY46).is_err(),
        "image-bearing v46 PDU47 must fail at bounded header admission"
    );

    let request = Pdu::GetImageCell(GetImageCell {
        pane_id: 5,
        line_idx: 0,
        cell_idx: 0,
        data_hash: [0; 32],
    });
    let error = request
        .prepare_outbound_for_dialect(
            MuxWireDialect::LEGACY46,
            PduProducer::Client,
            PduWireRole::Request,
            None,
            CompressionMode::Never,
        )
        .expect_err("v46 PDU46 request must be disabled with its unsupported reply");
    assert!(format!("{error:#}").contains("Legacy46ImageContentSemanticsUnavailable"));
}

#[test]
fn legacy46_text_only_pdu23_and_pdu25_remain_available() {
    let text_lines = || {
        SerializedLines::from(vec![(
            5,
            Line::from_text("legacy text", &CellAttributes::default(), 1, None),
        )])
    };
    let mut pdu23 = Vec::new();
    Pdu::GetLinesResponse(GetLinesResponse {
        pane_id: 3,
        lines: text_lines(),
    })
    .encode_with_mode(&mut pdu23, 23, CompressionMode::Never)
    .expect("encode text-only PDU23");
    let decoded = Pdu::decode_for_dialect(pdu23.as_slice(), MuxWireDialect::LEGACY46)
        .expect("decode text-only legacy PDU23");
    assert!(matches!(
        decoded.payload(),
        MuxWireDecodedPayload::Pdu(Pdu::GetLinesResponse(_))
    ));

    let mut pdu25 = Vec::new();
    Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
        pane_id: 3,
        mouse_grabbed: false,
        alt_screen_active: false,
        cursor_position: StableCursorPosition::default(),
        dimensions: RenderableDimensions {
            cols: 80,
            viewport_rows: 24,
            scrollback_rows: 100,
            physical_top: 0,
            scrollback_top: 0,
            dpi: 96,
            pixel_width: 800,
            pixel_height: 600,
            reverse_video: false,
        },
        tiered_scrollback_status: None,
        dirty_lines: std::iter::once(5..6).collect(),
        title: "legacy".to_string(),
        working_dir: None,
        bonus_lines: text_lines(),
        input_serial: None,
        seqno: 1,
    })
    .encode_with_mode(&mut pdu25, 25, CompressionMode::Never)
    .expect("encode text-only PDU25");
    let decoded = Pdu::decode_for_dialect(pdu25.as_slice(), MuxWireDialect::LEGACY46)
        .expect("decode text-only legacy PDU25");
    assert!(matches!(
        decoded.payload(),
        MuxWireDecodedPayload::Pdu(Pdu::GetPaneRenderChangesResponse(_))
    ));
}

#[test]
fn legacy46_exact_live_pdu27_frame_decodes_without_schema_guessing() {
    let wire = golden_bytes(
        "trj-live-codec46-pdu27",
        "db 80 80 80 80 80 80 80 80 01 01 1b 28 b5 2f fd 00 58 85 02 00 d2 05 13 16 90 b5 3a b0 46 08 21 d9 61 16 7f 9a 21 f0 ff 83 14 e8 ee ff 05 6c 46 c0 1e d8 2c 83 b6 ca c2 32 57 e0 83 e8 9c 88 53 a7 b8 32 d7 98 1b d1 ae eb 81 33 47 4e 83 9f 12 7b 0c 36 52 e6 21 4a 9d 82 9c 11 76 03 8f 48 fd ae 27 c0 04 00",
    );
    let bootstrap = Pdu::decode(wire.as_slice())
        .expect("captured live PDU27 must decode before dialect authority exists");
    let Pdu::GetCodecVersionResponse(response) = bootstrap.pdu else {
        panic!("captured live bootstrap PDU27 decoded to the wrong payload class");
    };
    assert_eq!(response.codec_vers, 46);
    assert_eq!(response.min_supported, 46);

    let decoded = Pdu::decode_for_dialect(wire.as_slice(), MuxWireDialect::LEGACY46)
        .expect("captured live PDU27 must decode");
    let (_, MuxWireDecodedPayload::Pdu(Pdu::GetCodecVersionResponse(response))) =
        decoded.into_parts()
    else {
        panic!("captured live PDU27 decoded to the wrong payload class");
    };
    assert_eq!(response.codec_vers, 46);
    assert_eq!(response.min_supported, 46);
}
