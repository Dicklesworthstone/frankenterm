use crate::Dimensions;
use crate::color::LinearRgba;
use crate::glyphcache::LoadState;
use crate::quad::{QuadAllocator, QuadTrait};
use crate::termwindow::{RenderState, TermWindowNotif};
use crate::utilsprites::RenderMetrics;
use ::window::WindowOps;
use anyhow::Context;
use config::{
    BackgroundHorizontalAlignment, BackgroundLayer, BackgroundRepeat, BackgroundSize,
    BackgroundSource, BackgroundVerticalAlignment, ConfigHandle, DimensionContext, Gradient,
    GradientOrientation,
};
use std::collections::HashMap;
use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::SystemTime;
use termwiz::image::{
    ImageData, ImageDataType, ImageDataValidationLimits, MAX_IMAGE_WIRE_BYTES,
    MAX_IMAGE_WIRE_FRAMES, MAX_TRUSTED_LOCAL_IMAGE_DECODED_BYTES,
};
use wezterm_term::StableRowIndex;

lazy_static::lazy_static! {
    static ref IMAGE_CACHE: Mutex<BackgroundImageCache> = Mutex::new(BackgroundImageCache::default());
    static ref GRADIENT_CACHE: Mutex<BackgroundGradientCache> = Mutex::new(BackgroundGradientCache::default());
    static ref BACKGROUND_WORK_POOL: Result<rayon::ThreadPool, String> = {
        // Keep decode traffic off the GUI thread without allowing it to occupy
        // multiple performance cores on typical Apple Silicon machines. Very
        // high-core-count workstations can safely prepare two windows at once.
        let available = std::thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        let workers = if available >= 32 { 2 } else { 1 };
        rayon::ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("background-image-{index}"))
            .build()
            .map_err(|error| error.to_string())
    };
}

const MAX_BACKGROUND_DECODED_BYTES: usize = MAX_TRUSTED_LOCAL_IMAGE_DECODED_BYTES;
const MAX_BACKGROUND_CACHE_BYTES: usize = MAX_BACKGROUND_DECODED_BYTES;
const MAX_BACKGROUND_CACHE_ENTRIES: usize = 32;
// Background z-indices occupy the negative i8 range -127..=-1. Keep both
// preparation and rendering inside those slots so background content can
// never spill into the z=0 text layer.
const MAX_ACTIVE_BACKGROUND_LAYERS: usize = 127;
// Cached entries can be evicted while their Arc remains active in a window.
// Bound that active decoded footprint independently of the process caches.
const MAX_ACTIVE_BACKGROUND_DECODED_BYTES: usize = MAX_BACKGROUND_CACHE_BYTES;
const BACKGROUND_IO_CHUNK_BYTES: usize = 64 * 1024;
const MAX_BACKGROUND_TILES_PER_LAYER: usize = 16_384;
#[cfg(test)]
const FIRST_F64_INTEGER_WITHOUT_UNIT_PRECISION: f64 = 9_007_199_254_740_992.0;
const BACKGROUND_IMAGE_VALIDATION_LIMITS: ImageDataValidationLimits = ImageDataValidationLimits {
    max_decoded_bytes: MAX_BACKGROUND_DECODED_BYTES,
    max_frame_count: MAX_IMAGE_WIRE_FRAMES,
    max_width: 16_384,
    max_height: 16_384,
};
static BACKGROUND_POOL_ERROR_REPORTED: AtomicBool = AtomicBool::new(false);
static BACKGROUND_LAYER_LIMIT_REPORTED: AtomicBool = AtomicBool::new(false);
static BACKGROUND_BYTE_LIMIT_REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct BackgroundImageCache {
    entries: HashMap<String, CachedImage>,
    retained_bytes: usize,
    access_clock: u64,
}

impl BackgroundImageCache {
    fn recompute_retained_bytes(&mut self) {
        self.retained_bytes = self
            .entries
            .values()
            .try_fold(0usize, |total, entry| {
                total.checked_add(entry.retained_bytes)
            })
            .unwrap_or(usize::MAX);
    }

    fn next_access(&mut self) -> u64 {
        if self.access_clock == u64::MAX {
            for entry in self.entries.values_mut() {
                entry.last_access /= 2;
            }
            self.access_clock = self
                .entries
                .values()
                .map(|entry| entry.last_access)
                .max()
                .unwrap_or(0);
        }
        self.access_clock += 1;
        self.access_clock
    }

    fn insert(&mut self, path: String, image: CachedImage) {
        self.entries.insert(path, image);
        self.recompute_retained_bytes();
        while self.entries.len() > MAX_BACKGROUND_CACHE_ENTRIES
            || self.retained_bytes > MAX_BACKGROUND_CACHE_BYTES
        {
            let Some(eviction_key) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_access)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if self.entries.remove(&eviction_key).is_none() {
                break;
            }
            self.recompute_retained_bytes();
            log::trace!("evicted background image {eviction_key} from the decoded cache");
        }
    }
}

#[derive(Default)]
struct BackgroundGradientCache {
    entries: Vec<CachedGradient>,
    retained_bytes: usize,
    access_clock: u64,
}

impl BackgroundGradientCache {
    fn recompute_retained_bytes(&mut self) {
        self.retained_bytes = self
            .entries
            .iter()
            .try_fold(0usize, |total, entry| {
                total.checked_add(entry.retained_bytes)
            })
            .unwrap_or(usize::MAX);
    }

    fn next_access(&mut self) -> u64 {
        if self.access_clock == u64::MAX {
            for entry in &mut self.entries {
                entry.last_access /= 2;
            }
            self.access_clock = self
                .entries
                .iter()
                .map(|entry| entry.last_access)
                .max()
                .unwrap_or(0);
        }
        self.access_clock += 1;
        self.access_clock
    }

    fn insert(&mut self, gradient: CachedGradient) {
        self.entries.push(gradient);
        self.recompute_retained_bytes();
        while self.entries.len() > MAX_BACKGROUND_CACHE_ENTRIES
            || self.retained_bytes > MAX_BACKGROUND_CACHE_BYTES
        {
            let Some((index, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.last_access)
            else {
                break;
            };
            self.entries.swap_remove(index);
            self.recompute_retained_bytes();
        }
    }
}

fn checked_background_pixel_bytes(width: u32, height: u32) -> anyhow::Result<usize> {
    anyhow::ensure!(
        width > 0 && height > 0,
        "background dimensions must be non-zero"
    );
    let pixel_bytes = usize::try_from(u128::from(width) * u128::from(height) * 4)
        .context("background dimensions exceed addressable memory")?;
    anyhow::ensure!(
        pixel_bytes <= MAX_BACKGROUND_DECODED_BYTES,
        "background retains {pixel_bytes} bytes, exceeding the {MAX_BACKGROUND_DECODED_BYTES}-byte limit"
    );
    Ok(pixel_bytes)
}

fn bounded_active_background_layer_count(requested: usize) -> usize {
    requested.min(MAX_ACTIVE_BACKGROUND_LAYERS)
}

fn background_image_validation_limits(max_decoded_bytes: usize) -> ImageDataValidationLimits {
    ImageDataValidationLimits {
        max_decoded_bytes: max_decoded_bytes.min(MAX_BACKGROUND_DECODED_BYTES),
        ..BACKGROUND_IMAGE_VALIDATION_LIMITS
    }
}

struct ActiveBackgroundByteBudget {
    limit: usize,
    retained_bytes: usize,
    // Own each admitted allocation for the lifetime of the ledger. That makes
    // pointer identity immune to drop/address-reuse ABA and keeps exact
    // de-duplication self-contained rather than relying on caller ordering.
    sources: HashMap<*const ImageData, (Arc<ImageData>, usize)>,
}

impl ActiveBackgroundByteBudget {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            retained_bytes: 0,
            sources: HashMap::new(),
        }
    }

    fn try_admit(&mut self, source: &Arc<ImageData>, retained_bytes: usize) -> bool {
        let identity = Arc::as_ptr(source);
        if let Some((admitted, admitted_bytes)) = self.sources.get(&identity) {
            return Arc::ptr_eq(admitted, source) && *admitted_bytes == retained_bytes;
        }
        let Some(total) = self.retained_bytes.checked_add(retained_bytes) else {
            return false;
        };
        if total > self.limit {
            return false;
        }
        self.sources
            .insert(identity, (Arc::clone(source), retained_bytes));
        self.retained_bytes = total;
        true
    }
}

/// Publish local validation authority for the exact decoded revision that the
/// renderer will receive. Background preparation already runs on the bounded
/// worker pool, so completing this validation here avoids making the GUI
/// thread present a placeholder while it schedules the same pixel scan again.
fn publish_decoded_background_authority(
    image: ImageData,
    is_cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<(Arc<ImageData>, usize)> {
    let revision = image.current_content_hash();
    if let Some(summary) = image.validated_summary_for_content_revision(
        revision,
        BACKGROUND_IMAGE_VALIDATION_LIMITS,
    ) {
        // Decode and metadata-only speed adjustment publish private authority
        // for their exact revisions. Reuse it rather than hashing the same
        // pixels again before GUI handoff.
        return Ok((Arc::new(image), summary.decoded_bytes));
    }
    let validation = image
        .normalize_for_content_revision_with_limits(
            revision,
            0,
            BACKGROUND_IMAGE_VALIDATION_LIMITS,
            is_cancelled,
        )
        .context("validating decoded background image")?;
    anyhow::ensure!(
        validation.replacement.is_none(),
        "decoded background validation unexpectedly produced a replacement"
    );
    debug_assert_eq!(
        image.validated_summary_for_content_revision(
            revision,
            BACKGROUND_IMAGE_VALIDATION_LIMITS,
        ),
        Some(validation.summary)
    );
    Ok((Arc::new(image), validation.summary.decoded_bytes))
}

fn checked_background_repeat_count(
    span: f32,
    step: f32,
    repeat: BackgroundRepeat,
) -> anyhow::Result<usize> {
    anyhow::ensure!(
        span.is_finite() && span >= 0.0,
        "background repeat span must be finite and non-negative"
    );
    anyhow::ensure!(
        step.is_finite() && step > 0.0,
        "background repeat step must be finite and greater than zero"
    );
    if repeat == BackgroundRepeat::NoRepeat {
        return Ok(usize::from(span > 0.0));
    }
    if span == 0.0 {
        return Ok(0);
    }
    let count = (span / step).ceil();
    anyhow::ensure!(
        count.is_finite() && count <= MAX_BACKGROUND_TILES_PER_LAYER as f32,
        "background repeat count {count} exceeds the per-layer tile limit"
    );
    Ok(count as usize)
}

fn prepare_background_repeat_axis(
    origin: f32,
    lower_bound: f32,
    step: f32,
    repeat: BackgroundRepeat,
    scroll_distance: f64,
) -> anyhow::Result<(f32, bool)> {
    anyhow::ensure!(
        origin.is_finite() && lower_bound.is_finite(),
        "background repeat origin and bound must be finite"
    );
    anyhow::ensure!(
        step.is_finite() && step > 0.0,
        "background repeat step must be finite and greater than zero"
    );
    anyhow::ensure!(
        scroll_distance.is_finite(),
        "background scroll distance must be finite"
    );

    if repeat == BackgroundRepeat::NoRepeat {
        let adjusted = (f64::from(origin) - scroll_distance) as f32;
        anyhow::ensure!(
            adjusted.is_finite(),
            "background no-repeat origin overflowed while applying scroll"
        );
        return Ok((adjusted, false));
    }

    // Keep the coordinate close to the viewport even after long scrollback.
    // The integral tile displacement affects only mirror parity; the
    // fractional displacement supplies the visible sub-tile phase. Stable
    // rows are signed pointer-width indices, so retain f64 precision through
    // this reduction; converting the row to f32 first loses phase and parity
    // beyond 2^24 rows on 64-bit long-session hosts.
    let step = f64::from(step);
    let Some((scroll_is_odd, scroll_remainder)) =
        exact_scroll_tile_phase(scroll_distance, step)
    else {
        anyhow::bail!("background repeat scroll could not be reduced exactly");
    };
    let shifted_origin = f64::from(origin) - scroll_remainder;
    let backward_steps = ((shifted_origin - f64::from(lower_bound)) / step)
        .ceil()
        .max(0.0);
    anyhow::ensure!(
        backward_steps.is_finite()
            && backward_steps <= MAX_BACKGROUND_TILES_PER_LAYER as f64,
        "background repeat alignment exceeds the per-layer tile limit"
    );
    let normalized_origin = (shifted_origin - backward_steps * step) as f32;
    anyhow::ensure!(
        normalized_origin.is_finite(),
        "background repeat origin overflowed while aligning to the viewport"
    );

    let backward_is_odd = backward_steps.rem_euclid(2.0) >= 1.0;
    Ok((
        normalized_origin,
        scroll_is_odd ^ backward_is_odd,
    ))
}

/// Return the exact truncated whole-tile parity and signed sub-tile remainder
/// for two finite binary floating-point values.
///
/// Computing `distance / step` and then multiplying its fractional part back
/// by `step` is not exact even while the quotient is below 2^53. For example,
/// `2^54 / 3` rounds to an integer-valued f64 and erases its exact one-pixel
/// remainder. Decomposing both inputs as binary rationals lets us preserve the
/// only quotient property the renderer consumes (odd/even parity) plus the
/// exact remainder, without materializing an arbitrarily wide whole-tile
/// count. When the distance has the larger exponent, reducing its aligned
/// numerator modulo twice the denominator yields both values: the upper half
/// identifies an odd quotient and the position within that half is the
/// remainder. Fast modular exponentiation covers every finite f64 exponent.
fn exact_scroll_tile_phase(distance: f64, step: f64) -> Option<(bool, f64)> {
    if !distance.is_finite() || !step.is_finite() || step <= 0.0 {
        return None;
    }

    let magnitude = distance.abs();
    if magnitude < step {
        return Some((false, distance));
    }

    let (distance_significand, distance_exponent) = binary_rational_parts(magnitude)?;
    let (step_significand, step_exponent) = binary_rational_parts(step)?;
    let exponent_delta = distance_exponent.checked_sub(step_exponent)?;
    let (quotient_is_odd, remainder_significand, common_exponent) = if exponent_delta >= 0 {
        let denominator = u128::from(step_significand);
        let modulus = denominator.checked_mul(2)?;
        let aligned_numerator_modulus = u128::from(distance_significand)
            .checked_mul(power_of_two_modulo(
                u32::try_from(exponent_delta).ok()?,
                modulus,
            )?)?
            % modulus;
        (
            aligned_numerator_modulus >= denominator,
            aligned_numerator_modulus % denominator,
            step_exponent,
        )
    } else {
        // `magnitude >= step` bounds this aligned denominator by the distance
        // significand, so the negative-delta branch is always small enough to
        // materialize. Keep the checked arithmetic as an invariant guard.
        let denominator = u128::from(step_significand).checked_mul(
            1_u128.checked_shl(exponent_delta.unsigned_abs())?,
        )?;
        let numerator = u128::from(distance_significand);
        (
            (numerator / denominator) % 2 == 1,
            numerator % denominator,
            distance_exponent,
        )
    };
    let remainder = (remainder_significand as f64) * 2.0_f64.powi(common_exponent);
    if !remainder.is_finite() || remainder >= step {
        return None;
    }
    let signed_remainder = if distance.is_sign_negative() {
        -remainder
    } else {
        remainder
    };
    Some((quotient_is_odd, signed_remainder))
}

/// Compute `2^exponent mod modulus` without constructing `2^exponent`.
fn power_of_two_modulo(mut exponent: u32, modulus: u128) -> Option<u128> {
    if modulus == 0 {
        return None;
    }

    let mut result = 1 % modulus;
    let mut base = 2 % modulus;
    while exponent != 0 {
        if exponent & 1 == 1 {
            result = result.checked_mul(base)? % modulus;
        }
        exponent >>= 1;
        if exponent != 0 {
            base = base.checked_mul(base)? % modulus;
        }
    }
    Some(result)
}

/// Decompose a positive finite f64 into an odd integer significand and a
/// base-two exponent, preserving the exact represented value.
fn binary_rational_parts(value: f64) -> Option<(u64, i32)> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    let bits = value.to_bits();
    let exponent_field = ((bits >> 52) & 0x7ff) as u16;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (mut significand, mut exponent) = if exponent_field == 0 {
        (fraction, -1074)
    } else {
        (
            (1_u64 << 52) | fraction,
            i32::from(exponent_field) - 1023 - 52,
        )
    };
    if significand == 0 {
        return None;
    }
    let trailing_zeros = significand.trailing_zeros();
    significand >>= trailing_zeros;
    exponent = exponent.checked_add(i32::try_from(trailing_zeros).ok()?)?;
    Some((significand, exponent))
}

fn background_scroll_distance(
    top: StableRowIndex,
    cell_height: isize,
    factor: f32,
) -> Option<f64> {
    if !factor.is_finite() || cell_height <= 0 {
        return None;
    }
    if factor == 0.0 {
        return Some(0.0);
    }
    let row_pixels = (top as i128).checked_mul(cell_height as i128)?;
    let row_magnitude = row_pixels.unsigned_abs();
    if row_magnitude == 0 {
        return Some(0.0);
    }

    // A fixed magnitude ceiling rejects large values that are nevertheless
    // exactly representable (powers of two and factor=1 are common examples).
    // Strip the integer's powers of two and multiply the two odd
    // significands. The f64 product is exact for this path precisely when the
    // resulting significand fits its 53-bit precision; the stripped powers of
    // two affect only the exponent, and every i128/f32 exponent fits f64.
    let row_significand = row_magnitude >> row_magnitude.trailing_zeros();
    let (factor_significand, _) = binary_rational_parts(f64::from(factor).abs())?;
    let product_significand = row_significand.checked_mul(u128::from(factor_significand))?;
    let product_significant_bits = u128::BITS - product_significand.leading_zeros();
    if product_significant_bits > f64::MANTISSA_DIGITS {
        return None;
    }

    // The significand proof above makes both this integer conversion and the
    // multiplication exact rather than merely finite.
    let distance = (row_pixels as f64) * f64::from(factor);
    distance.is_finite().then_some(distance)
}

fn linear_gradient_projection_half_extent(
    width: f64,
    height: f64,
    angle: f64,
) -> anyhow::Result<f64> {
    anyhow::ensure!(
        width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0,
        "linear-gradient dimensions must be finite and greater than zero"
    );
    anyhow::ensure!(angle.is_finite(), "background gradient angle must be finite");
    let extent = (width * angle.cos().abs() + height * angle.sin().abs()) / 2.0;
    anyhow::ensure!(
        extent.is_finite() && extent > 0.0,
        "linear-gradient projection must be finite and greater than zero"
    );
    Ok(extent)
}

fn radial_gradient_distance(
    x: f64,
    y: f64,
    center_x: f64,
    center_y: f64,
    noise_x: f64,
    noise_y: f64,
    radius: f64,
) -> f64 {
    ((x + noise_x - center_x).powi(2) + (y + noise_y - center_y).powi(2)).sqrt()
        / radius
}

fn background_sources_match(left: &BackgroundSource, right: &BackgroundSource) -> bool {
    match (left, right) {
        (BackgroundSource::Gradient(left), BackgroundSource::Gradient(right)) => left == right,
        (BackgroundSource::Color(left), BackgroundSource::Color(right)) => left == right,
        (BackgroundSource::File(left), BackgroundSource::File(right)) => {
            left.path == right.path && left.speed == right.speed
        }
        _ => false,
    }
}

fn lock_cache<'a, T>(cache: &'a Mutex<T>, label: &str) -> MutexGuard<'a, T> {
    match cache.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("recovering poisoned {label} cache");
            cache.clear_poison();
            poisoned.into_inner()
        }
    }
}

struct CachedGradient {
    g: Gradient,
    width: u32,
    height: u32,
    image: Arc<ImageData>,
    retained_bytes: usize,
    last_access: u64,
}

impl CachedGradient {
    fn compute(
        g: &Gradient,
        width: u32,
        height: u32,
        is_cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Arc<ImageData>> {
        anyhow::ensure!(!is_cancelled(), "background gradient load was superseded");
        let pixel_bytes = checked_background_pixel_bytes(width, height)
            .context("validating background gradient dimensions")?;
        let grad = g
            .build()
            .with_context(|| format!("building gradient {:?}", g))?;

        let mut pixels = Vec::new();
        pixels
            .try_reserve_exact(pixel_bytes)
            .context("reserving background gradient pixels")?;
        pixels.resize(pixel_bytes, 0);
        let mut imgbuf = image::RgbaImage::from_raw(width, height, pixels)
            .context("constructing bounded background gradient image")?;
        let fw = width as f64;
        let fh = height as f64;

        fn to_pixel(c: colorgrad::Color) -> image::Rgba<u8> {
            image::Rgba(c.to_rgba8())
        }

        // Map t which is in range [a, b] to range [c, d]
        fn remap(t: f64, a: f64, b: f64, c: f32, d: f32) -> f32 {
            ((t - a) * (f64::from(d - c) / (b - a)) + f64::from(c)) as f32
        }

        let (dmin, dmax) = grad.domain();
        anyhow::ensure!(
            dmin.is_finite() && dmax.is_finite() && dmax > dmin,
            "background gradient domain must be finite and increasing"
        );

        let mut rng = fastrand::Rng::new();

        // We add some randomness to the position that we use to
        // index into the color gradient, so that we can avoid
        // visible color banding.  The default 64 was selected
        // because it it was the smallest value on my mac where
        // the banding wasn't obvious.
        let noise_amount = g.noise.unwrap_or_else(|| {
            if matches!(g.orientation, GradientOrientation::Radial { .. }) {
                16
            } else {
                64
            }
        });

        fn noise(rng: &mut fastrand::Rng, noise_amount: usize) -> f64 {
            if noise_amount == 0 {
                0.
            } else {
                rng.usize(0..noise_amount) as f64 * -1.
            }
        }

        match g.orientation {
            GradientOrientation::Horizontal => {
                for (index, (x, _, pixel)) in imgbuf.enumerate_pixels_mut().enumerate() {
                    if index.is_multiple_of(4_096) && is_cancelled() {
                        anyhow::bail!("background gradient load was superseded");
                    }
                    *pixel = to_pixel(grad.at(remap(
                        x as f64 + noise(&mut rng, noise_amount),
                        0.0,
                        fw,
                        dmin,
                        dmax,
                    )));
                }
            }
            GradientOrientation::Vertical => {
                for (index, (_, y, pixel)) in imgbuf.enumerate_pixels_mut().enumerate() {
                    if index.is_multiple_of(4_096) && is_cancelled() {
                        anyhow::bail!("background gradient load was superseded");
                    }
                    *pixel = to_pixel(grad.at(remap(
                        y as f64 + noise(&mut rng, noise_amount),
                        0.0,
                        fh,
                        dmin,
                        dmax,
                    )));
                }
            }
            GradientOrientation::Linear { angle } => {
                let angle = angle.unwrap_or(0.0).to_radians();
                let half_extent = linear_gradient_projection_half_extent(fw, fh, angle)?;
                for (index, (x, y, pixel)) in imgbuf.enumerate_pixels_mut().enumerate() {
                    if index.is_multiple_of(4_096) && is_cancelled() {
                        anyhow::bail!("background gradient load was superseded");
                    }
                    let (x, y) = (x as f64, y as f64);
                    let (x, y) = (x - fw / 2., y - fh / 2.);
                    let t = x * f64::cos(angle) - y * f64::sin(angle);
                    *pixel = to_pixel(grad.at(remap(
                        t + noise(&mut rng, noise_amount),
                        -half_extent,
                        half_extent,
                        dmin,
                        dmax,
                    )));
                }
            }
            GradientOrientation::Radial { radius, cx, cy } => {
                let radius = fw * radius.unwrap_or(0.5);
                let cx = fw * cx.unwrap_or(0.5);
                let cy = fh * cy.unwrap_or(0.5);
                anyhow::ensure!(
                    radius.is_finite() && radius > 0.0,
                    "background radial gradient radius must be finite and greater than zero"
                );
                anyhow::ensure!(
                    cx.is_finite() && cy.is_finite(),
                    "background radial gradient center must be finite"
                );

                for (index, (x, y, pixel)) in imgbuf.enumerate_pixels_mut().enumerate() {
                    if index.is_multiple_of(4_096) && is_cancelled() {
                        anyhow::bail!("background gradient load was superseded");
                    }
                    let x = x as f64;
                    let y = y as f64;

                    // If we are close to the center, stop applying noise,
                    // as the noise can wrap around and start using the
                    // color from the other end of the gradient and look weird
                    let nx = if ((cx - x).abs() as usize) < noise_amount {
                        0.
                    } else {
                        noise(&mut rng, noise_amount)
                    };
                    let ny = if ((cy - y).abs() as usize) < noise_amount {
                        0.
                    } else {
                        noise(&mut rng, noise_amount)
                    };

                    let t = radial_gradient_distance(x, y, cx, cy, nx, ny, radius);
                    *pixel = to_pixel(grad.at(t as f32));
                }
            }
        }

        let data = imgbuf.into_vec();
        let image = ImageData::with_data(ImageDataType::new_single_frame(width, height, data));
        let (image, retained_bytes) =
            publish_decoded_background_authority(image, is_cancelled)?;
        debug_assert_eq!(
            retained_bytes,
            checked_background_pixel_bytes(width, height)?
        );

        Ok(image)
    }

    fn load(
        g: &Gradient,
        width: u32,
        height: u32,
        max_decoded_bytes: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Arc<ImageData>> {
        anyhow::ensure!(!is_cancelled(), "background gradient load was superseded");
        let retained_bytes = checked_background_pixel_bytes(width, height)?;
        anyhow::ensure!(
            retained_bytes <= max_decoded_bytes,
            "background gradient retains {retained_bytes} bytes, exceeding the {max_decoded_bytes}-byte active-layer remainder"
        );
        {
            let mut cache = lock_cache(&GRADIENT_CACHE, "gradient");
            let access = cache.next_access();
            if let Some(entry) = cache
                .entries
                .iter_mut()
                .find(|entry| entry.g == *g && entry.width == width && entry.height == height)
            {
                entry.last_access = access;
                return Ok(Arc::clone(&entry.image));
            }
        }

        // Gradient construction is proportional to the target pixel count.
        // Never hold the process-wide cache lock while doing that work.
        let image = Self::compute(g, width, height, is_cancelled)?;
        anyhow::ensure!(!is_cancelled(), "background gradient load was superseded");

        let mut cache = lock_cache(&GRADIENT_CACHE, "gradient");
        let access = cache.next_access();
        if let Some(entry) = cache
            .entries
            .iter_mut()
            .find(|entry| entry.g == *g && entry.width == width && entry.height == height)
        {
            entry.last_access = access;
            return Ok(Arc::clone(&entry.image));
        }
        cache.insert(Self {
            g: g.clone(),
            width,
            height,
            image: Arc::clone(&image),
            retained_bytes,
            last_access: access,
        });
        Ok(image)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackgroundFileStamp {
    modified: SystemTime,
    file_len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    status_change_seconds: i64,
    #[cfg(unix)]
    status_change_nanoseconds: i64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

impl BackgroundFileStamp {
    fn from_metadata(path: &str, metadata: &std::fs::Metadata) -> anyhow::Result<Self> {
        let modified = metadata
            .modified()
            .with_context(|| format!("getting modification time for {path}"))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                modified,
                file_len: metadata.len(),
                device: metadata.dev(),
                inode: metadata.ino(),
                status_change_seconds: metadata.ctime(),
                status_change_nanoseconds: metadata.ctime_nsec(),
            })
        }

        #[cfg(not(unix))]
        Ok(Self {
            modified,
            file_len: metadata.len(),
            created: metadata.created().ok(),
        })
    }
}

struct CachedImage {
    stamp: BackgroundFileStamp,
    image: Arc<ImageData>,
    retained_bytes: usize,
    last_access: u64,
    speed: f32,
}

impl CachedImage {
    fn load(
        path: &str,
        speed: f32,
        max_decoded_bytes: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Arc<ImageData>> {
        anyhow::ensure!(!is_cancelled(), "background image load was superseded");
        if !speed.is_finite() || speed <= 0.0 {
            anyhow::bail!("background image speed must be finite and greater than zero");
        }
        let metadata = std::fs::metadata(path)
            .with_context(|| format!("getting metadata for {}", path))?;
        anyhow::ensure!(
            metadata.file_type().is_file(),
            "background image {path} must be a regular file"
        );
        let stamp = BackgroundFileStamp::from_metadata(path, &metadata)?;
        let file_len = stamp.file_len;
        if file_len > u64::try_from(MAX_IMAGE_WIRE_BYTES).unwrap_or(u64::MAX) {
            anyhow::bail!(
                "background image {path} retains {file_len} encoded bytes, exceeding the {MAX_IMAGE_WIRE_BYTES}-byte limit"
            );
        }
        {
            let mut cache = lock_cache(&IMAGE_CACHE, "image");
            let access = cache.next_access();
            if let Some(cached) = cache.entries.get_mut(path) {
                if cached.stamp == stamp && cached.speed == speed {
                    anyhow::ensure!(
                        cached.retained_bytes <= max_decoded_bytes,
                        "background image {path} retains {} decoded bytes, exceeding the {max_decoded_bytes}-byte active-layer remainder",
                        cached.retained_bytes,
                    );
                    cached.last_access = access;
                    return Ok(Arc::clone(&cached.image));
                }
            }
        }

        // Keep filesystem IO and image decoding outside the global cache lock.
        let mut reader = std::fs::File::open(path)
            .with_context(|| format!("opening window_background_image {path}"))?;
        let opened_metadata = reader
            .metadata()
            .with_context(|| format!("getting opened-file metadata for {path}"))?;
        anyhow::ensure!(
            opened_metadata.file_type().is_file(),
            "background image {path} stopped being a regular file before it was opened"
        );
        let opened_stamp = BackgroundFileStamp::from_metadata(path, &opened_metadata)?;
        anyhow::ensure!(
            opened_stamp == stamp,
            "background image {path} changed before it could be opened"
        );
        let mut encoded = Vec::new();
        encoded
            .try_reserve_exact(usize::try_from(file_len).unwrap_or(MAX_IMAGE_WIRE_BYTES))
            .with_context(|| format!("reserving window_background_image bytes for {path}"))?;
        let mut chunk = [0_u8; BACKGROUND_IO_CHUNK_BYTES];
        loop {
            anyhow::ensure!(!is_cancelled(), "background image load was superseded");
            let read = reader
                .read(&mut chunk)
                .with_context(|| format!("reading window_background_image {path}"))?;
            if read == 0 {
                break;
            }
            let next_len = encoded
                .len()
                .checked_add(read)
                .context("background encoded byte count overflowed")?;
            if next_len > MAX_IMAGE_WIRE_BYTES {
                anyhow::bail!(
                    "background image {path} exceeded the {MAX_IMAGE_WIRE_BYTES}-byte limit while reading"
                );
            }
            if next_len > encoded.capacity() {
                encoded
                    .try_reserve(next_len.saturating_sub(encoded.len()))
                    .with_context(|| {
                        format!("growing window_background_image buffer for {path}")
                    })?;
            }
            encoded.extend_from_slice(&chunk[..read]);
        }
        log::trace!("loaded {}", path);
        let source_revision = ImageDataType::hash_bytes(&encoded);
        let source = ImageData::with_data(ImageDataType::EncodedFile(encoded));
        let normalized = source
            .normalize_for_content_revision_with_limits(
                source_revision,
                MAX_IMAGE_WIRE_BYTES,
                background_image_validation_limits(max_decoded_bytes),
                is_cancelled,
            )
            .with_context(|| format!("decoding window_background_image {path}"))?;
        let decoded = normalized
            .replacement
            .context("encoded background normalization did not return decoded data")?;
        decoded
            .adjust_speed(speed)
            .with_context(|| format!("applying background image speed for {path}"))?;
        // Non-identity animation speed is a metadata-only content mutation;
        // static images and identity speed are exact no-ops. The bounded
        // adjustment validates duration scheduling bounds, preserves pixel
        // hashes, and rebinds the decode-time authority to its final revision.
        // Publication below can therefore avoid a second full pixel scan.
        let (image, retained_bytes) =
            publish_decoded_background_authority(decoded, is_cancelled)?;
        anyhow::ensure!(
            retained_bytes <= max_decoded_bytes,
            "background image {path} retains {retained_bytes} decoded bytes, exceeding the {max_decoded_bytes}-byte active-layer remainder"
        );
        anyhow::ensure!(!is_cancelled(), "background image load was superseded");

        // Bind the decoded bytes to the same opened file generation as the
        // path-level cache stamp. Checking both the still-open descriptor and
        // the path closes replacement-during-open and mutation-during-read
        // races before the pixels become cache authority.
        let decoded_file_stamp = BackgroundFileStamp::from_metadata(
            path,
            &reader
                .metadata()
                .with_context(|| format!("rechecking opened-file metadata for {path}"))?,
        )?;
        anyhow::ensure!(
            decoded_file_stamp == stamp,
            "background image {path} changed while it was being read"
        );

        // A strong metadata generation stamp is the cache authority. On Unix
        // it includes device/inode and status-change time in addition to mtime
        // and length, so same-sized atomic replacement and in-place rewrites
        // cannot silently retain the old pixels. A decode can be expensive, so
        // another reload may win while this thread is outside the lock. Never
        // let the older decode overwrite a newer file generation.
        let current_metadata = std::fs::metadata(path)
            .with_context(|| format!("rechecking metadata for {path}"))?;
        anyhow::ensure!(
            current_metadata.file_type().is_file(),
            "background image {path} stopped being a regular file while decoding"
        );
        let current_stamp = BackgroundFileStamp::from_metadata(path, &current_metadata)?;
        if current_stamp != stamp {
            let mut cache = lock_cache(&IMAGE_CACHE, "image");
            let access = cache.next_access();
            if let Some(cached) = cache.entries.get_mut(path) {
                if cached.stamp == current_stamp && cached.speed == speed {
                    anyhow::ensure!(
                        cached.retained_bytes <= max_decoded_bytes,
                        "background image {path} retains {} decoded bytes, exceeding the {max_decoded_bytes}-byte active-layer remainder",
                        cached.retained_bytes,
                    );
                    cached.last_access = access;
                    return Ok(Arc::clone(&cached.image));
                }
            }
            anyhow::bail!("background image {path} changed while it was being decoded");
        }

        anyhow::ensure!(!is_cancelled(), "background image load was superseded");
        let mut cache = lock_cache(&IMAGE_CACHE, "image");
        let access = cache.next_access();
        if let Some(cached) = cache.entries.get_mut(path) {
            if cached.stamp == stamp && cached.speed == speed {
                anyhow::ensure!(
                    cached.retained_bytes <= max_decoded_bytes,
                    "background image {path} retains {} decoded bytes, exceeding the {max_decoded_bytes}-byte active-layer remainder",
                    cached.retained_bytes,
                );
                cached.last_access = access;
                return Ok(Arc::clone(&cached.image));
            }
        }
        cache.insert(
            path.to_string(),
            Self {
                stamp,
                image: Arc::clone(&image),
                retained_bytes,
                last_access: access,
                speed,
            },
        );

        Ok(image)
    }
}

#[derive(Clone)]
pub struct LoadedBackgroundLayer {
    pub source: Arc<ImageData>,
    pub def: BackgroundLayer,
    retained_bytes: usize,
}

fn resolve_generated_background_axis(size: BackgroundSize, context: DimensionContext) -> u32 {
    (match size {
        // Generated sources don't have an intrinsic image size to preserve, so
        // `contain`/`cover` both mean "fill the available background area".
        BackgroundSize::Contain | BackgroundSize::Cover => context.pixel_max,
        BackgroundSize::Dimension(d) => d.evaluate_as_pixels(context),
    }) as u32
}

fn resolve_generated_background_size(
    width: BackgroundSize,
    height: BackgroundSize,
    h_context: DimensionContext,
    v_context: DimensionContext,
    radial_gradient: bool,
) -> (u32, u32) {
    let mut width = resolve_generated_background_axis(width, h_context);
    let mut height = resolve_generated_background_axis(height, v_context);

    if radial_gradient {
        let size = width.min(height);
        width = size;
        height = size;
    }

    (width, height)
}

fn load_background_layer(
    layer: &BackgroundLayer,
    dimensions: &Dimensions,
    render_metrics: &RenderMetrics,
    max_decoded_bytes: usize,
    is_cancelled: &dyn Fn() -> bool,
) -> anyhow::Result<LoadedBackgroundLayer> {
    anyhow::ensure!(!is_cancelled(), "background layer load was superseded");
    let h_context = DimensionContext {
        dpi: dimensions.dpi as f32,
        pixel_max: dimensions.pixel_width as f32,
        pixel_cell: render_metrics.cell_size.width as f32,
    };
    let v_context = DimensionContext {
        dpi: dimensions.dpi as f32,
        pixel_max: dimensions.pixel_height as f32,
        pixel_cell: render_metrics.cell_size.height as f32,
    };

    let data = match &layer.source {
        BackgroundSource::Gradient(g) => {
            let (width, height) = resolve_generated_background_size(
                layer.width,
                layer.height,
                h_context,
                v_context,
                matches!(g.orientation, GradientOrientation::Radial { .. }),
            );

            CachedGradient::load(g, width, height, max_decoded_bytes, is_cancelled)?
        }
        BackgroundSource::Color(color) => {
            // In theory we could just make a 1x1 texture and allow
            // the shader to stretch it, but if we do that, it'll blend
            // around the edges and look weird.
            // So we make a square texture in the ballpark of the window
            // surface.
            // It's not ideal.
            let (width, height) = resolve_generated_background_size(
                layer.width,
                layer.height,
                h_context,
                v_context,
                false,
            );

            let size = width.min(height);
            let pixel_bytes = checked_background_pixel_bytes(size, size)
                .context("validating solid-color background dimensions")?;
            anyhow::ensure!(
                pixel_bytes <= max_decoded_bytes,
                "solid-color background retains {pixel_bytes} bytes, exceeding the {max_decoded_bytes}-byte active-layer remainder"
            );
            let src_pixel = {
                let (r, g, b, a) = color.to_srgb_u8();
                [r, g, b, a]
            };
            let mut data = Vec::new();
            data.try_reserve_exact(pixel_bytes)
                .context("reserving solid-color background pixels")?;
            data.resize(pixel_bytes, 0);
            for (index, pixel) in data.chunks_exact_mut(4).enumerate() {
                if index.is_multiple_of(4_096) && is_cancelled() {
                    anyhow::bail!("background solid-color load was superseded");
                }
                pixel.copy_from_slice(&src_pixel);
            }
            let image = ImageData::with_data(ImageDataType::new_single_frame(size, size, data));
            publish_decoded_background_authority(image, is_cancelled)?.0
        }
        BackgroundSource::File(source) => {
            CachedImage::load(
                &source.path,
                source.speed,
                max_decoded_bytes,
                is_cancelled,
            )?
        }
    };

    let revision = data.current_content_hash();
    let retained_bytes = data
        .validated_summary_for_content_revision(revision, BACKGROUND_IMAGE_VALIDATION_LIMITS)
        .context("loaded background did not retain exact decoded validation authority")?
        .decoded_bytes;
    anyhow::ensure!(
        retained_bytes <= max_decoded_bytes,
        "background layer retains {retained_bytes} bytes, exceeding the {max_decoded_bytes}-byte active-layer remainder"
    );

    Ok(LoadedBackgroundLayer {
        source: data,
        def: layer.clone(),
        retained_bytes,
    })
}

fn reload_background_image(
    config: &ConfigHandle,
    existing: &[LoadedBackgroundLayer],
    dimensions: &Dimensions,
    render_metrics: &RenderMetrics,
    is_cancelled: &dyn Fn() -> bool,
) -> Vec<LoadedBackgroundLayer> {
    // We want to reuse the existing version of the image where possible
    // so that the textures we may have cached can be re-used and so that
    // animation state can be preserved across the reload.
    let map: HashMap<_, _> = existing
        .iter()
        .map(|layer| (layer.source.current_content_hash(), &layer.source))
        .collect();

    let layer_count = bounded_active_background_layer_count(config.background.len());
    if layer_count < config.background.len() {
        let dropped = config.background.len() - layer_count;
        metrics::counter!("gui.background.layers_rejected.total", "reason" => "layer_limit")
            .increment(u64::try_from(dropped).unwrap_or(u64::MAX));
        if !BACKGROUND_LAYER_LIMIT_REPORTED.swap(true, Ordering::AcqRel) {
            log::error!(
                "background configuration requested {} layers; only the first {MAX_ACTIVE_BACKGROUND_LAYERS} fit the renderer's negative z-index range",
                config.background.len(),
            );
        }
    }

    let mut result = Vec::with_capacity(layer_count);
    let mut active_budget =
        ActiveBackgroundByteBudget::new(MAX_ACTIVE_BACKGROUND_DECODED_BYTES);
    for (index, definition) in config.background.iter().take(layer_count).enumerate() {
        if is_cancelled() {
            return existing.to_vec();
        }

        let candidate = match load_background_layer(
            definition,
            dimensions,
            render_metrics,
            MAX_BACKGROUND_DECODED_BYTES,
            is_cancelled,
        ) {
            Ok(mut layer) => {
                let hash = layer.source.current_content_hash();
                if let Some(existing) = map.get(&hash) {
                    layer.source = Arc::clone(existing);
                }
                Some(layer)
            }
            Err(err) => {
                // Background reload is a prepare/commit operation. A transient
                // file race, decode failure, or allocation failure must not
                // blank a layer that is already on screen.
                if is_cancelled() {
                    return existing.to_vec();
                } else if let Some(previous) = existing.iter().find(|previous| {
                    background_sources_match(&previous.def.source, &definition.source)
                }) {
                    log::error!(
                        "Failed to replace background layer {index}; retaining pixels from the matching prior source: {err:#}"
                    );
                    Some(LoadedBackgroundLayer {
                        source: Arc::clone(&previous.source),
                        def: definition.clone(),
                        retained_bytes: previous.retained_bytes,
                    })
                } else {
                    log::error!("Failed to load background layer {index}: {err:#}");
                    None
                }
            }
        };

        let Some(layer) = candidate else {
            continue;
        };
        if !active_budget.try_admit(&layer.source, layer.retained_bytes) {
            metrics::counter!(
                "gui.background.layers_rejected.total",
                "reason" => "active_decoded_byte_limit",
            )
            .increment(1);
            if !BACKGROUND_BYTE_LIMIT_REPORTED.swap(true, Ordering::AcqRel) {
                log::error!(
                    "background layer {index} would exceed the {MAX_ACTIVE_BACKGROUND_DECODED_BYTES}-byte active decoded-image budget; it and remaining layers were not retained"
                );
            }
            break;
        }
        result.push(layer);
    }

    result
}

#[derive(Clone, Default)]
pub(crate) struct BackgroundLoadCoordinator {
    state: Arc<Mutex<BackgroundLoadState>>,
}

#[derive(Default)]
struct BackgroundLoadState {
    current: Option<Arc<AtomicBool>>,
    active: Option<Arc<AtomicBool>>,
    pending: Option<BackgroundLoadRequest>,
    ready: Option<BackgroundLoadCompletion>,
    worker_running: bool,
}

struct BackgroundLoadRequest {
    cancellation: Arc<AtomicBool>,
    config: ConfigHandle,
    existing: Vec<LoadedBackgroundLayer>,
    dimensions: Dimensions,
    render_metrics: RenderMetrics,
    window: ::window::Window,
}

struct BackgroundLoadCompletion {
    cancellation: Arc<AtomicBool>,
    layers: Vec<LoadedBackgroundLayer>,
}

struct BackgroundWorkerLease {
    coordinator: BackgroundLoadCoordinator,
    armed: bool,
}

impl Drop for BackgroundWorkerLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let restart = {
            let mut state = lock_cache(
                &self.coordinator.state,
                "background load coordinator after worker failure",
            );
            if let Some(active) = state.active.take() {
                active.store(true, Ordering::Release);
                if state
                    .current
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, &active))
                {
                    state.current = None;
                }
                if state
                    .ready
                    .as_ref()
                    .is_some_and(|ready| Arc::ptr_eq(&ready.cancellation, &active))
                {
                    state.ready = None;
                }
            }
            state.worker_running = state.pending.is_some();
            state.worker_running
        };
        metrics::counter!("gui.background.worker_aborted.total").increment(1);
        if restart {
            if let Ok(pool) = &*BACKGROUND_WORK_POOL {
                let coordinator = self.coordinator.clone();
                pool.spawn(move || coordinator.run_worker());
            } else {
                lock_cache(
                    &self.coordinator.state,
                    "background load coordinator after restart failure",
                )
                .worker_running = false;
            }
        }
    }
}

impl BackgroundLoadCoordinator {
    pub(crate) fn request(
        &self,
        config: ConfigHandle,
        existing: Vec<LoadedBackgroundLayer>,
        dimensions: Dimensions,
        render_metrics: RenderMetrics,
        window: ::window::Window,
    ) {
        let pool = match &*BACKGROUND_WORK_POOL {
            Ok(pool) => pool,
            Err(error) => {
                metrics::counter!("gui.background.worker_pool_unavailable.total").increment(1);
                if !BACKGROUND_POOL_ERROR_REPORTED.swap(true, Ordering::AcqRel) {
                    log::error!(
                        "background worker pool is unavailable; retaining prior layers: {error}"
                    );
                }
                return;
            }
        };

        let cancellation = Arc::new(AtomicBool::new(false));
        let request = BackgroundLoadRequest {
            cancellation: Arc::clone(&cancellation),
            config,
            existing,
            dimensions,
            render_metrics,
            window,
        };
        let start_worker = {
            let mut state = lock_cache(&self.state, "background load coordinator");
            if let Some(current) = state.current.replace(Arc::clone(&cancellation)) {
                current.store(true, Ordering::Release);
                metrics::counter!("gui.background.load_superseded.total").increment(1);
            }
            if let Some(ready) = state.ready.take() {
                ready.cancellation.store(true, Ordering::Release);
            }
            if let Some(pending) = state.pending.replace(request) {
                pending.cancellation.store(true, Ordering::Release);
            }
            if let Some(active) = &state.active {
                active.store(true, Ordering::Release);
            }
            if state.worker_running {
                false
            } else {
                state.worker_running = true;
                true
            }
        };

        metrics::counter!("gui.background.load_requested.total").increment(1);
        if start_worker {
            let coordinator = self.clone();
            pool.spawn(move || coordinator.run_worker());
        }
    }

    pub(crate) fn cancel(&self) {
        let mut state = lock_cache(&self.state, "background load coordinator");
        if let Some(current) = state.current.take() {
            current.store(true, Ordering::Release);
        }
        if let Some(active) = state.active.take() {
            active.store(true, Ordering::Release);
        }
        if let Some(pending) = state.pending.take() {
            pending.cancellation.store(true, Ordering::Release);
        }
        if let Some(ready) = state.ready.take() {
            ready.cancellation.store(true, Ordering::Release);
        }
    }

    fn run_worker(&self) {
        let mut worker_lease = BackgroundWorkerLease {
            coordinator: self.clone(),
            armed: true,
        };
        loop {
            let request = {
                let mut state = lock_cache(&self.state, "background load coordinator");
                let Some(request) = state.pending.take() else {
                    state.active = None;
                    state.worker_running = false;
                    break;
                };
                state.active = Some(Arc::clone(&request.cancellation));
                request
            };

            if !request.cancellation.load(Ordering::Acquire) {
                let started = std::time::Instant::now();
                let layers = reload_background_image(
                    &request.config,
                    &request.existing,
                    &request.dimensions,
                    &request.render_metrics,
                    &|| request.cancellation.load(Ordering::Acquire),
                );
                metrics::histogram!("gui.background.load_duration").record(started.elapsed());

                if !request.cancellation.load(Ordering::Acquire) {
                    let coordinator = self.clone();
                    let cancellation = Arc::clone(&request.cancellation);
                    let should_notify = {
                        let mut state =
                            lock_cache(&self.state, "background load coordinator completion");
                        if request.cancellation.load(Ordering::Acquire)
                            || !state
                                .current
                                .as_ref()
                                .is_some_and(|current| Arc::ptr_eq(current, &cancellation))
                        {
                            false
                        } else {
                            if let Some(stale) = state.ready.replace(BackgroundLoadCompletion {
                                cancellation: Arc::clone(&cancellation),
                                layers,
                            }) {
                                stale.cancellation.store(true, Ordering::Release);
                            }
                            true
                        }
                    };
                    if should_notify {
                        request.window.notify(TermWindowNotif::Apply(Box::new(
                            move |term_window| {
                                let Some(layers) =
                                    coordinator.take_ready_if_current(&cancellation)
                                else {
                                    metrics::counter!("gui.background.stale_completion.total")
                                        .increment(1);
                                    return;
                                };
                                term_window.window_background = layers;
                                if let Some(window) = term_window.window.as_ref() {
                                    window.invalidate();
                                }
                                metrics::counter!("gui.background.load_committed.total")
                                    .increment(1);
                            },
                        )));
                    } else {
                        metrics::counter!("gui.background.stale_completion.total").increment(1);
                    }
                }
            }

            let mut state = lock_cache(&self.state, "background load coordinator");
            if state
                .active
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &request.cancellation))
            {
                state.active = None;
            }
            if state.pending.is_none() {
                state.worker_running = false;
                break;
            }
        }
        worker_lease.armed = false;
    }

    fn take_ready_if_current(
        &self,
        cancellation: &Arc<AtomicBool>,
    ) -> Option<Vec<LoadedBackgroundLayer>> {
        let mut state = lock_cache(&self.state, "background load coordinator");
        if cancellation.load(Ordering::Acquire)
            || !state
                .current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, cancellation))
        {
            return None;
        }
        let ready = state.ready.take()?;
        if !Arc::ptr_eq(&ready.cancellation, cancellation) {
            state.ready = Some(ready);
            return None;
        }
        state.current = None;
        Some(ready.layers)
    }
}

impl crate::TermWindow {
    pub(crate) fn schedule_background_reload(&mut self) {
        let Some(window) = self.window.as_ref().cloned() else {
            return;
        };
        self.background_load.request(
            self.config.clone(),
            self.window_background.clone(),
            self.dimensions,
            self.render_metrics,
            window,
        );
    }

    pub fn render_backgrounds(
        &self,
        bg_color: LinearRgba,
        top: StableRowIndex,
    ) -> anyhow::Result<bool> {
        let gl_state = self
            .render_state
            .as_ref()
            .context("render state is not initialized")?;
        let mut layer_idx = -127;
        let mut loaded_any = false;
        for layer in self
            .window_background
            .iter()
            .take(MAX_ACTIVE_BACKGROUND_LAYERS)
        {
            if self.render_background(gl_state, bg_color, layer, layer_idx, top)? {
                loaded_any = true;
                layer_idx = layer_idx.saturating_add(1);
            }
        }
        Ok(loaded_any)
    }

    fn render_background(
        &self,
        gl_state: &RenderState,
        bg_color: LinearRgba,
        layer: &LoadedBackgroundLayer,
        layer_index: i8,
        top: StableRowIndex,
    ) -> anyhow::Result<bool> {
        let render_layer = gl_state.layer_for_zindex(layer_index)?;
        let vbs = render_layer.vb.borrow();
        let mut layer0 = vbs[0].map();

        // Compose per-layer opacity with the global window opacity. Without
        // this multiplication, a config that sets both `window_background_opacity
        // = 0.85` AND `config.background = { { opacity = 0.92, ... } }` would
        // get a window that's effectively 92% opaque (the only translucency
        // applied), instead of 92% × 85% ≈ 78% (the wezterm behavior).
        // The fallback no-layer path at paint.rs::paint_impl already applies
        // `window_background_opacity` to the terminal background; this brings
        // the layered path into parity so the wezterm-style config-
        // composition rules hold.
        let color = bg_color
            .mul_alpha(layer.def.opacity)
            .mul_alpha(self.config.window_background_opacity);

        let (sprite, next_due, load_state) = gl_state.glyph_cache.borrow_mut().cached_image(
            &layer.source,
            None,
            self.allow_images,
        )?;
        self.update_next_frame_time(next_due);

        if load_state != LoadState::Loaded {
            return Ok(false);
        }

        let pixel_width = self.dimensions.pixel_width as f32;
        let pixel_height = self.dimensions.pixel_height as f32;
        let tex_width = sprite.coords.width() as f32;
        let tex_height = sprite.coords.height() as f32;
        if !pixel_width.is_finite()
            || !pixel_height.is_finite()
            || !tex_width.is_finite()
            || !tex_height.is_finite()
            || pixel_width <= 0.0
            || pixel_height <= 0.0
            || tex_width <= 0.0
            || tex_height <= 0.0
        {
            metrics::counter!(
                "gui.background.layer_rejected.total",
                "reason" => "invalid_surface_geometry",
            )
            .increment(1);
            return Ok(false);
        }

        let scale_width = pixel_width / tex_width as f32;
        let scale_height = pixel_height / tex_height as f32;

        let h_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: pixel_width,
            pixel_cell: self.render_metrics.cell_size.width as f32,
        };
        let v_context = DimensionContext {
            dpi: self.dimensions.dpi as f32,
            pixel_max: pixel_height,
            pixel_cell: self.render_metrics.cell_size.height as f32,
        };

        // log::info!("tex {tex_width}x{tex_height} aspect={aspect}");

        // Compute the smallest aspect-preserved size that will fit the space
        let (min_aspect_width, min_aspect_height) = {
            let scale = scale_width.min(scale_height);
            (tex_width * scale, tex_height * scale)
        };
        // Compute the largest aspect-preserved size that will fill the space
        let (max_aspect_width, max_aspect_height) = {
            let scale = scale_width.max(scale_height);
            (tex_width * scale, tex_height * scale)
        };

        let width = match layer.def.width {
            BackgroundSize::Contain => min_aspect_width as f32,
            BackgroundSize::Cover => max_aspect_width as f32,
            BackgroundSize::Dimension(n) => n.evaluate_as_pixels(h_context),
        };

        let height = match layer.def.height {
            BackgroundSize::Contain => min_aspect_height as f32,
            BackgroundSize::Cover => max_aspect_height as f32,
            BackgroundSize::Dimension(n) => n.evaluate_as_pixels(v_context),
        };
        if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
            metrics::counter!(
                "gui.background.layer_rejected.total",
                "reason" => "invalid_layer_geometry",
            )
            .increment(1);
            return Ok(false);
        }

        let mut origin_x = pixel_width / -2.;
        let top_pixel = pixel_height / -2.;
        let mut origin_y = top_pixel;

        match layer.def.vertical_align {
            BackgroundVerticalAlignment::Top => {}
            BackgroundVerticalAlignment::Bottom => {
                origin_y += pixel_height - height;
            }
            BackgroundVerticalAlignment::Middle => {
                origin_y += (pixel_height - height) / 2.;
            }
        }
        match layer.def.horizontal_align {
            BackgroundHorizontalAlignment::Left => {}
            BackgroundHorizontalAlignment::Right => {
                origin_x += pixel_width - width;
            }
            BackgroundHorizontalAlignment::Center => {
                origin_x += (pixel_width - width) / 2.;
            }
        }

        let vertical_offset = layer
            .def
            .vertical_offset
            .map(|d| d.evaluate_as_pixels(v_context))
            .unwrap_or(0.);
        origin_y += vertical_offset;

        let horizontal_offset = layer
            .def
            .horizontal_offset
            .map(|d| d.evaluate_as_pixels(h_context))
            .unwrap_or(0.);
        if !vertical_offset.is_finite() || !horizontal_offset.is_finite() {
            metrics::counter!(
                "gui.background.layer_rejected.total",
                "reason" => "invalid_offset",
            )
            .increment(1);
            return Ok(false);
        }
        origin_x += horizontal_offset;

        let repeat_x = layer
            .def
            .repeat_x_size
            .map(|size| size.evaluate_as_pixels(h_context))
            .unwrap_or(width);
        let repeat_y = layer
            .def
            .repeat_y_size
            .map(|size| size.evaluate_as_pixels(v_context))
            .unwrap_or(height);

        // log::info!("computed {width}x{height}");

        let left_pixel = pixel_width / -2.0;
        let right_pixel = left_pixel + pixel_width;
        let limit_y = top_pixel + pixel_height;
        let scroll_distance = if let Some(factor) = layer.def.attachment.scroll_factor() {
            let Some(distance) = background_scroll_distance(
                top,
                self.render_metrics.cell_size.height,
                factor,
            ) else {
                metrics::counter!(
                    "gui.background.layer_rejected.total",
                    "reason" => "invalid_scroll_distance",
                )
                .increment(1);
                return Ok(false);
            };
            distance
        } else {
            0.0
        };
        let (origin_x, first_x_mirrored) = match prepare_background_repeat_axis(
            origin_x,
            left_pixel,
            repeat_x,
            layer.def.repeat_x,
            0.0,
        ) {
            Ok(prepared) => prepared,
            Err(_) => {
                metrics::counter!(
                    "gui.background.layer_rejected.total",
                    "reason" => "invalid_horizontal_repeat",
                )
                .increment(1);
                return Ok(false);
            }
        };
        let (origin_y, first_y_mirrored) = match prepare_background_repeat_axis(
            origin_y,
            top_pixel,
            repeat_y,
            layer.def.repeat_y,
            scroll_distance,
        ) {
            Ok(prepared) => prepared,
            Err(_) => {
                metrics::counter!(
                    "gui.background.layer_rejected.total",
                    "reason" => "invalid_vertical_repeat",
                )
                .increment(1);
                return Ok(false);
            }
        };
        let x_count = match checked_background_repeat_count(
            (right_pixel - origin_x).max(0.0),
            repeat_x,
            layer.def.repeat_x,
        ) {
            Ok(count) => count,
            Err(_) => {
                metrics::counter!(
                    "gui.background.layer_rejected.total",
                    "reason" => "invalid_horizontal_repeat",
                )
                .increment(1);
                return Ok(false);
            }
        };
        let y_count = match checked_background_repeat_count(
            (limit_y - origin_y).max(0.0),
            repeat_y,
            layer.def.repeat_y,
        ) {
            Ok(count) => count,
            Err(_) => {
                metrics::counter!(
                    "gui.background.layer_rejected.total",
                    "reason" => "invalid_vertical_repeat",
                )
                .increment(1);
                return Ok(false);
            }
        };
        if x_count
            .checked_mul(y_count)
            .is_none_or(|tiles| tiles > MAX_BACKGROUND_TILES_PER_LAYER)
        {
            metrics::counter!(
                "gui.background.layer_rejected.total",
                "reason" => "tile_budget",
            )
            .increment(1);
            return Ok(false);
        }

        let mut emitted = false;

        for y_offset in 0..y_count {
            let offset_y = y_offset as f32 * repeat_y;
            let origin_y = origin_y + offset_y;
            if origin_y >= limit_y {
                break;
            }

            for x_step in 0..x_count {
                let offset_x = x_step as f32 * repeat_x;
                let origin_x = origin_x + offset_x;
                if origin_x >= right_pixel {
                    break;
                }
                let mut quad = layer0.allocate()?;
                emitted = true;
                // log::info!("quad {origin_x},{origin_y} {width}x{height}");
                quad.set_position(origin_x, origin_y, origin_x + width, origin_y + height);

                let coords = sprite.texture_coords();
                let mut x1 = coords.min_x();
                let mut x2 = coords.max_x();
                let mut y1 = coords.min_y();
                let mut y2 = coords.max_y();
                if layer.def.repeat_x == BackgroundRepeat::Mirror
                    && (first_x_mirrored ^ (x_step % 2 == 1))
                {
                    std::mem::swap(&mut x1, &mut x2);
                }
                if layer.def.repeat_y == BackgroundRepeat::Mirror
                    && (first_y_mirrored ^ (y_offset % 2 == 1))
                {
                    std::mem::swap(&mut y1, &mut y2);
                }

                quad.set_texture_discrete(x1, x2, y1, y2);
                quad.set_is_background_image();
                quad.set_hsv(Some(layer.def.hsb));
                quad.set_fg_color(color);
            }
        }

        Ok(emitted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use config::Dimension;
    use std::time::Duration;

    fn cached_image(last_access: u64, retained_bytes: usize) -> CachedImage {
        CachedImage {
            stamp: BackgroundFileStamp {
                modified: SystemTime::UNIX_EPOCH,
                file_len: 4,
                #[cfg(unix)]
                device: 0,
                #[cfg(unix)]
                inode: 0,
                #[cfg(unix)]
                status_change_seconds: 0,
                #[cfg(unix)]
                status_change_nanoseconds: 0,
                #[cfg(not(unix))]
                created: None,
            },
            image: Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
                1,
                1,
                vec![0, 0, 0, 255],
            ))),
            retained_bytes,
            last_access,
            speed: 1.0,
        }
    }

    fn context(pixel_max: f32, pixel_cell: f32) -> DimensionContext {
        DimensionContext {
            dpi: 96.0,
            pixel_max,
            pixel_cell,
        }
    }

    #[test]
    fn generated_background_pixel_budget_rejects_zero_and_oversized_dimensions() {
        assert!(checked_background_pixel_bytes(0, 1).is_err());
        assert!(checked_background_pixel_bytes(1, 0).is_err());
        assert_eq!(checked_background_pixel_bytes(1, 1).unwrap(), 4);
        assert!(checked_background_pixel_bytes(16_384, 16_384).is_err());
    }

    #[test]
    fn active_background_layer_count_stays_inside_negative_z_slots() {
        assert_eq!(bounded_active_background_layer_count(0), 0);
        assert_eq!(
            bounded_active_background_layer_count(MAX_ACTIVE_BACKGROUND_LAYERS),
            MAX_ACTIVE_BACKGROUND_LAYERS,
        );
        assert_eq!(
            bounded_active_background_layer_count(MAX_ACTIVE_BACKGROUND_LAYERS + 1),
            MAX_ACTIVE_BACKGROUND_LAYERS,
        );
        assert_eq!(
            -127_i16 + (MAX_ACTIVE_BACKGROUND_LAYERS as i16 - 1),
            -1,
            "the last admitted background must remain below the z=0 text layer",
        );
    }

    #[test]
    fn radial_gradient_noise_offsets_both_axes_before_distance_is_squared() {
        let distance = radial_gradient_distance(10.0, 20.0, 0.0, 0.0, -3.0, -4.0, 2.0);
        let transposed =
            radial_gradient_distance(20.0, 10.0, 0.0, 0.0, -4.0, -3.0, 2.0);
        let expected = (7.0_f64.powi(2) + 16.0_f64.powi(2)).sqrt() / 2.0;

        assert!((distance - expected).abs() <= f64::EPSILON);
        assert!((transposed - expected).abs() <= f64::EPSILON);
    }

    #[test]
    fn active_background_byte_budget_counts_shared_arcs_once_and_distinct_arcs_exactly() {
        let shared = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0, 0, 0, 0xff],
        )));
        let distinct = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![1, 1, 1, 0xff],
        )));
        let overflow = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![2, 2, 2, 0xff],
        )));
        let mut budget = ActiveBackgroundByteBudget::new(8);
        let shared_alias = Arc::clone(&shared);

        assert!(budget.try_admit(&shared, 4));
        assert!(budget.try_admit(&shared_alias, 4));
        assert_eq!(budget.retained_bytes, 4, "a cloned Arc is not new pixel memory");
        assert!(budget.try_admit(&distinct, 4));
        assert_eq!(budget.retained_bytes, 8);
        assert!(!budget.try_admit(&overflow, 4));
        assert_eq!(budget.retained_bytes, 8, "a rejected Arc changes no authority");
    }

    #[test]
    fn active_background_byte_budget_owns_identity_and_rejects_inconsistent_accounting() {
        let source = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0, 0, 0, 0xff],
        )));
        let weak = Arc::downgrade(&source);
        let mut budget = ActiveBackgroundByteBudget::new(4);

        assert!(budget.try_admit(&source, 4));
        assert!(
            !budget.try_admit(&source, 3),
            "one allocation cannot carry two exact retained-byte authorities"
        );
        drop(source);
        assert!(
            weak.upgrade().is_some(),
            "the ledger must retain admitted identity until the pass ends"
        );
        assert_eq!(budget.retained_bytes, 4);
    }

    #[test]
    fn decoded_background_publication_carries_exact_renderer_authority() {
        let image = ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![0x11, 0x22, 0x33, 0xff],
        ));
        let revision = image.current_content_hash();
        assert!(
            image
                .validated_summary_for_content_revision(
                    revision,
                    BACKGROUND_IMAGE_VALIDATION_LIMITS,
                )
                .is_none(),
            "an unvalidated decoded image must not start with renderer authority"
        );

        let (image, retained_bytes) =
            publish_decoded_background_authority(image, &|| false).unwrap();
        assert_eq!(retained_bytes, 4);
        assert_eq!(
            image.validated_summary_for_content_revision(
                image.current_content_hash(),
                BACKGROUND_IMAGE_VALIDATION_LIMITS,
            ),
            Some(termwiz::image::ImageDataValidationSummary {
                decoded_bytes: 4,
                frame_count: 1,
            })
        );
    }

    #[test]
    fn animation_speed_metadata_mutation_rebinds_background_authority() {
        let first = vec![0x10, 0x20, 0x30, 0xff];
        let second = vec![0x40, 0x50, 0x60, 0xff];
        let image = ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
            durations: vec![Duration::from_millis(100), Duration::from_millis(200)],
        });
        let original_revision = image.current_content_hash();
        image
            .normalize_for_content_revision_with_limits(
                original_revision,
                0,
                BACKGROUND_IMAGE_VALIDATION_LIMITS,
                &|| false,
            )
            .unwrap();
        image.adjust_speed(2.0).unwrap();
        let adjusted_revision = image.current_content_hash();
        assert_ne!(adjusted_revision, original_revision);
        assert_eq!(
            image.validated_summary_for_content_revision(
                adjusted_revision,
                BACKGROUND_IMAGE_VALIDATION_LIMITS,
            ),
            Some(termwiz::image::ImageDataValidationSummary {
                decoded_bytes: 8,
                frame_count: 2,
            }),
            "metadata-only speed adjustment must rebind authority without rescanning pixels"
        );

        let (image, retained_bytes) =
            publish_decoded_background_authority(image, &|| false).unwrap();
        assert_eq!(retained_bytes, 8);
        assert_eq!(image.current_content_hash(), adjusted_revision);
        assert_eq!(
            image.validated_summary_for_content_revision(
                adjusted_revision,
                BACKGROUND_IMAGE_VALIDATION_LIMITS,
            ),
            Some(termwiz::image::ImageDataValidationSummary {
                decoded_bytes: 8,
                frame_count: 2,
            })
        );
    }

    #[test]
    fn background_repeat_count_rejects_zero_nan_and_pathological_tile_counts() {
        assert!(
            checked_background_repeat_count(100.0, 0.0, BackgroundRepeat::Repeat).is_err()
        );
        assert!(
            checked_background_repeat_count(f32::NAN, 1.0, BackgroundRepeat::Repeat).is_err()
        );
        assert!(
            checked_background_repeat_count(
                MAX_BACKGROUND_TILES_PER_LAYER as f32 + 1.0,
                1.0,
                BackgroundRepeat::Mirror,
            )
            .is_err()
        );
        assert_eq!(
            checked_background_repeat_count(100.0, 30.0, BackgroundRepeat::Repeat).unwrap(),
            4
        );
        assert_eq!(
            checked_background_repeat_count(100.0, 30.0, BackgroundRepeat::NoRepeat).unwrap(),
            1
        );
    }

    #[test]
    fn repeated_background_origin_extends_backward_and_preserves_mirror_parity() {
        let (origin, mirrored) = prepare_background_repeat_axis(
            400.0,
            -500.0,
            100.0,
            BackgroundRepeat::Mirror,
            0.0,
        )
        .unwrap();
        assert_eq!(origin, -500.0);
        assert!(mirrored, "nine backward tiles invert mirror parity");

        let (scrolled_origin, scrolled_mirrored) = prepare_background_repeat_axis(
            -500.0,
            -500.0,
            100.0,
            BackgroundRepeat::Mirror,
            -20.0,
        )
        .unwrap();
        assert_eq!(scrolled_origin, -580.0);
        assert!(scrolled_mirrored);
    }

    #[test]
    fn repeated_background_scroll_retains_phase_and_parity_beyond_f32_integer_precision() {
        let former_f32_boundary = 16_777_216.0_f64;
        let (origin, mirrored) = prepare_background_repeat_axis(
            0.0,
            0.0,
            1.0,
            BackgroundRepeat::Mirror,
            former_f32_boundary + 1.25,
        )
        .unwrap();

        assert_eq!(origin, -0.25);
        assert!(mirrored, "the 16,777,217th tile has odd mirror parity");
        for repeat in [BackgroundRepeat::Repeat, BackgroundRepeat::Mirror] {
            assert_eq!(
                prepare_background_repeat_axis(
                    0.0,
                    0.0,
                    1.0,
                    repeat,
                    FIRST_F64_INTEGER_WITHOUT_UNIT_PRECISION,
                )
                .unwrap(),
                (0.0, false),
                "exact rational reduction must not impose a false 2^53 quotient ceiling",
            );
        }
    }

    #[test]
    fn repeated_background_scroll_uses_exact_binary_remainder_beyond_two_pow_53() {
        let distance = 2.0_f64.powi(54);
        let (quotient_is_odd, remainder) = exact_scroll_tile_phase(distance, 3.0)
            .expect("the exact tile parity and remainder are representable");
        assert!(quotient_is_odd);
        assert_eq!(remainder, 1.0);

        let (origin, mirrored) = prepare_background_repeat_axis(
            0.0,
            0.0,
            3.0,
            BackgroundRepeat::Mirror,
            distance,
        )
        .expect("exact binary reduction must retain the one-pixel phase");
        assert_eq!(origin, -1.0);
        assert!(mirrored, "the exact whole-tile quotient is odd");

        let (negative_is_odd, negative_remainder) =
            exact_scroll_tile_phase(-distance, 3.0)
                .expect("negative scrolling uses the same exact magnitude");
        assert_eq!(negative_is_odd, quotient_is_odd);
        assert_eq!(negative_remainder, -1.0);
    }

    #[test]
    fn exact_scroll_tile_phase_rejects_invalid_inputs_without_a_quotient_ceiling() {
        assert_eq!(exact_scroll_tile_phase(0.0, 3.0), Some((false, 0.0)));
        assert_eq!(exact_scroll_tile_phase(1.0, 3.0), Some((false, 1.0)));
        assert_eq!(exact_scroll_tile_phase(-1.0, 3.0), Some((false, -1.0)));
        assert_eq!(exact_scroll_tile_phase(f64::NAN, 3.0), None);
        assert_eq!(exact_scroll_tile_phase(1.0, 0.0), None);
        assert_eq!(exact_scroll_tile_phase(1.0, f64::INFINITY), None);
        assert_eq!(
            exact_scroll_tile_phase(FIRST_F64_INTEGER_WITHOUT_UNIT_PRECISION, 1.0),
            Some((false, 0.0)),
        );
        assert_eq!(
            exact_scroll_tile_phase(2.0_f64.powi(63), 3.0),
            Some((false, 2.0)),
            "an exact nonzero remainder remains representable beyond 2^53 tiles",
        );
        assert_eq!(
            exact_scroll_tile_phase(2.0_f64.powi(64) - 2.0_f64.powi(11), 1.0),
            Some((false, 0.0)),
            "high-bit alignment retains parity without materializing the quotient",
        );
        assert_eq!(
            exact_scroll_tile_phase(2.0_f64.powi(64), 1.0),
            Some((false, 0.0)),
            "tile counts above u64 are admissible because only parity is consumed",
        );

        // This exact production-shaped pair needs a 129-bit aligned numerator.
        // A materialized u128 path either rejected it or, with checked_shl
        // alone, silently discarded its high bit. Modular alignment preserves
        // its even quotient and exact remainder instead. The step is an
        // exactly promoted finite f32, matching the production contract.
        let overflowing_distance = f64::from_bits((1075_u64 << 52) | 1);
        let tiny_f32_step = f32::from_bits((74_u32 << 23) | ((1_u32 << 23) - 1));
        assert_eq!(
            exact_scroll_tile_phase(overflowing_distance, f64::from(tiny_f32_step)),
            Some((false, 17.0 * 2.0_f64.powi(-72))),
        );
    }

    #[test]
    fn exact_scroll_tile_phase_uses_modular_alignment_beyond_u128() {
        let promoted_f32_step = f64::from(3.0_f32 * 2.0_f32.powi(-40));
        assert_eq!(
            exact_scroll_tile_phase(2.0_f64.powi(100), promoted_f32_step),
            Some((true, 2.0_f64.powi(-40))),
            "a 140-bit exponent delta must retain odd parity and exact phase",
        );
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn repeated_background_scroll_accepts_production_two_pow_68_distance() {
        let top: StableRowIndex = 1_isize << 62;
        let distance = background_scroll_distance(top, 64, 1.0)
            .expect("the exact power-of-two scroll distance must be admitted");
        assert_eq!(distance, 2.0_f64.powi(68));
        assert_eq!(
            prepare_background_repeat_axis(
                0.0,
                0.0,
                1.0,
                BackgroundRepeat::Mirror,
                distance,
            )
            .expect("mirror parity does not require materializing the tile count"),
            (0.0, false),
        );
    }

    #[test]
    fn background_scroll_distance_enforces_the_exact_product_significand_budget() {
        assert_eq!(
            background_scroll_distance(16_777_217, 1, 1.25),
            Some(20_971_521.25),
        );
        assert_eq!(background_scroll_distance(1, 1, f32::NAN), None);
        assert_eq!(background_scroll_distance(1, 0, 1.0), None);
        assert_eq!(
            background_scroll_distance(536_870_912, 1, 1.0),
            Some(536_870_912.0),
        );
        assert_eq!(
            background_scroll_distance(536_870_913, 1, 1.0),
            Some(536_870_913.0),
            "a factor with a one-bit significand must not impose a false 2^29 ceiling",
        );
        assert_eq!(
            background_scroll_distance(-536_870_913, 1, 1.0),
            Some(-536_870_913.0),
            "the exactness proof is sign-symmetric",
        );

        if let Ok(large_power_of_two) = isize::try_from(1_i64 << 62) {
            assert_eq!(
                background_scroll_distance(large_power_of_two, 1, 0.5),
                Some((1_u64 << 61) as f64),
                "large exponents remain exact when the odd significand is one bit",
            );
        }

        if let Ok(first_inexact_isize) = isize::try_from(9_007_199_254_740_993_i64) {
            assert_eq!(
                background_scroll_distance(first_inexact_isize, 1, 0.5),
                None,
                "a small power-of-two factor cannot recover a lost 54th significand bit",
            );
        }

        if let Ok(rounding_product) = isize::try_from(4_503_599_627_370_497_i64) {
            assert_eq!(
                background_scroll_distance(rounding_product, 1, 1.5),
                None,
                "two exact operands whose product needs 54 bits must fail closed",
            );
        }

        if let Ok(top) = isize::try_from(9_007_199_254_740_991_i64) {
            let distance = background_scroll_distance(top, 2, 1.0)
                .expect("the 53-bit odd significand and trailing factor remain exact");
            assert_eq!(distance, 18_014_398_509_481_982.0);
            let prepared = prepare_background_repeat_axis(
                0.0,
                0.0,
                4.0,
                BackgroundRepeat::Repeat,
                distance,
            )
            .ok();
            assert_eq!(
                prepared,
                Some((-2.0, true)),
                "exact large row products retain their two-pixel phase and odd parity",
            );
        }
    }

    #[test]
    fn backward_extended_repeat_still_covers_the_trailing_viewport_edge() {
        let viewport_left = 0.0;
        let viewport_right = 500.0;
        let step = 100.0;
        let (origin, _) = prepare_background_repeat_axis(
            20.0,
            viewport_left,
            step,
            BackgroundRepeat::Repeat,
            0.0,
        )
        .unwrap();
        assert_eq!(origin, -80.0);

        let count = checked_background_repeat_count(
            viewport_right - origin,
            step,
            BackgroundRepeat::Repeat,
        )
        .unwrap();
        let last_origin = (0..count)
            .map(|index| origin + index as f32 * step)
            .take_while(|tile_origin| *tile_origin < viewport_right)
            .last()
            .expect("the repeated background must emit at least one tile");

        assert_eq!(last_origin, 420.0);
        assert!(last_origin + step >= viewport_right);
    }

    #[test]
    fn no_repeat_background_applies_the_full_scroll_distance() {
        let (origin, mirrored) = prepare_background_repeat_axis(
            -500.0,
            -500.0,
            100.0,
            BackgroundRepeat::NoRepeat,
            250.0,
        )
        .unwrap();
        assert_eq!(origin, -750.0);
        assert!(!mirrored);
    }

    #[test]
    fn linear_gradient_projection_uses_both_window_axes() {
        let horizontal = linear_gradient_projection_half_extent(640.0, 480.0, 0.0).unwrap();
        let vertical =
            linear_gradient_projection_half_extent(640.0, 480.0, std::f64::consts::FRAC_PI_2)
                .unwrap();
        let diagonal =
            linear_gradient_projection_half_extent(640.0, 480.0, std::f64::consts::FRAC_PI_4)
                .unwrap();

        assert!((horizontal - 320.0).abs() < f64::EPSILON);
        assert!((vertical - 240.0).abs() < 1.0e-10);
        assert!(diagonal > horizontal);
        assert!(linear_gradient_projection_half_extent(f64::NAN, 480.0, 0.0).is_err());
    }

    #[test]
    fn decoded_background_cache_evicts_oldest_entries_at_its_count_limit() {
        let mut cache = BackgroundImageCache::default();
        for index in 0..=MAX_BACKGROUND_CACHE_ENTRIES {
            cache.insert(
                format!("image-{index}"),
                cached_image(index as u64, 4),
            );
        }

        assert_eq!(cache.entries.len(), MAX_BACKGROUND_CACHE_ENTRIES);
        assert!(!cache.entries.contains_key("image-0"));
        assert_eq!(cache.retained_bytes, MAX_BACKGROUND_CACHE_ENTRIES * 4);
    }

    #[test]
    fn decoded_background_cache_rejects_an_entry_beyond_its_byte_budget() {
        let mut cache = BackgroundImageCache::default();
        cache.insert(
            "oversized".to_string(),
            cached_image(1, MAX_BACKGROUND_CACHE_BYTES + 1),
        );
        assert!(cache.entries.is_empty());
        assert_eq!(cache.retained_bytes, 0);
    }

    #[test]
    fn background_completion_authority_is_exact_and_one_shot() {
        let coordinator = BackgroundLoadCoordinator::default();
        let current = Arc::new(AtomicBool::new(false));
        {
            let mut state = lock_cache(&coordinator.state, "test coordinator");
            state.current = Some(Arc::clone(&current));
            state.ready = Some(BackgroundLoadCompletion {
                cancellation: Arc::clone(&current),
                layers: Vec::new(),
            });
        }

        assert!(coordinator.take_ready_if_current(&current).is_some());
        assert!(coordinator.take_ready_if_current(&current).is_none());

        let cancelled = Arc::new(AtomicBool::new(true));
        {
            let mut state = lock_cache(&coordinator.state, "test coordinator");
            state.current = Some(Arc::clone(&cancelled));
            state.ready = Some(BackgroundLoadCompletion {
                cancellation: Arc::clone(&cancelled),
                layers: Vec::new(),
            });
        }
        assert!(coordinator.take_ready_if_current(&cancelled).is_none());
    }

    #[test]
    fn background_completion_slot_preserves_newer_authority_from_a_stale_callback() {
        let coordinator = BackgroundLoadCoordinator::default();
        let stale = Arc::new(AtomicBool::new(true));
        let current = Arc::new(AtomicBool::new(false));
        {
            let mut state = lock_cache(&coordinator.state, "test coordinator");
            state.current = Some(Arc::clone(&current));
            state.ready = Some(BackgroundLoadCompletion {
                cancellation: Arc::clone(&current),
                layers: Vec::new(),
            });
        }

        assert!(coordinator.take_ready_if_current(&stale).is_none());
        assert!(coordinator.take_ready_if_current(&current).is_some());
    }

    #[test]
    fn gui_visual_placeholder_generated_background_cover_and_contain_fill_available_space() {
        let h_context = context(640.0, 16.0);
        let v_context = context(480.0, 24.0);

        assert_eq!(
            resolve_generated_background_size(
                BackgroundSize::Contain,
                BackgroundSize::Cover,
                h_context,
                v_context,
                false,
            ),
            (640, 480)
        );
    }

    #[test]
    fn gui_visual_placeholder_generated_background_dimensions_keep_explicit_units() {
        let h_context = context(640.0, 16.0);
        let v_context = context(480.0, 24.0);

        assert_eq!(
            resolve_generated_background_size(
                BackgroundSize::Dimension(Dimension::Percent(0.5)),
                BackgroundSize::Dimension(Dimension::Cells(2.0)),
                h_context,
                v_context,
                false,
            ),
            (320, 48)
        );
    }

    #[test]
    fn gui_visual_placeholder_radial_generated_backgrounds_stay_square() {
        let h_context = context(640.0, 16.0);
        let v_context = context(480.0, 24.0);

        assert_eq!(
            resolve_generated_background_size(
                BackgroundSize::Cover,
                BackgroundSize::Dimension(Dimension::Pixels(300.0)),
                h_context,
                v_context,
                true,
            ),
            (300, 300)
        );
    }
}
