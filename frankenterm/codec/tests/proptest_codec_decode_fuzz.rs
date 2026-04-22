//! Metamorphic robustness harness for `Pdu::decode` / `Pdu::stream_decode`.
//!
//! Relation under test: for *any* byte sequence — including adversarial
//! inputs an attacker could plausibly send over an unauthenticated mux
//! socket — the decoder must terminate with either:
//!
//!   (a) `Ok(DecodedPdu)` with well-formed fields, or
//!   (b) a typed `anyhow::Error`.
//!
//! Never a panic, never an infinite loop. A panic in the decode path is
//! a denial-of-service against the mux server: a client can reliably
//! crash the server just by sending bytes.
//!
//! Three properties cover the fuzz surface:
//!
//!   (1) pure random bytes — exercises the header / length / leb128
//!       entry edges;
//!   (2) plausible header + random body — exercises the post-header
//!       varbincode deserialize path, where most of the per-variant
//!       codec logic lives;
//!   (3) `stream_decode` over random buffers — exercises the partial-
//!       frame path that the mux uses on every read from a socket.

use codec::{Pdu, Ping};
use proptest::prelude::*;

const COMPRESSED_MASK: u64 = 1 << 63;
const PING_IDENT: u64 = 1;

fn assert_decoded_pdu_is_well_formed(decoded: &codec::DecodedPdu) {
    // The serial is a u64; nothing stronger to assert at the envelope.
    let _ = decoded.serial;
    // pdu_name() must be one of the known variants — `Invalid` is the
    // documented fall-through for unknown idents and is acceptable.
    let name = decoded.pdu.pdu_name();
    assert!(!name.is_empty(), "decoded pdu must have a non-empty name");
}

fn structured_header_frame(ident: u64, serial: u64, body: &[u8], is_compressed: bool) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32 + body.len());

    let mut ident_buf = Vec::new();
    let _ = leb128::write::unsigned(&mut ident_buf, ident);

    let mut serial_buf = Vec::new();
    let _ = leb128::write::unsigned(&mut serial_buf, serial);

    let total_payload_len = ident_buf.len() + serial_buf.len() + body.len();
    let mut len_word = total_payload_len as u64;
    if is_compressed {
        len_word |= COMPRESSED_MASK;
    }
    let _ = leb128::write::unsigned(&mut buf, len_word);
    buf.extend_from_slice(&ident_buf);
    buf.extend_from_slice(&serial_buf);
    buf.extend_from_slice(body);
    buf
}

#[test]
fn structured_header_helper_uses_real_codec_compression_bit() {
    let decoded = Pdu::decode(structured_header_frame(PING_IDENT, 77, &[], false).as_slice())
        .expect("uncompressed empty Ping frame should decode");
    assert_eq!(decoded.serial, 77);
    assert_eq!(decoded.pdu, Pdu::Ping(Ping {}));

    let err = Pdu::decode(structured_header_frame(PING_IDENT, 77, &[], true).as_slice())
        .expect_err("compressed-flagged empty Ping frame should hit zstd decode and fail");
    assert!(
        !err.to_string().is_empty(),
        "invalid compressed payload should surface a typed error"
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// (1) Pure random-byte fuzz of `Pdu::decode`. The decoder MUST settle
    /// to Ok or Err for every input; any panic here is a remote DoS.
    #[test]
    fn decode_random_bytes_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        match Pdu::decode(bytes.as_slice()) {
            Ok(decoded) => assert_decoded_pdu_is_well_formed(&decoded),
            Err(_) => {}
        }
    }

    /// (2) Structure-aware fuzz: prepend a plausible leb128 header
    /// (length, ident, serial) so the input actually reaches the
    /// per-variant varbincode deserialize path, then feed random bytes
    /// as the body. This is the deeper surface where a malformed
    /// variant payload could trip the deserializer into an infinite
    /// loop or a debug_assert panic.
    #[test]
    fn decode_structured_header_never_panics(
        // Ident space: cover the known-variant idents (0..75) plus a
        // scattering of unknown idents that should map to Pdu::Invalid.
        ident in prop_oneof![
            0u64..=75,                 // registered idents + a margin
            Just(u64::MAX / 2),        // far-out unknown
            Just(u64::MAX),            // max u64
            Just(1_000_000u64),        // random unknown
        ],
        serial in any::<u64>(),
        body in proptest::collection::vec(any::<u8>(), 0..1024),
        is_compressed in any::<bool>(),
    ) {
        // Hand-roll a header: leb128(length) leb128(ident) leb128(serial) body.
        // The length field is `ident_bytes.len() + serial_bytes.len() +
        // body.len() | (is_compressed << 63_bit_equivalent)` — the codec uses
        // the top bit of the length word as the compressed flag. We don't
        // need to replicate the exact flag encoding for fuzz purposes;
        // generating plausible but possibly invalid headers is the point.
        let buf = structured_header_frame(ident, serial, body.as_slice(), is_compressed);

        match Pdu::decode(buf.as_slice()) {
            Ok(decoded) => assert_decoded_pdu_is_well_formed(&decoded),
            Err(_) => {}
        }
    }

    /// (3) `Pdu::stream_decode` over a random buffer. The mux server
    /// calls this on every read from a client socket, so any panic
    /// here is directly reachable from a hostile client. The stream
    /// decoder's extra contract: on a partial frame, it must return
    /// `Ok(None)` without disturbing the buffer; on a complete frame
    /// it must consume only that frame's bytes.
    #[test]
    fn stream_decode_random_bytes_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..2048),
    ) {
        let mut buf = bytes;
        let original_len = buf.len();
        match Pdu::stream_decode(&mut buf) {
            Ok(Some(decoded)) => {
                assert_decoded_pdu_is_well_formed(&decoded);
                // stream_decode must leave the buffer strictly smaller
                // (it consumed the one frame) or empty.
                prop_assert!(
                    buf.len() < original_len || original_len == 0,
                    "stream_decode returned Some but did not advance the buffer"
                );
            }
            Ok(None) => {
                // Partial frame — buffer must be unchanged.
                prop_assert_eq!(
                    buf.len(),
                    original_len,
                    "stream_decode returned None but mutated the buffer"
                );
            }
            Err(_) => {}
        }
    }
}
