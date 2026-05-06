//! Kitty graphics protocol substrate (ft-2okh0.1.2).
//!
//! Pure-logic substrate for the bead's "Kitty graphics protocol —
//! APC-encoded image transmission". The integration crate handles
//! the actual APC byte parsing, base64 decode, PNG/zlib decode,
//! GPU atlas upload, and render-layer compositing; this module
//! ships the protocol enums, multi-frame reassembly state machine,
//! image-cache policy + rejection rules, LRU eviction policy, and
//! telemetry counters.
//!
//! ## What this module ships
//!
//! - `KittyAction` 5-variant `Transmit / TransmitDisplay / Place /
//!   Delete / Query` — the bead's `a=t/T/p/d/q` command set.
//! - `KittyImageFormat` 4-variant `Png / Rgb24 / Rgba32 /
//!   ZlibCompressed` covering Kitty's `f=` field.
//! - `PlacementMode` 2-variant `Virtual { z, cell_x, cell_y } /
//!   Classical { px_x, px_y }`.
//! - `MultiFrameState` accumulator — bead's `m=1`-more / `m=0`-last
//!   reassembly. Tracks chunks; `feed_chunk` returns
//!   `MultiFrameOutcome::{ Accumulating, Complete { total_bytes },
//!   ProtocolError }`.
//! - `ImageId(u32)` opaque image handle.
//! - `ImageCacheEntry` per-image metadata.
//! - `ImageCachePolicy` operator-tunable: 16 MiB per image,
//!   64 MiB per pane, 500 ms decode timeout, 16 384²
//!   dimension cap.
//! - `should_accept_image` decision tree with the bead's
//!   security gates.
//! - `ImageRejectionReason` 4-variant
//!   `Oversized / DecodeTimeout / Malformed / DimensionsOverflow`.
//! - `select_eviction_target` LRU pick.
//! - `KittyGraphicsTelemetry` per the bead's structured logging.
//!
//! ## What is deferred to ft-2okh0.1.2.cont
//!
//! - APC byte parsing in `frankenterm/escape-parser`
//!   (`\x1b_G<options>;<base64-payload>\x1b\\`).
//! - Base64 decode of the payload.
//! - PNG / zlib decoding via the `image` crate.
//! - GPU upload as an atlas region (cross-link
//!   sparse_texture_atlas.rs).
//! - Render integration into the layered compositor.
//! - Query response emission
//!   (`\x1b_Gi=<id>;OK\x1b\\`).
//! - Async decode timeout via `runtime_async::timeout_with_cx`.

#![allow(dead_code)]

// ============================================================================
// Protocol commands
// ============================================================================

/// Kitty graphics action — the bead's `a=` field. Maps directly
/// to the protocol's command letter (see ft-2okh0.1.2 background
/// section).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KittyAction {
    /// `a=t` — transmit image bytes; do not display yet.
    Transmit,
    /// `a=T` — transmit + immediately display.
    TransmitDisplay,
    /// `a=p` — place a previously transmitted image.
    Place,
    /// `a=d` — delete an image (or images) from the cache.
    Delete,
    /// `a=q` — query whether the terminal supports the protocol.
    Query,
}

impl KittyAction {
    /// Single-letter token from the protocol. Returns `None` for
    /// unrecognised letters; the integration's parser uses this
    /// to validate the `a=` field.
    #[must_use]
    pub const fn from_letter(letter: char) -> Option<Self> {
        match letter {
            't' => Some(Self::Transmit),
            'T' => Some(Self::TransmitDisplay),
            'p' => Some(Self::Place),
            'd' => Some(Self::Delete),
            'q' => Some(Self::Query),
            _ => None,
        }
    }

    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            Self::Transmit => 't',
            Self::TransmitDisplay => 'T',
            Self::Place => 'p',
            Self::Delete => 'd',
            Self::Query => 'q',
        }
    }

    /// Whether this command writes payload bytes (and thus
    /// participates in multi-frame reassembly).
    #[must_use]
    pub const fn carries_payload(self) -> bool {
        matches!(self, Self::Transmit | Self::TransmitDisplay)
    }
}

// ============================================================================
// Image format
// ============================================================================

/// The bead's `f=` field. Kitty supports 24 (RGB raw), 32 (RGBA
/// raw), 100 (PNG); plus a `o=z` flag for zlib compression on top
/// of any pixel format. Substrate flattens that to the four
/// useful combinations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KittyImageFormat {
    /// `f=100` — PNG.
    #[default]
    Png,
    /// `f=24` — raw RGB.
    Rgb24,
    /// `f=32` — raw RGBA.
    Rgba32,
    /// `f=24` or `f=32` with `o=z` — pixel data behind a zlib
    /// stream. The integration's decoder wraps this around
    /// the relevant raw format.
    ZlibCompressed,
}

// ============================================================================
// Placement
// ============================================================================

/// Kitty supports two placement modes per the protocol. The
/// integration's renderer dispatches on this enum to put the
/// image in either the cell grid (virtual, layered) or at a
/// pixel position (classical, overlay).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PlacementMode {
    /// Image flows in the cell grid, anchored at `cell_x/cell_y`,
    /// stacked at z-index `z`. Multiple images can occupy the
    /// same cell with different `z`; higher draws over lower.
    Virtual { z: i32, cell_x: u32, cell_y: u32 },
    /// Image floats at absolute pixel `px_x/px_y` regardless of
    /// the underlying cell layout. Bead's "absolute pixel
    /// position" mode.
    Classical { px_x: i32, px_y: i32 },
}

// ============================================================================
// Multi-frame reassembly
// ============================================================================

/// State machine that reassembles multi-chunk image payloads.
/// Per the bead: `m=1` means more frames follow; `m=0` is the
/// last frame.
///
/// **Integrity invariant**: fields are `pub(crate)`. Mutation
/// goes through [`feed_chunk`] (which enforces the
/// id-constancy + post-completion checks) or [`Self::reset`]
/// (cleared back to start). External code that resets
/// `completed=false` mid-stream would bypass the
/// `ProtocolError` check at `feed_chunk` line "if state.completed";
/// field privacy structurally prevents that.
#[derive(Debug, Clone, Default)]
pub struct MultiFrameState {
    /// Which image-id this accumulator is collecting for. Set
    /// on the first chunk; subsequent chunks must match.
    pub(crate) current_image_id: Option<ImageId>,
    /// Total bytes received so far.
    pub(crate) bytes_received: u64,
    /// Number of chunks received so far.
    pub(crate) chunks_received: u32,
    /// Whether `m=0` has been seen.
    pub(crate) completed: bool,
}

impl MultiFrameState {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current_image_id: None,
            bytes_received: 0,
            chunks_received: 0,
            completed: false,
        }
    }

    /// Reset to the starting state (after a completion or error).
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    // ----- Read-only accessors -----

    #[must_use]
    pub const fn current_image_id(&self) -> Option<ImageId> {
        self.current_image_id
    }

    #[must_use]
    pub const fn bytes_received(&self) -> u64 {
        self.bytes_received
    }

    #[must_use]
    pub const fn chunks_received(&self) -> u32 {
        self.chunks_received
    }

    #[must_use]
    pub const fn completed(&self) -> bool {
        self.completed
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MultiFrameOutcome {
    /// More frames expected.
    Accumulating,
    /// `m=0` observed; the integration can decode now.
    Complete { total_bytes: u64, chunks: u32 },
    /// Stream-level error: image-id changed mid-flight, or
    /// chunk arrived after completion. Caller resets.
    ProtocolError,
}

/// Whether this chunk is the last one (m=0) in the multi-frame
/// stream. The bead's `m=1`-more / `m=0`-last fields are
/// semantically a single bool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChunkContinuation {
    More,
    Final,
}

/// Feed a chunk into the accumulator. `chunk_image_id` is the
/// `i=<n>` field; substrate verifies it stays constant across
/// the whole stream.
pub fn feed_chunk(
    state: &mut MultiFrameState,
    chunk_image_id: ImageId,
    chunk_bytes: u64,
    continuation: ChunkContinuation,
) -> MultiFrameOutcome {
    if state.completed {
        return MultiFrameOutcome::ProtocolError;
    }
    match state.current_image_id {
        None => state.current_image_id = Some(chunk_image_id),
        Some(existing) if existing == chunk_image_id => {}
        Some(_) => return MultiFrameOutcome::ProtocolError,
    }

    state.bytes_received = state.bytes_received.saturating_add(chunk_bytes);
    state.chunks_received = state.chunks_received.saturating_add(1);

    match continuation {
        ChunkContinuation::More => MultiFrameOutcome::Accumulating,
        ChunkContinuation::Final => {
            state.completed = true;
            MultiFrameOutcome::Complete {
                total_bytes: state.bytes_received,
                chunks: state.chunks_received,
            }
        }
    }
}

// ============================================================================
// Image identity + metadata
// ============================================================================

/// Image identifier from the protocol's `i=` field. 32-bit per
/// Kitty's spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct ImageId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageCacheEntry {
    pub id: ImageId,
    pub width: u32,
    pub height: u32,
    pub format: KittyImageFormat,
    /// Decoded byte size (post-decompress, post-base64).
    pub bytes_decoded: u64,
    /// Whether the integration has already uploaded the bytes
    /// to the GPU atlas. Substrate doesn't care, but exposes
    /// the field so the integration can avoid double-upload.
    pub gpu_resident: bool,
    /// Last access timestamp (ms since epoch). LRU eviction
    /// keys off this.
    pub last_access_ts_ms: u64,
}

impl ImageCacheEntry {
    #[must_use]
    pub fn idle_ms(&self, now_ms: u64) -> u64 {
        now_ms.saturating_sub(self.last_access_ts_ms)
    }

    pub fn touch(&mut self, now_ms: u64) {
        self.last_access_ts_ms = now_ms;
    }
}

// ============================================================================
// Cache policy + security gates
// ============================================================================

/// Operator-tunable image-cache config. Bead defaults pinned
/// here.
///
/// Fields are `pub(crate)` because these are **security caps**:
/// `per_image_max_bytes` + `max_dimension_px` are the
/// image-bomb guard. Arbitrary code paths must not be able to
/// silently raise either cap. Use the
/// [`Self::with_per_image_max_bytes`] etc. builder API for
/// explicit reconstruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCachePolicy {
    /// Per-image decoded-byte cap. Bead default 16 MiB.
    pub(crate) per_image_max_bytes: u64,
    /// Per-pane total cache cap. Bead default 64 MiB.
    pub(crate) per_pane_max_bytes: u64,
    /// Per-image decode timeout. Bead default 500 ms (the
    /// integration enforces; substrate just records).
    pub(crate) decode_timeout_ms: u32,
    /// Maximum claimed dimension on either axis. Bead default
    /// 16 384 (matches typical GPU 2D-texture limits).
    pub(crate) max_dimension_px: u32,
}

pub const DEFAULT_PER_IMAGE_MAX_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_PER_PANE_MAX_BYTES: u64 = 64 * 1024 * 1024;
pub const DEFAULT_DECODE_TIMEOUT_MS: u32 = 500;
pub const DEFAULT_MAX_DIMENSION_PX: u32 = 16_384;

impl Default for ImageCachePolicy {
    fn default() -> Self {
        Self {
            per_image_max_bytes: DEFAULT_PER_IMAGE_MAX_BYTES,
            per_pane_max_bytes: DEFAULT_PER_PANE_MAX_BYTES,
            decode_timeout_ms: DEFAULT_DECODE_TIMEOUT_MS,
            max_dimension_px: DEFAULT_MAX_DIMENSION_PX,
        }
    }
}

impl ImageCachePolicy {
    // ----- Read-only accessors -----

    #[must_use]
    pub const fn per_image_max_bytes(self) -> u64 {
        self.per_image_max_bytes
    }
    #[must_use]
    pub const fn per_pane_max_bytes(self) -> u64 {
        self.per_pane_max_bytes
    }
    #[must_use]
    pub const fn decode_timeout_ms(self) -> u32 {
        self.decode_timeout_ms
    }
    #[must_use]
    pub const fn max_dimension_px(self) -> u32 {
        self.max_dimension_px
    }

    // ----- Builder API (security-policy changes are explicit reconstruction events) -----

    #[must_use]
    pub const fn with_per_image_max_bytes(mut self, bytes: u64) -> Self {
        self.per_image_max_bytes = bytes;
        self
    }
    #[must_use]
    pub const fn with_per_pane_max_bytes(mut self, bytes: u64) -> Self {
        self.per_pane_max_bytes = bytes;
        self
    }
    #[must_use]
    pub const fn with_decode_timeout_ms(mut self, ms: u32) -> Self {
        self.decode_timeout_ms = ms;
        self
    }
    #[must_use]
    pub const fn with_max_dimension_px(mut self, px: u32) -> Self {
        self.max_dimension_px = px;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageRejectionReason {
    /// Decoded bytes exceed `per_image_max_bytes`.
    Oversized,
    /// Decode took longer than `decode_timeout_ms` (integration
    /// reports this; substrate just classifies).
    DecodeTimeout,
    /// PNG/zlib decoder reported invalid input.
    Malformed,
    /// Claimed dimensions exceed `max_dimension_px` on either
    /// axis (bead's image-bomb guard).
    DimensionsOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageAcceptDecision {
    Accepted,
    Rejected(ImageRejectionReason),
}

impl ImageAcceptDecision {
    #[must_use]
    pub fn is_accepted(self) -> bool {
        matches!(self, Self::Accepted)
    }
}

/// Composed security gate. Order matters:
///
/// 1. Dimensions overflow → reject (cheapest check; any image
///    claiming dimensions over the cap is rejected before we
///    look at byte size).
/// 2. Per-image byte cap → reject.
/// 3. (Decode timeout / malformed are signalled by the
///    integration; substrate accepts/classifies.)
///
/// `decoded_bytes` is the actual post-decode byte count;
/// `claimed_width/height` are the protocol-claimed dimensions.
#[must_use]
pub fn should_accept_image(
    claimed_width: u32,
    claimed_height: u32,
    decoded_bytes: u64,
    policy: ImageCachePolicy,
) -> ImageAcceptDecision {
    if claimed_width > policy.max_dimension_px || claimed_height > policy.max_dimension_px {
        return ImageAcceptDecision::Rejected(ImageRejectionReason::DimensionsOverflow);
    }
    if decoded_bytes > policy.per_image_max_bytes {
        return ImageAcceptDecision::Rejected(ImageRejectionReason::Oversized);
    }
    ImageAcceptDecision::Accepted
}

/// Whether the per-pane budget can absorb a new image of
/// `incoming_bytes` size given `current_used_bytes`. The
/// integration calls this before insertion; if `false`, the
/// integration runs `select_eviction_target` to free space.
#[must_use]
pub fn pane_budget_has_room(
    current_used_bytes: u64,
    incoming_bytes: u64,
    policy: ImageCachePolicy,
) -> bool {
    current_used_bytes.saturating_add(incoming_bytes) <= policy.per_pane_max_bytes
}

// ============================================================================
// Eviction (LRU)
// ============================================================================

/// Pick the image-cache entry to evict: largest `idle_ms`. Ties
/// broken by lower `id` for deterministic telemetry.
#[must_use]
pub fn select_eviction_target<'a>(
    entries: &'a [ImageCacheEntry],
    now_ms: u64,
) -> Option<&'a ImageCacheEntry> {
    entries.iter().max_by(|a, b| {
        a.idle_ms(now_ms)
            .cmp(&b.idle_ms(now_ms))
            .then_with(|| b.id.0.cmp(&a.id.0))
    })
}

// ============================================================================
// Telemetry
// ============================================================================

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KittyGraphicsTelemetry {
    pub images_transmitted: u64,
    pub images_displayed: u64,
    pub images_deleted: u64,
    pub multi_frame_completions: u64,
    pub multi_frame_protocol_errors: u64,
    pub peak_image_cache_bytes: u64,
    pub decode_errors: u64,
    pub rejected_oversized: u64,
    pub rejected_decode_timeout: u64,
    pub rejected_malformed: u64,
    pub rejected_dim_overflow: u64,
    pub queries_answered: u64,
}

impl KittyGraphicsTelemetry {
    pub fn record_action(&mut self, action: KittyAction) {
        match action {
            KittyAction::Transmit => {
                self.images_transmitted = self.images_transmitted.saturating_add(1);
            }
            KittyAction::TransmitDisplay => {
                self.images_transmitted = self.images_transmitted.saturating_add(1);
                self.images_displayed = self.images_displayed.saturating_add(1);
            }
            KittyAction::Place => {
                self.images_displayed = self.images_displayed.saturating_add(1);
            }
            KittyAction::Delete => {
                self.images_deleted = self.images_deleted.saturating_add(1);
            }
            KittyAction::Query => {
                self.queries_answered = self.queries_answered.saturating_add(1);
            }
        }
    }

    pub fn record_decision(&mut self, decision: ImageAcceptDecision) {
        if let ImageAcceptDecision::Rejected(reason) = decision {
            let slot = match reason {
                ImageRejectionReason::Oversized => &mut self.rejected_oversized,
                ImageRejectionReason::DecodeTimeout => &mut self.rejected_decode_timeout,
                ImageRejectionReason::Malformed => &mut self.rejected_malformed,
                ImageRejectionReason::DimensionsOverflow => &mut self.rejected_dim_overflow,
            };
            *slot = slot.saturating_add(1);
        }
    }

    pub fn record_multi_frame(&mut self, outcome: MultiFrameOutcome) {
        match outcome {
            MultiFrameOutcome::Complete { .. } => {
                self.multi_frame_completions = self.multi_frame_completions.saturating_add(1);
            }
            MultiFrameOutcome::ProtocolError => {
                self.multi_frame_protocol_errors =
                    self.multi_frame_protocol_errors.saturating_add(1);
            }
            MultiFrameOutcome::Accumulating => {}
        }
    }

    pub fn record_decode_error(&mut self) {
        self.decode_errors = self.decode_errors.saturating_add(1);
    }

    pub fn record_cache_size(&mut self, current_bytes: u64) {
        if current_bytes > self.peak_image_cache_bytes {
            self.peak_image_cache_bytes = current_bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, last_access_ts_ms: u64, bytes_decoded: u64) -> ImageCacheEntry {
        ImageCacheEntry {
            id: ImageId(id),
            width: 100,
            height: 100,
            format: KittyImageFormat::Png,
            bytes_decoded,
            gpu_resident: false,
            last_access_ts_ms,
        }
    }

    // ----------------------------------------------------------------
    // KittyAction
    // ----------------------------------------------------------------

    #[test]
    fn action_letter_roundtrip() {
        for action in [
            KittyAction::Transmit,
            KittyAction::TransmitDisplay,
            KittyAction::Place,
            KittyAction::Delete,
            KittyAction::Query,
        ] {
            let letter = action.letter();
            assert_eq!(KittyAction::from_letter(letter), Some(action));
        }
    }

    #[test]
    fn action_unknown_letter_none() {
        assert_eq!(KittyAction::from_letter('x'), None);
        assert_eq!(KittyAction::from_letter('P'), None); // case matters
    }

    #[test]
    fn action_carries_payload_only_for_t_and_upper_t() {
        assert!(KittyAction::Transmit.carries_payload());
        assert!(KittyAction::TransmitDisplay.carries_payload());
        assert!(!KittyAction::Place.carries_payload());
        assert!(!KittyAction::Delete.carries_payload());
        assert!(!KittyAction::Query.carries_payload());
    }

    // ----------------------------------------------------------------
    // KittyImageFormat
    // ----------------------------------------------------------------

    #[test]
    fn format_default_png() {
        assert_eq!(KittyImageFormat::default(), KittyImageFormat::Png);
    }

    // ----------------------------------------------------------------
    // PlacementMode
    // ----------------------------------------------------------------

    #[test]
    fn placement_virtual_carries_z_and_cell() {
        let p = PlacementMode::Virtual {
            z: 3,
            cell_x: 5,
            cell_y: 7,
        };
        match p {
            PlacementMode::Virtual { z, cell_x, cell_y } => {
                assert_eq!(z, 3);
                assert_eq!(cell_x, 5);
                assert_eq!(cell_y, 7);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn placement_classical_pixel() {
        let p = PlacementMode::Classical {
            px_x: 100,
            px_y: 200,
        };
        match p {
            PlacementMode::Classical { px_x, px_y } => {
                assert_eq!(px_x, 100);
                assert_eq!(px_y, 200);
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    // ----------------------------------------------------------------
    // MultiFrameState
    // ----------------------------------------------------------------

    #[test]
    fn multi_frame_single_chunk_completes() {
        let mut state = MultiFrameState::new();
        let outcome = feed_chunk(&mut state, ImageId(1), 1024, ChunkContinuation::Final);
        assert_eq!(
            outcome,
            MultiFrameOutcome::Complete {
                total_bytes: 1024,
                chunks: 1,
            }
        );
    }

    #[test]
    fn multi_frame_accumulates_then_completes() {
        let mut state = MultiFrameState::new();
        let r1 = feed_chunk(&mut state, ImageId(7), 512, ChunkContinuation::More);
        let r2 = feed_chunk(&mut state, ImageId(7), 512, ChunkContinuation::More);
        let r3 = feed_chunk(&mut state, ImageId(7), 256, ChunkContinuation::Final);
        assert_eq!(r1, MultiFrameOutcome::Accumulating);
        assert_eq!(r2, MultiFrameOutcome::Accumulating);
        assert_eq!(
            r3,
            MultiFrameOutcome::Complete {
                total_bytes: 1280,
                chunks: 3,
            }
        );
    }

    #[test]
    fn multi_frame_id_change_mid_stream_is_protocol_error() {
        let mut state = MultiFrameState::new();
        feed_chunk(&mut state, ImageId(1), 100, ChunkContinuation::More);
        let outcome = feed_chunk(&mut state, ImageId(2), 100, ChunkContinuation::More);
        assert_eq!(outcome, MultiFrameOutcome::ProtocolError);
    }

    #[test]
    fn multi_frame_chunk_after_completion_is_protocol_error() {
        let mut state = MultiFrameState::new();
        feed_chunk(&mut state, ImageId(1), 100, ChunkContinuation::Final);
        let outcome = feed_chunk(&mut state, ImageId(1), 100, ChunkContinuation::More);
        assert_eq!(outcome, MultiFrameOutcome::ProtocolError);
    }

    #[test]
    fn multi_frame_reset_clears_state() {
        let mut state = MultiFrameState::new();
        feed_chunk(&mut state, ImageId(1), 100, ChunkContinuation::Final);
        state.reset();
        assert_eq!(state.current_image_id, None);
        assert_eq!(state.bytes_received, 0);
        assert!(!state.completed);
        // Fresh stream after reset works.
        let outcome = feed_chunk(&mut state, ImageId(2), 200, ChunkContinuation::Final);
        assert!(matches!(outcome, MultiFrameOutcome::Complete { .. }));
    }

    // ----------------------------------------------------------------
    // ImageCachePolicy defaults
    // ----------------------------------------------------------------

    #[test]
    fn cache_policy_defaults_match_bead() {
        let p = ImageCachePolicy::default();
        assert_eq!(p.per_image_max_bytes, 16 * 1024 * 1024);
        assert_eq!(p.per_pane_max_bytes, 64 * 1024 * 1024);
        assert_eq!(p.decode_timeout_ms, 500);
        assert_eq!(p.max_dimension_px, 16_384);
    }

    // ----------------------------------------------------------------
    // should_accept_image — security gates
    // ----------------------------------------------------------------

    #[test]
    fn accept_normal_image() {
        let policy = ImageCachePolicy::default();
        let d = should_accept_image(800, 600, 1_440_000, policy);
        assert_eq!(d, ImageAcceptDecision::Accepted);
    }

    #[test]
    fn reject_dimensions_overflow_width() {
        let policy = ImageCachePolicy::default();
        let d = should_accept_image(20_000, 600, 1024, policy);
        assert_eq!(
            d,
            ImageAcceptDecision::Rejected(ImageRejectionReason::DimensionsOverflow),
        );
    }

    #[test]
    fn reject_dimensions_overflow_height() {
        let policy = ImageCachePolicy::default();
        let d = should_accept_image(600, 20_000, 1024, policy);
        assert_eq!(
            d,
            ImageAcceptDecision::Rejected(ImageRejectionReason::DimensionsOverflow),
        );
    }

    #[test]
    fn reject_oversized_bytes() {
        let policy = ImageCachePolicy::default();
        // 17 MiB > 16 MiB cap.
        let d = should_accept_image(800, 600, 17 * 1024 * 1024, policy);
        assert_eq!(
            d,
            ImageAcceptDecision::Rejected(ImageRejectionReason::Oversized),
        );
    }

    #[test]
    fn dimensions_check_runs_before_byte_check() {
        // Both gates would fire; DimensionsOverflow wins per
        // substrate's documented order.
        let policy = ImageCachePolicy::default();
        let d = should_accept_image(20_000, 20_000, 100 * 1024 * 1024, policy);
        assert_eq!(
            d,
            ImageAcceptDecision::Rejected(ImageRejectionReason::DimensionsOverflow),
        );
    }

    #[test]
    fn accept_at_exact_boundary() {
        let policy = ImageCachePolicy::default();
        // Exactly at dimension cap + exactly at byte cap.
        let d = should_accept_image(16_384, 16_384, 16 * 1024 * 1024, policy);
        assert_eq!(d, ImageAcceptDecision::Accepted);
    }

    #[test]
    fn image_cache_policy_builder_round_trips() {
        // Pin the builder API surface — pub(crate) fields can't
        // be mutated from outside, must use builder.
        let policy = ImageCachePolicy::default()
            .with_per_image_max_bytes(8 * 1024 * 1024)
            .with_per_pane_max_bytes(32 * 1024 * 1024)
            .with_decode_timeout_ms(1000)
            .with_max_dimension_px(8192);
        assert_eq!(policy.per_image_max_bytes(), 8 * 1024 * 1024);
        assert_eq!(policy.per_pane_max_bytes(), 32 * 1024 * 1024);
        assert_eq!(policy.decode_timeout_ms(), 1000);
        assert_eq!(policy.max_dimension_px(), 8192);
    }

    #[test]
    fn multi_frame_state_accessors_round_trip() {
        let mut s = MultiFrameState::new();
        assert_eq!(s.current_image_id(), None);
        assert_eq!(s.bytes_received(), 0);
        assert_eq!(s.chunks_received(), 0);
        assert!(!s.completed());

        // Feed a chunk through the API.
        let _ = feed_chunk(&mut s, ImageId(7), 100, ChunkContinuation::More);
        assert_eq!(s.current_image_id(), Some(ImageId(7)));
        assert_eq!(s.bytes_received(), 100);
        assert_eq!(s.chunks_received(), 1);
        assert!(!s.completed());

        // Complete the stream.
        let _ = feed_chunk(&mut s, ImageId(7), 50, ChunkContinuation::Final);
        assert!(s.completed());
        assert_eq!(s.bytes_received(), 150);

        // Pin the privacy invariant: external code cannot do
        // `s.completed = false` to bypass the
        // post-completion ProtocolError check. Field privacy
        // structurally enforces this — reset() is the only
        // path to clear completed.
        s.reset();
        assert!(!s.completed());
        assert_eq!(s.bytes_received(), 0);
    }

    // ----------------------------------------------------------------
    // pane_budget_has_room
    // ----------------------------------------------------------------

    #[test]
    fn pane_budget_room_when_below_cap() {
        let policy = ImageCachePolicy::default();
        assert!(pane_budget_has_room(0, 16 * 1024 * 1024, policy));
        assert!(pane_budget_has_room(
            48 * 1024 * 1024,
            16 * 1024 * 1024,
            policy
        ));
    }

    #[test]
    fn pane_budget_no_room_when_over_cap() {
        let policy = ImageCachePolicy::default();
        // 50 + 16 = 66 MiB > 64 MiB cap.
        assert!(!pane_budget_has_room(
            50 * 1024 * 1024,
            16 * 1024 * 1024,
            policy
        ));
    }

    #[test]
    fn pane_budget_room_at_exact_boundary() {
        let policy = ImageCachePolicy::default();
        assert!(pane_budget_has_room(
            48 * 1024 * 1024,
            16 * 1024 * 1024,
            policy
        ));
    }

    // ----------------------------------------------------------------
    // select_eviction_target
    // ----------------------------------------------------------------

    #[test]
    fn evict_picks_oldest_idle() {
        let entries = vec![
            entry(1, 9_000, 1024),
            entry(2, 5_000, 1024),
            entry(3, 7_000, 1024),
        ];
        let target = select_eviction_target(&entries, 10_000).unwrap();
        // Idle: 1=>1000, 2=>5000, 3=>3000. id=2 is oldest.
        assert_eq!(target.id, ImageId(2));
    }

    #[test]
    fn evict_breaks_ties_by_lower_id() {
        let entries = vec![
            entry(5, 1_000, 1024),
            entry(2, 1_000, 1024),
            entry(8, 1_000, 1024),
        ];
        let target = select_eviction_target(&entries, 10_000).unwrap();
        assert_eq!(target.id, ImageId(2));
    }

    #[test]
    fn evict_empty_list_none() {
        let entries: Vec<ImageCacheEntry> = vec![];
        assert!(select_eviction_target(&entries, 10_000).is_none());
    }

    // ----------------------------------------------------------------
    // ImageCacheEntry helpers
    // ----------------------------------------------------------------

    #[test]
    fn entry_idle_ms_saturating() {
        let e = entry(1, 5_000, 1024);
        assert_eq!(e.idle_ms(10_000), 5_000);
        assert_eq!(e.idle_ms(2_000), 0);
    }

    #[test]
    fn entry_touch_updates_access() {
        let mut e = entry(1, 5_000, 1024);
        e.touch(7_000);
        assert_eq!(e.last_access_ts_ms, 7_000);
    }

    // ----------------------------------------------------------------
    // KittyGraphicsTelemetry
    // ----------------------------------------------------------------

    #[test]
    fn telemetry_default_zero() {
        let t = KittyGraphicsTelemetry::default();
        assert_eq!(t.images_transmitted, 0);
        assert_eq!(t.peak_image_cache_bytes, 0);
    }

    #[test]
    fn telemetry_record_transmit() {
        let mut t = KittyGraphicsTelemetry::default();
        t.record_action(KittyAction::Transmit);
        assert_eq!(t.images_transmitted, 1);
        assert_eq!(t.images_displayed, 0);
    }

    #[test]
    fn telemetry_record_transmit_display_increments_both() {
        let mut t = KittyGraphicsTelemetry::default();
        t.record_action(KittyAction::TransmitDisplay);
        assert_eq!(t.images_transmitted, 1);
        assert_eq!(t.images_displayed, 1);
    }

    #[test]
    fn telemetry_record_place() {
        let mut t = KittyGraphicsTelemetry::default();
        t.record_action(KittyAction::Place);
        assert_eq!(t.images_displayed, 1);
        assert_eq!(t.images_transmitted, 0);
    }

    #[test]
    fn telemetry_record_delete_and_query() {
        let mut t = KittyGraphicsTelemetry::default();
        t.record_action(KittyAction::Delete);
        t.record_action(KittyAction::Query);
        assert_eq!(t.images_deleted, 1);
        assert_eq!(t.queries_answered, 1);
    }

    #[test]
    fn telemetry_record_rejection_routes() {
        let mut t = KittyGraphicsTelemetry::default();
        t.record_decision(ImageAcceptDecision::Rejected(
            ImageRejectionReason::Oversized,
        ));
        t.record_decision(ImageAcceptDecision::Rejected(
            ImageRejectionReason::DecodeTimeout,
        ));
        t.record_decision(ImageAcceptDecision::Rejected(
            ImageRejectionReason::Malformed,
        ));
        t.record_decision(ImageAcceptDecision::Rejected(
            ImageRejectionReason::DimensionsOverflow,
        ));
        // Accepted decisions don't increment any counter.
        t.record_decision(ImageAcceptDecision::Accepted);
        assert_eq!(t.rejected_oversized, 1);
        assert_eq!(t.rejected_decode_timeout, 1);
        assert_eq!(t.rejected_malformed, 1);
        assert_eq!(t.rejected_dim_overflow, 1);
    }

    #[test]
    fn telemetry_record_multi_frame_completion() {
        let mut t = KittyGraphicsTelemetry::default();
        t.record_multi_frame(MultiFrameOutcome::Accumulating);
        t.record_multi_frame(MultiFrameOutcome::Complete {
            total_bytes: 100,
            chunks: 2,
        });
        t.record_multi_frame(MultiFrameOutcome::ProtocolError);
        assert_eq!(t.multi_frame_completions, 1);
        assert_eq!(t.multi_frame_protocol_errors, 1);
    }

    #[test]
    fn telemetry_record_cache_size_tracks_peak() {
        let mut t = KittyGraphicsTelemetry::default();
        t.record_cache_size(1_000);
        t.record_cache_size(5_000);
        t.record_cache_size(3_000); // shouldn't lower peak
        assert_eq!(t.peak_image_cache_bytes, 5_000);
    }

    // ----------------------------------------------------------------
    // Cross-cut scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_image_nvim_inline_image() {
        // image.nvim transmits a 1280×720 PNG (~ 700 KiB
        // typical) — accepted under default policy.
        let policy = ImageCachePolicy::default();
        let d = should_accept_image(1280, 720, 700_000, policy);
        assert_eq!(d, ImageAcceptDecision::Accepted);
    }

    #[test]
    fn scenario_yazi_thumbnail_rejected_when_image_bomb() {
        // Malicious actor sends a "thumbnail" claiming 32 768 px.
        // Rejected by dimensions guard.
        let policy = ImageCachePolicy::default();
        let d = should_accept_image(32_768, 32_768, 1024, policy);
        assert_eq!(
            d,
            ImageAcceptDecision::Rejected(ImageRejectionReason::DimensionsOverflow),
        );
    }

    #[test]
    fn scenario_multi_frame_large_screenshot() {
        // 3 MiB screenshot split into 6 chunks. Substrate
        // accumulates and reports completion.
        let mut state = MultiFrameState::new();
        let id = ImageId(99);
        for _ in 0..5 {
            assert_eq!(
                feed_chunk(&mut state, id, 512 * 1024, ChunkContinuation::More),
                MultiFrameOutcome::Accumulating,
            );
        }
        let outcome = feed_chunk(&mut state, id, 512 * 1024, ChunkContinuation::Final);
        assert_eq!(
            outcome,
            MultiFrameOutcome::Complete {
                total_bytes: 6 * 512 * 1024,
                chunks: 6,
            }
        );
    }

    #[test]
    fn scenario_pane_budget_cleanup_pipeline() {
        // Pane has 5 cached images at 12 MiB each = 60 MiB used;
        // a 16 MiB image arrives. Budget exceeded → integration
        // calls select_eviction_target and frees the oldest.
        let policy = ImageCachePolicy::default();
        let used = 60 * 1024 * 1024;
        let incoming = 16 * 1024 * 1024;
        assert!(!pane_budget_has_room(used, incoming, policy));
        let now = 100_000;
        let entries = vec![
            entry(1, now - 1_000, 12 * 1024 * 1024),
            entry(2, now - 50_000, 12 * 1024 * 1024),
            entry(3, now - 5_000, 12 * 1024 * 1024),
            entry(4, now - 10_000, 12 * 1024 * 1024),
            entry(5, now - 2_000, 12 * 1024 * 1024),
        ];
        // Oldest idle is id=2 (50_000 ms idle).
        let target = select_eviction_target(&entries, now).unwrap();
        assert_eq!(target.id, ImageId(2));
    }

    #[test]
    fn scenario_query_response_flow() {
        // Terminal answers `a=q` with OK; integration tracks
        // count for ft doctor.
        let mut t = KittyGraphicsTelemetry::default();
        for _ in 0..3 {
            t.record_action(KittyAction::Query);
        }
        assert_eq!(t.queries_answered, 3);
    }
}
