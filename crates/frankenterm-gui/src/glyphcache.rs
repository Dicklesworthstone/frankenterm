use super::utilsprites::RenderMetrics;
use crate::customglyph::*;
use crate::renderstate::RenderContext;
use crate::termwindow::render::paint::AllowImage;
use ahash::AHashMap;
use anyhow::Context;
use config::{AllowSquareGlyphOverflow, ConfigHandle, TextStyle};
use euclid::num::Zero;
use frankenterm_core::atlas_tier_doctor::{TierSwapDoctorReport, TierSwapDoctorRow};
use frankenterm_core::atlas_tiered_swap::{MemoryBudget, TierSwapStats};
use frankenterm_core::font_features::{AxisVector, GlyphFormat, derive_axis_atlas_key};
use frankenterm_core::subpixel_positioning::SubpixelBin;
use frankenterm_font::units::*;
use frankenterm_font::{FontConfiguration, GlyphInfo, LoadedFont, LoadedFontId};
use lfucache::LfuCache;
use ordered_float::NotNan;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, LazyLock, Weak};
use std::time::{Duration, Instant};
use termwiz::color::RgbColor;
use termwiz::image::{
    ImageData, ImageDataType, ImageDataValidationError, ImageDataValidationLimits,
    ImageDataValidationSummary, MAX_IMAGE_WIRE_BYTES, MAX_IMAGE_WIRE_FRAMES,
    MAX_TRUSTED_LOCAL_IMAGE_DECODED_BYTES,
};
use termwiz::surface::CursorShape;
use wezterm_bidi::Direction;
use wezterm_term::Underline;
use window::bitmaps::atlas::{Atlas, OutOfTextureSpace, Sprite};
use window::bitmaps::{BitmapImage, Image, ImageTexture, Texture2d};
use window::color::SrgbaPixel;
use window::{Point, Rect};

static FRAME_ERROR_REPORTED: AtomicBool = AtomicBool::new(false);

/// We only want to report a frame error once at error level, because
/// if it is triggering it is likely in a animated image and will continue
/// to trigger multiple times per second as the frames are cycled.
fn report_frame_error<S: Into<String>>(message: S) {
    if FRAME_ERROR_REPORTED.load(Ordering::Relaxed) {
        log::debug!("{}", message.into());
    } else {
        log::error!("{}", message.into());
        FRAME_ERROR_REPORTED.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    Loading,
    Loaded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CellMetricKey {
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl From<&RenderMetrics> for CellMetricKey {
    fn from(metrics: &RenderMetrics) -> CellMetricKey {
        CellMetricKey {
            pixel_width: metrics.cell_size.width as u16,
            pixel_height: metrics.cell_size.height as u16,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SizedBlockKey {
    pub block: BlockKey,
    pub size: CellMetricKey,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GlyphKey {
    pub font_idx: usize,
    pub glyph_pos: u32,
    pub font_feature_atlas_key: u64,
    pub subpixel_bin: SubpixelBin,
    pub num_cells: u8,
    pub style: TextStyle,
    pub followed_by_space: bool,
    pub metric: CellMetricKey,
    pub id: LoadedFontId,
}

/// We'd like to avoid allocating when resolving from the cache
/// so this is the borrowed version of GlyphKey.
/// It's a bit involved to make this work; more details can be
/// found in the excellent guide here:
/// <https://github.com/sunshowers-code/borrow-complex-key-example/blob/main/src/lib.rs>
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct BorrowedGlyphKey<'a> {
    pub font_idx: usize,
    pub glyph_pos: u32,
    pub font_feature_atlas_key: u64,
    pub subpixel_bin: SubpixelBin,
    pub num_cells: u8,
    pub style: &'a TextStyle,
    pub followed_by_space: bool,
    pub metric: CellMetricKey,
    pub id: LoadedFontId,
}

impl<'a> BorrowedGlyphKey<'a> {
    fn to_owned(&self) -> GlyphKey {
        GlyphKey {
            font_idx: self.font_idx,
            glyph_pos: self.glyph_pos,
            font_feature_atlas_key: self.font_feature_atlas_key,
            subpixel_bin: self.subpixel_bin,
            num_cells: self.num_cells,
            style: self.style.clone(),
            followed_by_space: self.followed_by_space,
            metric: self.metric,
            id: self.id,
        }
    }
}

trait GlyphKeyTrait {
    fn key<'k>(&'k self) -> BorrowedGlyphKey<'k>;
}

impl GlyphKeyTrait for GlyphKey {
    fn key<'k>(&'k self) -> BorrowedGlyphKey<'k> {
        BorrowedGlyphKey {
            font_idx: self.font_idx,
            glyph_pos: self.glyph_pos,
            font_feature_atlas_key: self.font_feature_atlas_key,
            subpixel_bin: self.subpixel_bin,
            num_cells: self.num_cells,
            style: &self.style,
            followed_by_space: self.followed_by_space,
            metric: self.metric,
            id: self.id,
        }
    }
}

impl<'a> GlyphKeyTrait for BorrowedGlyphKey<'a> {
    fn key<'k>(&'k self) -> BorrowedGlyphKey<'k> {
        *self
    }
}

impl<'a> std::borrow::Borrow<dyn GlyphKeyTrait + 'a> for GlyphKey {
    fn borrow(&self) -> &(dyn GlyphKeyTrait + 'a) {
        self
    }
}

impl<'a> PartialEq for dyn GlyphKeyTrait + 'a {
    fn eq(&self, other: &Self) -> bool {
        self.key().eq(&other.key())
    }
}

impl<'a> Eq for dyn GlyphKeyTrait + 'a {}

impl<'a> std::hash::Hash for dyn GlyphKeyTrait + 'a {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state)
    }
}

#[inline]
#[must_use]
pub fn glyph_feature_atlas_key(
    font_id: LoadedFontId,
    glyph_id: u32,
    format: GlyphFormat,
    axis_vector: &AxisVector,
) -> u64 {
    derive_axis_atlas_key(font_id as u64, glyph_id, format, axis_vector)
}

#[inline]
#[must_use]
fn monochrome_glyph_feature_atlas_key(font_id: LoadedFontId, glyph_id: u32) -> u64 {
    glyph_feature_atlas_key(
        font_id,
        glyph_id,
        GlyphFormat::Monochrome,
        &AxisVector::new(),
    )
}

/// Caches a rendered glyph.
/// The image data may be None for whitespace glyphs.
pub struct CachedGlyph {
    pub has_color: bool,
    pub brightness_adjust: f32,
    pub x_offset: PixelLength,
    pub y_offset: PixelLength,
    pub x_advance: PixelLength,
    pub bearing_x: PixelLength,
    pub bearing_y: PixelLength,
    pub texture: Option<Sprite>,
    pub scale: f64,
}

impl std::fmt::Debug for CachedGlyph {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::result::Result<(), std::fmt::Error> {
        fmt.debug_struct("CachedGlyph")
            .field("has_color", &self.has_color)
            .field("x_advance", &self.x_advance)
            .field("x_offset", &self.x_offset)
            .field("y_offset", &self.y_offset)
            .field("bearing_x", &self.bearing_x)
            .field("bearing_y", &self.bearing_y)
            .field("scale", &self.scale)
            .field("texture", &self.texture)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GlyphWarmupPriority {
    AsciiPrintable,
    CommonLigature,
    NerdFontIcon,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GlyphWarmupRequest {
    pub text: String,
    pub priority: GlyphWarmupPriority,
}

impl GlyphWarmupRequest {
    fn new(text: impl Into<String>, priority: GlyphWarmupPriority) -> Self {
        Self {
            text: text.into(),
            priority,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlyphWarmupStats {
    pub attempted_requests: usize,
    pub warmed_glyphs: usize,
    pub cache_hits: usize,
    pub failed_glyphs: usize,
    pub budget_exhausted: bool,
    pub elapsed: Duration,
}

impl GlyphWarmupStats {
    fn new() -> Self {
        Self {
            attempted_requests: 0,
            warmed_glyphs: 0,
            cache_hits: 0,
            failed_glyphs: 0,
            budget_exhausted: false,
            elapsed: Duration::ZERO,
        }
    }
}

const COMMON_LIGATURE_WARMUP_TEXT: &[&str] = &["fi", "fl", "ff", "ffi", "ffl"];
const COMMON_NERD_FONT_ICON_WARMUP_CODEPOINTS: &[char] = &[
    '\u{e0a0}', '\u{e0a1}', '\u{e0a2}', '\u{e0b0}', '\u{e0b1}', '\u{e0b2}', '\u{e0b3}', '\u{f0a0}',
    '\u{f0c8}', '\u{f101}', '\u{f105}', '\u{f120}', '\u{f126}', '\u{f1c0}', '\u{f233}',
];

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
struct LineKey {
    strike_through: bool,
    underline: Underline,
    overline: bool,
    size: CellMetricKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct BlankFrameKey {
    width: usize,
    height: usize,
    padding: Option<usize>,
    scale_down: Option<usize>,
}

/// Atlas allocation is a function of both the decoded pixels and their
/// requested geometry. Pixel-only hashes are intentionally reusable across
/// independent image objects, but must never alias sprites with different
/// dimensions, padding, or scale-down policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FrameSpriteKey {
    hash: [u8; 32],
    width: usize,
    height: usize,
    padding: Option<usize>,
    scale_down: Option<usize>,
}

/// The single read-only `BitmapImage` adapter used by decoded image uploads.
/// `Payload` keeps mutable terminal image data locked for the complete atlas
/// read, while `Frame` borrows immutable worker-owned pixels. Centralizing both
/// sources here avoids adding another raw-pointer trait implementation for the
/// zero-copy worker path.
enum DecodedImageHandle<'a> {
    Payload {
        current_frame: Cell<usize>,
        data: termwiz::image::ImageDataReadGuard<'a>,
    },
    Frame(&'a DecodedFrame),
}

impl<'a> DecodedImageHandle<'a> {
    fn payload(data: termwiz::image::ImageDataReadGuard<'a>, current_frame: usize) -> Self {
        Self::Payload {
            current_frame: Cell::new(current_frame),
            data,
        }
    }

    fn frame(frame: &'a DecodedFrame) -> Self {
        Self::Frame(frame)
    }

    fn set_current_frame(&self, frame_index: usize) {
        let Self::Payload { current_frame, .. } = self else {
            unreachable!("worker frame adapters do not have a mutable frame index")
        };
        current_frame.set(frame_index);
    }

    fn payload_data(&self) -> &ImageDataType {
        let Self::Payload { data, .. } = self else {
            unreachable!("worker frame adapters do not contain terminal image payloads")
        };
        data
    }
}

// `DecodedImageHandle` is the read-only adapter that glyphcache hands to
// the atlas allocator from `cached_image_impl`.
// `atlas.allocate_with_padding(&handle, ...)` borrows it immutably, so only
// `pixel_data` and `image_dimensions` are reachable. The vendored
// `BitmapImage` trait still requires `pixel_data_mut`; that method exists to
// satisfy the trait but must remain unreachable for this adapter.
//
// The `Payload` variant can contain an encoded source while its decoded frames
// are arriving from the worker. `cached_image_impl` handles that match arm
// separately and never passes this adapter to the atlas in that state, so the
// `EncodedLease` / `EncodedFile` BitmapImage arms remain unreachable.
impl<'a> BitmapImage for DecodedImageHandle<'a> {
    unsafe fn pixel_data(&self) -> *const u8 {
        match self {
            Self::Payload {
                current_frame,
                data,
            } => match &**data {
                ImageDataType::Rgba8 { data, .. } => data.as_ptr(),
                ImageDataType::AnimRgba8 { frames, .. } => frames[current_frame.get()].as_ptr(),
                ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => unreachable!(
                    "ft-82pp1: DecodedImageHandle::pixel_data called with encoded variant; \
                     the cache should only hold decoded images"
                ),
            },
            Self::Frame(frame) => frame.pixels.as_ptr(),
        }
    }

    /// Never reachable — `DecodedImageHandle` is the read-only
    /// atlas-allocator adapter. The cache passes the handle by
    /// `&handle` (immutable), so the BitmapImage trait's mutable
    /// helpers (`pixel_data_mut`, `pixels_mut`, `pixel_mut`,
    /// `pixel_data_slice_mut`) are not called in production.
    unsafe fn pixel_data_mut(&mut self) -> *mut u8 {
        unreachable!(
            "ft-82pp1: DecodedImageHandle is read-only; pixel_data_mut must not be called. \
             If this fires, the caller is incorrectly routing a mutating BitmapImage helper \
             (pixels_mut / pixel_mut / pixel_data_slice_mut) through the read-only cache adapter."
        );
    }

    /// br-ft-82pp1: returns `false` — `DecodedImageHandle` is a
    /// read-only view. Callers branching on `is_mutable()` will
    /// see this and avoid the `pixel_data_mut`/`pixels_mut`/
    /// `pixel_mut`/`pixel_data_slice_mut` paths that would
    /// trigger the `unreachable!` above.
    fn is_mutable(&self) -> bool {
        false
    }

    fn image_dimensions(&self) -> (usize, usize) {
        match self {
            Self::Payload { data, .. } => match &**data {
                ImageDataType::Rgba8 { width, height, .. }
                | ImageDataType::AnimRgba8 { width, height, .. } => {
                    (*width as usize, *height as usize)
                }
                ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => unreachable!(
                    "ft-82pp1: DecodedImageHandle::image_dimensions called with encoded variant; \
                     the cache should only hold decoded images"
                ),
            },
            Self::Frame(frame) => (frame.width, frame.height),
        }
    }
}

#[derive(Clone)]
struct DecodedFrame {
    /// Immutable worker-produced pixels retained in memory under the decoded
    /// image cache's exact byte ledger. Keeping them here avoids synchronous
    /// temp-file reads and full-frame copies on the GUI render path.
    pixels: Arc<Vec<u8>>,
    hash: [u8; 32],
    duration: Duration,
    width: usize,
    height: usize,
}

fn checked_decoded_frame_bytes(width: usize, height: usize) -> Option<usize> {
    width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
}

impl DecodedFrame {
    fn checked_decoded_bytes(&self) -> Option<usize> {
        checked_decoded_frame_bytes(self.width, self.height)
    }
}

struct FrameDecoder {}

const MAX_FRAME_DECODER_WORKERS: usize = 2;
const MAX_PENDING_FRAME_DECODERS: usize = 8;
const MAX_FRAME_DECODER_DECODED_BYTES: usize = MAX_IMAGE_WIRE_BYTES;
const MAX_FRAME_DECODER_AXIS: u32 = 16_384;
const MAX_QUEUED_FRAME_DECODER_BYTES: usize = MAX_IMAGE_WIRE_BYTES * 2;

fn decoded_image_validation_limits() -> ImageDataValidationLimits {
    ImageDataValidationLimits {
        max_decoded_bytes: MAX_FRAME_DECODER_DECODED_BYTES,
        max_frame_count: MAX_IMAGE_WIRE_FRAMES,
        max_width: MAX_FRAME_DECODER_AXIS,
        max_height: MAX_FRAME_DECODER_AXIS,
    }
}

/// Limits for reusing validation authority that was already published by a
/// bounded local producer (background decode, gradient generation, or a remote
/// hydration worker). The larger byte ceiling does not weaken the 64 MiB wire
/// and fallback validator boundary: unattested decoded payloads still enter
/// `decoded_image_validation_limits`, while this path can only reuse private,
/// non-serialized, revision-bound authority.
fn trusted_decoded_image_authority_limits() -> ImageDataValidationLimits {
    ImageDataValidationLimits {
        max_decoded_bytes: MAX_TRUSTED_LOCAL_IMAGE_DECODED_BYTES,
        max_frame_count: MAX_IMAGE_WIRE_FRAMES,
        max_width: MAX_FRAME_DECODER_AXIS,
        max_height: MAX_FRAME_DECODER_AXIS,
    }
}

static FRAME_DECODER_JOBS: AtomicUsize = AtomicUsize::new(0);
static FRAME_DECODER_QUEUED_BYTES: AtomicUsize = AtomicUsize::new(0);
static FRAME_DECODER_POOL: LazyLock<Result<rayon::ThreadPool, String>> = LazyLock::new(|| {
    rayon::ThreadPoolBuilder::new()
        .num_threads(MAX_FRAME_DECODER_WORKERS)
        .thread_name(|index| format!("ft-frame-decoder-{index}"))
        .build()
        .map_err(|error| error.to_string())
});

/// Reserve `amount` from a bounded atomic counter without wrapping at either
/// the arithmetic or policy ceiling.
///
/// Keep this as an explicit compare/exchange loop rather than relying on
/// unstable or toolchain-specific atomic update conveniences: FrankenTerm's
/// supported Rust toolchain must compile the queue-pressure guards, and a
/// contended retry must always re-evaluate overflow and the bound against the
/// value that actually won the race.
fn try_reserve_bounded_atomic(counter: &AtomicUsize, amount: usize, limit: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount).filter(|next| *next <= limit) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

/// Release an exact reservation without permitting an underflow to wrap into
/// counterfeit capacity. `false` leaves the counter unchanged and lets the
/// owning RAII guard flag the invariant violation in debug builds.
fn try_release_atomic(counter: &AtomicUsize, amount: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_sub(amount) else {
            return false;
        };
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

struct FrameDecoderJobPermit<'a> {
    jobs: &'a AtomicUsize,
}

impl FrameDecoderJobPermit<'static> {
    fn try_acquire() -> Option<Self> {
        Self::try_acquire_from(&FRAME_DECODER_JOBS, MAX_PENDING_FRAME_DECODERS)
    }
}

impl<'a> FrameDecoderJobPermit<'a> {
    fn try_acquire_from(jobs: &'a AtomicUsize, limit: usize) -> Option<Self> {
        try_reserve_bounded_atomic(jobs, 1, limit).then_some(Self { jobs })
    }
}

impl Drop for FrameDecoderJobPermit<'_> {
    fn drop(&mut self) {
        let released = try_release_atomic(self.jobs, 1);
        debug_assert!(released);
    }
}

struct FrameDecoderReceiver {
    receiver: Receiver<QueuedDecodedFrame>,
    cancelled: Arc<AtomicBool>,
}

impl FrameDecoderReceiver {
    fn try_recv(&self) -> Result<DecodedFrame, TryRecvError> {
        self.receiver.try_recv().map(QueuedDecodedFrame::into_frame)
    }
}

impl Drop for FrameDecoderReceiver {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[derive(Debug)]
struct DecodedImageValidationReceiver {
    receiver: Receiver<Result<ImageDataValidationSummary, ImageDataValidationError>>,
    cancelled: Arc<AtomicBool>,
}

impl DecodedImageValidationReceiver {
    fn try_recv(
        &self,
    ) -> Result<Result<ImageDataValidationSummary, ImageDataValidationError>, TryRecvError> {
        self.receiver.try_recv()
    }
}

impl Drop for DecodedImageValidationReceiver {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

struct DecodedImageValidator;

impl DecodedImageValidator {
    fn start(
        image_data: Arc<ImageData>,
        expected_revision: [u8; 32],
    ) -> anyhow::Result<DecodedImageValidationReceiver> {
        Self::start_with_hook(image_data, expected_revision, || {})
    }

    fn start_with_hook<BeforeValidation>(
        image_data: Arc<ImageData>,
        expected_revision: [u8; 32],
        before_validation: BeforeValidation,
    ) -> anyhow::Result<DecodedImageValidationReceiver>
    where
        BeforeValidation: FnOnce() + Send + 'static,
    {
        let (tx, rx) = channel();
        let permit = FrameDecoderJobPermit::try_acquire()
            .context("bounded decoded-image validation queue is full")?;
        let pool = FRAME_DECODER_POOL.as_ref().map_err(|error| {
            anyhow::anyhow!("decoded-image validation pool is unavailable: {error}")
        })?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        pool.spawn(move || {
            let _permit = permit;
            before_validation();
            let result = image_data
                .normalize_for_content_revision_with_limits(
                    expected_revision,
                    MAX_IMAGE_WIRE_BYTES,
                    decoded_image_validation_limits(),
                    &|| worker_cancelled.load(Ordering::Acquire),
                )
                .and_then(|normalized| {
                    if normalized.replacement.is_some() {
                        // This lane is selected only while an exact decoded
                        // revision guard is held. A replacement means that the
                        // object changed variants before the worker acquired
                        // it; retry under a newly bound revision.
                        Err(ImageDataValidationError::ContentRevisionMismatch)
                    } else {
                        Ok(normalized.summary)
                    }
                });
            if !worker_cancelled.load(Ordering::Acquire) {
                let _ = tx.send(result);
            }
        });
        Ok(DecodedImageValidationReceiver {
            receiver: rx,
            cancelled,
        })
    }
}

struct QueuedFrameBudget {
    bytes: usize,
}

impl QueuedFrameBudget {
    fn try_acquire(bytes: usize) -> Option<Self> {
        try_reserve_bounded_atomic(
            &FRAME_DECODER_QUEUED_BYTES,
            bytes,
            MAX_QUEUED_FRAME_DECODER_BYTES,
        )
        .then_some(Self { bytes })
    }

    fn split(&mut self, bytes: usize) -> anyhow::Result<Self> {
        let remaining = self.bytes;
        let new_remaining = remaining.checked_sub(bytes).ok_or_else(|| {
            anyhow::anyhow!(
                "decoded-frame bytes exceed the decoder's reserved queue budget: \
                 requested {bytes}, remaining {remaining}"
            )
        })?;
        self.bytes = new_remaining;
        Ok(Self { bytes })
    }
}

impl Drop for QueuedFrameBudget {
    fn drop(&mut self) {
        let bytes = self.bytes;
        let released = try_release_atomic(&FRAME_DECODER_QUEUED_BYTES, bytes);
        debug_assert!(released);
    }
}

struct QueuedDecodedFrame {
    frame: DecodedFrame,
    budget: QueuedFrameBudget,
}

impl QueuedDecodedFrame {
    fn into_frame(self) -> DecodedFrame {
        let Self { frame, budget } = self;
        drop(budget);
        frame
    }
}

impl FrameDecoder {
    pub fn start(
        image_data: Arc<ImageData>,
        expected_revision: [u8; 32],
    ) -> anyhow::Result<FrameDecoderReceiver> {
        let (tx, rx) = channel();
        let permit = FrameDecoderJobPermit::try_acquire()
            .context("bounded image frame-decoder queue is full")?;
        let pool = FRAME_DECODER_POOL
            .as_ref()
            .map_err(|error| anyhow::anyhow!("image frame-decoder pool is unavailable: {error}"))?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        pool.spawn(move || {
            let _permit = permit;
            if let Err(err) =
                Self::run_decoder(image_data, expected_revision, tx, &worker_cancelled)
            {
                if !worker_cancelled.load(Ordering::Acquire)
                    && err
                        .downcast_ref::<std::sync::mpsc::SendError<QueuedDecodedFrame>>()
                        .is_none()
                {
                    log::error!("Error decoding image: {err:#}");
                }
            }
        });

        Ok(FrameDecoderReceiver {
            receiver: rx,
            cancelled,
        })
    }

    fn run_decoder(
        image_data: Arc<ImageData>,
        expected_revision: [u8; 32],
        tx: Sender<QueuedDecodedFrame>,
        cancelled: &AtomicBool,
    ) -> anyhow::Result<()> {
        let start = Instant::now();
        let normalized = image_data
            .normalize_for_content_revision_with_limits(
                expected_revision,
                MAX_IMAGE_WIRE_BYTES,
                ImageDataValidationLimits {
                    max_decoded_bytes: MAX_FRAME_DECODER_DECODED_BYTES,
                    max_frame_count: MAX_IMAGE_WIRE_FRAMES,
                    max_width: MAX_FRAME_DECODER_AXIS,
                    max_height: MAX_FRAME_DECODER_AXIS,
                },
                &|| cancelled.load(Ordering::Acquire),
            )
            .context("bounded frame decode")?;
        let mut queued_budget = QueuedFrameBudget::try_acquire(normalized.summary.decoded_bytes)
            .context("global decoded-frame queue byte budget is exhausted")?;
        let decoded = normalized
            .replacement
            .context("encoded frame decode did not produce decoded data")?
            .into_data();
        let mut frame_count = 0usize;
        let mut decoded_bytes = 0usize;
        let mut send_frame =
            |data: Vec<u8>, hash: [u8; 32], duration: Duration, width: u32, height: u32| {
                if cancelled.load(Ordering::Acquire) {
                    anyhow::bail!("image frame decode was cancelled");
                }
                let expected_bytes = checked_decoded_frame_bytes(width as usize, height as usize)
                    .context("decoded-frame dimensions overflow")?;
                if data.len() != expected_bytes {
                    anyhow::bail!(
                        "decoded-frame byte length mismatch: expected {expected_bytes}, got {}",
                        data.len()
                    );
                }
                decoded_bytes = decoded_bytes
                    .checked_add(data.len())
                    .context("decoded-frame byte total overflow")?;
                frame_count = frame_count
                    .checked_add(1)
                    .context("decoded-frame count overflow")?;
                let bytes = data.len();
                let frame = DecodedFrame {
                    pixels: Arc::new(data),
                    hash,
                    duration,
                    width: width as usize,
                    height: height as usize,
                };
                let budget = queued_budget.split(bytes)?;
                tx.send(QueuedDecodedFrame { frame, budget })
                    .context("sending decoded frame")?;
                Ok::<(), anyhow::Error>(())
            };

        match decoded {
            ImageDataType::Rgba8 {
                width,
                height,
                data,
                hash,
            } => send_frame(data, hash, Duration::from_secs(86_400), width, height)?,
            ImageDataType::AnimRgba8 {
                width,
                height,
                durations,
                frames,
                hashes,
            } => {
                if frames.is_empty() {
                    anyhow::bail!("bounded frame decode produced no animation frames");
                }
                if frames.len() != durations.len() {
                    anyhow::bail!(
                        "bounded frame decode produced {} frames but {} durations",
                        frames.len(),
                        durations.len()
                    );
                }
                if frames.len() != hashes.len() {
                    anyhow::bail!(
                        "bounded frame decode produced {} frames but {} hashes",
                        frames.len(),
                        hashes.len()
                    );
                }
                for ((data, duration), hash) in frames.into_iter().zip(durations).zip(hashes) {
                    send_frame(data, hash, duration, width, height)?;
                }
            }
            ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                anyhow::bail!("bounded frame decode retained encoded data")
            }
        }

        let elapsed = start.elapsed();
        let fps = frame_count as f32 / elapsed.as_secs_f32();

        log::debug!(
            "decoded {} frames, {} bytes in {elapsed:?}, {fps} fps",
            frame_count,
            decoded_bytes,
        );
        Ok(())
    }
}

enum FrameSource {
    Decoder(FrameDecoderReceiver),
    FrameIndex(usize),
}

struct FrameState {
    source: FrameSource,
    current_frame: DecodedFrame,
    frames: Vec<DecodedFrame>,
    /// Exact decoded-frame bytes retained by `frames`, or the transparent
    /// placeholder while no decoded frame has arrived. `usize::MAX` is the
    /// fail-closed overflow sentinel.
    retained_frame_bytes: usize,
    load_state: LoadState,
}

impl FrameState {
    fn new(rx: FrameDecoderReceiver) -> Self {
        const TRANSPARENT_SIZE: usize = 1;
        static TRANSPARENT: LazyLock<Arc<Vec<u8>>> =
            LazyLock::new(|| Arc::new(vec![0, 0, 0, 0x00]));

        let current_frame = DecodedFrame {
            pixels: Arc::clone(&*TRANSPARENT),
            hash: ImageDataType::hash_bytes(TRANSPARENT.as_slice()),
            width: TRANSPARENT_SIZE,
            height: TRANSPARENT_SIZE,
            duration: Duration::from_millis(0),
        };
        let retained_frame_bytes = current_frame.checked_decoded_bytes().unwrap_or(usize::MAX);

        Self {
            source: FrameSource::Decoder(rx),
            frames: vec![],
            current_frame,
            retained_frame_bytes,
            load_state: LoadState::Loading,
        }
    }

    fn load_next_frame(&mut self) -> bool {
        match &mut self.source {
            FrameSource::Decoder(rx) => match rx.try_recv() {
                Ok(frame) => {
                    let is_zero_duration_root = self.frames.is_empty() && frame.duration.is_zero();
                    let frame_bytes = frame.checked_decoded_bytes().unwrap_or(usize::MAX);
                    self.retained_frame_bytes = if self.frames.is_empty() {
                        // The first real frame replaces the transparent
                        // placeholder; it must not be counted in addition to it.
                        frame_bytes
                    } else {
                        self.retained_frame_bytes
                            .checked_add(frame_bytes)
                            .unwrap_or(usize::MAX)
                    };
                    self.frames.push(frame.clone());
                    self.current_frame = frame;
                    // A zero-duration first frame is the Kitty animation root,
                    // not a visible frame. Keep presenting the transparent
                    // placeholder until the first timed frame arrives.
                    self.load_state = if is_zero_duration_root {
                        LoadState::Loading
                    } else {
                        LoadState::Loaded
                    };
                    true
                }
                Err(TryRecvError::Empty) => false,
                Err(TryRecvError::Disconnected) => {
                    if self.frames.is_empty() {
                        self.source = FrameSource::FrameIndex(0);
                        log::warn!("image decoder thread terminated");
                        self.load_state = LoadState::Failed;
                        self.current_frame.duration = Duration::from_secs(86400);
                        self.frames.push(self.current_frame.clone());
                        false
                    } else if self.frames.len() == 1 {
                        // If there's only a single frame, we may as well ensure
                        // that it has a long duration so that we don't waste
                        // resources ticking to the same frame over and over
                        self.frames[0].duration = Duration::from_secs(86400);
                        self.current_frame = self.frames[0].clone();
                        self.source = FrameSource::FrameIndex(0);
                        self.load_state = LoadState::Loaded;
                        true
                    } else {
                        // The decoder path presents each received frame, so at
                        // disconnect `current_frame` is the last frame. Advance
                        // from that exact index rather than resetting the cursor
                        // to zero and then skipping frame zero on the next tick.
                        let mut next_index = self.frames.len() - 1;
                        Self::advance_frame_index(&self.frames, &mut next_index);
                        self.current_frame = self.frames[next_index].clone();
                        self.source = FrameSource::FrameIndex(next_index);
                        true
                    }
                }
            },
            FrameSource::FrameIndex(idx) => {
                Self::advance_frame_index(&self.frames, idx);
                self.current_frame = self.frames[*idx].clone();
                true
            }
        }
    }

    fn advance_frame_index(frames: &[DecodedFrame], idx: &mut usize) {
        debug_assert!(!frames.is_empty());
        *idx = (*idx).checked_add(1).unwrap_or(0);
        if *idx >= frames.len() {
            *idx = usize::from(frames.len() > 1 && frames[0].duration.is_zero());
        }
    }

    fn frame_duration(&self) -> Duration {
        self.current_frame.duration
    }

    fn frame_hash(&self) -> [u8; 32] {
        self.current_frame.hash
    }

    fn awaiting_first_visible_frame(&self) -> bool {
        matches!(&self.source, FrameSource::Decoder(_))
            && self.frames.len() == 1
            && self.frames[0].duration.is_zero()
    }

    fn next_frame_due(&self, due: Instant) -> Option<Instant> {
        match (&self.source, self.load_state) {
            (_, LoadState::Failed) => None,
            (FrameSource::Decoder(_), LoadState::Loading | LoadState::Loaded) => Some(due),
            (FrameSource::FrameIndex(_), LoadState::Loaded) if self.frames.len() > 1 => Some(due),
            (FrameSource::FrameIndex(_), LoadState::Loading | LoadState::Loaded) => None,
        }
    }

    fn retained_bytes(&self) -> usize {
        self.retained_frame_bytes
    }
}

/// Finalize the next decoder poll immediately before returning to the window
/// scheduler. Cold atlas allocation can outlast a short cadence, so a deadline
/// based on a timestamp captured before sprite construction can already be
/// expired when it is published and drive an immediate-repaint loop.
///
/// Only an open decoder queue that returned `Empty` takes this path. Loaded
/// animation frames keep their original deadline and catch-up semantics.
fn finalize_decoder_retry_after_empty_poll(
    frame_start: &mut Instant,
    current_due: Instant,
    frame_duration: Duration,
    min_frame_duration: Duration,
    decoder_queue_was_empty: bool,
) -> Option<Instant> {
    if !decoder_queue_was_empty {
        return Some(current_due);
    }

    let retry_start = Instant::now();
    let retry_interval = frame_duration
        .max(min_frame_duration)
        .max(Duration::from_millis(1));
    let retry_due = retry_start.checked_add(retry_interval)?;
    *frame_start = retry_start;
    Some(retry_due)
}

impl std::fmt::Debug for FrameState {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("FrameState").finish()
    }
}

#[derive(Debug)]
pub struct DecodedImage {
    frame_start: RefCell<Instant>,
    current_frame: RefCell<usize>,
    image: Arc<ImageData>,
    /// Revision that this decoded entry and its object-scoped cache key own.
    expected_revision: [u8; 32],
    /// Source-image bytes for this content revision, computed once at cache
    /// construction so animation hits do not rescan every source frame.
    source_retained_bytes: Cell<usize>,
    /// Unattested local decoded payloads are validated on the bounded image
    /// worker pool. Until this receiver completes, the renderer presents a
    /// nonblocking placeholder and never exposes the pixels to the atlas.
    decoded_validation: RefCell<Option<DecodedImageValidationReceiver>>,
    frames: RefCell<Option<FrameState>>,
    load_state: Cell<LoadState>,
}

#[derive(Debug, thiserror::Error)]
#[error("decoded image content revision changed during cache admission")]
struct DecodedImageRevisionMismatch;

#[derive(Debug, thiserror::Error)]
#[error("decoded image validation rejected the bound revision")]
struct DecodedImageValidationRejected {
    #[source]
    source: ImageDataValidationError,
}

#[derive(Debug, thiserror::Error)]
#[error("decoded image validation worker became unavailable")]
struct DecodedImageValidationUnavailable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ImageObjectKey(*const ImageData);

impl ImageObjectKey {
    fn of(image: &Arc<ImageData>) -> Self {
        Self(Arc::as_ptr(image))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ImageCacheKey {
    object: ImageObjectKey,
    revision: [u8; 32],
}

impl ImageCacheKey {
    #[cfg(test)]
    fn new(image: &Arc<ImageData>, revision: [u8; 32]) -> Self {
        Self {
            object: ImageObjectKey::of(image),
            revision,
        }
    }
}

#[derive(Debug)]
struct ImageRevisionOwner {
    image: Weak<ImageData>,
    revision: [u8; 32],
    validation_rejected: bool,
}

const IMAGE_REVISION_OWNER_PRUNE_INTERVAL: usize = 64;
const MAX_REJECTED_IMAGE_REVISIONS: usize = 256;

impl DecodedImage {
    fn start_frame_decoder(
        image_data: &Arc<ImageData>,
        expected_revision: [u8; 32],
        source_retained_bytes: usize,
    ) -> anyhow::Result<Self> {
        let rx = FrameDecoder::start(Arc::clone(image_data), expected_revision)?;
        Ok(Self {
            frame_start: RefCell::new(Instant::now()),
            current_frame: RefCell::new(0),
            image: Arc::clone(image_data),
            expected_revision,
            source_retained_bytes: Cell::new(source_retained_bytes),
            decoded_validation: RefCell::new(None),
            frames: RefCell::new(Some(FrameState::new(rx))),
            load_state: Cell::new(LoadState::Loading),
        })
    }

    fn start_decoded_validator(
        image_data: &Arc<ImageData>,
        expected_revision: [u8; 32],
    ) -> anyhow::Result<Self> {
        let validation = DecodedImageValidator::start(Arc::clone(image_data), expected_revision)?;
        Ok(Self {
            frame_start: RefCell::new(Instant::now()),
            current_frame: RefCell::new(0),
            image: Arc::clone(image_data),
            expected_revision,
            // Reserve the complete accepted decoded-image ceiling while the
            // worker hashes the payload. The exact validated total replaces
            // this reservation before any pixels are uploaded.
            source_retained_bytes: Cell::new(MAX_FRAME_DECODER_DECODED_BYTES),
            decoded_validation: RefCell::new(Some(validation)),
            frames: RefCell::new(None),
            load_state: Cell::new(LoadState::Loading),
        })
    }

    fn loaded_decoded(
        image_data: &Arc<ImageData>,
        expected_revision: [u8; 32],
        source_retained_bytes: usize,
    ) -> anyhow::Result<Self> {
        let data = image_data
            .data_for_content_revision(expected_revision)
            .ok_or(DecodedImageRevisionMismatch)?;
        let current_frame = match &*data {
            ImageDataType::AnimRgba8 { durations, .. }
                if durations.len() > 1 && durations[0].is_zero() =>
            {
                1
            }
            ImageDataType::AnimRgba8 { .. } | ImageDataType::Rgba8 { .. } => 0,
            ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                return Err(DecodedImageRevisionMismatch.into());
            }
        };
        drop(data);
        Ok(Self {
            frame_start: RefCell::new(Instant::now()),
            current_frame: RefCell::new(current_frame),
            image: Arc::clone(image_data),
            expected_revision,
            source_retained_bytes: Cell::new(source_retained_bytes),
            decoded_validation: RefCell::new(None),
            frames: RefCell::new(None),
            load_state: Cell::new(LoadState::Loaded),
        })
    }

    #[cfg(test)]
    fn load(image_data: &Arc<ImageData>) -> anyhow::Result<Self> {
        Self::load_for_revision(image_data, image_data.current_content_hash())
    }

    fn load_for_revision(
        image_data: &Arc<ImageData>,
        expected_revision: [u8; 32],
    ) -> anyhow::Result<Self> {
        if let Some(summary) = image_data.validated_summary_for_content_revision(
            expected_revision,
            trusted_decoded_image_authority_limits(),
        ) {
            return Self::loaded_decoded(image_data, expected_revision, summary.decoded_bytes);
        }

        let data = image_data
            .data_for_content_revision(expected_revision)
            .ok_or(DecodedImageRevisionMismatch)?;
        let encoded_source_retained_bytes = match &*data {
            ImageDataType::EncodedFile(encoded) => Some(encoded.len()),
            ImageDataType::EncodedLease(_) => Some(0),
            ImageDataType::Rgba8 { .. } | ImageDataType::AnimRgba8 { .. } => None,
        };
        drop(data);
        match encoded_source_retained_bytes {
            Some(source_retained_bytes) => {
                Self::start_frame_decoder(image_data, expected_revision, source_retained_bytes)
            }
            None => Self::start_decoded_validator(image_data, expected_revision),
        }
    }

    fn retained_bytes(&self) -> usize {
        let frame_bytes = self
            .frames
            .borrow()
            .as_ref()
            .map(FrameState::retained_bytes)
            .unwrap_or(0);
        self.source_retained_bytes
            .get()
            .checked_add(frame_bytes)
            .unwrap_or(usize::MAX)
    }
}

/// A number of items here are AHashMaps rather than LfuCaches;
/// eviction is managed by recreating Self when the Atlas is filled
pub struct GlyphCache {
    glyph_cache: AHashMap<GlyphKey, Rc<CachedGlyph>>,
    pub atlas: Atlas,
    pub fonts: Rc<FontConfiguration>,
    image_cache: LfuCache<ImageCacheKey, DecodedImage>,
    image_cache_retained_bytes: usize,
    image_cache_entry_bytes: AHashMap<ImageCacheKey, usize>,
    /// Binds a mutable `ImageData` allocation to the one content revision
    /// currently admitted on its behalf. The pointer is only an index; the
    /// retained `Weak` owns the allocation identity and prevents address reuse
    /// from conflating two different image objects.
    image_revision_owners: AHashMap<ImageObjectKey, ImageRevisionOwner>,
    /// FIFO authority for deterministic validation failures. Entries name the
    /// rejected revision through `image_revision_owners`; no strong image Arc
    /// is retained, and the hard cap bounds hostile malformed-image churn.
    image_validation_rejection_order: VecDeque<ImageCacheKey>,
    image_revision_owner_registrations_since_prune: usize,
    frame_cache: AHashMap<FrameSpriteKey, Sprite>,
    blank_frame_cache: AHashMap<BlankFrameKey, Sprite>,
    line_glyphs: AHashMap<LineKey, Sprite>,
    pub block_glyphs: AHashMap<SizedBlockKey, Sprite>,
    pub cursor_glyphs: AHashMap<(Option<CursorShape>, u8), Sprite>,
    pub color: AHashMap<(RgbColor, NotNan<f32>), Sprite>,
    min_frame_duration: Duration,
    /// Per-frame snapshot of `atlas.version()` at the start of the
    /// frame (ft-c9arc). Sprites whose stamped version is `<=
    /// last_synced_version` were already observed by the renderer
    /// and don't need re-syncing; sprites whose version is strictly
    /// greater were uploaded since the snapshot and need attention.
    /// The cursor is `0` until the first paint pass calls
    /// `snapshot_atlas_version`.
    last_synced_version: u64,
}

impl GlyphCache {
    pub fn new_in_memory(fonts: &Rc<FontConfiguration>, size: usize) -> anyhow::Result<Self> {
        let surface: Rc<dyn Texture2d> = Rc::new(ImageTexture::new(size, size));
        let atlas = Atlas::new(&surface).expect("failed to create new texture atlas");

        Ok(Self {
            fonts: Rc::clone(fonts),
            glyph_cache: AHashMap::new(),
            image_cache: LfuCache::new(
                "glyph_cache.image_cache.hit.rate",
                "glyph_cache.image_cache.miss.rate",
                |config| config.glyph_cache_image_cache_size,
                &fonts.config(),
            ),
            image_cache_retained_bytes: 0,
            image_cache_entry_bytes: AHashMap::new(),
            image_revision_owners: AHashMap::new(),
            image_validation_rejection_order: VecDeque::new(),
            image_revision_owner_registrations_since_prune: 0,
            frame_cache: AHashMap::new(),
            blank_frame_cache: AHashMap::new(),
            atlas,
            line_glyphs: AHashMap::new(),
            block_glyphs: AHashMap::new(),
            cursor_glyphs: AHashMap::new(),
            color: AHashMap::new(),
            min_frame_duration: config::frame_interval_for_max_fps(fonts.config().max_fps),
            last_synced_version: 0,
        })
    }
}

impl GlyphCache {
    pub fn new_gl(
        backend: &RenderContext,
        fonts: &Rc<FontConfiguration>,
        size: usize,
    ) -> anyhow::Result<Self> {
        let surface = backend.allocate_texture_atlas(size)?;
        let atlas = Atlas::new(&surface).expect("failed to create new texture atlas");

        Ok(Self {
            fonts: Rc::clone(fonts),
            glyph_cache: AHashMap::new(),
            image_cache: LfuCache::new(
                "glyph_cache.image_cache.hit.rate",
                "glyph_cache.image_cache.miss.rate",
                |config| config.glyph_cache_image_cache_size,
                &fonts.config(),
            ),
            image_cache_retained_bytes: 0,
            image_cache_entry_bytes: AHashMap::new(),
            image_revision_owners: AHashMap::new(),
            image_validation_rejection_order: VecDeque::new(),
            image_revision_owner_registrations_since_prune: 0,
            frame_cache: AHashMap::new(),
            blank_frame_cache: AHashMap::new(),
            atlas,
            line_glyphs: AHashMap::new(),
            block_glyphs: AHashMap::new(),
            cursor_glyphs: AHashMap::new(),
            color: AHashMap::new(),
            min_frame_duration: config::frame_interval_for_max_fps(fonts.config().max_fps),
            last_synced_version: 0,
        })
    }
}

impl GlyphCache {
    fn atlas_footprint_bytes(&self) -> u64 {
        let side = self.atlas.size() as u64;
        side.saturating_mul(side).saturating_mul(4)
    }

    fn image_cache_max_bytes() -> usize {
        MemoryBudget::default()
            .host_ram_budget_bytes
            .try_into()
            .unwrap_or(usize::MAX)
    }

    /// Move the complete decoded-image cache authority as one unit.
    ///
    /// Atlas recreation intentionally preserves decoded images so animations
    /// do not restart. The LFU residents, exact byte ledger, object-revision
    /// owners, and bounded rejection authority are one invariant: moving only
    /// the LFU loses accounting and makes later mutable-image revisions unable
    /// to retire or safely retry their prior entry.
    pub(crate) fn swap_decoded_image_cache_state(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.image_cache, &mut other.image_cache);
        std::mem::swap(
            &mut self.image_cache_retained_bytes,
            &mut other.image_cache_retained_bytes,
        );
        std::mem::swap(
            &mut self.image_cache_entry_bytes,
            &mut other.image_cache_entry_bytes,
        );
        std::mem::swap(
            &mut self.image_revision_owners,
            &mut other.image_revision_owners,
        );
        std::mem::swap(
            &mut self.image_validation_rejection_order,
            &mut other.image_validation_rejection_order,
        );
        std::mem::swap(
            &mut self.image_revision_owner_registrations_since_prune,
            &mut other.image_revision_owner_registrations_since_prune,
        );
    }

    fn prune_stale_image_revision_owners(&mut self) {
        self.image_revision_owners
            .retain(|_, owner| owner.image.strong_count() != 0);
        let owners = &self.image_revision_owners;
        self.image_validation_rejection_order.retain(|key| {
            owners.get(&key.object).is_some_and(|owner| {
                owner.image.as_ptr() == key.object.0
                    && owner.revision == key.revision
                    && owner.validation_rejected
            })
        });
    }

    fn image_revision_is_rejected(&self, key: &ImageCacheKey, image: &Arc<ImageData>) -> bool {
        key.object == ImageObjectKey::of(image)
            && self
                .image_revision_owners
                .get(&key.object)
                .is_some_and(|owner| {
                    owner.image.as_ptr() == Arc::as_ptr(image)
                        && owner.revision == key.revision
                        && owner.validation_rejected
                })
    }

    fn record_image_validation_rejection(&mut self, key: ImageCacheKey, image: &Arc<ImageData>) {
        let should_record = self
            .image_revision_owners
            .get_mut(&key.object)
            .filter(|owner| {
                owner.image.as_ptr() == Arc::as_ptr(image) && owner.revision == key.revision
            })
            .is_some_and(|owner| {
                if owner.validation_rejected {
                    false
                } else {
                    owner.validation_rejected = true;
                    true
                }
            });
        if !should_record {
            return;
        }

        self.image_validation_rejection_order.push_back(key);
        while self.image_validation_rejection_order.len() > MAX_REJECTED_IMAGE_REVISIONS {
            let Some(expired) = self.image_validation_rejection_order.pop_front() else {
                break;
            };
            let matches_expired =
                self.image_revision_owners
                    .get(&expired.object)
                    .is_some_and(|owner| {
                        owner.image.as_ptr() == expired.object.0
                            && owner.revision == expired.revision
                            && owner.validation_rejected
                    });
            if matches_expired {
                if self.image_cache.contains_key(&expired) {
                    if let Some(owner) = self.image_revision_owners.get_mut(&expired.object) {
                        owner.validation_rejected = false;
                    }
                } else {
                    self.image_revision_owners.remove(&expired.object);
                }
            }
        }
    }

    fn note_image_revision_owner_registration(&mut self) {
        self.image_revision_owner_registrations_since_prune = self
            .image_revision_owner_registrations_since_prune
            .checked_add(1)
            .unwrap_or(IMAGE_REVISION_OWNER_PRUNE_INTERVAL);
        if self.image_revision_owner_registrations_since_prune
            >= IMAGE_REVISION_OWNER_PRUNE_INTERVAL
        {
            self.prune_stale_image_revision_owners();
            self.image_revision_owner_registrations_since_prune = 0;
        }
    }

    /// Bind the current revision to this exact `Arc` allocation before cache
    /// lookup. If the same mutable object advanced to another revision, retire
    /// its prior object-scoped entry without disturbing equal-content objects.
    fn bind_image_revision(&mut self, image: &Arc<ImageData>, revision: [u8; 32]) -> ImageCacheKey {
        let key = ImageObjectKey::of(image);
        let mut prior_cache_key = None;
        let mut prior_rejection_key = None;
        let needs_new_owner = match self.image_revision_owners.get_mut(&key) {
            Some(owner) if owner.image.as_ptr() == key.0 => {
                if owner.revision != revision {
                    let prior_key = ImageCacheKey {
                        object: key,
                        revision: owner.revision,
                    };
                    prior_cache_key = Some(prior_key);
                    if owner.validation_rejected {
                        prior_rejection_key = Some(prior_key);
                    }
                    owner.revision = revision;
                    owner.validation_rejected = false;
                }
                false
            }
            // A retained Weak prevents real allocator reuse at this address.
            // Treat a mismatched Weak as a stale/corrupt identity slot and
            // replace it without touching the unrelated revision it names.
            Some(_) | None => true,
        };

        if needs_new_owner {
            self.image_revision_owners.insert(
                key,
                ImageRevisionOwner {
                    image: Arc::downgrade(image),
                    revision,
                    validation_rejected: false,
                },
            );
            self.note_image_revision_owner_registration();
        }

        if let Some(prior_cache_key) = prior_cache_key {
            self.remove_cached_image_key(&prior_cache_key);
        }
        if let Some(prior_rejection_key) = prior_rejection_key {
            self.image_validation_rejection_order
                .retain(|rejected| *rejected != prior_rejection_key);
        }

        ImageCacheKey {
            object: key,
            revision,
        }
    }

    fn forget_image_revision_owner(&mut self, key: &ImageCacheKey, image: &Arc<ImageData>) {
        if key.object == ImageObjectKey::of(image) {
            self.forget_image_revision_owner_by_key(key);
        }
    }

    fn forget_image_revision_owner_by_key(&mut self, key: &ImageCacheKey) {
        let (matches_removed_entry, was_rejected) = self
            .image_revision_owners
            .get(&key.object)
            .map_or((false, false), |owner| {
                let matches =
                    owner.image.as_ptr() == key.object.0 && owner.revision == key.revision;
                (matches, matches && owner.validation_rejected)
            });
        if matches_removed_entry {
            self.image_revision_owners.remove(&key.object);
        }
        if was_rejected {
            self.image_validation_rejection_order
                .retain(|rejected| rejected != key);
        }
    }

    fn remove_cached_image_key(&mut self, key: &ImageCacheKey) {
        if let Some((removed_key, decoded)) = self.image_cache.remove(key) {
            self.forget_decoded_image_bytes(&removed_key, &decoded);
        }
    }

    fn remove_cached_image_key_preserving_owner(&mut self, key: &ImageCacheKey) {
        if let Some((removed_key, _decoded)) = self.image_cache.remove(key) {
            self.forget_decoded_image_accounting(&removed_key);
        }
    }

    fn recompute_image_cache_retained_bytes(&mut self) {
        let mut entry_bytes = AHashMap::with_capacity(self.image_cache.len());
        let mut retained_bytes = 0usize;
        self.image_cache.for_each_resident(|key, decoded| {
            let bytes = decoded.retained_bytes();
            let replaced = entry_bytes.insert(*key, bytes);
            debug_assert!(replaced.is_none());
            retained_bytes = retained_bytes.checked_add(bytes).unwrap_or(usize::MAX);
        });
        self.image_cache_entry_bytes = entry_bytes;
        self.image_cache_retained_bytes = retained_bytes;
    }

    fn record_decoded_image_bytes(&mut self, key: ImageCacheKey, bytes: usize) {
        let prior = self.image_cache_entry_bytes.insert(key, bytes).unwrap_or(0);
        let updated = self
            .image_cache_retained_bytes
            .checked_sub(prior)
            .and_then(|retained| retained.checked_add(bytes));
        if let Some(updated) = updated {
            self.image_cache_retained_bytes = updated;
        } else {
            self.recompute_image_cache_retained_bytes();
        }
    }

    fn forget_decoded_image_bytes(&mut self, key: &ImageCacheKey, decoded: &DecodedImage) {
        if let Some(resident) = self.image_cache.peek(key) {
            // Same-revision replacement: retain the new binding when the
            // displaced and resident values share the exact Arc allocation;
            // otherwise retire only the displaced object's binding.
            if !Arc::ptr_eq(&resident.image, &decoded.image) {
                self.forget_image_revision_owner(key, &decoded.image);
            }
        } else {
            self.forget_image_revision_owner(key, &decoded.image);
        }
        self.forget_decoded_image_accounting(key);
    }

    fn forget_decoded_image_accounting(&mut self, key: &ImageCacheKey) {
        let Some(bytes) = self.image_cache_entry_bytes.remove(key) else {
            self.recompute_image_cache_retained_bytes();
            return;
        };
        if let Some(retained) = self.image_cache_retained_bytes.checked_sub(bytes) {
            self.image_cache_retained_bytes = retained;
        } else {
            self.recompute_image_cache_retained_bytes();
        }
    }

    fn apply_image_cache_evictions(&mut self, evicted: Vec<(ImageCacheKey, DecodedImage)>) {
        for (key, decoded) in evicted {
            self.forget_decoded_image_bytes(&key, &decoded);
        }
    }

    fn cache_decoded_image(&mut self, key: ImageCacheKey, decoded: DecodedImage) {
        let bytes = decoded.retained_bytes();
        self.cache_decoded_image_with_bytes(key, decoded, bytes);
    }

    fn cache_decoded_image_with_bytes(
        &mut self,
        key: ImageCacheKey,
        decoded: DecodedImage,
        bytes: usize,
    ) {
        if key.object != ImageObjectKey::of(&decoded.image)
            || key.revision != decoded.expected_revision
        {
            log::error!("refusing decoded image whose cache authority does not match its payload");
            self.forget_image_revision_owner_by_key(&key);
            return;
        }
        let max_bytes = Self::image_cache_max_bytes();
        if bytes == usize::MAX || bytes > max_bytes {
            self.forget_image_revision_owner_by_key(&key);
            return;
        }

        let evicted = self.image_cache.put_capturing_evictions(key, decoded);
        self.apply_image_cache_evictions(evicted);
        if self.image_cache.contains_key(&key) {
            self.record_decoded_image_bytes(key, bytes);
        } else if self.image_cache_entry_bytes.remove(&key).is_some() {
            self.recompute_image_cache_retained_bytes();
            self.forget_image_revision_owner_by_key(&key);
        } else {
            self.forget_image_revision_owner_by_key(&key);
        }
        self.enforce_image_cache_byte_budget();
    }

    fn refresh_cached_image_bytes(&mut self, key: ImageCacheKey, bytes: usize) {
        if bytes == usize::MAX || bytes > Self::image_cache_max_bytes() {
            if let Some((removed_key, decoded)) = self.image_cache.remove(&key) {
                self.forget_decoded_image_bytes(&removed_key, &decoded);
            }
            return;
        }

        if self.image_cache_retained_bytes == usize::MAX
            || !self.image_cache_entry_bytes.contains_key(&key)
            || self.image_cache_entry_bytes.len() != self.image_cache.len()
        {
            self.recompute_image_cache_retained_bytes();
        }
        self.record_decoded_image_bytes(key, bytes);
        self.enforce_image_cache_byte_budget();
    }

    fn enforce_image_cache_byte_budget(&mut self) {
        if self.image_cache_entry_bytes.len() != self.image_cache.len() {
            self.recompute_image_cache_retained_bytes();
        }
        let max_bytes = Self::image_cache_max_bytes();
        while self.image_cache_retained_bytes > max_bytes {
            let Some((key, decoded)) = self.image_cache.evict_lfu() else {
                // A positive retained total without an evictable resident is
                // an internal-index/accounting inconsistency. Drop the cache
                // fail-closed rather than leave unbudgeted decoded images or
                // dangling object-revision owners live.
                self.image_cache.clear();
                self.image_cache_retained_bytes = 0;
                self.image_cache_entry_bytes.clear();
                self.image_revision_owners.clear();
                self.image_validation_rejection_order.clear();
                break;
            };
            self.forget_decoded_image_bytes(&key, &decoded);
        }
    }

    pub fn tier_swap_doctor_row(&self, label: impl Into<String>) -> TierSwapDoctorRow {
        let mut stats = TierSwapStats::default();
        stats.record_peak(self.atlas_footprint_bytes(), 0);
        let budget = MemoryBudget::default();
        TierSwapDoctorRow::from_stats(
            label,
            stats,
            Some(budget.vram_budget_bytes),
            Some(budget.host_ram_budget_bytes),
        )
    }

    pub fn tier_swap_doctor_report(&self) -> TierSwapDoctorReport {
        TierSwapDoctorReport::from_rows(vec![self.tier_swap_doctor_row("gui.glyph_cache.atlas")])
    }

    pub fn default_warmup_plan() -> Vec<GlyphWarmupRequest> {
        let mut plan = Vec::with_capacity(
            95 + COMMON_LIGATURE_WARMUP_TEXT.len() + COMMON_NERD_FONT_ICON_WARMUP_CODEPOINTS.len(),
        );

        for byte in b' '..=b'~' {
            plan.push(GlyphWarmupRequest::new(
                char::from(byte).to_string(),
                GlyphWarmupPriority::AsciiPrintable,
            ));
        }

        for text in COMMON_LIGATURE_WARMUP_TEXT {
            plan.push(GlyphWarmupRequest::new(
                *text,
                GlyphWarmupPriority::CommonLigature,
            ));
        }

        for codepoint in COMMON_NERD_FONT_ICON_WARMUP_CODEPOINTS {
            plan.push(GlyphWarmupRequest::new(
                codepoint.to_string(),
                GlyphWarmupPriority::NerdFontIcon,
            ));
        }

        plan
    }

    pub fn warm_up_default_glyphs(
        &mut self,
        metrics: &RenderMetrics,
        budget: Duration,
    ) -> GlyphWarmupStats {
        self.warm_up_glyphs(&Self::default_warmup_plan(), metrics, budget)
    }

    /// ft-b4vw9: drop cached glyphs and block sprites whose `CellMetricKey` no
    /// longer matches `current`. After a font scale change the key's embedded
    /// `CellMetricKey` changes, so old-scale entries become unreachable from
    /// new-scale lookups (`GlyphKey::metric` / `SizedBlockKey::size`) and would
    /// otherwise accumulate dead across repeated scale changes. Returns the
    /// number of entries removed.
    ///
    /// Isomorphism: every evicted entry is keyed by a `CellMetricKey != current`,
    /// so no lookup at the current scale can resolve to it — rendered pixels are
    /// unchanged. A later scale-back re-rasterizes the dropped glyphs identically
    /// through `cached_glyph` (cache miss → same rasterizer, same metrics). This
    /// reclaims the CPU-side maps; the atlas is a bump allocator, so its
    /// texture space is reclaimed only by a subsequent grow/rebuild, not here.
    pub fn evict_stale_cell_metrics(&mut self, current: CellMetricKey) -> usize {
        let before = self.glyph_cache.len() + self.block_glyphs.len();
        self.glyph_cache.retain(|key, _| key.metric == current);
        self.block_glyphs.retain(|key, _| key.size == current);
        before - (self.glyph_cache.len() + self.block_glyphs.len())
    }

    pub fn warm_up_glyphs(
        &mut self,
        requests: &[GlyphWarmupRequest],
        metrics: &RenderMetrics,
        budget: Duration,
    ) -> GlyphWarmupStats {
        let start = Instant::now();
        let mut stats = GlyphWarmupStats::new();

        if requests.is_empty() {
            return stats;
        }

        if budget.is_zero() {
            stats.budget_exhausted = true;
            return stats;
        }

        let style = TextStyle::default();
        let font = match self.fonts.resolve_font(&style) {
            Ok(font) => font,
            Err(err) => {
                log::debug!("glyph warm-up could not resolve default font: {err:#}");
                stats.failed_glyphs = requests.len();
                stats.elapsed = start.elapsed();
                return stats;
            }
        };

        for request in requests {
            if start.elapsed() >= budget {
                stats.budget_exhausted = true;
                break;
            }

            stats.attempted_requests = stats.attempted_requests.saturating_add(1);

            let glyphs = match font.blocking_shape(
                &request.text,
                None,
                Direction::LeftToRight,
                None,
                None,
            ) {
                Ok(glyphs) => glyphs,
                Err(err) => {
                    log::debug!(
                        "glyph warm-up shaping failed for {:?} ({:?}): {err:#}",
                        request.text,
                        request.priority
                    );
                    stats.failed_glyphs = stats.failed_glyphs.saturating_add(1);
                    continue;
                }
            };

            for (index, info) in glyphs.iter().enumerate() {
                if start.elapsed() >= budget {
                    stats.budget_exhausted = true;
                    break;
                }

                // Use the same key as glyph_infos_to_glyphs during painting.
                // A request can shape into several glyphs with different spans.
                let followed_by_space = glyphs.get(index + 1).is_some_and(|next| next.is_space);
                match self.cached_glyph_for_subpixel_bin_with_status(
                    info,
                    &style,
                    followed_by_space,
                    &font,
                    metrics,
                    info.num_cells,
                    SubpixelBin::Quarter0,
                ) {
                    Ok((_, true)) => {
                        stats.cache_hits = stats.cache_hits.saturating_add(1);
                    }
                    Ok((_, false)) => {
                        stats.warmed_glyphs = stats.warmed_glyphs.saturating_add(1);
                    }
                    Err(err) => {
                        log::debug!(
                            "glyph warm-up rasterization failed for {:?} ({:?}): {err:#}",
                            request.text,
                            request.priority
                        );
                        stats.failed_glyphs = stats.failed_glyphs.saturating_add(1);
                    }
                }
            }
        }

        stats.elapsed = start.elapsed();
        stats
    }

    /// Resolve a glyph from the cache, rendering the glyph on-demand if
    /// the cache doesn't already hold the desired glyph.
    pub fn cached_glyph(
        &mut self,
        info: &GlyphInfo,
        style: &TextStyle,
        followed_by_space: bool,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        num_cells: u8,
    ) -> anyhow::Result<Rc<CachedGlyph>> {
        self.cached_glyph_for_subpixel_bin(
            info,
            style,
            followed_by_space,
            font,
            metrics,
            num_cells,
            SubpixelBin::Quarter0,
        )
    }

    pub fn cached_subpixel_glyph_bins(
        &mut self,
        info: &GlyphInfo,
        style: &TextStyle,
        followed_by_space: bool,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        num_cells: u8,
    ) -> anyhow::Result<Vec<Rc<CachedGlyph>>> {
        let mut glyphs = Vec::with_capacity(SubpixelBin::all().len());
        for &subpixel_bin in SubpixelBin::all() {
            glyphs.push(self.cached_glyph_for_subpixel_bin(
                info,
                style,
                followed_by_space,
                font,
                metrics,
                num_cells,
                subpixel_bin,
            )?);
        }
        Ok(glyphs)
    }

    pub fn cached_glyph_for_subpixel_bin(
        &mut self,
        info: &GlyphInfo,
        style: &TextStyle,
        followed_by_space: bool,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        num_cells: u8,
        subpixel_bin: SubpixelBin,
    ) -> anyhow::Result<Rc<CachedGlyph>> {
        self.cached_glyph_for_subpixel_bin_with_status(
            info,
            style,
            followed_by_space,
            font,
            metrics,
            num_cells,
            subpixel_bin,
        )
        .map(|(glyph, _)| glyph)
    }

    fn cached_glyph_for_subpixel_bin_with_status(
        &mut self,
        info: &GlyphInfo,
        style: &TextStyle,
        followed_by_space: bool,
        font: &Rc<LoadedFont>,
        metrics: &RenderMetrics,
        num_cells: u8,
        subpixel_bin: SubpixelBin,
    ) -> anyhow::Result<(Rc<CachedGlyph>, bool)> {
        let key = BorrowedGlyphKey {
            font_idx: info.font_idx,
            glyph_pos: info.glyph_pos,
            font_feature_atlas_key: monochrome_glyph_feature_atlas_key(font.id(), info.glyph_pos),
            subpixel_bin,
            num_cells,
            style,
            followed_by_space,
            metric: metrics.into(),
            id: font.id(),
        };

        if let Some(entry) = self.glyph_cache.get(&key as &dyn GlyphKeyTrait) {
            metrics::histogram!("glyph_cache.glyph_cache.hit.rate").record(1.);
            return Ok((Rc::clone(entry), true));
        }
        metrics::histogram!("glyph_cache.glyph_cache.miss.rate").record(1.);

        let glyph = match self.load_glyph(info, font, followed_by_space, num_cells) {
            Ok(g) => g,
            Err(err) => {
                if err
                    .root_cause()
                    .downcast_ref::<OutOfTextureSpace>()
                    .is_some()
                {
                    // Ensure that we propagate this signal to expand
                    // our available teexture space
                    return Err(err);
                }

                // But otherwise: don't allow glyph loading errors to propagate,
                // as that will result in incomplete window painting.
                // Log the error and substitute instead.
                log::error!(
                    "load_glyph failed; using blank instead. Error: {:#}. {:?} {:?}",
                    err,
                    info,
                    style
                );
                Rc::new(CachedGlyph {
                    brightness_adjust: 1.0,
                    has_color: false,
                    texture: None,
                    x_advance: PixelLength::zero(),
                    x_offset: PixelLength::zero(),
                    y_offset: PixelLength::zero(),
                    bearing_x: PixelLength::zero(),
                    bearing_y: PixelLength::zero(),
                    scale: 1.0,
                })
            }
        };
        self.glyph_cache.insert(key.to_owned(), Rc::clone(&glyph));
        Ok((glyph, false))
    }

    pub fn config_changed(&mut self, config: &ConfigHandle) {
        // The caller must pass the newly resolved handle explicitly. During a
        // TermWindow reload, FontConfiguration is updated later and still
        // exposes the prior handle at this point.
        self.min_frame_duration = config::frame_interval_for_max_fps(config.max_fps);
        let evicted = self.image_cache.update_config_capturing_evictions(config);
        self.apply_image_cache_evictions(evicted);
        self.enforce_image_cache_byte_budget();
        self.cursor_glyphs.clear();
    }

    /// Read the current per-frame "last synced" cursor (ft-c9arc).
    ///
    /// Sprites whose stamped version is `<= last_synced_version()`
    /// were already observed by the renderer in a prior frame;
    /// sprites whose version is strictly greater were uploaded
    /// since the snapshot and need attention from the current frame.
    #[inline]
    #[must_use]
    pub fn last_synced_version(&self) -> u64 {
        self.last_synced_version
    }

    /// Snapshot the atlas's current version into the per-frame cursor
    /// (ft-c9arc). Call this once at the start of each paint pass —
    /// future allocates during the pass will then bump the atlas
    /// version above the cursor and the renderer can detect drift
    /// via `sprite_needs_resync`.
    pub fn snapshot_atlas_version(&mut self) {
        self.last_synced_version = self.atlas.version();
    }

    /// True iff a sprite's version is newer than the per-frame
    /// snapshot — i.e. the sprite was uploaded since the start of
    /// the current paint pass and the renderer should re-sync it.
    #[inline]
    #[must_use]
    pub fn sprite_needs_resync(&self, sprite_version: u64) -> bool {
        sprite_version > self.last_synced_version
    }

    /// Perform the load and render of a glyph
    #[allow(clippy::float_cmp)]
    fn load_glyph(
        &mut self,
        info: &GlyphInfo,
        font: &Rc<LoadedFont>,
        followed_by_space: bool,
        num_cells: u8,
    ) -> anyhow::Result<Rc<CachedGlyph>> {
        let base_metrics;
        let idx_metrics;
        let brightness_adjust;
        let glyph;

        {
            base_metrics = font.metrics();
            glyph = font.rasterize_glyph(info.glyph_pos, info.font_idx)?;

            idx_metrics = font.metrics_for_idx(info.font_idx)?;
            brightness_adjust = font.brightness_adjust(info.font_idx);
        }

        let aspect = (idx_metrics.cell_width / idx_metrics.cell_height).get();

        // 0.7 is used for this as that is ~ the threshold for \u24e9 on a mac,
        // which is looks squareish and for which it is desirable to allow to
        // overflow.  0.5 is the typical monospace font aspect ratio.
        let is_square_or_wide = aspect >= 0.7;

        let allow_width_overflow = if is_square_or_wide {
            match self.fonts.config().allow_square_glyphs_to_overflow_width {
                AllowSquareGlyphOverflow::Never => false,
                AllowSquareGlyphOverflow::Always => true,
                AllowSquareGlyphOverflow::WhenFollowedBySpace => followed_by_space,
            }
        } else {
            false
        };

        // We shouldn't need to render a glyph that occupies zero cells, but that
        // can happen somehow; see <https://github.com/wezterm/wezterm/issues/1042>
        // so let's treat 0 cells as 1 cell so that we don't try to divide by
        // zero below.
        let num_cells = num_cells.max(1) as f64;

        // Maximum width allowed for this glyph based on its unicode width and
        // the dimensions of a cell
        let max_pixel_width = base_metrics.cell_width.get() * (num_cells + 0.25);

        let scale;

        // This helps to compensate for the !idx_metrics.is_scaled && glyph.is_scaled
        // case which happens when using the harfbuzz rasterizer with a bitmap font.
        // The default value is no compensation.
        let mut metrics_only_scale = 1.0;

        if info.font_idx == 0 {
            // We are the base font
            scale = if allow_width_overflow || glyph.width as f64 <= max_pixel_width {
                1.0
            } else {
                // Scale the glyph to fit in its number of cells
                1.0 / num_cells
            };
        } else if !glyph.is_scaled {
            // A bitmap font that isn't scaled to the requested height.
            let y_scale = base_metrics.cell_height.get() / idx_metrics.cell_height.get();
            let y_scaled_width = y_scale * glyph.width as f64;

            if allow_width_overflow || y_scaled_width <= max_pixel_width {
                // prefer height-wise scaling
                scale = y_scale;
            } else {
                // otherwise just make it fit the width
                scale = max_pixel_width / glyph.width as f64;
            }
        } else {
            // a scalable fallback font

            let f_width = glyph.width as f64;

            if allow_width_overflow || f_width <= max_pixel_width {
                scale = 1.0;
            } else {
                scale = max_pixel_width / f_width;
            }

            if !idx_metrics.is_scaled {
                // A special case: the shaper (eg: harfbuzz) processed
                // a bitmap font (eg: older versions of Noto Color Emoji)
                // to produce shaping info at the bitmap strike size,
                // which is 128 for that font.  The advance is expressed
                // at that size and not at the size of the font.
                // If we get to this condition, the rasterizer used a mode
                // where it has already scaled the glyph, so the dimensions
                // in the bitmap are correct, but the shaper metrics need
                // to be adjusted.
                let y_scale = base_metrics.cell_height.get() / idx_metrics.cell_height.get();
                metrics_only_scale = y_scale;
            }

            #[cfg(debug_assertions)]
            {
                log::debug!(
                    "{text} allow_width_overflow={allow_width_overflow} \
                     is_square_or_wide={is_square_or_wide} aspect={aspect} \
                     max_pixel_width={max_pixel_width} glyph.width={glyph_width} \
                     -> scale={scale} metrics_only_scale={metrics_only_scale}",
                    text = info.text,
                    glyph_width = glyph.width,
                );
            }
        };

        let descender_adjust = if info.font_idx == 0 {
            PixelLength::new(0.0)
        } else {
            idx_metrics.force_y_adjust
        };

        let (cell_width, cell_height) = (base_metrics.cell_width, base_metrics.cell_height);

        let glyph = if glyph.width == 0 || glyph.height == 0 {
            // a whitespace glyph
            CachedGlyph {
                brightness_adjust: 1.0,
                has_color: glyph.has_color,
                texture: None,
                x_offset: info.x_offset * scale,
                y_offset: info.y_offset * scale,
                x_advance: info.x_advance * scale,
                bearing_x: PixelLength::zero(),
                bearing_y: descender_adjust,
                scale,
            }
        } else {
            let raw_im = Image::with_rgba32(
                glyph.width as usize,
                glyph.height as usize,
                4 * glyph.width as usize,
                &glyph.data,
            );

            let bearing_x = glyph.bearing_x * scale * metrics_only_scale;
            // No metrics_only_scale adjustment to bearing_y is needed because
            // the value comes from the rasterized glyph and not from the
            // shaper stage.
            let bearing_y = descender_adjust + (glyph.bearing_y * scale);
            let x_offset = info.x_offset * scale * metrics_only_scale;
            let y_offset = info.y_offset * scale * metrics_only_scale;
            let x_advance = info.x_advance * scale * metrics_only_scale;

            log::trace!(
                "bearing_x={bearing_x:?} bearing_y={bearing_y:?} \
                 x_offset={x_offset:?} y_offset={y_offset:?} x_advance={x_advance:?}"
            );

            let (scale, raw_im) = if scale != 1.0 {
                log::trace!(
                    "physically scaling {:?} by {} bcos {}x{} > {:?}x{:?}. aspect={}",
                    info,
                    scale,
                    glyph.width,
                    glyph.height,
                    cell_width,
                    cell_height,
                    aspect,
                );
                (1.0, raw_im.scale_by(scale))
            } else {
                (scale, raw_im)
            };

            let tex = self.atlas.allocate(&raw_im)?;

            let g = CachedGlyph {
                brightness_adjust,
                has_color: glyph.has_color,
                texture: Some(tex),
                x_offset,
                y_offset,
                x_advance,
                bearing_x,
                bearing_y,
                scale,
            };

            if info.font_idx != 0 {
                // It's generally interesting to examine eg: emoji or ligatures
                // that we might have fallen back to
                log::trace!("{:?} {:?}", info, g);
            }

            g
        };

        Ok(Rc::new(glyph))
    }

    fn cached_image_impl(
        frame_cache: &mut AHashMap<FrameSpriteKey, Sprite>,
        blank_frame_cache: &mut AHashMap<BlankFrameKey, Sprite>,
        atlas: &mut Atlas,
        decoded: &DecodedImage,
        padding: Option<usize>,
        min_frame_duration: Duration,
        allow_image: AllowImage,
    ) -> anyhow::Result<(Sprite, Option<Instant>, LoadState)> {
        let scale_down = match allow_image {
            AllowImage::Scale(n) => Some(n),
            _ => None,
        };

        let validation_result = decoded
            .decoded_validation
            .borrow()
            .as_ref()
            .map(DecodedImageValidationReceiver::try_recv);
        if let Some(validation_result) = validation_result {
            match validation_result {
                Ok(Ok(summary)) => {
                    let _ = decoded.decoded_validation.borrow_mut().take();
                    decoded.source_retained_bytes.set(summary.decoded_bytes);
                    decoded.load_state.set(LoadState::Loaded);
                }
                Ok(Err(ImageDataValidationError::ContentRevisionMismatch)) => {
                    let _ = decoded.decoded_validation.borrow_mut().take();
                    return Err(DecodedImageRevisionMismatch.into());
                }
                Ok(Err(ImageDataValidationError::DecodeCancelled))
                | Err(TryRecvError::Disconnected) => {
                    let _ = decoded.decoded_validation.borrow_mut().take();
                    return Err(DecodedImageValidationUnavailable.into());
                }
                Ok(Err(source)) => {
                    let _ = decoded.decoded_validation.borrow_mut().take();
                    return Err(DecodedImageValidationRejected { source }.into());
                }
                Err(TryRecvError::Empty) => {
                    let sprite = Self::cached_blank_frame_sprite(
                        blank_frame_cache,
                        atlas,
                        1,
                        1,
                        padding,
                        scale_down,
                    )?;
                    let retry_delay = min_frame_duration.max(Duration::from_millis(1));
                    return Ok(match Instant::now().checked_add(retry_delay) {
                        Some(next_due) => (sprite, Some(next_due), LoadState::Loading),
                        None => (sprite, None, LoadState::Failed),
                    });
                }
            }
        }

        // Encoded sources are decoded into independently owned immutable pixel
        // frames. The decoder can hold the source ImageData payload mutex while
        // hashing and decoding up to the bounded wire ceiling, so the render
        // thread must not queue behind that mutex merely to present a ready
        // frame (or its placeholder). The cached revision probe is fail-closed:
        // mutable access clears it before exposing the source payload and the
        // frame snapshot is retired on the next cache admission attempt.
        if decoded.frames.borrow().is_some() {
            return Self::cached_encoded_frame_for_revision(
                frame_cache,
                blank_frame_cache,
                atlas,
                decoded,
                padding,
                min_frame_duration,
                scale_down,
                || {},
            );
        }

        // Poll pending worker state before acquiring the potentially large
        // payload mutex. If the validator won that lock, the GUI must render a
        // placeholder rather than wait behind a full-buffer hash pass. A ready
        // result is still accepted only after this exact-revision guard.
        let image_data = decoded
            .image
            .data_for_content_revision(decoded.expected_revision)
            .ok_or(DecodedImageRevisionMismatch)?;

        let handle = DecodedImageHandle::payload(image_data, *decoded.current_frame.borrow());

        match handle.payload_data() {
            ImageDataType::Rgba8 {
                hash,
                width,
                height,
                ..
            } => {
                let key = FrameSpriteKey {
                    hash: *hash,
                    width: *width as usize,
                    height: *height as usize,
                    padding,
                    scale_down,
                };
                if let Some(sprite) = frame_cache.get(&key) {
                    return Ok((sprite.clone(), None, decoded.load_state.get()));
                }
                let sprite = atlas
                    .allocate_with_padding(&handle, padding, scale_down)
                    .context("atlas.allocate_with_padding")?;
                frame_cache.insert(key, sprite.clone());

                return Ok((sprite, None, decoded.load_state.get()));
            }
            ImageDataType::AnimRgba8 {
                hashes,
                frames,
                durations,
                width,
                height,
                ..
            } => {
                let mut next = None;
                let mut decoded_frame_start = decoded.frame_start.borrow_mut();
                let mut decoded_current_frame = decoded.current_frame.borrow_mut();
                if frames.len() > 1 {
                    let now = Instant::now();

                    // We round up the frame duration to at least the minimum
                    // frame duration that wezterm can use when rendering.
                    // There's no point trying to deal with smaller intervals
                    // because we simply cannot render them without dropping
                    // frames.
                    // In addition, with a 1ms frame delay, there's a good chance
                    // that any given cell may switch to a different frame from
                    // its neighbor while we are rendering the entire terminal
                    // frame, so we want to avoid that.
                    // <https://github.com/wezterm/wezterm/issues/3260>
                    let mut next_due = decoded_frame_start
                        .checked_add(durations[*decoded_current_frame].max(min_frame_duration));
                    if next_due.is_some_and(|due| now >= due) {
                        // Advance to next frame
                        *decoded_current_frame = *decoded_current_frame + 1;
                        if *decoded_current_frame >= frames.len() {
                            *decoded_current_frame = 0;
                            // Skip potential 0-duration root frame
                            if durations[0].as_millis() == 0 && frames.len() > 1 {
                                *decoded_current_frame = *decoded_current_frame + 1;
                            }
                        }
                        *decoded_frame_start = now;
                        next_due = decoded_frame_start
                            .checked_add(durations[*decoded_current_frame].max(min_frame_duration));
                        handle.set_current_frame(*decoded_current_frame);
                    }

                    next = next_due;
                }

                let load_state = if frames.len() > 1 && next.is_none() {
                    LoadState::Failed
                } else {
                    decoded.load_state.get()
                };

                let key = FrameSpriteKey {
                    hash: hashes[*decoded_current_frame],
                    width: *width as usize,
                    height: *height as usize,
                    padding,
                    scale_down,
                };

                if let Some(sprite) = frame_cache.get(&key) {
                    return Ok((sprite.clone(), next, load_state));
                }

                let sprite = atlas
                    .allocate_with_padding(&handle, padding, scale_down)
                    .context("atlas.allocate_with_padding")?;

                frame_cache.insert(key, sprite.clone());

                return Ok((sprite, next, load_state));
            }
            ImageDataType::EncodedLease(_) | ImageDataType::EncodedFile(_) => {
                // `frames.is_some()` permanently identifies the encoded-source
                // lane and returned above. Reaching an encoded payload here
                // means a direct decoded object changed variants between cache
                // admission attempts, so fail closed and rebind its revision.
                Err(DecodedImageRevisionMismatch.into())
            }
        }
    }

    fn cached_encoded_frame_for_revision<AfterRender>(
        frame_cache: &mut AHashMap<FrameSpriteKey, Sprite>,
        blank_frame_cache: &mut AHashMap<BlankFrameKey, Sprite>,
        atlas: &mut Atlas,
        decoded: &DecodedImage,
        padding: Option<usize>,
        min_frame_duration: Duration,
        scale_down: Option<usize>,
        after_render: AfterRender,
    ) -> anyhow::Result<(Sprite, Option<Instant>, LoadState)>
    where
        AfterRender: FnOnce(),
    {
        if !decoded
            .image
            .cached_content_revision_is(decoded.expected_revision)
        {
            return Err(DecodedImageRevisionMismatch.into());
        }
        let rendered = Self::cached_encoded_frame_impl(
            frame_cache,
            blank_frame_cache,
            atlas,
            decoded,
            padding,
            min_frame_duration,
            scale_down,
        )?;
        after_render();
        // Snapshot reads and atlas allocation do not hold the source payload lock.
        // Rebind after those operations so a mutation that began after the
        // first probe cannot publish an obsolete snapshot.
        if !decoded
            .image
            .cached_content_revision_is(decoded.expected_revision)
        {
            return Err(DecodedImageRevisionMismatch.into());
        }
        Ok(rendered)
    }

    fn cached_encoded_frame_impl(
        frame_cache: &mut AHashMap<FrameSpriteKey, Sprite>,
        blank_frame_cache: &mut AHashMap<BlankFrameKey, Sprite>,
        atlas: &mut Atlas,
        decoded: &DecodedImage,
        padding: Option<usize>,
        min_frame_duration: Duration,
        scale_down: Option<usize>,
    ) -> anyhow::Result<(Sprite, Option<Instant>, LoadState)> {
        let mut frames = decoded.frames.borrow_mut();
        let frames = frames.as_mut().expect("to have frames");

        let mut decoded_frame_start = decoded.frame_start.borrow_mut();
        let mut decoded_current_frame = decoded.current_frame.borrow_mut();

        // This function runs on the GUI render path. Decoding and frame
        // construction happen in the bounded worker pool, and presentation
        // only borrows its immutable in-memory result; waiting here (the
        // historical code waited for as long as 125 ms) directly turns an image
        // miss into visible input and resize latency. The transparent frame is
        // a non-blocking placeholder and `next_due` schedules another poll.

        let now = Instant::now();
        // We round up the frame duration to at least the minimum frame duration
        // that FrankenTerm can use when rendering. There's no point trying to
        // deal with smaller intervals because we cannot render them without
        // dropping frames. With a 1 ms delay, neighboring cells may also switch
        // frames while the terminal frame is being rendered.
        // <https://github.com/wezterm/wezterm/issues/3260>
        let Some(mut next_due) =
            decoded_frame_start.checked_add(frames.frame_duration().max(min_frame_duration))
        else {
            frames.load_state = LoadState::Failed;
            let sprite = Self::cached_blank_frame_sprite(
                blank_frame_cache,
                atlas,
                1,
                1,
                padding,
                scale_down,
            )?;
            return Ok((sprite, None, LoadState::Failed));
        };
        let mut decoder_queue_was_empty = false;
        if now >= next_due {
            if frames.load_next_frame() {
                *decoded_current_frame = (*decoded_current_frame).saturating_add(1);
                // A zero-duration first frame is animation composition state,
                // not visible content. If the decoder already queued the
                // first timed frame, consume exactly that one additional frame
                // now so a fast decoder does not force one transparent
                // presentation interval. This is intentionally bounded to one
                // extra `try_recv`: draining an arbitrarily long animation on
                // the GUI thread would exchange the blank-frame bug for
                // unbounded paint latency.
                if frames.awaiting_first_visible_frame() {
                    if frames.load_next_frame() {
                        *decoded_current_frame = (*decoded_current_frame).saturating_add(1);
                    } else if matches!(&frames.source, FrameSource::Decoder(_)) {
                        decoder_queue_was_empty = true;
                    }
                }
            } else if matches!(&frames.source, FrameSource::Decoder(_)) {
                decoder_queue_was_empty = true;
            }
            // Advance the ordinary animation clock after each due poll. If the
            // open decoder queue was empty, the deadline is finalized again
            // from a fresh timestamp after sprite lookup/allocation below; a
            // cold atlas allocation must not make the published retry stale.
            *decoded_frame_start = now;
            let Some(updated_due) =
                decoded_frame_start.checked_add(frames.frame_duration().max(min_frame_duration))
            else {
                frames.load_state = LoadState::Failed;
                let sprite = Self::cached_blank_frame_sprite(
                    blank_frame_cache,
                    atlas,
                    1,
                    1,
                    padding,
                    scale_down,
                )?;
                return Ok((sprite, None, LoadState::Failed));
            };
            next_due = updated_due;
        }

        if frames.awaiting_first_visible_frame() {
            let sprite = Self::cached_blank_frame_sprite(
                blank_frame_cache,
                atlas,
                1,
                1,
                padding,
                scale_down,
            )?;
            let Some(next_due) = finalize_decoder_retry_after_empty_poll(
                &mut decoded_frame_start,
                next_due,
                frames.frame_duration(),
                min_frame_duration,
                decoder_queue_was_empty,
            ) else {
                frames.load_state = LoadState::Failed;
                return Ok((sprite, None, LoadState::Failed));
            };
            return Ok((sprite, frames.next_frame_due(next_due), LoadState::Loading));
        }

        let key = FrameSpriteKey {
            hash: frames.frame_hash(),
            width: frames.current_frame.width,
            height: frames.current_frame.height,
            padding,
            scale_down,
        };

        if let Some(sprite) = frame_cache.get(&key) {
            let Some(next_due) = finalize_decoder_retry_after_empty_poll(
                &mut decoded_frame_start,
                next_due,
                frames.frame_duration(),
                min_frame_duration,
                decoder_queue_was_empty,
            ) else {
                frames.load_state = LoadState::Failed;
                return Ok((sprite.clone(), None, LoadState::Failed));
            };
            return Ok((
                sprite.clone(),
                frames.next_frame_due(next_due),
                frames.load_state,
            ));
        }

        let Some(expected_byte_size) = frames.current_frame.checked_decoded_bytes() else {
            report_frame_error(format!(
                "frame data dimensions overflow: {}x{}",
                frames.current_frame.width, frames.current_frame.height
            ));
            frames.load_state = LoadState::Failed;
            let sprite = Self::cached_blank_frame_sprite(
                blank_frame_cache,
                atlas,
                1,
                1,
                padding,
                scale_down,
            )?;
            return Ok((sprite, frames.next_frame_due(next_due), frames.load_state));
        };

        let sprite = if frames.current_frame.pixels.len() == expected_byte_size {
            let frame = DecodedImageHandle::frame(&frames.current_frame);
            let sprite = atlas.allocate_with_padding(&frame, padding, scale_down)?;
            frame_cache.insert(key, sprite.clone());
            sprite
        } else {
            report_frame_error(format!(
                "frame data is corrupted: expected size {expected_byte_size} but have {}",
                frames.current_frame.pixels.len()
            ));
            frames.load_state = LoadState::Failed;
            Self::cached_blank_frame_sprite(
                blank_frame_cache,
                atlas,
                frames.current_frame.width,
                frames.current_frame.height,
                padding,
                scale_down,
            )?
        };

        let Some(next_due) = finalize_decoder_retry_after_empty_poll(
            &mut decoded_frame_start,
            next_due,
            frames.frame_duration(),
            min_frame_duration,
            decoder_queue_was_empty,
        ) else {
            frames.load_state = LoadState::Failed;
            return Ok((sprite, None, LoadState::Failed));
        };

        Ok((sprite, frames.next_frame_due(next_due), frames.load_state))
    }

    fn cached_blank_frame_sprite(
        blank_frame_cache: &mut AHashMap<BlankFrameKey, Sprite>,
        atlas: &mut Atlas,
        width: usize,
        height: usize,
        padding: Option<usize>,
        scale_down: Option<usize>,
    ) -> anyhow::Result<Sprite> {
        let key = BlankFrameKey {
            width,
            height,
            padding,
            scale_down,
        };
        if let Some(sprite) = blank_frame_cache.get(&key) {
            return Ok(sprite.clone());
        }

        let frame = Image::new(width, height);
        let sprite = atlas.allocate_with_padding(&frame, padding, scale_down)?;
        blank_frame_cache.insert(key, sprite.clone());
        Ok(sprite)
    }

    pub fn cached_image(
        &mut self,
        image_data: &Arc<ImageData>,
        padding: Option<usize>,
        allow_image: AllowImage,
    ) -> anyhow::Result<(Sprite, Option<Instant>, LoadState)> {
        self.cached_image_with_revision_observers(
            image_data,
            padding,
            allow_image,
            |_| {},
            |_| {},
            |_| {},
        )
    }

    fn cached_image_with_revision_observers<AfterKey, BeforeLoad, AfterLoad>(
        &mut self,
        image_data: &Arc<ImageData>,
        padding: Option<usize>,
        allow_image: AllowImage,
        mut after_key_bound: AfterKey,
        mut before_load: BeforeLoad,
        mut after_load: AfterLoad,
    ) -> anyhow::Result<(Sprite, Option<Instant>, LoadState)>
    where
        AfterKey: FnMut(usize),
        BeforeLoad: FnMut(usize),
        AfterLoad: FnMut(usize),
    {
        const MAX_REVISION_ADMISSION_ATTEMPTS: usize = 2;

        for attempt in 0..MAX_REVISION_ADMISSION_ATTEMPTS {
            match self.cached_image_once(
                image_data,
                padding,
                allow_image,
                attempt,
                &mut after_key_bound,
                &mut before_load,
                &mut after_load,
            ) {
                Err(error)
                    if error
                        .downcast_ref::<DecodedImageRevisionMismatch>()
                        .is_some() =>
                {
                    if attempt + 1 < MAX_REVISION_ADMISSION_ATTEMPTS {
                        continue;
                    }
                    log::debug!(
                        "image content kept changing during bounded cache admission; deferring"
                    );
                    return self.image_loading_placeholder(padding, allow_image);
                }
                result => return result,
            }
        }

        unreachable!("bounded image revision admission loop always returns")
    }

    fn cached_image_once<AfterKey, BeforeLoad, AfterLoad>(
        &mut self,
        image_data: &Arc<ImageData>,
        padding: Option<usize>,
        allow_image: AllowImage,
        attempt: usize,
        after_key_bound: &mut AfterKey,
        before_load: &mut BeforeLoad,
        after_load: &mut AfterLoad,
    ) -> anyhow::Result<(Sprite, Option<Instant>, LoadState)>
    where
        AfterKey: FnMut(usize),
        BeforeLoad: FnMut(usize),
        AfterLoad: FnMut(usize),
    {
        // Kitty edits mutate decoded pixels in place. Cache authority therefore
        // combines exact Arc allocation identity with the mutation-maintained
        // content revision: two equal-content objects cannot alias a mutable
        // DecodedImage, while frame hashes can still share uploaded sprites.
        let revision = image_data.current_content_hash();
        let cache_key = self.bind_image_revision(image_data, revision);
        after_key_bound(attempt);

        let cached = {
            self.image_cache.get(&cache_key).map(|decoded| {
                let result = Self::cached_image_impl(
                    &mut self.frame_cache,
                    &mut self.blank_frame_cache,
                    &mut self.atlas,
                    decoded,
                    padding,
                    self.min_frame_duration,
                    allow_image,
                );
                (result, decoded.retained_bytes())
            })
        };
        if let Some((result, retained_bytes)) = cached {
            if let Err(error) = &result {
                if error
                    .downcast_ref::<DecodedImageRevisionMismatch>()
                    .is_some()
                {
                    self.remove_cached_image_key(&cache_key);
                    return result;
                }
                if error
                    .downcast_ref::<DecodedImageValidationUnavailable>()
                    .is_some()
                {
                    self.remove_cached_image_key(&cache_key);
                    log::debug!("decoded image validation deferred: {error:#}");
                    return self.image_loading_placeholder(padding, allow_image);
                }
                if error
                    .downcast_ref::<DecodedImageValidationRejected>()
                    .is_some()
                {
                    if image_data
                        .data_for_content_revision(cache_key.revision)
                        .is_none()
                    {
                        self.remove_cached_image_key(&cache_key);
                        return Err(DecodedImageRevisionMismatch.into());
                    }
                    self.remove_cached_image_key_preserving_owner(&cache_key);
                    log::warn!("rejecting malformed decoded image: {error:#}");
                    self.record_image_validation_rejection(cache_key, image_data);
                    return self.image_failed_placeholder(padding, allow_image);
                }
            }
            self.refresh_cached_image_bytes(cache_key, retained_bytes);
            return result;
        }

        if self.image_revision_is_rejected(&cache_key, image_data) {
            if image_data
                .data_for_content_revision(cache_key.revision)
                .is_some()
            {
                return self.image_failed_placeholder(padding, allow_image);
            }
            self.forget_image_revision_owner(&cache_key, image_data);
            return Err(DecodedImageRevisionMismatch.into());
        }

        before_load(attempt);
        let decoded = match DecodedImage::load_for_revision(image_data, cache_key.revision) {
            Ok(decoded) => decoded,
            Err(error) => {
                if error
                    .downcast_ref::<DecodedImageRevisionMismatch>()
                    .is_some()
                {
                    self.forget_image_revision_owner(&cache_key, image_data);
                    return Err(error);
                }
                if error.downcast_ref::<ImageDataValidationError>().is_some() {
                    if image_data
                        .data_for_content_revision(cache_key.revision)
                        .is_none()
                    {
                        self.forget_image_revision_owner(&cache_key, image_data);
                        return Err(DecodedImageRevisionMismatch.into());
                    }
                    log::warn!("rejecting malformed decoded image: {error:#}");
                    self.record_image_validation_rejection(cache_key, image_data);
                    return self.image_failed_placeholder(padding, allow_image);
                }
                // Queue/pool admission failures are transient resource
                // pressure. Render a placeholder for this attempt but do not
                // poison the cache; a later frame may retry admission.
                self.forget_image_revision_owner(&cache_key, image_data);
                log::debug!("image decoder admission deferred: {error:#}");
                return self.image_loading_placeholder(padding, allow_image);
            }
        };
        after_load(attempt);
        let res = match Self::cached_image_impl(
            &mut self.frame_cache,
            &mut self.blank_frame_cache,
            &mut self.atlas,
            &decoded,
            padding,
            self.min_frame_duration,
            allow_image,
        ) {
            Ok(result) => result,
            Err(error) => {
                if error
                    .downcast_ref::<DecodedImageRevisionMismatch>()
                    .is_some()
                {
                    self.forget_image_revision_owner(&cache_key, image_data);
                    return Err(error);
                }
                if error
                    .downcast_ref::<DecodedImageValidationUnavailable>()
                    .is_some()
                {
                    self.forget_image_revision_owner(&cache_key, image_data);
                    log::debug!("decoded image validation deferred: {error:#}");
                    return self.image_loading_placeholder(padding, allow_image);
                }
                if error
                    .downcast_ref::<DecodedImageValidationRejected>()
                    .is_some()
                {
                    if image_data
                        .data_for_content_revision(cache_key.revision)
                        .is_none()
                    {
                        self.forget_image_revision_owner(&cache_key, image_data);
                        return Err(DecodedImageRevisionMismatch.into());
                    }
                    log::warn!("rejecting malformed decoded image: {error:#}");
                    self.record_image_validation_rejection(cache_key, image_data);
                    return self.image_failed_placeholder(padding, allow_image);
                }
                self.forget_image_revision_owner(&cache_key, image_data);
                return Err(error);
            }
        };
        self.cache_decoded_image(cache_key, decoded);
        Ok(res)
    }

    fn image_loading_placeholder(
        &mut self,
        padding: Option<usize>,
        allow_image: AllowImage,
    ) -> anyhow::Result<(Sprite, Option<Instant>, LoadState)> {
        let scale_down = match allow_image {
            AllowImage::Scale(n) => Some(n),
            _ => None,
        };
        let sprite = Self::cached_blank_frame_sprite(
            &mut self.blank_frame_cache,
            &mut self.atlas,
            1,
            1,
            padding,
            scale_down,
        )?;
        let retry_delay = self.min_frame_duration.max(Duration::from_millis(1));
        let Some(next_due) = Instant::now().checked_add(retry_delay) else {
            return Ok((sprite, None, LoadState::Failed));
        };
        Ok((sprite, Some(next_due), LoadState::Loading))
    }

    fn image_failed_placeholder(
        &mut self,
        padding: Option<usize>,
        allow_image: AllowImage,
    ) -> anyhow::Result<(Sprite, Option<Instant>, LoadState)> {
        let scale_down = match allow_image {
            AllowImage::Scale(n) => Some(n),
            _ => None,
        };
        let sprite = Self::cached_blank_frame_sprite(
            &mut self.blank_frame_cache,
            &mut self.atlas,
            1,
            1,
            padding,
            scale_down,
        )?;
        Ok((sprite, None, LoadState::Failed))
    }

    pub fn cached_color(&mut self, color: RgbColor, alpha: f32) -> anyhow::Result<Sprite> {
        let key = (color, NotNan::new(alpha).unwrap());

        if let Some(s) = self.color.get(&key) {
            return Ok(s.clone());
        }

        let (red, green, blue) = color.to_tuple_rgb8();
        let alpha = (alpha * 255.0) as u8;

        let data = vec![
            red, green, blue, alpha, red, green, blue, alpha, red, green, blue, alpha, red, green,
            blue, alpha,
        ];
        let image = Image::from_raw(2, 2, data);

        let sprite = self.atlas.allocate(&image)?;
        self.color.insert(key, sprite.clone());
        Ok(sprite)
    }

    pub fn cached_block(
        &mut self,
        block: BlockKey,
        metrics: &RenderMetrics,
    ) -> anyhow::Result<Sprite> {
        let key = SizedBlockKey {
            block,
            size: metrics.into(),
        };
        if let Some(s) = self.block_glyphs.get(&key) {
            return Ok(s.clone());
        }
        self.block_sprite(metrics, key)
    }

    fn line_sprite(&mut self, key: LineKey, metrics: &RenderMetrics) -> anyhow::Result<Sprite> {
        let mut buffer = Image::new(
            metrics.cell_size.width as usize,
            metrics.cell_size.height as usize,
        );
        let black = SrgbaPixel::rgba(0, 0, 0, 0);
        let white = SrgbaPixel::rgba(0xff, 0xff, 0xff, 0xff);

        let cell_rect = Rect::new(Point::new(0, 0), metrics.cell_size);

        let draw_single = |buffer: &mut Image| {
            for row in 0..metrics.underline_height {
                buffer.draw_line(
                    Point::new(
                        cell_rect.origin.x,
                        cell_rect.origin.y + metrics.descender_row + row,
                    ),
                    Point::new(
                        cell_rect.origin.x + metrics.cell_size.width,
                        cell_rect.origin.y + metrics.descender_row + row,
                    ),
                    white,
                );
            }
        };

        let draw_dotted = |buffer: &mut Image| {
            for row in 0..metrics.underline_height {
                let y = (cell_rect.origin.y + metrics.descender_row + row) as usize;
                if y >= metrics.cell_size.height as usize {
                    break;
                }

                let mut color = white;
                let segment_length = (metrics.cell_size.width / 4) as usize;
                let mut count = segment_length;
                let range =
                    buffer.horizontal_pixel_range_mut(0, metrics.cell_size.width as usize, y);
                for c in range.iter_mut() {
                    *c = color.as_srgba32();
                    count -= 1;
                    if count == 0 {
                        color = if color == white { black } else { white };
                        count = segment_length;
                    }
                }
            }
        };

        let draw_dashed = |buffer: &mut Image| {
            for row in 0..metrics.underline_height {
                let y = (cell_rect.origin.y + metrics.descender_row + row) as usize;
                if y >= metrics.cell_size.height as usize {
                    break;
                }
                let mut color = white;
                let third = (metrics.cell_size.width / 3) as usize + 1;
                let mut count = third;
                let range =
                    buffer.horizontal_pixel_range_mut(0, metrics.cell_size.width as usize, y);
                for c in range.iter_mut() {
                    *c = color.as_srgba32();
                    count -= 1;
                    if count == 0 {
                        color = if color == white { black } else { white };
                        count = third;
                    }
                }
            }
        };

        let draw_curly = |buffer: &mut Image| {
            let max_y = (metrics.cell_size.height as usize).saturating_sub(1);
            let x_factor = (2. * std::f32::consts::PI) / (metrics.cell_size.width as f32).max(1.0);

            // Have the wave go from the descender to the bottom of the cell
            let wave_height =
                metrics.cell_size.height - (cell_rect.origin.y + metrics.descender_row);

            let half_height = (wave_height as f32 / 4.).max(1.);
            let y = ((cell_rect.origin.y + metrics.descender_row) as usize)
                .saturating_sub(half_height as usize);

            fn add(x: usize, y: usize, val: u8, max_y: usize, buffer: &mut Image) {
                let y = y.min(max_y);
                let pixel = buffer.pixel_mut(x, y);
                let (current, _, _, _) = SrgbaPixel::with_srgba_u32(*pixel).as_rgba();
                let value = current.saturating_add(val);
                *pixel = SrgbaPixel::rgba(value, value, value, value).as_srgba32();
            }

            for x in 0..metrics.cell_size.width as usize {
                let vertical = -half_height * (x as f32 * x_factor).sin() + half_height;
                let v1 = vertical.floor();
                let v2 = vertical.ceil();

                for row in 0..metrics.underline_height as usize {
                    let value = (255. * (vertical - v1).abs()) as u8;
                    add(
                        x,
                        row.saturating_add(y).saturating_add(v1 as usize),
                        255u8.saturating_sub(value),
                        max_y,
                        buffer,
                    );
                    add(
                        x,
                        row.saturating_add(y).saturating_add(v2 as usize),
                        value,
                        max_y,
                        buffer,
                    );
                }
            }
        };

        let draw_double = |buffer: &mut Image| {
            let first_line = metrics
                .descender_row
                .min(metrics.descender_plus_two - 2 * metrics.underline_height);

            for row in 0..metrics.underline_height {
                buffer.draw_line(
                    Point::new(cell_rect.origin.x, cell_rect.origin.y + first_line + row),
                    Point::new(
                        cell_rect.origin.x + metrics.cell_size.width,
                        cell_rect.origin.y + first_line + row,
                    ),
                    white,
                );
                buffer.draw_line(
                    Point::new(
                        cell_rect.origin.x,
                        cell_rect.origin.y + metrics.descender_plus_two + row,
                    ),
                    Point::new(
                        cell_rect.origin.x + metrics.cell_size.width,
                        cell_rect.origin.y + metrics.descender_plus_two + row,
                    ),
                    white,
                );
            }
        };

        let draw_strike = |buffer: &mut Image| {
            for row in 0..metrics.underline_height {
                buffer.draw_line(
                    Point::new(
                        cell_rect.origin.x,
                        cell_rect.origin.y + metrics.strike_row + row,
                    ),
                    Point::new(
                        cell_rect.origin.x + metrics.cell_size.width,
                        cell_rect.origin.y + metrics.strike_row + row,
                    ),
                    white,
                );
            }
        };

        let draw_overline = |buffer: &mut Image| {
            for row in 0..metrics.underline_height {
                buffer.draw_line(
                    Point::new(cell_rect.origin.x, cell_rect.origin.y + row),
                    Point::new(
                        cell_rect.origin.x + metrics.cell_size.width,
                        cell_rect.origin.y + row,
                    ),
                    white,
                );
            }
        };

        buffer.clear_rect(cell_rect, black);
        if key.overline {
            draw_overline(&mut buffer);
        }
        match key.underline {
            Underline::None => {}
            Underline::Single => draw_single(&mut buffer),
            Underline::Curly => draw_curly(&mut buffer),
            Underline::Dashed => draw_dashed(&mut buffer),
            Underline::Dotted => draw_dotted(&mut buffer),
            Underline::Double => draw_double(&mut buffer),
        }
        if key.strike_through {
            draw_strike(&mut buffer);
        }
        let sprite = self.atlas.allocate(&buffer)?;
        self.line_glyphs.insert(key, sprite.clone());
        Ok(sprite)
    }

    /// Figure out what we're going to draw for the underline.
    /// If the current cell is part of the current URL highlight
    /// then we want to show the underline.
    pub fn cached_line_sprite(
        &mut self,
        is_highlited_hyperlink: bool,
        is_strike_through: bool,
        underline: Underline,
        overline: bool,
        metrics: &RenderMetrics,
    ) -> anyhow::Result<Sprite> {
        let effective_underline = match (is_highlited_hyperlink, underline) {
            (true, Underline::None) => Underline::Single,
            (true, Underline::Single) => Underline::Double,
            (true, _) => Underline::Single,
            (false, u) => u,
        };

        let key = LineKey {
            strike_through: is_strike_through,
            overline,
            underline: effective_underline,
            size: metrics.into(),
        };

        if let Some(s) = self.line_glyphs.get(&key) {
            return Ok(s.clone());
        }

        self.line_sprite(key, metrics)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::termwindow::render::paint::AllowImage;
    use frankenterm_core::font_features::{AxisValue, VariableAxis};
    use window::bitmaps::TextureRect;

    #[test]
    fn bounded_atomic_reservation_preserves_limit_overflow_and_release_boundaries() {
        let counter = AtomicUsize::new(0);

        assert!(try_reserve_bounded_atomic(&counter, 2, 2));
        assert_eq!(counter.load(Ordering::Acquire), 2);
        assert!(
            !try_reserve_bounded_atomic(&counter, 1, 2),
            "a counter already at its limit must reject another reservation"
        );
        assert_eq!(counter.load(Ordering::Acquire), 2);
        assert!(
            !try_release_atomic(&counter, 3),
            "an underflowing release must fail without wrapping"
        );
        assert_eq!(counter.load(Ordering::Acquire), 2);
        assert!(try_release_atomic(&counter, 2));
        assert_eq!(counter.load(Ordering::Acquire), 0);

        assert!(try_reserve_bounded_atomic(&counter, 0, 0));
        assert!(try_release_atomic(&counter, 0));
        assert_eq!(counter.load(Ordering::Acquire), 0);

        counter.store(usize::MAX, Ordering::Release);
        assert!(
            !try_reserve_bounded_atomic(&counter, 1, usize::MAX),
            "checked addition must reject arithmetic overflow"
        );
        assert_eq!(counter.load(Ordering::Acquire), usize::MAX);
    }

    fn test_glyph_cache() -> (GlyphCache, RenderMetrics) {
        test_glyph_cache_with_atlas_size(128)
    }

    fn test_glyph_cache_with_atlas_size(size: usize) -> (GlyphCache, RenderMetrics) {
        config::use_test_configuration();

        let dpi = config::configuration()
            .dpi
            .unwrap_or_else(::window::default_dpi) as usize;
        let fonts = Rc::new(FontConfiguration::new(None, dpi).unwrap());
        let metrics = RenderMetrics::new(&fonts).unwrap();
        let cache = GlyphCache::new_in_memory(&fonts, size).unwrap();

        (cache, metrics)
    }

    #[test]
    fn config_changed_refreshes_frame_interval_from_explicit_config() {
        let (mut cache, _) = test_glyph_cache();
        cache.min_frame_duration = Duration::ZERO;
        let explicit_config = config::configuration();

        cache.config_changed(&explicit_config);

        assert_eq!(
            cache.min_frame_duration,
            config::frame_interval_for_max_fps(explicit_config.max_fps)
        );
        assert!(!cache.min_frame_duration.is_zero());
    }

    #[test]
    fn tier_swap_doctor_report_walks_live_glyph_cache_atlas() {
        let (cache, _) = test_glyph_cache_with_atlas_size(128);
        let report = cache.tier_swap_doctor_report();

        assert_eq!(report.aggregate.atlas_count, 1);
        assert_eq!(report.atlases.len(), 1);

        let row = &report.atlases[0];
        assert_eq!(row.label, "gui.glyph_cache.atlas");
        assert_eq!(row.stats.vram_peak_bytes, 128 * 128 * 4);
        assert_eq!(row.stats.host_ram_peak_bytes, 0);
        assert_eq!(
            row.vram_budget_bytes,
            Some(MemoryBudget::default().vram_budget_bytes)
        );
        assert_eq!(
            row.host_ram_budget_bytes,
            Some(MemoryBudget::default().host_ram_budget_bytes)
        );
    }

    // Repeating the same warmup plan must reuse its entries. This proves
    // idempotence; the separate zoom_warmup_experiment demand-key test checks
    // that painting can reuse those entries with unchanged metrics and pixels.
    #[test]
    fn warm_up_glyphs_is_idempotent_and_cache_hit_equivalent() {
        let (mut cache, metrics) = test_glyph_cache_with_atlas_size(1024);
        let plan = GlyphCache::default_warmup_plan();
        // Generous budget: the test must warm the full set, not time-slice it.
        let budget = Duration::from_secs(30);

        let first = cache.warm_up_glyphs(&plan, &metrics, budget);
        assert!(
            !first.budget_exhausted,
            "test budget must cover the full warm-up"
        );
        assert!(
            first.warmed_glyphs > 0,
            "first warm-up must rasterize at least the ASCII printable set"
        );
        let resolved_first = first.warmed_glyphs + first.cache_hits;
        let atlas_count_after_first = cache.tier_swap_doctor_report().aggregate.atlas_count;

        // Re-warm the identical plan: every previously-resolved glyph is now a
        // cache hit, nothing is re-rasterized, and the atlas does not grow.
        let second = cache.warm_up_glyphs(&plan, &metrics, budget);
        assert_eq!(
            second.warmed_glyphs, 0,
            "re-warming must add no new glyphs (warmed entries are reused, not re-rasterized)"
        );
        assert_eq!(
            second.cache_hits, resolved_first,
            "every previously-resolved glyph must resolve as a cache hit on re-warm"
        );
        assert_eq!(
            cache.tier_swap_doctor_report().aggregate.atlas_count,
            atlas_count_after_first,
            "re-warming an already-warm cache must not grow or rebuild the atlas"
        );
    }

    // ft-b4vw9 eviction isomorphism: evicting against the CURRENT CellMetricKey
    // removes nothing (no entry is stale), and evicting against a DIFFERENT
    // metric removes exactly the entries keyed by the now-stale metric — the
    // ones unreachable from new-scale lookups. Proves eviction touches only
    // other-scale entries, never the current scale's, so rendering is unchanged.
    // (Run verification deferred with the warm-up test: frankenterm-gui build is
    // currently blocked by a `window`-lib E0308 under `headless-render`.)
    #[test]
    fn evict_stale_cell_metrics_removes_only_other_scale_entries() {
        let (mut cache, metrics) = test_glyph_cache_with_atlas_size(1024);
        cache.warm_up_default_glyphs(&metrics, Duration::from_secs(30));
        let current: CellMetricKey = (&metrics).into();
        let populated = cache.glyph_cache.len();
        assert!(populated > 0, "warm-up must populate the glyph cache");

        // Same metric => nothing is stale; current-scale entries are retained.
        assert_eq!(cache.evict_stale_cell_metrics(current), 0);
        assert_eq!(
            cache.glyph_cache.len(),
            populated,
            "evicting against the current metric must retain every entry"
        );

        // A different metric => every current-scale entry is now stale.
        let other = CellMetricKey {
            pixel_width: current.pixel_width.wrapping_add(1),
            pixel_height: current.pixel_height,
        };
        let evicted = cache.evict_stale_cell_metrics(other);
        assert_eq!(
            evicted, populated,
            "all current-scale entries are stale relative to a different metric"
        );
        assert_eq!(cache.glyph_cache.len(), 0, "stale entries must be dropped");
    }

    fn texture_rect_tuple(rect: TextureRect) -> (f32, f32, f32, f32) {
        (
            rect.min_x(),
            rect.min_y(),
            rect.size.width,
            rect.size.height,
        )
    }

    #[test]
    fn glyph_feature_atlas_key_distinguishes_axis_values_and_formats() {
        let mut regular = AxisVector::new();
        regular.push(AxisValue::new(VariableAxis::Weight, 400.0).unwrap());

        let mut bold = AxisVector::new();
        bold.push(AxisValue::new(VariableAxis::Weight, 700.0).unwrap());

        assert_ne!(
            glyph_feature_atlas_key(17, 42, GlyphFormat::VariableMono, &regular),
            glyph_feature_atlas_key(17, 42, GlyphFormat::VariableMono, &bold)
        );

        let static_axes = AxisVector::new();
        assert_ne!(
            glyph_feature_atlas_key(17, 42, GlyphFormat::Monochrome, &static_axes),
            glyph_feature_atlas_key(17, 42, GlyphFormat::ColrCpal, &static_axes)
        );
        assert_ne!(
            glyph_feature_atlas_key(17, 42, GlyphFormat::ColrCpal, &static_axes),
            glyph_feature_atlas_key(17, 42, GlyphFormat::Sbix, &static_axes)
        );
    }

    #[test]
    fn borrowed_glyph_key_preserves_font_feature_atlas_key() {
        let style = TextStyle::default();
        let base = BorrowedGlyphKey {
            font_idx: 0,
            glyph_pos: 42,
            font_feature_atlas_key: 1,
            subpixel_bin: SubpixelBin::Quarter0,
            num_cells: 1,
            style: &style,
            followed_by_space: false,
            metric: CellMetricKey {
                pixel_width: 8,
                pixel_height: 16,
            },
            id: 9,
        };

        let owned = base.to_owned();
        assert_eq!(owned.font_feature_atlas_key, 1);
        assert_eq!(owned.key(), base);

        let mut other = base;
        other.font_feature_atlas_key = 2;
        assert_ne!(base, other);
    }

    #[test]
    fn borrowed_glyph_key_distinguishes_subpixel_bins() {
        let style = TextStyle::default();
        let base = BorrowedGlyphKey {
            font_idx: 0,
            glyph_pos: 42,
            font_feature_atlas_key: 1,
            subpixel_bin: SubpixelBin::Quarter0,
            num_cells: 1,
            style: &style,
            followed_by_space: false,
            metric: CellMetricKey {
                pixel_width: 8,
                pixel_height: 16,
            },
            id: 9,
        };

        let owned = base.to_owned();
        assert_eq!(owned.subpixel_bin, SubpixelBin::Quarter0);
        assert_eq!(owned.key(), base);

        let mut shifted = base;
        shifted.subpixel_bin = SubpixelBin::Quarter2;
        assert_ne!(base, shifted);
    }

    #[test]
    fn default_warmup_plan_prioritizes_ascii_before_ligatures_and_icons() {
        let plan = GlyphCache::default_warmup_plan();

        assert_eq!(plan[0].text, " ");
        assert_eq!(plan[0].priority, GlyphWarmupPriority::AsciiPrintable);
        assert_eq!(plan[94].text, "~");
        assert_eq!(plan[94].priority, GlyphWarmupPriority::AsciiPrintable);
        assert_eq!(plan[95].priority, GlyphWarmupPriority::CommonLigature);
        assert_eq!(
            plan.last().unwrap().priority,
            GlyphWarmupPriority::NerdFontIcon
        );
    }

    #[test]
    fn zero_budget_warmup_does_not_touch_cache() {
        let (mut cache, metrics) = test_glyph_cache();
        let plan = vec![GlyphWarmupRequest::new(
            "A",
            GlyphWarmupPriority::AsciiPrintable,
        )];

        let stats = cache.warm_up_glyphs(&plan, &metrics, Duration::ZERO);

        assert_eq!(stats.attempted_requests, 0);
        assert_eq!(stats.warmed_glyphs, 0);
        assert_eq!(stats.cache_hits, 0);
        assert!(stats.budget_exhausted);
        assert!(cache.glyph_cache.is_empty());
    }

    #[test]
    fn warmup_caches_ascii_glyph_and_second_pass_hits_cache() {
        let (mut cache, metrics) = test_glyph_cache();
        let plan = vec![GlyphWarmupRequest::new(
            "A",
            GlyphWarmupPriority::AsciiPrintable,
        )];

        let first = cache.warm_up_glyphs(&plan, &metrics, Duration::from_secs(5));
        let second = cache.warm_up_glyphs(&plan, &metrics, Duration::from_secs(5));

        assert_eq!(first.attempted_requests, 1);
        assert!(first.warmed_glyphs > 0);
        assert_eq!(first.failed_glyphs, 0);
        assert_eq!(second.attempted_requests, 1);
        assert!(second.cache_hits > 0);
        assert_eq!(second.failed_glyphs, 0);
    }

    // Preparation for 4tenz.8.7: the actual CPU font/raster/atlas path, using
    // ImageTexture instead of a GPU. This deliberately does not call TermWindow,
    // open a window, or claim first-paint/presentation timing. The production
    // 16ms warmup default is unchanged. Only sprite-local bytes are comparable:
    // warmup may legitimately change atlas packing and sprite coordinates.
    mod zoom_warmup_experiment {
        use super::*;

        const BUDGETS_MS: [u64; 4] = [0, 2, 4, 16];
        const PHASES: [(&str, f64); 4] = [
            ("cold_glyph_cache", 1.25),
            ("warm_same_metrics", 1.25),
            ("scale_down", 0.875),
            ("scale_reverse", 1.25),
        ];
        // Built-in JetBrains Mono and Noto Color Emoji provide these scripts.
        // No operator font lookup is permitted. CJK and RTL are not qualified
        // by this corpus; they need additional pinned fixture font faces.
        const DEMANDS: [&str; 5] = ["A fi != ", "e\u{301}", "αβγ", "ЖЯ", "🙂"];

        #[derive(Clone, Debug, PartialEq, Eq)]
        struct GlyphWitness {
            face: String,
            glyph_pos: u32,
            cluster: u32,
            num_cells: u8,
            shaping_bits: [u64; 4],
            raster_bits: [u64; 6],
            brightness_bits: u32,
            has_color: bool,
            bitmap: Option<(usize, usize, Vec<u8>)>,
        }

        #[derive(Debug, PartialEq, Eq)]
        enum OracleError {
            MissingGlyph,
            MissingBitmap,
            StaleOrDifferentOutput,
        }

        struct PhaseSample {
            metric: CellMetricKey,
            glyphs: Vec<GlyphWitness>,
            warmup: GlyphWarmupStats,
            transition_ns: u128,
            warmup_wall_ns: u128,
            demand_shape_ns: u128,
            demand_raster_ns: u128,
            cpu_path_wall_ns: u128,
            demand_hits: usize,
            demand_misses: usize,
            atlas_version_delta: u64,
            resident_glyphs: usize,
        }

        fn fixture() -> (GlyphCache, RenderMetrics) {
            static LOGGING: std::sync::Once = std::sync::Once::new();
            LOGGING.call_once(|| {
                if let Err(err) = env_logger::Builder::from_env(
                    env_logger::Env::default().default_filter_or("warn"),
                )
                .is_test(true)
                .try_init()
                {
                    eprintln!("zoom diagnostic retains the existing logger: {err}");
                }
            });
            let (cache, metrics) = test_glyph_cache_with_atlas_size(2048);
            assert_eq!(
                cache.fonts.config().font_locator,
                config::FontLocatorSelection::ConfigDirsOnly,
                "the diagnostic must never use operator font discovery"
            );
            (cache, metrics)
        }

        fn witness(
            font: &LoadedFont,
            info: &GlyphInfo,
            glyph: &CachedGlyph,
        ) -> Result<GlyphWitness, OracleError> {
            if info.glyph_pos == 0 {
                return Err(OracleError::MissingGlyph);
            }
            let bitmap = if let Some(sprite) = &glyph.texture {
                let width = usize::try_from(sprite.coords.size.width).unwrap();
                let height = usize::try_from(sprite.coords.size.height).unwrap();
                let mut image = Image::new(width, height);
                sprite.texture.read(sprite.coords, &mut image).unwrap();
                let pixels = image.pixel_data_slice().to_vec();
                if !info.is_space && !pixels.chunks_exact(4).any(|pixel| pixel[3] != 0) {
                    return Err(OracleError::MissingBitmap);
                }
                Some((width, height, pixels))
            } else if info.is_space {
                None
            } else {
                return Err(OracleError::MissingBitmap);
            };
            let handles = font.clone_handles();
            Ok(GlyphWitness {
                face: handles[info.font_idx].names().full_name.clone(),
                glyph_pos: info.glyph_pos,
                cluster: info.cluster,
                num_cells: info.num_cells,
                shaping_bits: [
                    info.x_advance.get().to_bits(),
                    info.y_advance.get().to_bits(),
                    info.x_offset.get().to_bits(),
                    info.y_offset.get().to_bits(),
                ],
                raster_bits: [
                    glyph.x_advance.get().to_bits(),
                    glyph.x_offset.get().to_bits(),
                    glyph.y_offset.get().to_bits(),
                    glyph.bearing_x.get().to_bits(),
                    glyph.bearing_y.get().to_bits(),
                    glyph.scale.to_bits(),
                ],
                brightness_bits: glyph.brightness_adjust.to_bits(),
                has_color: glyph.has_color,
                bitmap,
            })
        }

        fn compare(expected: &[GlyphWitness], actual: &[GlyphWitness]) -> Result<(), OracleError> {
            if expected == actual && !actual.is_empty() {
                Ok(())
            } else {
                Err(OracleError::StaleOrDifferentOutput)
            }
        }

        #[test]
        fn warmup_populates_paint_keys_without_surplus_or_lazy_misses() {
            fn keys(cache: &GlyphCache) -> std::collections::HashSet<GlyphKey> {
                cache.glyph_cache.keys().cloned().collect()
            }

            for text in ["ab ", "e\u{301}", "ffi"] {
                let (mut cache, metrics) = fixture();
                let style = TextStyle::default();
                let font = cache.fonts.resolve_font(&style).unwrap();
                let infos = font
                    .blocking_shape(text, None, Direction::LeftToRight, None, None)
                    .unwrap();
                assert!(!infos.is_empty());
                assert!(infos.iter().all(|info| info.glyph_pos != 0));
                let request_cells = u8::try_from(text.chars().count()).unwrap();
                let different_spans = infos.iter().any(|info| info.num_cells != request_cells);
                let has_following_space = infos.iter().skip(1).any(|info| info.is_space);
                if text == "ab " {
                    assert!(different_spans && has_following_space);
                } else if text == "e\u{301}" {
                    assert!(different_spans);
                    assert_eq!(
                        infos
                            .iter()
                            .map(|info| u32::from(info.num_cells))
                            .sum::<u32>(),
                        1
                    );
                }

                let plan = [GlyphWarmupRequest::new(
                    text,
                    GlyphWarmupPriority::CommonLigature,
                )];
                let stats = cache.warm_up_glyphs(&plan, &metrics, Duration::from_secs(30));
                assert!(!stats.budget_exhausted);
                assert_eq!(stats.failed_glyphs, 0);
                assert_eq!(stats.attempted_requests, 1);
                assert_eq!(stats.warmed_glyphs + stats.cache_hits, infos.len());

                // The reference has its own glyph cache and atlas, sharing the
                // font configuration and loaded-font identity. Populate it
                // through actual paint keys, independently of warmup.
                let mut reference = GlyphCache::new_in_memory(&cache.fonts, 2048).unwrap();
                let mut expected = Vec::new();
                for (index, info) in infos.iter().enumerate() {
                    let followed_by_space = infos.get(index + 1).is_some_and(|next| next.is_space);
                    let glyph = reference
                        .cached_glyph(
                            info,
                            &style,
                            followed_by_space,
                            &font,
                            &metrics,
                            info.num_cells,
                        )
                        .unwrap();
                    expected.push(witness(&font, info, &glyph).unwrap());
                }
                let expected_keys = keys(&reference);
                // Check before demand can fill missing keys or hide surplus ones.
                assert_eq!(keys(&cache), expected_keys, "warmup keys for {text:?}");
                let warmed_len = cache.glyph_cache.len();
                let warmed_atlas_version = cache.atlas.version();
                let mut actual = Vec::new();
                for (index, info) in infos.iter().enumerate() {
                    let followed_by_space = infos.get(index + 1).is_some_and(|next| next.is_space);
                    let (glyph, hit) = cache
                        .cached_glyph_for_subpixel_bin_with_status(
                            info,
                            &style,
                            followed_by_space,
                            &font,
                            &metrics,
                            info.num_cells,
                            SubpixelBin::Quarter0,
                        )
                        .unwrap();
                    assert!(hit, "paint must reuse the warmed glyph for {text:?}");
                    actual.push(witness(&font, info, &glyph).unwrap());
                }
                assert_eq!(compare(&expected, &actual), Ok(()));
                assert_eq!(cache.glyph_cache.len(), warmed_len);
                assert_eq!(cache.atlas.version(), warmed_atlas_version);

                // Independently plant each old key defect. The same key-set
                // oracle must reject it even if lazy painting could repair it.
                for (wrong_spans, wrong_spacing) in [(true, false), (false, true)] {
                    if (wrong_spans && !different_spans) || (wrong_spacing && !has_following_space)
                    {
                        continue;
                    }
                    let mut planted = GlyphCache::new_in_memory(&cache.fonts, 2048).unwrap();
                    for (index, info) in infos.iter().enumerate() {
                        let followed_by_space = !wrong_spacing
                            && infos.get(index + 1).is_some_and(|next| next.is_space);
                        let num_cells = if wrong_spans {
                            request_cells
                        } else {
                            info.num_cells
                        };
                        planted
                            .cached_glyph(
                                info,
                                &style,
                                followed_by_space,
                                &font,
                                &metrics,
                                num_cells,
                            )
                            .unwrap();
                    }
                    assert_ne!(
                        keys(&planted),
                        expected_keys,
                        "old keys must fail for {text:?}"
                    );
                }
            }
        }

        fn phase(cache: &mut GlyphCache, scale: f64, budget_ms: u64) -> PhaseSample {
            let started = Instant::now();
            if cache.fonts.get_font_scale().to_bits() != scale.to_bits() {
                cache.fonts.change_scaling(scale, 96);
            }
            let metrics = RenderMetrics::new(&cache.fonts).unwrap();
            let metric = CellMetricKey::from(&metrics);
            cache.evict_stale_cell_metrics(metric);
            let transition_ns = started.elapsed().as_nanos();
            let atlas_before = cache.atlas.version();
            let warmup_started = Instant::now();
            let warmup = cache.warm_up_default_glyphs(&metrics, Duration::from_millis(budget_ms));
            // The public stats start after default-plan allocation, so retain
            // the complete call cost separately, including the zero-budget case.
            let warmup_wall_ns = warmup_started.elapsed().as_nanos();
            let style = TextStyle::default();
            let shape_started = Instant::now();
            let font = cache.fonts.resolve_font(&style).unwrap();
            let runs: Vec<_> = DEMANDS
                .iter()
                .map(|text| {
                    let infos = font
                        .blocking_shape(text, None, Direction::LeftToRight, None, None)
                        .unwrap();
                    assert!(!infos.is_empty(), "demanded text must produce glyphs");
                    assert!(
                        infos.iter().all(|info| info.glyph_pos != 0),
                        "missing fixture glyph: .notdef must not count as parity"
                    );
                    infos
                })
                .collect();
            let demand_shape_ns = shape_started.elapsed().as_nanos();
            let raster_started = Instant::now();
            let mut resolved = Vec::new();
            let mut demand_hits = 0;
            let mut demand_misses = 0;
            for infos in &runs {
                for (index, info) in infos.iter().enumerate() {
                    // Match glyph_infos_to_glyphs: use the shaped cell span and
                    // the following glyph's spacing, not text character count.
                    let followed_by_space = infos.get(index + 1).is_some_and(|next| next.is_space);
                    let (glyph, hit) = cache
                        .cached_glyph_for_subpixel_bin_with_status(
                            info,
                            &style,
                            followed_by_space,
                            &font,
                            &metrics,
                            info.num_cells,
                            SubpixelBin::Quarter0,
                        )
                        .unwrap();
                    demand_hits += usize::from(hit);
                    demand_misses += usize::from(!hit);
                    resolved.push((info, glyph));
                }
            }
            let demand_raster_ns = raster_started.elapsed().as_nanos();
            let cpu_path_wall_ns = started.elapsed().as_nanos();
            // Readback and comparison are deliberately outside the timed path.
            let glyphs = resolved
                .iter()
                .map(|(info, glyph)| witness(&font, info, glyph).unwrap())
                .collect();
            PhaseSample {
                metric,
                glyphs,
                warmup,
                transition_ns,
                warmup_wall_ns,
                demand_shape_ns,
                demand_raster_ns,
                cpu_path_wall_ns,
                demand_hits,
                demand_misses,
                atlas_version_delta: cache.atlas.version() - atlas_before,
                resident_glyphs: cache.glyph_cache.len(),
            }
        }

        fn trial(budget_ms: u64) -> Vec<PhaseSample> {
            let (mut cache, _) = fixture();
            let samples: Vec<_> = PHASES
                .iter()
                .map(|(_, scale)| phase(&mut cache, *scale, budget_ms))
                .collect();
            assert_eq!(samples[0].metric, samples[1].metric);
            assert_ne!(samples[1].metric, samples[2].metric);
            assert_eq!(samples[0].metric, samples[3].metric);
            assert_eq!(samples[1].demand_misses, 0);
            assert_eq!(samples[1].demand_hits, samples[1].glyphs.len());
            assert_eq!(compare(&samples[0].glyphs, &samples[1].glyphs), Ok(()));
            assert_eq!(compare(&samples[0].glyphs, &samples[3].glyphs), Ok(()));
            if budget_ms == 0 {
                for sample in &samples {
                    assert_eq!(sample.warmup.attempted_requests, 0);
                    assert_eq!(sample.warmup.warmed_glyphs, 0);
                }
                assert!(samples[0].demand_misses > 0);
                assert!(samples[2].demand_misses > 0);
                assert!(samples[3].demand_misses > 0);
            }
            samples
        }

        #[test]
        fn budgets_preserve_demanded_pixels_across_scale_changes() {
            let baseline = trial(0);
            for budget_ms in BUDGETS_MS.into_iter().skip(1) {
                for (expected, actual) in baseline.iter().zip(trial(budget_ms)) {
                    assert_eq!(expected.metric, actual.metric);
                    assert_eq!(compare(&expected.glyphs, &actual.glyphs), Ok(()));
                }
            }
        }

        #[test]
        fn oracle_rejects_stale_metric_and_missing_glyph_cache_entries() {
            let (mut cache, old_metrics) = fixture();
            let style = TextStyle::default();
            let old_font = cache.fonts.resolve_font(&style).unwrap();
            let old_info = old_font
                .blocking_shape("A", None, Direction::LeftToRight, None, None)
                .unwrap()
                .remove(0);
            let stale = cache
                .cached_glyph(&old_info, &style, false, &old_font, &old_metrics, 1)
                .unwrap();
            cache.fonts.change_scaling(1.5, 96);
            let metrics = RenderMetrics::new(&cache.fonts).unwrap();
            assert_ne!(
                CellMetricKey::from(&old_metrics),
                CellMetricKey::from(&metrics)
            );
            cache.evict_stale_cell_metrics((&metrics).into());
            let font = cache.fonts.resolve_font(&style).unwrap();
            let info = font
                .blocking_shape("A", None, Direction::LeftToRight, None, None)
                .unwrap()
                .remove(0);
            let correct = cache
                .cached_glyph(&info, &style, false, &font, &metrics, 1)
                .unwrap();
            let expected = witness(&font, &info, &correct).unwrap();
            assert_eq!(compare(&[expected.clone()], &[expected.clone()]), Ok(()));
            let key = cache
                .glyph_cache
                .iter()
                .find(|(_, value)| Rc::ptr_eq(value, &correct))
                .map(|(key, _)| key.clone())
                .unwrap();

            // Plant old-scale pixels under a current, production-generated key.
            // The real lookup must hit the canary; the output oracle must fail.
            cache.glyph_cache.insert(key.clone(), stale);
            let (corrupt, hit) = cache
                .cached_glyph_for_subpixel_bin_with_status(
                    &info,
                    &style,
                    false,
                    &font,
                    &metrics,
                    1,
                    SubpixelBin::Quarter0,
                )
                .unwrap();
            assert!(hit);
            assert_eq!(
                compare(&[expected], &[witness(&font, &info, &corrupt).unwrap()]),
                Err(OracleError::StaleOrDifferentOutput)
            );

            cache.glyph_cache.insert(
                key,
                Rc::new(CachedGlyph {
                    texture: None,
                    has_color: correct.has_color,
                    brightness_adjust: correct.brightness_adjust,
                    x_offset: correct.x_offset,
                    y_offset: correct.y_offset,
                    x_advance: correct.x_advance,
                    bearing_x: correct.bearing_x,
                    bearing_y: correct.bearing_y,
                    scale: correct.scale,
                }),
            );
            let missing = cache
                .cached_glyph(&info, &style, false, &font, &metrics, 1)
                .unwrap();
            assert_eq!(
                witness(&font, &info, &missing),
                Err(OracleError::MissingBitmap)
            );
            let mut notdef = info.clone();
            notdef.glyph_pos = 0;
            assert_eq!(
                witness(&font, &notdef, &correct),
                Err(OracleError::MissingGlyph)
            );
        }

        #[test]
        #[ignore = "CPU GlyphCache diagnostic only; run exact filter with --ignored --nocapture"]
        fn paired_budget_diagnostic() {
            let baseline = trial(0);
            for round in 0..10 {
                // Rotate ordering to avoid making one budget the universal
                // cold-start or final sample. Cold here means glyph cache, not
                // OS/font-database/process coldness or a native cold launch.
                for offset in 0..BUDGETS_MS.len() {
                    let budget_ms = BUDGETS_MS[(round + offset) % BUDGETS_MS.len()];
                    for (phase_index, sample) in trial(budget_ms).into_iter().enumerate() {
                        assert_eq!(
                            compare(&baseline[phase_index].glyphs, &sample.glyphs),
                            Ok(())
                        );
                        println!(
                            "ZOOM_WARMUP_CPU_SAMPLE {}",
                            serde_json::json!({
                                "claim_scope": "cpu_glyphcache_diagnostic_only",
                                "round": round,
                                "phase": PHASES[phase_index].0,
                                "scale": PHASES[phase_index].1,
                                "budget_ms": budget_ms,
                                "dpi": 96,
                                "cell_width": sample.metric.pixel_width,
                                "cell_height": sample.metric.pixel_height,
                                "transition_ns": sample.transition_ns,
                                "warmup_wall_ns": sample.warmup_wall_ns,
                                "warmup_inner_ns": sample.warmup.elapsed.as_nanos(),
                                "demand_shape_ns": sample.demand_shape_ns,
                                "demand_raster_ns": sample.demand_raster_ns,
                                "cpu_path_wall_ns": sample.cpu_path_wall_ns,
                                "warmup_requests": sample.warmup.attempted_requests,
                                "warmed_glyphs": sample.warmup.warmed_glyphs,
                                "warmup_hits": sample.warmup.cache_hits,
                                "warmup_failures": sample.warmup.failed_glyphs,
                                "budget_exhausted": sample.warmup.budget_exhausted,
                                "demand_hits": sample.demand_hits,
                                "demand_misses": sample.demand_misses,
                                "atlas_version_delta": sample.atlas_version_delta,
                                "resident_glyphs": sample.resident_glyphs,
                                "glyph_count": sample.glyphs.len(),
                                "byte_and_metric_equivalence": true,
                                "texture_backend": "ImageTexture_cpu",
                                "os": std::env::consts::OS,
                                "arch": std::env::consts::ARCH,
                                "resolved_faces": sample.glyphs.iter()
                                    .map(|glyph| glyph.face.as_str())
                                    .collect::<std::collections::BTreeSet<_>>(),
                                "unproven_corpus": ["cjk", "rtl"],
                                "presented_frames": 0,
                            })
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn cached_subpixel_glyph_bins_materializes_four_cache_variants() {
        let (mut cache, metrics) = test_glyph_cache();
        let style = TextStyle::default();
        let font = cache.fonts.resolve_font(&style).unwrap();
        let glyphs = font
            .blocking_shape("A", None, Direction::LeftToRight, None, None)
            .unwrap();
        let info = glyphs.first().unwrap();

        let first = cache
            .cached_subpixel_glyph_bins(info, &style, false, &font, &metrics, 1)
            .unwrap();
        let second = cache
            .cached_subpixel_glyph_bins(info, &style, false, &font, &metrics, 1)
            .unwrap();

        assert_eq!(first.len(), SubpixelBin::all().len());
        assert_eq!(cache.glyph_cache.len(), SubpixelBin::all().len());
        for (a, b) in first.iter().zip(second.iter()) {
            assert!(Rc::ptr_eq(a, b));
        }
    }

    #[test]
    fn cached_color_reuses_sprite_for_identical_rgba() {
        let (mut cache, _) = test_glyph_cache();

        let first = cache
            .cached_color(RgbColor::new_8bpc(0x12, 0x34, 0x56), 0.5)
            .unwrap();
        let second = cache
            .cached_color(RgbColor::new_8bpc(0x12, 0x34, 0x56), 0.5)
            .unwrap();

        assert_eq!(cache.color.len(), 1);
        assert_eq!(first.coords, second.coords);
        assert!(Rc::ptr_eq(&first.texture, &second.texture));
    }

    #[test]
    fn cached_color_distinguishes_alpha_variants() {
        let (mut cache, _) = test_glyph_cache();

        let opaque = cache
            .cached_color(RgbColor::new_8bpc(0xaa, 0xbb, 0xcc), 1.0)
            .unwrap();
        let translucent = cache
            .cached_color(RgbColor::new_8bpc(0xaa, 0xbb, 0xcc), 0.25)
            .unwrap();

        assert_eq!(cache.color.len(), 2);
        assert_ne!(opaque.coords, translucent.coords);
    }

    #[test]
    fn cached_line_sprite_promotes_hyperlink_without_underline_to_single() {
        let (mut cache, metrics) = test_glyph_cache();

        let hyperlink = cache
            .cached_line_sprite(true, false, Underline::None, false, &metrics)
            .unwrap();
        let plain_single = cache
            .cached_line_sprite(false, false, Underline::Single, false, &metrics)
            .unwrap();

        assert_eq!(cache.line_glyphs.len(), 1);
        assert_eq!(hyperlink.coords, plain_single.coords);
        assert!(Rc::ptr_eq(&hyperlink.texture, &plain_single.texture));
    }

    #[test]
    fn cached_line_sprite_promotes_hyperlink_single_to_double() {
        let (mut cache, metrics) = test_glyph_cache();

        let hyperlink = cache
            .cached_line_sprite(true, false, Underline::Single, false, &metrics)
            .unwrap();
        let plain_double = cache
            .cached_line_sprite(false, false, Underline::Double, false, &metrics)
            .unwrap();

        assert_eq!(cache.line_glyphs.len(), 1);
        assert_eq!(hyperlink.coords, plain_double.coords);
        assert!(Rc::ptr_eq(&hyperlink.texture, &plain_double.texture));
    }

    #[test]
    fn cached_block_reuses_sprite_for_same_shape_and_metrics() {
        let (mut cache, metrics) = test_glyph_cache();

        let first = cache
            .cached_block(BlockKey::CellDiagonals(CellDiagonal::UPPER_LEFT), &metrics)
            .unwrap();
        let second = cache
            .cached_block(BlockKey::CellDiagonals(CellDiagonal::UPPER_LEFT), &metrics)
            .unwrap();

        assert_eq!(cache.block_glyphs.len(), 1);
        assert_eq!(first.coords, second.coords);
        assert!(Rc::ptr_eq(&first.texture, &second.texture));
    }

    #[test]
    fn test_atlas_rect_lookup_returns_expected_uv_tuple_for_first_block_glyph() {
        let (mut cache, metrics) = test_glyph_cache();

        let sprite = cache
            .cached_block(BlockKey::CellDiagonals(CellDiagonal::UPPER_LEFT), &metrics)
            .unwrap();

        assert_eq!(sprite.coords.origin.x, 1);
        assert_eq!(sprite.coords.origin.y, 1);
        assert_eq!(sprite.coords.size.width, metrics.cell_size.width);
        assert_eq!(sprite.coords.size.height, metrics.cell_size.height);
        assert_eq!(
            texture_rect_tuple(sprite.texture_coords()),
            (
                1.0 / 128.0,
                1.0 / 128.0,
                metrics.cell_size.width as f32 / 128.0,
                metrics.cell_size.height as f32 / 128.0,
            )
        );
    }

    #[test]
    fn atlas_rect_lookup_uv_coords_stay_within_unit_square() {
        // The renderer samples the atlas via UV in [0, 1]. A glyph whose
        // texture_coords() exit that range would either wrap or sample
        // garbage — visible as glitched glyphs at frame edges.
        let (mut cache, metrics) = test_glyph_cache();
        let sprite = cache
            .cached_block(BlockKey::CellDiagonals(CellDiagonal::UPPER_LEFT), &metrics)
            .unwrap();

        let rect = sprite.texture_coords();
        let (min_x, min_y, w, h) = texture_rect_tuple(rect);
        assert!(min_x >= 0.0 && min_x < 1.0, "min_x out of range: {min_x}");
        assert!(min_y >= 0.0 && min_y < 1.0, "min_y out of range: {min_y}");
        assert!(w > 0.0, "width must be positive, got {w}");
        assert!(h > 0.0, "height must be positive, got {h}");
        assert!(
            min_x + w <= 1.0 + f32::EPSILON,
            "max_x exceeds 1: {}",
            min_x + w
        );
        assert!(
            min_y + h <= 1.0 + f32::EPSILON,
            "max_y exceeds 1: {}",
            min_y + h
        );
    }

    #[test]
    fn atlas_rect_lookup_repeated_glyph_returns_same_rect() {
        // Looking up the same glyph twice must yield the same atlas rect
        // and the same texture handle. This is the memoization contract
        // the per-frame quad allocator depends on; if it ever broke we'd
        // burn atlas space duplicating glyphs and start evicting earlier.
        let (mut cache, metrics) = test_glyph_cache();
        let first = cache
            .cached_block(BlockKey::CellDiagonals(CellDiagonal::UPPER_LEFT), &metrics)
            .unwrap();
        let second = cache
            .cached_block(BlockKey::CellDiagonals(CellDiagonal::UPPER_LEFT), &metrics)
            .unwrap();

        assert_eq!(first.coords, second.coords);
        assert_eq!(
            texture_rect_tuple(first.texture_coords()),
            texture_rect_tuple(second.texture_coords())
        );
    }

    #[test]
    fn atlas_rect_lookup_distinct_glyphs_get_distinct_rects() {
        // Two different cached entries must occupy different atlas rects.
        // If they ever collide we'd render glyph A's pixels in glyph B's
        // cell. Use two distinct cached_color entries (cheap to allocate
        // and exercise the same Atlas allocator that backs glyphs).
        let (mut cache, _metrics) = test_glyph_cache();
        let a = cache
            .cached_color(RgbColor::new_8bpc(0x10, 0x20, 0x30), 1.0)
            .unwrap();
        let b = cache
            .cached_color(RgbColor::new_8bpc(0xa0, 0xb0, 0xc0), 1.0)
            .unwrap();

        assert_ne!(a.coords, b.coords, "distinct keys must get distinct rects");
        assert_ne!(
            texture_rect_tuple(a.texture_coords()),
            texture_rect_tuple(b.texture_coords()),
            "distinct keys must produce distinct UV tuples"
        );
    }

    #[test]
    fn atlas_rect_lookup_uv_scales_inversely_with_atlas_size() {
        // The same conceptual glyph stored in a 64-wide atlas vs. a 256-wide
        // atlas must report UV coordinates that scale by the atlas size
        // ratio: pixel_offset / atlas_size. In practice the same source
        // glyph at the same metrics should land at origin (1, 1) in both
        // atlases (the Atlas leaves a 1-pixel border for filtering), so the
        // 64-atlas UV is 4× the 256-atlas UV at the origin.
        let (mut cache_small, metrics_small) = test_glyph_cache_with_atlas_size(64);
        let (mut cache_large, metrics_large) = test_glyph_cache_with_atlas_size(256);

        let sprite_small = cache_small
            .cached_block(
                BlockKey::CellDiagonals(CellDiagonal::UPPER_LEFT),
                &metrics_small,
            )
            .unwrap();
        let sprite_large = cache_large
            .cached_block(
                BlockKey::CellDiagonals(CellDiagonal::UPPER_LEFT),
                &metrics_large,
            )
            .unwrap();

        // Same pixel origin in both atlases (1, 1 — atlas leaves border).
        assert_eq!(sprite_small.coords.origin.x, 1);
        assert_eq!(sprite_small.coords.origin.y, 1);
        assert_eq!(sprite_large.coords.origin.x, 1);
        assert_eq!(sprite_large.coords.origin.y, 1);

        let (small_x, small_y, _, _) = texture_rect_tuple(sprite_small.texture_coords());
        let (large_x, large_y, _, _) = texture_rect_tuple(sprite_large.texture_coords());

        // small: 1/64 = 0.015625; large: 1/256 = 0.00390625; ratio = 4.0
        assert!(
            (small_x / large_x - 4.0).abs() < 1e-5,
            "UV x ratio {}",
            small_x / large_x
        );
        assert!(
            (small_y / large_y - 4.0).abs() < 1e-5,
            "UV y ratio {}",
            small_y / large_y
        );
    }

    #[test]
    fn atlas_pressure_reports_growth_without_evicting_oldest_cached_color() {
        let (mut cache, _) = test_glyph_cache_with_atlas_size(8);
        let first = cache
            .cached_color(RgbColor::new_8bpc(0x12, 0x34, 0x56), 0.5)
            .unwrap();

        cache
            .cached_color(RgbColor::new_8bpc(0x65, 0x43, 0x21), 0.5)
            .unwrap();
        cache
            .cached_color(RgbColor::new_8bpc(0xaa, 0xbb, 0xcc), 0.5)
            .unwrap();
        cache
            .cached_color(RgbColor::new_8bpc(0xde, 0xad, 0xbe), 0.5)
            .unwrap();

        let err = cache
            .cached_color(RgbColor::new_8bpc(0xfe, 0xed, 0xfa), 0.5)
            .unwrap_err();
        let atlas_err = err
            .downcast_ref::<OutOfTextureSpace>()
            .expect("atlas should report growth instead of evicting an entry");

        assert_eq!(atlas_err.current_size, 8);
        assert_eq!(atlas_err.size, Some(16));
        assert_eq!(cache.color.len(), 4);

        let first_again = cache
            .cached_color(RgbColor::new_8bpc(0x12, 0x34, 0x56), 0.5)
            .unwrap();
        assert_eq!(first_again.coords, first.coords);
    }

    #[test]
    fn atlas_eviction_returns_out_of_texture_space_with_grow_hint() {
        // Contract: when the atlas can't fit another sprite it MUST surface
        // OutOfTextureSpace with a non-None grow hint, not silently evict
        // and overwrite. The renderer relies on the grow hint to pick the
        // recreated atlas size; if that hint stops being populated the
        // outer GlyphCache recreate path can't decide how big to grow.
        let (mut cache, _) = test_glyph_cache_with_atlas_size(8);
        for (r, g, b) in [
            (0x12, 0x34, 0x56),
            (0x65, 0x43, 0x21),
            (0xaa, 0xbb, 0xcc),
            (0xde, 0xad, 0xbe),
        ] {
            cache
                .cached_color(RgbColor::new_8bpc(r, g, b), 0.5)
                .unwrap();
        }

        let err = cache
            .cached_color(RgbColor::new_8bpc(0xfe, 0xed, 0xfa), 0.5)
            .unwrap_err();
        let atlas_err = err
            .downcast_ref::<OutOfTextureSpace>()
            .expect("full atlas must surface OutOfTextureSpace");

        assert_eq!(
            atlas_err.current_size, 8,
            "current_size reports the size we filled"
        );
        let grow = atlas_err
            .size
            .expect("grow hint must be Some — caller relies on it");
        assert!(
            grow > atlas_err.current_size,
            "grow hint {grow} must exceed current_size 8"
        );
    }

    #[test]
    fn atlas_eviction_does_not_evict_oldest_entry_when_full() {
        // Contract: at Atlas pressure NO previously-cached entry is silently
        // dropped or relocated. This is the inverse of the LRU contract the
        // bead description assumed — the system was deliberately built
        // *without* per-entry LRU eviction (see glyphcache.rs:568 comment:
        // "eviction is managed by recreating Self when the Atlas is filled").
        // The test fills the atlas, triggers OutOfTextureSpace, then proves
        // every pre-existing entry still resolves to its original UV.
        let (mut cache, _) = test_glyph_cache_with_atlas_size(8);

        let pre_pressure_keys = [
            (RgbColor::new_8bpc(0x12, 0x34, 0x56), 0.5),
            (RgbColor::new_8bpc(0x65, 0x43, 0x21), 0.5),
            (RgbColor::new_8bpc(0xaa, 0xbb, 0xcc), 0.5),
            (RgbColor::new_8bpc(0xde, 0xad, 0xbe), 0.5),
        ];
        let original_coords: Vec<_> = pre_pressure_keys
            .iter()
            .map(|(color, alpha)| {
                let sprite = cache.cached_color(*color, *alpha).unwrap();
                (sprite.coords, texture_rect_tuple(sprite.texture_coords()))
            })
            .collect();

        // Trigger pressure.
        let err = cache
            .cached_color(RgbColor::new_8bpc(0xfe, 0xed, 0xfa), 0.5)
            .unwrap_err();
        assert!(err.downcast_ref::<OutOfTextureSpace>().is_some());

        // Every pre-existing entry must still resolve to the same coords.
        for ((color, alpha), (orig_coords, orig_uv)) in
            pre_pressure_keys.iter().zip(original_coords.iter())
        {
            let again = cache.cached_color(*color, *alpha).unwrap();
            assert_eq!(
                again.coords, *orig_coords,
                "atlas pressure must not relocate previously cached sprite for {color:?}"
            );
            assert_eq!(
                texture_rect_tuple(again.texture_coords()),
                *orig_uv,
                "UV for previously cached sprite changed after pressure for {color:?}"
            );
        }

        // And the cache len didn't shrink — no entry was silently dropped.
        assert_eq!(cache.color.len(), pre_pressure_keys.len());
    }

    #[test]
    fn test_atlas_eviction_lru_contract_is_recreate_not_per_entry_eviction() {
        let (mut cache, _) = test_glyph_cache_with_atlas_size(8);
        let oldest_color = RgbColor::new_8bpc(0x10, 0x20, 0x30);
        let newest_color = RgbColor::new_8bpc(0xd0, 0xe0, 0xf0);

        let oldest = cache.cached_color(oldest_color, 0.5).unwrap();
        for color in [
            RgbColor::new_8bpc(0x40, 0x50, 0x60),
            RgbColor::new_8bpc(0x70, 0x80, 0x90),
            newest_color,
        ] {
            cache.cached_color(color, 0.5).unwrap();
        }

        let full = cache
            .cached_color(RgbColor::new_8bpc(0x01, 0x02, 0x03), 0.5)
            .unwrap_err();
        let full = full
            .downcast_ref::<OutOfTextureSpace>()
            .expect("full atlas reports grow-and-recreate pressure");
        assert_eq!(full.current_size, 8);
        assert!(full.size.is_some_and(|size| size > full.current_size));

        let oldest_again = cache.cached_color(oldest_color, 0.5).unwrap();
        assert_eq!(
            texture_rect_tuple(oldest_again.texture_coords()),
            texture_rect_tuple(oldest.texture_coords()),
            "atlas pressure must not LRU-evict or relocate the oldest sprite"
        );
        assert!(
            cache.cached_color(newest_color, 0.5).is_ok(),
            "newest sprite remains cached too; callers must recreate the atlas"
        );
        assert_eq!(cache.color.len(), 4);
    }

    #[test]
    fn atlas_eviction_recreate_path_restores_capacity() {
        // The documented "recreate Self when the Atlas is filled" eviction
        // path: callers that hit OutOfTextureSpace are expected to throw the
        // GlyphCache away and build a fresh, larger one. Pin that this works
        // — the new cache's atlas honours the larger size, accepts the entry
        // that previously caused the failure, and starts at coords (1, 1).
        let (mut small_cache, fonts) = test_glyph_cache_with_atlas_size(8);
        for (r, g, b) in [
            (0x12, 0x34, 0x56),
            (0x65, 0x43, 0x21),
            (0xaa, 0xbb, 0xcc),
            (0xde, 0xad, 0xbe),
        ] {
            small_cache
                .cached_color(RgbColor::new_8bpc(r, g, b), 0.5)
                .unwrap();
        }
        let err = small_cache
            .cached_color(RgbColor::new_8bpc(0xfe, 0xed, 0xfa), 0.5)
            .unwrap_err();
        let new_size = err
            .downcast_ref::<OutOfTextureSpace>()
            .and_then(|e| e.size)
            .expect("OutOfTextureSpace with grow hint required");

        // Drop the old cache and recreate at the suggested size — this is
        // exactly what the renderer does in production.
        drop(small_cache);
        let _ = fonts; // RenderMetrics unused; we just need a fresh cache
        let (mut grown, _) = test_glyph_cache_with_atlas_size(new_size);

        // The previously-failing color now succeeds in the larger atlas,
        // landing at the canonical first-slot pixel origin.
        let sprite = grown
            .cached_color(RgbColor::new_8bpc(0xfe, 0xed, 0xfa), 0.5)
            .unwrap();
        assert_eq!(
            sprite.coords.origin.x, 1,
            "recreated atlas starts entries at x=1"
        );
        assert_eq!(
            sprite.coords.origin.y, 1,
            "recreated atlas starts entries at y=1"
        );
        assert_eq!(grown.color.len(), 1);
    }

    #[test]
    fn gui_visual_placeholder_invalid_encoded_image_decoder_settles_as_failed() {
        let image = Arc::new(ImageData::with_data(ImageDataType::EncodedFile(vec![
            0x00, 0x01, 0x02, 0x03,
        ])));

        let decoded = DecodedImage::load(&image).expect("decoder admission should succeed");
        let mut frames = decoded.frames.borrow_mut();
        let state = frames.as_mut().expect("encoded image has a frame state");
        let deadline = Instant::now() + Duration::from_secs(2);
        while state.load_state == LoadState::Loading {
            let _ = state.load_next_frame();
            assert!(
                Instant::now() < deadline,
                "bounded decoder did not settle invalid input before its test deadline"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(state.load_state, LoadState::Failed);
    }

    #[test]
    fn gui_visual_placeholder_cached_image_reports_failed_for_invalid_encodings() {
        let (mut cache, _) = test_glyph_cache();
        let image = Arc::new(ImageData::with_data(ImageDataType::EncodedFile(vec![
            0xde, 0xad, 0xbe, 0xef,
        ])));

        let deadline = Instant::now() + Duration::from_secs(2);
        let (sprite, next_due, load_state) = loop {
            let result = cache
                .cached_image(&image, Some(1), AllowImage::Yes)
                .unwrap();
            if result.2 != LoadState::Loading {
                break result;
            }
            assert!(
                result.1.is_some(),
                "an in-flight decoder must schedule another non-blocking poll"
            );
            assert!(
                Instant::now() < deadline,
                "cached invalid image did not settle before its test deadline"
            );
            std::thread::sleep(Duration::from_millis(1));
        };

        assert_eq!(load_state, LoadState::Failed);
        assert!(next_due.is_none());
        assert_eq!(sprite.coords.size.width, 1);
        assert_eq!(sprite.coords.size.height, 1);
    }

    #[test]
    fn decoded_frame_checked_bytes_rejects_overflowing_dimensions() {
        assert_eq!(checked_decoded_frame_bytes(2, 3), Some(24));
        assert_eq!(checked_decoded_frame_bytes(usize::MAX, 2), None);
    }

    fn decoder_test_frame(pixel: u8, duration: Duration) -> DecodedFrame {
        let pixels = vec![pixel, pixel, pixel, 0xff];
        DecodedFrame {
            hash: ImageDataType::hash_bytes(&pixels),
            pixels: Arc::new(pixels),
            duration,
            width: 1,
            height: 1,
        }
    }

    #[test]
    fn decoded_worker_frame_is_memory_resident_and_hash_authoritative_without_blob_io() {
        let frame = decoder_test_frame(0x5a, Duration::from_millis(17));
        let clone = frame.clone();
        let handle = DecodedImageHandle::frame(&frame);

        assert!(Arc::ptr_eq(&frame.pixels, &clone.pixels));
        assert_eq!(
            frame.hash,
            ImageDataType::hash_bytes(frame.pixels.as_slice())
        );
        assert_eq!(handle.image_dimensions(), (1, 1));
        assert!(!handle.is_mutable());
        assert_eq!(frame.duration, Duration::from_millis(17));
        assert_eq!(frame.checked_decoded_bytes(), Some(frame.pixels.len()));
    }

    fn disconnected_frame_state(frames: Vec<DecodedFrame>) -> FrameState {
        let (tx, receiver) = channel();
        drop(tx);
        let current_frame = frames.last().expect("at least one test frame").clone();
        let retained_frame_bytes = frames
            .iter()
            .try_fold(0usize, |total, frame| {
                total.checked_add(frame.checked_decoded_bytes()?)
            })
            .unwrap_or(usize::MAX);
        FrameState {
            source: FrameSource::Decoder(FrameDecoderReceiver {
                receiver,
                cancelled: Arc::new(AtomicBool::new(false)),
            }),
            current_frame,
            frames,
            retained_frame_bytes,
            load_state: LoadState::Loaded,
        }
    }

    fn queued_frame_state() -> (Sender<QueuedDecodedFrame>, FrameState) {
        let (sender, receiver) = channel();
        let state = FrameState::new(FrameDecoderReceiver {
            receiver,
            cancelled: Arc::new(AtomicBool::new(false)),
        });
        (sender, state)
    }

    fn send_test_frame(sender: &Sender<QueuedDecodedFrame>, frame: DecodedFrame) {
        sender
            .send(QueuedDecodedFrame {
                frame,
                budget: QueuedFrameBudget { bytes: 0 },
            })
            .expect("queue decoder test frame");
    }

    #[test]
    fn frame_retained_bytes_replace_placeholder_then_append_exactly_once() {
        let (sender, mut state) = queued_frame_state();
        assert_eq!(state.retained_bytes(), 4, "transparent placeholder bytes");

        send_test_frame(&sender, decoder_test_frame(0x11, Duration::from_millis(10)));
        assert!(state.load_next_frame());
        assert_eq!(
            state.retained_bytes(),
            4,
            "first decoded frame replaces rather than adds to the placeholder"
        );

        send_test_frame(&sender, decoder_test_frame(0x22, Duration::from_millis(20)));
        assert!(state.load_next_frame());
        assert_eq!(state.retained_bytes(), 8, "second frame is counted once");
    }

    #[test]
    fn frame_retained_bytes_fail_closed_on_dimension_overflow() {
        let (sender, mut state) = queued_frame_state();
        let pixels = vec![0, 0, 0, 0];
        let frame = DecodedFrame {
            hash: ImageDataType::hash_bytes(&pixels),
            pixels: Arc::new(pixels),
            duration: Duration::from_millis(10),
            width: usize::MAX,
            height: 2,
        };
        send_test_frame(&sender, frame);

        assert!(state.load_next_frame());
        assert_eq!(state.retained_bytes(), usize::MAX);
    }

    #[test]
    fn decoder_failure_retains_one_placeholder_without_double_counting() {
        let (sender, mut state) = queued_frame_state();
        drop(sender);

        assert!(!state.load_next_frame());
        assert_eq!(state.load_state, LoadState::Failed);
        assert_eq!(state.frames.len(), 1);
        assert_eq!(state.retained_bytes(), 4);
    }

    #[test]
    fn decoder_termination_keeps_stable_total_and_cache_eviction_subtracts_it_once() {
        let first = decoder_test_frame(0x11, Duration::from_millis(10));
        let second = decoder_test_frame(0x22, Duration::from_millis(20));
        let mut state = disconnected_frame_state(vec![first, second]);

        assert_eq!(state.retained_bytes(), 8);
        assert!(state.load_next_frame());
        assert!(matches!(&state.source, FrameSource::FrameIndex(_)));
        assert_eq!(state.retained_bytes(), 8);

        let source = Arc::new(ImageData::with_data(ImageDataType::EncodedLease(
            wezterm_blob_leases::BlobManager::store(&[0xaa]).expect("store encoded source lease"),
        )));
        let decoded = DecodedImage {
            frame_start: RefCell::new(Instant::now()),
            current_frame: RefCell::new(0),
            expected_revision: source.current_content_hash(),
            image: source,
            source_retained_bytes: Cell::new(0),
            decoded_validation: RefCell::new(None),
            frames: RefCell::new(Some(state)),
            load_state: Cell::new(LoadState::Loaded),
        };
        assert_eq!(decoded.retained_bytes(), 8);

        let (mut cache, _) = test_glyph_cache();
        let key = decoded_image_cache_key(&decoded);
        cache.cache_decoded_image(key, decoded);
        assert_eq!(cache.image_cache_retained_bytes, 8);
        let (removed_key, removed) = cache
            .image_cache
            .remove(&key)
            .expect("decoded image remains resident before explicit eviction");
        cache.forget_decoded_image_bytes(&removed_key, &removed);
        assert_eq!(cache.image_cache_retained_bytes, 0);
        assert!(cache.image_cache_entry_bytes.is_empty());
    }

    #[test]
    fn encoded_snapshot_render_avoids_source_payload_lock_and_rebinds_after_render() {
        let source = Arc::new(ImageData::with_data(ImageDataType::EncodedFile(vec![
            0xaa, 0xbb, 0xcc,
        ])));
        let expected_revision = source.current_content_hash();
        let frame = decoder_test_frame(0x31, Duration::from_secs(86_400));
        let decoded = DecodedImage {
            frame_start: RefCell::new(Instant::now()),
            current_frame: RefCell::new(0),
            image: Arc::clone(&source),
            expected_revision,
            source_retained_bytes: Cell::new(3),
            decoded_validation: RefCell::new(None),
            frames: RefCell::new(Some(FrameState {
                source: FrameSource::FrameIndex(0),
                current_frame: frame.clone(),
                frames: vec![frame],
                retained_frame_bytes: 4,
                load_state: LoadState::Loaded,
            })),
            load_state: Cell::new(LoadState::Loaded),
        };
        let (mut cache, _) = test_glyph_cache();

        // Holding the encoded source lock here is intentional. Rendering uses
        // only the independent immutable in-memory snapshot and must therefore
        // complete without recursively waiting on this payload mutex.
        let source_guard = source.data();
        let (_, _, state) = GlyphCache::cached_image_impl(
            &mut cache.frame_cache,
            &mut cache.blank_frame_cache,
            &mut cache.atlas,
            &decoded,
            None,
            cache.min_frame_duration,
            AllowImage::Yes,
        )
        .expect("encoded snapshot rendering does not acquire the source payload lock");
        assert_eq!(state, LoadState::Loaded);
        drop(source_guard);

        let error = GlyphCache::cached_encoded_frame_for_revision(
            &mut cache.frame_cache,
            &mut cache.blank_frame_cache,
            &mut cache.atlas,
            &decoded,
            None,
            cache.min_frame_duration,
            None,
            || {
                *source.data_mut() = ImageDataType::EncodedFile(vec![0x11, 0x22, 0x33]);
            },
        )
        .expect_err("a mutation after snapshot rendering invalidates the old revision");
        assert!(
            error
                .downcast_ref::<DecodedImageRevisionMismatch>()
                .is_some(),
            "the post-render revision probe must fail closed"
        );
        assert!(!source.cached_content_revision_is(expected_revision));
    }

    #[test]
    fn empty_decoder_retry_finalizer_samples_time_at_publication_boundary() {
        let original_start = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("short test offset is representable");
        let original_due = original_start
            .checked_add(Duration::from_secs(1))
            .expect("short test offset is representable");

        let mut loaded_start = original_start;
        let loaded_due = finalize_decoder_retry_after_empty_poll(
            &mut loaded_start,
            original_due,
            Duration::ZERO,
            Duration::from_secs(1),
            false,
        );
        assert_eq!(loaded_due, Some(original_due));
        assert_eq!(loaded_start, original_start);

        // This timestamp represents arbitrary sprite/atlas work completed
        // after the decoder poll. The empty-queue path must sample the clock
        // after that work rather than preserving the stale `original_due`.
        let before_publication = Instant::now();
        let mut empty_start = original_start;
        let empty_due = finalize_decoder_retry_after_empty_poll(
            &mut empty_start,
            original_due,
            Duration::ZERO,
            Duration::from_secs(1),
            true,
        )
        .expect("a short retry interval is representable");
        assert!(empty_start >= before_publication);
        assert!(empty_due >= before_publication + Duration::from_secs(1));
    }

    #[test]
    fn encoded_empty_decoder_poll_rebases_expired_deadline_without_hot_spin() {
        let (_sender, state) = queued_frame_state();
        let source = Arc::new(ImageData::with_data(ImageDataType::EncodedFile(vec![
            0xaa, 0xbb, 0xcc,
        ])));
        let expected_revision = source.current_content_hash();
        let decoded = DecodedImage {
            frame_start: RefCell::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .expect("short test offset is representable"),
            ),
            current_frame: RefCell::new(0),
            image: source,
            expected_revision,
            source_retained_bytes: Cell::new(3),
            decoded_validation: RefCell::new(None),
            frames: RefCell::new(Some(state)),
            load_state: Cell::new(LoadState::Loading),
        };
        let (mut cache, _) = test_glyph_cache();
        let retry_interval = Duration::from_secs(1);
        let before_poll = Instant::now();

        let (_, next_due, load_state) = GlyphCache::cached_image_impl(
            &mut cache.frame_cache,
            &mut cache.blank_frame_cache,
            &mut cache.atlas,
            &decoded,
            None,
            retry_interval,
            AllowImage::Yes,
        )
        .expect("an empty decoder queue renders a nonblocking placeholder");

        assert_eq!(load_state, LoadState::Loading);
        assert!(
            next_due.is_some_and(|due| due >= before_poll + retry_interval),
            "an empty queue must schedule a future retry rather than return its expired deadline"
        );
        assert!(cache.frame_cache.is_empty());
        assert_eq!(cache.blank_frame_cache.len(), 1);
    }

    #[test]
    fn encoded_zero_duration_root_stays_blank_until_first_visible_frame() {
        let (sender, mut state) = queued_frame_state();
        let root = decoder_test_frame(0x21, Duration::ZERO);
        let root_hash = root.hash;
        send_test_frame(&sender, root);
        assert!(state.load_next_frame());
        assert!(state.awaiting_first_visible_frame());
        assert_eq!(state.load_state, LoadState::Loading);
        assert_eq!(state.frame_hash(), root_hash);

        let source = Arc::new(ImageData::with_data(ImageDataType::EncodedFile(vec![
            0xaa, 0xbb, 0xcc,
        ])));
        let expected_revision = source.current_content_hash();
        let decoded = DecodedImage {
            frame_start: RefCell::new(Instant::now()),
            current_frame: RefCell::new(0),
            image: source,
            expected_revision,
            source_retained_bytes: Cell::new(3),
            decoded_validation: RefCell::new(None),
            frames: RefCell::new(Some(state)),
            load_state: Cell::new(LoadState::Loading),
        };
        let (mut cache, _) = test_glyph_cache();

        let (_, first_due, first_state) = GlyphCache::cached_image_impl(
            &mut cache.frame_cache,
            &mut cache.blank_frame_cache,
            &mut cache.atlas,
            &decoded,
            None,
            cache.min_frame_duration,
            AllowImage::Yes,
        )
        .expect("zero-duration root renders a nonblocking transparent placeholder");
        assert_eq!(first_state, LoadState::Loading);
        assert!(first_due.is_some());
        assert!(
            cache.frame_cache.is_empty(),
            "root pixels were not uploaded"
        );
        assert_eq!(cache.blank_frame_cache.len(), 1);

        let visible = decoder_test_frame(0x42, Duration::from_millis(20));
        let visible_hash = visible.hash;
        send_test_frame(&sender, visible);
        *decoded.frame_start.borrow_mut() = Instant::now()
            .checked_sub(Duration::from_secs(1))
            .expect("short test offset is representable");

        let (_, _, visible_state) = GlyphCache::cached_image_impl(
            &mut cache.frame_cache,
            &mut cache.blank_frame_cache,
            &mut cache.atlas,
            &decoded,
            None,
            cache.min_frame_duration,
            AllowImage::Yes,
        )
        .expect("first timed frame replaces the transparent root placeholder");
        assert_eq!(visible_state, LoadState::Loaded);
        assert_eq!(cache.frame_cache.len(), 1);
        let frames = decoded.frames.borrow();
        let frames = frames.as_ref().expect("encoded frame state remains live");
        assert!(!frames.awaiting_first_visible_frame());
        assert_eq!(frames.frame_hash(), visible_hash);
    }

    #[test]
    fn encoded_zero_duration_root_and_ready_timed_frame_publish_in_one_paint() {
        let (sender, state) = queued_frame_state();
        send_test_frame(&sender, decoder_test_frame(0x21, Duration::ZERO));
        let visible = decoder_test_frame(0x42, Duration::from_millis(20));
        let visible_hash = visible.hash;
        send_test_frame(&sender, visible);

        let source = Arc::new(ImageData::with_data(ImageDataType::EncodedFile(vec![
            0xaa, 0xbb, 0xcc,
        ])));
        let expected_revision = source.current_content_hash();
        let decoded = DecodedImage {
            frame_start: RefCell::new(
                Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .expect("short test offset is representable"),
            ),
            current_frame: RefCell::new(0),
            image: source,
            expected_revision,
            source_retained_bytes: Cell::new(3),
            decoded_validation: RefCell::new(None),
            frames: RefCell::new(Some(state)),
            load_state: Cell::new(LoadState::Loading),
        };
        let (mut cache, _) = test_glyph_cache();

        let (_, next_due, load_state) = GlyphCache::cached_image_impl(
            &mut cache.frame_cache,
            &mut cache.blank_frame_cache,
            &mut cache.atlas,
            &decoded,
            None,
            cache.min_frame_duration,
            AllowImage::Yes,
        )
        .expect("a queued timed frame must replace the zero-duration root immediately");

        assert_eq!(load_state, LoadState::Loaded);
        assert!(next_due.is_some());
        assert_eq!(cache.frame_cache.len(), 1);
        assert!(
            cache.blank_frame_cache.is_empty(),
            "the ready timed frame must not spend one paint behind a blank sprite"
        );
        let frames = decoded.frames.borrow();
        let frames = frames.as_ref().expect("encoded frame state remains live");
        assert!(!frames.awaiting_first_visible_frame());
        assert_eq!(frames.frame_hash(), visible_hash);
    }

    #[test]
    fn decoded_animation_source_bytes_are_aggregated_once_at_construction() {
        let first = vec![0x11; 4];
        let second = vec![0x22; 4];
        let image = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(10), Duration::from_millis(20)],
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
        }));
        attest_decoded_image(&image);

        let decoded = DecodedImage::load(&image).expect("attested animation loads synchronously");

        assert_eq!(decoded.source_retained_bytes.get(), 8);
        assert!(decoded.decoded_validation.borrow().is_none());
        assert_eq!(decoded.retained_bytes(), 8);
    }

    #[test]
    fn image_cache_regression_rejects_direct_empty_animation_before_render_indexing() {
        let image = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![],
            frames: vec![],
            hashes: vec![],
        }));

        let (mut cache, _) = test_glyph_cache();
        let (_, next_due, load_state) = wait_for_image_to_settle(&mut cache, &image);
        assert_eq!(load_state, LoadState::Failed);
        assert!(next_due.is_none());
        assert!(cache.image_cache.is_empty());
        assert!(cache.image_cache_entry_bytes.is_empty());
        let rejected_key = image_cache_key(&image);
        assert!(cache.image_revision_is_rejected(&rejected_key, &image));
        assert_eq!(cache.image_validation_rejection_order.len(), 1);
    }

    #[test]
    fn image_cache_regression_rejects_mutated_animation_cardinality_before_render_indexing() {
        let first = vec![0x11; 4];
        let second = vec![0x22; 4];
        let image = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(10), Duration::from_millis(20)],
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
        }));
        if let ImageDataType::AnimRgba8 { durations, .. } = &mut *image.data_mut() {
            let _ = durations.pop();
        }

        let (mut cache, _) = test_glyph_cache();
        let (_, next_due, load_state) = wait_for_image_to_settle(&mut cache, &image);
        assert_eq!(load_state, LoadState::Failed);
        assert!(next_due.is_none());
        let rejected_key = image_cache_key(&image);
        assert!(cache.image_revision_is_rejected(&rejected_key, &image));
    }

    #[test]
    fn decoder_disconnect_wraps_from_last_frame_to_first_without_skipping_it() {
        let first = decoder_test_frame(0x11, Duration::from_millis(10));
        let first_hash = first.hash;
        let second = decoder_test_frame(0x22, Duration::from_millis(20));
        let mut state = disconnected_frame_state(vec![first, second]);

        assert!(state.load_next_frame());
        assert!(matches!(&state.source, FrameSource::FrameIndex(0)));
        assert_eq!(state.current_frame.hash, first_hash);
    }

    #[test]
    fn decoder_disconnect_skips_zero_duration_animation_root_on_wrap() {
        let root = decoder_test_frame(0x11, Duration::ZERO);
        let first_visible = decoder_test_frame(0x22, Duration::from_millis(20));
        let first_visible_hash = first_visible.hash;
        let last = decoder_test_frame(0x33, Duration::from_millis(30));
        let mut state = disconnected_frame_state(vec![root, first_visible, last]);

        assert!(state.load_next_frame());
        assert!(matches!(&state.source, FrameSource::FrameIndex(1)));
        assert_eq!(state.current_frame.hash, first_visible_hash);
    }

    #[test]
    fn decoder_disconnect_updates_the_live_single_frame_duration() {
        let frame = decoder_test_frame(0x44, Duration::from_millis(1));
        let mut state = disconnected_frame_state(vec![frame]);

        assert!(state.load_next_frame());
        assert!(matches!(&state.source, FrameSource::FrameIndex(0)));
        assert_eq!(state.frame_duration(), Duration::from_secs(86_400));
        assert_eq!(state.current_frame.duration, state.frames[0].duration);
    }

    #[test]
    fn decoder_zero_duration_single_frame_becomes_visible_after_disconnect() {
        let (sender, mut state) = queued_frame_state();
        send_test_frame(&sender, decoder_test_frame(0x45, Duration::ZERO));

        assert!(state.load_next_frame());
        assert!(state.awaiting_first_visible_frame());
        assert_eq!(state.load_state, LoadState::Loading);

        drop(sender);
        assert!(state.load_next_frame());
        assert!(matches!(&state.source, FrameSource::FrameIndex(0)));
        assert!(!state.awaiting_first_visible_frame());
        assert_eq!(state.load_state, LoadState::Loaded);
        assert_eq!(state.frame_duration(), Duration::from_secs(86_400));
    }

    fn one_pixel_image(pixel: [u8; 4]) -> Arc<ImageData> {
        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            pixel.to_vec(),
        )));
        attest_decoded_image(&image);
        image
    }

    fn replace_one_pixel(image: &Arc<ImageData>, pixel: [u8; 4]) {
        *image.data_mut() = ImageDataType::new_single_frame(1, 1, pixel.to_vec());
        attest_decoded_image(image);
    }

    fn replace_one_pixel_unattested(image: &Arc<ImageData>, pixel: [u8; 4]) {
        *image.data_mut() = ImageDataType::new_single_frame(1, 1, pixel.to_vec());
    }

    fn attest_decoded_image(image: &Arc<ImageData>) {
        let revision = image.current_content_hash();
        let normalized = image
            .normalize_for_content_revision_with_limits(
                revision,
                MAX_IMAGE_WIRE_BYTES,
                decoded_image_validation_limits(),
                &|| false,
            )
            .expect("test decoded image validates");
        assert!(normalized.replacement.is_none());
    }

    fn wait_for_image_to_settle(
        cache: &mut GlyphCache,
        image: &Arc<ImageData>,
    ) -> (Sprite, Option<Instant>, LoadState) {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = cache
                .cached_image(image, None, AllowImage::Yes)
                .expect("bounded image validation returns a renderable placeholder");
            if result.2 != LoadState::Loading {
                return result;
            }
            assert!(
                result.1.is_some(),
                "an in-flight validation must schedule a bounded repaint"
            );
            assert!(
                Instant::now() < deadline,
                "decoded-image validation did not settle before its test deadline"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    static IMAGE_PIPELINE_GLOBAL_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn wait_for_frame_decoder_job_count(expected: usize) {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(2))
            .expect("short test deadline is representable");
        while FRAME_DECODER_JOBS.load(Ordering::Acquire) != expected {
            assert!(
                Instant::now() < deadline,
                "frame-decoder job count did not settle at {expected}; current={}",
                FRAME_DECODER_JOBS.load(Ordering::Acquire)
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn trusted_local_authority_above_wire_limit_bypasses_fallback_validator() {
        let _serial = IMAGE_PIPELINE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        wait_for_frame_decoder_job_count(0);

        // 4096 * 4097 * 4 = 64 MiB + 16 KiB: just above the remote/fallback
        // boundary and far below the 256 MiB trusted-local ceiling.
        let width = 4_096u32;
        let height = 4_097u32;
        let decoded_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|pixels| pixels.checked_mul(4))
            .expect("test dimensions fit usize");
        assert!(decoded_bytes > MAX_IMAGE_WIRE_BYTES);
        assert!(decoded_bytes <= MAX_TRUSTED_LOCAL_IMAGE_DECODED_BYTES);

        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            width,
            height,
            vec![0x5a; decoded_bytes],
        )));
        let revision = image.current_content_hash();
        let validation = image
            .normalize_for_content_revision_with_limits(
                revision,
                0,
                trusted_decoded_image_authority_limits(),
                &|| false,
            )
            .expect("bounded local producer publishes trusted authority");
        assert_eq!(validation.summary.decoded_bytes, decoded_bytes);
        assert!(validation.replacement.is_none());
        assert!(
            image
                .validated_summary_for_content_revision(revision, decoded_image_validation_limits())
                .is_none(),
            "the 64 MiB fallback validator must remain fail-closed"
        );

        let decoded = DecodedImage::load_for_revision(&image, revision)
            .expect("trusted local authority admits the larger bounded image");
        assert_eq!(decoded.load_state.get(), LoadState::Loaded);
        assert!(decoded.decoded_validation.borrow().is_none());
        assert!(decoded.frames.borrow().is_none());
        assert_eq!(decoded.source_retained_bytes.get(), decoded_bytes);
        assert_eq!(FRAME_DECODER_JOBS.load(Ordering::Acquire), 0);
    }

    #[test]
    fn transient_validation_queue_saturation_retries_without_negative_cache_poison() {
        let _serial = IMAGE_PIPELINE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        wait_for_frame_decoder_job_count(0);

        let mut permits = (0..MAX_PENDING_FRAME_DECODERS)
            .map(|_| {
                FrameDecoderJobPermit::try_acquire()
                    .expect("test owns every bounded validation queue slot")
            })
            .collect::<Vec<_>>();
        assert!(FrameDecoderJobPermit::try_acquire().is_none());

        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0x11, 0x22, 0x33, 0xff],
        )));
        let key = image_cache_key(&image);
        let (mut cache, _) = test_glyph_cache();
        let retry_delay = cache.min_frame_duration.max(Duration::from_millis(1));
        let requested_at = Instant::now();
        let (_, next_due, state) = cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("queue pressure returns a renderable placeholder");

        assert_eq!(state, LoadState::Loading);
        let next_due = next_due.expect("transient pressure schedules a retry");
        assert!(
            next_due
                >= requested_at
                    .checked_add(retry_delay)
                    .expect("short retry delay is representable")
        );
        assert!(
            next_due
                <= Instant::now()
                    .checked_add(retry_delay)
                    .expect("short retry delay is representable"),
            "retry must stay at the renderer's bounded frame cadence"
        );
        assert!(!cache.image_revision_is_rejected(&key, &image));
        assert!(cache.image_validation_rejection_order.is_empty());
        assert!(cache.image_cache.is_empty());

        drop(permits.pop());
        let retry = cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("freeing one queue slot retries normal admission");
        let (_, _, settled) = if retry.2 == LoadState::Loading {
            wait_for_image_to_settle(&mut cache, &image)
        } else {
            retry
        };
        assert_eq!(settled, LoadState::Loaded);
        assert!(!cache.image_revision_is_rejected(&key, &image));
        assert!(cache.image_validation_rejection_order.is_empty());
        assert!(cache.image_cache.contains_key(&key));

        drop(permits);
        wait_for_frame_decoder_job_count(0);
    }

    #[test]
    fn dropping_validation_receiver_cancels_worker_and_releases_shared_permit() {
        let _serial = IMAGE_PIPELINE_GLOBAL_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        wait_for_frame_decoder_job_count(0);

        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0x44, 0x55, 0x66, 0xff],
        )));
        let revision = image.current_content_hash();
        let (started_tx, started_rx) = channel();
        let (release_tx, release_rx) = channel();
        let receiver = DecodedImageValidator::start_with_hook(image, revision, move || {
            let _ = started_tx.send(());
            let _ = release_rx.recv();
        })
        .expect("validation worker is admitted");
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("validation worker reaches deterministic test gate");
        assert_eq!(FRAME_DECODER_JOBS.load(Ordering::Acquire), 1);

        let cancelled = Arc::clone(&receiver.cancelled);
        drop(receiver);
        assert!(cancelled.load(Ordering::Acquire));
        release_tx.send(()).expect("release cancelled worker gate");
        wait_for_frame_decoder_job_count(0);
    }

    #[test]
    fn unattested_decoded_load_returns_before_full_validation_and_publishes_authority() {
        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0x11, 0x22, 0x33, 0xff],
        )));
        let revision = image.current_content_hash();
        assert!(
            image
                .validated_summary_for_content_revision(revision, decoded_image_validation_limits())
                .is_none()
        );

        let decoded = DecodedImage::load(&image)
            .expect("unattested decoded image is admitted to the bounded validator");
        assert_eq!(decoded.load_state.get(), LoadState::Loading);
        assert!(decoded.decoded_validation.borrow().is_some());
        assert_eq!(
            decoded.source_retained_bytes.get(),
            MAX_FRAME_DECODER_DECODED_BYTES,
            "pending validation reserves the complete accepted byte ceiling"
        );

        let (mut cache, _) = test_glyph_cache();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let result = GlyphCache::cached_image_impl(
                &mut cache.frame_cache,
                &mut cache.blank_frame_cache,
                &mut cache.atlas,
                &decoded,
                None,
                cache.min_frame_duration,
                AllowImage::Yes,
            )
            .expect("validation polling remains renderable");
            if result.2 != LoadState::Loading {
                assert_eq!(result.2, LoadState::Loaded);
                break;
            }
            assert!(result.1.is_some());
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        }

        assert!(decoded.decoded_validation.borrow().is_none());
        assert_eq!(decoded.source_retained_bytes.get(), 4);
        assert_eq!(
            image.validated_summary_for_content_revision(
                revision,
                decoded_image_validation_limits()
            ),
            Some(ImageDataValidationSummary {
                decoded_bytes: 4,
                frame_count: 1,
            })
        );
    }

    #[test]
    fn pending_decoded_validation_uses_bounded_repaint_placeholder_without_uploading_pixels() {
        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0x44, 0x55, 0x66, 0xff],
        )));
        let expected_revision = image.current_content_hash();
        let (sender, receiver) = channel();
        let decoded = DecodedImage {
            frame_start: RefCell::new(Instant::now()),
            current_frame: RefCell::new(0),
            image,
            expected_revision,
            source_retained_bytes: Cell::new(MAX_FRAME_DECODER_DECODED_BYTES),
            decoded_validation: RefCell::new(Some(DecodedImageValidationReceiver {
                receiver,
                cancelled: Arc::new(AtomicBool::new(false)),
            })),
            frames: RefCell::new(None),
            load_state: Cell::new(LoadState::Loading),
        };
        let (mut cache, _) = test_glyph_cache();

        let (_, next_due, state) = GlyphCache::cached_image_impl(
            &mut cache.frame_cache,
            &mut cache.blank_frame_cache,
            &mut cache.atlas,
            &decoded,
            None,
            cache.min_frame_duration,
            AllowImage::Yes,
        )
        .expect("pending validation renders a transparent placeholder");

        assert_eq!(state, LoadState::Loading);
        assert!(next_due.is_some(), "pending work must schedule a repaint");
        assert!(
            cache.frame_cache.is_empty(),
            "source pixels were not uploaded"
        );
        assert_eq!(cache.blank_frame_cache.len(), 1);
        drop(sender);
    }

    #[test]
    fn mutation_clears_authority_and_reenters_nonblocking_validation_lane() {
        let (mut cache, _) = test_glyph_cache();
        let image = one_pixel_image([0x11, 0x22, 0x33, 0xff]);
        let old_key = image_cache_key(&image);
        let (_, _, first_state) = cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("attested revision uploads immediately");
        assert_eq!(first_state, LoadState::Loaded);

        replace_one_pixel_unattested(&image, [0xaa, 0xbb, 0xcc, 0xff]);
        let current_key = image_cache_key(&image);
        assert!(
            image
                .validated_summary_for_content_revision(
                    current_key.revision,
                    decoded_image_validation_limits()
                )
                .is_none(),
            "mutable access clears prior validation authority"
        );

        let first_retry = cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("unattested mutation enters bounded validation");
        assert!(matches!(
            first_retry.2,
            LoadState::Loading | LoadState::Loaded
        ));
        let (_, _, settled_state) = if first_retry.2 == LoadState::Loading {
            wait_for_image_to_settle(&mut cache, &image)
        } else {
            first_retry
        };
        assert_eq!(settled_state, LoadState::Loaded);
        assert!(!cache.image_cache.contains_key(&old_key));
        assert!(cache.image_cache.contains_key(&current_key));
        assert_eq!(cache.image_cache_retained_bytes, 4);
        assert!(
            image
                .validated_summary_for_content_revision(
                    current_key.revision,
                    decoded_image_validation_limits()
                )
                .is_some(),
            "worker success republishes authority for the exact new revision"
        );
    }

    fn image_cache_key(image: &Arc<ImageData>) -> ImageCacheKey {
        ImageCacheKey::new(image, image.current_content_hash())
    }

    fn decoded_image_cache_key(decoded: &DecodedImage) -> ImageCacheKey {
        image_cache_key(&decoded.image)
    }

    #[test]
    fn image_cache_regression_frame_sprite_key_covers_exact_atlas_geometry() {
        let (mut cache, _) = test_glyph_cache();
        let pixels = vec![0x5a; 16];
        let one_by_four = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            4,
            pixels.clone(),
        )));
        let two_by_two = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            2, 2, pixels,
        )));
        attest_decoded_image(&one_by_four);
        attest_decoded_image(&two_by_two);

        let (base, _, _) = cache
            .cached_image(&one_by_four, None, AllowImage::Yes)
            .expect("base geometry uploads");
        assert_eq!(cache.frame_cache.len(), 1);

        let (same, _, _) = cache
            .cached_image(&one_by_four, None, AllowImage::Yes)
            .expect("identical geometry hits the frame sprite cache");
        assert_eq!(same.coords, base.coords);
        assert_eq!(cache.frame_cache.len(), 1);

        let (padded, _, _) = cache
            .cached_image(&one_by_four, Some(1), AllowImage::Yes)
            .expect("different padding uploads independently");
        assert_ne!(padded.coords, base.coords);
        assert_eq!(cache.frame_cache.len(), 2);

        let (scaled, _, _) = cache
            .cached_image(&one_by_four, None, AllowImage::Scale(2))
            .expect("different scale-down policy uploads independently");
        assert_ne!(scaled.coords, base.coords);
        assert_eq!(cache.frame_cache.len(), 3);

        let (reshaped, _, _) = cache
            .cached_image(&two_by_two, None, AllowImage::Yes)
            .expect("equal pixel bytes with different dimensions upload independently");
        assert_ne!(reshaped.coords, base.coords);
        assert_eq!(cache.frame_cache.len(), 4);
    }

    #[test]
    fn image_cache_regression_atlas_recreation_moves_complete_decoded_state() {
        let (mut old_cache, _) = test_glyph_cache();
        let (mut replacement_cache, _) = test_glyph_cache();
        let image = one_pixel_image([0x11, 0x22, 0x33, 0xff]);
        let old_key = image_cache_key(&image);

        old_cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("decoded image primes the old atlas cache");
        let prior_registration_count = old_cache.image_revision_owner_registrations_since_prune;
        assert!(old_cache.image_cache.contains_key(&old_key));
        assert_eq!(old_cache.image_cache_entry_bytes.get(&old_key), Some(&4));
        assert_eq!(old_cache.image_cache_retained_bytes, 4);
        assert_eq!(old_cache.image_revision_owners.len(), 1);

        old_cache.swap_decoded_image_cache_state(&mut replacement_cache);

        assert!(old_cache.image_cache.is_empty());
        assert!(old_cache.image_cache_entry_bytes.is_empty());
        assert_eq!(old_cache.image_cache_retained_bytes, 0);
        assert!(old_cache.image_revision_owners.is_empty());
        assert_eq!(old_cache.image_revision_owner_registrations_since_prune, 0);
        assert!(replacement_cache.image_cache.contains_key(&old_key));
        assert_eq!(
            replacement_cache.image_cache_entry_bytes.get(&old_key),
            Some(&4)
        );
        assert_eq!(replacement_cache.image_cache_retained_bytes, 4);
        assert_eq!(replacement_cache.image_revision_owners.len(), 1);
        assert_eq!(
            replacement_cache.image_revision_owner_registrations_since_prune,
            prior_registration_count
        );

        replace_one_pixel(&image, [0xaa, 0xbb, 0xcc, 0xff]);
        let current_key = image_cache_key(&image);
        replacement_cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("retained owner retires the pre-recreation revision after mutation");
        assert!(!replacement_cache.image_cache.contains_key(&old_key));
        assert!(replacement_cache.image_cache.contains_key(&current_key));
        assert_eq!(replacement_cache.image_cache.len(), 1);
        assert_eq!(replacement_cache.image_cache_retained_bytes, 4);
        assert_eq!(replacement_cache.image_cache_entry_bytes.len(), 1);
        assert_eq!(replacement_cache.image_revision_owners.len(), 1);
    }

    #[test]
    fn image_cache_regression_rejection_survives_atlas_recreation_and_retries_new_revision() {
        let (mut old_cache, _) = test_glyph_cache();
        let (mut replacement_cache, _) = test_glyph_cache();
        let image = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![],
            frames: vec![],
            hashes: vec![],
        }));
        let rejected_key = image_cache_key(&image);
        let mut load_attempts = 0usize;

        let deadline = Instant::now() + Duration::from_secs(2);
        let (_, first_due, first_state) = loop {
            let result = old_cache
                .cached_image_with_revision_observers(
                    &image,
                    None,
                    AllowImage::Yes,
                    |_| {},
                    |_| load_attempts += 1,
                    |_| {},
                )
                .expect("first malformed revision yields a bounded placeholder");
            if result.2 != LoadState::Loading {
                break result;
            }
            assert!(result.1.is_some());
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(first_state, LoadState::Failed);
        assert!(first_due.is_none());
        assert_eq!(load_attempts, 1);
        assert!(old_cache.image_revision_is_rejected(&rejected_key, &image));
        assert_eq!(
            old_cache.image_validation_rejection_order.front(),
            Some(&rejected_key)
        );

        old_cache.swap_decoded_image_cache_state(&mut replacement_cache);

        assert!(old_cache.image_revision_owners.is_empty());
        assert!(old_cache.image_validation_rejection_order.is_empty());
        assert!(replacement_cache.image_revision_is_rejected(&rejected_key, &image));

        let (_, repeated_due, repeated_state) = replacement_cache
            .cached_image_with_revision_observers(
                &image,
                None,
                AllowImage::Yes,
                |_| {},
                |_| load_attempts += 1,
                |_| {},
            )
            .expect("same malformed revision hits bounded negative authority");
        assert_eq!(repeated_state, LoadState::Failed);
        assert!(repeated_due.is_none());
        assert_eq!(
            load_attempts, 1,
            "the same object revision must not be revalidated after rejection"
        );

        let image_for_hook = Arc::clone(&image);
        let (_, _, valid_state) = replacement_cache
            .cached_image_with_revision_observers(
                &image,
                None,
                AllowImage::Yes,
                move |attempt| {
                    if attempt == 0 {
                        replace_one_pixel(&image_for_hook, [0xaa, 0xbb, 0xcc, 0xff]);
                    }
                },
                |_| load_attempts += 1,
                |_| {},
            )
            .expect("a later valid content revision retries normal admission");
        let valid_key = image_cache_key(&image);
        assert_eq!(valid_state, LoadState::Loaded);
        assert_eq!(load_attempts, 2);
        assert!(!replacement_cache.image_cache.contains_key(&rejected_key));
        assert!(replacement_cache.image_cache.contains_key(&valid_key));
        assert!(
            replacement_cache
                .image_validation_rejection_order
                .is_empty()
        );
        assert!(!replacement_cache.image_revision_is_rejected(&valid_key, &image));
    }

    #[test]
    fn image_cache_regression_rejection_authority_is_weak_and_hard_bounded() {
        let (mut cache, _) = test_glyph_cache();
        let mut images = Vec::with_capacity(MAX_REJECTED_IMAGE_REVISIONS + 1);

        for _ in 0..=MAX_REJECTED_IMAGE_REVISIONS {
            let image = Arc::new(ImageData::with_data(ImageDataType::AnimRgba8 {
                width: 1,
                height: 1,
                durations: vec![],
                frames: vec![],
                hashes: vec![],
            }));
            let key = cache.bind_image_revision(&image, image.current_content_hash());
            cache.record_image_validation_rejection(key, &image);
            images.push(image);
        }

        assert_eq!(
            cache.image_validation_rejection_order.len(),
            MAX_REJECTED_IMAGE_REVISIONS
        );
        assert_eq!(
            cache.image_revision_owners.len(),
            MAX_REJECTED_IMAGE_REVISIONS
        );
        assert!(
            images.iter().all(|image| Arc::strong_count(image) == 1),
            "negative authority must retain Weak identities only"
        );
        let newest = images.last().expect("one bounded rejection remains");
        let newest_key = image_cache_key(newest);
        assert!(cache.image_revision_is_rejected(&newest_key, newest));

        drop(images);
        cache.prune_stale_image_revision_owners();
        assert!(cache.image_revision_owners.is_empty());
        assert!(cache.image_validation_rejection_order.is_empty());
    }

    #[test]
    fn glyph_image_cache_keys_mutable_images_by_current_content_revision() {
        let (mut cache, _) = test_glyph_cache();
        let image = one_pixel_image([0x11, 0x22, 0x33, 0xff]);
        let first_revision = image.current_content_hash();
        let first_key = ImageCacheKey::new(&image, first_revision);
        let (first_sprite, _, _) = cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("first revision should upload");

        replace_one_pixel(&image, [0xaa, 0xbb, 0xcc, 0xff]);
        let second_revision = image.current_content_hash();
        let second_key = ImageCacheKey::new(&image, second_revision);
        assert_ne!(first_revision, second_revision);

        let (second_sprite, _, _) = cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("mutated revision should upload independently");

        assert_ne!(
            first_sprite.coords, second_sprite.coords,
            "a Kitty-style in-place edit must not reuse the prior revision's atlas sprite"
        );
        assert!(!cache.image_cache.contains_key(&first_key));
        assert!(cache.image_cache.contains_key(&second_key));
        assert_eq!(cache.image_cache_retained_bytes, 4);
        assert_eq!(cache.image_cache_entry_bytes.len(), 1);
        assert_eq!(cache.image_revision_owners.len(), 1);
    }

    #[test]
    fn cached_image_retries_mutation_after_key_capture_on_cache_hit() {
        let (mut cache, _) = test_glyph_cache();
        let image = one_pixel_image([0x11, 0x22, 0x33, 0xff]);
        let stale_key = image_cache_key(&image);
        let (old_sprite, _, _) = cache
            .cached_image(&image, None, AllowImage::Yes)
            .expect("prime the old revision cache hit");
        let image_for_hook = Arc::clone(&image);

        let (new_sprite, _, load_state) = cache
            .cached_image_with_revision_observers(
                &image,
                None,
                AllowImage::Yes,
                move |attempt| {
                    if attempt == 0 {
                        replace_one_pixel(&image_for_hook, [0xaa, 0xbb, 0xcc, 0xff]);
                    }
                },
                |_| {},
                |_| {},
            )
            .expect("one bounded rebind renders the new revision");

        let current_key = image_cache_key(&image);
        assert_eq!(load_state, LoadState::Loaded);
        assert_ne!(old_sprite.coords, new_sprite.coords);
        assert!(!cache.image_cache.contains_key(&stale_key));
        assert!(cache.image_cache.contains_key(&current_key));
        assert_eq!(cache.image_cache.len(), 1);
        assert_eq!(cache.image_revision_owners.len(), 1);
    }

    #[test]
    fn cached_image_retries_mutation_between_load_and_first_payload_read() {
        let (mut cache, _) = test_glyph_cache();
        let image = one_pixel_image([0x11, 0x22, 0x33, 0xff]);
        let stale_key = image_cache_key(&image);
        let image_for_hook = Arc::clone(&image);

        let (_, _, load_state) = cache
            .cached_image_with_revision_observers(
                &image,
                None,
                AllowImage::Yes,
                |_| {},
                |_| {},
                move |attempt| {
                    if attempt == 0 {
                        replace_one_pixel(&image_for_hook, [0xaa, 0xbb, 0xcc, 0xff]);
                    }
                },
            )
            .expect("one bounded reload renders the post-load revision");

        let current_key = image_cache_key(&image);
        assert_eq!(load_state, LoadState::Loaded);
        assert!(!cache.image_cache.contains_key(&stale_key));
        assert!(cache.image_cache.contains_key(&current_key));
        assert_eq!(cache.image_cache.len(), 1);
        assert_eq!(cache.frame_cache.len(), 1);
        assert_eq!(cache.image_revision_owners.len(), 1);
    }

    #[test]
    fn repeated_revision_churn_returns_loading_placeholder_after_bounded_attempts() {
        let (mut cache, _) = test_glyph_cache();
        let image = one_pixel_image([0x11, 0x22, 0x33, 0xff]);
        let image_for_hook = Arc::clone(&image);

        let (_, next_due, load_state) = cache
            .cached_image_with_revision_observers(
                &image,
                None,
                AllowImage::Yes,
                move |attempt| {
                    let value = u8::try_from(attempt + 1).expect("bounded attempt fits u8");
                    replace_one_pixel(&image_for_hook, [value, value, value, 0xff]);
                },
                |_| {},
                |_| {},
            )
            .expect("repeated revision churn degrades to a retryable placeholder");

        assert_eq!(load_state, LoadState::Loading);
        assert!(next_due.is_some());
        assert!(cache.image_cache.is_empty());
        assert!(cache.image_cache_entry_bytes.is_empty());
        assert!(cache.image_revision_owners.is_empty());
    }

    #[test]
    fn object_scoped_key_prevents_old_content_collision_before_mutation_is_observed() {
        let (mut cache, _) = test_glyph_cache();
        let original_pixel = [0x11, 0x22, 0x33, 0xff];
        let mutable = one_pixel_image(original_pixel);
        let original_revision = mutable.current_content_hash();
        let mutable_original_key = ImageCacheKey::new(&mutable, original_revision);
        let (original_sprite, _, _) = cache
            .cached_image(&mutable, None, AllowImage::Yes)
            .expect("cache original mutable image");

        // Mutate the cached Arc, but deliberately do not render that object
        // yet. An independent object with the old pixels must not hit the
        // mutable object's now-stale DecodedImage.
        replace_one_pixel(&mutable, [0xaa, 0xbb, 0xcc, 0xff]);
        let changed_revision = mutable.current_content_hash();
        let mutable_changed_key = ImageCacheKey::new(&mutable, changed_revision);

        let same_as_original = one_pixel_image(original_pixel);
        assert_eq!(same_as_original.current_content_hash(), original_revision);
        let independent_original_key = image_cache_key(&same_as_original);
        let (original_again, _, _) = cache
            .cached_image(&same_as_original, None, AllowImage::Yes)
            .expect("old content must not hit the mutable object's changed payload");

        assert_eq!(original_again.coords, original_sprite.coords);
        assert!(cache.image_cache.contains_key(&mutable_original_key));
        assert!(cache.image_cache.contains_key(&independent_original_key));

        let (changed_sprite, _, _) = cache
            .cached_image(&mutable, None, AllowImage::Yes)
            .expect("cache changed mutable image");

        assert_ne!(original_again.coords, changed_sprite.coords);
        assert!(!cache.image_cache.contains_key(&mutable_original_key));
        assert!(cache.image_cache.contains_key(&independent_original_key));
        assert!(cache.image_cache.contains_key(&mutable_changed_key));
        assert_eq!(cache.image_cache_retained_bytes, 8);
        assert_eq!(cache.image_cache_entry_bytes.len(), 2);
    }

    #[test]
    fn equal_content_objects_keep_independent_revision_ownership() {
        let (mut cache, _) = test_glyph_cache();
        let first = one_pixel_image([0x10, 0x20, 0x30, 0xff]);
        let second = one_pixel_image([0x10, 0x20, 0x30, 0xff]);
        let shared_revision = first.current_content_hash();
        assert_eq!(second.current_content_hash(), shared_revision);
        let first_shared_key = ImageCacheKey::new(&first, shared_revision);
        let second_shared_key = ImageCacheKey::new(&second, shared_revision);

        let (first_sprite, _, _) = cache
            .cached_image(&first, None, AllowImage::Yes)
            .expect("cache first equal-content owner");
        let (second_sprite, _, _) = cache
            .cached_image(&second, None, AllowImage::Yes)
            .expect("cache second equal-content owner");
        assert_eq!(first_sprite.coords, second_sprite.coords);
        assert_eq!(cache.image_cache.len(), 2);
        assert_eq!(cache.image_cache_retained_bytes, 8);
        assert_eq!(cache.image_revision_owners.len(), 2);

        replace_one_pixel(&first, [0x40, 0x50, 0x60, 0xff]);
        let first_changed_revision = first.current_content_hash();
        let first_changed_key = ImageCacheKey::new(&first, first_changed_revision);
        cache
            .cached_image(&first, None, AllowImage::Yes)
            .expect("cache first owner's changed revision");
        assert!(!cache.image_cache.contains_key(&first_shared_key));
        assert!(cache.image_cache.contains_key(&second_shared_key));
        assert!(cache.image_cache.contains_key(&first_changed_key));
        assert_eq!(cache.image_revision_owners.len(), 2);

        cache
            .cached_image(&second, None, AllowImage::Yes)
            .expect("unchanged second owner retains its decoded entry");
        assert!(cache.image_cache.contains_key(&second_shared_key));
        assert_eq!(cache.image_revision_owners.len(), 2);

        replace_one_pixel(&second, [0x70, 0x80, 0x90, 0xff]);
        let second_changed_revision = second.current_content_hash();
        let second_changed_key = ImageCacheKey::new(&second, second_changed_revision);
        cache
            .cached_image(&second, None, AllowImage::Yes)
            .expect("cache second owner's changed revision");

        assert!(cache.image_cache.contains_key(&first_changed_key));
        assert!(cache.image_cache.contains_key(&second_changed_key));
        assert!(!cache.image_cache.contains_key(&second_shared_key));
        assert_eq!(cache.image_revision_owners.len(), 2);
        assert_eq!(cache.image_cache_retained_bytes, 8);
        assert_eq!(cache.image_cache_entry_bytes.len(), 2);
    }

    #[test]
    fn stale_weak_owner_cleanup_and_pointer_slot_reuse_are_fail_closed() {
        let (mut cache, _) = test_glyph_cache();

        for value in 0..IMAGE_REVISION_OWNER_PRUNE_INTERVAL {
            let value = u8::try_from(value).expect("prune interval values fit in one byte");
            let image = one_pixel_image([value, 0, 0, 0xff]);
            let revision = image.current_content_hash();
            let _ = cache.bind_image_revision(&image, revision);
        }
        assert!(
            cache.image_revision_owners.len() <= 1,
            "periodic pruning bounds dead Weak identities"
        );
        cache.prune_stale_image_revision_owners();
        assert!(cache.image_revision_owners.is_empty());

        let stale = one_pixel_image([0x11, 0x11, 0x11, 0xff]);
        let stale_weak = Arc::downgrade(&stale);
        let stale_revision = stale.current_content_hash();
        drop(stale);
        assert_eq!(stale_weak.strong_count(), 0);

        let current = one_pixel_image([0x22, 0x22, 0x22, 0xff]);
        let current_key = ImageObjectKey::of(&current);
        let current_revision = current.current_content_hash();
        cache.image_revision_owners.insert(
            current_key,
            ImageRevisionOwner {
                image: stale_weak,
                revision: stale_revision,
                validation_rejected: false,
            },
        );

        let rebound_key = cache.bind_image_revision(&current, current_revision);

        let owner = cache
            .image_revision_owners
            .get(&current_key)
            .expect("current pointer slot is rebound");
        assert_eq!(owner.image.as_ptr(), Arc::as_ptr(&current));
        assert_eq!(owner.revision, current_revision);
        assert_eq!(rebound_key, ImageCacheKey::new(&current, current_revision));
    }

    #[test]
    fn mutable_revision_churn_keeps_one_entry_and_one_exact_byte_total() {
        let (mut cache, _) = test_glyph_cache();
        let image = one_pixel_image([0, 0, 0, 0xff]);
        let mut prior_key = None;

        for value in 0..32u8 {
            replace_one_pixel(&image, [value, value.wrapping_mul(3), 0, 0xff]);
            let revision = image.current_content_hash();
            let key = ImageCacheKey::new(&image, revision);
            cache
                .cached_image(&image, None, AllowImage::Yes)
                .expect("cache churned mutable revision");

            if let Some(prior_key) = prior_key {
                assert!(!cache.image_cache.contains_key(&prior_key));
                assert!(!cache.image_cache_entry_bytes.contains_key(&prior_key));
            }
            assert!(cache.image_cache.contains_key(&key));
            assert_eq!(cache.image_cache.len(), 1);
            assert_eq!(cache.image_cache_retained_bytes, 4);
            assert_eq!(cache.image_cache_entry_bytes.get(&key), Some(&4));
            assert_eq!(cache.image_revision_owners.len(), 1);
            prior_key = Some(key);
        }
    }

    fn small_decoded_image() -> DecodedImage {
        let image = one_pixel_image([0, 0, 0, 0]);
        DecodedImage::load(&image).expect("attested decoded image does not need worker admission")
    }

    #[test]
    fn glyph_image_cache_refuses_single_decoded_image_over_host_budget() {
        let (mut cache, _) = test_glyph_cache();
        let decoded = small_decoded_image();
        let key = cache.bind_image_revision(&decoded.image, decoded.expected_revision);
        assert_eq!(cache.image_revision_owners.len(), 1);

        cache.cache_decoded_image_with_bytes(
            key,
            decoded,
            GlyphCache::image_cache_max_bytes()
                .checked_add(1)
                .unwrap_or(usize::MAX),
        );

        assert_eq!(cache.image_cache.len(), 0);
        assert_eq!(cache.image_cache_retained_bytes, 0);
        assert!(cache.image_cache_entry_bytes.is_empty());
        assert!(cache.image_revision_owners.is_empty());
    }

    #[test]
    fn glyph_image_cache_removes_entry_that_grows_over_host_budget() {
        let (mut cache, _) = test_glyph_cache();
        let decoded = small_decoded_image();
        let key = decoded_image_cache_key(&decoded);

        cache.cache_decoded_image(key, decoded);
        assert_eq!(cache.image_cache.len(), 1);
        assert!(cache.image_cache_retained_bytes > 0);

        cache.refresh_cached_image_bytes(
            key,
            GlyphCache::image_cache_max_bytes()
                .checked_add(1)
                .unwrap_or(usize::MAX),
        );

        assert_eq!(cache.image_cache.len(), 0);
        assert_eq!(cache.image_cache_retained_bytes, 0);
        assert!(cache.image_cache_entry_bytes.is_empty());
    }

    #[test]
    fn glyph_image_cache_evicts_cumulative_decoded_bytes_by_lfu() {
        let (mut cache, _) = test_glyph_cache();
        let hot = small_decoded_image();
        let hot_key = decoded_image_cache_key(&hot);
        let cold = small_decoded_image();
        let cold_key = decoded_image_cache_key(&cold);
        let newest = small_decoded_image();
        let newest_key = decoded_image_cache_key(&newest);

        cache.cache_decoded_image(hot_key, hot);
        cache.cache_decoded_image(cold_key, cold);
        cache.cache_decoded_image(newest_key, newest);

        assert!(cache.image_cache.get(&hot_key).is_some());

        let half_budget = GlyphCache::image_cache_max_bytes() / 2;
        for key in [hot_key, cold_key, newest_key] {
            let _ = cache.image_cache_entry_bytes.insert(key, half_budget);
        }
        cache.image_cache_retained_bytes = half_budget * 3;

        cache.enforce_image_cache_byte_budget();

        assert_eq!(cache.image_cache.len(), 2);
        assert_eq!(cache.image_cache_retained_bytes, half_budget * 2);
        assert!(
            cache.image_cache.get(&hot_key).is_some(),
            "frequently-used decoded image should survive cumulative byte pressure"
        );
        assert_eq!(cache.image_cache_entry_bytes.len(), 2);
    }

    #[test]
    fn glyph_image_cache_repairs_counter_drift_from_authoritative_entries() {
        let (mut cache, _) = test_glyph_cache();
        let decoded = small_decoded_image();
        let key = decoded_image_cache_key(&decoded);
        cache.cache_decoded_image_with_bytes(key, decoded, 4);
        cache.image_cache_retained_bytes = usize::MAX;

        cache.refresh_cached_image_bytes(key, 4);

        assert_eq!(cache.image_cache_retained_bytes, 4);
        assert_eq!(cache.image_cache_entry_bytes.get(&key), Some(&4));
    }

    #[test]
    fn glyph_image_cache_repairs_missing_metadata_from_authoritative_residents() {
        let (mut cache, _) = test_glyph_cache();
        let decoded = small_decoded_image();
        let key = decoded_image_cache_key(&decoded);
        cache.cache_decoded_image_with_bytes(key, decoded, 4);
        assert_eq!(cache.image_cache_entry_bytes.remove(&key), Some(4));

        cache.refresh_cached_image_bytes(key, 4);

        assert_eq!(cache.image_cache.len(), 1);
        assert_eq!(cache.image_cache_retained_bytes, 4);
        assert_eq!(cache.image_cache_entry_bytes.get(&key), Some(&4));
    }

    #[test]
    fn same_object_same_revision_replacement_preserves_current_owner_and_counts_once() {
        let (mut cache, _) = test_glyph_cache();
        let image = one_pixel_image([0x11, 0x22, 0x33, 0xff]);
        let revision = image.current_content_hash();
        let key = cache.bind_image_revision(&image, revision);
        let first = DecodedImage::load(&image).expect("load first decoded value");
        cache.cache_decoded_image_with_bytes(key, first, 4);

        let replacement = DecodedImage::load(&image).expect("load replacement decoded value");
        cache.cache_decoded_image_with_bytes(key, replacement, 4);

        assert_eq!(cache.image_cache.len(), 1);
        assert_eq!(cache.image_cache_retained_bytes, 4);
        assert_eq!(cache.image_cache_entry_bytes.get(&key), Some(&4));
        let owner = cache
            .image_revision_owners
            .get(&key.object)
            .expect("same-object replacement retains current owner binding");
        assert_eq!(owner.image.as_ptr(), Arc::as_ptr(&image));
        assert_eq!(owner.revision, revision);
    }
}
