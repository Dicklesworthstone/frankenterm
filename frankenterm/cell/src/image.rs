//! Images.
//! This module has some helpers for modeling terminal cells that are filled
//! with image data.
//! We're targeting the iTerm image protocol initially, with sixel as an obvious
//! follow up.
//! Kitty has an extensive and complex graphics protocol
//! whose docs are here:
//! <https://sw.kovidgoyal.net/kitty/graphics-protocol/>
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
use std::ops::{Deref, DerefMut};
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

#[cfg(feature = "use_serde")]
/// Zero-wire serde marker selecting the audited image byte-buffer admission.
pub const IMAGE_WIRE_BYTES_V1_NEWTYPE: &str = "frankenterm.image.WireBytesV1";

/// Maximum bytes admitted for one image wire payload. This is enforced by
/// every serde format, not only the mux codec's varbincode reader, so JSON and
/// lease-backed serialization cannot bypass the remote-render memory bound.
pub const MAX_IMAGE_WIRE_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_IMAGE_WIRE_FRAMES: usize = 4_096;

#[cfg(feature = "use_serde")]
fn deserialize_bounded_image_sequence<'de, D, T>(
    deserializer: D,
    label: &'static str,
) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    use serde::de::{IgnoredAny, SeqAccess, Visitor};
    use std::fmt;
    use std::marker::PhantomData;

    struct BoundedSequenceVisitor<T> {
        label: &'static str,
        marker: PhantomData<T>,
    }

    impl<'de, T: Deserialize<'de>> Visitor<'de> for BoundedSequenceVisitor<T> {
        type Value = Vec<T>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {MAX_IMAGE_WIRE_FRAMES} image {}", self.label)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let hinted = sequence.size_hint().unwrap_or(0);
            if hinted > MAX_IMAGE_WIRE_FRAMES {
                return Err(serde::de::Error::custom(format_args!(
                    "image {} advertise {hinted} items, exceeding the {MAX_IMAGE_WIRE_FRAMES}-item limit",
                    self.label
                )));
            }
            let mut items = Vec::new();
            items.try_reserve_exact(hinted).map_err(|error| {
                serde::de::Error::custom(format_args!(
                    "reserving image {} failed: {error}",
                    self.label
                ))
            })?;
            while items.len() < MAX_IMAGE_WIRE_FRAMES {
                let Some(item) = sequence.next_element()? else {
                    return Ok(items);
                };
                items.push(item);
            }
            if sequence.next_element::<IgnoredAny>()?.is_some() {
                return Err(serde::de::Error::custom(format_args!(
                    "image {} exceed the {MAX_IMAGE_WIRE_FRAMES}-item limit",
                    self.label
                )));
            }
            Ok(items)
        }
    }

    deserializer.deserialize_seq(BoundedSequenceVisitor {
        label,
        marker: PhantomData,
    })
}

#[cfg(feature = "use_serde")]
mod image_wire_durations {
    use super::{MAX_IMAGE_WIRE_FRAMES, deserialize_bounded_image_sequence};
    use serde::{Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(items: &[Duration], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if items.len() > MAX_IMAGE_WIRE_FRAMES {
            return Err(serde::ser::Error::custom(format_args!(
                "image durations contain {} items, exceeding the {}-item limit",
                items.len(),
                MAX_IMAGE_WIRE_FRAMES
            )));
        }
        items.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_bounded_image_sequence(deserializer, "durations")
    }
}

#[cfg(feature = "use_serde")]
mod image_wire_hashes {
    use super::{MAX_IMAGE_WIRE_FRAMES, deserialize_bounded_image_sequence};
    use serde::{Deserializer, Serialize, Serializer};

    pub fn serialize<S>(items: &[[u8; 32]], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if items.len() > MAX_IMAGE_WIRE_FRAMES {
            return Err(serde::ser::Error::custom(format_args!(
                "image hashes contain {} items, exceeding the {}-item limit",
                items.len(),
                MAX_IMAGE_WIRE_FRAMES
            )));
        }
        items.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<[u8; 32]>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_bounded_image_sequence(deserializer, "hashes")
    }
}

#[cfg(feature = "use_serde")]
mod image_wire_bytes {
    use super::{IMAGE_WIRE_BYTES_V1_NEWTYPE, MAX_IMAGE_WIRE_BYTES};
    use serde::de::{DeserializeSeed, SeqAccess, Visitor};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::fmt;

    struct ByteSlice<'a>(&'a [u8]);

    impl Serialize for ByteSlice<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_bytes(self.0)
        }
    }

    pub(super) struct Ref<'a>(pub(super) &'a [u8]);

    impl Serialize for Ref<'_> {
        fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            serializer.serialize_newtype_struct(
                IMAGE_WIRE_BYTES_V1_NEWTYPE,
                &ByteSlice(self.0),
            )
        }
    }

    struct ByteBufferVisitor {
        max_bytes: usize,
    }

    impl<'de> Visitor<'de> for ByteBufferVisitor {
        type Value = Vec<u8>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("bounded image bytes")
        }

        fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value.len() > self.max_bytes {
                return Err(E::custom(format_args!(
                    "image byte buffer retains {} bytes, exceeding the {}-byte limit",
                    value.len(),
                    self.max_bytes
                )));
            }
            Ok(value)
        }

        fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            if value.len() > self.max_bytes {
                return Err(E::custom(format_args!(
                    "image byte buffer retains {} bytes, exceeding the {}-byte limit",
                    value.len(),
                    self.max_bytes
                )));
            }
            let mut owned = Vec::new();
            owned.try_reserve_exact(value.len()).map_err(|error| {
                E::custom(format_args!("reserving image byte buffer failed: {error}"))
            })?;
            owned.extend_from_slice(value);
            Ok(owned)
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: SeqAccess<'de>,
        {
            let hinted = sequence.size_hint().unwrap_or(0);
            if hinted > self.max_bytes {
                return Err(serde::de::Error::custom(format_args!(
                    "image byte sequence advertises {hinted} bytes, exceeding the {}-byte limit",
                    self.max_bytes
                )));
            }
            let mut bytes = Vec::new();
            bytes.try_reserve_exact(hinted).map_err(|error| {
                serde::de::Error::custom(format_args!(
                    "reserving image byte sequence failed: {error}"
                ))
            })?;
            while let Some(byte) = sequence.next_element::<u8>()? {
                if bytes.len() == self.max_bytes {
                    return Err(serde::de::Error::custom(format_args!(
                        "image byte sequence exceeds the {}-byte limit",
                        self.max_bytes
                    )));
                }
                bytes.push(byte);
            }
            Ok(bytes)
        }
    }

    pub(super) struct Owned(pub(super) Vec<u8>);

    struct NewtypeVisitor {
        max_bytes: usize,
    }

    impl<'de> Visitor<'de> for NewtypeVisitor {
        type Value = Owned;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a bounded image-byte newtype")
        }

        fn visit_newtype_struct<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer
                .deserialize_byte_buf(ByteBufferVisitor {
                    max_bytes: self.max_bytes,
                })
                .map(Owned)
        }
    }

    pub(super) struct Seed {
        max_bytes: usize,
    }

    impl Seed {
        pub(super) fn new(max_bytes: usize) -> Self {
            Self { max_bytes }
        }
    }

    impl<'de> DeserializeSeed<'de> for Seed {
        type Value = Owned;

        fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_newtype_struct(
                IMAGE_WIRE_BYTES_V1_NEWTYPE,
                NewtypeVisitor {
                    max_bytes: self.max_bytes,
                },
            )
        }
    }

    impl<'de> Deserialize<'de> for Owned {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_newtype_struct(
                IMAGE_WIRE_BYTES_V1_NEWTYPE,
                NewtypeVisitor {
                    max_bytes: MAX_IMAGE_WIRE_BYTES,
                },
            )
        }
    }

    pub fn serialize<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if bytes.len() > MAX_IMAGE_WIRE_BYTES {
            return Err(serde::ser::Error::custom(format_args!(
                "image byte buffer retains {} bytes, exceeding the {}-byte limit",
                bytes.len(),
                MAX_IMAGE_WIRE_BYTES
            )));
        }
        Ref(bytes).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Owned::deserialize(deserializer).map(|owned| owned.0)
    }
}

#[cfg(feature = "use_serde")]
mod image_wire_frames {
    use super::{MAX_IMAGE_WIRE_BYTES, MAX_IMAGE_WIRE_FRAMES, image_wire_bytes};
    use serde::de::{IgnoredAny, SeqAccess, Visitor};
    use serde::ser::SerializeSeq;
    use serde::{Deserializer, Serializer};
    use std::fmt;

    pub fn serialize<S>(frames: &[Vec<u8>], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serialize_with_limits(
            frames,
            serializer,
            MAX_IMAGE_WIRE_FRAMES,
            MAX_IMAGE_WIRE_BYTES,
        )
    }

    fn serialize_with_limits<S>(
        frames: &[Vec<u8>],
        serializer: S,
        max_frames: usize,
        max_bytes: usize,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if frames.len() > max_frames {
            return Err(serde::ser::Error::custom(format_args!(
                "image frames contain {} items, exceeding the {}-item limit",
                frames.len(),
                max_frames
            )));
        }
        let total_bytes = frames.iter().try_fold(
            0usize,
            |total, frame| -> Result<usize, S::Error> {
                total.checked_add(frame.len()).ok_or_else(|| {
                    serde::ser::Error::custom("image frame byte accounting overflowed")
                })
            },
        )?;
        if total_bytes > max_bytes {
            return Err(serde::ser::Error::custom(format_args!(
                "image frames retain {total_bytes} bytes, exceeding the {max_bytes}-byte limit"
            )));
        }
        let mut sequence = serializer.serialize_seq(Some(frames.len()))?;
        for frame in frames {
            sequence.serialize_element(&image_wire_bytes::Ref(frame))?;
        }
        sequence.end()
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_with_limits(deserializer, MAX_IMAGE_WIRE_FRAMES, MAX_IMAGE_WIRE_BYTES)
    }

    fn deserialize_with_limits<'de, D>(
        deserializer: D,
        max_frames: usize,
        max_bytes: usize,
    ) -> Result<Vec<Vec<u8>>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct FramesVisitor {
            max_frames: usize,
            max_bytes: usize,
        }

        impl<'de> Visitor<'de> for FramesVisitor {
            type Value = Vec<Vec<u8>>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "at most {} image frames retaining at most {} bytes",
                    self.max_frames, self.max_bytes
                )
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let hinted = sequence.size_hint().unwrap_or(0);
                if hinted > self.max_frames {
                    return Err(serde::de::Error::custom(format_args!(
                        "image frames advertise {hinted} items, exceeding the {}-item limit",
                        self.max_frames
                    )));
                }
                let mut frames = Vec::new();
                frames
                    .try_reserve_exact(hinted.min(self.max_frames))
                    .map_err(|error| {
                        serde::de::Error::custom(format_args!(
                            "reserving image frames failed: {error}"
                        ))
                    })?;
                let mut total_bytes = 0usize;
                while frames.len() < self.max_frames {
                    let remaining_bytes = self.max_bytes.checked_sub(total_bytes).ok_or_else(|| {
                        serde::de::Error::custom("image frame byte accounting overflowed")
                    })?;
                    let Some(frame) = sequence
                        .next_element_seed(image_wire_bytes::Seed::new(remaining_bytes))?
                    else {
                        return Ok(frames);
                    };
                    total_bytes = total_bytes.checked_add(frame.0.len()).ok_or_else(|| {
                        serde::de::Error::custom("image frame byte accounting overflowed")
                    })?;
                    frames.push(frame.0);
                }
                if sequence.next_element::<IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom(format_args!(
                        "image frames exceed the {}-item limit",
                        self.max_frames
                    )));
                }
                Ok(frames)
            }
        }

        deserializer.deserialize_seq(FramesVisitor {
            max_frames,
            max_bytes,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::{deserialize_with_limits, serialize_with_limits};
        use crate::image::{MAX_IMAGE_WIRE_BYTES, MAX_IMAGE_WIRE_FRAMES};

        #[test]
        fn frame_deserialization_applies_the_remaining_aggregate_byte_budget() {
            let mut accepted = serde_json::Deserializer::from_str("[[1,2],[3]]");
            assert_eq!(
                deserialize_with_limits(&mut accepted, 2, 3).unwrap(),
                vec![vec![1, 2], vec![3]]
            );

            let mut rejected = serde_json::Deserializer::from_str("[[1,2],[3,4]]");
            let error = deserialize_with_limits(&mut rejected, 2, 3).unwrap_err();
            assert!(
                error.to_string().contains("1-byte limit"),
                "the second frame must receive only the aggregate budget remaining after the first: {}",
                error
            );
        }

        #[test]
        fn frame_deserialization_rejects_an_item_beyond_the_frame_budget() {
            let mut rejected = serde_json::Deserializer::from_str("[[1],[2],[3]]");
            let error = deserialize_with_limits(&mut rejected, 2, 3).unwrap_err();
            assert!(error.to_string().contains("2-item limit"), "{}", error);
        }

        #[test]
        fn frame_serialization_rejects_aggregate_bytes_beyond_the_budget() {
            let frames = vec![vec![1, 2], vec![3, 4]];
            let mut encoded = Vec::new();
            let mut serializer = serde_json::Serializer::new(&mut encoded);
            let error = serialize_with_limits(&frames, &mut serializer, 2, 3).unwrap_err();
            assert!(error.to_string().contains("3-byte limit"), "{}", error);
            assert!(
                encoded.is_empty(),
                "aggregate accounting must reject before emitting a partial sequence"
            );
        }

        #[test]
        fn frame_wire_accepts_exact_frame_cardinality_limit() {
            let frames = vec![Vec::new(); MAX_IMAGE_WIRE_FRAMES];
            let mut encoded = Vec::new();
            let mut serializer = serde_json::Serializer::new(&mut encoded);
            serialize_with_limits(
                &frames,
                &mut serializer,
                MAX_IMAGE_WIRE_FRAMES,
                MAX_IMAGE_WIRE_BYTES,
            )
            .unwrap();

            let mut deserializer = serde_json::Deserializer::from_slice(&encoded);
            let decoded = deserialize_with_limits(
                &mut deserializer,
                MAX_IMAGE_WIRE_FRAMES,
                MAX_IMAGE_WIRE_BYTES,
            )
            .unwrap();
            assert_eq!(decoded.len(), MAX_IMAGE_WIRE_FRAMES);
        }

        #[test]
        fn frame_wire_rejects_4097th_frame_on_encode_and_decode() {
            let frames = vec![Vec::new(); MAX_IMAGE_WIRE_FRAMES + 1];
            let mut encoded = Vec::new();
            let mut serializer = serde_json::Serializer::new(&mut encoded);
            let encode_error = serialize_with_limits(
                &frames,
                &mut serializer,
                MAX_IMAGE_WIRE_FRAMES,
                MAX_IMAGE_WIRE_BYTES,
            )
            .unwrap_err();
            assert!(
                encode_error
                    .to_string()
                    .contains("4096-item limit"),
                "{}",
                encode_error
            );

            let encoded = format!("[{}]", vec!["[]"; MAX_IMAGE_WIRE_FRAMES + 1].join(","));
            let mut deserializer = serde_json::Deserializer::from_str(&encoded);
            let decode_error = deserialize_with_limits(
                &mut deserializer,
                MAX_IMAGE_WIRE_FRAMES,
                MAX_IMAGE_WIRE_BYTES,
            )
            .unwrap_err();
            assert!(
                decode_error
                    .to_string()
                    .contains("4096-item limit"),
                "{}",
                decode_error
            );
        }
    }
}

#[cfg(all(feature = "use_serde", feature = "std"))]
mod image_wire_lease_bytes {
    use super::{BlobLease, BlobManager, MAX_IMAGE_WIRE_BYTES, image_wire_bytes};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(lease: &BlobLease, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::Error as _;
        use std::io::{Read, Seek, SeekFrom};
        let mut reader = lease
            .get_reader()
            .map_err(|error| S::Error::custom(error.to_string()))?;
        let declared_len = reader
            .seek(SeekFrom::End(0))
            .map_err(|error| S::Error::custom(error.to_string()))?;
        if declared_len > u64::try_from(MAX_IMAGE_WIRE_BYTES).unwrap_or(u64::MAX) {
            return Err(S::Error::custom(format_args!(
                "lease-backed image retains {declared_len} bytes, exceeding the {MAX_IMAGE_WIRE_BYTES}-byte limit"
            )));
        }
        reader
            .seek(SeekFrom::Start(0))
            .map_err(|error| S::Error::custom(error.to_string()))?;
        let declared_len = usize::try_from(declared_len)
            .map_err(|_| S::Error::custom("lease-backed image length does not fit usize"))?;
        let mut data = Vec::new();
        data.try_reserve_exact(declared_len).map_err(|error| {
            S::Error::custom(format_args!("reserving lease-backed image bytes failed: {error}"))
        })?;
        reader
            .take(u64::try_from(MAX_IMAGE_WIRE_BYTES).unwrap_or(u64::MAX) + 1)
            .read_to_end(&mut data)
            .map_err(|error| S::Error::custom(error.to_string()))?;
        if data.len() > MAX_IMAGE_WIRE_BYTES {
            return Err(S::Error::custom(format_args!(
                "lease-backed image exceeded the {MAX_IMAGE_WIRE_BYTES}-byte limit while reading"
            )));
        }
        image_wire_bytes::Ref(&data).serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<BlobLease, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error as _;
        let data = image_wire_bytes::Owned::deserialize(deserializer)?.0;
        BlobManager::store(&data).map_err(|error| D::Error::custom(error.to_string()))
    }
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
        self.data.current_content_hash().hash(hasher);
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
    EncodedFile(
        #[cfg_attr(feature = "use_serde", serde(with = "image_wire_bytes"))]
        Vec<u8>,
    ),
    /// Data is in the native image file format,
    /// (best for file formats that have animated content)
    /// and is stored as a blob via the blob manager.
    #[cfg(feature = "std")]
    EncodedLease(
        #[cfg_attr(
            feature = "use_serde",
            serde(with = "image_wire_lease_bytes")
        )]
        BlobLease,
    ),
    /// Data is RGBA u8 data
    Rgba8 {
        #[cfg_attr(feature = "use_serde", serde(with = "image_wire_bytes"))]
        data: Vec<u8>,
        width: u32,
        height: u32,
        hash: [u8; 32],
    },
    /// Data is an animated sequence
    AnimRgba8 {
        width: u32,
        height: u32,
        #[cfg_attr(feature = "use_serde", serde(with = "image_wire_durations"))]
        durations: Vec<Duration>,
        #[cfg_attr(feature = "use_serde", serde(with = "image_wire_frames"))]
        frames: Vec<Vec<u8>>,
        #[cfg_attr(feature = "use_serde", serde(with = "image_wire_hashes"))]
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

/// A structural validation failure for image data received across a trust
/// boundary.
///
/// The decoded variants contain redundant dimensions, byte buffers, and
/// hashes. Those fields must agree before the renderer can safely construct
/// bitmap slices or index animation metadata. Encoded variants require a
/// separate decoder with explicit byte, dimension, frame, and time budgets;
/// this validator deliberately never performs an unbounded decode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageDataValidationSummary {
    /// Exact decoded pixel bytes retained by all frames.
    pub decoded_bytes: usize,
    /// Number of independently timed frames (one for a static image).
    pub frame_count: usize,
}

/// Result of admitting one image revision at a remote-render trust boundary.
///
/// `replacement` is present when the wire payload used an encoded form and
/// therefore had to be decoded. Already-decoded mutable objects retain their
/// allocation without cloning; ImageCell shape hashes and remote caches key
/// their cached current revision independently of stable object identity.
#[derive(Debug)]
pub struct ImageDataRevisionValidation {
    pub summary: ImageDataValidationSummary,
    pub replacement: Option<ImageData>,
}

/// Caller-owned resource ceilings for validating already-decoded image data.
/// Limits are checked before hashing pixel buffers, so an oversized peer
/// payload cannot consume unbounded validation CPU before being rejected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImageDataValidationLimits {
    /// Maximum sum of decoded RGBA bytes retained by the image.
    pub max_decoded_bytes: usize,
    /// Maximum number of decoded frames retained by the image.
    pub max_frame_count: usize,
    /// Maximum decoded width accepted by the consumer.
    pub max_width: u32,
    /// Maximum decoded height accepted by the consumer.
    pub max_height: u32,
}

impl ImageDataValidationLimits {
    pub const UNBOUNDED: Self = Self {
        max_decoded_bytes: usize::MAX,
        max_frame_count: usize::MAX,
        max_width: u32::MAX,
        max_height: u32::MAX,
    };
}

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum ImageDataValidationError {
    #[error("encoded image data must be decoded under a caller-owned resource budget")]
    EncodedDataRequiresBoundedDecode,

    #[error("image content does not match the requested revision")]
    ContentRevisionMismatch,

    #[error("encoded image retains {requested} bytes, exceeding the {limit}-byte limit")]
    EncodedByteLimitExceeded { requested: usize, limit: usize },

    #[error("encoded image decoding was cancelled")]
    DecodeCancelled,

    #[error("encoded image could not be decoded within the bounded pipeline: {message}")]
    EncodedDecodeFailed { message: String },

    #[cfg(feature = "use_image")]
    #[error("encoded image decoding exceeded a resource limit: {source}")]
    EncodedResourceLimit {
        #[source]
        source: image::error::LimitError,
    },

    #[error("encoded image resource I/O failed during {operation}: {source}")]
    EncodedResourceIo {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[cfg(feature = "std")]
    #[error("encoded image blob lease is unavailable: {source}")]
    EncodedLeaseUnavailable {
        #[source]
        source: frankenterm_blob_leases::Error,
    },

    #[error("validated image payload remained externally shared/mutable")]
    SharedMutablePayload,

    #[error("animation speed factor must be finite and greater than zero")]
    InvalidAnimationSpeedFactor,

    #[error("animation frame {frame_index} duration overflows after speed adjustment")]
    AdjustedFrameDurationOutOfRange { frame_index: usize },

    #[error("decoded image has no frames")]
    AnimationHasNoFrames,

    #[error("decoded image dimensions must be non-zero, got {width}x{height}")]
    ZeroDimensions { width: u32, height: u32 },

    #[error(
        "decoded image dimensions {width}x{height} exceed the {max_width}x{max_height} consumer limit"
    )]
    DimensionLimitExceeded {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },

    #[error("decoded RGBA dimensions {width}x{height} overflow the addressable byte length")]
    RgbaByteLengthOverflow { width: u32, height: u32 },

    #[error(
        "decoded RGBA byte length does not match {width}x{height}: expected {expected}, got {actual}"
    )]
    RgbaByteLengthMismatch {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },

    #[error("decoded RGBA content hash does not match its pixel bytes")]
    RgbaHashMismatch,

    #[error("decoded animation byte accounting overflowed")]
    DecodedByteLengthOverflow,

    #[error("decoded image retains {requested} bytes, exceeding the {limit}-byte limit")]
    DecodedByteLimitExceeded { requested: usize, limit: usize },

    #[error("decoded image has {requested} frames, exceeding the {limit}-frame limit")]
    FrameCountLimitExceeded { requested: usize, limit: usize },

    #[error(
        "animation metadata cardinality mismatch: {frames} frames, {durations} durations, {hashes} hashes"
    )]
    AnimationCardinalityMismatch {
        frames: usize,
        durations: usize,
        hashes: usize,
    },

    #[error(
        "animation frame {frame_index} byte length does not match {width}x{height}: expected {expected}, got {actual}"
    )]
    AnimationFrameByteLengthMismatch {
        frame_index: usize,
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },

    #[error("animation frame {frame_index} content hash does not match its pixel bytes")]
    AnimationFrameHashMismatch { frame_index: usize },

    #[error("animation frame {frame_index} duration is outside the supported renderer range")]
    AnimationFrameDurationOutOfRange { frame_index: usize },
}

#[cfg(feature = "use_image")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EncodedImageSource {
    InMemory,
    BlobLease,
}

impl ImageDataType {
    pub fn new_single_frame(width: u32, height: u32, data: Vec<u8>) -> Self {
        assert!(
            width != 0 && height != 0,
            "single-frame image dimensions must be non-zero, got {width}x{height}"
        );
        let expected_len = u128::from(width) * u128::from(height) * 4;
        assert_eq!(
            expected_len,
            data.len() as u128,
            "invalid dimensions {}x{} for pixel data of length {}",
            width,
            height,
            data.len()
        );
        let hash = Self::hash_bytes(&data);
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

    fn hash_bytes_with_cancellation(
        bytes: &[u8],
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<[u8; 32], ImageDataValidationError> {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        for chunk in bytes.chunks(64 * 1024) {
            if is_cancelled() {
                return Err(ImageDataValidationError::DecodeCancelled);
            }
            hasher.update(chunk);
        }
        if is_cancelled() {
            return Err(ImageDataValidationError::DecodeCancelled);
        }
        Ok(hasher.finalize().into())
    }

    pub fn compute_hash(&self) -> [u8; 32] {
        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        match self {
            ImageDataType::EncodedFile(data) => hasher.update(data),
            ImageDataType::EncodedLease(lease) => return lease.content_id().as_hash_bytes(),
            ImageDataType::Rgba8 {
                width,
                height,
                hash,
                ..
            } => {
                hasher.update(b"frankenterm-image-rgba8-v1\0");
                hasher.update(width.to_be_bytes());
                hasher.update(height.to_be_bytes());
                hasher.update(hash);
            }
            ImageDataType::AnimRgba8 {
                width,
                height,
                durations,
                frames,
                hashes,
            } => {
                hasher.update(b"frankenterm-image-anim-rgba8-v1\0");
                hasher.update(width.to_be_bytes());
                hasher.update(height.to_be_bytes());
                hasher.update((frames.len() as u128).to_be_bytes());
                hasher.update((durations.len() as u128).to_be_bytes());
                for duration in durations {
                    hasher.update(duration.as_secs().to_be_bytes());
                    hasher.update(duration.subsec_nanos().to_be_bytes());
                }
                hasher.update((hashes.len() as u128).to_be_bytes());
                for hash in hashes {
                    hasher.update(hash);
                }
            }
        };
        hasher.finalize().into()
    }

    /// Recompute the redundant pixel hashes carried by decoded variants.
    ///
    /// Mutable image access exposes the complete enum so callers can edit
    /// pixels, replace frames, or even change variants. The outer content
    /// revision deliberately hashes these embedded digests rather than every
    /// pixel on each cache lookup; consequently the mutation guard must repair
    /// every embedded digest before publishing the new outer revision.
    fn refresh_embedded_hashes(&mut self) {
        match self {
            Self::Rgba8 { data, hash, .. } => {
                *hash = Self::hash_bytes(data);
            }
            Self::AnimRgba8 { frames, hashes, .. } => {
                hashes.resize(frames.len(), [0; 32]);
                for (hash, frame) in hashes.iter_mut().zip(frames) {
                    *hash = Self::hash_bytes(frame);
                }
            }
            Self::EncodedFile(_) | Self::EncodedLease(_) => {}
        }
    }

    fn expected_rgba8_len(
        width: u32,
        height: u32,
    ) -> Result<usize, ImageDataValidationError> {
        if width == 0 || height == 0 {
            return Err(ImageDataValidationError::ZeroDimensions { width, height });
        }

        // u32 * u32 * 4 always fits in u128; the fallible conversion is
        // therefore the exact check for whether the decoded buffer length is
        // addressable on this target.
        let byte_len = usize::try_from(u128::from(width) * u128::from(height) * 4)
            .map_err(|_| ImageDataValidationError::RgbaByteLengthOverflow { width, height })?;

        Ok(byte_len)
    }

    fn validate_rgba8(
        data: &[u8],
        width: u32,
        height: u32,
        hash: &[u8; 32],
        limits: ImageDataValidationLimits,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ImageDataValidationSummary, ImageDataValidationError> {
        Self::validate_dimensions(width, height, limits)?;
        let expected = Self::expected_rgba8_len(width, height)?;
        if data.len() != expected {
            return Err(ImageDataValidationError::RgbaByteLengthMismatch {
                width,
                height,
                expected,
                actual: data.len(),
            });
        }
        if limits.max_frame_count < 1 {
            return Err(ImageDataValidationError::FrameCountLimitExceeded {
                requested: 1,
                limit: limits.max_frame_count,
            });
        }
        if data.len() > limits.max_decoded_bytes {
            return Err(ImageDataValidationError::DecodedByteLimitExceeded {
                requested: data.len(),
                limit: limits.max_decoded_bytes,
            });
        }
        if Self::hash_bytes_with_cancellation(data, is_cancelled)? != *hash {
            return Err(ImageDataValidationError::RgbaHashMismatch);
        }
        Ok(ImageDataValidationSummary {
            decoded_bytes: data.len(),
            frame_count: 1,
        })
    }

    fn validate_dimensions(
        width: u32,
        height: u32,
        limits: ImageDataValidationLimits,
    ) -> Result<(), ImageDataValidationError> {
        if width == 0 || height == 0 {
            return Err(ImageDataValidationError::ZeroDimensions { width, height });
        }
        if width > limits.max_width || height > limits.max_height {
            return Err(ImageDataValidationError::DimensionLimitExceeded {
                width,
                height,
                max_width: limits.max_width,
                max_height: limits.max_height,
            });
        }
        Ok(())
    }

    /// Validate all redundant structure needed by decoded-image consumers.
    ///
    /// This method is deliberately non-panicking for malformed dimensions,
    /// buffers, animation metadata, hashes, and durations. Encoded variants
    /// fail closed so that callers cannot accidentally decode peer-controlled
    /// data without a caller-owned resource budget.
    pub fn validate_decoded_structure(
        &self,
    ) -> Result<ImageDataValidationSummary, ImageDataValidationError> {
        self.validate_decoded_structure_with_limits(ImageDataValidationLimits::UNBOUNDED)
    }

    pub fn validate_decoded_structure_with_limits(
        &self,
        limits: ImageDataValidationLimits,
    ) -> Result<ImageDataValidationSummary, ImageDataValidationError> {
        self.validate_decoded_structure_with_limits_and_cancellation(limits, &|| false)
    }

    fn validate_decoded_structure_with_limits_and_cancellation(
        &self,
        limits: ImageDataValidationLimits,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ImageDataValidationSummary, ImageDataValidationError> {
        if is_cancelled() {
            return Err(ImageDataValidationError::DecodeCancelled);
        }
        match self {
            Self::EncodedFile(_) | Self::EncodedLease(_) => {
                Err(ImageDataValidationError::EncodedDataRequiresBoundedDecode)
            }
            Self::Rgba8 {
                data,
                width,
                height,
                hash,
            } => Self::validate_rgba8(data, *width, *height, hash, limits, is_cancelled),
            Self::AnimRgba8 {
                width,
                height,
                durations,
                frames,
                hashes,
            } => {
                Self::validate_dimensions(*width, *height, limits)?;
                if frames.is_empty() {
                    return Err(ImageDataValidationError::AnimationHasNoFrames);
                }
                if frames.len() != durations.len() || frames.len() != hashes.len() {
                    return Err(ImageDataValidationError::AnimationCardinalityMismatch {
                        frames: frames.len(),
                        durations: durations.len(),
                        hashes: hashes.len(),
                    });
                }
                if frames.len() > limits.max_frame_count {
                    return Err(ImageDataValidationError::FrameCountLimitExceeded {
                        requested: frames.len(),
                        limit: limits.max_frame_count,
                    });
                }

                let expected = Self::expected_rgba8_len(*width, *height)?;
                let now = std::time::Instant::now();
                let mut decoded_bytes = 0usize;
                // This covers the full u32-millisecond timing domain used by
                // the Kitty image protocol while putting a deterministic,
                // cross-platform ceiling on renderer scheduling.
                let max_frame_duration = Duration::from_millis(u64::from(u32::MAX));
                for (frame_index, (frame, duration)) in
                    frames.iter().zip(durations.iter()).enumerate()
                {
                    if frame.len() != expected {
                        return Err(
                            ImageDataValidationError::AnimationFrameByteLengthMismatch {
                                frame_index,
                                width: *width,
                                height: *height,
                                expected,
                                actual: frame.len(),
                            },
                        );
                    }
                    // The renderer schedules with `Instant + duration`, which
                    // panics when the duration is outside the platform range.
                    // Zero durations remain valid: frame zero is used as a
                    // root frame by the Kitty protocol and rendering clamps
                    // frame cadence to its configured minimum.
                    if *duration > max_frame_duration || now.checked_add(*duration).is_none() {
                        return Err(
                            ImageDataValidationError::AnimationFrameDurationOutOfRange {
                                frame_index,
                            },
                        );
                    }
                    decoded_bytes = decoded_bytes
                        .checked_add(frame.len())
                        .ok_or(ImageDataValidationError::DecodedByteLengthOverflow)?;
                }
                if decoded_bytes > limits.max_decoded_bytes {
                    return Err(ImageDataValidationError::DecodedByteLimitExceeded {
                        requested: decoded_bytes,
                        limit: limits.max_decoded_bytes,
                    });
                }
                for (frame_index, (frame, hash)) in frames.iter().zip(hashes).enumerate() {
                    if Self::hash_bytes_with_cancellation(frame, is_cancelled)? != *hash {
                        return Err(ImageDataValidationError::AnimationFrameHashMismatch {
                            frame_index,
                        });
                    }
                }
                Ok(ImageDataValidationSummary {
                    decoded_bytes,
                    frame_count: frames.len(),
                })
            }
        }
    }

    #[cfg(feature = "use_image")]
    fn image_decoder_limits(limits: ImageDataValidationLimits) -> image::Limits {
        let mut decoder_limits = image::Limits::default();
        // The image crate enforces these before pixel allocation. Keep the
        // consumer's renderer-compatible axis ceiling independent from the
        // aggregate byte ceiling: a 1x16M strip is byte-bounded but can still
        // trigger pathological atlas sizing and repeated scale attempts.
        decoder_limits.max_image_width = Some(limits.max_width);
        decoder_limits.max_image_height = Some(limits.max_height);
        decoder_limits.max_alloc = Some(
            u64::try_from(limits.max_decoded_bytes).unwrap_or(u64::MAX),
        );
        decoder_limits
    }

    #[cfg(feature = "use_image")]
    fn bounded_decode_error(error: impl std::fmt::Display) -> ImageDataValidationError {
        ImageDataValidationError::EncodedDecodeFailed {
            message: error.to_string(),
        }
    }

    #[cfg(feature = "use_image")]
    fn encoded_source_io_error(
        source_kind: EncodedImageSource,
        operation: &'static str,
        source: std::io::Error,
    ) -> ImageDataValidationError {
        match source_kind {
            EncodedImageSource::InMemory => Self::bounded_decode_error(source),
            EncodedImageSource::BlobLease => ImageDataValidationError::EncodedResourceIo {
                operation,
                source,
            },
        }
    }

    #[cfg(feature = "use_image")]
    fn nested_io_error(error: &(dyn std::error::Error + 'static)) -> Option<std::io::Error> {
        let mut source = error.source();
        while let Some(error) = source {
            if let Some(error) = error.downcast_ref::<std::io::Error>() {
                return Some(std::io::Error::new(error.kind(), error.to_string()));
            }
            source = error.source();
        }
        None
    }

    #[cfg(feature = "use_image")]
    fn classified_image_decode_error(
        source_kind: EncodedImageSource,
        operation: &'static str,
        source: image::ImageError,
    ) -> ImageDataValidationError {
        match source {
            image::ImageError::Limits(source) => {
                ImageDataValidationError::EncodedResourceLimit { source }
            }
            image::ImageError::IoError(source) => {
                Self::encoded_source_io_error(source_kind, operation, source)
            }
            source => {
                // Some image format decoders wrap reader failures inside a
                // DecodingError.  Those failures are malformed input for an
                // in-memory cursor, but a transient backing-store failure for
                // a blob lease.  Preserve that distinction through the image
                // crate's error chain so callers do not negative-cache a
                // temporarily unavailable lease as permanently malformed.
                if source_kind == EncodedImageSource::BlobLease
                    && let Some(io_error) = Self::nested_io_error(&source)
                {
                    return ImageDataValidationError::EncodedResourceIo {
                        operation,
                        source: io_error,
                    };
                }
                Self::bounded_decode_error(source)
            }
        }
    }

    #[cfg(all(feature = "use_image", feature = "std"))]
    fn encoded_lease_error(
        source: frankenterm_blob_leases::Error,
    ) -> ImageDataValidationError {
        ImageDataValidationError::EncodedLeaseUnavailable { source }
    }

    #[cfg(feature = "use_image")]
    fn verify_encoded_reader_revision<R: std::io::Read + std::io::Seek>(
        reader: &mut R,
        expected_revision: [u8; 32],
        max_encoded_bytes: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<(), ImageDataValidationError> {
        use sha2::Digest;
        use std::io::SeekFrom;

        reader.seek(SeekFrom::Start(0)).map_err(|source| {
            ImageDataValidationError::EncodedResourceIo {
                operation: "seeking blob lease before revision verification",
                source,
            }
        })?;
        let mut hasher = sha2::Sha256::new();
        let mut bytes_read = 0usize;
        let mut buffer = [0u8; 32 * 1024];
        loop {
            if is_cancelled() {
                return Err(ImageDataValidationError::DecodeCancelled);
            }
            let read = std::io::Read::read(reader, &mut buffer).map_err(|source| {
                ImageDataValidationError::EncodedResourceIo {
                    operation: "reading blob lease for revision verification",
                    source,
                }
            })?;
            if read == 0 {
                break;
            }
            bytes_read = bytes_read.checked_add(read).ok_or(
                ImageDataValidationError::EncodedByteLimitExceeded {
                    requested: usize::MAX,
                    limit: max_encoded_bytes,
                },
            )?;
            if bytes_read > max_encoded_bytes {
                return Err(ImageDataValidationError::EncodedByteLimitExceeded {
                    requested: bytes_read,
                    limit: max_encoded_bytes,
                });
            }
            hasher.update(&buffer[..read]);
        }
        if <[u8; 32]>::from(hasher.finalize()) != expected_revision {
            return Err(ImageDataValidationError::ContentRevisionMismatch);
        }
        reader.seek(SeekFrom::Start(0)).map_err(|source| {
            ImageDataValidationError::EncodedResourceIo {
                operation: "rewinding blob lease after revision verification",
                source,
            }
        })?;
        Ok(())
    }

    #[cfg(feature = "use_image")]
    fn decoded_static_from_decoder(
        decoder: impl image::ImageDecoder,
        source_kind: EncodedImageSource,
        limits: ImageDataValidationLimits,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, ImageDataValidationError> {
        if is_cancelled() {
            return Err(ImageDataValidationError::DecodeCancelled);
        }
        let (width, height) = decoder.dimensions();
        Self::validate_dimensions(width, height, limits)?;
        let expected = Self::expected_rgba8_len(width, height)?;
        if expected > limits.max_decoded_bytes {
            return Err(ImageDataValidationError::DecodedByteLimitExceeded {
                requested: expected,
                limit: limits.max_decoded_bytes,
            });
        }
        if limits.max_frame_count < 1 {
            return Err(ImageDataValidationError::FrameCountLimitExceeded {
                requested: 1,
                limit: limits.max_frame_count,
            });
        }
        let image = image::DynamicImage::from_decoder(decoder)
            .map_err(|source| {
                Self::classified_image_decode_error(
                    source_kind,
                    "decoding static image pixels",
                    source,
                )
            })?
            .into_rgba8();
        if is_cancelled() {
            return Err(ImageDataValidationError::DecodeCancelled);
        }
        let (decoded_width, decoded_height) = image.dimensions();
        if (decoded_width, decoded_height) != (width, height) {
            return Err(Self::bounded_decode_error(format_args!(
                "decoder dimensions changed from {width}x{height} to {decoded_width}x{decoded_height}"
            )));
        }
        let data = image.into_vec();
        if data.len() != expected {
            return Err(ImageDataValidationError::RgbaByteLengthMismatch {
                width,
                height,
                expected,
                actual: data.len(),
            });
        }
        let hash = Self::hash_bytes_with_cancellation(&data, is_cancelled)?;
        Ok(Self::Rgba8 {
            data,
            width,
            height,
            hash,
        })
    }

    #[cfg(feature = "use_image")]
    fn decoded_animation_from_frames(
        frames: impl IntoIterator<Item = image::ImageResult<image::Frame>>,
        width: u32,
        height: u32,
        source_kind: EncodedImageSource,
        limits: ImageDataValidationLimits,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, ImageDataValidationError> {
        Self::validate_dimensions(width, height, limits)?;
        let expected = Self::expected_rgba8_len(width, height)?;
        if expected > limits.max_decoded_bytes {
            return Err(ImageDataValidationError::DecodedByteLimitExceeded {
                requested: expected,
                limit: limits.max_decoded_bytes,
            });
        }

        let mut decoded_frames = Vec::new();
        let mut durations = Vec::new();
        let mut hashes = Vec::new();
        let mut decoded_bytes = 0usize;
        let mut frames = frames.into_iter();
        loop {
            if is_cancelled() {
                return Err(ImageDataValidationError::DecodeCancelled);
            }
            let Some(frame) = frames.next() else {
                break;
            };
            let next_frame_count = decoded_frames
                .len()
                .checked_add(1)
                .ok_or(ImageDataValidationError::FrameCountLimitExceeded {
                    requested: usize::MAX,
                    limit: limits.max_frame_count,
                })?;
            if next_frame_count > limits.max_frame_count {
                return Err(ImageDataValidationError::FrameCountLimitExceeded {
                    requested: next_frame_count,
                    limit: limits.max_frame_count,
                });
            }
            let frame = frame.map_err(|source| {
                Self::classified_image_decode_error(
                    source_kind,
                    "decoding animated image frame",
                    source,
                )
            })?;
            let duration: Duration = frame.delay().into();
            let image = frame.into_buffer();
            let (frame_width, frame_height) = image.dimensions();
            if (frame_width, frame_height) != (width, height) {
                return Err(Self::bounded_decode_error(format_args!(
                    "animation frame dimensions {frame_width}x{frame_height} do not match {width}x{height}"
                )));
            }
            let data = image.into_vec();
            if data.len() != expected {
                return Err(ImageDataValidationError::AnimationFrameByteLengthMismatch {
                    frame_index: decoded_frames.len(),
                    width,
                    height,
                    expected,
                    actual: data.len(),
                });
            }
            decoded_bytes = decoded_bytes.checked_add(data.len()).ok_or(
                ImageDataValidationError::DecodedByteLengthOverflow,
            )?;
            if decoded_bytes > limits.max_decoded_bytes {
                return Err(ImageDataValidationError::DecodedByteLimitExceeded {
                    requested: decoded_bytes,
                    limit: limits.max_decoded_bytes,
                });
            }
            hashes.push(Self::hash_bytes_with_cancellation(&data, is_cancelled)?);
            durations.push(duration);
            decoded_frames.push(data);
        }

        if decoded_frames.is_empty() {
            return Err(ImageDataValidationError::AnimationHasNoFrames);
        }
        Ok(Self::AnimRgba8 {
            width,
            height,
            durations,
            frames: decoded_frames,
            hashes,
        })
    }

    #[cfg(feature = "use_image")]
    fn decode_reader_with_limits<R: std::io::BufRead + std::io::Seek>(
        reader: R,
        source_kind: EncodedImageSource,
        limits: ImageDataValidationLimits,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, ImageDataValidationError> {
        use image::{AnimationDecoder, ImageDecoder, ImageFormat};

        if is_cancelled() {
            return Err(ImageDataValidationError::DecodeCancelled);
        }
        let mut reader = image::ImageReader::new(reader)
            .with_guessed_format()
            .map_err(|source| {
                Self::encoded_source_io_error(
                    source_kind,
                    "detecting encoded image format",
                    source,
                )
            })?;
        let format = reader.format().ok_or_else(|| {
            Self::bounded_decode_error("encoded image format could not be identified")
        })?;
        let decoder_limits = Self::image_decoder_limits(limits);

        match format {
            ImageFormat::Gif => {
                let mut decoder = image::codecs::gif::GifDecoder::new(reader.into_inner())
                    .map_err(|source| {
                        Self::classified_image_decode_error(
                            source_kind,
                            "opening GIF decoder",
                            source,
                        )
                    })?;
                decoder.set_limits(decoder_limits).map_err(|source| {
                    Self::classified_image_decode_error(
                        source_kind,
                        "applying GIF decoder limits",
                        source,
                    )
                })?;
                let (width, height) = decoder.dimensions();
                Self::decoded_animation_from_frames(
                    decoder.into_frames(),
                    width,
                    height,
                    source_kind,
                    limits,
                    is_cancelled,
                )
            }
            ImageFormat::Png => {
                let decoder = image::codecs::png::PngDecoder::with_limits(
                    reader.into_inner(),
                    decoder_limits,
                )
                .map_err(|source| {
                    Self::classified_image_decode_error(
                        source_kind,
                        "opening PNG decoder",
                        source,
                    )
                })?;
                let (width, height) = decoder.dimensions();
                if decoder.is_apng().map_err(|source| {
                    Self::classified_image_decode_error(
                        source_kind,
                        "inspecting PNG animation metadata",
                        source,
                    )
                })? {
                    let decoder = decoder.apng().map_err(|source| {
                        Self::classified_image_decode_error(
                            source_kind,
                            "opening APNG decoder",
                            source,
                        )
                    })?;
                    Self::decoded_animation_from_frames(
                        decoder.into_frames(),
                        width,
                        height,
                        source_kind,
                        limits,
                        is_cancelled,
                    )
                } else {
                    Self::decoded_static_from_decoder(
                        decoder,
                        source_kind,
                        limits,
                        is_cancelled,
                    )
                }
            }
            ImageFormat::WebP => {
                let mut decoder = image::codecs::webp::WebPDecoder::new(reader.into_inner())
                    .map_err(|source| {
                        Self::classified_image_decode_error(
                            source_kind,
                            "opening WebP decoder",
                            source,
                        )
                    })?;
                decoder.set_limits(decoder_limits).map_err(|source| {
                    Self::classified_image_decode_error(
                        source_kind,
                        "applying WebP decoder limits",
                        source,
                    )
                })?;
                let (width, height) = decoder.dimensions();
                Self::decoded_animation_from_frames(
                    decoder.into_frames(),
                    width,
                    height,
                    source_kind,
                    limits,
                    is_cancelled,
                )
            }
            _ => {
                reader.limits(decoder_limits);
                let decoder = reader.into_decoder().map_err(|source| {
                    Self::classified_image_decode_error(
                        source_kind,
                        "opening static image decoder",
                        source,
                    )
                })?;
                Self::decoded_static_from_decoder(
                    decoder,
                    source_kind,
                    limits,
                    is_cancelled,
                )
            }
        }
    }

    /// Divide animation frame durations by `speed_factor`; a factor of 2 halves
    /// each duration. Invalid or overflowing factors fail without partially
    /// mutating the animation.
    pub fn adjust_speed(
        &mut self,
        speed_factor: f32,
    ) -> Result<(), ImageDataValidationError> {
        if !speed_factor.is_finite() || speed_factor <= 0.0 {
            return Err(ImageDataValidationError::InvalidAnimationSpeedFactor);
        }
        match self {
            Self::AnimRgba8 { durations, .. } => {
                let adjusted = durations
                    .iter()
                    .enumerate()
                    .map(|(frame_index, duration)| {
                        Duration::try_from_secs_f64(
                            duration.as_secs_f64() / f64::from(speed_factor),
                        )
                        .map_err(|_| {
                            ImageDataValidationError::AdjustedFrameDurationOutOfRange {
                                frame_index,
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                *durations = adjusted;
            }
            _ => {}
        }
        Ok(())
    }

    #[cfg(feature = "use_image")]
    pub fn dimensions(&self) -> Result<(u32, u32), ImageCellError> {
        fn dimensions_for_reader<R: std::io::BufRead + std::io::Seek>(
            reader: R,
        ) -> image::ImageResult<(u32, u32)> {
            let reader = image::ImageReader::new(reader).with_guessed_format()?;
            let (width, height) = reader.into_dimensions()?;

            Ok((width, height))
        }

        match self {
            ImageDataType::EncodedFile(data) => {
                if data.len() > MAX_IMAGE_WIRE_BYTES {
                    return Err(ImageCellError::EncodedImageTooLarge {
                        requested: data.len(),
                        limit: MAX_IMAGE_WIRE_BYTES,
                    });
                }
                Ok(dimensions_for_reader(std::io::Cursor::new(data))?)
            }
            ImageDataType::EncodedLease(lease) => {
                use std::io::{Seek, SeekFrom};
                let mut reader = lease.get_reader()?;
                let len = reader.seek(SeekFrom::End(0))?;
                if len > u64::try_from(MAX_IMAGE_WIRE_BYTES).unwrap_or(u64::MAX) {
                    return Err(ImageCellError::EncodedImageTooLarge {
                        requested: usize::try_from(len).unwrap_or(usize::MAX),
                        limit: MAX_IMAGE_WIRE_BYTES,
                    });
                }
                reader.seek(SeekFrom::Start(0))?;
                Ok(dimensions_for_reader(reader)?)
            }
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
        match self {
            Self::EncodedFile(data) if data.len() <= MAX_IMAGE_WIRE_BYTES => {
                match Self::decode_reader_with_limits(
                    std::io::Cursor::new(data.as_slice()),
                    EncodedImageSource::InMemory,
                    ImageDataValidationLimits {
                        max_decoded_bytes: MAX_IMAGE_WIRE_BYTES,
                        max_frame_count: MAX_IMAGE_WIRE_FRAMES,
                        max_width: 16_384,
                        max_height: 16_384,
                    },
                    &|| false,
                ) {
                    Ok(decoded) => decoded,
                    Err(error) => {
                        log::warn!("retaining encoded image after bounded decode failed: {error}");
                        Self::EncodedFile(data)
                    }
                }
            }
            Self::EncodedFile(data) => Self::EncodedFile(data),
            data => data,
        }
    }

    #[cfg(not(feature = "use_image"))]
    pub fn decode(self) -> Self {
        self
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

    #[error("encoded image retains {requested} bytes, exceeding the {limit}-byte limit")]
    EncodedImageTooLarge { requested: usize, limit: usize },
}

#[cfg_attr(feature = "use_serde", derive(Deserialize))]
pub struct ImageData {
    data: Mutex<ImageDataType>,
    hash: [u8; 32],
    #[cfg_attr(feature = "use_serde", serde(skip, default))]
    current_revision: Mutex<Option<[u8; 32]>>,
}

/// Read-only image payload guard. Mutations must use [`ImageData::data_mut`]
/// so the cached content revision cannot silently go stale.
pub struct ImageDataReadGuard<'a>(MutexGuard<'a, ImageDataType>);

impl Deref for ImageDataReadGuard<'_> {
    type Target = ImageDataType;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Exclusive mutable image payload access that invalidates the cached content
/// revision before mutation and republishes it while still holding the data
/// lock on drop.
pub struct ImageDataMutGuard<'a> {
    data: MutexGuard<'a, ImageDataType>,
    current_revision: &'a Mutex<Option<[u8; 32]>>,
}

impl Deref for ImageDataMutGuard<'_> {
    type Target = ImageDataType;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl DerefMut for ImageDataMutGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.data
    }
}

impl Drop for ImageDataMutGuard<'_> {
    fn drop(&mut self) {
        self.data.refresh_embedded_hashes();
        let revision = self.data.compute_hash();
        let mut cached = self.current_revision.lock().unwrap_or_else(|poisoned| {
            self.current_revision.clear_poison();
            poisoned.into_inner()
        });
        *cached = Some(revision);
    }
}

#[cfg(feature = "use_serde")]
impl Serialize for ImageData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        use serde::ser::SerializeStruct;

        // `Mutex<T>`'s derived Serialize implementation propagates poison as
        // a serialization error. ImageData's public accessors intentionally
        // recover the payload after a writer panic, so the wire path must use
        // that same recovery contract rather than making a recoverable image
        // permanently impossible to hydrate remotely.
        let data = self.data();
        let mut state = serializer.serialize_struct("ImageData", 2)?;
        state.serialize_field("data", &*data)?;
        state.serialize_field("hash", &self.hash)?;
        state.end()
    }
}

struct HexSlice<'a>(&'a [u8]);
impl<'a> std::fmt::Display for HexSlice<'a> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        for byte in self.0 {
            write!(fmt, "{byte:02x}")?;
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
        #[cfg(feature = "use_image")]
        let decoded = if data.len() > MAX_IMAGE_WIRE_BYTES {
            ImageDataType::EncodedFile(data)
        } else {
            match ImageDataType::decode_reader_with_limits(
                std::io::Cursor::new(data.as_slice()),
                EncodedImageSource::InMemory,
                ImageDataValidationLimits {
                    max_decoded_bytes: MAX_IMAGE_WIRE_BYTES,
                    max_frame_count: MAX_IMAGE_WIRE_FRAMES,
                    max_width: 16_384,
                    max_height: 16_384,
                },
                &|| false,
            ) {
                Ok(decoded) => decoded,
                Err(error) => {
                    log::warn!("retaining undecoded raw image after bounded decode failed: {error}");
                    ImageDataType::EncodedFile(data)
                }
            }
        };
        #[cfg(not(feature = "use_image"))]
        let decoded = ImageDataType::EncodedFile(data);
        Self::with_data_and_hash(decoded, hash)
    }

    fn with_data_and_hash(data: ImageDataType, hash: [u8; 32]) -> Self {
        let current_revision = data.compute_hash();
        Self {
            data: Mutex::new(data),
            hash,
            current_revision: Mutex::new(Some(current_revision)),
        }
    }

    /// Construct a canonical decoded payload that retains the verified source
    /// revision used to request it.
    ///
    /// Encoded bytes and their decoded RGBA representation intentionally have
    /// different canonical hashes. Remote image caches, however, are keyed by
    /// the source revision advertised by the producer. Once those bytes have
    /// been hashed, decoded, and structurally validated, the replacement must
    /// continue to publish that verified revision or the cache's pre-publish
    /// authority check will reject every encoded image. A later mutable access
    /// invalidates this cache and republishes the decoded content hash in the
    /// usual way.
    fn with_validated_source_revision(
        data: ImageDataType,
        source_revision: [u8; 32],
    ) -> Self {
        Self {
            data: Mutex::new(data),
            hash: source_revision,
            current_revision: Mutex::new(Some(source_revision)),
        }
    }

    fn revision_for_locked_data(&self, data: &ImageDataType) -> [u8; 32] {
        let mut cached = self.current_revision.lock().unwrap_or_else(|poisoned| {
            self.current_revision.clear_poison();
            poisoned.into_inner()
        });
        if let Some(revision) = *cached {
            revision
        } else {
            let revision = data.compute_hash();
            *cached = Some(revision);
            revision
        }
    }

    pub fn with_data(data: ImageDataType) -> Self {
        let hash = data.compute_hash();
        Self {
            data: Mutex::new(data),
            hash,
            current_revision: Mutex::new(Some(hash)),
        }
    }

    /// Hash of the image's current content revision.
    ///
    /// [`ImageData::hash`] is the stable identity assigned when the object was
    /// created. Kitty protocol operations deliberately mutate an existing
    /// image object in place; they update the decoded frame hashes, so this
    /// revision changes without changing that stable identity. Remote render
    /// references and cross-process caches must use this revision, not the
    /// stable object identity, to avoid replaying pre-edit pixels.
    #[must_use]
    pub fn current_content_hash(&self) -> [u8; 32] {
        {
            let cached = self.current_revision.lock().unwrap_or_else(|poisoned| {
                self.current_revision.clear_poison();
                poisoned.into_inner()
            });
            if let Some(revision) = *cached {
                return revision;
            }
        }

        // A mutable guard clears the cache before taking the payload lock, so
        // a miss must wait for any in-flight edit and then derive the complete
        // post-edit revision. The common cached path above never hashes or
        // takes the large payload mutex.
        let data = self.data();
        let revision = data.compute_hash();
        let mut cached = self.current_revision.lock().unwrap_or_else(|poisoned| {
            self.current_revision.clear_poison();
            poisoned.into_inner()
        });
        *cached = Some(revision);
        revision
    }

    /// Validate a requested content revision and, when necessary, produce a
    /// canonical decoded replacement suitable for renderer caches.
    ///
    /// Encoded variants are decoded with strict dimension limits, a bounded
    /// decoder allocation budget, an aggregate decoded-byte budget, and a
    /// frame-count budget. The caller supplies cancellation so queued or
    /// abandoned off-main work can stop between animation frames and hashing
    /// chunks. Already-decoded mutable objects are validated in place and use
    /// the separately cached current revision, avoiding whole-image clones.
    #[cfg(feature = "use_image")]
    pub fn normalize_for_content_revision_with_limits(
        &self,
        expected_revision: [u8; 32],
        max_encoded_bytes: usize,
        limits: ImageDataValidationLimits,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ImageDataRevisionValidation, ImageDataValidationError> {
        if is_cancelled() {
            return Err(ImageDataValidationError::DecodeCancelled);
        }

        let data = self.data();
        let normalized = match &*data {
            ImageDataType::EncodedFile(encoded) => {
                if encoded.len() > max_encoded_bytes {
                    return Err(ImageDataValidationError::EncodedByteLimitExceeded {
                        requested: encoded.len(),
                        limit: max_encoded_bytes,
                    });
                }
                if ImageDataType::hash_bytes_with_cancellation(encoded, is_cancelled)?
                    != expected_revision
                {
                    return Err(ImageDataValidationError::ContentRevisionMismatch);
                }
                ImageDataType::decode_reader_with_limits(
                    std::io::Cursor::new(encoded.as_slice()),
                    EncodedImageSource::InMemory,
                    limits,
                    is_cancelled,
                )?
            }
            ImageDataType::EncodedLease(lease) => {
                if lease.content_id().as_hash_bytes() != expected_revision {
                    return Err(ImageDataValidationError::ContentRevisionMismatch);
                }
                let mut reader = lease
                    .get_reader()
                    .map_err(ImageDataType::encoded_lease_error)?;
                use std::io::{Seek, SeekFrom};
                let encoded_len = reader.seek(SeekFrom::End(0)).map_err(|source| {
                    ImageDataValidationError::EncodedResourceIo {
                        operation: "seeking blob lease to determine encoded length",
                        source,
                    }
                })?;
                if encoded_len > u64::try_from(max_encoded_bytes).unwrap_or(u64::MAX) {
                    return Err(ImageDataValidationError::EncodedByteLimitExceeded {
                        requested: usize::try_from(encoded_len).unwrap_or(usize::MAX),
                        limit: max_encoded_bytes,
                    });
                }
                ImageDataType::verify_encoded_reader_revision(
                    &mut reader,
                    expected_revision,
                    max_encoded_bytes,
                    is_cancelled,
                )?;
                ImageDataType::decode_reader_with_limits(
                    reader,
                    EncodedImageSource::BlobLease,
                    limits,
                    is_cancelled,
                )?
            }
            decoded => {
                // A canonical replacement decoded from an encoded source
                // deliberately retains the already-verified source revision
                // in its cache. Honor that authority so normalization is
                // idempotent. Deserialized payloads start with an empty cache,
                // and therefore still derive their revision from the decoded
                // structure here rather than trusting a peer-provided field.
                let current_revision = self.revision_for_locked_data(decoded);
                if current_revision != expected_revision {
                    return Err(ImageDataValidationError::ContentRevisionMismatch);
                }
                // The cheap revision check above uses the stored per-frame
                // hashes to reject a stale coordinate before hashing all
                // pixels. Structural validation below independently hashes
                // every decoded buffer, so an untrusted peer cannot make that
                // fast path authoritative by forging the inner hashes.
                let summary = decoded
                    .validate_decoded_structure_with_limits_and_cancellation(
                        limits,
                        is_cancelled,
                    )?;
                return Ok(ImageDataRevisionValidation {
                    summary,
                    // The client cache is explicitly keyed by the requested
                    // source revision, and ImageCell shape hashes now use the
                    // cached current revision. Retaining this decoded object
                    // avoids cloning up to the full image/animation budget.
                    replacement: None,
                });
            }
        };

        let summary = normalized.validate_decoded_structure_with_limits_and_cancellation(
            limits,
            is_cancelled,
        )?;
        let replacement = Self::with_validated_source_revision(normalized, expected_revision);
        debug_assert_eq!(replacement.hash(), expected_revision);
        debug_assert_eq!(replacement.current_content_hash(), expected_revision);
        Ok(ImageDataRevisionValidation {
            summary,
            replacement: Some(replacement),
        })
    }

    /// Validate image payload structure before admitting it to decoded-image
    /// consumers such as the renderer.
    ///
    /// This validates decoded structure only. [`ImageData::hash`] is a stable
    /// source/object identity and may intentionally differ from the current
    /// decoded revision after `with_raw_data` or a Kitty in-place edit. Trust
    /// boundaries must call
    /// [`ImageData::normalize_for_content_revision_with_limits`] with the
    /// requested revision to bind structure to cache authority.
    pub fn validate_decoded_structure(
        &self,
    ) -> Result<ImageDataValidationSummary, ImageDataValidationError> {
        self.validate_decoded_structure_with_limits(ImageDataValidationLimits::UNBOUNDED)
    }

    pub fn validate_decoded_structure_with_limits(
        &self,
        limits: ImageDataValidationLimits,
    ) -> Result<ImageDataValidationSummary, ImageDataValidationError> {
        let data = self.data();
        data.validate_decoded_structure_with_limits(limits)
    }

    /// Returns the in-memory footprint
    pub fn len(&self) -> usize {
        match &*self.data() {
            ImageDataType::EncodedFile(d) => d.len(),
            ImageDataType::EncodedLease(_) => 0,
            ImageDataType::Rgba8 { data, .. } => data.len(),
            ImageDataType::AnimRgba8 { frames, .. } => frames
                .iter()
                .fold(0usize, |acc, frame| acc.saturating_add(frame.len())),
        }
    }

    pub fn data(&self) -> ImageDataReadGuard<'_> {
        ImageDataReadGuard(self.data.lock().unwrap_or_else(|poisoned| {
            #[cfg(feature = "use_image")]
            log::warn!(
                "recovering poisoned ImageData lock for image hash {:x?}",
                self.hash
            );
            self.data.clear_poison();
            poisoned.into_inner()
        }))
    }

    /// Mutably access image pixels or animation metadata while maintaining the
    /// O(1) content-revision cache used by remote hydration and shape caching.
    pub fn data_mut(&self) -> ImageDataMutGuard<'_> {
        {
            let mut cached = self.current_revision.lock().unwrap_or_else(|poisoned| {
                self.current_revision.clear_poison();
                poisoned.into_inner()
            });
            *cached = None;
        }
        let data = self.data.lock().unwrap_or_else(|poisoned| {
            #[cfg(feature = "use_image")]
            log::warn!(
                "recovering poisoned mutable ImageData lock for image hash {:x?}",
                self.hash
            );
            self.data.clear_poison();
            poisoned.into_inner()
        });
        // Another writer may have completed and repopulated the cache while
        // this writer waited for the payload lock. Clear it again before
        // handing mutable access to the caller.
        {
            let mut cached = self.current_revision.lock().unwrap_or_else(|poisoned| {
                self.current_revision.clear_poison();
                poisoned.into_inner()
            });
            *cached = None;
        }
        ImageDataMutGuard {
            data,
            current_revision: &self.current_revision,
        }
    }

    /// Consume this uniquely owned wrapper and return its payload without
    /// cloning potentially large decoded frame buffers.
    pub fn into_data(self) -> ImageDataType {
        self.data.into_inner().unwrap_or_else(|poisoned| {
            #[cfg(feature = "use_image")]
            log::warn!("recovering poisoned uniquely owned ImageData payload");
            poisoned.into_inner()
        })
    }

    pub fn hash(&self) -> [u8; 32] {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one_pixel_png() -> Vec<u8> {
        use image::ImageEncoder;

        let mut encoded = Vec::new();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(
                &[0x11, 0x22, 0x33, 0xff],
                1,
                1,
                image::ExtendedColorType::Rgba8,
            )
            .unwrap();
        encoded
    }

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
    #[should_panic(expected = "single-frame image dimensions must be non-zero, got 0x1")]
    fn image_data_type_new_single_frame_rejects_zero_dimensions() {
        ImageDataType::new_single_frame(0, 1, Vec::new());
    }

    #[test]
    #[should_panic(expected = "invalid dimensions 4294967295x1 for pixel data of length 12")]
    fn image_data_type_new_single_frame_rejects_wrapped_byte_len() {
        ImageDataType::new_single_frame(u32::MAX, 1, vec![0u8; 12]);
    }

    #[test]
    #[should_panic(
        expected = "invalid dimensions 4294967295x4294967295 for pixel data of length 0"
    )]
    fn image_data_type_new_single_frame_preserves_diagnostic_for_extreme_dimensions() {
        ImageDataType::new_single_frame(u32::MAX, u32::MAX, Vec::new());
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
        idt.adjust_speed(2.0).unwrap();
        assert_eq!(idt, ImageDataType::EncodedFile(vec![1, 2, 3]));
    }

    #[test]
    fn image_data_type_adjust_speed_is_transactional_and_checked() {
        let frame = vec![0x33; 4];
        let mut image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(80)],
            hashes: vec![ImageDataType::hash_bytes(&frame)],
            frames: vec![frame],
        };
        image.adjust_speed(2.0).unwrap();
        let ImageDataType::AnimRgba8 { durations, .. } = &image else {
            unreachable!();
        };
        assert_eq!(durations, &[Duration::from_millis(40)]);

        let before = image.clone();
        assert!(matches!(
            image.adjust_speed(0.0),
            Err(ImageDataValidationError::InvalidAnimationSpeedFactor)
        ));
        assert_eq!(image, before, "a rejected speed must not partially mutate");
    }

    #[test]
    fn image_data_validation_accepts_consistent_rgba8() {
        let image = ImageDataType::new_single_frame(2, 2, vec![0x5a; 16]);
        assert_eq!(
            image.validate_decoded_structure().unwrap(),
            ImageDataValidationSummary {
                decoded_bytes: 16,
                frame_count: 1,
            }
        );
    }

    #[test]
    fn image_data_validation_rejects_zero_dimensions() {
        let image = ImageDataType::Rgba8 {
            data: Vec::new(),
            width: 0,
            height: 1,
            hash: ImageDataType::hash_bytes(&[]),
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::ZeroDimensions {
                width: 0,
                height: 1
            })
        ));
    }

    #[test]
    fn image_data_validation_rejects_rgba8_dimension_overflow() {
        let image = ImageDataType::Rgba8 {
            data: Vec::new(),
            width: u32::MAX,
            height: u32::MAX,
            hash: ImageDataType::hash_bytes(&[]),
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::RgbaByteLengthOverflow {
                width: u32::MAX,
                height: u32::MAX
            })
        ));
    }

    #[test]
    fn image_data_validation_rejects_rgba8_byte_length_mismatch() {
        let data = vec![0x7f; 15];
        let image = ImageDataType::Rgba8 {
            hash: ImageDataType::hash_bytes(&data),
            data,
            width: 2,
            height: 2,
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::RgbaByteLengthMismatch {
                width: 2,
                height: 2,
                expected: 16,
                actual: 15
            })
        ));
    }

    #[test]
    fn image_data_validation_rejects_rgba8_internal_hash_mismatch() {
        let image = ImageDataType::Rgba8 {
            data: vec![0x44; 4],
            width: 1,
            height: 1,
            hash: [0; 32],
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::RgbaHashMismatch)
        ));
    }

    #[test]
    fn image_data_validation_rejects_rgba_before_hashing_past_byte_limit() {
        let data = vec![0x45; 16];
        let image = ImageDataType::Rgba8 {
            hash: ImageDataType::hash_bytes(&data),
            data,
            width: 2,
            height: 2,
        };
        assert!(matches!(
            image.validate_decoded_structure_with_limits(ImageDataValidationLimits {
                max_decoded_bytes: 15,
                max_frame_count: 1,
                ..ImageDataValidationLimits::UNBOUNDED
            }),
            Err(ImageDataValidationError::DecodedByteLimitExceeded {
                requested: 16,
                limit: 15,
            })
        ));
    }

    #[test]
    fn image_data_validation_rejects_renderer_axis_limit_before_hashing() {
        let data = vec![0x45; 8];
        let image = ImageDataType::Rgba8 {
            hash: ImageDataType::hash_bytes(&data),
            data,
            width: 2,
            height: 1,
        };
        assert!(matches!(
            image.validate_decoded_structure_with_limits(ImageDataValidationLimits {
                max_width: 1,
                max_height: 1,
                ..ImageDataValidationLimits::UNBOUNDED
            }),
            Err(ImageDataValidationError::DimensionLimitExceeded {
                width: 2,
                height: 1,
                max_width: 1,
                max_height: 1,
            })
        ));
    }

    #[test]
    fn image_data_validation_accepts_animation_with_zero_duration_root_frame() {
        let first = vec![0x10; 4];
        let second = vec![0x20; 4];
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::ZERO, Duration::from_millis(20)],
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
        };
        assert_eq!(
            image.validate_decoded_structure().unwrap(),
            ImageDataValidationSummary {
                decoded_bytes: 8,
                frame_count: 2,
            }
        );
    }

    #[test]
    fn image_data_validation_rejects_empty_animation() {
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: Vec::new(),
            frames: Vec::new(),
            hashes: Vec::new(),
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::AnimationHasNoFrames)
        ));
    }

    #[test]
    fn image_data_validation_rejects_animation_frame_count_limit() {
        let first = vec![0x21; 4];
        let second = vec![0x22; 4];
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::ZERO, Duration::from_millis(1)],
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
        };
        assert!(matches!(
            image.validate_decoded_structure_with_limits(ImageDataValidationLimits {
                max_decoded_bytes: 8,
                max_frame_count: 1,
                ..ImageDataValidationLimits::UNBOUNDED
            }),
            Err(ImageDataValidationError::FrameCountLimitExceeded {
                requested: 2,
                limit: 1,
            })
        ));
    }

    #[test]
    fn image_data_validation_rejects_animation_aggregate_byte_limit() {
        let first = vec![0x23; 4];
        let second = vec![0x24; 4];
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::ZERO, Duration::from_millis(1)],
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
        };
        assert!(matches!(
            image.validate_decoded_structure_with_limits(ImageDataValidationLimits {
                max_decoded_bytes: 7,
                max_frame_count: 2,
                ..ImageDataValidationLimits::UNBOUNDED
            }),
            Err(ImageDataValidationError::DecodedByteLimitExceeded {
                requested: 8,
                limit: 7,
            })
        ));
    }

    #[test]
    fn image_data_validation_rejects_animation_duration_cardinality_mismatch() {
        let frame = vec![0x30; 4];
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: Vec::new(),
            hashes: vec![ImageDataType::hash_bytes(&frame)],
            frames: vec![frame],
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::AnimationCardinalityMismatch {
                frames: 1,
                durations: 0,
                hashes: 1
            })
        ));
    }

    #[test]
    fn image_data_validation_rejects_animation_hash_cardinality_mismatch() {
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(10)],
            hashes: Vec::new(),
            frames: vec![vec![0x40; 4]],
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::AnimationCardinalityMismatch {
                frames: 1,
                durations: 1,
                hashes: 0
            })
        ));
    }

    #[test]
    fn image_data_validation_rejects_animation_frame_byte_length_mismatch() {
        let frame = vec![0x50; 3];
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(10)],
            hashes: vec![ImageDataType::hash_bytes(&frame)],
            frames: vec![frame],
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(
                ImageDataValidationError::AnimationFrameByteLengthMismatch {
                    frame_index: 0,
                    width: 1,
                    height: 1,
                    expected: 4,
                    actual: 3
                }
            )
        ));
    }

    #[test]
    fn image_data_validation_rejects_animation_frame_internal_hash_mismatch() {
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(10)],
            hashes: vec![[0; 32]],
            frames: vec![vec![0x60; 4]],
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::AnimationFrameHashMismatch { frame_index: 0 })
        ));
    }

    #[test]
    fn image_data_validation_rejects_animation_duration_that_would_overflow_instant() {
        let frame = vec![0x70; 4];
        let image = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::MAX],
            hashes: vec![ImageDataType::hash_bytes(&frame)],
            frames: vec![frame],
        };
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::AnimationFrameDurationOutOfRange {
                frame_index: 0
            })
        ));
    }

    // ── ImageData ──────────────────────────────────────────

    #[test]
    fn image_data_validation_requires_bounded_decode_for_encoded_file() {
        let image = ImageDataType::EncodedFile(one_pixel_png());
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::EncodedDataRequiresBoundedDecode)
        ));
    }

    #[test]
    fn image_data_validation_requires_bounded_decode_even_for_empty_encoded_file() {
        let image = ImageDataType::EncodedFile(Vec::new());
        assert!(matches!(
            image.validate_decoded_structure(),
            Err(ImageDataValidationError::EncodedDataRequiresBoundedDecode)
        ));
    }

    #[cfg(all(feature = "use_image", feature = "std"))]
    #[test]
    fn encoded_decode_error_classification_preserves_resource_failures() {
        let resource_io = ImageDataType::classified_image_decode_error(
            EncodedImageSource::BlobLease,
            "test lease read",
            image::ImageError::IoError(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "temporary lease read failure",
            )),
        );
        match resource_io {
            ImageDataValidationError::EncodedResourceIo {
                operation,
                source,
            } => {
                assert_eq!(operation, "test lease read");
                assert_eq!(source.kind(), std::io::ErrorKind::WouldBlock);
            }
            other => panic!("lease I/O must remain resource-classified, got {other:?}"),
        }

        let wrapped_resource_io = ImageDataType::classified_image_decode_error(
            EncodedImageSource::BlobLease,
            "test wrapped lease read",
            image::ImageError::Decoding(image::error::DecodingError::new(
                image::error::ImageFormatHint::Unknown,
                std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "wrapped temporary lease read failure",
                ),
            )),
        );
        match wrapped_resource_io {
            ImageDataValidationError::EncodedResourceIo {
                operation,
                source,
            } => {
                assert_eq!(operation, "test wrapped lease read");
                assert_eq!(source.kind(), std::io::ErrorKind::Interrupted);
            }
            other => panic!("wrapped lease I/O must remain resource-classified, got {other:?}"),
        }

        let in_memory_io = ImageDataType::classified_image_decode_error(
            EncodedImageSource::InMemory,
            "test cursor read",
            image::ImageError::IoError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated in-memory image",
            )),
        );
        assert!(matches!(
            in_memory_io,
            ImageDataValidationError::EncodedDecodeFailed { .. }
        ));

        let wrapped_in_memory_io = ImageDataType::classified_image_decode_error(
            EncodedImageSource::InMemory,
            "test wrapped cursor read",
            image::ImageError::Decoding(image::error::DecodingError::new(
                image::error::ImageFormatHint::Unknown,
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "wrapped truncated in-memory image",
                ),
            )),
        );
        assert!(matches!(
            wrapped_in_memory_io,
            ImageDataValidationError::EncodedDecodeFailed { .. }
        ));

        let resource_limit = ImageDataType::classified_image_decode_error(
            EncodedImageSource::InMemory,
            "test allocation",
            image::ImageError::Limits(image::error::LimitError::from_kind(
                image::error::LimitErrorKind::InsufficientMemory,
            )),
        );
        assert!(matches!(
            resource_limit,
            ImageDataValidationError::EncodedResourceLimit { .. }
        ));

        let lease_unavailable = ImageDataType::encoded_lease_error(
            frankenterm_blob_leases::Error::StorageNotInit,
        );
        assert!(matches!(
            lease_unavailable,
            ImageDataValidationError::EncodedLeaseUnavailable { .. }
        ));
    }

    #[test]
    fn decoded_image_hash_includes_dimensions() {
        let pixels = vec![0x31; 16];
        let wide = ImageDataType::Rgba8 {
            data: pixels.clone(),
            width: 2,
            height: 2,
            hash: ImageDataType::hash_bytes(&pixels),
        };
        let tall = ImageDataType::Rgba8 {
            data: pixels.clone(),
            width: 1,
            height: 4,
            hash: ImageDataType::hash_bytes(&pixels),
        };
        assert_ne!(wide.compute_hash(), tall.compute_hash());
    }

    #[test]
    fn animated_image_hash_includes_exact_duration() {
        let frame = vec![0x41; 4];
        let hash = ImageDataType::hash_bytes(&frame);
        let short = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_nanos(1)],
            frames: vec![frame.clone()],
            hashes: vec![hash],
        };
        let long = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_nanos(2)],
            frames: vec![frame],
            hashes: vec![hash],
        };
        assert_ne!(short.compute_hash(), long.compute_hash());
    }

    #[test]
    fn animated_image_hash_covers_mismatched_metadata_tails() {
        let frame = vec![0x51; 4];
        let frame_hash = ImageDataType::hash_bytes(&frame);
        let base = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(1)],
            frames: vec![frame],
            hashes: vec![frame_hash, [0x61; 32]],
        };

        let mut changed_trailing_hash = base.clone();
        let ImageDataType::AnimRgba8 { hashes, .. } = &mut changed_trailing_hash else {
            panic!("test image must remain animated");
        };
        hashes[1] = [0x62; 32];
        assert_ne!(
            base.compute_hash(),
            changed_trailing_hash.compute_hash(),
            "a frame hash beyond a short duration vector must remain revision-authoritative"
        );

        let mut added_trailing_duration = base.clone();
        let ImageDataType::AnimRgba8 { durations, .. } = &mut added_trailing_duration else {
            panic!("test image must remain animated");
        };
        durations.push(Duration::from_millis(2));
        assert_ne!(
            base.compute_hash(),
            added_trailing_duration.compute_hash(),
            "a duration beyond a short frame/hash vector must remain revision-authoritative"
        );

        let mut added_trailing_frame = base.clone();
        let ImageDataType::AnimRgba8 { frames, .. } = &mut added_trailing_frame else {
            panic!("test image must remain animated");
        };
        frames.push(vec![0x51; 4]);
        assert_ne!(
            base.compute_hash(),
            added_trailing_frame.compute_hash(),
            "a frame-count mismatch must remain revision-authoritative"
        );
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn animation_wire_rejects_4097th_duration_frame_and_hash() {
        let durations_over_limit = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::ZERO; MAX_IMAGE_WIRE_FRAMES + 1],
            frames: Vec::new(),
            hashes: Vec::new(),
        };
        let error = serde_json::to_string(&durations_over_limit).unwrap_err();
        assert!(error.to_string().contains("image durations"), "{}", error);

        let frames_over_limit = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: Vec::new(),
            frames: vec![Vec::new(); MAX_IMAGE_WIRE_FRAMES + 1],
            hashes: Vec::new(),
        };
        let error = serde_json::to_string(&frames_over_limit).unwrap_err();
        assert!(error.to_string().contains("image frames"), "{}", error);

        let hashes_over_limit = ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: Vec::new(),
            frames: Vec::new(),
            hashes: vec![[0; 32]; MAX_IMAGE_WIRE_FRAMES + 1],
        };
        let error = serde_json::to_string(&hashes_over_limit).unwrap_err();
        assert!(error.to_string().contains("image hashes"), "{}", error);
    }

    #[test]
    fn image_data_with_data() {
        let idt = ImageDataType::new_single_frame(2, 2, vec![0u8; 16]);
        let id = ImageData::with_data(idt);
        assert_ne!(id.hash(), [0u8; 32]);
    }

    #[test]
    fn mutable_image_guard_republishes_cached_content_revision() {
        let image = ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![1, 2, 3, 4],
        ));
        let stable_identity = image.hash();
        let before = image.current_content_hash();
        {
            let mut payload = image.data_mut();
            let ImageDataType::Rgba8 { data, .. } = &mut *payload else {
                panic!("expected RGBA payload");
            };
            data[0] = 9;
        }
        let after = image.current_content_hash();
        assert_ne!(after, before);
        assert_eq!(after, image.current_content_hash());
        assert_eq!(image.hash(), stable_identity);
        assert!(
            image.validate_decoded_structure().is_ok(),
            "the mutation guard must repair a stale per-pixel hash"
        );
    }

    #[test]
    fn mutable_image_guard_repairs_every_animation_frame_hash() {
        let first = vec![1, 2, 3, 4];
        let second = vec![5, 6, 7, 8];
        let image = ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::ZERO, Duration::from_millis(1)],
            hashes: vec![
                ImageDataType::hash_bytes(&first),
                ImageDataType::hash_bytes(&second),
            ],
            frames: vec![first, second],
        });
        let before = image.current_content_hash();

        {
            let mut payload = image.data_mut();
            let ImageDataType::AnimRgba8 { frames, hashes, .. } = &mut *payload else {
                panic!("expected animated RGBA payload");
            };
            frames[0][0] = 0xa1;
            frames[1][0] = 0xb2;
            hashes.clear();
        }

        assert_ne!(image.current_content_hash(), before);
        assert!(
            image.validate_decoded_structure().is_ok(),
            "the mutation guard must restore hash cardinality and recompute every frame digest"
        );
    }

    #[test]
    fn image_data_with_raw_data_preserves_source_identity_and_tracks_decoded_revision() {
        let encoded = one_pixel_png();
        let encoded_hash = ImageDataType::hash_bytes(&encoded);
        let image = ImageData::with_raw_data(encoded);

        let decoded_revision = image.current_content_hash();
        assert_eq!(image.hash(), encoded_hash);
        assert_ne!(decoded_revision, image.hash());
        let normalized = image
            .normalize_for_content_revision_with_limits(
                decoded_revision,
                1024,
                ImageDataValidationLimits {
                    max_decoded_bytes: 4,
                    max_frame_count: 1,
                    ..ImageDataValidationLimits::UNBOUNDED
                },
                &|| false,
            )
            .unwrap();
        assert!(
            normalized.replacement.is_none(),
            "decoded mutable objects must not clone their complete pixel buffers"
        );
        assert_eq!(normalized.summary.decoded_bytes, 4);
    }

    #[test]
    fn bounded_normalization_decodes_encoded_png_without_losing_source_revision() {
        let encoded = one_pixel_png();
        let source_revision = ImageDataType::hash_bytes(&encoded);
        let image = ImageData::with_data(ImageDataType::EncodedFile(encoded));
        let normalized = image
            .normalize_for_content_revision_with_limits(
                source_revision,
                1024,
                ImageDataValidationLimits {
                    max_decoded_bytes: 4,
                    max_frame_count: 1,
                    ..ImageDataValidationLimits::UNBOUNDED
                },
                &|| false,
            )
            .unwrap();
        let decoded = normalized
            .replacement
            .expect("encoded input must produce a canonical decoded replacement");
        assert_eq!(normalized.summary.decoded_bytes, 4);
        assert_eq!(normalized.summary.frame_count, 1);
        assert_eq!(decoded.hash(), source_revision);
        assert_eq!(decoded.current_content_hash(), source_revision);
        assert!(matches!(&*decoded.data(), ImageDataType::Rgba8 { .. }));

        let renormalized = decoded
            .normalize_for_content_revision_with_limits(
                source_revision,
                1024,
                ImageDataValidationLimits {
                    max_decoded_bytes: 4,
                    max_frame_count: 1,
                    ..ImageDataValidationLimits::UNBOUNDED
                },
                &|| false,
            )
            .expect("normalization must be idempotent for a verified decoded replacement");
        assert!(
            renormalized.replacement.is_none(),
            "an already canonical replacement must validate in place"
        );

        {
            let mut decoded_data = decoded.data_mut();
            let ImageDataType::Rgba8 { data, .. } = &mut *decoded_data else {
                panic!("normalization must retain the decoded RGBA representation");
            };
            data[0] ^= 0xff;
        }
        assert_eq!(
            decoded.hash(),
            source_revision,
            "the verified source identity remains stable after a local edit"
        );
        assert_ne!(
            decoded.current_content_hash(),
            source_revision,
            "a local edit must invalidate the preserved source revision"
        );
    }

    #[test]
    fn bounded_normalization_rejects_encoded_bytes_before_decode() {
        let encoded = one_pixel_png();
        let source_revision = ImageDataType::hash_bytes(&encoded);
        let image = ImageData::with_data(ImageDataType::EncodedFile(encoded));
        assert!(matches!(
            image.normalize_for_content_revision_with_limits(
                source_revision,
                1,
                ImageDataValidationLimits {
                    max_decoded_bytes: 4,
                    max_frame_count: 1,
                    ..ImageDataValidationLimits::UNBOUNDED
                },
                &|| false,
            ),
            Err(ImageDataValidationError::EncodedByteLimitExceeded {
                requested: _,
                limit: 1
            })
        ));
    }

    #[test]
    fn bounded_normalization_can_cancel_during_encoded_revision_hashing() {
        use std::cell::Cell;

        let encoded = vec![0x5a; 3 * 64 * 1024];
        let source_revision = ImageDataType::hash_bytes(&encoded);
        let image = ImageData::with_data(ImageDataType::EncodedFile(encoded));
        let cancellation_checks = Cell::new(0usize);
        let is_cancelled = || {
            let check = cancellation_checks.get();
            cancellation_checks.set(check + 1);
            check >= 2
        };

        assert!(matches!(
            image.normalize_for_content_revision_with_limits(
                source_revision,
                3 * 64 * 1024,
                ImageDataValidationLimits {
                    max_decoded_bytes: 4,
                    max_frame_count: 1,
                    ..ImageDataValidationLimits::UNBOUNDED
                },
                &is_cancelled,
            ),
            Err(ImageDataValidationError::DecodeCancelled)
        ));
        assert!(
            cancellation_checks.get() >= 3,
            "cancellation must be observed after hashing has begun, not only at normalization entry"
        );
    }

    #[test]
    fn animation_decode_checks_cancellation_before_pulling_the_next_frame() {
        use std::cell::Cell;

        let frame_pulls = Cell::new(0usize);
        let frames = std::iter::from_fn(|| {
            frame_pulls.set(frame_pulls.get() + 1);
            None::<image::ImageResult<image::Frame>>
        });
        let error = ImageDataType::decoded_animation_from_frames(
            frames,
            1,
            1,
            EncodedImageSource::InMemory,
            ImageDataValidationLimits {
                max_decoded_bytes: 4,
                max_frame_count: 1,
                ..ImageDataValidationLimits::UNBOUNDED
            },
            &|| true,
        )
        .expect_err("a cancelled decode must stop before asking for another frame");
        assert!(matches!(error, ImageDataValidationError::DecodeCancelled));
        assert_eq!(
            frame_pulls.get(),
            0,
            "cancellation must be checked before an iterator pull that may decode and allocate"
        );
    }

    #[test]
    fn revision_normalization_ignores_stable_outer_identity_but_rejects_wrong_revision() {
        let payload = ImageDataType::new_single_frame(1, 1, vec![1, 2, 3, 4]);
        let image = ImageData::with_data_and_hash(payload, [0xa5; 32]);

        assert!(image.validate_decoded_structure().is_ok());
        assert!(matches!(
            image.normalize_for_content_revision_with_limits(
                [0xa5; 32],
                1024,
                ImageDataValidationLimits {
                    max_decoded_bytes: 4,
                    max_frame_count: 1,
                    ..ImageDataValidationLimits::UNBOUNDED
                },
                &|| false,
            ),
            Err(ImageDataValidationError::ContentRevisionMismatch)
        ));
    }

    #[test]
    fn image_data_len() {
        let idt = ImageDataType::new_single_frame(2, 2, vec![0u8; 16]);
        let id = ImageData::with_data(idt);
        assert_eq!(id.len(), 16);
    }

    #[test]
    fn image_data_len_empty_animation() {
        let id = ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: Vec::new(),
            frames: Vec::new(),
            hashes: Vec::new(),
        });

        assert_eq!(id.len(), 0);
    }

    #[test]
    fn image_data_len_sums_animation_frame_buffers() {
        let id = ImageData::with_data(ImageDataType::AnimRgba8 {
            width: 1,
            height: 1,
            durations: vec![Duration::from_millis(10), Duration::from_millis(20)],
            frames: vec![vec![0u8; 4], vec![0u8; 8]],
            hashes: vec![
                ImageDataType::hash_bytes(&[0u8; 4]),
                ImageDataType::hash_bytes(&[0u8; 8]),
            ],
        });

        assert_eq!(id.len(), 12);
    }

    #[test]
    fn image_data_len_encoded_file() {
        let idt = ImageDataType::EncodedFile(vec![1, 2, 3, 4, 5]);
        let id = ImageData::with_data(idt);
        assert_eq!(id.len(), 5);
    }

    #[test]
    fn image_data_recovers_after_poisoned_data_lock() {
        let id = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![1, 2, 3, 4],
        )));
        let poisoned = Arc::clone(&id);

        let handle = std::thread::spawn(move || {
            let _guard = poisoned.data.lock().unwrap();
            panic!("simulate ImageData mutex poison");
        });

        assert!(handle.join().is_err());
        assert_eq!(id.len(), 4);

        let data = id.data();
        match &*data {
            ImageDataType::Rgba8 {
                width,
                height,
                data,
                ..
            } => {
                assert_eq!((*width, *height), (1, 1));
                assert_eq!(data, &[1, 2, 3, 4]);
            }
            other => panic!("expected Rgba8 after poison recovery, got {other:?}"),
        }
    }

    #[cfg(feature = "use_serde")]
    #[test]
    fn image_data_serialization_recovers_after_poisoned_data_lock() {
        let id = Arc::new(ImageData::with_data(ImageDataType::new_single_frame(
            1,
            1,
            vec![1, 2, 3, 4],
        )));
        let poisoned = Arc::clone(&id);
        let handle = std::thread::spawn(move || {
            let _guard = poisoned.data.lock().unwrap();
            panic!("simulate ImageData mutex poison before serialization");
        });
        assert!(handle.join().is_err());

        let encoded = serde_json::to_string(&*id)
            .expect("recoverable mutex poison must not make image serialization fail");
        let decoded: ImageData = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.hash(), id.hash());
        assert_eq!(&*decoded.data(), &*id.data());
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
        let err = ImageCellError::Io(std::io::Error::other("test"));
        let dbg = format!("{:?}", err);
        assert!(dbg.contains("Io"));
    }

    #[test]
    fn image_cell_error_display() {
        let err = ImageCellError::Io(std::io::Error::other("test error"));
        let msg = format!("{}", err);
        assert!(msg.contains("test error"));
    }
}
