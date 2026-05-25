#![no_main]
//! Redactor secret-leak fuzz target (policy/redaction/security domain).
//!
//! Exercises the read-path redactor against adversarial input — arbitrary
//! (possibly invalid) UTF-8 bytes, secrets split across chunk boundaries at
//! every offset, and the no-separator keyed-secret class (ft-b1p6x:
//! `azureopenaikey<value>`, `devicecode<value>` where the key name abuts the
//! value with no `=`/`:`/space). Oracles:
//!
//! 1. **No panic** on any byte stream through either the one-shot or streaming
//!    API (covers UTF-8 boundary slicing, multibyte pending buffers, overflow
//!    flushing).
//! 2. **No residue**: scanning the streamed output with a fresh `Redactor`
//!    reports clean — streaming redaction must be at least as strong as batch,
//!    so a cross-boundary miss surfaces here.
//! 3. **No raw-secret leak**: when a known catalog secret is injected, neither
//!    the one-shot nor the streamed output may contain the raw secret bytes,
//!    no matter where the chunk boundaries fall inside it.
//! 4. **Idempotence**: `redact(redact(s)) == redact(s)`.
//! 5. **detect() span safety**: every reported span lies on UTF-8 char
//!    boundaries with `start <= end <= len`.

use arbitrary::Arbitrary;
use frankenterm_core::redactor::{Redactor, StreamingRedactor};
use libfuzzer_sys::fuzz_target;

// Kept well under DEFAULT_STREAMING_REDACTOR_MAX_PENDING_BYTES (8 MiB) so the
// streaming overflow-flush path never fires within the fuzzed input range. That
// keeps oracle 2 (streamed output carries no residue) sound: with no bounded-
// memory flush mid-secret, streaming redaction is complete and equivalent to
// batch. Exercising the overflow path is a separate, larger-input concern.
const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Adversarial secret samples. Includes the no-separator keyed-secret class
/// (key name directly abutting a high-entropy value) that has historically
/// slipped past anchor coverage, plus canonical provider tokens.
const INJECTED_SECRETS: &[&str] = &[
    "sk-ant-api03-AaBbCcDdEeFfGgHhIiJjKkLlMmNnOoPpQqRrSsTtUuVv0123456789",
    "ghp_AbCdEf0123456789AbCdEf0123456789AbCd",
    "AKIAIOSFODNN7EXAMPLE",
    // No-separator keyed secrets (ft-b1p6x): no `=`/`:`/space between key and value.
    "azureopenaikeyAbCdEf0123456789AbCdEf0123456789",
    "devicecodeWDJBMJHT0123456789ABCDEF",
    "usercodeABCD1234EFGH5678",
    // Long high-entropy bare token.
    "Zx9Qw7Lm2Kp5Rt8Nv1Cd4Gh6Jf0Bs3Yw5Tn8Mr2Vk7",
];

#[derive(Arbitrary, Debug)]
struct FuzzCase {
    /// Raw surrounding bytes (may be invalid UTF-8).
    data: Vec<u8>,
    /// Chunk split sizes for the streaming path.
    chunk_sizes: Vec<u16>,
    /// Whether to inject a known secret and which one / where.
    inject: bool,
    secret_pick: u8,
    inject_offset: u16,
}

/// Split `data` into chunks per `chunk_sizes`, cycling through the sizes and
/// flushing whatever remains as a final chunk. A zero size is treated as 1 so
/// we always make progress.
fn chunks<'a>(data: &'a [u8], sizes: &[u16]) -> Vec<&'a [u8]> {
    let mut out = Vec::new();
    let mut rest = data;
    let mut i = 0usize;
    while !rest.is_empty() {
        let size = if sizes.is_empty() {
            rest.len()
        } else {
            (sizes[i % sizes.len()] as usize).max(1).min(rest.len())
        };
        let (head, tail) = rest.split_at(size);
        out.push(head);
        rest = tail;
        i += 1;
        if i > data.len() + 1 {
            // Defensive: never loop more than once-per-byte.
            if !rest.is_empty() {
                out.push(rest);
            }
            break;
        }
    }
    out
}

fn stream_redact(input: &[u8], sizes: &[u16]) -> Vec<u8> {
    let mut streaming = StreamingRedactor::new();
    let mut out = Vec::new();
    for chunk in chunks(input, sizes) {
        out.extend_from_slice(&streaming.redact_chunk(chunk).bytes);
    }
    out.extend_from_slice(&streaming.finish().bytes);
    out
}

fuzz_target!(|case: FuzzCase| {
    let mut data = case.data;
    data.truncate(MAX_INPUT_BYTES);

    // Optionally weave a known secret into the byte stream at an arbitrary
    // (char-agnostic) offset, so it can land split across chunk boundaries.
    let injected = if case.inject && !INJECTED_SECRETS.is_empty() {
        let secret = INJECTED_SECRETS[case.secret_pick as usize % INJECTED_SECRETS.len()];
        let pos = if data.is_empty() {
            0
        } else {
            (case.inject_offset as usize) % (data.len() + 1)
        };
        data.splice(pos..pos, secret.bytes().collect::<Vec<u8>>());
        Some(secret)
    } else {
        None
    };

    // --- Streaming path -----------------------------------------------------
    let streamed = stream_redact(&data, &case.chunk_sizes);
    let streamed_str = String::from_utf8_lossy(&streamed);

    // Oracle 2: streaming output carries no residue the batch redactor catches.
    let scanner = Redactor::new();
    assert!(
        !scanner.contains_secrets(&streamed_str),
        "streaming redaction left a residue the batch redactor still detects"
    );

    // --- One-shot path ------------------------------------------------------
    let input_str = String::from_utf8_lossy(&data).into_owned();
    let once = scanner.redact(&input_str);

    // Oracle 4: idempotence.
    let twice = scanner.redact(&once);
    assert_eq!(once, twice, "redact is not idempotent");

    // Oracle 2 (one-shot): no residue.
    assert!(
        !scanner.contains_secrets(&once),
        "one-shot redaction left a residue"
    );

    // Oracle 5: detect() spans are char-boundary-safe and in range.
    for (name, start, end) in scanner.detect(&input_str) {
        assert!(start <= end, "detect span `{name}` start>end");
        assert!(end <= input_str.len(), "detect span `{name}` end past len");
        assert!(
            input_str.is_char_boundary(start),
            "detect span `{name}` start not on char boundary"
        );
        assert!(
            input_str.is_char_boundary(end),
            "detect span `{name}` end not on char boundary"
        );
    }

    // Oracle 3: an injected raw secret must never survive either path.
    if let Some(secret) = injected {
        // The secret is ASCII here, so it survives the lossy decode intact and a
        // substring check is exact. If the redactor missed it (one-shot or
        // across the streamed chunk boundaries), this trips.
        assert!(
            !streamed_str.contains(secret),
            "streamed output leaked injected secret across chunk boundaries"
        );
        assert!(
            !once.contains(secret),
            "one-shot output leaked injected secret"
        );
    }
});
