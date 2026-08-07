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
    MAX_IMAGE_WIRE_FRAMES,
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

const MAX_BACKGROUND_DECODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_BACKGROUND_CACHE_BYTES: usize = 256 * 1024 * 1024;
const MAX_BACKGROUND_CACHE_ENTRIES: usize = 32;
const BACKGROUND_IO_CHUNK_BYTES: usize = 64 * 1024;
const MAX_BACKGROUND_TILES_PER_LAYER: usize = 16_384;
static BACKGROUND_POOL_ERROR_REPORTED: AtomicBool = AtomicBool::new(false);

#[derive(Default)]
struct BackgroundImageCache {
    entries: HashMap<String, CachedImage>,
    retained_bytes: usize,
    access_clock: u64,
}

impl BackgroundImageCache {
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
        if let Some(replaced) = self.entries.insert(path.clone(), image) {
            self.retained_bytes = self.retained_bytes.saturating_sub(replaced.retained_bytes);
        }
        self.retained_bytes = self
            .retained_bytes
            .saturating_add(self.entries[&path].retained_bytes);
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
            let Some(evicted) = self.entries.remove(&eviction_key) else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(evicted.retained_bytes);
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
        self.retained_bytes = self.retained_bytes.saturating_add(gradient.retained_bytes);
        self.entries.push(gradient);
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
            let evicted = self.entries.swap_remove(index);
            self.retained_bytes = self.retained_bytes.saturating_sub(evicted.retained_bytes);
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
    scroll_distance: f32,
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
        let adjusted = origin - scroll_distance;
        anyhow::ensure!(
            adjusted.is_finite(),
            "background no-repeat origin overflowed while applying scroll"
        );
        return Ok((adjusted, false));
    }

    // Keep the coordinate close to the viewport even after long scrollback.
    // The integral tile displacement affects only mirror parity; the
    // fractional displacement supplies the visible sub-tile phase.
    let scroll_tiles = scroll_distance / step;
    anyhow::ensure!(
        scroll_tiles.is_finite(),
        "background scroll tile displacement must be finite"
    );
    let shifted_origin = origin - scroll_tiles.fract() * step;
    let backward_steps = ((shifted_origin - lower_bound) / step).ceil().max(0.0);
    anyhow::ensure!(
        backward_steps.is_finite()
            && backward_steps <= MAX_BACKGROUND_TILES_PER_LAYER as f32,
        "background repeat alignment exceeds the per-layer tile limit"
    );
    let normalized_origin = shifted_origin - backward_steps * step;
    anyhow::ensure!(
        normalized_origin.is_finite(),
        "background repeat origin overflowed while aligning to the viewport"
    );

    if repeat == BackgroundRepeat::Mirror {
        anyhow::ensure!(
            scroll_tiles.abs() <= 16_777_216.0,
            "mirrored background scroll exceeds exact f32 tile parity"
        );
    }
    let scroll_is_odd = scroll_tiles.trunc().rem_euclid(2.0) >= 1.0;
    let backward_is_odd = backward_steps.rem_euclid(2.0) >= 1.0;
    Ok((
        normalized_origin,
        scroll_is_odd ^ backward_is_odd,
    ))
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

                    let t = (nx + (x - cx).powi(2) + (ny + y - cy).powi(2)).sqrt() / radius;
                    *pixel = to_pixel(grad.at(t as f32));
                }
            }
        }

        let data = imgbuf.into_vec();
        let image = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            width, height, data,
        )));

        Ok(image)
    }

    fn load(
        g: &Gradient,
        width: u32,
        height: u32,
        is_cancelled: &dyn Fn() -> bool,
    ) -> anyhow::Result<Arc<ImageData>> {
        anyhow::ensure!(!is_cancelled(), "background gradient load was superseded");
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
        let retained_bytes = checked_background_pixel_bytes(width, height)?;

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

struct CachedImage {
    modified: SystemTime,
    file_len: u64,
    image: Arc<ImageData>,
    retained_bytes: usize,
    last_access: u64,
    speed: f32,
}

impl CachedImage {
    fn load(
        path: &str,
        speed: f32,
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
        let modified = metadata
            .modified()
            .with_context(|| format!("getting modification time for {}", path))?;
        let file_len = metadata.len();
        if file_len > u64::try_from(MAX_IMAGE_WIRE_BYTES).unwrap_or(u64::MAX) {
            anyhow::bail!(
                "background image {path} retains {file_len} encoded bytes, exceeding the {MAX_IMAGE_WIRE_BYTES}-byte limit"
            );
        }
        {
            let mut cache = lock_cache(&IMAGE_CACHE, "image");
            let access = cache.next_access();
            if let Some(cached) = cache.entries.get_mut(path) {
                if cached.modified == modified
                    && cached.file_len == file_len
                    && cached.speed == speed
                {
                    cached.last_access = access;
                    return Ok(Arc::clone(&cached.image));
                }
            }
        }

        // Keep filesystem IO and image decoding outside the global cache lock.
        let mut reader = std::fs::File::open(path)
            .with_context(|| format!("opening window_background_image {path}"))?;
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
                ImageDataValidationLimits {
                    max_decoded_bytes: MAX_BACKGROUND_DECODED_BYTES,
                    max_frame_count: MAX_IMAGE_WIRE_FRAMES,
                    max_width: 16_384,
                    max_height: 16_384,
                },
                is_cancelled,
            )
            .with_context(|| format!("decoding window_background_image {path}"))?;
        let retained_bytes = normalized.summary.decoded_bytes;
        let mut decoded = normalized
            .replacement
            .context("encoded background normalization did not return decoded data")?
            .into_data();
        decoded
            .adjust_speed(speed)
            .with_context(|| format!("applying background image speed for {path}"))?;
        let image = Arc::new(ImageData::with_data(decoded));
        anyhow::ensure!(!is_cancelled(), "background image load was superseded");

        // File metadata is the cache authority. A decode can be expensive, so
        // another reload may win while this thread is outside the lock. Never
        // let the older decode overwrite a cache entry for a newer file
        // generation.
        let current_metadata = std::fs::metadata(path)
            .with_context(|| format!("rechecking metadata for {path}"))?;
        anyhow::ensure!(
            current_metadata.file_type().is_file(),
            "background image {path} stopped being a regular file while decoding"
        );
        let current_modified = current_metadata
            .modified()
            .with_context(|| format!("rechecking modification time for {path}"))?;
        let current_file_len = current_metadata.len();
        if current_modified != modified || current_file_len != file_len {
            let mut cache = lock_cache(&IMAGE_CACHE, "image");
            let access = cache.next_access();
            if let Some(cached) = cache.entries.get_mut(path) {
                if cached.modified == current_modified
                    && cached.file_len == current_file_len
                    && cached.speed == speed
                {
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
            if cached.modified == modified
                && cached.file_len == file_len
                && cached.speed == speed
            {
                cached.last_access = access;
                return Ok(Arc::clone(&cached.image));
            }
        }
        cache.insert(
            path.to_string(),
            Self {
                modified,
                file_len,
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

            CachedGradient::load(g, width, height, is_cancelled)?
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
            Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
                size, size, data,
            )))
        }
        BackgroundSource::File(source) => {
            CachedImage::load(&source.path, source.speed, is_cancelled)?
        }
    };

    Ok(LoadedBackgroundLayer {
        source: data,
        def: layer.clone(),
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
        .map(|layer| (layer.source.hash(), &layer.source))
        .collect();

    let mut result = Vec::with_capacity(config.background.len());
    for (index, definition) in config.background.iter().enumerate() {
        if is_cancelled() {
            return existing.to_vec();
        }
        match load_background_layer(definition, dimensions, render_metrics, is_cancelled) {
            Ok(mut layer) => {
                let hash = layer.source.hash();
                if let Some(existing) = map.get(&hash) {
                    layer.source = Arc::clone(existing);
                }
                result.push(layer);
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
                    result.push(LoadedBackgroundLayer {
                        source: Arc::clone(&previous.source),
                        def: definition.clone(),
                    });
                } else {
                    log::error!("Failed to load background layer {index}: {err:#}");
                }
            }
        }
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
        for layer in self.window_background.iter() {
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
            if !factor.is_finite() {
                metrics::counter!(
                    "gui.background.layer_rejected.total",
                    "reason" => "invalid_scroll_factor",
                )
                .increment(1);
                return Ok(false);
            }
            top as f32 * self.render_metrics.cell_size.height as f32 * factor
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
                if offset_x >= pixel_width {
                    break;
                }
                let origin_x = origin_x + offset_x;
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

    fn cached_image(last_access: u64, retained_bytes: usize) -> CachedImage {
        CachedImage {
            modified: SystemTime::UNIX_EPOCH,
            file_len: 4,
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
