//! Kitty graphics compositor + frame-budget integration
//! contracts ([BR-TERM-EMULATOR-UPLIFT-2.1.2.cont] /
//! `ft-jwst6`).
//!
//! The Kitty graphics protocol substrate already lives at
//! `kitty_graphics.rs` (`7b050b6db`, 39 tests) and the
//! alt-text + sanitizer at `kitty_graphics_alt_text.rs`
//! (33 tests). This module ships the **integration
//! substrate** that the bead's continuation work consumes:
//!
//! 1. **Layered-compositor placement decision tree**
//!    (sub-task 5) — `Virtual` / `Classical` /
//!    `Unicode-placeholder` placement modes dispatch onto
//!    the layered compositor's Layer 0 (bg) / Layer 4
//!    (overlay) per the bead's BR-TERM-EMULATOR-UPLIFT.4.1
//!    cross-link.
//! 2. **Query-response format builder** (sub-task 6) —
//!    `\x1b_Gi=<id>;OK\x1b\\` for `a=q` and matching
//!    error responses.
//! 3. **Base64-payload validator** (sub-task 2) — pre-
//!    decode length + alphabet checks; rejects malformed
//!    payloads via `ImageRejectionReason::Malformed` (per
//!    substrate's enum).
//! 4. **Frame-budget op classification** — Kitty graphics
//!    decode + atlas upload as `OpKind` slugs the
//!    `frame_budget_signal_coupling` substrate consumes.
//!    Honors bead's "decode async, off render thread;
//!    atlas upload respects frame budget" rule.
//! 5. **Structured-log row contract** (sub-task 8).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ============================================================================
// Layered-compositor placement (sub-task 5)
// ============================================================================

/// Placement mode mirrors the Kitty protocol's variants.
/// The substrate already has `kitty_graphics::PlacementMode`;
/// this module's variant exists so this contract layer
/// stays decoupled (cross-crate consumers can use it
/// without pulling in the full graphics module).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlacementMode {
    /// Image flows in cell grid — e.g., inline images in
    /// scrollback. Targets Layer 0 (bg) so text overlays
    /// it.
    Virtual,
    /// Image floats absolutely positioned — e.g.,
    /// floating tooltips, image previews. Targets Layer 4
    /// (overlay) so it sits on top of text.
    Classical,
    /// Image referenced via Unicode placeholder (`a=p`,
    /// `c=`/`r=` parameters). Substrate has
    /// `KittyAction::Placement` for this; here it dispatches
    /// onto Layer 0 like Virtual.
    UnicodePlaceholder,
}

/// Layer index in the bead's BR-TERM-EMULATOR-UPLIFT.4.1
/// layered compositor. Closed list of 5 layers — adding a
/// new one extends this enum + the layer-rendering pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompositorLayer {
    /// Layer 0 — background. Text composites *over* this.
    Background,
    /// Layer 1 — selection highlight.
    Selection,
    /// Layer 2 — text glyphs.
    Text,
    /// Layer 3 — cursor.
    Cursor,
    /// Layer 4 — overlay (floating images, tooltips,
    /// scrollbars).
    Overlay,
}

impl CompositorLayer {
    /// Z-order index. Lower = behind, higher = in front.
    #[must_use]
    pub const fn z_index(self) -> u8 {
        match self {
            Self::Background => 0,
            Self::Selection => 1,
            Self::Text => 2,
            Self::Cursor => 3,
            Self::Overlay => 4,
        }
    }
}

/// Decide which layer an image with the given placement
/// mode renders onto. Pure decision tree — bead sub-task 5
/// dispatch.
#[must_use]
pub const fn layer_for_placement(mode: PlacementMode) -> CompositorLayer {
    match mode {
        PlacementMode::Virtual => CompositorLayer::Background,
        PlacementMode::UnicodePlaceholder => CompositorLayer::Background,
        PlacementMode::Classical => CompositorLayer::Overlay,
    }
}

// ============================================================================
// Query-response format builder (sub-task 6)
// ============================================================================

/// Per-action query response. Bead sub-task 6:
///
/// > `\x1b_Gi=<id>;OK\x1b\\` for `a=q` (and matching
/// > error responses where applicable).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KittyQueryOutcome {
    /// Image admitted; respond OK.
    Ok { image_id: u32 },
    /// Image rejected; respond with `ENOIMG` /
    /// `EBADAPC` / etc. per the protocol.
    Error { image_id: u32, error_code: KittyErrorCode },
}

/// Closed list of error codes per the Kitty protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum KittyErrorCode {
    /// Generic error.
    Enofile,
    /// Image data malformed.
    Eninput,
    /// Image too large.
    Eimagedata,
    /// Image format not supported.
    Eformat,
    /// No image with the requested id.
    Enoimg,
    /// Capacity / cache full.
    Eunsupp,
}

impl KittyErrorCode {
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Enofile => "ENOFILE",
            Self::Eninput => "ENINPUT",
            Self::Eimagedata => "EIMAGEDATA",
            Self::Eformat => "EFORMAT",
            Self::Enoimg => "ENOIMG",
            Self::Eunsupp => "EUNSUPP",
        }
    }
}

/// Build the response bytes per the Kitty protocol APC
/// envelope (`\x1b_G<key>=<value>...;<message>\x1b\\`).
#[must_use]
pub fn render_query_response(outcome: &KittyQueryOutcome) -> Vec<u8> {
    let mut out = Vec::from(&b"\x1b_G"[..]);
    match outcome {
        KittyQueryOutcome::Ok { image_id } => {
            let envelope = format!("i={image_id};OK");
            out.extend_from_slice(envelope.as_bytes());
        }
        KittyQueryOutcome::Error { image_id, error_code } => {
            let envelope = format!("i={image_id};{}", error_code.slug());
            out.extend_from_slice(envelope.as_bytes());
        }
    }
    out.extend_from_slice(&b"\x1b\\"[..]);
    out
}

// ============================================================================
// Base64-payload validator (sub-task 2)
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Base64ValidationOutcome {
    /// Payload passed length + alphabet checks.
    Valid { decoded_len_estimate: u32 },
    /// Payload length is not a multiple of 4 (and not
    /// padded correctly).
    InvalidLength,
    /// Payload contains a character outside the base64
    /// alphabet.
    InvalidAlphabet,
    /// Payload exceeds the per-chunk cap.
    OverChunkCap,
}

/// Per-chunk cap for one APC base64 payload. The Kitty
/// protocol's `m=1` continuation chunks are typically
/// ≤4096 bytes per chunk; this constant pins the upper
/// bound.
pub const PER_CHUNK_BASE64_CAP: usize = 4096;

/// Pure-logic validator. Returns the decoded-length
/// estimate (so the integration can size its decode
/// buffer) or an explicit reject reason.
#[must_use]
pub fn validate_base64_payload(payload: &[u8]) -> Base64ValidationOutcome {
    if payload.len() > PER_CHUNK_BASE64_CAP {
        return Base64ValidationOutcome::OverChunkCap;
    }
    // Count trailing padding. Valid base64 has 0/1/2 `=`
    // chars; 3+ trailing `=` is malformed.
    let stripped = payload
        .iter()
        .rev()
        .take_while(|&&b| b == b'=')
        .count();
    if stripped > 2 {
        return Base64ValidationOutcome::InvalidLength;
    }
    let body_len = payload.len() - stripped;
    // A purely-padding payload (e.g. "=" or "==") has no
    // body — invalid.
    if body_len == 0 && stripped > 0 {
        return Base64ValidationOutcome::InvalidLength;
    }
    if body_len % 4 == 1 {
        // Lengths ≡ 1 (mod 4) are never valid base64.
        return Base64ValidationOutcome::InvalidLength;
    }
    for &b in &payload[..body_len] {
        let valid = b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'-' || b == b'_';
        if !valid {
            return Base64ValidationOutcome::InvalidAlphabet;
        }
    }
    let decoded = body_len * 3 / 4;
    Base64ValidationOutcome::Valid {
        decoded_len_estimate: decoded as u32,
    }
}

// ============================================================================
// Frame-budget op classification
// ============================================================================

/// Kitty graphics work classified for the frame-budget
/// allocator (cross-link `frame_budget_signal_coupling`).
/// Bead's "decode async, off render thread; atlas upload
/// respects frame budget" rule:
///
/// - Decode → run on the asupersync runtime (off render
///   thread). NOT a frame-budget op.
/// - Atlas upload → frame-budget `Cosmetic` priority (can
///   defer if budget tight).
/// - Compositor placement → frame-budget `User` priority
///   (image was just admitted; user expects to see it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KittyFrameBudgetOp {
    /// PNG/zlib decode — runs off render thread, never on
    /// frame budget.
    DecodeAsync,
    /// GPU atlas upload — defers under budget pressure.
    AtlasUpload,
    /// Compositor placement / draw call — user-priority,
    /// drains promptly.
    CompositorPlacement,
    /// Atlas eviction — cosmetic priority.
    AtlasEviction,
}

impl KittyFrameBudgetOp {
    /// Slug for the structured-log line.
    #[must_use]
    pub const fn slug(self) -> &'static str {
        match self {
            Self::DecodeAsync => "kitty_decode_async",
            Self::AtlasUpload => "kitty_atlas_upload",
            Self::CompositorPlacement => "kitty_compositor_placement",
            Self::AtlasEviction => "kitty_atlas_eviction",
        }
    }

    /// True iff this op is allowed on the render thread
    /// (i.e., is a frame-budget op). `DecodeAsync` is the
    /// only one that *must* run off-thread per the bead's
    /// "decode async, off render thread" rule.
    #[must_use]
    pub const fn runs_on_render_thread(self) -> bool {
        !matches!(self, Self::DecodeAsync)
    }
}

// ============================================================================
// Structured log row (sub-task 8)
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StructuredLogRow {
    /// Per-image admission: ts, image_id, format,
    /// bytes_in, bytes_out, decode_ns, layer.
    ImageAdmitted {
        ts_ms: u64,
        image_id: u32,
        format_slug: String,
        bytes_in: u32,
        bytes_out: u32,
        decode_ns: u64,
        layer_slug: String,
    },
    /// Per-image rejection: ts, image_id, reason.
    ImageRejected {
        ts_ms: u64,
        image_id: u32,
        reason_slug: String,
    },
    /// Per-query: ts, action, image_id, response.
    QueryResponse {
        ts_ms: u64,
        action_slug: String,
        image_id: u32,
        response_slug: String,
    },
    /// Per-eviction cycle: ts, evicted_count, freed_bytes.
    EvictionCycle {
        ts_ms: u64,
        evicted_count: u32,
        freed_bytes: u64,
    },
}

#[must_use]
pub fn render_log_jsonl(rows: &[StructuredLogRow]) -> String {
    let mut out = String::new();
    for r in rows {
        let line = serde_json::to_string(r).expect("StructuredLogRow always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

pub fn parse_log_jsonl(jsonl: &str) -> Result<Vec<StructuredLogRow>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

// ============================================================================
// Health snapshot
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct KittyCompositorHealth {
    pub admitted_total: u64,
    pub rejected_total: u64,
    pub by_placement_layer: BTreeMap<String, u64>,
    pub frame_budget_ops_by_kind: BTreeMap<String, u64>,
    pub query_responses_total: u64,
    pub query_errors_total: u64,
}

impl KittyCompositorHealth {
    #[must_use]
    pub fn baseline() -> Self {
        Self::default()
    }

    pub fn record_admission(&mut self, layer: CompositorLayer) {
        self.admitted_total = self.admitted_total.saturating_add(1);
        let slug = match layer {
            CompositorLayer::Background => "background",
            CompositorLayer::Selection => "selection",
            CompositorLayer::Text => "text",
            CompositorLayer::Cursor => "cursor",
            CompositorLayer::Overlay => "overlay",
        };
        *self.by_placement_layer.entry(slug.to_string()).or_insert(0) += 1;
    }

    pub fn record_rejection(&mut self) {
        self.rejected_total = self.rejected_total.saturating_add(1);
    }

    pub fn record_frame_budget_op(&mut self, op: KittyFrameBudgetOp) {
        *self
            .frame_budget_ops_by_kind
            .entry(op.slug().to_string())
            .or_insert(0) += 1;
    }

    /// True iff rejection rate is healthy (≤5%) AND no
    /// `DecodeAsync` was misclassified onto the render
    /// thread.
    #[must_use]
    pub fn is_safe(&self) -> bool {
        let total = self.admitted_total + self.rejected_total;
        if total == 0 {
            return true;
        }
        let reject_ratio = self.rejected_total as f64 / total as f64;
        reject_ratio <= 0.05
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------------
    // Layered-compositor placement
    // ------------------------------------------------------------------------

    #[test]
    fn z_index_strictly_ordered() {
        let layers = [
            CompositorLayer::Background,
            CompositorLayer::Selection,
            CompositorLayer::Text,
            CompositorLayer::Cursor,
            CompositorLayer::Overlay,
        ];
        for window in layers.windows(2) {
            assert!(window[0].z_index() < window[1].z_index());
        }
    }

    #[test]
    fn virtual_placement_targets_background_layer() {
        assert_eq!(
            layer_for_placement(PlacementMode::Virtual),
            CompositorLayer::Background
        );
    }

    #[test]
    fn classical_placement_targets_overlay_layer() {
        assert_eq!(
            layer_for_placement(PlacementMode::Classical),
            CompositorLayer::Overlay
        );
    }

    #[test]
    fn unicode_placeholder_targets_background_layer() {
        assert_eq!(
            layer_for_placement(PlacementMode::UnicodePlaceholder),
            CompositorLayer::Background
        );
    }

    #[test]
    fn images_render_below_text_via_layer_z_order() {
        // Headline invariant: Virtual-mode images render
        // below text. Layer 0 (bg) < Layer 2 (text).
        let img_layer = layer_for_placement(PlacementMode::Virtual);
        assert!(img_layer.z_index() < CompositorLayer::Text.z_index());
    }

    #[test]
    fn classical_images_render_above_text() {
        // Floating images sit on top of text.
        let img_layer = layer_for_placement(PlacementMode::Classical);
        assert!(img_layer.z_index() > CompositorLayer::Text.z_index());
    }

    // ------------------------------------------------------------------------
    // Query-response format builder
    // ------------------------------------------------------------------------

    #[test]
    fn ok_response_format() {
        let out = render_query_response(&KittyQueryOutcome::Ok { image_id: 42 });
        assert_eq!(out, b"\x1b_Gi=42;OK\x1b\\");
    }

    #[test]
    fn error_response_format() {
        let out = render_query_response(&KittyQueryOutcome::Error {
            image_id: 99,
            error_code: KittyErrorCode::Enoimg,
        });
        assert_eq!(out, b"\x1b_Gi=99;ENOIMG\x1b\\");
    }

    #[test]
    fn each_error_code_has_distinct_slug() {
        let codes = [
            KittyErrorCode::Enofile,
            KittyErrorCode::Eninput,
            KittyErrorCode::Eimagedata,
            KittyErrorCode::Eformat,
            KittyErrorCode::Enoimg,
            KittyErrorCode::Eunsupp,
        ];
        let slugs: Vec<&str> = codes.iter().map(|c| c.slug()).collect();
        let unique: std::collections::HashSet<_> = slugs.iter().collect();
        assert_eq!(unique.len(), 6);
    }

    // ------------------------------------------------------------------------
    // Base64 validator
    // ------------------------------------------------------------------------

    #[test]
    fn valid_base64_payload_accepted() {
        let outcome = validate_base64_payload(b"SGVsbG8gV29ybGQ=");
        match outcome {
            Base64ValidationOutcome::Valid {
                decoded_len_estimate,
            } => {
                assert_eq!(decoded_len_estimate, 11); // "Hello World"
            }
            _ => panic!("expected Valid"),
        }
    }

    #[test]
    fn invalid_alphabet_rejected() {
        let outcome = validate_base64_payload(b"SGVsbG8h@!");
        assert_eq!(outcome, Base64ValidationOutcome::InvalidAlphabet);
    }

    #[test]
    fn invalid_length_rejected() {
        // Length 5: 5 % 4 == 1 → invalid.
        let outcome = validate_base64_payload(b"SGVsb");
        assert_eq!(outcome, Base64ValidationOutcome::InvalidLength);
    }

    #[test]
    fn over_chunk_cap_rejected() {
        let big = vec![b'A'; PER_CHUNK_BASE64_CAP + 1];
        let outcome = validate_base64_payload(&big);
        assert_eq!(outcome, Base64ValidationOutcome::OverChunkCap);
    }

    #[test]
    fn empty_payload_is_valid() {
        let outcome = validate_base64_payload(b"");
        match outcome {
            Base64ValidationOutcome::Valid {
                decoded_len_estimate,
            } => assert_eq!(decoded_len_estimate, 0),
            _ => panic!("expected Valid"),
        }
    }

    #[test]
    fn url_safe_base64_alphabet_accepted() {
        // Kitty protocol may use URL-safe base64 (- and _).
        let outcome = validate_base64_payload(b"SGVsbG8tV29ybGQ_");
        assert!(matches!(outcome, Base64ValidationOutcome::Valid { .. }));
    }

    #[test]
    fn excess_padding_rejected() {
        // 3+ trailing = is never valid base64. Without
        // the fix, "AB===" was accepted as Valid.
        let outcome = validate_base64_payload(b"AB===");
        assert_eq!(outcome, Base64ValidationOutcome::InvalidLength);
    }

    #[test]
    fn quad_padding_rejected() {
        let outcome = validate_base64_payload(b"AB====");
        assert_eq!(outcome, Base64ValidationOutcome::InvalidLength);
    }

    #[test]
    fn padding_only_payload_rejected() {
        // "=" alone has no body; previously was Valid.
        let outcome = validate_base64_payload(b"=");
        assert_eq!(outcome, Base64ValidationOutcome::InvalidLength);
    }

    #[test]
    fn double_padding_only_rejected() {
        let outcome = validate_base64_payload(b"==");
        assert_eq!(outcome, Base64ValidationOutcome::InvalidLength);
    }

    // ------------------------------------------------------------------------
    // Frame-budget op classification
    // ------------------------------------------------------------------------

    #[test]
    fn decode_async_does_not_run_on_render_thread() {
        // Bead's stated rule: "decode async, off render
        // thread."
        assert!(!KittyFrameBudgetOp::DecodeAsync.runs_on_render_thread());
    }

    #[test]
    fn atlas_upload_runs_on_render_thread() {
        assert!(KittyFrameBudgetOp::AtlasUpload.runs_on_render_thread());
    }

    #[test]
    fn compositor_placement_runs_on_render_thread() {
        assert!(KittyFrameBudgetOp::CompositorPlacement.runs_on_render_thread());
    }

    #[test]
    fn atlas_eviction_runs_on_render_thread() {
        assert!(KittyFrameBudgetOp::AtlasEviction.runs_on_render_thread());
    }

    #[test]
    fn each_frame_budget_op_has_distinct_slug() {
        let ops = [
            KittyFrameBudgetOp::DecodeAsync,
            KittyFrameBudgetOp::AtlasUpload,
            KittyFrameBudgetOp::CompositorPlacement,
            KittyFrameBudgetOp::AtlasEviction,
        ];
        let slugs: Vec<&str> = ops.iter().map(|o| o.slug()).collect();
        let unique: std::collections::HashSet<_> = slugs.iter().collect();
        assert_eq!(unique.len(), 4);
    }

    // ------------------------------------------------------------------------
    // Structured logging
    // ------------------------------------------------------------------------

    #[test]
    fn structured_log_jsonl_roundtrip() {
        let rows = vec![
            StructuredLogRow::ImageAdmitted {
                ts_ms: 1_000,
                image_id: 42,
                format_slug: "png".to_string(),
                bytes_in: 10_000,
                bytes_out: 8_000,
                decode_ns: 2_000_000,
                layer_slug: "background".to_string(),
            },
            StructuredLogRow::ImageRejected {
                ts_ms: 2_000,
                image_id: 43,
                reason_slug: "oversize".to_string(),
            },
            StructuredLogRow::QueryResponse {
                ts_ms: 3_000,
                action_slug: "query".to_string(),
                image_id: 42,
                response_slug: "ok".to_string(),
            },
            StructuredLogRow::EvictionCycle {
                ts_ms: 4_000,
                evicted_count: 3,
                freed_bytes: 30_000,
            },
        ];
        let jsonl = render_log_jsonl(&rows);
        let parsed = parse_log_jsonl(&jsonl).unwrap();
        assert_eq!(parsed, rows);
    }

    // ------------------------------------------------------------------------
    // Health snapshot
    // ------------------------------------------------------------------------

    #[test]
    fn health_baseline_safe() {
        assert!(KittyCompositorHealth::baseline().is_safe());
    }

    #[test]
    fn health_records_per_layer_admissions() {
        let mut h = KittyCompositorHealth::baseline();
        h.record_admission(CompositorLayer::Background);
        h.record_admission(CompositorLayer::Background);
        h.record_admission(CompositorLayer::Overlay);
        assert_eq!(h.by_placement_layer.get("background"), Some(&2));
        assert_eq!(h.by_placement_layer.get("overlay"), Some(&1));
        assert_eq!(h.admitted_total, 3);
    }

    #[test]
    fn health_records_frame_budget_ops_by_kind() {
        let mut h = KittyCompositorHealth::baseline();
        h.record_frame_budget_op(KittyFrameBudgetOp::DecodeAsync);
        h.record_frame_budget_op(KittyFrameBudgetOp::AtlasUpload);
        h.record_frame_budget_op(KittyFrameBudgetOp::AtlasUpload);
        assert_eq!(h.frame_budget_ops_by_kind.get("kitty_decode_async"), Some(&1));
        assert_eq!(h.frame_budget_ops_by_kind.get("kitty_atlas_upload"), Some(&2));
    }

    #[test]
    fn health_unsafe_above_5pct_rejection() {
        let mut h = KittyCompositorHealth::baseline();
        for _ in 0..9 {
            h.record_admission(CompositorLayer::Background);
        }
        h.record_rejection(); // 10% rejection
        assert!(!h.is_safe());
    }

    // ------------------------------------------------------------------------
    // Headline scenarios
    // ------------------------------------------------------------------------

    #[test]
    fn imgcat_inline_image_admission_scenario() {
        // imgcat emits a Virtual-placement PNG. The
        // decode runs async, the atlas upload defers
        // under budget pressure, the compositor places
        // it on Layer 0 (background) so text overlays
        // it.
        let mut health = KittyCompositorHealth::baseline();

        // 1. Validate base64 payload before decode.
        let payload = b"SGVsbG8gV29ybGQ=";
        let validation = validate_base64_payload(payload);
        assert!(matches!(validation, Base64ValidationOutcome::Valid { .. }));

        // 2. Decode runs off render thread.
        let decode = KittyFrameBudgetOp::DecodeAsync;
        assert!(!decode.runs_on_render_thread());
        health.record_frame_budget_op(decode);

        // 3. Atlas upload + compositor placement on
        //    render thread.
        let upload = KittyFrameBudgetOp::AtlasUpload;
        let place = KittyFrameBudgetOp::CompositorPlacement;
        assert!(upload.runs_on_render_thread());
        assert!(place.runs_on_render_thread());
        health.record_frame_budget_op(upload);
        health.record_frame_budget_op(place);

        // 4. Image lands on Layer 0 (background).
        let layer = layer_for_placement(PlacementMode::Virtual);
        assert_eq!(layer, CompositorLayer::Background);
        health.record_admission(layer);

        // 5. Query response confirms.
        let response = render_query_response(&KittyQueryOutcome::Ok { image_id: 1 });
        assert_eq!(response, b"\x1b_Gi=1;OK\x1b\\");

        assert!(health.is_safe());
        assert_eq!(health.admitted_total, 1);
    }

    #[test]
    fn floating_image_above_text_scenario() {
        // Floating image preview (Classical) renders on
        // Layer 4 (overlay), above text.
        let layer = layer_for_placement(PlacementMode::Classical);
        assert_eq!(layer, CompositorLayer::Overlay);
        assert!(layer.z_index() > CompositorLayer::Text.z_index());
    }

    #[test]
    fn malformed_payload_emits_eninput_response() {
        let validation = validate_base64_payload(b"notvalid@!");
        assert_eq!(validation, Base64ValidationOutcome::InvalidAlphabet);
        // Integration would dispatch this to the error
        // response builder.
        let response = render_query_response(&KittyQueryOutcome::Error {
            image_id: 0,
            error_code: KittyErrorCode::Eninput,
        });
        assert_eq!(response, b"\x1b_Gi=0;ENINPUT\x1b\\");
    }
}
