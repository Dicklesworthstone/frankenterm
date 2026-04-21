//! Regression test for ft-gy9eo: `codec::Pdu::stream_decode` must reject
//! the fuzzer-discovered OOM crasher at
//! `fuzz/artifacts/pdu_stream_decode/oom-5c320deb87390600368019c6ffe451a1e575b82b`
//! without allocating.
//!
//! Before the fix, an attacker-controlled varbincode container length
//! caused either `vec![0u8; len]` in `read_vec` (bounded only by the
//! 256 MiB outer frame cap) or a visitor-driven `Vec::with_capacity(len)`
//! to allocate a huge buffer, tripping libFuzzer's OOM detector.
//!
//! The fix introduces a 16 MiB `MAX_CONTAINER_BYTES` cap in
//! `bounded_varbincode` and clamps the `SeqAccess`/`MapAccess` size_hint
//! so visitor preallocation stays bounded regardless of attacker input.

use codec::Pdu;

/// The exact 17-byte crasher from libFuzzer. Inlined (not loaded from the
/// artifact file) so the regression is self-contained and survives
/// artifact-directory moves.
///
/// Layout per `codec::encode_raw`:
/// - `0E`          : tagged_len = 14 (no compression bit)
/// - `44`          : serial = 68
/// - `04`          : ident = 4
/// - 14-byte body  : varbincode payload whose decoded-container-length
///                   field reaches into the tens of billions, previously
///                   causing an allocation bomb.
const OOM_CRASHER: &[u8] = &[
    0x0E, 0x44, 0x04, 0x00, 0x04, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x71, 0x71, 0x71, 0x30, 0x71, 0x71,
    0xFE,
];

/// The 71-byte ASan crasher from `ft-y87lf`. The first frame is a crafted
/// `GetLinesResponse`; the trailing bytes keep the fuzz target in its
/// multi-step decode loop.
const ASAN_ALLOCATION_TOO_BIG_CRASHER: &[u8] = &[
    0x3A, 0x26, 0x17, 0x43, 0x22, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1E,
    0x01, 0x00, 0x0E, 0x44, 0x04, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01, 0x07, 0xFF, 0xFF, 0x03, 0xFF,
    0xFF, 0xFF, 0xFF, 0xE8, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00, 0x16, 0xFF, 0xFF, 0xFF, 0xFA,
    0x00, 0x00, 0x04, 0x49, 0x03, 0x03, 0xFF, 0xFF, 0xFF, 0xFF, 0xE8, 0xFF, 0xFF, 0xFF, 0xFF, 0x02,
    0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0xFB, 0x29,
];

#[test]
fn oom_crasher_5c320deb_rejected_without_allocation() {
    // The test process has a normal 64-bit address space — the pre-fix
    // decoder would not panic here, it would just allocate gigabytes and
    // hang. If this test ever regresses we want it to fail fast: the
    // decoder MUST return a typed error, not succeed and not hang.
    let mut buf = OOM_CRASHER.to_vec();

    match Pdu::stream_decode(&mut buf) {
        Ok(None) => {
            panic!(
                "stream_decode returned Ok(None) — the frame is complete per its \
                 tagged_len header, so this indicates a logic error in the test"
            );
        }
        Ok(Some(decoded)) => {
            // If the fix is ever strengthened enough that the crasher
            // decodes to a valid PDU::Invalid (unknown ident), that is
            // also acceptable — but the allocator must not have blown up.
            // Pin this by checking the outer frame-level accept path is at
            // least consistent.
            assert_eq!(
                decoded.serial, 68,
                "unexpected decode of OOM crasher produced serial={}",
                decoded.serial
            );
        }
        Err(err) => {
            // Expected path: inner varbincode deserializer rejects the
            // oversized container length before allocation. Message wording
            // is not part of the contract, but we assert something useful
            // came out so a future mis-edit that swallows the error shows up.
            let msg = format!("{err:#}");
            assert!(
                !msg.is_empty(),
                "stream_decode returned an empty error message"
            );
        }
    }
}

/// Complementary guard: even if a future refactor accepts the crasher,
/// calling `stream_decode` on it must not take an unreasonable amount of
/// wall time (a stand-in for "did not attempt a huge allocation"). The
/// threshold is generous to stay stable under CI jitter — the pre-fix
/// behavior was libFuzzer's 2 GiB OOM, not slowness — so we only need
/// to catch the obvious pathological case.
#[test]
fn oom_crasher_5c320deb_completes_quickly() {
    use std::time::Instant;

    let mut buf = OOM_CRASHER.to_vec();
    let started = Instant::now();
    let _ = Pdu::stream_decode(&mut buf);
    let elapsed = started.elapsed();

    assert!(
        elapsed.as_secs() < 2,
        "stream_decode on the OOM crasher took {:?} — expected sub-second rejection; \
         a slow path suggests the attacker-controlled length reached an allocator",
        elapsed
    );
}

#[test]
fn asan_crasher_22974e1f_completes_without_allocator_blowup() {
    let mut buf = ASAN_ALLOCATION_TOO_BIG_CRASHER.to_vec();
    let mut saw_progress = false;

    for _ in 0..8 {
        let before_len = buf.len();

        match Pdu::stream_decode(&mut buf) {
            Ok(Some(decoded)) => {
                saw_progress = true;
                let _ = decoded.serial;
                let _ = decoded.pdu.pdu_name();
                assert!(
                    buf.len() < before_len,
                    "stream_decode must consume bytes when it returns a frame"
                );
            }
            Ok(None) => break,
            Err(err) => {
                let msg = format!("{err:#}");
                assert!(!msg.is_empty(), "stream_decode returned an empty error");
                return;
            }
        }
    }

    assert!(
        saw_progress,
        "ASan crasher made no decode progress and produced no error"
    );
}
