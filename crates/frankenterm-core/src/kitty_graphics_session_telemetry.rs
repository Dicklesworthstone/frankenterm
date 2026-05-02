//! Per-session telemetry aggregator for Kitty graphics
//! (br-ft-jwst6 sub-task 8 substrate slice).
//!
//! The substrate's
//! [`StructuredLogRow`] and `render_log_jsonl` are
//! pure-data primitives; this module ships the **stateful
//! aggregator** the integration wraps around them: a
//! per-session collector that buffers per-action JSONL
//! rows, accumulates per-format / per-rejection-reason
//! histograms, and emits a session summary on close.
//!
//! [`StructuredLogRow`]: crate::kitty_graphics_compositor::StructuredLogRow
//!
//! ## Bead's sub-task 8
//!
//! > "JSON-line telemetry emit — substrate provides
//! > counters; integration emits per-action and per-session."
//!
//! Per-action emission already round-trips via
//! `render_log_jsonl` / `parse_log_jsonl`. The remaining
//! pieces are:
//!
//! 1. Per-action helpers that map a real outcome
//!    ([`ImageAcceptDecision`] / [`KittyQueryOutcome`] /
//!    eviction sweep) to the right
//!    [`StructuredLogRow`] variant. Without these the
//!    integration would re-implement the variant
//!    construction at every call site.
//! 2. Per-session aggregator that holds counters across
//!    actions, exposes a typed [`KittySessionSummary`] on
//!    finalize, and returns the buffered rows as a single
//!    JSONL blob for log shipping.
//!
//! [`ImageAcceptDecision`]: crate::kitty_graphics::ImageAcceptDecision
//! [`KittyQueryOutcome`]: crate::kitty_graphics_compositor::KittyQueryOutcome
//!
//! ## What this module is NOT
//!
//! - Not a wire-format change. The session-summary row is a
//!   sibling type to [`StructuredLogRow`], serialized via
//!   `serde_json` independently. The existing rows continue
//!   to round-trip unchanged.
//! - Not a log-shipping layer. The aggregator hands back
//!   `Vec<StructuredLogRow>` + `KittySessionSummary` blobs;
//!   the integration writes them to disk / network /
//!   stderr.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::kitty_graphics::{
    ImageAcceptDecision, ImageRejectionReason, KittyAction, KittyImageFormat,
};
use crate::kitty_graphics_compositor::{CompositorLayer, KittyQueryOutcome, StructuredLogRow};

// ============================================================================
// Slug mappings (kept in this module so the substrate's other
// types don't grow new pub fns just for telemetry)
// ============================================================================

/// Stable slug for a [`KittyImageFormat`] variant. Used as the
/// `format_slug` field of [`StructuredLogRow::ImageAdmitted`]
/// and as the histogram key in the session summary.
#[must_use]
pub const fn format_slug(format: KittyImageFormat) -> &'static str {
    match format {
        KittyImageFormat::Png => "png",
        KittyImageFormat::Rgb24 => "rgb24",
        KittyImageFormat::Rgba32 => "rgba32",
        KittyImageFormat::ZlibCompressed => "zlib_compressed",
    }
}

/// Stable slug for an [`ImageRejectionReason`] variant.
#[must_use]
pub const fn rejection_reason_slug(reason: ImageRejectionReason) -> &'static str {
    match reason {
        ImageRejectionReason::Oversized => "oversized",
        ImageRejectionReason::DecodeTimeout => "decode_timeout",
        ImageRejectionReason::Malformed => "malformed",
        ImageRejectionReason::DimensionsOverflow => "dimensions_overflow",
    }
}

/// Stable slug for the four-entry [`KittyAction`] subset that
/// appears in [`StructuredLogRow::QueryResponse`]. The action
/// is the protocol command letter.
#[must_use]
pub const fn action_slug(action: KittyAction) -> &'static str {
    match action {
        KittyAction::Transmit => "transmit",
        KittyAction::TransmitDisplay => "transmit_display",
        KittyAction::Place => "place",
        KittyAction::Delete => "delete",
        KittyAction::Query => "query",
    }
}

/// Stable slug for a [`KittyQueryOutcome`] variant. Used as
/// the `response_slug` field of
/// [`StructuredLogRow::QueryResponse`] so the integration
/// distinguishes ok/error responses without parsing the
/// nested struct.
#[must_use]
pub fn query_response_slug(outcome: &KittyQueryOutcome) -> &'static str {
    match outcome {
        KittyQueryOutcome::Ok { .. } => "ok",
        KittyQueryOutcome::Error { .. } => "error",
    }
}

/// Stable slug for a [`CompositorLayer`] variant. The
/// substrate's [`CompositorLayer`] only ships `z_index()`;
/// telemetry uses snake-case names so JSONL grep'ability
/// matches the rest of the rows.
#[must_use]
pub const fn layer_slug(layer: CompositorLayer) -> &'static str {
    match layer {
        CompositorLayer::Background => "background",
        CompositorLayer::Selection => "selection",
        CompositorLayer::Text => "text",
        CompositorLayer::Cursor => "cursor",
        CompositorLayer::Overlay => "overlay",
    }
}

// ============================================================================
// Per-action log-row builders
// ============================================================================

/// Build a [`StructuredLogRow::ImageAdmitted`] row from a
/// successful admission. The integration calls this from
/// the same code path that updates `KittyGraphicsTelemetry`
/// on accept.
#[must_use]
pub fn admitted_row(
    ts_ms: u64,
    image_id: u32,
    format: KittyImageFormat,
    bytes_in: u32,
    bytes_out: u32,
    decode_ns: u64,
    layer: CompositorLayer,
) -> StructuredLogRow {
    StructuredLogRow::ImageAdmitted {
        ts_ms,
        image_id,
        format_slug: format_slug(format).to_string(),
        bytes_in,
        bytes_out,
        decode_ns,
        layer_slug: layer_slug(layer).to_string(),
    }
}

/// Build a [`StructuredLogRow::ImageRejected`] row for a
/// rejection. `image_id` is the slot the integration would
/// have allocated; tests that assert "rejected payloads
/// don't consume id-space" pass a sentinel.
#[must_use]
pub fn rejected_row(ts_ms: u64, image_id: u32, reason: ImageRejectionReason) -> StructuredLogRow {
    StructuredLogRow::ImageRejected {
        ts_ms,
        image_id,
        reason_slug: rejection_reason_slug(reason).to_string(),
    }
}

/// Build a [`StructuredLogRow::QueryResponse`] row for a
/// query reply.
#[must_use]
pub fn query_response_row(
    ts_ms: u64,
    action: KittyAction,
    outcome: &KittyQueryOutcome,
) -> StructuredLogRow {
    let image_id = match outcome {
        KittyQueryOutcome::Ok { image_id } => *image_id,
        KittyQueryOutcome::Error { image_id, .. } => *image_id,
    };
    StructuredLogRow::QueryResponse {
        ts_ms,
        action_slug: action_slug(action).to_string(),
        image_id,
        response_slug: query_response_slug(outcome).to_string(),
    }
}

/// Build a [`StructuredLogRow::EvictionCycle`] row for a
/// cache-eviction sweep.
#[must_use]
pub fn eviction_row(ts_ms: u64, evicted_count: u32, freed_bytes: u64) -> StructuredLogRow {
    StructuredLogRow::EvictionCycle {
        ts_ms,
        evicted_count,
        freed_bytes,
    }
}

// ============================================================================
// Session summary
// ============================================================================

/// Per-session summary the aggregator emits at finalize.
/// Histograms are `BTreeMap<&'static str, u64>` so the JSON
/// output is canonically ordered for byte-level diffing
/// against expected fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KittySessionSummary {
    pub session_id: u64,
    pub started_ts_ms: u64,
    pub ended_ts_ms: u64,
    pub total_admitted: u64,
    pub total_rejected: u64,
    pub total_queries: u64,
    pub total_evictions: u64,
    pub total_evicted_images: u64,
    pub total_bytes_in: u64,
    pub total_bytes_out: u64,
    pub total_decode_ns: u64,
    pub total_freed_bytes: u64,
    /// `format_slug` → admitted-image count.
    pub admitted_by_format: BTreeMap<String, u64>,
    /// `reason_slug` → rejected-image count.
    pub rejected_by_reason: BTreeMap<String, u64>,
}

impl KittySessionSummary {
    /// Average decode time per admitted image, in nanoseconds.
    /// Returns 0 when no images were admitted.
    #[must_use]
    pub const fn avg_decode_ns(&self) -> u64 {
        if self.total_admitted == 0 {
            0
        } else {
            self.total_decode_ns / self.total_admitted
        }
    }

    /// Compression ratio across the session (bytes_out /
    /// bytes_in × 10000, integer-scaled to avoid float). 0
    /// when bytes_in is 0.
    #[must_use]
    pub const fn compression_ratio_bp(&self) -> u64 {
        if self.total_bytes_in == 0 {
            0
        } else {
            (self.total_bytes_out * 10_000) / self.total_bytes_in
        }
    }
}

/// Render a session summary as a single JSON line (no
/// trailing newline). The integration appends `\n` and
/// flushes.
#[must_use]
pub fn render_session_summary_jsonl(summary: &KittySessionSummary) -> String {
    serde_json::to_string(summary).expect("KittySessionSummary serializes")
}

// ============================================================================
// Aggregator
// ============================================================================

/// Stateful per-session telemetry collector.
///
/// One per Kitty-graphics session. Caller drives:
/// 1. `new(session_id, started_ts_ms)` at session start.
/// 2. `record_admitted` / `record_rejected` /
///    `record_query` / `record_eviction` per action.
/// 3. `flush_log_rows()` periodically (e.g., every 256 rows
///    or every second) to ship JSONL to the log sink.
/// 4. `finalize(ended_ts_ms)` at session end → returns
///    the [`KittySessionSummary`] for the per-session emit.
#[derive(Debug, Clone)]
pub struct KittySessionAggregator {
    summary: KittySessionSummary,
    pending_rows: Vec<StructuredLogRow>,
}

impl KittySessionAggregator {
    /// Construct a fresh session aggregator.
    #[must_use]
    pub fn new(session_id: u64, started_ts_ms: u64) -> Self {
        Self {
            summary: KittySessionSummary {
                session_id,
                started_ts_ms,
                ended_ts_ms: 0,
                total_admitted: 0,
                total_rejected: 0,
                total_queries: 0,
                total_evictions: 0,
                total_evicted_images: 0,
                total_bytes_in: 0,
                total_bytes_out: 0,
                total_decode_ns: 0,
                total_freed_bytes: 0,
                admitted_by_format: BTreeMap::new(),
                rejected_by_reason: BTreeMap::new(),
            },
            pending_rows: Vec::new(),
        }
    }

    /// Record one admitted image: bumps `total_admitted` +
    /// per-format histogram + byte counters + decode-time
    /// accumulator, then buffers the
    /// [`StructuredLogRow::ImageAdmitted`] row.
    pub fn record_admitted(
        &mut self,
        ts_ms: u64,
        image_id: u32,
        format: KittyImageFormat,
        bytes_in: u32,
        bytes_out: u32,
        decode_ns: u64,
        layer: CompositorLayer,
    ) {
        self.summary.total_admitted = self.summary.total_admitted.saturating_add(1);
        self.summary.total_bytes_in = self
            .summary
            .total_bytes_in
            .saturating_add(u64::from(bytes_in));
        self.summary.total_bytes_out = self
            .summary
            .total_bytes_out
            .saturating_add(u64::from(bytes_out));
        self.summary.total_decode_ns = self.summary.total_decode_ns.saturating_add(decode_ns);
        let slot = self
            .summary
            .admitted_by_format
            .entry(format_slug(format).to_string())
            .or_insert(0);
        *slot = slot.saturating_add(1);
        self.pending_rows.push(admitted_row(
            ts_ms, image_id, format, bytes_in, bytes_out, decode_ns, layer,
        ));
    }

    /// Record one rejected image.
    pub fn record_rejected(&mut self, ts_ms: u64, image_id: u32, reason: ImageRejectionReason) {
        self.summary.total_rejected = self.summary.total_rejected.saturating_add(1);
        let slot = self
            .summary
            .rejected_by_reason
            .entry(rejection_reason_slug(reason).to_string())
            .or_insert(0);
        *slot = slot.saturating_add(1);
        self.pending_rows
            .push(rejected_row(ts_ms, image_id, reason));
    }

    /// Record one query response.
    pub fn record_query(&mut self, ts_ms: u64, action: KittyAction, outcome: &KittyQueryOutcome) {
        self.summary.total_queries = self.summary.total_queries.saturating_add(1);
        self.pending_rows
            .push(query_response_row(ts_ms, action, outcome));
    }

    /// Record one cache-eviction sweep.
    pub fn record_eviction(&mut self, ts_ms: u64, evicted_count: u32, freed_bytes: u64) {
        self.summary.total_evictions = self.summary.total_evictions.saturating_add(1);
        self.summary.total_evicted_images = self
            .summary
            .total_evicted_images
            .saturating_add(u64::from(evicted_count));
        self.summary.total_freed_bytes = self.summary.total_freed_bytes.saturating_add(freed_bytes);
        self.pending_rows
            .push(eviction_row(ts_ms, evicted_count, freed_bytes));
    }

    /// Number of buffered (un-flushed) rows.
    #[must_use]
    pub fn pending_row_count(&self) -> usize {
        self.pending_rows.len()
    }

    /// Read-only summary snapshot — useful for `ft doctor`
    /// surfaces and tests. The caller can render this via
    /// `render_session_summary_jsonl` without finalizing.
    #[must_use]
    pub const fn summary(&self) -> &KittySessionSummary {
        &self.summary
    }

    /// Drain buffered rows and return them. The aggregator's
    /// internal counters remain untouched — only the buffer
    /// is reset.
    #[must_use]
    pub fn flush_log_rows(&mut self) -> Vec<StructuredLogRow> {
        std::mem::take(&mut self.pending_rows)
    }

    /// Finalize the session: stamp the end timestamp and
    /// return the summary. The aggregator can still be used
    /// after this (counters keep accumulating), but the
    /// returned summary is a snapshot.
    #[must_use]
    pub fn finalize(&mut self, ended_ts_ms: u64) -> KittySessionSummary {
        self.summary.ended_ts_ms = ended_ts_ms;
        self.summary.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kitty_graphics_compositor::KittyErrorCode;

    // ----------------------------------------------------------------
    // Slug mappings
    // ----------------------------------------------------------------

    #[test]
    fn format_slug_covers_every_variant() {
        assert_eq!(format_slug(KittyImageFormat::Png), "png");
        assert_eq!(format_slug(KittyImageFormat::Rgb24), "rgb24");
        assert_eq!(format_slug(KittyImageFormat::Rgba32), "rgba32");
        assert_eq!(
            format_slug(KittyImageFormat::ZlibCompressed),
            "zlib_compressed"
        );
    }

    #[test]
    fn rejection_reason_slug_covers_every_variant() {
        assert_eq!(
            rejection_reason_slug(ImageRejectionReason::Oversized),
            "oversized"
        );
        assert_eq!(
            rejection_reason_slug(ImageRejectionReason::DecodeTimeout),
            "decode_timeout"
        );
        assert_eq!(
            rejection_reason_slug(ImageRejectionReason::Malformed),
            "malformed"
        );
        assert_eq!(
            rejection_reason_slug(ImageRejectionReason::DimensionsOverflow),
            "dimensions_overflow"
        );
    }

    #[test]
    fn action_slug_covers_every_variant() {
        for action in [
            KittyAction::Transmit,
            KittyAction::TransmitDisplay,
            KittyAction::Place,
            KittyAction::Delete,
            KittyAction::Query,
        ] {
            // Slug must be non-empty and not contain whitespace.
            let slug = action_slug(action);
            assert!(!slug.is_empty());
            assert!(!slug.contains(' '));
        }
    }

    #[test]
    fn query_response_slug_distinguishes_ok_and_error() {
        let ok = KittyQueryOutcome::Ok { image_id: 1 };
        let err = KittyQueryOutcome::Error {
            image_id: 1,
            error_code: KittyErrorCode::Eninput,
        };
        assert_eq!(query_response_slug(&ok), "ok");
        assert_eq!(query_response_slug(&err), "error");
    }

    // ----------------------------------------------------------------
    // Per-action row builders
    // ----------------------------------------------------------------

    #[test]
    fn admitted_row_carries_all_fields() {
        let row = admitted_row(
            123_456,
            42,
            KittyImageFormat::Png,
            4_096,
            1_024,
            5_000_000,
            CompositorLayer::Background,
        );
        match row {
            StructuredLogRow::ImageAdmitted {
                ts_ms,
                image_id,
                format_slug,
                bytes_in,
                bytes_out,
                decode_ns,
                layer_slug,
            } => {
                assert_eq!(ts_ms, 123_456);
                assert_eq!(image_id, 42);
                assert_eq!(format_slug, "png");
                assert_eq!(bytes_in, 4_096);
                assert_eq!(bytes_out, 1_024);
                assert_eq!(decode_ns, 5_000_000);
                assert!(!layer_slug.is_empty());
            }
            other => panic!("expected ImageAdmitted, got {other:?}"),
        }
    }

    #[test]
    fn rejected_row_carries_reason() {
        let row = rejected_row(100, 7, ImageRejectionReason::Oversized);
        match row {
            StructuredLogRow::ImageRejected {
                ts_ms,
                image_id,
                reason_slug,
            } => {
                assert_eq!(ts_ms, 100);
                assert_eq!(image_id, 7);
                assert_eq!(reason_slug, "oversized");
            }
            other => panic!("expected ImageRejected, got {other:?}"),
        }
    }

    #[test]
    fn query_response_row_extracts_image_id_from_outcome() {
        let outcome_ok = KittyQueryOutcome::Ok { image_id: 99 };
        let row = query_response_row(7, KittyAction::Query, &outcome_ok);
        match row {
            StructuredLogRow::QueryResponse {
                ts_ms,
                action_slug,
                image_id,
                response_slug,
            } => {
                assert_eq!(ts_ms, 7);
                assert_eq!(action_slug, "query");
                assert_eq!(image_id, 99);
                assert_eq!(response_slug, "ok");
            }
            other => panic!("expected QueryResponse, got {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // Aggregator
    // ----------------------------------------------------------------

    #[test]
    fn new_aggregator_is_empty() {
        let agg = KittySessionAggregator::new(123, 1_000);
        let s = agg.summary();
        assert_eq!(s.session_id, 123);
        assert_eq!(s.started_ts_ms, 1_000);
        assert_eq!(s.ended_ts_ms, 0);
        assert_eq!(s.total_admitted, 0);
        assert_eq!(s.total_rejected, 0);
        assert_eq!(s.total_queries, 0);
        assert_eq!(s.total_evictions, 0);
        assert!(s.admitted_by_format.is_empty());
        assert!(s.rejected_by_reason.is_empty());
        assert_eq!(agg.pending_row_count(), 0);
    }

    #[test]
    fn record_admitted_accumulates_byte_and_decode_counters() {
        let mut agg = KittySessionAggregator::new(1, 0);
        agg.record_admitted(
            10,
            1,
            KittyImageFormat::Png,
            4_096,
            1_024,
            5_000_000,
            CompositorLayer::Background,
        );
        agg.record_admitted(
            20,
            2,
            KittyImageFormat::Rgba32,
            16_384,
            16_384,
            1_000_000,
            CompositorLayer::Text,
        );
        let s = agg.summary();
        assert_eq!(s.total_admitted, 2);
        assert_eq!(s.total_bytes_in, 4_096 + 16_384);
        assert_eq!(s.total_bytes_out, 1_024 + 16_384);
        assert_eq!(s.total_decode_ns, 6_000_000);
        assert_eq!(s.admitted_by_format.get("png"), Some(&1));
        assert_eq!(s.admitted_by_format.get("rgba32"), Some(&1));
    }

    #[test]
    fn record_rejected_accumulates_reason_histogram() {
        let mut agg = KittySessionAggregator::new(1, 0);
        agg.record_rejected(10, 1, ImageRejectionReason::Oversized);
        agg.record_rejected(20, 2, ImageRejectionReason::Oversized);
        agg.record_rejected(30, 3, ImageRejectionReason::Malformed);
        let s = agg.summary();
        assert_eq!(s.total_rejected, 3);
        assert_eq!(s.rejected_by_reason.get("oversized"), Some(&2));
        assert_eq!(s.rejected_by_reason.get("malformed"), Some(&1));
    }

    #[test]
    fn record_query_bumps_count_and_buffers_row() {
        let mut agg = KittySessionAggregator::new(1, 0);
        let outcome = KittyQueryOutcome::Ok { image_id: 5 };
        agg.record_query(10, KittyAction::Query, &outcome);
        assert_eq!(agg.summary().total_queries, 1);
        assert_eq!(agg.pending_row_count(), 1);
    }

    #[test]
    fn record_eviction_accumulates_counts_and_freed_bytes() {
        let mut agg = KittySessionAggregator::new(1, 0);
        agg.record_eviction(10, 3, 12_345);
        agg.record_eviction(20, 1, 999);
        let s = agg.summary();
        assert_eq!(s.total_evictions, 2);
        assert_eq!(s.total_evicted_images, 4);
        assert_eq!(s.total_freed_bytes, 12_345 + 999);
    }

    #[test]
    fn flush_log_rows_drains_buffer_without_resetting_counters() {
        let mut agg = KittySessionAggregator::new(1, 0);
        agg.record_admitted(
            10,
            1,
            KittyImageFormat::Png,
            100,
            50,
            1_000,
            CompositorLayer::Background,
        );
        agg.record_rejected(20, 2, ImageRejectionReason::Oversized);
        let drained = agg.flush_log_rows();
        assert_eq!(drained.len(), 2);
        assert_eq!(agg.pending_row_count(), 0);
        // Counters preserved.
        assert_eq!(agg.summary().total_admitted, 1);
        assert_eq!(agg.summary().total_rejected, 1);
        // A second flush returns empty.
        let drained2 = agg.flush_log_rows();
        assert!(drained2.is_empty());
    }

    #[test]
    fn finalize_stamps_end_timestamp_and_returns_clone() {
        let mut agg = KittySessionAggregator::new(7, 1_000);
        agg.record_admitted(
            10,
            1,
            KittyImageFormat::Png,
            100,
            50,
            500_000,
            CompositorLayer::Background,
        );
        let summary = agg.finalize(2_000);
        assert_eq!(summary.session_id, 7);
        assert_eq!(summary.started_ts_ms, 1_000);
        assert_eq!(summary.ended_ts_ms, 2_000);
        assert_eq!(summary.total_admitted, 1);
        // Aggregator's internal end-ts also stamped.
        assert_eq!(agg.summary().ended_ts_ms, 2_000);
    }

    #[test]
    fn render_session_summary_jsonl_is_one_line_and_roundtrips() {
        let mut agg = KittySessionAggregator::new(1, 0);
        agg.record_admitted(
            5,
            1,
            KittyImageFormat::Png,
            128,
            64,
            10_000,
            CompositorLayer::Background,
        );
        let summary = agg.finalize(100);
        let line = render_session_summary_jsonl(&summary);
        assert!(!line.contains('\n'), "JSONL line must not contain newline");
        let back: KittySessionSummary = serde_json::from_str(&line).unwrap();
        assert_eq!(back, summary);
    }

    // ----------------------------------------------------------------
    // Derived metrics
    // ----------------------------------------------------------------

    #[test]
    fn avg_decode_ns_zero_when_no_admissions() {
        let agg = KittySessionAggregator::new(1, 0);
        assert_eq!(agg.summary().avg_decode_ns(), 0);
    }

    #[test]
    fn avg_decode_ns_divides_total_by_admitted() {
        let mut agg = KittySessionAggregator::new(1, 0);
        agg.record_admitted(
            0,
            1,
            KittyImageFormat::Png,
            100,
            50,
            3_000_000,
            CompositorLayer::Background,
        );
        agg.record_admitted(
            0,
            2,
            KittyImageFormat::Png,
            100,
            50,
            5_000_000,
            CompositorLayer::Background,
        );
        // (3M + 5M) / 2 = 4M.
        assert_eq!(agg.summary().avg_decode_ns(), 4_000_000);
    }

    #[test]
    fn compression_ratio_bp_handles_zero_input() {
        let agg = KittySessionAggregator::new(1, 0);
        assert_eq!(agg.summary().compression_ratio_bp(), 0);
    }

    #[test]
    fn compression_ratio_bp_basis_points() {
        let mut agg = KittySessionAggregator::new(1, 0);
        // 1000 bytes in, 250 bytes out → 0.25 → 2500 bp.
        agg.record_admitted(
            0,
            1,
            KittyImageFormat::Png,
            1_000,
            250,
            0,
            CompositorLayer::Background,
        );
        assert_eq!(agg.summary().compression_ratio_bp(), 2_500);
    }

    // ----------------------------------------------------------------
    // End-to-end: clean session with mixed actions
    // ----------------------------------------------------------------

    #[test]
    fn scenario_mixed_session_finalizes_with_consistent_summary() {
        let mut agg = KittySessionAggregator::new(42, 1_000);

        // 3 admitted PNGs.
        for i in 0..3 {
            agg.record_admitted(
                1_000 + i * 10,
                i as u32 + 1,
                KittyImageFormat::Png,
                500,
                100,
                100_000,
                CompositorLayer::Background,
            );
        }
        // 1 admitted RGBA32.
        agg.record_admitted(
            1_100,
            10,
            KittyImageFormat::Rgba32,
            16_384,
            16_384,
            1_000,
            CompositorLayer::Text,
        );
        // 2 rejections.
        agg.record_rejected(1_200, 11, ImageRejectionReason::Oversized);
        agg.record_rejected(1_210, 12, ImageRejectionReason::DimensionsOverflow);
        // 1 query.
        agg.record_query(
            1_300,
            KittyAction::Query,
            &KittyQueryOutcome::Ok { image_id: 1 },
        );
        // 1 eviction sweep.
        agg.record_eviction(1_400, 2, 1_000);

        // Flush mid-session — all 4 + 2 + 1 + 1 = 8 rows.
        let mid_flush = agg.flush_log_rows();
        assert_eq!(mid_flush.len(), 8);

        let summary = agg.finalize(2_000);
        assert_eq!(summary.session_id, 42);
        assert_eq!(summary.total_admitted, 4);
        assert_eq!(summary.total_rejected, 2);
        assert_eq!(summary.total_queries, 1);
        assert_eq!(summary.total_evictions, 1);
        assert_eq!(summary.total_evicted_images, 2);
        assert_eq!(summary.admitted_by_format.get("png"), Some(&3));
        assert_eq!(summary.admitted_by_format.get("rgba32"), Some(&1));
        assert_eq!(summary.rejected_by_reason.get("oversized"), Some(&1));
        assert_eq!(
            summary.rejected_by_reason.get("dimensions_overflow"),
            Some(&1)
        );
    }
}
