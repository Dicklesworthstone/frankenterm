//! encode and decode the frames for the mux protocol.
//! The frames include the length of a PDU as well as an identifier
//! that informs us how to decode it.  The length, ident and serial
//! number are encoded using a variable length integer encoding.
//! Rather than rely solely on serde to serialize and deserialize an
//! enum, we encode the enum variants with a version/identifier tag
//! for ourselves.  This will make it a little easier to manage
//! client and server instances that are built from different versions
//! of this code; in this way the client and server can more gracefully
//! manage unknown enum variants.
#![allow(dead_code)]
#![allow(clippy::range_plus_one)]

// Both async-smol and async-asupersync may be enabled simultaneously due to Cargo
// workspace feature unification. While legacy vendored clients still pass
// smol Async streams into codec async APIs, mixed graphs must continue to use
// the smol path until those callers migrate.

use anyhow::{bail, Context as _, Error};
use config::keyassignment::{PaneDirection, ScrollbackEraseMode};
use frankenterm_term::color::ColorPalette;
use frankenterm_term::{Alert, ClipboardSelection, SemanticZone, StableRowIndex, TerminalSize};
use mux::client::{ClientId, ClientInfo};
use mux::pane::PaneId;
use mux::renderable::{PaneTieredScrollbackStatus, RenderableDimensions, StableCursorPosition};
use mux::tab::{FloatingPaneRect, PaneNode, SerdeUrl, SplitRequest, TabId, TabStackId};
use mux::window::WindowId;
use mux::{MuxSessionIncarnation, TopologyRevision};
use portable_pty::CommandBuilder;
use rangeset::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

#[cfg(all(feature = "async-asupersync", not(feature = "async-smol")))]
use asupersync::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

#[cfg(feature = "async-smol")]
use smol::io::AsyncWriteExt;
#[cfg(feature = "async-smol")]
use smol::prelude::*;

use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::io::{BufReader, Cursor, Read};
use std::ops::{Deref, Range};
use std::path::PathBuf;
use std::sync::Arc;
use termwiz::hyperlink::Hyperlink;
use termwiz::image::{ImageData, TextureCoordinate};
use termwiz::surface::{Line, SequenceNo};
use thiserror::Error;

mod bounded_varbincode;

/// Bounded deserialize for **untrusted** varbincode payloads. Caps container
/// length (`MAX_CONTAINER_ITEMS`), per-container bytes (`MAX_CONTAINER_BYTES`),
/// and the serde `size_hint` so a malicious length prefix cannot drive an
/// unbounded `Vec::with_capacity`. Use this — not raw `varbincode::deserialize` —
/// on any attacker-influenced input (wire frames, persisted scrollback, etc.).
/// Wire-format-compatible with `varbincode::serialize`. (gauntlet FND-013)
pub use bounded_varbincode::deserialize as bounded_varbincode_deserialize;

/// Content-defined-chunking (CDC) rolling-hash dedup of mux output payloads
/// (ft-6c1t0, round-3 moonshot — OPT-IN, default-inert).
///
/// The mux wire repeats a lot of output: the same prompt redrawn, identical
/// frames mirrored across panes, repeated ANSI runs. This module provides a
/// stateful, **byte-for-byte lossless** dedup codec that a connection can run
/// over serialized PDU payloads before framing (and the inverse after
/// unframing): payloads are split into content-defined chunks via a FastCDC
/// gear rolling hash, and a chunk seen earlier in the session is replaced on
/// the wire by a small back-reference instead of its bytes.
///
/// It is intentionally self-contained (no wire-format change to the existing
/// PDU framing) and opt-in: nothing in the default encode/decode path calls it,
/// so the build is byte-identical until a caller wires
/// [`CdcDedupEncoder`]/[`CdcDedupDecoder`] in per connection-direction. That
/// keeps this experiment trivially revertable (delete the module) and lets the
/// orchestrator measure the dedup ratio + throughput on a real mux-output
/// corpus before any protocol change.
///
/// ## Synchronization contract (losslessness)
///
/// The encoder and the matching decoder process the *same* token stream in the
/// *same order* over a reliable, ordered transport (TCP / the mux wire). Each
/// `LITERAL_CACHE` token deterministically assigns the next sequential chunk id
/// on both sides, so a `REFERENCE(id)` resolves identically. Decode of a stream
/// produced by a synchronized encoder reconstructs the original bytes exactly.
/// Decode of arbitrary/adversarial bytes returns `Err` (never panics, never
/// allocates unboundedly): every length is bounds-checked, every reference id is
/// range-checked, the chunk cache is capped, and reconstructed output is capped
/// at [`MAX_PDU_SIZE`].
pub mod cdc_dedup {
    use anyhow::{bail, Result};
    use std::collections::HashMap;
    use std::convert::TryFrom;

    use super::MAX_PDU_SIZE;

    /// Minimum content-defined chunk length (bytes). Below this no boundary is
    /// taken, bounding per-chunk token overhead on incompressible input.
    const MIN_CHUNK: usize = 64;
    /// Target average chunk length. MUST be a power of two so `AVG_CHUNK - 1` is
    /// the boundary mask; tuned small because terminal output repeats at the
    /// granularity of short ANSI/line runs.
    const AVG_CHUNK: usize = 256;
    /// Maximum chunk length (bytes); forces a boundary so a non-repetitive run
    /// cannot grow an unbounded chunk.
    const MAX_CHUNK: usize = 1024;
    /// Low-bit mask used to detect a content-defined boundary.
    const BOUNDARY_MASK: u64 = (AVG_CHUNK as u64) - 1;
    /// Cap on distinct cached chunks per direction (bounds dictionary memory and,
    /// on the decode side, makes a malformed stream fail closed rather than OOM).
    const MAX_CACHED_CHUNKS: usize = 1 << 16;

    // Token tag (low 2 bits of the per-token leb128 header):
    //   00 = LITERAL_CACHE   (len = hdr>>2) bytes follow; cache as next chunk id
    //   01 = REFERENCE       (id  = hdr>>2) to a previously cached chunk
    //   10 = LITERAL_NOCACHE (len = hdr>>2) bytes follow; do NOT cache (cap hit)
    const TAG_LITERAL_CACHE: u64 = 0b00;
    const TAG_REFERENCE: u64 = 0b01;
    const TAG_LITERAL_NOCACHE: u64 = 0b10;

    /// FastCDC gear table: 256 pseudo-random u64s derived deterministically at
    /// compile time via splitmix64 (so encoder and decoder builds agree).
    const GEAR: [u64; 256] = {
        let mut table = [0u64; 256];
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut i = 0;
        while i < 256 {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^= z >> 31;
            table[i] = z;
            i += 1;
        }
        table
    };

    /// Find the end index (exclusive) of the content-defined chunk that starts
    /// at `start`. Deterministic and dependent only on the bytes, so encoder and
    /// decoder agree without exchanging boundaries.
    fn next_boundary(data: &[u8], start: usize) -> usize {
        let n = data.len();
        let mut hash = 0u64;
        let mut i = start;
        while i < n {
            hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
            let len = i - start + 1;
            if (len >= MIN_CHUNK && (hash & BOUNDARY_MASK) == 0) || len >= MAX_CHUNK {
                return i + 1;
            }
            i += 1;
        }
        n
    }

    fn write_leb(out: &mut Vec<u8>, mut value: u64) {
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
    }

    /// Read a leb128 value from `data` starting at `pos`; returns `(value,
    /// next_pos)`. Bounds- and overflow-checked so malformed input fails closed.
    fn read_leb(data: &[u8], pos: usize) -> Result<(u64, usize)> {
        let mut value = 0u64;
        let mut shift = 0u32;
        let mut i = pos;
        loop {
            let byte = *data
                .get(i)
                .ok_or_else(|| anyhow::anyhow!("cdc: truncated leb128"))?;
            i += 1;
            if shift >= 64 {
                bail!("cdc: leb128 overflow");
            }
            let payload = u64::from(byte & 0x7f);
            if payload > (u64::MAX >> shift) {
                bail!("cdc: leb128 overflow");
            }
            value |= payload << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        Ok((value, i))
    }

    /// Per-direction dedup encoder. Owns the session chunk dictionary; reuse the
    /// same instance for every payload sent in one direction of a connection.
    #[derive(Debug, Default)]
    pub struct CdcDedupEncoder {
        /// chunk bytes -> assigned id
        dict: HashMap<Box<[u8]>, u32>,
        next_id: u32,
    }

    impl CdcDedupEncoder {
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Number of distinct chunks currently cached.
        #[must_use]
        pub fn cached_chunks(&self) -> usize {
            self.dict.len()
        }

        /// Dedup-encode one payload. The output is consumed by a
        /// [`CdcDedupDecoder`] that has seen the same prior payloads in order.
        #[must_use]
        pub fn encode(&mut self, data: &[u8]) -> Vec<u8> {
            let mut out = Vec::with_capacity(data.len() / 2 + 16);
            let mut start = 0;
            while start < data.len() {
                let end = next_boundary(data, start);
                let chunk = &data[start..end];
                if let Some(&id) = self.dict.get(chunk) {
                    write_leb(&mut out, (u64::from(id) << 2) | TAG_REFERENCE);
                } else if (self.next_id as usize) < MAX_CACHED_CHUNKS {
                    write_leb(&mut out, ((chunk.len() as u64) << 2) | TAG_LITERAL_CACHE);
                    out.extend_from_slice(chunk);
                    self.dict.insert(chunk.into(), self.next_id);
                    self.next_id += 1;
                } else {
                    write_leb(&mut out, ((chunk.len() as u64) << 2) | TAG_LITERAL_NOCACHE);
                    out.extend_from_slice(chunk);
                }
                start = end;
            }
            out
        }
    }

    /// Per-direction dedup decoder. Mirror of [`CdcDedupEncoder`]; reuse one
    /// instance for every payload received in one direction of a connection.
    #[derive(Debug, Default)]
    pub struct CdcDedupDecoder {
        chunks: Vec<Box<[u8]>>,
    }

    impl CdcDedupDecoder {
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Reconstruct the original payload from a token stream produced by a
        /// synchronized [`CdcDedupEncoder`]. Lossless for well-formed input;
        /// returns `Err` (never panics/OOMs) for malformed input. Reconstructed
        /// output is capped at [`MAX_PDU_SIZE`].
        pub fn decode(&mut self, data: &[u8]) -> Result<Vec<u8>> {
            self.decode_with_cap(data, MAX_PDU_SIZE)
        }

        /// As [`Self::decode`], but with an explicit reconstruction cap. The
        /// public `decode` always uses [`MAX_PDU_SIZE`]; the injectable cap lets
        /// the gate exercise over-cap rejection without allocating 256 MiB.
        fn decode_with_cap(&mut self, data: &[u8], cap: usize) -> Result<Vec<u8>> {
            let mut out = Vec::new();
            let base_chunk_count = self.chunks.len();
            let mut pending_chunks: Vec<Box<[u8]>> = Vec::new();
            let mut pos = 0;
            while pos < data.len() {
                let (header, next) = read_leb(data, pos)?;
                pos = next;
                let tag = header & 0b11;
                let payload = header >> 2;
                match tag {
                    TAG_REFERENCE => {
                        let id = usize::try_from(payload)
                            .map_err(|_| anyhow::anyhow!("cdc: reference id overflow"))?;
                        let chunk = if id < base_chunk_count {
                            self.chunks
                                .get(id)
                                .ok_or_else(|| anyhow::anyhow!("cdc: reference id out of range"))?
                                .as_ref()
                        } else {
                            pending_chunks
                                .get(id - base_chunk_count)
                                .ok_or_else(|| anyhow::anyhow!("cdc: reference id out of range"))?
                                .as_ref()
                        };
                        push_capped(&mut out, chunk, cap)?;
                    }
                    TAG_LITERAL_CACHE | TAG_LITERAL_NOCACHE => {
                        let len = usize::try_from(payload)
                            .map_err(|_| anyhow::anyhow!("cdc: literal length overflow"))?;
                        let end = pos
                            .checked_add(len)
                            .ok_or_else(|| anyhow::anyhow!("cdc: literal length overflow"))?;
                        if end > data.len() {
                            bail!("cdc: literal overruns token stream");
                        }
                        let chunk = &data[pos..end];
                        pos = end;
                        push_capped(&mut out, chunk, cap)?;
                        if tag == TAG_LITERAL_CACHE {
                            let staged_chunk_count = base_chunk_count
                                .checked_add(pending_chunks.len())
                                .ok_or_else(|| anyhow::anyhow!("cdc: cache length overflow"))?;
                            if staged_chunk_count >= MAX_CACHED_CHUNKS {
                                bail!("cdc: cache cap exceeded (stream desynchronized)");
                            }
                            pending_chunks.push(chunk.into());
                        }
                    }
                    _ => bail!("cdc: invalid token tag"),
                }
            }
            self.chunks.extend(pending_chunks);
            Ok(out)
        }
    }

    fn push_capped(out: &mut Vec<u8>, chunk: &[u8], cap: usize) -> Result<()> {
        if out.len().saturating_add(chunk.len()) > cap {
            bail!("cdc: reconstructed payload exceeds reconstruction cap");
        }
        out.extend_from_slice(chunk);
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn round_trip(payloads: &[&[u8]]) {
            let mut enc = CdcDedupEncoder::new();
            let mut dec = CdcDedupDecoder::new();
            for &p in payloads {
                let encoded = enc.encode(p);
                let decoded = dec.decode(&encoded).expect("decode must succeed");
                assert_eq!(decoded, p, "lossless round-trip failed");
            }
        }

        #[test]
        fn round_trip_empty_and_small() {
            round_trip(&[b"", b"x", b"hello world", b"\x00\x01\x02\xff"]);
        }

        #[test]
        fn round_trip_large_and_binary() {
            let big: Vec<u8> = (0..100_000u32)
                .map(|i| (i.wrapping_mul(2654435761) >> 16) as u8)
                .collect();
            round_trip(&[&big]);
        }

        #[test]
        fn round_trip_repeated_payloads_dedup() {
            // The same payload sent 5x: first carries literals, the rest should
            // be almost entirely references (the win), and all decode exactly.
            let payload: Vec<u8> = b"PROMPT> the quick brown fox jumps over the lazy dog\r\n"
                .iter()
                .cycle()
                .take(8000)
                .copied()
                .collect();
            let mut enc = CdcDedupEncoder::new();
            let mut dec = CdcDedupDecoder::new();
            let first = enc.encode(&payload);
            assert_eq!(dec.decode(&first).unwrap(), payload);
            let mut later_total = 0usize;
            for _ in 0..4 {
                let e = enc.encode(&payload);
                later_total += e.len();
                assert_eq!(dec.decode(&e).unwrap(), payload);
            }
            // A repeated payload must compress hard on the wire.
            assert!(
                later_total < payload.len(),
                "repeated payloads did not dedup: {later_total} vs {}",
                payload.len()
            );
        }

        #[test]
        fn cross_payload_dedup_across_frames() {
            // Content shared between two *different* payloads (mirrored across
            // panes) should reference chunks cached by the earlier payload.
            let shared = b"\x1b[2J\x1b[H==== shared header block repeated across panes ====\r\n";
            let a: Vec<u8> = shared
                .iter()
                .chain(b"pane-A unique tail")
                .copied()
                .collect();
            let b: Vec<u8> = shared
                .iter()
                .chain(b"pane-B unique tail")
                .copied()
                .collect();
            round_trip(&[&a, &b]);
        }

        #[test]
        fn encoding_is_deterministic() {
            let data = b"deterministic content-defined chunking, same in same out".repeat(50);
            let e1 = CdcDedupEncoder::new().encode(&data);
            let e2 = CdcDedupEncoder::new().encode(&data);
            assert_eq!(e1, e2);
        }

        #[test]
        fn decode_arbitrary_input_never_panics() {
            // A fresh decoder on pseudo-random bytes must return Ok or Err, never
            // panic or hang. (proptest-style smoke without the dep.)
            let mut state = 0xDEAD_BEEF_CAFE_F00Du64;
            for _ in 0..5000 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let len = (state >> 56) as usize % 64;
                let bytes: Vec<u8> = (0..len)
                    .map(|k| {
                        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                        (state >> 33) as u8 ^ k as u8
                    })
                    .collect();
                let _ = CdcDedupDecoder::new().decode(&bytes);
            }
        }

        #[test]
        fn reference_out_of_range_is_rejected() {
            // A REFERENCE to id 0 with an empty dictionary must fail, not panic.
            let mut out = Vec::new();
            write_leb(&mut out, TAG_REFERENCE);
            assert!(CdcDedupDecoder::new().decode(&out).is_err());
        }

        #[test]
        fn leb128_overflow_is_rejected() {
            // Ten-byte u64 LEB128 values may only use bit 63 in the final byte.
            // Anything larger must fail closed rather than wrapping to a small
            // literal length/reference id.
            let mut overflow = vec![0x80; 9];
            overflow.push(0x02);
            assert!(CdcDedupDecoder::new().decode(&overflow).is_err());
        }

        #[test]
        fn failed_decode_does_not_poison_decoder_cache() {
            let payload = b"legitimate CDC payload".repeat(16);
            let mut enc = CdcDedupEncoder::new();
            let first = enc.encode(&payload);
            let second = enc.encode(&payload);

            let mut poisoned = Vec::new();
            write_leb(&mut poisoned, (6u64 << 2) | TAG_LITERAL_CACHE);
            poisoned.extend_from_slice(b"poison");
            poisoned.push(0x80); // truncated next leb128 token

            let mut dec = CdcDedupDecoder::new();
            assert!(dec.decode(&poisoned).is_err());
            assert!(
                dec.chunks.is_empty(),
                "failed token streams must not commit decoder cache entries"
            );
            assert_eq!(dec.decode(&first).expect("first valid frame"), payload);
            assert_eq!(
                dec.decode(&second).expect("reference frame after failure"),
                payload,
                "failed malicious frame poisoned the reference dictionary"
            );
        }

        // -- gate-hardening: adversarial / property round-trip (ft-6c1t0) --
        // A dedup that loses or corrupts a single byte must fail these.

        fn lcg(state: &mut u64) -> u64 {
            *state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *state >> 33
        }

        fn random_bytes(state: &mut u64, len: usize) -> Vec<u8> {
            (0..len).map(|_| (lcg(state) & 0xff) as u8).collect()
        }

        /// Round-trip one stream through a fresh encoder/decoder; assert exact.
        fn assert_lossless(stream: &[u8]) {
            let mut enc = CdcDedupEncoder::new();
            let mut dec = CdcDedupDecoder::new();
            let encoded = enc.encode(stream);
            let decoded = dec.decode(&encoded).expect("decode must succeed");
            assert_eq!(
                decoded,
                stream,
                "lossless round-trip lost/corrupted bytes (len={})",
                stream.len()
            );
        }

        /// Round-trip a sequence through ONE encoder/decoder pair (cross-frame
        /// dedup statefulness); assert each frame is exact.
        fn assert_lossless_sequence(streams: &[Vec<u8>]) {
            let mut enc = CdcDedupEncoder::new();
            let mut dec = CdcDedupDecoder::new();
            for s in streams {
                let encoded = enc.encode(s);
                let decoded = dec.decode(&encoded).expect("decode must succeed");
                assert_eq!(&decoded, s, "stateful round-trip lost/corrupted bytes");
            }
        }

        #[test]
        fn gate_adversarial_roundtrip_is_byte_exact() {
            // empty + every single-byte value class
            assert_lossless(b"");
            for b in [0x00u8, 0x01, 0x41, 0x7f, 0x80, 0xff] {
                assert_lossless(&[b]);
            }

            // all-same-byte at lengths straddling MIN/AVG/MAX chunk sizes
            for &len in &[2usize, 63, 64, 65, 255, 256, 257, 1023, 1024, 1025, 9000] {
                for b in [0x00u8, 0x41, 0xff] {
                    assert_lossless(&vec![b; len]);
                }
            }

            // highly repetitive (short ANSI-ish pattern cycled large)
            let rep: Vec<u8> = b"\x1b[32mok\x1b[0m "
                .iter()
                .cycle()
                .take(50_000)
                .copied()
                .collect();
            assert_lossless(&rep);

            // random streams of many lengths (fresh enc/dec each)
            let mut st = 0x0123_4567_89ab_cdefu64;
            for _ in 0..400 {
                let len = (lcg(&mut st) % 4096) as usize;
                let bytes = random_bytes(&mut st, len);
                assert_lossless(&bytes);
            }
            for &len in &[10_000usize, 65_537, 131_072] {
                let bytes = random_bytes(&mut st, len);
                assert_lossless(&bytes);
            }

            // chunk-boundary-straddling: an identical block at every alignment in
            // 0..=130 (content-defined chunking must still round-trip exactly when
            // the same bytes land at different offsets relative to the boundary).
            let block: Vec<u8> = b"REPEATED-BLOCK-0123456789-"
                .iter()
                .cycle()
                .take(3000)
                .copied()
                .collect();
            for shift in 0..=130usize {
                let mut s = vec![0x2au8; shift];
                s.extend_from_slice(&block);
                assert_lossless(&s);
            }

            // stateful cross-frame: overlapping payloads through one pair
            let shared: Vec<u8> = b"==== mirrored across panes ====\r\n"
                .iter()
                .cycle()
                .take(4000)
                .copied()
                .collect();
            let frames: Vec<Vec<u8>> = (0..8u8)
                .map(|tag| {
                    let mut f = shared.clone();
                    f.push(tag);
                    f
                })
                .collect();
            assert_lossless_sequence(&frames);
        }

        #[test]
        fn gate_cache_eviction_roundtrip_is_lossless() {
            // Feed > MAX_CACHED_CHUNKS distinct sub-MIN_CHUNK payloads (each is a
            // single distinct chunk) through one pair. Once the dictionary fills,
            // the encoder falls back to LITERAL_NOCACHE and the decoder must
            // reconstruct those uncached chunks exactly — no byte may be lost.
            let mut enc = CdcDedupEncoder::new();
            let mut dec = CdcDedupDecoder::new();
            let total = MAX_CACHED_CHUNKS + 2048;
            for i in 0..total {
                let mut chunk = [0u8; 24]; // < MIN_CHUNK => exactly one chunk
                chunk[..8].copy_from_slice(&(i as u64).to_le_bytes());
                chunk[8] = 0xAB;
                let encoded = enc.encode(&chunk);
                let decoded = dec.decode(&encoded).expect("decode must succeed");
                assert_eq!(
                    decoded,
                    chunk.to_vec(),
                    "post-eviction round-trip corrupted bytes (i={i})"
                );
            }
            assert_eq!(
                enc.cached_chunks(),
                MAX_CACHED_CHUNKS,
                "cache did not fill — the eviction path was never exercised"
            );
        }

        #[test]
        fn gate_exceeds_reconstruction_cap_is_rejected_not_corrupted() {
            // A stream whose reconstruction exceeds the cap must fail closed
            // (Err) rather than truncate/corrupt. A small injected cap keeps the
            // test cheap; the production cap is MAX_PDU_SIZE.
            let payload: Vec<u8> = (0..20_000u32)
                .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
                .collect();
            let encoded = CdcDedupEncoder::new().encode(&payload);

            assert_eq!(
                CdcDedupDecoder::new()
                    .decode_with_cap(&encoded, payload.len())
                    .expect("decode at exact cap must succeed"),
                payload,
                "exact-cap decode must reproduce the payload byte-for-byte"
            );

            assert!(
                CdcDedupDecoder::new()
                    .decode_with_cap(&encoded, payload.len() - 1)
                    .is_err(),
                "over-cap reconstruction must be rejected, not silently corrupted"
            );
        }
    }
}

#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub use bounded_varbincode::deserialize as bounded_varbincode_deserialize_for_fuzz;

#[cfg(test)]
mod runtime {
    #[cfg(all(feature = "async-asupersync", not(feature = "async-smol")))]
    static ASUPERSYNC_RUNTIME: std::sync::LazyLock<asupersync::runtime::Runtime> =
        std::sync::LazyLock::new(|| {
            asupersync::runtime::RuntimeBuilder::current_thread()
                .build()
                .expect("failed to build codec asupersync runtime")
        });

    #[cfg(all(feature = "async-asupersync", not(feature = "async-smol")))]
    pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
        ASUPERSYNC_RUNTIME.block_on(future)
    }

    #[cfg(feature = "async-smol")]
    pub fn block_on<F: std::future::Future>(future: F) -> F::Output {
        smol::block_on(future)
    }

    #[cfg(all(feature = "async-asupersync", not(feature = "async-smol")))]
    pub type Cursor<T> = std::io::Cursor<T>;

    #[cfg(feature = "async-smol")]
    pub type Cursor<T> = smol::io::Cursor<T>;
}

#[derive(Error, Clone, PartialEq, Eq)]
pub enum CorruptResponse {
    #[error("Corrupt Response: {0}")]
    Message(String),
    #[error(
        "Corrupt Response: serial {serial} exceeds the highest serial issued by this transport \
         ({max_serial})"
    )]
    SerialAboveCeiling { serial: u64, max_serial: u64 },
}

impl std::fmt::Debug for CorruptResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Message(message) => f.debug_tuple("CorruptResponse").field(message).finish(),
            Self::SerialAboveCeiling { serial, max_serial } => f
                .debug_struct("CorruptResponse::SerialAboveCeiling")
                .field("serial", serial)
                .field("max_serial", max_serial)
                .finish(),
        }
    }
}

/// Returns the encoded length of the leb128 representation of value
fn encoded_length(value: u64) -> usize {
    let mut len = 1;
    let mut remaining = value >> 7;
    while remaining != 0 {
        len += 1;
        remaining >>= 7;
    }

    len
}

const COMPRESSED_MASK: u64 = 1 << 63;
/// Maximum allowed PDU payload size (256 MB). Prevents allocation bombs from
/// malformed or malicious length fields.
const MAX_PDU_SIZE: usize = 256 * 1024 * 1024;
// `bounded_varbincode` legitimately performs many small reads. Feeding those
// directly into zstd would turn field decoding into repeated decompressor/FFI
// calls. Small authority PDUs use a modest floor; larger compressed payloads
// scale only as far as zstd's bounded recommended output size.
const MIN_EXACT_ZSTD_DECODE_BUFFER_SIZE: usize = 8 * 1024;
const PAYLOAD_READ_CHUNK: usize = 64 * 1024;
// Keep the abandoned-body memory envelope independent from materializing
// decoder growth-policy tuning.
const DISCARDED_PAYLOAD_READ_CHUNK: usize = 64 * 1024;

fn max_pdu_read_limit() -> anyhow::Result<u64> {
    u64::try_from(MAX_PDU_SIZE)
        .context("MAX_PDU_SIZE does not fit in u64")?
        .checked_add(1)
        .context("MAX_PDU_SIZE read limit overflow")
}

fn encoded_frame_len(
    ident: u64,
    serial: u64,
    data_len: usize,
    is_compressed: bool,
) -> anyhow::Result<usize> {
    let len = data_len
        .checked_add(encoded_length(ident))
        .and_then(|len| len.checked_add(encoded_length(serial)))
        .context("encoded PDU body length overflow")?;
    let len_u64 = u64::try_from(len).context("encoded PDU length does not fit in u64")?;
    let masked_len = if is_compressed {
        len_u64 | COMPRESSED_MASK
    } else {
        len_u64
    };
    len.checked_add(encoded_length(masked_len))
        .context("encoded PDU frame length overflow")
}

fn encode_raw_as_vec(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
) -> anyhow::Result<Vec<u8>> {
    encode_raw_as_vec_impl(ident, serial, data, is_compressed, true)
}

fn encode_raw_as_vec_impl(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    record_metrics: bool,
) -> anyhow::Result<Vec<u8>> {
    let len = data
        .len()
        .checked_add(encoded_length(ident))
        .and_then(|len| len.checked_add(encoded_length(serial)))
        .context("encoded PDU body length overflow")?;
    let len_u64 = u64::try_from(len).context("encoded PDU length does not fit in u64")?;
    let masked_len = if is_compressed {
        len_u64 | COMPRESSED_MASK
    } else {
        len_u64
    };

    // Double-buffer the data; since we run with nodelay enabled, it is
    // desirable for the write to be a single packet (or at least, for
    // the header portion to go out in a single packet)
    let capacity = encoded_frame_len(ident, serial, data.len(), is_compressed)?;
    let mut buffer = Vec::with_capacity(capacity);

    leb128::write::unsigned(&mut buffer, masked_len).context("writing pdu len")?;
    leb128::write::unsigned(&mut buffer, serial).context("writing pdu serial")?;
    leb128::write::unsigned(&mut buffer, ident).context("writing pdu ident")?;
    buffer.extend_from_slice(data);

    if record_metrics {
        if is_compressed {
            metrics::histogram!("pdu.encode.compressed.size").record(buffer.len() as f64);
        } else {
            metrics::histogram!("pdu.encode.size").record(buffer.len() as f64);
        }
    }

    Ok(buffer)
}

/// Encode a frame.  If the data is compressed, the high bit of the length
/// is set to indicate that.  The data written out has the format:
/// tagged_len: leb128  (u64 msb is set if data is compressed)
/// serial: leb128
/// ident: leb128
/// data bytes
fn encode_raw<W: std::io::Write>(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    mut w: W,
) -> anyhow::Result<usize> {
    let buffer = encode_raw_as_vec(ident, serial, data, is_compressed)?;
    w.write_all(&buffer).context("writing pdu data buffer")?;
    Ok(buffer.len())
}

async fn encode_raw_async<W: Unpin + AsyncWriteExt>(
    ident: u64,
    serial: u64,
    data: &[u8],
    is_compressed: bool,
    w: &mut W,
) -> anyhow::Result<usize> {
    let buffer = encode_raw_as_vec(ident, serial, data, is_compressed)?;
    w.write_all(&buffer)
        .await
        .context("writing pdu data buffer")?;
    Ok(buffer.len())
}

/// Read a single leb128 encoded value from the stream
async fn read_u64_async_with_len<R>(r: &mut R) -> anyhow::Result<(u64, usize)>
where
    R: Unpin + AsyncRead + std::fmt::Debug,
{
    let mut buf = vec![];
    loop {
        let mut byte = [0u8];
        if let Err(err) = r.read_exact(&mut byte).await {
            if err.kind() == std::io::ErrorKind::UnexpectedEof {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF while reading leb128 encoded value",
                )
                .into());
            }

            return Err(err.into());
        }
        let [decoded_byte] = byte;
        buf.push(decoded_byte);

        match leb128::read::unsigned(&mut buf.as_slice()) {
            Ok(n) => {
                return Ok((n, buf.len()));
            }
            Err(leb128::read::Error::IoError(_)) => continue,
            Err(leb128::read::Error::Overflow) => anyhow::bail!("leb128 is too large"),
        }
    }
}

/// Read a single leb128 encoded value from the stream.
async fn read_u64_async<R>(r: &mut R) -> anyhow::Result<u64>
where
    R: Unpin + AsyncRead + std::fmt::Debug,
{
    read_u64_async_with_len(r).await.map(|(value, _)| value)
}

/// Read a single leb128 encoded value from the stream
fn read_u64_with_len<R: std::io::Read>(r: &mut R) -> anyhow::Result<(u64, usize)> {
    let mut buf = vec![];
    loop {
        let mut byte = [0u8];
        r.read_exact(&mut byte).context("reading leb128")?;
        let [decoded_byte] = byte;
        buf.push(decoded_byte);

        match leb128::read::unsigned(&mut buf.as_slice()) {
            Ok(n) => return Ok((n, buf.len())),
            Err(leb128::read::Error::IoError(_)) => continue,
            Err(leb128::read::Error::Overflow) => anyhow::bail!("leb128 is too large"),
        }
    }
}

/// Read a single leb128 encoded value from the stream
fn read_u64<R: std::io::Read>(mut r: R) -> anyhow::Result<u64> {
    read_u64_with_len(&mut r).map(|(value, _)| value)
}

#[derive(Debug)]
struct Decoded {
    ident: u64,
    serial: u64,
    data: Vec<u8>,
    is_compressed: bool,
}

/// A complete or still-arriving frame declared a size above a caller's cap.
///
/// The declared length belongs to the first frame only; bytes already
/// coalesced from later frames are deliberately excluded.
#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "buffered PDU frame declares {declared_frame_bytes} bytes, exceeding caller limit \
     {max_frame_bytes}"
)]
pub struct StreamingPduFrameLimitExceeded {
    declared_frame_bytes: usize,
    max_frame_bytes: usize,
}

impl StreamingPduFrameLimitExceeded {
    #[must_use]
    pub const fn declared_frame_bytes(&self) -> usize {
        self.declared_frame_bytes
    }

    #[must_use]
    pub const fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes
    }
}

fn buffered_frame_len_with_limit(
    buffer: &[u8],
    max_frame_bytes: usize,
) -> anyhow::Result<Option<usize>> {
    let mut slice = buffer;
    let tagged_len = match leb128::read::unsigned(&mut slice) {
        Ok(len) => len,
        Err(leb128::read::Error::IoError(err))
            if matches!(
                err.kind(),
                std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::WouldBlock
            ) =>
        {
            return Ok(None);
        }
        Err(leb128::read::Error::IoError(err)) => {
            return Err(anyhow::Error::new(err).context("reading buffered PDU length"));
        }
        Err(leb128::read::Error::Overflow) => anyhow::bail!("buffered PDU length leb128 overflow"),
    };

    let raw_len = tagged_len & !COMPRESSED_MASK;
    let payload_len: usize = raw_len
        .try_into()
        .map_err(|_| anyhow::anyhow!("buffered PDU length {raw_len} does not fit in usize"))?;

    let prefix_len = buffer.len().saturating_sub(slice.len());
    let total_len = prefix_len
        .checked_add(payload_len)
        .context("buffered PDU length overflow")?;

    if total_len > max_frame_bytes {
        return Err(StreamingPduFrameLimitExceeded {
            declared_frame_bytes: total_len,
            max_frame_bytes,
        }
        .into());
    }

    // [ft-phz7x] Reject oversize headers BEFORE an unlimited caller's read
    // loop accumulates an attacker-advertised payload. A tighter caller limit
    // is checked first so it retains its typed authority and recovery class.
    if payload_len > MAX_PDU_SIZE {
        anyhow::bail!(
            "buffered PDU payload size {} exceeds maximum {} — refusing to accumulate",
            payload_len,
            MAX_PDU_SIZE,
        );
    }

    if buffer.len() < total_len {
        return Ok(None);
    }

    Ok(Some(total_len))
}

#[cfg(test)]
fn buffered_frame_len(buffer: &[u8]) -> anyhow::Result<Option<usize>> {
    buffered_frame_len_with_limit(buffer, usize::MAX)
}

fn reserve_next_payload_chunk(
    data: &mut Vec<u8>,
    data_len: usize,
    len: u64,
    serial: u64,
    ident: u64,
) -> anyhow::Result<(usize, usize)> {
    let start = data.len();
    let chunk_len = data_len.saturating_sub(start).min(PAYLOAD_READ_CHUNK);
    let end = start
        .checked_add(chunk_len)
        .context("payload chunk length overflow")?;
    // Amortized (geometric) growth, NOT `try_reserve_exact`. This runs once per
    // PAYLOAD_READ_CHUNK (64 KiB) while accumulating a multi-chunk payload, so
    // `_exact` -- which never over-allocates -- reallocated and recopied the
    // ENTIRE buffer on every chunk: O(n^2) memcpy in the payload size (a 256 MiB
    // PDU recopies on the order of hundreds of GiB). `try_reserve` over-allocates
    // geometrically, making total growth O(n) amortized. It still bounds
    // allocation to what has actually been DELIVERED (capacity tracks bytes read
    // so far, ~x2, never the attacker-advertised `data_len`), preserving the
    // incremental-allocation DoS guard; and it stays fallible to reject OOM
    // rather than abort. Output `data` is byte-identical -- only capacity differs.
    data.try_reserve(chunk_len).with_context(|| {
        format!(
            "allocating next {} bytes for PDU payload of length {} \
            with frame length {} serial={} ident={}",
            chunk_len, data_len, len, serial, ident
        )
    })?;
    data.resize(end, 0);
    Ok((start, end))
}

fn read_payload_chunked<R: std::io::Read>(
    r: &mut R,
    data_len: usize,
    len: u64,
    serial: u64,
    ident: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut data = Vec::new();
    while data.len() < data_len {
        let (start, end) = reserve_next_payload_chunk(&mut data, data_len, len, serial, ident)?;
        let payload_chunk = data
            .get_mut(start..end)
            .context("reserved payload chunk range missing")?;
        r.read_exact(payload_chunk).with_context(|| {
            format!(
                "reading bytes {}..{} of {} for PDU of length {} \
                with serial={} ident={}",
                start, end, data_len, len, serial, ident
            )
        })?;
    }
    Ok(data)
}

async fn read_payload_chunked_async<R: Unpin + AsyncRead + std::fmt::Debug>(
    r: &mut R,
    data_len: usize,
    len: u64,
    serial: u64,
    ident: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut data = Vec::new();
    while data.len() < data_len {
        let (start, end) = reserve_next_payload_chunk(&mut data, data_len, len, serial, ident)?;
        let payload_chunk = data
            .get_mut(start..end)
            .context("reserved async payload chunk range missing")?;
        r.read_exact(payload_chunk).await.with_context(|| {
            format!(
                "decode_raw_async failed to read bytes {}..{} of {} \
                for PDU of length {} with serial={} ident={}",
                start, end, data_len, len, serial, ident
            )
        })?;
    }
    Ok(data)
}

/// Validated mux frame metadata whose payload has not yet been read.
///
/// The fields are intentionally private and this type is intentionally neither
/// `Clone` nor `Copy`. It is exposed only by reference to the synchronous
/// selector passed to [`Pdu::decode_async_with_selector`], so external callers
/// cannot leave the ordered stream half-consumed by taking ownership of it.
#[derive(Debug, PartialEq, Eq)]
pub struct PduFrameHeader {
    frame_len: u64,
    data_len: usize,
    serial: u64,
    ident: u64,
    is_compressed: bool,
}

impl PduFrameHeader {
    /// Request/reply correlation serial from the validated frame header.
    #[must_use]
    pub const fn serial(&self) -> u64 {
        self.serial
    }

    /// PDU identifier/version tag from the validated frame header.
    #[must_use]
    pub const fn ident(&self) -> u64 {
        self.ident
    }

    /// Encoded payload bytes that follow this header on the stream.
    #[must_use]
    pub const fn encoded_payload_len(&self) -> usize {
        self.data_len
    }

    /// Whether the encoded payload carries the frame compression flag.
    #[must_use]
    pub const fn is_compressed(&self) -> bool {
        self.is_compressed
    }
}

/// Non-content accounting for one successfully discarded frame body.
///
/// These values make the fixed-memory contract directly testable without
/// retaining or formatting any payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscardedPduBody {
    encoded_bytes: usize,
    chunk_reads: usize,
    max_chunk_bytes: usize,
}

/// Body action selected after validated frame metadata is available and before
/// any payload allocation or materialization occurs.
///
/// `Discard` is an authorization decision by the selector: it consumes the
/// encoded bytes of an uncompressed body without deserializing or validating
/// that body's payload schema. Compressed bodies cannot use this path and are
/// rejected before any raw drainage begins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PduBodyDisposition {
    Materialize,
    Discard,
}

/// Result of one codec-owned header-plus-body async operation.
#[derive(Debug, PartialEq)]
// `DecodedPdu` already contains the deliberately inline `Pdu` enum. Boxing the
// normal branch here would add a heap allocation to every live frame merely to
// shrink this short-lived selector result; the discard branch exists to avoid
// allocating abandoned payloads, not to penalize ordinary decoding.
#[allow(clippy::large_enum_variant)]
pub enum AsyncPduDecode {
    Decoded(DecodedPdu),
    Discarded {
        serial: u64,
        ident: u64,
        body: DiscardedPduBody,
    },
}

impl DiscardedPduBody {
    #[must_use]
    pub const fn encoded_bytes(self) -> usize {
        self.encoded_bytes
    }

    #[must_use]
    pub const fn chunk_reads(self) -> usize {
        self.chunk_reads
    }

    #[must_use]
    pub const fn max_chunk_bytes(self) -> usize {
        self.max_chunk_bytes
    }

    /// Maximum scratch-buffer capacity for a discarded body. Tiny payloads use
    /// a smaller heap buffer; peer-advertised lengths can never raise this cap.
    #[must_use]
    pub const fn scratch_capacity() -> usize {
        DISCARDED_PAYLOAD_READ_CHUNK
    }
}

fn decoded_payload_len(
    label: &str,
    len: u64,
    serial: u64,
    serial_len: usize,
    ident: u64,
    ident_len: usize,
) -> anyhow::Result<usize> {
    let header_len = serial_len
        .checked_add(ident_len)
        .with_context(|| format!("{label}: serial + ident header length overflow"))?;
    let frame_len = match usize::try_from(len) {
        Ok(frame_len) => frame_len,
        Err(_) => {
            return Err(CorruptResponse::Message(format!(
                "{label}: PDU length {len} does not fit in usize"
            ))
            .into());
        }
    };

    match frame_len.checked_sub(header_len) {
        Some(data_len) => Ok(data_len),
        None => Err(CorruptResponse::Message(format!(
            "{label}: sizes don't make sense: \
             len:{len} serial:{serial} (enc={serial_len}) ident:{ident} (enc={ident_len})",
        ))
        .into()),
    }
}

/// Validate and consume only a frame header, leaving its payload unread.
async fn decode_raw_header_async<R: Unpin + AsyncRead + std::fmt::Debug>(
    r: &mut R,
    max_serial: Option<u64>,
) -> anyhow::Result<PduFrameHeader> {
    let (len, _len_len) = read_u64_async_with_len(r)
        .await
        .context("decode_raw_async failed to read PDU length")?;
    let (len, is_compressed) = if (len & COMPRESSED_MASK) != 0 {
        (len & !COMPRESSED_MASK, true)
    } else {
        (len, false)
    };
    let (serial, serial_len) = read_u64_async_with_len(r)
        .await
        .context("decode_raw_async failed to read PDU serial")?;
    if let Some(max_serial) = max_serial {
        if serial > max_serial {
            return Err(CorruptResponse::SerialAboveCeiling { serial, max_serial }.into());
        }
    }
    let (ident, ident_len) = read_u64_async_with_len(r)
        .await
        .context("decode_raw_async failed to read PDU ident")?;
    let data_len = decoded_payload_len(
        "decode_raw_async",
        len,
        serial,
        serial_len,
        ident,
        ident_len,
    )?;

    if data_len > MAX_PDU_SIZE {
        anyhow::bail!(
            "decode_raw_async: PDU payload size {} exceeds maximum {} \
            (serial={} ident={})",
            data_len,
            MAX_PDU_SIZE,
            serial,
            ident
        );
    }

    if is_compressed {
        metrics::histogram!("pdu.decode.compressed.size").record(data_len as f64);
    } else {
        metrics::histogram!("pdu.decode.size").record(data_len as f64);
    }

    Ok(PduFrameHeader {
        frame_len: len,
        data_len,
        serial,
        ident,
        is_compressed,
    })
}

async fn decode_raw_body_async<R: Unpin + AsyncRead + std::fmt::Debug>(
    r: &mut R,
    header: PduFrameHeader,
) -> anyhow::Result<Decoded> {
    let data = read_payload_chunked_async(
        r,
        header.data_len,
        header.frame_len,
        header.serial,
        header.ident,
    )
    .await?;
    Ok(Decoded {
        ident: header.ident,
        serial: header.serial,
        data,
        is_compressed: header.is_compressed,
    })
}

async fn discard_raw_body_async<R: Unpin + AsyncRead + std::fmt::Debug>(
    r: &mut R,
    header: PduFrameHeader,
) -> anyhow::Result<DiscardedPduBody> {
    // Keep the bounded discard window off the async worker stack. This future is
    // nested inside the client actor's already-large decode future; embedding a
    // 64 KiB array here can overflow bounded executor stacks while the future is
    // moved or polled.  Reserve fallibly before setting the length so allocation
    // failure remains an ordinary decode error rather than an abort.
    let scratch_len = header.data_len.min(DISCARDED_PAYLOAD_READ_CHUNK);
    let mut scratch = Vec::new();
    scratch
        .try_reserve_exact(scratch_len)
        .context("allocating abandoned-PDU discard scratch buffer")?;
    scratch.resize(scratch_len, 0_u8);
    let mut consumed = 0_usize;
    let mut chunk_reads = 0_usize;
    let mut max_chunk_bytes = 0_usize;

    while consumed < header.data_len {
        let read_len = header
            .data_len
            .saturating_sub(consumed)
            .min(scratch.len());
        r.read_exact(&mut scratch[..read_len]).await.with_context(|| {
            format!(
                "discarding bytes {}..{} of {} for abandoned PDU body \
                 with serial={} ident={}",
                consumed,
                consumed.saturating_add(read_len),
                header.data_len,
                header.serial,
                header.ident,
            )
        })?;
        consumed = consumed
            .checked_add(read_len)
            .context("discarded PDU payload length overflow")?;
        chunk_reads = chunk_reads
            .checked_add(1)
            .context("discarded PDU chunk count overflow")?;
        max_chunk_bytes = max_chunk_bytes.max(read_len);
    }

    Ok(DiscardedPduBody {
        encoded_bytes: consumed,
        chunk_reads,
        max_chunk_bytes,
    })
}

/// Decode a frame.
/// See encode_raw() for the frame format.
async fn decode_raw_async<R: Unpin + AsyncRead + std::fmt::Debug>(
    r: &mut R,
    max_serial: Option<u64>,
) -> anyhow::Result<Decoded> {
    let header = decode_raw_header_async(r, max_serial).await?;
    decode_raw_body_async(r, header).await
}

/// Decode a frame.
/// See encode_raw() for the frame format.
fn decode_raw<R: std::io::Read>(mut r: R) -> anyhow::Result<Decoded> {
    decode_raw_impl(&mut r, true)
}

fn decode_raw_impl<R: std::io::Read>(mut r: R, record_metrics: bool) -> anyhow::Result<Decoded> {
    let (len, _len_len) = read_u64_with_len(&mut r).context("reading PDU length")?;
    let (len, is_compressed) = if (len & COMPRESSED_MASK) != 0 {
        (len & !COMPRESSED_MASK, true)
    } else {
        (len, false)
    };
    let (serial, serial_len) = read_u64_with_len(&mut r).context("reading PDU serial")?;
    let (ident, ident_len) = read_u64_with_len(&mut r).context("reading PDU ident")?;
    let data_len = decoded_payload_len("decode_raw", len, serial, serial_len, ident, ident_len)?;

    if data_len > MAX_PDU_SIZE {
        anyhow::bail!(
            "PDU payload size {} exceeds maximum {} (serial={} ident={})",
            data_len,
            MAX_PDU_SIZE,
            serial,
            ident
        );
    }

    if record_metrics {
        if is_compressed {
            metrics::histogram!("pdu.decode.compressed.size").record(data_len as f64);
        } else {
            metrics::histogram!("pdu.decode.size").record(data_len as f64);
        }
    }

    let data = read_payload_chunked(&mut r, data_len, len, serial, ident)?;
    Ok(Decoded {
        ident,
        serial,
        data,
        is_compressed,
    })
}

#[derive(Debug, PartialEq)]
pub struct DecodedPdu {
    pub serial: u64,
    pub pdu: Pdu,
}

/// A decoded PDU bound to optional logical-retention admission metadata.
///
/// Private fields prevent callers from pairing a charge with a different
/// decoded value. The charge is the conservative size of an uncompressed
/// serial-zero frame containing the complete decoded payload bytes, including
/// compatible additive tails. It is not a measurement of Rust heap capacity,
/// allocator overhead, or process RSS.
#[derive(Debug)]
pub struct DecodedPduWithRetentionMetadata {
    decoded: DecodedPdu,
    retained_frame_bytes: Option<usize>,
}

impl DecodedPduWithRetentionMetadata {
    #[must_use]
    pub const fn decoded(&self) -> &DecodedPdu {
        &self.decoded
    }

    #[must_use]
    pub const fn retained_frame_bytes(&self) -> Option<usize> {
        self.retained_frame_bytes
    }

    #[must_use]
    pub fn into_parts(self) -> (DecodedPdu, Option<usize>) {
        (self.decoded, self.retained_frame_bytes)
    }
}

/// If the serialized size is larger than this, then we'll consider compressing it
const COMPRESS_THRESH: usize = 32;

/// Wire compression policy for PDU encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionMode {
    /// Preserve legacy behavior: compress only when beneficial.
    Auto,
    /// Always compress payload bytes before framing.
    Always,
    /// Never compress payload bytes before framing.
    Never,
}

fn serialize<T: serde::Serialize>(t: &T) -> Result<(Vec<u8>, bool), Error> {
    serialize_with_mode(t, CompressionMode::Auto)
}

fn serialize_with_mode<T: serde::Serialize>(
    t: &T,
    compression_mode: CompressionMode,
) -> Result<(Vec<u8>, bool), Error> {
    // Serialize once into `uncompressed`. If we end up needing compression,
    // we feed THIS buffer through zstd directly via `encode_all` instead of
    // re-running the serializer through a streaming zstd encoder. ft-gbpoy.
    let mut uncompressed = Vec::with_capacity(64);
    let mut encode = varbincode::Serializer::new(&mut uncompressed);
    t.serialize(&mut encode)?;

    if compression_mode == CompressionMode::Never {
        return Ok((uncompressed, false));
    }

    if compression_mode == CompressionMode::Auto && uncompressed.len() <= COMPRESS_THRESH {
        return Ok((uncompressed, false));
    }
    // It's a little heavy; compress the already-serialized buffer.
    // Replaces the previous "serialize a second time through zstd::Encoder"
    // pattern, which doubled serializer work above the threshold (ft-gbpoy).
    let compressed =
        zstd::stream::encode_all(uncompressed.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)?;

    log::debug!(
        "serialized+compress len {} vs {}",
        compressed.len(),
        uncompressed.len()
    );

    if compression_mode == CompressionMode::Always {
        return Ok((compressed, true));
    }

    if compressed.len() < uncompressed.len() {
        Ok((compressed, true))
    } else {
        Ok((uncompressed, false))
    }
}

fn deserialize<T: serde::de::DeserializeOwned, R: std::io::Read>(
    r: R,
    is_compressed: bool,
) -> Result<T, Error> {
    let read_limit = max_pdu_read_limit()?;
    if is_compressed {
        let mut decompress = zstd::Decoder::new(r)?.take(read_limit);
        bounded_varbincode::deserialize(&mut decompress).map_err(Into::into)
    } else {
        let mut limited = r.take(read_limit);
        bounded_varbincode::deserialize(&mut limited).map_err(Into::into)
    }
}

fn deserialize_with_retention_payload_len<T: serde::de::DeserializeOwned>(
    data: &[u8],
    is_compressed: bool,
) -> Result<(T, usize), Error> {
    if !is_compressed {
        let decoded = deserialize(data, false)?;
        return Ok((decoded, data.len()));
    }

    // Count decompressed bytes through the original typed decode. Draining the
    // same bounded reader validates the complete compressed payload and counts
    // compatible additive tails without allocating a second full payload.
    let read_limit = max_pdu_read_limit()?;
    let decoder = zstd::Decoder::with_buffer(data)?;
    let recommended_output_size = zstd::Decoder::<&[u8]>::recommended_output_size()
        .max(MIN_EXACT_ZSTD_DECODE_BUFFER_SIZE);
    let output_buffer_size = data
        .len()
        .clamp(MIN_EXACT_ZSTD_DECODE_BUFFER_SIZE, recommended_output_size);
    let mut reader = BufReader::with_capacity(output_buffer_size, decoder).take(read_limit);
    let decoded = bounded_varbincode::deserialize::<T, _>(&mut reader)?;
    std::io::copy(&mut reader, &mut std::io::sink())
        .context("draining retention-metadata PDU payload")?;
    let decompressed_bytes = read_limit
        .checked_sub(reader.limit())
        .context("counting retention-metadata PDU payload bytes")?;
    if decompressed_bytes
        > u64::try_from(MAX_PDU_SIZE).context("MAX_PDU_SIZE does not fit in u64")?
    {
        bail!(
            "retention-metadata PDU decompressed payload size exceeds maximum {}",
            MAX_PDU_SIZE
        );
    }

    let buffered_decoder = reader.into_inner();
    debug_assert!(buffered_decoder.buffer().is_empty());
    let decoder = buffered_decoder.into_inner();
    if !decoder.finish().is_empty() {
        bail!("retention-metadata PDU has trailing compressed bytes");
    }
    let decompressed_bytes = usize::try_from(decompressed_bytes)
        .context("retention-metadata PDU payload length does not fit in usize")?;
    Ok((decoded, decompressed_bytes))
}

macro_rules! deserialize_pdu_payload {
    ($data:expr, $is_compressed:expr) => {
        deserialize($data, $is_compressed)
    };
    ($data:expr, $is_compressed:expr, $decoder:path) => {
        $decoder($data, $is_compressed)
    };
}

macro_rules! deserialize_pdu_payload_with_retention_metadata {
    (
        GetPaneRenderChangesResponse,
        $vers:expr,
        $data:expr,
        $is_compressed:expr,
        $collect_metadata:expr
    ) => {{
        if $collect_metadata {
            let (payload, decompressed_bytes) =
                deserialize_with_retention_payload_len($data, $is_compressed)?;
            let retained_frame_bytes = encoded_frame_len(
                <GetPaneRenderChangesResponse as PduWireIdent>::IDENT,
                0,
                decompressed_bytes,
                false,
            )?;
            (payload, Some(retained_frame_bytes))
        } else {
            (deserialize($data, $is_compressed)?, None)
        }
    }};
    (
        $name:ident,
        $vers:expr,
        $data:expr,
        $is_compressed:expr,
        $collect_metadata:expr
        $(, $decoder:path)?
    ) => {{
        (
            deserialize_pdu_payload!($data, $is_compressed $(, $decoder)?)?,
            None,
        )
    }};
}

/// Stable numeric wire identity generated for each concrete PDU payload type.
///
/// Typed RPC callers use this associated constant rather than performing a
/// string lookup at admission time.
pub trait PduWireIdent {
    const IDENT: u64;

    /// Complete admission metadata for this concrete wire payload.
    ///
    /// This associated constant is generated by the same declaration that
    /// assigns [`Self::IDENT`], so a new PDU cannot acquire a wire identity
    /// without also declaring its dialect, producer, role, and capability
    /// policy.
    const WIRE_SPEC: PduWireSpec;
}

/// Endpoint that is permitted to produce a PDU.
///
/// [`Self::Bidirectional`] summarizes PDUs with distinct client and server
/// authorities. It is not itself a concrete endpoint and is therefore never
/// accepted by [`PduWireSpec::authorizes`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PduProducer {
    Client,
    Server,
    Bidirectional,
}

/// Correlation role of one PDU on the ordered mux wire.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PduWireRole {
    /// A client-originated request carrying a non-zero serial.
    Request,
    /// A server-originated response carrying the request's non-zero serial.
    CorrelatedReply,
    /// A server-originated notification carrying serial zero.
    Unilateral,
}

/// Capability admission associated with a PDU family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PduCapabilityUse {
    None,
    /// The PDU establishes or declines the listed capability. The capability
    /// must not be required before this PDU is admitted.
    Negotiates(TopologyCapabilities),
    /// The PDU is legal only after all listed capabilities are established on
    /// the current connection generation.
    Requires(TopologyCapabilities),
}

impl PduCapabilityUse {
    /// Capabilities that must already be established before transmission.
    #[must_use]
    pub const fn required(self) -> TopologyCapabilities {
        match self {
            Self::Requires(required) => required,
            Self::None | Self::Negotiates(_) => TopologyCapabilities::NONE,
        }
    }
}

/// One exact producer/serial-role authority.
///
/// Keeping these as tuples prevents a bidirectional PDU from accidentally
/// acquiring the cartesian product of independently stored producer and role
/// flags (for example, a client-originated unilateral notification).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PduWireAuthority {
    pub producer: PduProducer,
    pub role: PduWireRole,
}

/// Exhaustive static admission policy for one assigned PDU identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PduWireSpec {
    pub ident: u64,
    pub name: &'static str,
    pub min_codec_version: usize,
    pub producer: PduProducer,
    pub capability: PduCapabilityUse,
    pub authorities: &'static [PduWireAuthority],
}

impl PduWireSpec {
    /// Whether this PDU permits the exact concrete producer/role tuple.
    ///
    /// `producer` must be [`PduProducer::Client`] or
    /// [`PduProducer::Server`]; [`PduProducer::Bidirectional`] is summary
    /// metadata rather than an endpoint identity.
    #[must_use]
    pub fn authorizes(&self, producer: PduProducer, role: PduWireRole) -> bool {
        producer != PduProducer::Bidirectional
            && self
                .authorities
                .iter()
                .any(|authority| authority.producer == producer && authority.role == role)
    }
}

macro_rules! pdu_authorities {
    (client_request) => {
        &[PduWireAuthority {
            producer: PduProducer::Client,
            role: PduWireRole::Request,
        }]
    };
    (server_reply) => {
        &[PduWireAuthority {
            producer: PduProducer::Server,
            role: PduWireRole::CorrelatedReply,
        }]
    };
    (server_unilateral) => {
        &[PduWireAuthority {
            producer: PduProducer::Server,
            role: PduWireRole::Unilateral,
        }]
    };
    (server_reply_or_unilateral) => {
        &[
            PduWireAuthority {
                producer: PduProducer::Server,
                role: PduWireRole::CorrelatedReply,
            },
            PduWireAuthority {
                producer: PduProducer::Server,
                role: PduWireRole::Unilateral,
            },
        ]
    };
    (client_request_or_server_unilateral) => {
        &[
            PduWireAuthority {
                producer: PduProducer::Client,
                role: PduWireRole::Request,
            },
            PduWireAuthority {
                producer: PduProducer::Server,
                role: PduWireRole::Unilateral,
            },
        ]
    };
}

macro_rules! pdu_producer {
    (client_request) => {
        PduProducer::Client
    };
    (server_reply) => {
        PduProducer::Server
    };
    (server_unilateral) => {
        PduProducer::Server
    };
    (server_reply_or_unilateral) => {
        PduProducer::Server
    };
    (client_request_or_server_unilateral) => {
        PduProducer::Bidirectional
    };
}

macro_rules! pdu_capability_use {
    (none) => {
        PduCapabilityUse::None
    };
    (negotiates_fenced) => {
        PduCapabilityUse::Negotiates(TopologyCapabilities::FENCED_SNAPSHOT_V1)
    };
    (requires_fenced) => {
        PduCapabilityUse::Requires(TopologyCapabilities::FENCED_SNAPSHOT_V1)
    };
    (negotiates_ordered) => {
        PduCapabilityUse::Negotiates(TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        ))
    };
    (requires_ordered) => {
        PduCapabilityUse::Requires(TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        ))
    };
    (requires_reorder) => {
        PduCapabilityUse::Requires(TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits()
                | TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(),
        ))
    };
}

macro_rules! pdu {
    ($(
        $name:ident: $vers:expr, $min_codec_version:expr,
        $authority_policy:ident, $capability_policy:ident
        $(=> $decoder:path)?
    );* $(;)?) => {
        #[derive(PartialEq, Debug)]
        #[allow(clippy::large_enum_variant)]
        pub enum Pdu {
            Invalid{ident: u64},
            $(
                $name($name)
            ,)*
        }

        $(
            impl PduWireIdent for $name {
                const IDENT: u64 = $vers;
                const WIRE_SPEC: PduWireSpec = PduWireSpec {
                    ident: $vers,
                    name: stringify!($name),
                    min_codec_version: $min_codec_version,
                    producer: pdu_producer!($authority_policy),
                    capability: pdu_capability_use!($capability_policy),
                    authorities: pdu_authorities!($authority_policy),
                };
            }
        )*

        /// Complete registry of assigned PDU wire identities and policies.
        pub const PDU_WIRE_SPECS: &[PduWireSpec] = &[
            $(
                <$name as PduWireIdent>::WIRE_SPEC,
            )*
        ];

        impl Pdu {
            pub fn encode<W: std::io::Write>(&self, w: W, serial: u64) -> Result<(), Error> {
                self.encode_with_mode(w, serial, CompressionMode::Auto)
            }

            pub fn encode_with_mode<W: std::io::Write>(
                &self,
                w: W,
                serial: u64,
                compression_mode: CompressionMode,
            ) -> Result<(), Error> {
                self.validate_before_encode()?;
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed) =
                                serialize_with_mode(s, compression_mode)?;
                            let encoded_size = encode_raw($vers, serial, &data, is_compressed, w)?;
                            log::debug!("encode {} size={encoded_size}", stringify!($name));
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(encoded_size as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(encoded_size as f64);
                            Ok(())
                        }
                    ,)*
                }
            }

            /// Serialize one complete framed PDU without touching a transport.
            ///
            /// Callers that need an authority check immediately before the
            /// first socket write can build the frame here, validate that
            /// authority, and then issue exactly one `write_all`. Unlike
            /// encoding into a temporary `Vec` through `encode`, this returns
            /// the codec's own frame allocation directly and avoids a second
            /// full-frame copy.
            pub fn encode_frame(&self, serial: u64) -> Result<Vec<u8>, Error> {
                self.encode_frame_with_mode(serial, CompressionMode::Auto)
            }

            pub fn encode_frame_with_mode(
                &self,
                serial: u64,
                compression_mode: CompressionMode,
            ) -> Result<Vec<u8>, Error> {
                self.encode_frame_with_mode_impl(serial, compression_mode, true)
            }

            /// Encode an uncompressed frame for bounded in-memory retention
            /// without recording a second outbound wire sample.
            ///
            /// This is intended for a queue that already decoded the physical
            /// frame and needs a compact, ownership-complete representation
            /// while protocol dispatch is temporarily quarantined.
            pub fn encode_retained_frame(&self, serial: u64) -> Result<Vec<u8>, Error> {
                self.encode_frame_with_mode_impl(serial, CompressionMode::Never, false)
            }

            fn encode_frame_with_mode_impl(
                &self,
                serial: u64,
                compression_mode: CompressionMode,
                record_metrics: bool,
            ) -> Result<Vec<u8>, Error> {
                self.validate_before_encode()?;
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed) =
                                serialize_with_mode(s, compression_mode)?;
                            let frame =
                                encode_raw_as_vec_impl(
                                    $vers,
                                    serial,
                                    &data,
                                    is_compressed,
                                    record_metrics,
                                )?;
                            log::debug!(
                                "encode_frame {} size={}",
                                stringify!($name),
                                frame.len()
                            );
                            if record_metrics {
                                metrics::histogram!("pdu.size", "pdu" => stringify!($name))
                                    .record(frame.len() as f64);
                                metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name))
                                    .record(frame.len() as f64);
                            }
                            Ok(frame)
                        }
                    ,)*
                }
            }

            /// Measure the canonical framed size without allocating the final
            /// frame or recording an outbound-size metric.
            ///
            /// The serializer still materializes its payload once. Callers can
            /// select [`CompressionMode::Never`] when the result is used as a
            /// retained-memory admission weight rather than a wire-size sample.
            pub fn encoded_frame_len_with_mode(
                &self,
                serial: u64,
                compression_mode: CompressionMode,
            ) -> Result<usize, Error> {
                self.validate_before_encode()?;
                match self {
                    Pdu::Invalid{..} => bail!("attempted to measure Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed) =
                                serialize_with_mode(s, compression_mode)?;
                            encoded_frame_len($vers, serial, data.len(), is_compressed)
                        }
                    ,)*
                }
            }

            pub async fn encode_async<W: Unpin + AsyncWriteExt>(&self, w: &mut W, serial: u64) -> Result<(), Error> {
                self.encode_async_with_mode(w, serial, CompressionMode::Auto).await
            }

            pub async fn encode_async_with_mode<W: Unpin + AsyncWriteExt>(
                &self,
                w: &mut W,
                serial: u64,
                compression_mode: CompressionMode,
            ) -> Result<(), Error> {
                self.validate_before_encode()?;
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let (data, is_compressed) =
                                serialize_with_mode(s, compression_mode)?;
                            let encoded_size = encode_raw_async($vers, serial, &data, is_compressed, w).await?;
                            log::debug!("encode_async {} size={encoded_size}", stringify!($name));
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(encoded_size as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(encoded_size as f64);
                            Ok(())
                        }
                    ,)*
                }
            }

            pub fn pdu_name(&self) -> &'static str {
                match self {
                    Pdu::Invalid{..} => "Invalid",
                    $(
                        Pdu::$name(_) => {
                            stringify!($name)
                        }
                    ,)*
                }
            }

            /// Complete wire admission policy for this decoded PDU.
            #[must_use]
            pub fn wire_spec(&self) -> Option<&'static PduWireSpec> {
                match self {
                    Pdu::Invalid { .. } => None,
                    $(
                        Pdu::$name(_) => Some(&<$name as PduWireIdent>::WIRE_SPEC),
                    )*
                }
            }

            /// Lowest negotiated codec dialect that may carry this PDU.
            #[must_use]
            pub fn minimum_codec_version(&self) -> Option<usize> {
                self.wire_spec().map(|spec| spec.min_codec_version)
            }

            /// Endpoint summary for this PDU.
            #[must_use]
            pub fn producer(&self) -> Option<PduProducer> {
                self.wire_spec().map(|spec| spec.producer)
            }

            /// Capabilities that must already be established for this PDU.
            ///
            /// Negotiation PDUs return [`TopologyCapabilities::NONE`]; their
            /// proposed/accepted capability mask remains available through
            /// [`PduWireSpec::capability`].
            #[must_use]
            pub fn required_topology_capabilities(&self) -> Option<TopologyCapabilities> {
                self.wire_spec()
                    .map(|spec| spec.capability.required())
            }

            /// Resolve complete admission metadata from a validated header ID.
            #[must_use]
            pub fn wire_spec_for_ident(ident: u64) -> Option<&'static PduWireSpec> {
                match ident {
                    $(
                        $vers => Some(&<$name as PduWireIdent>::WIRE_SPEC),
                    )*
                    _ => None,
                }
            }

            /// Iterate every assigned PDU specification in identifier order.
            #[must_use]
            pub const fn all_wire_specs() -> &'static [PduWireSpec] {
                PDU_WIRE_SPECS
            }

            /// Resolve a validated wire identifier without reading or
            /// deserializing its payload.
            #[must_use]
            pub fn pdu_name_for_ident(ident: u64) -> Option<&'static str> {
                match ident {
                    $(
                        $vers => Some(stringify!($name)),
                    )*
                    _ => None,
                }
            }

            pub fn decode<R: std::io::Read>(r: R) -> Result<DecodedPdu, Error> {
                Self::decode_impl(r, true)
            }

            /// Decode one frame and bind any available logical-retention
            /// admission metadata to the decoded value.
            pub fn decode_with_retention_metadata<R: std::io::Read>(
                r: R,
            ) -> Result<DecodedPduWithRetentionMetadata, Error> {
                Self::decode_impl_with_retention_metadata(r, true, true)
            }

            /// Decode a frame previously produced by
            /// [`Self::encode_retained_frame`] without recording a duplicate
            /// inbound wire-size sample.
            pub fn decode_retained_frame<R: std::io::Read>(r: R) -> Result<DecodedPdu, Error> {
                Self::decode_impl(r, false)
            }

            fn decode_impl<R: std::io::Read>(
                r: R,
                record_metrics: bool,
            ) -> Result<DecodedPdu, Error> {
                Ok(Self::decode_impl_with_retention_metadata(r, record_metrics, false)?.decoded)
            }

            fn decode_impl_with_retention_metadata<R: std::io::Read>(
                r: R,
                record_metrics: bool,
                collect_metadata: bool,
            ) -> Result<DecodedPduWithRetentionMetadata, Error> {
                let decoded =
                    decode_raw_impl(r, record_metrics).context("decoding a PDU")?;
                match decoded.ident {
                    $(
                        $vers => {
                            if record_metrics {
                                metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                                metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            }
                            let (payload, retained_frame_bytes) =
                                deserialize_pdu_payload_with_retention_metadata!(
                                    $name,
                                    $vers,
                                    decoded.data.as_slice(),
                                    decoded.is_compressed,
                                    collect_metadata
                                    $(, $decoder)?
                                );
                            Ok(DecodedPduWithRetentionMetadata {
                                decoded: DecodedPdu {
                                    serial: decoded.serial,
                                    pdu: Pdu::$name(payload),
                                },
                                retained_frame_bytes,
                            })
                        }
                    ,)*
                    _ => {
                        if record_metrics {
                            metrics::histogram!("pdu.size", "pdu" => "??").record(decoded.data.len() as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => "??").record(decoded.data.len() as f64);
                        }
                        Ok(DecodedPduWithRetentionMetadata {
                            decoded: DecodedPdu {
                                serial: decoded.serial,
                                pdu: Pdu::Invalid{ident:decoded.ident},
                            },
                            retained_frame_bytes: None,
                        })
                    }
                }
            }

            fn decode_materialized_async(decoded: Decoded) -> Result<DecodedPdu, Error> {
                match decoded.ident {
                    $(
                        $vers => {
                            metrics::histogram!("pdu.size", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            metrics::histogram!("pdu.size.rate", "pdu" => stringify!($name)).record(decoded.data.len() as f64);
                            Ok(DecodedPdu {
                                serial: decoded.serial,
                                pdu: Pdu::$name(deserialize_pdu_payload!(
                                    decoded.data.as_slice(),
                                    decoded.is_compressed
                                    $(, $decoder)?
                                )?)
                            })
                        }
                    ,)*
                    _ => {
                        metrics::histogram!("pdu.size", "pdu" => "??").record(decoded.data.len() as f64);
                        metrics::histogram!("pdu.size.rate", "pdu" => "??").record(decoded.data.len() as f64);
                        Ok(DecodedPdu {
                            serial: decoded.serial,
                            pdu: Pdu::Invalid{ident:decoded.ident}
                        })
                    }
                }
            }

            /// Decode one complete frame while allowing a synchronous policy
            /// decision after its header is validated and before its body is
            /// allocated. The codec retains ownership of the header and always
            /// consumes the selected body action before returning success.
            ///
            /// # Stream state after an error
            ///
            /// A partial-header failure, selector error, rejected compressed
            /// discard, or body-read error may leave the reader away from a
            /// frame boundary. The caller must retire that ordered stream; it
            /// must not infer recoverability from the observed cursor position
            /// or attempt to decode another frame from it.
            pub async fn decode_async_with_selector<R, F>(
                r: &mut R,
                max_serial: Option<u64>,
                select_body: F,
            ) -> Result<AsyncPduDecode, Error>
                where R: std::marker::Unpin,
                      R: AsyncRead,
                      R: std::fmt::Debug,
                      F: FnOnce(&PduFrameHeader) -> Result<PduBodyDisposition, Error>
            {
                let header = decode_raw_header_async(r, max_serial)
                    .await
                    .context("decoding a PDU")?;
                match select_body(&header)? {
                    PduBodyDisposition::Materialize => {
                        let decoded = decode_raw_body_async(r, header)
                            .await
                            .context("decoding a PDU")?;
                        Ok(AsyncPduDecode::Decoded(Self::decode_materialized_async(decoded)?))
                    }
                    PduBodyDisposition::Discard => {
                        // A raw compressed-byte drain would bypass the existing
                        // materializing decompression and typed-schema path.
                        // Keep that case there until a bounded streaming policy
                        // defines complete-frame and size-limit semantics.
                        if header.is_compressed() {
                            bail!(
                                "refusing to discard compressed PDU body without bounded zstd validation"
                            );
                        }
                        let serial = header.serial();
                        let ident = header.ident();
                        let discarded = discard_raw_body_async(r, header)
                            .await
                            .context("discarding an abandoned PDU body")?;
                        metrics::counter!("pdu.decode.discarded.frames").increment(1);
                        metrics::counter!("pdu.decode.discarded.encoded_bytes").increment(
                            u64::try_from(discarded.encoded_bytes()).unwrap_or(u64::MAX),
                        );
                        metrics::counter!("pdu.decode.discarded.chunk_reads").increment(
                            u64::try_from(discarded.chunk_reads()).unwrap_or(u64::MAX),
                        );
                        metrics::histogram!("pdu.decode.discarded.max_chunk.size")
                            .record(discarded.max_chunk_bytes() as f64);
                        let pdu_name = Self::pdu_name_for_ident(ident).unwrap_or("??");
                        metrics::histogram!("pdu.size", "pdu" => pdu_name)
                            .record(discarded.encoded_bytes() as f64);
                        metrics::histogram!("pdu.size.rate", "pdu" => pdu_name)
                            .record(discarded.encoded_bytes() as f64);
                        Ok(AsyncPduDecode::Discarded {
                            serial,
                            ident,
                            body: discarded,
                        })
                    }
                }
            }

            pub async fn decode_async<R>(r: &mut R, max_serial: Option<u64>) -> Result<DecodedPdu, Error>
                where R: std::marker::Unpin,
                      R: AsyncRead,
                      R: std::fmt::Debug
            {
                match Self::decode_async_with_selector(r, max_serial, |_| {
                    Ok(PduBodyDisposition::Materialize)
                }).await? {
                    AsyncPduDecode::Decoded(decoded) => Ok(decoded),
                    AsyncPduDecode::Discarded { .. } => {
                        bail!("materializing PDU decode unexpectedly discarded its body")
                    }
                }
            }
        }
    }
}

/// The overall version of the codec.
/// This must be bumped when backwards incompatible changes
/// are made to the types and protocol.
pub const CODEC_VERSION: usize = 51;

/// Lowest codec version this build can decode wire frames from.
///
/// Together with [`CODEC_VERSION`] this defines the rolling-upgrade
/// compatibility window per ft-kuxho/B
/// (`docs/proposals/ft-kuxho-B-codec-version-min-supported-window.md`):
/// a peer announcing version `v` such that
/// `CODEC_VERSION_MIN_SUPPORTED <= v <= CODEC_VERSION` is interop-safe.
///
/// Strictly additive PDU changes use new identifiers and bump
/// `CODEC_VERSION` without bumping this constant, opening a
/// backward-compatibility window. A positional varbincode struct-field
/// addition is not bidirectionally additive merely because it is final or
/// carries `serde(default)`: the newer decoder still requests that field from
/// an older payload and reaches EOF. Such a change needs either a distinct PDU
/// identifier or an explicit dual-schema decoder proven with real old and new
/// frames. Removals, reorders, and type changes bump both constants.
///
/// Advancing this constant is a breaking change. Per `docs/codec-versions.md`
/// every advance requires a release-note row paired with a `tracing::warn!`
/// at handshake time for the full release cycle before the bump. The CI
/// guard `scripts/check_codec_version_release_notes.sh` (ft-8smkj) blocks
/// silent advances.
pub const CODEC_VERSION_MIN_SUPPORTED: usize = 46;

/// Outcome of [`check_compat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompatDecision {
    /// The peers can interop. `agreed` is the codec version both sides
    /// will speak for the lifetime of the session — the lower of the two
    /// peers' canonical versions, clamped to the joint compat window.
    Compatible { agreed: usize },
}

/// Codec-level handshake compatibility error returned by [`check_compat`]
/// when either peer advertises an impossible window or the two valid windows
/// do not overlap.
///
/// Stays inside the `codec` crate so the handshake helper has no upstream
/// dependency on the client crate's `IncompatibleVersionError` (which
/// wraps this with frankenterm-version context for end-user display).
#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[error(
    "Codec version mismatch: local={local} (min {local_min}), remote={remote} (min {remote_min}). \
     A compat window is invalid or the windows do not overlap. See docs/codec-atomic-redeploy.md \
     for the operator runbook."
)]
pub struct CompatError {
    pub local: usize,
    pub local_min: usize,
    pub remote: usize,
    pub remote_min: usize,
}

/// Decide whether two peers' codec versions are compatible per the
/// rolling-upgrade window contract (ft-kuxho.B.1).
///
/// Both peers contribute a `(version, min_supported)` pair. The peers can
/// interop iff their windows overlap — i.e. the overlap range
/// `[max(local_min, remote_min), min(local, remote)]` is non-empty. When
/// it is, both sides agree on `min(local, remote)` so the lower-versioned
/// peer can speak its native dialect. The higher peer must then gate every
/// PDU to that agreed dialect; overlap never authorizes a newer family merely
/// because its decoder can represent the identifier.
///
/// Bootstrap note: current `GetCodecVersionResponse` frames carry
/// `min_supported`. Its explicit dual-schema decoder maps a canonical legacy
/// response to the sentinel `min_supported = 0`; handshake callers must clamp
/// that sentinel to the peer's `codec_vers` before calling this function.
/// This function deliberately rejects rather than repairs impossible windows.
pub fn check_compat(
    local: usize,
    local_min: usize,
    remote: usize,
    remote_min: usize,
) -> Result<CompatDecision, CompatError> {
    if local_min > local || remote_min > remote {
        return Err(CompatError {
            local,
            local_min,
            remote,
            remote_min,
        });
    }

    let overlap_low = local_min.max(remote_min);
    let overlap_high = local.min(remote);

    if overlap_low <= overlap_high {
        Ok(CompatDecision::Compatible {
            agreed: overlap_high,
        })
    } else {
        Err(CompatError {
            local,
            local_min,
            remote,
            remote_min,
        })
    }
}

// Defines the Pdu enum.
// Each struct has an explicit identifying number.
// This allows removal of obsolete structs,
// and defining newer structs as the protocol evolves.
pdu! {
    ErrorResponse: 0, 46, server_reply, none;
    Ping: 1, 46, client_request, none;
    Pong: 2, 46, server_reply, none;
    ListPanes: 3, 46, client_request, none;
    ListPanesResponse: 4, 46, server_reply, none;
    SpawnResponse: 8, 46, server_reply, none;
    WriteToPane: 9, 46, client_request, none;
    UnitResponse: 10, 46, server_reply, none;
    SendKeyDown: 11, 46, client_request, none;
    SendMouseEvent: 12, 46, client_request, none;
    SendPaste: 13, 46, client_request, none;
    Resize: 14, 46, client_request, none;
    SetClipboard: 20, 46, server_unilateral, none;
    GetLines: 22, 46, client_request, none;
    GetLinesResponse: 23, 46, server_reply, none;
    GetPaneRenderChanges: 24, 46, client_request, none;
    GetPaneRenderChangesResponse: 25, 46, server_reply_or_unilateral, none;
    GetCodecVersion: 26, 46, client_request, none;
    GetCodecVersionResponse: 27, 46, server_reply, none
        => deserialize_get_codec_version_response;
    GetTlsCreds: 28, 46, client_request, none;
    GetTlsCredsResponse: 29, 46, server_reply, none;
    LivenessResponse: 30, 46, server_reply, none;
    SearchScrollbackRequest: 31, 46, client_request, none;
    SearchScrollbackResponse: 32, 46, server_reply, none;
    SetPaneZoomed: 33, 46, client_request, none;
    SplitPane: 34, 46, client_request, none;
    KillPane: 35, 46, client_request, none;
    SpawnV2: 36, 46, client_request, none;
    PaneRemoved: 37, 46, server_unilateral, none;
    SetPalette: 38, 46, client_request_or_server_unilateral, none;
    NotifyAlert: 39, 46, server_unilateral, none;
    SetClientId: 40, 46, client_request, none;
    GetClientList: 41, 46, client_request, none;
    GetClientListResponse: 42, 46, server_reply, none;
    SetWindowWorkspace: 43, 46, client_request, none;
    WindowWorkspaceChanged: 44, 46, server_unilateral, none;
    SetFocusedPane: 45, 46, client_request, none;
    GetImageCell: 46, 46, client_request, none;
    GetImageCellResponse: 47, 46, server_reply, none;
    MovePaneToNewTab: 48, 46, client_request, none;
    MovePaneToNewTabResponse: 49, 46, server_reply, none;
    ActivatePaneDirection: 50, 46, client_request, none;
    GetPaneRenderableDimensions: 51, 46, client_request, none;
    GetPaneRenderableDimensionsResponse: 52, 46, server_reply, none;
    PaneFocused: 53, 46, server_unilateral, none;
    TabResized: 54, 46, server_unilateral, none;
    TabAddedToWindow: 55, 46, server_unilateral, none;
    TabTitleChanged: 56, 46, client_request_or_server_unilateral, none;
    WindowTitleChanged: 57, 46, client_request_or_server_unilateral, none;
    RenameWorkspace: 58, 46, client_request_or_server_unilateral, none;
    EraseScrollbackRequest: 59, 46, client_request, none;
    GetPaneDirection: 60, 46, client_request, none;
    GetPaneDirectionResponse: 61, 46, server_reply, none;
    AdjustPaneSize: 62, 46, client_request, none;
    CreateFloatingPane: 63, 46, client_request, none;
    MoveFloatingPane: 64, 46, client_request, none;
    SetFloatingPaneZ: 65, 46, client_request, none;
    ToggleFloatingPane: 66, 46, client_request, none;
    RemoveFloatingPane: 67, 46, client_request, none;
    SwapToLayout: 68, 46, client_request, none;
    SetLayoutCycle: 69, 46, client_request, none;
    CycleStack: 70, 46, client_request, none;
    SelectStackPane: 71, 46, client_request, none;
    UpdatePaneConstraints: 72, 46, client_request, none;
    SendKeyUp: 73, 46, client_request, none;
    SetActiveWorkspace: 74, 46, client_request, none;
    ListPanesTabStacks: 75, 47, client_request, none;
    ListPanesTabStacksResponse: 76, 47, server_reply, none;
    GetSemanticZones: 77, 47, client_request, none;
    GetSemanticZonesResponse: 78, 47, server_reply, none;
    RenderApplicationUpdateV1: 79, 48, server_unilateral, none;
    RenderApplicationResultV1: 80, 48, client_request, none;
    ListPanesCoherent: 81, 49, client_request, negotiates_fenced
        => deserialize_list_panes_coherent;
    ListPanesCoherentResponse: 82, 49, server_reply, negotiates_fenced
        => deserialize_list_panes_coherent_response;
    TopologyEvent: 83, 49, server_unilateral, requires_fenced
        => deserialize_topology_event;
    RenderApplicationUpdate: 84, 50, server_unilateral, requires_fenced;
    RenderApplicationResult: 85, 50, client_request, requires_fenced;
    ListPanesOrderedV1: 86, 51, client_request, negotiates_ordered
        => deserialize_list_panes_ordered_v1;
    ListPanesOrderedV1Response: 87, 51, server_reply, negotiates_ordered
        => deserialize_list_panes_ordered_v1_response;
    ReorderWindowTabsV1: 88, 51, client_request, requires_reorder
        => deserialize_reorder_window_tabs_v1;
    ReorderWindowTabsV1Response: 89, 51, server_reply, requires_reorder
        => deserialize_reorder_window_tabs_v1_response;
    WindowOrderEventV1: 90, 51, server_unilateral, requires_ordered
        => deserialize_window_order_event_v1;
}

impl Pdu {
    #[inline]
    fn validate_before_encode(&self) -> Result<(), Error> {
        match self {
            Self::ListPanesOrderedV1(value) => value.validate()?,
            Self::ListPanesOrderedV1Response(value) => value.validate()?,
            Self::ReorderWindowTabsV1(value) => value.validate()?,
            Self::ReorderWindowTabsV1Response(value) => value.validate()?,
            Self::WindowOrderEventV1(value) => value.validate()?,
            _ => {}
        }
        Ok(())
    }
}

/// Minimum consumed prefix reclaimed by a streaming PDU buffer compaction.
///
/// Compaction also requires the consumed prefix to be at least as large as the
/// unread suffix. Therefore every moved byte can be charged to at least one
/// byte consumed since the preceding compaction, giving amortized linear byte
/// movement while avoiding tiny-prefix memmoves.
const STREAM_BUFFER_MIN_COMPACTION_PREFIX: usize = 64 * 1024;

/// Cumulative work counters for one [`StreamingPduBuffer`].
///
/// These counters describe buffer mechanics only. They are useful for
/// deterministic complexity tests and do not constitute target-host latency or
/// allocator evidence.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StreamingPduBufferStats {
    /// Number of suffix-preserving prefix compactions.
    pub compactions: u64,
    /// Total unread bytes moved by those compactions.
    pub compacted_bytes: u64,
    /// Number of append operations that grew the backing allocation.
    pub growth_events: u64,
    /// Largest backing allocation capacity observed by this buffer.
    pub peak_capacity: usize,
}

/// Owned accumulation buffer for incrementally decoded mux PDUs.
///
/// Successful frame consumption advances a checked head in O(1); it does not
/// memmove the unread suffix. The consumed prefix is reclaimed only when an
/// append needs tail capacity, at least 64 KiB has been consumed, and the
/// consumed prefix is no smaller than the unread suffix. That policy bounds
/// compaction to amortized linear byte movement. Complete malformed frames and
/// incomplete frames leave the logical unread bytes unchanged.
///
/// Only the initialized unread suffix is exposed. There is deliberately no
/// mutable slice or `DerefMut` escape hatch that could mutate already-consumed
/// storage or invalidate the checked head.
#[derive(Default)]
pub struct StreamingPduBuffer {
    storage: Vec<u8>,
    head: usize,
    stats: StreamingPduBufferStats,
}

impl Clone for StreamingPduBuffer {
    fn clone(&self) -> Self {
        let storage = self.as_slice().to_vec();
        let mut stats = self.stats;
        stats.peak_capacity = stats.peak_capacity.max(storage.capacity());
        Self {
            storage,
            head: 0,
            stats,
        }
    }
}

impl std::fmt::Debug for StreamingPduBuffer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StreamingPduBuffer")
            .field("unread_len", &self.len())
            .field("consumed_prefix_len", &self.consumed_prefix_len())
            .field("retained_len", &self.retained_len())
            .field("capacity", &self.capacity())
            .field("stats", &self.stats)
            .finish()
    }
}

impl StreamingPduBuffer {
    /// Construct an empty streaming buffer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            storage: Vec::new(),
            head: 0,
            stats: StreamingPduBufferStats {
                compactions: 0,
                compacted_bytes: 0,
                growth_events: 0,
                peak_capacity: 0,
            },
        }
    }

    /// Construct an empty streaming buffer with retained allocation capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let storage = Vec::with_capacity(capacity);
        Self {
            stats: StreamingPduBufferStats {
                peak_capacity: storage.capacity(),
                ..StreamingPduBufferStats::default()
            },
            storage,
            head: 0,
        }
    }

    /// Number of initialized unread bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.storage.len() - self.head
    }

    /// Whether no unread bytes remain.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.head == self.storage.len()
    }

    /// Current backing allocation capacity, including reusable consumed space.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.storage.capacity()
    }

    /// Initialized bytes retained in the backing vector, including the
    /// consumed prefix that may be reclaimed by a later append.
    #[must_use]
    pub fn retained_len(&self) -> usize {
        self.storage.len()
    }

    /// Length of the initialized, already-consumed prefix.
    #[must_use]
    pub fn consumed_prefix_len(&self) -> usize {
        self.head
    }

    /// Exact initialized unread bytes.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        &self.storage[self.head..]
    }

    /// Cumulative buffer-mechanics counters.
    #[must_use]
    pub const fn stats(&self) -> StreamingPduBufferStats {
        self.stats
    }

    /// Append initialized bytes to the unread suffix.
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.prepare_for_append(bytes.len());
        let capacity_before = self.storage.capacity();
        self.storage.extend_from_slice(bytes);
        let capacity_after = self.storage.capacity();
        if capacity_after > capacity_before {
            self.stats.growth_events = self.stats.growth_events.saturating_add(1);
        }
        self.stats.peak_capacity = self.stats.peak_capacity.max(capacity_after);
    }

    /// Discard every unread and consumed byte while retaining allocation.
    pub fn clear(&mut self) {
        self.storage.clear();
        self.head = 0;
    }

    /// Return the logical unread bytes in one compact vector.
    #[must_use]
    pub fn into_unread_bytes(mut self) -> Vec<u8> {
        if self.head == 0 {
            return self.storage;
        }
        let unread = self.len();
        self.storage.copy_within(self.head.., 0);
        self.storage.truncate(unread);
        self.storage
    }

    fn consume_prefix(&mut self, consumed: usize) -> anyhow::Result<()> {
        if consumed > self.len() {
            anyhow::bail!(
                "streaming PDU buffer cannot consume {consumed} bytes from {} unread bytes",
                self.len()
            );
        }
        self.head = self
            .head
            .checked_add(consumed)
            .context("streaming PDU buffer head overflow")?;
        if self.head == self.storage.len() {
            self.storage.clear();
            self.head = 0;
        }
        Ok(())
    }

    fn prepare_for_append(&mut self, additional: usize) {
        if self.is_empty() {
            self.storage.clear();
            self.head = 0;
            return;
        }

        let tail_capacity = self.storage.capacity() - self.storage.len();
        if tail_capacity >= additional {
            return;
        }

        let unread = self.len();
        if self.head < STREAM_BUFFER_MIN_COMPACTION_PREFIX || self.head < unread {
            return;
        }

        self.storage.copy_within(self.head.., 0);
        self.storage.truncate(unread);
        self.head = 0;
        self.stats.compactions = self.stats.compactions.saturating_add(1);
        self.stats.compacted_bytes = self
            .stats
            .compacted_bytes
            .saturating_add(u64::try_from(unread).unwrap_or(u64::MAX));
    }
}

impl From<Vec<u8>> for StreamingPduBuffer {
    fn from(storage: Vec<u8>) -> Self {
        let peak_capacity = storage.capacity();
        Self {
            storage,
            head: 0,
            stats: StreamingPduBufferStats {
                peak_capacity,
                ..StreamingPduBufferStats::default()
            },
        }
    }
}

impl Deref for StreamingPduBuffer {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl Pdu {
    /// Returns true if this type of Pdu represents action taken
    /// directly by a user, rather than background traffic on
    /// a live connection
    pub fn is_user_input(&self) -> bool {
        matches!(
            self,
            Self::WriteToPane(_)
                | Self::SendKeyDown(_)
                | Self::SendKeyUp(_)
                | Self::SendMouseEvent(_)
                | Self::SendPaste(_)
                | Self::Resize(_)
                | Self::SetPaneZoomed(_)
                | Self::SpawnV2(_)
                | Self::ReorderWindowTabsV1(_)
        )
    }

    pub fn stream_decode(
        buffer: &mut StreamingPduBuffer,
    ) -> anyhow::Result<Option<DecodedPdu>> {
        Ok(
            Self::stream_decode_with_options(buffer, usize::MAX, false)?
                .map(|decoded| decoded.into_parts().0),
        )
    }

    /// Decode one streaming frame while enforcing a cap on that frame's
    /// declared size. Coalesced bytes belonging to later frames do not count
    /// against the first frame.
    pub fn stream_decode_with_frame_limit(
        buffer: &mut StreamingPduBuffer,
        max_frame_bytes: usize,
    ) -> anyhow::Result<Option<DecodedPdu>> {
        Ok(
            Self::stream_decode_with_options(buffer, max_frame_bytes, false)?
                .map(|decoded| decoded.into_parts().0),
        )
    }

    /// Decode one streaming frame and bind logical-retention metadata to it.
    pub fn stream_decode_with_retention_metadata(
        buffer: &mut StreamingPduBuffer,
    ) -> anyhow::Result<Option<DecodedPduWithRetentionMetadata>> {
        Self::stream_decode_with_options(buffer, usize::MAX, true)
    }

    /// Decode one streaming frame with both identity-bound retention metadata
    /// and a cap on that frame's declared size.
    pub fn stream_decode_with_retention_metadata_and_frame_limit(
        buffer: &mut StreamingPduBuffer,
        max_frame_bytes: usize,
    ) -> anyhow::Result<Option<DecodedPduWithRetentionMetadata>> {
        Self::stream_decode_with_options(buffer, max_frame_bytes, true)
    }

    fn stream_decode_with_options(
        buffer: &mut StreamingPduBuffer,
        max_frame_bytes: usize,
        collect_retention_metadata: bool,
    ) -> anyhow::Result<Option<DecodedPduWithRetentionMetadata>> {
        let Some(frame_len) =
            buffered_frame_len_with_limit(buffer.as_slice(), max_frame_bytes)?
        else {
            return Ok(None);
        };

        let frame = buffer
            .as_slice()
            .get(..frame_len)
            .context("stream_decode frame length beyond buffer")?;
        let mut cursor = Cursor::new(frame);
        let decoded = match Self::decode_impl_with_retention_metadata(
            &mut cursor,
            true,
            collect_retention_metadata,
        ) {
            Ok(decoded) => {
                let consumed = cursor.position() as usize;
                if consumed != frame_len {
                    bail!(
                        "stream_decode consumed {} bytes from a {} byte PDU frame",
                        consumed,
                        frame_len
                    );
                }
                decoded
            }
            Err(err) => {
                log::error!("not an ioerror in stream_decode: {:?}", err);
                return Err(err);
            }
        };

        buffer.consume_prefix(frame_len)?;
        Ok(Some(decoded))
    }

    pub fn try_read_and_decode<R: std::io::Read>(
        r: &mut R,
        buffer: &mut StreamingPduBuffer,
    ) -> anyhow::Result<Option<DecodedPdu>> {
        loop {
            if let Some(decoded) =
                Self::stream_decode(buffer).context("stream_decode of buffer for PDU")?
            {
                return Ok(Some(decoded));
            }

            let mut buf = [0u8; 4096];
            let size = match r.read(&mut buf) {
                Ok(size) => size,
                Err(err) => {
                    if err.kind() == std::io::ErrorKind::WouldBlock {
                        return Ok(None);
                    }
                    return Err(err.into());
                }
            };
            if size == 0 {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "End Of File").into(),
                );
            }

            let read_chunk = buf
                .get(..size)
                .context("read returned more bytes than buffer length")?;
            buffer.extend_from_slice(read_chunk);
        }
    }

    pub fn pane_id(&self) -> Option<PaneId> {
        match self {
            Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse { pane_id, .. })
            | Pdu::GetSemanticZonesResponse(GetSemanticZonesResponse { pane_id, .. })
            | Pdu::RenderApplicationUpdate(RenderApplicationUpdate {
                identity: RenderApplicationIdentity { pane_id, .. },
                ..
            })
            | Pdu::RenderApplicationResult(RenderApplicationResult {
                identity: RenderApplicationIdentity { pane_id, .. },
                ..
            })
            | Pdu::SetPalette(SetPalette { pane_id, .. })
            | Pdu::NotifyAlert(NotifyAlert { pane_id, .. })
            | Pdu::SetClipboard(SetClipboard { pane_id, .. })
            | Pdu::PaneFocused(PaneFocused { pane_id })
            | Pdu::PaneRemoved(PaneRemoved { pane_id }) => Some(*pane_id),
            _ => None,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct UnitResponse {}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ErrorResponse {
    pub reason: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetCodecVersion {}

/// Default for `GetCodecVersionResponse::min_supported` when the field is
/// absent on the wire — i.e. when decoding a payload from a pre-ft-kuxho.B.3
/// peer that did not yet emit the field. The conservative choice is to
/// treat the legacy peer as supporting only its own `codec_vers`, which
/// the deserializer can't know at default-eval time. Returning 0 instead
/// is wrong (would falsely widen the window); returning the local
/// `CODEC_VERSION_MIN_SUPPORTED` is also wrong (would falsely promise
/// remote support of older versions). The right answer — "treat remote_min
/// as remote when no min was advertised" — has to be applied at the
/// handshake call-site after the value is observed, because the default
/// fn here can't see `codec_vers`. We default to 0 as a sentinel; the
/// client handshake checks for it and substitutes `codec_vers` before
/// passing into `check_compat`.
fn default_legacy_min_supported() -> usize {
    0
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetCodecVersionResponse {
    pub codec_vers: usize,
    pub version_string: String,
    pub executable_path: PathBuf,
    pub config_file_path: Option<PathBuf>,
    /// Lowest codec version the responder accepts (ft-kuxho.B.3).
    ///
    /// PDU 27 uses an explicit dual-schema decoder because positional
    /// varbincode does not apply this default after an older payload reaches
    /// EOF. The legacy decoder maps a canonical four-field response to the
    /// sentinel `0`; the handshake call-site then substitutes `codec_vers`
    /// before invoking `check_compat`.
    ///
    /// NOTE: NO `skip_serializing_if` — varbincode is a positional binary
    /// format; eliding the field on the encode side would shift offsets
    /// for any future tail field added after this one. Always serialize.
    /// See lib.rs:1226 / MEMORY varbincode-skip-serializing-if-bug.
    #[serde(default = "default_legacy_min_supported")]
    pub min_supported: usize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
struct LegacyGetCodecVersionResponse {
    codec_vers: usize,
    version_string: String,
    executable_path: PathBuf,
    config_file_path: Option<PathBuf>,
}

fn materialize_uncompressed_payload<'a>(
    data: &'a [u8],
    is_compressed: bool,
) -> Result<std::borrow::Cow<'a, [u8]>, Error> {
    if !is_compressed {
        return Ok(std::borrow::Cow::Borrowed(data));
    }

    let mut decompressed = Vec::new();
    let mut decoder = zstd::Decoder::new(data)?.take(max_pdu_read_limit()?);
    decoder.read_to_end(&mut decompressed)?;
    if decompressed.len() > MAX_PDU_SIZE {
        bail!(
            "decompressed PDU payload size {} exceeds maximum {}",
            decompressed.len(),
            MAX_PDU_SIZE
        );
    }
    Ok(std::borrow::Cow::Owned(decompressed))
}

fn materialize_exact_payload_with_limit<'a>(
    data: &'a [u8],
    is_compressed: bool,
    payload_name: &'static str,
    max_payload_bytes: usize,
) -> Result<std::borrow::Cow<'a, [u8]>, Error> {
    if max_payload_bytes > MAX_PDU_SIZE {
        bail!(
            "{payload_name} decompressed limit {max_payload_bytes} exceeds outer maximum {MAX_PDU_SIZE}"
        );
    }
    if !is_compressed {
        if data.len() > max_payload_bytes {
            bail!(
                "{payload_name} decompressed payload size {} exceeds maximum {}",
                data.len(),
                max_payload_bytes
            );
        }
        return Ok(std::borrow::Cow::Borrowed(data));
    }

    let read_limit = u64::try_from(max_payload_bytes)
        .with_context(|| format!("{payload_name} payload limit does not fit in u64"))?
        .checked_add(1)
        .with_context(|| format!("{payload_name} payload read limit overflow"))?;
    let decoder = zstd::Decoder::with_buffer(data)?.single_frame();
    let mut limited = decoder.take(read_limit);
    let mut decompressed = Vec::with_capacity(data.len().min(max_payload_bytes));
    limited.read_to_end(&mut decompressed)?;
    if decompressed.len() > max_payload_bytes {
        bail!(
            "{payload_name} decompressed payload size exceeds maximum {}",
            max_payload_bytes
        );
    }
    let decoder = limited.into_inner();
    if !decoder.finish().is_empty() {
        bail!("{payload_name} payload has trailing compressed frame bytes");
    }
    Ok(std::borrow::Cow::Owned(decompressed))
}

/// Decode an authority-bearing schema that is closed for its current wire ID.
///
/// Bytes after the outer frame remain valid input for the next PDU, but bytes
/// left inside this payload are neither a legacy extension nor a future field:
/// accepting them would give multiple encodings the same topology authority.
/// Schema growth must therefore use a new PDU identifier.
fn deserialize_exact_payload<T: serde::de::DeserializeOwned>(
    data: &[u8],
    is_compressed: bool,
    payload_name: &'static str,
) -> Result<T, Error> {
    deserialize_exact_payload_with_limit(data, is_compressed, payload_name, MAX_PDU_SIZE)
}

/// Exact-consumption decoder with a schema-specific decompressed byte limit.
///
/// The outer frame cap remains a defense in depth bound for every PDU. Closed
/// authority schemas with a smaller semantic budget use this helper so a
/// compressed body cannot allocate or deserialize past that tighter ceiling.
fn deserialize_exact_payload_with_limit<T: serde::de::DeserializeOwned>(
    data: &[u8],
    is_compressed: bool,
    payload_name: &'static str,
    max_payload_bytes: usize,
) -> Result<T, Error> {
    if max_payload_bytes > MAX_PDU_SIZE {
        bail!(
            "{payload_name} decompressed limit {max_payload_bytes} exceeds outer maximum {MAX_PDU_SIZE}"
        );
    }

    if !is_compressed {
        if data.len() > max_payload_bytes {
            bail!(
                "{payload_name} decompressed payload size {} exceeds maximum {}",
                data.len(),
                max_payload_bytes
            );
        }
        let mut reader = data;
        let decoded = bounded_varbincode::deserialize::<T, _>(&mut reader)?;
        if !reader.is_empty() {
            bail!("{payload_name} payload has trailing schema bytes");
        }
        return Ok(decoded);
    }

    // Decode straight through zstd instead of materializing a second complete
    // copy of a potentially 256 MiB coherent snapshot.  The extra byte in the
    // limit distinguishes a legal boundary-sized value from decompressed data
    // that crossed the cap.
    let read_limit = u64::try_from(max_payload_bytes)
        .with_context(|| format!("{payload_name} payload limit does not fit in u64"))?
        .checked_add(1)
        .with_context(|| format!("{payload_name} payload read limit overflow"))?;
    // `data` is already a `BufRead`; using it directly avoids allocating a
    // fresh zstd-sized `BufReader` for every compressed authority PDU.
    let decoder = zstd::Decoder::with_buffer(data)?.single_frame();
    // The already-materialized, outer-frame-bounded compressed length is only
    // a performance hint. Both ends are clamped, so hostile input cannot pick
    // an unbounded output allocation and small topology events stay small.
    let recommended_output_size = zstd::Decoder::<&[u8]>::recommended_output_size()
        .max(MIN_EXACT_ZSTD_DECODE_BUFFER_SIZE);
    let output_buffer_size = data
        .len()
        .clamp(MIN_EXACT_ZSTD_DECODE_BUFFER_SIZE, recommended_output_size);
    let mut reader = BufReader::with_capacity(output_buffer_size, decoder).take(read_limit);
    let decoded = bounded_varbincode::deserialize::<T, _>(&mut reader)?;
    let decoded_bytes = read_limit
        .checked_sub(reader.limit())
        .context("counting decoded exact PDU payload bytes")?;
    if decoded_bytes
        > u64::try_from(max_payload_bytes)
            .with_context(|| format!("{payload_name} payload limit does not fit in u64"))?
    {
        bail!(
            "{payload_name} decompressed payload size exceeds maximum {}",
            max_payload_bytes
        );
    }

    // One read is sufficient.  A byte proves that the schema was not exact;
    // EOF proves that zstd reached and validated the end of the frame.  Do not
    // drain a hostile compressed tail just to calculate an exact error count.
    let mut trailing = [0_u8; 1];
    if reader
        .read(&mut trailing)
        .with_context(|| format!("validating {payload_name} compressed payload termination"))?
        != 0
    {
        bail!("{payload_name} payload has trailing schema bytes");
    }
    // The successful EOF probe above guarantees that the decompressed-output
    // buffer is empty, so unwrapping it cannot discard unread schema bytes.
    let buffered_decoder = reader.into_inner();
    debug_assert!(buffered_decoder.buffer().is_empty());
    let decoder = buffered_decoder.into_inner();
    if !decoder.finish().is_empty() {
        bail!("{payload_name} payload has trailing compressed frame bytes");
    }
    Ok(decoded)
}

fn deserialize_list_panes_coherent(
    data: &[u8],
    is_compressed: bool,
) -> Result<ListPanesCoherent, Error> {
    deserialize_exact_payload(data, is_compressed, "ListPanesCoherent")
}

fn deserialize_list_panes_coherent_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<ListPanesCoherentResponse, Error> {
    deserialize_exact_payload(data, is_compressed, "ListPanesCoherentResponse")
}

fn deserialize_topology_event(
    data: &[u8],
    is_compressed: bool,
) -> Result<TopologyEvent, Error> {
    deserialize_exact_payload(data, is_compressed, "TopologyEvent")
}

fn deserialize_list_panes_ordered_v1(
    data: &[u8],
    is_compressed: bool,
) -> Result<ListPanesOrderedV1, Error> {
    let request: ListPanesOrderedV1 =
        deserialize_exact_payload(data, is_compressed, "ListPanesOrderedV1")?;
    request.validate()?;
    Ok(request)
}

fn deserialize_list_panes_ordered_v1_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<ListPanesOrderedV1Response, Error> {
    let response: ListPanesOrderedV1Response =
        deserialize_exact_payload(data, is_compressed, "ListPanesOrderedV1Response")?;
    response.validate()?;
    Ok(response)
}

fn deserialize_reorder_window_tabs_v1(
    data: &[u8],
    is_compressed: bool,
) -> Result<ReorderWindowTabsV1, Error> {
    let payload = materialize_exact_payload_with_limit(
        data,
        is_compressed,
        "ReorderWindowTabsV1",
        MAX_REORDER_WINDOW_TABS_DECOMPRESSED_BYTES,
    )?;
    let mut reader = payload.as_ref();
    let request = bounded_varbincode::deserialize::<ReorderWindowTabsV1, _>(&mut reader)?;
    if !reader.is_empty() {
        bail!("ReorderWindowTabsV1 payload has trailing schema bytes");
    }
    request.validate()?;
    Ok(request)
}

fn deserialize_reorder_window_tabs_v1_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<ReorderWindowTabsV1Response, Error> {
    let response: ReorderWindowTabsV1Response = deserialize_exact_payload(
        data,
        is_compressed,
        "ReorderWindowTabsV1Response",
    )?;
    response.validate()?;
    Ok(response)
}

fn deserialize_window_order_event_v1(
    data: &[u8],
    is_compressed: bool,
) -> Result<WindowOrderEventV1, Error> {
    let event: WindowOrderEventV1 =
        deserialize_exact_payload(data, is_compressed, "WindowOrderEventV1")?;
    event.validate()?;
    Ok(event)
}

fn deserialize_get_codec_version_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<GetCodecVersionResponse, Error> {
    let payload = materialize_uncompressed_payload(data, is_compressed)?;

    let mut current_reader = payload.as_ref();
    match bounded_varbincode::deserialize::<GetCodecVersionResponse, _>(&mut current_reader) {
        Ok(current) if current_reader.is_empty() => Ok(current),
        Ok(_) => {
            bail!("current GetCodecVersionResponse payload has trailing schema bytes");
        }
        Err(current_error) => {
            let mut legacy_reader = payload.as_ref();
            let legacy = bounded_varbincode::deserialize::<LegacyGetCodecVersionResponse, _>(
                &mut legacy_reader,
            )
            .map_err(|legacy_error| {
                anyhow::anyhow!(
                    "GetCodecVersionResponse matched neither current nor legacy schema: \
                     current={current_error}; legacy={legacy_error}"
                )
            })?;
            if !legacy_reader.is_empty() {
                bail!(
                    "legacy GetCodecVersionResponse payload has {} trailing schema bytes",
                    legacy_reader.len()
                );
            }
            Ok(GetCodecVersionResponse {
                codec_vers: legacy.codec_vers,
                version_string: legacy.version_string,
                executable_path: legacy.executable_path,
                config_file_path: legacy.config_file_path,
                min_supported: default_legacy_min_supported(),
            })
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct Ping {}
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct Pong {}

/// Requests a client certificate to authenticate against
/// the TLS based server
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetTlsCreds {}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetTlsCredsResponse {
    /// The signing certificate
    pub ca_cert_pem: String,
    /// A client authentication certificate and private
    /// key, PEM encoded
    pub client_cert_pem: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanes {}

/// Negotiated topology-fence features.
///
/// Unknown bits are preserved on decode but never implicitly accepted. A
/// server computes the intersection with [`Self::SERVER_SUPPORTED`] and must
/// return [`ListPanesCoherentOutcome::Unsupported`] when the request's
/// `required` set is not a subset of that intersection.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct TopologyCapabilities(u64);

impl TopologyCapabilities {
    pub const NONE: Self = Self(0);
    pub const FENCED_SNAPSHOT_V1: Self = Self(1 << 0);
    /// The peer understands explicit ordered-window snapshots and events.
    pub const ORDERED_WINDOW_STREAM_V1: Self = Self(1 << 1);
    /// The peer understands idempotent per-window reorder compare-and-set.
    ///
    /// This capability is meaningful only together with
    /// [`Self::ORDERED_WINDOW_STREAM_V1`].
    pub const WINDOW_REORDER_CAS_V1: Self = Self(1 << 2);

    /// Runtime-advertised capabilities.
    ///
    /// The v51 codec knows the two ordered-window bits, but neither may be
    /// advertised until the mux authority, server dispatch, and client
    /// reconciliation beads complete. Keep this mask intentionally unchanged.
    pub const SERVER_SUPPORTED: Self = Self::FENCED_SNAPSHOT_V1;

    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    pub fn validate(self) -> Result<(), TopologyCapabilitiesError> {
        if self.contains(Self::WINDOW_REORDER_CAS_V1)
            && !self.contains(Self::ORDERED_WINDOW_STREAM_V1)
        {
            return Err(TopologyCapabilitiesError::ReorderCasWithoutOrderedStream {
                bits: self.bits(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TopologyCapabilitiesError {
    #[error(
        "topology capability bits {bits:#x} request WINDOW_REORDER_CAS_V1 without ORDERED_WINDOW_STREAM_V1"
    )]
    ReorderCasWithoutOrderedStream { bits: u64 },
}

/// Unpredictable identity of one connection-generation topology stream.
///
/// It is server-owned, rotates on reconnect or any loss-terminal transition,
/// and is bound to a mux-session incarnation by a coherent snapshot.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct TopologyStreamId([u8; 16]);

impl TopologyStreamId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Exact, restart-safe identity of one render connection.
///
/// The connection-local numeric generation remains useful for ordering within
/// one client incarnation, but it can repeat after a client restart. Likewise,
/// process-local scheduler and ledger counters can repeat after a server
/// restart. Binding both the unpredictable topology-stream identity and the
/// unpredictable mux-session incarnation makes a stale render unit
/// unambiguously foreign across reconnect, client restart, server restart, and
/// route failover.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct RenderConnectionIdentity {
    pub stream_id: TopologyStreamId,
    pub session_incarnation: MuxSessionIncarnation,
}

impl RenderConnectionIdentity {
    #[must_use]
    pub const fn new(
        stream_id: TopologyStreamId,
        session_incarnation: MuxSessionIncarnation,
    ) -> Self {
        Self {
            stream_id,
            session_incarnation,
        }
    }

    pub fn validate(self) -> Result<(), RenderApplicationContractError> {
        if self.stream_id.as_bytes() == [0; 16]
            || self.session_incarnation.as_bytes() == [0; 16]
        {
            return Err(RenderApplicationContractError::ReservedConnectionIdentity);
        }
        Ok(())
    }
}

/// Request a mux-wide pane snapshot that is validated at one topology
/// revision and establishes a connection-generation topology stream.
///
/// This is deliberately a separate PDU from legacy [`ListPanes`]. Receiving
/// or decoding `ListPanesResponse` never grants fence authority.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesCoherent {
    pub supported: TopologyCapabilities,
    pub required: TopologyCapabilities,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct CoherentPaneSnapshot {
    pub session_incarnation: MuxSessionIncarnation,
    pub snapshot_revision: TopologyRevision,
    pub panes: ListPanesResponse,
}

/// Typed result of bounded coherent-snapshot construction.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum ListPanesCoherentOutcome {
    Snapshot(CoherentPaneSnapshot),
    /// Every bounded attempt observed topology movement. No snapshot in this
    /// response is authoritative.
    Contended {
        attempts: u8,
        first_revision: TopologyRevision,
        last_revision: TopologyRevision,
    },
    /// The mux revision authority exhausted its nonwrapping namespace.
    RevisionExhausted,
    /// The server cannot satisfy every capability the requester marked
    /// required. `supported` is the server's exact finite bit set.
    Unsupported {
        supported: TopologyCapabilities,
    },
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesCoherentResponse {
    pub negotiated: TopologyCapabilities,
    pub stream_id: TopologyStreamId,
    pub outcome: ListPanesCoherentOutcome,
}

/// Every mux notification that advances [`TopologyRevision`] has an explicit
/// wire representation. Variants that legacy clients historically ignored
/// remain represented so a fenced stream never creates an unexplained gap.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum TopologyEventKind {
    PaneAdded {
        pane_id: PaneId,
    },
    PaneRemoved {
        pane_id: PaneId,
    },
    WindowCreated {
        window_id: WindowId,
    },
    WindowRemoved {
        window_id: WindowId,
    },
    WindowInvalidated {
        window_id: WindowId,
    },
    WindowWorkspaceChanged {
        window_id: WindowId,
        workspace: Option<String>,
    },
    Empty,
    TabAddedToWindow {
        tab_id: TabId,
        window_id: WindowId,
    },
    PaneFocused {
        pane_id: PaneId,
    },
    TabResized {
        tab_id: TabId,
    },
    TabTitleChanged {
        tab_id: TabId,
        title: String,
    },
    WindowTitleChanged {
        window_id: WindowId,
        title: String,
    },
    WorkspaceRenamed {
        old_workspace: String,
        new_workspace: String,
    },
}

/// One connection-scoped, revision-stamped topology transition.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct TopologyEvent {
    pub stream_id: TopologyStreamId,
    pub revision: TopologyRevision,
    pub event: TopologyEventKind,
}

/// Closed schema version carried by ordered-window PDU IDs 86-90.
pub const ORDERED_WINDOW_PROTOCOL_VERSION: u16 = 1;
/// Oldest negotiated codec dialect that may send ordered-window PDU IDs 86-90.
pub const ORDERED_WINDOW_V1_MIN_CODEC_VERSION: usize = 51;

#[must_use]
pub const fn codec_version_supports_ordered_window_v1(codec_version: usize) -> bool {
    codec_version >= ORDERED_WINDOW_V1_MIN_CODEC_VERSION
}

/// Hard v1 resource ceilings. Live implementations may negotiate smaller
/// budgets, but no v1 sender or receiver may exceed these values.
pub const MAX_ORDERED_WINDOWS_PER_SNAPSHOT: usize = 4_096;
pub const MAX_ORDERED_TABS_PER_WINDOW: usize = 4_096;
pub const MAX_ORDERED_TABS_PER_SNAPSHOT: usize = 16_384;
pub const MAX_ORDERED_WINDOW_SECTION_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_REORDER_WINDOW_TABS_DECOMPRESSED_BYTES: usize = 512 * 1024;

/// A single committed transition can freeze one affected window or both sides
/// of one cross-window move. Publishing separate same-revision events would be
/// ambiguous to revision-keyed consumers.
pub const MAX_ORDERED_WINDOWS_PER_EVENT: usize = 2;

/// Domain-separated canonical digest grammar for [`ReorderWindowTabsV1`].
///
/// Fields are appended in declaration order using fixed-width big-endian
/// integers and raw fixed-size identity bytes. The topology stream ID is
/// intentionally excluded: it rotates on reconnect, while an idempotent retry
/// must retain the same request digest on the successor stream.
pub const WINDOW_REORDER_DIGEST_DOMAIN_V1: &[u8] =
    b"frankenterm.window-reorder.v1\0";

macro_rules! stable_order_wire_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        pub struct $name(u64);

        impl $name {
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            pub const fn get(self) -> u64 {
                self.0
            }

            /// Checked conversion for the current mux's process-local ID type.
            /// Wire decoding itself remains architecture-independent `u64`.
            pub fn try_into_usize(self) -> Result<usize, OrderedWindowProtocolError> {
                usize::try_from(self.0).map_err(|_| {
                    OrderedWindowProtocolError::WireIdDoesNotFitUsize {
                        field: stringify!($name),
                        value: self.0,
                    }
                })
            }
        }
    };
}

stable_order_wire_id!(RemoteWindowId);
stable_order_wire_id!(RemoteTabId);

/// Per-window nonwrapping order/membership/active revision.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct WindowOrderRevision(u64);

impl WindowOrderRevision {
    pub const INITIAL: Self = Self(0);

    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Client-owned durable binding identity. It is routing and audit context, not
/// proof of the server session reached by a connection.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct DomainBindingId([u8; 16]);

impl DomainBindingId {
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Idempotency identity unique inside one random client mutation namespace.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct WindowOrderMutationId {
    pub namespace: [u8; 16],
    pub sequence: u64,
}

impl WindowOrderMutationId {
    pub const fn new(namespace: [u8; 16], sequence: u64) -> Self {
        Self {
            namespace,
            sequence,
        }
    }

    fn validate(self) -> Result<(), OrderedWindowProtocolError> {
        if self.namespace == [0; 16] {
            return Err(OrderedWindowProtocolError::ReservedIdentity {
                field: "mutation_namespace",
            });
        }
        if self.sequence == 0 || self.sequence == u64::MAX {
            return Err(OrderedWindowProtocolError::ReservedWireId {
                field: "mutation_sequence",
                value: self.sequence,
            });
        }
        Ok(())
    }
}

/// SHA-256 binding of one frozen reorder intent.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct WindowReorderDigest([u8; 32]);

impl WindowReorderDigest {
    pub const ZERO: Self = Self([0; 32]);

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }
}

/// Complete authoritative state of one exact remote mux window.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct OrderedWindowStateV1 {
    pub window_id: RemoteWindowId,
    pub order_revision: WindowOrderRevision,
    #[serde(
        serialize_with = "serialize_ordered_tab_ids",
        deserialize_with = "deserialize_ordered_tab_ids"
    )]
    pub ordered_tab_ids: Vec<RemoteTabId>,
    pub active_tab_id: Option<RemoteTabId>,
}

impl OrderedWindowStateV1 {
    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_ordered_windows_with_section_limit(
            std::slice::from_ref(self),
            false,
            MAX_ORDERED_WINDOW_SECTION_BYTES,
        )
    }
}

/// One coherent pane plus ordered-window bootstrap at a shared topology
/// revision. This is the PDU87 success body; flat PDU4/PDU82 vectors alone do
/// not acquire ordering authority.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct OrderedPaneSnapshotV1 {
    pub session_incarnation: MuxSessionIncarnation,
    pub topology_revision: TopologyRevision,
    pub panes: ListPanesResponse,
    #[serde(
        serialize_with = "serialize_ordered_window_section",
        deserialize_with = "deserialize_ordered_window_section"
    )]
    pub ordered_windows: Vec<OrderedWindowStateV1>,
}

impl OrderedPaneSnapshotV1 {
    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_nonzero_identity(
            "session_incarnation",
            self.session_incarnation.as_bytes(),
        )?;
        validate_topology_revision(self.topology_revision)?;
        validate_ordered_windows_with_section_limit(
            &self.ordered_windows,
            false,
            MAX_ORDERED_WINDOW_SECTION_BYTES,
        )
    }
}

/// Negotiated request for a coherent pane and ordered-window bootstrap.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesOrderedV1 {
    pub protocol_version: u16,
    pub supported: TopologyCapabilities,
    pub required: TopologyCapabilities,
}

impl ListPanesOrderedV1 {
    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        self.supported.validate()?;
        self.required.validate()?;
        let foundation = TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        );
        if !self.supported.contains(self.required) {
            return Err(OrderedWindowProtocolError::RequiredCapabilitiesNotOffered {
                supported: self.supported.bits(),
                required: self.required.bits(),
            });
        }
        if !self.required.contains(foundation) {
            return Err(OrderedWindowProtocolError::MissingRequiredCapabilities {
                required: self.required.bits(),
                missing: foundation.bits() & !self.required.bits(),
            });
        }
        Ok(())
    }
}

/// Typed result of bounded coherent ordered-window snapshot construction.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum ListPanesOrderedV1Outcome {
    Snapshot(OrderedPaneSnapshotV1),
    Contended {
        attempts: u8,
        first_revision: TopologyRevision,
        last_revision: TopologyRevision,
    },
    RevisionExhausted,
    Unsupported {
        supported: TopologyCapabilities,
    },
}

impl ListPanesOrderedV1Outcome {
    fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        match self {
            Self::Snapshot(snapshot) => snapshot.validate(),
            Self::Contended {
                attempts,
                first_revision,
                last_revision,
            } => {
                if *attempts == 0
                    || first_revision.get() == u64::MAX
                    || last_revision.get() == u64::MAX
                    || first_revision > last_revision
                {
                    return Err(OrderedWindowProtocolError::InvalidContentionRange {
                        attempts: *attempts,
                        first_revision: first_revision.get(),
                        last_revision: last_revision.get(),
                    });
                }
                Ok(())
            }
            Self::RevisionExhausted => Ok(()),
            Self::Unsupported { supported } => {
                supported.validate()?;
                Ok(())
            }
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesOrderedV1Response {
    pub protocol_version: u16,
    pub negotiated: TopologyCapabilities,
    pub stream_id: TopologyStreamId,
    pub outcome: ListPanesOrderedV1Outcome,
}

impl ListPanesOrderedV1Response {
    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        self.negotiated.validate()?;
        validate_nonzero_identity("stream_id", self.stream_id.as_bytes())?;
        self.outcome.validate()?;
        if matches!(&self.outcome, ListPanesOrderedV1Outcome::Snapshot(_)) {
            let foundation = TopologyCapabilities::from_bits(
                TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                    | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
            );
            if !self.negotiated.contains(foundation) {
                return Err(OrderedWindowProtocolError::MissingNegotiatedCapabilities {
                    negotiated: self.negotiated.bits(),
                    missing: foundation.bits() & !self.negotiated.bits(),
                });
            }
        }
        Ok(())
    }
}

/// One exact, bounded, idempotent pure-window reorder compare-and-set.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct ReorderWindowTabsV1 {
    pub protocol_version: u16,
    pub domain_binding_id: DomainBindingId,
    pub stream_id: TopologyStreamId,
    pub session_incarnation: MuxSessionIncarnation,
    pub window_id: RemoteWindowId,
    pub expected_order_revision: WindowOrderRevision,
    #[serde(
        serialize_with = "serialize_ordered_tab_ids",
        deserialize_with = "deserialize_ordered_tab_ids"
    )]
    pub desired_tab_ids: Vec<RemoteTabId>,
    pub desired_active_tab_id: Option<RemoteTabId>,
    pub mutation_id: WindowOrderMutationId,
    pub digest: WindowReorderDigest,
}

impl ReorderWindowTabsV1 {
    /// Recompute the canonical intent digest. `stream_id` is deliberately not
    /// mixed so the same idempotent request survives a reconnect.
    #[must_use]
    pub fn canonical_digest(&self) -> WindowReorderDigest {
        let mut hasher = Sha256::new();
        hasher.update(WINDOW_REORDER_DIGEST_DOMAIN_V1);
        hasher.update(self.protocol_version.to_be_bytes());
        hasher.update(self.domain_binding_id.as_bytes());
        hasher.update(self.session_incarnation.as_bytes());
        hasher.update(self.window_id.get().to_be_bytes());
        hasher.update(self.expected_order_revision.get().to_be_bytes());
        hasher.update(
            u64::try_from(self.desired_tab_ids.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        for tab_id in &self.desired_tab_ids {
            hasher.update(tab_id.get().to_be_bytes());
        }
        match self.desired_active_tab_id {
            None => hasher.update([0]),
            Some(tab_id) => {
                hasher.update([1]);
                hasher.update(tab_id.get().to_be_bytes());
            }
        }
        hasher.update(self.mutation_id.namespace);
        hasher.update(self.mutation_id.sequence.to_be_bytes());
        WindowReorderDigest::from_bytes(hasher.finalize().into())
    }

    /// Replace the digest with the canonical binding of the current frozen
    /// fields. Useful while constructing a request before admission.
    #[must_use]
    pub fn with_computed_digest(mut self) -> Self {
        self.digest = self.canonical_digest();
        self
    }

    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        validate_nonzero_identity("domain_binding_id", self.domain_binding_id.as_bytes())?;
        validate_nonzero_identity("stream_id", self.stream_id.as_bytes())?;
        validate_nonzero_identity(
            "session_incarnation",
            self.session_incarnation.as_bytes(),
        )?;
        validate_remote_wire_id("window_id", self.window_id.get())?;
        validate_window_order_revision(self.expected_order_revision)?;
        self.mutation_id.validate()?;
        validate_ordered_window_components(
            self.window_id,
            self.expected_order_revision,
            &self.desired_tab_ids,
            self.desired_active_tab_id,
        )?;

        let expected_digest = self.canonical_digest();
        if self.digest != expected_digest {
            return Err(OrderedWindowProtocolError::DigestMismatch {
                expected: expected_digest,
                actual: self.digest,
            });
        }
        Ok(())
    }
}

/// Frozen window state and session-global topology stamp returned by an
/// applied or conflicting reorder decision.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct WindowOrderCommitV1 {
    pub topology_revision: TopologyRevision,
    pub window: OrderedWindowStateV1,
}

impl WindowOrderCommitV1 {
    fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_topology_revision(self.topology_revision)?;
        self.window.validate()
    }
}

/// Replayable terminal decision retained by the bounded server receipt ledger.
/// A replay cannot recursively contain another replay marker.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum WindowReorderTerminalOutcomeV1 {
    Applied(WindowOrderCommitV1),
    Conflict(WindowOrderCommitV1),
    StaleIncarnation,
    Malformed,
    Exhausted,
}

impl WindowReorderTerminalOutcomeV1 {
    fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        match self {
            Self::Applied(commit) | Self::Conflict(commit) => commit.validate(),
            Self::StaleIncarnation | Self::Malformed | Self::Exhausted => Ok(()),
        }
    }
}

/// Typed reorder result. `Replay` contains the exact terminal decision bound
/// to the echoed mutation identity and request digest.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub enum ReorderWindowTabsV1Outcome {
    Applied(WindowOrderCommitV1),
    Replay(WindowReorderTerminalOutcomeV1),
    Conflict(WindowOrderCommitV1),
    StaleIncarnation,
    Malformed,
    Exhausted,
}

impl ReorderWindowTabsV1Outcome {
    fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        match self {
            Self::Applied(commit) | Self::Conflict(commit) => commit.validate(),
            Self::Replay(outcome) => outcome.validate(),
            Self::StaleIncarnation | Self::Malformed | Self::Exhausted => Ok(()),
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct ReorderWindowTabsV1Response {
    pub protocol_version: u16,
    pub stream_id: TopologyStreamId,
    pub session_incarnation: MuxSessionIncarnation,
    pub mutation_id: WindowOrderMutationId,
    pub request_digest: WindowReorderDigest,
    pub outcome: ReorderWindowTabsV1Outcome,
}

impl ReorderWindowTabsV1Response {
    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        validate_nonzero_identity("stream_id", self.stream_id.as_bytes())?;
        validate_nonzero_identity(
            "session_incarnation",
            self.session_incarnation.as_bytes(),
        )?;
        self.mutation_id.validate()?;
        self.outcome.validate()
    }
}

/// One lossless connection-scoped order transition. Cross-window moves carry
/// both frozen states under one topology revision in this single PDU.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone)]
pub struct WindowOrderEventV1 {
    pub protocol_version: u16,
    pub stream_id: TopologyStreamId,
    pub session_incarnation: MuxSessionIncarnation,
    pub topology_revision: TopologyRevision,
    #[serde(
        serialize_with = "serialize_ordered_window_section",
        deserialize_with = "deserialize_ordered_window_section"
    )]
    pub windows: Vec<OrderedWindowStateV1>,
}

impl WindowOrderEventV1 {
    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        validate_nonzero_identity("stream_id", self.stream_id.as_bytes())?;
        validate_nonzero_identity(
            "session_incarnation",
            self.session_incarnation.as_bytes(),
        )?;
        validate_topology_revision(self.topology_revision)?;
        if self.windows.len() > MAX_ORDERED_WINDOWS_PER_EVENT {
            return Err(OrderedWindowProtocolError::TooManyEventWindows {
                count: self.windows.len(),
                max: MAX_ORDERED_WINDOWS_PER_EVENT,
            });
        }
        validate_ordered_windows_with_section_limit(
            &self.windows,
            true,
            MAX_ORDERED_WINDOW_SECTION_BYTES,
        )
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OrderedWindowProtocolError {
    #[error(
        "ordered-window protocol version {actual} is unsupported; expected {expected}"
    )]
    UnsupportedProtocolVersion { actual: u16, expected: u16 },
    #[error(transparent)]
    InvalidCapabilities(#[from] TopologyCapabilitiesError),
    #[error(
        "ordered-window required capabilities {required:#x} are not a subset of offered capabilities {supported:#x}"
    )]
    RequiredCapabilitiesNotOffered { supported: u64, required: u64 },
    #[error(
        "ordered-window request is missing required capability bits {missing:#x} from {required:#x}"
    )]
    MissingRequiredCapabilities { required: u64, missing: u64 },
    #[error(
        "ordered-window snapshot negotiated bits {negotiated:#x} are missing authority bits {missing:#x}"
    )]
    MissingNegotiatedCapabilities { negotiated: u64, missing: u64 },
    #[error("ordered-window identity {field} uses the reserved all-zero value")]
    ReservedIdentity { field: &'static str },
    #[error("ordered-window wire identity {field} uses reserved value {value}")]
    ReservedWireId { field: &'static str, value: u64 },
    #[error("ordered-window wire identity {field}={value} does not fit in usize")]
    WireIdDoesNotFitUsize { field: &'static str, value: u64 },
    #[error("ordered-window revision {field} uses the terminal u64::MAX sentinel")]
    RevisionExhausted { field: &'static str },
    #[error("ordered-window snapshot has {count} windows; maximum is {max}")]
    TooManyWindows { count: usize, max: usize },
    #[error("ordered-window event has {count} windows; maximum is {max}")]
    TooManyEventWindows { count: usize, max: usize },
    #[error("ordered-window event must contain at least one frozen window")]
    EmptyWindowEvent,
    #[error("window {window_id} has {count} tabs; maximum is {max}")]
    TooManyTabs {
        window_id: u64,
        count: usize,
        max: usize,
    },
    #[error("ordered-window snapshot has {count} total tabs; maximum is {max}")]
    TooManyTotalTabs { count: usize, max: usize },
    #[error("ordered-window count arithmetic overflowed before validation completed")]
    CountOverflow,
    #[error("ordered-window snapshot repeats window id {window_id}")]
    DuplicateWindowId { window_id: u64 },
    #[error("ordered-window snapshot repeats tab id {tab_id}")]
    DuplicateTabId { tab_id: u64 },
    #[error("non-empty window {window_id} has no authoritative active tab")]
    ActiveTabRequired { window_id: u64 },
    #[error("window {window_id} names non-member active tab {active_tab_id}")]
    ActiveTabNotInWindow {
        window_id: u64,
        active_tab_id: u64,
    },
    #[error("encoded ordered-window section has {bytes} bytes; maximum is {max}")]
    OrderSectionTooLarge { bytes: usize, max: usize },
    #[error("ordered-window section could not be canonically measured")]
    OrderSectionEncoding,
    #[error(
        "invalid ordered-window contention range attempts={attempts} first={first_revision} last={last_revision}"
    )]
    InvalidContentionRange {
        attempts: u8,
        first_revision: u64,
        last_revision: u64,
    },
    #[error("reorder request digest mismatch: expected {expected:?}, received {actual:?}")]
    DigestMismatch {
        expected: WindowReorderDigest,
        actual: WindowReorderDigest,
    },
}

fn validate_protocol_version(version: u16) -> Result<(), OrderedWindowProtocolError> {
    if version != ORDERED_WINDOW_PROTOCOL_VERSION {
        return Err(OrderedWindowProtocolError::UnsupportedProtocolVersion {
            actual: version,
            expected: ORDERED_WINDOW_PROTOCOL_VERSION,
        });
    }
    Ok(())
}

fn validate_nonzero_identity(
    field: &'static str,
    bytes: [u8; 16],
) -> Result<(), OrderedWindowProtocolError> {
    if bytes == [0; 16] {
        return Err(OrderedWindowProtocolError::ReservedIdentity { field });
    }
    Ok(())
}

fn validate_remote_wire_id(
    field: &'static str,
    value: u64,
) -> Result<(), OrderedWindowProtocolError> {
    if value == 0 || value == u64::MAX {
        return Err(OrderedWindowProtocolError::ReservedWireId { field, value });
    }
    Ok(())
}

fn validate_topology_revision(
    revision: TopologyRevision,
) -> Result<(), OrderedWindowProtocolError> {
    if revision.get() == u64::MAX {
        return Err(OrderedWindowProtocolError::RevisionExhausted {
            field: "topology_revision",
        });
    }
    Ok(())
}

fn validate_window_order_revision(
    revision: WindowOrderRevision,
) -> Result<(), OrderedWindowProtocolError> {
    if revision.get() == u64::MAX {
        return Err(OrderedWindowProtocolError::RevisionExhausted {
            field: "window_order_revision",
        });
    }
    Ok(())
}

fn encoded_ordered_window_section_len(
    windows: &[OrderedWindowStateV1],
) -> Result<usize, OrderedWindowProtocolError> {
    #[derive(Default)]
    struct CountingWriter {
        bytes: usize,
    }

    impl std::io::Write for CountingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.bytes = self.bytes.checked_add(buffer.len()).ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "ordered-window encoded length overflow",
                )
            })?;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = CountingWriter::default();
    let mut serializer = varbincode::Serializer::new(&mut counter);
    windows
        .serialize(&mut serializer)
        .map_err(|_| OrderedWindowProtocolError::OrderSectionEncoding)?;
    Ok(counter.bytes)
}

fn validate_ordered_windows_with_section_limit(
    windows: &[OrderedWindowStateV1],
    require_nonempty: bool,
    max_section_bytes: usize,
) -> Result<(), OrderedWindowProtocolError> {
    if require_nonempty && windows.is_empty() {
        return Err(OrderedWindowProtocolError::EmptyWindowEvent);
    }
    if windows.len() > MAX_ORDERED_WINDOWS_PER_SNAPSHOT {
        return Err(OrderedWindowProtocolError::TooManyWindows {
            count: windows.len(),
            max: MAX_ORDERED_WINDOWS_PER_SNAPSHOT,
        });
    }

    let mut window_ids = HashSet::with_capacity(windows.len());
    let mut tab_ids = HashSet::new();
    let mut total_tabs = 0_usize;
    for window in windows {
        if !window_ids.insert(window.window_id) {
            return Err(OrderedWindowProtocolError::DuplicateWindowId {
                window_id: window.window_id.get(),
            });
        }
        let local_tabs = validate_ordered_window_components(
            window.window_id,
            window.order_revision,
            &window.ordered_tab_ids,
            window.active_tab_id,
        )?;
        total_tabs = total_tabs
            .checked_add(window.ordered_tab_ids.len())
            .ok_or(OrderedWindowProtocolError::CountOverflow)?;
        if total_tabs > MAX_ORDERED_TABS_PER_SNAPSHOT {
            return Err(OrderedWindowProtocolError::TooManyTotalTabs {
                count: total_tabs,
                max: MAX_ORDERED_TABS_PER_SNAPSHOT,
            });
        }

        for tab_id in local_tabs {
            if !tab_ids.insert(tab_id) {
                return Err(OrderedWindowProtocolError::DuplicateTabId {
                    tab_id: tab_id.get(),
                });
            }
        }
    }

    let section_bytes = encoded_ordered_window_section_len(windows)?;
    if section_bytes > max_section_bytes {
        return Err(OrderedWindowProtocolError::OrderSectionTooLarge {
            bytes: section_bytes,
            max: max_section_bytes,
        });
    }
    Ok(())
}

fn validate_ordered_window_components(
    window_id: RemoteWindowId,
    order_revision: WindowOrderRevision,
    ordered_tab_ids: &[RemoteTabId],
    active_tab_id: Option<RemoteTabId>,
) -> Result<HashSet<RemoteTabId>, OrderedWindowProtocolError> {
    validate_remote_wire_id("window_id", window_id.get())?;
    validate_window_order_revision(order_revision)?;
    if ordered_tab_ids.len() > MAX_ORDERED_TABS_PER_WINDOW {
        return Err(OrderedWindowProtocolError::TooManyTabs {
            window_id: window_id.get(),
            count: ordered_tab_ids.len(),
            max: MAX_ORDERED_TABS_PER_WINDOW,
        });
    }

    let mut local_tabs = HashSet::with_capacity(ordered_tab_ids.len());
    for tab_id in ordered_tab_ids {
        validate_remote_wire_id("tab_id", tab_id.get())?;
        if !local_tabs.insert(*tab_id) {
            return Err(OrderedWindowProtocolError::DuplicateTabId {
                tab_id: tab_id.get(),
            });
        }
    }
    if let Some(active_tab_id) = active_tab_id {
        validate_remote_wire_id("active_tab_id", active_tab_id.get())?;
    }
    match (ordered_tab_ids.is_empty(), active_tab_id) {
        (false, None) => {
            return Err(OrderedWindowProtocolError::ActiveTabRequired {
                window_id: window_id.get(),
            });
        }
        (_, Some(active_tab_id)) if !local_tabs.contains(&active_tab_id) => {
            return Err(OrderedWindowProtocolError::ActiveTabNotInWindow {
                window_id: window_id.get(),
                active_tab_id: active_tab_id.get(),
            });
        }
        _ => {}
    }
    Ok(local_tabs)
}

fn serialize_bounded_vec<S, T, const MAX: usize>(
    values: &[T],
    serializer: S,
    label: &'static str,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: Serialize,
{
    if values.len() > MAX {
        return Err(serde::ser::Error::custom(format_args!(
            "{label} length {} exceeds maximum {MAX}",
            values.len()
        )));
    }
    values.serialize(serializer)
}

struct BoundedVecVisitor<T, const MAX: usize> {
    label: &'static str,
    marker: std::marker::PhantomData<T>,
}

impl<'de, T, const MAX: usize> serde::de::Visitor<'de> for BoundedVecVisitor<T, MAX>
where
    T: Deserialize<'de>,
{
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "at most {MAX} {}", self.label)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let hinted = sequence.size_hint().unwrap_or(0);
        if hinted > MAX {
            return Err(serde::de::Error::custom(format_args!(
                "{} length {hinted} exceeds maximum {MAX}",
                self.label
            )));
        }
        let mut values = Vec::new();
        values.try_reserve(hinted).map_err(|error| {
            serde::de::Error::custom(format_args!(
                "allocating {} length {hinted} failed: {error}",
                self.label
            ))
        })?;
        while let Some(value) = sequence.next_element()? {
            if values.len() == MAX {
                return Err(serde::de::Error::custom(format_args!(
                    "{} length exceeds maximum {MAX}",
                    self.label
                )));
            }
            values.push(value);
        }
        Ok(values)
    }
}

fn deserialize_bounded_vec<'de, D, T, const MAX: usize>(
    deserializer: D,
    label: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    deserializer.deserialize_seq(BoundedVecVisitor::<T, MAX> {
        label,
        marker: std::marker::PhantomData,
    })
}

fn serialize_ordered_tab_ids<S>(
    values: &[RemoteTabId],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serialize_bounded_vec::<S, _, MAX_ORDERED_TABS_PER_WINDOW>(
        values,
        serializer,
        "ordered tab ids",
    )
}

fn deserialize_ordered_tab_ids<'de, D>(deserializer: D) -> Result<Vec<RemoteTabId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, _, MAX_ORDERED_TABS_PER_WINDOW>(
        deserializer,
        "ordered tab ids",
    )
}

fn serialize_ordered_windows<S>(
    values: &[OrderedWindowStateV1],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serialize_bounded_vec::<S, _, MAX_ORDERED_WINDOWS_PER_SNAPSHOT>(
        values,
        serializer,
        "ordered windows",
    )
}

fn deserialize_ordered_windows<'de, D>(
    deserializer: D,
) -> Result<Vec<OrderedWindowStateV1>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, _, MAX_ORDERED_WINDOWS_PER_SNAPSHOT>(
        deserializer,
        "ordered windows",
    )
}

fn serialize_ordered_window_section<S>(
    values: &[OrderedWindowStateV1],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    validate_ordered_windows_with_section_limit(
        values,
        false,
        MAX_ORDERED_WINDOW_SECTION_BYTES,
    )
    .map_err(serde::ser::Error::custom)?;

    let mut section = Vec::new();
    let mut section_serializer = varbincode::Serializer::new(&mut section);
    serialize_ordered_windows(values, &mut section_serializer)
        .map_err(serde::ser::Error::custom)?;
    if section.len() > MAX_ORDERED_WINDOW_SECTION_BYTES {
        return Err(serde::ser::Error::custom(format_args!(
            "ordered-window section length {} exceeds maximum {}",
            section.len(),
            MAX_ORDERED_WINDOW_SECTION_BYTES
        )));
    }
    section.serialize(serializer)
}

fn deserialize_ordered_window_section<'de, D>(
    deserializer: D,
) -> Result<Vec<OrderedWindowStateV1>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct OrderedWindowsWire(
        #[serde(deserialize_with = "deserialize_ordered_windows")]
        Vec<OrderedWindowStateV1>,
    );

    let section = deserialize_bounded_vec::<D, u8, MAX_ORDERED_WINDOW_SECTION_BYTES>(
        deserializer,
        "ordered-window section bytes",
    )?;
    let mut reader = section.as_slice();
    let OrderedWindowsWire(windows) =
        bounded_varbincode::deserialize::<OrderedWindowsWire, _>(&mut reader)
            .map_err(serde::de::Error::custom)?;
    if !reader.is_empty() {
        return Err(serde::de::Error::custom(
            "ordered-window section has trailing schema bytes",
        ));
    }
    validate_ordered_windows_with_section_limit(
        &windows,
        false,
        MAX_ORDERED_WINDOW_SECTION_BYTES,
    )
    .map_err(serde::de::Error::custom)?;
    Ok(windows)
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesTabStacks {}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesTabStackEntry {
    pub window_id: WindowId,
    pub stack_id: TabStackId,
    pub tab_id: TabId,
    pub position: usize,
    pub is_visible: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesResponse {
    pub tabs: Vec<PaneNode>,
    pub tab_titles: Vec<String>,
    pub window_titles: HashMap<WindowId, String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesTabStacksResponse {
    pub tab_stack_entries: Vec<ListPanesTabStackEntry>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SplitPane {
    pub pane_id: PaneId,
    pub split_request: SplitRequest,
    pub command: Option<CommandBuilder>,
    pub command_dir: Option<String>,
    pub domain: config::keyassignment::SpawnTabDomain,
    /// Instead of spawning a command, move the specified
    /// pane into the new split target
    pub move_pane_id: Option<PaneId>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct MovePaneToNewTab {
    pub pane_id: PaneId,
    pub window_id: Option<WindowId>,
    pub workspace_for_new_window: Option<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct MovePaneToNewTabResponse {
    pub tab_id: TabId,
    pub window_id: WindowId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SpawnV2 {
    pub domain: config::keyassignment::SpawnTabDomain,
    /// If None, create a new window for this new tab
    pub window_id: Option<WindowId>,
    pub command: Option<CommandBuilder>,
    pub command_dir: Option<String>,
    pub size: TerminalSize,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneRemoved {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct KillPane {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SpawnResponse {
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub window_id: WindowId,
    pub size: TerminalSize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct WriteToPane {
    pub pane_id: PaneId,
    pub data: Vec<u8>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SendPaste {
    pub pane_id: PaneId,
    pub data: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SendKeyDown {
    pub pane_id: TabId,
    pub event: termwiz::input::KeyEvent,
    pub input_serial: InputSerial,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SendKeyUp {
    pub pane_id: TabId,
    pub event: termwiz::input::KeyEvent,
}

/// Client-generated identity used to order input dispatch acknowledgements.
///
/// Values retain a millisecond-since-epoch floor so a returned serial can
/// estimate dispatch round-trip time, but [`InputSerial::now`] also enforces
/// process-local monotonicity. Before terminal `u64` exhaustion, wall-clock
/// rollback and multiple keystrokes in one millisecond therefore cannot reverse
/// or alias the ordering relation.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, PartialOrd, Ord)]
pub struct InputSerial(u64);

static LAST_INPUT_SERIAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

impl InputSerial {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub fn now() -> Self {
        use std::sync::atomic::Ordering;

        let wall_clock = input_serial_from_system_time(std::time::SystemTime::now()).0;
        let mut observed = LAST_INPUT_SERIAL.load(Ordering::Relaxed);
        loop {
            let candidate = wall_clock.max(observed.saturating_add(1));
            match LAST_INPUT_SERIAL.compare_exchange_weak(
                observed,
                candidate,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Self(candidate),
                Err(current) => observed = current,
            }
        }
    }

    pub fn elapsed_millis(&self) -> u64 {
        let now = input_serial_from_system_time(std::time::SystemTime::now());
        now.0.saturating_sub(self.0)
    }
}

impl From<std::time::SystemTime> for InputSerial {
    fn from(val: std::time::SystemTime) -> Self {
        input_serial_from_system_time(val)
    }
}

fn input_serial_from_system_time(value: std::time::SystemTime) -> InputSerial {
    match value.duration_since(std::time::SystemTime::UNIX_EPOCH) {
        Ok(duration) => input_serial_from_epoch_duration(duration),
        Err(_) => InputSerial::empty(),
    }
}

fn input_serial_from_epoch_duration(duration: std::time::Duration) -> InputSerial {
    InputSerial(duration.as_millis().try_into().unwrap_or(u64::MAX))
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SendMouseEvent {
    pub pane_id: PaneId,
    pub event: frankenterm_term::input::MouseEvent,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetClipboard {
    pub pane_id: PaneId,
    pub clipboard: Option<String>,
    pub selection: ClipboardSelection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetWindowWorkspace {
    pub window_id: WindowId,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct RenameWorkspace {
    pub old_workspace: String,
    pub new_workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetActiveWorkspace {
    pub workspace: String,
}

/// This is used both as a notification from server->client
/// and as a configuration request from client->server when
/// the client's preferred configuration changes
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetPalette {
    pub pane_id: PaneId,
    pub palette: ColorPalette,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct NotifyAlert {
    pub pane_id: PaneId,
    pub alert: Alert,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct TabAddedToWindow {
    pub tab_id: TabId,
    pub window_id: WindowId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct TabResized {
    pub tab_id: TabId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct TabTitleChanged {
    pub tab_id: TabId,
    pub title: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct WindowTitleChanged {
    pub window_id: WindowId,
    pub title: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct PaneFocused {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct WindowWorkspaceChanged {
    pub window_id: WindowId,
    pub workspace: String,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetClientId {
    pub client_id: ClientId,
    pub is_proxy: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetFocusedPane {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetClientList;

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetClientListResponse {
    pub clients: Vec<ClientInfo>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct Resize {
    pub containing_tab_id: TabId,
    pub pane_id: PaneId,
    pub size: TerminalSize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetPaneZoomed {
    pub containing_tab_id: TabId,
    pub pane_id: PaneId,
    pub zoomed: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetPaneDirection {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct AdjustPaneSize {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
    pub amount: usize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct CreateFloatingPane {
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub rect: FloatingPaneRect,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct MoveFloatingPane {
    pub pane_id: PaneId,
    pub rect: FloatingPaneRect,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetFloatingPaneZ {
    pub pane_id: PaneId,
    pub z_order: u32,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ToggleFloatingPane {
    pub pane_id: PaneId,
    pub visible: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct RemoveFloatingPane {
    pub pane_id: PaneId,
}

// --- Swap layout and stack PDUs (ft-2dd4s.5) ---

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SwapToLayout {
    pub tab_id: TabId,
    pub layout_index: usize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SetLayoutCycle {
    pub tab_id: TabId,
    pub layout_names: Vec<String>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct CycleStack {
    pub tab_id: TabId,
    pub slot_index: usize,
    /// true = forward (next), false = backward (prev)
    pub forward: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SelectStackPane {
    pub tab_id: TabId,
    pub slot_index: usize,
    pub pane_index: usize,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct UpdatePaneConstraints {
    pub pane_id: PaneId,
    pub min_width: Option<usize>,
    pub max_width: Option<usize>,
    pub min_height: Option<usize>,
    pub max_height: Option<usize>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetPaneDirectionResponse {
    pub pane_id: Option<PaneId>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ActivatePaneDirection {
    pub pane_id: PaneId,
    pub direction: PaneDirection,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetPaneRenderChanges {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetSemanticZones {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetSemanticZonesResponse {
    pub pane_id: PaneId,
    pub zones: Vec<SemanticZone>,
    pub zone_texts: Vec<String>,
    pub last_exit_code: Option<i32>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetPaneRenderableDimensions {
    pub pane_id: PaneId,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetPaneRenderableDimensionsResponse {
    pub pane_id: PaneId,
    pub cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
    // NOTE: skip_serializing_if removed — varbincode is a positional binary
    // format where skipping fields misaligns all subsequent field positions.
    // The default remains useful to self-describing serde formats; varbincode
    // compatibility with a schema that omitted this field requires a distinct
    // PDU identifier or an explicit dual-schema decoder.
    #[serde(default)]
    pub tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct LivenessResponse {
    pub pane_id: PaneId,
    pub is_alive: bool,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetPaneRenderChangesResponse {
    pub pane_id: PaneId,
    pub mouse_grabbed: bool,
    #[serde(default)]
    pub alt_screen_active: bool,
    pub cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,
    // NOTE: skip_serializing_if removed — varbincode is a positional binary format
    // where skipping fields misaligns all subsequent field positions. When
    // tiered_scrollback_status is None and skip_serializing_if is active, the None
    // tag byte is not written, causing dirty_lines/title/etc. to shift position and
    // produce "failed to fill whole buffer" codec errors during deserialization.
    #[serde(default)]
    pub tiered_scrollback_status: Option<PaneTieredScrollbackStatus>,
    pub dirty_lines: Vec<Range<StableRowIndex>>,
    pub title: String,
    pub working_dir: Option<SerdeUrl>,
    /// Lines that the server thought we'd almost certainly
    /// want to fetch as soon as we received this response
    pub bonus_lines: SerializedLines,

    /// Highest client input serial whose `pane.key_down` dispatch completed
    /// before this surface snapshot was sampled.
    ///
    /// This is a protocol-dispatch acknowledgement, not proof that the PTY or
    /// application echoed the input. Consumers must pair it with this snapshot's
    /// `seqno` as a fence and wait for later authoritative terminal state before
    /// settling speculative local echo.
    pub input_serial: Option<InputSerial>,
    pub seqno: SequenceNo,
}

/// Stable identifier for the application-level render settlement protocol.
///
/// Transport enqueue, socket write, frame decode, and GUI scheduling are not
/// application acknowledgement. A server may commit a prepared render
/// baseline only after receiving a matching [`RenderApplicationResult`] whose
/// outcome is [`RenderApplicationOutcome::Applied`].
pub const RENDER_APPLICATION_PROTOCOL_VERSION: u16 = 2;

/// Oldest codec dialect that knows the authoritative v2 render PDU IDs 84/85.
///
/// Live activation must negotiate at least this dialect before emitting either
/// PDU. IDs 79/80 remain permanently bound to their v1 schemas.
pub const RENDER_APPLICATION_V2_MIN_CODEC_VERSION: usize = 50;

/// Hard upper bound for application attempts retained for one render
/// obligation. Live code may configure a lower limit, but never a higher one.
pub const MAX_RENDER_APPLICATION_ATTEMPTS: u16 = 8;

/// Hard upper bound for exact, event-like alerts carried by one atomic render
/// application. State-like progress is represented by the latest retained
/// value before an update is prepared; event-like occurrences stay explicit.
pub const MAX_RENDER_APPLICATION_ALERTS: usize = 64;

/// Hard wire bounds for one atomic render application. Receivers may advertise
/// lower limits, but no sender may construct a v2 application above these
/// ceilings.
pub const MAX_RENDER_APPLICATION_DIRTY_RANGES: usize = 16_384;
pub const MAX_RENDER_APPLICATION_LINES: usize = 16_384;
pub const MAX_RENDER_APPLICATION_CELLS: usize = 4 * 1024 * 1024;
pub const MAX_RENDER_APPLICATION_HYPERLINK_SPANS: usize = 65_536;
pub const MAX_RENDER_APPLICATION_IMAGE_REFERENCES: usize = 4_096;
pub const MAX_RENDER_APPLICATION_SEMANTIC_ZONES: usize = 16_384;
pub const MAX_RENDER_APPLICATION_SEMANTIC_TEXT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES: usize = 1024 * 1024;
pub const MAX_RENDER_APPLICATION_TITLE_BYTES: usize = 64 * 1024;
pub const MAX_RENDER_APPLICATION_WORKING_DIR_BYTES: usize = 64 * 1024;
pub const MAX_RENDER_APPLICATION_SCROLLBACK_ROWS: usize = 10_000_000;

/// Wire-stable identity for one scheduler attempt and its underlying render
/// ledger obligation.
///
/// Every numeric authority is non-zero and non-reusing within its owning
/// process incarnation. The tuple is intentionally redundant: a matching
/// scheduler sequence alone cannot prove a matching connection, coordinator,
/// render generation, or external ledger obligation.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct RenderApplicationToken {
    pub connection_generation: u64,
    pub coordinator_instance: u64,
    pub scheduler_sequence: u64,
    pub attempt: u64,
    pub ledger_instance: u64,
    pub render_generation: u64,
    pub ledger_obligation: u64,
}

impl RenderApplicationToken {
    fn validate(self) -> Result<(), RenderApplicationContractError> {
        if self.connection_generation == 0
            || self.coordinator_instance == 0
            || self.scheduler_sequence == 0
            || self.attempt == 0
            || self.ledger_instance == 0
            || self.render_generation == 0
            || self.ledger_obligation == 0
        {
            return Err(RenderApplicationContractError::ZeroAuthorityIdentity);
        }
        Ok(())
    }
}

/// Never-reused identity of an authoritative client-visible render state.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct RenderStateIdentity {
    pub render_generation: u64,
    pub state_sequence: u64,
}

/// Whether an update advances an exact prior state or replaces it
/// authoritatively.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub enum RenderApplicationKind {
    Delta,
    Snapshot,
}

/// Exact identity repeated by both the server update and client settlement.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct RenderApplicationIdentity {
    pub protocol_version: u16,
    pub token: RenderApplicationToken,
    pub pane_id: PaneId,
    /// `Some` for a delta and `None` for an authoritative snapshot.
    pub base_state: Option<RenderStateIdentity>,
    pub resulting_state: RenderStateIdentity,
    pub kind: RenderApplicationKind,
}

impl RenderApplicationIdentity {
    /// Validate the closed identity contract without consulting ambient
    /// connection or pane state.
    pub fn validate(self) -> Result<(), RenderApplicationContractError> {
        if self.protocol_version != RENDER_APPLICATION_PROTOCOL_VERSION {
            return Err(RenderApplicationContractError::UnsupportedProtocolVersion);
        }
        self.token.validate()?;
        if self.resulting_state.render_generation == 0 {
            return Err(RenderApplicationContractError::ZeroStateIdentity);
        }
        if self.resulting_state.render_generation != self.token.render_generation {
            return Err(RenderApplicationContractError::StateGenerationMismatch);
        }
        match (self.kind, self.base_state) {
            (RenderApplicationKind::Delta, None) => {
                Err(RenderApplicationContractError::DeltaMissingBase)
            }
            (RenderApplicationKind::Snapshot, Some(_)) => {
                Err(RenderApplicationContractError::SnapshotHasBase)
            }
            (RenderApplicationKind::Snapshot, None) => Ok(()),
            (RenderApplicationKind::Delta, Some(base)) => {
                if base.render_generation != self.resulting_state.render_generation {
                    return Err(RenderApplicationContractError::StateGenerationMismatch);
                }
                if base.state_sequence >= self.resulting_state.state_sequence {
                    return Err(RenderApplicationContractError::NonAdvancingDelta);
                }
                Ok(())
            }
        }
    }
}

/// Bounded retry and deadline information visible to the receiver.
///
/// `remaining_millis` is a duration, not a cross-machine wall-clock instant.
/// The sender retains the authoritative monotonic deadline locally.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct RenderApplicationRetryBudget {
    pub attempt_ordinal: u16,
    pub max_attempts: u16,
    pub remaining_millis: u32,
}

impl RenderApplicationRetryBudget {
    fn validate(self) -> Result<(), RenderApplicationContractError> {
        if self.max_attempts == 0
            || self.max_attempts > MAX_RENDER_APPLICATION_ATTEMPTS
            || self.attempt_ordinal == 0
            || self.attempt_ordinal > self.max_attempts
            || self.remaining_millis == 0
        {
            return Err(RenderApplicationContractError::InvalidRetryBudget);
        }
        Ok(())
    }
}

/// Explicit state for an optional component of an atomic render application.
///
/// `Unchanged` is semantically different from a missing or truncated field:
/// the enum tag must be present on the wire.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub enum RenderComponentUpdate<T> {
    Unchanged,
    Replace(T),
}

/// Complete application unit sent by the server.
///
/// Image payload hydration referenced by `surface.bonus_lines` is part of
/// applying this unit; the client must not ACK while any referenced image,
/// semantic-zone replacement, palette change, or alert remains unapplied.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct RenderApplicationUpdateV1 {
    pub identity: RenderApplicationIdentity,
    pub retry_budget: RenderApplicationRetryBudget,
    pub surface: GetPaneRenderChangesResponse,
    pub semantic_zones: RenderComponentUpdate<GetSemanticZonesResponse>,
    pub palette: RenderComponentUpdate<SetPalette>,
    pub alerts: Vec<NotifyAlert>,
}

/// Authoritative v2 render application carried only by PDU 84.
///
/// PDU 79 remains bound to [`RenderApplicationUpdateV1`]. Assigning v2 a new
/// identifier makes old/new schema dispatch unambiguous before varbincode sees
/// the payload.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct RenderApplicationUpdate {
    pub identity: RenderApplicationIdentity,
    pub retry_budget: RenderApplicationRetryBudget,
    pub surface: GetPaneRenderChangesResponse,
    pub semantic_zones: RenderComponentUpdate<GetSemanticZonesResponse>,
    pub palette: RenderComponentUpdate<SetPalette>,
    pub alerts: Vec<NotifyAlert>,
    pub connection_identity: RenderConnectionIdentity,
}

impl RenderApplicationUpdate {
    /// Return the aggregate UTF-8 bytes retained by alert string fields.
    #[must_use]
    pub fn alert_text_bytes(&self) -> Option<usize> {
        self.alerts.iter().try_fold(0usize, |total, alert| {
            render_application_alert_text_bytes(&alert.alert)
                .and_then(|bytes| total.checked_add(bytes))
        })
    }

    /// Validate all fixed authority, pane, component, and bound invariants.
    pub fn validate(&self) -> Result<(), RenderApplicationContractError> {
        self.connection_identity.validate()?;
        self.identity.validate()?;
        self.retry_budget.validate()?;
        let pane_id = self.identity.pane_id;
        if self.surface.pane_id != pane_id {
            return Err(RenderApplicationContractError::ComponentPaneMismatch);
        }
        let surface_sequence = u64::try_from(self.surface.seqno)
            .map_err(|_| RenderApplicationContractError::StateSequenceOutOfRange)?;
        if surface_sequence != self.identity.resulting_state.state_sequence {
            return Err(RenderApplicationContractError::ResultingStateMismatch);
        }
        if matches!(
            &self.semantic_zones,
            RenderComponentUpdate::Replace(zones) if zones.pane_id != pane_id
        ) || matches!(
            &self.palette,
            RenderComponentUpdate::Replace(palette) if palette.pane_id != pane_id
        ) || self.alerts.iter().any(|alert| alert.pane_id != pane_id)
        {
            return Err(RenderApplicationContractError::ComponentPaneMismatch);
        }
        if self.identity.kind == RenderApplicationKind::Snapshot {
            if matches!(&self.semantic_zones, RenderComponentUpdate::Unchanged) {
                return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                    component: RenderApplicationComponent::SemanticZones,
                });
            }
            if matches!(&self.palette, RenderComponentUpdate::Unchanged) {
                return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                    component: RenderApplicationComponent::Palette,
                });
            }
        }
        if self.alerts.len() > MAX_RENDER_APPLICATION_ALERTS {
            return Err(RenderApplicationContractError::TooManyAlerts);
        }
        let alert_text_bytes = self.alert_text_bytes();
        let Some(alert_text_bytes) = alert_text_bytes else {
            return Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Alerts,
                requested: u64::MAX,
                limit: u64::try_from(MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES)
                    .unwrap_or(u64::MAX),
            });
        };
        if alert_text_bytes > MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES {
            return Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Alerts,
                requested: u64::try_from(alert_text_bytes).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES)
                    .unwrap_or(u64::MAX),
            });
        }
        let mut has_unseen_output = false;
        let mut has_progress = false;
        for alert in &self.alerts {
            let duplicate_state_alert = match &alert.alert {
                Alert::OutputSinceFocusLost => std::mem::replace(&mut has_unseen_output, true),
                Alert::Progress(_) => std::mem::replace(&mut has_progress, true),
                // Palette state is carried by the atomic component and the
                // client emits exactly one notification after installing it.
                // Accepting a wire alert here would either duplicate that
                // notification or announce a change with no replacement.
                Alert::PaletteChanged => true,
                _ => false,
            };
            if duplicate_state_alert {
                return Err(RenderApplicationContractError::DuplicateStateAlert);
            }
        }

        if self.surface.dirty_lines.len() > MAX_RENDER_APPLICATION_DIRTY_RANGES {
            return Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Lines,
                requested: u64::try_from(self.surface.dirty_lines.len()).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX_RENDER_APPLICATION_DIRTY_RANGES).unwrap_or(u64::MAX),
            });
        }
        if self.surface.title.len() > MAX_RENDER_APPLICATION_TITLE_BYTES {
            return Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Title,
                requested: u64::try_from(self.surface.title.len()).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX_RENDER_APPLICATION_TITLE_BYTES).unwrap_or(u64::MAX),
            });
        }
        if let Some(working_dir) = &self.surface.working_dir {
            let requested = working_dir.url.as_str().len();
            if requested > MAX_RENDER_APPLICATION_WORKING_DIR_BYTES {
                return Err(RenderApplicationContractError::ResourceLimitExceeded {
                    resource: RenderApplicationResource::WorkingDirectory,
                    requested: u64::try_from(requested).unwrap_or(u64::MAX),
                    limit: u64::try_from(MAX_RENDER_APPLICATION_WORKING_DIR_BYTES)
                        .unwrap_or(u64::MAX),
                });
            }
        }

        let dimensions = self.surface.dimensions;
        let viewport_cells = dimensions
            .cols
            .checked_mul(dimensions.viewport_rows)
            .ok_or(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Dimensions,
                requested: u64::MAX,
                limit: u64::try_from(MAX_RENDER_APPLICATION_CELLS).unwrap_or(u64::MAX),
            })?;
        if viewport_cells > MAX_RENDER_APPLICATION_CELLS {
            return Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Dimensions,
                requested: u64::try_from(viewport_cells).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX_RENDER_APPLICATION_CELLS).unwrap_or(u64::MAX),
            });
        }
        if dimensions.scrollback_rows > MAX_RENDER_APPLICATION_SCROLLBACK_ROWS {
            return Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Lines,
                requested: u64::try_from(dimensions.scrollback_rows).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX_RENDER_APPLICATION_SCROLLBACK_ROWS).unwrap_or(u64::MAX),
            });
        }
        let history_rows = dimensions
            .physical_top
            .checked_sub(dimensions.scrollback_top)
            .and_then(|rows| usize::try_from(rows).ok());
        let viewport_end = isize::try_from(dimensions.viewport_rows)
            .ok()
            .and_then(|rows| dimensions.physical_top.checked_add(rows));
        let Some(history_rows) = history_rows else {
            return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::Dimensions,
            });
        };
        let Some(viewport_end) = viewport_end else {
            return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::Dimensions,
            });
        };
        if dimensions.cols == 0
            || dimensions.viewport_rows == 0
            || dimensions.scrollback_rows < dimensions.viewport_rows
            || history_rows.checked_add(dimensions.viewport_rows)
                != Some(dimensions.scrollback_rows)
            || self.surface.cursor_position.x > dimensions.cols
            || self.surface.cursor_position.y < dimensions.physical_top
            || self.surface.cursor_position.y >= viewport_end
        {
            return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::Dimensions,
            });
        }

        if self.surface.bonus_lines.line_count() > MAX_RENDER_APPLICATION_LINES {
            return Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Lines,
                requested: u64::try_from(self.surface.bonus_lines.line_count())
                    .unwrap_or(u64::MAX),
                limit: u64::try_from(MAX_RENDER_APPLICATION_LINES).unwrap_or(u64::MAX),
            });
        }
        let line_counts = self
            .surface
            .bonus_lines
            .validate_structure()
            .map_err(|error| {
                let component = match error {
                    SerializedLinesStructureError::HyperlinkLineOutOfRange
                    | SerializedLinesStructureError::HyperlinkCellRangeOutOfRange => {
                        RenderApplicationComponent::Hyperlinks
                    }
                    SerializedLinesStructureError::ImageLineMissing
                    | SerializedLinesStructureError::ImageCellOutOfRange => {
                        RenderApplicationComponent::Images
                    }
                    SerializedLinesStructureError::DuplicateStableRow
                    | SerializedLinesStructureError::CellCountOverflow => {
                        RenderApplicationComponent::Lines
                    }
                };
                RenderApplicationContractError::MalformedSurfaceComponent { component }
            })?;
        for (resource, requested, limit) in [
            (
                RenderApplicationResource::Lines,
                line_counts.lines,
                MAX_RENDER_APPLICATION_LINES,
            ),
            (
                RenderApplicationResource::Cells,
                line_counts.cells,
                MAX_RENDER_APPLICATION_CELLS,
            ),
            (
                RenderApplicationResource::Hyperlinks,
                line_counts.hyperlink_spans,
                MAX_RENDER_APPLICATION_HYPERLINK_SPANS,
            ),
            (
                RenderApplicationResource::Images,
                line_counts.images,
                MAX_RENDER_APPLICATION_IMAGE_REFERENCES,
            ),
        ] {
            if requested > limit {
                return Err(RenderApplicationContractError::ResourceLimitExceeded {
                    resource,
                    requested: u64::try_from(requested).unwrap_or(u64::MAX),
                    limit: u64::try_from(limit).unwrap_or(u64::MAX),
                });
            }
        }

        let supplied_rows = if self.surface.dirty_lines.is_empty()
            && self.identity.kind != RenderApplicationKind::Snapshot
        {
            None
        } else {
            Some(
                self.surface
                    .bonus_lines
                    .stable_rows()
                    .collect::<HashSet<_>>(),
            )
        };
        if self.identity.kind == RenderApplicationKind::Snapshot {
            if dimensions.viewport_rows > MAX_RENDER_APPLICATION_LINES {
                return Err(RenderApplicationContractError::ResourceLimitExceeded {
                    resource: RenderApplicationResource::Lines,
                    requested: u64::try_from(dimensions.viewport_rows).unwrap_or(u64::MAX),
                    limit: u64::try_from(MAX_RENDER_APPLICATION_LINES).unwrap_or(u64::MAX),
                });
            }
            if (dimensions.physical_top..viewport_end).any(|row| {
                supplied_rows
                    .as_ref()
                    .is_none_or(|rows| !rows.contains(&row))
            }) {
                return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                    component: RenderApplicationComponent::Lines,
                });
            }
        }
        let mut dirty_rows = 0usize;
        let mut prior_end = None;
        for range in &self.surface.dirty_lines {
            let Some(span) = range
                .end
                .checked_sub(range.start)
                .and_then(|span| usize::try_from(span).ok())
            else {
                return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                    component: RenderApplicationComponent::Lines,
                });
            };
            if range.is_empty()
                || prior_end.is_some_and(|end| end > range.start)
                || range.start < dimensions.scrollback_top
                || range.end > viewport_end
            {
                return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                    component: RenderApplicationComponent::Lines,
                });
            }
            dirty_rows = dirty_rows
                .checked_add(span)
                .ok_or(RenderApplicationContractError::ResourceLimitExceeded {
                    resource: RenderApplicationResource::Lines,
                    requested: u64::MAX,
                    limit: u64::try_from(MAX_RENDER_APPLICATION_LINES).unwrap_or(u64::MAX),
                })?;
            if dirty_rows > MAX_RENDER_APPLICATION_LINES {
                return Err(RenderApplicationContractError::ResourceLimitExceeded {
                    resource: RenderApplicationResource::Lines,
                    requested: u64::try_from(dirty_rows).unwrap_or(u64::MAX),
                    limit: u64::try_from(MAX_RENDER_APPLICATION_LINES).unwrap_or(u64::MAX),
                });
            }
            if range.clone().any(|row| {
                supplied_rows
                    .as_ref()
                    .is_none_or(|rows| !rows.contains(&row))
            }) {
                return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                    component: RenderApplicationComponent::Lines,
                });
            }
            prior_end = Some(range.end);
        }
        if self.surface.bonus_lines.lines().any(|(row, line)| {
            *row < dimensions.scrollback_top
                || *row >= viewport_end
                || u64::try_from(line.current_seqno())
                    .map_or(true, |seqno| seqno > self.identity.resulting_state.state_sequence)
        }) {
            return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::Lines,
            });
        }

        if let RenderComponentUpdate::Replace(semantic) = &self.semantic_zones {
            if semantic.zones.len() != semantic.zone_texts.len() {
                return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                    component: RenderApplicationComponent::SemanticZones,
                });
            }
            if semantic.zones.len() > MAX_RENDER_APPLICATION_SEMANTIC_ZONES {
                return Err(RenderApplicationContractError::ResourceLimitExceeded {
                    resource: RenderApplicationResource::SemanticZones,
                    requested: u64::try_from(semantic.zones.len()).unwrap_or(u64::MAX),
                    limit: u64::try_from(MAX_RENDER_APPLICATION_SEMANTIC_ZONES)
                        .unwrap_or(u64::MAX),
                });
            }
            let semantic_text_bytes = semantic.zone_texts.iter().try_fold(
                0usize,
                |total, text| total.checked_add(text.len()),
            );
            let Some(semantic_text_bytes) = semantic_text_bytes else {
                return Err(RenderApplicationContractError::ResourceLimitExceeded {
                    resource: RenderApplicationResource::SemanticZones,
                    requested: u64::MAX,
                    limit: u64::try_from(MAX_RENDER_APPLICATION_SEMANTIC_TEXT_BYTES)
                        .unwrap_or(u64::MAX),
                });
            };
            if semantic_text_bytes > MAX_RENDER_APPLICATION_SEMANTIC_TEXT_BYTES {
                return Err(RenderApplicationContractError::ResourceLimitExceeded {
                    resource: RenderApplicationResource::SemanticZones,
                    requested: u64::try_from(semantic_text_bytes).unwrap_or(u64::MAX),
                    limit: u64::try_from(MAX_RENDER_APPLICATION_SEMANTIC_TEXT_BYTES)
                        .unwrap_or(u64::MAX),
                });
            }
            if semantic.zones.iter().any(|zone| {
                zone.start_y < dimensions.scrollback_top
                    || zone.end_y >= viewport_end
                    || zone.start_x >= dimensions.cols
                    || zone.end_x >= dimensions.cols
                    || zone.start_y > zone.end_y
                    || (zone.start_y == zone.end_y && zone.start_x > zone.end_x)
            }) {
                return Err(RenderApplicationContractError::MalformedSurfaceComponent {
                    component: RenderApplicationComponent::SemanticZones,
                });
            }
        }
        Ok(())
    }
}

fn render_application_alert_text_bytes(alert: &Alert) -> Option<usize> {
    match alert {
        Alert::ToastNotification { title, body, .. } => title
            .as_ref()
            .map_or(0, String::len)
            .checked_add(body.len()),
        Alert::IconTitleChanged(title) | Alert::TabTitleChanged(title) => {
            Some(title.as_ref().map_or(0, String::len))
        }
        Alert::WindowTitleChanged(title) => Some(title.len()),
        Alert::SetUserVar { name, value } => name.len().checked_add(value.len()),
        Alert::SetProfileRequested { name } => Some(name.len()),
        Alert::MouseShapeRequested { shape } => Some(shape.len()),
        Alert::ImageAltText { text, .. } => Some(text.len()),
        Alert::Bell
        | Alert::CurrentWorkingDirectoryChanged
        | Alert::PaletteChanged
        | Alert::OutputSinceFocusLost
        | Alert::Progress(_) => Some(0),
    }
}

/// Component whose validation or application failed.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub enum RenderApplicationComponent {
    Surface,
    Lines,
    Hyperlinks,
    Cursor,
    Modes,
    Dimensions,
    Title,
    SemanticZones,
    Images,
    Palette,
    Alerts,
}

/// Resource class used by typed unsupported or bounded-rejection NACKs.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub enum RenderApplicationResource {
    Cells,
    Lines,
    Dimensions,
    Images,
    Hyperlinks,
    SemanticZones,
    Title,
    WorkingDirectory,
    Palette,
    Alerts,
}

/// Stage at which an otherwise well-formed application failed.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub enum RenderApplicationStage {
    Hydrate,
    Validate,
    ApplySurface,
    ApplySemanticZones,
    ApplyImages,
    ApplyPalette,
    ApplyAlerts,
    Commit,
}

/// Typed reason why the client could not apply a complete render unit.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub enum RenderApplicationNackReason {
    BaseMismatch,
    GenerationMismatch,
    MalformedOrIncomplete {
        component: RenderApplicationComponent,
    },
    UnsupportedResource {
        resource: RenderApplicationResource,
    },
    BoundedResourceRejected {
        resource: RenderApplicationResource,
        requested: u64,
        limit: u64,
    },
    ApplicationFailure {
        stage: RenderApplicationStage,
    },
    DetectedGap,
}

/// Closed recovery class for every NACK reason.
#[derive(PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub enum RenderApplicationNackRecovery {
    BoundedRetry,
    AuthoritativeResync,
    Terminal,
}

impl RenderApplicationNackReason {
    #[must_use]
    pub const fn recovery(self) -> RenderApplicationNackRecovery {
        match self {
            Self::BaseMismatch
            | Self::GenerationMismatch
            | Self::MalformedOrIncomplete { .. }
            | Self::DetectedGap => RenderApplicationNackRecovery::AuthoritativeResync,
            Self::ApplicationFailure { .. } => RenderApplicationNackRecovery::BoundedRetry,
            Self::UnsupportedResource { .. } | Self::BoundedResourceRejected { .. } => {
                RenderApplicationNackRecovery::Terminal
            }
        }
    }

    const fn requires_observed_state(self) -> bool {
        matches!(
            self,
            Self::BaseMismatch | Self::GenerationMismatch | Self::DetectedGap
        )
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct RenderApplicationNack {
    pub reason: RenderApplicationNackReason,
    pub observed_state: RenderApplicationObservedState,
}

/// Explicit client-side state observation carried by a NACK.
///
/// `Uninitialized` is distinct from `NotApplicable`: a delta received before
/// any authoritative baseline is a real base mismatch, while an application
/// failure does not claim that a state comparison failed.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub enum RenderApplicationObservedState {
    NotApplicable,
    Uninitialized,
    Applied(RenderStateIdentity),
}

impl RenderApplicationObservedState {
    fn validate(self) -> Result<(), RenderApplicationContractError> {
        if let Self::Applied(state) = self {
            if state.render_generation == 0 {
                return Err(RenderApplicationContractError::InvalidObservedState);
            }
        }
        Ok(())
    }
}

/// Client disposition emitted only after validation and complete application.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub enum RenderApplicationOutcome {
    Applied {
        applied_state: RenderStateIdentity,
    },
    Nack(RenderApplicationNack),
}

/// Application-level settlement sent from client to server.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct RenderApplicationResultV1 {
    pub identity: RenderApplicationIdentity,
    pub outcome: RenderApplicationOutcome,
}

/// Authoritative v2 settlement carried only by PDU 85.
///
/// PDU 80 remains bound to [`RenderApplicationResultV1`].
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, Hash)]
pub struct RenderApplicationResult {
    pub identity: RenderApplicationIdentity,
    pub outcome: RenderApplicationOutcome,
    pub connection_identity: RenderConnectionIdentity,
}

impl RenderApplicationResult {
    /// Validate this settlement against exact retained identity metadata.
    pub fn validate_for_identity(
        self,
        expected: RenderApplicationIdentity,
        expected_connection_identity: RenderConnectionIdentity,
    ) -> Result<(), RenderApplicationContractError> {
        self.connection_identity.validate()?;
        self.identity.validate()?;
        if self.identity != expected || self.connection_identity != expected_connection_identity {
            return Err(RenderApplicationContractError::SettlementIdentityMismatch);
        }
        if let RenderApplicationOutcome::Nack(nack) = self.outcome {
            nack.observed_state.validate()?;
        }
        match self.outcome {
            RenderApplicationOutcome::Applied { applied_state }
                if applied_state != self.identity.resulting_state =>
            {
                Err(RenderApplicationContractError::AppliedStateMismatch)
            }
            RenderApplicationOutcome::Nack(nack)
                if nack.reason.requires_observed_state()
                    && nack.observed_state == RenderApplicationObservedState::NotApplicable =>
            {
                Err(RenderApplicationContractError::NackMissingObservedState)
            }
            RenderApplicationOutcome::Nack(nack)
                if !nack.reason.requires_observed_state()
                    && nack.observed_state != RenderApplicationObservedState::NotApplicable =>
            {
                Err(RenderApplicationContractError::NackHasUnexpectedObservedState)
            }
            RenderApplicationOutcome::Applied { .. } | RenderApplicationOutcome::Nack(_) => Ok(()),
        }
    }

    /// Validate this settlement against the exact update it purports to settle.
    pub fn validate_for(
        self,
        update: &RenderApplicationUpdate,
    ) -> Result<(), RenderApplicationContractError> {
        update.validate()?;
        self.validate_for_identity(update.identity, update.connection_identity)
    }
}

#[derive(Error, PartialEq, Eq, Debug, Clone, Copy)]
pub enum RenderApplicationContractError {
    #[error("unsupported render-application protocol version")]
    UnsupportedProtocolVersion,
    #[error("render connection identity uses a reserved zero stream or session incarnation")]
    ReservedConnectionIdentity,
    #[error("render-application authority identities must be non-zero")]
    ZeroAuthorityIdentity,
    #[error("render state generation identities must be non-zero")]
    ZeroStateIdentity,
    #[error("render state generation does not match the delivery token")]
    StateGenerationMismatch,
    #[error("a render delta requires an exact base state")]
    DeltaMissingBase,
    #[error("an authoritative render snapshot cannot require a base state")]
    SnapshotHasBase,
    #[error("a render delta must advance beyond its exact base state")]
    NonAdvancingDelta,
    #[error("invalid bounded render retry or deadline budget")]
    InvalidRetryBudget,
    #[error("a render component targets a different pane")]
    ComponentPaneMismatch,
    #[error("surface sequence cannot be represented in the wire state identity")]
    StateSequenceOutOfRange,
    #[error("surface sequence does not match the resulting state identity")]
    ResultingStateMismatch,
    #[error("render application carries too many exact alerts")]
    TooManyAlerts,
    #[error("render application repeats a latest-value state alert")]
    DuplicateStateAlert,
    #[error("render application exceeds a hard {resource:?} resource limit")]
    ResourceLimitExceeded {
        resource: RenderApplicationResource,
        requested: u64,
        limit: u64,
    },
    #[error("render application contains a malformed or incomplete {component:?} component")]
    MalformedSurfaceComponent {
        component: RenderApplicationComponent,
    },
    #[error("render settlement identity does not match the in-flight update")]
    SettlementIdentityMismatch,
    #[error("applied state does not match the update result identity")]
    AppliedStateMismatch,
    #[error("this NACK reason requires the client's observed state identity")]
    NackMissingObservedState,
    #[error("this NACK reason does not permit a client state observation")]
    NackHasUnexpectedObservedState,
    #[error("a reported client state observation must have a non-zero render generation")]
    InvalidObservedState,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetLines {
    pub pane_id: PaneId,
    pub lines: Vec<Range<StableRowIndex>>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
struct CellCoordinates {
    line_idx: usize,
    cols: Range<usize>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
struct LineHyperlink {
    link: Hyperlink,
    coords: Vec<CellCoordinates>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SerializedImageCell {
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    // The following fields are taken from termwiz::image::ImageCell
    pub top_left: TextureCoordinate,
    pub bottom_right: TextureCoordinate,
    /// Image::data::hash() for the ImageCell::data field
    pub data_hash: [u8; 32],
    pub z_index: i32,
    pub padding_left: u16,
    pub padding_top: u16,
    pub padding_right: u16,
    pub padding_bottom: u16,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
}

/// What's all this?
/// Cells hold references to Arc<Hyperlink> and it is important to us to
/// maintain identity of the hyperlinks in the individual cells, while also
/// only sending a single copy of the associated URL.
/// This section of code extracts the hyperlinks from the cells and builds
/// up a mapping that can be used to restore the identity when the `lines()`
/// method is called.
#[derive(Deserialize, Serialize, PartialEq, Debug, Default, Clone)]
pub struct SerializedLines {
    lines: Vec<(StableRowIndex, Line)>,
    hyperlinks: Vec<LineHyperlink>,
    images: Vec<SerializedImageCell>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SerializedLinesResourceCounts {
    pub lines: usize,
    pub cells: usize,
    pub hyperlink_spans: usize,
    pub images: usize,
}

/// Reconstituted line payload plus image references awaiting hydration.
pub type ExtractedSerializedLines = (
    Vec<(StableRowIndex, Line)>,
    Vec<SerializedImageCell>,
);

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum SerializedLinesStructureError {
    #[error("serialized lines contain a duplicate stable row")]
    DuplicateStableRow,
    #[error("serialized line cell accounting overflowed")]
    CellCountOverflow,
    #[error("serialized hyperlink references a missing line")]
    HyperlinkLineOutOfRange,
    #[error("serialized hyperlink references an invalid or empty cell range")]
    HyperlinkCellRangeOutOfRange,
    #[error("serialized image references a missing stable row")]
    ImageLineMissing,
    #[error("serialized image references a missing cell")]
    ImageCellOutOfRange,
}

impl SerializedLines {
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn stable_rows(&self) -> impl Iterator<Item = StableRowIndex> + '_ {
        self.lines.iter().map(|(row, _)| *row)
    }

    pub fn lines(&self) -> impl Iterator<Item = &(StableRowIndex, Line)> {
        self.lines.iter()
    }

    /// Validate all internal line, hyperlink, and image references before any
    /// state is mutated, and return fixed resource counts for receiver limits.
    pub fn validate_structure(
        &self,
    ) -> Result<SerializedLinesResourceCounts, SerializedLinesStructureError> {
        let mut line_lengths = HashMap::with_capacity(self.lines.len());
        let mut cells = 0usize;
        for (stable_row, line) in &self.lines {
            if line_lengths.insert(*stable_row, line.len()).is_some() {
                return Err(SerializedLinesStructureError::DuplicateStableRow);
            }
            cells = cells
                .checked_add(line.len())
                .ok_or(SerializedLinesStructureError::CellCountOverflow)?;
        }

        let mut hyperlink_spans = 0usize;
        for hyperlink in &self.hyperlinks {
            for coordinates in &hyperlink.coords {
                let Some((_, line)) = self.lines.get(coordinates.line_idx) else {
                    return Err(SerializedLinesStructureError::HyperlinkLineOutOfRange);
                };
                if coordinates.cols.is_empty() || coordinates.cols.end > line.len() {
                    return Err(SerializedLinesStructureError::HyperlinkCellRangeOutOfRange);
                }
                hyperlink_spans = hyperlink_spans
                    .checked_add(1)
                    .ok_or(SerializedLinesStructureError::CellCountOverflow)?;
            }
        }

        for image in &self.images {
            let Some(line_len) = line_lengths.get(&image.line_idx) else {
                return Err(SerializedLinesStructureError::ImageLineMissing);
            };
            if image.cell_idx >= *line_len {
                return Err(SerializedLinesStructureError::ImageCellOutOfRange);
            }
        }

        Ok(SerializedLinesResourceCounts {
            lines: self.lines.len(),
            cells,
            hyperlink_spans,
            images: self.images.len(),
        })
    }

    /// Reconstitute a structurally validated line payload.
    pub fn extract_data_checked(
        self,
    ) -> Result<ExtractedSerializedLines, SerializedLinesStructureError> {
        self.validate_structure()?;
        Ok(self.extract_data())
    }

    /// Reconsitute hyperlinks or other attributes that were decomposed for
    /// serialization, and return the line data.
    pub fn extract_data(self) -> (Vec<(StableRowIndex, Line)>, Vec<SerializedImageCell>) {
        let lines = if self.hyperlinks.is_empty() {
            self.lines
        } else {
            let mut lines = self.lines;

            for link in self.hyperlinks {
                let url = Arc::new(link.link);

                for coord in link.coords {
                    if let Some((_, line)) = lines.get_mut(coord.line_idx) {
                        if let Some(cells) =
                            line.cells_mut_for_attr_changes_only().get_mut(coord.cols)
                        {
                            for cell in cells {
                                cell.attrs_mut().set_hyperlink(Some(Arc::clone(&url)));
                            }
                        }
                    }
                }
            }

            lines
        };
        (lines, self.images)
    }
}

impl From<Vec<(StableRowIndex, Line)>> for SerializedLines {
    fn from(mut lines: Vec<(StableRowIndex, Line)>) -> Self {
        let mut hyperlinks = vec![];
        let mut images = vec![];

        for (line_idx, (stable_row_idx, line)) in lines.iter_mut().enumerate() {
            let mut current_link: Option<Arc<Hyperlink>> = None;
            let mut current_range = 0..0;

            for (x, cell) in line
                .cells_mut_for_attr_changes_only()
                .iter_mut()
                .enumerate()
            {
                // Unset the hyperlink on the cell, if any, and record that
                // in the hyperlinks data for later restoration.
                if let Some(link) = cell.attrs_mut().hyperlink().map(Arc::clone) {
                    cell.attrs_mut().set_hyperlink(None);
                    match current_link.as_ref() {
                        Some(current) if Arc::ptr_eq(current, &link) => {
                            // Continue the current streak
                            current_range = range_union(current_range, x..x + 1);
                        }
                        Some(prior) => {
                            // It's a different URL, push the current data and start a new one
                            hyperlinks.push(LineHyperlink {
                                link: (**prior).clone(),
                                coords: vec![CellCoordinates {
                                    line_idx,
                                    cols: current_range,
                                }],
                            });
                            current_range = x..x + 1;
                            current_link = Some(link);
                        }
                        None => {
                            // Starting a new streak
                            current_range = x..x + 1;
                            current_link = Some(link);
                        }
                    }
                } else if let Some(link) = current_link.take() {
                    // Wrap up a prior streak
                    hyperlinks.push(LineHyperlink {
                        link: (*link).clone(),
                        coords: vec![CellCoordinates {
                            line_idx,
                            cols: current_range,
                        }],
                    });
                    current_range = 0..0;
                }

                if let Some(cell_images) = cell.attrs().images() {
                    for imcell in cell_images {
                        let (padding_left, padding_top, padding_right, padding_bottom) =
                            imcell.padding();
                        images.push(SerializedImageCell {
                            line_idx: *stable_row_idx,
                            cell_idx: x,
                            top_left: imcell.top_left(),
                            bottom_right: imcell.bottom_right(),
                            z_index: imcell.z_index(),
                            padding_left,
                            padding_top,
                            padding_right,
                            padding_bottom,
                            image_id: imcell.image_id(),
                            placement_id: imcell.placement_id(),
                            data_hash: imcell.image_data().hash(),
                        });
                    }
                }
                cell.attrs_mut().clear_images();
            }
            if let Some(link) = current_link.take() {
                // Wrap up final streak
                hyperlinks.push(LineHyperlink {
                    link: (*link).clone(),
                    coords: vec![CellCoordinates {
                        line_idx,
                        cols: current_range,
                    }],
                });
            }
        }

        Self {
            lines,
            hyperlinks,
            images,
        }
    }
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetLinesResponse {
    pub pane_id: PaneId,
    pub lines: SerializedLines,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct EraseScrollbackRequest {
    pub pane_id: PaneId,
    pub erase_mode: ScrollbackEraseMode,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SearchScrollbackRequest {
    pub pane_id: PaneId,
    pub pattern: mux::pane::Pattern,
    pub range: Range<StableRowIndex>,
    pub limit: Option<u32>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct SearchScrollbackResponse {
    pub results: Vec<mux::pane::SearchResult>,
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetImageCell {
    pub pane_id: PaneId,
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    pub data_hash: [u8; 32],
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct GetImageCellResponse {
    pub pane_id: PaneId,
    pub data: Option<Arc<ImageData>>,
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_frame() {
        let mut encoded = Vec::new();
        encode_raw(0x81, 0x42, b"hello", false, &mut encoded).unwrap();
        assert_eq!(&encoded, b"\x08\x42\x81\x01hello");
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, 0x81);
        assert_eq!(decoded.serial, 0x42);
        assert_eq!(decoded.data, b"hello");
    }

    #[test]
    fn test_frame_lengths() {
        for (serial, target_len) in (1..).zip([128, 247, 256, 65536, 16777216]) {
            let mut payload = Vec::with_capacity(target_len);
            payload.resize(target_len, b'a');
            let mut encoded = Vec::new();
            encode_raw(0x42, serial, payload.as_slice(), false, &mut encoded).unwrap();
            let decoded = decode_raw(encoded.as_slice()).unwrap();
            assert_eq!(decoded.ident, 0x42);
            assert_eq!(decoded.serial, serial);
            assert_eq!(decoded.data, payload);
        }
    }

    #[test]
    fn test_pdu_ping() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 0x40).unwrap();
        assert_eq!(&encoded, &[2, 0x40, 1]);
        assert_eq!(
            DecodedPdu {
                serial: 0x40,
                pdu: Pdu::Ping(Ping {})
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn test_pdu_encode_with_mode_never_disables_compression() {
        let mut encoded = Vec::new();
        let payload = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![b'x'; 512],
        });
        payload
            .encode_with_mode(&mut encoded, 0x51, CompressionMode::Never)
            .unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert!(!decoded.is_compressed);
    }

    #[test]
    fn test_pdu_encode_with_mode_always_forces_compression() {
        let mut encoded = Vec::new();
        let payload = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![b'x'; 512],
        });
        payload
            .encode_with_mode(&mut encoded, 0x52, CompressionMode::Always)
            .unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert!(decoded.is_compressed);
    }

    #[test]
    fn stream_decode() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 0x1).unwrap();
        Pdu::Pong(Pong {}).encode(&mut encoded, 0x2).unwrap();
        assert_eq!(encoded.len(), 6);

        let mut cursor = Cursor::new(encoded.as_slice());
        let mut read_buffer = StreamingPduBuffer::new();

        assert_eq!(
            Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap(),
            Some(DecodedPdu {
                serial: 1,
                pdu: Pdu::Ping(Ping {})
            })
        );
        assert_eq!(
            Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap(),
            Some(DecodedPdu {
                serial: 2,
                pdu: Pdu::Pong(Pong {})
            })
        );
        let err = Pdu::try_read_and_decode(&mut cursor, &mut read_buffer).unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn buffered_frame_len_rejects_oversize_tagged_len_ft_phz7x() {
        // Craft a header whose leb128 tagged_len advertises MAX_PDU_SIZE + 1.
        // buffered_frame_len must Err out immediately, instead of returning
        // Ok(None) and inviting try_read_and_decode to accumulate the full
        // advertised size into memory. See ft-phz7x.
        let oversize = (MAX_PDU_SIZE as u64) + 1;
        let mut header = Vec::new();
        leb128::write::unsigned(&mut header, oversize).unwrap();
        // Feed a buffer that contains only the leb128 header plus a stub
        // byte — not enough to satisfy the advertised length. Pre-fix, this
        // returned Ok(None); post-fix, it returns Err.
        header.push(0);
        let result = buffered_frame_len(&header);
        let err = result.expect_err(
            "buffered_frame_len must reject oversize tagged_len (ft-phz7x); \
             got Ok which means callers would keep accumulating",
        );
        let msg = format!("{err}");
        assert!(
            msg.contains("exceeds maximum") && msg.contains("refusing to accumulate"),
            "ft-phz7x rejection must name the cap and the refusal reason; got {msg:?}",
            msg = msg,
        );

        // Sanity: the same cap boundary at MAX_PDU_SIZE exactly is still
        // considered complete (no early bail) when the buffer is long
        // enough — this ensures we didn't off-by-one the comparison. We
        // only check the length-decode path, not a full PDU: a buffer
        // matching the advertised length should report Some(total_len).
        let boundary = MAX_PDU_SIZE as u64;
        let mut boundary_header = Vec::new();
        leb128::write::unsigned(&mut boundary_header, boundary).unwrap();
        let prefix_len = boundary_header.len();
        // Don't actually allocate 256 MiB — just confirm the path that
        // would reach the "buffer.len() < total_len" check. When we're
        // below total_len we get Ok(None), which is the correct "need
        // more bytes" signal for a legitimate boundary-sized frame.
        let below = buffered_frame_len(&boundary_header).unwrap();
        assert_eq!(
            below, None,
            "boundary-sized frame at the MAX_PDU_SIZE cap must be treated \
             as incomplete (Ok(None)), not rejected; header_len={prefix_len}",
        );
    }

    #[test]
    fn test_pdu_ping_base91() {
        let mut encoded = Vec::new();
        {
            let mut encoder = base91::Base91Encoder::new(&mut encoded);
            Pdu::Ping(Ping {}).encode(&mut encoder, 0x41).unwrap();
        }
        assert_eq!(&encoded, &[60, 67, 75, 65]);
        let decoded = base91::decode(&encoded);
        assert_eq!(
            DecodedPdu {
                serial: 0x41,
                pdu: Pdu::Ping(Ping {})
            },
            Pdu::decode(decoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn test_pdu_pong() {
        let mut encoded = Vec::new();
        Pdu::Pong(Pong {}).encode(&mut encoded, 0x42).unwrap();
        assert_eq!(&encoded, &[2, 0x42, 2]);
        assert_eq!(
            DecodedPdu {
                serial: 0x42,
                pdu: Pdu::Pong(Pong {})
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }

    #[test]
    fn test_bogus_pdu() {
        let mut encoded = Vec::new();
        encode_raw(0xdeadbeef, 0x42, b"hello", false, &mut encoded).unwrap();
        assert_eq!(
            DecodedPdu {
                serial: 0x42,
                pdu: Pdu::Invalid { ident: 0xdeadbeef }
            },
            Pdu::decode(encoded.as_slice()).unwrap()
        );
    }

    // --- encoded_length tests ---

    #[test]
    fn encoded_length_zero() {
        assert_eq!(encoded_length(0), 1);
    }

    #[test]
    fn encoded_length_small() {
        // Values < 128 fit in one byte
        assert_eq!(encoded_length(1), 1);
        assert_eq!(encoded_length(127), 1);
    }

    #[test]
    fn encoded_length_two_bytes() {
        assert_eq!(encoded_length(128), 2);
        assert_eq!(encoded_length(16383), 2);
    }

    #[test]
    fn encoded_length_large() {
        assert_eq!(encoded_length(16384), 3);
        // u64::MAX needs 10 bytes in leb128
        assert_eq!(encoded_length(u64::MAX), 10);
    }

    // --- encode_raw / decode_raw roundtrip tests ---

    #[test]
    fn encode_decode_empty_data() {
        let mut encoded = Vec::new();
        encode_raw(1, 1, b"", false, &mut encoded).unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, 1);
        assert_eq!(decoded.serial, 1);
        assert_eq!(decoded.data, b"");
        assert!(!decoded.is_compressed);
    }

    #[test]
    fn encode_decode_compressed_flag() {
        let mut encoded = Vec::new();
        encode_raw(5, 10, b"payload", true, &mut encoded).unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, 5);
        assert_eq!(decoded.serial, 10);
        assert_eq!(decoded.data, b"payload");
        assert!(decoded.is_compressed);
    }

    #[test]
    fn encode_decode_large_ident_serial() {
        let mut encoded = Vec::new();
        let ident = 0xFFFF;
        let serial = 0xDEAD;
        encode_raw(ident, serial, b"big", false, &mut encoded).unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, ident);
        assert_eq!(decoded.serial, serial);
        assert_eq!(decoded.data, b"big");
    }

    #[test]
    fn encode_raw_as_vec_matches_encode_raw() {
        let ident = 42;
        let serial = 7;
        let data = b"test data";

        let vec_result = encode_raw_as_vec(ident, serial, data, false).unwrap();
        let mut write_result = Vec::new();
        encode_raw(ident, serial, data, false, &mut write_result).unwrap();

        assert_eq!(vec_result, write_result);
    }

    #[test]
    fn pdu_encode_frame_matches_existing_wire_encoding() {
        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 42,
            data: b"generation-bound input".to_vec(),
        });
        let serial = 0x1_0000_0001;
        let direct_frame = pdu
            .encode_frame(serial)
            .expect("encode directly into the owned frame");
        let mut existing_encoding = Vec::new();
        pdu.encode(&mut existing_encoding, serial)
            .expect("encode through the existing writer API");

        assert_eq!(direct_frame, existing_encoding);
        let decoded = Pdu::decode(direct_frame.as_slice()).expect("decode direct frame");
        assert_eq!(decoded.serial, serial);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn retained_pdu_frame_is_uncompressed_bounded_and_roundtrips() {
        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 42,
            data: vec![b'x'; 4_096],
        });
        let retained = pdu.encode_retained_frame(0).expect("encode retained frame");
        let measured = pdu
            .encoded_frame_len_with_mode(0, CompressionMode::Never)
            .expect("measure retained frame");

        assert_eq!(retained.len(), measured);
        let decoded =
            Pdu::decode_retained_frame(retained.as_slice()).expect("decode retained frame");
        assert_eq!(decoded.serial, 0);
        assert_eq!(decoded.pdu, pdu);
    }

    // --- COMPRESSED_MASK tests ---

    #[test]
    fn compressed_mask_is_high_bit() {
        assert_eq!(COMPRESSED_MASK, 1 << 63);
        assert_eq!(COMPRESSED_MASK & 0x7FFF_FFFF_FFFF_FFFF, 0);
    }

    // --- CompressionMode tests ---

    #[test]
    fn compression_mode_debug() {
        assert_eq!(format!("{:?}", CompressionMode::Auto), "Auto");
        assert_eq!(format!("{:?}", CompressionMode::Always), "Always");
        assert_eq!(format!("{:?}", CompressionMode::Never), "Never");
    }

    #[test]
    fn compression_mode_eq() {
        assert_eq!(CompressionMode::Auto, CompressionMode::Auto);
        assert_ne!(CompressionMode::Auto, CompressionMode::Always);
        assert_ne!(CompressionMode::Always, CompressionMode::Never);
    }

    #[test]
    fn compression_mode_clone() {
        let mode = CompressionMode::Always;
        let cloned = mode;
        assert_eq!(mode, cloned);
    }

    // --- serialize / deserialize roundtrips ---

    #[test]
    fn serialize_deserialize_small_uncompressed() {
        // Small data stays uncompressed in Auto mode
        let val: u32 = 42;
        let (data, is_compressed) = serialize(&val).unwrap();
        assert!(!is_compressed, "small data should not be compressed");
        let result: u32 = deserialize(data.as_slice(), false).unwrap();
        assert_eq!(result, val);
    }

    #[test]
    fn serialize_never_mode() {
        // Even large data stays uncompressed with Never mode
        let val: Vec<u8> = vec![0xAA; 512];
        let (data, is_compressed) = serialize_with_mode(&val, CompressionMode::Never).unwrap();
        assert!(!is_compressed);
        let result: Vec<u8> = deserialize(data.as_slice(), false).unwrap();
        assert_eq!(result, val);
    }

    #[test]
    fn serialize_always_mode() {
        let val: Vec<u8> = vec![0xBB; 512];
        let (data, is_compressed) = serialize_with_mode(&val, CompressionMode::Always).unwrap();
        assert!(is_compressed);
        let result: Vec<u8> = deserialize(data.as_slice(), true).unwrap();
        assert_eq!(result, val);
    }

    #[test]
    fn serialize_auto_mode_large_data() {
        // Repetitive large data should compress well
        let val: Vec<u8> = vec![0xCC; 4096];
        let (data, is_compressed) = serialize_with_mode(&val, CompressionMode::Auto).unwrap();
        // Auto may or may not compress depending on ratio, but roundtrip must work
        let result: Vec<u8> = deserialize(data.as_slice(), is_compressed).unwrap();
        assert_eq!(result, val);
    }

    // --- InputSerial tests ---

    #[test]
    fn input_serial_empty() {
        let empty = InputSerial::empty();
        assert_eq!(format!("{:?}", empty), "InputSerial(0)");
    }

    #[test]
    fn input_serial_now_nonzero() {
        let now = InputSerial::now();
        // Should be a large number of milliseconds since epoch
        assert_ne!(format!("{:?}", now), "InputSerial(0)");
    }

    #[test]
    fn input_serial_now_is_strictly_monotonic_within_one_process() {
        let first = InputSerial::now();
        let second = InputSerial::now();
        assert!(second > first);
    }

    #[test]
    fn input_serial_elapsed_millis() {
        let before = InputSerial::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let elapsed = before.elapsed_millis();
        assert!(
            elapsed >= 5,
            "{}",
            format!("elapsed should be at least ~10ms, got {}", elapsed)
        );
    }

    #[test]
    fn input_serial_elapsed_millis_saturates_when_serial_in_future() {
        // Remote host with clock skew (or an adversarial payload) can produce an
        // InputSerial whose millis value is greater than the local clock. The
        // naive `now - self` panics on u64 underflow in debug; saturate to 0.
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(60 * 60 * 24);
        let skewed: InputSerial = future.into();
        assert_eq!(skewed.elapsed_millis(), 0);
    }

    #[test]
    fn input_serial_from_system_time() {
        let time = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(12345);
        let serial: InputSerial = time.into();
        assert_eq!(format!("{:?}", serial), "InputSerial(12345)");
    }

    #[test]
    fn input_serial_from_pre_epoch_system_time_saturates_to_empty() {
        let time = std::time::SystemTime::UNIX_EPOCH - std::time::Duration::from_millis(1);
        let serial: InputSerial = time.into();
        assert_eq!(serial, InputSerial::empty());
    }

    #[test]
    fn input_serial_from_epoch_duration_saturates_on_u64_overflow() {
        let overflowing = std::time::Duration::from_millis(u64::MAX)
            .checked_add(std::time::Duration::from_millis(1))
            .expect("duration addition should fit");
        assert_eq!(
            input_serial_from_epoch_duration(overflowing),
            InputSerial(u64::MAX)
        );
    }

    #[test]
    fn input_serial_clone_eq_ord() {
        let a = InputSerial::empty();
        let b = a;
        assert_eq!(a, b);
        let c = InputSerial::now();
        assert!(c > a);
    }

    // --- Pdu::is_user_input tests ---

    #[test]
    fn pdu_is_user_input_true_variants() {
        assert!(Pdu::WriteToPane(WriteToPane {
            pane_id: 0,
            data: vec![]
        })
        .is_user_input());
        assert!(Pdu::SendPaste(SendPaste {
            pane_id: 0,
            data: String::new()
        })
        .is_user_input());
        assert!(Pdu::Resize(Resize {
            containing_tab_id: 0,
            pane_id: 0,
            size: TerminalSize::default(),
        })
        .is_user_input());
    }

    #[test]
    fn pdu_is_user_input_false_variants() {
        assert!(!Pdu::Ping(Ping {}).is_user_input());
        assert!(!Pdu::Pong(Pong {}).is_user_input());
        assert!(!Pdu::ListPanes(ListPanes {}).is_user_input());
        assert!(!Pdu::GetCodecVersion(GetCodecVersion {}).is_user_input());
        assert!(!Pdu::GetTlsCreds(GetTlsCreds {}).is_user_input());
        assert!(!Pdu::Invalid { ident: 99 }.is_user_input());
    }

    // --- Pdu::pdu_name tests ---

    #[test]
    fn pdu_name_known_variants() {
        assert_eq!(Pdu::Ping(Ping {}).pdu_name(), "Ping");
        assert_eq!(Pdu::Pong(Pong {}).pdu_name(), "Pong");
        assert_eq!(Pdu::ListPanes(ListPanes {}).pdu_name(), "ListPanes");
        assert_eq!(
            Pdu::GetCodecVersion(GetCodecVersion {}).pdu_name(),
            "GetCodecVersion"
        );
        assert_eq!(
            Pdu::UnitResponse(UnitResponse {}).pdu_name(),
            "UnitResponse"
        );
        assert_eq!(
            Pdu::ErrorResponse(ErrorResponse { reason: "x".into() }).pdu_name(),
            "ErrorResponse"
        );
    }

    #[test]
    fn pdu_name_invalid() {
        assert_eq!(Pdu::Invalid { ident: 0 }.pdu_name(), "Invalid");
    }

    // --- Pdu::pane_id tests ---

    #[test]
    fn pdu_pane_id_some() {
        assert_eq!(
            Pdu::PaneRemoved(PaneRemoved { pane_id: 42 }).pane_id(),
            Some(42)
        );
        assert_eq!(
            Pdu::PaneFocused(PaneFocused { pane_id: 7 }).pane_id(),
            Some(7)
        );
    }

    #[test]
    fn pdu_pane_id_none() {
        assert_eq!(Pdu::Ping(Ping {}).pane_id(), None);
        assert_eq!(Pdu::Pong(Pong {}).pane_id(), None);
        assert_eq!(Pdu::Invalid { ident: 0 }.pane_id(), None);
    }

    // --- Pdu encode/decode roundtrips for additional variants ---

    #[test]
    fn pdu_roundtrip_error_response() {
        let mut buf = Vec::new();
        let pdu = Pdu::ErrorResponse(ErrorResponse {
            reason: "something went wrong".into(),
        });
        pdu.encode(&mut buf, 100).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 100);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_unit_response() {
        let mut buf = Vec::new();
        let pdu = Pdu::UnitResponse(UnitResponse {});
        pdu.encode(&mut buf, 200).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 200);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_get_codec_version() {
        let mut buf = Vec::new();
        let pdu = Pdu::GetCodecVersion(GetCodecVersion {});
        pdu.encode(&mut buf, 300).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 300);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_write_to_pane() {
        let mut buf = Vec::new();
        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 5,
            data: b"hello world".to_vec(),
        });
        pdu.encode(&mut buf, 400).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 400);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_send_paste() {
        let mut buf = Vec::new();
        let pdu = Pdu::SendPaste(SendPaste {
            pane_id: 3,
            data: "clipboard text".into(),
        });
        pdu.encode(&mut buf, 500).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 500);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_kill_pane() {
        let mut buf = Vec::new();
        let pdu = Pdu::KillPane(KillPane { pane_id: 99 });
        pdu.encode(&mut buf, 600).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 600);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_pane_removed() {
        let mut buf = Vec::new();
        let pdu = Pdu::PaneRemoved(PaneRemoved { pane_id: 42 });
        pdu.encode(&mut buf, 700).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 700);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_tab_resized() {
        let mut buf = Vec::new();
        let pdu = Pdu::TabResized(TabResized { tab_id: 11 });
        pdu.encode(&mut buf, 800).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 800);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_pane_focused() {
        let mut buf = Vec::new();
        let pdu = Pdu::PaneFocused(PaneFocused { pane_id: 77 });
        pdu.encode(&mut buf, 900).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 900);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_rename_workspace() {
        let mut buf = Vec::new();
        let pdu = Pdu::RenameWorkspace(RenameWorkspace {
            old_workspace: "old".into(),
            new_workspace: "new".into(),
        });
        pdu.encode(&mut buf, 1000).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 1000);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn topology_capability_negotiation_is_explicit_and_preserves_unknown_bits() {
        let offered = TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits() | (1_u64 << 63),
        );
        let negotiated = offered.intersection(TopologyCapabilities::SERVER_SUPPORTED);

        assert_eq!(
            negotiated,
            TopologyCapabilities::FENCED_SNAPSHOT_V1,
            "unknown capability bits must not be implicitly accepted",
        );
        assert!(offered.contains(TopologyCapabilities::FENCED_SNAPSHOT_V1));
        assert!(!TopologyCapabilities::NONE.contains(offered));
        assert_eq!(offered.bits(), (1_u64 << 63) | 1);
    }

    fn ordered_window_foundation_capabilities() -> TopologyCapabilities {
        TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        )
    }

    fn ordered_window_all_capabilities() -> TopologyCapabilities {
        TopologyCapabilities::from_bits(
            ordered_window_foundation_capabilities().bits()
                | TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(),
        )
    }

    fn sample_ordered_window() -> OrderedWindowStateV1 {
        OrderedWindowStateV1 {
            window_id: RemoteWindowId::new(u64::from(u32::MAX) + 17),
            order_revision: WindowOrderRevision::new(7),
            ordered_tab_ids: vec![
                RemoteTabId::new(u64::from(u32::MAX) + 31),
                RemoteTabId::new(u64::from(u32::MAX) + 37),
                RemoteTabId::new(u64::from(u32::MAX) + 41),
            ],
            active_tab_id: Some(RemoteTabId::new(u64::from(u32::MAX) + 37)),
        }
    }

    fn empty_pane_list() -> ListPanesResponse {
        ListPanesResponse {
            tabs: Vec::new(),
            tab_titles: Vec::new(),
            window_titles: HashMap::new(),
        }
    }

    fn sample_reorder_window_tabs_v1() -> ReorderWindowTabsV1 {
        let window = sample_ordered_window();
        ReorderWindowTabsV1 {
            protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: DomainBindingId::from_bytes([0x11; 16]),
            stream_id: TopologyStreamId::from_bytes([0x22; 16]),
            session_incarnation: MuxSessionIncarnation::from_bytes([0x33; 16]),
            window_id: window.window_id,
            expected_order_revision: window.order_revision,
            desired_tab_ids: window.ordered_tab_ids,
            desired_active_tab_id: window.active_tab_id,
            mutation_id: WindowOrderMutationId::new([0x44; 16], 9),
            digest: WindowReorderDigest::ZERO,
        }
        .with_computed_digest()
    }

    fn encode_reorder_window_tabs_unchecked(
        request: &ReorderWindowTabsV1,
        serial: u64,
    ) -> Vec<u8> {
        let (payload, compressed) = serialize_with_mode(
            &(
                request.protocol_version,
                request.domain_binding_id,
                request.stream_id,
                request.session_incarnation,
                request.window_id,
                request.expected_order_revision,
                &request.desired_tab_ids,
                request.desired_active_tab_id,
                request.mutation_id,
                request.digest,
            ),
            CompressionMode::Never,
        )
        .expect("unchecked reorder wire tuple should serialize");
        assert!(!compressed);
        let mut frame = Vec::new();
        encode_raw(88, serial, &payload, false, &mut frame)
            .expect("unchecked reorder wire tuple should frame");
        frame
    }

    fn encode_window_order_event_unchecked(event: &WindowOrderEventV1, serial: u64) -> Vec<u8> {
        let mut section = Vec::new();
        let mut serializer = varbincode::Serializer::new(&mut section);
        event
            .windows
            .serialize(&mut serializer)
            .expect("unchecked ordered-window section should serialize");
        let (payload, compressed) = serialize_with_mode(
            &(
                event.protocol_version,
                event.stream_id,
                event.session_incarnation,
                event.topology_revision,
                &section,
            ),
            CompressionMode::Never,
        )
        .expect("unchecked window-order event tuple should serialize");
        assert!(!compressed);
        let mut frame = Vec::new();
        encode_raw(90, serial, &payload, false, &mut frame)
            .expect("unchecked window-order event tuple should frame");
        frame
    }

    fn sample_ordered_window_pdus() -> Vec<(u64, Pdu)> {
        let reorder = sample_reorder_window_tabs_v1();
        let window = sample_ordered_window();
        let commit = WindowOrderCommitV1 {
            topology_revision: TopologyRevision::new(12),
            window: window.clone(),
        };
        vec![
            (
                86,
                Pdu::ListPanesOrderedV1(ListPanesOrderedV1 {
                    protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
                    supported: ordered_window_all_capabilities(),
                    required: ordered_window_foundation_capabilities(),
                }),
            ),
            (
                87,
                Pdu::ListPanesOrderedV1Response(ListPanesOrderedV1Response {
                    protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
                    negotiated: ordered_window_foundation_capabilities(),
                    stream_id: reorder.stream_id,
                    outcome: ListPanesOrderedV1Outcome::Snapshot(OrderedPaneSnapshotV1 {
                        session_incarnation: reorder.session_incarnation,
                        topology_revision: TopologyRevision::new(11),
                        panes: empty_pane_list(),
                        ordered_windows: vec![window.clone()],
                    }),
                }),
            ),
            (88, Pdu::ReorderWindowTabsV1(reorder.clone())),
            (
                89,
                Pdu::ReorderWindowTabsV1Response(ReorderWindowTabsV1Response {
                    protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
                    stream_id: reorder.stream_id,
                    session_incarnation: reorder.session_incarnation,
                    mutation_id: reorder.mutation_id,
                    request_digest: reorder.digest,
                    outcome: ReorderWindowTabsV1Outcome::Applied(commit),
                }),
            ),
            (
                90,
                Pdu::WindowOrderEventV1(WindowOrderEventV1 {
                    protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
                    stream_id: reorder.stream_id,
                    session_incarnation: reorder.session_incarnation,
                    topology_revision: TopologyRevision::new(12),
                    windows: vec![window],
                }),
            ),
        ]
    }

    #[test]
    fn ordered_window_v1_capabilities_are_known_but_not_advertised() {
        assert_eq!(
            TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
            1 << 1
        );
        assert_eq!(
            TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(),
            1 << 2
        );
        assert_eq!(
            TopologyCapabilities::SERVER_SUPPORTED,
            TopologyCapabilities::FENCED_SNAPSHOT_V1,
            "codec knowledge must not activate ordered-window runtime support"
        );
        assert!(ordered_window_all_capabilities().validate().is_ok());
        assert_eq!(
            TopologyCapabilities::WINDOW_REORDER_CAS_V1.validate(),
            Err(TopologyCapabilitiesError::ReorderCasWithoutOrderedStream {
                bits: 1 << 2,
            })
        );

        let (payload, compressed) = serialize_with_mode(
            &(
                ORDERED_WINDOW_PROTOCOL_VERSION,
                TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(),
                TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(),
            ),
            CompressionMode::Never,
        )
        .expect("raw malformed capability tuple should serialize");
        assert!(!compressed);
        let mut frame = Vec::new();
        encode_raw(86, 1, &payload, false, &mut frame)
            .expect("malformed capability tuple should frame");
        let error = Pdu::decode(frame.as_slice())
            .expect_err("bit2 without bit1 must fail during wire decode");
        assert!(
            format!("{error:#}").contains("without ORDERED_WINDOW_STREAM_V1"),
            "unexpected capability rejection: {error:#}",
            error = error,
        );
    }

    #[test]
    fn ordered_window_v1_pdus_freeze_ids_and_roundtrip_in_every_mode() {
        assert_eq!(<ListPanesOrderedV1 as PduWireIdent>::IDENT, 86);
        assert_eq!(<ListPanesOrderedV1Response as PduWireIdent>::IDENT, 87);
        assert_eq!(<ReorderWindowTabsV1 as PduWireIdent>::IDENT, 88);
        assert_eq!(<ReorderWindowTabsV1Response as PduWireIdent>::IDENT, 89);
        assert_eq!(<WindowOrderEventV1 as PduWireIdent>::IDENT, 90);

        for (expected_ident, pdu) in sample_ordered_window_pdus() {
            for mode in [
                CompressionMode::Auto,
                CompressionMode::Never,
                CompressionMode::Always,
            ] {
                let frame = pdu
                    .encode_frame_with_mode(0x1234, mode)
                    .expect("ordered-window PDU should encode");
                assert_eq!(
                    decode_raw(frame.as_slice())
                        .expect("ordered-window raw frame should decode")
                        .ident,
                    expected_ident
                );
                let decoded = Pdu::decode(frame.as_slice())
                    .expect("ordered-window PDU should validate and decode");
                assert_eq!(decoded.serial, 0x1234);
                assert_eq!(decoded.pdu, pdu);
            }
        }
    }

    #[test]
    fn reorder_response_roundtrips_every_typed_terminal_outcome() {
        let request = sample_reorder_window_tabs_v1();
        let commit = WindowOrderCommitV1 {
            topology_revision: TopologyRevision::new(12),
            window: sample_ordered_window(),
        };
        let outcomes = vec![
            ReorderWindowTabsV1Outcome::Applied(commit.clone()),
            ReorderWindowTabsV1Outcome::Replay(
                WindowReorderTerminalOutcomeV1::Applied(commit.clone()),
            ),
            ReorderWindowTabsV1Outcome::Replay(
                WindowReorderTerminalOutcomeV1::Conflict(commit.clone()),
            ),
            ReorderWindowTabsV1Outcome::Replay(
                WindowReorderTerminalOutcomeV1::StaleIncarnation,
            ),
            ReorderWindowTabsV1Outcome::Replay(
                WindowReorderTerminalOutcomeV1::Malformed,
            ),
            ReorderWindowTabsV1Outcome::Replay(WindowReorderTerminalOutcomeV1::Exhausted),
            ReorderWindowTabsV1Outcome::Conflict(commit),
            ReorderWindowTabsV1Outcome::StaleIncarnation,
            ReorderWindowTabsV1Outcome::Malformed,
            ReorderWindowTabsV1Outcome::Exhausted,
        ];

        for (index, outcome) in outcomes.into_iter().enumerate() {
            let pdu = Pdu::ReorderWindowTabsV1Response(ReorderWindowTabsV1Response {
                protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
                stream_id: request.stream_id,
                session_incarnation: request.session_incarnation,
                mutation_id: request.mutation_id,
                request_digest: request.digest,
                outcome,
            });
            let frame = pdu
                .encode_frame_with_mode(
                    u64::try_from(index).expect("small outcome index fits u64"),
                    CompressionMode::Never,
                )
                .expect("typed reorder outcome should encode");
            assert_eq!(
                Pdu::decode(frame.as_slice())
                    .expect("typed reorder outcome should decode")
                    .pdu,
                pdu
            );
        }
    }

    #[test]
    fn reorder_digest_has_a_golden_grammar_and_excludes_only_stream_id() {
        let request = sample_reorder_window_tabs_v1();
        assert_eq!(
            request.digest.as_bytes(),
            [
                0x19, 0x77, 0xaf, 0x8b, 0x79, 0xde, 0xaf, 0x45,
                0x80, 0x8f, 0xef, 0x59, 0x9b, 0xee, 0x58, 0xe4,
                0xc7, 0xc2, 0xcd, 0xe0, 0x28, 0x6f, 0x2f, 0xdf,
                0xd6, 0x12, 0x0a, 0x0d, 0x2c, 0x4d, 0xde, 0xf2,
            ]
        );

        let mut successor_stream = request.clone();
        successor_stream.stream_id = TopologyStreamId::from_bytes([0x99; 16]);
        assert_eq!(successor_stream.canonical_digest(), request.digest);
        successor_stream
            .validate()
            .expect("stream rotation must preserve idempotent digest validity");

        let mut changed = request.clone();
        changed.expected_order_revision = WindowOrderRevision::new(8);
        assert_ne!(changed.canonical_digest(), request.digest);
        changed = request.clone();
        changed.desired_tab_ids.swap(0, 1);
        assert_ne!(changed.canonical_digest(), request.digest);
        changed = request.clone();
        changed.desired_active_tab_id = changed.desired_tab_ids.first().copied();
        assert_ne!(changed.canonical_digest(), request.digest);
        changed = request.clone();
        changed.mutation_id.sequence += 1;
        assert_ne!(changed.canonical_digest(), request.digest);
        changed = request.clone();
        changed.session_incarnation = MuxSessionIncarnation::from_bytes([0x55; 16]);
        assert_ne!(changed.canonical_digest(), request.digest);
        changed = request.clone();
        changed.domain_binding_id = DomainBindingId::from_bytes([0x66; 16]);
        assert_ne!(changed.canonical_digest(), request.digest);
        changed = request.clone();
        changed.window_id = RemoteWindowId::new(request.window_id.get() + 1);
        assert_ne!(changed.canonical_digest(), request.digest);
        changed = request.clone();
        changed.mutation_id.namespace = [0x77; 16];
        assert_ne!(changed.canonical_digest(), request.digest);
        changed = request;
        changed.protocol_version += 1;
        assert_ne!(changed.canonical_digest(), changed.digest);
    }

    #[test]
    fn ordered_window_v1_rejects_zero_duplicate_and_impossible_identities() {
        let valid = sample_reorder_window_tabs_v1();
        let mut zero_window = valid.clone();
        zero_window.window_id = RemoteWindowId::new(0);
        zero_window = zero_window.with_computed_digest();

        let mut duplicate_tab = valid.clone();
        duplicate_tab.desired_tab_ids[1] = duplicate_tab.desired_tab_ids[0];
        duplicate_tab.desired_active_tab_id = Some(duplicate_tab.desired_tab_ids[0]);
        duplicate_tab = duplicate_tab.with_computed_digest();

        let mut zero_tab = valid.clone();
        zero_tab.desired_tab_ids[0] = RemoteTabId::new(0);
        zero_tab = zero_tab.with_computed_digest();

        let mut missing_active = valid.clone();
        missing_active.desired_active_tab_id = None;
        missing_active = missing_active.with_computed_digest();

        let mut foreign_active = valid.clone();
        foreign_active.desired_active_tab_id = Some(RemoteTabId::new(999_999));
        foreign_active = foreign_active.with_computed_digest();

        for (name, malformed) in [
            ("zero window", zero_window),
            ("duplicate tab", duplicate_tab),
            ("zero tab", zero_tab),
            ("missing active", missing_active),
            ("foreign active", foreign_active),
        ] {
            assert!(
                Pdu::ReorderWindowTabsV1(malformed.clone())
                    .encode_frame_with_mode(7, CompressionMode::Never)
                    .is_err(),
                "{} must fail before sender serialization",
                name,
            );
            let frame = encode_reorder_window_tabs_unchecked(&malformed, 7);
            assert!(
                Pdu::decode(frame.as_slice()).is_err(),
                "{} must fail closed during decode",
                name,
            );
        }

        let mut digest_mismatch = valid;
        digest_mismatch.digest = WindowReorderDigest::from_bytes([0xaa; 32]);
        let frame = encode_reorder_window_tabs_unchecked(&digest_mismatch, 8);
        let error = Pdu::decode(frame.as_slice())
            .expect_err("digest mismatch must fail during decode");
        assert!(format!("{error:#}").contains("digest mismatch"));

        let first = sample_ordered_window();
        let duplicate_window = WindowOrderEventV1 {
            protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
            stream_id: TopologyStreamId::from_bytes([0x77; 16]),
            session_incarnation: MuxSessionIncarnation::from_bytes([0x78; 16]),
            topology_revision: TopologyRevision::new(9),
            windows: vec![first.clone(), first.clone()],
        };
        assert!(
            Pdu::WindowOrderEventV1(duplicate_window.clone())
                .encode_frame_with_mode(9, CompressionMode::Never)
                .is_err(),
            "duplicate windows must fail sender validation"
        );
        let error = Pdu::decode(
            encode_window_order_event_unchecked(&duplicate_window, 9).as_slice(),
        )
        .expect_err("duplicate windows must fail receiver validation");
        assert!(format!("{error:#}").contains("repeats window id"));

        let mut second = first.clone();
        second.window_id = RemoteWindowId::new(first.window_id.get() + 1);
        let duplicate_tab = WindowOrderEventV1 {
            windows: vec![first, second],
            ..duplicate_window
        };
        let error = Pdu::decode(
            encode_window_order_event_unchecked(&duplicate_tab, 10).as_slice(),
        )
        .expect_err("a tab cannot appear in two ordered windows");
        assert!(format!("{error:#}").contains("repeats tab id"));
    }

    #[test]
    fn ordered_window_v1_exact_decoders_reject_trailing_and_truncated_bytes() {
        for mode in [CompressionMode::Never, CompressionMode::Always] {
            for (ident, pdu) in sample_ordered_window_pdus() {
                let frame = match &pdu {
                    Pdu::ListPanesOrderedV1(value) => {
                        encode_authority_payload_with_trailing_schema_byte(
                            ident, 1, value, mode,
                        )
                    }
                    Pdu::ListPanesOrderedV1Response(value) => {
                        encode_authority_payload_with_trailing_schema_byte(
                            ident, 1, value, mode,
                        )
                    }
                    Pdu::ReorderWindowTabsV1(value) => {
                        encode_authority_payload_with_trailing_schema_byte(
                            ident, 1, value, mode,
                        )
                    }
                    Pdu::ReorderWindowTabsV1Response(value) => {
                        encode_authority_payload_with_trailing_schema_byte(
                            ident, 1, value, mode,
                        )
                    }
                    Pdu::WindowOrderEventV1(value) => {
                        encode_authority_payload_with_trailing_schema_byte(
                            ident, 1, value, mode,
                        )
                    }
                    _ => unreachable!("sample contains only ordered-window authority PDUs"),
                };
                let error = Pdu::decode(frame.as_slice())
                    .expect_err("closed ordered-window schema must reject trailing bytes");
                assert!(
                    format!("{error:#}").contains("trailing schema bytes"),
                    "unexpected PDU {} trailing-byte rejection under {:?}: {:#}",
                    ident,
                    mode,
                    error,
                );
            }
        }

        for (_, pdu) in sample_ordered_window_pdus() {
            let mut frame = pdu
                .encode_frame_with_mode(2, CompressionMode::Never)
                .expect("valid ordered-window PDU should frame");
            frame.pop().expect("sample frame is non-empty");
            Pdu::decode(frame.as_slice())
                .expect_err("truncated ordered-window frame must fail closed");
        }
    }

    #[test]
    fn ordered_window_v1_enforces_count_and_byte_limits() {
        let mut max_request = sample_reorder_window_tabs_v1();
        max_request.desired_tab_ids = (1..=MAX_ORDERED_TABS_PER_WINDOW)
            .map(|id| RemoteTabId::new(u64::try_from(id).expect("bounded id fits u64")))
            .collect();
        max_request.desired_active_tab_id = max_request.desired_tab_ids.first().copied();
        max_request = max_request.with_computed_digest();
        let max_pdu = Pdu::ReorderWindowTabsV1(max_request.clone());
        let max_frame = max_pdu
            .encode_frame_with_mode(20, CompressionMode::Never)
            .expect("exact per-window tab limit should encode");
        assert_eq!(
            Pdu::decode(max_frame.as_slice())
                .expect("exact per-window tab limit should decode")
                .pdu,
            max_pdu
        );

        let mut over_request = max_request;
        over_request
            .desired_tab_ids
            .push(RemoteTabId::new(50_000));
        over_request = over_request.with_computed_digest();
        let (payload, compressed) = serialize_with_mode(
            &(
                over_request.protocol_version,
                over_request.domain_binding_id,
                over_request.stream_id,
                over_request.session_incarnation,
                over_request.window_id,
                over_request.expected_order_revision,
                &over_request.desired_tab_ids,
                over_request.desired_active_tab_id,
                over_request.mutation_id,
                over_request.digest,
            ),
            CompressionMode::Never,
        )
        .expect("hostile unbounded reorder tuple should serialize");
        assert!(!compressed);
        let mut frame = Vec::new();
        encode_raw(88, 21, &payload, false, &mut frame)
            .expect("hostile over-limit request should frame");
        let error = Pdu::decode(frame.as_slice())
            .expect_err("over-limit tab vector must fail before allocation");
        assert!(format!("{error:#}").contains("exceeds maximum 4096"));

        let windows_at_limit: Vec<_> = (1..=MAX_ORDERED_WINDOWS_PER_SNAPSHOT)
            .map(|id| OrderedWindowStateV1 {
                window_id: RemoteWindowId::new(
                    u64::try_from(id).expect("bounded window id fits u64"),
                ),
                order_revision: WindowOrderRevision::INITIAL,
                ordered_tab_ids: Vec::new(),
                active_tab_id: None,
            })
            .collect();
        let snapshot_at_limit = Pdu::ListPanesOrderedV1Response(
            ListPanesOrderedV1Response {
                protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
                negotiated: ordered_window_foundation_capabilities(),
                stream_id: TopologyStreamId::from_bytes([0x71; 16]),
                outcome: ListPanesOrderedV1Outcome::Snapshot(OrderedPaneSnapshotV1 {
                    session_incarnation: MuxSessionIncarnation::from_bytes([0x72; 16]),
                    topology_revision: TopologyRevision::new(1),
                    panes: empty_pane_list(),
                    ordered_windows: windows_at_limit.clone(),
                }),
            },
        );
        let frame = snapshot_at_limit
            .encode_frame_with_mode(22, CompressionMode::Never)
            .expect("exact ordered-window count limit should encode");
        assert_eq!(
            Pdu::decode(frame.as_slice())
                .expect("exact ordered-window count limit should decode")
                .pdu,
            snapshot_at_limit
        );

        let mut windows_over_limit = windows_at_limit;
        windows_over_limit.push(OrderedWindowStateV1 {
            window_id: RemoteWindowId::new(50_000),
            order_revision: WindowOrderRevision::INITIAL,
            ordered_tab_ids: Vec::new(),
            active_tab_id: None,
        });
        let mut section = Vec::new();
        let mut section_serializer = varbincode::Serializer::new(&mut section);
        windows_over_limit
            .serialize(&mut section_serializer)
            .expect("hostile unbounded ordered-window section should serialize");
        let (payload, compressed) = serialize_with_mode(
            &(
                ORDERED_WINDOW_PROTOCOL_VERSION,
                TopologyStreamId::from_bytes([0x71; 16]),
                MuxSessionIncarnation::from_bytes([0x72; 16]),
                TopologyRevision::new(2),
                &section,
            ),
            CompressionMode::Never,
        )
        .expect("hostile unbounded window event tuple should serialize");
        assert!(!compressed);
        let mut frame = Vec::new();
        encode_raw(90, 23, &payload, false, &mut frame)
            .expect("hostile over-limit event should frame");
        let error = Pdu::decode(frame.as_slice())
            .expect_err("over-limit window vector must fail before allocation");
        assert!(format!("{error:#}").contains("exceeds maximum 4096"));

        let aggregate_at_limit: Vec<_> = (0..4)
            .map(|window_offset| {
                let first = window_offset * MAX_ORDERED_TABS_PER_WINDOW + 1;
                let tabs: Vec<_> = (first..first + MAX_ORDERED_TABS_PER_WINDOW)
                    .map(|id| {
                        RemoteTabId::new(u64::try_from(id).expect("bounded tab id fits u64"))
                    })
                    .collect();
                OrderedWindowStateV1 {
                    window_id: RemoteWindowId::new(
                        u64::try_from(window_offset + 1)
                            .expect("bounded window id fits u64"),
                    ),
                    order_revision: WindowOrderRevision::new(1),
                    active_tab_id: tabs.first().copied(),
                    ordered_tab_ids: tabs,
                }
            })
            .collect();
        validate_ordered_windows_with_section_limit(
            &aggregate_at_limit,
            false,
            MAX_ORDERED_WINDOW_SECTION_BYTES,
        )
        .expect("exact aggregate tab limit should validate");
        let mut aggregate_over_limit = aggregate_at_limit;
        aggregate_over_limit.push(OrderedWindowStateV1 {
            window_id: RemoteWindowId::new(5),
            order_revision: WindowOrderRevision::new(1),
            ordered_tab_ids: vec![RemoteTabId::new(50_000)],
            active_tab_id: Some(RemoteTabId::new(50_000)),
        });
        assert_eq!(
            validate_ordered_windows_with_section_limit(
                &aggregate_over_limit,
                false,
                MAX_ORDERED_WINDOW_SECTION_BYTES,
            ),
            Err(OrderedWindowProtocolError::TooManyTotalTabs {
                count: MAX_ORDERED_TABS_PER_SNAPSHOT + 1,
                max: MAX_ORDERED_TABS_PER_SNAPSHOT,
            })
        );

        let one_window = [sample_ordered_window()];
        let exact_section_bytes = encoded_ordered_window_section_len(&one_window)
            .expect("sample order section should have a canonical length");
        validate_ordered_windows_with_section_limit(
            &one_window,
            false,
            exact_section_bytes,
        )
        .expect("exact injected section byte limit should validate");
        assert_eq!(
            validate_ordered_windows_with_section_limit(
                &one_window,
                false,
                exact_section_bytes - 1,
            ),
            Err(OrderedWindowProtocolError::OrderSectionTooLarge {
                bytes: exact_section_bytes,
                max: exact_section_bytes - 1,
            })
        );

        let oversized_payload = vec![0_u8; MAX_REORDER_WINDOW_TABS_DECOMPRESSED_BYTES + 1];
        for (compressed_payload, is_compressed) in [
            (oversized_payload.clone(), false),
            (
                zstd::stream::encode_all(
                    oversized_payload.as_slice(),
                    zstd::DEFAULT_COMPRESSION_LEVEL,
                )
                .expect("oversized zero payload should compress"),
                true,
            ),
        ] {
            let mut frame = Vec::new();
            encode_raw(88, 24, &compressed_payload, is_compressed, &mut frame)
                .expect("bounded compression-bomb fixture should frame");
            let error = Pdu::decode(frame.as_slice())
                .expect_err("reorder payload above 512 KiB must fail before decode");
            assert!(
                format!("{error:#}").contains("exceeds maximum 524288"),
                "unexpected 512 KiB rejection: {error:#}",
                error = error,
            );
        }
    }

    #[test]
    fn codec_v51_keeps_v50_legacy_dialect_inert_and_compatible() {
        assert_eq!(CODEC_VERSION, 51);
        assert_eq!(CODEC_VERSION_MIN_SUPPORTED, 46);
        assert_eq!(ORDERED_WINDOW_V1_MIN_CODEC_VERSION, 51);
        assert!(!codec_version_supports_ordered_window_v1(50));
        assert!(codec_version_supports_ordered_window_v1(51));
        assert_eq!(
            check_compat(51, 46, 50, 46).expect("v50 remains in the additive window"),
            CompatDecision::Compatible { agreed: 50 }
        );
        assert_eq!(<ListPanesCoherent as PduWireIdent>::IDENT, 81);
        assert_eq!(<RenderApplicationResult as PduWireIdent>::IDENT, 85);
        assert_eq!(
            TopologyCapabilities::SERVER_SUPPORTED.bits(),
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
        );

        let legacy = Pdu::ListPanesCoherent(ListPanesCoherent {
            supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
        });
        let frame = legacy
            .encode_frame_with_mode(50, CompressionMode::Never)
            .expect("v50 coherent snapshot request should retain its schema");
        assert_eq!(
            decode_raw(frame.as_slice())
                .expect("v50 legacy frame should decode raw")
                .ident,
            81
        );
        assert_eq!(
            Pdu::decode(frame.as_slice())
                .expect("v51 decoder must retain v50 PDU81")
                .pdu,
            legacy
        );
    }

    fn encode_authority_payload_with_trailing_schema_byte<T: Serialize>(
        ident: u64,
        serial: u64,
        value: &T,
        mode: CompressionMode,
    ) -> Vec<u8> {
        let (payload, is_compressed) = serialize_with_mode(&(value, 0xa5_u8), mode)
            .expect("authority payload with trailing byte should serialize");
        let mut frame = Vec::new();
        encode_raw(ident, serial, &payload, is_compressed, &mut frame)
            .expect("authority payload with trailing byte should frame");
        frame
    }

    fn encode_authority_payload_with_truncated_zstd<T: Serialize>(
        ident: u64,
        serial: u64,
        value: &T,
    ) -> Vec<u8> {
        let (uncompressed, is_compressed) = serialize_with_mode(value, CompressionMode::Never)
            .expect("authority payload should serialize");
        assert!(!is_compressed);
        let mut encoder = zstd::Encoder::new(Vec::new(), zstd::DEFAULT_COMPRESSION_LEVEL)
            .expect("checksum-bearing zstd encoder should initialize");
        encoder
            .include_checksum(true)
            .expect("zstd encoder should enable its frame checksum");
        std::io::Write::write_all(&mut encoder, &uncompressed)
            .expect("zstd encoder should receive the complete authority value");
        let mut payload = encoder
            .finish()
            .expect("checksum-bearing authority frame should finish");
        payload
            .pop()
            .expect("checksum-bearing authority payload should not be empty");
        let mut frame = Vec::new();
        encode_raw(ident, serial, &payload, true, &mut frame)
            .expect("truncated compressed authority payload should still frame");
        frame
    }

    fn encode_authority_payload_with_compressed_suffix<T: Serialize>(
        ident: u64,
        serial: u64,
        value: &T,
        suffix: &[u8],
    ) -> Vec<u8> {
        let (mut payload, is_compressed) =
            serialize_with_mode(value, CompressionMode::Always)
                .expect("authority payload should compress");
        assert!(is_compressed);
        payload.extend_from_slice(suffix);
        let mut frame = Vec::new();
        encode_raw(ident, serial, &payload, true, &mut frame)
            .expect("authority payload with compressed suffix should frame");
        frame
    }

    #[test]
    fn topology_authority_pdus_reject_inner_trailing_bytes_in_both_compression_modes() {
        let request = ListPanesCoherent {
            supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
        };
        let response = ListPanesCoherentResponse {
            negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            stream_id: TopologyStreamId::from_bytes([0x61; 16]),
            outcome: ListPanesCoherentOutcome::Unsupported {
                supported: TopologyCapabilities::NONE,
            },
        };
        let event = TopologyEvent {
            stream_id: TopologyStreamId::from_bytes([0x62; 16]),
            revision: TopologyRevision::new(7),
            event: TopologyEventKind::Empty,
        };

        for mode in [CompressionMode::Never, CompressionMode::Always] {
            let malformed_frames = [
                (
                    "ListPanesCoherent",
                    encode_authority_payload_with_trailing_schema_byte(81, 1, &request, mode),
                ),
                (
                    "ListPanesCoherentResponse",
                    encode_authority_payload_with_trailing_schema_byte(82, 2, &response, mode),
                ),
                (
                    "TopologyEvent",
                    encode_authority_payload_with_trailing_schema_byte(83, 3, &event, mode),
                ),
            ];

            for (payload_name, frame) in malformed_frames {
                let err = Pdu::decode(frame.as_slice())
                    .expect_err("topology authority payload must reject trailing schema bytes");
                let message = format!("{err:#}");
                assert!(
                    message.contains(&format!(
                        "{payload_name} payload has trailing schema bytes"
                    )),
                    "unexpected {} rejection under {:?}: {}",
                    payload_name,
                    mode,
                    message,
                );

                let async_err = runtime::block_on(async {
                    let mut reader = runtime::Cursor::new(frame);
                    Pdu::decode_async(&mut reader, None)
                        .await
                        .expect_err(
                            "async topology authority payload must reject trailing schema bytes",
                        )
                });
                let async_message = format!("{async_err:#}");
                assert!(
                    async_message.contains(&format!(
                        "{payload_name} payload has trailing schema bytes"
                    )),
                    "unexpected async {} rejection under {:?}: {}",
                    payload_name,
                    mode,
                    async_message,
                );
            }
        }
    }

    #[test]
    fn every_exact_authority_decode_preserves_the_next_outer_frame() {
        let stream_id = TopologyStreamId::from_bytes([0x63; 16]);
        let authority_pdus = [
            (
                "ListPanesCoherent",
                Pdu::ListPanesCoherent(ListPanesCoherent {
                    supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                    required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                }),
            ),
            (
                "ListPanesCoherentResponse",
                Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                    negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                    stream_id,
                    outcome: ListPanesCoherentOutcome::Unsupported {
                        supported: TopologyCapabilities::NONE,
                    },
                }),
            ),
            (
                "TopologyEvent",
                Pdu::TopologyEvent(TopologyEvent {
                    stream_id,
                    revision: TopologyRevision::new(8),
                    event: TopologyEventKind::PaneAdded { pane_id: 9 },
                }),
            ),
        ];
        let next_outer = Pdu::Ping(Ping {});

        for (authority_index, (authority_name, authority)) in
            IntoIterator::into_iter(authority_pdus).enumerate()
        {
            for (mode_index, mode) in IntoIterator::into_iter([
                CompressionMode::Never,
                CompressionMode::Always,
            ])
            .enumerate()
            {
                let first_serial = 11_u64
                    + u64::try_from(authority_index * 2 + mode_index)
                        .expect("small authority case index must fit u64");
                let next_serial = first_serial + 100;
                let first_frame = authority
                    .encode_frame_with_mode(first_serial, mode)
                    .expect("encode first authority frame");
                let next_frame = next_outer
                    .encode_frame_with_mode(next_serial, CompressionMode::Never)
                    .expect("encode next outer frame");
                let mut concatenated = first_frame.clone();
                concatenated.extend_from_slice(&next_frame);

                let mut sync_reader = Cursor::new(concatenated.clone());
                let decoded = Pdu::decode(&mut sync_reader)
                    .expect("sync decode first authority frame");
                assert_eq!(decoded.serial, first_serial);
                assert_eq!(
                    decoded.pdu, authority,
                    "sync {authority_name} under {mode:?}"
                );
                assert_eq!(
                    usize::try_from(sync_reader.position())
                        .expect("sync cursor position should fit usize"),
                    first_frame.len(),
                    "sync {authority_name} under {mode:?} must stop at its outer boundary"
                );
                let decoded = Pdu::decode(&mut sync_reader)
                    .expect("sync decode preserved next outer frame");
                assert_eq!(decoded.serial, next_serial);
                assert_eq!(decoded.pdu, next_outer);
                assert_eq!(
                    usize::try_from(sync_reader.position())
                        .expect("sync cursor position should fit usize"),
                    concatenated.len()
                );

                let (decoded_first, decoded_second) = runtime::block_on(async {
                    let mut reader = runtime::Cursor::new(concatenated.clone());
                    let first = Pdu::decode_async(&mut reader, None)
                        .await
                        .expect("async decode first authority frame");
                    let second = Pdu::decode_async(&mut reader, None)
                        .await
                        .expect("async decode preserved next outer frame");
                    (first, second)
                });
                assert_eq!(decoded_first.serial, first_serial);
                assert_eq!(
                    decoded_first.pdu, authority,
                    "async {authority_name} under {mode:?}"
                );
                assert_eq!(decoded_second.serial, next_serial);
                assert_eq!(decoded_second.pdu, next_outer);

                let mut buffered = StreamingPduBuffer::from(concatenated);
                let decoded = Pdu::stream_decode(&mut buffered)
                    .expect("stream decode first authority frame")
                    .expect("first authority frame should be complete");
                assert_eq!(decoded.serial, first_serial);
                assert_eq!(
                    decoded.pdu, authority,
                    "stream {authority_name} under {mode:?}"
                );
                assert_eq!(
                    buffered.as_slice(), next_frame.as_slice(),
                    "stream {authority_name} under {mode:?} must preserve the exact next frame"
                );

                let decoded = Pdu::stream_decode(&mut buffered)
                    .expect("stream decode preserved next outer frame")
                    .expect("next outer frame should remain complete");
                assert_eq!(decoded.serial, next_serial);
                assert_eq!(decoded.pdu, next_outer);
                assert!(buffered.is_empty());
            }
        }
    }

    #[test]
    fn compressed_exact_authority_decode_validates_zstd_termination_after_value() {
        let request = ListPanesCoherent {
            supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
        };
        let frame = encode_authority_payload_with_truncated_zstd(81, 13, &request);

        let sync_error = Pdu::decode(frame.as_slice())
            .expect_err("sync authority decode must reject truncated zstd termination");
        assert!(
            format!("{sync_error:#}").contains(
                "validating ListPanesCoherent compressed payload termination"
            ),
            "sync truncated-zstd rejection should come from the post-value EOF probe: {:#}",
            sync_error
        );

        let async_error = runtime::block_on(async {
            let mut reader = runtime::Cursor::new(frame);
            Pdu::decode_async(&mut reader, None)
                .await
                .expect_err("async authority decode must reject truncated zstd termination")
        });
        assert!(
            format!("{async_error:#}").contains(
                "validating ListPanesCoherent compressed payload termination"
            ),
            "async truncated-zstd rejection should come from the post-value EOF probe: {:#}",
            async_error
        );
    }

    #[test]
    fn compressed_exact_authority_decode_rejects_unread_frame_suffixes() {
        let request = ListPanesCoherent {
            supported: TopologyCapabilities::FENCED_SNAPSHOT_V1,
            required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
        };
        let empty_frame = zstd::stream::encode_all(
            std::io::empty(),
            zstd::DEFAULT_COMPRESSION_LEVEL,
        )
        .expect("empty zstd frame should encode");
        let empty_skippable_frame = [
            0x50, 0x2a, 0x4d, 0x18, // skippable-frame magic, little endian
            0x00, 0x00, 0x00, 0x00, // zero-byte payload length
        ];

        for (suffix_name, suffix) in [
            ("empty zstd frame", empty_frame.as_slice()),
            ("empty skippable frame", empty_skippable_frame.as_slice()),
        ] {
            let frame =
                encode_authority_payload_with_compressed_suffix(81, 14, &request, suffix);
            let sync_error = Pdu::decode(frame.as_slice())
                .expect_err("sync authority decode must reject a compressed frame suffix");
            assert!(
                format!("{sync_error:#}")
                    .contains("ListPanesCoherent payload has trailing compressed frame bytes"),
                "unexpected sync rejection for {}: {:#}",
                suffix_name,
                sync_error
            );

            let async_error = runtime::block_on(async {
                let mut reader = runtime::Cursor::new(frame);
                Pdu::decode_async(&mut reader, None)
                    .await
                    .expect_err("async authority decode must reject a compressed frame suffix")
            });
            assert!(
                format!("{async_error:#}")
                    .contains("ListPanesCoherent payload has trailing compressed frame bytes"),
                "unexpected async rejection for {}: {:#}",
                suffix_name,
                async_error
            );
        }
    }

    #[test]
    fn coherent_list_panes_request_and_typed_outcomes_roundtrip() {
        let request = Pdu::ListPanesCoherent(ListPanesCoherent {
            supported: TopologyCapabilities::from_bits(0b101),
            required: TopologyCapabilities::FENCED_SNAPSHOT_V1,
        });
        let mut encoded = Vec::new();
        request
            .encode_with_mode(&mut encoded, 1100, CompressionMode::Never)
            .expect("encode coherent snapshot request");
        let decoded = Pdu::decode(encoded.as_slice()).expect("decode coherent snapshot request");
        assert_eq!(decoded.serial, 1100);
        assert_eq!(decoded.pdu, request);

        let stream_id = TopologyStreamId::from_bytes([0x5a; 16]);
        let outcomes = [
            ListPanesCoherentOutcome::Snapshot(CoherentPaneSnapshot {
                session_incarnation: MuxSessionIncarnation::from_bytes([0xa5; 16]),
                snapshot_revision: TopologyRevision::new(41),
                panes: ListPanesResponse {
                    tabs: Vec::new(),
                    tab_titles: Vec::new(),
                    window_titles: HashMap::new(),
                },
            }),
            ListPanesCoherentOutcome::Contended {
                attempts: 3,
                first_revision: TopologyRevision::new(41),
                last_revision: TopologyRevision::new(47),
            },
            ListPanesCoherentOutcome::RevisionExhausted,
            ListPanesCoherentOutcome::Unsupported {
                supported: TopologyCapabilities::NONE,
            },
        ];

        for (offset, outcome) in outcomes.iter().cloned().enumerate() {
            let response = Pdu::ListPanesCoherentResponse(ListPanesCoherentResponse {
                negotiated: TopologyCapabilities::FENCED_SNAPSHOT_V1,
                stream_id,
                outcome,
            });
            let serial = 1200 + offset as u64;
            let mut encoded = Vec::new();
            response
                .encode_with_mode(&mut encoded, serial, CompressionMode::Never)
                .expect("encode coherent snapshot response");
            let decoded =
                Pdu::decode(encoded.as_slice()).expect("decode coherent snapshot response");
            assert_eq!(decoded.serial, serial);
            assert_eq!(decoded.pdu, response);
        }
    }

    #[test]
    fn every_revision_advancing_topology_event_has_a_wire_roundtrip() {
        let stream_id = TopologyStreamId::from_bytes([0x33; 16]);
        let events = [
            TopologyEventKind::PaneAdded { pane_id: 1 },
            TopologyEventKind::PaneRemoved { pane_id: 2 },
            TopologyEventKind::WindowCreated { window_id: 3 },
            TopologyEventKind::WindowRemoved { window_id: 4 },
            TopologyEventKind::WindowInvalidated { window_id: 5 },
            TopologyEventKind::WindowWorkspaceChanged {
                window_id: 6,
                workspace: Some("workspace".to_string()),
            },
            TopologyEventKind::WindowWorkspaceChanged {
                window_id: 7,
                workspace: None,
            },
            TopologyEventKind::Empty,
            TopologyEventKind::TabAddedToWindow {
                tab_id: 8,
                window_id: 9,
            },
            TopologyEventKind::PaneFocused { pane_id: 10 },
            TopologyEventKind::TabResized { tab_id: 11 },
            TopologyEventKind::TabTitleChanged {
                tab_id: 12,
                title: "tab".to_string(),
            },
            TopologyEventKind::WindowTitleChanged {
                window_id: 13,
                title: "window".to_string(),
            },
            TopologyEventKind::WorkspaceRenamed {
                old_workspace: "before".to_string(),
                new_workspace: "after".to_string(),
            },
        ];

        for (offset, event) in events.iter().cloned().enumerate() {
            let pdu = Pdu::TopologyEvent(TopologyEvent {
                stream_id,
                revision: TopologyRevision::new(offset as u64 + 1),
                event,
            });
            let mut encoded = Vec::new();
            pdu.encode_with_mode(&mut encoded, 0, CompressionMode::Never)
                .expect("encode topology event");
            let decoded = Pdu::decode(encoded.as_slice()).expect("decode topology event");
            assert_eq!(decoded.serial, 0);
            assert_eq!(decoded.pdu, pdu);
        }
    }

    // --- Pdu::encode Invalid should fail ---

    #[test]
    fn pdu_encode_invalid_fails() {
        let mut buf = Vec::new();
        let result = Pdu::Invalid { ident: 0 }.encode(&mut buf, 0);
        assert!(result.is_err());
    }

    // --- stream_decode edge cases ---

    #[test]
    fn stream_decode_empty_buffer() {
        let mut buffer = StreamingPduBuffer::new();
        let result = Pdu::stream_decode(&mut buffer).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn stream_decode_partial_frame() {
        // Just the length byte, no payload
        let mut buffer = StreamingPduBuffer::from(vec![2u8]);
        let result = Pdu::stream_decode(&mut buffer).unwrap();
        assert!(result.is_none());
        // Buffer should be preserved for future reads
        assert_eq!(buffer.as_slice(), &[2u8]);
    }

    #[test]
    fn stream_decode_rejects_complete_frame_with_truncated_inner_body() {
        let mut encoded = Vec::new();
        // tagged_len counts the complete outer frame: serial + ident + one
        // payload byte. The payload byte starts an ErrorResponse string body
        // that advertises five bytes, so the outer frame is complete but the
        // inner varbincode body is malformed.
        leb128::write::unsigned(&mut encoded, 3).unwrap();
        leb128::write::unsigned(&mut encoded, 1).unwrap();
        leb128::write::unsigned(&mut encoded, 0).unwrap();
        encoded.push(5);
        let original = encoded.clone();
        let mut buffer = StreamingPduBuffer::from(encoded);

        let err = Pdu::stream_decode(&mut buffer)
            .expect_err("complete malformed frame must not be treated as partial");
        assert!(
            !format!("{err:#}").is_empty(),
            "malformed complete frame should return a useful error"
        );
        assert_eq!(
            buffer.as_slice(), original.as_slice(),
            "malformed complete frame must remain available for quarantine"
        );
    }

    #[test]
    fn stream_decode_does_not_read_header_leb128_past_declared_frame() {
        let mut encoded = Vec::new();
        // The declared frame body is one byte long and contains only the start
        // of a multi-byte serial leb128. Bytes after that body are a separate
        // frame and must not be borrowed to finish this malformed header.
        leb128::write::unsigned(&mut encoded, 1).unwrap();
        encoded.push(0x80);
        Pdu::Ping(Ping {}).encode(&mut encoded, 7).unwrap();
        let original = encoded.clone();
        let mut buffer = StreamingPduBuffer::from(encoded);

        let err = Pdu::stream_decode(&mut buffer)
            .expect_err("malformed first frame must fail inside its declared bounds");
        let message = format!("{err:#}");
        assert!(
            message.contains("reading PDU serial") && message.contains("reading leb128"),
            "stream_decode should report the truncated serial inside the declared frame; got {message:?}",
            message = message,
        );
        assert!(
            !message.contains("sizes don't make sense"),
            "stream_decode read past the declared frame and misclassified the malformed header: {message:?}",
            message = message,
        );
        assert_eq!(
            buffer.as_slice(), original.as_slice(),
            "malformed frame must remain available for quarantine"
        );
    }

    #[test]
    fn stream_decode_partial_compressed_frame() -> anyhow::Result<()> {
        let mut encoded = Vec::new();
        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![b'A'; 1024],
        });
        pdu.encode_with_mode(&mut encoded, 42, CompressionMode::Always)?;

        let split = (encoded.len() / 2).max(1).min(encoded.len() - 1);
        let prefix = encoded
            .get(..split)
            .context("split prefix must stay inside encoded frame")?;
        let mut partial = StreamingPduBuffer::from(prefix.to_vec());
        let result = Pdu::stream_decode(&mut partial).context("partial compressed decode")?;
        assert!(
            result.is_none(),
            "partial compressed frame should not decode"
        );
        assert_eq!(
            partial.as_slice(), prefix,
            "partial compressed bytes should remain buffered"
        );

        let suffix = encoded
            .get(split..)
            .context("split suffix must stay inside encoded frame")?;
        partial.extend_from_slice(suffix);
        let decoded = Pdu::stream_decode(&mut partial)
            .context("complete compressed decode")?
            .context("compressed frame should decode")?;
        assert_eq!(decoded.serial, 42);
        assert_eq!(decoded.pdu, pdu);
        assert!(partial.is_empty(), "full frame should be consumed");
        Ok(())
    }

    #[test]
    fn stream_decode_consumes_one_frame() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 1).unwrap();
        Pdu::Pong(Pong {}).encode(&mut encoded, 2).unwrap();
        let total_len = encoded.len();
        let mut buffer = StreamingPduBuffer::from(encoded);

        let decoded = Pdu::stream_decode(&mut buffer).unwrap().unwrap();
        assert_eq!(decoded.pdu, Pdu::Ping(Ping {}));
        assert_eq!(decoded.serial, 1);
        // Buffer should still contain the Pong frame
        assert!(buffer.len() < total_len);

        let decoded2 = Pdu::stream_decode(&mut buffer).unwrap().unwrap();
        assert_eq!(decoded2.pdu, Pdu::Pong(Pong {}));
        assert_eq!(decoded2.serial, 2);
        assert!(buffer.is_empty());
    }

    #[test]
    fn stream_buffer_clone_retains_only_logical_unread_bytes() {
        let mut encoded = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut encoded, 1).unwrap();
        Pdu::Pong(Pong {}).encode(&mut encoded, 2).unwrap();
        let mut buffer = StreamingPduBuffer::from(encoded);
        Pdu::stream_decode(&mut buffer)
            .expect("decode clone fixture prefix")
            .expect("clone fixture prefix must be complete");
        assert!(buffer.consumed_prefix_len() > 0);

        let cloned = buffer.clone();
        assert_eq!(cloned.as_slice(), buffer.as_slice());
        assert_eq!(cloned.consumed_prefix_len(), 0);
        assert_eq!(cloned.retained_len(), cloned.len());
        assert!(
            !format!("{cloned:?}").contains("storage"),
            "stream-buffer diagnostics must not expose backing wire bytes"
        );
    }

    #[test]
    fn stream_decode_tiny_frame_bursts_advance_without_prefix_compaction() {
        for frame_count in [32usize, 256, 4_096] {
            let mut encoded = Vec::new();
            for _ in 0..frame_count {
                Pdu::Ping(Ping {})
                    .encode(&mut encoded, 1)
                    .expect("encode tiny burst frame");
            }
            encoded.shrink_to_fit();
            let initial_capacity = encoded.capacity();
            let mut buffer = StreamingPduBuffer::from(encoded);

            for decoded_count in 0..frame_count {
                let decoded = Pdu::stream_decode(&mut buffer)
                    .expect("decode tiny burst frame")
                    .expect("tiny burst frame must be complete");
                assert_eq!(decoded.serial, 1);
                assert_eq!(decoded.pdu, Pdu::Ping(Ping {}));
                if decoded_count + 1 != frame_count {
                    assert!(buffer.consumed_prefix_len() > 0);
                }
            }

            assert!(buffer.is_empty());
            assert_eq!(buffer.capacity(), initial_capacity);
            assert_eq!(
                buffer.stats(),
                StreamingPduBufferStats {
                    peak_capacity: initial_capacity,
                    ..StreamingPduBufferStats::default()
                },
                "decoding {frame_count} coalesced tiny frames must not memmove or grow the buffer"
            );
        }
    }

    #[test]
    fn stream_buffer_compacts_once_after_consumed_prefix_dominates_suffix() {
        const TOTAL_FRAMES: usize = 35_000;
        const CONSUMED_FRAMES: usize = 25_000;

        let mut encoded = Vec::new();
        for _ in 0..TOTAL_FRAMES {
            Pdu::Ping(Ping {})
                .encode(&mut encoded, 1)
                .expect("encode compaction fixture frame");
        }
        encoded.shrink_to_fit();
        let mut buffer = StreamingPduBuffer::from(encoded);

        for _ in 0..CONSUMED_FRAMES {
            Pdu::stream_decode(&mut buffer)
                .expect("decode compaction fixture frame")
                .expect("compaction fixture frame must be complete");
        }
        assert!(
            buffer.consumed_prefix_len() >= STREAM_BUFFER_MIN_COMPACTION_PREFIX,
            "fixture must cross the minimum compaction prefix"
        );
        assert!(buffer.consumed_prefix_len() >= buffer.len());
        let unread_before_append = buffer.as_slice().to_vec();

        let mut appended_frame = Vec::new();
        Pdu::Pong(Pong {})
            .encode(&mut appended_frame, 2)
            .expect("encode post-compaction successor frame");
        buffer.extend_from_slice(&appended_frame);

        let stats = buffer.stats();
        assert_eq!(stats.compactions, 1);
        assert_eq!(
            stats.compacted_bytes,
            u64::try_from(unread_before_append.len()).expect("fixture length fits u64")
        );
        assert_eq!(buffer.consumed_prefix_len(), 0);
        assert_eq!(
            buffer.as_slice(),
            [unread_before_append.as_slice(), appended_frame.as_slice()].concat(),
            "one amortized compaction must preserve every unread byte and its appended successor"
        );
    }

    #[test]
    fn stream_decode_rejects_oversized_container_lengths_before_allocation() {
        let mut buffer = StreamingPduBuffer::from(vec![
            0x0E, 0x44, 0x04, 0x00, 0x04, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0x71, 0x71, 0x71, 0x30,
            0x71, 0x71, 0xFE,
        ]);
        let err = Pdu::stream_decode(&mut buffer).expect_err("crafted input should be rejected");
        let message = err.to_string();
        assert!(
            !message.trim().is_empty(),
            "stream_decode should surface a non-empty rejection reason",
        );
    }

    // --- SerializedLines tests ---

    #[test]
    fn serialized_lines_default_empty() {
        let sl = SerializedLines::default();
        let (lines, images) = sl.extract_data();
        assert!(lines.is_empty());
        assert!(images.is_empty());
    }

    #[test]
    fn serialized_lines_from_empty_vec() {
        let sl: SerializedLines = vec![].into();
        let (lines, images) = sl.extract_data();
        assert!(lines.is_empty());
        assert!(images.is_empty());
    }

    // --- CODEC_VERSION test ---

    #[test]
    fn codec_version_is_current() {
        assert_eq!(CODEC_VERSION, 51);
    }

    #[test]
    fn pdu_wire_registry_covers_every_assigned_id_and_only_the_historical_gaps() {
        const GAPS: &[u64] = &[5, 6, 7, 15, 16, 17, 18, 19, 21];

        for ident in 0..=90 {
            let spec = Pdu::wire_spec_for_ident(ident);
            assert_eq!(
                spec.is_none(),
                GAPS.contains(&ident),
                "wire ID {ident} has the wrong assigned/gap disposition",
            );
            if let Some(spec) = spec {
                assert_eq!(spec.ident, ident);
                assert_eq!(Pdu::pdu_name_for_ident(ident), Some(spec.name));
                assert!(!spec.authorities.is_empty());
            }
        }

        assert!(Pdu::wire_spec_for_ident(91).is_none());
        assert!(Pdu::wire_spec_for_ident(u64::MAX).is_none());
        assert_eq!(Pdu::all_wire_specs().len(), 91 - GAPS.len());
    }

    #[test]
    fn pdu_wire_registry_ids_and_names_are_unique_and_ordered() {
        let specs = Pdu::all_wire_specs();
        let mut ids = std::collections::HashSet::with_capacity(specs.len());
        let mut names = std::collections::HashSet::with_capacity(specs.len());

        for spec in specs {
            assert!(ids.insert(spec.ident), "duplicate PDU ID {}", spec.ident);
            assert!(names.insert(spec.name), "duplicate PDU name {}", spec.name);
        }
        assert!(
            specs.windows(2).all(|pair| pair[0].ident < pair[1].ident),
            "the generated registry must remain in strictly increasing ID order",
        );
    }

    #[test]
    fn pdu_wire_registry_minimum_dialects_are_exhaustive() {
        for spec in Pdu::all_wire_specs() {
            let expected = match spec.ident {
                0..=74 => 46,
                75..=78 => 47,
                79..=80 => 48,
                81..=83 => 49,
                84..=85 => 50,
                86..=90 => 51,
                ident => panic!("unexpected assigned PDU ID {}", ident),
            };
            assert_eq!(
                spec.min_codec_version, expected,
                "wrong minimum dialect for PDU {} ({})",
                spec.ident, spec.name,
            );
            assert!((CODEC_VERSION_MIN_SUPPORTED..=CODEC_VERSION)
                .contains(&spec.min_codec_version));
        }

        assert_eq!(
            Pdu::Ping(Ping {}).minimum_codec_version(),
            Some(CODEC_VERSION_MIN_SUPPORTED),
        );
        assert_eq!(Pdu::Invalid { ident: 5 }.minimum_codec_version(), None);
        assert_eq!(Pdu::Invalid { ident: 5 }.producer(), None);
        assert_eq!(
            Pdu::Invalid { ident: 5 }.required_topology_capabilities(),
            None,
        );
        assert!(Pdu::Invalid { ident: 5 }.wire_spec().is_none());
    }

    #[test]
    fn pdu_wire_registry_authorizes_the_exact_dispatch_matrix() {
        const CLIENT_REQUESTS: &[u64] = &[
            1, 3, 9, 11, 12, 13, 14, 22, 24, 26, 28, 31, 33, 34, 35, 36, 38, 40, 41, 43,
            45, 46, 48, 50, 51, 56, 57, 58, 59, 60, 62, 63, 64, 65, 66, 67, 68, 69, 70,
            71, 72, 73, 74, 75, 77, 80, 81, 85, 86, 88,
        ];
        const SERVER_REPLIES: &[u64] = &[
            0, 2, 4, 8, 10, 23, 25, 27, 29, 30, 32, 42, 47, 49, 52, 61, 76, 78, 82, 87,
            89,
        ];
        const SERVER_UNILATERALS: &[u64] = &[
            20, 25, 37, 38, 39, 44, 53, 54, 55, 56, 57, 58, 79, 83, 84, 90,
        ];

        for spec in Pdu::all_wire_specs() {
            assert_eq!(
                spec.authorizes(PduProducer::Client, PduWireRole::Request),
                CLIENT_REQUESTS.contains(&spec.ident),
                "wrong client/request authority for PDU {} ({})",
                spec.ident,
                spec.name,
            );
            assert_eq!(
                spec.authorizes(PduProducer::Server, PduWireRole::CorrelatedReply),
                SERVER_REPLIES.contains(&spec.ident),
                "wrong server/reply authority for PDU {} ({})",
                spec.ident,
                spec.name,
            );
            assert_eq!(
                spec.authorizes(PduProducer::Server, PduWireRole::Unilateral),
                SERVER_UNILATERALS.contains(&spec.ident),
                "wrong server/unilateral authority for PDU {} ({})",
                spec.ident,
                spec.name,
            );

            assert!(!spec.authorizes(PduProducer::Bidirectional, PduWireRole::Request));
            assert!(!spec.authorizes(PduProducer::Client, PduWireRole::CorrelatedReply));
            assert!(!spec.authorizes(PduProducer::Client, PduWireRole::Unilateral));
            assert!(!spec.authorizes(PduProducer::Server, PduWireRole::Request));

            let expected_producer = match (
                CLIENT_REQUESTS.contains(&spec.ident),
                SERVER_REPLIES.contains(&spec.ident)
                    || SERVER_UNILATERALS.contains(&spec.ident),
            ) {
                (true, true) => PduProducer::Bidirectional,
                (true, false) => PduProducer::Client,
                (false, true) => PduProducer::Server,
                (false, false) => panic!("PDU {} has no producer", spec.ident),
            };
            assert_eq!(spec.producer, expected_producer);
        }

        assert_eq!(
            Pdu::wire_spec_for_ident(25).expect("PDU 25 assigned").authorities.len(),
            2,
            "render changes are both a correlated reply and a unilateral sideband",
        );
        for ident in [38, 56, 57, 58] {
            assert_eq!(
                Pdu::wire_spec_for_ident(ident)
                    .expect("bidirectional PDU assigned")
                    .producer,
                PduProducer::Bidirectional,
            );
        }
    }

    #[test]
    fn pdu_wire_registry_capability_use_is_exhaustive_and_keeps_ordering_disabled() {
        let fenced = TopologyCapabilities::FENCED_SNAPSHOT_V1;
        let ordered = TopologyCapabilities::from_bits(
            fenced.bits() | TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
        );
        let reorder = TopologyCapabilities::from_bits(
            ordered.bits() | TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(),
        );

        for spec in Pdu::all_wire_specs() {
            let expected = match spec.ident {
                81 | 82 => PduCapabilityUse::Negotiates(fenced),
                83..=85 => PduCapabilityUse::Requires(fenced),
                86 | 87 => PduCapabilityUse::Negotiates(ordered),
                88 | 89 => PduCapabilityUse::Requires(reorder),
                90 => PduCapabilityUse::Requires(ordered),
                _ => PduCapabilityUse::None,
            };
            assert_eq!(
                spec.capability, expected,
                "wrong capability use for PDU {} ({})",
                spec.ident, spec.name,
            );
            assert_eq!(
                spec.capability.required(),
                match expected {
                    PduCapabilityUse::Requires(required) => required,
                    PduCapabilityUse::None | PduCapabilityUse::Negotiates(_) => {
                        TopologyCapabilities::NONE
                    }
                },
            );
        }

        assert_eq!(TopologyCapabilities::SERVER_SUPPORTED, fenced);
        assert!(!TopologyCapabilities::SERVER_SUPPORTED
            .contains(TopologyCapabilities::ORDERED_WINDOW_STREAM_V1));
        assert!(!TopologyCapabilities::SERVER_SUPPORTED
            .contains(TopologyCapabilities::WINDOW_REORDER_CAS_V1));
    }

    #[test]
    fn codec_version_response_decodes_canonical_legacy_wire_in_both_compression_modes() {
        let legacy = LegacyGetCodecVersionResponse {
            codec_vers: 46,
            version_string: "legacy-frankenterm".to_string(),
            executable_path: PathBuf::from("/usr/local/bin/frankenterm"),
            config_file_path: Some(PathBuf::from("/etc/frankenterm.lua")),
        };

        for mode in [CompressionMode::Never, CompressionMode::Always] {
            let (payload, is_compressed) =
                serialize_with_mode(&legacy, mode).expect("legacy response should serialize");
            let mut frame = Vec::new();
            encode_raw(27, 9, &payload, is_compressed, &mut frame)
                .expect("legacy response should frame");
            let decoded =
                Pdu::decode(frame.as_slice()).expect("current decoder must accept legacy PDU 27");
            assert_eq!(decoded.serial, 9);
            let Pdu::GetCodecVersionResponse(response) = decoded.pdu else {
                panic!("PDU 27 must remain GetCodecVersionResponse");
            };
            assert_eq!(response.codec_vers, legacy.codec_vers);
            assert_eq!(response.version_string, legacy.version_string);
            assert_eq!(response.executable_path, legacy.executable_path);
            assert_eq!(response.config_file_path, legacy.config_file_path);
            assert_eq!(response.min_supported, 0);
        }
    }

    #[test]
    fn codec_version_response_dual_schema_decoder_rejects_noncanonical_trailing_bytes() {
        let legacy = LegacyGetCodecVersionResponse {
            codec_vers: 46,
            version_string: "legacy-frankenterm".to_string(),
            executable_path: PathBuf::from("/usr/local/bin/frankenterm"),
            config_file_path: None,
        };
        let (mut payload, is_compressed) =
            serialize_with_mode(&legacy, CompressionMode::Never)
                .expect("legacy response should serialize");
        assert!(!is_compressed);
        payload.extend_from_slice(&[0, 0]);
        let mut frame = Vec::new();
        encode_raw(27, 9, &payload, false, &mut frame)
            .expect("malformed legacy response should frame");
        Pdu::decode(frame.as_slice())
            .expect_err("dual-schema fallback must reject trailing schema bytes");
    }

    // --- check_compat / CODEC_VERSION_MIN_SUPPORTED tests (ft-kuxho.B.1) ---

    #[test]
    fn check_compat_additive_window_keeps_min_supported() {
        assert_eq!(CODEC_VERSION_MIN_SUPPORTED, 46);
        assert_eq!(CODEC_VERSION, 51);
    }

    #[test]
    fn check_compat_same_version_both_sides() {
        // (a) Both peers at v46 with min=46. Overlap is [46, 46], non-empty.
        let decision = check_compat(46, 46, 46, 46).expect("v46 vs v46 must be compatible");
        assert_eq!(decision, CompatDecision::Compatible { agreed: 46 });
    }

    #[test]
    fn check_compat_local_newer_inside_remote_window() {
        // (b) local=47 (min=46), remote=46 (min=46). Local's window
        // [46, 47] overlaps remote's window [46, 46] at [46, 46]. Both
        // sides agree on 46 so the older peer speaks its native dialect.
        let decision = check_compat(47, 46, 46, 46)
            .expect("local newer but inside remote_min must be compatible");
        assert_eq!(decision, CompatDecision::Compatible { agreed: 46 });

        // Symmetric: local=46, remote=47, both min=46. Same outcome.
        let decision = check_compat(46, 46, 47, 46)
            .expect("remote newer but inside local_min must be compatible");
        assert_eq!(decision, CompatDecision::Compatible { agreed: 46 });
    }

    #[test]
    fn check_compat_local_below_remote_minimum_is_incompatible() {
        // (c) local=46 (max), remote=48 (min=47). Local's window [46, 46]
        // and remote's window [47, 48] do not overlap — remote refuses
        // to speak v46 because its min is 47.
        let err =
            check_compat(46, 46, 48, 47).expect_err("local below remote_min must be incompatible");
        assert_eq!(
            err,
            CompatError {
                local: 46,
                local_min: 46,
                remote: 48,
                remote_min: 47,
            }
        );

        // Error message must surface both triples and the runbook link
        // so on-call has a one-click path to the operator procedure.
        let rendered = err.to_string();
        assert!(rendered.contains("local=46"));
        assert!(rendered.contains("remote=48"));
        assert!(rendered.contains("min 46"));
        assert!(rendered.contains("min 47"));
        assert!(rendered.contains("docs/codec-atomic-redeploy.md"));
    }

    #[test]
    fn check_compat_agrees_on_lower_version_within_window() {
        // Both peers have wide windows. local=50 (min=46), remote=48 (min=46).
        // Overlap [46, 48]. Agreed = min(50, 48) = 48 — the older peer's
        // canonical version is what gets spoken, not the overlap edge.
        // The higher peer must gate every outbound PDU to minimum dialect 48;
        // it must not emit v49/v50 families and rely on an older decoder to
        // ignore them.
        let decision = check_compat(50, 46, 48, 46).expect("wide windows must overlap");
        assert_eq!(decision, CompatDecision::Compatible { agreed: 48 });
    }

    #[test]
    fn check_compat_rejects_impossible_local_or_remote_windows() {
        assert_eq!(
            check_compat(50, 51, 50, 46),
            Err(CompatError {
                local: 50,
                local_min: 51,
                remote: 50,
                remote_min: 46,
            }),
            "an impossible local window must fail closed even if its unchecked endpoints overlap",
        );
        assert_eq!(
            check_compat(50, 46, 50, 51),
            Err(CompatError {
                local: 50,
                local_min: 46,
                remote: 50,
                remote_min: 51,
            }),
            "an impossible peer window must fail closed rather than be repaired",
        );
        assert_eq!(
            check_compat(0, 1, 0, 1),
            Err(CompatError {
                local: 0,
                local_min: 1,
                remote: 0,
                remote_min: 1,
            }),
        );
    }

    // --- CorruptResponse tests ---

    #[test]
    fn corrupt_response_display() {
        let err = CorruptResponse::Message("bad data".into());
        assert_eq!(format!("{}", err), "Corrupt Response: bad data");
    }

    #[test]
    fn corrupt_response_debug() {
        let err = CorruptResponse::Message("test".into());
        assert_eq!(format!("{err:?}"), "CorruptResponse(\"test\")");
    }

    #[test]
    fn serial_above_ceiling_debug_is_typed() {
        let err = CorruptResponse::SerialAboveCeiling {
            serial: 43,
            max_serial: 42,
        };
        assert_eq!(
            format!("{err:?}"),
            "CorruptResponse::SerialAboveCeiling { serial: 43, max_serial: 42 }"
        );
    }

    // --- DecodedPdu tests ---

    #[test]
    fn decoded_pdu_debug() {
        let dp = DecodedPdu {
            serial: 42,
            pdu: Pdu::Ping(Ping {}),
        };
        let dbg = format!("{:?}", dp);
        assert!(dbg.contains("42"));
        assert!(dbg.contains("Ping"));
    }

    #[test]
    fn decoded_pdu_partial_eq() {
        let a = DecodedPdu {
            serial: 1,
            pdu: Pdu::Ping(Ping {}),
        };
        let b = DecodedPdu {
            serial: 1,
            pdu: Pdu::Ping(Ping {}),
        };
        let c = DecodedPdu {
            serial: 2,
            pdu: Pdu::Ping(Ping {}),
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    // --- PDU struct construction tests ---

    #[test]
    fn error_response_construction() {
        let err = ErrorResponse {
            reason: "test error".into(),
        };
        assert_eq!(err.reason, "test error");
        let clone_check = format!("{:?}", err);
        assert!(clone_check.contains("test error"));
    }

    #[test]
    fn get_codec_version_response_construction() {
        let resp = GetCodecVersionResponse {
            codec_vers: CODEC_VERSION,
            version_string: "1.0.0".into(),
            executable_path: PathBuf::from("/usr/bin/ft"),
            config_file_path: Some(PathBuf::from("/etc/ft.toml")),
            min_supported: CODEC_VERSION_MIN_SUPPORTED,
        };
        assert_eq!(resp.codec_vers, CODEC_VERSION);
        assert_eq!(resp.version_string, "1.0.0");
    }

    #[test]
    fn get_tls_creds_response_construction() {
        let resp = GetTlsCredsResponse {
            ca_cert_pem: "CA".into(),
            client_cert_pem: "CLIENT".into(),
        };
        assert_eq!(resp.ca_cert_pem, "CA");
        assert_eq!(resp.client_cert_pem, "CLIENT");
    }

    #[test]
    fn set_window_workspace_construction() {
        let msg = SetWindowWorkspace {
            window_id: 1,
            workspace: "default".into(),
        };
        assert_eq!(msg.window_id, 1);
        assert_eq!(msg.workspace, "default");
    }

    #[test]
    fn set_active_workspace_construction() {
        let msg = SetActiveWorkspace {
            workspace: "remote-dev".into(),
        };
        assert_eq!(msg.workspace, "remote-dev");
    }

    #[test]
    fn tab_title_changed_construction() {
        let msg = TabTitleChanged {
            tab_id: 5,
            title: "my tab".into(),
        };
        assert_eq!(msg.tab_id, 5);
        assert_eq!(msg.title, "my tab");
    }

    #[test]
    fn window_title_changed_construction() {
        let msg = WindowTitleChanged {
            window_id: 3,
            title: "my window".into(),
        };
        assert_eq!(msg.window_id, 3);
        assert_eq!(msg.title, "my window");
    }

    #[test]
    fn serialized_image_cell_debug_and_clone() {
        // SerializedImageCell requires NotNan<f32> for TextureCoordinate,
        // so test Debug/Clone/Eq on the struct via serde roundtrip instead
        let sl = SerializedLines::default();
        assert!(sl.images.is_empty());
        let dbg = format!("{:?}", sl);
        assert!(dbg.contains("SerializedLines"));
    }

    // --- read_u64 tests ---

    #[test]
    fn read_u64_small() {
        let data = [42u8]; // leb128 for 42
        let result = read_u64(data.as_slice()).unwrap();
        assert_eq!(result, 42);
    }

    #[test]
    fn read_u64_two_byte() {
        // leb128 encoding of 128: 0x80 0x01
        let data = [0x80u8, 0x01];
        let result = read_u64(data.as_slice()).unwrap();
        assert_eq!(result, 128);
    }

    #[test]
    fn read_u64_empty_fails() {
        let data: &[u8] = &[];
        assert!(read_u64(data).is_err());
    }

    // --- Multiple PDU encode/decode in sequence (using Pdu::decode directly) ---

    #[test]
    fn multiple_pdus_sequential_decode() {
        // Encode three PDUs into a single buffer
        let mut buf = Vec::new();
        Pdu::Ping(Ping {}).encode(&mut buf, 1).unwrap();
        Pdu::Pong(Pong {}).encode(&mut buf, 2).unwrap();
        Pdu::UnitResponse(UnitResponse {})
            .encode(&mut buf, 3)
            .unwrap();

        // Decode them sequentially using Pdu::decode on a Cursor
        let mut cursor = Cursor::new(buf.as_slice());

        let d1 = Pdu::decode(&mut cursor).unwrap();
        assert_eq!(d1.serial, 1);
        assert_eq!(d1.pdu, Pdu::Ping(Ping {}));

        let d2 = Pdu::decode(&mut cursor).unwrap();
        assert_eq!(d2.serial, 2);
        assert_eq!(d2.pdu, Pdu::Pong(Pong {}));

        let d3 = Pdu::decode(&mut cursor).unwrap();
        assert_eq!(d3.serial, 3);
        assert_eq!(d3.pdu, Pdu::UnitResponse(UnitResponse {}));
    }

    // --- Compression roundtrip through full PDU encode/decode ---

    #[test]
    fn pdu_roundtrip_compressed_write_to_pane() {
        let mut buf = Vec::new();
        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 1,
            data: vec![b'A'; 1024],
        });
        pdu.encode_with_mode(&mut buf, 42, CompressionMode::Always)
            .unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 42);
        assert_eq!(decoded.pdu, pdu);
    }

    // --- Additional codec edge and async coverage (wa-2mina) ---

    #[test]
    fn encode_raw_as_vec_sets_compressed_length_bit() {
        let uncompressed = encode_raw_as_vec(7, 9, b"abc", false).unwrap();
        let compressed = encode_raw_as_vec(7, 9, b"abc", true).unwrap();

        let uncompressed_len = read_u64(uncompressed.as_slice()).unwrap();
        let compressed_len = read_u64(compressed.as_slice()).unwrap();

        assert_eq!(uncompressed_len & COMPRESSED_MASK, 0);
        assert_eq!(compressed_len & COMPRESSED_MASK, COMPRESSED_MASK);
        assert_eq!(
            compressed_len & !COMPRESSED_MASK,
            uncompressed_len & !COMPRESSED_MASK
        );
    }

    #[test]
    fn decode_raw_errors_on_header_length_underflow() {
        // len=1, serial=1, ident=1 => encoded(serial)+encoded(ident)=2, impossible frame
        let malformed = vec![1u8, 1u8, 1u8];
        let err = decode_raw(malformed.as_slice()).expect_err("expected malformed frame to fail");
        let message = err.to_string();
        assert!(
            message.contains("sizes don't make sense"),
            "unexpected error message: {}",
            message
        );
    }

    #[test]
    fn decoded_payload_len_rejects_lengths_too_wide_for_usize() {
        let Some(wide_len) = (usize::MAX as u64).checked_add(1) else {
            return;
        };

        let err = decoded_payload_len("test", wide_len, 1, 1, 1, 1)
            .expect_err("wire length wider than usize must fail before truncating");
        assert!(
            err.to_string().contains("does not fit in usize"),
            "unexpected error message: {err}",
            err = err,
        );
    }

    #[test]
    fn decode_incomplete_max_size_frame_reads_payload_in_chunks() {
        use std::io::{Cursor, Read};
        use std::sync::{Arc, Mutex};

        struct PrefixThenEof {
            prefix: Cursor<Vec<u8>>,
            max_requested: Arc<Mutex<usize>>,
        }

        impl Read for PrefixThenEof {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let mut max_requested = match self.max_requested.lock() {
                    Ok(max_requested) => max_requested,
                    Err(poisoned) => poisoned.into_inner(),
                };
                *max_requested = (*max_requested).max(buf.len());
                drop(max_requested);
                // Disambiguate: under `--features async-asupersync` the module's
                // `asupersync::io::AsyncReadExt` is in scope and its blanket `read`
                // collides with `std::io::Read::read` for `Cursor` (E0034). This is
                // the sync `impl Read`, so spell out the std trait.
                std::io::Read::read(&mut self.prefix, buf)
            }
        }

        let mut header = Vec::new();
        leb128::write::unsigned(&mut header, MAX_PDU_SIZE as u64).unwrap();
        leb128::write::unsigned(&mut header, 1).unwrap();
        leb128::write::unsigned(&mut header, 1).unwrap();

        let max_requested = Arc::new(Mutex::new(0usize));
        let reader = PrefixThenEof {
            prefix: Cursor::new(header),
            max_requested: Arc::clone(&max_requested),
        };

        let err = Pdu::decode(reader).expect_err("incomplete max-size frame must fail");
        assert!(
            !err.to_string().is_empty(),
            "incomplete max-size frame should surface a typed error"
        );

        let observed = match max_requested.lock() {
            Ok(max_requested) => *max_requested,
            Err(poisoned) => *poisoned.into_inner(),
        };
        assert!(
            observed <= PAYLOAD_READ_CHUNK,
            "decoder requested a {} byte payload read; expected chunked reads no larger than {}",
            observed,
            PAYLOAD_READ_CHUNK
        );
    }

    #[test]
    fn deserialize_invalid_compressed_payload_errors() {
        let err =
            deserialize::<u64, _>(b"not-zstd".as_slice(), true).expect_err("expected zstd error");
        assert!(
            !err.to_string().is_empty(),
            "deserialize should surface a non-empty error"
        );
    }

    #[test]
    fn serialize_with_mode_always_compresses_small_payload() {
        let (payload, is_compressed) =
            serialize_with_mode(&7u8, CompressionMode::Always).expect("serialize");
        assert!(is_compressed);
        let roundtrip: u8 = deserialize(payload.as_slice(), true).expect("deserialize");
        assert_eq!(roundtrip, 7u8);
    }

    #[test]
    fn encode_raw_async_roundtrip_uncompressed() {
        runtime::block_on(async {
            let mut writer = runtime::Cursor::new(Vec::<u8>::new());
            encode_raw_async(17, 23, b"async-raw", false, &mut writer)
                .await
                .expect("encode_raw_async");
            let encoded = writer.into_inner();

            let decoded = decode_raw(encoded.as_slice()).expect("decode_raw");
            assert_eq!(decoded.ident, 17);
            assert_eq!(decoded.serial, 23);
            assert_eq!(decoded.data, b"async-raw");
            assert!(!decoded.is_compressed);
        });
    }

    #[test]
    fn decode_raw_async_roundtrip_uncompressed() {
        runtime::block_on(async {
            let mut encoded = Vec::new();
            encode_raw(11, 13, b"decode-async", false, &mut encoded).expect("encode_raw");

            let mut reader = runtime::Cursor::new(encoded);
            let decoded = decode_raw_async(&mut reader, None)
                .await
                .expect("decode_raw_async");
            assert_eq!(decoded.ident, 11);
            assert_eq!(decoded.serial, 13);
            assert_eq!(decoded.data, b"decode-async");
            assert!(!decoded.is_compressed);
        });
    }

    #[test]
    fn decode_raw_async_roundtrip_compressed_flag() {
        runtime::block_on(async {
            let mut encoded = Vec::new();
            encode_raw(31, 9, b"decode-async-compressed", true, &mut encoded).expect("encode_raw");

            let mut reader = runtime::Cursor::new(encoded);
            let decoded = decode_raw_async(&mut reader, None)
                .await
                .expect("decode_raw_async");
            assert_eq!(decoded.ident, 31);
            assert_eq!(decoded.serial, 9);
            assert_eq!(decoded.data, b"decode-async-compressed");
            assert!(decoded.is_compressed);
        });
    }

    #[test]
    fn decode_raw_async_rejects_serial_over_max() {
        runtime::block_on(async {
            let mut encoded = Vec::new();
            encode_raw(3, 99, b"x", false, &mut encoded).expect("encode_raw");

            let mut reader = runtime::Cursor::new(encoded);
            let err = decode_raw_async(&mut reader, Some(10))
                .await
                .expect_err("serial should be rejected");
            assert_eq!(
                err.downcast_ref::<CorruptResponse>(),
                Some(&CorruptResponse::SerialAboveCeiling {
                    serial: 99,
                    max_serial: 10,
                })
            );
        });
    }

    #[test]
    fn decode_raw_async_zero_ceiling_accepts_only_serial_zero() {
        runtime::block_on(async {
            let mut unilateral = Vec::new();
            encode_raw(3, 0, b"push", false, &mut unilateral).expect("encode unilateral");
            let mut unilateral_reader = runtime::Cursor::new(unilateral);
            let decoded = decode_raw_async(&mut unilateral_reader, Some(0))
                .await
                .expect("serial zero is within the zero ceiling");
            assert_eq!(decoded.serial, 0);
            assert_eq!(decoded.data, b"push");

            let mut response = Vec::new();
            encode_raw(3, 1, b"reply", false, &mut response).expect("encode response");
            let mut response_reader = runtime::Cursor::new(response);
            let err = decode_raw_async(&mut response_reader, Some(0))
                .await
                .expect_err("a nonzero serial exceeds the zero ceiling");
            assert_eq!(
                err.downcast_ref::<CorruptResponse>(),
                Some(&CorruptResponse::SerialAboveCeiling {
                    serial: 1,
                    max_serial: 0,
                }),
                "unexpected zero-ceiling error: {:#}",
                err
            );
        });
    }

    #[test]
    fn decode_raw_async_accepts_serial_equal_to_ceiling() {
        runtime::block_on(async {
            let mut encoded = Vec::new();
            encode_raw(3, 10, b"exact", false, &mut encoded).expect("encode exact ceiling");
            let mut reader = runtime::Cursor::new(encoded);
            let decoded = decode_raw_async(&mut reader, Some(10))
                .await
                .expect("the serial ceiling is inclusive");
            assert_eq!(decoded.serial, 10);
            assert_eq!(decoded.data, b"exact");
        });
    }

    #[test]
    fn async_selector_discard_is_fixed_bound_and_exactly_resynchronizes() {
        for data_len in [
            0,
            1,
            DISCARDED_PAYLOAD_READ_CHUNK - 1,
            DISCARDED_PAYLOAD_READ_CHUNK,
            DISCARDED_PAYLOAD_READ_CHUNK + 1,
        ] {
            runtime::block_on(async {
                let mut wire = Vec::new();
                encode_raw(99, 1, &vec![0x5a; data_len], false, &mut wire)
                    .expect("encode discard candidate");
                Pdu::Pong(Pong {})
                    .encode(&mut wire, 2)
                    .expect("encode live successor");
                let mut reader = runtime::Cursor::new(wire);

                let discarded = Pdu::decode_async_with_selector(
                    &mut reader,
                    Some(2),
                    |header| {
                        assert_eq!(header.serial(), 1);
                        assert_eq!(header.ident(), 99);
                        assert_eq!(header.encoded_payload_len(), data_len);
                        assert!(!header.is_compressed());
                        Ok(PduBodyDisposition::Discard)
                    },
                )
                .await
                .expect("discard exact body");
                let AsyncPduDecode::Discarded {
                    serial,
                    ident,
                    body,
                } = discarded
                else {
                    panic!("selector requested discard but codec materialized the body");
                };
                assert_eq!(serial, 1);
                assert_eq!(ident, 99);
                assert_eq!(body.encoded_bytes(), data_len);
                assert!(body.max_chunk_bytes() <= DiscardedPduBody::scratch_capacity());
                assert_eq!(
                    body.chunk_reads(),
                    data_len.div_ceil(DiscardedPduBody::scratch_capacity())
                );

                let successor = Pdu::decode_async(&mut reader, Some(2))
                    .await
                    .expect("discard must leave the next frame exactly aligned");
                assert_eq!(successor.serial, 2);
                assert_eq!(successor.pdu, Pdu::Pong(Pong {}));
            });
        }
    }

    #[test]
    fn async_selector_rejects_oversize_header_before_body_selection() {
        runtime::block_on(async {
            let mut header = Vec::new();
            let frame_len = u64::try_from(MAX_PDU_SIZE)
                .expect("MAX_PDU_SIZE fits u64")
                .checked_add(3)
                .expect("test frame length fits u64");
            leb128::write::unsigned(&mut header, frame_len).expect("encode frame length");
            leb128::write::unsigned(&mut header, 1).expect("encode serial");
            leb128::write::unsigned(&mut header, 99).expect("encode ident");
            let mut reader = runtime::Cursor::new(header);
            let mut selector_was_called = false;

            let error = Pdu::decode_async_with_selector(&mut reader, Some(1), |_| {
                selector_was_called = true;
                Ok(PduBodyDisposition::Discard)
            })
            .await
            .expect_err("oversize payload declaration must fail before selection");
            let rendered_error = format!("{error:#}");
            assert!(
                rendered_error.contains("exceeds maximum"),
                "unexpected oversize-header error: {:#}",
                error
            );
            assert!(
                !selector_was_called,
                "invalid length must not reach the body-disposition authority"
            );
        });
    }

    #[test]
    fn async_selector_truncated_discard_fails_closed() {
        runtime::block_on(async {
            let mut wire = Vec::new();
            encode_raw(99, 1, &[0x41; 128], false, &mut wire)
                .expect("encode discard candidate");
            wire.pop().expect("encoded frame has a payload byte");
            let mut reader = runtime::Cursor::new(wire);

            let error = Pdu::decode_async_with_selector(&mut reader, Some(1), |_| {
                Ok(PduBodyDisposition::Discard)
            })
            .await
            .expect_err("truncated discarded body must fail closed");
            assert!(
                error.to_string().contains("discarding an abandoned PDU body"),
                "unexpected truncated-discard error: {:#}",
                error
            );
        });
    }

    #[test]
    fn async_selector_never_raw_discards_compressed_payloads() {
        runtime::block_on(async {
            let wire = Pdu::WriteToPane(WriteToPane {
                pane_id: 7,
                data: vec![b'x'; 4 * 1024],
            })
            .encode_frame_with_mode(1, CompressionMode::Always)
            .expect("encode compressed frame");
            let mut reader = runtime::Cursor::new(wire);

            let error = Pdu::decode_async_with_selector(&mut reader, Some(1), |header| {
                assert!(header.is_compressed());
                Ok(PduBodyDisposition::Discard)
            })
            .await
            .expect_err("compressed discard without zstd validation must fail closed");
            assert!(
                error.to_string().contains("refusing to discard compressed PDU body"),
                "unexpected compressed-discard error: {:#}",
                error
            );
        });
    }

    #[test]
    fn decode_accepts_valid_non_canonical_leb128_headers() {
        let wire = [0x84, 0x00, 0x81, 0x00, 0xE3, 0x00];
        let decoded = Pdu::decode(wire.as_slice()).expect("valid non-canonical header");
        assert_eq!(decoded.serial, 1);
        assert_eq!(decoded.pdu, Pdu::Invalid { ident: 99 });
    }

    #[test]
    fn decode_raw_async_accepts_valid_non_canonical_leb128_headers() {
        runtime::block_on(async {
            let mut reader = runtime::Cursor::new(vec![0x84, 0x00, 0x81, 0x00, 0xE3, 0x00]);
            let decoded = Pdu::decode_async(&mut reader, None)
                .await
                .expect("valid non-canonical header");
            assert_eq!(decoded.serial, 1);
            assert_eq!(decoded.pdu, Pdu::Invalid { ident: 99 });
        });
    }

    #[test]
    fn read_u64_async_returns_eof_on_empty_input() {
        runtime::block_on(async {
            let mut reader = runtime::Cursor::new(Vec::<u8>::new());
            let err = read_u64_async(&mut reader)
                .await
                .expect_err("empty stream should error");
            let io_err = err
                .downcast_ref::<std::io::Error>()
                .expect("expected io::Error");
            assert_eq!(io_err.kind(), std::io::ErrorKind::UnexpectedEof);
        });
    }

    #[cfg(all(feature = "async-smol", feature = "async-asupersync"))]
    #[test]
    fn mixed_feature_mode_still_accepts_smol_cursors() {
        fn assert_smol_cursor(_: &smol::io::Cursor<Vec<u8>>) {}

        runtime::block_on(async {
            let mut writer = runtime::Cursor::new(Vec::<u8>::new());
            assert_smol_cursor(&writer);

            encode_raw_async(19, 29, b"mixed-features", false, &mut writer)
                .await
                .expect("encode_raw_async");

            let mut reader = smol::io::Cursor::new(writer.into_inner());
            let decoded = decode_raw_async(&mut reader, None)
                .await
                .expect("decode_raw_async");

            assert_eq!(decoded.ident, 19);
            assert_eq!(decoded.serial, 29);
            assert_eq!(decoded.data, b"mixed-features");
            assert!(!decoded.is_compressed);
        });
    }

    // --- Additional PDU roundtrip tests (wa-2tcrj) ---

    #[test]
    fn pdu_roundtrip_list_panes() {
        let mut buf = Vec::new();
        let pdu = Pdu::ListPanes(ListPanes {});
        pdu.encode(&mut buf, 111).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 111);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_get_tls_creds() {
        let mut buf = Vec::new();
        let pdu = Pdu::GetTlsCreds(GetTlsCreds {});
        pdu.encode(&mut buf, 222).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 222);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_get_tls_creds_response() {
        let mut buf = Vec::new();
        let pdu = Pdu::GetTlsCredsResponse(GetTlsCredsResponse {
            ca_cert_pem: "CERT".into(),
            client_cert_pem: "KEY".into(),
        });
        pdu.encode(&mut buf, 333).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 333);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_get_codec_version_response() -> anyhow::Result<()> {
        let mut buf = Vec::new();
        let pdu = Pdu::GetCodecVersionResponse(GetCodecVersionResponse {
            codec_vers: CODEC_VERSION,
            version_string: "test".into(),
            executable_path: PathBuf::from("/bin/test"),
            config_file_path: None,
            min_supported: CODEC_VERSION_MIN_SUPPORTED,
        });
        pdu.encode(&mut buf, 444)?;
        let decoded = Pdu::decode(buf.as_slice())?;
        assert_eq!(decoded.serial, 444);
        assert_eq!(decoded.pdu, pdu);
        // ft-kuxho.B.3: explicit field-level check that min_supported
        // survives the roundtrip. The body of the equality assertion
        // above already guarantees this, but the named extraction
        // below documents the contract for future readers and pins
        // the field's wire-format presence as load-bearing.
        if let Pdu::GetCodecVersionResponse(resp) = decoded.pdu {
            assert_eq!(
                resp.min_supported, CODEC_VERSION_MIN_SUPPORTED,
                "min_supported must survive the wire roundtrip byte-for-byte"
            );
        } else {
            bail!("decoded PDU was not GetCodecVersionResponse");
        }
        Ok(())
    }

    #[test]
    fn pdu_roundtrip_tab_added_to_window() {
        let mut buf = Vec::new();
        let pdu = Pdu::TabAddedToWindow(TabAddedToWindow {
            tab_id: 10,
            window_id: 20,
        });
        pdu.encode(&mut buf, 555).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 555);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_tab_title_changed() {
        let mut buf = Vec::new();
        let pdu = Pdu::TabTitleChanged(TabTitleChanged {
            tab_id: 3,
            title: "new title".into(),
        });
        pdu.encode(&mut buf, 666).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 666);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_window_title_changed() {
        let mut buf = Vec::new();
        let pdu = Pdu::WindowTitleChanged(WindowTitleChanged {
            window_id: 7,
            title: "window title".into(),
        });
        pdu.encode(&mut buf, 777).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 777);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_set_window_workspace() {
        let mut buf = Vec::new();
        let pdu = Pdu::SetWindowWorkspace(SetWindowWorkspace {
            window_id: 2,
            workspace: "dev".into(),
        });
        pdu.encode(&mut buf, 888).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 888);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn pdu_roundtrip_set_active_workspace() {
        let mut buf = Vec::new();
        let pdu = Pdu::SetActiveWorkspace(SetActiveWorkspace {
            workspace: "ops".into(),
        });
        pdu.encode(&mut buf, 889).unwrap();
        let decoded = Pdu::decode(buf.as_slice()).unwrap();
        assert_eq!(decoded.serial, 889);
        assert_eq!(decoded.pdu, pdu);
    }

    // --- Additional pdu_name tests ---

    #[test]
    fn pdu_name_more_variants() {
        assert_eq!(
            Pdu::WriteToPane(WriteToPane {
                pane_id: 0,
                data: vec![]
            })
            .pdu_name(),
            "WriteToPane"
        );
        assert_eq!(
            Pdu::KillPane(KillPane { pane_id: 0 }).pdu_name(),
            "KillPane"
        );
        assert_eq!(
            Pdu::TabResized(TabResized { tab_id: 0 }).pdu_name(),
            "TabResized"
        );
        assert_eq!(
            Pdu::PaneRemoved(PaneRemoved { pane_id: 0 }).pdu_name(),
            "PaneRemoved"
        );
        assert_eq!(
            Pdu::RenameWorkspace(RenameWorkspace {
                old_workspace: String::new(),
                new_workspace: String::new(),
            })
            .pdu_name(),
            "RenameWorkspace"
        );
        assert_eq!(
            Pdu::SetActiveWorkspace(SetActiveWorkspace {
                workspace: String::new(),
            })
            .pdu_name(),
            "SetActiveWorkspace"
        );
    }

    // --- Additional pane_id tests ---

    #[test]
    fn pdu_pane_id_set_clipboard() {
        assert_eq!(
            Pdu::SetClipboard(SetClipboard {
                pane_id: 55,
                clipboard: None,
                selection: ClipboardSelection::Clipboard,
            })
            .pane_id(),
            Some(55)
        );
    }

    #[test]
    fn pdu_pane_id_list_panes_is_none() {
        assert_eq!(Pdu::ListPanes(ListPanes {}).pane_id(), None);
    }

    // --- Additional is_user_input tests ---

    #[test]
    fn pdu_is_user_input_set_pane_zoomed() {
        assert!(Pdu::SetPaneZoomed(SetPaneZoomed {
            containing_tab_id: 0,
            pane_id: 0,
            zoomed: true,
        })
        .is_user_input());
    }

    #[test]
    fn pdu_is_user_input_kill_pane_is_false() {
        assert!(!Pdu::KillPane(KillPane { pane_id: 0 }).is_user_input());
    }

    #[test]
    fn server_unilateral_clipboard_is_not_client_input() {
        assert!(
            !Pdu::SetClipboard(SetClipboard {
                pane_id: 55,
                clipboard: Some("copied".to_string()),
                selection: ClipboardSelection::Clipboard,
            })
            .is_user_input()
        );
    }

    // --- Additional encode/decode edge cases ---

    #[test]
    fn encode_decode_binary_data() {
        let mut encoded = Vec::new();
        let data: Vec<u8> = (0u8..=255).collect();
        encode_raw(0xFF, 0xAB, &data, false, &mut encoded).unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.data, data);
    }

    #[test]
    fn encode_decode_zero_ident_serial() {
        let mut encoded = Vec::new();
        encode_raw(0, 0, b"zero", false, &mut encoded).unwrap();
        let decoded = decode_raw(encoded.as_slice()).unwrap();
        assert_eq!(decoded.ident, 0);
        assert_eq!(decoded.serial, 0);
        assert_eq!(decoded.data, b"zero");
    }

    #[test]
    fn serialize_deserialize_string() {
        let val = "hello world".to_string();
        let (data, is_compressed) = serialize(&val).unwrap();
        let result: String = deserialize(data.as_slice(), is_compressed).unwrap();
        assert_eq!(result, val);
    }

    fn sample_tiered_scrollback_status() -> PaneTieredScrollbackStatus {
        PaneTieredScrollbackStatus {
            tiering_enabled: true,
            configured_scrollback_rows: 200_000,
            configured_hot_lines: 4_096,
            configured_warm_max_bytes: 8 * 1024 * 1024,
            visible_rows: 72,
            in_memory_scrollback_rows: 8_192,
            warm_resident_lines: 2_048,
            warm_resident_bytes: 262_144,
            warm_spill_lines_total: 32_768,
            warm_spill_bytes_total: 4_194_304,
            cold_spill_lines_total: 65_536,
            cold_spill_bytes_total: 8_388_608,
            cold_sink_retained_lines: 4096,
            cold_sink_retained_bytes: 1_048_576,
            cold_worker_peak_backlog_depth: 48,
            cold_worker_completion_throughput_lines_per_sec: 1_024,
            cold_worker_completed_lines_total: 98_765,
            cold_worker_completed_batches_total: 321,
            cold_worker_cancellation_count: 7,
        }
    }

    fn sample_retention_metadata_render_change() -> GetPaneRenderChangesResponse {
        GetPaneRenderChangesResponse {
            pane_id: 7,
            mouse_grabbed: false,
            alt_screen_active: false,
            cursor_position: StableCursorPosition::default(),
            dimensions: RenderableDimensions {
                cols: 120,
                viewport_rows: 72,
                scrollback_rows: 2_048,
                physical_top: 0,
                scrollback_top: 0,
                dpi: 96,
                pixel_width: 1_920,
                pixel_height: 1_080,
                reverse_video: false,
            },
            tiered_scrollback_status: None,
            dirty_lines: vec![0..2, 9..10],
            title: "retention-metadata-".repeat(256),
            working_dir: None,
            bonus_lines: SerializedLines::default(),
            input_serial: None,
            seqno: 17,
        }
    }

    #[test]
    fn input_serial_ordering() {
        let a = InputSerial::empty();
        let b = InputSerial::now();
        assert!(b > a, "now() should be greater than empty()");
        assert!(a < b);
        assert_eq!(a, a);
    }

    // --- Floating pane PDU roundtrip tests ---

    #[test]
    fn create_floating_pane_pdu_roundtrip() {
        let pdu = Pdu::CreateFloatingPane(CreateFloatingPane {
            tab_id: 1,
            pane_id: 42,
            rect: FloatingPaneRect {
                left: 10,
                top: 5,
                width: 30,
                height: 15,
            },
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 100).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.serial, 100);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn move_floating_pane_pdu_roundtrip() {
        let pdu = Pdu::MoveFloatingPane(MoveFloatingPane {
            pane_id: 7,
            rect: FloatingPaneRect {
                left: 20,
                top: 10,
                width: 40,
                height: 20,
            },
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 101).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn get_pane_render_changes_response_roundtrip_preserves_tiered_scrollback_status() {
        let pdu = Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
            pane_id: 7,
            mouse_grabbed: false,
            alt_screen_active: false,
            cursor_position: StableCursorPosition {
                x: 3,
                y: 9,
                ..Default::default()
            },
            dimensions: RenderableDimensions {
                cols: 120,
                viewport_rows: 72,
                scrollback_rows: 200_000,
                physical_top: 0,
                scrollback_top: 0,
                dpi: 96,
                pixel_width: 1_920,
                pixel_height: 1_080,
                reverse_video: false,
            },
            tiered_scrollback_status: Some(sample_tiered_scrollback_status()),
            dirty_lines: vec![0..2, 9..10],
            title: "scrollback-pane".to_string(),
            working_dir: None,
            bonus_lines: SerializedLines::default(),
            input_serial: None,
            seqno: 17,
        });

        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 0x53).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.serial, 0x53);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn get_pane_render_changes_response_roundtrip_none_tiered_scrollback() {
        // Regression test for ft-1qbjk: when tiered_scrollback_status is None,
        // varbincode positional format must still roundtrip correctly. Previously,
        // skip_serializing_if caused the None tag byte to be omitted, misaligning
        // all subsequent fields and producing "failed to fill whole buffer" errors.
        let pdu = Pdu::GetPaneRenderChangesResponse(GetPaneRenderChangesResponse {
            pane_id: 7,
            mouse_grabbed: false,
            alt_screen_active: false,
            cursor_position: StableCursorPosition::default(),
            dimensions: RenderableDimensions {
                cols: 80,
                viewport_rows: 24,
                scrollback_rows: 0,
                physical_top: 0,
                scrollback_top: 0,
                dpi: 96,
                pixel_width: 0,
                pixel_height: 0,
                reverse_video: false,
            },
            tiered_scrollback_status: None,
            dirty_lines: Vec::new(),
            title: "pane-7".to_string(),
            working_dir: None,
            bonus_lines: SerializedLines::default(),
            input_serial: None,
            seqno: 7,
        });

        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 0x55).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.serial, 0x55);
        assert_eq!(decoded.pdu, pdu);

        // Also verify stream_decode works (which is what the mux_pool mock server uses)
        let mut buf = StreamingPduBuffer::from(encoded.clone());
        let stream_decoded = Pdu::stream_decode(&mut buf)
            .unwrap()
            .expect("stream_decode should succeed");
        assert_eq!(stream_decoded.serial, 0x55);
        assert_eq!(stream_decoded.pdu, pdu);
    }

    #[test]
    fn render_retention_metadata_matches_canonical_frame_in_both_compression_modes() {
        let pdu = Pdu::GetPaneRenderChangesResponse(
            sample_retention_metadata_render_change(),
        );
        let expected_retained_bytes = pdu
            .encode_retained_frame(0)
            .expect("encode canonical retained render frame")
            .len();

        for mode in [CompressionMode::Never, CompressionMode::Always] {
            let frame = pdu
                .encode_frame_with_mode(0, mode)
                .expect("encode render frame");
            let mut buffer = StreamingPduBuffer::from(frame);
            let decoded = Pdu::stream_decode_with_retention_metadata(&mut buffer)
                .expect("decode render frame with retention metadata")
                .expect("complete render frame");
            assert_eq!(decoded.decoded().serial, 0);
            assert_eq!(&decoded.decoded().pdu, &pdu);
            assert_eq!(
                decoded.retained_frame_bytes(),
                Some(expected_retained_bytes),
                "mode={mode:?}"
            );
            assert!(buffer.is_empty());
        }

        let ping = Pdu::Ping(Ping {});
        let mut buffer = StreamingPduBuffer::from(
            ping.encode_frame(9).expect("encode non-render frame"),
        );
        let decoded = Pdu::stream_decode_with_retention_metadata(&mut buffer)
            .expect("decode non-render frame")
            .expect("complete non-render frame");
        assert_eq!(decoded.decoded().serial, 9);
        assert_eq!(decoded.retained_frame_bytes(), None);
    }

    #[test]
    fn render_retention_metadata_conservatively_charges_additive_payload_tails() {
        let payload = sample_retention_metadata_render_change();
        let (known_payload, compressed) =
            serialize_with_mode(&payload, CompressionMode::Never)
                .expect("serialize known render schema");
        assert!(!compressed);
        let known_frame_bytes = encoded_frame_len(
            <GetPaneRenderChangesResponse as PduWireIdent>::IDENT,
            0,
            known_payload.len(),
            false,
        )
        .expect("measure known render frame");
        let additive_tail = b"future-additive-render-fields";

        let mut payload_with_tail = known_payload.clone();
        payload_with_tail.extend_from_slice(additive_tail);
        let expected_bytes = encoded_frame_len(
            <GetPaneRenderChangesResponse as PduWireIdent>::IDENT,
            0,
            payload_with_tail.len(),
            false,
        )
        .expect("measure additive render frame");
        assert!(expected_bytes > known_frame_bytes);

        let compressed_with_tail = zstd::stream::encode_all(
            payload_with_tail.as_slice(),
            zstd::DEFAULT_COMPRESSION_LEVEL,
        )
        .expect("compress render payload with additive tail");
        let mut concatenated_compressed = zstd::stream::encode_all(
            known_payload.as_slice(),
            zstd::DEFAULT_COMPRESSION_LEVEL,
        )
        .expect("compress known render payload");
        concatenated_compressed.extend_from_slice(
            &zstd::stream::encode_all(
                additive_tail.as_slice(),
                zstd::DEFAULT_COMPRESSION_LEVEL,
            )
            .expect("compress additive render tail as a second frame"),
        );

        for (encoded_payload, is_compressed, case) in [
            (payload_with_tail, false, "uncompressed"),
            (compressed_with_tail, true, "compressed"),
            (
                concatenated_compressed,
                true,
                "concatenated-compressed",
            ),
        ] {
            let frame = encode_raw_as_vec(
                <GetPaneRenderChangesResponse as PduWireIdent>::IDENT,
                0,
                &encoded_payload,
                is_compressed,
            )
            .expect("encode additive render frame");
            let mut buffer = StreamingPduBuffer::from(frame);
            let decoded = Pdu::stream_decode_with_retention_metadata(&mut buffer)
                .unwrap_or_else(|error| panic!("{} charged decode failed: {:#}", case, error))
                .unwrap_or_else(|| panic!("{} charged frame was incomplete", case));
            assert_eq!(
                &decoded.decoded().pdu,
                &Pdu::GetPaneRenderChangesResponse(payload.clone()),
                "case={case}"
            );
            assert_eq!(
                decoded.retained_frame_bytes(),
                Some(expected_bytes),
                "case={case}"
            );
            assert!(buffer.is_empty(), "case={}", case);
        }
    }

    #[test]
    fn charged_stream_decode_rejects_invalid_compressed_tail_without_consuming_prefix() {
        let payload = sample_retention_metadata_render_change();
        let (known_payload, compressed) =
            serialize_with_mode(&payload, CompressionMode::Never)
                .expect("serialize known render schema");
        assert!(!compressed);
        let mut encoded_payload = zstd::stream::encode_all(
            known_payload.as_slice(),
            zstd::DEFAULT_COMPRESSION_LEVEL,
        )
        .expect("compress known render payload");
        encoded_payload.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
        let frame = encode_raw_as_vec(
            <GetPaneRenderChangesResponse as PduWireIdent>::IDENT,
            0,
            &encoded_payload,
            true,
        )
        .expect("encode corrupt compressed render frame");
        let mut buffer = StreamingPduBuffer::from(frame.clone());

        Pdu::stream_decode_with_retention_metadata(&mut buffer)
            .expect_err("invalid compressed tail must fail charged decode");
        assert_eq!(buffer.as_slice(), frame.as_slice());
    }

    #[test]
    fn per_frame_limit_accepts_coalesced_render_and_following_frame() {
        let render = Pdu::GetPaneRenderChangesResponse(
            sample_retention_metadata_render_change(),
        );
        let render_frame = render
            .encode_frame_with_mode(0, CompressionMode::Never)
            .expect("encode render frame");
        let ping = Pdu::Ping(Ping {});
        let ping_frame = ping.encode_frame(41).expect("encode following ping");
        let max_frame_bytes = render_frame.len().max(ping_frame.len());
        let aggregate_bytes = render_frame
            .len()
            .checked_add(ping_frame.len())
            .expect("small coalesced frames must add");
        assert!(aggregate_bytes > max_frame_bytes);
        let mut coalesced = render_frame;
        coalesced.extend_from_slice(&ping_frame);
        let mut buffer = StreamingPduBuffer::from(coalesced);

        let first = Pdu::stream_decode_with_retention_metadata_and_frame_limit(
            &mut buffer,
            max_frame_bytes,
        )
        .expect("decode first coalesced frame")
        .expect("complete first frame");
        assert_eq!(&first.decoded().pdu, &render);
        assert!(first.retained_frame_bytes().is_some());
        assert_eq!(buffer.as_slice(), ping_frame.as_slice());

        let second = Pdu::stream_decode_with_frame_limit(&mut buffer, max_frame_bytes)
            .expect("decode second coalesced frame")
            .expect("complete second frame");
        assert_eq!(second.serial, 41);
        assert_eq!(second.pdu, ping);
        assert!(buffer.is_empty());
    }

    #[test]
    fn per_frame_limit_rejects_declared_oversize_from_header_without_consumption() {
        let frame = Pdu::Ping(Ping {})
            .encode_frame(7)
            .expect("encode bounded ping frame");
        let mut suffix = frame.as_slice();
        leb128::read::unsigned(&mut suffix).expect("decode frame length prefix");
        let prefix_len = frame.len() - suffix.len();
        let header = frame[..prefix_len].to_vec();
        let max_frame_bytes = frame.len() - 1;
        let mut buffer = StreamingPduBuffer::from(header.clone());

        let error = Pdu::stream_decode_with_frame_limit(&mut buffer, max_frame_bytes)
            .expect_err("oversized declared frame must fail from its header");
        let limit = error
            .downcast_ref::<StreamingPduFrameLimitExceeded>()
            .expect("typed streaming frame limit error");
        assert_eq!(limit.declared_frame_bytes(), frame.len());
        assert_eq!(limit.max_frame_bytes(), max_frame_bytes);
        assert_eq!(buffer.as_slice(), header.as_slice());

        let hostile_body_bytes = MAX_PDU_SIZE + 1;
        let mut hostile_header = Vec::new();
        leb128::write::unsigned(
            &mut hostile_header,
            u64::try_from(hostile_body_bytes).expect("hostile fixture length fits u64"),
        )
        .expect("encode hostile declared length");
        let mut hostile_buffer = StreamingPduBuffer::from(hostile_header.clone());
        let error = Pdu::stream_decode_with_frame_limit(&mut hostile_buffer, 4 * 1024 * 1024)
            .expect_err("caller cap must classify a declaration above both limits");
        let limit = error
            .downcast_ref::<StreamingPduFrameLimitExceeded>()
            .expect("caller limit remains the typed first authority");
        assert_eq!(
            limit.declared_frame_bytes(),
            hostile_header.len() + hostile_body_bytes
        );
        assert_eq!(limit.max_frame_bytes(), 4 * 1024 * 1024);
        assert_eq!(hostile_buffer.as_slice(), hostile_header.as_slice());
    }

    fn sample_render_application_update() -> RenderApplicationUpdate {
        let pane_id = 7;
        RenderApplicationUpdate {
            identity: RenderApplicationIdentity {
                protocol_version: RENDER_APPLICATION_PROTOCOL_VERSION,
                token: RenderApplicationToken {
                    connection_generation: 11,
                    coordinator_instance: 13,
                    scheduler_sequence: 17,
                    attempt: 19,
                    ledger_instance: 23,
                    render_generation: 29,
                    ledger_obligation: 31,
                },
                pane_id,
                base_state: Some(RenderStateIdentity {
                    render_generation: 29,
                    state_sequence: 40,
                }),
                resulting_state: RenderStateIdentity {
                    render_generation: 29,
                    state_sequence: 41,
                },
                kind: RenderApplicationKind::Delta,
            },
            retry_budget: RenderApplicationRetryBudget {
                attempt_ordinal: 1,
                max_attempts: 3,
                remaining_millis: 250,
            },
            surface: GetPaneRenderChangesResponse {
                pane_id,
                mouse_grabbed: false,
                alt_screen_active: false,
                cursor_position: StableCursorPosition::default(),
                dimensions: RenderableDimensions {
                    cols: 80,
                    viewport_rows: 24,
                    scrollback_rows: 24,
                    physical_top: 0,
                    scrollback_top: 0,
                    dpi: 96,
                    pixel_width: 800,
                    pixel_height: 480,
                    reverse_video: false,
                },
                tiered_scrollback_status: None,
                dirty_lines: Vec::new(),
                title: "render-application".to_string(),
                working_dir: None,
                bonus_lines: SerializedLines::default(),
                input_serial: None,
                seqno: 41,
            },
            semantic_zones: RenderComponentUpdate::Unchanged,
            palette: RenderComponentUpdate::Unchanged,
            alerts: Vec::new(),
            connection_identity: RenderConnectionIdentity::new(
                TopologyStreamId::from_bytes([0x35; 16]),
                MuxSessionIncarnation::from_bytes([0x57; 16]),
            ),
        }
    }

    #[test]
    fn render_application_v2_uses_distinct_wire_ids_and_preserves_v1_schemas() {
        let update = sample_render_application_update();
        let mut legacy_identity = update.identity;
        legacy_identity.protocol_version = 1;
        let legacy_update = RenderApplicationUpdateV1 {
            identity: legacy_identity,
            retry_budget: update.retry_budget,
            surface: update.surface.clone(),
            semantic_zones: update.semantic_zones.clone(),
            palette: update.palette.clone(),
            alerts: update.alerts.clone(),
        };

        let mut legacy_frame = Vec::new();
        Pdu::RenderApplicationUpdateV1(legacy_update.clone())
            .encode_with_mode(&mut legacy_frame, 17, CompressionMode::Never)
            .expect("legacy render update should frame");
        assert_eq!(
            decode_raw(legacy_frame.as_slice())
                .expect("legacy update frame should decode")
                .ident,
            79
        );
        let decoded =
            Pdu::decode(legacy_frame.as_slice()).expect("v50 must retain the v1 PDU 79 schema");
        assert_eq!(decoded.serial, 17);
        assert_eq!(
            decoded.pdu,
            Pdu::RenderApplicationUpdateV1(legacy_update)
        );

        let mut current_frame = Vec::new();
        Pdu::RenderApplicationUpdate(update.clone())
            .encode_with_mode(&mut current_frame, 18, CompressionMode::Never)
            .expect("v2 render update should frame");
        assert_eq!(
            decode_raw(current_frame.as_slice())
                .expect("v2 update frame should decode")
                .ident,
            84
        );
        let decoded =
            Pdu::decode(current_frame.as_slice()).expect("v50 must decode the v2 PDU 84 schema");
        assert_eq!(decoded.serial, 18);
        assert_eq!(decoded.pdu, Pdu::RenderApplicationUpdate(update.clone()));

        let result = RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Applied {
                applied_state: update.identity.resulting_state,
            },
            connection_identity: update.connection_identity,
        };
        let legacy_result = RenderApplicationResultV1 {
            identity: legacy_identity,
            outcome: result.outcome,
        };
        let mut legacy_frame = Vec::new();
        Pdu::RenderApplicationResultV1(legacy_result)
            .encode_with_mode(&mut legacy_frame, 19, CompressionMode::Never)
            .expect("legacy render result should frame");
        assert_eq!(
            decode_raw(legacy_frame.as_slice())
                .expect("legacy result frame should decode")
                .ident,
            80
        );
        let decoded =
            Pdu::decode(legacy_frame.as_slice()).expect("v50 must retain the v1 PDU 80 schema");
        assert_eq!(decoded.serial, 19);
        assert_eq!(decoded.pdu, Pdu::RenderApplicationResultV1(legacy_result));

        let mut current_frame = Vec::new();
        Pdu::RenderApplicationResult(result)
            .encode_with_mode(&mut current_frame, 20, CompressionMode::Never)
            .expect("v2 render result should frame");
        assert_eq!(
            decode_raw(current_frame.as_slice())
                .expect("v2 result frame should decode")
                .ident,
            85
        );
        let decoded =
            Pdu::decode(current_frame.as_slice()).expect("v50 must decode the v2 PDU 85 schema");
        assert_eq!(decoded.serial, 20);
        assert_eq!(decoded.pdu, Pdu::RenderApplicationResult(result));
        assert_eq!(RENDER_APPLICATION_V2_MIN_CODEC_VERSION, 50);
    }

    #[test]
    fn render_application_update_and_result_roundtrip_exact_identity() {
        let update = sample_render_application_update();
        update.validate().expect("sample update is valid");
        let update_pdu = Pdu::RenderApplicationUpdate(update.clone());
        let mut encoded_update = Vec::new();
        update_pdu
            .encode(&mut encoded_update, 0)
            .expect("render update encodes");
        let decoded_update = Pdu::decode(encoded_update.as_slice()).expect("render update decodes");
        assert_eq!(decoded_update.serial, 0);
        assert_eq!(decoded_update.pdu.pane_id(), Some(7));
        assert_eq!(decoded_update.pdu, update_pdu);

        let result = RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Applied {
                applied_state: update.identity.resulting_state,
            },
            connection_identity: update.connection_identity,
        };
        result
            .validate_for(&update)
            .expect("matching post-application ACK is valid");
        let result_pdu = Pdu::RenderApplicationResult(result);
        let mut encoded_result = Vec::new();
        result_pdu
            .encode(&mut encoded_result, 43)
            .expect("render result encodes");
        let decoded_result =
            Pdu::decode(encoded_result.as_slice()).expect("render result decodes");
        assert_eq!(decoded_result.serial, 43);
        assert_eq!(decoded_result.pdu.pane_id(), Some(7));
        assert_eq!(decoded_result.pdu, result_pdu);
    }

    #[test]
    fn render_application_requires_complete_bounded_dirty_surface() {
        let mut update = sample_render_application_update();
        update.surface.dirty_lines = std::iter::once(0..1).collect();
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::Lines,
            })
        );

        let line = Line::from_text(
            "complete",
            &termwiz::cell::CellAttributes::default(),
            update.surface.seqno,
            None,
        );
        update.surface.bonus_lines = SerializedLines::from(vec![(0, line)]);
        update
            .validate()
            .expect("every dirty row is carried in the atomic surface payload");

        let over_limit = MAX_RENDER_APPLICATION_LINES + 1;
        update.surface.dimensions.cols = 1;
        update.surface.dimensions.viewport_rows = over_limit;
        update.surface.dimensions.scrollback_rows = over_limit;
        update.surface.dirty_lines =
            std::iter::once(0..isize::try_from(over_limit).expect("test limit fits isize"))
                .collect();
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Lines,
                requested: u64::try_from(over_limit).expect("test limit fits u64"),
                limit: u64::try_from(MAX_RENDER_APPLICATION_LINES)
                    .expect("test limit fits u64"),
            })
        );
    }

    #[test]
    fn render_application_rejects_incoherent_dimensions_before_apply() {
        let mut update = sample_render_application_update();
        update.surface.dimensions.scrollback_rows += 1;
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::Dimensions,
            })
        );
    }

    #[test]
    fn render_application_snapshot_is_complete_before_ack() {
        let mut snapshot = sample_render_application_update();
        snapshot.identity.kind = RenderApplicationKind::Snapshot;
        snapshot.identity.base_state = None;
        assert_eq!(
            snapshot.validate(),
            Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::SemanticZones,
            })
        );

        snapshot.semantic_zones =
            RenderComponentUpdate::Replace(GetSemanticZonesResponse {
                pane_id: snapshot.identity.pane_id,
                zones: Vec::new(),
                zone_texts: Vec::new(),
                last_exit_code: None,
            });
        assert_eq!(
            snapshot.validate(),
            Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::Palette,
            })
        );

        snapshot.palette = RenderComponentUpdate::Replace(SetPalette {
            pane_id: snapshot.identity.pane_id,
            palette: ColorPalette::default(),
        });
        assert_eq!(
            snapshot.validate(),
            Err(RenderApplicationContractError::MalformedSurfaceComponent {
                component: RenderApplicationComponent::Lines,
            })
        );

        let snapshot_seqno = snapshot.surface.seqno;
        snapshot.surface.bonus_lines = SerializedLines::from(
            (0isize..24)
                .map(|row| (row, Line::with_width(80, snapshot_seqno)))
                .collect::<Vec<_>>(),
        );
        snapshot
            .validate()
            .expect("snapshot carries the complete viewport and authoritative components");
    }

    #[test]
    fn render_application_bounds_aggregate_alert_text() {
        let mut update = sample_render_application_update();
        update.alerts.push(NotifyAlert {
            pane_id: update.identity.pane_id,
            alert: Alert::WindowTitleChanged(
                "x".repeat(MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES + 1),
            ),
        });
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Alerts,
                requested: u64::try_from(MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES + 1)
                    .expect("test limit fits u64"),
                limit: u64::try_from(MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES)
                    .expect("test limit fits u64"),
            })
        );
    }

    #[test]
    fn render_application_contract_rejects_ambiguous_or_cross_pane_authority() {
        let mut update = sample_render_application_update();
        update.connection_identity = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0; 16]),
            MuxSessionIncarnation::from_bytes([0x57; 16]),
        );
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::ReservedConnectionIdentity)
        );

        let mut update = sample_render_application_update();
        update.identity.token.coordinator_instance = 0;
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::ZeroAuthorityIdentity)
        );

        let mut update = sample_render_application_update();
        update.surface.pane_id = 8;
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::ComponentPaneMismatch)
        );

        let mut update = sample_render_application_update();
        update.identity.resulting_state.state_sequence = 40;
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::NonAdvancingDelta)
        );

        let mut update = sample_render_application_update();
        update.identity.kind = RenderApplicationKind::Snapshot;
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::SnapshotHasBase)
        );
    }

    #[test]
    fn render_application_settlement_rejects_stale_ack_and_incomplete_nack() {
        let update = sample_render_application_update();
        let mut stale_identity = update.identity;
        stale_identity.token.attempt += 1;
        let stale = RenderApplicationResult {
            identity: stale_identity,
            outcome: RenderApplicationOutcome::Applied {
                applied_state: stale_identity.resulting_state,
            },
            connection_identity: update.connection_identity,
        };
        assert_eq!(
            stale.validate_for(&update),
            Err(RenderApplicationContractError::SettlementIdentityMismatch)
        );

        let foreign_connection = RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Applied {
                applied_state: update.identity.resulting_state,
            },
            connection_identity: RenderConnectionIdentity::new(
                TopologyStreamId::from_bytes([0x71; 16]),
                MuxSessionIncarnation::from_bytes([0x73; 16]),
            ),
        };
        assert_eq!(
            foreign_connection.validate_for(&update),
            Err(RenderApplicationContractError::SettlementIdentityMismatch)
        );

        let wrong_state = RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Applied {
                applied_state: RenderStateIdentity {
                    render_generation: update.identity.resulting_state.render_generation,
                    state_sequence: update.identity.resulting_state.state_sequence + 1,
                },
            },
            connection_identity: update.connection_identity,
        };
        assert_eq!(
            wrong_state.validate_for(&update),
            Err(RenderApplicationContractError::AppliedStateMismatch)
        );

        let incomplete_nack = RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Nack(RenderApplicationNack {
                reason: RenderApplicationNackReason::BaseMismatch,
                observed_state: RenderApplicationObservedState::NotApplicable,
            }),
            connection_identity: update.connection_identity,
        };
        assert_eq!(
            incomplete_nack.validate_for(&update),
            Err(RenderApplicationContractError::NackMissingObservedState)
        );
    }

    #[test]
    fn render_application_nack_recovery_is_exhaustive_and_bounded() {
        let resync = [
            RenderApplicationNackReason::BaseMismatch,
            RenderApplicationNackReason::GenerationMismatch,
            RenderApplicationNackReason::MalformedOrIncomplete {
                component: RenderApplicationComponent::Surface,
            },
            RenderApplicationNackReason::DetectedGap,
        ];
        assert!(resync.iter().copied().all(|reason| {
            reason.recovery() == RenderApplicationNackRecovery::AuthoritativeResync
        }));
        assert_eq!(
            RenderApplicationNackReason::ApplicationFailure {
                stage: RenderApplicationStage::ApplySurface,
            }
            .recovery(),
            RenderApplicationNackRecovery::BoundedRetry
        );
        assert_eq!(
            RenderApplicationNackReason::UnsupportedResource {
                resource: RenderApplicationResource::Images,
            }
            .recovery(),
            RenderApplicationNackRecovery::Terminal
        );
        assert_eq!(
            RenderApplicationNackReason::BoundedResourceRejected {
                resource: RenderApplicationResource::Lines,
                requested: 2,
                limit: 1,
            }
            .recovery(),
            RenderApplicationNackRecovery::Terminal
        );

        let mut update = sample_render_application_update();
        update.alerts = (0..=MAX_RENDER_APPLICATION_ALERTS)
            .map(|_| NotifyAlert {
                pane_id: update.identity.pane_id,
                alert: Alert::OutputSinceFocusLost,
            })
            .collect();
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::TooManyAlerts)
        );

        let mut update = sample_render_application_update();
        update.alerts = vec![
            NotifyAlert {
                pane_id: update.identity.pane_id,
                alert: Alert::Progress(frankenterm_term::Progress::Percentage(42)),
            },
            NotifyAlert {
                pane_id: update.identity.pane_id,
                alert: Alert::Progress(frankenterm_term::Progress::Percentage(64)),
            },
        ];
        assert_eq!(
            update.validate(),
            Err(RenderApplicationContractError::DuplicateStateAlert)
        );
    }

    #[test]
    fn render_application_serialized_lines_reject_corrupt_internal_references() {
        let line = || {
            Line::from_text(
                "x",
                &termwiz::cell::CellAttributes::default(),
                1,
                None,
            )
        };
        let duplicate_row = SerializedLines {
            lines: vec![(0, line()), (0, line())],
            hyperlinks: Vec::new(),
            images: Vec::new(),
        };
        assert_eq!(
            duplicate_row.validate_structure(),
            Err(SerializedLinesStructureError::DuplicateStableRow)
        );

        let missing_image_line = SerializedLines {
            lines: vec![(0, line())],
            hyperlinks: Vec::new(),
            images: vec![SerializedImageCell {
                line_idx: 1,
                cell_idx: 0,
                top_left: TextureCoordinate::new_f32(0.0, 0.0),
                bottom_right: TextureCoordinate::new_f32(1.0, 1.0),
                data_hash: [0x5a; 32],
                z_index: 0,
                padding_left: 0,
                padding_top: 0,
                padding_right: 0,
                padding_bottom: 0,
                image_id: None,
                placement_id: None,
            }],
        };
        assert_eq!(
            missing_image_line.validate_structure(),
            Err(SerializedLinesStructureError::ImageLineMissing)
        );

        let invalid_image_cell = SerializedLines {
            lines: vec![(0, line())],
            hyperlinks: Vec::new(),
            images: vec![SerializedImageCell {
                line_idx: 0,
                cell_idx: 1,
                top_left: TextureCoordinate::new_f32(0.0, 0.0),
                bottom_right: TextureCoordinate::new_f32(1.0, 1.0),
                data_hash: [0xa5; 32],
                z_index: 0,
                padding_left: 0,
                padding_top: 0,
                padding_right: 0,
                padding_bottom: 0,
                image_id: None,
                placement_id: None,
            }],
        };
        assert_eq!(
            invalid_image_cell.validate_structure(),
            Err(SerializedLinesStructureError::ImageCellOutOfRange)
        );
    }

    #[test]
    fn render_application_nack_observation_contract_is_fail_closed() {
        let update = sample_render_application_update();
        let base_mismatch = RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Nack(RenderApplicationNack {
                reason: RenderApplicationNackReason::BaseMismatch,
                observed_state: RenderApplicationObservedState::Uninitialized,
            }),
            connection_identity: update.connection_identity,
        };
        base_mismatch
            .validate_for(&update)
            .expect("uninitialized is an explicit base-mismatch observation");

        let unexpected_observation = RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Nack(RenderApplicationNack {
                reason: RenderApplicationNackReason::ApplicationFailure {
                    stage: RenderApplicationStage::Commit,
                },
                observed_state: RenderApplicationObservedState::Applied(
                    update.identity.resulting_state,
                ),
            }),
            connection_identity: update.connection_identity,
        };
        assert_eq!(
            unexpected_observation.validate_for(&update),
            Err(RenderApplicationContractError::NackHasUnexpectedObservedState)
        );

        let invalid_observation = RenderApplicationResult {
            identity: update.identity,
            outcome: RenderApplicationOutcome::Nack(RenderApplicationNack {
                reason: RenderApplicationNackReason::DetectedGap,
                observed_state: RenderApplicationObservedState::Applied(RenderStateIdentity {
                    render_generation: 0,
                    state_sequence: 1,
                }),
            }),
            connection_identity: update.connection_identity,
        };
        assert_eq!(
            invalid_observation.validate_for(&update),
            Err(RenderApplicationContractError::InvalidObservedState)
        );
    }

    #[test]
    fn get_pane_renderable_dimensions_response_roundtrip_preserves_tiered_scrollback_status() {
        let pdu = Pdu::GetPaneRenderableDimensionsResponse(GetPaneRenderableDimensionsResponse {
            pane_id: 7,
            cursor_position: StableCursorPosition {
                x: 3,
                y: 9,
                ..Default::default()
            },
            dimensions: RenderableDimensions {
                cols: 120,
                viewport_rows: 72,
                scrollback_rows: 200_000,
                physical_top: 0,
                scrollback_top: 0,
                dpi: 96,
                pixel_width: 1_920,
                pixel_height: 1_080,
                reverse_video: false,
            },
            tiered_scrollback_status: Some(sample_tiered_scrollback_status()),
        });

        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 0x54).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.serial, 0x54);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn set_floating_pane_z_pdu_roundtrip() {
        let pdu = Pdu::SetFloatingPaneZ(SetFloatingPaneZ {
            pane_id: 3,
            z_order: 99,
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 102).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn toggle_floating_pane_pdu_roundtrip() {
        for visible in [true, false] {
            let pdu = Pdu::ToggleFloatingPane(ToggleFloatingPane {
                pane_id: 5,
                visible,
            });
            let mut encoded = Vec::new();
            pdu.encode(&mut encoded, 103).unwrap();
            let decoded = Pdu::decode(encoded.as_slice()).unwrap();
            assert_eq!(decoded.pdu, pdu);
        }
    }

    #[test]
    fn remove_floating_pane_pdu_roundtrip() {
        let pdu = Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id: 99 });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 104).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn floating_pane_pdus_pdu_name() {
        assert_eq!(
            Pdu::CreateFloatingPane(CreateFloatingPane {
                tab_id: 0,
                pane_id: 0,
                rect: FloatingPaneRect {
                    left: 0,
                    top: 0,
                    width: 5,
                    height: 3,
                },
            })
            .pdu_name(),
            "CreateFloatingPane"
        );
        assert_eq!(
            Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id: 0 }).pdu_name(),
            "RemoveFloatingPane"
        );
    }

    // --- Swap layout and stack PDU roundtrip tests (ft-2dd4s.5) ---

    #[test]
    fn swap_to_layout_pdu_roundtrip() {
        let pdu = Pdu::SwapToLayout(SwapToLayout {
            tab_id: 1,
            layout_index: 3,
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 200).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.serial, 200);
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn set_layout_cycle_pdu_roundtrip() {
        let pdu = Pdu::SetLayoutCycle(SetLayoutCycle {
            tab_id: 2,
            layout_names: vec![
                "grid-4".to_string(),
                "main-side".to_string(),
                "stacked".to_string(),
            ],
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 201).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn cycle_stack_pdu_roundtrip() {
        for forward in [true, false] {
            let pdu = Pdu::CycleStack(CycleStack {
                tab_id: 1,
                slot_index: 0,
                forward,
            });
            let mut encoded = Vec::new();
            pdu.encode(&mut encoded, 202).unwrap();
            let decoded = Pdu::decode(encoded.as_slice()).unwrap();
            assert_eq!(decoded.pdu, pdu);
        }
    }

    #[test]
    fn select_stack_pane_pdu_roundtrip() {
        let pdu = Pdu::SelectStackPane(SelectStackPane {
            tab_id: 3,
            slot_index: 2,
            pane_index: 1,
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 203).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn update_pane_constraints_pdu_roundtrip() {
        let pdu = Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
            pane_id: 42,
            min_width: Some(10),
            max_width: None,
            min_height: Some(5),
            max_height: Some(50),
        });
        let mut encoded = Vec::new();
        pdu.encode(&mut encoded, 204).unwrap();
        let decoded = Pdu::decode(encoded.as_slice()).unwrap();
        assert_eq!(decoded.pdu, pdu);
    }

    #[test]
    fn frankenmux_pdus_pdu_name() {
        assert_eq!(
            Pdu::SwapToLayout(SwapToLayout {
                tab_id: 0,
                layout_index: 0,
            })
            .pdu_name(),
            "SwapToLayout"
        );
        assert_eq!(
            Pdu::SetLayoutCycle(SetLayoutCycle {
                tab_id: 0,
                layout_names: vec![],
            })
            .pdu_name(),
            "SetLayoutCycle"
        );
        assert_eq!(
            Pdu::CycleStack(CycleStack {
                tab_id: 0,
                slot_index: 0,
                forward: true,
            })
            .pdu_name(),
            "CycleStack"
        );
        assert_eq!(
            Pdu::SelectStackPane(SelectStackPane {
                tab_id: 0,
                slot_index: 0,
                pane_index: 0,
            })
            .pdu_name(),
            "SelectStackPane"
        );
        assert_eq!(
            Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                pane_id: 0,
                min_width: None,
                max_width: None,
                min_height: None,
                max_height: None,
            })
            .pdu_name(),
            "UpdatePaneConstraints"
        );
    }

    // ─── ft-e1emx: PDU conformance harness ────────────────────────────────
    //
    // Per docs/proposals (testing-conformance-harnesses skill): the existing
    // per-PDU roundtrip tests above cover *one* canonical encoding each.
    // This harness lifts that into a matrix that runs the same conformance
    // contract against every custom PDU type (IDs 63-76) under three axes:
    //
    //   1. canonical encode → decode equality
    //   2. encode is deterministic (encode-decode-encode is byte-stable)
    //   3. decode is robust to tail-padded input (extra bytes after the
    //      canonical frame must not corrupt the decoded PDU because the
    //      length-prefixed framing tells the decoder when to stop)
    //   4. all three CompressionMode variants produce equivalent decoded PDUs
    //
    // The harness also includes targeted regression guards for the
    // varbincode positional-format bug — `skip_serializing_if` on `Option`
    // fields silently misaligns the decoder. The guard tests assert that
    // `Option<T>::None` *does* contribute a tag byte to the encoded payload.

    /// Parameterized conformance contract checked against every entry in the
    /// matrix. Returns the canonical encoded bytes so caller-level guards
    /// (e.g. the skip_serializing_if regression check) can compare lengths
    /// across configurations.
    fn assert_pdu_conforms(label: &str, pdu: Pdu, serial: u64) -> anyhow::Result<Vec<u8>> {
        // 1. Canonical encode → decode equality.
        let mut canonical = Vec::new();
        pdu.encode(&mut canonical, serial)
            .with_context(|| format!("{label}: canonical encode failed"))?;
        let decoded = Pdu::decode(canonical.as_slice())
            .with_context(|| format!("{label}: canonical decode failed"))?;
        assert_eq!(decoded.pdu, pdu, "{label}: canonical roundtrip not equal");
        assert_eq!(decoded.serial, serial, "{label}: serial drift");

        // 2. Encode determinism — re-encode of decoded PDU must be byte-identical.
        let mut reencoded = Vec::new();
        decoded
            .pdu
            .encode(&mut reencoded, serial)
            .with_context(|| format!("{label}: re-encode failed"))?;
        assert_eq!(
            canonical, reencoded,
            "{label}: encode-decode-encode not byte-stable"
        );

        // 3. Tail-padded decode robustness. The frame is length-prefixed so
        // trailing bytes after the canonical encoding belong to a *different*
        // logical frame. `Pdu::decode` reads only what the length header
        // says; arbitrary tail bytes must not corrupt the result.
        for tail in [vec![0x00u8], vec![0xFFu8; 16], b"GARBAGE_TAIL".to_vec()] {
            let mut padded = canonical.clone();
            padded.extend_from_slice(&tail);
            let decoded_padded = Pdu::decode(padded.as_slice()).with_context(|| {
                format!(
                    "{label}: tail-padded decode failed (tail={} bytes)",
                    tail.len()
                )
            })?;
            assert_eq!(
                decoded_padded.pdu, pdu,
                "{label}: tail-padded decode produced different PDU"
            );
        }

        // 4. Compression-mode invariance — every mode must decode back to
        // the same logical PDU even if the on-wire bytes differ.
        for mode in [
            CompressionMode::Auto,
            CompressionMode::Never,
            CompressionMode::Always,
        ] {
            let mut encoded = Vec::new();
            pdu.encode_with_mode(&mut encoded, serial, mode)
                .with_context(|| format!("{label}: encode_with_mode({mode:?}) failed"))?;
            let d = Pdu::decode(encoded.as_slice())
                .with_context(|| format!("{label}: decode after {mode:?} failed"))?;
            assert_eq!(
                d.pdu, pdu,
                "{label}: compression mode {mode:?} altered semantic payload"
            );
        }

        Ok(canonical)
    }

    /// ft-e1emx: drive the conformance contract across the full custom-PDU
    /// matrix (IDs 63-72) plus a representative all-None UpdatePaneConstraints
    /// to exercise the Option<T> tag-byte path that the varbincode positional
    /// format depends on.
    #[test]
    fn custom_pdu_conformance_matrix() -> anyhow::Result<()> {
        let cases: Vec<(&str, Pdu)> = vec![
            (
                "MoveFloatingPane",
                Pdu::MoveFloatingPane(MoveFloatingPane {
                    pane_id: 7,
                    rect: FloatingPaneRect {
                        left: 0,
                        top: 0,
                        width: 80,
                        height: 24,
                    },
                }),
            ),
            (
                "SetFloatingPaneZ",
                Pdu::SetFloatingPaneZ(SetFloatingPaneZ {
                    pane_id: 7,
                    z_order: 3,
                }),
            ),
            (
                "ToggleFloatingPane",
                Pdu::ToggleFloatingPane(ToggleFloatingPane {
                    pane_id: 7,
                    visible: true,
                }),
            ),
            (
                "RemoveFloatingPane",
                Pdu::RemoveFloatingPane(RemoveFloatingPane { pane_id: 7 }),
            ),
            (
                "SwapToLayout",
                Pdu::SwapToLayout(SwapToLayout {
                    tab_id: 9,
                    layout_index: 2,
                }),
            ),
            (
                "SetLayoutCycle/empty",
                Pdu::SetLayoutCycle(SetLayoutCycle {
                    tab_id: 9,
                    layout_names: vec![],
                }),
            ),
            (
                "SetLayoutCycle/non_empty",
                Pdu::SetLayoutCycle(SetLayoutCycle {
                    tab_id: 9,
                    layout_names: vec!["main".into(), "split".into(), "stack".into()],
                }),
            ),
            (
                "CycleStack/forward",
                Pdu::CycleStack(CycleStack {
                    tab_id: 9,
                    slot_index: 1,
                    forward: true,
                }),
            ),
            (
                "CycleStack/backward",
                Pdu::CycleStack(CycleStack {
                    tab_id: 9,
                    slot_index: 1,
                    forward: false,
                }),
            ),
            (
                "SelectStackPane",
                Pdu::SelectStackPane(SelectStackPane {
                    tab_id: 9,
                    slot_index: 2,
                    pane_index: 4,
                }),
            ),
            (
                "UpdatePaneConstraints/all_none",
                Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                    pane_id: 42,
                    min_width: None,
                    max_width: None,
                    min_height: None,
                    max_height: None,
                }),
            ),
            (
                "UpdatePaneConstraints/all_some",
                Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                    pane_id: 42,
                    min_width: Some(10),
                    max_width: Some(200),
                    min_height: Some(5),
                    max_height: Some(60),
                }),
            ),
            (
                "UpdatePaneConstraints/mixed",
                Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
                    pane_id: 42,
                    min_width: Some(10),
                    max_width: None,
                    min_height: Some(5),
                    max_height: None,
                }),
            ),
            (
                "ListPanesTabStacks",
                Pdu::ListPanesTabStacks(ListPanesTabStacks {}),
            ),
            (
                "ListPanesTabStacksResponse/empty",
                Pdu::ListPanesTabStacksResponse(ListPanesTabStacksResponse {
                    tab_stack_entries: vec![],
                }),
            ),
            (
                "ListPanesTabStacksResponse/non_empty",
                Pdu::ListPanesTabStacksResponse(ListPanesTabStacksResponse {
                    tab_stack_entries: vec![ListPanesTabStackEntry {
                        window_id: 1,
                        stack_id: TabStackId(7),
                        tab_id: 9,
                        position: 2,
                        is_visible: true,
                    }],
                }),
            ),
        ];

        let mut serial: u64 = 0x1000;
        for (label, pdu) in cases {
            assert_pdu_conforms(label, pdu, serial)?;
            serial = serial.wrapping_add(1);
        }
        Ok(())
    }

    /// ft-e1emx (varbincode positional-format guard): every `Option<T>` field
    /// in a varbincode-encoded PDU must contribute a tag byte (0x00 for
    /// `None`, 0x01 for `Some`). If a future serde attribute (such as
    /// `skip_serializing_if = "Option::is_none"`) is reintroduced on an
    /// `Option<T>` field, the `None` payload will be a strict prefix of the
    /// `Some` payload — the encoded byte counts will become "all_some > all_none"
    /// minus the tag-byte saving, and the decoder will misalign. We assert
    /// the *invariant* that all-None and all-Some encodings have *different*
    /// length envelopes (a None can never produce zero extra bytes per
    /// field, because the tag byte is mandatory).
    #[test]
    fn update_pane_constraints_options_emit_tag_bytes() {
        let serial: u64 = 0xCAFE;

        let none_pdu = Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
            pane_id: 0,
            min_width: None,
            max_width: None,
            min_height: None,
            max_height: None,
        });
        let mut none_encoded = Vec::new();
        none_pdu
            .encode_with_mode(&mut none_encoded, serial, CompressionMode::Never)
            .expect("encode all-none");

        let some_pdu = Pdu::UpdatePaneConstraints(UpdatePaneConstraints {
            pane_id: 0,
            min_width: Some(7),
            max_width: Some(7),
            min_height: Some(7),
            max_height: Some(7),
        });
        let mut some_encoded = Vec::new();
        some_pdu
            .encode_with_mode(&mut some_encoded, serial, CompressionMode::Never)
            .expect("encode all-some");

        // All-some carries 4 extra leb128-ish u64 bodies + 4 Some tags;
        // all-none carries 4 None tags only. Strict inequality is the
        // skip_serializing_if regression signal: equality would mean None
        // was serialized to *zero* bytes, which is the corruption mode.
        assert_ne!(
            none_encoded.len(),
            some_encoded.len(),
            "skip_serializing_if regression: None and Some encode to identical lengths"
        );
        assert!(
            some_encoded.len() > none_encoded.len(),
            "all-Some payload should be larger than all-None (none={}, some={})",
            none_encoded.len(),
            some_encoded.len()
        );

        // And both must roundtrip cleanly through the decoder. The
        // misalignment failure mode would surface here as a decode error
        // because subsequent fields would be parsed at the wrong offset.
        assert_eq!(Pdu::decode(none_encoded.as_slice()).unwrap().pdu, none_pdu);
        assert_eq!(Pdu::decode(some_encoded.as_slice()).unwrap().pdu, some_pdu);
    }

    // ─── Positional wire-format mutation guards ────────────────────────────
    //
    // Bytes appended after a complete framed PDU are outside the length
    // prefix. Ignoring them is a single-frame reader property, not evidence
    // that an older payload can satisfy a newer positional struct schema.
    // Real compatibility tests must encode both actual schemas inside their
    // frame boundaries, as the PDU 27 and render-v1/v2 tests above do.

    /// A single-frame decoder leaves bytes after the framed PDU untouched.
    #[test]
    fn bytes_after_complete_frame_do_not_change_single_frame_decode() -> anyhow::Result<()> {
        let pdu = Pdu::SetLayoutCycle(SetLayoutCycle {
            tab_id: 9,
            layout_names: vec!["main".to_string(), "split".to_string()],
        });
        let mut encoded = Vec::new();
        pdu.encode_with_mode(&mut encoded, 0xCAFE, CompressionMode::Never)
            .context("canonical encode must succeed")?;

        for tail in [
            vec![0x42_u8, 0x42, 0x42, 0x42],
            vec![0x00_u8],
            vec![0xFF_u8; 32],
        ] {
            let mut framed = encoded.clone();
            framed.extend_from_slice(&tail);

            let decoded = Pdu::decode(framed.as_slice()).with_context(|| {
                format!(
                    "single-frame decode failed with {} following bytes",
                    tail.len()
                )
            })?;
            assert_eq!(
                decoded.pdu,
                pdu,
                "following bytes changed the decoded PDU (tail={} bytes)",
                tail.len()
            );
            assert_eq!(
                decoded.serial,
                0xCAFE,
                "following bytes changed the serial (tail={} bytes)",
                tail.len()
            );
        }
        Ok(())
    }

    /// A byte inserted inside a positional frame must not silently preserve
    /// the canonical PDU.
    #[test]
    fn interior_byte_insert_does_not_silently_decode_canonically() -> anyhow::Result<()> {
        let pdu = Pdu::SetLayoutCycle(SetLayoutCycle {
            tab_id: 9,
            layout_names: vec!["main".to_string(), "split".to_string()],
        });
        let mut encoded = Vec::new();
        pdu.encode_with_mode(&mut encoded, 0xCAFE, CompressionMode::Never)
            .context("canonical encode must succeed")?;

        // Try every interior byte position. For each, inject a single
        // byte and assert that the decoder either fails OR produces a
        // PDU that is *not* equal to the canonical one. The forbidden
        // outcome is "decode succeeds AND result == canonical PDU" —
        // that would mean a positional drift was silently absorbed.
        //
        // We skip position 0 (frame length prefix; corrupting it is
        // detected by the framing layer, not interesting here) and
        // position encoded.len() (= bytes after the complete frame, covered by
        // the preceding single-frame reader test).
        let mut detected_count = 0usize;
        for insert_pos in 1..encoded.len() {
            let mut corrupted = encoded.clone();
            corrupted.insert(insert_pos, 0xAB);

            match Pdu::decode(corrupted.as_slice()) {
                Ok(decoded) => {
                    assert!(
                        decoded.pdu != pdu || decoded.serial != 0xCAFE,
                        "middle-insert at byte {} silently decoded \
                         to canonical PDU — varbincode positional drift not detected",
                        insert_pos
                    );
                    // Decoded but to a different PDU: detection counts.
                    detected_count += 1;
                }
                Err(_) => {
                    // Decoder errored out: detection counts.
                    detected_count += 1;
                }
            }
        }
        // Sanity: every interior position must have been detected.
        assert_eq!(
            detected_count,
            encoded.len() - 1,
            "every interior insertion position must surface as detect-or-error"
        );
        Ok(())
    }
}
