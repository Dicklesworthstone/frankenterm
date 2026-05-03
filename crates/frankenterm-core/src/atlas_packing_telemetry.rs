//! JSONL telemetry for atlas packing scenarios (ft-gtcm9.4).
//!
//! Records one [`PackingEvent`] per glyph allocation as a single
//! JSON object on its own line. The schema follows the original
//! ft-i1y15 bead's declaration:
//!
//! ```text
//! { ts, glyph_id, glyph_size, allocation_pos, packer,
//!   wasted_space_after_alloc, total_atlas_bytes }
//! ```
//!
//! ## Why a separate module
//!
//! The [`crate::atlas_bin_packing`] substrate is pure logic — no
//! I/O, no serde dependency on per-event records. Telemetry is a
//! cross-cutting concern that belongs next to the substrate but
//! never inside it: this keeps the packers themselves testable
//! without serde-roundtrip noise and lets ops dial the recorder
//! in or out without touching the algorithm.
//!
//! ## Recording flow
//!
//! ```ignore
//! use frankenterm_core::atlas_bin_packing::{
//!     Atlas2DSize, GlyphSize, PackerKind, make_packer,
//! };
//! use frankenterm_core::atlas_packing_telemetry::PackingScenarioRecorder;
//!
//! let size = Atlas2DSize::try_new(2048, 2048).unwrap();
//! let packer = make_packer(PackerKind::Skyline, size);
//! let mut sink: Vec<u8> = Vec::new();
//! let mut recorder = PackingScenarioRecorder::new(packer, &mut sink);
//! for (id, glyph) in glyphs.iter().enumerate() {
//!     let _ = recorder.record_alloc(id as u64, *glyph);
//! }
//! // `sink` now holds one JSON object per line.
//! ```
//!
//! Each emitted line is a complete record — readers can stream the
//! file, parse line-by-line, and not worry about partial JSON in
//! the face of a crash mid-write. The recorder calls `flush()` on
//! Drop so callers do not have to thread a `flush()` step through
//! their own teardown.

use std::io::{self, Write};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::atlas_bin_packing::{
    AllocationOutcome, Atlas2DSize, BinPacker, GlyphSize, PackedRect, PackerKind, RejectReason,
};

/// Glyph dimensions as carried in the JSONL record. Mirrors
/// [`GlyphSize`] but with serde so the recorder doesn't have to
/// derive serde on the substrate type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GlyphSizeRecord {
    pub width: u32,
    pub height: u32,
}

impl From<GlyphSize> for GlyphSizeRecord {
    fn from(g: GlyphSize) -> Self {
        Self {
            width: g.width,
            height: g.height,
        }
    }
}

/// Allocation position as carried in the JSONL record. `None` for
/// rejected allocations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AllocationPosRecord {
    pub x: u32,
    pub y: u32,
}

impl From<PackedRect> for AllocationPosRecord {
    fn from(r: PackedRect) -> Self {
        Self { x: r.x, y: r.y }
    }
}

/// Stable string label for [`PackerKind`]. Carried as a string in
/// the JSONL record so log readers don't have to track the enum's
/// discriminant ordering across substrate revisions.
#[must_use]
pub fn packer_label(kind: PackerKind) -> &'static str {
    match kind {
        PackerKind::Shelf => "Shelf",
        PackerKind::Skyline => "Skyline",
        PackerKind::MaximalRectangles => "MaximalRectangles",
    }
}

/// One JSONL record per atlas allocation attempt. The schema is
/// fixed by the ft-i1y15 bead — additions must append at the tail
/// so existing readers keep parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackingEvent {
    /// Timestamp of the allocation in milliseconds since the Unix
    /// epoch. UTC, monotonic within a single recorder instance only
    /// (clock skew across hosts is not normalized — the consumer
    /// joins on `run_id` if cross-host correlation is needed).
    pub ts: u64,
    /// Caller-supplied glyph identifier. Has no schema-level
    /// meaning; the recorder does not interpret it.
    pub glyph_id: u64,
    pub glyph_size: GlyphSizeRecord,
    /// `Some` for placed glyphs, `None` for rejected ones.
    /// Serde emits `null` for `None`; consumers should treat absent
    /// + null as equivalent.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub allocation_pos: Option<AllocationPosRecord>,
    /// Stable label from [`packer_label`] — `"Shelf"` /
    /// `"Skyline"` / `"MaximalRectangles"`.
    pub packer: String,
    /// Atlas bytes minus used bytes after the allocation completed.
    /// Computed in u64 so the per-event delta cannot wrap on a
    /// 4096² atlas.
    pub wasted_space_after_alloc: u64,
    /// Total atlas size in bytes (`atlas.width * atlas.height`).
    /// Constant across all events from a single recorder; included
    /// per-event so a single line is enough to compute efficiency
    /// without joining against a header.
    pub total_atlas_bytes: u64,
    /// Reject reason as a stable label, present only for rejected
    /// allocations. Schema-level optional so older readers ignore
    /// it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reject_reason: Option<String>,
}

impl PackingEvent {
    /// Serialize the event as a single JSON line (no trailing
    /// newline). Writers append the newline themselves so the
    /// record can be embedded inside a larger newline-delimited
    /// stream.
    pub fn to_jsonl_line(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

#[must_use]
fn reject_label(reason: RejectReason) -> &'static str {
    match reason {
        RejectReason::GlyphWiderThanAtlas => "GlyphWiderThanAtlas",
        RejectReason::GlyphTallerThanAtlas => "GlyphTallerThanAtlas",
        RejectReason::AtlasFull => "AtlasFull",
    }
}

/// Wraps a `Box<dyn BinPacker>` and emits one [`PackingEvent`] per
/// allocation attempt to the supplied writer. The recorder keeps a
/// running `used_bytes` total internally so the per-event
/// `wasted_space_after_alloc` field is exact rather than
/// approximated from the packer's own placements.
///
/// Lifetimes: the recorder borrows the writer for the duration of
/// the scenario. Callers that need ownership should pass an owned
/// `Vec<u8>` or a `BufWriter<File>`; both implement `Write`.
pub struct PackingScenarioRecorder<W: Write> {
    packer: Box<dyn BinPacker>,
    writer: W,
    total_atlas_bytes: u64,
    used_bytes: u64,
    now_ms: fn() -> u64,
}

impl<W: Write> PackingScenarioRecorder<W> {
    /// New recorder over the given packer + writer. Uses the wall
    /// clock for `ts`; tests that need deterministic timestamps
    /// should call [`Self::with_clock`].
    pub fn new(packer: Box<dyn BinPacker>, writer: W) -> Self {
        Self::with_clock(packer, writer, default_now_ms)
    }

    /// New recorder with a custom clock function. Used by the
    /// inline tests to pin `ts` at a known value so the JSONL
    /// fixture is reproducible.
    pub fn with_clock(packer: Box<dyn BinPacker>, writer: W, now_ms: fn() -> u64) -> Self {
        let total_atlas_bytes = packer.size().area();
        Self {
            packer,
            writer,
            total_atlas_bytes,
            used_bytes: 0,
            now_ms,
        }
    }

    /// Record one allocation attempt. Threads the outcome back to
    /// the caller so the scenario can drive its own loop control.
    /// Writer errors propagate as `io::Error`; the underlying
    /// packer call is infallible (mirrors [`BinPacker::try_alloc`]).
    pub fn record_alloc(
        &mut self,
        glyph_id: u64,
        glyph: GlyphSize,
    ) -> io::Result<AllocationOutcome> {
        let outcome = self.packer.try_alloc(glyph);
        let (allocation_pos, reject_reason) = match outcome {
            AllocationOutcome::Placed(rect) => {
                self.used_bytes = self.used_bytes.saturating_add(rect.area());
                (Some(AllocationPosRecord::from(rect)), None)
            }
            AllocationOutcome::Rejected(reason) => (None, Some(reject_label(reason).to_string())),
        };
        let wasted_space_after_alloc = self.total_atlas_bytes.saturating_sub(self.used_bytes);
        let event = PackingEvent {
            ts: (self.now_ms)(),
            glyph_id,
            glyph_size: GlyphSizeRecord::from(glyph),
            allocation_pos,
            packer: packer_label(self.packer.kind()).to_string(),
            wasted_space_after_alloc,
            total_atlas_bytes: self.total_atlas_bytes,
            reject_reason,
        };
        let line = event
            .to_jsonl_line()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.writer.write_all(line.as_bytes())?;
        self.writer.write_all(b"\n")?;
        Ok(outcome)
    }

    /// Atlas dimensions the recorder was constructed with. Pass-
    /// through for the inner [`BinPacker::size`].
    pub fn atlas_size(&self) -> Atlas2DSize {
        self.packer.size()
    }

    /// Flush the underlying writer. Auto-called from `Drop`; expose
    /// it so callers that need errors-as-results can flush
    /// explicitly before the recorder goes out of scope.
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write> Drop for PackingScenarioRecorder<W> {
    fn drop(&mut self) {
        // br-ft-x2oyy: function-level drop contract. This recorder exposes
        // `flush()` for callers that need errors-as-results; Drop is only a
        // destructor safety net and must never panic while unwinding.
        //
        // Best-effort flush. If the writer is poisoned the next allocation
        // would have surfaced the error already.
        // br-ft-x2oyy: intentional Drop-path swallow; Rust destructors have
        // no result channel and must not panic on best-effort cleanup.
        let _ = self.writer.flush();
    }
}

fn default_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas_bin_packing::make_packer;

    fn fixed_clock() -> u64 {
        1_700_000_000_000
    }

    fn atlas() -> Atlas2DSize {
        Atlas2DSize::try_new(64, 64).expect("atlas")
    }

    fn glyph(w: u32, h: u32) -> GlyphSize {
        GlyphSize::new(w, h)
    }

    #[test]
    fn packer_label_covers_every_kind() {
        assert_eq!(packer_label(PackerKind::Shelf), "Shelf");
        assert_eq!(packer_label(PackerKind::Skyline), "Skyline");
        assert_eq!(
            packer_label(PackerKind::MaximalRectangles),
            "MaximalRectangles"
        );
    }

    #[test]
    fn packing_event_serializes_to_single_line_with_required_fields() {
        let event = PackingEvent {
            ts: 1_700_000_000_000,
            glyph_id: 42,
            glyph_size: GlyphSizeRecord {
                width: 10,
                height: 12,
            },
            allocation_pos: Some(AllocationPosRecord { x: 0, y: 0 }),
            packer: "Skyline".to_string(),
            wasted_space_after_alloc: 4_096 - 120,
            total_atlas_bytes: 4_096,
            reject_reason: None,
        };
        let line = event.to_jsonl_line().expect("serialize");
        assert!(!line.contains('\n'), "JSONL line must not contain newlines");
        let v: serde_json::Value = serde_json::from_str(&line).expect("parse");
        for required in [
            "ts",
            "glyph_id",
            "glyph_size",
            "allocation_pos",
            "packer",
            "wasted_space_after_alloc",
            "total_atlas_bytes",
        ] {
            assert!(
                v.get(required).is_some(),
                "schema field `{required}` missing from {line}"
            );
        }
        assert_eq!(
            v.get("reject_reason"),
            None,
            "reject_reason omitted on Placed"
        );
    }

    #[test]
    fn rejected_event_serializes_with_reject_reason_and_no_pos() {
        let event = PackingEvent {
            ts: 1,
            glyph_id: 7,
            glyph_size: GlyphSizeRecord {
                width: 999,
                height: 1,
            },
            allocation_pos: None,
            packer: "Shelf".to_string(),
            wasted_space_after_alloc: 4_096,
            total_atlas_bytes: 4_096,
            reject_reason: Some("GlyphWiderThanAtlas".to_string()),
        };
        let v: serde_json::Value = serde_json::from_str(&event.to_jsonl_line().unwrap()).unwrap();
        assert_eq!(
            v.get("reject_reason").and_then(|x| x.as_str()),
            Some("GlyphWiderThanAtlas")
        );
        assert!(
            v.get("allocation_pos").is_none(),
            "allocation_pos must be omitted on rejection"
        );
    }

    #[test]
    fn packing_event_roundtrips_through_serde() {
        let event = PackingEvent {
            ts: 12345,
            glyph_id: 1,
            glyph_size: GlyphSizeRecord {
                width: 5,
                height: 5,
            },
            allocation_pos: Some(AllocationPosRecord { x: 10, y: 20 }),
            packer: "MaximalRectangles".to_string(),
            wasted_space_after_alloc: 100,
            total_atlas_bytes: 200,
            reject_reason: None,
        };
        let line = event.to_jsonl_line().unwrap();
        let parsed: PackingEvent = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed, event);
    }

    #[test]
    fn recorder_emits_one_line_per_alloc() {
        let mut sink = Vec::<u8>::new();
        {
            let mut rec = PackingScenarioRecorder::with_clock(
                make_packer(PackerKind::Skyline, atlas()),
                &mut sink,
                fixed_clock,
            );
            rec.record_alloc(0, glyph(10, 10)).unwrap();
            rec.record_alloc(1, glyph(20, 20)).unwrap();
            rec.record_alloc(2, glyph(5, 5)).unwrap();
        }
        let s = String::from_utf8(sink).unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert_eq!(lines.len(), 3, "one line per alloc");
        for line in &lines {
            let _: PackingEvent = serde_json::from_str(line).expect("each line is valid JSON");
        }
    }

    #[test]
    fn recorder_tracks_wasted_space_after_each_alloc() {
        let mut sink = Vec::<u8>::new();
        {
            let mut rec = PackingScenarioRecorder::with_clock(
                make_packer(PackerKind::Shelf, atlas()),
                &mut sink,
                fixed_clock,
            );
            // First alloc 10x10 = 100 bytes. Total atlas = 64*64 = 4096.
            rec.record_alloc(0, glyph(10, 10)).unwrap();
            // Second alloc 20x10 = 200 bytes. Cumulative used = 300.
            rec.record_alloc(1, glyph(20, 10)).unwrap();
        }
        let s = String::from_utf8(sink).unwrap();
        let events: Vec<PackingEvent> = s
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].wasted_space_after_alloc, 4096 - 100);
        assert_eq!(events[1].wasted_space_after_alloc, 4096 - 300);
        assert_eq!(events[0].total_atlas_bytes, 4096);
        assert_eq!(events[1].total_atlas_bytes, 4096);
    }

    #[test]
    fn recorder_emits_reject_reason_for_oversized_glyph() {
        let mut sink = Vec::<u8>::new();
        {
            let mut rec = PackingScenarioRecorder::with_clock(
                make_packer(PackerKind::Shelf, atlas()),
                &mut sink,
                fixed_clock,
            );
            rec.record_alloc(0, glyph(100, 1)).unwrap(); // wider
        }
        let s = String::from_utf8(sink).unwrap();
        let event: PackingEvent = serde_json::from_str(s.trim()).unwrap();
        assert_eq!(event.allocation_pos, None);
        assert_eq!(event.reject_reason.as_deref(), Some("GlyphWiderThanAtlas"));
        // Wasted space unchanged on rejection.
        assert_eq!(event.wasted_space_after_alloc, 4096);
    }

    #[test]
    fn recorder_threads_packer_label_through_each_event() {
        let mut sink = Vec::<u8>::new();
        {
            let mut rec = PackingScenarioRecorder::with_clock(
                make_packer(PackerKind::MaximalRectangles, atlas()),
                &mut sink,
                fixed_clock,
            );
            rec.record_alloc(0, glyph(10, 10)).unwrap();
        }
        let event: PackingEvent =
            serde_json::from_str(String::from_utf8(sink).unwrap().trim()).unwrap();
        assert_eq!(event.packer, "MaximalRectangles");
    }

    #[test]
    fn recorder_returns_outcome_from_underlying_packer() {
        let mut sink = Vec::<u8>::new();
        let mut rec = PackingScenarioRecorder::with_clock(
            make_packer(PackerKind::Shelf, atlas()),
            &mut sink,
            fixed_clock,
        );
        let outcome = rec.record_alloc(0, glyph(10, 10)).unwrap();
        match outcome {
            AllocationOutcome::Placed(rect) => {
                assert_eq!(rect.width, 10);
                assert_eq!(rect.height, 10);
            }
            AllocationOutcome::Rejected(_) => panic!("expected Placed"),
        }
    }

    #[test]
    fn recorder_atlas_size_passes_through() {
        let sink = Vec::<u8>::new();
        let rec = PackingScenarioRecorder::new(make_packer(PackerKind::Skyline, atlas()), sink);
        assert_eq!(rec.atlas_size(), atlas());
    }

    #[test]
    fn recorder_with_io_error_writer_surfaces_error() {
        struct ExplodingWriter;
        impl Write for ExplodingWriter {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                Err(io::Error::new(io::ErrorKind::Other, "fault"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut rec = PackingScenarioRecorder::with_clock(
            make_packer(PackerKind::Shelf, atlas()),
            ExplodingWriter,
            fixed_clock,
        );
        let err = rec.record_alloc(0, glyph(10, 10)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }
}
