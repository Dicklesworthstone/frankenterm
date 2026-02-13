//! Images.
//! This module has some helpers for modeling terminal cells that are filled
//! with image data.
//! We're targeting the iTerm image protocol initially, with sixel as an obvious
//! follow up.
//! Kitty has an extensive and complex graphics protocol
//! whose docs are here:
//! <https://github.com/kovidgoyal/kitty/blob/master/docs/graphics-protocol.rst>
//! Both iTerm2 and Sixel appear to have semantics that allow replacing the
//! contents of a single chararcter cell with image data, whereas the kitty
//! protocol appears to track the images out of band as attachments with
//! z-order.

#[cfg(feature = "std")]
use frankenterm_blob_leases::{BlobLease, BlobManager};
use ordered_float::NotNan;
#[cfg(feature = "use_serde")]
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

#[cfg(feature = "use_serde")]
fn deserialize_notnan<'de, D>(deserializer: D) -> Result<NotNan<f32>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = f32::deserialize(deserializer)?;
    NotNan::new(value).map_err(|e| serde::de::Error::custom(format!("{:?}", e)))
}

#[cfg(feature = "use_serde")]
#[allow(clippy::trivially_copy_pass_by_ref)]
fn serialize_notnan<S>(value: &NotNan<f32>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    value.into_inner().serialize(serializer)
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextureCoordinate {
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_notnan",
            serialize_with = "serialize_notnan"
        )
    )]
    pub x: NotNan<f32>,
    #[cfg_attr(
        feature = "use_serde",
        serde(
            deserialize_with = "deserialize_notnan",
            serialize_with = "serialize_notnan"
        )
    )]
    pub y: NotNan<f32>,
}

impl TextureCoordinate {
    pub fn new(x: NotNan<f32>, y: NotNan<f32>) -> Self {
        Self { x, y }
    }

    pub fn new_f32(x: f32, y: f32) -> Self {
        let x = NotNan::new(x).unwrap();
        let y = NotNan::new(y).unwrap();
        Self::new(x, y)
    }
}

/// Tracks data for displaying an image in the place of the normal cell
/// character data.  Since an Image can span multiple cells, we need to logically
/// carve up the image and track each slice of it.  Each cell needs to know
/// its "texture coordinates" within that image so that we can render the
/// right slice.
#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageCell {
    /// Texture coordinate for the top left of this cell.
    /// (0,0) is the top left of the ImageData. (1, 1) is
    /// the bottom right.
    top_left: TextureCoordinate,
    /// Texture coordinates for the bottom right of this cell.
    bottom_right: TextureCoordinate,
    /// References the underlying image data
    data: Arc<ImageData>,
    z_index: i32,
    /// When rendering in the cell, use this offset from the top left
    /// of the cell
    padding_left: u16,
    padding_top: u16,
    padding_right: u16,
    padding_bottom: u16,

    image_id: Option<u32>,
    placement_id: Option<u32>,
}

impl ImageCell {
    pub fn new(
        top_left: TextureCoordinate,
        bottom_right: TextureCoordinate,
        data: Arc<ImageData>,
    ) -> Self {
        Self::with_z_index(top_left, bottom_right, data, 0, 0, 0, 0, 0, None, None)
    }

    pub fn compute_shape_hash<H: Hasher>(&self, hasher: &mut H) {
        self.top_left.hash(hasher);
        self.bottom_right.hash(hasher);
        self.data.hash.hash(hasher);
        self.z_index.hash(hasher);
        self.padding_left.hash(hasher);
        self.padding_top.hash(hasher);
        self.padding_right.hash(hasher);
        self.padding_bottom.hash(hasher);
        self.image_id.hash(hasher);
        self.placement_id.hash(hasher);
    }

    pub fn with_z_index(
        top_left: TextureCoordinate,
        bottom_right: TextureCoordinate,
        data: Arc<ImageData>,
        z_index: i32,
        padding_left: u16,
        padding_top: u16,
        padding_right: u16,
        padding_bottom: u16,
        image_id: Option<u32>,
        placement_id: Option<u32>,
    ) -> Self {
        Self {
            top_left,
            bottom_right,
            data,
            z_index,
            padding_left,
            padding_top,
            padding_right,
            padding_bottom,
            image_id,
            placement_id,
        }
    }

    pub fn matches_placement(&self, image_id: u32, placement_id: Option<u32>) -> bool {
        self.image_id == Some(image_id) && self.placement_id == placement_id
    }

    pub fn has_placement_id(&self) -> bool {
        self.placement_id.is_some()
    }

    pub fn image_id(&self) -> Option<u32> {
        self.image_id
    }

    pub fn placement_id(&self) -> Option<u32> {
        self.placement_id
    }

    pub fn top_left(&self) -> TextureCoordinate {
        self.top_left
    }

    pub fn bottom_right(&self) -> TextureCoordinate {
        self.bottom_right
    }

    pub fn image_data(&self) -> &Arc<ImageData> {
        &self.data
    }

    /// negative z_index is rendered beneath the text layer.
    /// >= 0 is rendered above the text.
    /// negative z_index < INT32_MIN/2 will be drawn under cells
    /// with non-default background colors
    pub fn z_index(&self) -> i32 {
        self.z_index
    }

    /// Returns padding (left, top, right, bottom)
    pub fn padding(&self) -> (u16, u16, u16, u16) {
        (
            self.padding_left,
            self.padding_top,
            self.padding_right,
            self.padding_bottom,
        )
    }
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
#[derive(Clone, PartialEq, Eq)]
pub enum ImageDataType {
    /// Data is in the native image file format
    /// (best for file formats that have animated content)
    EncodedFile(Vec<u8>),
    /// Data is in the native image file format,
    /// (best for file formats that have animated content)
    /// and is stored as a blob via the blob manager.
    #[cfg(feature = "std")]
    EncodedLease(
        #[cfg_attr(
            feature = "use_serde",
            serde(with = "frankenterm_blob_leases::lease_bytes")
        )]
        BlobLease,
    ),
    /// Data is RGBA u8 data
    Rgba8 {
        data: Vec<u8>,
        width: u32,
        height: u32,
        hash: [u8; 32],
    },
    /// Data is an animated sequence
    AnimRgba8 {
        width: u32,
        height: u32,
        durations: Vec<Duration>,
        frames: Vec<Vec<u8>>,
        hashes: Vec<[u8; 32]>,
    },
}

impl std::fmt::Debug for ImageDataType {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::EncodedFile(data) => fmt
                .debug_struct("EncodedFile")
                .field("data_of_len", &data.len())
                .finish(),
            Self::EncodedLease(lease) => lease.fmt(fmt),
            Self::Rgba8 {
                data,
                width,
                height,
                hash,
            } => fmt
                .debug_struct("Rgba8")
                .field("data_of_len", &data.len())
                .field("width", &width)
                .field("height", &height)
                .field("hash", &hash)
                .finish(),
            Self::AnimRgba8 {
                frames,
                width,
                height,
                durations,
                hashes,
            } => fmt
                .debug_struct("AnimRgba8")
                .field("frames_of_len", &frames.len())
                .field("width", &width)
                .field("height", &height)
                .field("durations", durations)
                .field("hashes", hashes)
                .finish(),
        }
    }
}

impl ImageDataType {
    pub fn new_single_frame(width: u32, height: u32, data: Vec<u8>) -> Self {
        let hash = Self::hash_bytes(&data);
        assert_eq!(
            width * height * 4,
            data.len() as u32,
            "invalid dimensions {}x{} for pixel data of length {}",
            width,
            height,
            data.len()
        );
        Self::Rgba8 {
            width,
            height,
            data,
            hash,
        }
    }

    /// Black pixels
    pub fn placeholder() -> Self {
        let mut data = vec![];
        let size = 8;
        for _ in 0..size * size {
            data.extend_from_slice(&[0, 0, 0, 0xff]);
        }
        ImageDataType::new_single_frame(size, size, data)
    }

    pub fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(bytes);
        hasher.finalize().into()
    }

    pub fn compute_hash(&self) -> [u8; 32] {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        match self {
            ImageDataType::EncodedFile(data) => hasher.update(data),
            ImageDataType::EncodedLease(lease) => return lease.content_id().as_hash_bytes(),
            ImageDataType::Rgba8 { data, .. } => hasher.update(data),
            ImageDataType::AnimRgba8 {
                frames, durations, ..
            } => {
                for data in frames {
                    hasher.update(data);
                }
                for d in durations {
                    let d = d.as_secs_f32();
                    let b = d.to_ne_bytes();
                    hasher.update(b);
                }
            }
        };
        hasher.finalize().into()
    }

    /// Divides the animation frame durations by the provided
    /// speed_factor, so a factor of 2 will halve the duration.
    /// # Panics
    /// if the speed_factor is negative, non-finite or the result
    /// overflows the allow Duration range.
    pub fn adjust_speed(&mut self, speed_factor: f32) {
        match self {
            Self::AnimRgba8 { durations, .. } => {
                for d in durations {
                    *d = d.mul_f32(1. / speed_factor);
                }
            }
            _ => {}
        }
    }

    #[cfg(feature = "use_image")]
    pub fn dimensions(&self) -> Result<(u32, u32), ImageCellError> {
        fn dimensions_for_data(data: &[u8]) -> image::ImageResult<(u32, u32)> {
            let reader =
                image::ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?;
            let (width, height) = reader.into_dimensions()?;

            Ok((width, height))
        }

        match self {
            ImageDataType::EncodedFile(data) => Ok(dimensions_for_data(data)?),
            ImageDataType::EncodedLease(lease) => Ok(dimensions_for_data(&lease.get_data()?)?),
            ImageDataType::AnimRgba8 { width, height, .. }
            | ImageDataType::Rgba8 { width, height, .. } => Ok((*width, *height)),
        }
    }

    /// Migrate an in-memory encoded image blob to on-disk to reduce
    /// the memory footprint
    pub fn swap_out(self) -> Result<Self, ImageCellError> {
        match self {
            Self::EncodedFile(data) => match BlobManager::store(&data) {
                Ok(lease) => Ok(Self::EncodedLease(lease)),
                Err(frankenterm_blob_leases::Error::StorageNotInit) => Ok(Self::EncodedFile(data)),
                Err(err) => Err(err.into()),
            },
            other => Ok(other),
        }
    }

    /// Decode an encoded file into either an Rgba8 or AnimRgba8 variant
    /// if we recognize the file format, otherwise the EncodedFile data
    /// is preserved as is.
    #[cfg(feature = "use_image")]
    pub fn decode(self) -> Self {
        use image::{AnimationDecoder, ImageFormat};

        match self {
            Self::EncodedFile(data) => {
                let format = match image::guess_format(&data) {
                    Ok(format) => format,
                    Err(err) => {
                        log::warn!("Unable to decode raw image data: {:#}", err);
                        return Self::EncodedFile(data);
                    }
                };
                let cursor = std::io::Cursor::new(&*data);
                match format {
                    ImageFormat::Gif => image::codecs::gif::GifDecoder::new(cursor)
                        .and_then(|decoder| decoder.into_frames().collect_frames())
                        .and_then(|frames| {
                            if frames.is_empty() {
                                log::error!("decoded image has 0 frames, using placeholder");
                                Ok(Self::placeholder())
                            } else {
                                Ok(Self::decode_frames(frames))
                            }
                        })
                        .unwrap_or_else(|err| {
                            log::error!(
                                "Unable to parse animated gif: {:#}, trying as single frame",
                                err
                            );
                            Self::decode_single(data)
                        }),
                    ImageFormat::Png => {
                        let decoder = match image::codecs::png::PngDecoder::new(cursor) {
                            Ok(d) => d,
                            _ => return Self::EncodedFile(data),
                        };
                        if decoder.is_apng().unwrap_or(false) {
                            match decoder
                                .apng()
                                .and_then(|d| d.into_frames().collect_frames())
                            {
                                Ok(frames) if frames.is_empty() => {
                                    log::error!("decoded image has 0 frames, using placeholder");
                                    Self::placeholder()
                                }
                                Ok(frames) => Self::decode_frames(frames),
                                _ => Self::EncodedFile(data),
                            }
                        } else {
                            Self::decode_single(data)
                        }
                    }
                    ImageFormat::WebP => {
                        let decoder = match image::codecs::webp::WebPDecoder::new(cursor) {
                            Ok(d) => d,
                            _ => return Self::EncodedFile(data),
                        };
                        match decoder.into_frames().collect_frames() {
                            Ok(frames) if frames.is_empty() => {
                                log::error!("decoded image has 0 frames, using placeholder");
                                Self::placeholder()
                            }
                            Ok(frames) => Self::decode_frames(frames),
                            _ => Self::EncodedFile(data),
                        }
                    }
                    _ => Self::decode_single(data),
                }
            }
            data => data,
        }
    }

    #[cfg(not(feature = "use_image"))]
    pub fn decode(self) -> Self {
        self
    }

    #[cfg(feature = "use_image")]
    fn decode_frames(img_frames: Vec<image::Frame>) -> Self {
        let mut width = 0;
        let mut height = 0;
        let mut frames = vec![];
        let mut durations = vec![];
        let mut hashes = vec![];
        for frame in img_frames.into_iter() {
            let duration: Duration = frame.delay().into();
            durations.push(duration);
            let image = image::DynamicImage::ImageRgba8(frame.into_buffer()).to_rgba8();
            let (w, h) = image.dimensions();
            width = w;
            height = h;
            let data = image.into_vec();
            hashes.push(Self::hash_bytes(&data));
            frames.push(data);
        }
        Self::AnimRgba8 {
            width,
            height,
            frames,
            durations,
            hashes,
        }
    }

    #[cfg(feature = "use_image")]
    fn decode_single(data: Vec<u8>) -> Self {
        match image::load_from_memory(&data) {
            Ok(image) => {
                let image = image.to_rgba8();
                let (width, height) = image.dimensions();
                let data = image.into_vec();
                let hash = Self::hash_bytes(&data);
                Self::Rgba8 {
                    width,
                    height,
                    data,
                    hash,
                }
            }
            _ => Self::EncodedFile(data),
        }
    }
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ImageCellError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    BlobLease(#[from] frankenterm_blob_leases::Error),

    #[error(transparent)]
    ImageError(#[from] image::ImageError),
}

#[cfg_attr(feature = "use_serde", derive(Serialize, Deserialize))]
pub struct ImageData {
    data: Mutex<ImageDataType>,
    hash: [u8; 32],
}

struct HexSlice<'a>(&'a [u8]);
impl<'a> std::fmt::Display for HexSlice<'a> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        for byte in self.0 {
            write!(fmt, "{byte:x}")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.debug_struct("ImageData")
            .field("data", &self.data)
            .field("hash", &format_args!("{}", HexSlice(&self.hash)))
            .finish()
    }
}

impl Eq for ImageData {}
impl PartialEq for ImageData {
    fn eq(&self, rhs: &Self) -> bool {
        self.hash == rhs.hash
    }
}

impl ImageData {
    /// Create a new ImageData struct with the provided raw data.
    pub fn with_raw_data(data: Vec<u8>) -> Self {
        let hash = ImageDataType::hash_bytes(&data);
        Self::with_data_and_hash(ImageDataType::EncodedFile(data).decode(), hash)
    }

    fn with_data_and_hash(data: ImageDataType, hash: [u8; 32]) -> Self {
        Self {
            data: Mutex::new(data),
            hash,
        }
    }

    pub fn with_data(data: ImageDataType) -> Self {
        let hash = data.compute_hash();
        Self {
            data: Mutex::new(data),
            hash,
        }
    }

    /// Returns the in-memory footprint
    pub fn len(&self) -> usize {
        match &*self.data() {
            ImageDataType::EncodedFile(d) => d.len(),
            ImageDataType::EncodedLease(_) => 0,
            ImageDataType::Rgba8 { data, .. } => data.len(),
            ImageDataType::AnimRgba8 { frames, .. } => frames.len() * frames[0].len(),
        }
    }

    pub fn data(&self) -> MutexGuard<'_, ImageDataType> {
        self.data.lock().unwrap()
    }

    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── TextureCoordinate ──────────────────────────────────

    #[test]
    fn texture_coordinate_new() {
        let x = NotNan::new(0.5f32).unwrap();
        let y = NotNan::new(0.75f32).unwrap();
        let tc = TextureCoordinate::new(x, y);
        assert_eq!(tc.x, x);
        assert_eq!(tc.y, y);
    }

    #[test]
    fn texture_coordinate_new_f32() {
        let tc = TextureCoordinate::new_f32(0.25, 0.5);
        assert_eq!(tc.x.into_inner(), 0.25);
        assert_eq!(tc.y.into_inner(), 0.5);
    }

    #[test]
    fn texture_coordinate_clone_copy() {
        let tc = TextureCoordinate::new_f32(0.1, 0.2);
        let copied = tc;
        assert_eq!(tc, copied);
    }

    #[test]
    fn texture_coordinate_eq_ne() {
        let a = TextureCoordinate::new_f32(0.0, 0.0);
        let b = TextureCoordinate::new_f32(1.0, 1.0);
        assert_eq!(a, a);
        assert_ne!(a, b);
    }

    #[test]
    fn texture_coordinate_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(TextureCoordinate::new_f32(0.0, 0.0));
        set.insert(TextureCoordinate::new_f32(1.0, 1.0));
        set.insert(TextureCoordinate::new_f32(0.0, 0.0)); // duplicate
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn texture_coordinate_debug() {
        let tc = TextureCoordinate::new_f32(0.5, 0.5);
        let dbg = format!("{:?}", tc);
        assert!(dbg.contains("TextureCoordinate"));
    }

    // ── ImageDataType ──────────────────────────────────────

    #[test]
    fn image_data_type_new_single_frame() {
        let data = vec![0u8; 4 * 2 * 2]; // 2x2 RGBA
        let idt = ImageDataType::new_single_frame(2, 2, data);
        match &idt {
            ImageDataType::Rgba8 {
                width,
                height,
                data,
                hash,
            } => {
                assert_eq!(*width, 2);
                assert_eq!(*height, 2);
                assert_eq!(data.len(), 16);
                assert_ne!(*hash, [0u8; 32]); // hash should be computed
            }
            other => panic!("expected Rgba8, got {:?}", other),
        }
    }

    #[test]
    fn image_data_type_placeholder() {
        let placeholder = ImageDataType::placeholder();
        match &placeholder {
            ImageDataType::Rgba8 { width, height, .. } => {
                assert_eq!(*width, 8);
                assert_eq!(*height, 8);
            }
            other => panic!("expected Rgba8, got {:?}", other),
        }
    }

    #[test]
    fn image_data_type_hash_bytes_deterministic() {
        let data = b"hello world";
        let h1 = ImageDataType::hash_bytes(data);
        let h2 = ImageDataType::hash_bytes(data);
        assert_eq!(h1, h2);
    }

    #[test]
    fn image_data_type_hash_bytes_different_inputs() {
        let h1 = ImageDataType::hash_bytes(b"hello");
        let h2 = ImageDataType::hash_bytes(b"world");
        assert_ne!(h1, h2);
    }

    #[test]
    fn image_data_type_compute_hash_encoded_file() {
        let idt = ImageDataType::EncodedFile(vec![1, 2, 3]);
        let hash = idt.compute_hash();
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn image_data_type_compute_hash_rgba8() {
        let data = vec![0u8; 16];
        let idt = ImageDataType::new_single_frame(2, 2, data);
        let hash = idt.compute_hash();
        assert_ne!(hash, [0u8; 32]);
    }

    #[test]
    fn image_data_type_clone_eq() {
        let a = ImageDataType::EncodedFile(vec![10, 20, 30]);
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn image_data_type_debug_encoded_file() {
        let idt = ImageDataType::EncodedFile(vec![1, 2, 3]);
        let dbg = format!("{:?}", idt);
        assert!(dbg.contains("EncodedFile"));
        assert!(dbg.contains("data_of_len"));
    }

    #[test]
    fn image_data_type_debug_rgba8() {
        let idt = ImageDataType::new_single_frame(1, 1, vec![0; 4]);
        let dbg = format!("{:?}", idt);
        assert!(dbg.contains("Rgba8"));
        assert!(dbg.contains("width"));
        assert!(dbg.contains("height"));
    }

    #[test]
    fn image_data_type_adjust_speed_on_non_anim_is_noop() {
        let mut idt = ImageDataType::EncodedFile(vec![1, 2, 3]);
        idt.adjust_speed(2.0); // should not panic
        assert_eq!(idt, ImageDataType::EncodedFile(vec![1, 2, 3]));
    }

    // ── ImageData ──────────────────────────────────────────

    #[test]
    fn image_data_with_data() {
        let idt = ImageDataType::new_single_frame(2, 2, vec![0u8; 16]);
        let id = ImageData::with_data(idt);
        assert_ne!(id.hash(), [0u8; 32]);
    }

    #[test]
    fn image_data_len() {
        let idt = ImageDataType::new_single_frame(2, 2, vec![0u8; 16]);
        let id = ImageData::with_data(idt);
        assert_eq!(id.len(), 16);
    }

    #[test]
    fn image_data_len_encoded_file() {
        let idt = ImageDataType::EncodedFile(vec![1, 2, 3, 4, 5]);
        let id = ImageData::with_data(idt);
        assert_eq!(id.len(), 5);
    }

    #[test]
    fn image_data_eq_same_hash() {
        let data = vec![0u8; 16];
        let id1 = ImageData::with_data(ImageDataType::new_single_frame(2, 2, data.clone()));
        let id2 = ImageData::with_data(ImageDataType::new_single_frame(2, 2, data));
        assert_eq!(id1, id2);
    }

    #[test]
    fn image_data_ne_different_hash() {
        let id1 = ImageData::with_data(ImageDataType::new_single_frame(1, 1, vec![0, 0, 0, 255]));
        let id2 = ImageData::with_data(ImageDataType::new_single_frame(1, 1, vec![255, 0, 0, 255]));
        assert_ne!(id1, id2);
    }

    #[test]
    fn image_data_debug() {
        let idt = ImageDataType::new_single_frame(1, 1, vec![0; 4]);
        let id = ImageData::with_data(idt);
        let dbg = format!("{:?}", id);
        assert!(dbg.contains("ImageData"));
        assert!(dbg.contains("hash"));
    }

    // ── ImageCell ──────────────────────────────────────────

    #[test]
    fn image_cell_new() {
        let tl = TextureCoordinate::new_f32(0.0, 0.0);
        let br = TextureCoordinate::new_f32(1.0, 1.0);
        let data = Arc::new(ImageData::with_data(ImageDataType::placeholder()));
        let cell = ImageCell::new(tl, br, data);
        assert_eq!(cell.top_left(), tl);
        assert_eq!(cell.bottom_right(), br);
        assert_eq!(cell.z_index(), 0);
        assert_eq!(cell.padding(), (0, 0, 0, 0));
        assert_eq!(cell.image_id(), None);
        assert_eq!(cell.placement_id(), None);
        assert!(!cell.has_placement_id());
    }

    #[test]
    fn image_cell_with_z_index() {
        let tl = TextureCoordinate::new_f32(0.0, 0.0);
        let br = TextureCoordinate::new_f32(0.5, 0.5);
        let data = Arc::new(ImageData::with_data(ImageDataType::placeholder()));
        let cell = ImageCell::with_z_index(tl, br, data, -1, 2, 3, 4, 5, Some(42), Some(7));
        assert_eq!(cell.z_index(), -1);
        assert_eq!(cell.padding(), (2, 3, 4, 5));
        assert_eq!(cell.image_id(), Some(42));
        assert_eq!(cell.placement_id(), Some(7));
        assert!(cell.has_placement_id());
    }

    #[test]
    fn image_cell_matches_placement() {
        let tl = TextureCoordinate::new_f32(0.0, 0.0);
        let br = TextureCoordinate::new_f32(1.0, 1.0);
        let data = Arc::new(ImageData::with_data(ImageDataType::placeholder()));
        let cell = ImageCell::with_z_index(tl, br, data, 0, 0, 0, 0, 0, Some(10), Some(20));
        assert!(cell.matches_placement(10, Some(20)));
        assert!(!cell.matches_placement(10, Some(99)));
        assert!(!cell.matches_placement(99, Some(20)));
        assert!(!cell.matches_placement(10, None));
    }

    #[test]
    fn image_cell_clone_eq() {
        let tl = TextureCoordinate::new_f32(0.0, 0.0);
        let br = TextureCoordinate::new_f32(1.0, 1.0);
        let data = Arc::new(ImageData::with_data(ImageDataType::placeholder()));
        let cell = ImageCell::new(tl, br, data);
        let cloned = cell.clone();
        assert_eq!(cell, cloned);
    }

    #[test]
    fn image_cell_debug() {
        let tl = TextureCoordinate::new_f32(0.0, 0.0);
        let br = TextureCoordinate::new_f32(1.0, 1.0);
        let data = Arc::new(ImageData::with_data(ImageDataType::placeholder()));
        let cell = ImageCell::new(tl, br, data);
        let dbg = format!("{:?}", cell);
        assert!(dbg.contains("ImageCell"));
    }

    // ── ImageCellError ─────────────────────────────────────

    #[test]
    fn image_cell_error_debug() {
        let err = ImageCellError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test"));
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("Io"));
    }

    #[test]
    fn image_cell_error_display() {
        let err = ImageCellError::Io(std::io::Error::new(std::io::ErrorKind::Other, "test error"));
        let msg = format!("{}", err);
        assert!(msg.contains("test error"));
    }
}
