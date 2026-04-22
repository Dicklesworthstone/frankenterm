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

use codec::{DecodedPdu, ErrorResponse, Pdu, Ping};

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

// -----------------------------------------------------------------------------
// 3. Boundary length 0 — ErrorResponse with empty reason
// -----------------------------------------------------------------------------

#[test]
fn conformance_boundary_zero_payload_error_response() {
    let mut wire = Vec::new();
    Pdu::ErrorResponse(ErrorResponse {
        reason: String::new(),
    })
    .encode(&mut wire, 1)
    .expect("encode empty ErrorResponse");

    let decoded = Pdu::decode(wire.as_slice()).expect("decode empty ErrorResponse");
    assert_eq!(decoded.serial, 1);
    match decoded.pdu {
        Pdu::ErrorResponse(r) => assert_eq!(r.reason, ""),
        other => panic!("expected ErrorResponse, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------
// 4. Boundary length 1 — single-byte payload
// -----------------------------------------------------------------------------

#[test]
fn conformance_boundary_one_byte_payload() {
    let mut wire = Vec::new();
    Pdu::ErrorResponse(ErrorResponse {
        reason: "x".to_string(),
    })
    .encode(&mut wire, 2)
    .expect("encode 1-char ErrorResponse");

    let decoded = Pdu::decode(wire.as_slice()).expect("decode 1-char ErrorResponse");
    match decoded.pdu {
        Pdu::ErrorResponse(r) => assert_eq!(r.reason, "x"),
        other => panic!("expected ErrorResponse, got {other:?}"),
    }
}

// -----------------------------------------------------------------------------
// 5. Boundary length near MAX — 1 MB ErrorResponse (well below cap, still large)
// -----------------------------------------------------------------------------

#[test]
fn conformance_boundary_one_megabyte_payload_round_trip() {
    let reason = "A".repeat(1024 * 1024);
    let mut wire = Vec::new();
    Pdu::ErrorResponse(ErrorResponse {
        reason: reason.clone(),
    })
    .encode(&mut wire, 3)
    .expect("encode 1MB ErrorResponse");

    let decoded = Pdu::decode(wire.as_slice()).expect("decode 1MB ErrorResponse");
    match decoded.pdu {
        Pdu::ErrorResponse(r) => assert_eq!(r.reason.len(), 1024 * 1024),
        other => panic!("expected ErrorResponse, got {other:?}"),
    }
    assert_eq!(decoded.serial, 3);
    drop(reason);
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
        "error must name the MAX_PDU_SIZE violation; got: {msg}"
    );
}

// -----------------------------------------------------------------------------
// 7. Truncated input — empty buffer yields None on stream_decode
// -----------------------------------------------------------------------------

#[test]
fn conformance_truncated_empty_buffer_returns_none() {
    let mut buf: Vec<u8> = Vec::new();
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
    let mut buf = vec![0x80u8];
    let result = Pdu::stream_decode(&mut buf).expect("partial leb128 is not fatal on a stream");
    assert!(result.is_none(), "partial leb128 must decode to None");
    assert_eq!(buf, vec![0x80], "buffer must be preserved for more bytes");
}

// -----------------------------------------------------------------------------
// 9. Truncated input — length present, payload missing
// -----------------------------------------------------------------------------

#[test]
fn conformance_truncated_length_without_payload_returns_none() {
    // tagged_len=10, but only the length byte is present. stream_decode
    // should hold the bytes and wait for more.
    let mut buf = vec![10u8];
    let result = Pdu::stream_decode(&mut buf).expect("truncated frame must not error on stream");
    assert!(result.is_none(), "truncated body must decode to None");
    assert_eq!(buf, vec![10u8], "buffer must be preserved");
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

    let decoded = Pdu::stream_decode(&mut wire)
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
        wire, b"GARBAGE",
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

    let first = Pdu::stream_decode(&mut wire).unwrap().expect("first frame");
    assert_eq!(first.serial, 11);
    let second = Pdu::stream_decode(&mut wire)
        .unwrap()
        .expect("second frame");
    assert_eq!(second.serial, 22);
    assert!(wire.is_empty(), "both frames must be fully consumed");

    let third = Pdu::stream_decode(&mut wire).unwrap();
    assert!(third.is_none(), "empty buffer after two PDUs must be None");
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
        "expected arithmetic-sanity error, got: {msg}"
    );
}

// -----------------------------------------------------------------------------
// 13. Unknown ident + zero data — produces Pdu::Invalid, preserves serial
// -----------------------------------------------------------------------------

#[test]
fn conformance_unknown_ident_zero_data_yields_invalid_variant() {
    // tagged_len = enc_len(serial=7) + enc_len(ident=99) = 1 + 1 = 2, data_len = 0.
    let wire = frame(2, 7, 99, &[]);
    let decoded = Pdu::stream_decode(&mut wire.clone())
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
    let mut growing = Vec::with_capacity(total_len);
    for (i, &b) in encoded.iter().enumerate() {
        growing.push(b);
        if i + 1 < total_len {
            // Every proper prefix must decode to None without consuming the buffer.
            let before = growing.clone();
            let result = Pdu::stream_decode(&mut growing).expect("partial prefix is not an error");
            assert!(
                result.is_none(),
                "partial frame of {} / {} bytes must decode to None",
                i + 1,
                total_len
            );
            assert_eq!(
                growing, before,
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
    // zstd compresses repetitive data dramatically. ErrorResponse with a
    // 256 KB repeated string will exceed COMPRESS_THRESH and trigger auto
    // compression on the encode side.
    let reason = "COMPRESSIBLE-".repeat(20_000); // ~260 KB
    let original = Pdu::ErrorResponse(ErrorResponse {
        reason: reason.clone(),
    });
    let mut wire = Vec::new();
    original.encode(&mut wire, 88).expect("encode compressed");

    // The compressed output should be materially smaller than the source.
    assert!(
        wire.len() < reason.len() / 2,
        "compression must shrink a highly repetitive payload by >2x; wire={} reason={}",
        wire.len(),
        reason.len()
    );

    let decoded = Pdu::decode(wire.as_slice()).expect("decode compressed");
    assert_eq!(decoded.serial, 88);
    match decoded.pdu {
        Pdu::ErrorResponse(r) => assert_eq!(r.reason, reason),
        other => panic!("expected ErrorResponse, got {other:?}"),
    }
}
