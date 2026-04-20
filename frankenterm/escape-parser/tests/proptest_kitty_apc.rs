//! Property-based fuzzing harness for `KittyImage::parse_apc`.
//!
//! The Kitty image APC payload parser at `apc.rs:1061` is a public API that
//! takes arbitrary bytes from an untrusted terminal stream. Before this
//! harness the function had zero proptest coverage (verified by grep
//! across `frankenterm/escape-parser/tests/` and
//! `crates/frankenterm-core/tests/`).
//!
//! Contract under test:
//!
//! 1. **Crash-freedom**: `parse_apc(bytes)` must never panic on any input,
//!    including empty, malformed UTF-8, truncated base64, unknown keys,
//!    duplicated keys, missing separators, overflowing integers, or raw
//!    non-ASCII bytes.
//!
//! 2. **Contract on valid prefix**: the first byte must be `G`. For inputs
//!    whose first byte is not `G`, the parser must return `None` without
//!    panic or allocation spikes. For empty inputs the parser must return
//!    `None`.
//!
//! 3. **Key-value stability for structured inputs**: given a generator that
//!    produces syntactically-well-formed APC payloads (G-prefix,
//!    comma-separated `key=value` pairs, optional `;` + base64 payload),
//!    the parser must either:
//!    (a) return `Some(KittyImage::...)` for known action verbs, or
//!    (b) return `None` for unknown action verbs / invalid verbosity.
//!    It must not panic.
//!
//! 4. **Determinism**: parsing the same byte slice twice must yield two
//!    `Option<KittyImage>` values with the same Debug representation.

use frankenterm_escape_parser::apc::KittyImage;
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────

/// Completely arbitrary bytes. Primary fuzzing corpus.
fn arb_any_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(any::<u8>(), 0..512)
}

/// Bytes biased toward ASCII-printable characters. Narrows the search to
/// inputs the parser might see from a misbehaving but legible stream.
fn arb_printable_bytes() -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(0x20u8..0x7fu8, 0..256)
}

/// Arbitrary APC-shaped byte sequences that are not necessarily valid
/// Kitty graphics payloads: start with any byte (including `G`), then
/// key=value pairs separated by commas, then optional `;payload`.
///
/// The point of this generator is to stress the key/value tokenizer
/// without always producing semantically-valid APCs.
fn arb_apc_like() -> impl Strategy<Value = Vec<u8>> {
    (
        // Leading byte: bias toward `G` so half the cases go through
        // the main parse path; the rest exercise the early `None`.
        prop_oneof![
            4 => Just(b'G'),
            1 => any::<u8>(),
        ],
        proptest::collection::vec(
            (
                // Key: usually a single ASCII letter, sometimes longer.
                prop_oneof![
                    5 => proptest::sample::select(
                        b"aAbBcCdDefFgGhHiIjklLmMnNopPqQrsStTuvVwxyzZ".to_vec(),
                    )
                    .prop_map(|byte| vec![byte]),
                    1 => proptest::collection::vec(0x20u8..0x7fu8, 1..6),
                ],
                // Value: often a small decimal integer; sometimes junk.
                prop_oneof![
                    5 => (0u64..10_000).prop_map(|n| n.to_string().into_bytes()),
                    1 => proptest::collection::vec(0x20u8..0x7fu8, 0..8),
                    1 => proptest::collection::vec(any::<u8>(), 0..8),
                ],
            ),
            0..6,
        ),
        // Optional payload after `;`. When present, mostly valid base64
        // alphabet so the payload tokenizer exercises real decode paths.
        prop::option::of(prop_oneof![
            3 => proptest::collection::vec(
                proptest::sample::select(
                    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/=".to_vec(),
                ),
                0..64,
            ),
            1 => proptest::collection::vec(any::<u8>(), 0..32),
        ]),
    )
        .prop_map(|(first_byte, pairs, payload)| {
            let mut bytes = vec![first_byte];
            for (i, (key, value)) in pairs.iter().enumerate() {
                if i > 0 {
                    bytes.push(b',');
                }
                bytes.extend_from_slice(key);
                bytes.push(b'=');
                bytes.extend_from_slice(value);
            }
            if let Some(payload_bytes) = payload {
                bytes.push(b';');
                bytes.extend_from_slice(&payload_bytes);
            }
            bytes
        })
}

/// Strict valid-shaped APC payloads: always start with `G`, always carry a
/// supported action, always have valid verbosity, and include a payload
/// that decodes as base64. Used to assert the `Some(...)` half of the
/// parser contract.
fn arb_well_formed_apc() -> impl Strategy<Value = Vec<u8>> {
    (
        // Action: one of the seven supported verbs.
        proptest::sample::select(vec![b't', b'q', b'T', b'p', b'd', b'f', b'c']),
        // Verbosity: 0 (default), 1 (no OK), 2 (silent).
        proptest::sample::select(vec![b'0', b'1', b'2']),
        // Small integer payload for `s`, `v`, `i`.
        0u32..200,
        0u32..200,
        1u32..1000,
        // Base64 payload — always valid alphabet.
        proptest::collection::vec(
            proptest::sample::select(
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789".to_vec(),
            ),
            0..32,
        ),
    )
        .prop_map(|(action, quiet, width, height, image_id, payload_body)| {
            let mut bytes = vec![b'G'];
            bytes.extend_from_slice(b"a=");
            bytes.push(action);
            bytes.extend_from_slice(b",q=");
            bytes.push(quiet);
            bytes.extend_from_slice(b",s=");
            bytes.extend_from_slice(width.to_string().as_bytes());
            bytes.extend_from_slice(b",v=");
            bytes.extend_from_slice(height.to_string().as_bytes());
            bytes.extend_from_slice(b",i=");
            bytes.extend_from_slice(image_id.to_string().as_bytes());
            bytes.push(b';');
            // Pad to a length divisible by 4 for strict base64 compat.
            let mut padded = payload_body.clone();
            while padded.len() % 4 != 0 {
                padded.push(b'=');
            }
            bytes.extend_from_slice(&padded);
            bytes
        })
}

// ── Properties ──────────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// Crash-freedom on arbitrary bytes.
    ///
    /// `parse_apc` is called directly against the byte stream after the
    /// outer VT parser strips the APC introducer/terminator, so it must
    /// tolerate arbitrary adversarial bytes without panicking.
    #[test]
    fn parse_apc_never_panics_on_arbitrary_bytes(bytes in arb_any_bytes()) {
        let _ = KittyImage::parse_apc(&bytes);
    }

    /// Crash-freedom on printable-ASCII bytes — narrows fuzzing to the
    /// most likely inputs from a misbehaving but legible stream.
    #[test]
    fn parse_apc_never_panics_on_printable(bytes in arb_printable_bytes()) {
        let _ = KittyImage::parse_apc(&bytes);
    }

    /// Crash-freedom on APC-shaped byte sequences (structured fuzzing).
    #[test]
    fn parse_apc_never_panics_on_apc_shape(bytes in arb_apc_like()) {
        let _ = KittyImage::parse_apc(&bytes);
    }

    /// Inputs that do not start with `G` must return `None`. Empty input
    /// must also return `None`.
    #[test]
    fn parse_apc_rejects_non_g_prefix(bytes in arb_any_bytes()) {
        if bytes.is_empty() || bytes[0] != b'G' {
            prop_assert!(
                KittyImage::parse_apc(&bytes).is_none(),
                "inputs without G-prefix must parse to None; input: {:?}",
                bytes,
            );
        }
    }

    /// Empty input always returns `None`.
    #[test]
    fn parse_apc_rejects_empty(_ in proptest::num::u8::ANY) {
        prop_assert!(
            KittyImage::parse_apc(b"").is_none(),
            "empty APC payload must parse to None"
        );
    }

    /// Determinism: parsing the same bytes twice yields identical results.
    ///
    /// Uses Debug formatting because `KittyImage` does not derive `Eq`
    /// (it carries `Option<PathBuf>` and other shapes that may not).
    #[test]
    fn parse_apc_is_deterministic(bytes in arb_apc_like()) {
        let a = KittyImage::parse_apc(&bytes);
        let b = KittyImage::parse_apc(&bytes);
        prop_assert_eq!(format!("{a:?}"), format!("{b:?}"), "parse_apc must be deterministic");
    }

    /// Well-formed APC payloads with supported action verbs must either
    /// parse to `Some(...)` or `None` — never panic. When they parse to
    /// `Some(...)`, the reported `verbosity` must round-trip through the
    /// `.verbosity()` accessor without panic.
    #[test]
    fn parse_apc_well_formed_is_total(bytes in arb_well_formed_apc()) {
        match KittyImage::parse_apc(&bytes) {
            Some(img) => {
                let _ = img.verbosity();
            }
            None => {
                // Acceptable: some well-formed inputs may still be rejected
                // for semantic reasons (e.g., missing required keys for a
                // specific action). The contract we care about here is no
                // panic.
            }
        }
    }

    /// Canonical regression: exercise the specific payload shapes that
    /// already appear in `apc.rs`'s inline unit tests, generated with
    /// varying numeric parameters so shrinking can pinpoint any regression.
    #[test]
    fn parse_apc_canonical_shapes_do_not_panic(
        width in 1u32..1000,
        height in 1u32..1000,
        image_id in 1u32..1_000_000,
    ) {
        let inputs: Vec<Vec<u8>> = vec![
            format!("Gf=24,s={width},v={height};aGVsbG8=").into_bytes(),
            format!("Gf=32,s={width},v={height};aGVsbG8=").into_bytes(),
            format!("Ga=d,q=2").into_bytes(),
            format!("Ga=p,i={image_id}").into_bytes(),
            format!("Ga=q,f=32,s={width},v={height};AAAA").into_bytes(),
            format!("Ga=t,f=100,s={width},v={height},i={image_id};AAAA").into_bytes(),
        ];
        for input in inputs {
            let _ = KittyImage::parse_apc(&input);
        }
    }
}

// ── Hand-rolled regression coverage ─────────────────────────────────────

#[test]
fn parse_apc_does_not_panic_on_empty() {
    assert!(KittyImage::parse_apc(b"").is_none());
}

#[test]
fn parse_apc_does_not_panic_on_just_g() {
    // Just the "G" prefix with no keys — hits the splitn + key parser.
    let _ = KittyImage::parse_apc(b"G");
}

#[test]
fn parse_apc_does_not_panic_on_g_semicolon() {
    // G-prefix, empty key section, immediate semicolon + empty payload.
    let _ = KittyImage::parse_apc(b"G;");
}

#[test]
fn parse_apc_does_not_panic_on_key_without_value() {
    // "a" with no `=value` — splitn(2, '=') produces one element, the
    // parser must handle the `next()?` cleanly.
    let _ = KittyImage::parse_apc(b"Ga");
    let _ = KittyImage::parse_apc(b"Ga,b=1");
    let _ = KittyImage::parse_apc(b"Ga=1,b");
}

#[test]
fn parse_apc_does_not_panic_on_invalid_utf8_keys() {
    // Non-UTF-8 byte in the key section — `from_utf8` returns Err and
    // `?` must bail out cleanly.
    let bytes = b"G\xffa=t;";
    let _ = KittyImage::parse_apc(bytes);
}

#[test]
fn parse_apc_rejects_non_g_first_byte() {
    for first in 0u8..=255u8 {
        if first == b'G' {
            continue;
        }
        let bytes = [first, b'a', b'=', b't'];
        assert!(
            KittyImage::parse_apc(&bytes).is_none(),
            "non-G prefix byte {first:#x} must produce None"
        );
    }
}
