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
#[cfg(test)]
use mux::tab::PaneEntry;
use mux::tab::{
    FloatingPaneRect, PaneArena, PaneArenaNode, PaneArenaTree, PaneArenaWindowTitle, PaneNode,
    SerdeUrl, SplitRequest, TabId, TabStackId,
};
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

/// Maximum decoded pixel bytes admitted for one ordinary image-hydration
/// batch. The client also uses this as the per-image encoded-byte ceiling.
pub const MAX_IMAGE_HYDRATION_DECODED_BYTES: usize = termwiz::image::MAX_IMAGE_WIRE_BYTES;
/// Bounded wire body for one GetImageCellResponse: the image budget plus a
/// fixed envelope for enum/vector/animation metadata and varbincode framing.
pub const MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES: usize =
    MAX_IMAGE_HYDRATION_DECODED_BYTES + 1024 * 1024;
/// Worst-case zstd encoder output for a legal response body. For inputs above
/// 128 KiB, zstd's compress-bound formula is `size + (size >> 8)`.
pub const MAX_GET_IMAGE_CELL_RESPONSE_ZSTD_ENCODED_BYTES: usize =
    MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES
        + (MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES >> 8);
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

/// Turn an owned serialized payload into its framed representation without
/// allocating and copying a second payload-sized vector.
///
/// The payload is shifted by the small LEB128 header inside its existing
/// allocation. This retains the single contiguous write required by the wire
/// path while avoiding the peak live allocation of `payload + frame`.
fn prepend_frame_header_to_owned_payload(
    ident: u64,
    serial: u64,
    mut data: Vec<u8>,
    is_compressed: bool,
    record_metrics: bool,
) -> anyhow::Result<Vec<u8>> {
    let body_len = data
        .len()
        .checked_add(encoded_length(ident))
        .and_then(|len| len.checked_add(encoded_length(serial)))
        .context("encoded PDU body length overflow")?;
    let body_len_u64 = u64::try_from(body_len).context("encoded PDU length does not fit in u64")?;
    let masked_len = if is_compressed {
        body_len_u64 | COMPRESSED_MASK
    } else {
        body_len_u64
    };
    let frame_len = encoded_frame_len(ident, serial, data.len(), is_compressed)?;
    let header_len = frame_len
        .checked_sub(data.len())
        .context("encoded PDU header length underflow")?;
    let payload_len = data.len();
    data.try_reserve_exact(header_len)
        .context("reserving owned PDU frame header")?;
    data.resize(frame_len, 0);
    data.copy_within(0..payload_len, header_len);

    let mut header = &mut data[..header_len];
    leb128::write::unsigned(&mut header, masked_len).context("writing pdu len")?;
    leb128::write::unsigned(&mut header, serial).context("writing pdu serial")?;
    leb128::write::unsigned(&mut header, ident).context("writing pdu ident")?;
    debug_assert!(header.is_empty());

    if record_metrics {
        if is_compressed {
            metrics::histogram!("pdu.encode.compressed.size").record(data.len() as f64);
        } else {
            metrics::histogram!("pdu.encode.size").record(data.len() as f64);
        }
    }
    Ok(data)
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

/// An encoded body crossed the byte ceiling published by its wire schema.
///
/// A decoder reports the length declared by a validated frame header before it
/// reserves or materializes payload storage. A producer reports the first
/// cumulative serialization length that crossed the same limit. Unknown and
/// legacy identifiers retain [`MAX_PDU_SIZE`]; closed authority schemas may
/// publish a tighter limit in [`PDU_WIRE_SPECS`].
#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "PDU encoded payload size {declared_payload_bytes} exceeds maximum \
     {max_payload_bytes} (serial={serial} ident={ident} compressed={is_compressed})"
)]
pub struct PduEncodedBodyLimitExceeded {
    declared_payload_bytes: usize,
    max_payload_bytes: usize,
    serial: u64,
    ident: u64,
    is_compressed: bool,
}

impl PduEncodedBodyLimitExceeded {
    #[must_use]
    pub const fn declared_payload_bytes(&self) -> usize {
        self.declared_payload_bytes
    }

    #[must_use]
    pub const fn max_payload_bytes(&self) -> usize {
        self.max_payload_bytes
    }

    #[must_use]
    pub const fn serial(&self) -> u64 {
        self.serial
    }

    #[must_use]
    pub const fn ident(&self) -> u64 {
        self.ident
    }

    #[must_use]
    pub const fn is_compressed(&self) -> bool {
        self.is_compressed
    }
}

fn read_buffered_header_u64(
    input: &mut &[u8],
    frame_complete: bool,
    field_context: &'static str,
) -> anyhow::Result<Option<(u64, usize)>> {
    let before = input.len();
    match leb128::read::unsigned(input) {
        Ok(value) => Ok(Some((value, before.saturating_sub(input.len())))),
        Err(leb128::read::Error::IoError(err))
            if !frame_complete
                && matches!(
                    err.kind(),
                    std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::WouldBlock
                ) =>
        {
            Ok(None)
        }
        Err(leb128::read::Error::IoError(err)) => Err(anyhow::Error::new(err)
            .context("reading leb128")
            .context(field_context)),
        Err(leb128::read::Error::Overflow) => {
            Err(anyhow::anyhow!("leb128 is too large").context(field_context))
        }
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

    let is_compressed = (tagged_len & COMPRESSED_MASK) != 0;
    let raw_len = tagged_len & !COMPRESSED_MASK;
    let frame_body_len: usize = raw_len
        .try_into()
        .map_err(|_| anyhow::anyhow!("buffered PDU length {raw_len} does not fit in usize"))?;

    let prefix_len = buffer.len().saturating_sub(slice.len());
    let total_len = prefix_len
        .checked_add(frame_body_len)
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
    if frame_body_len > MAX_PDU_SIZE {
        anyhow::bail!(
            "buffered PDU payload size {} exceeds maximum {} — refusing to accumulate",
            frame_body_len,
            MAX_PDU_SIZE,
        );
    }

    // Parse only inside the first frame's declared bounds. Coalesced bytes from
    // a successor must never complete a truncated serial or identifier.
    let frame_complete = buffer.len() >= total_len;
    let available_end = buffer.len().min(total_len);
    let mut header = buffer
        .get(prefix_len..available_end)
        .context("buffered PDU header range is invalid")?;
    let Some((serial, serial_len)) =
        read_buffered_header_u64(&mut header, frame_complete, "reading PDU serial")?
    else {
        return Ok(None);
    };
    let Some((ident, ident_len)) =
        read_buffered_header_u64(&mut header, frame_complete, "reading PDU ident")?
    else {
        return Ok(None);
    };
    let data_len = decoded_payload_len(
        "buffered PDU",
        raw_len,
        serial,
        serial_len,
        ident,
        ident_len,
    )?;
    validate_encoded_body_admission(data_len, serial, ident, is_compressed)?;

    if !frame_complete {
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

    /// Registry-derived ceiling already applied to this header before it was
    /// exposed to a body selector.
    #[must_use]
    pub fn maximum_encoded_payload_bytes(&self) -> usize {
        Pdu::wire_spec_for_ident(self.ident)
            .map(|spec| {
                spec.encoded_body_limit
                    .maximum_encoded_payload_bytes(self.is_compressed)
            })
            .unwrap_or(MAX_PDU_SIZE)
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

fn validate_encoded_body_admission(
    data_len: usize,
    serial: u64,
    ident: u64,
    is_compressed: bool,
) -> anyhow::Result<()> {
    let max_payload_bytes = Pdu::wire_spec_for_ident(ident)
        .map(|spec| {
            spec.encoded_body_limit
                .maximum_encoded_payload_bytes(is_compressed)
        })
        .unwrap_or(MAX_PDU_SIZE);
    if data_len > max_payload_bytes {
        return Err(PduEncodedBodyLimitExceeded {
            declared_payload_bytes: data_len,
            max_payload_bytes,
            serial,
            ident,
            is_compressed,
        }
        .into());
    }
    Ok(())
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

    validate_encoded_body_admission(data_len, serial, ident, is_compressed)?;

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
        let read_len = header.data_len.saturating_sub(consumed).min(scratch.len());
        r.read_exact(&mut scratch[..read_len])
            .await
            .with_context(|| {
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

    validate_encoded_body_admission(data_len, serial, ident, is_compressed)?;

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

struct SerializedPayload {
    data: Vec<u8>,
    is_compressed: bool,
    uncompressed_len: usize,
}

#[cfg(test)]
std::thread_local! {
    static TEST_SERIALIZE_INVOCATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static TEST_BOUNDED_SERIALIZE_GROWTH_EVENTS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static TEST_BOUNDED_SERIALIZE_BUFFER_CONSTRUCTIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static TEST_BOUNDED_SERIALIZE_MAX_REQUESTED_CAPACITY: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static TEST_COMPRESSION_INVOCATIONS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
    static TEST_EXACT_RENDER_VALIDATION_ROW_VISITS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

#[cfg(test)]
fn reset_test_serialize_invocations() {
    TEST_SERIALIZE_INVOCATIONS.set(0);
}

#[cfg(test)]
fn test_serialize_invocations() -> usize {
    TEST_SERIALIZE_INVOCATIONS.get()
}

#[cfg(test)]
fn record_test_serialize_invocation() {
    TEST_SERIALIZE_INVOCATIONS.set(TEST_SERIALIZE_INVOCATIONS.get().saturating_add(1));
}

#[cfg(test)]
fn reset_test_bounded_serialize_growth_events() {
    TEST_BOUNDED_SERIALIZE_BUFFER_CONSTRUCTIONS.set(0);
    TEST_BOUNDED_SERIALIZE_GROWTH_EVENTS.set(0);
    TEST_BOUNDED_SERIALIZE_MAX_REQUESTED_CAPACITY.set(0);
}

#[cfg(test)]
fn test_bounded_serialize_buffer_constructions() -> usize {
    TEST_BOUNDED_SERIALIZE_BUFFER_CONSTRUCTIONS.get()
}

#[cfg(test)]
fn test_bounded_serialize_growth_events() -> usize {
    TEST_BOUNDED_SERIALIZE_GROWTH_EVENTS.get()
}

#[cfg(test)]
fn test_bounded_serialize_max_requested_capacity() -> usize {
    TEST_BOUNDED_SERIALIZE_MAX_REQUESTED_CAPACITY.get()
}

#[cfg(test)]
fn reset_test_compression_invocations() {
    TEST_COMPRESSION_INVOCATIONS.set(0);
}

#[cfg(test)]
fn test_compression_invocations() -> usize {
    TEST_COMPRESSION_INVOCATIONS.get()
}

#[cfg(test)]
fn record_test_bounded_serialize_growth_event(requested_capacity: usize) {
    TEST_BOUNDED_SERIALIZE_GROWTH_EVENTS
        .set(TEST_BOUNDED_SERIALIZE_GROWTH_EVENTS.get().saturating_add(1));
    TEST_BOUNDED_SERIALIZE_MAX_REQUESTED_CAPACITY.set(
        TEST_BOUNDED_SERIALIZE_MAX_REQUESTED_CAPACITY
            .get()
            .max(requested_capacity),
    );
}

#[cfg(test)]
fn record_test_bounded_serialize_buffer_construction() {
    TEST_BOUNDED_SERIALIZE_BUFFER_CONSTRUCTIONS.set(
        TEST_BOUNDED_SERIALIZE_BUFFER_CONSTRUCTIONS
            .get()
            .saturating_add(1),
    );
}

#[cfg(test)]
fn record_test_compression_invocation() {
    TEST_COMPRESSION_INVOCATIONS.set(TEST_COMPRESSION_INVOCATIONS.get().saturating_add(1));
}

#[cfg(test)]
fn reset_test_exact_render_validation_row_visits() {
    TEST_EXACT_RENDER_VALIDATION_ROW_VISITS.set(0);
}

#[cfg(test)]
fn test_exact_render_validation_row_visits() -> usize {
    TEST_EXACT_RENDER_VALIDATION_ROW_VISITS.get()
}

#[cfg(test)]
fn record_test_exact_render_validation_row_visit() {
    TEST_EXACT_RENDER_VALIDATION_ROW_VISITS.set(
        TEST_EXACT_RENDER_VALIDATION_ROW_VISITS
            .get()
            .saturating_add(1),
    );
}

fn serialize_uncompressed<T: serde::Serialize>(t: &T) -> Result<Vec<u8>, Error> {
    #[cfg(test)]
    record_test_serialize_invocation();
    let mut uncompressed = Vec::with_capacity(64);
    let mut encode = varbincode::Serializer::new(&mut uncompressed);
    t.serialize(&mut encode)?;
    Ok(uncompressed)
}

struct BoundedSerializeBuffer {
    bytes: Vec<u8>,
    logical_len: usize,
    max_bytes: usize,
    exceeded: bool,
}

#[derive(Debug)]
enum CountingFailure {
    Encoding(Error),
    Overflow,
    LimitExceeded {
        declared_bytes: usize,
        max_bytes: usize,
    },
}

struct CheckedCountingWriter {
    logical_len: usize,
    max_bytes: usize,
    failure: Option<CountingFailure>,
}

impl CheckedCountingWriter {
    const fn new(max_bytes: usize) -> Self {
        Self {
            logical_len: 0,
            max_bytes,
            failure: None,
        }
    }
}

impl std::io::Write for CheckedCountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self.logical_len.checked_add(buffer.len()) else {
            self.failure = Some(CountingFailure::Overflow);
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "counted PDU body length overflow",
            ));
        };
        self.logical_len = next_len;
        if next_len > self.max_bytes {
            self.failure = Some(CountingFailure::LimitExceeded {
                declared_bytes: next_len,
                max_bytes: self.max_bytes,
            });
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "counted PDU body exceeded its logical limit",
            ));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn count_varbincode_value_raw<T: serde::Serialize>(
    value: &T,
    max_bytes: usize,
) -> Result<usize, CountingFailure> {
    let mut output = CheckedCountingWriter::new(max_bytes);
    let serialize_result = {
        let mut encoder = varbincode::Serializer::new(&mut output);
        value.serialize(&mut encoder)
    };
    if let Some(failure) = output.failure {
        return Err(failure);
    }
    serialize_result.map_err(|source| CountingFailure::Encoding(source.into()))?;
    Ok(output.logical_len)
}

std::thread_local! {
    /// Bytes elided only while the outbound counting serializer measures a
    /// nested ordered-window byte section.  The ordinary encoder never enters
    /// this scope and therefore retains byte-for-byte wire behavior.
    static OUTBOUND_COUNTING_STATE: std::cell::Cell<Option<OutboundCountingState>> =
        const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy)]
struct OutboundCountingState {
    elided_bytes: usize,
    ordered_window_section_bytes: Option<usize>,
}

struct OutboundCountingScope {
    active: bool,
}

impl OutboundCountingScope {
    fn enter(ident: u64) -> Result<Self, PduOutboundPlanError> {
        OUTBOUND_COUNTING_STATE.with(|state| {
            if let Some(previous) = state.replace(Some(OutboundCountingState {
                elided_bytes: 0,
                ordered_window_section_bytes: None,
            })) {
                state.set(Some(previous));
                return Err(PduOutboundPlanError::NestedCountingScope { ident });
            }
            Ok(Self { active: true })
        })
    }

    fn finish(mut self, ident: u64) -> Result<OutboundCountingState, PduOutboundPlanError> {
        let state = OUTBOUND_COUNTING_STATE.with(|state| state.replace(None));
        self.active = false;
        state.ok_or(PduOutboundPlanError::NestedCountingScope { ident })
    }
}

impl Drop for OutboundCountingScope {
    fn drop(&mut self) {
        if self.active {
            OUTBOUND_COUNTING_STATE.with(|state| state.set(None));
        }
    }
}

fn record_outbound_counting_ordered_section(
    section_bytes: usize,
    encoded_section_bytes: usize,
) -> Result<bool, &'static str> {
    OUTBOUND_COUNTING_STATE.with(|state| {
        let Some(mut current) = state.get() else {
            return Ok(false);
        };
        if current.ordered_window_section_bytes.is_some() {
            return Err("outbound PDU contains multiple ordered-window byte sections");
        }
        current.elided_bytes = current
            .elided_bytes
            .checked_add(encoded_section_bytes)
            .ok_or("outbound counting elided-byte overflow")?;
        current.ordered_window_section_bytes = Some(section_bytes);
        state.set(Some(current));
        Ok(true)
    })
}

struct CountedPduPayload {
    logical_payload_bytes: usize,
    ordered_window_section_bytes: Option<usize>,
}

std::thread_local! {
    /// Exact nested-section authority installed only while the bounded
    /// prepared encoder writes one validated PDU synchronously.
    static OUTBOUND_DIRECT_ORDERED_SECTION: std::cell::Cell<Option<DirectOrderedSectionState>> =
        const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy)]
struct DirectOrderedSectionState {
    section_bytes: usize,
    consumed: bool,
}

struct DirectOrderedSectionScope {
    active: bool,
}

impl DirectOrderedSectionScope {
    fn enter(ident: u64, section_bytes: Option<usize>) -> Result<Self, PduOutboundEncodeError> {
        let Some(section_bytes) = section_bytes else {
            return Ok(Self { active: false });
        };
        OUTBOUND_DIRECT_ORDERED_SECTION.with(|state| {
            if let Some(previous) = state.replace(Some(DirectOrderedSectionState {
                section_bytes,
                consumed: false,
            })) {
                state.set(Some(previous));
                return Err(PduOutboundEncodeError::Codec {
                    ident,
                    stage: "direct ordered-section admission",
                    cause: anyhow::anyhow!("nested direct ordered-section scope"),
                });
            }
            Ok(Self { active: true })
        })
    }

    fn finish(mut self, ident: u64) -> Result<(), PduOutboundEncodeError> {
        if !self.active {
            return Ok(());
        }
        let state = OUTBOUND_DIRECT_ORDERED_SECTION.with(|state| state.replace(None));
        self.active = false;
        let state = state.ok_or_else(|| PduOutboundEncodeError::Codec {
            ident,
            stage: "direct ordered-section finalization",
            cause: anyhow::anyhow!("direct ordered-section scope disappeared"),
        })?;
        if !state.consumed {
            return Err(PduOutboundEncodeError::PlanMismatch {
                ident,
                field: "ordered_window_section_bytes",
                planned: state.section_bytes,
                actual: 0,
            });
        }
        Ok(())
    }
}

impl Drop for DirectOrderedSectionScope {
    fn drop(&mut self) {
        if self.active {
            OUTBOUND_DIRECT_ORDERED_SECTION.with(|state| state.set(None));
        }
    }
}

fn count_pdu_payload<T: serde::Serialize>(
    value: &T,
    max_payload_bytes: usize,
    ident: u64,
) -> Result<CountedPduPayload, PduOutboundPlanError> {
    let scope = OutboundCountingScope::enter(ident)?;
    let counted = count_varbincode_value_raw(value, max_payload_bytes);
    let counting_state = scope.finish(ident)?;
    let counted = match counted {
        Ok(counted) => counted,
        Err(CountingFailure::Encoding(cause)) => {
            return Err(PduOutboundPlanError::CountingSerialization { ident, cause });
        }
        Err(CountingFailure::Overflow) => {
            return Err(PduOutboundPlanError::ArithmeticOverflow {
                ident,
                field: "logical_payload_bytes",
            });
        }
        Err(CountingFailure::LimitExceeded {
            declared_bytes,
            max_bytes,
        }) => {
            return Err(PduOutboundPlanError::LogicalPayloadLimitExceeded {
                ident,
                declared_payload_bytes: declared_bytes,
                max_payload_bytes: max_bytes,
            });
        }
    };
    let logical_payload_bytes = counted.checked_add(counting_state.elided_bytes).ok_or(
        PduOutboundPlanError::ArithmeticOverflow {
            ident,
            field: "logical_payload_bytes",
        },
    )?;
    if logical_payload_bytes > max_payload_bytes {
        return Err(PduOutboundPlanError::LogicalPayloadLimitExceeded {
            ident,
            declared_payload_bytes: logical_payload_bytes,
            max_payload_bytes,
        });
    }
    Ok(CountedPduPayload {
        logical_payload_bytes,
        ordered_window_section_bytes: counting_state.ordered_window_section_bytes,
    })
}

impl BoundedSerializeBuffer {
    fn new(max_bytes: usize) -> Self {
        #[cfg(test)]
        record_test_bounded_serialize_buffer_construction();
        Self {
            bytes: Vec::with_capacity(64.min(max_bytes)),
            logical_len: 0,
            max_bytes,
            exceeded: false,
        }
    }

    fn try_with_exact_capacity(max_bytes: usize) -> Result<Self, Error> {
        #[cfg(test)]
        record_test_bounded_serialize_buffer_construction();
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(max_bytes)
            .context("reserving exact planned PDU payload capacity")?;
        #[cfg(test)]
        record_test_bounded_serialize_growth_event(max_bytes);
        Ok(Self {
            bytes,
            logical_len: 0,
            max_bytes,
            exceeded: false,
        })
    }
}

impl std::io::Write for BoundedSerializeBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let Some(next_len) = self.logical_len.checked_add(buf.len()) else {
            self.logical_len = usize::MAX;
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "serialized PDU body length overflow",
            ));
        };
        self.logical_len = next_len;
        if next_len > self.max_bytes {
            self.exceeded = true;
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "serialized PDU body exceeded its wire limit",
            ));
        }
        // Retain amortized growth without requesting capacity beyond the wire
        // ceiling. Exact-reserving every serializer write makes a q-element
        // payload perform O(q) allocator requests; choosing the next bounded
        // geometric capacity keeps that to O(log(bytes)).
        if next_len > self.bytes.capacity() {
            let doubled = self.bytes.capacity().saturating_mul(2);
            let target_capacity = next_len.max(doubled).min(self.max_bytes);
            let additional = target_capacity.saturating_sub(self.bytes.len());
            self.bytes
                .try_reserve_exact(additional)
                .map_err(std::io::Error::other)?;
            #[cfg(test)]
            record_test_bounded_serialize_growth_event(target_capacity);
        }
        self.bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_uncompressed_bounded<T: serde::Serialize>(
    value: &T,
    max_payload_bytes: usize,
    serial: u64,
    ident: u64,
) -> Result<Vec<u8>, Error> {
    #[cfg(test)]
    record_test_serialize_invocation();
    let mut output = BoundedSerializeBuffer::new(max_payload_bytes);
    let serialize_result = {
        let mut encoder = varbincode::Serializer::new(&mut output);
        value.serialize(&mut encoder)
    };
    if output.exceeded {
        return Err(PduEncodedBodyLimitExceeded {
            declared_payload_bytes: output.logical_len,
            max_payload_bytes,
            serial,
            ident,
            is_compressed: false,
        }
        .into());
    }
    serialize_result?;
    Ok(output.bytes)
}

fn finish_serialized_payload(
    uncompressed: Vec<u8>,
    compression_mode: CompressionMode,
) -> Result<SerializedPayload, Error> {
    let uncompressed_len = uncompressed.len();
    if compression_mode == CompressionMode::Never
        || (compression_mode == CompressionMode::Auto && uncompressed_len <= COMPRESS_THRESH)
    {
        return Ok(SerializedPayload {
            data: uncompressed,
            is_compressed: false,
            uncompressed_len,
        });
    }

    // Compress the one canonical serialization; never run serde a second time.
    #[cfg(test)]
    record_test_compression_invocation();
    let compressed =
        zstd::stream::encode_all(uncompressed.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)?;

    log::debug!(
        "serialized+compress len {} vs {}",
        compressed.len(),
        uncompressed_len
    );

    if compression_mode == CompressionMode::Always || compressed.len() < uncompressed_len {
        Ok(SerializedPayload {
            data: compressed,
            is_compressed: true,
            uncompressed_len,
        })
    } else {
        Ok(SerializedPayload {
            data: uncompressed,
            is_compressed: false,
            uncompressed_len,
        })
    }
}

fn serialize_uncompressed_from_plan<T: serde::Serialize>(
    value: &T,
    plan: &PduOutboundPlan,
) -> Result<Vec<u8>, PduOutboundEncodeError> {
    #[cfg(test)]
    record_test_serialize_invocation();
    let mut output = BoundedSerializeBuffer::try_with_exact_capacity(plan.logical_payload_bytes)
        .map_err(|cause| PduOutboundEncodeError::Codec {
            ident: plan.ident,
            stage: "payload reservation",
            cause,
        })?;
    let serialize_result = {
        let mut encoder = varbincode::Serializer::new(&mut output);
        value.serialize(&mut encoder)
    };
    if output.exceeded {
        return Err(PduOutboundEncodeError::PlanMismatch {
            ident: plan.ident,
            field: "logical_payload_bytes",
            planned: plan.logical_payload_bytes,
            actual: output.logical_len,
        });
    }
    serialize_result.map_err(|cause| PduOutboundEncodeError::Codec {
        ident: plan.ident,
        stage: "payload serialization",
        cause: cause.into(),
    })?;
    if output.logical_len != plan.logical_payload_bytes
        || output.bytes.len() != plan.logical_payload_bytes
    {
        return Err(PduOutboundEncodeError::PlanMismatch {
            ident: plan.ident,
            field: "logical_payload_bytes",
            planned: plan.logical_payload_bytes,
            actual: output.logical_len.max(output.bytes.len()),
        });
    }
    Ok(output.bytes)
}

fn compress_payload_from_plan(
    uncompressed: &[u8],
    plan: &PduOutboundPlan,
) -> Result<Vec<u8>, PduOutboundEncodeError> {
    #[cfg(test)]
    record_test_compression_invocation();
    let output = BoundedSerializeBuffer::new(plan.maximum_compression_output_bytes);
    let mut encoder = zstd::stream::write::Encoder::new(output, zstd::DEFAULT_COMPRESSION_LEVEL)
        .map_err(|cause| PduOutboundEncodeError::Codec {
            ident: plan.ident,
            stage: "compression initialization",
            cause: cause.into(),
        })?;
    let mut input = uncompressed;
    let copied = match std::io::copy(&mut input, &mut encoder) {
        Ok(copied) => copied,
        Err(cause) => {
            let output = encoder.get_ref();
            if output.exceeded {
                return Err(PduOutboundEncodeError::PlanMismatch {
                    ident: plan.ident,
                    field: "maximum_compression_output_bytes",
                    planned: plan.maximum_compression_output_bytes,
                    actual: output.logical_len,
                });
            }
            return Err(PduOutboundEncodeError::Codec {
                ident: plan.ident,
                stage: "compression",
                cause: cause.into(),
            });
        }
    };
    let copied = usize::try_from(copied).map_err(|cause| PduOutboundEncodeError::Codec {
        ident: plan.ident,
        stage: "compression input accounting",
        cause: cause.into(),
    })?;
    if copied != uncompressed.len() || !input.is_empty() {
        return Err(PduOutboundEncodeError::PlanMismatch {
            ident: plan.ident,
            field: "logical_payload_bytes",
            planned: uncompressed.len(),
            actual: copied,
        });
    }
    let output = match encoder.try_finish() {
        Ok(output) => output,
        Err((encoder, cause)) => {
            let output = encoder.get_ref();
            if output.exceeded {
                return Err(PduOutboundEncodeError::PlanMismatch {
                    ident: plan.ident,
                    field: "maximum_compression_output_bytes",
                    planned: plan.maximum_compression_output_bytes,
                    actual: output.logical_len,
                });
            }
            return Err(PduOutboundEncodeError::Codec {
                ident: plan.ident,
                stage: "compression finalization",
                cause: cause.into(),
            });
        }
    };
    if output.logical_len > plan.maximum_compression_output_bytes
        || output.bytes.len() > plan.maximum_compression_output_bytes
    {
        return Err(PduOutboundEncodeError::PlanMismatch {
            ident: plan.ident,
            field: "maximum_compression_output_bytes",
            planned: plan.maximum_compression_output_bytes,
            actual: output.logical_len.max(output.bytes.len()),
        });
    }
    Ok(output.bytes)
}

fn serialize_pdu_payload_from_plan<T: serde::Serialize>(
    value: &T,
    plan: &PduOutboundPlan,
) -> Result<SerializedPayload, PduOutboundEncodeError> {
    let uncompressed = serialize_uncompressed_from_plan(value, plan)?;
    if plan.compression_mode == CompressionMode::Never
        || (plan.compression_mode == CompressionMode::Auto && uncompressed.len() <= COMPRESS_THRESH)
    {
        return Ok(SerializedPayload {
            uncompressed_len: uncompressed.len(),
            data: uncompressed,
            is_compressed: false,
        });
    }

    let compressed = compress_payload_from_plan(uncompressed.as_slice(), plan)?;
    if plan.compression_mode == CompressionMode::Always || compressed.len() < uncompressed.len() {
        Ok(SerializedPayload {
            uncompressed_len: uncompressed.len(),
            data: compressed,
            is_compressed: true,
        })
    } else {
        Ok(SerializedPayload {
            uncompressed_len: uncompressed.len(),
            data: uncompressed,
            is_compressed: false,
        })
    }
}

fn serialize_pdu_payload<T: serde::Serialize>(
    value: &T,
    wire_spec: &PduWireSpec,
    serial: u64,
    compression_mode: CompressionMode,
) -> Result<SerializedPayload, Error> {
    let max_uncompressed_bytes = wire_spec
        .encoded_body_limit
        .maximum_encoded_payload_bytes(false);
    let uncompressed =
        serialize_uncompressed_bounded(value, max_uncompressed_bytes, serial, wire_spec.ident)?;
    // This defense-in-depth check uses the real retained serialization length;
    // the bounded writer already stopped serialization at the same
    // schema-specific ceiling before zstd or final-frame work.
    validate_encoded_body_admission(uncompressed.len(), serial, wire_spec.ident, false)?;
    let serialized = finish_serialized_payload(uncompressed, compression_mode)?;
    if !serialized.is_compressed {
        debug_assert_eq!(serialized.uncompressed_len, serialized.data.len());
    }
    if serialized.is_compressed {
        validate_encoded_body_admission(serialized.data.len(), serial, wire_spec.ident, true)?;
    }
    Ok(serialized)
}

fn serialize_with_mode<T: serde::Serialize>(
    t: &T,
    compression_mode: CompressionMode,
) -> Result<(Vec<u8>, bool), Error> {
    let serialized = finish_serialized_payload(serialize_uncompressed(t)?, compression_mode)?;
    Ok((serialized.data, serialized.is_compressed))
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
    let recommended_output_size =
        zstd::Decoder::<&[u8]>::recommended_output_size().max(MIN_EXACT_ZSTD_DECODE_BUFFER_SIZE);
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
    /// without also declaring its dialect, producer, role, capability,
    /// semantic class, admission-cap key, and queue service policy.
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

/// Semantic work represented by one outbound PDU.
///
/// This classification is deliberately independent of wire direction. A
/// request and its typed response can describe the same class of work, while
/// generic replies inherit the class of the correlated request explicitly.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PduSemanticClass {
    /// Connection establishment, liveness, and protocol control.
    ConnectionControl,
    /// User input whose key-to-remote-echo latency is directly observable.
    InteractiveInput,
    /// User-visible pane, tab, window, layout, or workspace mutation.
    InteractiveState,
    /// Replication of authoritative mux state and topology.
    StateSync,
    /// Screen-content production, delivery, and acknowledgement.
    Render,
    /// Bounded information retrieval that is not itself a bulk transfer.
    Query,
    /// Potentially large history, image, search, clipboard, or snapshot data.
    BulkData,
}

/// Admission-accounting bucket selected before codec allocation.
///
/// Numerical ceilings are intentionally defined by the later outbound-plan
/// layer. This enum freezes the exhaustive key space without pretending that
/// unmeasured defaults are already performance evidence.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PduAdmissionCapKey {
    Control,
    InteractiveInput,
    InteractiveState,
    StateSync,
    Render,
    Query,
    BulkData,
}

/// Scheduling service class for the bounded outbound queue.
///
/// These labels do not define a strict-priority order. The queue authority
/// must give every admitted class bounded progress so a bulk reply cannot be
/// starved behind an indefinitely active interactive lane.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PduQueueQos {
    /// Protocol progress and liveness traffic.
    Control,
    /// Latency-sensitive human input and visible state changes.
    Interactive,
    /// Ordinary queries and state replication.
    Normal,
    /// Large transfers that must not monopolize interactive service.
    Bulk,
}

/// Whether one metadata field is fixed or inherited from a correlated request.
///
/// Generic [`UnitResponse`] and [`ErrorResponse`] frames cannot be classified
/// correctly without the request they answer. Encoding that fact in the type
/// prevents them from silently falling into a global response bucket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PduCorrelatedRequestPolicy<T> {
    Fixed(T),
    InheritCorrelatedRequest,
}

/// Fully resolved outbound admission metadata.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PduOutboundMetadata {
    pub semantic_class: PduSemanticClass,
    pub admission_cap_key: PduAdmissionCapKey,
    pub queue_qos: PduQueueQos,
}

/// Allocation-free failure returned before outbound admission or wire effects.
///
/// Every variant has *definitely not sent* delivery certainty: no serial has
/// been reserved, no payload encoded, no queue mutated, and no transport
/// callback invoked when this error is produced.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum PduOutboundMetadataError {
    #[error("invalid PDU identifier {ident} cannot be planned for outbound delivery")]
    InvalidPdu { ident: u64 },
    #[error("PDU identifier {ident} does not match its canonical outbound registry entry")]
    NonCanonicalWireSpec { ident: u64 },
    #[error("PDU {ident} is not authorized for outbound producer {producer:?} and role {role:?}")]
    DirectionNotAuthorized {
        ident: u64,
        producer: PduProducer,
        role: PduWireRole,
    },
    #[error("PDU {response_ident} requires a correlated request before outbound planning")]
    CorrelatedRequestRequired { response_ident: u64 },
    #[error(
        "PDU {request_ident} is not a valid client request for inherited response {response_ident}"
    )]
    InvalidCorrelatedRequest {
        response_ident: u64,
        request_ident: u64,
    },
    #[error("PDU {ident} has an inconsistent correlated-request metadata policy")]
    InconsistentInheritancePolicy { ident: u64 },
    #[error(
        "correlated request {request_ident} for response {response_ident} cannot itself inherit metadata"
    )]
    RecursiveInheritancePolicy {
        response_ident: u64,
        request_ident: u64,
    },
}

/// Complete pre-allocation bound for one outbound PDU.
///
/// The plan reserves the maximum-width `u64` serial rather than a serial that
/// has already been issued.  Its byte counts describe codec-owned logical
/// storage, not allocator capacity, native zstd workspace, or process RSS.
#[derive(Debug, Eq, PartialEq)]
pub struct PduOutboundPlan {
    ident: u64,
    metadata: PduOutboundMetadata,
    compression_mode: CompressionMode,
    logical_payload_bytes: usize,
    ordered_window_section_bytes: Option<usize>,
    maximum_compression_output_bytes: usize,
    maximum_encoded_payload_bytes: usize,
    maximum_frame_bytes: usize,
    retained_frame_bytes: usize,
    codec_peak_bytes: usize,
}

impl PduOutboundPlan {
    #[must_use]
    pub const fn ident(&self) -> u64 {
        self.ident
    }

    #[must_use]
    pub const fn metadata(&self) -> PduOutboundMetadata {
        self.metadata
    }

    #[must_use]
    pub const fn compression_mode(&self) -> CompressionMode {
        self.compression_mode
    }

    #[must_use]
    pub const fn logical_payload_bytes(&self) -> usize {
        self.logical_payload_bytes
    }

    #[must_use]
    pub const fn maximum_compression_output_bytes(&self) -> usize {
        self.maximum_compression_output_bytes
    }

    #[must_use]
    pub const fn maximum_encoded_payload_bytes(&self) -> usize {
        self.maximum_encoded_payload_bytes
    }

    #[must_use]
    pub const fn maximum_frame_bytes(&self) -> usize {
        self.maximum_frame_bytes
    }

    #[must_use]
    pub const fn retained_frame_bytes(&self) -> usize {
        self.retained_frame_bytes
    }

    #[must_use]
    pub const fn codec_peak_bytes(&self) -> usize {
        self.codec_peak_bytes
    }
}

/// Exact-owner capability binding one immutable plan to the PDU it measured.
///
/// This type is intentionally neither `Clone` nor `Copy`, and its fields are
/// private.  A later admitted encoder can consume it without accepting a plan
/// measured from a different PDU value.
///
/// For a generic correlated response, this capability proves only the codec
/// planning inputs.  The transport must still resolve the correlated request
/// from its exact pending-request authority before admission; a caller-chosen
/// canonical [`PduWireSpec`] is not a pending-request ownership witness.
#[must_use = "a prepared outbound PDU must be admitted, encoded, or rejected"]
pub struct PreparedPduOutbound<'pdu> {
    pdu: &'pdu Pdu,
    plan: PduOutboundPlan,
}

impl std::fmt::Debug for PreparedPduOutbound<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedPduOutbound")
            .field("pdu_name", &self.pdu.pdu_name())
            .field("plan", &self.plan)
            .finish()
    }
}

impl PreparedPduOutbound<'_> {
    #[must_use]
    pub const fn pdu(&self) -> &Pdu {
        self.pdu
    }

    #[must_use]
    pub const fn plan(&self) -> &PduOutboundPlan {
        &self.plan
    }

    /// Consume this exact-PDU planning capability and build one bounded frame.
    ///
    /// The caller still owns transport admission and serial reservation.  This
    /// method performs no socket, queue, or delivery-ledger side effect.
    pub fn encode_frame(self, serial: u64) -> Result<Vec<u8>, PduOutboundEncodeError> {
        self.pdu.encode_frame_from_outbound_plan(serial, &self.plan)
    }
}

impl std::ops::Deref for PreparedPduOutbound<'_> {
    type Target = PduOutboundPlan;

    fn deref(&self) -> &Self::Target {
        &self.plan
    }
}

/// Typed pre-allocation planning failure.
///
/// Every variant has *definitely not sent* delivery certainty.  Planning does
/// not issue a serial, allocate a codec payload or compression destination,
/// mutate a queue, or invoke a transport.
#[derive(Debug, Error)]
pub enum PduOutboundPlanError {
    #[error(transparent)]
    Metadata(#[from] PduOutboundMetadataError),
    #[error("PDU {ident} failed outbound payload validation before codec allocation: {cause:#}")]
    InvalidPayload { ident: u64, cause: Error },
    #[error("PDU {ident} could not be measured by the canonical counting serializer: {cause:#}")]
    CountingSerialization { ident: u64, cause: Error },
    #[error(
        "PDU {ident} logical payload size {declared_payload_bytes} exceeds maximum {max_payload_bytes}"
    )]
    LogicalPayloadLimitExceeded {
        ident: u64,
        declared_payload_bytes: usize,
        max_payload_bytes: usize,
    },
    #[error(
        "PDU {ident} maximum compressed payload size {maximum_compressed_bytes} exceeds maximum {max_payload_bytes}"
    )]
    CompressionBoundExceeded {
        ident: u64,
        maximum_compressed_bytes: usize,
        max_payload_bytes: usize,
    },
    #[error("PDU {ident} outbound plan arithmetic overflowed while computing {field}")]
    ArithmeticOverflow { ident: u64, field: &'static str },
    #[error("PDU {ident} encountered a nested outbound counting scope")]
    NestedCountingScope { ident: u64 },
}

/// Failure while consuming one exact outbound plan into a bounded frame.
///
/// All variants retain *definitely not sent* certainty.  Frame construction
/// does not enqueue or write; callers decide those effects only after success.
#[derive(Debug, Error)]
pub enum PduOutboundEncodeError {
    #[error("PDU {ident} failed bounded outbound {stage}: {cause:#}")]
    Codec {
        ident: u64,
        stage: &'static str,
        cause: Error,
    },
    #[error(
        "PDU {ident} outbound plan mismatch for {field}: planned {planned} bytes, observed {actual}"
    )]
    PlanMismatch {
        ident: u64,
        field: &'static str,
        planned: usize,
        actual: usize,
    },
    #[error(
        "outbound plan belongs to PDU {planned_ident}, but encoder received PDU {actual_ident}"
    )]
    IdentityMismatch {
        planned_ident: u64,
        actual_ident: u64,
    },
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

/// Encoded-body admission policy applied before a decoder allocates body
/// storage.
///
/// `SchemaDecompressedWithZstdBound` admits an uncompressed body up to the
/// schema ceiling. For a compressed body it admits the worst-case output size
/// of FrankenTerm's single-frame zstd encoder for a legal schema payload. This
/// permits bounded compression overhead without reopening the global 256 MiB
/// allocation envelope to a small authority PDU.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PduEncodedBodyLimit {
    GlobalMaximum,
    SchemaDecompressedWithZstdBound {
        max_decompressed_bytes: usize,
        max_zstd_encoded_bytes: usize,
    },
}

impl PduEncodedBodyLimit {
    /// Largest encoded body accepted for this schema and compression flag.
    // The pinned compiler does not yet permit `Ord::min` in this const
    // context. Keep the branch const-evaluable until that toolchain advances.
    #[must_use]
    pub const fn maximum_encoded_payload_bytes(self, is_compressed: bool) -> usize {
        match self {
            Self::GlobalMaximum => MAX_PDU_SIZE,
            Self::SchemaDecompressedWithZstdBound {
                max_decompressed_bytes,
                max_zstd_encoded_bytes,
            } => {
                if is_compressed {
                    if max_zstd_encoded_bytes < MAX_PDU_SIZE {
                        max_zstd_encoded_bytes
                    } else {
                        MAX_PDU_SIZE
                    }
                } else {
                    if max_decompressed_bytes < MAX_PDU_SIZE {
                        max_decompressed_bytes
                    } else {
                        MAX_PDU_SIZE
                    }
                }
            }
        }
    }
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
    pub encoded_body_limit: PduEncodedBodyLimit,
    pub semantic_class: PduCorrelatedRequestPolicy<PduSemanticClass>,
    pub admission_cap_key: PduCorrelatedRequestPolicy<PduAdmissionCapKey>,
    pub queue_qos: PduCorrelatedRequestPolicy<PduQueueQos>,
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

    fn fixed_outbound_metadata(&self) -> Option<PduOutboundMetadata> {
        let PduCorrelatedRequestPolicy::Fixed(semantic_class) = self.semantic_class else {
            return None;
        };
        let PduCorrelatedRequestPolicy::Fixed(admission_cap_key) = self.admission_cap_key else {
            return None;
        };
        let PduCorrelatedRequestPolicy::Fixed(queue_qos) = self.queue_qos else {
            return None;
        };
        Some(PduOutboundMetadata {
            semantic_class,
            admission_cap_key,
            queue_qos,
        })
    }

    /// Validate one concrete outbound direction and resolve its admission
    /// metadata without allocating, reserving a serial, or touching a queue.
    pub fn resolve_outbound_metadata(
        &self,
        producer: PduProducer,
        role: PduWireRole,
        correlated_request: Option<&Self>,
    ) -> Result<PduOutboundMetadata, PduOutboundMetadataError> {
        if Pdu::wire_spec_for_ident(self.ident) != Some(self) {
            return Err(PduOutboundMetadataError::NonCanonicalWireSpec { ident: self.ident });
        }
        self.resolve_canonical_outbound_metadata(producer, role, correlated_request)
    }

    fn resolve_canonical_outbound_metadata(
        &self,
        producer: PduProducer,
        role: PduWireRole,
        correlated_request: Option<&Self>,
    ) -> Result<PduOutboundMetadata, PduOutboundMetadataError> {
        if !self.authorizes(producer, role) {
            return Err(PduOutboundMetadataError::DirectionNotAuthorized {
                ident: self.ident,
                producer,
                role,
            });
        }

        let inheritance_fields = [
            matches!(
                self.semantic_class,
                PduCorrelatedRequestPolicy::InheritCorrelatedRequest
            ),
            matches!(
                self.admission_cap_key,
                PduCorrelatedRequestPolicy::InheritCorrelatedRequest
            ),
            matches!(
                self.queue_qos,
                PduCorrelatedRequestPolicy::InheritCorrelatedRequest
            ),
        ];
        let inherited_count = inheritance_fields
            .iter()
            .filter(|inherited| **inherited)
            .count();
        if inherited_count == 0 {
            return self.fixed_outbound_metadata().ok_or(
                PduOutboundMetadataError::InconsistentInheritancePolicy { ident: self.ident },
            );
        }
        if inherited_count != inheritance_fields.len() {
            return Err(PduOutboundMetadataError::InconsistentInheritancePolicy {
                ident: self.ident,
            });
        }

        let request =
            correlated_request.ok_or(PduOutboundMetadataError::CorrelatedRequestRequired {
                response_ident: self.ident,
            })?;
        let canonical_request = Pdu::wire_spec_for_ident(request.ident);
        if canonical_request != Some(request)
            || !request.authorizes(PduProducer::Client, PduWireRole::Request)
        {
            return Err(PduOutboundMetadataError::InvalidCorrelatedRequest {
                response_ident: self.ident,
                request_ident: request.ident,
            });
        }
        request.fixed_outbound_metadata().ok_or(
            PduOutboundMetadataError::RecursiveInheritancePolicy {
                response_ident: self.ident,
                request_ident: request.ident,
            },
        )
    }
}

/// Serial value reserved by outbound planning before the connection allocates
/// an actual request identity.  Its LEB128 encoding is the widest possible.
pub const OUTBOUND_PLAN_RESERVED_SERIAL: u64 = u64::MAX;

impl PduOutboundPlan {
    fn checked_add(
        ident: u64,
        field: &'static str,
        left: usize,
        right: usize,
    ) -> Result<usize, PduOutboundPlanError> {
        left.checked_add(right)
            .ok_or(PduOutboundPlanError::ArithmeticOverflow { ident, field })
    }

    fn frame_bound(
        ident: u64,
        payload_bytes: usize,
        compressed: bool,
    ) -> Result<usize, PduOutboundPlanError> {
        encoded_frame_len(
            ident,
            OUTBOUND_PLAN_RESERVED_SERIAL,
            payload_bytes,
            compressed,
        )
        .map_err(|_| PduOutboundPlanError::ArithmeticOverflow {
            ident,
            field: "maximum_frame_bytes",
        })
    }

    fn from_counted_payload(
        spec: &PduWireSpec,
        metadata: PduOutboundMetadata,
        compression_mode: CompressionMode,
        counted: CountedPduPayload,
    ) -> Result<Self, PduOutboundPlanError> {
        let ident = spec.ident;
        let logical_payload_bytes = counted.logical_payload_bytes;
        let uncompressed_frame_bytes = Self::frame_bound(ident, logical_payload_bytes, false)?;
        let should_attempt_compression = compression_mode == CompressionMode::Always
            || (compression_mode == CompressionMode::Auto
                && logical_payload_bytes > COMPRESS_THRESH);
        let maximum_compression_output_bytes = if should_attempt_compression {
            zstd::zstd_safe::compress_bound(logical_payload_bytes)
        } else {
            0
        };

        let (maximum_encoded_payload_bytes, maximum_frame_bytes) = match compression_mode {
            CompressionMode::Never => (logical_payload_bytes, uncompressed_frame_bytes),
            CompressionMode::Always => {
                let max_payload_bytes = spec.encoded_body_limit.maximum_encoded_payload_bytes(true);
                if maximum_compression_output_bytes > max_payload_bytes {
                    return Err(PduOutboundPlanError::CompressionBoundExceeded {
                        ident,
                        maximum_compressed_bytes: maximum_compression_output_bytes,
                        max_payload_bytes,
                    });
                }
                (
                    maximum_compression_output_bytes,
                    Self::frame_bound(ident, maximum_compression_output_bytes, true)?,
                )
            }
            CompressionMode::Auto if should_attempt_compression => {
                // Auto retains compressed bytes only when the payload is
                // strictly smaller.  The zstd destination itself is charged at
                // compressBound, while the possible wire result is capped at
                // logical-1 and checked against the compressed schema limit.
                let selected_compressed_bytes =
                    maximum_compression_output_bytes.min(logical_payload_bytes.saturating_sub(1));
                let max_payload_bytes = spec.encoded_body_limit.maximum_encoded_payload_bytes(true);
                if selected_compressed_bytes > max_payload_bytes {
                    return Err(PduOutboundPlanError::CompressionBoundExceeded {
                        ident,
                        maximum_compressed_bytes: selected_compressed_bytes,
                        max_payload_bytes,
                    });
                }
                let compressed_frame_bytes =
                    Self::frame_bound(ident, selected_compressed_bytes, true)?;
                (
                    logical_payload_bytes,
                    uncompressed_frame_bytes.max(compressed_frame_bytes),
                )
            }
            CompressionMode::Auto => (logical_payload_bytes, uncompressed_frame_bytes),
        };

        // This intentionally charges all live codec-owned buffers at their
        // conservative bounds, including allocator handoff while the final
        // frame grows.  A later capacity-limited encoder may lower the peak,
        // but admission must never depend on that future optimization.
        let payload_and_compression = Self::checked_add(
            ident,
            "codec_peak_bytes",
            logical_payload_bytes,
            maximum_compression_output_bytes,
        )?;
        let codec_peak_bytes = Self::checked_add(
            ident,
            "codec_peak_bytes",
            payload_and_compression,
            maximum_frame_bytes,
        )?;

        Ok(Self {
            ident,
            metadata,
            compression_mode,
            logical_payload_bytes,
            ordered_window_section_bytes: counted.ordered_window_section_bytes,
            maximum_compression_output_bytes,
            maximum_encoded_payload_bytes,
            maximum_frame_bytes,
            retained_frame_bytes: maximum_frame_bytes,
            codec_peak_bytes,
        })
    }
}

macro_rules! pdu_semantic_class {
    (connection_control) => {
        PduCorrelatedRequestPolicy::Fixed(PduSemanticClass::ConnectionControl)
    };
    (interactive_input) => {
        PduCorrelatedRequestPolicy::Fixed(PduSemanticClass::InteractiveInput)
    };
    (interactive_state) => {
        PduCorrelatedRequestPolicy::Fixed(PduSemanticClass::InteractiveState)
    };
    (state_sync) => {
        PduCorrelatedRequestPolicy::Fixed(PduSemanticClass::StateSync)
    };
    (render) => {
        PduCorrelatedRequestPolicy::Fixed(PduSemanticClass::Render)
    };
    (query) => {
        PduCorrelatedRequestPolicy::Fixed(PduSemanticClass::Query)
    };
    (bulk_data) => {
        PduCorrelatedRequestPolicy::Fixed(PduSemanticClass::BulkData)
    };
    (inherit_request) => {
        PduCorrelatedRequestPolicy::InheritCorrelatedRequest
    };
}

macro_rules! pdu_admission_cap_key {
    (control) => {
        PduCorrelatedRequestPolicy::Fixed(PduAdmissionCapKey::Control)
    };
    (interactive_input) => {
        PduCorrelatedRequestPolicy::Fixed(PduAdmissionCapKey::InteractiveInput)
    };
    (interactive_state) => {
        PduCorrelatedRequestPolicy::Fixed(PduAdmissionCapKey::InteractiveState)
    };
    (state_sync) => {
        PduCorrelatedRequestPolicy::Fixed(PduAdmissionCapKey::StateSync)
    };
    (render) => {
        PduCorrelatedRequestPolicy::Fixed(PduAdmissionCapKey::Render)
    };
    (query) => {
        PduCorrelatedRequestPolicy::Fixed(PduAdmissionCapKey::Query)
    };
    (bulk_data) => {
        PduCorrelatedRequestPolicy::Fixed(PduAdmissionCapKey::BulkData)
    };
    (inherit_request) => {
        PduCorrelatedRequestPolicy::InheritCorrelatedRequest
    };
}

macro_rules! pdu_queue_qos {
    (control) => {
        PduCorrelatedRequestPolicy::Fixed(PduQueueQos::Control)
    };
    (interactive) => {
        PduCorrelatedRequestPolicy::Fixed(PduQueueQos::Interactive)
    };
    (normal) => {
        PduCorrelatedRequestPolicy::Fixed(PduQueueQos::Normal)
    };
    (bulk) => {
        PduCorrelatedRequestPolicy::Fixed(PduQueueQos::Bulk)
    };
    (inherit_request) => {
        PduCorrelatedRequestPolicy::InheritCorrelatedRequest
    };
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
    (requires_exact_render) => {
        PduCapabilityUse::Requires(TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::EXACT_RENDER_DELIVERY_V1.bits(),
        ))
    };
}

macro_rules! pdu_encoded_body_limit {
    (GetPaneTieredScrollbackStatusesV1, none) => {
        PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_TIERED_SCROLLBACK_STATUS_REQUEST_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_TIERED_SCROLLBACK_STATUS_REQUEST_ZSTD_ENCODED_BYTES,
        }
    };
    (GetPaneTieredScrollbackStatusesV1Response, none) => {
        PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_ZSTD_ENCODED_BYTES,
        }
    };
    (GetImageCellResponse, none) => {
        PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_GET_IMAGE_CELL_RESPONSE_ZSTD_ENCODED_BYTES,
        }
    };
    (ListPanesOrderedV1Response, negotiates_ordered) => {
        PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES,
        }
    };
    ($_name:ident, requires_reorder) => {
        PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_REORDER_WINDOW_TABS_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_REORDER_WINDOW_TABS_ZSTD_ENCODED_BYTES,
        }
    };
    (GetPaneRenderDeliveryV1, requires_exact_render) => {
        PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_EXACT_RENDER_REQUEST_ZSTD_ENCODED_BYTES,
        }
    };
    ($_name:ident, requires_exact_render) => {
        PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_EXACT_RENDER_DELIVERY_ZSTD_ENCODED_BYTES,
        }
    };
    ($_name:ident, $_other:ident) => {
        PduEncodedBodyLimit::GlobalMaximum
    };
}

macro_rules! pdu {
    ($(
        $name:ident: $vers:expr, $min_codec_version:expr,
        $authority_policy:ident, $capability_policy:ident,
        $semantic_class:ident, $admission_cap_key:ident, $queue_qos:ident
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
                    encoded_body_limit: pdu_encoded_body_limit!($name, $capability_policy),
                    semantic_class: pdu_semantic_class!($semantic_class),
                    admission_cap_key: pdu_admission_cap_key!($admission_cap_key),
                    queue_qos: pdu_queue_qos!($queue_qos),
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
                            let serialized = {
                                let _scope =
                                    ValidatedOrderedSerializationScope::for_pdu(self)?;
                                serialize_pdu_payload(
                                    s,
                                    &<$name as PduWireIdent>::WIRE_SPEC,
                                    serial,
                                    compression_mode,
                                )?
                            };
                            let encoded_size = encode_raw(
                                $vers,
                                serial,
                                &serialized.data,
                                serialized.is_compressed,
                                w,
                            )?;
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
                self.encode_frame_with_mode_after_validation(
                    serial,
                    compression_mode,
                    record_metrics,
                )
            }

            fn encode_frame_with_mode_after_validation(
                &self,
                serial: u64,
                compression_mode: CompressionMode,
                record_metrics: bool,
            ) -> Result<Vec<u8>, Error> {
                match self {
                    Pdu::Invalid{..} => bail!("attempted to serialize Pdu::Invalid"),
                    $(
                        Pdu::$name(s) => {
                            let serialized = {
                                let _scope =
                                    ValidatedOrderedSerializationScope::for_pdu(self)?;
                                serialize_pdu_payload(
                                    s,
                                    &<$name as PduWireIdent>::WIRE_SPEC,
                                    serial,
                                    compression_mode,
                                )?
                            };
                            let frame = prepend_frame_header_to_owned_payload(
                                $vers,
                                serial,
                                serialized.data,
                                serialized.is_compressed,
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
                            let serialized = {
                                let _scope =
                                    ValidatedOrderedSerializationScope::for_pdu(self)?;
                                serialize_pdu_payload(
                                    s,
                                    &<$name as PduWireIdent>::WIRE_SPEC,
                                    serial,
                                    compression_mode,
                                )?
                            };
                            encoded_frame_len(
                                $vers,
                                serial,
                                serialized.data.len(),
                                serialized.is_compressed,
                            )
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
                            // Keep the thread-local proof scope synchronous: an
                            // async writer may move this future to another
                            // executor thread after the payload is owned.
                            let serialized = {
                                let _scope =
                                    ValidatedOrderedSerializationScope::for_pdu(self)?;
                                serialize_pdu_payload(
                                    s,
                                    &<$name as PduWireIdent>::WIRE_SPEC,
                                    serial,
                                    compression_mode,
                                )?
                            };
                            let encoded_size = encode_raw_async(
                                $vers,
                                serial,
                                &serialized.data,
                                serialized.is_compressed,
                                w,
                            ).await?;
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

            /// Resolve direction-aware outbound admission metadata.
            ///
            /// Invalid PDUs and direction mismatches return a typed error
            /// before serial allocation, queue admission, or codec work.
            pub fn resolve_outbound_metadata(
                &self,
                producer: PduProducer,
                role: PduWireRole,
                correlated_request: Option<&PduWireSpec>,
            ) -> Result<PduOutboundMetadata, PduOutboundMetadataError> {
                let spec = match self {
                    Pdu::Invalid { ident } => {
                        return Err(PduOutboundMetadataError::InvalidPdu { ident: *ident });
                    }
                    $(
                        Pdu::$name(_) => &<$name as PduWireIdent>::WIRE_SPEC,
                    )*
                };
                spec.resolve_canonical_outbound_metadata(producer, role, correlated_request)
            }

            /// Validate and measure one outbound PDU before serial allocation,
            /// codec payload allocation, compression, queueing, or wire work.
            pub fn plan_outbound(
                &self,
                producer: PduProducer,
                role: PduWireRole,
                correlated_request: Option<&PduWireSpec>,
                compression_mode: CompressionMode,
            ) -> Result<PreparedPduOutbound<'_>, PduOutboundPlanError> {
                let spec = match self {
                    Pdu::Invalid { ident } => {
                        return Err(PduOutboundMetadataError::InvalidPdu { ident: *ident }.into());
                    }
                    $(
                        Pdu::$name(_) => &<$name as PduWireIdent>::WIRE_SPEC,
                    )*
                };
                let metadata = spec.resolve_canonical_outbound_metadata(
                    producer,
                    role,
                    correlated_request,
                )?;
                self.validate_before_encode().map_err(|cause| {
                    PduOutboundPlanError::InvalidPayload {
                        ident: spec.ident,
                        cause,
                    }
                })?;
                let _validated_snapshot =
                    ValidatedOrderedSerializationScope::for_pdu(self).map_err(|cause| {
                        PduOutboundPlanError::CountingSerialization {
                            ident: spec.ident,
                            cause,
                        }
                    })?;
                let max_payload_bytes = spec
                    .encoded_body_limit
                    .maximum_encoded_payload_bytes(false);
                let counted = match self {
                    Pdu::Invalid { .. } => unreachable!("invalid PDU returned before counting"),
                    $(
                        Pdu::$name(value) => {
                            count_pdu_payload(value, max_payload_bytes, spec.ident)?
                        }
                    )*
                };
                let plan = PduOutboundPlan::from_counted_payload(
                    spec,
                    metadata,
                    compression_mode,
                    counted,
                )?;
                Ok(PreparedPduOutbound { pdu: self, plan })
            }

            fn encode_frame_from_outbound_plan(
                &self,
                serial: u64,
                plan: &PduOutboundPlan,
            ) -> Result<Vec<u8>, PduOutboundEncodeError> {
                let ident = self
                    .wire_spec()
                    .map_or(u64::MAX, |spec| spec.ident);
                if ident != plan.ident {
                    return Err(PduOutboundEncodeError::IdentityMismatch {
                        planned_ident: plan.ident,
                        actual_ident: ident,
                    });
                }
                let serialized = {
                    let _validated_snapshot =
                        ValidatedOrderedSerializationScope::for_pdu(self).map_err(|cause| {
                            PduOutboundEncodeError::Codec {
                                ident,
                                stage: "validated ordered serialization admission",
                                cause,
                            }
                        })?;
                    let direct_ordered_section = DirectOrderedSectionScope::enter(
                        ident,
                        plan.ordered_window_section_bytes,
                    )?;
                    let serialized = match self {
                        Pdu::Invalid { .. } => {
                            return Err(PduOutboundEncodeError::IdentityMismatch {
                                planned_ident: plan.ident,
                                actual_ident: u64::MAX,
                            });
                        }
                        $(
                            Pdu::$name(value) => {
                                serialize_pdu_payload_from_plan(value, plan)?
                            },
                        )*
                    };
                    direct_ordered_section.finish(ident)?;
                    serialized
                };
                if serialized.uncompressed_len != plan.logical_payload_bytes {
                    return Err(PduOutboundEncodeError::PlanMismatch {
                        ident,
                        field: "logical_payload_bytes",
                        planned: plan.logical_payload_bytes,
                        actual: serialized.uncompressed_len,
                    });
                }
                if serialized.data.len() > plan.maximum_encoded_payload_bytes {
                    return Err(PduOutboundEncodeError::PlanMismatch {
                        ident,
                        field: "maximum_encoded_payload_bytes",
                        planned: plan.maximum_encoded_payload_bytes,
                        actual: serialized.data.len(),
                    });
                }
                validate_encoded_body_admission(
                    serialized.data.len(),
                    serial,
                    ident,
                    serialized.is_compressed,
                )
                .map_err(|cause| PduOutboundEncodeError::Codec {
                    ident,
                    stage: "encoded body admission",
                    cause,
                })?;
                let frame = prepend_frame_header_to_owned_payload(
                    ident,
                    serial,
                    serialized.data,
                    serialized.is_compressed,
                    true,
                )
                .map_err(|cause| PduOutboundEncodeError::Codec {
                    ident,
                    stage: "frame construction",
                    cause,
                })?;
                if frame.len() > plan.maximum_frame_bytes {
                    return Err(PduOutboundEncodeError::PlanMismatch {
                        ident,
                        field: "maximum_frame_bytes",
                        planned: plan.maximum_frame_bytes,
                        actual: frame.len(),
                    });
                }
                log::debug!("encode_prepared {} size={}", self.pdu_name(), frame.len());
                metrics::histogram!("pdu.size", "pdu" => self.pdu_name())
                    .record(frame.len() as f64);
                metrics::histogram!("pdu.size.rate", "pdu" => self.pdu_name())
                    .record(frame.len() as f64);
                Ok(frame)
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
pub const CODEC_VERSION: usize = 58;

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
pub const CODEC_VERSION_MIN_SUPPORTED: usize = 58;

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
    ErrorResponse: 0, 46, server_reply, none,
        inherit_request, inherit_request, inherit_request;
    Ping: 1, 46, client_request, none,
        connection_control, control, control;
    Pong: 2, 46, server_reply, none,
        connection_control, control, control;
    ListPanes: 3, 46, client_request, none,
        state_sync, query, normal;
    ListPanesResponse: 4, 58, server_reply, none,
        state_sync, bulk_data, bulk
        => deserialize_list_panes_response;
    SpawnResponse: 8, 46, server_reply, none,
        interactive_state, interactive_state, interactive;
    WriteToPane: 9, 46, client_request, none,
        interactive_input, interactive_input, interactive;
    UnitResponse: 10, 46, server_reply, none,
        inherit_request, inherit_request, inherit_request;
    SendKeyDown: 11, 46, client_request, none,
        interactive_input, interactive_input, interactive;
    SendMouseEvent: 12, 46, client_request, none,
        interactive_input, interactive_input, interactive;
    SendPaste: 13, 56, client_request, none,
        interactive_input, bulk_data, interactive;
    Resize: 14, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    SetClipboard: 20, 46, server_unilateral, none,
        interactive_state, bulk_data, interactive;
    GetLines: 22, 46, client_request, none,
        query, query, normal;
    GetLinesResponse: 23, 55, server_reply, none,
        bulk_data, bulk_data, bulk;
    GetPaneRenderChanges: 24, 46, client_request, none,
        render, render, normal;
    GetPaneRenderChangesResponse: 25, 46, server_reply_or_unilateral, none,
        render, render, normal;
    GetCodecVersion: 26, 46, client_request, none,
        connection_control, control, control;
    GetCodecVersionResponse: 27, 46, server_reply, none,
        connection_control, control, control
        => deserialize_get_codec_version_response;
    GetTlsCreds: 28, 46, client_request, none,
        connection_control, control, control;
    GetTlsCredsResponse: 29, 46, server_reply, none,
        connection_control, control, control;
    LivenessResponse: 30, 46, server_reply, none,
        connection_control, control, control;
    SearchScrollbackRequest: 31, 46, client_request, none,
        query, query, normal;
    SearchScrollbackResponse: 32, 46, server_reply, none,
        bulk_data, bulk_data, bulk;
    SetPaneZoomed: 33, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    SplitPane: 34, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    KillPane: 35, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    SpawnV2: 36, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    PaneRemoved: 37, 46, server_unilateral, none,
        state_sync, state_sync, normal;
    SetPalette: 38, 46, client_request_or_server_unilateral, none,
        interactive_state, interactive_state, interactive;
    NotifyAlert: 39, 46, server_unilateral, none,
        state_sync, state_sync, normal;
    SetClientId: 40, 46, client_request, none,
        connection_control, control, control;
    GetClientList: 41, 46, client_request, none,
        query, query, normal;
    GetClientListResponse: 42, 46, server_reply, none,
        bulk_data, bulk_data, bulk;
    SetWindowWorkspace: 43, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    WindowWorkspaceChanged: 44, 46, server_unilateral, none,
        state_sync, state_sync, normal;
    SetFocusedPane: 45, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    GetImageCell: 46, 46, client_request, none,
        query, query, normal;
    GetImageCellResponse: 47, 55, server_reply, none,
        bulk_data, bulk_data, bulk
        => deserialize_get_image_cell_response;
    MovePaneToNewTab: 48, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    MovePaneToNewTabResponse: 49, 46, server_reply, none,
        interactive_state, interactive_state, interactive;
    ActivatePaneDirection: 50, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    GetPaneRenderableDimensions: 51, 46, client_request, none,
        query, query, normal;
    GetPaneRenderableDimensionsResponse: 52, 46, server_reply, none,
        query, query, normal;
    PaneFocused: 53, 46, server_unilateral, none,
        state_sync, state_sync, normal;
    TabResized: 54, 46, server_unilateral, none,
        state_sync, state_sync, normal;
    TabAddedToWindow: 55, 46, server_unilateral, none,
        state_sync, state_sync, normal;
    TabTitleChanged: 56, 46, client_request_or_server_unilateral, none,
        interactive_state, interactive_state, interactive;
    WindowTitleChanged: 57, 46, client_request_or_server_unilateral, none,
        interactive_state, interactive_state, interactive;
    RenameWorkspace: 58, 46, client_request_or_server_unilateral, none,
        interactive_state, interactive_state, interactive;
    EraseScrollbackRequest: 59, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    GetPaneDirection: 60, 46, client_request, none,
        query, query, normal;
    GetPaneDirectionResponse: 61, 46, server_reply, none,
        query, query, normal;
    AdjustPaneSize: 62, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    CreateFloatingPane: 63, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    MoveFloatingPane: 64, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    SetFloatingPaneZ: 65, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    ToggleFloatingPane: 66, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    RemoveFloatingPane: 67, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    SwapToLayout: 68, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    SetLayoutCycle: 69, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    CycleStack: 70, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    SelectStackPane: 71, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    UpdatePaneConstraints: 72, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    SendKeyUp: 73, 46, client_request, none,
        interactive_input, interactive_input, interactive;
    SetActiveWorkspace: 74, 46, client_request, none,
        interactive_state, interactive_state, interactive;
    ListPanesTabStacks: 75, 47, client_request, none,
        state_sync, query, normal;
    ListPanesTabStacksResponse: 76, 47, server_reply, none,
        state_sync, bulk_data, bulk;
    GetSemanticZones: 77, 47, client_request, none,
        query, query, normal;
    GetSemanticZonesResponse: 78, 47, server_reply, none,
        bulk_data, bulk_data, bulk;
    RenderApplicationUpdateV1: 79, 48, server_unilateral, none,
        render, render, normal;
    RenderApplicationResultV1: 80, 48, client_request, none,
        render, render, normal;
    ListPanesCoherent: 81, 49, client_request, negotiates_fenced,
        state_sync, query, normal
        => deserialize_list_panes_coherent;
    ListPanesCoherentResponse: 82, 49, server_reply, negotiates_fenced,
        state_sync, bulk_data, bulk
        => deserialize_list_panes_coherent_response;
    TopologyEvent: 83, 49, server_unilateral, requires_fenced,
        state_sync, state_sync, normal
        => deserialize_topology_event;
    RenderApplicationUpdate: 84, 50, server_unilateral, requires_fenced,
        render, render, normal;
    RenderApplicationResult: 85, 50, client_request, requires_fenced,
        render, render, normal;
    ListPanesOrderedV1: 86, 54, client_request, negotiates_ordered,
        state_sync, query, normal
        => deserialize_list_panes_ordered_v1;
    ListPanesOrderedV1Response: 87, 54, server_reply, negotiates_ordered,
        state_sync, bulk_data, bulk
        => deserialize_list_panes_ordered_v1_response;
    ReorderWindowTabsV1: 88, 54, client_request, requires_reorder,
        interactive_state, interactive_state, interactive
        => deserialize_reorder_window_tabs_v1;
    ReorderWindowTabsV1Response: 89, 54, server_reply, requires_reorder,
        interactive_state, interactive_state, interactive
        => deserialize_reorder_window_tabs_v1_response;
    WindowOrderEventV1: 90, 54, server_unilateral, requires_ordered,
        state_sync, state_sync, normal
        => deserialize_window_order_event_v1;
    GetPaneRenderDeliveryV1: 91, 52, client_request, requires_exact_render,
        render, render, normal
        => deserialize_get_pane_render_delivery_v1;
    GetPaneRenderDeliveryV1Response: 92, 52, server_reply, requires_exact_render,
        render, render, normal
        => deserialize_get_pane_render_delivery_v1_response;
    GetPaneTieredScrollbackStatusesV1: 93, 57, client_request, none,
        query, query, normal
        => deserialize_get_pane_tiered_scrollback_statuses_v1;
    GetPaneTieredScrollbackStatusesV1Response: 94, 57, server_reply, none,
        bulk_data, bulk_data, bulk
        => deserialize_get_pane_tiered_scrollback_statuses_v1_response;
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
            Self::GetPaneRenderDeliveryV1(value) => value.validate()?,
            Self::GetPaneRenderDeliveryV1Response(value) => value.validate()?,
            Self::GetPaneTieredScrollbackStatusesV1(value) => value.validate()?,
            Self::GetPaneTieredScrollbackStatusesV1Response(value) => value.validate()?,
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

    pub fn stream_decode(buffer: &mut StreamingPduBuffer) -> anyhow::Result<Option<DecodedPdu>> {
        Ok(Self::stream_decode_with_options(buffer, usize::MAX, false)?
            .map(|decoded| decoded.into_parts().0))
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
        let Some(frame_len) = buffered_frame_len_with_limit(buffer.as_slice(), max_frame_bytes)?
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
            Pdu::GetPaneRenderDeliveryV1(GetPaneRenderDeliveryV1 {
                identity: ExactRenderDeliveryRequestIdentity { pane_id, .. },
                ..
            })
            | Pdu::GetPaneRenderDeliveryV1Response(GetPaneRenderDeliveryV1Response {
                request_identity: ExactRenderDeliveryRequestIdentity { pane_id, .. },
                ..
            }) => pane_id.try_into_mux().ok(),
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
    let recommended_output_size =
        zstd::Decoder::<&[u8]>::recommended_output_size().max(MIN_EXACT_ZSTD_DECODE_BUFFER_SIZE);
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

fn deserialize_list_panes_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<ListPanesResponse, Error> {
    let response: ListPanesResponse =
        deserialize_exact_payload(data, is_compressed, "ListPanesResponse")?;
    response.validate_floating_panes()?;
    Ok(response)
}

fn deserialize_list_panes_coherent_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<ListPanesCoherentResponse, Error> {
    let response: ListPanesCoherentResponse =
        deserialize_exact_payload(data, is_compressed, "ListPanesCoherentResponse")?;
    if let ListPanesCoherentOutcome::Snapshot(snapshot) = &response.outcome {
        snapshot.panes.validate_floating_panes()?;
    }
    Ok(response)
}

fn deserialize_topology_event(data: &[u8], is_compressed: bool) -> Result<TopologyEvent, Error> {
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
    let response: ListPanesOrderedV1Response = deserialize_exact_payload_with_limit(
        data,
        is_compressed,
        "ListPanesOrderedV1Response",
        MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
    )?;
    response.validate_after_bounded_snapshot_deserialization()?;
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
    let response: ReorderWindowTabsV1Response =
        deserialize_exact_payload(data, is_compressed, "ReorderWindowTabsV1Response")?;
    response.validate()?;
    Ok(response)
}

fn deserialize_get_pane_render_delivery_v1(
    data: &[u8],
    is_compressed: bool,
) -> Result<GetPaneRenderDeliveryV1, Error> {
    let payload = materialize_exact_payload_with_limit(
        data,
        is_compressed,
        "GetPaneRenderDeliveryV1",
        MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES,
    )?;
    let mut reader = payload.as_ref();
    let request = bounded_varbincode::deserialize::<GetPaneRenderDeliveryV1, _>(&mut reader)?;
    if !reader.is_empty() {
        bail!("GetPaneRenderDeliveryV1 payload has trailing schema bytes");
    }
    ensure_exact_render_canonical_payload(&request, payload.as_ref(), "GetPaneRenderDeliveryV1")?;
    request.validate()?;
    Ok(request)
}

fn deserialize_get_pane_render_delivery_v1_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<GetPaneRenderDeliveryV1Response, Error> {
    let payload = materialize_exact_payload_with_limit(
        data,
        is_compressed,
        "GetPaneRenderDeliveryV1Response",
        MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES,
    )?;
    let mut reader = payload.as_ref();
    let response =
        bounded_varbincode::deserialize::<GetPaneRenderDeliveryV1Response, _>(&mut reader)?;
    if !reader.is_empty() {
        bail!("GetPaneRenderDeliveryV1Response payload has trailing schema bytes");
    }
    ensure_exact_render_canonical_payload(
        &response,
        payload.as_ref(),
        "GetPaneRenderDeliveryV1Response",
    )?;
    response.validate_with_decompressed_bytes(
        u64::try_from(payload.len()).context("exact render response length does not fit u64")?,
    )?;
    Ok(response)
}

fn ensure_exact_render_canonical_payload<T: Serialize>(
    value: &T,
    payload: &[u8],
    payload_name: &'static str,
) -> Result<(), Error> {
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Mismatch {
        Byte { offset: usize },
        CanonicalLonger { payload_bytes: usize },
    }

    struct ComparingWriter<'a> {
        payload: &'a [u8],
        offset: usize,
        mismatch: Option<Mismatch>,
    }

    impl std::io::Write for ComparingWriter<'_> {
        fn write(&mut self, canonical: &[u8]) -> std::io::Result<usize> {
            let Some(end) = self.offset.checked_add(canonical.len()) else {
                self.mismatch = Some(Mismatch::CanonicalLonger {
                    payload_bytes: self.payload.len(),
                });
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "canonical exact-render payload length overflow",
                ));
            };
            if end > self.payload.len() {
                self.mismatch = Some(Mismatch::CanonicalLonger {
                    payload_bytes: self.payload.len(),
                });
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "canonical exact-render payload is longer than received payload",
                ));
            }
            if let Some(relative) = canonical
                .iter()
                .zip(&self.payload[self.offset..end])
                .position(|(canonical, received)| canonical != received)
            {
                self.mismatch = Some(Mismatch::Byte {
                    offset: self.offset + relative,
                });
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "canonical exact-render payload byte mismatch",
                ));
            }
            self.offset = end;
            Ok(canonical.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut writer = ComparingWriter {
        payload,
        offset: 0,
        mismatch: None,
    };
    let serialization = {
        let mut serializer = varbincode::Serializer::new(&mut writer);
        value.serialize(&mut serializer)
    };
    match writer.mismatch {
        Some(Mismatch::Byte { offset }) => {
            bail!(
                "{payload_name} payload is not canonical varbincode: byte mismatch at offset {offset}"
            );
        }
        Some(Mismatch::CanonicalLonger { payload_bytes }) => {
            bail!(
                "{payload_name} payload is not canonical varbincode: canonical serialization is longer than the {payload_bytes}-byte payload"
            );
        }
        None => {
            serialization?;
        }
    }
    if writer.offset != payload.len() {
        bail!(
            "{payload_name} payload is not canonical varbincode: canonical serialization is {} bytes shorter than the {}-byte payload",
            payload.len() - writer.offset,
            payload.len(),
        );
    }
    Ok(())
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
    /// The peer understands exact generation-bound render delivery, typed
    /// settlement, and bounded immutable snapshot continuation.
    pub const EXACT_RENDER_DELIVERY_V1: Self = Self(1 << 3);

    /// Runtime-advertised capabilities.
    ///
    /// The current codec knows the ordered-window and exact-render bits, but none
    /// may be advertised until their mux authority, server dispatch, and client
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
        if self.contains(Self::EXACT_RENDER_DELIVERY_V1) && !self.contains(Self::FENCED_SNAPSHOT_V1)
        {
            return Err(
                TopologyCapabilitiesError::ExactRenderDeliveryWithoutFencedSnapshot {
                    bits: self.bits(),
                },
            );
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
    #[error(
        "topology capability bits {bits:#x} request EXACT_RENDER_DELIVERY_V1 without FENCED_SNAPSHOT_V1"
    )]
    ExactRenderDeliveryWithoutFencedSnapshot { bits: u64 },
}

/// Unpredictable identity of one connection-generation topology stream.
///
/// It is server-owned, rotates on reconnect or any loss-terminal transition,
/// and is bound to a mux-session incarnation by a coherent snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
        if self.stream_id.as_bytes() == [0; 16] || self.session_incarnation.as_bytes() == [0; 16] {
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
    /// One atomic pane-publication and floating-tab attachment transition.
    /// Geometry and presentation state are recovered from the authoritative
    /// snapshot; keeping this event compact bounds retained stream memory.
    FloatingPaneSpawned {
        pane_id: PaneId,
        tab_id: TabId,
        window_id: WindowId,
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
pub const ORDERED_WINDOW_PROTOCOL_VERSION: u16 = mux::WINDOW_REORDER_PROTOCOL_VERSION_V1;
/// Oldest negotiated codec dialect that may send ordered-window PDU IDs 86-90.
pub const ORDERED_WINDOW_V1_MIN_CODEC_VERSION: usize = 54;

#[must_use]
pub const fn codec_version_supports_ordered_window_v1(codec_version: usize) -> bool {
    codec_version >= ORDERED_WINDOW_V1_MIN_CODEC_VERSION
}

/// Hard v1 resource ceilings. Live implementations may negotiate smaller
/// budgets, but no v1 sender or receiver may exceed these values.
pub const MAX_ORDERED_WINDOWS_PER_SNAPSHOT: usize = 4_096;
pub const MAX_ORDERED_TABS_PER_WINDOW: usize = 4_096;
pub const MAX_ORDERED_TABS_PER_SNAPSHOT: usize = 16_384;
pub const MAX_ORDERED_PANE_TREE_DEPTH: usize = 64;
pub const MAX_ORDERED_PANE_LEAVES_PER_TREE: usize = 4_096;
pub const MAX_ORDERED_PANE_NODES_PER_TREE: usize = 8_191;
/// Producer-side ceiling for raw pane carriers inspected per tab before any
/// pane callback runs. This includes tree leaves, stack containers and stack
/// members, floating panes, and zoom state; it is deliberately independent of
/// the tab's position in the aggregate pane-node arena.
pub const MAX_ORDERED_PANE_CENSUS_WORK_PER_TREE: usize = 32_767;
pub const MAX_ORDERED_PANE_LEAVES_PER_SNAPSHOT: usize = 16_384;
pub const MAX_ORDERED_PANE_NODES_PER_SNAPSHOT: usize = 32_767;
/// A legal v1 section is bounded below 332 KiB by the frozen cardinality and
/// integer-only schema. The 512 KiB ceiling leaves explicit headroom while
/// keeping a hostile temporary byte buffer eight times smaller than the
/// original provisional 4 MiB cap.
pub const MAX_ORDERED_WINDOW_SECTION_BYTES: usize = 512 * 1024;
const MAX_STRUCTURALLY_VALID_ORDERED_WINDOW_SECTION_BYTES: usize = 10
    + MAX_ORDERED_WINDOWS_PER_SNAPSHOT * (10 + 10 + 10 + 1 + 10)
    + MAX_ORDERED_TABS_PER_SNAPSHOT * 10;
const _: () = assert!(
    MAX_STRUCTURALLY_VALID_ORDERED_WINDOW_SECTION_BYTES <= MAX_ORDERED_WINDOW_SECTION_BYTES
);
const _: () = assert!(
    MAX_ORDERED_WINDOW_SECTION_BYTES == bounded_varbincode::ORDERED_WINDOW_SECTION_V1_MAX_BYTES
);
const _: () = assert!(
    MAX_ORDERED_TABS_PER_SNAPSHOT == bounded_varbincode::ORDERED_PANE_TREE_DESCRIPTORS_V1_MAX_ITEMS
);
const _: () = assert!(
    MAX_ORDERED_PANE_NODES_PER_SNAPSHOT == bounded_varbincode::ORDERED_PANE_NODES_V1_MAX_ITEMS
);
const _: () = assert!(
    MAX_ORDERED_WINDOWS_PER_SNAPSHOT == bounded_varbincode::ORDERED_PANE_WINDOW_TITLES_V1_MAX_ITEMS
);
const _: () =
    assert!(MAX_ORDERED_WINDOWS_PER_SNAPSHOT == bounded_varbincode::ORDERED_WINDOWS_V1_MAX_ITEMS);
const _: () =
    assert!(MAX_ORDERED_TABS_PER_WINDOW == bounded_varbincode::ORDERED_TAB_IDS_V1_MAX_ITEMS);
/// Total decompressed body ceiling for PDU 87, including the pane snapshot and
/// ordered-window section. Sixteen MiB admits the pinned representative
/// 4,096-window/16,384-tab fixture without inheriting the global 256 MiB PDU
/// allocation envelope. Structurally valid snapshots with unusually large
/// pane metadata can still exceed this explicit wire budget and fail closed.
pub const MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES: usize = 16 * 1024 * 1024;
/// Frozen zstd `compressBound` result for a legal PDU 87 body. At sixteen MiB
/// the small-input term is zero, leaving `input + input / 256`.
pub const MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES: usize =
    MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES
        + (MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES >> 8);
/// Maximum complete PDU 87 frame allocation. A compressed frame has a
/// ten-byte tagged-length LEB128, a worst-case ten-byte serial, and the
/// one-byte wire identifier in addition to its bounded encoded body.
pub const MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES: usize =
    MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES + 21;
pub const MAX_REORDER_WINDOW_TABS_DECOMPRESSED_BYTES: usize = 512 * 1024;
/// The 512 KiB contract is an outer body ceiling, not merely a decompressed
/// budget. Canonical q4096 requests are far smaller, so legal frames need no
/// zstd worst-case headroom beyond this limit.
pub const MAX_REORDER_WINDOW_TABS_ZSTD_ENCODED_BYTES: usize =
    MAX_REORDER_WINDOW_TABS_DECOMPRESSED_BYTES;

/// Debug-build counters for deterministic validation-complexity tests.
///
/// Thread-local storage keeps parallel tests independent. Release builds carry
/// neither these counters nor increments on the interactive PDU 87 path.
#[cfg(debug_assertions)]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OrderedSnapshotValidationPasses {
    pub pane_arena: usize,
    pub ordered_windows: usize,
}

#[cfg(debug_assertions)]
std::thread_local! {
    static DEBUG_ORDERED_SNAPSHOT_VALIDATION_PASSES:
        std::cell::Cell<OrderedSnapshotValidationPasses> =
            const { std::cell::Cell::new(OrderedSnapshotValidationPasses {
                pane_arena: 0,
                ordered_windows: 0,
            }) };
}

#[cfg(debug_assertions)]
#[doc(hidden)]
pub fn debug_reset_ordered_snapshot_validation_passes() {
    DEBUG_ORDERED_SNAPSHOT_VALIDATION_PASSES.set(OrderedSnapshotValidationPasses::default());
}

#[cfg(debug_assertions)]
#[doc(hidden)]
#[must_use]
pub fn debug_ordered_snapshot_validation_passes() -> OrderedSnapshotValidationPasses {
    DEBUG_ORDERED_SNAPSHOT_VALIDATION_PASSES.get()
}

#[cfg(debug_assertions)]
fn debug_record_ordered_pane_arena_validation_pass() {
    let passes = DEBUG_ORDERED_SNAPSHOT_VALIDATION_PASSES.get();
    DEBUG_ORDERED_SNAPSHOT_VALIDATION_PASSES.set(OrderedSnapshotValidationPasses {
        pane_arena: passes.pane_arena.saturating_add(1),
        ..passes
    });
}

#[cfg(debug_assertions)]
fn debug_record_ordered_window_validation_pass() {
    let passes = DEBUG_ORDERED_SNAPSHOT_VALIDATION_PASSES.get();
    DEBUG_ORDERED_SNAPSHOT_VALIDATION_PASSES.set(OrderedSnapshotValidationPasses {
        ordered_windows: passes.ordered_windows.saturating_add(1),
        ..passes
    });
}

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
pub const WINDOW_REORDER_DIGEST_DOMAIN_V1: &[u8] = mux::WINDOW_REORDER_DIGEST_DOMAIN_V1;

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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
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
        validate_ordered_windows_structure(std::slice::from_ref(self), false)
    }
}

/// One coherent pane plus ordered-window bootstrap at a shared topology
/// revision. This is the PDU87 success body; legacy PDU4/PDU82 pane listings
/// alone do not acquire ordering authority.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct OrderedPaneSnapshotV1 {
    pub session_incarnation: MuxSessionIncarnation,
    pub topology_revision: TopologyRevision,
    #[serde(
        serialize_with = "serialize_ordered_panes",
        deserialize_with = "deserialize_ordered_panes"
    )]
    pub panes: PaneArena,
    #[serde(
        serialize_with = "serialize_floating_pane_snapshot",
        deserialize_with = "deserialize_floating_pane_snapshot"
    )]
    pub floating_panes: Vec<FloatingPaneSnapshotEntry>,
    #[serde(
        serialize_with = "serialize_ordered_window_section",
        deserialize_with = "deserialize_ordered_window_section"
    )]
    pub ordered_windows: Vec<OrderedWindowStateV1>,
}

impl OrderedPaneSnapshotV1 {
    fn validate_envelope(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_nonzero_identity("session_incarnation", self.session_incarnation.as_bytes())?;
        validate_topology_revision(self.topology_revision)
    }

    fn validate_envelope_and_panes(&self) -> Result<(), OrderedWindowProtocolError> {
        self.validate_envelope()?;
        validate_ordered_pane_arena(&self.panes)?;
        validate_floating_pane_snapshot(&self.floating_panes)
            .map_err(OrderedWindowProtocolError::FloatingPaneSnapshot)
    }

    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        self.validate_envelope_and_panes()?;
        validate_ordered_windows_structure(&self.ordered_windows, false)
    }
}

/// Negotiated request for a coherent pane and ordered-window bootstrap.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesOrderedV1 {
    pub protocol_version: u16,
    pub domain_binding_id: DomainBindingId,
    pub supported: TopologyCapabilities,
    pub required: TopologyCapabilities,
}

impl ListPanesOrderedV1 {
    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        validate_nonzero_identity("domain_binding_id", self.domain_binding_id.as_bytes())?;
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
    pub domain_binding_id: DomainBindingId,
    pub negotiated: TopologyCapabilities,
    pub stream_id: TopologyStreamId,
    pub outcome: ListPanesOrderedV1Outcome,
}

impl ListPanesOrderedV1Response {
    /// Complete the outer PDU checks after the custom bounded field decoders
    /// have already validated the exact pane arena and ordered-window section.
    fn validate_after_bounded_snapshot_deserialization(
        &self,
    ) -> Result<(), OrderedWindowProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        validate_nonzero_identity("domain_binding_id", self.domain_binding_id.as_bytes())?;
        self.negotiated.validate()?;
        validate_nonzero_identity("stream_id", self.stream_id.as_bytes())?;
        match &self.outcome {
            ListPanesOrderedV1Outcome::Snapshot(snapshot) => snapshot.validate_envelope()?,
            other => other.validate()?,
        }
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

    pub fn validate(&self) -> Result<(), OrderedWindowProtocolError> {
        validate_protocol_version(self.protocol_version)?;
        validate_nonzero_identity("domain_binding_id", self.domain_binding_id.as_bytes())?;
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

    /// Validate PDU87 against the exact PDU86 that established its connection
    /// generation. The binding echo prevents overlapping or retried snapshots
    /// from installing authority for a different durable client binding.
    pub fn validate_for_request(
        &self,
        request: &ListPanesOrderedV1,
    ) -> Result<(), OrderedWindowProtocolError> {
        request.validate()?;
        self.validate()?;
        if self.domain_binding_id != request.domain_binding_id {
            return Err(OrderedWindowProtocolError::DomainBindingEchoMismatch {
                expected: request.domain_binding_id,
                actual: self.domain_binding_id,
            });
        }
        if !request.supported.contains(self.negotiated) {
            return Err(
                OrderedWindowProtocolError::NegotiatedCapabilitiesNotOffered {
                    supported: request.supported.bits(),
                    negotiated: self.negotiated.bits(),
                },
            );
        }
        if matches!(&self.outcome, ListPanesOrderedV1Outcome::Snapshot(_))
            && !self.negotiated.contains(request.required)
        {
            return Err(OrderedWindowProtocolError::MissingNegotiatedCapabilities {
                negotiated: self.negotiated.bits(),
                missing: request.required.bits() & !self.negotiated.bits(),
            });
        }
        Ok(())
    }

    /// Consume and bind this response to the exact request that authorized it.
    ///
    /// The returned owner has no public constructor and cannot be cloned or
    /// separated from the value it proves. Its encoder therefore reuses this
    /// completed validation instead of rescanning the potentially q-sized pane
    /// arena and ordered-window graph. Borrowed validation and every ordinary
    /// [`Pdu`] encoding entry point remain independently fail-closed.
    pub fn validate_for_request_owned(
        self,
        request: &ListPanesOrderedV1,
    ) -> Result<ValidatedListPanesOrderedV1Response, OrderedWindowProtocolError> {
        self.validate_for_request(request)?;
        Ok(ValidatedListPanesOrderedV1Response { response: self })
    }
}

/// Opaque ownership proof for one request-correlated PDU 87 response.
///
/// Only [`ListPanesOrderedV1Response::validate_for_request_owned`] can create
/// this type. Keeping the validated value private prevents callers from
/// pairing a proof with a different response after validation.
#[must_use = "a validated ordered-pane response should be encoded or inspected"]
#[derive(Debug)]
pub struct ValidatedListPanesOrderedV1Response {
    response: ListPanesOrderedV1Response,
}

impl ValidatedListPanesOrderedV1Response {
    /// Borrow the exact response retained by this validation proof.
    #[must_use]
    pub const fn as_response(&self) -> &ListPanesOrderedV1Response {
        &self.response
    }

    /// Encode the request-correlated response without repeating structural
    /// validation of the exact owned pane arena or ordered-window graph.
    ///
    /// This is deliberately narrower than the public [`Pdu`] encoders: it
    /// always emits one ordinary auto-compressed PDU 87 frame and consumes the
    /// proof owner. Untrusted values must use the public validation/encoding
    /// entry points or first acquire this proof from the exact PDU 86 request.
    pub fn encode_frame(self, serial: u64) -> Result<Vec<u8>, Error> {
        Pdu::ListPanesOrderedV1Response(self.response).encode_frame_with_mode_after_validation(
            serial,
            CompressionMode::Auto,
            true,
        )
    }
}

#[derive(Clone, Copy)]
struct ValidatedOrderedSerializationTarget {
    panes: Option<*const PaneArena>,
    windows: *const OrderedWindowStateV1,
    windows_len: usize,
}

std::thread_local! {
    /// Exact-object capability active only during the synchronous serde pass
    /// of an already validated PDU 87 snapshot or PDU 90 order event. Raw
    /// pointers are compared but never dereferenced; the borrowed PDU outlives
    /// the scope that installs them.
    static VALIDATED_ORDERED_SERIALIZATION:
        std::cell::Cell<Option<ValidatedOrderedSerializationTarget>> =
            const { std::cell::Cell::new(None) };
}

/// Synchronous, thread-affine capability for eliding field-level validation
/// of one exact ordered value. It never crosses an async write await point.
struct ValidatedOrderedSerializationScope;

impl ValidatedOrderedSerializationScope {
    fn for_pdu(pdu: &Pdu) -> Result<Option<Self>, Error> {
        let target = match pdu {
            Pdu::ListPanesOrderedV1Response(ListPanesOrderedV1Response {
                outcome: ListPanesOrderedV1Outcome::Snapshot(snapshot),
                ..
            }) => ValidatedOrderedSerializationTarget {
                panes: Some(std::ptr::from_ref(&snapshot.panes)),
                windows: snapshot.ordered_windows.as_ptr(),
                windows_len: snapshot.ordered_windows.len(),
            },
            Pdu::WindowOrderEventV1(event) => ValidatedOrderedSerializationTarget {
                panes: None,
                windows: event.windows.as_ptr(),
                windows_len: event.windows.len(),
            },
            _ => return Ok(None),
        };
        VALIDATED_ORDERED_SERIALIZATION.with(|active| {
            if let Some(previous) = active.replace(Some(target)) {
                active.set(Some(previous));
                bail!("nested validated ordered serialization is not supported");
            }
            Ok(())
        })?;
        Ok(Some(Self))
    }
}

impl Drop for ValidatedOrderedSerializationScope {
    fn drop(&mut self) {
        VALIDATED_ORDERED_SERIALIZATION.with(|active| {
            debug_assert!(
                active.replace(None).is_some(),
                "validated ordered serialization scope lost its exact-object target"
            );
        });
    }
}

fn ordered_pane_arena_has_serialization_proof(panes: &PaneArena) -> bool {
    VALIDATED_ORDERED_SERIALIZATION.with(|active| {
        active.get().is_some_and(|target| {
            target
                .panes
                .is_some_and(|target_panes| std::ptr::eq(target_panes, panes))
        })
    })
}

fn ordered_windows_have_serialization_proof(windows: &[OrderedWindowStateV1]) -> bool {
    VALIDATED_ORDERED_SERIALIZATION.with(|active| {
        active.get().is_some_and(|target| {
            target.windows_len == windows.len() && std::ptr::eq(target.windows, windows.as_ptr())
        })
    })
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
        let digest = mux::canonical_window_reorder_digest_v1(
            mux::WindowReorderDigestInputV1 {
                protocol_version: self.protocol_version,
                domain_binding_id: self.domain_binding_id.as_bytes(),
                session_incarnation: self.session_incarnation,
                window_id: self.window_id.get(),
                expected_order_revision: self.expected_order_revision.get(),
                desired_active_tab_id: self.desired_active_tab_id.map(RemoteTabId::get),
                mutation_id: mux::WindowOrderMutationId::new(
                    self.mutation_id.namespace,
                    self.mutation_id.sequence,
                ),
            },
            self.desired_tab_ids.iter().map(|tab_id| tab_id.get()),
        );
        WindowReorderDigest::from_bytes(digest.as_bytes())
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
        validate_nonzero_identity("session_incarnation", self.session_incarnation.as_bytes())?;
        validate_remote_wire_id("window_id", self.window_id.get())?;
        validate_window_order_revision(self.expected_order_revision)?;
        self.mutation_id.validate()?;
        validate_reorder_window_representation(
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
        validate_nonzero_identity("session_incarnation", self.session_incarnation.as_bytes())?;
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
        validate_nonzero_identity("session_incarnation", self.session_incarnation.as_bytes())?;
        validate_topology_revision(self.topology_revision)?;
        if self.windows.len() > MAX_ORDERED_WINDOWS_PER_EVENT {
            return Err(OrderedWindowProtocolError::TooManyEventWindows {
                count: self.windows.len(),
                max: MAX_ORDERED_WINDOWS_PER_EVENT,
            });
        }
        validate_ordered_windows_structure(&self.windows, true)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum OrderedWindowProtocolError {
    #[error("ordered-window protocol version {actual} is unsupported; expected {expected}")]
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
    #[error("ordered-window PDU87 binding {actual:?} does not echo PDU86 binding {expected:?}")]
    DomainBindingEchoMismatch {
        expected: DomainBindingId,
        actual: DomainBindingId,
    },
    #[error(
        "ordered-window negotiated capabilities {negotiated:#x} are not a subset of offered capabilities {supported:#x}"
    )]
    NegotiatedCapabilitiesNotOffered { supported: u64, negotiated: u64 },
    #[error("ordered-window identity {field} uses the reserved all-zero value")]
    ReservedIdentity { field: &'static str },
    #[error("ordered-window wire identity {field} uses reserved value {value}")]
    ReservedWireId { field: &'static str, value: u64 },
    #[error("ordered-window wire identity {field}={value} does not fit in usize")]
    WireIdDoesNotFitUsize { field: &'static str, value: u64 },
    #[error("ordered-window process identity {field}={value} does not fit in u64")]
    ProcessIdDoesNotFitWire { field: &'static str, value: usize },
    #[error("ordered-window revision {field} uses the terminal u64::MAX sentinel")]
    RevisionExhausted { field: &'static str },
    #[error("ordered-window snapshot has {count} windows; maximum is {max}")]
    TooManyWindows { count: usize, max: usize },
    #[error("ordered-window pane snapshot has {count} tab trees; maximum is {max}")]
    TooManyPaneTrees { count: usize, max: usize },
    #[error("ordered-window pane snapshot has {count} tab titles; maximum is {max}")]
    TooManyPaneTabTitles { count: usize, max: usize },
    #[error("ordered-window pane snapshot has {count} window titles; maximum is {max}")]
    TooManyPaneWindowTitles { count: usize, max: usize },
    #[error("ordered-window pane snapshot has {trees} tab trees but {titles} tab titles")]
    PaneTreeTitleCardinalityMismatch { trees: usize, titles: usize },
    #[error("ordered-window pane snapshot has {count} arena nodes; maximum is {max}")]
    TooManyPaneNodes { count: usize, max: usize },
    #[error("ordered-window pane snapshot has {count} pane leaves; maximum is {max}")]
    TooManyPaneLeaves { count: usize, max: usize },
    #[error("ordered-window floating-pane snapshot is invalid: {0}")]
    FloatingPaneSnapshot(FloatingPaneSnapshotError),
    #[error(
        "ordered pane-arena conversion cannot discard {count} floating panes; carry them in OrderedPaneSnapshotV1::floating_panes"
    )]
    FloatingPaneProjectionLost { count: usize },
    #[error("ordered-window pane tree {tree_index} has {count} {resource}; maximum is {max}")]
    PaneTreeResourceLimit {
        tree_index: usize,
        resource: &'static str,
        count: usize,
        max: usize,
    },
    #[error("ordered-window pane tree {tree_index} descriptor is invalid: {detail}")]
    InvalidPaneTreeDescriptor {
        tree_index: usize,
        detail: &'static str,
    },
    #[error("ordered-window pane tree {tree_index} node {node_index} is invalid: {detail}")]
    InvalidPaneArenaNode {
        tree_index: usize,
        node_index: usize,
        detail: &'static str,
    },
    #[error("ordered-window pane arena references {referenced} nodes but carries {total}")]
    PaneArenaCardinalityMismatch { referenced: usize, total: usize },
    #[error("ordered-window pane arena allocation failed within its admitted bounds")]
    PaneArenaAllocation,
    #[error("ordered-window pane window titles are not in strictly increasing window-id order")]
    NonCanonicalPaneWindowTitleOrder,
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
    ActiveTabNotInWindow { window_id: u64, active_tab_id: u64 },
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
    // Live mux WindowId/TabId allocators begin at zero. Absence belongs in an
    // enclosing Option; only the nonrepresentable/exhausted sentinel is
    // reserved on this fixed-width wire surface.
    if value == u64::MAX {
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

#[cfg(test)]
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

#[cfg(test)]
fn validate_ordered_windows_with_section_limit(
    windows: &[OrderedWindowStateV1],
    require_nonempty: bool,
    max_section_bytes: usize,
) -> Result<(), OrderedWindowProtocolError> {
    validate_ordered_windows_structure(windows, require_nonempty)?;
    let section_bytes = encoded_ordered_window_section_len(windows)?;
    if section_bytes > max_section_bytes {
        return Err(OrderedWindowProtocolError::OrderSectionTooLarge {
            bytes: section_bytes,
            max: max_section_bytes,
        });
    }
    Ok(())
}

fn validate_ordered_windows_structure(
    windows: &[OrderedWindowStateV1],
    require_nonempty: bool,
) -> Result<(), OrderedWindowProtocolError> {
    #[cfg(debug_assertions)]
    debug_record_ordered_window_validation_pass();

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
        validate_ordered_window_components(
            window.window_id,
            window.order_revision,
            &window.ordered_tab_ids,
            window.active_tab_id,
            &mut tab_ids,
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
    }
    Ok(())
}

/// Validate only the closed wire representation of a reorder intent.
///
/// Duplicate, foreign, missing, and active-membership checks require the
/// exact authoritative mux window. Deferring those semantic checks preserves
/// the protocol outcome order: session/window identity and retained-receipt
/// classification must precede permutation malformation.
fn validate_reorder_window_representation(
    window_id: RemoteWindowId,
    expected_order_revision: WindowOrderRevision,
    desired_tab_ids: &[RemoteTabId],
    desired_active_tab_id: Option<RemoteTabId>,
) -> Result<(), OrderedWindowProtocolError> {
    validate_remote_wire_id("window_id", window_id.get())?;
    validate_window_order_revision(expected_order_revision)?;
    if desired_tab_ids.len() > MAX_ORDERED_TABS_PER_WINDOW {
        return Err(OrderedWindowProtocolError::TooManyTabs {
            window_id: window_id.get(),
            count: desired_tab_ids.len(),
            max: MAX_ORDERED_TABS_PER_WINDOW,
        });
    }
    for tab_id in desired_tab_ids {
        validate_remote_wire_id("tab_id", tab_id.get())?;
    }
    if let Some(active_tab_id) = desired_active_tab_id {
        validate_remote_wire_id("active_tab_id", active_tab_id.get())?;
    }
    Ok(())
}

fn validate_ordered_window_components(
    window_id: RemoteWindowId,
    order_revision: WindowOrderRevision,
    ordered_tab_ids: &[RemoteTabId],
    active_tab_id: Option<RemoteTabId>,
    snapshot_tab_ids: &mut HashSet<RemoteTabId>,
) -> Result<(), OrderedWindowProtocolError> {
    validate_remote_wire_id("window_id", window_id.get())?;
    validate_window_order_revision(order_revision)?;
    if ordered_tab_ids.len() > MAX_ORDERED_TABS_PER_WINDOW {
        return Err(OrderedWindowProtocolError::TooManyTabs {
            window_id: window_id.get(),
            count: ordered_tab_ids.len(),
            max: MAX_ORDERED_TABS_PER_WINDOW,
        });
    }

    for tab_id in ordered_tab_ids {
        validate_remote_wire_id("tab_id", tab_id.get())?;
        if !snapshot_tab_ids.insert(*tab_id) {
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
        (_, Some(active_tab_id)) if !ordered_tab_ids.contains(&active_tab_id) => {
            return Err(OrderedWindowProtocolError::ActiveTabNotInWindow {
                window_id: window_id.get(),
                active_tab_id: active_tab_id.get(),
            });
        }
        _ => {}
    }
    Ok(())
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

fn serialize_bounded_newtype_vec<S, T, const MAX: usize>(
    values: &[T],
    serializer: S,
    label: &'static str,
    newtype_name: &'static str,
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
    serializer.serialize_newtype_struct(newtype_name, values)
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
        while values.len() < MAX {
            let Some(value) = sequence.next_element()? else {
                return Ok(values);
            };
            values.push(value);
        }
        if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom(format_args!(
                "{} length exceeds maximum {MAX}",
                self.label
            )));
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

struct BoundedNewtypeVecVisitor<T, const MAX: usize> {
    label: &'static str,
    marker: std::marker::PhantomData<T>,
}

impl<'de, T, const MAX: usize> serde::de::Visitor<'de> for BoundedNewtypeVecVisitor<T, MAX>
where
    T: Deserialize<'de>,
{
    type Value = Vec<T>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "a bounded {} newtype", self.label)
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_bounded_vec::<D, T, MAX>(deserializer, self.label)
    }
}

fn deserialize_bounded_newtype_vec<'de, D, T, const MAX: usize>(
    deserializer: D,
    label: &'static str,
    newtype_name: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    deserializer.deserialize_newtype_struct(
        newtype_name,
        BoundedNewtypeVecVisitor::<T, MAX> {
            label,
            marker: std::marker::PhantomData,
        },
    )
}

struct BoundedMapVisitor<K, V, const MAX: usize> {
    label: &'static str,
    marker: std::marker::PhantomData<(K, V)>,
}

impl<'de, K, V, const MAX: usize> serde::de::Visitor<'de> for BoundedMapVisitor<K, V, MAX>
where
    K: Deserialize<'de> + Eq + std::hash::Hash,
    V: Deserialize<'de>,
{
    type Value = HashMap<K, V>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "at most {MAX} {}", self.label)
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let hinted = map.size_hint().unwrap_or(0);
        if hinted > MAX {
            return Err(serde::de::Error::custom(format_args!(
                "{} length {hinted} exceeds maximum {MAX}",
                self.label
            )));
        }
        let mut values = HashMap::new();
        values.try_reserve(hinted).map_err(|error| {
            serde::de::Error::custom(format_args!(
                "allocating {} length {hinted} failed: {error}",
                self.label
            ))
        })?;
        let mut entries = 0_usize;
        while entries < MAX {
            let Some((key, value)) = map.next_entry()? else {
                return Ok(values);
            };
            if let std::collections::hash_map::Entry::Vacant(entry) = values.entry(key) {
                entry.insert(value);
            } else {
                return Err(serde::de::Error::custom(format_args!(
                    "{} contains a duplicate key",
                    self.label
                )));
            }
            entries += 1;
        }
        if map.next_key::<serde::de::IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom(format_args!(
                "{} length exceeds maximum {MAX}",
                self.label
            )));
        }
        Ok(values)
    }
}

fn deserialize_bounded_map<'de, D, K, V, const MAX: usize>(
    deserializer: D,
    label: &'static str,
) -> Result<HashMap<K, V>, D::Error>
where
    D: serde::Deserializer<'de>,
    K: Deserialize<'de> + Eq + std::hash::Hash,
    V: Deserialize<'de>,
{
    deserializer.deserialize_map(BoundedMapVisitor::<K, V, MAX> {
        label,
        marker: std::marker::PhantomData,
    })
}

fn serialize_ordered_panes<S>(panes: &PaneArena, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if !ordered_pane_arena_has_serialization_proof(panes) {
        validate_ordered_pane_arena(panes).map_err(serde::ser::Error::custom)?;
    }
    PaneArenaWireRef {
        trees: PaneArenaTreesWire(panes.trees()),
        nodes: PaneArenaNodesWire(panes.nodes()),
        window_titles: PaneArenaWindowTitlesWire(panes.window_titles()),
    }
    .serialize(serializer)
}

struct PaneArenaTreesWire<'a>(&'a [PaneArenaTree]);

impl Serialize for PaneArenaTreesWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_newtype_struct(
            bounded_varbincode::ORDERED_PANE_TREE_DESCRIPTORS_V1_NEWTYPE,
            self.0,
        )
    }
}

struct PaneArenaNodesWire<'a>(&'a [PaneArenaNode]);

impl Serialize for PaneArenaNodesWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer
            .serialize_newtype_struct(bounded_varbincode::ORDERED_PANE_NODES_V1_NEWTYPE, self.0)
    }
}

struct PaneArenaWindowTitlesWire<'a>(&'a [PaneArenaWindowTitle]);

impl Serialize for PaneArenaWindowTitlesWire<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_newtype_struct(
            bounded_varbincode::ORDERED_PANE_WINDOW_TITLES_V1_NEWTYPE,
            self.0,
        )
    }
}

#[derive(Serialize)]
struct PaneArenaWireRef<'a> {
    trees: PaneArenaTreesWire<'a>,
    nodes: PaneArenaNodesWire<'a>,
    window_titles: PaneArenaWindowTitlesWire<'a>,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct OrderedPanesFlattenStats {
    node_visits: usize,
    leaf_visits: usize,
    task_pushes: usize,
    peak_pending_tasks: usize,
    flatten_allocation_requests: usize,
}

struct OrderedPanesFlattenResult {
    panes: PaneArena,
    #[cfg(test)]
    stats: OrderedPanesFlattenStats,
}

enum FlattenPaneTask {
    Visit {
        node: PaneNode,
        depth: usize,
    },
    VisitRight {
        split_index: usize,
        right: PaneNode,
        depth: usize,
    },
}

fn reserve_flat_node_slot<T>(values: &mut Vec<T>) -> Result<bool, OrderedWindowProtocolError> {
    if values.len() == MAX_ORDERED_PANE_NODES_PER_SNAPSHOT {
        return Err(OrderedWindowProtocolError::TooManyPaneNodes {
            count: values.len().saturating_add(1),
            max: MAX_ORDERED_PANE_NODES_PER_SNAPSHOT,
        });
    }
    if values.len() == values.capacity() {
        let remaining = MAX_ORDERED_PANE_NODES_PER_SNAPSHOT - values.len();
        let additional = values.capacity().max(8).min(remaining);
        values
            .try_reserve_exact(additional)
            .map_err(|_| OrderedWindowProtocolError::PaneArenaAllocation)?;
        return Ok(true);
    }
    Ok(false)
}

/// Consume a legacy recursive pane listing into the PDU87 application arena.
///
/// Production ordered-snapshot capture now appends directly into the flat
/// arena. This bridge remains for bounded fixtures and callers that already
/// own a legacy [`ListPanesResponse`]. Pane entries and title strings move into
/// the arena without cloning, and no recursive transfer tree is reconstructed.
pub fn ordered_pane_arena_from_list_panes(
    panes: ListPanesResponse,
) -> Result<PaneArena, OrderedWindowProtocolError> {
    Ok(flatten_ordered_panes(panes)?.panes)
}

fn flatten_ordered_panes(
    panes: ListPanesResponse,
) -> Result<OrderedPanesFlattenResult, OrderedWindowProtocolError> {
    let ListPanesResponse {
        tabs,
        tab_titles,
        window_titles: pane_window_titles,
        floating_panes,
    } = panes;
    if !floating_panes.is_empty() {
        return Err(OrderedWindowProtocolError::FloatingPaneProjectionLost {
            count: floating_panes.len(),
        });
    }
    if tabs.len() > MAX_ORDERED_TABS_PER_SNAPSHOT {
        return Err(OrderedWindowProtocolError::TooManyPaneTrees {
            count: tabs.len(),
            max: MAX_ORDERED_TABS_PER_SNAPSHOT,
        });
    }
    if tabs.len() != tab_titles.len() {
        return Err(
            OrderedWindowProtocolError::PaneTreeTitleCardinalityMismatch {
                trees: tabs.len(),
                titles: tab_titles.len(),
            },
        );
    }
    if pane_window_titles.len() > MAX_ORDERED_WINDOWS_PER_SNAPSHOT {
        return Err(OrderedWindowProtocolError::TooManyPaneWindowTitles {
            count: pane_window_titles.len(),
            max: MAX_ORDERED_WINDOWS_PER_SNAPSHOT,
        });
    }

    let mut descriptors = Vec::new();
    descriptors
        .try_reserve_exact(tabs.len())
        .map_err(|_| OrderedWindowProtocolError::PaneArenaAllocation)?;
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(tabs.len().min(MAX_ORDERED_PANE_NODES_PER_SNAPSHOT))
        .map_err(|_| OrderedWindowProtocolError::PaneArenaAllocation)?;
    let mut tasks = Vec::new();
    tasks
        .try_reserve_exact(MAX_ORDERED_PANE_TREE_DEPTH.saturating_mul(2))
        .map_err(|_| OrderedWindowProtocolError::PaneArenaAllocation)?;
    let mut total_leaves = 0_usize;
    #[cfg(test)]
    let mut stats = OrderedPanesFlattenStats {
        flatten_allocation_requests: (if tabs.is_empty() { 0 } else { 2 })
            + 1
            + (if pane_window_titles.is_empty() { 0 } else { 1 }),
        ..OrderedPanesFlattenStats::default()
    };

    for (tree_index, (tree, title)) in tabs.into_iter().zip(tab_titles).enumerate() {
        if matches!(tree, PaneNode::Empty) {
            return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                tree_index,
                detail:
                    "ordered pane snapshots cannot recreate an empty tab without size authority",
            });
        }

        let tree_start = nodes.len();
        let root_index = u32::try_from(tree_start).map_err(|_| {
            OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                tree_index,
                detail: "root index does not fit the closed u32 wire type",
            }
        })?;
        let mut tree_leaves = 0_usize;
        tasks.push(FlattenPaneTask::Visit {
            node: tree,
            depth: 1,
        });
        #[cfg(test)]
        {
            stats.task_pushes += 1;
            stats.peak_pending_tasks = stats.peak_pending_tasks.max(tasks.len());
        }

        while let Some(task) = tasks.pop() {
            match task {
                FlattenPaneTask::Visit { node, depth } => {
                    #[cfg(test)]
                    {
                        stats.node_visits += 1;
                    }
                    if depth > MAX_ORDERED_PANE_TREE_DEPTH {
                        return Err(OrderedWindowProtocolError::PaneTreeResourceLimit {
                            tree_index,
                            resource: "levels",
                            count: depth,
                            max: MAX_ORDERED_PANE_TREE_DEPTH,
                        });
                    }
                    let next_tree_count = nodes.len() - tree_start + 1;
                    if next_tree_count > MAX_ORDERED_PANE_NODES_PER_TREE {
                        return Err(OrderedWindowProtocolError::PaneTreeResourceLimit {
                            tree_index,
                            resource: "nodes",
                            count: next_tree_count,
                            max: MAX_ORDERED_PANE_NODES_PER_TREE,
                        });
                    }
                    let node_allocation = reserve_flat_node_slot(&mut nodes)?;
                    #[cfg(test)]
                    {
                        if node_allocation {
                            stats.flatten_allocation_requests += 1;
                        }
                    }
                    #[cfg(not(test))]
                    let _ = node_allocation;
                    let node_index = nodes.len();
                    match node {
                        PaneNode::Empty => nodes.push(PaneArenaNode::Empty),
                        PaneNode::Leaf(entry) => {
                            #[cfg(test)]
                            {
                                stats.leaf_visits += 1;
                            }
                            tree_leaves = tree_leaves
                                .checked_add(1)
                                .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                            total_leaves = total_leaves
                                .checked_add(1)
                                .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                            if tree_leaves > MAX_ORDERED_PANE_LEAVES_PER_TREE {
                                return Err(OrderedWindowProtocolError::PaneTreeResourceLimit {
                                    tree_index,
                                    resource: "leaves",
                                    count: tree_leaves,
                                    max: MAX_ORDERED_PANE_LEAVES_PER_TREE,
                                });
                            }
                            if total_leaves > MAX_ORDERED_PANE_LEAVES_PER_SNAPSHOT {
                                return Err(OrderedWindowProtocolError::TooManyPaneLeaves {
                                    count: total_leaves,
                                    max: MAX_ORDERED_PANE_LEAVES_PER_SNAPSHOT,
                                });
                            }
                            nodes.push(PaneArenaNode::Leaf(entry));
                        }
                        PaneNode::Split { left, right, node } => {
                            if depth == MAX_ORDERED_PANE_TREE_DEPTH {
                                return Err(OrderedWindowProtocolError::PaneTreeResourceLimit {
                                    tree_index,
                                    resource: "levels",
                                    count: depth.saturating_add(1),
                                    max: MAX_ORDERED_PANE_TREE_DEPTH,
                                });
                            }
                            let left_index =
                                u32::try_from(node_index.saturating_add(1)).map_err(|_| {
                                    OrderedWindowProtocolError::InvalidPaneArenaNode {
                                        tree_index,
                                        node_index,
                                        detail: "left child index does not fit u32",
                                    }
                                })?;
                            nodes.push(PaneArenaNode::Split {
                                left: left_index,
                                right: u32::MAX,
                                node,
                            });
                            let child_depth = depth
                                .checked_add(1)
                                .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                            tasks.push(FlattenPaneTask::VisitRight {
                                split_index: node_index,
                                right: *right,
                                depth: child_depth,
                            });
                            tasks.push(FlattenPaneTask::Visit {
                                node: *left,
                                depth: child_depth,
                            });
                            #[cfg(test)]
                            {
                                stats.task_pushes += 2;
                                stats.peak_pending_tasks =
                                    stats.peak_pending_tasks.max(tasks.len());
                            }
                        }
                    }
                }
                FlattenPaneTask::VisitRight {
                    split_index,
                    right,
                    depth,
                } => {
                    let right_index = u32::try_from(nodes.len()).map_err(|_| {
                        OrderedWindowProtocolError::InvalidPaneArenaNode {
                            tree_index,
                            node_index: split_index,
                            detail: "right child index does not fit u32",
                        }
                    })?;
                    let PaneArenaNode::Split {
                        right: stored_right,
                        ..
                    } = &mut nodes[split_index]
                    else {
                        return Err(OrderedWindowProtocolError::InvalidPaneArenaNode {
                            tree_index,
                            node_index: split_index,
                            detail: "split completion did not reference a split node",
                        });
                    };
                    *stored_right = right_index;
                    tasks.push(FlattenPaneTask::Visit { node: right, depth });
                    #[cfg(test)]
                    {
                        stats.task_pushes += 1;
                        stats.peak_pending_tasks = stats.peak_pending_tasks.max(tasks.len());
                    }
                }
            }
        }

        if tree_leaves == 0 {
            return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                tree_index,
                detail: "non-empty tree contains no pane leaves",
            });
        }
        let node_count = u32::try_from(nodes.len() - tree_start).map_err(|_| {
            OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                tree_index,
                detail: "node count does not fit u32",
            }
        })?;
        descriptors.push(PaneArenaTree {
            root_index: Some(root_index),
            node_count,
            tab_title: title,
        });
    }

    let mut window_titles = Vec::new();
    window_titles
        .try_reserve_exact(pane_window_titles.len())
        .map_err(|_| OrderedWindowProtocolError::PaneArenaAllocation)?;
    for (window_id, title) in pane_window_titles {
        let window_id = u64::try_from(window_id).map_err(|_| {
            OrderedWindowProtocolError::ProcessIdDoesNotFitWire {
                field: "window_id",
                value: window_id,
            }
        })?;
        validate_remote_wire_id("window_id", window_id)?;
        window_titles.push(PaneArenaWindowTitle { window_id, title });
    }
    window_titles.sort_unstable_by_key(|entry| entry.window_id);

    let panes = PaneArena::from_unvalidated_parts(descriptors, nodes, window_titles);
    validate_ordered_pane_arena(&panes)?;
    Ok(OrderedPanesFlattenResult {
        panes,
        #[cfg(test)]
        stats,
    })
}

struct OrderedPaneTreeDescriptorsNewtypeVisitor;

impl<'de> serde::de::Visitor<'de> for OrderedPaneTreeDescriptorsNewtypeVisitor {
    type Value = Vec<PaneArenaTree>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded ordered pane tree-descriptor collection")
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_bounded_vec::<D, _, MAX_ORDERED_TABS_PER_SNAPSHOT>(
            deserializer,
            "ordered pane tree descriptors",
        )
    }
}

struct OrderedPaneNodesNewtypeVisitor;

impl<'de> serde::de::Visitor<'de> for OrderedPaneNodesNewtypeVisitor {
    type Value = Vec<PaneArenaNode>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded ordered pane node arena")
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_bounded_vec::<D, _, MAX_ORDERED_PANE_NODES_PER_SNAPSHOT>(
            deserializer,
            "ordered pane arena nodes",
        )
    }
}

struct OrderedPaneWindowTitlesNewtypeVisitor;

impl<'de> serde::de::Visitor<'de> for OrderedPaneWindowTitlesNewtypeVisitor {
    type Value = Vec<PaneArenaWindowTitle>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded canonical ordered pane window-title collection")
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_bounded_vec::<D, _, MAX_ORDERED_WINDOWS_PER_SNAPSHOT>(
            deserializer,
            "ordered pane window titles",
        )
    }
}

fn deserialize_ordered_pane_tree_descriptors<'de, D>(
    deserializer: D,
) -> Result<Vec<PaneArenaTree>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_newtype_struct(
        bounded_varbincode::ORDERED_PANE_TREE_DESCRIPTORS_V1_NEWTYPE,
        OrderedPaneTreeDescriptorsNewtypeVisitor,
    )
}

fn deserialize_ordered_pane_nodes<'de, D>(deserializer: D) -> Result<Vec<PaneArenaNode>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_newtype_struct(
        bounded_varbincode::ORDERED_PANE_NODES_V1_NEWTYPE,
        OrderedPaneNodesNewtypeVisitor,
    )
}

fn deserialize_ordered_pane_window_titles<'de, D>(
    deserializer: D,
) -> Result<Vec<PaneArenaWindowTitle>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_newtype_struct(
        bounded_varbincode::ORDERED_PANE_WINDOW_TITLES_V1_NEWTYPE,
        OrderedPaneWindowTitlesNewtypeVisitor,
    )
}

#[derive(Deserialize, Serialize)]
struct OrderedPanesFlatWireOwned {
    #[serde(deserialize_with = "deserialize_ordered_pane_tree_descriptors")]
    trees: Vec<PaneArenaTree>,
    #[serde(deserialize_with = "deserialize_ordered_pane_nodes")]
    nodes: Vec<PaneArenaNode>,
    #[serde(deserialize_with = "deserialize_ordered_pane_window_titles")]
    window_titles: Vec<PaneArenaWindowTitle>,
}

/// Validate an owned PDU87 pane arena against every codec resource and
/// canonical-topology limit without decoding or rebuilding a recursive tree.
///
/// Direct application seams must call this before allocating work from arena
/// counts because [`PaneArena::from_unvalidated_parts`] is intentionally public
/// at the dependency-lower mux layer.
pub fn validate_ordered_pane_arena(panes: &PaneArena) -> Result<(), OrderedWindowProtocolError> {
    #[cfg(debug_assertions)]
    debug_record_ordered_pane_arena_validation_pass();

    let trees = panes.trees();
    let nodes = panes.nodes();
    let window_titles = panes.window_titles();
    if trees.len() > MAX_ORDERED_TABS_PER_SNAPSHOT {
        return Err(OrderedWindowProtocolError::TooManyPaneTrees {
            count: trees.len(),
            max: MAX_ORDERED_TABS_PER_SNAPSHOT,
        });
    }
    if nodes.len() > MAX_ORDERED_PANE_NODES_PER_SNAPSHOT {
        return Err(OrderedWindowProtocolError::TooManyPaneNodes {
            count: nodes.len(),
            max: MAX_ORDERED_PANE_NODES_PER_SNAPSHOT,
        });
    }
    if window_titles.len() > MAX_ORDERED_WINDOWS_PER_SNAPSHOT {
        return Err(OrderedWindowProtocolError::TooManyPaneWindowTitles {
            count: window_titles.len(),
            max: MAX_ORDERED_WINDOWS_PER_SNAPSHOT,
        });
    }
    if window_titles
        .windows(2)
        .any(|pair| pair[0].window_id >= pair[1].window_id)
    {
        return Err(OrderedWindowProtocolError::NonCanonicalPaneWindowTitleOrder);
    }
    for entry in window_titles {
        validate_remote_wire_id("window_id", entry.window_id)?;
    }

    let mut cursor = 0_usize;
    let mut total_leaves = 0_usize;
    let mut work = Vec::new();
    work.try_reserve_exact(MAX_ORDERED_PANE_TREE_DEPTH)
        .map_err(|_| OrderedWindowProtocolError::PaneArenaAllocation)?;

    for (tree_index, descriptor) in trees.iter().enumerate() {
        let node_count = usize::try_from(descriptor.node_count).map_err(|_| {
            OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                tree_index,
                detail: "node count does not fit usize",
            }
        })?;
        match (descriptor.root_index, node_count) {
            (None, 0) => {
                return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                    tree_index,
                    detail:
                        "ordered pane snapshots cannot recreate an empty tab without size authority",
                });
            }
            (None, _) => {
                return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                    tree_index,
                    detail: "non-empty range has no root index",
                });
            }
            (Some(_), 0) => {
                return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                    tree_index,
                    detail: "root index names an empty range",
                });
            }
            (Some(root), _) => {
                let root = usize::try_from(root).map_err(|_| {
                    OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                        tree_index,
                        detail: "root index does not fit usize",
                    }
                })?;
                if root != cursor {
                    return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                        tree_index,
                        detail: "root is not the next canonical arena index",
                    });
                }
                if node_count > MAX_ORDERED_PANE_NODES_PER_TREE {
                    return Err(OrderedWindowProtocolError::PaneTreeResourceLimit {
                        tree_index,
                        resource: "nodes",
                        count: node_count,
                        max: MAX_ORDERED_PANE_NODES_PER_TREE,
                    });
                }
                let end = cursor
                    .checked_add(node_count)
                    .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                if end > nodes.len() {
                    return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                        tree_index,
                        detail: "node range exceeds the admitted arena",
                    });
                }
                if matches!(nodes.get(root), Some(PaneArenaNode::Empty)) {
                    return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                        tree_index,
                        detail: "empty root must use the zero-node descriptor",
                    });
                }

                work.push((root, 1_usize));
                let mut expected = root;
                let mut tree_leaves = 0_usize;
                let mut active_leaves = 0_usize;
                let mut zoomed_leaves = 0_usize;
                while let Some((node_index, depth)) = work.pop() {
                    if depth > MAX_ORDERED_PANE_TREE_DEPTH {
                        return Err(OrderedWindowProtocolError::PaneTreeResourceLimit {
                            tree_index,
                            resource: "levels",
                            count: depth,
                            max: MAX_ORDERED_PANE_TREE_DEPTH,
                        });
                    }
                    if node_index != expected || node_index >= end {
                        return Err(OrderedWindowProtocolError::InvalidPaneArenaNode {
                            tree_index,
                            node_index,
                            detail: "child index violates contiguous preorder",
                        });
                    }
                    expected = expected
                        .checked_add(1)
                        .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                    match &nodes[node_index] {
                        PaneArenaNode::Empty => {}
                        PaneArenaNode::Leaf(entry) => {
                            tree_leaves = tree_leaves
                                .checked_add(1)
                                .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                            active_leaves = active_leaves
                                .checked_add(usize::from(entry.is_active_pane))
                                .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                            zoomed_leaves = zoomed_leaves
                                .checked_add(usize::from(entry.is_zoomed_pane))
                                .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                            total_leaves = total_leaves
                                .checked_add(1)
                                .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                            if tree_leaves > MAX_ORDERED_PANE_LEAVES_PER_TREE {
                                return Err(OrderedWindowProtocolError::PaneTreeResourceLimit {
                                    tree_index,
                                    resource: "leaves",
                                    count: tree_leaves,
                                    max: MAX_ORDERED_PANE_LEAVES_PER_TREE,
                                });
                            }
                            if total_leaves > MAX_ORDERED_PANE_LEAVES_PER_SNAPSHOT {
                                return Err(OrderedWindowProtocolError::TooManyPaneLeaves {
                                    count: total_leaves,
                                    max: MAX_ORDERED_PANE_LEAVES_PER_SNAPSHOT,
                                });
                            }
                        }
                        PaneArenaNode::Split { left, right, .. } => {
                            if depth == MAX_ORDERED_PANE_TREE_DEPTH {
                                return Err(OrderedWindowProtocolError::PaneTreeResourceLimit {
                                    tree_index,
                                    resource: "levels",
                                    count: depth.saturating_add(1),
                                    max: MAX_ORDERED_PANE_TREE_DEPTH,
                                });
                            }
                            let left = usize::try_from(*left).map_err(|_| {
                                OrderedWindowProtocolError::InvalidPaneArenaNode {
                                    tree_index,
                                    node_index,
                                    detail: "left child index does not fit usize",
                                }
                            })?;
                            let right = usize::try_from(*right).map_err(|_| {
                                OrderedWindowProtocolError::InvalidPaneArenaNode {
                                    tree_index,
                                    node_index,
                                    detail: "right child index does not fit usize",
                                }
                            })?;
                            if left != node_index.saturating_add(1) {
                                return Err(OrderedWindowProtocolError::InvalidPaneArenaNode {
                                    tree_index,
                                    node_index,
                                    detail: "left child is not the next preorder node",
                                });
                            }
                            if right <= left || right >= end {
                                return Err(OrderedWindowProtocolError::InvalidPaneArenaNode {
                                    tree_index,
                                    node_index,
                                    detail: "right child is outside the remaining tree range",
                                });
                            }
                            let child_depth = depth
                                .checked_add(1)
                                .ok_or(OrderedWindowProtocolError::CountOverflow)?;
                            work.push((right, child_depth));
                            work.push((left, child_depth));
                        }
                    }
                }
                if expected != end {
                    return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                        tree_index,
                        detail: "tree range contains unreachable or multiply referenced nodes",
                    });
                }
                if tree_leaves == 0 {
                    return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                        tree_index,
                        detail: "non-empty tree contains no pane leaves",
                    });
                }
                if active_leaves > 1 || zoomed_leaves > 1 {
                    return Err(OrderedWindowProtocolError::InvalidPaneTreeDescriptor {
                        tree_index,
                        detail: "pane tree has multiple active or zoomed leaves",
                    });
                }
                cursor = end;
            }
        }
    }
    if cursor != nodes.len() {
        return Err(OrderedWindowProtocolError::PaneArenaCardinalityMismatch {
            referenced: cursor,
            total: nodes.len(),
        });
    }
    Ok(())
}

fn deserialize_ordered_panes<'de, D>(deserializer: D) -> Result<PaneArena, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let wire = OrderedPanesFlatWireOwned::deserialize(deserializer)?;
    let OrderedPanesFlatWireOwned {
        trees,
        nodes,
        window_titles,
    } = wire;
    let panes = PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
    validate_ordered_pane_arena(&panes).map_err(serde::de::Error::custom)?;
    Ok(panes)
}

fn serialize_ordered_tab_ids<S>(values: &[RemoteTabId], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if values.len() > MAX_ORDERED_TABS_PER_WINDOW {
        return Err(serde::ser::Error::custom(format_args!(
            "ordered tab ids length {} exceeds maximum {}",
            values.len(),
            MAX_ORDERED_TABS_PER_WINDOW,
        )));
    }
    serializer.serialize_newtype_struct(bounded_varbincode::ORDERED_TAB_IDS_V1_NEWTYPE, values)
}

struct OrderedTabIdsNewtypeVisitor;

impl<'de> serde::de::Visitor<'de> for OrderedTabIdsNewtypeVisitor {
    type Value = Vec<RemoteTabId>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded ordered tab-id collection")
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_bounded_vec::<D, _, MAX_ORDERED_TABS_PER_WINDOW>(
            deserializer,
            "ordered tab ids",
        )
    }
}

fn deserialize_ordered_tab_ids<'de, D>(deserializer: D) -> Result<Vec<RemoteTabId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_newtype_struct(
        bounded_varbincode::ORDERED_TAB_IDS_V1_NEWTYPE,
        OrderedTabIdsNewtypeVisitor,
    )
}

fn serialize_ordered_windows<S>(
    values: &[OrderedWindowStateV1],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if values.len() > MAX_ORDERED_WINDOWS_PER_SNAPSHOT {
        return Err(serde::ser::Error::custom(format_args!(
            "ordered windows length {} exceeds maximum {}",
            values.len(),
            MAX_ORDERED_WINDOWS_PER_SNAPSHOT,
        )));
    }
    serializer.serialize_newtype_struct(bounded_varbincode::ORDERED_WINDOWS_V1_NEWTYPE, values)
}

struct OrderedWindowsNewtypeVisitor;

impl<'de> serde::de::Visitor<'de> for OrderedWindowsNewtypeVisitor {
    type Value = Vec<OrderedWindowStateV1>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded ordered-window collection")
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserialize_bounded_vec::<D, _, MAX_ORDERED_WINDOWS_PER_SNAPSHOT>(
            deserializer,
            "ordered windows",
        )
    }
}

fn deserialize_ordered_windows<'de, D>(
    deserializer: D,
) -> Result<Vec<OrderedWindowStateV1>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_newtype_struct(
        bounded_varbincode::ORDERED_WINDOWS_V1_NEWTYPE,
        OrderedWindowsNewtypeVisitor,
    )
}

struct OrderedWindowSectionWireBytes<'a>(&'a [u8]);

impl Serialize for OrderedWindowSectionWireBytes<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

struct OrderedWindowsWireRef<'a>(&'a [OrderedWindowStateV1]);

impl Serialize for OrderedWindowsWireRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serialize_ordered_windows(self.0, serializer)
    }
}

struct DirectOrderedWindowSectionWireRef<'a> {
    section_bytes: u64,
    windows: &'a [OrderedWindowStateV1],
}

impl Serialize for DirectOrderedWindowSectionWireRef<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeTuple as _;

        // varbincode tuples carry no tuple tag.  This therefore emits the
        // exact byte-slice length prefix followed by the canonical nested
        // section directly into the outer writer, byte-for-byte matching
        // `serialize_bytes` without first materializing a section Vec.
        let mut tuple = serializer.serialize_tuple(2)?;
        tuple.serialize_element(&self.section_bytes)?;
        tuple.serialize_element(&OrderedWindowsWireRef(self.windows))?;
        tuple.end()
    }
}

struct OrderedWindowSectionBytesVisitor;

impl<'de> serde::de::Visitor<'de> for OrderedWindowSectionBytesVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "at most {MAX_ORDERED_WINDOW_SECTION_BYTES} ordered-window section bytes"
        )
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAX_ORDERED_WINDOW_SECTION_BYTES {
            return Err(E::custom(format_args!(
                "ordered-window section length {} exceeds maximum {}",
                value.len(),
                MAX_ORDERED_WINDOW_SECTION_BYTES,
            )));
        }
        let mut owned = Vec::new();
        owned.try_reserve_exact(value.len()).map_err(E::custom)?;
        owned.extend_from_slice(value);
        Ok(owned)
    }

    fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_bytes(value)
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAX_ORDERED_WINDOW_SECTION_BYTES {
            return Err(E::custom(format_args!(
                "ordered-window section length {} exceeds maximum {}",
                value.len(),
                MAX_ORDERED_WINDOW_SECTION_BYTES,
            )));
        }
        Ok(value)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let hinted = sequence.size_hint().unwrap_or(0);
        if hinted > MAX_ORDERED_WINDOW_SECTION_BYTES {
            return Err(serde::de::Error::custom(format_args!(
                "ordered-window section length {hinted} exceeds maximum {}",
                MAX_ORDERED_WINDOW_SECTION_BYTES,
            )));
        }
        let mut value = Vec::new();
        value
            .try_reserve(hinted)
            .map_err(serde::de::Error::custom)?;
        while let Some(byte) = sequence.next_element()? {
            if value.len() == MAX_ORDERED_WINDOW_SECTION_BYTES {
                return Err(serde::de::Error::custom(format_args!(
                    "ordered-window section length exceeds maximum {}",
                    MAX_ORDERED_WINDOW_SECTION_BYTES,
                )));
            }
            value.push(byte);
        }
        Ok(value)
    }
}

struct OrderedWindowSectionNewtypeVisitor;

impl<'de> serde::de::Visitor<'de> for OrderedWindowSectionNewtypeVisitor {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded ordered-window section newtype")
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_byte_buf(OrderedWindowSectionBytesVisitor)
    }
}

fn serialize_ordered_window_section<S>(
    values: &[OrderedWindowStateV1],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if !ordered_windows_have_serialization_proof(values) {
        validate_ordered_windows_structure(values, false).map_err(serde::ser::Error::custom)?;
    }

    // PDU outbound planning uses a stack-only counting writer. During counting,
    // count the canonical nested value independently and charge its exact
    // byte-slice prefix plus contents to the outer scope; serializing unit
    // contributes no varbincode bytes. The prepared encoder then uses that
    // exact section length to write the canonical prefix and nested value
    // directly into its bounded outer buffer. Other legacy/general serde paths
    // retain the materialized byte-section representation below.
    if OUTBOUND_COUNTING_STATE.with(|state| state.get().is_some()) {
        let section_len = count_varbincode_value_raw(
            &OrderedWindowsWireRef(values),
            MAX_ORDERED_WINDOW_SECTION_BYTES,
        )
        .map_err(|failure| {
            serde::ser::Error::custom(format_args!(
                "counting ordered-window section failed: {failure:?}"
            ))
        })?;
        let section_len_u64 = u64::try_from(section_len).map_err(serde::ser::Error::custom)?;
        let encoded_section_bytes = encoded_length(section_len_u64)
            .checked_add(section_len)
            .ok_or_else(|| serde::ser::Error::custom("ordered-window section counting overflow"))?;
        record_outbound_counting_ordered_section(section_len, encoded_section_bytes)
            .map_err(serde::ser::Error::custom)?;
        return serializer.serialize_unit();
    }

    if let Some(mut direct) = OUTBOUND_DIRECT_ORDERED_SECTION.with(|state| state.get()) {
        if direct.consumed {
            return Err(serde::ser::Error::custom(
                "direct ordered-window section was consumed more than once",
            ));
        }
        direct.consumed = true;
        OUTBOUND_DIRECT_ORDERED_SECTION.with(|state| state.set(Some(direct)));
        let section_bytes =
            u64::try_from(direct.section_bytes).map_err(serde::ser::Error::custom)?;
        return serializer.serialize_newtype_struct(
            bounded_varbincode::ORDERED_WINDOW_SECTION_V1_NEWTYPE,
            &DirectOrderedWindowSectionWireRef {
                section_bytes,
                windows: values,
            },
        );
    }

    let mut section = BoundedSerializeBuffer::new(MAX_ORDERED_WINDOW_SECTION_BYTES);
    let serialize_result = {
        let mut section_serializer = varbincode::Serializer::new(&mut section);
        serialize_ordered_windows(values, &mut section_serializer)
    };
    if section.exceeded {
        return Err(serde::ser::Error::custom(format_args!(
            "ordered-window section length {} exceeds maximum {}",
            section.logical_len, MAX_ORDERED_WINDOW_SECTION_BYTES
        )));
    }
    serialize_result.map_err(serde::ser::Error::custom)?;
    serializer.serialize_newtype_struct(
        bounded_varbincode::ORDERED_WINDOW_SECTION_V1_NEWTYPE,
        &OrderedWindowSectionWireBytes(&section.bytes),
    )
}

fn deserialize_ordered_window_section<'de, D>(
    deserializer: D,
) -> Result<Vec<OrderedWindowStateV1>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    struct OrderedWindowsWire(
        #[serde(deserialize_with = "deserialize_ordered_windows")] Vec<OrderedWindowStateV1>,
    );

    let section = deserializer.deserialize_newtype_struct(
        bounded_varbincode::ORDERED_WINDOW_SECTION_V1_NEWTYPE,
        OrderedWindowSectionNewtypeVisitor,
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
    validate_ordered_windows_structure(&windows, false).map_err(serde::de::Error::custom)?;
    Ok(windows)
}

/// Closed schema version carried by exact render-delivery PDU IDs 91/92.
pub const EXACT_RENDER_DELIVERY_PROTOCOL_VERSION: u16 = 1;
/// Oldest negotiated codec dialect that may carry PDU IDs 91/92.
pub const EXACT_RENDER_DELIVERY_V1_MIN_CODEC_VERSION: usize = 52;

#[must_use]
pub const fn codec_version_supports_exact_render_delivery_v1(codec_version: usize) -> bool {
    codec_version >= EXACT_RENDER_DELIVERY_V1_MIN_CODEC_VERSION
}

/// Hard v1 resource ceilings. A receiver may advertise lower limits, but no
/// encoded or decoded v1 value may exceed these protocol maxima.
pub const MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES: usize = 4 * 1024 * 1024;
/// PDU 91 is a compact request/control schema and must not inherit the much
/// larger response allocation envelope. This conservative ceiling leaves
/// implementation headroom for the closed v1 body while bounding pre-body
/// work; schema growth still requires a new PDU identifier.
pub const MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES: usize = 64 * 1024;
/// Frozen zstd `compressBound` result for the request ceiling. For inputs below
/// 128 KiB zstd adds its small-input term in addition to `input + input / 256`.
pub const MAX_EXACT_RENDER_REQUEST_ZSTD_ENCODED_BYTES: usize =
    MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES
        + (MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES >> 8)
        + ((128 * 1024 - MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES) >> 11);
/// Largest single-frame zstd encoding emitted for a legal v1 payload under the
/// pinned zstd `compressBound` grammar. At 4 MiB the small-input term is zero,
/// leaving `input + input / 256`. Keeping this a frozen constant removes an FFI
/// call from every compressed exact-render header admission; a test pins it to
/// the linked zstd implementation.
pub const MAX_EXACT_RENDER_DELIVERY_ZSTD_ENCODED_BYTES: usize =
    MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES
        + (MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES >> 8);
/// Smallest reply envelope a receiver may advertise. Exact delivery must
/// always leave room for every zero-content terminal outcome, including a
/// typed `LimitsExceeded` response; permitting a one-byte envelope would make
/// the failure protocol itself impossible to encode.
pub const MIN_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES: u64 = 4 * 1024;
pub const MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES: usize = 2 * 1024 * 1024;
/// Per-row UTF-8 ceiling. This remains at or below the bounded varbincode
/// item ceiling; its zero-wire newtype marker selects this raw allocation cap
/// before the bounded decoder materializes the byte buffer.
pub const MAX_EXACT_RENDER_ROW_TEXT_BYTES: usize =
    bounded_varbincode::EXACT_RENDER_ROW_UTF8_V1_MAX_BYTES;
pub const MAX_EXACT_RENDER_DELIVERY_ROWS: usize = 16_384;
pub const MAX_EXACT_RENDER_DELIVERY_PATCHES: usize = 4_096;
const MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES_U64: u64 = 4 * 1024 * 1024;
const MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64: u64 = 2 * 1024 * 1024;
const MAX_EXACT_RENDER_DELIVERY_ROWS_U64: u64 = 16_384;
const MAX_EXACT_RENDER_DELIVERY_PATCHES_U64: u64 = 4_096;
pub const MAX_EXACT_RENDER_SNAPSHOT_TEXT_BYTES: u64 = 1024 * 1024 * 1024;
pub const MAX_EXACT_RENDER_SNAPSHOT_ROWS: u64 = 10_000_000;
pub const MAX_EXACT_RENDER_SNAPSHOT_CHUNKS: u64 = 4_096;
pub const MAX_EXACT_RENDER_BATCH_MEMBERS: u64 = 4_096;
pub const MAX_EXACT_RENDER_BATCH_DECOMPRESSED_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_EXACT_RENDER_BATCH_TEXT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_EXACT_RENDER_BATCH_ROWS: u64 = 262_144;
/// Per-coordinator retained immutable-snapshot ceilings. These are independent
/// of the much smaller bytes/rows physically present in a batch of manifest or
/// chunk responses: every distinct manifest reserves its complete backing
/// snapshot exactly once.
pub const MAX_EXACT_RENDER_BACKING_DISTINCT_SNAPSHOTS: u64 = MAX_EXACT_RENDER_BATCH_MEMBERS;
pub const MAX_EXACT_RENDER_BACKING_TEXT_BYTES: u64 = MAX_EXACT_RENDER_SNAPSHOT_TEXT_BYTES;
pub const MAX_EXACT_RENDER_BACKING_ROWS: u64 = MAX_EXACT_RENDER_SNAPSHOT_ROWS;
pub const MAX_EXACT_RENDER_BACKING_CHUNKS: u64 = MAX_EXACT_RENDER_SNAPSHOT_CHUNKS;
pub const MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES: usize =
    bounded_varbincode::EXACT_RENDER_METADATA_UTF8_V1_MAX_BYTES;
pub const MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES: usize =
    bounded_varbincode::EXACT_RENDER_METADATA_UTF8_V1_MAX_BYTES;
const MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64: u64 = 64 * 1024;
const MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES_U64: u64 = 64 * 1024;
pub const MAX_EXACT_RENDER_PROJECTION_VIEWPORT_CELLS: u64 = 4 * 1024 * 1024;

const EXACT_RENDER_DELTA_DIGEST_DOMAIN_V1: &[u8] = b"frankenterm.exact-render-delta.v1\0";
const EXACT_RENDER_SNAPSHOT_DIGEST_DOMAIN_V1: &[u8] = b"frankenterm.exact-render-snapshot.v1\0";
const EXACT_RENDER_SNAPSHOT_MANIFEST_DIGEST_DOMAIN_V1: &[u8] =
    b"frankenterm.exact-render-snapshot-manifest.v1\0";
const EXACT_RENDER_SNAPSHOT_CHUNK_DIGEST_DOMAIN_V1: &[u8] =
    b"frankenterm.exact-render-snapshot-chunk.v1\0";
/// Domain-separated canonical digest grammar for one complete exact-render
/// request body. This binds retry identity to intent without conflating the
/// independently useful request sequence with its contents.
pub const EXACT_RENDER_REQUEST_DIGEST_DOMAIN_V1: &[u8] = b"frankenterm.exact-render-request.v1\0";

macro_rules! exact_render_nonzero_wire_id {
    ($name:ident, $field:literal) => {
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        pub struct $name(u64);

        impl $name {
            pub fn try_new(value: u64) -> Result<Self, ExactRenderDeliveryProtocolError> {
                let identity = Self(value);
                identity.validate()?;
                Ok(identity)
            }

            #[must_use]
            pub const fn get(self) -> u64 {
                self.0
            }

            pub fn checked_next(self) -> Result<Self, ExactRenderDeliveryProtocolError> {
                self.validate()?;
                self.0
                    .checked_add(1)
                    .map(Self)
                    .ok_or(ExactRenderDeliveryProtocolError::AuthorityExhausted { field: $field })
            }

            fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
                if self.0 == 0 {
                    return Err(ExactRenderDeliveryProtocolError::ReservedNumericIdentity {
                        field: $field,
                        value: self.0,
                    });
                }
                Ok(())
            }
        }
    };
}

exact_render_nonzero_wire_id!(ExactRenderPaneGeneration, "pane_generation");
exact_render_nonzero_wire_id!(ExactRenderDeliveryGeneration, "delivery_generation");
exact_render_nonzero_wire_id!(ExactRenderDeliverySequence, "delivery_sequence");
exact_render_nonzero_wire_id!(ExactRenderRequestSequence, "request_sequence");

/// Architecture-independent exact-delivery pane identity. Pane zero is valid:
/// the mux allocator starts at zero, so absence must be represented by an
/// enclosing enum/option rather than a numeric sentinel.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ExactRenderPaneId(u64);

impl ExactRenderPaneId {
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn try_from_mux(pane_id: PaneId) -> Result<Self, ExactRenderDeliveryProtocolError> {
        let pane_id = u64::try_from(pane_id)
            .map_err(|_| ExactRenderDeliveryProtocolError::PaneIdOutOfRange)?;
        Ok(Self(pane_id))
    }

    pub fn try_into_mux(self) -> Result<PaneId, ExactRenderDeliveryProtocolError> {
        PaneId::try_from(self.0).map_err(|_| ExactRenderDeliveryProtocolError::PaneIdOutOfRange)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Explicit delivery baseline. Terminal mutation sequence is deliberately not
/// part of this identity and cannot establish continuity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ExactRenderDeliveryCursor {
    pub pane_generation: ExactRenderPaneGeneration,
    pub delivery_generation: ExactRenderDeliveryGeneration,
    pub sequence: ExactRenderDeliverySequence,
}

impl ExactRenderDeliveryCursor {
    pub fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.pane_generation.validate()?;
        self.delivery_generation.validate()?;
        self.sequence.validate()
    }

    pub fn checked_next(self) -> Result<Self, ExactRenderDeliveryProtocolError> {
        self.validate()?;
        Ok(Self {
            sequence: self.sequence.checked_next()?,
            ..self
        })
    }
}

/// Client state before applying the response to this request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ExactRenderAppliedBaseline {
    Uninitialized,
    Applied(ExactRenderDeliveryCursor),
}

impl ExactRenderAppliedBaseline {
    fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        if let Self::Applied(cursor) = self {
            cursor.validate()?;
        }
        Ok(())
    }
}

/// SHA-256 identity of one exact delivery unit or immutable snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ExactRenderDigest([u8; 32]);

impl ExactRenderDigest {
    pub const ZERO: Self = Self([0; 32]);

    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_bytes(self) -> [u8; 32] {
        self.0
    }

    fn validate(self, field: &'static str) -> Result<(), ExactRenderDeliveryProtocolError> {
        if self == Self::ZERO {
            return Err(ExactRenderDeliveryProtocolError::ReservedDigest { field });
        }
        Ok(())
    }
}

fn validate_exact_render_connection_identity(
    identity: RenderConnectionIdentity,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    if identity.stream_id.as_bytes() == [0; 16]
        || identity.session_incarnation.as_bytes() == [0; 16]
    {
        return Err(ExactRenderDeliveryProtocolError::ReservedConnectionIdentity);
    }
    Ok(())
}

/// Stable retry identity for one client request inside one connection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ExactRenderDeliveryRequestIdentity {
    pub connection_identity: RenderConnectionIdentity,
    pub pane_id: ExactRenderPaneId,
    pub request_sequence: ExactRenderRequestSequence,
}

impl ExactRenderDeliveryRequestIdentity {
    pub fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        validate_exact_render_connection_identity(self.connection_identity)?;
        self.pane_id.try_into_mux()?;
        self.request_sequence.validate()
    }
}

/// Exact result token retained until an application/persistence settlement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct ExactRenderDeliveryToken {
    pub connection_identity: RenderConnectionIdentity,
    pub pane_id: ExactRenderPaneId,
    pub resulting_baseline: ExactRenderDeliveryCursor,
    pub content_digest: ExactRenderDigest,
}

impl ExactRenderDeliveryToken {
    pub fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        validate_exact_render_connection_identity(self.connection_identity)?;
        self.pane_id.try_into_mux()?;
        self.resulting_baseline.validate()?;
        self.content_digest.validate("content_digest")
    }
}

/// Per-request receive limits. These values are authority, not hints: a server
/// must return `LimitsExceeded` instead of constructing a larger reply.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderReceiverCaps {
    pub max_decompressed_bytes: u64,
    pub max_text_bytes: u64,
    pub max_rows: u64,
    /// Complete retained snapshot text, including row bytes plus title and
    /// working-directory projection metadata.
    pub max_snapshot_text_bytes: u64,
    pub max_snapshot_rows: u64,
    pub max_snapshot_chunks: u64,
}

impl ExactRenderReceiverCaps {
    #[must_use]
    pub const fn protocol_maximum() -> Self {
        Self {
            max_decompressed_bytes: MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES_U64,
            max_text_bytes: MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64,
            max_rows: MAX_EXACT_RENDER_DELIVERY_ROWS_U64,
            max_snapshot_text_bytes: MAX_EXACT_RENDER_SNAPSHOT_TEXT_BYTES,
            max_snapshot_rows: MAX_EXACT_RENDER_SNAPSHOT_ROWS,
            max_snapshot_chunks: MAX_EXACT_RENDER_SNAPSHOT_CHUNKS,
        }
    }

    pub fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        validate_exact_render_cap_with_minimum(
            "max_decompressed_bytes",
            self.max_decompressed_bytes,
            MIN_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES,
            MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES_U64,
        )?;
        validate_exact_render_cap(
            "max_text_bytes",
            self.max_text_bytes,
            MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64,
        )?;
        validate_exact_render_cap(
            "max_rows",
            self.max_rows,
            MAX_EXACT_RENDER_DELIVERY_ROWS_U64,
        )?;
        validate_exact_render_cap(
            "max_snapshot_text_bytes",
            self.max_snapshot_text_bytes,
            MAX_EXACT_RENDER_SNAPSHOT_TEXT_BYTES,
        )?;
        validate_exact_render_cap(
            "max_snapshot_rows",
            self.max_snapshot_rows,
            MAX_EXACT_RENDER_SNAPSHOT_ROWS,
        )?;
        validate_exact_render_cap(
            "max_snapshot_chunks",
            self.max_snapshot_chunks,
            MAX_EXACT_RENDER_SNAPSHOT_CHUNKS,
        )?;
        if self.max_text_bytes > self.max_decompressed_bytes {
            return Err(ExactRenderDeliveryProtocolError::InconsistentReceiverCaps {
                smaller: "max_decompressed_bytes",
                smaller_value: self.max_decompressed_bytes,
                larger: "max_text_bytes",
                larger_value: self.max_text_bytes,
            });
        }
        if self.max_snapshot_chunks > self.max_snapshot_rows {
            return Err(ExactRenderDeliveryProtocolError::InconsistentReceiverCaps {
                smaller: "max_snapshot_rows",
                smaller_value: self.max_snapshot_rows,
                larger: "max_snapshot_chunks",
                larger_value: self.max_snapshot_chunks,
            });
        }
        Ok(())
    }
}

fn validate_exact_render_cap(
    resource: &'static str,
    requested: u64,
    protocol_maximum: u64,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    validate_exact_render_cap_with_minimum(resource, requested, 1, protocol_maximum)
}

fn validate_exact_render_cap_with_minimum(
    resource: &'static str,
    requested: u64,
    protocol_minimum: u64,
    protocol_maximum: u64,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    if requested < protocol_minimum || requested > protocol_maximum {
        return Err(ExactRenderDeliveryProtocolError::InvalidReceiverCap {
            resource,
            requested,
            protocol_minimum,
            protocol_maximum,
        });
    }
    Ok(())
}

/// UTF-8 wire bytes with a schema-specific preallocation ceiling. Encoding as
/// a bounded byte sequence keeps hostile length prefixes from reaching the
/// global varbincode String allocation cap before exact-delivery validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactRenderUtf8V1<const MAX: usize>(Vec<u8>);

impl<const MAX: usize> ExactRenderUtf8V1<MAX> {
    pub fn try_from_string(value: String) -> Result<Self, ExactRenderDeliveryProtocolError> {
        if value.len() > MAX {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "exact_render_utf8_bytes",
                requested: u64::try_from(value.len()).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX).unwrap_or(u64::MAX),
            });
        }
        Ok(Self(value.into_bytes()))
    }

    pub fn try_from_str(value: &str) -> Result<Self, ExactRenderDeliveryProtocolError> {
        if value.len() > MAX {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "exact_render_utf8_bytes",
                requested: u64::try_from(value.len()).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX).unwrap_or(u64::MAX),
            });
        }
        Ok(Self(value.as_bytes().to_vec()))
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn exact_render_utf8_wire_schema<const MAX: usize>() -> Option<(&'static str, &'static str)> {
    if MAX == MAX_EXACT_RENDER_ROW_TEXT_BYTES {
        Some((
            bounded_varbincode::EXACT_RENDER_ROW_UTF8_V1_NEWTYPE,
            "exact render row UTF-8 bytes",
        ))
    } else if MAX == MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES {
        Some((
            bounded_varbincode::EXACT_RENDER_METADATA_UTF8_V1_NEWTYPE,
            "exact render metadata UTF-8 bytes",
        ))
    } else {
        None
    }
}

struct ExactRenderUtf8WireBytes<'a>(&'a [u8]);

impl Serialize for ExactRenderUtf8WireBytes<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(self.0)
    }
}

struct ExactRenderUtf8BytesVisitor<const MAX: usize> {
    label: &'static str,
}

impl<'de, const MAX: usize> serde::de::Visitor<'de> for ExactRenderUtf8BytesVisitor<MAX> {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "at most {MAX} bytes of valid UTF-8")
    }

    fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAX {
            return Err(E::custom(format_args!(
                "{} length {} exceeds maximum {MAX}",
                self.label,
                value.len(),
            )));
        }
        std::str::from_utf8(value).map_err(E::custom)?;
        Ok(value.to_vec())
    }

    fn visit_borrowed_bytes<E>(self, value: &'de [u8]) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.visit_bytes(value)
    }

    fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        if value.len() > MAX {
            return Err(E::custom(format_args!(
                "{} length {} exceeds maximum {MAX}",
                self.label,
                value.len(),
            )));
        }
        std::str::from_utf8(&value).map_err(E::custom)?;
        Ok(value)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let hinted = sequence.size_hint().unwrap_or(0);
        if hinted > MAX {
            return Err(serde::de::Error::custom(format_args!(
                "{} length {hinted} exceeds maximum {MAX}",
                self.label,
            )));
        }
        let mut value = Vec::<u8>::new();
        value.try_reserve(hinted).map_err(|error| {
            serde::de::Error::custom(format_args!(
                "allocating {} length {hinted} failed: {error}",
                self.label,
            ))
        })?;
        while let Some(byte) = sequence.next_element()? {
            if value.len() == MAX {
                return Err(serde::de::Error::custom(format_args!(
                    "{} length exceeds maximum {MAX}",
                    self.label,
                )));
            }
            value.push(byte);
        }
        std::str::from_utf8(&value).map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

struct ExactRenderUtf8NewtypeVisitor<const MAX: usize> {
    label: &'static str,
}

impl<'de, const MAX: usize> serde::de::Visitor<'de> for ExactRenderUtf8NewtypeVisitor<MAX> {
    type Value = Vec<u8>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded exact-render UTF-8 newtype")
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_byte_buf(ExactRenderUtf8BytesVisitor::<MAX> { label: self.label })
    }
}

impl<const MAX: usize> Serialize for ExactRenderUtf8V1<MAX> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let Some((newtype_name, _)) = exact_render_utf8_wire_schema::<MAX>() else {
            return Err(serde::ser::Error::custom(format_args!(
                "unsupported exact render UTF-8 schema maximum {MAX}",
            )));
        };
        serializer
            .serialize_newtype_struct(newtype_name, &ExactRenderUtf8WireBytes(self.as_bytes()))
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for ExactRenderUtf8V1<MAX> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let Some((newtype_name, label)) = exact_render_utf8_wire_schema::<MAX>() else {
            return Err(serde::de::Error::custom(format_args!(
                "unsupported exact render UTF-8 schema maximum {MAX}",
            )));
        };
        deserializer
            .deserialize_newtype_struct(
                newtype_name,
                ExactRenderUtf8NewtypeVisitor::<MAX> { label },
            )
            .map(Self)
    }
}

pub type ExactRenderRowTextV1 = ExactRenderUtf8V1<MAX_EXACT_RENDER_ROW_TEXT_BYTES>;
pub type ExactRenderTitleV1 = ExactRenderUtf8V1<MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES>;
pub type ExactRenderWorkingDirectoryV1 =
    ExactRenderUtf8V1<MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES>;

/// Canonical physical-row projection. `wrapped=true` means the following row
/// is part of the same logical line; consumers must not synthesize a newline.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderRowV1 {
    pub stable_row: i64,
    pub text: ExactRenderRowTextV1,
    pub wrapped: bool,
}

/// Architecture-independent wire counterpart of [`StableCursorPosition`].
/// The mux-native type uses `usize`/`isize`; freezing those widths directly
/// into a new exact-delivery digest would make otherwise identical 32- and
/// 64-bit peers compute different authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExactRenderCursorShapeV1 {
    Default,
    BlinkingBlock,
    SteadyBlock,
    BlinkingUnderline,
    SteadyUnderline,
    BlinkingBar,
    SteadyBar,
}

impl ExactRenderCursorShapeV1 {
    const fn digest_tag(self) -> u8 {
        match self {
            Self::Default => 0,
            Self::BlinkingBlock => 1,
            Self::SteadyBlock => 2,
            Self::BlinkingUnderline => 3,
            Self::SteadyUnderline => 4,
            Self::BlinkingBar => 5,
            Self::SteadyBar => 6,
        }
    }
}

impl From<termwiz::surface::CursorShape> for ExactRenderCursorShapeV1 {
    fn from(shape: termwiz::surface::CursorShape) -> Self {
        match shape {
            termwiz::surface::CursorShape::Default => Self::Default,
            termwiz::surface::CursorShape::BlinkingBlock => Self::BlinkingBlock,
            termwiz::surface::CursorShape::SteadyBlock => Self::SteadyBlock,
            termwiz::surface::CursorShape::BlinkingUnderline => Self::BlinkingUnderline,
            termwiz::surface::CursorShape::SteadyUnderline => Self::SteadyUnderline,
            termwiz::surface::CursorShape::BlinkingBar => Self::BlinkingBar,
            termwiz::surface::CursorShape::SteadyBar => Self::SteadyBar,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExactRenderCursorVisibilityV1 {
    Hidden,
    Visible,
}

impl ExactRenderCursorVisibilityV1 {
    const fn digest_tag(self) -> u8 {
        match self {
            Self::Hidden => 0,
            Self::Visible => 1,
        }
    }
}

impl From<termwiz::surface::CursorVisibility> for ExactRenderCursorVisibilityV1 {
    fn from(visibility: termwiz::surface::CursorVisibility) -> Self {
        match visibility {
            termwiz::surface::CursorVisibility::Hidden => Self::Hidden,
            termwiz::surface::CursorVisibility::Visible => Self::Visible,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderCursorPositionV1 {
    pub x: u64,
    pub y: i64,
    pub shape: ExactRenderCursorShapeV1,
    pub visibility: ExactRenderCursorVisibilityV1,
}

impl ExactRenderCursorPositionV1 {
    pub fn try_from_stable(
        cursor: StableCursorPosition,
    ) -> Result<Self, ExactRenderDeliveryProtocolError> {
        Ok(Self {
            x: u64::try_from(cursor.x).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_cursor_x",
                }
            })?,
            y: i64::try_from(cursor.y).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_cursor_y",
                }
            })?,
            shape: cursor.shape.into(),
            visibility: cursor.visibility.into(),
        })
    }
}

/// Architecture-independent wire counterpart of [`RenderableDimensions`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderDimensionsV1 {
    pub cols: u64,
    pub viewport_rows: u64,
    pub scrollback_rows: u64,
    pub physical_top: i64,
    pub scrollback_top: i64,
    pub dpi: u32,
    pub pixel_width: u64,
    pub pixel_height: u64,
    pub reverse_video: bool,
}

impl ExactRenderDimensionsV1 {
    pub fn try_from_renderable(
        dimensions: RenderableDimensions,
    ) -> Result<Self, ExactRenderDeliveryProtocolError> {
        Ok(Self {
            cols: u64::try_from(dimensions.cols).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_cols",
                }
            })?,
            viewport_rows: u64::try_from(dimensions.viewport_rows).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_viewport_rows",
                }
            })?,
            scrollback_rows: u64::try_from(dimensions.scrollback_rows).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_scrollback_rows",
                }
            })?,
            physical_top: i64::try_from(dimensions.physical_top).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_physical_top",
                }
            })?,
            scrollback_top: i64::try_from(dimensions.scrollback_top).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_scrollback_top",
                }
            })?,
            dpi: dimensions.dpi,
            pixel_width: u64::try_from(dimensions.pixel_width).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_pixel_width",
                }
            })?,
            pixel_height: u64::try_from(dimensions.pixel_height).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_pixel_height",
                }
            })?,
            reverse_video: dimensions.reverse_video,
        })
    }
}

/// Complete persisted-text projection metadata bound to a delta or immutable
/// full snapshot. These fields are part of the content digest so resize,
/// reflow, scrollback, cursor, mouse-capture, title, working-directory, DPI and
/// reverse-video changes converge even when the row text itself is unchanged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderProjectionV1 {
    pub first_stable_row: i64,
    pub row_count: u64,
    pub alt_screen_active: bool,
    pub mouse_grabbed: bool,
    pub cursor_position: ExactRenderCursorPositionV1,
    pub dimensions: ExactRenderDimensionsV1,
    pub title: ExactRenderTitleV1,
    /// Canonical working-directory URL bytes. Keeping the wire value as UTF-8
    /// avoids architecture- or parser-version-dependent URL normalization.
    pub working_dir: Option<ExactRenderWorkingDirectoryV1>,
}

impl ExactRenderProjectionV1 {
    fn validate(&self) -> Result<(), ExactRenderDeliveryProtocolError> {
        if self.row_count > MAX_EXACT_RENDER_SNAPSHOT_ROWS {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "projection_rows",
                requested: self.row_count,
                limit: MAX_EXACT_RENDER_SNAPSHOT_ROWS,
            });
        }
        checked_stable_row_offset(self.first_stable_row, self.row_count, "projection_end")?;

        let dimensions = self.dimensions;
        let viewport_end = dimensions
            .physical_top
            .checked_add(i64::try_from(dimensions.viewport_rows).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_viewport_end",
                }
            })?)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "projection_viewport_end",
            })?;
        let history_rows = dimensions
            .physical_top
            .checked_sub(dimensions.scrollback_top)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "projection_history_rows",
            })?;
        let viewport_cells = dimensions
            .cols
            .checked_mul(dimensions.viewport_rows)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "projection_viewport_cells",
            })?;
        if dimensions.cols == 0
            || dimensions.viewport_rows == 0
            || viewport_cells > MAX_EXACT_RENDER_PROJECTION_VIEWPORT_CELLS
            || dimensions.scrollback_rows != self.row_count
            || dimensions.scrollback_top != self.first_stable_row
            || dimensions.scrollback_rows < dimensions.viewport_rows
            || history_rows < 0
            || u64::try_from(history_rows)
                .ok()
                .and_then(|history| history.checked_add(dimensions.viewport_rows))
                != Some(dimensions.scrollback_rows)
            || self.cursor_position.x > dimensions.cols
            || self.cursor_position.y < dimensions.physical_top
            || self.cursor_position.y >= viewport_end
        {
            return Err(ExactRenderDeliveryProtocolError::ProjectionMetadataInvalid);
        }

        let title_bytes = u64::try_from(self.title.len()).map_err(|_| {
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "projection_title_bytes",
            }
        })?;
        if title_bytes > MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64 {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "projection_title_bytes",
                requested: title_bytes,
                limit: MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64,
            });
        }
        if let Some(working_dir) = &self.working_dir {
            let working_dir_bytes = u64::try_from(working_dir.len()).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_working_dir_bytes",
                }
            })?;
            if working_dir.is_empty() {
                return Err(ExactRenderDeliveryProtocolError::ProjectionMetadataInvalid);
            }
            if working_dir_bytes > MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES_U64 {
                return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                    resource: "projection_working_dir_bytes",
                    requested: working_dir_bytes,
                    limit: MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES_U64,
                });
            }
        }
        Ok(())
    }

    fn text_bytes(&self) -> Result<u64, ExactRenderDeliveryProtocolError> {
        let title_bytes = u64::try_from(self.title.len()).map_err(|_| {
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "projection_title_bytes",
            }
        })?;
        let working_dir_bytes = match &self.working_dir {
            Some(working_dir) => u64::try_from(working_dir.len()).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "projection_working_dir_bytes",
                }
            })?,
            None => 0,
        };
        title_bytes.checked_add(working_dir_bytes).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "projection_text_bytes",
            },
        )
    }
}

fn checked_stable_row_offset(
    first: i64,
    offset: u64,
    field: &'static str,
) -> Result<i64, ExactRenderDeliveryProtocolError> {
    let offset = i64::try_from(offset)
        .map_err(|_| ExactRenderDeliveryProtocolError::ArithmeticOverflow { field })?;
    first
        .checked_add(offset)
        .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow { field })
}

/// Resources physically represented by one response on the wire. Full
/// immutable-snapshot retention is intentionally accounted by
/// [`ExactRenderBackingReservationUsage`] instead.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExactRenderDeliveryResourceUsage {
    pub decompressed_bytes: u64,
    pub text_bytes: u64,
    pub rows: u64,
}

fn exact_render_rows_usage(
    rows: &[ExactRenderRowV1],
) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
    let mut usage = ExactRenderDeliveryResourceUsage {
        rows: u64::try_from(rows.len()).map_err(|_| {
            ExactRenderDeliveryProtocolError::ArithmeticOverflow { field: "row_count" }
        })?,
        ..ExactRenderDeliveryResourceUsage::default()
    };
    for row in rows {
        accumulate_exact_render_row_usage(&mut usage, row)?;
    }
    Ok(usage)
}

fn accumulate_exact_render_row_usage(
    usage: &mut ExactRenderDeliveryResourceUsage,
    row: &ExactRenderRowV1,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    usage.text_bytes = usage
        .text_bytes
        .checked_add(u64::try_from(row.text.len()).map_err(|_| {
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "row_text_bytes",
            }
        })?)
        .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
            field: "row_text_bytes",
        })?;
    Ok(())
}

fn accumulate_and_hash_exact_render_row(
    usage: &mut ExactRenderDeliveryResourceUsage,
    hasher: &mut Sha256,
    row: &ExactRenderRowV1,
) -> Result<u64, ExactRenderDeliveryProtocolError> {
    let text_bytes = u64::try_from(row.text.len()).map_err(|_| {
        ExactRenderDeliveryProtocolError::ArithmeticOverflow {
            field: "row_text_bytes",
        }
    })?;
    usage.text_bytes = usage.text_bytes.checked_add(text_bytes).ok_or(
        ExactRenderDeliveryProtocolError::ArithmeticOverflow {
            field: "row_text_bytes",
        },
    )?;
    hasher.update(row.stable_row.to_be_bytes());
    hasher.update([u8::from(row.wrapped)]);
    hasher.update(text_bytes.to_be_bytes());
    hasher.update(row.text.as_bytes());
    Ok(text_bytes)
}

fn serialize_exact_render_rows<S>(
    values: &[ExactRenderRowV1],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serialize_bounded_vec::<S, _, MAX_EXACT_RENDER_DELIVERY_ROWS>(
        values,
        serializer,
        "exact render rows",
    )
}

fn deserialize_exact_render_rows<'de, D>(deserializer: D) -> Result<Vec<ExactRenderRowV1>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, _, MAX_EXACT_RENDER_DELIVERY_ROWS>(
        deserializer,
        "exact render rows",
    )
}

/// Requested delivery policy. Force-full is an explicit operation and cannot
/// be satisfied by a delta, even when the server retains a usable baseline.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExactRenderDeliveryMode {
    Incremental,
    ForceFull,
}

impl ExactRenderDeliveryMode {
    const fn digest_tag(self) -> u8 {
        match self {
            Self::Incremental => 0,
            Self::ForceFull => 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExactRenderDeliveryNackReason {
    BaseMismatch,
    GenerationMismatch,
    SnapshotCorrupt,
    BoundedResourceRejected,
    PersistenceFailure,
}

impl ExactRenderDeliveryNackReason {
    const fn digest_tag(self) -> u8 {
        match self {
            Self::BaseMismatch => 0,
            Self::GenerationMismatch => 1,
            Self::SnapshotCorrupt => 2,
            Self::BoundedResourceRejected => 3,
            Self::PersistenceFailure => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExactRenderDeliverySettlementOutcome {
    Applied,
    Nack {
        reason: ExactRenderDeliveryNackReason,
        observed_baseline: ExactRenderAppliedBaseline,
    },
}

/// Settlement of a prior exact result. An ACK is valid only after complete
/// application/persistence and advances to the token's resulting baseline.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderDeliverySettlement {
    pub delivery: ExactRenderDeliveryToken,
    pub outcome: ExactRenderDeliverySettlementOutcome,
}

impl ExactRenderDeliverySettlement {
    fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.delivery.validate()?;
        if let ExactRenderDeliverySettlementOutcome::Nack {
            observed_baseline, ..
        } = self.outcome
        {
            observed_baseline.validate()?;
        }
        Ok(())
    }
}

/// Cursor for the next immutable full-snapshot chunk.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderSnapshotContinuationV1 {
    pub snapshot: ExactRenderDeliveryToken,
    pub manifest_digest: ExactRenderDigest,
    pub source_version: u64,
    pub next_chunk_ordinal: u64,
    pub next_row_ordinal: u64,
    pub next_text_byte: u64,
}

impl ExactRenderSnapshotContinuationV1 {
    fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.snapshot.validate()?;
        self.manifest_digest.validate("snapshot_manifest_digest")?;
        if self.next_chunk_ordinal >= MAX_EXACT_RENDER_SNAPSHOT_CHUNKS {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "snapshot_chunk_ordinal",
                requested: self.next_chunk_ordinal,
                limit: MAX_EXACT_RENDER_SNAPSHOT_CHUNKS - 1,
            });
        }
        if self.next_row_ordinal > MAX_EXACT_RENDER_SNAPSHOT_ROWS {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "snapshot_row_ordinal",
                requested: self.next_row_ordinal,
                limit: MAX_EXACT_RENDER_SNAPSHOT_ROWS,
            });
        }
        if self.next_text_byte > MAX_EXACT_RENDER_SNAPSHOT_TEXT_BYTES {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "snapshot_text_offset",
                requested: self.next_text_byte,
                limit: MAX_EXACT_RENDER_SNAPSHOT_TEXT_BYTES,
            });
        }
        Ok(())
    }
}

/// Correlated client request for one exact delta or one force-full snapshot
/// manifest/chunk. The optional settlement always describes an earlier result.
///
/// A server retry ledger must index [`Self::identity`] and retain
/// [`Self::request_digest`] with that entry; the pair is the complete retry
/// authority. Before inserting a miss, the ledger must reject an already-seen
/// identity carrying a different digest as request equivocation. It is never an
/// alias or a second executable request, and neither body may receive the
/// other's cached outcome.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GetPaneRenderDeliveryV1 {
    pub protocol_version: u16,
    pub identity: ExactRenderDeliveryRequestIdentity,
    pub request_digest: ExactRenderDigest,
    pub applied_baseline: ExactRenderAppliedBaseline,
    pub settlement: Option<ExactRenderDeliverySettlement>,
    pub mode: ExactRenderDeliveryMode,
    pub receiver_caps: ExactRenderReceiverCaps,
    pub continuation: Option<ExactRenderSnapshotContinuationV1>,
}

impl GetPaneRenderDeliveryV1 {
    /// Architecture-independent SHA-256 over every request field except the
    /// digest slot itself. The preimage is the domain followed by declaration
    /// order using fixed-width big-endian integers, raw identity/digest bytes,
    /// and explicit one-byte option/enum tags.
    pub fn canonical_request_digest(
        &self,
    ) -> Result<ExactRenderDigest, ExactRenderDeliveryProtocolError> {
        let mut hasher = Sha256::new();
        hasher.update(EXACT_RENDER_REQUEST_DIGEST_DOMAIN_V1);
        hasher.update(self.protocol_version.to_be_bytes());
        hash_exact_render_request_identity(&mut hasher, self.identity);
        hash_exact_render_applied_baseline(&mut hasher, self.applied_baseline);
        match self.settlement {
            Some(settlement) => {
                hasher.update([1]);
                hash_exact_render_token(&mut hasher, settlement.delivery)?;
                match settlement.outcome {
                    ExactRenderDeliverySettlementOutcome::Applied => hasher.update([0]),
                    ExactRenderDeliverySettlementOutcome::Nack {
                        reason,
                        observed_baseline,
                    } => {
                        hasher.update([1, reason.digest_tag()]);
                        hash_exact_render_applied_baseline(&mut hasher, observed_baseline);
                    }
                }
            }
            None => hasher.update([0]),
        }
        hasher.update([self.mode.digest_tag()]);
        hasher.update(self.receiver_caps.max_decompressed_bytes.to_be_bytes());
        hasher.update(self.receiver_caps.max_text_bytes.to_be_bytes());
        hasher.update(self.receiver_caps.max_rows.to_be_bytes());
        hasher.update(self.receiver_caps.max_snapshot_text_bytes.to_be_bytes());
        hasher.update(self.receiver_caps.max_snapshot_rows.to_be_bytes());
        hasher.update(self.receiver_caps.max_snapshot_chunks.to_be_bytes());
        match self.continuation {
            Some(continuation) => {
                hasher.update([1]);
                hash_exact_render_token(&mut hasher, continuation.snapshot)?;
                hasher.update(continuation.manifest_digest.as_bytes());
                hasher.update(continuation.source_version.to_be_bytes());
                hasher.update(continuation.next_chunk_ordinal.to_be_bytes());
                hasher.update(continuation.next_row_ordinal.to_be_bytes());
                hasher.update(continuation.next_text_byte.to_be_bytes());
            }
            None => hasher.update([0]),
        }
        Ok(ExactRenderDigest::from_bytes(hasher.finalize().into()))
    }

    pub fn with_computed_request_digest(
        mut self,
    ) -> Result<Self, ExactRenderDeliveryProtocolError> {
        let digest = self.canonical_request_digest()?;
        digest.validate("request_digest")?;
        self.request_digest = digest;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), ExactRenderDeliveryProtocolError> {
        validate_exact_render_protocol_version(self.protocol_version)?;
        self.identity.validate()?;
        self.applied_baseline.validate()?;
        self.receiver_caps.validate()?;
        if self.mode == ExactRenderDeliveryMode::Incremental
            && self.applied_baseline == ExactRenderAppliedBaseline::Uninitialized
        {
            return Err(ExactRenderDeliveryProtocolError::IncrementalRequiresBaseline);
        }

        if let Some(settlement) = self.settlement {
            settlement.validate()?;
            if settlement.delivery.connection_identity != self.identity.connection_identity
                || settlement.delivery.pane_id != self.identity.pane_id
            {
                return Err(ExactRenderDeliveryProtocolError::SettlementIdentityMismatch);
            }
            match settlement.outcome {
                ExactRenderDeliverySettlementOutcome::Applied
                    if self.applied_baseline
                        != ExactRenderAppliedBaseline::Applied(
                            settlement.delivery.resulting_baseline,
                        ) =>
                {
                    return Err(ExactRenderDeliveryProtocolError::SettlementBaselineMismatch);
                }
                ExactRenderDeliverySettlementOutcome::Nack {
                    observed_baseline, ..
                } if observed_baseline != self.applied_baseline => {
                    return Err(ExactRenderDeliveryProtocolError::SettlementBaselineMismatch);
                }
                ExactRenderDeliverySettlementOutcome::Applied
                | ExactRenderDeliverySettlementOutcome::Nack { .. } => {}
            }
        }

        if let Some(continuation) = self.continuation {
            continuation.validate()?;
            if self.mode != ExactRenderDeliveryMode::ForceFull {
                return Err(ExactRenderDeliveryProtocolError::ContinuationRequiresForceFull);
            }
            if continuation.snapshot.connection_identity != self.identity.connection_identity
                || continuation.snapshot.pane_id != self.identity.pane_id
            {
                return Err(ExactRenderDeliveryProtocolError::ContinuationIdentityMismatch);
            }
            if self.settlement.is_some() {
                return Err(ExactRenderDeliveryProtocolError::SettlementWithContinuation);
            }
        }
        self.request_digest.validate("request_digest")?;
        let expected = self.canonical_request_digest()?;
        if self.request_digest != expected {
            return Err(ExactRenderDeliveryProtocolError::RequestDigestMismatch {
                expected,
                actual: self.request_digest,
            });
        }
        Ok(())
    }
}

/// One replacement operation over the prior projection. Replacement rows are
/// stored in the delta's single flat bounded row vector; the offsets below
/// prevent nested hostile length prefixes from multiplying allocations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderRowPatchV1 {
    pub start_stable_row: i64,
    pub removed_rows: u64,
    pub replacement_start: u64,
    pub replacement_count: u64,
}

fn serialize_exact_render_patches<S>(
    values: &[ExactRenderRowPatchV1],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serialize_bounded_vec::<S, _, MAX_EXACT_RENDER_DELIVERY_PATCHES>(
        values,
        serializer,
        "exact render patches",
    )
}

fn deserialize_exact_render_patches<'de, D>(
    deserializer: D,
) -> Result<Vec<ExactRenderRowPatchV1>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, _, MAX_EXACT_RENDER_DELIVERY_PATCHES>(
        deserializer,
        "exact render patches",
    )
}

/// Bounded exact change from one delivery baseline to another. `source_version`
/// records the sampled terminal mutation sequence for diagnosis only. It may
/// jump or repeat and never participates in delivery-continuity validation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderDeltaV1 {
    pub delivery: ExactRenderDeliveryToken,
    pub base: ExactRenderDeliveryCursor,
    pub source_version: u64,
    pub resulting_projection: ExactRenderProjectionV1,
    #[serde(
        serialize_with = "serialize_exact_render_patches",
        deserialize_with = "deserialize_exact_render_patches"
    )]
    pub patches: Vec<ExactRenderRowPatchV1>,
    #[serde(
        serialize_with = "serialize_exact_render_rows",
        deserialize_with = "deserialize_exact_render_rows"
    )]
    pub rows: Vec<ExactRenderRowV1>,
}

impl ExactRenderDeltaV1 {
    pub fn validate(&self) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.validated_resource_usage().map(|_| ())
    }

    fn validated_resource_usage(
        &self,
    ) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
        self.delivery.validate()?;
        self.base.validate()?;
        self.resulting_projection.validate()?;
        let result = self.delivery.resulting_baseline;
        if self.base.pane_generation != result.pane_generation
            || self.base.delivery_generation != result.delivery_generation
        {
            return Err(ExactRenderDeliveryProtocolError::DeliveryGenerationMismatch);
        }
        if self.base.sequence >= result.sequence {
            return Err(ExactRenderDeliveryProtocolError::NonAdvancingDelta);
        }
        if self.patches.len() > MAX_EXACT_RENDER_DELIVERY_PATCHES {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "reply_patches",
                requested: u64::try_from(self.patches.len()).unwrap_or(u64::MAX),
                limit: MAX_EXACT_RENDER_DELIVERY_PATCHES_U64,
            });
        }
        if self.rows.len() > MAX_EXACT_RENDER_DELIVERY_ROWS {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "reply_rows",
                requested: u64::try_from(self.rows.len()).unwrap_or(u64::MAX),
                limit: MAX_EXACT_RENDER_DELIVERY_ROWS_U64,
            });
        }

        let row_count = u64::try_from(self.rows.len()).map_err(|_| {
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "delta_row_count",
            }
        })?;
        let mut replacement_cursor = 0_u64;
        let mut prior_patch_start = None;
        let mut prior_removed_end = None;
        let mut prior_replacement_end = None;
        let projection_end = checked_stable_row_offset(
            self.resulting_projection.first_stable_row,
            self.resulting_projection.row_count,
            "resulting_projection_end",
        )?;
        let mut hasher = Sha256::new();
        hasher.update(EXACT_RENDER_DELTA_DIGEST_DOMAIN_V1);
        hash_exact_render_token_context(&mut hasher, self.delivery)?;
        hash_exact_render_cursor(&mut hasher, self.base);
        hasher.update(self.source_version.to_be_bytes());
        hash_exact_render_projection(&mut hasher, &self.resulting_projection)?;
        hasher.update(
            u64::try_from(self.patches.len())
                .map_err(|_| ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "patch_count",
                })?
                .to_be_bytes(),
        );
        for patch in &self.patches {
            hasher.update(patch.start_stable_row.to_be_bytes());
            hasher.update(patch.removed_rows.to_be_bytes());
            hasher.update(patch.replacement_start.to_be_bytes());
            hasher.update(patch.replacement_count.to_be_bytes());
            if patch.removed_rows == 0 && patch.replacement_count == 0 {
                return Err(ExactRenderDeliveryProtocolError::EmptyRowPatch);
            }
            if patch.replacement_start != replacement_cursor {
                return Err(ExactRenderDeliveryProtocolError::PatchRowsNotPartitioned {
                    expected_start: replacement_cursor,
                    actual_start: patch.replacement_start,
                });
            }
            let replacement_end = patch
                .replacement_start
                .checked_add(patch.replacement_count)
                .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "patch_replacement_end",
                })?;
            if replacement_end > row_count {
                return Err(
                    ExactRenderDeliveryProtocolError::PatchReplacementOutOfRange {
                        end: replacement_end,
                        row_count,
                    },
                );
            }
            if prior_patch_start.is_some_and(|prior| patch.start_stable_row <= prior) {
                return Err(ExactRenderDeliveryProtocolError::PatchOrderInvalid);
            }
            if prior_removed_end.is_some_and(|prior| patch.start_stable_row < prior) {
                return Err(ExactRenderDeliveryProtocolError::PatchOrderInvalid);
            }
            if prior_replacement_end.is_some_and(|prior| patch.start_stable_row < prior) {
                return Err(ExactRenderDeliveryProtocolError::PatchOrderInvalid);
            }
            let removed_end = checked_stable_row_offset(
                patch.start_stable_row,
                patch.removed_rows,
                "patch_removed_end",
            )?;
            let replacement_stable_end = checked_stable_row_offset(
                patch.start_stable_row,
                patch.replacement_count,
                "patch_replacement_stable_end",
            )?;
            if patch.replacement_count != 0
                && (patch.start_stable_row < self.resulting_projection.first_stable_row
                    || replacement_stable_end > projection_end)
            {
                return Err(ExactRenderDeliveryProtocolError::PatchProjectionOutOfRange);
            }
            replacement_cursor = replacement_end;
            prior_patch_start = Some(patch.start_stable_row);
            prior_removed_end = Some(removed_end);
            prior_replacement_end = Some(replacement_stable_end);
        }
        if replacement_cursor != row_count {
            return Err(ExactRenderDeliveryProtocolError::PatchRowsNotPartitioned {
                expected_start: row_count,
                actual_start: replacement_cursor,
            });
        }
        hasher.update(row_count.to_be_bytes());
        let mut usage = ExactRenderDeliveryResourceUsage {
            rows: row_count,
            ..ExactRenderDeliveryResourceUsage::default()
        };
        for patch in &self.patches {
            let replacement_start = usize::try_from(patch.replacement_start).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "patch_replacement_start",
                }
            })?;
            let replacement_end = patch
                .replacement_start
                .checked_add(patch.replacement_count)
                .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "patch_replacement_end",
                })?;
            let replacement_end = usize::try_from(replacement_end).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "patch_replacement_end",
                }
            })?;
            let mut expected = patch.start_stable_row;
            for row in &self.rows[replacement_start..replacement_end] {
                #[cfg(test)]
                record_test_exact_render_validation_row_visit();
                if row.stable_row != expected {
                    return Err(ExactRenderDeliveryProtocolError::UnexpectedStableRow {
                        expected,
                        actual: row.stable_row,
                    });
                }
                accumulate_and_hash_exact_render_row(&mut usage, &mut hasher, row)?;
                if usage.text_bytes > MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64 {
                    return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                        resource: "reply_text_bytes",
                        requested: usage.text_bytes,
                        limit: MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64,
                    });
                }
                expected = expected.checked_add(1).ok_or(
                    ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                        field: "stable_row",
                    },
                )?;
            }
        }
        usage.text_bytes = usage
            .text_bytes
            .checked_add(self.resulting_projection.text_bytes()?)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "reply_text_bytes",
            })?;
        if usage.text_bytes > MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64 {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "reply_text_bytes",
                requested: usage.text_bytes,
                limit: MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64,
            });
        }
        let expected_digest = ExactRenderDigest::from_bytes(hasher.finalize().into());
        if self.delivery.content_digest != expected_digest {
            return Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "delta_content_digest",
            });
        }
        Ok(usage)
    }

    pub fn canonical_digest(&self) -> Result<ExactRenderDigest, ExactRenderDeliveryProtocolError> {
        let mut hasher = Sha256::new();
        hasher.update(EXACT_RENDER_DELTA_DIGEST_DOMAIN_V1);
        hash_exact_render_token_context(&mut hasher, self.delivery)?;
        hash_exact_render_cursor(&mut hasher, self.base);
        hasher.update(self.source_version.to_be_bytes());
        hash_exact_render_projection(&mut hasher, &self.resulting_projection)?;
        hasher.update(
            u64::try_from(self.patches.len())
                .map_err(|_| ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "patch_count",
                })?
                .to_be_bytes(),
        );
        for patch in &self.patches {
            hasher.update(patch.start_stable_row.to_be_bytes());
            hasher.update(patch.removed_rows.to_be_bytes());
            hasher.update(patch.replacement_start.to_be_bytes());
            hasher.update(patch.replacement_count.to_be_bytes());
        }
        hash_exact_render_rows(&mut hasher, &self.rows)?;
        Ok(ExactRenderDigest::from_bytes(hasher.finalize().into()))
    }

    pub fn with_computed_digest(mut self) -> Result<Self, ExactRenderDeliveryProtocolError> {
        self.delivery.content_digest = self.canonical_digest()?;
        Ok(self)
    }
}

/// Immutable full-snapshot manifest. The content digest is independent of
/// chunk boundaries; changing chunk size cannot change snapshot identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderSnapshotManifestV1 {
    pub snapshot: ExactRenderDeliveryToken,
    pub source_version: u64,
    pub projection: ExactRenderProjectionV1,
    pub total_rows: u64,
    /// UTF-8 bytes in rows only. Retention and receiver-cap accounting add the
    /// projection title and working directory exactly once.
    pub total_text_bytes: u64,
    pub chunk_count: u64,
}

impl ExactRenderSnapshotManifestV1 {
    pub fn validate(&self) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.snapshot.validate()?;
        self.validated_retained_text_bytes().map(|_| ())
    }

    /// Validate totals and the aggregate lower bounds for a legal v1 chunk
    /// plan. The projection bytes are retained with the snapshot and repeated
    /// in every `FullChunk` response, so they consume both the snapshot and
    /// per-chunk text envelopes. Exact feasibility for the immutable,
    /// indivisible row sequence is checked by [`Self::computed_content_digest`]
    /// once those rows are available.
    fn validated_retained_text_bytes(&self) -> Result<u64, ExactRenderDeliveryProtocolError> {
        self.projection.validate()?;
        let projection_text_bytes = self.projection.text_bytes()?;
        let retained_text_bytes = self
            .total_text_bytes
            .checked_add(projection_text_bytes)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "snapshot_retained_text_bytes",
            })?;
        if self.total_rows != self.projection.row_count
            || self.total_rows > MAX_EXACT_RENDER_SNAPSHOT_ROWS
            || self.total_text_bytes > MAX_EXACT_RENDER_SNAPSHOT_TEXT_BYTES
            || retained_text_bytes > MAX_EXACT_RENDER_SNAPSHOT_TEXT_BYTES
        {
            return Err(ExactRenderDeliveryProtocolError::SnapshotTotalsInvalid);
        }
        if self.chunk_count > MAX_EXACT_RENDER_SNAPSHOT_CHUNKS
            || (self.total_rows == 0 && self.chunk_count != 0)
            || (self.total_rows != 0 && self.chunk_count == 0)
            || self.chunk_count > self.total_rows
            || (self.total_rows == 0 && self.total_text_bytes != 0)
        {
            return Err(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid);
        }

        let row_text_capacity = self
            .total_rows
            .checked_mul(u64::try_from(MAX_EXACT_RENDER_ROW_TEXT_BYTES).map_err(|_| {
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "snapshot_row_text_capacity",
                }
            })?)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "snapshot_row_text_capacity",
            })?;
        if self.total_text_bytes > row_text_capacity {
            return Err(ExactRenderDeliveryProtocolError::SnapshotTotalsInvalid);
        }

        let chunk_row_capacity = self
            .chunk_count
            .checked_mul(MAX_EXACT_RENDER_DELIVERY_ROWS_U64)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "snapshot_chunk_row_capacity",
            })?;
        let row_text_bytes_per_chunk = MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64
            .checked_sub(projection_text_bytes)
            .ok_or(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid)?;
        let chunk_text_capacity = self
            .chunk_count
            .checked_mul(row_text_bytes_per_chunk)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "snapshot_chunk_text_capacity",
            })?;
        if self.total_rows > chunk_row_capacity || self.total_text_bytes > chunk_text_capacity {
            return Err(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid);
        }
        Ok(retained_text_bytes)
    }

    pub fn canonical_manifest_digest(
        &self,
    ) -> Result<ExactRenderDigest, ExactRenderDeliveryProtocolError> {
        self.validate()?;
        let mut hasher = Sha256::new();
        hasher.update(EXACT_RENDER_SNAPSHOT_MANIFEST_DIGEST_DOMAIN_V1);
        hash_exact_render_token(&mut hasher, self.snapshot)?;
        hasher.update(self.source_version.to_be_bytes());
        hash_exact_render_projection(&mut hasher, &self.projection)?;
        hasher.update(self.total_rows.to_be_bytes());
        hasher.update(self.total_text_bytes.to_be_bytes());
        hasher.update(self.chunk_count.to_be_bytes());
        Ok(ExactRenderDigest::from_bytes(hasher.finalize().into()))
    }

    pub fn computed_content_digest(
        &self,
        rows: &[ExactRenderRowV1],
    ) -> Result<ExactRenderDigest, ExactRenderDeliveryProtocolError> {
        validate_exact_render_connection_identity(self.snapshot.connection_identity)?;
        self.snapshot.pane_id.try_into_mux()?;
        self.snapshot.resulting_baseline.validate()?;
        self.validated_retained_text_bytes()?;
        let row_count = u64::try_from(rows.len()).map_err(|_| {
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "snapshot_row_count",
            }
        })?;
        if row_count != self.total_rows {
            return Err(ExactRenderDeliveryProtocolError::SnapshotTotalsInvalid);
        }
        let mut hasher = Sha256::new();
        hasher.update(EXACT_RENDER_SNAPSHOT_DIGEST_DOMAIN_V1);
        hash_exact_render_token_context(&mut hasher, self.snapshot)?;
        hasher.update(self.source_version.to_be_bytes());
        hash_exact_render_projection(&mut hasher, &self.projection)?;
        hasher.update(self.total_rows.to_be_bytes());
        hasher.update(self.total_text_bytes.to_be_bytes());
        hasher.update(row_count.to_be_bytes());
        let mut usage = ExactRenderDeliveryResourceUsage {
            rows: row_count,
            ..ExactRenderDeliveryResourceUsage::default()
        };
        let projection_text_bytes = self.projection.text_bytes()?;
        let chunk_text_capacity = MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64
            .checked_sub(projection_text_bytes)
            .ok_or(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid)?;
        let mut minimum_chunk_count = 0_u64;
        let mut current_chunk_rows = 0_u64;
        let mut current_chunk_text_bytes = 0_u64;
        let mut expected = self.projection.first_stable_row;
        for row in rows {
            #[cfg(test)]
            record_test_exact_render_validation_row_visit();
            if row.stable_row != expected {
                return Err(ExactRenderDeliveryProtocolError::UnexpectedStableRow {
                    expected,
                    actual: row.stable_row,
                });
            }
            let row_text_bytes =
                accumulate_and_hash_exact_render_row(&mut usage, &mut hasher, row)?;
            if usage.text_bytes > self.total_text_bytes {
                return Err(ExactRenderDeliveryProtocolError::SnapshotTotalsInvalid);
            }

            if row_text_bytes > chunk_text_capacity {
                return Err(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid);
            }
            let combined_text_bytes = current_chunk_text_bytes.checked_add(row_text_bytes).ok_or(
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "snapshot_chunk_text_bytes",
                },
            )?;
            if current_chunk_rows != 0
                && (current_chunk_rows == MAX_EXACT_RENDER_DELIVERY_ROWS_U64
                    || combined_text_bytes > chunk_text_capacity)
            {
                minimum_chunk_count = minimum_chunk_count.checked_add(1).ok_or(
                    ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                        field: "snapshot_minimum_chunk_count",
                    },
                )?;
                current_chunk_rows = 0;
                current_chunk_text_bytes = 0;
            }
            current_chunk_rows = current_chunk_rows.checked_add(1).ok_or(
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "snapshot_chunk_rows",
                },
            )?;
            current_chunk_text_bytes = current_chunk_text_bytes.checked_add(row_text_bytes).ok_or(
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "snapshot_chunk_text_bytes",
                },
            )?;
            expected = expected.checked_add(1).ok_or(
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "snapshot_stable_row",
                },
            )?;
        }
        if usage.text_bytes != self.total_text_bytes {
            return Err(ExactRenderDeliveryProtocolError::SnapshotTotalsInvalid);
        }
        if current_chunk_rows != 0 {
            minimum_chunk_count = minimum_chunk_count.checked_add(1).ok_or(
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "snapshot_minimum_chunk_count",
                },
            )?;
        }
        // Greedily taking the longest legal contiguous prefix minimizes the
        // number of chunks for non-negative, indivisible row sizes. Any such
        // chunk can be split until every row is its own chunk, so the aggregate
        // `chunk_count <= total_rows` check plus this lower bound proves that
        // exactly the declared number of chunks is attainable.
        if minimum_chunk_count > self.chunk_count {
            return Err(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid);
        }
        Ok(ExactRenderDigest::from_bytes(hasher.finalize().into()))
    }

    pub fn validate_complete_rows(
        &self,
        rows: &[ExactRenderRowV1],
    ) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.snapshot
            .content_digest
            .validate("snapshot_content_digest")?;
        if self.computed_content_digest(rows)? != self.snapshot.content_digest {
            return Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "snapshot_content_digest",
            });
        }
        Ok(())
    }

    pub fn with_computed_content_digest(
        mut self,
        rows: &[ExactRenderRowV1],
    ) -> Result<Self, ExactRenderDeliveryProtocolError> {
        self.snapshot.content_digest = self.computed_content_digest(rows)?;
        Ok(self)
    }
}

/// One independently bounded chunk of an immutable snapshot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExactRenderSnapshotChunkV1 {
    pub source_version: u64,
    pub ordinal: u64,
    pub first_row_ordinal: u64,
    pub first_text_byte: u64,
    #[serde(
        serialize_with = "serialize_exact_render_rows",
        deserialize_with = "deserialize_exact_render_rows"
    )]
    pub rows: Vec<ExactRenderRowV1>,
    pub chunk_digest: ExactRenderDigest,
}

impl ExactRenderSnapshotChunkV1 {
    pub fn validate_for(
        &self,
        manifest: &ExactRenderSnapshotManifestV1,
    ) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.validated_rows_usage_for(manifest).map(|_| ())
    }

    fn validated_rows_usage_for(
        &self,
        manifest: &ExactRenderSnapshotManifestV1,
    ) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
        manifest.validate()?;
        if self.source_version != manifest.source_version {
            return Err(ExactRenderDeliveryProtocolError::SnapshotSourceVersionMismatch);
        }
        if self.ordinal >= manifest.chunk_count || self.rows.is_empty() {
            return Err(ExactRenderDeliveryProtocolError::SnapshotChunkRangeInvalid);
        }
        let expected_first_stable_row = checked_stable_row_offset(
            manifest.projection.first_stable_row,
            self.first_row_ordinal,
            "snapshot_chunk_first_stable_row",
        )?;
        if self.rows.len() > MAX_EXACT_RENDER_DELIVERY_ROWS {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "reply_rows",
                requested: u64::try_from(self.rows.len()).unwrap_or(u64::MAX),
                limit: MAX_EXACT_RENDER_DELIVERY_ROWS_U64,
            });
        }
        let row_count = u64::try_from(self.rows.len()).map_err(|_| {
            ExactRenderDeliveryProtocolError::ArithmeticOverflow { field: "row_count" }
        })?;
        let mut usage = ExactRenderDeliveryResourceUsage {
            rows: row_count,
            ..ExactRenderDeliveryResourceUsage::default()
        };
        let mut hasher = Sha256::new();
        hasher.update(EXACT_RENDER_SNAPSHOT_CHUNK_DIGEST_DOMAIN_V1);
        hash_exact_render_token(&mut hasher, manifest.snapshot)?;
        hasher.update(self.source_version.to_be_bytes());
        hasher.update(self.ordinal.to_be_bytes());
        hasher.update(self.first_row_ordinal.to_be_bytes());
        hasher.update(self.first_text_byte.to_be_bytes());
        hasher.update(row_count.to_be_bytes());
        let mut expected = expected_first_stable_row;
        for row in &self.rows {
            #[cfg(test)]
            record_test_exact_render_validation_row_visit();
            if row.stable_row != expected {
                return Err(ExactRenderDeliveryProtocolError::UnexpectedStableRow {
                    expected,
                    actual: row.stable_row,
                });
            }
            accumulate_and_hash_exact_render_row(&mut usage, &mut hasher, row)?;
            if usage.text_bytes > MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64 {
                return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                    resource: "reply_text_bytes",
                    requested: usage.text_bytes,
                    limit: MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64,
                });
            }
            expected = expected.checked_add(1).ok_or(
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "stable_row",
                },
            )?;
        }
        let row_end = self.first_row_ordinal.checked_add(usage.rows).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "snapshot_chunk_row_end",
            },
        )?;
        let text_end = self.first_text_byte.checked_add(usage.text_bytes).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "snapshot_chunk_text_end",
            },
        )?;
        let is_last = self.ordinal + 1 == manifest.chunk_count;
        if self.first_row_ordinal >= manifest.total_rows
            || row_end > manifest.total_rows
            || self.first_text_byte > manifest.total_text_bytes
            || text_end > manifest.total_text_bytes
            || (self.ordinal == 0 && (self.first_row_ordinal != 0 || self.first_text_byte != 0))
            || (is_last
                && (row_end != manifest.total_rows || text_end != manifest.total_text_bytes))
            || (!is_last && row_end >= manifest.total_rows)
        {
            return Err(ExactRenderDeliveryProtocolError::SnapshotChunkRangeInvalid);
        }
        self.chunk_digest.validate("snapshot_chunk_digest")?;
        if self.chunk_digest != ExactRenderDigest::from_bytes(hasher.finalize().into()) {
            return Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "snapshot_chunk_digest",
            });
        }
        Ok(usage)
    }

    pub fn canonical_digest(
        &self,
        manifest: &ExactRenderSnapshotManifestV1,
    ) -> Result<ExactRenderDigest, ExactRenderDeliveryProtocolError> {
        let mut hasher = Sha256::new();
        hasher.update(EXACT_RENDER_SNAPSHOT_CHUNK_DIGEST_DOMAIN_V1);
        hash_exact_render_token(&mut hasher, manifest.snapshot)?;
        hasher.update(self.source_version.to_be_bytes());
        hasher.update(self.ordinal.to_be_bytes());
        hasher.update(self.first_row_ordinal.to_be_bytes());
        hasher.update(self.first_text_byte.to_be_bytes());
        hash_exact_render_rows(&mut hasher, &self.rows)?;
        Ok(ExactRenderDigest::from_bytes(hasher.finalize().into()))
    }

    pub fn with_computed_digest(
        mut self,
        manifest: &ExactRenderSnapshotManifestV1,
    ) -> Result<Self, ExactRenderDeliveryProtocolError> {
        self.chunk_digest = self.canonical_digest(manifest)?;
        Ok(self)
    }

    pub fn next_continuation(
        &self,
        manifest: &ExactRenderSnapshotManifestV1,
    ) -> Result<Option<ExactRenderSnapshotContinuationV1>, ExactRenderDeliveryProtocolError> {
        let usage = self.validated_rows_usage_for(manifest)?;
        if self.ordinal + 1 == manifest.chunk_count {
            return Ok(None);
        }
        Ok(Some(ExactRenderSnapshotContinuationV1 {
            snapshot: manifest.snapshot,
            manifest_digest: manifest.canonical_manifest_digest()?,
            source_version: manifest.source_version,
            next_chunk_ordinal: self.ordinal + 1,
            next_row_ordinal: self.first_row_ordinal.checked_add(usage.rows).ok_or(
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "snapshot_next_row_ordinal",
                },
            )?,
            next_text_byte: self.first_text_byte.checked_add(usage.text_bytes).ok_or(
                ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                    field: "snapshot_next_text_byte",
                },
            )?,
        }))
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExactRenderAuthority {
    ConnectionIdentity,
    PaneGeneration,
    DeliveryGeneration,
    DeliverySequence,
    SnapshotIdentity,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExactRenderLimitResource {
    DecompressedBytes,
    TextBytes,
    Rows,
    SnapshotTextBytes,
    SnapshotRows,
    SnapshotChunks,
}

/// Closed correlated result. No string reason is delivery authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ExactRenderDeliveryOutcomeV1 {
    NoChange {
        current: ExactRenderDeliveryCursor,
        source_version: u64,
    },
    ExactDelta(ExactRenderDeltaV1),
    FullManifest(ExactRenderSnapshotManifestV1),
    FullChunk {
        manifest: ExactRenderSnapshotManifestV1,
        chunk: ExactRenderSnapshotChunkV1,
    },
    BaselineTooOld {
        requested: ExactRenderDeliveryCursor,
        oldest_available: ExactRenderDeliveryCursor,
        current: ExactRenderDeliveryCursor,
    },
    GenerationChanged {
        requested: ExactRenderDeliveryCursor,
        current_pane_generation: ExactRenderPaneGeneration,
        current_delivery_generation: ExactRenderDeliveryGeneration,
    },
    PaneRemoved {
        last_pane_generation: Option<ExactRenderPaneGeneration>,
    },
    AuthorityExhausted {
        authority: ExactRenderAuthority,
    },
    LimitsExceeded {
        resource: ExactRenderLimitResource,
        required: u64,
        limit: u64,
    },
}

impl ExactRenderDeliveryOutcomeV1 {
    fn validated_resource_usage(
        &self,
    ) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
        match self {
            Self::NoChange { current, .. } => {
                current.validate()?;
                Ok(ExactRenderDeliveryResourceUsage::default())
            }
            Self::ExactDelta(delta) => delta.validated_resource_usage(),
            Self::FullManifest(manifest) => {
                manifest.validate()?;
                Ok(ExactRenderDeliveryResourceUsage {
                    text_bytes: manifest.projection.text_bytes()?,
                    ..ExactRenderDeliveryResourceUsage::default()
                })
            }
            Self::FullChunk { manifest, chunk } => {
                let mut usage = chunk.validated_rows_usage_for(manifest)?;
                usage.text_bytes = usage
                    .text_bytes
                    .checked_add(manifest.projection.text_bytes()?)
                    .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                        field: "reply_text_bytes",
                    })?;
                Ok(usage)
            }
            Self::BaselineTooOld {
                requested,
                oldest_available,
                current,
            } => {
                requested.validate()?;
                oldest_available.validate()?;
                current.validate()?;
                if requested.pane_generation != oldest_available.pane_generation
                    || requested.delivery_generation != oldest_available.delivery_generation
                    || requested.pane_generation != current.pane_generation
                    || requested.delivery_generation != current.delivery_generation
                    || requested.sequence >= oldest_available.sequence
                    || oldest_available.sequence > current.sequence
                {
                    return Err(ExactRenderDeliveryProtocolError::BaselineTooOldInvalid);
                }
                Ok(ExactRenderDeliveryResourceUsage::default())
            }
            Self::GenerationChanged {
                requested,
                current_pane_generation,
                current_delivery_generation,
            } => {
                requested.validate()?;
                current_pane_generation.validate()?;
                current_delivery_generation.validate()?;
                if requested.pane_generation == *current_pane_generation
                    && requested.delivery_generation == *current_delivery_generation
                {
                    return Err(ExactRenderDeliveryProtocolError::GenerationChangeInvalid);
                }
                Ok(ExactRenderDeliveryResourceUsage::default())
            }
            Self::PaneRemoved {
                last_pane_generation,
            } => {
                if let Some(generation) = last_pane_generation {
                    generation.validate()?;
                }
                Ok(ExactRenderDeliveryResourceUsage::default())
            }
            Self::AuthorityExhausted { .. } => Ok(ExactRenderDeliveryResourceUsage::default()),
            Self::LimitsExceeded {
                required, limit, ..
            } => {
                if *limit == 0 || required <= limit {
                    return Err(ExactRenderDeliveryProtocolError::LimitsOutcomeInvalid);
                }
                Ok(ExactRenderDeliveryResourceUsage::default())
            }
        }
    }

    fn delivery_token(&self) -> Option<ExactRenderDeliveryToken> {
        match self {
            Self::ExactDelta(delta) => Some(delta.delivery),
            Self::FullManifest(manifest) | Self::FullChunk { manifest, .. } => {
                Some(manifest.snapshot)
            }
            Self::NoChange { .. }
            | Self::BaselineTooOld { .. }
            | Self::GenerationChanged { .. }
            | Self::PaneRemoved { .. }
            | Self::AuthorityExhausted { .. }
            | Self::LimitsExceeded { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GetPaneRenderDeliveryV1Response {
    pub protocol_version: u16,
    pub request_identity: ExactRenderDeliveryRequestIdentity,
    /// Echo of the validated request-body digest. The identity and digest are
    /// deliberately separate so retry-ledger equivocation remains observable.
    pub request_digest: ExactRenderDigest,
    pub outcome: ExactRenderDeliveryOutcomeV1,
}

impl GetPaneRenderDeliveryV1Response {
    /// Validate the closed response structure and its text/row resource caps.
    ///
    /// The codec's encode path checks the actual uncompressed byte count from
    /// its one canonical serialization before compression. Call
    /// [`Self::resource_usage`] when an independent caller explicitly needs
    /// measured decompressed-byte accounting.
    pub fn validate(&self) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.validated_structural_resource_usage().map(|_| ())
    }

    fn validated_resource_usage(
        &self,
    ) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
        let usage = self.validated_structural_resource_usage()?;
        let decompressed_bytes = exact_render_encoded_len(self)?;
        Self::with_validated_decompressed_bytes(usage, decompressed_bytes)
    }

    fn validate_with_decompressed_bytes(
        &self,
        decompressed_bytes: u64,
    ) -> Result<(), ExactRenderDeliveryProtocolError> {
        self.validated_resource_usage_with_decompressed_bytes(decompressed_bytes)
            .map(|_| ())
    }

    fn validated_resource_usage_with_decompressed_bytes(
        &self,
        decompressed_bytes: u64,
    ) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
        let usage = self.validated_structural_resource_usage()?;
        Self::with_validated_decompressed_bytes(usage, decompressed_bytes)
    }

    fn with_validated_decompressed_bytes(
        mut usage: ExactRenderDeliveryResourceUsage,
        decompressed_bytes: u64,
    ) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
        usage.decompressed_bytes = decompressed_bytes;
        if usage.decompressed_bytes > MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES_U64 {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "reply_decompressed_bytes",
                requested: usage.decompressed_bytes,
                limit: MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES_U64,
            });
        }
        Ok(usage)
    }

    fn validated_structural_resource_usage(
        &self,
    ) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
        validate_exact_render_protocol_version(self.protocol_version)?;
        self.request_identity.validate()?;
        self.request_digest.validate("request_digest")?;
        // Outcome validation returns its wire usage from the same traversal;
        // row-heavy deltas/chunks are not rescanned for accounting after their
        // continuity and digest checks.
        let usage = self.outcome.validated_resource_usage()?;
        if usage.text_bytes > MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64 {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "reply_text_bytes",
                requested: usage.text_bytes,
                limit: MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64,
            });
        }
        if usage.rows > MAX_EXACT_RENDER_DELIVERY_ROWS_U64 {
            return Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "reply_rows",
                requested: usage.rows,
                limit: MAX_EXACT_RENDER_DELIVERY_ROWS_U64,
            });
        }
        Ok(usage)
    }

    pub fn resource_usage(
        &self,
    ) -> Result<ExactRenderDeliveryResourceUsage, ExactRenderDeliveryProtocolError> {
        self.validated_resource_usage()
    }

    /// Validate this correlated reply against the exact request and its
    /// receiver-advertised limits.
    pub fn validate_for(
        &self,
        request: &GetPaneRenderDeliveryV1,
    ) -> Result<(), ExactRenderDeliveryProtocolError> {
        request.validate()?;
        validate_exact_render_protocol_version(self.protocol_version)?;
        self.request_identity.validate()?;
        self.request_digest.validate("request_digest")?;
        if self.request_identity != request.identity {
            return Err(ExactRenderDeliveryProtocolError::ReplyRequestMismatch);
        }
        if self.request_digest != request.request_digest {
            return Err(
                ExactRenderDeliveryProtocolError::ReplyRequestDigestMismatch {
                    expected: request.request_digest,
                    actual: self.request_digest,
                },
            );
        }
        if let Some(token) = self.outcome.delivery_token() {
            if token.connection_identity != request.identity.connection_identity
                || token.pane_id != request.identity.pane_id
            {
                return Err(ExactRenderDeliveryProtocolError::ReplyRequestMismatch);
            }
        }
        // Correlation failures are rejected before measuring or validating a
        // potentially multi-megabyte body. Successful replies then perform the
        // single fused structural row traversal plus encoded-size accounting.
        let usage = self.validated_resource_usage()?;

        match &self.outcome {
            ExactRenderDeliveryOutcomeV1::NoChange { current, .. } => {
                if request.mode != ExactRenderDeliveryMode::Incremental
                    || request.continuation.is_some()
                    || request.applied_baseline != ExactRenderAppliedBaseline::Applied(*current)
                {
                    return Err(ExactRenderDeliveryProtocolError::OutcomeModeMismatch);
                }
            }
            ExactRenderDeliveryOutcomeV1::ExactDelta(delta) => {
                if request.mode == ExactRenderDeliveryMode::ForceFull {
                    return Err(ExactRenderDeliveryProtocolError::ForceFullReturnedDelta);
                }
                if request.continuation.is_some()
                    || request.applied_baseline != ExactRenderAppliedBaseline::Applied(delta.base)
                {
                    return Err(ExactRenderDeliveryProtocolError::DeltaBaseMismatch);
                }
            }
            ExactRenderDeliveryOutcomeV1::FullManifest(manifest) => {
                if request.mode != ExactRenderDeliveryMode::ForceFull
                    || request.continuation.is_some()
                {
                    return Err(ExactRenderDeliveryProtocolError::OutcomeModeMismatch);
                }
                validate_snapshot_result_against_baseline(
                    manifest.snapshot.resulting_baseline,
                    request.applied_baseline,
                )?;
            }
            ExactRenderDeliveryOutcomeV1::FullChunk { manifest, chunk } => {
                let continuation = request
                    .continuation
                    .ok_or(ExactRenderDeliveryProtocolError::ChunkContinuationMismatch)?;
                let manifest_digest = manifest.canonical_manifest_digest()?;
                if request.mode != ExactRenderDeliveryMode::ForceFull
                    || continuation.snapshot != manifest.snapshot
                    || continuation.manifest_digest != manifest_digest
                    || continuation.source_version != manifest.source_version
                    || continuation.next_chunk_ordinal != chunk.ordinal
                    || continuation.next_row_ordinal != chunk.first_row_ordinal
                    || continuation.next_text_byte != chunk.first_text_byte
                {
                    return Err(ExactRenderDeliveryProtocolError::ChunkContinuationMismatch);
                }
                validate_snapshot_result_against_baseline(
                    manifest.snapshot.resulting_baseline,
                    request.applied_baseline,
                )?;
            }
            ExactRenderDeliveryOutcomeV1::BaselineTooOld { requested, .. } => {
                if request.mode != ExactRenderDeliveryMode::Incremental
                    || request.continuation.is_some()
                    || request.applied_baseline != ExactRenderAppliedBaseline::Applied(*requested)
                {
                    return Err(ExactRenderDeliveryProtocolError::OutcomeModeMismatch);
                }
            }
            ExactRenderDeliveryOutcomeV1::GenerationChanged { requested, .. } => {
                if request.applied_baseline != ExactRenderAppliedBaseline::Applied(*requested) {
                    return Err(ExactRenderDeliveryProtocolError::OutcomeModeMismatch);
                }
            }
            ExactRenderDeliveryOutcomeV1::PaneRemoved { .. }
            | ExactRenderDeliveryOutcomeV1::AuthorityExhausted { .. } => {}
            ExactRenderDeliveryOutcomeV1::LimitsExceeded {
                resource, limit, ..
            } => {
                let expected_limit = match resource {
                    ExactRenderLimitResource::DecompressedBytes => {
                        request.receiver_caps.max_decompressed_bytes
                    }
                    ExactRenderLimitResource::TextBytes => request.receiver_caps.max_text_bytes,
                    ExactRenderLimitResource::Rows => request.receiver_caps.max_rows,
                    ExactRenderLimitResource::SnapshotTextBytes => {
                        request.receiver_caps.max_snapshot_text_bytes
                    }
                    ExactRenderLimitResource::SnapshotRows => {
                        request.receiver_caps.max_snapshot_rows
                    }
                    ExactRenderLimitResource::SnapshotChunks => {
                        request.receiver_caps.max_snapshot_chunks
                    }
                };
                if *limit != expected_limit {
                    return Err(ExactRenderDeliveryProtocolError::LimitsOutcomeInvalid);
                }
            }
        }

        validate_response_cap(
            "decompressed_bytes",
            usage.decompressed_bytes,
            request.receiver_caps.max_decompressed_bytes,
        )?;
        validate_response_cap(
            "text_bytes",
            usage.text_bytes,
            request.receiver_caps.max_text_bytes,
        )?;
        validate_response_cap("rows", usage.rows, request.receiver_caps.max_rows)?;
        if let ExactRenderDeliveryOutcomeV1::FullManifest(manifest)
        | ExactRenderDeliveryOutcomeV1::FullChunk { manifest, .. } = &self.outcome
        {
            validate_response_cap(
                "snapshot_text_bytes",
                manifest.validated_retained_text_bytes()?,
                request.receiver_caps.max_snapshot_text_bytes,
            )?;
            validate_response_cap(
                "snapshot_rows",
                manifest.total_rows,
                request.receiver_caps.max_snapshot_rows,
            )?;
            validate_response_cap(
                "snapshot_chunks",
                manifest.chunk_count,
                request.receiver_caps.max_snapshot_chunks,
            )?;
        }
        Ok(())
    }
}

fn validate_snapshot_result_against_baseline(
    result: ExactRenderDeliveryCursor,
    applied: ExactRenderAppliedBaseline,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    let ExactRenderAppliedBaseline::Applied(applied) = applied else {
        return Ok(());
    };
    // Equal sequence is intentionally legal: ForceFull may repair corrupt or
    // lost local content without requiring a new terminal mutation. A lower
    // sequence within the same generation would regress acknowledged state.
    if result.pane_generation == applied.pane_generation
        && result.delivery_generation == applied.delivery_generation
        && result.sequence < applied.sequence
    {
        return Err(ExactRenderDeliveryProtocolError::SnapshotRegressesBaseline);
    }
    Ok(())
}

fn validate_response_cap(
    resource: &'static str,
    requested: u64,
    limit: u64,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    if requested > limit {
        return Err(
            ExactRenderDeliveryProtocolError::ResponseExceedsReceiverCap {
                resource,
                requested,
                limit,
            },
        );
    }
    Ok(())
}

/// Connection/coordinator aggregate wire-response admission caps. This is not
/// serialized and deliberately does not count the complete immutable backing
/// described by a small manifest; use
/// [`validate_exact_render_backing_reservations`] for that separate resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactRenderDeliveryAggregateCaps {
    pub max_members: u64,
    pub max_decompressed_bytes: u64,
    pub max_text_bytes: u64,
    pub max_rows: u64,
}

impl ExactRenderDeliveryAggregateCaps {
    #[must_use]
    pub const fn protocol_maximum() -> Self {
        Self {
            max_members: MAX_EXACT_RENDER_BATCH_MEMBERS,
            max_decompressed_bytes: MAX_EXACT_RENDER_BATCH_DECOMPRESSED_BYTES,
            max_text_bytes: MAX_EXACT_RENDER_BATCH_TEXT_BYTES,
            max_rows: MAX_EXACT_RENDER_BATCH_ROWS,
        }
    }

    fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        validate_exact_render_cap(
            "aggregate_members",
            self.max_members,
            MAX_EXACT_RENDER_BATCH_MEMBERS,
        )?;
        validate_exact_render_cap(
            "aggregate_decompressed_bytes",
            self.max_decompressed_bytes,
            MAX_EXACT_RENDER_BATCH_DECOMPRESSED_BYTES,
        )?;
        validate_exact_render_cap(
            "aggregate_text_bytes",
            self.max_text_bytes,
            MAX_EXACT_RENDER_BATCH_TEXT_BYTES,
        )?;
        validate_exact_render_cap("aggregate_rows", self.max_rows, MAX_EXACT_RENDER_BATCH_ROWS)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExactRenderDeliveryAggregateUsage {
    pub members: u64,
    pub decompressed_bytes: u64,
    pub text_bytes: u64,
    pub rows: u64,
}

pub fn validate_exact_render_delivery_aggregate(
    responses: &[GetPaneRenderDeliveryV1Response],
    caps: ExactRenderDeliveryAggregateCaps,
) -> Result<ExactRenderDeliveryAggregateUsage, ExactRenderDeliveryProtocolError> {
    caps.validate()?;
    let mut aggregate = ExactRenderDeliveryAggregateUsage::default();
    for response in responses {
        let usage = response.validated_resource_usage()?;
        aggregate.members = aggregate.members.checked_add(1).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "aggregate_members",
            },
        )?;
        aggregate.decompressed_bytes = aggregate
            .decompressed_bytes
            .checked_add(usage.decompressed_bytes)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "aggregate_decompressed_bytes",
            })?;
        aggregate.text_bytes = aggregate.text_bytes.checked_add(usage.text_bytes).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "aggregate_text_bytes",
            },
        )?;
        aggregate.rows = aggregate.rows.checked_add(usage.rows).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "aggregate_rows",
            },
        )?;
        validate_aggregate_cap("members", aggregate.members, caps.max_members)?;
        validate_aggregate_cap(
            "decompressed_bytes",
            aggregate.decompressed_bytes,
            caps.max_decompressed_bytes,
        )?;
        validate_aggregate_cap("text_bytes", aggregate.text_bytes, caps.max_text_bytes)?;
        validate_aggregate_cap("rows", aggregate.rows, caps.max_rows)?;
    }
    Ok(aggregate)
}

/// Aggregate admission caps for immutable snapshot backing retained on behalf
/// of manifest/chunk responses. Zero is a valid local policy (retain none), but
/// a caller cannot raise any ceiling above the v1 protocol maximum.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExactRenderBackingReservationCaps {
    pub max_distinct_snapshots: u64,
    /// Row text plus retained projection title/working-directory bytes.
    pub max_total_text_bytes: u64,
    pub max_total_rows: u64,
    pub max_total_chunks: u64,
}

impl ExactRenderBackingReservationCaps {
    #[must_use]
    pub const fn protocol_maximum() -> Self {
        Self {
            max_distinct_snapshots: MAX_EXACT_RENDER_BACKING_DISTINCT_SNAPSHOTS,
            max_total_text_bytes: MAX_EXACT_RENDER_BACKING_TEXT_BYTES,
            max_total_rows: MAX_EXACT_RENDER_BACKING_ROWS,
            max_total_chunks: MAX_EXACT_RENDER_BACKING_CHUNKS,
        }
    }

    fn validate(self) -> Result<(), ExactRenderDeliveryProtocolError> {
        for (resource, requested, maximum) in [
            (
                "backing_distinct_snapshots",
                self.max_distinct_snapshots,
                MAX_EXACT_RENDER_BACKING_DISTINCT_SNAPSHOTS,
            ),
            (
                "backing_text_bytes",
                self.max_total_text_bytes,
                MAX_EXACT_RENDER_BACKING_TEXT_BYTES,
            ),
            (
                "backing_rows",
                self.max_total_rows,
                MAX_EXACT_RENDER_BACKING_ROWS,
            ),
            (
                "backing_chunks",
                self.max_total_chunks,
                MAX_EXACT_RENDER_BACKING_CHUNKS,
            ),
        ] {
            if requested > maximum {
                return Err(
                    ExactRenderDeliveryProtocolError::InvalidBackingReservationCap {
                        resource,
                        requested,
                        maximum,
                    },
                );
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ExactRenderBackingReservationUsage {
    pub distinct_snapshots: u64,
    pub total_text_bytes: u64,
    pub total_rows: u64,
    pub total_chunks: u64,
}

/// Logical immutable-snapshot identity for deduplication and equivocation.
/// The claimed content digest is deliberately excluded: changing that digest
/// for the same connection/pane/baseline is the equivocation this key must
/// expose, not a second reservable snapshot.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ExactRenderSnapshotBackingIdentity {
    connection_identity: RenderConnectionIdentity,
    pane_id: ExactRenderPaneId,
    resulting_baseline: ExactRenderDeliveryCursor,
}

impl From<ExactRenderDeliveryToken> for ExactRenderSnapshotBackingIdentity {
    fn from(token: ExactRenderDeliveryToken) -> Self {
        Self {
            connection_identity: token.connection_identity,
            pane_id: token.pane_id,
            resulting_baseline: token.resulting_baseline,
        }
    }
}

/// Validate and reserve complete immutable backing once per logical snapshot
/// identity. Repeated chunks with the same manifest are deduplicated; changing
/// any manifest claim, including the content digest, for the same
/// connection/pane/baseline is equivocation and fails closed.
pub fn validate_exact_render_backing_reservations(
    responses: &[GetPaneRenderDeliveryV1Response],
    caps: ExactRenderBackingReservationCaps,
) -> Result<ExactRenderBackingReservationUsage, ExactRenderDeliveryProtocolError> {
    caps.validate()?;
    let member_count = u64::try_from(responses.len()).map_err(|_| {
        ExactRenderDeliveryProtocolError::ArithmeticOverflow {
            field: "backing_members",
        }
    })?;
    if member_count > MAX_EXACT_RENDER_BATCH_MEMBERS {
        return Err(
            ExactRenderDeliveryProtocolError::BackingReservationLimitExceeded {
                resource: "members",
                requested: member_count,
                limit: MAX_EXACT_RENDER_BATCH_MEMBERS,
            },
        );
    }
    let mut manifest_by_snapshot = HashMap::new();
    let mut usage = ExactRenderBackingReservationUsage::default();
    for response in responses {
        response.validate()?;
        let manifest = match &response.outcome {
            ExactRenderDeliveryOutcomeV1::FullManifest(manifest)
            | ExactRenderDeliveryOutcomeV1::FullChunk { manifest, .. } => manifest,
            ExactRenderDeliveryOutcomeV1::NoChange { .. }
            | ExactRenderDeliveryOutcomeV1::ExactDelta(_)
            | ExactRenderDeliveryOutcomeV1::BaselineTooOld { .. }
            | ExactRenderDeliveryOutcomeV1::GenerationChanged { .. }
            | ExactRenderDeliveryOutcomeV1::PaneRemoved { .. }
            | ExactRenderDeliveryOutcomeV1::AuthorityExhausted { .. }
            | ExactRenderDeliveryOutcomeV1::LimitsExceeded { .. } => continue,
        };
        let retained_text_bytes = manifest.validated_retained_text_bytes()?;
        let manifest_digest = manifest.canonical_manifest_digest()?;
        let snapshot_identity = ExactRenderSnapshotBackingIdentity::from(manifest.snapshot);
        if let Some(retained_digest) = manifest_by_snapshot.get(&snapshot_identity) {
            if *retained_digest != manifest_digest {
                return Err(ExactRenderDeliveryProtocolError::SnapshotManifestEquivocation);
            }
            continue;
        }
        manifest_by_snapshot.insert(snapshot_identity, manifest_digest);
        usage.distinct_snapshots = usage.distinct_snapshots.checked_add(1).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "backing_distinct_snapshots",
            },
        )?;
        usage.total_text_bytes = usage
            .total_text_bytes
            .checked_add(retained_text_bytes)
            .ok_or(ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "backing_text_bytes",
            })?;
        usage.total_rows = usage.total_rows.checked_add(manifest.total_rows).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "backing_rows",
            },
        )?;
        usage.total_chunks = usage.total_chunks.checked_add(manifest.chunk_count).ok_or(
            ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "backing_chunks",
            },
        )?;
        validate_backing_reservation_cap(
            "distinct_snapshots",
            usage.distinct_snapshots,
            caps.max_distinct_snapshots,
        )?;
        validate_backing_reservation_cap(
            "text_bytes",
            usage.total_text_bytes,
            caps.max_total_text_bytes,
        )?;
        validate_backing_reservation_cap("rows", usage.total_rows, caps.max_total_rows)?;
        validate_backing_reservation_cap("chunks", usage.total_chunks, caps.max_total_chunks)?;
    }
    Ok(usage)
}

fn validate_backing_reservation_cap(
    resource: &'static str,
    requested: u64,
    limit: u64,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    if requested > limit {
        return Err(
            ExactRenderDeliveryProtocolError::BackingReservationLimitExceeded {
                resource,
                requested,
                limit,
            },
        );
    }
    Ok(())
}

fn validate_aggregate_cap(
    resource: &'static str,
    requested: u64,
    limit: u64,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    if requested > limit {
        return Err(ExactRenderDeliveryProtocolError::AggregateLimitExceeded {
            resource,
            requested,
            limit,
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum ExactRenderDeliveryProtocolError {
    #[error("exact render-delivery protocol version {actual} is unsupported; expected {expected}")]
    UnsupportedProtocolVersion { actual: u16, expected: u16 },
    #[error("exact render connection identity uses a reserved zero value")]
    ReservedConnectionIdentity,
    #[error("exact render numeric identity {field} uses reserved value {value}")]
    ReservedNumericIdentity { field: &'static str, value: u64 },
    #[error("exact render digest {field} uses the reserved all-zero value")]
    ReservedDigest { field: &'static str },
    #[error("exact render authority {field} exhausted without a reusable successor")]
    AuthorityExhausted { field: &'static str },
    #[error(
        "exact render receiver cap {resource}={requested} is outside {protocol_minimum}..={protocol_maximum}"
    )]
    InvalidReceiverCap {
        resource: &'static str,
        requested: u64,
        protocol_minimum: u64,
        protocol_maximum: u64,
    },
    #[error("exact render receiver cap {larger}={larger_value} exceeds {smaller}={smaller_value}")]
    InconsistentReceiverCaps {
        smaller: &'static str,
        smaller_value: u64,
        larger: &'static str,
        larger_value: u64,
    },
    #[error("exact render resource {resource} requested {requested}; protocol limit is {limit}")]
    ResourceLimitExceeded {
        resource: &'static str,
        requested: u64,
        limit: u64,
    },
    #[error("exact render arithmetic overflow while computing {field}")]
    ArithmeticOverflow { field: &'static str },
    #[error("exact render row order expected stable row {expected}, received {actual}")]
    UnexpectedStableRow { expected: i64, actual: i64 },
    #[error("exact render persisted-text projection metadata is internally inconsistent")]
    ProjectionMetadataInvalid,
    #[error("exact render settlement targets a different connection or pane")]
    SettlementIdentityMismatch,
    #[error("exact render settlement does not match the request's applied baseline")]
    SettlementBaselineMismatch,
    #[error("exact render snapshot continuation requires ForceFull mode")]
    ContinuationRequiresForceFull,
    #[error("exact render incremental request requires an applied delivery baseline")]
    IncrementalRequiresBaseline,
    #[error("exact render snapshot continuation targets a different connection or pane")]
    ContinuationIdentityMismatch,
    #[error("exact render settlement and snapshot continuation cannot share one request")]
    SettlementWithContinuation,
    #[error("exact render delta crosses a pane or delivery generation")]
    DeliveryGenerationMismatch,
    #[error("exact render delta does not advance its explicit delivery sequence")]
    NonAdvancingDelta,
    #[error("exact render row patch removes and inserts zero rows")]
    EmptyRowPatch,
    #[error("exact render row patches are not strictly ordered and disjoint")]
    PatchOrderInvalid,
    #[error(
        "exact render patch replacement rows expected offset {expected_start}, received {actual_start}"
    )]
    PatchRowsNotPartitioned {
        expected_start: u64,
        actual_start: u64,
    },
    #[error("exact render patch replacement end {end} exceeds row count {row_count}")]
    PatchReplacementOutOfRange { end: u64, row_count: u64 },
    #[error("exact render replacement rows fall outside the resulting projection")]
    PatchProjectionOutOfRange,
    #[error("exact render digest mismatch for {field}")]
    DigestMismatch { field: &'static str },
    #[error("exact render request digest mismatch: expected {expected:?}, received {actual:?}")]
    RequestDigestMismatch {
        expected: ExactRenderDigest,
        actual: ExactRenderDigest,
    },
    #[error("exact render snapshot totals are inconsistent or exceed protocol maxima")]
    SnapshotTotalsInvalid,
    #[error("exact render snapshot chunk plan cannot carry its declared totals")]
    SnapshotChunkCountInvalid,
    #[error("exact render snapshot chunk source version differs from its immutable manifest")]
    SnapshotSourceVersionMismatch,
    #[error("exact render snapshot chunk ordinal or row/text range is invalid")]
    SnapshotChunkRangeInvalid,
    #[error("exact render LimitsExceeded outcome must report required > nonzero limit")]
    LimitsOutcomeInvalid,
    #[error("exact render reply does not echo the exact request identity")]
    ReplyRequestMismatch,
    #[error(
        "exact render reply request digest mismatch: expected {expected:?}, received {actual:?}"
    )]
    ReplyRequestDigestMismatch {
        expected: ExactRenderDigest,
        actual: ExactRenderDigest,
    },
    #[error("exact render outcome is incompatible with the request mode or baseline")]
    OutcomeModeMismatch,
    #[error("ForceFull exact render request returned a delta")]
    ForceFullReturnedDelta,
    #[error("exact render delta does not advance the request's applied baseline")]
    DeltaBaseMismatch,
    #[error("exact render full chunk does not match the requested continuation")]
    ChunkContinuationMismatch,
    #[error("exact render full snapshot regresses the applied baseline within one generation")]
    SnapshotRegressesBaseline,
    #[error("exact render baseline-too-old range is inconsistent")]
    BaselineTooOldInvalid,
    #[error("exact render generation-change result did not change a generation")]
    GenerationChangeInvalid,
    #[error("exact render response {resource}={requested} exceeds receiver cap {limit}")]
    ResponseExceedsReceiverCap {
        resource: &'static str,
        requested: u64,
        limit: u64,
    },
    #[error("exact render aggregate {resource}={requested} exceeds cap {limit}")]
    AggregateLimitExceeded {
        resource: &'static str,
        requested: u64,
        limit: u64,
    },
    #[error("exact render backing reservation {resource}={requested} exceeds cap {limit}")]
    BackingReservationLimitExceeded {
        resource: &'static str,
        requested: u64,
        limit: u64,
    },
    #[error(
        "exact render backing reservation cap {resource}={requested} exceeds protocol maximum {maximum}"
    )]
    InvalidBackingReservationCap {
        resource: &'static str,
        requested: u64,
        maximum: u64,
    },
    #[error("exact render snapshot token was paired with inconsistent immutable manifests")]
    SnapshotManifestEquivocation,
    #[error("exact render pane id cannot be represented by the local mux architecture")]
    PaneIdOutOfRange,
    #[error("exact render authority value could not be canonically measured")]
    Encoding,
}

fn validate_exact_render_protocol_version(
    version: u16,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    if version != EXACT_RENDER_DELIVERY_PROTOCOL_VERSION {
        return Err(
            ExactRenderDeliveryProtocolError::UnsupportedProtocolVersion {
                actual: version,
                expected: EXACT_RENDER_DELIVERY_PROTOCOL_VERSION,
            },
        );
    }
    Ok(())
}

fn exact_render_encoded_len<T: Serialize>(
    value: &T,
) -> Result<u64, ExactRenderDeliveryProtocolError> {
    struct CountingWriter {
        bytes: u64,
    }

    impl std::io::Write for CountingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.bytes = self
                .bytes
                .checked_add(u64::try_from(buffer.len()).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "exact render encoded length does not fit u64",
                    )
                })?)
                .ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "exact render encoded length overflow",
                    )
                })?;
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = CountingWriter { bytes: 0 };
    let mut serializer = varbincode::Serializer::new(&mut counter);
    #[cfg(test)]
    record_test_serialize_invocation();
    value
        .serialize(&mut serializer)
        .map_err(|_| ExactRenderDeliveryProtocolError::Encoding)?;
    Ok(counter.bytes)
}

fn hash_exact_render_cursor(hasher: &mut Sha256, cursor: ExactRenderDeliveryCursor) {
    hasher.update(cursor.pane_generation.get().to_be_bytes());
    hasher.update(cursor.delivery_generation.get().to_be_bytes());
    hasher.update(cursor.sequence.get().to_be_bytes());
}

fn hash_exact_render_request_identity(
    hasher: &mut Sha256,
    identity: ExactRenderDeliveryRequestIdentity,
) {
    hasher.update(identity.connection_identity.stream_id.as_bytes());
    hasher.update(identity.connection_identity.session_incarnation.as_bytes());
    hasher.update(identity.pane_id.get().to_be_bytes());
    hasher.update(identity.request_sequence.get().to_be_bytes());
}

fn hash_exact_render_applied_baseline(hasher: &mut Sha256, baseline: ExactRenderAppliedBaseline) {
    match baseline {
        ExactRenderAppliedBaseline::Uninitialized => hasher.update([0]),
        ExactRenderAppliedBaseline::Applied(cursor) => {
            hasher.update([1]);
            hash_exact_render_cursor(hasher, cursor);
        }
    }
}

fn hash_exact_render_projection(
    hasher: &mut Sha256,
    projection: &ExactRenderProjectionV1,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    hasher.update(projection.first_stable_row.to_be_bytes());
    hasher.update(projection.row_count.to_be_bytes());
    hasher.update([u8::from(projection.alt_screen_active)]);
    hasher.update([u8::from(projection.mouse_grabbed)]);
    hasher.update(projection.cursor_position.x.to_be_bytes());
    hasher.update(projection.cursor_position.y.to_be_bytes());
    hasher.update([projection.cursor_position.shape.digest_tag()]);
    hasher.update([projection.cursor_position.visibility.digest_tag()]);
    hasher.update(projection.dimensions.cols.to_be_bytes());
    hasher.update(projection.dimensions.viewport_rows.to_be_bytes());
    hasher.update(projection.dimensions.scrollback_rows.to_be_bytes());
    hasher.update(projection.dimensions.physical_top.to_be_bytes());
    hasher.update(projection.dimensions.scrollback_top.to_be_bytes());
    hasher.update(projection.dimensions.dpi.to_be_bytes());
    hasher.update(projection.dimensions.pixel_width.to_be_bytes());
    hasher.update(projection.dimensions.pixel_height.to_be_bytes());
    hasher.update([u8::from(projection.dimensions.reverse_video)]);
    hash_exact_render_bytes(hasher, projection.title.as_bytes())?;
    match &projection.working_dir {
        Some(working_dir) => {
            hasher.update([1]);
            hash_exact_render_bytes(hasher, working_dir.as_bytes())?;
        }
        None => hasher.update([0]),
    }
    Ok(())
}

fn hash_exact_render_bytes(
    hasher: &mut Sha256,
    bytes: &[u8],
) -> Result<(), ExactRenderDeliveryProtocolError> {
    hasher.update(
        u64::try_from(bytes.len())
            .map_err(|_| ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "digest_byte_length",
            })?
            .to_be_bytes(),
    );
    hasher.update(bytes);
    Ok(())
}

fn hash_exact_render_token_context(
    hasher: &mut Sha256,
    token: ExactRenderDeliveryToken,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    hasher.update(token.connection_identity.stream_id.as_bytes());
    hasher.update(token.connection_identity.session_incarnation.as_bytes());
    hasher.update(token.pane_id.get().to_be_bytes());
    hash_exact_render_cursor(hasher, token.resulting_baseline);
    Ok(())
}

fn hash_exact_render_token(
    hasher: &mut Sha256,
    token: ExactRenderDeliveryToken,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    hash_exact_render_token_context(hasher, token)?;
    hasher.update(token.content_digest.as_bytes());
    Ok(())
}

fn hash_exact_render_rows(
    hasher: &mut Sha256,
    rows: &[ExactRenderRowV1],
) -> Result<(), ExactRenderDeliveryProtocolError> {
    hasher.update(
        u64::try_from(rows.len())
            .map_err(|_| ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "digest_row_count",
            })?
            .to_be_bytes(),
    );
    for row in rows {
        hash_exact_render_row(hasher, row)?;
    }
    Ok(())
}

fn hash_exact_render_row(
    hasher: &mut Sha256,
    row: &ExactRenderRowV1,
) -> Result<(), ExactRenderDeliveryProtocolError> {
    hasher.update(row.stable_row.to_be_bytes());
    hasher.update([u8::from(row.wrapped)]);
    hasher.update(
        u64::try_from(row.text.len())
            .map_err(|_| ExactRenderDeliveryProtocolError::ArithmeticOverflow {
                field: "digest_row_text_bytes",
            })?
            .to_be_bytes(),
    );
    hasher.update(row.text.as_bytes());
    Ok(())
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

/// Maximum number of floating panes carried by one authoritative topology
/// snapshot. Matching the ordered tiled-leaf ceiling keeps the two independent
/// pane classes under a simple combined `2 * ceiling` cardinality bound.
pub const MAX_FLOATING_PANES_PER_SNAPSHOT: usize = MAX_ORDERED_PANE_LEAVES_PER_SNAPSHOT;

/// Complete remote state for one floating pane. `pane` carries its exact
/// window/tab owner plus the same process and terminal metadata as a tiled
/// leaf; the remaining fields reconstruct overlay geometry and presentation.
#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct FloatingPaneSnapshotEntry {
    pub pane: mux::tab::PaneEntry,
    pub rect: FloatingPaneRect,
    pub z_order: u32,
    pub visible: bool,
    pub pinned: bool,
    pub opacity: f32,
    pub focused: bool,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum FloatingPaneSnapshotError {
    #[error("floating pane snapshot has {count} entries; maximum is {max}")]
    TooManyEntries { count: usize, max: usize },
    #[error("floating pane snapshot contains duplicate pane id {pane_id}")]
    DuplicatePaneId { pane_id: PaneId },
    #[error("floating pane {pane_id} has an empty {axis} extent")]
    EmptyExtent { pane_id: PaneId, axis: &'static str },
    #[error("floating pane {pane_id} has invalid opacity {opacity}")]
    InvalidOpacity { pane_id: PaneId, opacity: String },
    #[error("floating pane {pane_id} is focused while hidden")]
    FocusedPaneHidden { pane_id: PaneId },
    #[error(
        "floating pane {pane_id} focus disagrees with pane-entry active state: focused={focused}, pane_entry_active={pane_entry_active}"
    )]
    FocusStateMismatch {
        pane_id: PaneId,
        focused: bool,
        pane_entry_active: bool,
    },
    #[error("floating pane {pane_id} cannot carry tiled-pane zoom state")]
    ZoomStatePresent { pane_id: PaneId },
    #[error("floating pane {pane_id} geometry disagrees with its pane entry")]
    GeometryMismatch { pane_id: PaneId },
    #[error("floating tab {tab_id} contains more than one focused pane")]
    MultipleFocusedPanes { tab_id: TabId },
}

fn serialize_floating_pane_snapshot<S>(
    values: &[FloatingPaneSnapshotEntry],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serialize_bounded_vec::<S, _, MAX_FLOATING_PANES_PER_SNAPSHOT>(
        values,
        serializer,
        "floating pane snapshot entries",
    )
}

fn deserialize_floating_pane_snapshot<'de, D>(
    deserializer: D,
) -> Result<Vec<FloatingPaneSnapshotEntry>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_vec::<D, _, MAX_FLOATING_PANES_PER_SNAPSHOT>(
        deserializer,
        "floating pane snapshot entries",
    )
}

fn validate_floating_pane_snapshot(
    floating_panes: &[FloatingPaneSnapshotEntry],
) -> Result<(), FloatingPaneSnapshotError> {
    if floating_panes.len() > MAX_FLOATING_PANES_PER_SNAPSHOT {
        return Err(FloatingPaneSnapshotError::TooManyEntries {
            count: floating_panes.len(),
            max: MAX_FLOATING_PANES_PER_SNAPSHOT,
        });
    }
    let mut pane_ids = HashSet::with_capacity(floating_panes.len());
    let mut focused_tabs = HashSet::new();
    for floating in floating_panes {
        let pane_id = floating.pane.pane_id;
        if !pane_ids.insert(pane_id) {
            return Err(FloatingPaneSnapshotError::DuplicatePaneId { pane_id });
        }
        if floating.rect.width == 0 {
            return Err(FloatingPaneSnapshotError::EmptyExtent {
                pane_id,
                axis: "width",
            });
        }
        if floating.rect.height == 0 {
            return Err(FloatingPaneSnapshotError::EmptyExtent {
                pane_id,
                axis: "height",
            });
        }
        if !floating.opacity.is_finite() || !(0.0..=1.0).contains(&floating.opacity) {
            return Err(FloatingPaneSnapshotError::InvalidOpacity {
                pane_id,
                opacity: floating.opacity.to_string(),
            });
        }
        if floating.focused && !floating.visible {
            return Err(FloatingPaneSnapshotError::FocusedPaneHidden { pane_id });
        }
        if floating.pane.is_active_pane != floating.focused {
            return Err(FloatingPaneSnapshotError::FocusStateMismatch {
                pane_id,
                focused: floating.focused,
                pane_entry_active: floating.pane.is_active_pane,
            });
        }
        if floating.pane.is_zoomed_pane {
            return Err(FloatingPaneSnapshotError::ZoomStatePresent { pane_id });
        }
        if floating.pane.left_col != floating.rect.left
            || floating.pane.top_row != floating.rect.top
            || floating.pane.size.cols != floating.rect.width
            || floating.pane.size.rows != floating.rect.height
        {
            return Err(FloatingPaneSnapshotError::GeometryMismatch { pane_id });
        }
        if floating.focused && !focused_tabs.insert(floating.pane.tab_id) {
            return Err(FloatingPaneSnapshotError::MultipleFocusedPanes {
                tab_id: floating.pane.tab_id,
            });
        }
    }
    Ok(())
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
pub struct ListPanesResponse {
    pub tabs: Vec<PaneNode>,
    pub tab_titles: Vec<String>,
    pub window_titles: HashMap<WindowId, String>,
    #[serde(
        serialize_with = "serialize_floating_pane_snapshot",
        deserialize_with = "deserialize_floating_pane_snapshot"
    )]
    pub floating_panes: Vec<FloatingPaneSnapshotEntry>,
}

impl ListPanesResponse {
    pub fn validate_floating_panes(&self) -> Result<(), FloatingPaneSnapshotError> {
        validate_floating_pane_snapshot(&self.floating_panes)
    }
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
    /// Exact client-generated identity for the dispatch fence returned after
    /// this paste has been committed to the pane.
    pub input_serial: InputSerial,
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
/// or alias the ordering relation. The terminal value can be issued once;
/// subsequent local allocation fails closed rather than reusing that identity.
#[derive(Deserialize, Serialize, PartialEq, Eq, Debug, Clone, Copy, PartialOrd, Ord)]
pub struct InputSerial(u64);

static LAST_INPUT_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl InputSerial {
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Constructs an exact wire-domain identity without routing through
    /// platform-specific [`std::time::SystemTime`] arithmetic.
    ///
    /// The full `u64` domain is valid on the wire, including values that a
    /// particular operating system cannot represent as an epoch-relative
    /// `SystemTime`.
    pub const fn from_millis_since_epoch(millis: u64) -> Self {
        Self(millis)
    }

    pub fn now() -> Self {
        let wall_clock = input_serial_from_system_time(std::time::SystemTime::now()).0;
        let serial = next_input_serial(&LAST_INPUT_SERIAL, wall_clock).unwrap_or_else(|| {
            panic!("process-local input serial space exhausted; refusing to reuse an identity")
        });
        Self(serial)
    }

    pub fn elapsed_millis(&self) -> u64 {
        let now = input_serial_from_system_time(std::time::SystemTime::now());
        now.0.saturating_sub(self.0)
    }
}

fn next_input_serial(counter: &std::sync::atomic::AtomicU64, wall_clock: u64) -> Option<u64> {
    use std::sync::atomic::Ordering;

    let mut observed = counter.load(Ordering::Relaxed);
    loop {
        let candidate = wall_clock.max(observed.checked_add(1)?);
        match counter.compare_exchange_weak(
            observed,
            candidate,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return Some(candidate),
            Err(current) => observed = current,
        }
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

/// Maximum number of panes sampled by one lightweight scrollback-health turn.
///
/// The fleet campaign's largest ordinary class is 200 panes, so 256 keeps one
/// cycle to one RPC while bounding mux-main callback work, decoded collection
/// allocation, and response bytes independently of session age.
pub const MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES: usize = 256;

/// Schema-specific wire ceilings for the bounded health request/response.
pub const MAX_TIERED_SCROLLBACK_STATUS_REQUEST_DECOMPRESSED_BYTES: usize = 4 * 1024;
pub const MAX_TIERED_SCROLLBACK_STATUS_REQUEST_ZSTD_ENCODED_BYTES: usize =
    MAX_TIERED_SCROLLBACK_STATUS_REQUEST_DECOMPRESSED_BYTES + 128;
pub const MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_DECOMPRESSED_BYTES: usize = 32 * 1024;
pub const MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_ZSTD_ENCODED_BYTES: usize =
    MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_DECOMPRESSED_BYTES + 256;

fn serialize_tiered_scrollback_batch_pane_ids<S>(
    pane_ids: &[PaneId],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if pane_ids.is_empty() || pane_ids.len() > MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES {
        return Err(serde::ser::Error::custom(format_args!(
            "tiered scrollback batch pane-id count {} is outside 1..={}",
            pane_ids.len(),
            MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES,
        )));
    }
    serializer.serialize_newtype_struct(
        bounded_varbincode::TIERED_SCROLLBACK_BATCH_PANE_IDS_V1_NEWTYPE,
        pane_ids,
    )
}

fn deserialize_tiered_scrollback_batch_pane_ids<'de, D>(
    deserializer: D,
) -> Result<Vec<PaneId>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_newtype_vec::<D, PaneId, MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES>(
        deserializer,
        "tiered scrollback batch pane ids",
        bounded_varbincode::TIERED_SCROLLBACK_BATCH_PANE_IDS_V1_NEWTYPE,
    )
}

fn serialize_tiered_scrollback_batch_entries<S>(
    entries: &[PaneTieredScrollbackStatusEntryV1],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    if entries.is_empty() || entries.len() > MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES {
        return Err(serde::ser::Error::custom(format_args!(
            "tiered scrollback batch entry count {} is outside 1..={}",
            entries.len(),
            MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES,
        )));
    }
    serializer.serialize_newtype_struct(
        bounded_varbincode::TIERED_SCROLLBACK_BATCH_ENTRIES_V1_NEWTYPE,
        entries,
    )
}

fn deserialize_tiered_scrollback_batch_entries<'de, D>(
    deserializer: D,
) -> Result<Vec<PaneTieredScrollbackStatusEntryV1>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_newtype_vec::<
        D,
        PaneTieredScrollbackStatusEntryV1,
        MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES,
    >(
        deserializer,
        "tiered scrollback batch entries",
        bounded_varbincode::TIERED_SCROLLBACK_BATCH_ENTRIES_V1_NEWTYPE,
    )
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PaneTieredScrollbackSummaryV1 {
    pub tiering_enabled: bool,
    pub configured_scrollback_rows: usize,
    pub configured_hot_lines: usize,
    pub configured_warm_max_bytes: usize,
    pub visible_rows: usize,
    pub in_memory_scrollback_rows: usize,
    pub warm_resident_lines: usize,
    pub warm_resident_bytes: usize,
    pub warm_spill_lines_total: u64,
    pub warm_spill_bytes_total: u64,
}

impl From<PaneTieredScrollbackStatus> for PaneTieredScrollbackSummaryV1 {
    fn from(status: PaneTieredScrollbackStatus) -> Self {
        Self {
            tiering_enabled: status.tiering_enabled,
            configured_scrollback_rows: status.configured_scrollback_rows,
            configured_hot_lines: status.configured_hot_lines,
            configured_warm_max_bytes: status.configured_warm_max_bytes,
            visible_rows: status.visible_rows,
            in_memory_scrollback_rows: status.in_memory_scrollback_rows,
            warm_resident_lines: status.warm_resident_lines,
            warm_resident_bytes: status.warm_resident_bytes,
            warm_spill_lines_total: status.warm_spill_lines_total,
            warm_spill_bytes_total: status.warm_spill_bytes_total,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PaneTieredScrollbackStatusOutcomeV1 {
    Available(PaneTieredScrollbackSummaryV1),
    /// The pane is live but its implementation has no tiered-scrollback state.
    Unavailable,
    /// No pane registration existed for the requested identity in this turn.
    Missing,
    /// The captured registration stopped being current before its callback.
    Closed,
    /// The pane callback panicked inside the canonical recovery boundary.
    CallbackPanicked,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PaneTieredScrollbackStatusEntryV1 {
    pub pane_id: PaneId,
    pub outcome: PaneTieredScrollbackStatusOutcomeV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GetPaneTieredScrollbackStatusesV1 {
    #[serde(
        serialize_with = "serialize_tiered_scrollback_batch_pane_ids",
        deserialize_with = "deserialize_tiered_scrollback_batch_pane_ids"
    )]
    pub pane_ids: Vec<PaneId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GetPaneTieredScrollbackStatusesV1Response {
    #[serde(
        serialize_with = "serialize_tiered_scrollback_batch_entries",
        deserialize_with = "deserialize_tiered_scrollback_batch_entries"
    )]
    pub entries: Vec<PaneTieredScrollbackStatusEntryV1>,
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum TieredScrollbackStatusBatchError {
    #[error("tiered scrollback status batch must contain at least one pane")]
    Empty,
    #[error("tiered scrollback status batch contains {count} panes; maximum is {max}")]
    TooMany { count: usize, max: usize },
    #[error("tiered scrollback status batch repeats pane {pane_id}")]
    DuplicatePane { pane_id: PaneId },
}

fn validate_tiered_scrollback_status_batch_ids(
    pane_ids: impl IntoIterator<Item = PaneId>,
    count: usize,
) -> Result<(), TieredScrollbackStatusBatchError> {
    if count == 0 {
        return Err(TieredScrollbackStatusBatchError::Empty);
    }
    if count > MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES {
        return Err(TieredScrollbackStatusBatchError::TooMany {
            count,
            max: MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES,
        });
    }
    let mut unique = HashSet::with_capacity(count);
    for pane_id in pane_ids {
        if !unique.insert(pane_id) {
            return Err(TieredScrollbackStatusBatchError::DuplicatePane { pane_id });
        }
    }
    Ok(())
}

impl GetPaneTieredScrollbackStatusesV1 {
    pub fn validate(&self) -> Result<(), TieredScrollbackStatusBatchError> {
        validate_tiered_scrollback_status_batch_ids(
            self.pane_ids.iter().copied(),
            self.pane_ids.len(),
        )
    }
}

impl GetPaneTieredScrollbackStatusesV1Response {
    pub fn validate(&self) -> Result<(), TieredScrollbackStatusBatchError> {
        validate_tiered_scrollback_status_batch_ids(
            self.entries.iter().map(|entry| entry.pane_id),
            self.entries.len(),
        )
    }
}

fn deserialize_get_pane_tiered_scrollback_statuses_v1(
    data: &[u8],
    is_compressed: bool,
) -> Result<GetPaneTieredScrollbackStatusesV1, Error> {
    let request: GetPaneTieredScrollbackStatusesV1 = deserialize(data, is_compressed)?;
    request.validate()?;
    Ok(request)
}

fn deserialize_get_pane_tiered_scrollback_statuses_v1_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<GetPaneTieredScrollbackStatusesV1Response, Error> {
    let response: GetPaneTieredScrollbackStatusesV1Response = deserialize(data, is_compressed)?;
    response.validate()?;
    Ok(response)
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
                limit: u64::try_from(MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES).unwrap_or(u64::MAX),
            });
        };
        if alert_text_bytes > MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES {
            return Err(RenderApplicationContractError::ResourceLimitExceeded {
                resource: RenderApplicationResource::Alerts,
                requested: u64::try_from(alert_text_bytes).unwrap_or(u64::MAX),
                limit: u64::try_from(MAX_RENDER_APPLICATION_ALERT_TEXT_BYTES).unwrap_or(u64::MAX),
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
                requested: u64::try_from(self.surface.bonus_lines.line_count()).unwrap_or(u64::MAX),
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
                    | SerializedLinesStructureError::ImageCellOutOfRange
                    | SerializedLinesStructureError::ImageTextureCoordinatesInvalid => {
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
            dirty_rows = dirty_rows.checked_add(span).ok_or(
                RenderApplicationContractError::ResourceLimitExceeded {
                    resource: RenderApplicationResource::Lines,
                    requested: u64::MAX,
                    limit: u64::try_from(MAX_RENDER_APPLICATION_LINES).unwrap_or(u64::MAX),
                },
            )?;
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
                || u64::try_from(line.current_seqno()).map_or(true, |seqno| {
                    seqno > self.identity.resulting_state.state_sequence
                })
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
                    limit: u64::try_from(MAX_RENDER_APPLICATION_SEMANTIC_ZONES).unwrap_or(u64::MAX),
                });
            }
            let semantic_text_bytes = semantic
                .zone_texts
                .iter()
                .try_fold(0usize, |total, text| total.checked_add(text.len()));
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
    Applied { applied_state: RenderStateIdentity },
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

fn serialize_hyperlink_coordinates<S>(
    values: &[CellCoordinates],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serialize_bounded_newtype_vec::<S, _, MAX_RENDER_APPLICATION_HYPERLINK_SPANS>(
        values,
        serializer,
        "serialized hyperlink coordinates",
        bounded_varbincode::SERIALIZED_HYPERLINK_COORDINATES_V1_NEWTYPE,
    )
}

fn deserialize_hyperlink_coordinates<'de, D>(
    deserializer: D,
) -> Result<Vec<CellCoordinates>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_newtype_vec::<D, _, MAX_RENDER_APPLICATION_HYPERLINK_SPANS>(
        deserializer,
        "serialized hyperlink coordinates",
        bounded_varbincode::SERIALIZED_HYPERLINK_COORDINATES_V1_NEWTYPE,
    )
}

#[derive(Deserialize, Serialize, PartialEq, Debug, Clone)]
struct LineHyperlink {
    link: Hyperlink,
    #[serde(
        serialize_with = "serialize_hyperlink_coordinates",
        deserialize_with = "deserialize_hyperlink_coordinates"
    )]
    coords: Vec<CellCoordinates>,
}

fn record_serialized_hyperlink_span(
    hyperlinks: &mut Vec<LineHyperlink>,
    hyperlink_by_identity: &mut HashMap<*const Hyperlink, (Arc<Hyperlink>, usize)>,
    link: &Arc<Hyperlink>,
    line_idx: usize,
    cols: Range<usize>,
) {
    let identity = Arc::as_ptr(link);
    if let Some((identity_owner, index)) = hyperlink_by_identity.get(&identity) {
        debug_assert!(Arc::ptr_eq(identity_owner, link));
        hyperlinks[*index]
            .coords
            .push(CellCoordinates { line_idx, cols });
    } else {
        let index = hyperlinks.len();
        hyperlinks.push(LineHyperlink {
            link: (**link).clone(),
            coords: vec![CellCoordinates { line_idx, cols }],
        });
        // Keep the allocation that supplied the raw identity alive until the
        // whole serialization pass ends.  Retaining only a cloned Hyperlink
        // value permits the source Arc to drop after its cells are cleared;
        // an allocator may then reuse that address for a distinct hyperlink
        // and spuriously merge their spans (pointer-identity ABA).
        hyperlink_by_identity.insert(identity, (Arc::clone(link), index));
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
pub struct SerializedImageCell {
    pub line_idx: StableRowIndex,
    pub cell_idx: usize,
    // The following fields are taken from termwiz::image::ImageCell
    pub top_left: TextureCoordinate,
    pub bottom_right: TextureCoordinate,
    /// Current content revision for the ImageCell::data field. This is
    /// intentionally distinct from ImageData's stable object identity because
    /// Kitty can edit an existing image in place.
    pub data_hash: [u8; 32],
    pub z_index: i32,
    pub padding_left: u16,
    pub padding_top: u16,
    pub padding_right: u16,
    pub padding_bottom: u16,
    pub image_id: Option<u32>,
    pub placement_id: Option<u32>,
}

impl SerializedImageCell {
    /// Return texture coordinates canonicalized to the renderer's closed unit
    /// square. Structure validation permits only a tiny f32 serialization
    /// tolerance outside that square; clamping here prevents that harmless
    /// rounding residue from reaching atlas or shader arithmetic.
    #[must_use]
    pub fn canonical_texture_coordinates(&self) -> (TextureCoordinate, TextureCoordinate) {
        (
            TextureCoordinate::new_f32(
                self.top_left.x.into_inner().clamp(0.0, 1.0),
                self.top_left.y.into_inner().clamp(0.0, 1.0),
            ),
            TextureCoordinate::new_f32(
                self.bottom_right.x.into_inner().clamp(0.0, 1.0),
                self.bottom_right.y.into_inner().clamp(0.0, 1.0),
            ),
        )
    }
}

fn serialize_line_entries<S>(
    values: &[(StableRowIndex, Line)],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serialize_bounded_newtype_vec::<S, _, MAX_RENDER_APPLICATION_LINES>(
        values,
        serializer,
        "serialized line entries",
        bounded_varbincode::SERIALIZED_LINE_ENTRIES_V1_NEWTYPE,
    )
}

fn deserialize_line_entries<'de, D>(
    deserializer: D,
) -> Result<Vec<(StableRowIndex, Line)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_newtype_vec::<D, _, MAX_RENDER_APPLICATION_LINES>(
        deserializer,
        "serialized line entries",
        bounded_varbincode::SERIALIZED_LINE_ENTRIES_V1_NEWTYPE,
    )
}

fn serialize_line_hyperlinks<S>(values: &[LineHyperlink], serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    let total_spans = values.iter().try_fold(0usize, |total, hyperlink| {
        total.checked_add(hyperlink.coords.len()).ok_or_else(|| {
            serde::ser::Error::custom("serialized hyperlink span accounting overflowed")
        })
    })?;
    if total_spans > MAX_RENDER_APPLICATION_HYPERLINK_SPANS {
        return Err(serde::ser::Error::custom(format_args!(
            "serialized hyperlinks contain {total_spans} spans, exceeding maximum {MAX_RENDER_APPLICATION_HYPERLINK_SPANS}"
        )));
    }
    serialize_bounded_newtype_vec::<S, _, MAX_RENDER_APPLICATION_HYPERLINK_SPANS>(
        values,
        serializer,
        "serialized hyperlinks",
        bounded_varbincode::SERIALIZED_HYPERLINKS_V1_NEWTYPE,
    )
}

struct BoundedLineHyperlinksVisitor;

impl<'de> serde::de::Visitor<'de> for BoundedLineHyperlinksVisitor {
    type Value = Vec<LineHyperlink>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "at most {MAX_RENDER_APPLICATION_HYPERLINK_SPANS} serialized hyperlink spans"
        )
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let hinted = sequence.size_hint().unwrap_or(0);
        if hinted > MAX_RENDER_APPLICATION_HYPERLINK_SPANS {
            return Err(serde::de::Error::custom(format_args!(
                "serialized hyperlinks length {hinted} exceeds maximum {MAX_RENDER_APPLICATION_HYPERLINK_SPANS}"
            )));
        }
        let mut hyperlinks = Vec::new();
        hyperlinks.try_reserve(hinted).map_err(|error| {
            serde::de::Error::custom(format_args!(
                "allocating serialized hyperlinks length {hinted} failed: {error}"
            ))
        })?;
        let mut total_spans = 0usize;
        while hyperlinks.len() < MAX_RENDER_APPLICATION_HYPERLINK_SPANS {
            let Some(hyperlink) = sequence.next_element::<LineHyperlink>()? else {
                return Ok(hyperlinks);
            };
            total_spans = total_spans
                .checked_add(hyperlink.coords.len())
                .ok_or_else(|| {
                    serde::de::Error::custom("serialized hyperlink span accounting overflowed")
                })?;
            if total_spans > MAX_RENDER_APPLICATION_HYPERLINK_SPANS {
                return Err(serde::de::Error::custom(format_args!(
                    "serialized hyperlinks contain {total_spans} spans, exceeding maximum {MAX_RENDER_APPLICATION_HYPERLINK_SPANS}"
                )));
            }
            hyperlinks.push(hyperlink);
        }
        if sequence.next_element::<serde::de::IgnoredAny>()?.is_some() {
            return Err(serde::de::Error::custom(format_args!(
                "serialized hyperlinks length exceeds maximum {MAX_RENDER_APPLICATION_HYPERLINK_SPANS}"
            )));
        }
        Ok(hyperlinks)
    }
}

struct BoundedLineHyperlinksNewtypeVisitor;

impl<'de> serde::de::Visitor<'de> for BoundedLineHyperlinksNewtypeVisitor {
    type Value = Vec<LineHyperlink>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded serialized-hyperlinks newtype")
    }

    fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_seq(BoundedLineHyperlinksVisitor)
    }
}

fn deserialize_line_hyperlinks<'de, D>(deserializer: D) -> Result<Vec<LineHyperlink>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserializer.deserialize_newtype_struct(
        bounded_varbincode::SERIALIZED_HYPERLINKS_V1_NEWTYPE,
        BoundedLineHyperlinksNewtypeVisitor,
    )
}

fn serialize_image_references<S>(
    values: &[SerializedImageCell],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serialize_bounded_newtype_vec::<S, _, MAX_RENDER_APPLICATION_IMAGE_REFERENCES>(
        values,
        serializer,
        "serialized image references",
        bounded_varbincode::SERIALIZED_IMAGE_REFERENCES_V1_NEWTYPE,
    )
}

fn deserialize_image_references<'de, D>(
    deserializer: D,
) -> Result<Vec<SerializedImageCell>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bounded_newtype_vec::<D, _, MAX_RENDER_APPLICATION_IMAGE_REFERENCES>(
        deserializer,
        "serialized image references",
        bounded_varbincode::SERIALIZED_IMAGE_REFERENCES_V1_NEWTYPE,
    )
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
    #[serde(
        serialize_with = "serialize_line_entries",
        deserialize_with = "deserialize_line_entries"
    )]
    lines: Vec<(StableRowIndex, Line)>,
    #[serde(
        serialize_with = "serialize_line_hyperlinks",
        deserialize_with = "deserialize_line_hyperlinks"
    )]
    hyperlinks: Vec<LineHyperlink>,
    #[serde(
        serialize_with = "serialize_image_references",
        deserialize_with = "deserialize_image_references"
    )]
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
pub type ExtractedSerializedLines = (Vec<(StableRowIndex, Line)>, Vec<SerializedImageCell>);

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
    #[error(
        "serialized image has non-finite, reversed, empty, or out-of-range texture coordinates"
    )]
    ImageTextureCoordinatesInvalid,
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
            let left = image.top_left.x.into_inner();
            let top = image.top_left.y.into_inner();
            let right = image.bottom_right.x.into_inner();
            let bottom = image.bottom_right.y.into_inner();
            const TEXTURE_COORDINATE_WIRE_TOLERANCE: f32 = f32::EPSILON * 8.0;
            if !left.is_finite()
                || !top.is_finite()
                || !right.is_finite()
                || !bottom.is_finite()
                || left < -TEXTURE_COORDINATE_WIRE_TOLERANCE
                || top < -TEXTURE_COORDINATE_WIRE_TOLERANCE
                || right > 1.0 + TEXTURE_COORDINATE_WIRE_TOLERANCE
                || bottom > 1.0 + TEXTURE_COORDINATE_WIRE_TOLERANCE
                || left >= right
                || top >= bottom
            {
                return Err(SerializedLinesStructureError::ImageTextureCoordinatesInvalid);
            }
            let canonical_left = left.clamp(0.0, 1.0);
            let canonical_top = top.clamp(0.0, 1.0);
            let canonical_right = right.clamp(0.0, 1.0);
            let canonical_bottom = bottom.clamp(0.0, 1.0);
            if canonical_left >= canonical_right || canonical_top >= canonical_bottom {
                return Err(SerializedLinesStructureError::ImageTextureCoordinatesInvalid);
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
        let mut hyperlinks = Vec::new();
        let mut hyperlink_by_identity = HashMap::new();
        let mut images = vec![];
        let mut image_revision_by_identity =
            HashMap::<*const ImageData, (Arc<ImageData>, [u8; 32])>::new();

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
                            record_serialized_hyperlink_span(
                                &mut hyperlinks,
                                &mut hyperlink_by_identity,
                                prior,
                                line_idx,
                                current_range,
                            );
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
                    record_serialized_hyperlink_span(
                        &mut hyperlinks,
                        &mut hyperlink_by_identity,
                        &link,
                        line_idx,
                        current_range,
                    );
                    current_range = 0..0;
                }

                if let Some(cell_images) = cell.attrs().images() {
                    for imcell in cell_images {
                        let image_data = imcell.image_data();
                        let image_identity = Arc::as_ptr(image_data);
                        let data_hash = if let Some((identity_owner, revision)) =
                            image_revision_by_identity.get(&image_identity)
                        {
                            debug_assert!(Arc::ptr_eq(identity_owner, image_data));
                            *revision
                        } else {
                            let revision = image_data.current_content_hash();
                            image_revision_by_identity
                                .insert(image_identity, (Arc::clone(image_data), revision));
                            revision
                        };
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
                            data_hash,
                        });
                    }
                }
                cell.attrs_mut().clear_images();
            }
            if let Some(link) = current_link.take() {
                // Wrap up final streak
                record_serialized_hyperlink_span(
                    &mut hyperlinks,
                    &mut hyperlink_by_identity,
                    &link,
                    line_idx,
                    current_range,
                );
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

fn deserialize_get_image_cell_response(
    data: &[u8],
    is_compressed: bool,
) -> Result<GetImageCellResponse, Error> {
    deserialize_exact_payload_with_limit(
        data,
        is_compressed,
        "GetImageCellResponse",
        MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES,
    )
}

#[cfg(test)]
mod test {
    use super::*;
    use proptest::prelude::*;

    thread_local! {
        static BOUNDED_OVERFLOW_VALUE_DESERIALIZATIONS: std::cell::Cell<usize> =
            const { std::cell::Cell::new(0) };
    }

    #[derive(Debug)]
    struct CountedBoundedValue;

    impl<'de> serde::Deserialize<'de> for CountedBoundedValue {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            BOUNDED_OVERFLOW_VALUE_DESERIALIZATIONS.with(|count| count.set(count.get() + 1));
            let _ = u8::deserialize(deserializer)?;
            Ok(Self)
        }
    }

    #[test]
    fn bounded_visitors_do_not_materialize_the_first_overflow_value() {
        BOUNDED_OVERFLOW_VALUE_DESERIALIZATIONS.with(|count| count.set(0));
        let mut sequence = serde_json::Deserializer::from_str("[1,2,3]");
        let sequence_error =
            deserialize_bounded_vec::<_, CountedBoundedValue, 2>(&mut sequence, "counted sequence")
                .expect_err("the third sequence value must exceed the bound");
        assert!(
            sequence_error.to_string().contains("maximum 2"),
            "unexpected sequence admission error: {}",
            sequence_error
        );
        BOUNDED_OVERFLOW_VALUE_DESERIALIZATIONS.with(|count| {
            assert_eq!(
                count.get(),
                2,
                "the first overflow sequence value must be ignored without materializing T"
            );
            count.set(0);
        });

        let mut map = serde_json::Deserializer::from_str(r#"{"a":1,"b":2,"c":3}"#);
        let map_error =
            deserialize_bounded_map::<_, String, CountedBoundedValue, 2>(&mut map, "counted map")
                .expect_err("the third map value must exceed the bound");
        assert!(
            map_error.to_string().contains("maximum 2"),
            "unexpected map admission error: {}",
            map_error
        );
        BOUNDED_OVERFLOW_VALUE_DESERIALIZATIONS.with(|count| {
            assert_eq!(
                count.get(),
                2,
                "the first overflow map value must not be deserialized"
            );
        });
    }

    fn declared_pdu_frame_header(
        ident: u64,
        serial: u64,
        encoded_payload_bytes: usize,
        is_compressed: bool,
    ) -> Vec<u8> {
        let frame_body_bytes = encoded_payload_bytes
            .checked_add(encoded_length(serial))
            .and_then(|len| len.checked_add(encoded_length(ident)))
            .expect("test frame-body length must fit usize");
        let frame_body_bytes =
            u64::try_from(frame_body_bytes).expect("test frame-body length must fit u64");
        let tagged_len = if is_compressed {
            frame_body_bytes | COMPRESSED_MASK
        } else {
            frame_body_bytes
        };
        let mut header = Vec::new();
        leb128::write::unsigned(&mut header, tagged_len).expect("encode test frame length");
        leb128::write::unsigned(&mut header, serial).expect("encode test serial");
        leb128::write::unsigned(&mut header, ident).expect("encode test identifier");
        header
    }

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
    fn pdu_frame_memory_owned_header_prepend_matches_borrowed_framing_at_leb128_boundaries() {
        for ident in [0, 1, 127, 128, u16::MAX as u64, u64::MAX] {
            for serial in [0, 1, 127, 128, u32::MAX as u64, u64::MAX] {
                for payload_len in [0_usize, 1, 127, 128, 16_384] {
                    let payload = (0..payload_len)
                        .map(|ordinal| ordinal.wrapping_mul(131) as u8)
                        .collect::<Vec<_>>();
                    for is_compressed in [false, true] {
                        let expected =
                            encode_raw_as_vec_impl(ident, serial, &payload, is_compressed, false)
                                .expect("borrowed framing must succeed");
                        let actual = prepend_frame_header_to_owned_payload(
                            ident,
                            serial,
                            payload.clone(),
                            is_compressed,
                            false,
                        )
                        .expect("owned framing must succeed");
                        assert_eq!(actual, expected);
                    }
                }
            }
        }
    }

    #[test]
    fn pdu_frame_memory_bounded_growth_is_logarithmic_and_never_exceeds_its_ceiling() {
        let payload = (0..1_048_576_u32)
            .map(|ordinal| ordinal.wrapping_mul(2_654_435_761) as u8)
            .collect::<Vec<_>>();
        reset_test_bounded_serialize_growth_events();
        let encoded = serialize_uncompressed_bounded(&payload, payload.len() + 16, 91, 87)
            .expect("bounded serializer must admit an in-limit payload");
        let growth_events = test_bounded_serialize_growth_events();
        assert!(
            growth_events <= 16,
            "one-megabyte payload used {} allocation-growth events",
            growth_events,
        );
        assert!(encoded.len() <= payload.len() + 16);
        assert!(test_bounded_serialize_max_requested_capacity() <= payload.len() + 16);

        reset_test_bounded_serialize_growth_events();
        let error = serialize_uncompressed_bounded(&payload, payload.len() - 1, 92, 87)
            .expect_err("an over-limit payload must fail before exceeding its ceiling");
        assert!(error
            .downcast_ref::<PduEncodedBodyLimitExceeded>()
            .is_some());
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
    fn input_serial_allocator_preserves_wall_clock_floor_and_monotonicity() {
        let counter = std::sync::atomic::AtomicU64::new(41);

        assert_eq!(next_input_serial(&counter, 100), Some(100));
        assert_eq!(next_input_serial(&counter, 1), Some(101));
    }

    #[test]
    fn input_serial_allocator_issues_terminal_value_once_then_fails_closed() {
        use std::sync::atomic::Ordering;

        let counter = std::sync::atomic::AtomicU64::new(u64::MAX - 1);

        assert_eq!(next_input_serial(&counter, 0), Some(u64::MAX));
        assert_eq!(next_input_serial(&counter, 0), None);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
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

    #[test]
    fn input_serial_raw_domain_constructor_roundtrips_edge_values() {
        for millis in [0, 1, u64::MAX - 1, u64::MAX] {
            let input_serial = InputSerial::from_millis_since_epoch(millis);
            assert_eq!(input_serial.0, millis);

            let pdu = Pdu::SendPaste(SendPaste {
                pane_id: 3,
                data: "edge".to_string(),
                input_serial,
            });
            let mut encoded = Vec::new();
            pdu.encode(&mut encoded, millis)
                .expect("edge input serial should encode");
            let decoded = Pdu::decode(encoded.as_slice()).expect("edge input serial should decode");
            assert_eq!(decoded.serial, millis);
            assert_eq!(decoded.pdu, pdu);
        }
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
            data: String::new(),
            input_serial: InputSerial::empty(),
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
            input_serial: InputSerial::now(),
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
            floating_panes: Vec::new(),
        }
    }

    fn sample_pane_entry(ordinal: usize) -> PaneEntry {
        PaneEntry {
            window_id: 1,
            tab_id: ordinal.saturating_add(1),
            pane_id: ordinal.saturating_add(1),
            title: format!("pane-{ordinal}"),
            size: TerminalSize::default(),
            working_dir: None,
            alt_screen_active: false,
            is_active_pane: ordinal == 0,
            is_zoomed_pane: false,
            workspace: "ordered-flat-test".to_string(),
            cursor_pos: StableCursorPosition::default(),
            physical_top: 0,
            top_row: 0,
            left_col: 0,
            tty_name: None,
        }
    }

    fn sample_split() -> mux::tab::SplitDirectionAndSize {
        mux::tab::SplitDirectionAndSize {
            direction: mux::tab::SplitDirection::Horizontal,
            first: TerminalSize::default(),
            second: TerminalSize::default(),
        }
    }

    fn left_deep_pane_tree(depth: usize) -> PaneNode {
        assert!(depth > 0, "test pane-tree depth must be nonzero");
        let mut tree = PaneNode::Leaf(sample_pane_entry(0));
        for _ in 1..depth {
            tree = PaneNode::Split {
                left: Box::new(tree),
                right: Box::new(PaneNode::Empty),
                node: sample_split(),
            };
        }
        tree
    }

    fn broad_pane_tree(leaves: usize) -> PaneNode {
        assert!(
            leaves.is_power_of_two(),
            "test leaf count must be a power of two"
        );
        pane_tree_with_slots(leaves, 0, 0)
    }

    fn pane_tree_with_slots(slots: usize, empty_slots: usize, ordinal_base: usize) -> PaneNode {
        assert!(slots > 0, "test pane tree must have at least one slot");
        assert!(
            empty_slots < slots,
            "test pane tree must retain at least one pane leaf"
        );
        let mut level = (0..slots)
            .map(|offset| {
                if offset < empty_slots {
                    PaneNode::Empty
                } else {
                    PaneNode::Leaf(sample_pane_entry(ordinal_base.saturating_add(offset)))
                }
            })
            .collect::<Vec<_>>();
        while level.len() > 1 {
            let mut next = Vec::with_capacity(level.len().div_ceil(2));
            let mut current = level.into_iter();
            while let Some(left) = current.next() {
                if let Some(right) = current.next() {
                    next.push(PaneNode::Split {
                        left: Box::new(left),
                        right: Box::new(right),
                        node: sample_split(),
                    });
                } else {
                    next.push(left);
                }
            }
            level = next;
        }
        level.pop().expect("positive slot count produces one root")
    }

    fn sample_flat_wire() -> OrderedPanesFlatWireOwned {
        OrderedPanesFlatWireOwned {
            trees: vec![PaneArenaTree {
                root_index: Some(0),
                node_count: 3,
                tab_title: "tab-1".to_string(),
            }],
            nodes: vec![
                PaneArenaNode::Split {
                    left: 1,
                    right: 2,
                    node: sample_split(),
                },
                PaneArenaNode::Leaf(sample_pane_entry(0)),
                PaneArenaNode::Leaf(sample_pane_entry(1)),
            ],
            window_titles: vec![PaneArenaWindowTitle {
                window_id: 1,
                title: "window-1".to_string(),
            }],
        }
    }

    fn sample_flat_tree() -> PaneArena {
        let wire = sample_flat_wire();
        PaneArena::from_unvalidated_parts(wire.trees, wire.nodes, wire.window_titles)
    }

    struct OrderedPanesTestWire<'a>(&'a PaneArena);

    impl Serialize for OrderedPanesTestWire<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serialize_ordered_panes(self.0, serializer)
        }
    }

    struct OrderedPanesTestOwned(PaneArena);

    impl<'de> Deserialize<'de> for OrderedPanesTestOwned {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserialize_ordered_panes(deserializer).map(Self)
        }
    }

    struct OrderedWindowSectionTestWire<'a>(&'a [OrderedWindowStateV1]);

    impl Serialize for OrderedWindowSectionTestWire<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serialize_ordered_window_section(self.0, serializer)
        }
    }

    struct DeclaredEmptySequence(usize);

    impl Serialize for DeclaredEmptySequence {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let sequence = serializer.serialize_seq(Some(self.0))?;
            serde::ser::SerializeSeq::end(sequence)
        }
    }

    struct DuplicateWindowTitleSequence;

    impl Serialize for DuplicateWindowTitleSequence {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut sequence = serializer.serialize_seq(Some(2))?;
            serde::ser::SerializeSeq::serialize_element(
                &mut sequence,
                &PaneArenaWindowTitle {
                    window_id: 7,
                    title: "first".to_string(),
                },
            )?;
            serde::ser::SerializeSeq::serialize_element(
                &mut sequence,
                &PaneArenaWindowTitle {
                    window_id: 7,
                    title: "second".to_string(),
                },
            )?;
            serde::ser::SerializeSeq::end(sequence)
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum HostileOrderedSnapshotField {
        PaneTreeDescriptors,
        PaneArenaNodes,
        PaneWindowTitles,
        DuplicatePaneWindowTitle,
        DuplicatePaneChildIndex,
        PaneBackEdge,
        PaneChildOutOfRange,
        TrailingPaneArenaNode,
        OrderedSectionBytes,
        OrderedWindows,
        OrderedTabIds,
    }

    struct HostileOrderedPanes(HostileOrderedSnapshotField);

    impl Serialize for HostileOrderedPanes {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            if matches!(
                self.0,
                HostileOrderedSnapshotField::DuplicatePaneChildIndex
                    | HostileOrderedSnapshotField::PaneBackEdge
                    | HostileOrderedSnapshotField::PaneChildOutOfRange
                    | HostileOrderedSnapshotField::TrailingPaneArenaNode
            ) {
                let mut panes = sample_flat_wire();
                match self.0 {
                    HostileOrderedSnapshotField::DuplicatePaneChildIndex => {
                        let PaneArenaNode::Split { right, .. } = &mut panes.nodes[0] else {
                            unreachable!("hostile control root is split");
                        };
                        *right = 1;
                    }
                    HostileOrderedSnapshotField::PaneBackEdge => {
                        let PaneArenaNode::Split { right, .. } = &mut panes.nodes[0] else {
                            unreachable!("hostile control root is split");
                        };
                        *right = 0;
                    }
                    HostileOrderedSnapshotField::PaneChildOutOfRange => {
                        let PaneArenaNode::Split { right, .. } = &mut panes.nodes[0] else {
                            unreachable!("hostile control root is split");
                        };
                        *right = 3;
                    }
                    HostileOrderedSnapshotField::TrailingPaneArenaNode => {
                        panes.nodes.push(PaneArenaNode::Leaf(sample_pane_entry(2)))
                    }
                    _ => unreachable!("malformed pane variant was exhaustively matched"),
                }
                return panes.serialize(serializer);
            }
            let mut state = serializer.serialize_struct("ListPanesResponse", 3)?;
            let empty: &[u8] = &[];

            if self.0 == HostileOrderedSnapshotField::PaneTreeDescriptors {
                serde::ser::SerializeStruct::serialize_field(
                    &mut state,
                    "trees",
                    &DeclaredEmptySequence(MAX_ORDERED_TABS_PER_SNAPSHOT + 1),
                )?;
            } else {
                serde::ser::SerializeStruct::serialize_field(&mut state, "trees", empty)?;
            }
            if self.0 == HostileOrderedSnapshotField::PaneArenaNodes {
                serde::ser::SerializeStruct::serialize_field(
                    &mut state,
                    "nodes",
                    &DeclaredEmptySequence(MAX_ORDERED_PANE_NODES_PER_SNAPSHOT + 1),
                )?;
            } else {
                serde::ser::SerializeStruct::serialize_field(&mut state, "nodes", empty)?;
            }
            match self.0 {
                HostileOrderedSnapshotField::PaneWindowTitles => {
                    serde::ser::SerializeStruct::serialize_field(
                        &mut state,
                        "window_titles",
                        &DeclaredEmptySequence(MAX_ORDERED_WINDOWS_PER_SNAPSHOT + 1),
                    )?;
                }
                HostileOrderedSnapshotField::DuplicatePaneWindowTitle => {
                    serde::ser::SerializeStruct::serialize_field(
                        &mut state,
                        "window_titles",
                        &DuplicateWindowTitleSequence,
                    )?;
                }
                _ => {
                    serde::ser::SerializeStruct::serialize_field(
                        &mut state,
                        "window_titles",
                        empty,
                    )?;
                }
            }
            serde::ser::SerializeStruct::end(state)
        }
    }

    struct OrderedSectionBytes<'a>(&'a [u8]);

    impl Serialize for OrderedSectionBytes<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serializer.serialize_bytes(self.0)
        }
    }

    struct EmptyFloatingPaneSnapshot;

    impl Serialize for EmptyFloatingPaneSnapshot {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            serialize_floating_pane_snapshot(&[], serializer)
        }
    }

    #[derive(Serialize)]
    struct UncheckedOrderedWindowWithDeclaredTabs {
        window_id: RemoteWindowId,
        order_revision: WindowOrderRevision,
        ordered_tab_ids: DeclaredEmptySequence,
        active_tab_id: Option<RemoteTabId>,
    }

    struct HostileOrderedPaneSnapshot {
        field: HostileOrderedSnapshotField,
        ordered_section: Vec<u8>,
    }

    impl Serialize for HostileOrderedPaneSnapshot {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: serde::Serializer,
        {
            let mut state = serializer.serialize_struct("OrderedPaneSnapshotV1", 5)?;
            serde::ser::SerializeStruct::serialize_field(
                &mut state,
                "session_incarnation",
                &MuxSessionIncarnation::from_bytes([0x91; 16]),
            )?;
            serde::ser::SerializeStruct::serialize_field(
                &mut state,
                "topology_revision",
                &TopologyRevision::new(1),
            )?;
            serde::ser::SerializeStruct::serialize_field(
                &mut state,
                "panes",
                &HostileOrderedPanes(self.field),
            )?;
            serde::ser::SerializeStruct::serialize_field(
                &mut state,
                "floating_panes",
                &EmptyFloatingPaneSnapshot,
            )?;
            if self.field == HostileOrderedSnapshotField::OrderedSectionBytes {
                serde::ser::SerializeStruct::serialize_field(
                    &mut state,
                    "ordered_windows",
                    &DeclaredEmptySequence(MAX_ORDERED_WINDOW_SECTION_BYTES + 1),
                )?;
            } else {
                serde::ser::SerializeStruct::serialize_field(
                    &mut state,
                    "ordered_windows",
                    &OrderedSectionBytes(&self.ordered_section),
                )?;
            }
            serde::ser::SerializeStruct::end(state)
        }
    }

    #[derive(Serialize)]
    enum HostileListPanesOrderedV1Outcome {
        Snapshot(HostileOrderedPaneSnapshot),
    }

    #[derive(Serialize)]
    struct HostileListPanesOrderedV1Response {
        protocol_version: u16,
        domain_binding_id: DomainBindingId,
        negotiated: TopologyCapabilities,
        stream_id: TopologyStreamId,
        outcome: HostileListPanesOrderedV1Outcome,
    }

    fn hostile_ordered_snapshot_response_body(field: HostileOrderedSnapshotField) -> Vec<u8> {
        let ordered_section = match field {
            HostileOrderedSnapshotField::OrderedWindows => {
                serialize_uncompressed(&DeclaredEmptySequence(MAX_ORDERED_WINDOWS_PER_SNAPSHOT + 1))
                    .expect("encode prefix-only hostile ordered-window collection")
            }
            HostileOrderedSnapshotField::OrderedTabIds => {
                serialize_uncompressed(&vec![UncheckedOrderedWindowWithDeclaredTabs {
                    window_id: RemoteWindowId::new(1),
                    order_revision: WindowOrderRevision::INITIAL,
                    ordered_tab_ids: DeclaredEmptySequence(MAX_ORDERED_TABS_PER_WINDOW + 1),
                    active_tab_id: None,
                }])
                .expect("encode prefix-only hostile ordered-tab collection")
            }
            _ => serialize_uncompressed(&Vec::<OrderedWindowStateV1>::new())
                .expect("encode legal empty ordered-window section"),
        };
        serialize_uncompressed(&HostileListPanesOrderedV1Response {
            protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: DomainBindingId::from_bytes([0x92; 16]),
            negotiated: ordered_window_foundation_capabilities(),
            stream_id: TopologyStreamId::from_bytes([0x93; 16]),
            outcome: HostileListPanesOrderedV1Outcome::Snapshot(HostileOrderedPaneSnapshot {
                field,
                ordered_section,
            }),
        })
        .expect("encode unchecked hostile PDU 87 body")
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

    fn encode_reorder_window_tabs_unchecked(request: &ReorderWindowTabsV1, serial: u64) -> Vec<u8> {
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
                    domain_binding_id: reorder.domain_binding_id,
                    supported: ordered_window_all_capabilities(),
                    required: ordered_window_foundation_capabilities(),
                }),
            ),
            (
                87,
                Pdu::ListPanesOrderedV1Response(ListPanesOrderedV1Response {
                    protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
                    domain_binding_id: reorder.domain_binding_id,
                    negotiated: ordered_window_foundation_capabilities(),
                    stream_id: reorder.stream_id,
                    outcome: ListPanesOrderedV1Outcome::Snapshot(OrderedPaneSnapshotV1 {
                        session_incarnation: reorder.session_incarnation,
                        topology_revision: TopologyRevision::new(11),
                        panes: ordered_pane_arena_from_list_panes(empty_pane_list())
                            .expect("empty ordered-pane arena must be valid"),
                        floating_panes: Vec::new(),
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

    fn exact_render_connection() -> RenderConnectionIdentity {
        RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0xa1; 16]),
            MuxSessionIncarnation::from_bytes([0xb2; 16]),
        )
    }

    fn exact_render_cursor(sequence: u64) -> ExactRenderDeliveryCursor {
        ExactRenderDeliveryCursor {
            pane_generation: ExactRenderPaneGeneration::try_new(1)
                .expect("sample pane generation is nonzero"),
            delivery_generation: ExactRenderDeliveryGeneration::try_new(1)
                .expect("sample delivery generation is nonzero"),
            sequence: ExactRenderDeliverySequence::try_new(sequence)
                .expect("sample delivery sequence is nonzero"),
        }
    }

    fn exact_render_request_identity(sequence: u64) -> ExactRenderDeliveryRequestIdentity {
        ExactRenderDeliveryRequestIdentity {
            connection_identity: exact_render_connection(),
            pane_id: ExactRenderPaneId::new(42),
            request_sequence: ExactRenderRequestSequence::try_new(sequence)
                .expect("sample request sequence is nonzero"),
        }
    }

    fn sample_exact_render_rows() -> Vec<ExactRenderRowV1> {
        vec![
            ExactRenderRowV1 {
                stable_row: -2,
                text: ExactRenderRowTextV1::try_from_str("wide: 界")
                    .expect("sample row text is bounded UTF-8"),
                wrapped: true,
            },
            ExactRenderRowV1 {
                stable_row: -1,
                text: ExactRenderRowTextV1::try_from_str("combining: e\u{301}")
                    .expect("sample combining row is bounded UTF-8"),
                wrapped: false,
            },
        ]
    }

    fn sample_exact_render_projection() -> ExactRenderProjectionV1 {
        ExactRenderProjectionV1 {
            first_stable_row: -2,
            row_count: 2,
            alt_screen_active: false,
            mouse_grabbed: false,
            cursor_position: ExactRenderCursorPositionV1 {
                x: 7,
                y: -1,
                shape: ExactRenderCursorShapeV1::SteadyBlock,
                visibility: ExactRenderCursorVisibilityV1::Visible,
            },
            dimensions: ExactRenderDimensionsV1 {
                cols: 80,
                viewport_rows: 2,
                scrollback_rows: 2,
                physical_top: -2,
                scrollback_top: -2,
                dpi: 144,
                pixel_width: 1_600,
                pixel_height: 900,
                reverse_video: false,
            },
            title: ExactRenderTitleV1::try_from_str("sample exact render")
                .expect("sample title is bounded UTF-8"),
            working_dir: Some(
                ExactRenderWorkingDirectoryV1::try_from_str("file:///tmp/frankenterm")
                    .expect("sample working directory is bounded UTF-8"),
            ),
        }
    }

    fn sample_exact_render_delta(source_version: u64) -> ExactRenderDeltaV1 {
        let rows = sample_exact_render_rows();
        ExactRenderDeltaV1 {
            delivery: ExactRenderDeliveryToken {
                connection_identity: exact_render_connection(),
                pane_id: ExactRenderPaneId::new(42),
                resulting_baseline: exact_render_cursor(8),
                content_digest: ExactRenderDigest::ZERO,
            },
            base: exact_render_cursor(7),
            source_version,
            resulting_projection: sample_exact_render_projection(),
            patches: vec![ExactRenderRowPatchV1 {
                start_stable_row: -2,
                removed_rows: 2,
                replacement_start: 0,
                replacement_count: 2,
            }],
            rows,
        }
        .with_computed_digest()
        .expect("sample delta digest must compute")
    }

    fn sample_exact_render_request(mode: ExactRenderDeliveryMode) -> GetPaneRenderDeliveryV1 {
        GetPaneRenderDeliveryV1 {
            protocol_version: EXACT_RENDER_DELIVERY_PROTOCOL_VERSION,
            identity: exact_render_request_identity(1),
            request_digest: ExactRenderDigest::ZERO,
            applied_baseline: ExactRenderAppliedBaseline::Applied(exact_render_cursor(7)),
            settlement: None,
            mode,
            receiver_caps: ExactRenderReceiverCaps::protocol_maximum(),
            continuation: None,
        }
        .with_computed_request_digest()
        .expect("sample request digest must compute")
    }

    fn sample_exact_render_manifest_and_chunks() -> (
        Vec<ExactRenderRowV1>,
        ExactRenderSnapshotManifestV1,
        ExactRenderSnapshotChunkV1,
        ExactRenderSnapshotChunkV1,
    ) {
        let rows = sample_exact_render_rows();
        let total_text_bytes = exact_render_rows_usage(&rows)
            .expect("sample rows have bounded usage")
            .text_bytes;
        let manifest = ExactRenderSnapshotManifestV1 {
            snapshot: ExactRenderDeliveryToken {
                connection_identity: exact_render_connection(),
                pane_id: ExactRenderPaneId::new(42),
                resulting_baseline: exact_render_cursor(9),
                content_digest: ExactRenderDigest::ZERO,
            },
            source_version: 500,
            projection: sample_exact_render_projection(),
            total_rows: 2,
            total_text_bytes,
            chunk_count: 2,
        }
        .with_computed_content_digest(&rows)
        .expect("sample snapshot content digest must compute");
        let first_text_bytes =
            u64::try_from(rows[0].text.len()).expect("sample row text length fits u64");
        let first = ExactRenderSnapshotChunkV1 {
            source_version: manifest.source_version,
            ordinal: 0,
            first_row_ordinal: 0,
            first_text_byte: 0,
            rows: vec![rows[0].clone()],
            chunk_digest: ExactRenderDigest::ZERO,
        }
        .with_computed_digest(&manifest)
        .expect("first sample chunk digest must compute");
        let second = ExactRenderSnapshotChunkV1 {
            source_version: manifest.source_version,
            ordinal: 1,
            first_row_ordinal: 1,
            first_text_byte: first_text_bytes,
            rows: vec![rows[1].clone()],
            chunk_digest: ExactRenderDigest::ZERO,
        }
        .with_computed_digest(&manifest)
        .expect("second sample chunk digest must compute");
        (rows, manifest, first, second)
    }

    fn sample_exact_render_response(
        outcome: ExactRenderDeliveryOutcomeV1,
    ) -> GetPaneRenderDeliveryV1Response {
        let request = sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        sample_exact_render_response_for(&request, outcome)
    }

    fn sample_exact_render_response_for(
        request: &GetPaneRenderDeliveryV1,
        outcome: ExactRenderDeliveryOutcomeV1,
    ) -> GetPaneRenderDeliveryV1Response {
        GetPaneRenderDeliveryV1Response {
            protocol_version: EXACT_RENDER_DELIVERY_PROTOCOL_VERSION,
            request_identity: request.identity,
            request_digest: request.request_digest,
            outcome,
        }
    }

    #[test]
    fn exact_render_delivery_v1_is_known_but_not_runtime_advertised() {
        let exact_render = TopologyCapabilities::from_bits(
            TopologyCapabilities::FENCED_SNAPSHOT_V1.bits()
                | TopologyCapabilities::EXACT_RENDER_DELIVERY_V1.bits(),
        );
        assert_eq!(
            TopologyCapabilities::EXACT_RENDER_DELIVERY_V1.bits(),
            1 << 3
        );
        assert!(exact_render.validate().is_ok());
        assert_eq!(
            TopologyCapabilities::EXACT_RENDER_DELIVERY_V1.validate(),
            Err(
                TopologyCapabilitiesError::ExactRenderDeliveryWithoutFencedSnapshot {
                    bits: 1 << 3,
                }
            )
        );
        assert!(!TopologyCapabilities::SERVER_SUPPORTED
            .contains(TopologyCapabilities::EXACT_RENDER_DELIVERY_V1));
        assert_eq!(EXACT_RENDER_DELIVERY_V1_MIN_CODEC_VERSION, 52);
        assert!(!codec_version_supports_exact_render_delivery_v1(51));
        assert!(codec_version_supports_exact_render_delivery_v1(52));

        let request_spec =
            Pdu::wire_spec_for_ident(91).expect("exact render request ID must be assigned");
        assert_eq!(request_spec.min_codec_version, 52);
        assert!(request_spec.authorizes(PduProducer::Client, PduWireRole::Request));
        assert!(!request_spec.authorizes(PduProducer::Server, PduWireRole::CorrelatedReply));
        assert_eq!(
            request_spec.capability,
            PduCapabilityUse::Requires(exact_render)
        );
        let request_body_limit = PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_EXACT_RENDER_REQUEST_ZSTD_ENCODED_BYTES,
        };
        assert_eq!(request_spec.encoded_body_limit, request_body_limit);
        assert_eq!(
            request_spec
                .encoded_body_limit
                .maximum_encoded_payload_bytes(false),
            MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES,
        );
        assert_eq!(
            request_spec
                .encoded_body_limit
                .maximum_encoded_payload_bytes(true),
            MAX_EXACT_RENDER_REQUEST_ZSTD_ENCODED_BYTES,
        );
        assert_eq!(
            MAX_EXACT_RENDER_REQUEST_ZSTD_ENCODED_BYTES,
            zstd::zstd_safe::compress_bound(MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES),
            "frozen request ceiling must continue to admit the pinned zstd encoder bound",
        );
        assert!(
            request_spec
                .encoded_body_limit
                .maximum_encoded_payload_bytes(true)
                > MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES
        );

        let response_spec =
            Pdu::wire_spec_for_ident(92).expect("exact render response ID must be assigned");
        assert_eq!(response_spec.min_codec_version, 52);
        assert!(response_spec.authorizes(PduProducer::Server, PduWireRole::CorrelatedReply));
        assert!(!response_spec.authorizes(PduProducer::Server, PduWireRole::Unilateral));
        assert_eq!(
            response_spec.capability,
            PduCapabilityUse::Requires(exact_render)
        );
        let response_body_limit = PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_EXACT_RENDER_DELIVERY_ZSTD_ENCODED_BYTES,
        };
        assert_eq!(response_spec.encoded_body_limit, response_body_limit);
        assert!(
            request_spec
                .encoded_body_limit
                .maximum_encoded_payload_bytes(false)
                < response_spec
                    .encoded_body_limit
                    .maximum_encoded_payload_bytes(false),
            "compact PDU 91 must not inherit PDU 92's multi-megabyte body cap",
        );
        assert_eq!(
            MAX_EXACT_RENDER_DELIVERY_ZSTD_ENCODED_BYTES,
            zstd::zstd_safe::compress_bound(MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES),
            "frozen response ceiling must continue to admit the pinned zstd encoder bound",
        );
        assert_eq!(
            Pdu::wire_spec_for_ident(1)
                .expect("legacy ping ID must be assigned")
                .encoded_body_limit,
            PduEncodedBodyLimit::GlobalMaximum,
        );
        assert_eq!(
            Pdu::GetPaneRenderDeliveryV1(sample_exact_render_request(
                ExactRenderDeliveryMode::Incremental
            ))
            .pane_id(),
            Some(42)
        );
        assert_eq!(
            Pdu::GetPaneRenderDeliveryV1Response(sample_exact_render_response(
                ExactRenderDeliveryOutcomeV1::NoChange {
                    current: exact_render_cursor(7),
                    source_version: 1,
                }
            ))
            .pane_id(),
            Some(42)
        );
    }

    #[test]
    fn exact_render_encoded_body_cap_precedes_sync_and_async_allocation() {
        for ident in [91, 92] {
            let spec = Pdu::wire_spec_for_ident(ident).expect("exact render ID must be assigned");
            for is_compressed in [false, true] {
                let limit = spec
                    .encoded_body_limit
                    .maximum_encoded_payload_bytes(is_compressed);

                let exact_header = declared_pdu_frame_header(ident, 17, limit, is_compressed);
                let exact_error = decode_raw(exact_header.as_slice())
                    .expect_err("admitted header without its declared body must reach body read");
                assert!(
                    exact_error
                        .downcast_ref::<PduEncodedBodyLimitExceeded>()
                        .is_none(),
                    "exact boundary was rejected as oversized: {exact_error:#}",
                    exact_error = exact_error,
                );

                let plus = limit.checked_add(1).expect("exact body limit has headroom");
                let plus_header = declared_pdu_frame_header(ident, 17, plus, is_compressed);
                let plus_error = decode_raw(plus_header.as_slice())
                    .expect_err("limit-plus-one header must fail before body allocation");
                let exceeded = plus_error
                    .downcast_ref::<PduEncodedBodyLimitExceeded>()
                    .expect("sync decoder must return the typed schema-body limit error");
                assert_eq!(exceeded.declared_payload_bytes(), plus);
                assert_eq!(exceeded.max_payload_bytes(), limit);
                assert_eq!(exceeded.serial(), 17);
                assert_eq!(exceeded.ident(), ident);
                assert_eq!(exceeded.is_compressed(), is_compressed);

                runtime::block_on(async {
                    let mut exact_reader = runtime::Cursor::new(exact_header);
                    let header = decode_raw_header_async(&mut exact_reader, Some(17))
                        .await
                        .expect("async header decoder must admit the exact boundary");
                    assert_eq!(header.encoded_payload_len(), limit);
                    assert_eq!(header.maximum_encoded_payload_bytes(), limit);

                    let mut plus_reader = runtime::Cursor::new(plus_header);
                    let plus_error = decode_raw_header_async(&mut plus_reader, Some(17))
                        .await
                        .expect_err("async limit-plus-one header must fail before body allocation");
                    let exceeded = plus_error
                        .downcast_ref::<PduEncodedBodyLimitExceeded>()
                        .expect("async decoder must return the typed schema-body limit error");
                    assert_eq!(exceeded.declared_payload_bytes(), plus);
                    assert_eq!(exceeded.max_payload_bytes(), limit);
                    assert_eq!(exceeded.serial(), 17);
                    assert_eq!(exceeded.ident(), ident);
                    assert_eq!(exceeded.is_compressed(), is_compressed);
                });
            }
        }
    }

    #[test]
    fn exact_render_encode_serializes_once_after_structural_validation() {
        let response = sample_exact_render_response(ExactRenderDeliveryOutcomeV1::NoChange {
            current: exact_render_cursor(7),
            source_version: 77,
        });
        reset_test_serialize_invocations();
        response
            .validate()
            .expect("structural response validation must succeed");
        assert_eq!(
            test_serialize_invocations(),
            0,
            "pre-encode structural validation must not perform a counting serialization",
        );
        response
            .resource_usage()
            .expect("explicit resource accounting must remain measurable");
        assert_eq!(
            test_serialize_invocations(),
            1,
            "explicit byte accounting must perform exactly one counting traversal",
        );
        reset_test_serialize_invocations();

        for mode in [
            CompressionMode::Auto,
            CompressionMode::Always,
            CompressionMode::Never,
        ] {
            let before = test_serialize_invocations();
            Pdu::GetPaneRenderDeliveryV1Response(response.clone())
                .encode_frame_with_mode(31, mode)
                .expect("valid exact-render response must encode");
            assert_eq!(
                test_serialize_invocations(),
                before + 1,
                "one frame encode must perform exactly one serde traversal in {mode:?}",
            );
        }
    }

    #[test]
    fn streaming_exact_render_body_cap_rejects_limit_plus_one_without_consumption() {
        for ident in [91, 92] {
            let spec = Pdu::wire_spec_for_ident(ident).expect("exact render ID must be assigned");
            for is_compressed in [false, true] {
                let limit = spec
                    .encoded_body_limit
                    .maximum_encoded_payload_bytes(is_compressed);
                let exact_header = declared_pdu_frame_header(ident, 23, limit, is_compressed);
                let mut exact_buffer = StreamingPduBuffer::from(exact_header.clone());
                assert_eq!(
                    Pdu::stream_decode(&mut exact_buffer)
                        .expect("exact boundary declaration is admissible"),
                    None,
                );
                assert_eq!(exact_buffer.as_slice(), exact_header.as_slice());

                let plus = limit.checked_add(1).expect("exact body limit has headroom");
                let plus_header = declared_pdu_frame_header(ident, 23, plus, is_compressed);
                let mut plus_buffer = StreamingPduBuffer::from(plus_header.clone());
                let error = Pdu::stream_decode(&mut plus_buffer)
                    .expect_err("limit-plus-one stream must fail before body accumulation");
                let exceeded = error
                    .downcast_ref::<PduEncodedBodyLimitExceeded>()
                    .expect("stream decoder must return the typed schema-body limit error");
                assert_eq!(exceeded.declared_payload_bytes(), plus);
                assert_eq!(exceeded.max_payload_bytes(), limit);
                assert_eq!(exceeded.serial(), 23);
                assert_eq!(exceeded.ident(), ident);
                assert_eq!(exceeded.is_compressed(), is_compressed);
                assert_eq!(plus_buffer.as_slice(), plus_header.as_slice());
            }
        }
    }

    #[test]
    fn streaming_header_parser_waits_for_serial_and_ident_without_consumption() {
        let mut buffer = StreamingPduBuffer::new();
        let mut tagged_len = Vec::new();
        leb128::write::unsigned(&mut tagged_len, 4).expect("encode test frame body length");
        buffer.extend_from_slice(&tagged_len);
        assert_eq!(
            Pdu::stream_decode(&mut buffer).expect("length prefix"),
            None
        );
        assert_eq!(buffer.as_slice(), tagged_len.as_slice());

        for byte in [0x80, 0x01, 0x81] {
            buffer.extend_from_slice(&[byte]);
            let before = buffer.as_slice().to_vec();
            assert_eq!(
                Pdu::stream_decode(&mut buffer).expect("incomplete header remains pending"),
                None,
            );
            assert_eq!(buffer.as_slice(), before.as_slice());
        }

        buffer.extend_from_slice(&[0x01]);
        let decoded = Pdu::stream_decode(&mut buffer)
            .expect("complete unknown-PDU header must decode")
            .expect("complete unknown-PDU frame must be present");
        assert_eq!(decoded.serial, 128);
        assert_eq!(decoded.pdu, Pdu::Invalid { ident: 129 });
        assert!(buffer.is_empty());
    }

    #[test]
    fn exact_render_delivery_v1_roundtrips_every_closed_outcome_in_every_mode() {
        assert_eq!(<GetPaneRenderDeliveryV1 as PduWireIdent>::IDENT, 91);
        assert_eq!(<GetPaneRenderDeliveryV1Response as PduWireIdent>::IDENT, 92);
        let (rows, manifest, first_chunk, _) = sample_exact_render_manifest_and_chunks();
        manifest
            .validate_complete_rows(&rows)
            .expect("sample manifest must bind the complete row sequence");
        let old_requested = ExactRenderDeliveryCursor {
            sequence: ExactRenderDeliverySequence::try_new(1).expect("nonzero"),
            ..exact_render_cursor(7)
        };
        let oldest = ExactRenderDeliveryCursor {
            sequence: ExactRenderDeliverySequence::try_new(2).expect("nonzero"),
            ..exact_render_cursor(7)
        };
        let outcomes = vec![
            ExactRenderDeliveryOutcomeV1::NoChange {
                current: exact_render_cursor(7),
                source_version: 900,
            },
            ExactRenderDeliveryOutcomeV1::ExactDelta(sample_exact_render_delta(901)),
            ExactRenderDeliveryOutcomeV1::FullManifest(manifest.clone()),
            ExactRenderDeliveryOutcomeV1::FullChunk {
                manifest: manifest.clone(),
                chunk: first_chunk,
            },
            ExactRenderDeliveryOutcomeV1::BaselineTooOld {
                requested: old_requested,
                oldest_available: oldest,
                current: exact_render_cursor(7),
            },
            ExactRenderDeliveryOutcomeV1::GenerationChanged {
                requested: exact_render_cursor(7),
                current_pane_generation: ExactRenderPaneGeneration::try_new(2).expect("nonzero"),
                current_delivery_generation: ExactRenderDeliveryGeneration::try_new(1)
                    .expect("nonzero"),
            },
            ExactRenderDeliveryOutcomeV1::PaneRemoved {
                last_pane_generation: Some(ExactRenderPaneGeneration::try_new(1).expect("nonzero")),
            },
            ExactRenderDeliveryOutcomeV1::AuthorityExhausted {
                authority: ExactRenderAuthority::DeliverySequence,
            },
            ExactRenderDeliveryOutcomeV1::LimitsExceeded {
                resource: ExactRenderLimitResource::Rows,
                required: 2,
                limit: 1,
            },
        ];

        let request = Pdu::GetPaneRenderDeliveryV1(sample_exact_render_request(
            ExactRenderDeliveryMode::Incremental,
        ));
        for mode in [
            CompressionMode::Auto,
            CompressionMode::Never,
            CompressionMode::Always,
        ] {
            let frame = request
                .encode_frame_with_mode(0x51, mode)
                .expect("exact render request must encode");
            assert_eq!(
                Pdu::decode(frame.as_slice())
                    .expect("exact render request must decode")
                    .pdu,
                request
            );
            for outcome in &outcomes {
                let pdu = Pdu::GetPaneRenderDeliveryV1Response(sample_exact_render_response(
                    outcome.clone(),
                ));
                let frame = pdu
                    .encode_frame_with_mode(0x52, mode)
                    .expect("closed exact render outcome must encode");
                assert_eq!(
                    Pdu::decode(frame.as_slice())
                        .expect("closed exact render outcome must decode")
                        .pdu,
                    pdu
                );
            }
        }
    }

    #[test]
    fn exact_render_delivery_authorities_never_wrap_and_pane_ids_are_fixed_width() {
        assert!(ExactRenderPaneGeneration::try_new(0).is_err());
        assert!(ExactRenderDeliveryGeneration::try_new(0).is_err());
        assert!(ExactRenderDeliverySequence::try_new(0).is_err());
        assert!(ExactRenderRequestSequence::try_new(0).is_err());
        assert_eq!(ExactRenderPaneId::new(0).try_into_mux(), Ok(0));
        let mut pane_zero_request =
            sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        pane_zero_request.identity.pane_id = ExactRenderPaneId::new(0);
        pane_zero_request = pane_zero_request
            .with_computed_request_digest()
            .expect("pane-zero request digest must compute");
        pane_zero_request
            .validate()
            .expect("mux pane zero is a valid exact-render authority");
        let pane_id =
            ExactRenderPaneId::try_from_mux(42).expect("local pane id fits fixed wire id");
        assert_eq!(pane_id.get(), 42);
        assert_eq!(pane_id.try_into_mux(), Ok(42));
        let widest = ExactRenderPaneId::new(u64::MAX);
        if usize::BITS < 64 {
            assert_eq!(
                widest.try_into_mux(),
                Err(ExactRenderDeliveryProtocolError::PaneIdOutOfRange)
            );
        } else {
            assert_eq!(widest.try_into_mux(), Ok(usize::MAX));
        }
        let mut zero_connection = sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        zero_connection.identity.connection_identity = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0; 16]),
            MuxSessionIncarnation::from_bytes([0; 16]),
        );
        assert_eq!(
            zero_connection.validate(),
            Err(ExactRenderDeliveryProtocolError::ReservedConnectionIdentity)
        );

        assert_eq!(
            ExactRenderPaneGeneration::try_new(u64::MAX)
                .expect("maximum generation is the final usable identity")
                .checked_next(),
            Err(ExactRenderDeliveryProtocolError::AuthorityExhausted {
                field: "pane_generation",
            })
        );
        assert_eq!(
            ExactRenderDeliveryGeneration::try_new(u64::MAX)
                .expect("maximum generation is the final usable identity")
                .checked_next(),
            Err(ExactRenderDeliveryProtocolError::AuthorityExhausted {
                field: "delivery_generation",
            })
        );
        assert_eq!(
            ExactRenderDeliverySequence::try_new(u64::MAX)
                .expect("maximum sequence is the final usable identity")
                .checked_next(),
            Err(ExactRenderDeliveryProtocolError::AuthorityExhausted {
                field: "delivery_sequence",
            })
        );
        assert_eq!(
            ExactRenderRequestSequence::try_new(u64::MAX)
                .expect("maximum request is the final usable identity")
                .checked_next(),
            Err(ExactRenderDeliveryProtocolError::AuthorityExhausted {
                field: "request_sequence",
            })
        );
        assert_eq!(
            exact_render_cursor(7)
                .checked_next()
                .expect("7 advances")
                .sequence
                .get(),
            8
        );

        let mut uninitialized_incremental =
            sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        uninitialized_incremental.applied_baseline = ExactRenderAppliedBaseline::Uninitialized;
        assert_eq!(
            uninitialized_incremental.validate(),
            Err(ExactRenderDeliveryProtocolError::IncrementalRequiresBaseline)
        );
        uninitialized_incremental.mode = ExactRenderDeliveryMode::ForceFull;
        uninitialized_incremental = uninitialized_incremental
            .with_computed_request_digest()
            .expect("force-full bootstrap request digest must compute");
        uninitialized_incremental
            .validate()
            .expect("an uninitialized client must bootstrap through ForceFull");
    }

    #[test]
    fn exact_render_request_digest_has_frozen_architecture_independent_preimage() {
        const REQUEST_GOLDEN: ExactRenderDigest = ExactRenderDigest::from_bytes([
            0x59, 0x80, 0x36, 0x25, 0x12, 0xf4, 0xe3, 0x2d, 0x18, 0x82, 0xb7, 0x66, 0xb5, 0x7d,
            0x62, 0x1a, 0x1f, 0x14, 0x6b, 0x40, 0x14, 0x41, 0x62, 0xd5, 0xf8, 0xd5, 0xc3, 0xc2,
            0x43, 0xb3, 0x45, 0xe4,
        ]);
        let connection = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0x01; 16]),
            MuxSessionIncarnation::from_bytes([0x02; 16]),
        );
        let cursor = |sequence| ExactRenderDeliveryCursor {
            pane_generation: ExactRenderPaneGeneration::try_new(4).expect("nonzero"),
            delivery_generation: ExactRenderDeliveryGeneration::try_new(5).expect("nonzero"),
            sequence: ExactRenderDeliverySequence::try_new(sequence).expect("nonzero"),
        };
        let request = GetPaneRenderDeliveryV1 {
            protocol_version: EXACT_RENDER_DELIVERY_PROTOCOL_VERSION,
            identity: ExactRenderDeliveryRequestIdentity {
                connection_identity: connection,
                pane_id: ExactRenderPaneId::new(3),
                request_sequence: ExactRenderRequestSequence::try_new(9).expect("nonzero"),
            },
            request_digest: ExactRenderDigest::ZERO,
            applied_baseline: ExactRenderAppliedBaseline::Applied(cursor(6)),
            settlement: Some(ExactRenderDeliverySettlement {
                delivery: ExactRenderDeliveryToken {
                    connection_identity: connection,
                    pane_id: ExactRenderPaneId::new(3),
                    resulting_baseline: cursor(7),
                    content_digest: ExactRenderDigest::from_bytes([0xaa; 32]),
                },
                outcome: ExactRenderDeliverySettlementOutcome::Nack {
                    reason: ExactRenderDeliveryNackReason::SnapshotCorrupt,
                    observed_baseline: ExactRenderAppliedBaseline::Applied(cursor(6)),
                },
            }),
            mode: ExactRenderDeliveryMode::Incremental,
            receiver_caps: ExactRenderReceiverCaps {
                max_decompressed_bytes: 4_096,
                max_text_bytes: 123,
                max_rows: 4,
                max_snapshot_text_bytes: 999,
                max_snapshot_rows: 20,
                max_snapshot_chunks: 5,
            },
            continuation: None,
        }
        .with_computed_request_digest()
        .expect("golden request digest must compute");
        request
            .validate()
            .expect("golden request must be self-consistent");
        assert_eq!(request.request_digest, REQUEST_GOLDEN);

        let mut preimage = EXACT_RENDER_REQUEST_DIGEST_DOMAIN_V1.to_vec();
        preimage.extend_from_slice(&1_u16.to_be_bytes());
        preimage.extend_from_slice(&[0x01; 16]);
        preimage.extend_from_slice(&[0x02; 16]);
        preimage.extend_from_slice(&3_u64.to_be_bytes());
        preimage.extend_from_slice(&9_u64.to_be_bytes());
        preimage.push(1);
        for value in [4_u64, 5, 6] {
            preimage.extend_from_slice(&value.to_be_bytes());
        }
        preimage.push(1);
        preimage.extend_from_slice(&[0x01; 16]);
        preimage.extend_from_slice(&[0x02; 16]);
        preimage.extend_from_slice(&3_u64.to_be_bytes());
        for value in [4_u64, 5, 7] {
            preimage.extend_from_slice(&value.to_be_bytes());
        }
        preimage.extend_from_slice(&[0xaa; 32]);
        preimage.extend_from_slice(&[1, 2, 1]);
        for value in [4_u64, 5, 6] {
            preimage.extend_from_slice(&value.to_be_bytes());
        }
        preimage.push(0);
        for value in [4_096_u64, 123, 4, 999, 20, 5] {
            preimage.extend_from_slice(&value.to_be_bytes());
        }
        preimage.push(0);
        assert_eq!(preimage.len(), 285);
        assert_eq!(
            ExactRenderDigest::from_bytes(Sha256::digest(&preimage).into()),
            REQUEST_GOLDEN,
        );
    }

    #[test]
    fn exact_render_request_digest_binds_identity_body_and_response_echo() {
        fn assert_digest_changes(
            base: &GetPaneRenderDeliveryV1,
            field: &str,
            mutate: impl FnOnce(&mut GetPaneRenderDeliveryV1),
        ) {
            let mut changed = base.clone();
            mutate(&mut changed);
            assert_ne!(
                changed
                    .canonical_request_digest()
                    .expect("mutated request digest must compute"),
                base.request_digest,
                "request digest omitted {field}",
            );
        }

        let base = sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        assert_digest_changes(&base, "protocol_version", |request| {
            request.protocol_version += 1;
        });
        assert_digest_changes(&base, "stream_id", |request| {
            request.identity.connection_identity.stream_id =
                TopologyStreamId::from_bytes([0xc1; 16]);
        });
        assert_digest_changes(&base, "session_incarnation", |request| {
            request.identity.connection_identity.session_incarnation =
                MuxSessionIncarnation::from_bytes([0xc2; 16]);
        });
        assert_digest_changes(&base, "pane_id", |request| {
            request.identity.pane_id = ExactRenderPaneId::new(43);
        });
        assert_digest_changes(&base, "request_sequence", |request| {
            request.identity.request_sequence =
                ExactRenderRequestSequence::try_new(2).expect("nonzero");
        });
        assert_digest_changes(&base, "applied_baseline", |request| {
            request.applied_baseline = ExactRenderAppliedBaseline::Uninitialized;
        });
        assert_digest_changes(&base, "settlement", |request| {
            request.settlement = Some(ExactRenderDeliverySettlement {
                delivery: sample_exact_render_delta(44).delivery,
                outcome: ExactRenderDeliverySettlementOutcome::Nack {
                    reason: ExactRenderDeliveryNackReason::BaseMismatch,
                    observed_baseline: request.applied_baseline,
                },
            });
        });
        assert_digest_changes(&base, "mode", |request| {
            request.mode = ExactRenderDeliveryMode::ForceFull;
        });
        assert_digest_changes(&base, "max_decompressed_bytes", |request| {
            request.receiver_caps.max_decompressed_bytes -= 1;
        });
        assert_digest_changes(&base, "max_text_bytes", |request| {
            request.receiver_caps.max_text_bytes -= 1;
        });
        assert_digest_changes(&base, "max_rows", |request| {
            request.receiver_caps.max_rows -= 1;
        });
        assert_digest_changes(&base, "max_snapshot_text_bytes", |request| {
            request.receiver_caps.max_snapshot_text_bytes -= 1;
        });
        assert_digest_changes(&base, "max_snapshot_rows", |request| {
            request.receiver_caps.max_snapshot_rows -= 1;
        });
        assert_digest_changes(&base, "max_snapshot_chunks", |request| {
            request.receiver_caps.max_snapshot_chunks -= 1;
        });

        let mut mismatched_body = base.clone();
        mismatched_body.receiver_caps.max_rows -= 1;
        let expected = mismatched_body
            .canonical_request_digest()
            .expect("mismatched body digest must compute");
        assert_eq!(
            mismatched_body.validate(),
            Err(ExactRenderDeliveryProtocolError::RequestDigestMismatch {
                expected,
                actual: base.request_digest,
            }),
        );
        let changed_body = mismatched_body
            .with_computed_request_digest()
            .expect("changed body digest must compute");
        assert_eq!(changed_body.identity, base.identity);
        assert_ne!(changed_body.request_digest, base.request_digest);
        let response = sample_exact_render_response_for(
            &base,
            ExactRenderDeliveryOutcomeV1::NoChange {
                current: exact_render_cursor(7),
                source_version: 1,
            },
        );
        assert_eq!(
            response.validate_for(&changed_body),
            Err(
                ExactRenderDeliveryProtocolError::ReplyRequestDigestMismatch {
                    expected: changed_body.request_digest,
                    actual: base.request_digest,
                }
            ),
            "same request sequence with a different body is equivocation, not a retry alias",
        );

        let (_, manifest, first, _) = sample_exact_render_manifest_and_chunks();
        let mut continuation = sample_exact_render_request(ExactRenderDeliveryMode::ForceFull);
        continuation.continuation = Some(ExactRenderSnapshotContinuationV1 {
            snapshot: manifest.snapshot,
            manifest_digest: manifest
                .canonical_manifest_digest()
                .expect("sample manifest digest must compute"),
            source_version: manifest.source_version,
            next_chunk_ordinal: first.ordinal,
            next_row_ordinal: first.first_row_ordinal,
            next_text_byte: first.first_text_byte,
        });
        continuation = continuation
            .with_computed_request_digest()
            .expect("continuation request digest must compute");
        assert_digest_changes(&continuation, "continuation snapshot", |request| {
            request
                .continuation
                .as_mut()
                .expect("continuation")
                .snapshot
                .content_digest = ExactRenderDigest::from_bytes([0xcc; 32]);
        });
        assert_digest_changes(&continuation, "continuation manifest digest", |request| {
            request
                .continuation
                .as_mut()
                .expect("continuation")
                .manifest_digest = ExactRenderDigest::from_bytes([0xdd; 32]);
        });
        assert_digest_changes(&continuation, "continuation source version", |request| {
            request
                .continuation
                .as_mut()
                .expect("continuation")
                .source_version += 1;
        });
        assert_digest_changes(&continuation, "continuation chunk ordinal", |request| {
            request
                .continuation
                .as_mut()
                .expect("continuation")
                .next_chunk_ordinal += 1;
        });
        assert_digest_changes(&continuation, "continuation row ordinal", |request| {
            request
                .continuation
                .as_mut()
                .expect("continuation")
                .next_row_ordinal += 1;
        });
        assert_digest_changes(&continuation, "continuation text offset", |request| {
            request
                .continuation
                .as_mut()
                .expect("continuation")
                .next_text_byte += 1;
        });
    }

    #[test]
    fn exact_render_delivery_continuity_never_infers_from_source_version() {
        for source_version in [0, 1, 4, 1_000_000, u64::MAX] {
            sample_exact_render_delta(source_version)
                .validate()
                .expect("arbitrary terminal mutation jumps are diagnostic only");
        }

        let mut non_advancing = sample_exact_render_delta(4);
        non_advancing.delivery.resulting_baseline.sequence = non_advancing.base.sequence;
        non_advancing = non_advancing
            .with_computed_digest()
            .expect("non-advancing fixture digest must still compute");
        assert_eq!(
            non_advancing.validate(),
            Err(ExactRenderDeliveryProtocolError::NonAdvancingDelta)
        );

        let mut wrong_generation = sample_exact_render_delta(5);
        wrong_generation
            .delivery
            .resulting_baseline
            .delivery_generation = ExactRenderDeliveryGeneration::try_new(2).expect("nonzero");
        wrong_generation = wrong_generation
            .with_computed_digest()
            .expect("wrong-generation fixture digest must still compute");
        assert_eq!(
            wrong_generation.validate(),
            Err(ExactRenderDeliveryProtocolError::DeliveryGenerationMismatch)
        );

        let mut outside_projection = sample_exact_render_delta(6);
        outside_projection.patches[0].start_stable_row = 100;
        outside_projection.rows[0].stable_row = 100;
        outside_projection.rows[1].stable_row = 101;
        outside_projection = outside_projection
            .with_computed_digest()
            .expect("out-of-projection fixture digest must still compute");
        assert_eq!(
            outside_projection.validate(),
            Err(ExactRenderDeliveryProtocolError::PatchProjectionOutOfRange)
        );

        let mut overlapping_replacements = sample_exact_render_delta(7);
        overlapping_replacements.patches = vec![
            ExactRenderRowPatchV1 {
                start_stable_row: -2,
                removed_rows: 1,
                replacement_start: 0,
                replacement_count: 2,
            },
            ExactRenderRowPatchV1 {
                start_stable_row: -1,
                removed_rows: 1,
                replacement_start: 2,
                replacement_count: 1,
            },
        ];
        overlapping_replacements.rows.push(ExactRenderRowV1 {
            stable_row: -1,
            text: ExactRenderRowTextV1::try_from_str("overlap")
                .expect("overlap fixture text is bounded"),
            wrapped: false,
        });
        overlapping_replacements = overlapping_replacements
            .with_computed_digest()
            .expect("overlap fixture digest must still compute");
        assert_eq!(
            overlapping_replacements.validate(),
            Err(ExactRenderDeliveryProtocolError::PatchOrderInvalid)
        );
    }

    #[test]
    fn exact_render_settlement_and_retry_identity_are_idempotent_and_generation_bound() {
        let delta = sample_exact_render_delta(75);
        let mut request = sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        request.identity.request_sequence =
            ExactRenderRequestSequence::try_new(2).expect("nonzero");
        request.applied_baseline =
            ExactRenderAppliedBaseline::Applied(delta.delivery.resulting_baseline);
        request.settlement = Some(ExactRenderDeliverySettlement {
            delivery: delta.delivery,
            outcome: ExactRenderDeliverySettlementOutcome::Applied,
        });
        request = request
            .with_computed_request_digest()
            .expect("settlement request digest must compute");
        request
            .validate()
            .expect("matching post-persistence ACK must validate");

        let first = Pdu::GetPaneRenderDeliveryV1(request.clone())
            .encode_frame_with_mode(81, CompressionMode::Never)
            .expect("first retry representation must encode");
        let duplicate = Pdu::GetPaneRenderDeliveryV1(request.clone())
            .encode_frame_with_mode(82, CompressionMode::Never)
            .expect("duplicate retry representation must encode");
        let first_raw = decode_raw(first.as_slice()).expect("first retry frame must decode raw");
        let duplicate_raw =
            decode_raw(duplicate.as_slice()).expect("duplicate retry frame must decode raw");
        assert_ne!(first_raw.serial, duplicate_raw.serial);
        assert_eq!(first_raw.ident, duplicate_raw.ident);
        assert_eq!(
            first_raw.data, duplicate_raw.data,
            "lost-ACK retry must preserve one application identity while the transport serial remains independently owned",
        );

        let response = sample_exact_render_response_for(
            &request,
            ExactRenderDeliveryOutcomeV1::NoChange {
                current: delta.delivery.resulting_baseline,
                source_version: u64::MAX,
            },
        );
        response
            .validate_for(&request)
            .expect("correlated duplicate settlement must retain exact identity");

        for reason in [
            ExactRenderDeliveryNackReason::BaseMismatch,
            ExactRenderDeliveryNackReason::GenerationMismatch,
            ExactRenderDeliveryNackReason::SnapshotCorrupt,
            ExactRenderDeliveryNackReason::BoundedResourceRejected,
            ExactRenderDeliveryNackReason::PersistenceFailure,
        ] {
            let mut nack_request =
                sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
            nack_request.identity.request_sequence =
                ExactRenderRequestSequence::try_new(3).expect("nonzero");
            nack_request.settlement = Some(ExactRenderDeliverySettlement {
                delivery: delta.delivery,
                outcome: ExactRenderDeliverySettlementOutcome::Nack {
                    reason,
                    observed_baseline: nack_request.applied_baseline,
                },
            });
            nack_request = nack_request
                .with_computed_request_digest()
                .expect("NACK request digest must compute");
            nack_request
                .validate()
                .expect("typed NACK must retain the exact observed baseline");
            let pdu = Pdu::GetPaneRenderDeliveryV1(nack_request);
            let frame = pdu
                .encode_frame_with_mode(82, CompressionMode::Never)
                .expect("typed NACK request must encode");
            assert_eq!(
                Pdu::decode(frame.as_slice())
                    .expect("typed NACK request must decode")
                    .pdu,
                pdu
            );
        }

        let mut wrong_generation = request.clone();
        wrong_generation.applied_baseline =
            ExactRenderAppliedBaseline::Applied(ExactRenderDeliveryCursor {
                delivery_generation: ExactRenderDeliveryGeneration::try_new(2).expect("nonzero"),
                ..delta.delivery.resulting_baseline
            });
        assert_eq!(
            wrong_generation.validate(),
            Err(ExactRenderDeliveryProtocolError::SettlementBaselineMismatch)
        );

        let mut wrong_connection = request;
        let mut settlement = wrong_connection.settlement.expect("settlement present");
        settlement.delivery.connection_identity = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0xc3; 16]),
            MuxSessionIncarnation::from_bytes([0xd4; 16]),
        );
        wrong_connection.settlement = Some(settlement);
        assert_eq!(
            wrong_connection.validate(),
            Err(ExactRenderDeliveryProtocolError::SettlementIdentityMismatch)
        );
    }

    #[test]
    fn exact_render_force_full_manifest_and_chunks_are_exactly_ordered() {
        let (rows, manifest, first, second) = sample_exact_render_manifest_and_chunks();
        manifest
            .validate_complete_rows(&rows)
            .expect("complete snapshot digest and totals must validate");
        let mut rechunked_manifest = manifest.clone();
        rechunked_manifest.chunk_count = 1;
        rechunked_manifest
            .validate_complete_rows(&rows)
            .expect("content identity must be independent of chunk boundaries");
        assert_eq!(
            rechunked_manifest.snapshot.content_digest,
            manifest.snapshot.content_digest,
        );
        assert_ne!(
            rechunked_manifest
                .canonical_manifest_digest()
                .expect("rechunked manifest identity must compute"),
            manifest
                .canonical_manifest_digest()
                .expect("sample manifest identity must compute"),
            "continuation identity must still bind the immutable chunk plan",
        );
        first
            .validate_for(&manifest)
            .expect("first immutable chunk must validate");
        second
            .validate_for(&manifest)
            .expect("last immutable chunk must validate");

        let force_full = sample_exact_render_request(ExactRenderDeliveryMode::ForceFull);
        sample_exact_render_response_for(
            &force_full,
            ExactRenderDeliveryOutcomeV1::FullManifest(manifest.clone()),
        )
        .validate_for(&force_full)
        .expect("force-full must begin with the immutable manifest");
        let mut same_baseline_refresh = force_full.clone();
        same_baseline_refresh.applied_baseline =
            ExactRenderAppliedBaseline::Applied(manifest.snapshot.resulting_baseline);
        same_baseline_refresh = same_baseline_refresh
            .with_computed_request_digest()
            .expect("same-baseline refresh request digest must compute");
        sample_exact_render_response_for(
            &same_baseline_refresh,
            ExactRenderDeliveryOutcomeV1::FullManifest(manifest.clone()),
        )
        .validate_for(&same_baseline_refresh)
        .expect("ForceFull may repair content at the same non-regressing baseline");
        let mut stale_snapshot_request = force_full.clone();
        stale_snapshot_request.applied_baseline =
            ExactRenderAppliedBaseline::Applied(ExactRenderDeliveryCursor {
                sequence: ExactRenderDeliverySequence::try_new(10).expect("nonzero"),
                ..manifest.snapshot.resulting_baseline
            });
        stale_snapshot_request = stale_snapshot_request
            .with_computed_request_digest()
            .expect("stale snapshot request digest must compute");
        assert_eq!(
            sample_exact_render_response_for(
                &stale_snapshot_request,
                ExactRenderDeliveryOutcomeV1::FullManifest(manifest.clone()),
            )
            .validate_for(&stale_snapshot_request),
            Err(ExactRenderDeliveryProtocolError::SnapshotRegressesBaseline)
        );
        assert_eq!(
            sample_exact_render_response_for(
                &force_full,
                ExactRenderDeliveryOutcomeV1::ExactDelta(sample_exact_render_delta(500)),
            )
            .validate_for(&force_full),
            Err(ExactRenderDeliveryProtocolError::ForceFullReturnedDelta)
        );

        let mut first_request = force_full.clone();
        first_request.identity.request_sequence =
            ExactRenderRequestSequence::try_new(2).expect("nonzero");
        first_request.continuation = Some(ExactRenderSnapshotContinuationV1 {
            snapshot: manifest.snapshot,
            manifest_digest: manifest
                .canonical_manifest_digest()
                .expect("sample manifest identity must compute"),
            source_version: manifest.source_version,
            next_chunk_ordinal: 0,
            next_row_ordinal: 0,
            next_text_byte: 0,
        });
        first_request = first_request
            .with_computed_request_digest()
            .expect("first continuation request digest must compute");
        let first_response = sample_exact_render_response_for(
            &first_request,
            ExactRenderDeliveryOutcomeV1::FullChunk {
                manifest: manifest.clone(),
                chunk: first.clone(),
            },
        );
        first_response
            .validate_for(&first_request)
            .expect("requested first chunk must validate");

        let mut drifted_manifest = manifest.clone();
        drifted_manifest.projection.title =
            ExactRenderTitleV1::try_from_str("drifted mid-stream title")
                .expect("drifted title remains bounded");
        let drifted_response = sample_exact_render_response_for(
            &first_request,
            ExactRenderDeliveryOutcomeV1::FullChunk {
                manifest: drifted_manifest,
                chunk: first.clone(),
            },
        );
        assert_eq!(
            drifted_response.validate_for(&first_request),
            Err(ExactRenderDeliveryProtocolError::ChunkContinuationMismatch),
            "continuation must bind the entire immutable manifest, not only its snapshot token",
        );

        let continuation = first
            .next_continuation(&manifest)
            .expect("first chunk continuation must compute")
            .expect("first of two chunks has a successor");
        let mut second_request = first_request;
        second_request.identity.request_sequence =
            ExactRenderRequestSequence::try_new(3).expect("nonzero");
        second_request.continuation = Some(continuation);
        second_request = second_request
            .with_computed_request_digest()
            .expect("second continuation request digest must compute");
        let second_response = sample_exact_render_response_for(
            &second_request,
            ExactRenderDeliveryOutcomeV1::FullChunk {
                manifest: manifest.clone(),
                chunk: second.clone(),
            },
        );
        second_response
            .validate_for(&second_request)
            .expect("requested final chunk must validate");
        assert_eq!(
            second
                .next_continuation(&manifest)
                .expect("last chunk must validate"),
            None
        );
        let snapshot_ack = GetPaneRenderDeliveryV1 {
            protocol_version: EXACT_RENDER_DELIVERY_PROTOCOL_VERSION,
            identity: exact_render_request_identity(4),
            request_digest: ExactRenderDigest::ZERO,
            applied_baseline: ExactRenderAppliedBaseline::Applied(
                manifest.snapshot.resulting_baseline,
            ),
            settlement: Some(ExactRenderDeliverySettlement {
                delivery: manifest.snapshot,
                outcome: ExactRenderDeliverySettlementOutcome::Applied,
            }),
            mode: ExactRenderDeliveryMode::Incremental,
            receiver_caps: ExactRenderReceiverCaps::protocol_maximum(),
            continuation: None,
        }
        .with_computed_request_digest()
        .expect("snapshot ACK request digest must compute");
        snapshot_ack
            .validate()
            .expect("snapshot baseline can advance only in the post-persistence ACK request");

        let mut wrong_order = second.clone();
        wrong_order.ordinal = 0;
        wrong_order = wrong_order
            .with_computed_digest(&manifest)
            .expect("wrong-order fixture digest must still compute");
        assert_eq!(
            wrong_order.validate_for(&manifest),
            Err(ExactRenderDeliveryProtocolError::SnapshotChunkRangeInvalid)
        );

        let mut wrong_digest = first.clone();
        wrong_digest.chunk_digest = ExactRenderDigest::from_bytes([0xee; 32]);
        assert_eq!(
            wrong_digest.validate_for(&manifest),
            Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "snapshot_chunk_digest",
            })
        );

        let mut wrong_totals = manifest.clone();
        wrong_totals.total_text_bytes += 1;
        assert_eq!(
            wrong_totals.validate_complete_rows(&rows),
            Err(ExactRenderDeliveryProtocolError::SnapshotTotalsInvalid)
        );

        let mut wrong_source = first;
        wrong_source.source_version += 1;
        wrong_source = wrong_source
            .with_computed_digest(&manifest)
            .expect("wrong-source fixture digest must still compute");
        assert_eq!(
            wrong_source.validate_for(&manifest),
            Err(ExactRenderDeliveryProtocolError::SnapshotSourceVersionMismatch)
        );
    }

    #[test]
    fn exact_render_receiver_and_q200_aggregate_caps_fail_closed() {
        let mut request = sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        let outcome = ExactRenderDeliveryOutcomeV1::ExactDelta(sample_exact_render_delta(88));
        let response = sample_exact_render_response(outcome.clone());
        let usage = response
            .resource_usage()
            .expect("sample usage must measure");
        request.receiver_caps.max_text_bytes = usage.text_bytes - 1;
        request = request
            .with_computed_request_digest()
            .expect("bounded request digest must compute");
        let response = sample_exact_render_response_for(&request, outcome);
        assert_eq!(
            response.validate_for(&request),
            Err(
                ExactRenderDeliveryProtocolError::ResponseExceedsReceiverCap {
                    resource: "text_bytes",
                    requested: usage.text_bytes,
                    limit: usage.text_bytes - 1,
                }
            )
        );

        let mut zero_cap = ExactRenderReceiverCaps::protocol_maximum();
        zero_cap.max_rows = 0;
        assert!(zero_cap.validate().is_err());
        let mut excessive_cap = ExactRenderReceiverCaps::protocol_maximum();
        excessive_cap.max_snapshot_chunks = MAX_EXACT_RENDER_SNAPSHOT_CHUNKS + 1;
        assert!(excessive_cap.validate().is_err());

        let (_, manifest, _, _) = sample_exact_render_manifest_and_chunks();
        let retained_snapshot_text = manifest
            .validated_retained_text_bytes()
            .expect("sample retained snapshot text must measure");
        assert!(retained_snapshot_text > manifest.total_text_bytes);
        let mut text_bounded_full = sample_exact_render_request(ExactRenderDeliveryMode::ForceFull);
        text_bounded_full.receiver_caps.max_snapshot_text_bytes = retained_snapshot_text - 1;
        text_bounded_full = text_bounded_full
            .with_computed_request_digest()
            .expect("snapshot-text-bounded request digest must compute");
        assert_eq!(
            sample_exact_render_response_for(
                &text_bounded_full,
                ExactRenderDeliveryOutcomeV1::FullManifest(manifest.clone()),
            )
            .validate_for(&text_bounded_full),
            Err(
                ExactRenderDeliveryProtocolError::ResponseExceedsReceiverCap {
                    resource: "snapshot_text_bytes",
                    requested: retained_snapshot_text,
                    limit: retained_snapshot_text - 1,
                }
            ),
            "snapshot receiver caps must charge retained title and working-directory bytes",
        );

        let mut bounded_full = sample_exact_render_request(ExactRenderDeliveryMode::ForceFull);
        bounded_full.receiver_caps.max_snapshot_rows = 1;
        bounded_full.receiver_caps.max_snapshot_chunks = 1;
        bounded_full = bounded_full
            .with_computed_request_digest()
            .expect("bounded force-full request digest must compute");
        assert_eq!(
            sample_exact_render_response_for(
                &bounded_full,
                ExactRenderDeliveryOutcomeV1::FullManifest(manifest),
            )
            .validate_for(&bounded_full),
            Err(
                ExactRenderDeliveryProtocolError::ResponseExceedsReceiverCap {
                    resource: "snapshot_rows",
                    requested: 2,
                    limit: 1,
                }
            )
        );

        let responses: Vec<_> = (1..=200)
            .map(|request_sequence| {
                let mut response = response.clone();
                response.request_identity.request_sequence =
                    ExactRenderRequestSequence::try_new(request_sequence)
                        .expect("q200 request sequence is nonzero");
                let mut correlated_request =
                    sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
                correlated_request.identity = response.request_identity;
                correlated_request = correlated_request
                    .with_computed_request_digest()
                    .expect("q200 request digest must compute");
                response.request_digest = correlated_request.request_digest;
                response
            })
            .collect();
        let aggregate = validate_exact_render_delivery_aggregate(
            &responses,
            ExactRenderDeliveryAggregateCaps::protocol_maximum(),
        )
        .expect("q200 sample must fit the protocol aggregate ceiling");
        assert_eq!(aggregate.members, 200);
        assert_eq!(aggregate.text_bytes, usage.text_bytes * 200);
        assert_eq!(aggregate.rows, usage.rows * 200);

        let exact_caps = ExactRenderDeliveryAggregateCaps {
            max_members: aggregate.members,
            max_decompressed_bytes: aggregate.decompressed_bytes,
            max_text_bytes: aggregate.text_bytes,
            max_rows: aggregate.rows,
        };
        assert_eq!(
            validate_exact_render_delivery_aggregate(&responses, exact_caps)
                .expect("exact q200 aggregate boundary must pass"),
            aggregate
        );
        let one_byte_short = ExactRenderDeliveryAggregateCaps {
            max_text_bytes: aggregate.text_bytes - 1,
            ..exact_caps
        };
        assert_eq!(
            validate_exact_render_delivery_aggregate(&responses, one_byte_short),
            Err(ExactRenderDeliveryProtocolError::AggregateLimitExceeded {
                resource: "text_bytes",
                requested: aggregate.text_bytes,
                limit: aggregate.text_bytes - 1,
            })
        );
    }

    #[test]
    fn exact_render_snapshot_backing_is_reserved_once_and_equivocation_fails_closed() {
        let (rows, manifest, first, _) = sample_exact_render_manifest_and_chunks();
        let manifest_response = sample_exact_render_response(
            ExactRenderDeliveryOutcomeV1::FullManifest(manifest.clone()),
        );
        let chunk_response =
            sample_exact_render_response(ExactRenderDeliveryOutcomeV1::FullChunk {
                manifest: manifest.clone(),
                chunk: first,
            });
        let repeated = [manifest_response.clone(), chunk_response];
        let retained_text_bytes = manifest
            .total_text_bytes
            .checked_add(
                manifest
                    .projection
                    .text_bytes()
                    .expect("sample projection text bytes must measure"),
            )
            .expect("sample retained text bytes must not overflow");
        let expected_once = ExactRenderBackingReservationUsage {
            distinct_snapshots: 1,
            total_text_bytes: retained_text_bytes,
            total_rows: manifest.total_rows,
            total_chunks: manifest.chunk_count,
        };
        let exact_once = ExactRenderBackingReservationCaps {
            max_distinct_snapshots: expected_once.distinct_snapshots,
            max_total_text_bytes: expected_once.total_text_bytes,
            max_total_rows: expected_once.total_rows,
            max_total_chunks: expected_once.total_chunks,
        };
        assert_eq!(
            validate_exact_render_backing_reservations(&repeated, exact_once)
                .expect("repeated chunks reserve one immutable snapshot"),
            expected_once,
        );
        for (resource, caps, requested, limit) in [
            (
                "distinct_snapshots",
                ExactRenderBackingReservationCaps {
                    max_distinct_snapshots: 0,
                    ..exact_once
                },
                1,
                0,
            ),
            (
                "text_bytes",
                ExactRenderBackingReservationCaps {
                    max_total_text_bytes: expected_once.total_text_bytes - 1,
                    ..exact_once
                },
                expected_once.total_text_bytes,
                expected_once.total_text_bytes - 1,
            ),
            (
                "rows",
                ExactRenderBackingReservationCaps {
                    max_total_rows: expected_once.total_rows - 1,
                    ..exact_once
                },
                expected_once.total_rows,
                expected_once.total_rows - 1,
            ),
            (
                "chunks",
                ExactRenderBackingReservationCaps {
                    max_total_chunks: expected_once.total_chunks - 1,
                    ..exact_once
                },
                expected_once.total_chunks,
                expected_once.total_chunks - 1,
            ),
        ] {
            assert_eq!(
                validate_exact_render_backing_reservations(&repeated, caps),
                Err(
                    ExactRenderDeliveryProtocolError::BackingReservationLimitExceeded {
                        resource,
                        requested,
                        limit,
                    }
                ),
            );
        }

        let mut second_manifest = manifest.clone();
        second_manifest.snapshot.resulting_baseline = second_manifest
            .snapshot
            .resulting_baseline
            .checked_next()
            .expect("sample snapshot baseline advances");
        second_manifest = second_manifest
            .with_computed_content_digest(&rows)
            .expect("second immutable snapshot digest must compute");
        let second_response = sample_exact_render_response(
            ExactRenderDeliveryOutcomeV1::FullManifest(second_manifest),
        );
        let distinct = [manifest_response.clone(), second_response];
        validate_exact_render_delivery_aggregate(
            &distinct,
            ExactRenderDeliveryAggregateCaps::protocol_maximum(),
        )
        .expect("two tiny wire manifests fit the independent wire aggregate");
        let expected_two = ExactRenderBackingReservationUsage {
            distinct_snapshots: 2,
            total_text_bytes: expected_once.total_text_bytes * 2,
            total_rows: expected_once.total_rows * 2,
            total_chunks: expected_once.total_chunks * 2,
        };
        let exact_two = ExactRenderBackingReservationCaps {
            max_distinct_snapshots: expected_two.distinct_snapshots,
            max_total_text_bytes: expected_two.total_text_bytes,
            max_total_rows: expected_two.total_rows,
            max_total_chunks: expected_two.total_chunks,
        };
        assert_eq!(
            validate_exact_render_backing_reservations(&distinct, exact_two)
                .expect("two distinct snapshots fit their exact backing reservation"),
            expected_two,
        );
        assert_eq!(
            validate_exact_render_backing_reservations(
                &distinct,
                ExactRenderBackingReservationCaps {
                    max_total_text_bytes: expected_once.total_text_bytes,
                    ..exact_two
                },
            ),
            Err(
                ExactRenderDeliveryProtocolError::BackingReservationLimitExceeded {
                    resource: "text_bytes",
                    requested: expected_two.total_text_bytes,
                    limit: expected_once.total_text_bytes,
                }
            ),
            "tiny manifest bodies cannot hide complete immutable text backing",
        );

        let mut content_digest_equivocation = manifest.clone();
        content_digest_equivocation.snapshot.content_digest =
            ExactRenderDigest::from_bytes([0xcc; 32]);
        assert_eq!(
            validate_exact_render_backing_reservations(
                &[
                    manifest_response.clone(),
                    sample_exact_render_response(ExactRenderDeliveryOutcomeV1::FullManifest(
                        content_digest_equivocation,
                    )),
                ],
                ExactRenderBackingReservationCaps::protocol_maximum(),
            ),
            Err(ExactRenderDeliveryProtocolError::SnapshotManifestEquivocation),
            "content digest is a manifest claim, not part of snapshot dedup identity",
        );

        let mut equivocated_manifest = manifest;
        equivocated_manifest.source_version += 1;
        let equivocated = [
            manifest_response,
            sample_exact_render_response(ExactRenderDeliveryOutcomeV1::FullManifest(
                equivocated_manifest,
            )),
        ];
        assert_eq!(
            validate_exact_render_backing_reservations(
                &equivocated,
                ExactRenderBackingReservationCaps::protocol_maximum(),
            ),
            Err(ExactRenderDeliveryProtocolError::SnapshotManifestEquivocation),
        );
    }

    #[test]
    fn exact_render_manifest_rejects_impossible_chunk_plans_and_row_text_totals() {
        let (_, manifest, _, _) = sample_exact_render_manifest_and_chunks();

        let mut row_infeasible = manifest.clone();
        row_infeasible.total_rows = MAX_EXACT_RENDER_DELIVERY_ROWS_U64 + 1;
        row_infeasible.total_text_bytes = 0;
        row_infeasible.chunk_count = 1;
        row_infeasible.projection.row_count = row_infeasible.total_rows;
        row_infeasible.projection.first_stable_row =
            -i64::try_from(row_infeasible.total_rows).expect("test row count fits i64");
        row_infeasible.projection.dimensions.scrollback_rows = row_infeasible.total_rows;
        row_infeasible.projection.dimensions.scrollback_top =
            row_infeasible.projection.first_stable_row;
        assert_eq!(
            row_infeasible.validate(),
            Err(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid),
            "one chunk cannot claim more than the per-response row envelope",
        );

        let mut text_infeasible = manifest.clone();
        text_infeasible.total_rows = 3;
        text_infeasible.chunk_count = 1;
        text_infeasible.projection.row_count = 3;
        text_infeasible.projection.first_stable_row = -3;
        text_infeasible.projection.dimensions.scrollback_rows = 3;
        text_infeasible.projection.dimensions.scrollback_top = -3;
        let projection_text_bytes = text_infeasible
            .projection
            .text_bytes()
            .expect("sample projection text bytes must measure");
        text_infeasible.total_text_bytes = MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64
            .checked_sub(projection_text_bytes)
            .and_then(|capacity| capacity.checked_add(1))
            .expect("test chunk text capacity must fit u64");
        assert_eq!(
            text_infeasible.validate(),
            Err(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid),
            "chunk feasibility must reserve the repeated projection metadata",
        );

        let mut row_text_inconsistent = manifest;
        row_text_inconsistent.total_rows = 1;
        row_text_inconsistent.total_text_bytes =
            u64::try_from(MAX_EXACT_RENDER_ROW_TEXT_BYTES).expect("row text ceiling fits u64") + 1;
        row_text_inconsistent.chunk_count = 1;
        row_text_inconsistent.projection.row_count = 1;
        row_text_inconsistent.projection.first_stable_row = -1;
        row_text_inconsistent.projection.dimensions.viewport_rows = 1;
        row_text_inconsistent.projection.dimensions.scrollback_rows = 1;
        row_text_inconsistent.projection.dimensions.physical_top = -1;
        row_text_inconsistent.projection.dimensions.scrollback_top = -1;
        assert_eq!(
            row_text_inconsistent.validate(),
            Err(ExactRenderDeliveryProtocolError::SnapshotTotalsInvalid),
            "one row cannot claim more text than the row schema can contain",
        );
    }

    #[test]
    fn exact_render_manifest_proves_contiguous_row_partition_feasibility() {
        let (_, mut template, _, _) = sample_exact_render_manifest_and_chunks();
        template.projection.first_stable_row = -3;
        template.projection.row_count = 3;
        template.projection.dimensions.scrollback_rows = 3;
        template.projection.dimensions.scrollback_top = -3;
        template.projection.title = ExactRenderTitleV1::try_from_string(
            "t".repeat(MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES),
        )
        .expect("maximum-size title must be representable");
        template.projection.working_dir = Some(
            ExactRenderWorkingDirectoryV1::try_from_string(
                "w".repeat(MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES),
            )
            .expect("maximum-size working directory must be representable"),
        );

        let projection_text_bytes = template
            .projection
            .text_bytes()
            .expect("maximum projection text must measure");
        let chunk_text_capacity = MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64
            .checked_sub(projection_text_bytes)
            .expect("projection must leave row-text capacity");
        assert_eq!(
            projection_text_bytes,
            MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64
                + MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES_U64,
        );

        let rows_with_lengths = |lengths: [usize; 3]| {
            [-3_i64, -2, -1]
                .iter()
                .copied()
                .zip(lengths.iter().copied())
                .map(|(stable_row, length)| ExactRenderRowV1 {
                    stable_row,
                    text: ExactRenderRowTextV1::try_from_string("r".repeat(length))
                        .expect("test row length must fit the row schema"),
                    wrapped: false,
                })
                .collect::<Vec<_>>()
        };
        let manifest_for = |rows: &[ExactRenderRowV1], chunk_count| {
            let mut manifest = template.clone();
            manifest.total_rows = u64::try_from(rows.len()).expect("test row count fits u64");
            manifest.total_text_bytes = exact_render_rows_usage(rows)
                .expect("test row text must measure")
                .text_bytes;
            manifest.chunk_count = chunk_count;
            manifest
        };

        let three_maximum_rows = rows_with_lengths([MAX_EXACT_RENDER_ROW_TEXT_BYTES; 3]);
        let two_chunk_counterexample = manifest_for(&three_maximum_rows, 2);
        two_chunk_counterexample
            .validate()
            .expect("aggregate chunk capacities alone admit this counterexample");
        assert_eq!(
            two_chunk_counterexample.with_computed_content_digest(&three_maximum_rows),
            Err(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid),
            "three indivisible one-million-byte rows cannot fit two contiguous chunks",
        );
        manifest_for(&three_maximum_rows, 3)
            .with_computed_content_digest(&three_maximum_rows)
            .expect("one legal chunk per maximum-size row must remain representable");

        let exact_second_row = usize::try_from(chunk_text_capacity)
            .expect("chunk text capacity fits usize")
            .checked_sub(MAX_EXACT_RENDER_ROW_TEXT_BYTES)
            .expect("one maximum row fits the chunk text capacity");
        let exact_boundary_rows = rows_with_lengths([
            MAX_EXACT_RENDER_ROW_TEXT_BYTES,
            exact_second_row,
            MAX_EXACT_RENDER_ROW_TEXT_BYTES,
        ]);
        manifest_for(&exact_boundary_rows, 2)
            .with_computed_content_digest(&exact_boundary_rows)
            .expect("an exact-capacity first chunk plus one final row must be feasible");

        let one_byte_over_rows = rows_with_lengths([
            MAX_EXACT_RENDER_ROW_TEXT_BYTES,
            exact_second_row + 1,
            MAX_EXACT_RENDER_ROW_TEXT_BYTES,
        ]);
        let one_byte_over_manifest = manifest_for(&one_byte_over_rows, 2);
        one_byte_over_manifest
            .validate()
            .expect("aggregate capacities still admit the one-byte partition counterexample");
        assert_eq!(
            one_byte_over_manifest.with_computed_content_digest(&one_byte_over_rows),
            Err(ExactRenderDeliveryProtocolError::SnapshotChunkCountInvalid),
            "one byte over both adjacent split points requires a third chunk",
        );
    }

    #[test]
    fn exact_render_delta_rechecks_text_limit_after_projection_metadata() {
        let mut delta = sample_exact_render_delta(101);
        delta.rows[0].text =
            ExactRenderRowTextV1::try_from_string("a".repeat(MAX_EXACT_RENDER_ROW_TEXT_BYTES))
                .expect("first maximum-size row must be representable");
        delta.rows[1].text =
            ExactRenderRowTextV1::try_from_string("b".repeat(MAX_EXACT_RENDER_ROW_TEXT_BYTES))
                .expect("second maximum-size row must be representable");
        delta.resulting_projection.title = ExactRenderTitleV1::try_from_string(
            "t".repeat(MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES),
        )
        .expect("maximum-size title must be representable");
        delta.resulting_projection.working_dir = Some(
            ExactRenderWorkingDirectoryV1::try_from_string(
                "w".repeat(MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES),
            )
            .expect("maximum-size working directory must be representable"),
        );
        delta = delta
            .with_computed_digest()
            .expect("oversized aggregate fixture digest must compute");
        let requested = 2_u64
            .checked_mul(
                u64::try_from(MAX_EXACT_RENDER_ROW_TEXT_BYTES).expect("row text ceiling fits u64"),
            )
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES)
                        .expect("title ceiling fits u64"),
                )
            })
            .and_then(|bytes| {
                bytes.checked_add(
                    u64::try_from(MAX_EXACT_RENDER_PROJECTION_WORKING_DIR_BYTES)
                        .expect("working-directory ceiling fits u64"),
                )
            })
            .expect("test text usage must fit u64");
        assert_eq!(
            delta.validate(),
            Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "reply_text_bytes",
                requested,
                limit: MAX_EXACT_RENDER_DELIVERY_TEXT_BYTES_U64,
            }),
        );
    }

    #[test]
    fn exact_render_validation_visits_each_heavy_row_once() {
        let delta = sample_exact_render_delta(99);
        let delta_rows = delta.rows.len();
        reset_test_exact_render_validation_row_visits();
        sample_exact_render_response(ExactRenderDeliveryOutcomeV1::ExactDelta(delta))
            .validate()
            .expect("sample delta must validate");
        assert_eq!(test_exact_render_validation_row_visits(), delta_rows);

        let (rows, manifest, first, _) = sample_exact_render_manifest_and_chunks();
        let chunk_rows = first.rows.len();
        reset_test_exact_render_validation_row_visits();
        sample_exact_render_response(ExactRenderDeliveryOutcomeV1::FullChunk {
            manifest: manifest.clone(),
            chunk: first,
        })
        .validate()
        .expect("sample chunk must validate");
        assert_eq!(test_exact_render_validation_row_visits(), chunk_rows);

        reset_test_exact_render_validation_row_visits();
        manifest
            .validate_complete_rows(&rows)
            .expect("complete sample snapshot must validate");
        assert_eq!(test_exact_render_validation_row_visits(), rows.len());
    }

    #[test]
    fn exact_render_minimum_envelope_can_report_every_zero_content_terminal_outcome() {
        let mut request = sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        request.receiver_caps.max_decompressed_bytes = MIN_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES;
        request.receiver_caps.max_text_bytes = 1;
        request = request
            .with_computed_request_digest()
            .expect("minimum-envelope request digest must compute");
        request
            .validate()
            .expect("the protocol reply-envelope floor must be admissible");

        let oldest = ExactRenderDeliveryCursor {
            sequence: ExactRenderDeliverySequence::try_new(8).expect("nonzero"),
            ..exact_render_cursor(7)
        };
        let current = ExactRenderDeliveryCursor {
            sequence: ExactRenderDeliverySequence::try_new(9).expect("nonzero"),
            ..exact_render_cursor(7)
        };
        let outcomes = [
            ExactRenderDeliveryOutcomeV1::NoChange {
                current: exact_render_cursor(7),
                source_version: 1,
            },
            ExactRenderDeliveryOutcomeV1::BaselineTooOld {
                requested: exact_render_cursor(7),
                oldest_available: oldest,
                current,
            },
            ExactRenderDeliveryOutcomeV1::GenerationChanged {
                requested: exact_render_cursor(7),
                current_pane_generation: ExactRenderPaneGeneration::try_new(2).expect("nonzero"),
                current_delivery_generation: ExactRenderDeliveryGeneration::try_new(1)
                    .expect("nonzero"),
            },
            ExactRenderDeliveryOutcomeV1::PaneRemoved {
                last_pane_generation: Some(ExactRenderPaneGeneration::try_new(1).expect("nonzero")),
            },
            ExactRenderDeliveryOutcomeV1::AuthorityExhausted {
                authority: ExactRenderAuthority::DeliverySequence,
            },
            ExactRenderDeliveryOutcomeV1::LimitsExceeded {
                resource: ExactRenderLimitResource::Rows,
                required: request.receiver_caps.max_rows + 1,
                limit: request.receiver_caps.max_rows,
            },
        ];

        for outcome in outcomes {
            let response = sample_exact_render_response_for(&request, outcome);
            response
                .validate_for(&request)
                .expect("every zero-content terminal outcome must fit the reply floor");
            let usage = response
                .resource_usage()
                .expect("terminal response usage must be measurable");
            assert_eq!(usage.text_bytes, 0);
            assert_eq!(usage.rows, 0);
            assert!(
                usage.decompressed_bytes <= MIN_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES,
                "terminal response used {} bytes above the {}-byte protocol floor",
                usage.decompressed_bytes,
                MIN_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES,
            );
        }

        let mut impossible = request.receiver_caps;
        impossible.max_decompressed_bytes = MIN_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES - 1;
        assert_eq!(
            impossible.validate(),
            Err(ExactRenderDeliveryProtocolError::InvalidReceiverCap {
                resource: "max_decompressed_bytes",
                requested: MIN_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES - 1,
                protocol_minimum: MIN_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES,
                protocol_maximum: MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES_U64,
            })
        );
    }

    #[test]
    fn exact_render_projection_metadata_converges_and_digest_failures_surface() {
        let (rows, manifest, _, _) = sample_exact_render_manifest_and_chunks();
        let sample_projection = sample_exact_render_projection();
        assert_eq!(
            ExactRenderCursorPositionV1::try_from_stable(StableCursorPosition {
                x: 7,
                y: -1,
                shape: termwiz::surface::CursorShape::SteadyBlock,
                visibility: termwiz::surface::CursorVisibility::Visible,
            })
            .expect("native cursor must narrow into fixed-width v1 authority"),
            sample_projection.cursor_position,
        );
        assert_eq!(
            ExactRenderDimensionsV1::try_from_renderable(RenderableDimensions {
                cols: 80,
                viewport_rows: 2,
                scrollback_rows: 2,
                physical_top: -2,
                scrollback_top: -2,
                dpi: 144,
                pixel_width: 1_600,
                pixel_height: 900,
                reverse_video: false,
            })
            .expect("native dimensions must narrow into fixed-width v1 authority"),
            sample_projection.dimensions,
        );

        let mut changed_title = manifest.clone();
        changed_title.projection.title = ExactRenderTitleV1::try_from_str("sample exact render!")
            .expect("changed title remains bounded");
        assert_eq!(
            changed_title.validate_complete_rows(&rows),
            Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "snapshot_content_digest",
            })
        );
        changed_title = changed_title
            .with_computed_content_digest(&rows)
            .expect("bounded title metadata must have a canonical digest");
        changed_title
            .validate_complete_rows(&rows)
            .expect("re-digested title metadata must converge");

        let mut changed_cursor = manifest.clone();
        changed_cursor.projection.cursor_position.x += 1;
        assert_eq!(
            changed_cursor.validate_complete_rows(&rows),
            Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "snapshot_content_digest",
            })
        );

        let mut changed_mouse_capture = manifest.clone();
        changed_mouse_capture.projection.mouse_grabbed = true;
        assert_eq!(
            changed_mouse_capture.validate_complete_rows(&rows),
            Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "snapshot_content_digest",
            })
        );

        let mut changed_dimensions = manifest.clone();
        changed_dimensions.projection.dimensions.cols += 1;
        assert_eq!(
            changed_dimensions.validate_complete_rows(&rows),
            Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "snapshot_content_digest",
            }),
            "a valid resize must still change the persisted projection identity",
        );

        let mut changed_working_dir = manifest.clone();
        changed_working_dir.projection.working_dir = Some(
            ExactRenderWorkingDirectoryV1::try_from_str("file:///tmp/other")
                .expect("changed working directory remains bounded"),
        );
        assert_eq!(
            changed_working_dir.validate_complete_rows(&rows),
            Err(ExactRenderDeliveryProtocolError::DigestMismatch {
                field: "snapshot_content_digest",
            })
        );

        let mut malformed_dimensions = manifest.clone();
        malformed_dimensions.projection.dimensions.viewport_rows = 1;
        assert_eq!(
            malformed_dimensions.validate(),
            Err(ExactRenderDeliveryProtocolError::ProjectionMetadataInvalid)
        );

        assert_eq!(
            ExactRenderTitleV1::try_from_string(
                "x".repeat(MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES + 1),
            ),
            Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "exact_render_utf8_bytes",
                requested: MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64 + 1,
                limit: MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64,
            })
        );
        let oversized_borrowed_title = "x".repeat(MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES + 1);
        assert_eq!(
            ExactRenderTitleV1::try_from_str(&oversized_borrowed_title),
            Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "exact_render_utf8_bytes",
                requested: MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64 + 1,
                limit: MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64,
            }),
            "borrowed UTF-8 must fail its byte bound before any owned copy",
        );
        let mut hostile_title_prefix = Vec::new();
        leb128::write::unsigned(
            &mut hostile_title_prefix,
            MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES_U64 + 1,
        )
        .expect("hostile title length prefix must encode");
        let title_error = bounded_varbincode::deserialize::<ExactRenderTitleV1, _>(
            &mut hostile_title_prefix.as_slice(),
        )
        .expect_err("title length must fail before payload allocation");
        assert!(
            title_error
                .to_string()
                .contains("exact render metadata UTF-8 bytes length 65537 exceeds maximum 65536"),
            "unexpected schema-specific title bound: {title_error}",
            title_error = title_error,
        );

        let mut hostile_row_prefix = Vec::new();
        leb128::write::unsigned(
            &mut hostile_row_prefix,
            u64::try_from(MAX_EXACT_RENDER_ROW_TEXT_BYTES + 1).expect("row text ceiling fits u64"),
        )
        .expect("hostile row length prefix must encode");
        let row_error = bounded_varbincode::deserialize::<ExactRenderRowTextV1, _>(
            &mut hostile_row_prefix.as_slice(),
        )
        .expect_err("row length must fail before payload allocation");
        assert!(
            row_error
                .to_string()
                .contains("exact render row UTF-8 bytes length 1000001 exceeds maximum 1000000"),
            "unexpected schema-specific row bound: {row_error}",
            row_error = row_error,
        );
        let exact_row =
            ExactRenderRowTextV1::try_from_string("x".repeat(MAX_EXACT_RENDER_ROW_TEXT_BYTES))
                .expect("the exact per-row UTF-8 ceiling must be constructible");
        let (exact_row_payload, exact_row_compressed) =
            serialize_with_mode(&exact_row, CompressionMode::Never)
                .expect("the exact per-row ceiling must serialize");
        assert!(!exact_row_compressed);
        let mut expected_row_payload = Vec::new();
        leb128::write::unsigned(
            &mut expected_row_payload,
            u64::try_from(MAX_EXACT_RENDER_ROW_TEXT_BYTES).expect("row text ceiling fits u64"),
        )
        .expect("row text length prefix must encode");
        expected_row_payload.extend_from_slice(exact_row.as_bytes());
        assert!(
            exact_row_payload == expected_row_payload,
            "schema markers must add zero bytes to the frozen varbincode wire",
        );
        let decoded_exact_row = bounded_varbincode::deserialize::<ExactRenderRowTextV1, _>(
            &mut exact_row_payload.as_slice(),
        )
        .expect("the exact per-row ceiling must deserialize symmetrically");
        assert!(decoded_exact_row == exact_row);

        let exact_title = ExactRenderTitleV1::try_from_string(
            "m".repeat(MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES),
        )
        .expect("the exact metadata UTF-8 ceiling must be constructible");
        let (exact_title_payload, exact_title_compressed) =
            serialize_with_mode(&exact_title, CompressionMode::Never)
                .expect("the exact metadata ceiling must serialize");
        assert!(!exact_title_compressed);
        let decoded_exact_title = bounded_varbincode::deserialize::<ExactRenderTitleV1, _>(
            &mut exact_title_payload.as_slice(),
        )
        .expect("the exact metadata ceiling must deserialize symmetrically");
        assert!(decoded_exact_title == exact_title);
        let legacy_string = "s".repeat(MAX_EXACT_RENDER_PROJECTION_TITLE_BYTES + 1);
        let (scoped_payload, scoped_compressed) = serialize_with_mode(
            &(decoded_exact_title, legacy_string.clone()),
            CompressionMode::Never,
        )
        .expect("metadata marker scope fixture must serialize");
        assert!(!scoped_compressed);
        let (_, decoded_legacy_string) = bounded_varbincode::deserialize::<
            (ExactRenderTitleV1, String),
            _,
        >(&mut scoped_payload.as_slice())
        .expect("metadata byte cap must be restored after its marked newtype");
        assert!(decoded_legacy_string == legacy_string);
        assert_eq!(
            ExactRenderRowTextV1::try_from_string("x".repeat(MAX_EXACT_RENDER_ROW_TEXT_BYTES + 1),),
            Err(ExactRenderDeliveryProtocolError::ResourceLimitExceeded {
                resource: "exact_render_utf8_bytes",
                requested: u64::try_from(MAX_EXACT_RENDER_ROW_TEXT_BYTES + 1)
                    .expect("row text ceiling fits u64"),
                limit: u64::try_from(MAX_EXACT_RENDER_ROW_TEXT_BYTES)
                    .expect("row text ceiling fits u64"),
            })
        );

        let mut mismatched_totals = manifest;
        mismatched_totals.total_rows += 1;
        assert_eq!(
            mismatched_totals.with_computed_content_digest(&rows),
            Err(ExactRenderDeliveryProtocolError::SnapshotTotalsInvalid),
            "digest constructors must propagate canonicalization failure",
        );
    }

    #[test]
    fn exact_render_v1_authority_digests_match_frozen_golden_preimages() {
        const DELTA_GOLDEN: ExactRenderDigest = ExactRenderDigest::from_bytes([
            0xad, 0x7f, 0x05, 0x21, 0xc7, 0x59, 0x6e, 0x6e, 0xad, 0x37, 0xb3, 0x1d, 0x15, 0x67,
            0xdb, 0xdb, 0x85, 0x49, 0xc6, 0xc4, 0xcb, 0xe1, 0x14, 0x47, 0xba, 0xa2, 0x3f, 0x9f,
            0x26, 0xe7, 0xd0, 0xad,
        ]);
        const SNAPSHOT_GOLDEN: ExactRenderDigest = ExactRenderDigest::from_bytes([
            0x4f, 0x3b, 0x1c, 0xce, 0xb7, 0x99, 0x41, 0x18, 0xf6, 0xfc, 0x88, 0xe5, 0xe2, 0x7e,
            0x4a, 0xdb, 0x5c, 0xa1, 0x48, 0x79, 0x21, 0xf4, 0xb3, 0x18, 0xb1, 0x9f, 0xea, 0xdf,
            0x80, 0xa2, 0x03, 0xac,
        ]);
        const MANIFEST_GOLDEN: ExactRenderDigest = ExactRenderDigest::from_bytes([
            0x2e, 0x85, 0xad, 0x8d, 0x7c, 0x6d, 0x67, 0xfe, 0x80, 0x76, 0xdd, 0x22, 0xfe, 0xda,
            0x20, 0x05, 0x94, 0x91, 0x6e, 0x94, 0x28, 0x11, 0xbb, 0xfc, 0xa1, 0x88, 0x94, 0xb1,
            0xc3, 0xae, 0xfc, 0x8f,
        ]);
        const CHUNK_GOLDEN: ExactRenderDigest = ExactRenderDigest::from_bytes([
            0xd2, 0x58, 0x8c, 0x96, 0xe2, 0x46, 0x8b, 0x28, 0x4b, 0x3c, 0x46, 0x78, 0x1e, 0x95,
            0x8d, 0xe9, 0x62, 0xd8, 0xd4, 0x36, 0xff, 0xf4, 0x15, 0x9a, 0x15, 0xa1, 0x63, 0xa1,
            0x53, 0x9f, 0x14, 0xca,
        ]);

        let connection = RenderConnectionIdentity::new(
            TopologyStreamId::from_bytes([0x01; 16]),
            MuxSessionIncarnation::from_bytes([0x02; 16]),
        );
        let result_cursor = ExactRenderDeliveryCursor {
            pane_generation: ExactRenderPaneGeneration::try_new(4).expect("nonzero"),
            delivery_generation: ExactRenderDeliveryGeneration::try_new(5).expect("nonzero"),
            sequence: ExactRenderDeliverySequence::try_new(7).expect("nonzero"),
        };
        let base_cursor = ExactRenderDeliveryCursor {
            sequence: ExactRenderDeliverySequence::try_new(6).expect("nonzero"),
            ..result_cursor
        };
        let projection = ExactRenderProjectionV1 {
            first_stable_row: 0,
            row_count: 1,
            alt_screen_active: false,
            mouse_grabbed: true,
            cursor_position: ExactRenderCursorPositionV1 {
                x: 1,
                y: 0,
                shape: ExactRenderCursorShapeV1::SteadyBlock,
                visibility: ExactRenderCursorVisibilityV1::Visible,
            },
            dimensions: ExactRenderDimensionsV1 {
                cols: 80,
                viewport_rows: 1,
                scrollback_rows: 1,
                physical_top: 0,
                scrollback_top: 0,
                dpi: 96,
                pixel_width: 800,
                pixel_height: 600,
                reverse_video: true,
            },
            title: ExactRenderTitleV1::try_from_str("T").expect("bounded title"),
            working_dir: Some(
                ExactRenderWorkingDirectoryV1::try_from_str("W")
                    .expect("bounded working directory"),
            ),
        };
        let rows = vec![ExactRenderRowV1 {
            stable_row: 0,
            text: ExactRenderRowTextV1::try_from_str("x").expect("bounded row"),
            wrapped: false,
        }];
        let delta = ExactRenderDeltaV1 {
            delivery: ExactRenderDeliveryToken {
                connection_identity: connection,
                pane_id: ExactRenderPaneId::new(3),
                resulting_baseline: result_cursor,
                content_digest: ExactRenderDigest::ZERO,
            },
            base: base_cursor,
            source_version: 8,
            resulting_projection: projection.clone(),
            patches: vec![ExactRenderRowPatchV1 {
                start_stable_row: 0,
                removed_rows: 1,
                replacement_start: 0,
                replacement_count: 1,
            }],
            rows: rows.clone(),
        }
        .with_computed_digest()
        .expect("golden delta digest must compute");
        assert_eq!(delta.delivery.content_digest, DELTA_GOLDEN);

        let manifest = ExactRenderSnapshotManifestV1 {
            snapshot: ExactRenderDeliveryToken {
                connection_identity: connection,
                pane_id: ExactRenderPaneId::new(3),
                resulting_baseline: result_cursor,
                content_digest: ExactRenderDigest::ZERO,
            },
            source_version: 8,
            projection,
            total_rows: 1,
            total_text_bytes: 1,
            chunk_count: 1,
        }
        .with_computed_content_digest(&rows)
        .expect("golden snapshot digest must compute");
        assert_eq!(manifest.snapshot.content_digest, SNAPSHOT_GOLDEN);
        assert_eq!(
            manifest
                .canonical_manifest_digest()
                .expect("golden manifest digest must compute"),
            MANIFEST_GOLDEN,
        );
        let chunk = ExactRenderSnapshotChunkV1 {
            source_version: 8,
            ordinal: 0,
            first_row_ordinal: 0,
            first_text_byte: 0,
            rows,
            chunk_digest: ExactRenderDigest::ZERO,
        }
        .with_computed_digest(&manifest)
        .expect("golden chunk digest must compute");
        assert_eq!(chunk.chunk_digest, CHUNK_GOLDEN);

        // Independent, byte-explicit grammar audit. Every integer is fixed-width
        // big endian; booleans/tags are one byte; text is u64 length + UTF-8.
        let mut token_context = Vec::new();
        token_context.extend_from_slice(&[0x01; 16]);
        token_context.extend_from_slice(&[0x02; 16]);
        token_context.extend_from_slice(&3_u64.to_be_bytes());
        token_context.extend_from_slice(&4_u64.to_be_bytes());
        token_context.extend_from_slice(&5_u64.to_be_bytes());
        token_context.extend_from_slice(&7_u64.to_be_bytes());

        let mut projection_preimage = Vec::new();
        projection_preimage.extend_from_slice(&0_i64.to_be_bytes());
        projection_preimage.extend_from_slice(&1_u64.to_be_bytes());
        projection_preimage.extend_from_slice(&[0, 1]);
        projection_preimage.extend_from_slice(&1_u64.to_be_bytes());
        projection_preimage.extend_from_slice(&0_i64.to_be_bytes());
        projection_preimage.extend_from_slice(&[2, 1]);
        projection_preimage.extend_from_slice(&80_u64.to_be_bytes());
        projection_preimage.extend_from_slice(&1_u64.to_be_bytes());
        projection_preimage.extend_from_slice(&1_u64.to_be_bytes());
        projection_preimage.extend_from_slice(&0_i64.to_be_bytes());
        projection_preimage.extend_from_slice(&0_i64.to_be_bytes());
        projection_preimage.extend_from_slice(&96_u32.to_be_bytes());
        projection_preimage.extend_from_slice(&800_u64.to_be_bytes());
        projection_preimage.extend_from_slice(&600_u64.to_be_bytes());
        projection_preimage.push(1);
        projection_preimage.extend_from_slice(&1_u64.to_be_bytes());
        projection_preimage.push(b'T');
        projection_preimage.push(1);
        projection_preimage.extend_from_slice(&1_u64.to_be_bytes());
        projection_preimage.push(b'W');
        assert_eq!(projection_preimage.len(), 116);

        let mut row_preimage = Vec::new();
        row_preimage.extend_from_slice(&1_u64.to_be_bytes());
        row_preimage.extend_from_slice(&0_i64.to_be_bytes());
        row_preimage.push(0);
        row_preimage.extend_from_slice(&1_u64.to_be_bytes());
        row_preimage.push(b'x');

        let mut delta_preimage = b"frankenterm.exact-render-delta.v1\0".to_vec();
        delta_preimage.extend_from_slice(&token_context);
        delta_preimage.extend_from_slice(&4_u64.to_be_bytes());
        delta_preimage.extend_from_slice(&5_u64.to_be_bytes());
        delta_preimage.extend_from_slice(&6_u64.to_be_bytes());
        delta_preimage.extend_from_slice(&8_u64.to_be_bytes());
        delta_preimage.extend_from_slice(&projection_preimage);
        delta_preimage.extend_from_slice(&1_u64.to_be_bytes());
        delta_preimage.extend_from_slice(&0_i64.to_be_bytes());
        delta_preimage.extend_from_slice(&1_u64.to_be_bytes());
        delta_preimage.extend_from_slice(&0_u64.to_be_bytes());
        delta_preimage.extend_from_slice(&1_u64.to_be_bytes());
        delta_preimage.extend_from_slice(&row_preimage);
        assert_eq!(delta_preimage.len(), 312);
        assert_eq!(
            ExactRenderDigest::from_bytes(Sha256::digest(&delta_preimage).into()),
            DELTA_GOLDEN,
        );

        let mut snapshot_preimage = b"frankenterm.exact-render-snapshot.v1\0".to_vec();
        snapshot_preimage.extend_from_slice(&token_context);
        snapshot_preimage.extend_from_slice(&8_u64.to_be_bytes());
        snapshot_preimage.extend_from_slice(&projection_preimage);
        snapshot_preimage.extend_from_slice(&1_u64.to_be_bytes());
        snapshot_preimage.extend_from_slice(&1_u64.to_be_bytes());
        snapshot_preimage.extend_from_slice(&row_preimage);
        assert_eq!(snapshot_preimage.len(), 267);
        assert_eq!(
            ExactRenderDigest::from_bytes(Sha256::digest(&snapshot_preimage).into()),
            SNAPSHOT_GOLDEN,
        );

        let mut token = token_context.clone();
        token.extend_from_slice(&SNAPSHOT_GOLDEN.as_bytes());
        let mut manifest_preimage = b"frankenterm.exact-render-snapshot-manifest.v1\0".to_vec();
        manifest_preimage.extend_from_slice(&token);
        manifest_preimage.extend_from_slice(&8_u64.to_be_bytes());
        manifest_preimage.extend_from_slice(&projection_preimage);
        manifest_preimage.extend_from_slice(&1_u64.to_be_bytes());
        manifest_preimage.extend_from_slice(&1_u64.to_be_bytes());
        manifest_preimage.extend_from_slice(&1_u64.to_be_bytes());
        assert_eq!(manifest_preimage.len(), 290);
        assert_eq!(
            ExactRenderDigest::from_bytes(Sha256::digest(&manifest_preimage).into()),
            MANIFEST_GOLDEN,
        );

        let mut chunk_preimage = b"frankenterm.exact-render-snapshot-chunk.v1\0".to_vec();
        chunk_preimage.extend_from_slice(&token);
        chunk_preimage.extend_from_slice(&8_u64.to_be_bytes());
        chunk_preimage.extend_from_slice(&0_u64.to_be_bytes());
        chunk_preimage.extend_from_slice(&0_u64.to_be_bytes());
        chunk_preimage.extend_from_slice(&0_u64.to_be_bytes());
        chunk_preimage.extend_from_slice(&row_preimage);
        assert_eq!(chunk_preimage.len(), 197);
        assert_eq!(
            ExactRenderDigest::from_bytes(Sha256::digest(&chunk_preimage).into()),
            CHUNK_GOLDEN,
        );
    }

    #[test]
    fn exact_render_canonical_comparator_rejects_both_length_directions_and_byte_drift() {
        let value = 7_u64;
        let canonical = serialize_with_mode(&value, CompressionMode::Never)
            .expect("canonical scalar must serialize")
            .0;
        ensure_exact_render_canonical_payload(&value, &canonical, "test")
            .expect("exact canonical bytes must compare equal");

        let mut byte_drift = canonical.clone();
        let last = byte_drift
            .last_mut()
            .expect("canonical scalar encoding is nonempty");
        *last ^= 1;
        let byte_error = ensure_exact_render_canonical_payload(&value, &byte_drift, "test")
            .expect_err("one changed byte must fail");
        assert!(format!("{byte_error:#}").contains("byte mismatch at offset"));

        let canonical_longer = &canonical[..canonical.len() - 1];
        let longer_error = ensure_exact_render_canonical_payload(&value, canonical_longer, "test")
            .expect_err("canonical serialization longer than payload must fail");
        assert!(format!("{longer_error:#}").contains("canonical serialization is longer"));

        let mut canonical_shorter = canonical;
        canonical_shorter.push(0);
        let shorter_error =
            ensure_exact_render_canonical_payload(&value, &canonical_shorter, "test")
                .expect_err("canonical serialization shorter than payload must fail");
        assert!(format!("{shorter_error:#}").contains("canonical serialization is 1 bytes shorter"));
    }

    #[test]
    fn exact_render_decoders_reject_trailing_truncated_and_over_limit_payloads() {
        let request = sample_exact_render_request(ExactRenderDeliveryMode::Incremental);
        let response = sample_exact_render_response(ExactRenderDeliveryOutcomeV1::ExactDelta(
            sample_exact_render_delta(91),
        ));
        for mode in [CompressionMode::Never, CompressionMode::Always] {
            for (name, frame) in [
                (
                    "GetPaneRenderDeliveryV1",
                    encode_authority_payload_with_trailing_schema_byte(91, 1, &request, mode),
                ),
                (
                    "GetPaneRenderDeliveryV1Response",
                    encode_authority_payload_with_trailing_schema_byte(92, 2, &response, mode),
                ),
            ] {
                let error = Pdu::decode(frame.as_slice())
                    .expect_err("closed exact render schema must reject trailing bytes");
                assert!(
                    format!("{error:#}")
                        .contains(&format!("{name} payload has trailing schema bytes")),
                    "unexpected trailing-byte rejection for {}: {:#}",
                    name,
                    error,
                );
            }
        }

        for (name, frame) in [
            (
                "GetPaneRenderDeliveryV1",
                encode_authority_payload_with_compressed_suffix(91, 3, &request, &[0xde, 0xad]),
            ),
            (
                "GetPaneRenderDeliveryV1Response",
                encode_authority_payload_with_compressed_suffix(92, 4, &response, &[0xbe, 0xef]),
            ),
        ] {
            let error = Pdu::decode(frame.as_slice())
                .expect_err("compressed suffix after exact render schema must fail closed");
            assert!(
                format!("{error:#}").contains("trailing compressed frame bytes"),
                "unexpected compressed-suffix rejection for {}: {:#}",
                name,
                error,
            );
        }

        let truncated_request = encode_authority_payload_with_truncated_zstd(91, 5, &request);
        Pdu::decode(truncated_request.as_slice())
            .expect_err("checksum-truncated compressed request must fail closed");
        let truncated_response = encode_authority_payload_with_truncated_zstd(92, 6, &response);
        Pdu::decode(truncated_response.as_slice())
            .expect_err("checksum-truncated compressed response must fail closed");

        for mut frame in [
            Pdu::GetPaneRenderDeliveryV1(request.clone())
                .encode_frame_with_mode(5, CompressionMode::Never)
                .expect("request fixture must encode"),
            Pdu::GetPaneRenderDeliveryV1Response(response.clone())
                .encode_frame_with_mode(6, CompressionMode::Never)
                .expect("response fixture must encode"),
        ] {
            frame.pop().expect("fixture frame is non-empty");
            Pdu::decode(frame.as_slice())
                .expect_err("truncated uncompressed exact render frame must fail closed");
        }

        for (name, ident, serial, canonical) in [
            (
                "GetPaneRenderDeliveryV1",
                91,
                7,
                serialize_with_mode(&request, CompressionMode::Never)
                    .expect("canonical request payload must serialize")
                    .0,
            ),
            (
                "GetPaneRenderDeliveryV1Response",
                92,
                8,
                serialize_with_mode(&response, CompressionMode::Never)
                    .expect("canonical response payload must serialize")
                    .0,
            ),
        ] {
            assert_eq!(canonical.first(), Some(&1));
            let mut noncanonical = Vec::with_capacity(canonical.len() + 1);
            noncanonical.extend_from_slice(&[0x81, 0x00]);
            noncanonical.extend_from_slice(&canonical[1..]);
            let mut frame = Vec::new();
            encode_raw(ident, serial, &noncanonical, false, &mut frame)
                .expect("noncanonical exact render payload should frame");
            let error = Pdu::decode(frame.as_slice())
                .expect_err("non-minimal LEB authority must fail canonical decode");
            assert!(
                format!("{error:#}")
                    .contains(&format!("{name} payload is not canonical varbincode")),
                "unexpected noncanonical authority rejection for {}: {:#}",
                name,
                error,
            );
        }

        for (ident, maximum) in [
            (91, MAX_EXACT_RENDER_REQUEST_DECOMPRESSED_BYTES),
            (92, MAX_EXACT_RENDER_DELIVERY_DECOMPRESSED_BYTES),
        ] {
            let oversized = vec![0_u8; maximum + 1];
            for (payload, compressed) in [
                (oversized.clone(), false),
                (
                    zstd::stream::encode_all(oversized.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)
                        .expect("oversized exact render fixture should compress"),
                    true,
                ),
            ] {
                let mut frame = Vec::new();
                encode_raw(ident, 9, &payload, compressed, &mut frame)
                    .expect("oversized exact render payload should frame");
                let error = Pdu::decode(frame.as_slice())
                    .expect_err("schema-specific decompressed ceiling must fail closed");
                assert!(
                    format!("{error:#}").contains(&format!("exceeds maximum {maximum}")),
                    "unexpected PDU {ident} decompressed-cap rejection: {error:#}",
                    ident = ident,
                    error = error,
                );
            }
        }
    }

    #[test]
    fn exact_render_frames_survive_every_fragment_boundary_and_coalescing() {
        let request = Pdu::GetPaneRenderDeliveryV1(sample_exact_render_request(
            ExactRenderDeliveryMode::Incremental,
        ));
        let response = Pdu::GetPaneRenderDeliveryV1Response(sample_exact_render_response(
            ExactRenderDeliveryOutcomeV1::ExactDelta(sample_exact_render_delta(123_456)),
        ));
        let request_frame = request
            .encode_frame_with_mode(71, CompressionMode::Never)
            .expect("request fixture must encode");
        let response_frame = response
            .encode_frame_with_mode(71, CompressionMode::Always)
            .expect("response fixture must encode");

        for split in 0..request_frame.len() {
            let mut buffer = StreamingPduBuffer::new();
            buffer.extend_from_slice(&request_frame[..split]);
            assert!(
                Pdu::stream_decode(&mut buffer)
                    .expect("incomplete request fragment must remain decodable")
                    .is_none(),
                "split {split} unexpectedly completed the request",
                split = split,
            );
            buffer.extend_from_slice(&request_frame[split..]);
            assert_eq!(
                Pdu::stream_decode(&mut buffer)
                    .expect("completed request fragment must decode")
                    .expect("completed request must be present")
                    .pdu,
                request
            );
            assert!(buffer.is_empty());
        }

        let mut coalesced = request_frame;
        coalesced.extend_from_slice(&response_frame);
        let mut buffer = StreamingPduBuffer::from(coalesced);
        assert_eq!(
            Pdu::stream_decode(&mut buffer)
                .expect("first coalesced frame must decode")
                .expect("request must be present")
                .pdu,
            request
        );
        assert_eq!(
            Pdu::stream_decode(&mut buffer)
                .expect("second coalesced frame must decode")
                .expect("response must be present")
                .pdu,
            response
        );
        assert!(buffer.is_empty());
    }

    #[test]
    fn ordered_window_v1_capabilities_are_known_but_not_advertised() {
        assert_eq!(
            TopologyCapabilities::ORDERED_WINDOW_STREAM_V1.bits(),
            1 << 1
        );
        assert_eq!(TopologyCapabilities::WINDOW_REORDER_CAS_V1.bits(), 1 << 2);
        assert_eq!(
            TopologyCapabilities::SERVER_SUPPORTED,
            TopologyCapabilities::FENCED_SNAPSHOT_V1,
            "codec knowledge must not activate ordered-window runtime support"
        );
        assert!(ordered_window_all_capabilities().validate().is_ok());
        assert_eq!(
            TopologyCapabilities::WINDOW_REORDER_CAS_V1.validate(),
            Err(TopologyCapabilitiesError::ReorderCasWithoutOrderedStream { bits: 1 << 2 })
        );
        let reorder_body_limit = PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_REORDER_WINDOW_TABS_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_REORDER_WINDOW_TABS_ZSTD_ENCODED_BYTES,
        };
        for ident in [88, 89] {
            let spec = Pdu::wire_spec_for_ident(ident)
                .expect("reorder request and response IDs must be assigned");
            assert_eq!(spec.encoded_body_limit, reorder_body_limit);
            assert_eq!(
                spec.encoded_body_limit.maximum_encoded_payload_bytes(false),
                MAX_REORDER_WINDOW_TABS_DECOMPRESSED_BYTES,
            );
            assert_eq!(
                spec.encoded_body_limit.maximum_encoded_payload_bytes(true),
                MAX_REORDER_WINDOW_TABS_ZSTD_ENCODED_BYTES,
            );
        }
        assert_eq!(
            MAX_REORDER_WINDOW_TABS_ZSTD_ENCODED_BYTES,
            512 * 1024,
            "compressed and decompressed reorder bodies share the frozen outer ceiling",
        );

        let (payload, compressed) = serialize_with_mode(
            &(
                ORDERED_WINDOW_PROTOCOL_VERSION,
                DomainBindingId::from_bytes([0x10; 16]),
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
    fn pdu87_snapshot_body_limit_is_frozen_and_scoped_to_the_response() {
        assert_eq!(MAX_STRUCTURALLY_VALID_ORDERED_WINDOW_SECTION_BYTES, 331_786);
        assert_eq!(MAX_ORDERED_WINDOW_SECTION_BYTES, 512 * 1024);
        assert_eq!(
            MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
            16 * 1024 * 1024,
        );
        assert_eq!(
            MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES,
            MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES
                + (MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES >> 8),
        );
        assert_eq!(
            MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES,
            zstd::zstd_safe::compress_bound(MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,),
            "frozen PDU 87 ceiling must continue to admit the pinned zstd encoder bound",
        );
        assert_eq!(
            MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES,
            encoded_frame_len(
                87,
                u64::MAX,
                MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES,
                true,
            )
            .expect("the maximum legal PDU 87 frame length must be representable"),
            "the dispatch reservation ceiling must include the complete worst-case frame",
        );

        let expected = PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES,
        };
        let response_spec =
            Pdu::wire_spec_for_ident(87).expect("ordered snapshot response ID must be assigned");
        assert_eq!(
            response_spec,
            &<ListPanesOrderedV1Response as PduWireIdent>::WIRE_SPEC,
        );
        assert_eq!(response_spec.encoded_body_limit, expected);
        assert_eq!(
            response_spec
                .encoded_body_limit
                .maximum_encoded_payload_bytes(false),
            MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
        );
        assert_eq!(
            response_spec
                .encoded_body_limit
                .maximum_encoded_payload_bytes(true),
            MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES,
        );

        for ident in [86, 90] {
            assert_eq!(
                Pdu::wire_spec_for_ident(ident)
                    .expect("adjacent ordered-window ID must be assigned")
                    .encoded_body_limit,
                PduEncodedBodyLimit::GlobalMaximum,
                "the PDU 87 total-body ceiling must not spread to ident {ident}",
            );
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn pdu87_validated_owner_and_public_encoder_each_scan_snapshot_once() {
        let mut samples = sample_ordered_window_pdus();
        let (_, Pdu::ListPanesOrderedV1(request)) = samples.remove(0) else {
            panic!("first ordered-window sample must be PDU 86");
        };
        let (_, Pdu::ListPanesOrderedV1Response(response)) = samples.remove(0) else {
            panic!("second ordered-window sample must be PDU 87");
        };

        debug_reset_ordered_snapshot_validation_passes();
        let public_frame = Pdu::ListPanesOrderedV1Response(response.clone())
            .encode_frame(0x871)
            .expect("ordinary public PDU 87 encoding must remain safe");
        assert_eq!(
            debug_ordered_snapshot_validation_passes(),
            OrderedSnapshotValidationPasses {
                pane_arena: 1,
                ordered_windows: 1,
            },
            "public pre-encode validation must authorize its exact field serialization",
        );

        debug_reset_ordered_snapshot_validation_passes();
        let validated = response
            .validate_for_request_owned(&request)
            .expect("sample PDU 87 must bind to its exact PDU 86");
        assert_eq!(
            debug_ordered_snapshot_validation_passes(),
            OrderedSnapshotValidationPasses {
                pane_arena: 1,
                ordered_windows: 1,
            },
            "request binding must perform the sole structural validation pass",
        );
        let validated_frame = validated
            .encode_frame(0x871)
            .expect("validated PDU 87 owner must encode");
        assert_eq!(
            debug_ordered_snapshot_validation_passes(),
            OrderedSnapshotValidationPasses {
                pane_arena: 1,
                ordered_windows: 1,
            },
            "validated-owner encoding must not rescan the arena or window graph",
        );
        assert_eq!(validated_frame, public_frame);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn pdu87_validated_owner_cannot_be_acquired_for_malformed_arena() {
        let mut samples = sample_ordered_window_pdus();
        let (_, Pdu::ListPanesOrderedV1(request)) = samples.remove(0) else {
            panic!("first ordered-window sample must be PDU 86");
        };
        let (_, Pdu::ListPanesOrderedV1Response(mut malformed)) = samples.remove(0) else {
            panic!("second ordered-window sample must be PDU 87");
        };
        let ListPanesOrderedV1Outcome::Snapshot(snapshot) = &mut malformed.outcome else {
            panic!("sample PDU 87 must contain a snapshot");
        };
        let (trees, mut nodes, window_titles) = snapshot.panes.clone().into_parts();
        nodes.push(PaneArenaNode::Empty);
        snapshot.panes = PaneArena::from_unvalidated_parts(trees, nodes, window_titles);

        debug_reset_ordered_snapshot_validation_passes();
        let error = malformed
            .clone()
            .validate_for_request_owned(&request)
            .expect_err("malformed arena must not acquire an encoding proof");
        assert!(matches!(
            error,
            OrderedWindowProtocolError::PaneArenaCardinalityMismatch { .. }
        ));
        assert_eq!(
            debug_ordered_snapshot_validation_passes(),
            OrderedSnapshotValidationPasses {
                pane_arena: 1,
                ordered_windows: 0,
            },
        );

        debug_reset_ordered_snapshot_validation_passes();
        let error = Pdu::ListPanesOrderedV1Response(malformed)
            .encode_frame(0x872)
            .expect_err("ordinary public encoding must also reject the malformed arena");
        assert!(error
            .downcast_ref::<OrderedWindowProtocolError>()
            .is_some_and(|error| matches!(
                error,
                OrderedWindowProtocolError::PaneArenaCardinalityMismatch { .. }
            )));
        assert_eq!(
            debug_ordered_snapshot_validation_passes(),
            OrderedSnapshotValidationPasses {
                pane_arena: 1,
                ordered_windows: 0,
            },
        );
    }

    #[test]
    fn pdu87_encoded_body_cap_rejects_limit_plus_one_before_body_allocation() {
        let spec = Pdu::wire_spec_for_ident(87).expect("PDU 87 must be assigned");
        for is_compressed in [false, true] {
            let limit = spec
                .encoded_body_limit
                .maximum_encoded_payload_bytes(is_compressed);
            let exact_header = declared_pdu_frame_header(87, 29, limit, is_compressed);
            let exact_error = decode_raw(exact_header.as_slice())
                .expect_err("admitted PDU 87 header without its body must reach the body read");
            assert!(
                exact_error
                    .downcast_ref::<PduEncodedBodyLimitExceeded>()
                    .is_none(),
                "exact PDU 87 boundary was rejected as oversized: {:#}",
                exact_error,
            );

            let plus = limit.checked_add(1).expect("PDU 87 limit has headroom");
            let plus_header = declared_pdu_frame_header(87, 29, plus, is_compressed);
            let plus_error = decode_raw(plus_header.as_slice())
                .expect_err("limit-plus-one PDU 87 header must fail before body allocation");
            let exceeded = plus_error
                .downcast_ref::<PduEncodedBodyLimitExceeded>()
                .expect("PDU 87 decoder must return the typed schema-body limit error");
            assert_eq!(exceeded.declared_payload_bytes(), plus);
            assert_eq!(exceeded.max_payload_bytes(), limit);
            assert_eq!(exceeded.serial(), 29);
            assert_eq!(exceeded.ident(), 87);
            assert_eq!(exceeded.is_compressed(), is_compressed);

            runtime::block_on(async {
                let mut exact_reader = runtime::Cursor::new(exact_header);
                let header = decode_raw_header_async(&mut exact_reader, Some(29))
                    .await
                    .expect("async PDU 87 header decoder must admit the exact boundary");
                assert_eq!(header.encoded_payload_len(), limit);
                assert_eq!(header.maximum_encoded_payload_bytes(), limit);

                let mut plus_reader = runtime::Cursor::new(plus_header);
                let plus_error = decode_raw_header_async(&mut plus_reader, Some(29))
                    .await
                    .expect_err("async limit-plus-one PDU 87 header must fail before allocation");
                let exceeded = plus_error
                    .downcast_ref::<PduEncodedBodyLimitExceeded>()
                    .expect("async PDU 87 decoder must return the typed body-limit error");
                assert_eq!(exceeded.declared_payload_bytes(), plus);
                assert_eq!(exceeded.max_payload_bytes(), limit);
                assert_eq!(exceeded.serial(), 29);
                assert_eq!(exceeded.ident(), 87);
                assert_eq!(exceeded.is_compressed(), is_compressed);
            });
        }
    }

    #[test]
    fn pdu87_exact_decoder_accepts_legal_payload_and_enforces_its_bound() {
        let mut samples = sample_ordered_window_pdus();
        let (_, Pdu::ListPanesOrderedV1Response(response)) = samples.remove(1) else {
            panic!("second ordered-window sample must be PDU 87");
        };
        let canonical =
            serialize_uncompressed(&response).expect("legal PDU 87 payload must serialize");
        let legal_len = canonical.len();
        let too_small_limit = legal_len
            .checked_sub(1)
            .expect("legal PDU 87 fixture must not be empty");

        for mode in [CompressionMode::Never, CompressionMode::Always] {
            let (payload, is_compressed) = serialize_with_mode(&response, mode)
                .expect("legal PDU 87 payload must serialize in either wire mode");
            assert!(matches!(
                (mode, is_compressed),
                (CompressionMode::Never, false) | (CompressionMode::Always, true)
            ));
            assert_eq!(
                deserialize_list_panes_ordered_v1_response(&payload, is_compressed)
                    .expect("the production PDU 87 decoder must accept a legal payload"),
                response,
            );
            assert_eq!(
                deserialize_exact_payload_with_limit::<ListPanesOrderedV1Response>(
                    &payload,
                    is_compressed,
                    "ListPanesOrderedV1Response",
                    legal_len,
                )
                .expect("the exact decompressed boundary must remain legal"),
                response,
            );

            let error = deserialize_exact_payload_with_limit::<ListPanesOrderedV1Response>(
                &payload,
                is_compressed,
                "ListPanesOrderedV1Response",
                too_small_limit,
            )
            .expect_err("one byte below a legal PDU 87 body must fail closed");
            assert!(
                format!("{error:#}").contains(&format!("maximum {too_small_limit}")),
                "unexpected bounded PDU 87 rejection under {:?}: {:#}",
                mode,
                error,
            );
        }
    }

    #[test]
    fn ordered_snapshot_flat_schema_is_canonical_and_section_markers_are_zero_wire() {
        let mut panes = ListPanesResponse {
            tabs: vec![PaneNode::Leaf(mux::tab::PaneEntry {
                window_id: 7,
                tab_id: 11,
                pane_id: 13,
                title: "wire-golden-pane".to_string(),
                size: TerminalSize::default(),
                working_dir: None,
                alt_screen_active: false,
                is_active_pane: true,
                is_zoomed_pane: false,
                workspace: "wire-golden".to_string(),
                cursor_pos: StableCursorPosition::default(),
                physical_top: 0,
                top_row: 0,
                left_col: 0,
                tty_name: Some("tty-wire-golden".to_string()),
            })],
            tab_titles: vec!["wire-golden-tab".to_string()],
            window_titles: HashMap::from([
                (9, "wire-golden-window-nine".to_string()),
                (7, "wire-golden-window-seven".to_string()),
            ]),
            floating_panes: Vec::new(),
        };
        let legacy_panes =
            serialize_uncompressed(&panes).expect("serialize legacy pane representation");
        let arena = ordered_pane_arena_from_list_panes(panes.clone())
            .expect("convert canonical pane representation");
        let flat_panes = serialize_uncompressed(&OrderedPanesTestWire(&arena))
            .expect("serialize canonical flat pane representation");
        assert_ne!(
            flat_panes, legacy_panes,
            "codec v54 must not silently retain the recursive PDU87 pane wire schema",
        );
        panes.window_titles = HashMap::from([
            (7, "wire-golden-window-seven".to_string()),
            (9, "wire-golden-window-nine".to_string()),
        ]);
        let reordered_arena = ordered_pane_arena_from_list_panes(panes)
            .expect("convert reordered title map through canonical flat schema");
        assert_eq!(
            serialize_uncompressed(&OrderedPanesTestWire(&reordered_arena))
                .expect("serialize reordered title map through canonical flat schema"),
            flat_panes,
            "PDU87 window-title bytes must not depend on HashMap iteration order",
        );

        let windows = [sample_ordered_window()];
        let section = serialize_uncompressed(&windows.as_slice())
            .expect("serialize legacy ordered-window section");
        let legacy_section_wire =
            serialize_uncompressed(&section).expect("serialize legacy section byte vector");
        let marked_section_wire = serialize_uncompressed(&OrderedWindowSectionTestWire(&windows))
            .expect("serialize schema-marked section bytes");
        assert_eq!(
            marked_section_wire, legacy_section_wire,
            "serde newtype admission markers must add no section wire bytes",
        );
    }

    #[test]
    fn pdu87_v54_flat_split_tree_has_pinned_wire_digest() {
        let panes = ListPanesResponse {
            tabs: vec![PaneNode::Split {
                left: Box::new(PaneNode::Leaf(sample_pane_entry(0))),
                right: Box::new(PaneNode::Leaf(sample_pane_entry(1))),
                node: sample_split(),
            }],
            tab_titles: vec!["golden-tab".to_string()],
            window_titles: HashMap::from([(1, "golden-window".to_string())]),
            floating_panes: Vec::new(),
        };
        let arena =
            ordered_pane_arena_from_list_panes(panes).expect("convert v54 split-tree golden body");
        let encoded = serialize_uncompressed(&OrderedPanesTestWire(&arena))
            .expect("serialize v54 split-tree golden body");
        let digest: [u8; 32] = Sha256::digest(&encoded).into();
        assert_eq!(
            digest,
            [
                5, 105, 80, 21, 39, 242, 241, 54, 63, 62, 77, 47, 13, 44, 132, 181, 19, 99, 69, 88,
                146, 84, 162, 57, 142, 211, 77, 188, 7, 196, 84, 219,
            ],
            "v54 flat ordered-pane bytes changed without an explicit codec-version review",
        );

        let mut reader = encoded.as_slice();
        let OrderedPanesTestOwned(decoded) = bounded_varbincode::deserialize(&mut reader)
            .expect("decode the exact v54 split-tree golden body");
        assert!(reader.is_empty());
        assert_eq!(decoded, arena);
    }

    #[test]
    fn pdu87_flat_pane_arena_accepts_depth_boundary_and_rejects_plus_one() {
        let panes = ListPanesResponse {
            tabs: vec![left_deep_pane_tree(MAX_ORDERED_PANE_TREE_DEPTH)],
            tab_titles: vec!["depth-boundary".to_string()],
            window_titles: HashMap::from([(1, "window-1".to_string())]),
            floating_panes: Vec::new(),
        };
        let arena = ordered_pane_arena_from_list_panes(panes)
            .expect("maximum admitted pane-tree depth must flatten");
        let encoded = serialize_uncompressed(&OrderedPanesTestWire(&arena))
            .expect("maximum admitted pane-tree depth must serialize");
        let mut reader = encoded.as_slice();
        let OrderedPanesTestOwned(decoded) = bounded_varbincode::deserialize(&mut reader)
            .expect("maximum admitted pane-tree depth must decode iteratively");
        assert!(
            reader.is_empty(),
            "flat pane DTO must consume its exact bytes"
        );
        assert_eq!(decoded, arena);

        let too_deep = ListPanesResponse {
            tabs: vec![left_deep_pane_tree(MAX_ORDERED_PANE_TREE_DEPTH + 1)],
            tab_titles: vec!["depth-plus-one".to_string()],
            window_titles: HashMap::new(),
            floating_panes: Vec::new(),
        };
        let error = ordered_pane_arena_from_list_panes(too_deep)
            .expect_err("depth-plus-one pane tree must fail before wire emission");
        assert!(
            error.to_string().contains("has 65 levels; maximum is 64"),
            "unexpected depth-plus-one rejection: {:#}",
            error,
        );
    }

    #[test]
    fn pdu87_flat_pane_arena_roundtrips_broad_tree_at_exact_leaf_and_node_limits() {
        let panes = ListPanesResponse {
            tabs: vec![broad_pane_tree(MAX_ORDERED_PANE_LEAVES_PER_TREE)],
            tab_titles: vec!["broad-boundary".to_string()],
            window_titles: HashMap::from([(1, "window-1".to_string())]),
            floating_panes: Vec::new(),
        };
        let flat = flatten_ordered_panes(panes)
            .expect("exact broad-tree resource boundaries must flatten");
        assert_eq!(
            flat.panes.trees()[0].node_count as usize,
            MAX_ORDERED_PANE_NODES_PER_TREE
        );
        assert_eq!(flat.panes.nodes().len(), MAX_ORDERED_PANE_NODES_PER_TREE);
        assert_eq!(
            flat.panes
                .nodes()
                .iter()
                .filter(|node| matches!(node, PaneArenaNode::Leaf(_)))
                .count(),
            MAX_ORDERED_PANE_LEAVES_PER_TREE,
        );

        let encoded = serialize_uncompressed(&OrderedPanesTestWire(&flat.panes))
            .expect("exact broad-tree resource boundaries must serialize");
        let mut reader = encoded.as_slice();
        let OrderedPanesTestOwned(decoded) = bounded_varbincode::deserialize(&mut reader)
            .expect("exact broad-tree resource boundaries must decode");
        assert!(reader.is_empty());
        assert_eq!(decoded, flat.panes);
    }

    #[test]
    fn pdu87_flat_pane_arena_roundtrips_exact_snapshot_node_and_leaf_limits() {
        let slot_counts = [4_096_usize, 4_096, 4_096, 2_049, 2_049];
        let empty_counts = [0_usize, 0, 0, 1, 1];
        let mut ordinal_base = 0_usize;
        let tabs = slot_counts
            .iter()
            .copied()
            .zip(empty_counts.iter().copied())
            .map(|(slots, empty_slots)| {
                let tree = pane_tree_with_slots(slots, empty_slots, ordinal_base);
                ordinal_base = ordinal_base.saturating_add(slots);
                tree
            })
            .collect::<Vec<_>>();
        let panes = ListPanesResponse {
            tab_titles: (0..tabs.len())
                .map(|ordinal| format!("boundary-tab-{ordinal}"))
                .collect(),
            tabs,
            window_titles: HashMap::new(),
            floating_panes: Vec::new(),
        };
        let flat = flatten_ordered_panes(panes)
            .expect("exact snapshot node and leaf ceilings must flatten");
        assert_eq!(
            flat.panes.nodes().len(),
            MAX_ORDERED_PANE_NODES_PER_SNAPSHOT
        );
        assert_eq!(flat.stats.node_visits, MAX_ORDERED_PANE_NODES_PER_SNAPSHOT);
        assert_eq!(flat.stats.leaf_visits, MAX_ORDERED_PANE_LEAVES_PER_SNAPSHOT);

        reset_test_bounded_serialize_growth_events();
        let encoded = serialize_uncompressed_bounded(
            &OrderedPanesTestWire(&flat.panes),
            MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
            87,
            87,
        )
        .expect("exact snapshot node and leaf ceilings must serialize");
        assert!(test_bounded_serialize_growth_events() <= 24);
        assert!(
            test_bounded_serialize_max_requested_capacity()
                <= MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
        );
        let mut reader = encoded.as_slice();
        let OrderedPanesTestOwned(decoded) = bounded_varbincode::deserialize(&mut reader)
            .expect("exact snapshot node and leaf ceilings must decode");
        assert!(reader.is_empty());
        assert_eq!(decoded, flat.panes);
    }

    #[test]
    fn pdu87_flat_pane_arena_rejects_noncanonical_indices_and_trailing_nodes() {
        let valid = sample_flat_tree();
        validate_ordered_pane_arena(&valid).expect("control flat tree must be canonical");

        let (trees, mut nodes, window_titles) = sample_flat_tree().into_parts();
        let PaneArenaNode::Split { right, .. } = &mut nodes[0] else {
            unreachable!("control root is split");
        };
        *right = 1;
        let duplicate_child = PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
        assert!(matches!(
            validate_ordered_pane_arena(&duplicate_child),
            Err(OrderedWindowProtocolError::InvalidPaneArenaNode { .. })
        ));

        let (trees, mut nodes, window_titles) = sample_flat_tree().into_parts();
        let PaneArenaNode::Split { right, .. } = &mut nodes[0] else {
            unreachable!("control root is split");
        };
        *right = 0;
        let back_edge = PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
        assert!(matches!(
            validate_ordered_pane_arena(&back_edge),
            Err(OrderedWindowProtocolError::InvalidPaneArenaNode { .. })
        ));

        let (trees, mut nodes, window_titles) = sample_flat_tree().into_parts();
        let PaneArenaNode::Split { right, .. } = &mut nodes[0] else {
            unreachable!("control root is split");
        };
        *right = 3;
        let out_of_range = PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
        assert!(matches!(
            validate_ordered_pane_arena(&out_of_range),
            Err(OrderedWindowProtocolError::InvalidPaneArenaNode { .. })
        ));

        let (trees, mut nodes, window_titles) = sample_flat_tree().into_parts();
        nodes.push(PaneArenaNode::Leaf(sample_pane_entry(2)));
        let trailing = PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
        assert!(matches!(
            validate_ordered_pane_arena(&trailing),
            Err(OrderedWindowProtocolError::PaneArenaCardinalityMismatch {
                referenced: 3,
                total: 4,
            })
        ));
    }

    #[test]
    fn pdu87_rejects_malformed_flat_graphs_in_every_wire_mode() {
        let cases = [
            (
                HostileOrderedSnapshotField::DuplicatePaneChildIndex,
                "pane tree 0 node 0 is invalid",
            ),
            (
                HostileOrderedSnapshotField::PaneBackEdge,
                "pane tree 0 node 0 is invalid",
            ),
            (
                HostileOrderedSnapshotField::PaneChildOutOfRange,
                "pane tree 0 node 0 is invalid",
            ),
            (
                HostileOrderedSnapshotField::TrailingPaneArenaNode,
                "pane arena references 3 nodes but carries 4",
            ),
        ];
        for (field, expected_error) in cases {
            let canonical = hostile_ordered_snapshot_response_body(field);
            for (payload, is_compressed) in [
                (canonical.clone(), false),
                (
                    zstd::stream::encode_all(canonical.as_slice(), 1)
                        .expect("compress malformed flat PDU87 body"),
                    true,
                ),
            ] {
                let mut frame = Vec::new();
                encode_raw(87, 41, &payload, is_compressed, &mut frame)
                    .expect("frame malformed flat PDU87 body");
                let error = Pdu::decode(frame.as_slice())
                    .expect_err("malformed flat PDU87 graph must fail closed");
                assert!(
                    format!("{error:#}").contains(expected_error),
                    "unexpected {:?} rejection compressed={}: {:#}",
                    field,
                    is_compressed,
                    error,
                );
            }
        }
    }

    #[test]
    fn pdu87_flat_pane_arena_checks_every_admitted_chain_depth() {
        for depth in 1..=MAX_ORDERED_PANE_TREE_DEPTH {
            let panes = ListPanesResponse {
                tabs: vec![left_deep_pane_tree(depth)],
                tab_titles: vec![format!("depth-{depth}")],
                window_titles: HashMap::new(),
                floating_panes: Vec::new(),
            };
            let flat = flatten_ordered_panes(panes)
                .unwrap_or_else(|error| panic!("admitted depth {} failed: {error:#}", depth));
            assert_eq!(flat.panes.trees().len(), 1);
            assert_eq!(
                flat.panes.nodes().len(),
                depth.saturating_mul(2).saturating_sub(1)
            );
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn pdu87_flat_pane_arena_converts_every_generated_admitted_chain(
            left_branch in proptest::collection::vec(any::<bool>(), 0..MAX_ORDERED_PANE_TREE_DEPTH)
        ) {
            let mut tree = PaneNode::Leaf(sample_pane_entry(0));
            for put_existing_on_left in left_branch {
                tree = if put_existing_on_left {
                    PaneNode::Split {
                        left: Box::new(tree),
                        right: Box::new(PaneNode::Empty),
                        node: sample_split(),
                    }
                } else {
                    PaneNode::Split {
                        left: Box::new(PaneNode::Empty),
                        right: Box::new(tree),
                        node: sample_split(),
                    }
                };
            }
            let panes = ListPanesResponse {
                tabs: vec![tree],
                tab_titles: vec!["generated-chain".to_string()],
                window_titles: HashMap::new(),
                floating_panes: Vec::new(),
            };
            let arena = ordered_pane_arena_from_list_panes(panes)
                .unwrap_or_else(|error| {
                    panic!("generated admitted chain failed to flatten: {:#}", error)
                });
            let encoded = serialize_uncompressed(&OrderedPanesTestWire(&arena))
                .unwrap_or_else(|error| {
                    panic!("generated admitted chain failed to serialize: {:#}", error)
                });
            let mut reader = encoded.as_slice();
            let OrderedPanesTestOwned(decoded) = bounded_varbincode::deserialize(&mut reader)
                .unwrap_or_else(|error| {
                    panic!("generated admitted chain failed to decode: {}", error)
                });
            prop_assert!(reader.is_empty());
            prop_assert_eq!(decoded, arena);
        }
    }

    #[test]
    fn pdu87_flat_pane_encoder_has_deterministic_q_scale_work_and_allocation_counts() {
        for q in [1_usize, 20, 200, 4_096] {
            let panes = ListPanesResponse {
                tabs: vec![pane_tree_with_slots(q, 0, 0)],
                tab_titles: vec![format!("split-heavy-q-{q}")],
                window_titles: HashMap::new(),
                floating_panes: Vec::new(),
            };
            let first = flatten_ordered_panes(panes.clone())
                .unwrap_or_else(|error| panic!("q={} first flatten failed: {error:#}", q));
            let second = flatten_ordered_panes(panes)
                .unwrap_or_else(|error| panic!("q={} second flatten failed: {error:#}", q));

            assert_eq!(
                first.stats, second.stats,
                "q={} flatten work must be deterministic across repetitions",
                q,
            );
            assert_eq!(first.stats.node_visits, q * 2 - 1);
            assert_eq!(first.stats.leaf_visits, q);
            assert_eq!(first.stats.task_pushes, q.saturating_sub(1) * 3 + 1);
            assert!(first.stats.peak_pending_tasks <= MAX_ORDERED_PANE_TREE_DEPTH);
            assert!(first.stats.flatten_allocation_requests <= 24);
            assert_eq!(first.panes.trees().len(), 1);
            assert_eq!(first.panes.nodes().len(), q * 2 - 1);
            assert_eq!(first.panes, second.panes);

            reset_test_bounded_serialize_growth_events();
            let encoded = serialize_uncompressed_bounded(
                &OrderedPanesTestWire(&first.panes),
                MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
                87,
                87,
            )
            .unwrap_or_else(|error| panic!("q={} serialization failed: {error:#}", q));
            let growth_events = test_bounded_serialize_growth_events();
            assert!(
                growth_events <= 24,
                "q={} serialization used {} growth events",
                q,
                growth_events,
            );
            assert!(
                test_bounded_serialize_max_requested_capacity()
                    <= MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
            );
            let mut reader = encoded.as_slice();
            let OrderedPanesTestOwned(decoded) = bounded_varbincode::deserialize(&mut reader)
                .unwrap_or_else(|error| panic!("q={} decode failed: {error}", q));
            assert!(reader.is_empty());
            assert_eq!(decoded, first.panes);
        }
    }

    #[test]
    fn pdu87_rejects_hostile_collection_prefixes_before_elements_in_every_mode() {
        let cases = [
            (
                HostileOrderedSnapshotField::PaneTreeDescriptors,
                "ordered pane tree descriptors length 16385 exceeds maximum 16384",
            ),
            (
                HostileOrderedSnapshotField::PaneArenaNodes,
                "ordered pane arena nodes length 32768 exceeds maximum 32767",
            ),
            (
                HostileOrderedSnapshotField::PaneWindowTitles,
                "ordered pane window titles length 4097 exceeds maximum 4096",
            ),
            (
                HostileOrderedSnapshotField::OrderedSectionBytes,
                "ordered-window section bytes length 524289 exceeds maximum 524288",
            ),
            (
                HostileOrderedSnapshotField::OrderedWindows,
                "ordered windows length 4097 exceeds maximum 4096",
            ),
            (
                HostileOrderedSnapshotField::OrderedTabIds,
                "ordered tab ids length 4097 exceeds maximum 4096",
            ),
        ];

        for (field, expected_error) in cases {
            let canonical = hostile_ordered_snapshot_response_body(field);
            assert!(
                canonical.len() < 1024,
                "{:?} fixture must contain only hostile prefixes, not materialized elements",
                field,
            );
            for (payload, is_compressed) in [
                (canonical.clone(), false),
                (
                    zstd::stream::encode_all(canonical.as_slice(), 1)
                        .expect("compress prefix-only hostile PDU 87 body"),
                    true,
                ),
            ] {
                let mut frame = Vec::new();
                encode_raw(87, 39, &payload, is_compressed, &mut frame)
                    .expect("frame prefix-only hostile PDU 87 body");
                let error = Pdu::decode(frame.as_slice())
                    .expect_err("hostile PDU 87 collection prefix must fail closed");
                assert!(
                    format!("{error:#}").contains(expected_error),
                    "unexpected {:?} rejection compressed={}: {:#}",
                    field,
                    is_compressed,
                    error,
                );
            }
        }
    }

    #[test]
    fn pdu87_rejects_duplicate_window_title_keys_in_every_mode() {
        let canonical = hostile_ordered_snapshot_response_body(
            HostileOrderedSnapshotField::DuplicatePaneWindowTitle,
        );
        for (payload, is_compressed) in [
            (canonical.clone(), false),
            (
                zstd::stream::encode_all(canonical.as_slice(), 1)
                    .expect("compress duplicate-window-title PDU 87 body"),
                true,
            ),
        ] {
            let mut frame = Vec::new();
            encode_raw(87, 40, &payload, is_compressed, &mut frame)
                .expect("frame duplicate-window-title PDU 87 body");
            let error = Pdu::decode(frame.as_slice())
                .expect_err("duplicate PDU 87 window-title key must fail closed");
            assert!(
                format!("{error:#}").contains(
                    "ordered-window pane window titles are not in strictly increasing window-id order"
                ),
                "unexpected duplicate-key rejection compressed={}: {:#}",
                is_compressed,
                error,
            );
        }
    }

    #[test]
    fn maximum_width_ordered_section_stays_below_its_structural_proof() {
        const TABS_PER_WINDOW: usize =
            MAX_ORDERED_TABS_PER_SNAPSHOT / MAX_ORDERED_WINDOWS_PER_SNAPSHOT;
        let mut windows = Vec::with_capacity(MAX_ORDERED_WINDOWS_PER_SNAPSHOT);
        for window_offset in 0..MAX_ORDERED_WINDOWS_PER_SNAPSHOT {
            let ordered_tab_ids = (0..TABS_PER_WINDOW)
                .map(|tab_offset| {
                    let ordinal = window_offset * TABS_PER_WINDOW + tab_offset;
                    RemoteTabId::new(
                        (u64::MAX - 1)
                            - u64::try_from(ordinal).expect("bounded tab ordinal fits in u64"),
                    )
                })
                .collect::<Vec<_>>();
            windows.push(OrderedWindowStateV1 {
                window_id: RemoteWindowId::new(
                    (u64::MAX - 1)
                        - u64::try_from(window_offset).expect("bounded window ordinal fits in u64"),
                ),
                order_revision: WindowOrderRevision::new(u64::MAX - 1),
                active_tab_id: ordered_tab_ids.first().copied(),
                ordered_tab_ids,
            });
        }
        validate_ordered_windows_structure(&windows, false)
            .expect("maximum-width maximum-cardinality section must be structurally legal");
        let encoded = encoded_ordered_window_section_len(&windows)
            .expect("maximum-width ordered section must have a representable length");
        assert!(
            encoded <= MAX_STRUCTURALLY_VALID_ORDERED_WINDOW_SECTION_BYTES,
            "maximum-width section {encoded} exceeded conservative proof {}",
            MAX_STRUCTURALLY_VALID_ORDERED_WINDOW_SECTION_BYTES,
        );
        assert!(encoded <= MAX_ORDERED_WINDOW_SECTION_BYTES);
    }

    #[test]
    fn pdu87_producer_and_expansion_decoder_enforce_total_body_ceiling() {
        let mut samples = sample_ordered_window_pdus();
        let (_, Pdu::ListPanesOrderedV1Response(mut response)) = samples.remove(1) else {
            panic!("second ordered-window sample must be PDU 87");
        };
        {
            let ListPanesOrderedV1Outcome::Snapshot(snapshot) = &mut response.outcome else {
                panic!("sample PDU 87 must carry a snapshot");
            };
            let window = snapshot
                .ordered_windows
                .first_mut()
                .expect("sample PDU 87 must carry one ordered window");
            window.window_id = RemoteWindowId::new(7);
            for (offset, tab_id) in window.ordered_tab_ids.iter_mut().enumerate() {
                *tab_id = RemoteTabId::new(
                    u64::try_from(offset + 11).expect("small test tab id fits u64"),
                );
            }
            window.active_tab_id = window.ordered_tab_ids.first().copied();
            let (remote_window_id, remote_active_tab_id, remote_tab_ids) = {
                let window = snapshot
                    .ordered_windows
                    .first()
                    .expect("sample PDU 87 must carry one ordered window");
                (
                    window.window_id,
                    window.active_tab_id,
                    window.ordered_tab_ids.clone(),
                )
            };
            let window_id = remote_window_id
                .try_into_usize()
                .expect("sample remote window id must fit this mux target");
            let tabs = remote_tab_ids
                .iter()
                .map(|remote_tab_id| {
                    let tab_id = remote_tab_id
                        .try_into_usize()
                        .expect("sample remote tab id must fit this mux target");
                    PaneNode::Leaf(mux::tab::PaneEntry {
                        window_id,
                        tab_id,
                        pane_id: tab_id,
                        title: format!("pane-{tab_id}"),
                        size: TerminalSize::default(),
                        working_dir: None,
                        alt_screen_active: false,
                        is_active_pane: Some(*remote_tab_id) == remote_active_tab_id,
                        is_zoomed_pane: false,
                        workspace: "body-limit-fixture".to_string(),
                        cursor_pos: StableCursorPosition::default(),
                        physical_top: 0,
                        top_row: 0,
                        left_col: 0,
                        tty_name: Some(format!("tty-{tab_id}")),
                    })
                })
                .collect();
            let mut tab_titles = remote_tab_ids
                .iter()
                .map(|remote_tab_id| format!("tab-{}", remote_tab_id.get()))
                .collect::<Vec<_>>();
            tab_titles[0].clear();
            snapshot.panes = ordered_pane_arena_from_list_panes(ListPanesResponse {
                tabs,
                tab_titles,
                window_titles: HashMap::from([(window_id, format!("window-{window_id}"))]),
                floating_panes: Vec::new(),
            })
            .expect("body-limit fixture must flatten");
        }
        let empty_title_body =
            serialize_uncompressed(&response).expect("baseline PDU 87 fixture must serialize");
        let target_body_bytes = MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES + 1;
        let fixed_body_bytes = empty_title_body
            .len()
            .checked_sub(encoded_length(0))
            .expect("empty title length prefix must be present");
        let target_body_bytes_u64 =
            u64::try_from(target_body_bytes).expect("PDU 87 body limit fits in u64");
        let initial_overhead = fixed_body_bytes
            .checked_add(encoded_length(target_body_bytes_u64))
            .expect("PDU 87 fixture overhead is representable");
        let mut title_bytes = target_body_bytes
            .checked_sub(initial_overhead)
            .expect("PDU 87 fixture overhead must fit beneath the target");
        loop {
            let next = target_body_bytes
                .checked_sub(
                    fixed_body_bytes
                        .checked_add(encoded_length(
                            u64::try_from(title_bytes).expect("title length fits in u64"),
                        ))
                        .expect("PDU 87 fixture overhead is representable"),
                )
                .expect("PDU 87 fixture overhead must fit beneath the target");
            if next == title_bytes {
                break;
            }
            title_bytes = next;
        }
        let ListPanesOrderedV1Outcome::Snapshot(snapshot) = &mut response.outcome else {
            unreachable!("sample PDU 87 snapshot outcome was not replaced");
        };
        let (mut trees, nodes, window_titles) = snapshot.panes.clone().into_parts();
        trees[0].tab_title = "x".repeat(title_bytes);
        snapshot.panes = PaneArena::from_unvalidated_parts(trees, nodes, window_titles);
        response
            .validate()
            .expect("the oversized fixture must remain structurally valid");

        let canonical =
            serialize_uncompressed(&response).expect("oversized PDU 87 fixture must serialize");
        assert_eq!(
            canonical.len(),
            target_body_bytes,
            "the expansion fixture must cross the aggregate cap by exactly one byte",
        );

        let oversized = Pdu::ListPanesOrderedV1Response(response);
        for mode in [CompressionMode::Never, CompressionMode::Always] {
            let error = oversized
                .encode_frame_with_mode(37, mode)
                .expect_err("PDU 87 producer must reject an oversized canonical body");
            let exceeded = error
                .downcast_ref::<PduEncodedBodyLimitExceeded>()
                .expect("PDU 87 producer must retain the typed body-limit error");
            assert_eq!(exceeded.serial(), 37);
            assert_eq!(exceeded.ident(), 87);
            assert!(!exceeded.is_compressed());
            assert_eq!(
                exceeded.max_payload_bytes(),
                MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
            );
            assert_eq!(exceeded.declared_payload_bytes(), target_body_bytes);
        }

        let compressed = zstd::stream::encode_all(canonical.as_slice(), 1)
            .expect("compress highly repetitive oversized PDU 87 fixture");
        assert!(
            compressed.len() < MAX_LIST_PANES_ORDERED_V1_RESPONSE_ZSTD_ENCODED_BYTES,
            "the expansion fixture must pass the encoded-body admission ceiling",
        );
        let error = deserialize_list_panes_ordered_v1_response(&compressed, true)
            .expect_err("bounded PDU 87 decompression must reject limit-plus-one output");
        assert!(
            format!("{error:#}").contains(&format!(
                "decompressed payload size exceeds maximum {}",
                MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
            )),
            "unexpected PDU 87 expansion rejection: {:#}",
            error,
        );
    }

    #[test]
    fn pdu87_dense_max_cardinality_snapshot_roundtrips_under_total_body_cap() {
        const TABS_PER_REPRESENTATIVE_WINDOW: usize =
            MAX_ORDERED_TABS_PER_SNAPSHOT / MAX_ORDERED_WINDOWS_PER_SNAPSHOT;
        assert_eq!(TABS_PER_REPRESENTATIVE_WINDOW, 4);

        let mut ordered_windows = Vec::with_capacity(MAX_ORDERED_WINDOWS_PER_SNAPSHOT);
        let mut tabs = Vec::with_capacity(MAX_ORDERED_TABS_PER_SNAPSHOT);
        let mut tab_titles = Vec::with_capacity(MAX_ORDERED_TABS_PER_SNAPSHOT);
        let mut window_titles = HashMap::with_capacity(MAX_ORDERED_WINDOWS_PER_SNAPSHOT);

        for window_offset in 0..MAX_ORDERED_WINDOWS_PER_SNAPSHOT {
            let window_id = window_offset + 1;
            let first_tab_offset = window_offset * TABS_PER_REPRESENTATIVE_WINDOW;
            let ordered_tab_ids = (0..TABS_PER_REPRESENTATIVE_WINDOW)
                .map(|tab_offset| {
                    let tab_id = first_tab_offset + tab_offset + 1;
                    RemoteTabId::new(u64::try_from(tab_id).expect("bounded tab id fits in u64"))
                })
                .collect::<Vec<_>>();

            ordered_windows.push(OrderedWindowStateV1 {
                window_id: RemoteWindowId::new(
                    u64::try_from(window_id).expect("bounded window id fits in u64"),
                ),
                order_revision: WindowOrderRevision::INITIAL,
                active_tab_id: ordered_tab_ids.first().copied(),
                ordered_tab_ids,
            });
            window_titles.insert(window_id, format!("window-{window_id}"));

            for tab_offset in 0..TABS_PER_REPRESENTATIVE_WINDOW {
                let tab_id = first_tab_offset + tab_offset + 1;
                tab_titles.push(format!("tab-{tab_id}"));
                tabs.push(PaneNode::Leaf(mux::tab::PaneEntry {
                    window_id,
                    tab_id,
                    pane_id: tab_id,
                    title: format!("pane-{tab_id}"),
                    size: TerminalSize::default(),
                    working_dir: None,
                    alt_screen_active: false,
                    is_active_pane: tab_offset == 0,
                    is_zoomed_pane: false,
                    workspace: "dense-large-session".to_string(),
                    cursor_pos: StableCursorPosition::default(),
                    physical_top: 0,
                    top_row: 0,
                    left_col: 0,
                    tty_name: Some(format!("tty-{tab_id}")),
                }));
            }
        }

        let response = ListPanesOrderedV1Response {
            protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: DomainBindingId::from_bytes([0x81; 16]),
            negotiated: ordered_window_foundation_capabilities(),
            stream_id: TopologyStreamId::from_bytes([0x82; 16]),
            outcome: ListPanesOrderedV1Outcome::Snapshot(OrderedPaneSnapshotV1 {
                session_incarnation: MuxSessionIncarnation::from_bytes([0x83; 16]),
                topology_revision: TopologyRevision::new(1),
                panes: ordered_pane_arena_from_list_panes(ListPanesResponse {
                    tabs,
                    tab_titles,
                    window_titles,
                    floating_panes: Vec::new(),
                })
                .expect("dense maximum-cardinality pane listing must flatten"),
                floating_panes: Vec::new(),
                ordered_windows,
            }),
        };
        response
            .validate()
            .expect("dense maximum-cardinality snapshot must be structurally valid");
        let pdu = Pdu::ListPanesOrderedV1Response(response);
        let frame = pdu
            .encode_frame_with_mode(38, CompressionMode::Never)
            .expect("dense maximum-cardinality PDU 87 must fit its body ceiling");
        assert!(
            frame.len() <= MAX_LIST_PANES_ORDERED_V1_RESPONSE_FRAME_BYTES,
            "dense PDU 87 complete frame exceeded its dispatch ceiling",
        );
        let raw = decode_raw(frame.as_slice()).expect("decode dense PDU 87 frame");
        assert_eq!(raw.ident, 87);
        assert_eq!(raw.serial, 38);
        assert!(!raw.is_compressed);
        assert!(
            raw.data.len() <= MAX_LIST_PANES_ORDERED_V1_RESPONSE_DECOMPRESSED_BYTES,
            "dense PDU 87 body exceeded its schema ceiling",
        );
        drop(raw);
        let decoded =
            Pdu::decode(frame.as_slice()).expect("roundtrip dense maximum-cardinality PDU 87");
        let Pdu::ListPanesOrderedV1Response(expected) = pdu else {
            unreachable!("fixture was constructed as PDU 87");
        };
        let Pdu::ListPanesOrderedV1Response(actual) = decoded.pdu else {
            panic!("dense maximum-cardinality frame must decode as PDU 87");
        };
        assert_eq!(actual.protocol_version, expected.protocol_version);
        assert_eq!(actual.domain_binding_id, expected.domain_binding_id);
        assert_eq!(actual.negotiated, expected.negotiated);
        assert_eq!(actual.stream_id, expected.stream_id);
        let (
            ListPanesOrderedV1Outcome::Snapshot(actual),
            ListPanesOrderedV1Outcome::Snapshot(expected),
        ) = (actual.outcome, expected.outcome)
        else {
            panic!("dense maximum-cardinality PDU 87 must retain its snapshot outcome");
        };
        assert_eq!(actual.session_incarnation, expected.session_incarnation);
        assert_eq!(actual.topology_revision, expected.topology_revision);
        assert_eq!(actual.panes, expected.panes);
        assert!(actual
            .ordered_windows
            .iter()
            .eq(expected.ordered_windows.iter()));
    }

    #[test]
    fn ordered_snapshot_binding_is_nonzero_and_response_is_request_correlated() {
        let mut samples = sample_ordered_window_pdus();
        let (_, Pdu::ListPanesOrderedV1(request)) = samples.remove(0) else {
            panic!("first ordered-window sample must be PDU86");
        };
        let (_, Pdu::ListPanesOrderedV1Response(response)) = samples.remove(0) else {
            panic!("second ordered-window sample must be PDU87");
        };
        response
            .validate_for_request(&request)
            .expect("sample PDU87 must echo and satisfy its exact PDU86");

        let mut zero_request_binding = request.clone();
        zero_request_binding.domain_binding_id = DomainBindingId::from_bytes([0; 16]);
        assert!(matches!(
            zero_request_binding.validate(),
            Err(OrderedWindowProtocolError::ReservedIdentity {
                field: "domain_binding_id"
            })
        ));

        let mut zero_response_binding = response.clone();
        zero_response_binding.domain_binding_id = DomainBindingId::from_bytes([0; 16]);
        assert!(matches!(
            zero_response_binding.validate(),
            Err(OrderedWindowProtocolError::ReservedIdentity {
                field: "domain_binding_id"
            })
        ));

        let mut wrong_binding_echo = response.clone();
        wrong_binding_echo.domain_binding_id = DomainBindingId::from_bytes([0x66; 16]);
        assert!(matches!(
            wrong_binding_echo.validate_for_request(&request),
            Err(OrderedWindowProtocolError::DomainBindingEchoMismatch { .. })
        ));

        let foundation = ordered_window_foundation_capabilities();
        let mut foundation_only_request = request.clone();
        foundation_only_request.supported = foundation;
        foundation_only_request.required = foundation;
        let mut unoffered_response = response.clone();
        unoffered_response.negotiated = ordered_window_all_capabilities();
        assert!(matches!(
            unoffered_response.validate_for_request(&foundation_only_request),
            Err(OrderedWindowProtocolError::NegotiatedCapabilitiesNotOffered { .. })
        ));

        let mut reorder_required = request;
        reorder_required.required = ordered_window_all_capabilities();
        assert!(matches!(
            response.validate_for_request(&reorder_required),
            Err(OrderedWindowProtocolError::MissingNegotiatedCapabilities { .. })
        ));
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
            ReorderWindowTabsV1Outcome::Replay(WindowReorderTerminalOutcomeV1::Applied(
                commit.clone(),
            )),
            ReorderWindowTabsV1Outcome::Replay(WindowReorderTerminalOutcomeV1::Conflict(
                commit.clone(),
            )),
            ReorderWindowTabsV1Outcome::Replay(WindowReorderTerminalOutcomeV1::StaleIncarnation),
            ReorderWindowTabsV1Outcome::Replay(WindowReorderTerminalOutcomeV1::Malformed),
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
                0x19, 0x77, 0xaf, 0x8b, 0x79, 0xde, 0xaf, 0x45, 0x80, 0x8f, 0xef, 0x59, 0x9b, 0xee,
                0x58, 0xe4, 0xc7, 0xc2, 0xcd, 0xe0, 0x28, 0x6f, 0x2f, 0xdf, 0xd6, 0x12, 0x0a, 0x0d,
                0x2c, 0x4d, 0xde, 0xf2,
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
    fn ordered_window_v1_separates_wire_admission_from_mux_permutation_semantics() {
        let valid = sample_reorder_window_tabs_v1();
        assert_eq!(RemoteWindowId::new(0).try_into_usize(), Ok(0));
        assert_eq!(RemoteTabId::new(0).try_into_usize(), Ok(0));
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

        let mut active_zero_tab = zero_tab.clone();
        active_zero_tab.desired_active_tab_id = Some(RemoteTabId::new(0));
        active_zero_tab = active_zero_tab.with_computed_digest();

        for zero_is_valid in [zero_window, zero_tab, active_zero_tab] {
            let pdu = Pdu::ReorderWindowTabsV1(zero_is_valid);
            let frame = pdu
                .encode_frame_with_mode(6, CompressionMode::Never)
                .expect("live mux ID zero must encode");
            assert_eq!(
                Pdu::decode(frame.as_slice())
                    .expect("live mux ID zero must decode")
                    .pdu,
                pdu,
            );
        }

        let mut maximum_live_ids = valid.clone();
        maximum_live_ids.window_id = RemoteWindowId::new(u64::MAX - 1);
        maximum_live_ids.desired_tab_ids[0] = RemoteTabId::new(u64::MAX - 1);
        maximum_live_ids.desired_active_tab_id = Some(RemoteTabId::new(u64::MAX - 1));
        maximum_live_ids = maximum_live_ids.with_computed_digest();
        maximum_live_ids
            .validate()
            .expect("u64::MAX - 1 window, tab, and active identities must validate");
        let maximum_live_pdu = Pdu::ReorderWindowTabsV1(maximum_live_ids);
        let maximum_live_frame = maximum_live_pdu
            .encode_frame_with_mode(6, CompressionMode::Never)
            .expect("u64::MAX - 1 mux identities must encode");
        assert_eq!(
            Pdu::decode(maximum_live_frame.as_slice())
                .expect("u64::MAX - 1 mux identities must decode")
                .pdu,
            maximum_live_pdu,
        );

        let mut reserved_window = valid.clone();
        reserved_window.window_id = RemoteWindowId::new(u64::MAX);
        reserved_window = reserved_window.with_computed_digest();

        let mut reserved_tab = valid.clone();
        reserved_tab.desired_tab_ids[0] = RemoteTabId::new(u64::MAX);
        reserved_tab = reserved_tab.with_computed_digest();

        let mut zero_binding = valid.clone();
        zero_binding.domain_binding_id = DomainBindingId::from_bytes([0; 16]);
        zero_binding = zero_binding.with_computed_digest();

        let mut zero_stream = valid.clone();
        zero_stream.stream_id = TopologyStreamId::from_bytes([0; 16]);
        zero_stream = zero_stream.with_computed_digest();

        let mut zero_session = valid.clone();
        zero_session.session_incarnation = MuxSessionIncarnation::from_bytes([0; 16]);
        zero_session = zero_session.with_computed_digest();

        let mut zero_mutation_namespace = valid.clone();
        zero_mutation_namespace.mutation_id.namespace = [0; 16];
        zero_mutation_namespace = zero_mutation_namespace.with_computed_digest();

        let mut missing_active = valid.clone();
        missing_active.desired_active_tab_id = None;
        missing_active = missing_active.with_computed_digest();

        let mut foreign_active = valid.clone();
        foreign_active.desired_active_tab_id = Some(RemoteTabId::new(999_999));
        foreign_active = foreign_active.with_computed_digest();

        for (name, mux_semantic_decision) in [
            ("duplicate tab", duplicate_tab),
            ("missing active", missing_active),
            ("foreign active", foreign_active),
        ] {
            let pdu = Pdu::ReorderWindowTabsV1(mux_semantic_decision);
            let frame = pdu
                .encode_frame_with_mode(7, CompressionMode::Never)
                .unwrap_or_else(|error| {
                    panic!(
                        "{} must reach authoritative mux classification: {:#}",
                        name, error
                    )
                });
            assert_eq!(
                Pdu::decode(frame.as_slice())
                    .unwrap_or_else(|error| {
                        panic!("{} must survive bounded wire admission: {:#}", name, error)
                    })
                    .pdu,
                pdu,
            );
        }

        for (name, malformed) in [
            ("reserved window", reserved_window),
            ("reserved tab", reserved_tab),
            ("zero binding identity", zero_binding),
            ("zero stream identity", zero_stream),
            ("zero session identity", zero_session),
            ("zero mutation namespace", zero_mutation_namespace),
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
        let error =
            Pdu::decode(frame.as_slice()).expect_err("digest mismatch must fail during decode");
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
        let error =
            Pdu::decode(encode_window_order_event_unchecked(&duplicate_window, 9).as_slice())
                .expect_err("duplicate windows must fail receiver validation");
        assert!(format!("{error:#}").contains("repeats window id"));

        let mut second = first.clone();
        second.window_id = RemoteWindowId::new(first.window_id.get() + 1);
        let duplicate_tab = WindowOrderEventV1 {
            windows: vec![first, second],
            ..duplicate_window
        };
        let error = Pdu::decode(encode_window_order_event_unchecked(&duplicate_tab, 10).as_slice())
            .expect_err("a tab cannot appear in two ordered windows");
        assert!(format!("{error:#}").contains("repeats tab id"));
    }

    #[test]
    fn ordered_window_v1_exact_decoders_reject_trailing_and_truncated_bytes() {
        for mode in [CompressionMode::Never, CompressionMode::Always] {
            for (ident, pdu) in sample_ordered_window_pdus() {
                let frame = match &pdu {
                    Pdu::ListPanesOrderedV1(value) => {
                        encode_authority_payload_with_trailing_schema_byte(ident, 1, value, mode)
                    }
                    Pdu::ListPanesOrderedV1Response(value) => {
                        encode_authority_payload_with_trailing_schema_byte(ident, 1, value, mode)
                    }
                    Pdu::ReorderWindowTabsV1(value) => {
                        encode_authority_payload_with_trailing_schema_byte(ident, 1, value, mode)
                    }
                    Pdu::ReorderWindowTabsV1Response(value) => {
                        encode_authority_payload_with_trailing_schema_byte(ident, 1, value, mode)
                    }
                    Pdu::WindowOrderEventV1(value) => {
                        encode_authority_payload_with_trailing_schema_byte(ident, 1, value, mode)
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
        over_request.desired_tab_ids.push(RemoteTabId::new(50_000));
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
        let snapshot_at_limit = Pdu::ListPanesOrderedV1Response(ListPanesOrderedV1Response {
            protocol_version: ORDERED_WINDOW_PROTOCOL_VERSION,
            domain_binding_id: DomainBindingId::from_bytes([0x70; 16]),
            negotiated: ordered_window_foundation_capabilities(),
            stream_id: TopologyStreamId::from_bytes([0x71; 16]),
            outcome: ListPanesOrderedV1Outcome::Snapshot(OrderedPaneSnapshotV1 {
                session_incarnation: MuxSessionIncarnation::from_bytes([0x72; 16]),
                topology_revision: TopologyRevision::new(1),
                panes: ordered_pane_arena_from_list_panes(empty_pane_list())
                    .expect("empty ordered-pane arena must be valid"),
                floating_panes: Vec::new(),
                ordered_windows: windows_at_limit.clone(),
            }),
        });
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
                    .map(|id| RemoteTabId::new(u64::try_from(id).expect("bounded tab id fits u64")))
                    .collect();
                OrderedWindowStateV1 {
                    window_id: RemoteWindowId::new(
                        u64::try_from(window_offset + 1).expect("bounded window id fits u64"),
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
        validate_ordered_windows_with_section_limit(&one_window, false, exact_section_bytes)
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

        let oversized_compressed_body = vec![0_u8; MAX_REORDER_WINDOW_TABS_ZSTD_ENCODED_BYTES + 1];
        let mut oversized_compressed_frame = Vec::new();
        encode_raw(
            88,
            25,
            &oversized_compressed_body,
            true,
            &mut oversized_compressed_frame,
        )
        .expect("hostile compressed-body header fixture should frame");
        let error = Pdu::decode(oversized_compressed_frame.as_slice())
            .expect_err("compressed reorder body above zstd bound must fail at header admission");
        assert!(
            format!("{error:#}").contains(&MAX_REORDER_WINDOW_TABS_ZSTD_ENCODED_BYTES.to_string()),
            "unexpected compressed header rejection: {error:#}",
            error = error,
        );
    }

    #[test]
    fn codec_v58_requires_atomic_floating_snapshot_redeploy_and_retains_feature_minima() {
        assert_eq!(CODEC_VERSION, 58);
        assert_eq!(CODEC_VERSION_MIN_SUPPORTED, 58);
        assert_eq!(ORDERED_WINDOW_V1_MIN_CODEC_VERSION, 54);
        assert!(!codec_version_supports_ordered_window_v1(50));
        assert!(!codec_version_supports_ordered_window_v1(51));
        assert!(!codec_version_supports_ordered_window_v1(52));
        assert!(!codec_version_supports_ordered_window_v1(53));
        assert!(codec_version_supports_ordered_window_v1(54));
        assert!(!codec_version_supports_exact_render_delivery_v1(51));
        assert!(codec_version_supports_exact_render_delivery_v1(52));
        assert!(
            check_compat(56, 56, 55, 55).is_err(),
            "the SendPaste byte-schema change requires an atomic v56 redeploy"
        );
        assert!(
            check_compat(58, 58, 57, 56).is_err(),
            "the authoritative floating snapshot schema requires an atomic v58 redeploy"
        );
        assert_eq!(<ListPanesCoherent as PduWireIdent>::IDENT, 81);
        assert_eq!(<RenderApplicationResult as PduWireIdent>::IDENT, 85);
        assert_eq!(
            <GetPaneRenderDeliveryV1 as PduWireIdent>::WIRE_SPEC.min_codec_version,
            52
        );
        assert_eq!(
            <GetPaneRenderDeliveryV1Response as PduWireIdent>::WIRE_SPEC.min_codec_version,
            52
        );
        assert_eq!(
            <ListPanesOrderedV1 as PduWireIdent>::WIRE_SPEC.min_codec_version,
            54
        );
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
                .expect("current decoder must retain v50 PDU81 for historical fixture decoding")
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
        let (mut payload, is_compressed) = serialize_with_mode(value, CompressionMode::Always)
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
                    message.contains(&format!("{payload_name} payload has trailing schema bytes")),
                    "unexpected {} rejection under {:?}: {}",
                    payload_name,
                    mode,
                    message,
                );

                let async_err = runtime::block_on(async {
                    let mut reader = runtime::Cursor::new(frame);
                    Pdu::decode_async(&mut reader, None).await.expect_err(
                        "async topology authority payload must reject trailing schema bytes",
                    )
                });
                let async_message = format!("{async_err:#}");
                assert!(
                    async_message
                        .contains(&format!("{payload_name} payload has trailing schema bytes")),
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
            for (mode_index, mode) in
                IntoIterator::into_iter([CompressionMode::Never, CompressionMode::Always])
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
                let decoded =
                    Pdu::decode(&mut sync_reader).expect("sync decode first authority frame");
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
                let decoded =
                    Pdu::decode(&mut sync_reader).expect("sync decode preserved next outer frame");
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
                    buffered.as_slice(),
                    next_frame.as_slice(),
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
            format!("{sync_error:#}")
                .contains("validating ListPanesCoherent compressed payload termination"),
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
            format!("{async_error:#}")
                .contains("validating ListPanesCoherent compressed payload termination"),
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
        let empty_frame =
            zstd::stream::encode_all(std::io::empty(), zstd::DEFAULT_COMPRESSION_LEVEL)
                .expect("empty zstd frame should encode");
        let empty_skippable_frame = [
            0x50, 0x2a, 0x4d, 0x18, // skippable-frame magic, little endian
            0x00, 0x00, 0x00, 0x00, // zero-byte payload length
        ];

        for (suffix_name, suffix) in [
            ("empty zstd frame", empty_frame.as_slice()),
            ("empty skippable frame", empty_skippable_frame.as_slice()),
        ] {
            let frame = encode_authority_payload_with_compressed_suffix(81, 14, &request, suffix);
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
                    floating_panes: Vec::new(),
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
    fn authoritative_floating_pane_snapshot_roundtrips_complete_state() {
        let mut tiled = sample_pane_entry(0);
        tiled.pane_id = 1;
        let mut floating_pane = sample_pane_entry(1);
        floating_pane.window_id = tiled.window_id;
        floating_pane.tab_id = tiled.tab_id;
        floating_pane.pane_id = 2;
        floating_pane.is_active_pane = true;
        floating_pane.left_col = 7;
        floating_pane.top_row = 3;
        let floating = FloatingPaneSnapshotEntry {
            rect: FloatingPaneRect {
                left: 7,
                top: 3,
                width: floating_pane.size.cols,
                height: floating_pane.size.rows,
            },
            pane: floating_pane,
            z_order: 91,
            visible: true,
            pinned: true,
            opacity: 0.625,
            focused: true,
        };
        let response = Pdu::ListPanesResponse(ListPanesResponse {
            tabs: vec![PaneNode::Leaf(tiled)],
            tab_titles: vec!["floating owner".to_string()],
            window_titles: HashMap::from([(1, "floating window".to_string())]),
            floating_panes: vec![floating],
        });

        let frame = response
            .encode_frame_with_mode(44, CompressionMode::Never)
            .expect("bounded floating pane snapshot should encode");
        let decoded =
            Pdu::decode(frame.as_slice()).expect("bounded floating pane snapshot should decode");
        assert_eq!(decoded.serial, 44);
        assert_eq!(decoded.pdu, response);
    }

    #[test]
    fn floating_pane_snapshot_validation_rejects_duplicate_and_ambiguous_focus() {
        let pane = sample_pane_entry(0);
        let entry = FloatingPaneSnapshotEntry {
            rect: FloatingPaneRect {
                left: 0,
                top: 0,
                width: pane.size.cols,
                height: pane.size.rows,
            },
            pane,
            z_order: 0,
            visible: true,
            pinned: false,
            opacity: 1.0,
            focused: true,
        };
        assert_eq!(
            validate_floating_pane_snapshot(&[entry.clone(), entry.clone()]),
            Err(FloatingPaneSnapshotError::DuplicatePaneId {
                pane_id: entry.pane.pane_id,
            })
        );

        let mut second = entry.clone();
        second.pane.pane_id = second.pane.pane_id.saturating_add(1);
        assert_eq!(
            validate_floating_pane_snapshot(&[entry.clone(), second]),
            Err(FloatingPaneSnapshotError::MultipleFocusedPanes {
                tab_id: entry.pane.tab_id,
            })
        );

        let mut invalid_opacity = entry.clone();
        invalid_opacity.focused = false;
        invalid_opacity.pane.is_active_pane = false;
        invalid_opacity.opacity = f32::NAN;
        assert!(matches!(
            validate_floating_pane_snapshot(&[invalid_opacity]),
            Err(FloatingPaneSnapshotError::InvalidOpacity { .. })
        ));

        let mut hidden_focus = entry.clone();
        hidden_focus.visible = false;
        assert_eq!(
            validate_floating_pane_snapshot(&[hidden_focus]),
            Err(FloatingPaneSnapshotError::FocusedPaneHidden {
                pane_id: entry.pane.pane_id,
            })
        );

        let mut mismatched_geometry = entry.clone();
        mismatched_geometry.rect.left = mismatched_geometry.rect.left.saturating_add(1);
        assert_eq!(
            validate_floating_pane_snapshot(&[mismatched_geometry]),
            Err(FloatingPaneSnapshotError::GeometryMismatch {
                pane_id: entry.pane.pane_id,
            })
        );
    }

    #[test]
    fn every_revision_advancing_topology_event_has_a_wire_roundtrip() {
        let stream_id = TopologyStreamId::from_bytes([0x33; 16]);
        let events = [
            TopologyEventKind::PaneAdded { pane_id: 1 },
            TopologyEventKind::FloatingPaneSpawned {
                pane_id: 14,
                tab_id: 15,
                window_id: 16,
            },
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
            buffer.as_slice(),
            original.as_slice(),
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
            buffer.as_slice(),
            original.as_slice(),
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
            partial.as_slice(),
            prefix,
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

    #[test]
    fn serialized_lines_preserve_hyperlink_arc_identity_across_physical_rows() {
        let shared = Arc::new(Hyperlink::new_implicit("https://example.com"));
        let mut attrs = termwiz::cell::CellAttributes::default();
        attrs.set_hyperlink(Some(Arc::clone(&shared)));
        let first = Line::from_text("first", &attrs, 0, None);
        let second = Line::from_text("second", &attrs, 0, None);

        let serialized = SerializedLines::from(vec![(10, first), (11, second)]);
        assert_eq!(serialized.hyperlinks.len(), 1);
        assert_eq!(serialized.hyperlinks[0].coords.len(), 2);

        let (lines, images) = serialized.extract_data();
        assert!(images.is_empty());
        let first_link = lines[0]
            .1
            .get_cell(0)
            .and_then(|cell| cell.attrs().hyperlink().cloned())
            .expect("first row should retain its hyperlink");
        let second_link = lines[1]
            .1
            .get_cell(0)
            .and_then(|cell| cell.attrs().hyperlink().cloned())
            .expect("second row should retain its hyperlink");
        assert!(Arc::ptr_eq(&first_link, &second_link));
    }

    #[test]
    fn serialized_lines_do_not_merge_equal_urls_with_distinct_arc_identity() {
        let first_link = Arc::new(Hyperlink::new_implicit("https://example.com"));
        let second_link = Arc::new(Hyperlink::new_implicit("https://example.com"));
        let mut first_attrs = termwiz::cell::CellAttributes::default();
        first_attrs.set_hyperlink(Some(first_link));
        let mut second_attrs = termwiz::cell::CellAttributes::default();
        second_attrs.set_hyperlink(Some(second_link));

        let serialized = SerializedLines::from(vec![
            (10, Line::from_text("first", &first_attrs, 0, None)),
            (11, Line::from_text("second", &second_attrs, 0, None)),
        ]);

        assert_eq!(serialized.hyperlinks.len(), 2);
        let (lines, _) = serialized.extract_data();
        let first_link = lines[0]
            .1
            .get_cell(0)
            .and_then(|cell| cell.attrs().hyperlink().cloned())
            .unwrap();
        let second_link = lines[1]
            .1
            .get_cell(0)
            .and_then(|cell| cell.attrs().hyperlink().cloned())
            .unwrap();
        assert!(!Arc::ptr_eq(&first_link, &second_link));
    }

    #[test]
    fn hyperlink_identity_index_owns_each_pointer_for_the_full_pass() {
        let first = Arc::new(Hyperlink::new_implicit("https://first.example"));
        let first_identity = Arc::as_ptr(&first);
        let mut serialized = Vec::new();
        let mut by_identity = HashMap::new();

        record_serialized_hyperlink_span(&mut serialized, &mut by_identity, &first, 0, 0..1);
        assert_eq!(Arc::strong_count(&first), 2);
        drop(first);

        let (retained_owner, retained_index) = by_identity
            .get(&first_identity)
            .expect("the identity index must retain the source Arc");
        assert_eq!(*retained_index, 0);
        assert_eq!(
            retained_owner.uri(),
            "https://first.example",
            "the raw identity cannot outlive its owning allocation"
        );

        let second = Arc::new(Hyperlink::new_implicit("https://second.example"));
        record_serialized_hyperlink_span(&mut serialized, &mut by_identity, &second, 1, 0..1);
        assert_eq!(serialized.len(), 2);
    }

    // --- CODEC_VERSION test ---

    #[test]
    fn codec_version_is_current() {
        assert_eq!(CODEC_VERSION, 58);
    }

    #[test]
    fn tiered_scrollback_status_batch_has_schema_specific_wire_caps() {
        let request_limit = PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_TIERED_SCROLLBACK_STATUS_REQUEST_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_TIERED_SCROLLBACK_STATUS_REQUEST_ZSTD_ENCODED_BYTES,
        };
        let response_limit = PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_ZSTD_ENCODED_BYTES,
        };
        assert_eq!(
            <GetPaneTieredScrollbackStatusesV1 as PduWireIdent>::WIRE_SPEC.encoded_body_limit,
            request_limit
        );
        assert_eq!(
            <GetPaneTieredScrollbackStatusesV1Response as PduWireIdent>::WIRE_SPEC
                .encoded_body_limit,
            response_limit
        );
        assert!(
            MAX_TIERED_SCROLLBACK_STATUS_REQUEST_ZSTD_ENCODED_BYTES
                >= zstd::zstd_safe::compress_bound(
                    MAX_TIERED_SCROLLBACK_STATUS_REQUEST_DECOMPRESSED_BYTES,
                )
        );
        assert!(
            MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_ZSTD_ENCODED_BYTES
                >= zstd::zstd_safe::compress_bound(
                    MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_DECOMPRESSED_BYTES,
                )
        );
        const {
            assert!(
                MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_DECOMPRESSED_BYTES < MAX_PDU_SIZE,
                "fleet-health responses must not inherit the global allocation envelope"
            );
        }
    }

    fn maximal_tiered_scrollback_summary_v1() -> PaneTieredScrollbackSummaryV1 {
        PaneTieredScrollbackSummaryV1 {
            tiering_enabled: true,
            configured_scrollback_rows: usize::MAX,
            configured_hot_lines: usize::MAX,
            configured_warm_max_bytes: usize::MAX,
            visible_rows: usize::MAX,
            in_memory_scrollback_rows: usize::MAX,
            warm_resident_lines: usize::MAX,
            warm_resident_bytes: usize::MAX,
            warm_spill_lines_total: u64::MAX,
            warm_spill_bytes_total: u64::MAX,
        }
    }

    #[test]
    fn tiered_scrollback_status_batch_round_trips_at_exact_bound_in_order() {
        let entries = (0..MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES)
            .map(|pane_id| PaneTieredScrollbackStatusEntryV1 {
                pane_id,
                outcome: match pane_id % 5 {
                    0 => PaneTieredScrollbackStatusOutcomeV1::Available(
                        maximal_tiered_scrollback_summary_v1(),
                    ),
                    1 => PaneTieredScrollbackStatusOutcomeV1::Unavailable,
                    2 => PaneTieredScrollbackStatusOutcomeV1::Missing,
                    3 => PaneTieredScrollbackStatusOutcomeV1::Closed,
                    _ => PaneTieredScrollbackStatusOutcomeV1::CallbackPanicked,
                },
            })
            .collect::<Vec<_>>();
        let expected = GetPaneTieredScrollbackStatusesV1Response { entries };
        let frame = Pdu::GetPaneTieredScrollbackStatusesV1Response(expected.clone())
            .encode_frame_with_mode(701, CompressionMode::Never)
            .expect("maximum bounded health response must encode");
        let raw = decode_raw(frame.as_slice()).expect("maximum response frame must decode raw");
        assert!(
            raw.data.len() <= MAX_TIERED_SCROLLBACK_STATUS_RESPONSE_DECOMPRESSED_BYTES,
            "maximum response body escaped its schema ceiling"
        );
        let decoded = Pdu::decode(frame.as_slice()).expect("maximum response must decode");
        assert_eq!(
            decoded.pdu,
            Pdu::GetPaneTieredScrollbackStatusesV1Response(expected)
        );
    }

    #[test]
    fn tiered_scrollback_status_batch_rejects_empty_duplicate_and_oversized_inputs() {
        let empty = Pdu::GetPaneTieredScrollbackStatusesV1(GetPaneTieredScrollbackStatusesV1 {
            pane_ids: Vec::new(),
        });
        assert!(empty.encode_frame(702).is_err());

        let duplicate = Pdu::GetPaneTieredScrollbackStatusesV1(GetPaneTieredScrollbackStatusesV1 {
            pane_ids: vec![7, 7],
        });
        assert!(duplicate.encode_frame(703).is_err());

        let duplicate_response = Pdu::GetPaneTieredScrollbackStatusesV1Response(
            GetPaneTieredScrollbackStatusesV1Response {
                entries: vec![
                    PaneTieredScrollbackStatusEntryV1 {
                        pane_id: 7,
                        outcome: PaneTieredScrollbackStatusOutcomeV1::Unavailable,
                    },
                    PaneTieredScrollbackStatusEntryV1 {
                        pane_id: 7,
                        outcome: PaneTieredScrollbackStatusOutcomeV1::Missing,
                    },
                ],
            },
        );
        assert!(duplicate_response.encode_frame(704).is_err());

        let oversized = Pdu::GetPaneTieredScrollbackStatusesV1(GetPaneTieredScrollbackStatusesV1 {
            pane_ids: (0..=MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES).collect(),
        });
        assert!(oversized.encode_frame(705).is_err());

        let (oversized_payload, compressed) = serialize_with_mode(
            &(vec![0_usize; MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES + 1],),
            CompressionMode::Never,
        )
        .expect("hostile unbounded request tuple must serialize");
        assert!(!compressed);
        let mut hostile_frame = Vec::new();
        encode_raw(
            GetPaneTieredScrollbackStatusesV1::IDENT,
            706,
            &oversized_payload,
            false,
            &mut hostile_frame,
        )
        .expect("hostile declared-count fixture must frame");
        let error = Pdu::decode(hostile_frame.as_slice())
            .expect_err("the 257th declared pane must fail bounded admission");
        assert!(
            format!("{error:#}").contains("maximum 256"),
            "unexpected bounded-admission rejection: {error:#}",
            error = error,
        );

        let hostile_entry = PaneTieredScrollbackStatusEntryV1 {
            pane_id: 9,
            outcome: PaneTieredScrollbackStatusOutcomeV1::Unavailable,
        };
        let (oversized_response_payload, compressed) = serialize_with_mode(
            &(vec![
                hostile_entry;
                MAX_TIERED_SCROLLBACK_STATUS_BATCH_PANES + 1
            ],),
            CompressionMode::Never,
        )
        .expect("hostile unbounded response tuple must serialize");
        assert!(!compressed);
        let mut hostile_response_frame = Vec::new();
        encode_raw(
            GetPaneTieredScrollbackStatusesV1Response::IDENT,
            707,
            &oversized_response_payload,
            false,
            &mut hostile_response_frame,
        )
        .expect("hostile declared-count response fixture must frame");
        let error = Pdu::decode(hostile_response_frame.as_slice())
            .expect_err("the 257th declared response entry must fail bounded admission");
        assert!(
            format!("{error:#}").contains("maximum 256"),
            "unexpected response bounded-admission rejection: {error:#}",
            error = error,
        );
    }

    #[test]
    fn image_cell_response_has_a_schema_specific_preallocation_cap() {
        let expected = PduEncodedBodyLimit::SchemaDecompressedWithZstdBound {
            max_decompressed_bytes: MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES,
            max_zstd_encoded_bytes: MAX_GET_IMAGE_CELL_RESPONSE_ZSTD_ENCODED_BYTES,
        };
        let spec = Pdu::wire_spec_for_ident(GetImageCellResponse::IDENT)
            .expect("GetImageCellResponse must remain registered");
        assert_eq!(spec.encoded_body_limit, expected);
        assert_eq!(
            spec.encoded_body_limit.maximum_encoded_payload_bytes(false),
            MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES
        );
        assert!(
            MAX_GET_IMAGE_CELL_RESPONSE_ZSTD_ENCODED_BYTES
                >= zstd::zstd_safe::compress_bound(MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES),
            "the encoded ceiling must admit zstd's worst-case legal output"
        );
        const {
            assert!(
                MAX_GET_IMAGE_CELL_RESPONSE_DECOMPRESSED_BYTES < MAX_PDU_SIZE,
                "image hydration must not inherit the global allocation envelope"
            );
        }
    }

    #[test]
    fn image_cell_response_admits_large_raw_byte_buffers_under_image_cap() {
        use termwiz::image::ImageDataType;

        const WIDTH: u32 = 2_048;
        const HEIGHT: u32 = 2_049;
        let pixel_bytes = vec![0x6d; WIDTH as usize * HEIGHT as usize * 4];
        assert!(
            pixel_bytes.len() > bounded_varbincode::MAX_CONTAINER_BYTES,
            "the regression must cross the generic 16 MiB byte-buffer admission"
        );
        assert!(pixel_bytes.len() <= MAX_IMAGE_HYDRATION_DECODED_BYTES);
        let response = GetImageCellResponse {
            pane_id: 41,
            data: Some(Arc::new(ImageData::with_data(
                ImageDataType::new_single_frame(WIDTH, HEIGHT, pixel_bytes),
            ))),
        };
        let mut frame = Vec::new();
        Pdu::GetImageCellResponse(response)
            .encode(&mut frame, 9_771)
            .expect("large but bounded image response must encode");
        let decoded = Pdu::decode(frame.as_slice())
            .expect("image-specific byte admission must bypass the generic item cap");
        let Pdu::GetImageCellResponse(decoded) = decoded.pdu else {
            panic!("wrong PDU variant");
        };
        let data = decoded.data.expect("decoded image payload");
        assert_eq!(data.len(), WIDTH as usize * HEIGHT as usize * 4);
    }

    #[test]
    fn image_cell_response_enforces_animation_frame_cardinality_on_the_binary_wire() {
        use termwiz::image::{ImageDataType, MAX_IMAGE_WIRE_FRAMES};

        let frame = vec![0x7c; 4];
        let frame_hash = ImageDataType::hash_bytes(&frame);
        let accepted = GetImageCellResponse {
            pane_id: 42,
            data: Some(Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
                width: 1,
                height: 1,
                durations: vec![std::time::Duration::ZERO; MAX_IMAGE_WIRE_FRAMES],
                frames: vec![frame.clone(); MAX_IMAGE_WIRE_FRAMES],
                hashes: vec![frame_hash; MAX_IMAGE_WIRE_FRAMES],
            }))),
        };
        let mut frame_bytes = Vec::new();
        Pdu::GetImageCellResponse(accepted)
            .encode(&mut frame_bytes, 9_772)
            .expect("exactly 4096 animation frames must remain wire-admissible");
        let decoded = Pdu::decode(frame_bytes.as_slice()).unwrap();
        let Pdu::GetImageCellResponse(decoded) = decoded.pdu else {
            panic!("wrong PDU variant");
        };
        let decoded = decoded.data.expect("decoded animation payload");
        let decoded = decoded.data();
        let ImageDataType::AnimRgba8 { frames, .. } = &*decoded else {
            panic!("expected animated RGBA payload");
        };
        assert_eq!(frames.len(), MAX_IMAGE_WIRE_FRAMES);

        let rejected = GetImageCellResponse {
            pane_id: 43,
            data: Some(Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
                width: 1,
                height: 1,
                durations: vec![std::time::Duration::ZERO; MAX_IMAGE_WIRE_FRAMES + 1],
                frames: vec![frame; MAX_IMAGE_WIRE_FRAMES + 1],
                hashes: vec![frame_hash; MAX_IMAGE_WIRE_FRAMES + 1],
            }))),
        };
        let mut rejected_bytes = Vec::new();
        let error = Pdu::GetImageCellResponse(rejected)
            .encode(&mut rejected_bytes, 9_773)
            .expect_err("the 4097th animation frame must be rejected before wire emission");
        assert!(
            format!("{error:#}").contains("image durations contain 4097 items"),
            "unexpected cardinality rejection: {:#}",
            error
        );
    }

    #[test]
    fn pdu_wire_registry_covers_every_assigned_id_and_only_the_historical_gaps() {
        const GAPS: &[u64] = &[5, 6, 7, 15, 16, 17, 18, 19, 21];

        for ident in 0..=94 {
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

        assert!(Pdu::wire_spec_for_ident(95).is_none());
        assert!(Pdu::wire_spec_for_ident(u64::MAX).is_none());
        assert_eq!(Pdu::all_wire_specs().len(), 95 - GAPS.len());
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
                4 => 58,
                13 => 56,
                23 | 47 => 55,
                0..=74 => 46,
                75..=78 => 47,
                79..=80 => 48,
                81..=83 => 49,
                84..=85 => 50,
                86..=90 => 54,
                91..=92 => 52,
                93..=94 => 57,
                ident => panic!("unexpected assigned PDU ID {}", ident),
            };
            assert_eq!(
                spec.min_codec_version, expected,
                "wrong minimum dialect for PDU {} ({})",
                spec.ident, spec.name,
            );
            assert!(spec.min_codec_version <= CODEC_VERSION);
        }

        assert_eq!(Pdu::Ping(Ping {}).minimum_codec_version(), Some(46),);
        assert_eq!(Pdu::Invalid { ident: 5 }.minimum_codec_version(), None);
        assert_eq!(<SendPaste as PduWireIdent>::WIRE_SPEC.min_codec_version, 56);
        assert_eq!(
            <GetLinesResponse as PduWireIdent>::WIRE_SPEC.min_codec_version,
            55
        );
        assert_eq!(
            <GetImageCellResponse as PduWireIdent>::WIRE_SPEC.min_codec_version,
            55
        );
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
            1, 3, 9, 11, 12, 13, 14, 22, 24, 26, 28, 31, 33, 34, 35, 36, 38, 40, 41, 43, 45, 46,
            48, 50, 51, 56, 57, 58, 59, 60, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75,
            77, 80, 81, 85, 86, 88, 91, 93,
        ];
        const SERVER_REPLIES: &[u64] = &[
            0, 2, 4, 8, 10, 23, 25, 27, 29, 30, 32, 42, 47, 49, 52, 61, 76, 78, 82, 87, 89, 92, 94,
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
                SERVER_REPLIES.contains(&spec.ident) || SERVER_UNILATERALS.contains(&spec.ident),
            ) {
                (true, true) => PduProducer::Bidirectional,
                (true, false) => PduProducer::Client,
                (false, true) => PduProducer::Server,
                (false, false) => panic!("PDU {} has no producer", spec.ident),
            };
            assert_eq!(spec.producer, expected_producer);
        }

        assert_eq!(
            Pdu::wire_spec_for_ident(25)
                .expect("PDU 25 assigned")
                .authorities
                .len(),
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
    fn pdu_wire_registry_has_exhaustive_outbound_class_cap_and_qos_metadata() {
        use PduAdmissionCapKey as Cap;
        use PduCorrelatedRequestPolicy::{Fixed, InheritCorrelatedRequest};
        use PduQueueQos as Qos;
        use PduSemanticClass as Class;

        for spec in Pdu::all_wire_specs() {
            if matches!(spec.ident, 0 | 10) {
                assert_eq!(spec.semantic_class, InheritCorrelatedRequest);
                assert_eq!(spec.admission_cap_key, InheritCorrelatedRequest);
                assert_eq!(spec.queue_qos, InheritCorrelatedRequest);
                continue;
            }

            let expected_class = match spec.ident {
                1 | 2 | 26..=30 | 40 => Class::ConnectionControl,
                9 | 11..=13 | 73 => Class::InteractiveInput,
                8
                | 14
                | 20
                | 33..=36
                | 38
                | 43
                | 45
                | 48..=50
                | 56..=59
                | 62..=72
                | 74
                | 88
                | 89 => Class::InteractiveState,
                3 | 4 | 37 | 39 | 44 | 53..=55 | 75 | 76 | 81..=83 | 86 | 87 | 90 => {
                    Class::StateSync
                }
                24 | 25 | 79 | 80 | 84 | 85 | 91 | 92 => Class::Render,
                22 | 31 | 41 | 46 | 51 | 52 | 60 | 61 | 77 | 93 => Class::Query,
                23 | 32 | 42 | 47 | 78 | 94 => Class::BulkData,
                ident => panic!("PDU {} is missing from the semantic-class census", ident),
            };
            let expected_cap = match spec.ident {
                1 | 2 | 26..=30 | 40 => Cap::Control,
                9 | 11 | 12 | 73 => Cap::InteractiveInput,
                8 | 14 | 33..=36 | 38 | 43 | 45 | 48..=50 | 56..=59 | 62..=72 | 74 | 88 | 89 => {
                    Cap::InteractiveState
                }
                37 | 39 | 44 | 53..=55 | 83 | 90 => Cap::StateSync,
                24 | 25 | 79 | 80 | 84 | 85 | 91 | 92 => Cap::Render,
                3 | 22 | 31 | 41 | 46 | 51 | 52 | 60 | 61 | 75 | 77 | 81 | 86 | 93 => Cap::Query,
                4 | 13 | 20 | 23 | 32 | 42 | 47 | 76 | 78 | 82 | 87 | 94 => Cap::BulkData,
                ident => panic!("PDU {} is missing from the admission-cap census", ident),
            };
            let expected_qos = match spec.ident {
                1 | 2 | 26..=30 | 40 => Qos::Control,
                8
                | 9
                | 11..=14
                | 20
                | 33..=36
                | 38
                | 43
                | 45
                | 48..=50
                | 56..=59
                | 62..=74
                | 88
                | 89 => Qos::Interactive,
                3
                | 22
                | 24
                | 25
                | 31
                | 37
                | 39
                | 41
                | 44
                | 46
                | 51..=55
                | 60
                | 61
                | 75
                | 77
                | 79..=81
                | 83..=86
                | 90..=93 => Qos::Normal,
                4 | 23 | 32 | 42 | 47 | 76 | 78 | 82 | 87 | 94 => Qos::Bulk,
                ident => panic!("PDU {} is missing from the queue-QoS census", ident),
            };

            assert_eq!(
                spec.semantic_class,
                Fixed(expected_class),
                "PDU {}",
                spec.name
            );
            assert_eq!(
                spec.admission_cap_key,
                Fixed(expected_cap),
                "PDU {}",
                spec.name
            );
            assert_eq!(spec.queue_qos, Fixed(expected_qos), "PDU {}", spec.name);
        }
    }

    #[test]
    fn generic_replies_inherit_the_exact_correlated_request_metadata() {
        let request = <SendPaste as PduWireIdent>::WIRE_SPEC;
        let expected = PduOutboundMetadata {
            semantic_class: PduSemanticClass::InteractiveInput,
            admission_cap_key: PduAdmissionCapKey::BulkData,
            queue_qos: PduQueueQos::Interactive,
        };

        for response_ident in [ErrorResponse::IDENT, UnitResponse::IDENT] {
            let response = *Pdu::wire_spec_for_ident(response_ident).expect("assigned response");
            assert_eq!(
                response.resolve_outbound_metadata(
                    PduProducer::Server,
                    PduWireRole::CorrelatedReply,
                    Some(&request),
                ),
                Ok(expected),
            );
            assert_eq!(
                response.resolve_outbound_metadata(
                    PduProducer::Server,
                    PduWireRole::CorrelatedReply,
                    None,
                ),
                Err(PduOutboundMetadataError::CorrelatedRequestRequired { response_ident }),
            );
        }
    }

    #[test]
    fn outbound_metadata_resolves_every_generated_allowed_direction() {
        let inherited_request = <Ping as PduWireIdent>::WIRE_SPEC;
        let mut resolved_authorities = 0usize;

        for spec in Pdu::all_wire_specs() {
            let inherits = matches!(
                spec.semantic_class,
                PduCorrelatedRequestPolicy::InheritCorrelatedRequest
            );
            for authority in spec.authorities {
                let metadata = spec
                    .resolve_outbound_metadata(
                        authority.producer,
                        authority.role,
                        inherits.then_some(&inherited_request),
                    )
                    .unwrap_or_else(|error| {
                        panic!(
                            "allowed outbound direction failed for PDU {} ({}): {error}",
                            spec.ident, spec.name,
                        )
                    });
                if inherits {
                    assert_eq!(
                        metadata,
                        PduOutboundMetadata {
                            semantic_class: PduSemanticClass::ConnectionControl,
                            admission_cap_key: PduAdmissionCapKey::Control,
                            queue_qos: PduQueueQos::Control,
                        },
                    );
                }
                resolved_authorities = resolved_authorities
                    .checked_add(1)
                    .expect("authority census cannot overflow usize");
            }
        }

        let declared_authorities = Pdu::all_wire_specs()
            .iter()
            .try_fold(0usize, |total, spec| {
                total.checked_add(spec.authorities.len())
            })
            .expect("declared authority census cannot overflow usize");
        assert_eq!(resolved_authorities, declared_authorities);
    }

    #[test]
    fn outbound_metadata_planning_rejects_invalid_direction_and_request_authority() {
        assert_eq!(
            Pdu::Invalid { ident: u64::MAX }.resolve_outbound_metadata(
                PduProducer::Client,
                PduWireRole::Request,
                None,
            ),
            Err(PduOutboundMetadataError::InvalidPdu { ident: u64::MAX }),
        );

        let ping = Pdu::Ping(Ping {});
        assert_eq!(
            ping.resolve_outbound_metadata(PduProducer::Server, PduWireRole::Unilateral, None,),
            Err(PduOutboundMetadataError::DirectionNotAuthorized {
                ident: Ping::IDENT,
                producer: PduProducer::Server,
                role: PduWireRole::Unilateral,
            }),
        );

        let mut forged_spec = <Ping as PduWireIdent>::WIRE_SPEC;
        forged_spec.queue_qos = PduCorrelatedRequestPolicy::Fixed(PduQueueQos::Bulk);
        assert_eq!(
            forged_spec.resolve_outbound_metadata(PduProducer::Client, PduWireRole::Request, None,),
            Err(PduOutboundMetadataError::NonCanonicalWireSpec { ident: Ping::IDENT }),
        );

        let response = <ErrorResponse as PduWireIdent>::WIRE_SPEC;
        let invalid_request = <Pong as PduWireIdent>::WIRE_SPEC;
        assert_eq!(
            response.resolve_outbound_metadata(
                PduProducer::Server,
                PduWireRole::CorrelatedReply,
                Some(&invalid_request),
            ),
            Err(PduOutboundMetadataError::InvalidCorrelatedRequest {
                response_ident: ErrorResponse::IDENT,
                request_ident: Pong::IDENT,
            }),
        );

        let mut forged_request = <SendPaste as PduWireIdent>::WIRE_SPEC;
        forged_request.queue_qos = PduCorrelatedRequestPolicy::Fixed(PduQueueQos::Control);
        assert_eq!(
            response.resolve_outbound_metadata(
                PduProducer::Server,
                PduWireRole::CorrelatedReply,
                Some(&forged_request),
            ),
            Err(PduOutboundMetadataError::InvalidCorrelatedRequest {
                response_ident: ErrorResponse::IDENT,
                request_ident: SendPaste::IDENT,
            }),
        );

        assert_eq!(
            Pdu::Pong(Pong {}).resolve_outbound_metadata(
                PduProducer::Server,
                PduWireRole::CorrelatedReply,
                None,
            ),
            Ok(PduOutboundMetadata {
                semantic_class: PduSemanticClass::ConnectionControl,
                admission_cap_key: PduAdmissionCapKey::Control,
                queue_qos: PduQueueQos::Control,
            }),
        );
    }

    fn assert_never_plan_matches_canonical_frame(
        pdu: Pdu,
        producer: PduProducer,
        role: PduWireRole,
        expected_class: PduSemanticClass,
    ) {
        reset_test_serialize_invocations();
        reset_test_bounded_serialize_growth_events();
        reset_test_compression_invocations();

        let plan = pdu
            .plan_outbound(producer, role, None, CompressionMode::Never)
            .expect("outbound sample must have a definitely-not-sent plan");
        assert!(std::ptr::eq(plan.pdu(), &pdu));
        assert_eq!(plan.metadata.semantic_class, expected_class);
        assert_eq!(plan.compression_mode, CompressionMode::Never);
        assert_eq!(plan.maximum_compression_output_bytes, 0);
        assert_eq!(
            plan.maximum_encoded_payload_bytes,
            plan.logical_payload_bytes
        );
        assert_eq!(test_serialize_invocations(), 0);
        assert_eq!(test_bounded_serialize_buffer_constructions(), 0);
        assert_eq!(test_bounded_serialize_growth_events(), 0);
        assert_eq!(test_compression_invocations(), 0);

        let frame = pdu
            .encode_frame_with_mode(OUTBOUND_PLAN_RESERVED_SERIAL, CompressionMode::Never)
            .expect("canonical comparison frame must encode");
        let decoded = decode_raw(frame.as_slice()).expect("canonical comparison frame must decode");
        assert_eq!(decoded.data.len(), plan.logical_payload_bytes);
        assert_eq!(frame.len(), plan.maximum_frame_bytes);
        assert_eq!(plan.retained_frame_bytes, plan.maximum_frame_bytes);
        assert!(plan.codec_peak_bytes >= plan.maximum_frame_bytes);
    }

    #[test]
    fn outbound_plan_counting_matches_every_semantic_class_without_codec_allocation() {
        let samples = vec![
            (
                Pdu::Ping(Ping {}),
                PduProducer::Client,
                PduWireRole::Request,
                PduSemanticClass::ConnectionControl,
            ),
            (
                Pdu::WriteToPane(WriteToPane {
                    pane_id: 1,
                    data: b"keypress".to_vec(),
                }),
                PduProducer::Client,
                PduWireRole::Request,
                PduSemanticClass::InteractiveInput,
            ),
            (
                Pdu::SetPaneZoomed(SetPaneZoomed {
                    containing_tab_id: 1,
                    pane_id: 2,
                    zoomed: true,
                }),
                PduProducer::Client,
                PduWireRole::Request,
                PduSemanticClass::InteractiveState,
            ),
            (
                Pdu::ListPanes(ListPanes {}),
                PduProducer::Client,
                PduWireRole::Request,
                PduSemanticClass::StateSync,
            ),
            (
                Pdu::RenderApplicationUpdate(sample_render_application_update()),
                PduProducer::Server,
                PduWireRole::Unilateral,
                PduSemanticClass::Render,
            ),
            (
                Pdu::GetSemanticZones(GetSemanticZones { pane_id: 3 }),
                PduProducer::Client,
                PduWireRole::Request,
                PduSemanticClass::Query,
            ),
            (
                Pdu::GetSemanticZonesResponse(GetSemanticZonesResponse {
                    pane_id: 3,
                    zones: Vec::new(),
                    zone_texts: Vec::new(),
                    last_exit_code: None,
                }),
                PduProducer::Server,
                PduWireRole::CorrelatedReply,
                PduSemanticClass::BulkData,
            ),
        ];

        for (pdu, producer, role, expected_class) in samples {
            assert_never_plan_matches_canonical_frame(pdu, producer, role, expected_class);
        }
    }

    #[test]
    fn outbound_plan_counts_nested_ordered_window_sections_without_materializing_them() {
        for (expected_ident, role) in IntoIterator::into_iter([
            (
                ListPanesOrderedV1Response::IDENT,
                PduWireRole::CorrelatedReply,
            ),
            (WindowOrderEventV1::IDENT, PduWireRole::Unilateral),
        ]) {
            let (_, pdu) = sample_ordered_window_pdus()
                .into_iter()
                .find(|(ident, _)| *ident == expected_ident)
                .expect("ordered sample must include every nested-section PDU");
            assert_never_plan_matches_canonical_frame(
                pdu,
                PduProducer::Server,
                role,
                PduSemanticClass::StateSync,
            );
        }
    }

    #[test]
    fn outbound_counting_fails_at_limit_plus_one_and_restores_nested_scope() {
        let value = WriteToPane {
            pane_id: 9,
            data: vec![0x5a; 64],
        };
        let exact = count_pdu_payload(&value, usize::MAX, WriteToPane::IDENT)
            .expect("unbounded control count must succeed")
            .logical_payload_bytes;
        assert!(exact > 0);
        assert!(matches!(
            count_pdu_payload(&value, exact - 1, WriteToPane::IDENT),
            Err(PduOutboundPlanError::LogicalPayloadLimitExceeded {
                ident: WriteToPane::IDENT,
                declared_payload_bytes,
                max_payload_bytes,
            }) if declared_payload_bytes == exact && max_payload_bytes == exact - 1
        ));
        assert_eq!(
            count_pdu_payload(&value, exact, WriteToPane::IDENT)
                .expect("exact logical limit must admit the payload")
                .logical_payload_bytes,
            exact
        );

        let outer = OutboundCountingScope::enter(WriteToPane::IDENT)
            .expect("test outer counting scope must install");
        let pdu = Pdu::WriteToPane(value);
        assert!(matches!(
            pdu.plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Never,
            ),
            Err(PduOutboundPlanError::NestedCountingScope {
                ident: WriteToPane::IDENT,
            })
        ));
        drop(outer);
        assert!(OUTBOUND_COUNTING_STATE.with(|state| state.get().is_none()));
    }

    #[test]
    fn outbound_counting_fails_closed_on_checked_length_overflow() {
        let mut writer = CheckedCountingWriter {
            logical_len: usize::MAX,
            max_bytes: usize::MAX,
            failure: None,
        };
        let error = std::io::Write::write(&mut writer, &[0])
            .expect_err("one byte beyond usize::MAX must fail before wrapping");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(matches!(writer.failure, Some(CountingFailure::Overflow)));
        assert_eq!(writer.logical_len, usize::MAX);
    }

    #[test]
    fn outbound_plan_rejects_direction_and_payload_before_codec_work() {
        reset_test_serialize_invocations();
        reset_test_bounded_serialize_growth_events();
        reset_test_compression_invocations();

        let ping = Pdu::Ping(Ping {});
        assert!(matches!(
            ping.plan_outbound(
                PduProducer::Server,
                PduWireRole::Unilateral,
                None,
                CompressionMode::Always,
            ),
            Err(PduOutboundPlanError::Metadata(
                PduOutboundMetadataError::DirectionNotAuthorized {
                    ident: Ping::IDENT,
                    producer: PduProducer::Server,
                    role: PduWireRole::Unilateral,
                }
            ))
        ));

        let empty = Pdu::GetPaneTieredScrollbackStatusesV1(GetPaneTieredScrollbackStatusesV1 {
            pane_ids: Vec::new(),
        });
        let error = empty
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Always,
            )
            .expect_err("an invalid bounded request must fail before codec work");
        assert!(matches!(
            &error,
            PduOutboundPlanError::InvalidPayload {
                ident,
                ..
            } if *ident == GetPaneTieredScrollbackStatusesV1::IDENT
        ));
        assert!(error.to_string().contains("at least one pane"));

        assert_eq!(test_serialize_invocations(), 0);
        assert_eq!(test_bounded_serialize_buffer_constructions(), 0);
        assert_eq!(test_bounded_serialize_growth_events(), 0);
        assert_eq!(test_compression_invocations(), 0);
    }

    #[test]
    fn generic_response_plan_inherits_correlated_request_metadata() {
        let pdu = Pdu::ErrorResponse(ErrorResponse {
            reason: "bounded failure".to_string(),
        });
        let request = <GetPaneTieredScrollbackStatusesV1 as PduWireIdent>::WIRE_SPEC;
        let plan = pdu
            .plan_outbound(
                PduProducer::Server,
                PduWireRole::CorrelatedReply,
                Some(&request),
                CompressionMode::Never,
            )
            .expect("generic response must inherit its exact request class");
        assert_eq!(plan.metadata(), request.fixed_outbound_metadata().unwrap());
        assert_eq!(plan.ident(), ErrorResponse::IDENT);
    }

    #[test]
    fn outbound_compression_plan_uses_bound_without_serializing_or_invoking_zstd() {
        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 11,
            data: vec![b'x'; 4 * 1024],
        });
        reset_test_serialize_invocations();
        reset_test_bounded_serialize_growth_events();
        reset_test_compression_invocations();

        let plan = pdu
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Always,
            )
            .expect("compressible input must admit a bounded plan");
        assert!(plan.maximum_compression_output_bytes >= plan.logical_payload_bytes);
        assert_eq!(
            plan.maximum_encoded_payload_bytes,
            plan.maximum_compression_output_bytes
        );
        assert_eq!(test_serialize_invocations(), 0);
        assert_eq!(test_bounded_serialize_buffer_constructions(), 0);
        assert_eq!(test_bounded_serialize_growth_events(), 0);
        assert_eq!(test_compression_invocations(), 0);

        let frame = pdu
            .encode_frame_with_mode(OUTBOUND_PLAN_RESERVED_SERIAL, CompressionMode::Always)
            .expect("planned compressed frame must encode");
        assert!(frame.len() <= plan.maximum_frame_bytes);
        assert_eq!(test_compression_invocations(), 1);
    }

    fn deterministic_incompressible_bytes(len: usize) -> Vec<u8> {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[0]
            })
            .collect()
    }

    #[test]
    fn outbound_auto_plan_bounds_threshold_and_incompressible_paths_without_codec_work() {
        let small = Pdu::Ping(Ping {});
        reset_test_serialize_invocations();
        reset_test_bounded_serialize_growth_events();
        reset_test_compression_invocations();
        let small_plan = small
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Auto,
            )
            .expect("small auto payload must admit an uncompressed plan");
        assert!(small_plan.logical_payload_bytes() <= COMPRESS_THRESH);
        assert_eq!(small_plan.maximum_compression_output_bytes(), 0);
        let small_frame = small
            .encode_frame_with_mode(OUTBOUND_PLAN_RESERVED_SERIAL, CompressionMode::Auto)
            .expect("small planned frame must encode");
        assert_eq!(small_frame.len(), small_plan.maximum_frame_bytes());
        assert!(!decode_raw(small_frame.as_slice()).unwrap().is_compressed);
        assert_eq!(test_compression_invocations(), 0);

        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 13,
            data: deterministic_incompressible_bytes(4 * 1024),
        });
        reset_test_serialize_invocations();
        reset_test_bounded_serialize_growth_events();
        reset_test_compression_invocations();
        let auto_plan = pdu
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Auto,
            )
            .expect("incompressible auto payload must admit a conservative plan");
        let always_plan = pdu
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Always,
            )
            .expect("incompressible always payload must admit a compress-bound plan");
        assert!(auto_plan.maximum_compression_output_bytes() >= auto_plan.logical_payload_bytes());
        assert_eq!(
            always_plan.maximum_encoded_payload_bytes(),
            always_plan.maximum_compression_output_bytes()
        );
        assert_eq!(test_serialize_invocations(), 0);
        assert_eq!(test_bounded_serialize_buffer_constructions(), 0);
        assert_eq!(test_bounded_serialize_growth_events(), 0);
        assert_eq!(test_compression_invocations(), 0);

        let auto_frame = pdu
            .encode_frame_with_mode(OUTBOUND_PLAN_RESERVED_SERIAL, CompressionMode::Auto)
            .expect("incompressible auto frame must encode");
        let auto_raw = decode_raw(auto_frame.as_slice()).expect("auto frame must decode raw");
        assert!(
            !auto_raw.is_compressed,
            "fixed high-entropy fixture must exercise Auto's uncompressed retention path"
        );
        assert!(auto_frame.len() <= auto_plan.maximum_frame_bytes());

        let always_frame = pdu
            .encode_frame_with_mode(OUTBOUND_PLAN_RESERVED_SERIAL, CompressionMode::Always)
            .expect("incompressible always frame must encode");
        assert!(decode_raw(always_frame.as_slice()).unwrap().is_compressed);
        assert!(always_frame.len() <= always_plan.maximum_frame_bytes());
        assert_eq!(test_compression_invocations(), 2);
    }

    fn assert_prepared_encoder_matches_legacy(
        pdu: &Pdu,
        producer: PduProducer,
        role: PduWireRole,
        correlated_request: Option<PduWireSpec>,
    ) {
        for compression_mode in [
            CompressionMode::Never,
            CompressionMode::Auto,
            CompressionMode::Always,
        ] {
            let serial = OUTBOUND_PLAN_RESERVED_SERIAL;
            let legacy = pdu
                .encode_frame_with_mode(serial, compression_mode)
                .expect("legacy encoder must accept the bounded fixture");
            let prepared = pdu
                .plan_outbound(
                    producer,
                    role,
                    correlated_request.as_ref(),
                    compression_mode,
                )
                .expect("fixture must produce an exact outbound plan");
            let logical_payload_bytes = prepared.logical_payload_bytes();
            let maximum_compression_output_bytes = prepared.maximum_compression_output_bytes();
            let maximum_frame_bytes = prepared.maximum_frame_bytes();

            reset_test_serialize_invocations();
            reset_test_bounded_serialize_growth_events();
            reset_test_compression_invocations();
            let bounded = prepared
                .encode_frame(serial)
                .expect("bounded encoder must consume its exact plan");

            assert_eq!(bounded, legacy, "bounded wire bytes must remain canonical");
            assert!(bounded.len() <= maximum_frame_bytes);
            assert_eq!(test_serialize_invocations(), 1);
            let compression_attempted = compression_mode == CompressionMode::Always
                || (compression_mode == CompressionMode::Auto
                    && logical_payload_bytes > COMPRESS_THRESH);
            assert_eq!(
                test_compression_invocations(),
                usize::from(compression_attempted)
            );
            assert_eq!(
                test_bounded_serialize_buffer_constructions(),
                1 + usize::from(compression_attempted)
            );
            assert!(
                test_bounded_serialize_max_requested_capacity()
                    <= logical_payload_bytes.max(maximum_compression_output_bytes),
                "bounded writers requested capacity beyond the exact plan"
            );
        }
    }

    #[test]
    fn prepared_encoder_is_wire_equivalent_for_large_outbound_schema_families() {
        let large = "bounded-wire".repeat(1_024);
        let mut render = sample_render_application_update();
        render.surface.title = large.clone();
        let topology = sample_ordered_window_pdus()
            .into_iter()
            .find_map(|(ident, pdu)| (ident == WindowOrderEventV1::IDENT).then_some(pdu))
            .expect("ordered fixtures must include the topology event");
        let fixtures = vec![
            (
                Pdu::RenameWorkspace(RenameWorkspace {
                    old_workspace: large.clone(),
                    new_workspace: large.clone(),
                }),
                PduProducer::Client,
                PduWireRole::Request,
                None,
            ),
            (
                Pdu::ErrorResponse(ErrorResponse {
                    reason: large.clone(),
                }),
                PduProducer::Server,
                PduWireRole::CorrelatedReply,
                Some(<RenameWorkspace as PduWireIdent>::WIRE_SPEC),
            ),
            (
                Pdu::SetClipboard(SetClipboard {
                    pane_id: 21,
                    clipboard: Some(large.clone()),
                    selection: ClipboardSelection::Clipboard,
                }),
                PduProducer::Server,
                PduWireRole::Unilateral,
                None,
            ),
            (
                Pdu::RenderApplicationUpdate(render),
                PduProducer::Server,
                PduWireRole::Unilateral,
                None,
            ),
            (topology, PduProducer::Server, PduWireRole::Unilateral, None),
            (
                Pdu::WindowTitleChanged(WindowTitleChanged {
                    window_id: 22,
                    title: large.clone(),
                }),
                PduProducer::Server,
                PduWireRole::Unilateral,
                None,
            ),
            (
                Pdu::SendPaste(SendPaste {
                    pane_id: 23,
                    data: large,
                    input_serial: InputSerial::from_millis_since_epoch(24),
                }),
                PduProducer::Client,
                PduWireRole::Request,
                None,
            ),
            (
                Pdu::WriteToPane(WriteToPane {
                    pane_id: 25,
                    data: deterministic_incompressible_bytes(16 * 1024),
                }),
                PduProducer::Client,
                PduWireRole::Request,
                None,
            ),
        ];

        for (pdu, producer, role, correlated_request) in fixtures {
            assert_prepared_encoder_matches_legacy(&pdu, producer, role, correlated_request);
        }
    }

    #[test]
    fn prepared_encoder_enforces_exact_body_compression_and_frame_bounds() {
        for incompressible in [false, true] {
            let payload = |len| {
                if incompressible {
                    deterministic_incompressible_bytes(len)
                } else {
                    vec![b'x'; len]
                }
            };
            let pdus = [4_095_usize, 4_096, 4_097].map(|len| {
                Pdu::WriteToPane(WriteToPane {
                    pane_id: 26,
                    data: payload(len),
                })
            });
            let logical = pdus.each_ref().map(|pdu| {
                pdu.plan_outbound(
                    PduProducer::Client,
                    PduWireRole::Request,
                    None,
                    CompressionMode::Never,
                )
                .expect("boundary fixture must plan")
                .logical_payload_bytes()
            });
            let cap = logical[1];
            assert_eq!(logical, [cap - 1, cap, cap + 1]);

            for pdu in &pdus {
                for compression_mode in [
                    CompressionMode::Never,
                    CompressionMode::Auto,
                    CompressionMode::Always,
                ] {
                    let prepared = pdu
                        .plan_outbound(
                            PduProducer::Client,
                            PduWireRole::Request,
                            None,
                            compression_mode,
                        )
                        .expect("boundary fixture must plan in every compression mode");
                    let maximum_frame_bytes = prepared.maximum_frame_bytes();
                    let frame = prepared
                        .encode_frame(OUTBOUND_PLAN_RESERVED_SERIAL)
                        .expect("cap-minus-one, cap, and cap-plus-one fixtures must encode");
                    assert!(frame.len() <= maximum_frame_bytes);
                }
            }

            let mut undersized = pdus[2]
                .plan_outbound(
                    PduProducer::Client,
                    PduWireRole::Request,
                    None,
                    CompressionMode::Never,
                )
                .expect("cap-plus-one fixture must plan against its real schema bound");
            undersized.plan.logical_payload_bytes = cap;
            assert!(matches!(
                undersized.encode_frame(OUTBOUND_PLAN_RESERVED_SERIAL),
                Err(PduOutboundEncodeError::PlanMismatch {
                    ident: WriteToPane::IDENT,
                    field: "logical_payload_bytes",
                    planned,
                    actual,
                }) if planned == cap && actual == cap + 1
            ));
        }

        let pdu = Pdu::WriteToPane(WriteToPane {
            pane_id: 27,
            data: deterministic_incompressible_bytes(8 * 1024),
        });
        let legacy = pdu
            .encode_frame_with_mode(OUTBOUND_PLAN_RESERVED_SERIAL, CompressionMode::Always)
            .expect("legacy compressed fixture must encode");
        let compressed_bytes = decode_raw(legacy.as_slice())
            .expect("legacy compressed fixture must decode raw")
            .data
            .len();
        assert!(compressed_bytes > 0);
        let mut compression_undersized = pdu
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Always,
            )
            .expect("compressed boundary fixture must plan");
        compression_undersized.plan.maximum_compression_output_bytes = compressed_bytes - 1;
        assert!(matches!(
            compression_undersized.encode_frame(OUTBOUND_PLAN_RESERVED_SERIAL),
            Err(PduOutboundEncodeError::PlanMismatch {
                ident: WriteToPane::IDENT,
                field: "maximum_compression_output_bytes",
                planned,
                actual,
            }) if planned == compressed_bytes - 1 && actual >= compressed_bytes
        ));

        let mut frame_undersized = pdu
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Never,
            )
            .expect("frame boundary fixture must plan");
        let actual_frame_bytes = pdu
            .encode_frame_with_mode(OUTBOUND_PLAN_RESERVED_SERIAL, CompressionMode::Never)
            .expect("frame boundary fixture must encode")
            .len();
        frame_undersized.plan.maximum_frame_bytes = actual_frame_bytes - 1;
        assert!(matches!(
            frame_undersized.encode_frame(OUTBOUND_PLAN_RESERVED_SERIAL),
            Err(PduOutboundEncodeError::PlanMismatch {
                ident: WriteToPane::IDENT,
                field: "maximum_frame_bytes",
                planned,
                actual,
            }) if planned == actual_frame_bytes - 1 && actual == actual_frame_bytes
        ));
    }

    #[test]
    fn prepared_outbound_debug_is_content_free() {
        let secret = "OUTBOUND-PLAN-MUST-NOT-LOG-THIS-CONTENT";
        let pdu = Pdu::SendPaste(SendPaste {
            pane_id: 12,
            data: secret.to_string(),
            input_serial: InputSerial::from_millis_since_epoch(1),
        });
        let prepared = pdu
            .plan_outbound(
                PduProducer::Client,
                PduWireRole::Request,
                None,
                CompressionMode::Never,
            )
            .expect("paste must produce an exact-owner plan");
        let debug = format!("{prepared:?}");
        assert!(debug.contains("SendPaste"));
        assert!(!debug.contains(secret));
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
        let exact_render = TopologyCapabilities::from_bits(
            fenced.bits() | TopologyCapabilities::EXACT_RENDER_DELIVERY_V1.bits(),
        );

        for spec in Pdu::all_wire_specs() {
            let expected = match spec.ident {
                81 | 82 => PduCapabilityUse::Negotiates(fenced),
                83..=85 => PduCapabilityUse::Requires(fenced),
                86 | 87 => PduCapabilityUse::Negotiates(ordered),
                88 | 89 => PduCapabilityUse::Requires(reorder),
                90 => PduCapabilityUse::Requires(ordered),
                91 | 92 => PduCapabilityUse::Requires(exact_render),
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
        assert!(!TopologyCapabilities::SERVER_SUPPORTED
            .contains(TopologyCapabilities::EXACT_RENDER_DELIVERY_V1));
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
        let (mut payload, is_compressed) = serialize_with_mode(&legacy, CompressionMode::Never)
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
    fn check_compat_current_build_requires_atomic_v58_floating_snapshot_schema() {
        assert_eq!(CODEC_VERSION_MIN_SUPPORTED, 58);
        assert_eq!(CODEC_VERSION, 58);
        assert!(check_compat(58, 58, 57, 56).is_err());
        assert_eq!(
            check_compat(58, 58, 58, 58),
            Ok(CompatDecision::Compatible { agreed: 58 })
        );
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

                let discarded = Pdu::decode_async_with_selector(&mut reader, Some(2), |header| {
                    assert_eq!(header.serial(), 1);
                    assert_eq!(header.ident(), 99);
                    assert_eq!(header.encoded_payload_len(), data_len);
                    assert!(!header.is_compressed());
                    Ok(PduBodyDisposition::Discard)
                })
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
            encode_raw(99, 1, &[0x41; 128], false, &mut wire).expect("encode discard candidate");
            wire.pop().expect("encoded frame has a payload byte");
            let mut reader = runtime::Cursor::new(wire);

            let error = Pdu::decode_async_with_selector(&mut reader, Some(1), |_| {
                Ok(PduBodyDisposition::Discard)
            })
            .await
            .expect_err("truncated discarded body must fail closed");
            assert!(
                error
                    .to_string()
                    .contains("discarding an abandoned PDU body"),
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
                error
                    .to_string()
                    .contains("refusing to discard compressed PDU body"),
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
        assert!(!Pdu::SetClipboard(SetClipboard {
            pane_id: 55,
            clipboard: Some("copied".to_string()),
            selection: ClipboardSelection::Clipboard,
        })
        .is_user_input());
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
        let pdu = Pdu::GetPaneRenderChangesResponse(sample_retention_metadata_render_change());
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
        let mut buffer =
            StreamingPduBuffer::from(ping.encode_frame(9).expect("encode non-render frame"));
        let decoded = Pdu::stream_decode_with_retention_metadata(&mut buffer)
            .expect("decode non-render frame")
            .expect("complete non-render frame");
        assert_eq!(decoded.decoded().serial, 9);
        assert_eq!(decoded.retained_frame_bytes(), None);
    }

    #[test]
    fn render_retention_metadata_conservatively_charges_additive_payload_tails() {
        let payload = sample_retention_metadata_render_change();
        let (known_payload, compressed) = serialize_with_mode(&payload, CompressionMode::Never)
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
        let mut concatenated_compressed =
            zstd::stream::encode_all(known_payload.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)
                .expect("compress known render payload");
        concatenated_compressed.extend_from_slice(
            &zstd::stream::encode_all(additive_tail.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)
                .expect("compress additive render tail as a second frame"),
        );

        for (encoded_payload, is_compressed, case) in [
            (payload_with_tail, false, "uncompressed"),
            (compressed_with_tail, true, "compressed"),
            (concatenated_compressed, true, "concatenated-compressed"),
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
        let (known_payload, compressed) = serialize_with_mode(&payload, CompressionMode::Never)
            .expect("serialize known render schema");
        assert!(!compressed);
        let mut encoded_payload =
            zstd::stream::encode_all(known_payload.as_slice(), zstd::DEFAULT_COMPRESSION_LEVEL)
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
        let render = Pdu::GetPaneRenderChangesResponse(sample_retention_metadata_render_change());
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
        assert_eq!(decoded.pdu, Pdu::RenderApplicationUpdateV1(legacy_update));

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
        let decoded_result = Pdu::decode(encoded_result.as_slice()).expect("render result decodes");
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
                limit: u64::try_from(MAX_RENDER_APPLICATION_LINES).expect("test limit fits u64"),
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

        snapshot.semantic_zones = RenderComponentUpdate::Replace(GetSemanticZonesResponse {
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
        let line = || Line::from_text("x", &termwiz::cell::CellAttributes::default(), 1, None);
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

        for (top_left, bottom_right) in [
            (
                TextureCoordinate::new_f32(f32::INFINITY, 0.0),
                TextureCoordinate::new_f32(1.0, 1.0),
            ),
            (
                TextureCoordinate::new_f32(0.8, 0.0),
                TextureCoordinate::new_f32(0.2, 1.0),
            ),
            (
                TextureCoordinate::new_f32(-0.1, 0.0),
                TextureCoordinate::new_f32(1.0, 1.0),
            ),
            (
                TextureCoordinate::new_f32(0.0, 0.0),
                TextureCoordinate::new_f32(1.1, 1.0),
            ),
            (
                TextureCoordinate::new_f32(0.0, 0.5),
                TextureCoordinate::new_f32(1.0, 0.5),
            ),
            (
                TextureCoordinate::new_f32(-4.0 * f32::EPSILON, 0.0),
                TextureCoordinate::new_f32(0.0, 1.0),
            ),
            (
                TextureCoordinate::new_f32(1.0, 0.0),
                TextureCoordinate::new_f32(1.0 + 4.0 * f32::EPSILON, 1.0),
            ),
            (
                TextureCoordinate::new_f32(0.0, -4.0 * f32::EPSILON),
                TextureCoordinate::new_f32(1.0, 0.0),
            ),
            (
                TextureCoordinate::new_f32(0.0, 1.0),
                TextureCoordinate::new_f32(1.0, 1.0 + 4.0 * f32::EPSILON),
            ),
        ] {
            let invalid_geometry = SerializedLines {
                lines: vec![(0, line())],
                hyperlinks: Vec::new(),
                images: vec![SerializedImageCell {
                    line_idx: 0,
                    cell_idx: 0,
                    top_left,
                    bottom_right,
                    data_hash: [0x3c; 32],
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
                invalid_geometry.validate_structure(),
                Err(SerializedLinesStructureError::ImageTextureCoordinatesInvalid)
            );
        }

        let rounded_geometry = SerializedLines {
            lines: vec![(0, line())],
            hyperlinks: Vec::new(),
            images: vec![SerializedImageCell {
                line_idx: 0,
                cell_idx: 0,
                top_left: TextureCoordinate::new_f32(-4.0 * f32::EPSILON, 0.0),
                bottom_right: TextureCoordinate::new_f32(1.0 + 4.0 * f32::EPSILON, 1.0),
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
        assert!(rounded_geometry.validate_structure().is_ok());
        let (top_left, bottom_right) = rounded_geometry.images[0].canonical_texture_coordinates();
        assert_eq!(top_left, TextureCoordinate::new_f32(0.0, 0.0));
        assert_eq!(bottom_right, TextureCoordinate::new_f32(1.0, 1.0));
    }

    #[test]
    fn serialized_lines_wire_enforces_aggregate_hyperlink_span_limit() {
        let first_count = MAX_RENDER_APPLICATION_HYPERLINK_SPANS / 2;
        let second_count = MAX_RENDER_APPLICATION_HYPERLINK_SPANS - first_count + 1;
        let coordinate = CellCoordinates {
            line_idx: 0,
            cols: 0..1,
        };
        let hyperlinks = vec![
            LineHyperlink {
                link: Hyperlink::new_implicit("https://first.example"),
                coords: vec![coordinate.clone(); first_count],
            },
            LineHyperlink {
                link: Hyperlink::new_implicit("https://second.example"),
                coords: vec![coordinate; second_count],
            },
        ];

        let bounded = SerializedLines {
            lines: Vec::new(),
            hyperlinks: hyperlinks.clone(),
            images: Vec::new(),
        };
        let encode_error = serde_json::to_vec(&bounded)
            .expect_err("aggregate hyperlink spans above the limit must not serialize");
        assert!(
            encode_error.to_string().contains("65536"),
            "unexpected aggregate hyperlink encode error: {}",
            encode_error
        );

        // Produce a structurally equivalent JSON object without using
        // SerializedLines' bounded field serializer so the receive-side
        // aggregate gate is exercised independently.
        #[derive(Serialize)]
        struct UnboundedSerializedLines<'a> {
            lines: &'a [(StableRowIndex, Line)],
            hyperlinks: &'a [LineHyperlink],
            images: &'a [SerializedImageCell],
        }
        let lines: Vec<(StableRowIndex, Line)> = Vec::new();
        let images: Vec<SerializedImageCell> = Vec::new();
        let wire = serde_json::to_vec(&UnboundedSerializedLines {
            lines: &lines,
            hyperlinks: &hyperlinks,
            images: &images,
        })
        .unwrap();
        let decode_error = serde_json::from_slice::<SerializedLines>(&wire)
            .expect_err("aggregate hyperlink spans above the limit must not deserialize");
        assert!(
            decode_error.to_string().contains("65536"),
            "unexpected aggregate hyperlink decode error: {}",
            decode_error
        );
    }

    #[test]
    fn serialized_lines_preserve_layered_images_in_one_cell() {
        use termwiz::cell::CellAttributes;
        use termwiz::image::{ImageCell, ImageDataType};

        let first_data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0x11; 4],
        )));
        let second_data = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0x22; 4],
        )));
        let mut attrs = CellAttributes::default();
        attrs.attach_image(Box::new(ImageCell::with_z_index(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            first_data,
            -1,
            0,
            0,
            0,
            0,
            Some(7),
            Some(11),
        )));
        attrs.attach_image(Box::new(ImageCell::with_z_index(
            TextureCoordinate::new_f32(0.0, 0.0),
            TextureCoordinate::new_f32(1.0, 1.0),
            second_data,
            2,
            0,
            0,
            0,
            0,
            Some(13),
            Some(17),
        )));

        let serialized = SerializedLines::from(vec![(5, Line::from_text("x", &attrs, 1, None))]);
        assert_eq!(serialized.validate_structure().unwrap().images, 2);
        let (_, images) = serialized.extract_data_checked().unwrap();
        assert_eq!(images.len(), 2);
        assert!(images
            .iter()
            .all(|image| { image.line_idx == 5 && image.cell_idx == 0 }));
        assert_eq!(
            images.iter().map(|image| image.z_index).collect::<Vec<_>>(),
            vec![-1, 2],
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
