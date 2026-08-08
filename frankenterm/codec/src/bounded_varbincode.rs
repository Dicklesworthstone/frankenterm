use byteorder::{LittleEndian, ReadBytesExt};
use serde::de::{self, Error as _, IntoDeserializer};
use std::convert::TryInto;
use std::io::Read;
use varbincode::error::{Error, Result};

const MAX_CONTAINER_ITEMS: usize = 1_000_000;

/// Zero-wire serde newtype names used by the exact-render schema to select a
/// tighter raw byte-buffer admission limit before allocation. The names are
/// serializer metadata only: varbincode does not write newtype names.
pub(crate) const EXACT_RENDER_ROW_UTF8_V1_NEWTYPE: &str = "frankenterm.codec.ExactRenderRowUtf8V1";
pub(crate) const EXACT_RENDER_METADATA_UTF8_V1_NEWTYPE: &str =
    "frankenterm.codec.ExactRenderMetadataUtf8V1";
pub(crate) const ORDERED_WINDOW_SECTION_V1_NEWTYPE: &str =
    "frankenterm.codec.OrderedWindowSectionV1";
pub(crate) const ORDERED_PANE_TREE_DESCRIPTORS_V1_NEWTYPE: &str =
    "frankenterm.codec.OrderedPaneTreeDescriptorsV1";
pub(crate) const ORDERED_PANE_NODES_V1_NEWTYPE: &str = "frankenterm.codec.OrderedPaneNodesV1";
pub(crate) const ORDERED_PANE_WINDOW_TITLES_V1_NEWTYPE: &str =
    "frankenterm.codec.OrderedPaneWindowTitlesV1";
pub(crate) const ORDERED_WINDOWS_V1_NEWTYPE: &str = "frankenterm.codec.OrderedWindowsV1";
pub(crate) const ORDERED_TAB_IDS_V1_NEWTYPE: &str = "frankenterm.codec.OrderedTabIdsV1";
pub(crate) const SERIALIZED_LINE_ENTRIES_V1_NEWTYPE: &str =
    "frankenterm.codec.SerializedLineEntriesV1";
pub(crate) const SERIALIZED_HYPERLINKS_V1_NEWTYPE: &str =
    "frankenterm.codec.SerializedHyperlinksV1";
pub(crate) const SERIALIZED_HYPERLINK_COORDINATES_V1_NEWTYPE: &str =
    "frankenterm.codec.SerializedHyperlinkCoordinatesV1";
pub(crate) const SERIALIZED_IMAGE_REFERENCES_V1_NEWTYPE: &str =
    "frankenterm.codec.SerializedImageReferencesV1";
pub(crate) const IMAGE_WIRE_BYTES_V1_NEWTYPE: &str = termwiz::image::IMAGE_WIRE_BYTES_V1_NEWTYPE;
pub(crate) const EXACT_RENDER_ROW_UTF8_V1_MAX_BYTES: usize = 1_000_000;
pub(crate) const EXACT_RENDER_METADATA_UTF8_V1_MAX_BYTES: usize = 65_536;
pub(crate) const ORDERED_WINDOW_SECTION_V1_MAX_BYTES: usize = 512 * 1024;
pub(crate) const ORDERED_PANE_TREE_DESCRIPTORS_V1_MAX_ITEMS: usize = 16_384;
pub(crate) const ORDERED_PANE_NODES_V1_MAX_ITEMS: usize = 32_767;
pub(crate) const ORDERED_PANE_WINDOW_TITLES_V1_MAX_ITEMS: usize = 4_096;
pub(crate) const ORDERED_WINDOWS_V1_MAX_ITEMS: usize = 4_096;
pub(crate) const ORDERED_TAB_IDS_V1_MAX_ITEMS: usize = 4_096;
pub(crate) const SERIALIZED_LINE_ENTRIES_V1_MAX_ITEMS: usize = super::MAX_RENDER_APPLICATION_LINES;
pub(crate) const SERIALIZED_HYPERLINKS_V1_MAX_ITEMS: usize =
    super::MAX_RENDER_APPLICATION_HYPERLINK_SPANS;
pub(crate) const SERIALIZED_HYPERLINK_COORDINATES_V1_MAX_ITEMS: usize =
    super::MAX_RENDER_APPLICATION_HYPERLINK_SPANS;
pub(crate) const SERIALIZED_IMAGE_REFERENCES_V1_MAX_ITEMS: usize =
    super::MAX_RENDER_APPLICATION_IMAGE_REFERENCES;
pub(crate) const IMAGE_WIRE_BYTES_V1_MAX_BYTES: usize = super::MAX_IMAGE_HYDRATION_DECODED_BYTES;

/// Default hard byte budget for an unmarked varbincode container or byte
/// buffer. Attacker-controlled leb128 lengths get clamped before they reach
/// [`Deserializer::read_vec`] allocation or visitor-driven serde collection
/// preallocation. Audited schema newtypes may select their own explicit cap;
/// notably, image bytes use the 64 MiB image-hydration budget, while nested
/// markers can only tighten an admission already in force. The 16 MiB default
/// keeps ordinary and fuzzed containers small and remains independent from the
/// outer frame-level [`super::MAX_PDU_SIZE`] (256 MiB).
pub(crate) const MAX_CONTAINER_BYTES: usize = 16 * 1024 * 1024;

pub fn deserialize<T: serde::de::DeserializeOwned, R: Read>(reader: &mut R) -> Result<T> {
    let mut deserializer = Deserializer::new(reader);
    serde::Deserialize::deserialize(&mut deserializer)
}

pub struct Deserializer<'a, R: Read> {
    reader: &'a mut R,
    pending_byte_buffer_admission: Option<ByteBufferAdmission>,
    pending_container_admission: Option<ContainerAdmission>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ByteBufferAdmission {
    label: &'static str,
    maximum: usize,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ContainerAdmission {
    label: &'static str,
    maximum: usize,
    preallocation_maximum: usize,
}

impl ContainerAdmission {
    const GLOBAL: Self = Self {
        label: "container",
        maximum: MAX_CONTAINER_ITEMS,
        preallocation_maximum: 4_096,
    };

    const ORDERED_PANE_TREE_DESCRIPTORS: Self = Self {
        label: "ordered pane tree descriptors",
        maximum: ORDERED_PANE_TREE_DESCRIPTORS_V1_MAX_ITEMS,
        preallocation_maximum: ORDERED_PANE_TREE_DESCRIPTORS_V1_MAX_ITEMS,
    };

    const ORDERED_PANE_NODES: Self = Self {
        label: "ordered pane arena nodes",
        maximum: ORDERED_PANE_NODES_V1_MAX_ITEMS,
        preallocation_maximum: ORDERED_PANE_NODES_V1_MAX_ITEMS,
    };

    const ORDERED_PANE_WINDOW_TITLES: Self = Self {
        label: "ordered pane window titles",
        maximum: ORDERED_PANE_WINDOW_TITLES_V1_MAX_ITEMS,
        preallocation_maximum: ORDERED_PANE_WINDOW_TITLES_V1_MAX_ITEMS,
    };

    const ORDERED_WINDOWS: Self = Self {
        label: "ordered windows",
        maximum: ORDERED_WINDOWS_V1_MAX_ITEMS,
        preallocation_maximum: ORDERED_WINDOWS_V1_MAX_ITEMS,
    };

    const ORDERED_TAB_IDS: Self = Self {
        label: "ordered tab ids",
        maximum: ORDERED_TAB_IDS_V1_MAX_ITEMS,
        preallocation_maximum: ORDERED_TAB_IDS_V1_MAX_ITEMS,
    };

    const SERIALIZED_LINE_ENTRIES: Self = Self {
        label: "serialized line entries",
        maximum: SERIALIZED_LINE_ENTRIES_V1_MAX_ITEMS,
        preallocation_maximum: SERIALIZED_LINE_ENTRIES_V1_MAX_ITEMS,
    };

    const SERIALIZED_HYPERLINKS: Self = Self {
        label: "serialized hyperlinks",
        maximum: SERIALIZED_HYPERLINKS_V1_MAX_ITEMS,
        preallocation_maximum: 4_096,
    };

    const SERIALIZED_HYPERLINK_COORDINATES: Self = Self {
        label: "serialized hyperlink coordinates",
        maximum: SERIALIZED_HYPERLINK_COORDINATES_V1_MAX_ITEMS,
        preallocation_maximum: 4_096,
    };

    const SERIALIZED_IMAGE_REFERENCES: Self = Self {
        label: "serialized image references",
        maximum: SERIALIZED_IMAGE_REFERENCES_V1_MAX_ITEMS,
        preallocation_maximum: SERIALIZED_IMAGE_REFERENCES_V1_MAX_ITEMS,
    };

    const fn restricted_by(self, requested: Self) -> Self {
        if requested.maximum <= self.maximum {
            requested
        } else {
            self
        }
    }
}

impl ByteBufferAdmission {
    const GLOBAL: Self = Self {
        label: "byte buffer",
        maximum: MAX_CONTAINER_BYTES,
    };

    const EXACT_RENDER_ROW_UTF8: Self = Self {
        label: "exact render row UTF-8 bytes",
        maximum: EXACT_RENDER_ROW_UTF8_V1_MAX_BYTES,
    };

    const EXACT_RENDER_METADATA_UTF8: Self = Self {
        label: "exact render metadata UTF-8 bytes",
        maximum: EXACT_RENDER_METADATA_UTF8_V1_MAX_BYTES,
    };

    const ORDERED_WINDOW_SECTION: Self = Self {
        label: "ordered-window section bytes",
        maximum: ORDERED_WINDOW_SECTION_V1_MAX_BYTES,
    };

    const IMAGE_WIRE_BYTES: Self = Self {
        label: "image wire bytes",
        maximum: IMAGE_WIRE_BYTES_V1_MAX_BYTES,
    };

    const fn restricted_by(self, requested: Self) -> Self {
        if requested.maximum < self.maximum {
            requested
        } else {
            self
        }
    }
}

impl<'a, R: Read> Deserializer<'a, R> {
    pub fn new(reader: &'a mut R) -> Self {
        Self {
            reader,
            pending_byte_buffer_admission: None,
            pending_container_admission: None,
        }
    }

    fn read_signed(&mut self) -> Result<i64> {
        leb128::read::signed(&mut self.reader).map_err(Into::into)
    }

    fn read_unsigned(&mut self) -> Result<u64> {
        leb128::read::unsigned(&mut self.reader).map_err(Into::into)
    }

    fn read_len_prefix(&mut self, kind: &str) -> Result<usize> {
        let raw_len = self.read_unsigned()?;
        raw_len
            .try_into()
            .map_err(|_| Error::custom(format!("{kind} length {raw_len} does not fit in usize")))
    }

    fn read_container_len(&mut self, kind: &str, admission: ContainerAdmission) -> Result<usize> {
        let len = self.read_len_prefix(kind)?;
        if len > admission.maximum {
            return Err(Error::custom(format!(
                "{} length {len} exceeds maximum {}",
                admission.label, admission.maximum,
            )));
        }
        Ok(len)
    }

    fn read_vec(&mut self) -> Result<Vec<u8>> {
        let admission = self
            .pending_byte_buffer_admission
            .take()
            .unwrap_or(ByteBufferAdmission::GLOBAL);
        let len = self.read_len_prefix(admission.label)?;
        if len > admission.maximum {
            return Err(Error::custom(format!(
                "{} length {len} exceeds maximum {}",
                admission.label, admission.maximum,
            )));
        }
        let mut result = Vec::new();
        result.try_reserve_exact(len).map_err(|err| {
            Error::custom(format!(
                "{} length {len} could not be allocated safely: {err}",
                admission.label,
            ))
        })?;
        result.resize(len, 0);
        self.reader.read_exact(&mut result)?;
        Ok(result)
    }

    fn read_string(&mut self) -> Result<String> {
        let vec = self.read_vec()?;
        String::from_utf8(vec).map_err(|e| Error::InvalidUtf8Encoding(e.utf8_error()))
    }
}

macro_rules! impl_uint {
    ($ty:ty, $dser_method:ident, $visitor_method:ident, $reader_method:ident) => {
        #[inline]
        fn $dser_method<V>(self, visitor: V) -> Result<V::Value>
        where
            V: de::Visitor<'de>,
        {
            let value = self.$reader_method()?;
            if value > <$ty>::MAX as u64 {
                Err(Error::NumberOutOfRange)
            } else {
                visitor.$visitor_method(value as $ty)
            }
        }
    };
}

macro_rules! impl_int {
    ($ty:ty, $dser_method:ident, $visitor_method:ident, $reader_method:ident) => {
        #[inline]
        fn $dser_method<V>(self, visitor: V) -> Result<V::Value>
        where
            V: de::Visitor<'de>,
        {
            let value = self.$reader_method()?;
            if value < <$ty>::MIN as i64 || value > <$ty>::MAX as i64 {
                Err(Error::NumberOutOfRange)
            } else {
                visitor.$visitor_method(value as $ty)
            }
        }
    };
}

macro_rules! impl_float {
    ($dser_method:ident, $visitor_method:ident, $reader_method:ident) => {
        #[inline]
        fn $dser_method<V>(self, visitor: V) -> Result<V::Value>
        where
            V: de::Visitor<'de>,
        {
            let value = self.reader.$reader_method::<LittleEndian>()?;
            visitor.$visitor_method(value)
        }
    };
}

impl<'de, 'a, 'b, R: Read> serde::Deserializer<'de> for &'a mut Deserializer<'b, R> {
    type Error = Error;

    #[inline]
    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_u8(self.reader.read_u8()?)
    }

    #[inline]
    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_i8(self.reader.read_i8()?)
    }

    impl_uint!(u16, deserialize_u16, visit_u16, read_unsigned);
    impl_uint!(u32, deserialize_u32, visit_u32, read_unsigned);
    impl_uint!(u64, deserialize_u64, visit_u64, read_unsigned);

    impl_int!(i16, deserialize_i16, visit_i16, read_signed);
    impl_int!(i32, deserialize_i32, visit_i32, read_signed);
    impl_int!(i64, deserialize_i64, visit_i64, read_signed);

    impl_float!(deserialize_f32, visit_f32, read_f32);
    impl_float!(deserialize_f64, visit_f64, read_f64);

    #[inline]
    fn deserialize_any<V>(self, _visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::DeserializeAnyNotSupported)
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let value: u8 = serde::Deserialize::deserialize(self)?;
        match value {
            1 => visitor.visit_bool(true),
            0 => visitor.visit_bool(false),
            value => Err(Error::InvalidBoolEncoding(value)),
        }
    }

    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let value: u32 = serde::Deserialize::deserialize(self)?;
        match std::char::from_u32(value) {
            Some(c) => visitor.visit_char(c),
            None => Err(Error::InvalidCharEncoding(value)),
        }
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_string(self.read_string()?)
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_string(self.read_string()?)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_byte_buf(self.read_vec()?)
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_byte_buf(self.read_vec()?)
    }

    fn deserialize_enum<V>(
        self,
        _enum: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_enum(self)
    }

    fn deserialize_tuple<V>(self, len: usize, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_seq(Access {
            deserializer: self,
            len,
            preallocation_maximum: ContainerAdmission::GLOBAL.preallocation_maximum,
        })
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let value: u8 = serde::de::Deserialize::deserialize(&mut *self)?;
        match value {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(&mut *self),
            v => Err(Error::InvalidTagEncoding(v as usize)),
        }
    }

    fn deserialize_seq<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let admission = self
            .pending_container_admission
            .take()
            .unwrap_or(ContainerAdmission::GLOBAL);
        let len = self.read_container_len("sequence", admission)?;
        visitor.visit_seq(Access {
            deserializer: self,
            len,
            preallocation_maximum: admission.preallocation_maximum,
        })
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let admission = self
            .pending_container_admission
            .take()
            .unwrap_or(ContainerAdmission::GLOBAL);
        let len = self.read_container_len("map", admission)?;
        visitor.visit_map(Access {
            deserializer: self,
            len,
            preallocation_maximum: admission.preallocation_maximum,
        })
    }

    fn deserialize_struct<V>(
        self,
        _name: &str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_tuple(fields.len(), visitor)
    }

    fn deserialize_identifier<V>(self, _visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::DeserializeIdentifierNotSupported)
    }

    fn deserialize_newtype_struct<V>(self, name: &str, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let requested_byte_buffer = if name == EXACT_RENDER_ROW_UTF8_V1_NEWTYPE {
            Some(ByteBufferAdmission::EXACT_RENDER_ROW_UTF8)
        } else if name == EXACT_RENDER_METADATA_UTF8_V1_NEWTYPE {
            Some(ByteBufferAdmission::EXACT_RENDER_METADATA_UTF8)
        } else if name == ORDERED_WINDOW_SECTION_V1_NEWTYPE {
            Some(ByteBufferAdmission::ORDERED_WINDOW_SECTION)
        } else if name == IMAGE_WIRE_BYTES_V1_NEWTYPE {
            Some(ByteBufferAdmission::IMAGE_WIRE_BYTES)
        } else {
            None
        };
        let requested_container = if name == ORDERED_PANE_TREE_DESCRIPTORS_V1_NEWTYPE {
            Some(ContainerAdmission::ORDERED_PANE_TREE_DESCRIPTORS)
        } else if name == ORDERED_PANE_NODES_V1_NEWTYPE {
            Some(ContainerAdmission::ORDERED_PANE_NODES)
        } else if name == ORDERED_PANE_WINDOW_TITLES_V1_NEWTYPE {
            Some(ContainerAdmission::ORDERED_PANE_WINDOW_TITLES)
        } else if name == ORDERED_WINDOWS_V1_NEWTYPE {
            Some(ContainerAdmission::ORDERED_WINDOWS)
        } else if name == ORDERED_TAB_IDS_V1_NEWTYPE {
            Some(ContainerAdmission::ORDERED_TAB_IDS)
        } else if name == SERIALIZED_LINE_ENTRIES_V1_NEWTYPE {
            Some(ContainerAdmission::SERIALIZED_LINE_ENTRIES)
        } else if name == SERIALIZED_HYPERLINKS_V1_NEWTYPE {
            Some(ContainerAdmission::SERIALIZED_HYPERLINKS)
        } else if name == SERIALIZED_HYPERLINK_COORDINATES_V1_NEWTYPE {
            Some(ContainerAdmission::SERIALIZED_HYPERLINK_COORDINATES)
        } else if name == SERIALIZED_IMAGE_REFERENCES_V1_NEWTYPE {
            Some(ContainerAdmission::SERIALIZED_IMAGE_REFERENCES)
        } else {
            None
        };
        let prior_byte_buffer = self.pending_byte_buffer_admission;
        let prior_container = self.pending_container_admission;
        if let Some(requested) = requested_byte_buffer {
            self.pending_byte_buffer_admission = Some(match prior_byte_buffer {
                // A nested schema marker may tighten, but never widen, an
                // admission already selected by its enclosing schema.
                Some(prior) => prior.restricted_by(requested),
                // A top-level schema marker is itself the authority. This is
                // what permits the audited 64 MiB image payload while ordinary
                // byte buffers retain the generic 16 MiB ceiling.
                None => requested,
            });
        }
        if let Some(requested) = requested_container {
            self.pending_container_admission = Some(match prior_container {
                Some(prior) => prior.restricted_by(requested),
                None => requested,
            });
        }
        let armed_byte_buffer = self.pending_byte_buffer_admission;
        let armed_container = self.pending_container_admission;
        let result = visitor.visit_newtype_struct(&mut *self);
        if self.pending_byte_buffer_admission == armed_byte_buffer {
            self.pending_byte_buffer_admission = prior_byte_buffer;
        }
        if self.pending_container_admission == armed_container {
            self.pending_container_admission = prior_container;
        }
        result
    }

    fn deserialize_unit_struct<V>(self, _name: &'static str, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_unit()
    }

    fn deserialize_tuple_struct<V>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_ignored_any<V>(self, _visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        Err(Error::DeserializeIgnoredAnyNotSupported)
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

struct Access<'a, 'b, R: Read> {
    deserializer: &'a mut Deserializer<'b, R>,
    len: usize,
    preallocation_maximum: usize,
}

impl<'de, 'a, 'b, R: Read> de::SeqAccess<'de> for Access<'a, 'b, R> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>>
    where
        T: de::DeserializeSeed<'de>,
    {
        if self.len > 0 {
            self.len -= 1;
            let value = de::DeserializeSeed::deserialize(seed, &mut *self.deserializer)?;
            Ok(Some(value))
        } else {
            Ok(None)
        }
    }

    fn size_hint(&self) -> Option<usize> {
        // Generic containers retain the conservative 4096-element hint.
        // A schema-scoped newtype can raise that hint only after its exact
        // length prefix has passed the correspondingly tighter hard ceiling,
        // allowing known q=16384 snapshots to allocate once without allowing
        // arbitrary wire lengths to become allocation requests.
        Some(self.len.min(self.preallocation_maximum))
    }
}

impl<'de, 'a, 'b, R: Read> de::MapAccess<'de> for Access<'a, 'b, R> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>>
    where
        K: de::DeserializeSeed<'de>,
    {
        if self.len > 0 {
            self.len -= 1;
            let key = de::DeserializeSeed::deserialize(seed, &mut *self.deserializer)?;
            Ok(Some(key))
        } else {
            Ok(None)
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value>
    where
        V: de::DeserializeSeed<'de>,
    {
        de::DeserializeSeed::deserialize(seed, &mut *self.deserializer)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.len.min(self.preallocation_maximum))
    }
}

impl<'de, 'a, 'b, R: Read> de::EnumAccess<'de> for &'a mut Deserializer<'b, R> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant)>
    where
        V: de::DeserializeSeed<'de>,
    {
        let idx: u32 = de::Deserialize::deserialize(&mut *self)?;
        let val: Result<_> = seed.deserialize(idx.into_deserializer());
        Ok((val?, self))
    }
}

impl<'de, 'a, 'b, R: Read> de::VariantAccess<'de> for &'a mut Deserializer<'b, R> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value>
    where
        T: de::DeserializeSeed<'de>,
    {
        de::DeserializeSeed::deserialize(seed, self)
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        serde::de::Deserializer::deserialize_tuple(self, len, visitor)
    }

    fn struct_variant<V>(self, fields: &'static [&'static str], visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        serde::de::Deserializer::deserialize_tuple(self, fields.len(), visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::convert::TryFrom;
    use std::fmt;

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    struct TwoSequences {
        first: Vec<u8>,
        second: Vec<u8>,
    }

    struct OrderedTabSequence(TwoSequences);

    struct OrderedTabSequenceVisitor;

    impl<'de> de::Visitor<'de> for OrderedTabSequenceVisitor {
        type Value = OrderedTabSequence;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an ordered-tab marker around two sequences")
        }

        fn visit_newtype_struct<D>(
            self,
            deserializer: D,
        ) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            TwoSequences::deserialize(deserializer).map(OrderedTabSequence)
        }
    }

    impl<'de> Deserialize<'de> for OrderedTabSequence {
        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer
                .deserialize_newtype_struct(ORDERED_TAB_IDS_V1_NEWTYPE, OrderedTabSequenceVisitor)
        }
    }

    #[derive(Debug)]
    struct OrderedTabVector(Vec<u8>);

    struct OrderedTabVectorVisitor;

    impl<'de> de::Visitor<'de> for OrderedTabVectorVisitor {
        type Value = OrderedTabVector;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an ordered-tab marker around one sequence")
        }

        fn visit_newtype_struct<D>(
            self,
            deserializer: D,
        ) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            Vec::<u8>::deserialize(deserializer).map(OrderedTabVector)
        }
    }

    impl<'de> Deserialize<'de> for OrderedTabVector {
        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer
                .deserialize_newtype_struct(ORDERED_TAB_IDS_V1_NEWTYPE, OrderedTabVectorVisitor)
        }
    }

    macro_rules! marked_vector_type {
        ($type_name:ident, $visitor_name:ident, $newtype_name:expr) => {
            #[derive(Debug)]
            struct $type_name(Vec<u8>);

            struct $visitor_name;

            impl<'de> de::Visitor<'de> for $visitor_name {
                type Value = $type_name;

                fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                    formatter.write_str("a schema-marked vector")
                }

                fn visit_newtype_struct<D>(
                    self,
                    deserializer: D,
                ) -> std::result::Result<Self::Value, D::Error>
                where
                    D: serde::Deserializer<'de>,
                {
                    Vec::<u8>::deserialize(deserializer).map($type_name)
                }
            }

            impl<'de> Deserialize<'de> for $type_name {
                fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
                where
                    D: serde::Deserializer<'de>,
                {
                    deserializer.deserialize_newtype_struct($newtype_name, $visitor_name)
                }
            }
        };
    }

    marked_vector_type!(
        SerializedLineEntriesVector,
        SerializedLineEntriesVectorVisitor,
        SERIALIZED_LINE_ENTRIES_V1_NEWTYPE
    );
    marked_vector_type!(
        SerializedHyperlinksVector,
        SerializedHyperlinksVectorVisitor,
        SERIALIZED_HYPERLINKS_V1_NEWTYPE
    );
    marked_vector_type!(
        SerializedHyperlinkCoordinatesVector,
        SerializedHyperlinkCoordinatesVectorVisitor,
        SERIALIZED_HYPERLINK_COORDINATES_V1_NEWTYPE
    );
    marked_vector_type!(
        SerializedImageReferencesVector,
        SerializedImageReferencesVectorVisitor,
        SERIALIZED_IMAGE_REFERENCES_V1_NEWTYPE
    );

    #[derive(Debug)]
    struct ImageWireBytes(Vec<u8>);

    struct ImageWireBytesVisitor;

    impl<'de> de::Visitor<'de> for ImageWireBytesVisitor {
        type Value = ImageWireBytes;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("schema-marked image bytes")
        }

        fn visit_newtype_struct<D>(
            self,
            deserializer: D,
        ) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_byte_buf(ImageWireBytesBufferVisitor)
        }
    }

    struct ImageWireBytesBufferVisitor;

    impl<'de> de::Visitor<'de> for ImageWireBytesBufferVisitor {
        type Value = ImageWireBytes;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an image byte buffer")
        }

        fn visit_byte_buf<E>(self, value: Vec<u8>) -> std::result::Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            Ok(ImageWireBytes(value))
        }
    }

    impl<'de> Deserialize<'de> for ImageWireBytes {
        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer
                .deserialize_newtype_struct(IMAGE_WIRE_BYTES_V1_NEWTYPE, ImageWireBytesVisitor)
        }
    }

    #[derive(Debug)]
    struct NestedWindowTitleVector(Vec<u8>);

    struct NestedWindowTitleVectorVisitor;

    impl<'de> de::Visitor<'de> for NestedWindowTitleVectorVisitor {
        type Value = NestedWindowTitleVector;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("nested ordered-window-title marker")
        }

        fn visit_newtype_struct<D>(
            self,
            deserializer: D,
        ) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            Vec::<u8>::deserialize(deserializer).map(NestedWindowTitleVector)
        }
    }

    impl<'de> Deserialize<'de> for NestedWindowTitleVector {
        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_newtype_struct(
                ORDERED_PANE_WINDOW_TITLES_V1_NEWTYPE,
                NestedWindowTitleVectorVisitor,
            )
        }
    }

    #[derive(Debug)]
    struct NestedOrderedTabVector(NestedWindowTitleVector);

    struct NestedOrderedTabVectorVisitor;

    impl<'de> de::Visitor<'de> for NestedOrderedTabVectorVisitor {
        type Value = NestedOrderedTabVector;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("nested ordered-tab marker")
        }

        fn visit_newtype_struct<D>(
            self,
            deserializer: D,
        ) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            NestedWindowTitleVector::deserialize(deserializer).map(NestedOrderedTabVector)
        }
    }

    impl<'de> Deserialize<'de> for NestedOrderedTabVector {
        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_newtype_struct(
                ORDERED_PANE_TREE_DESCRIPTORS_V1_NEWTYPE,
                NestedOrderedTabVectorVisitor,
            )
        }
    }

    #[derive(Debug)]
    struct NestedTreeDescriptorVector(Vec<u8>);

    struct NestedTreeDescriptorVectorVisitor;

    impl<'de> de::Visitor<'de> for NestedTreeDescriptorVectorVisitor {
        type Value = NestedTreeDescriptorVector;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("nested ordered-pane-tree descriptor marker")
        }

        fn visit_newtype_struct<D>(
            self,
            deserializer: D,
        ) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            Vec::<u8>::deserialize(deserializer).map(NestedTreeDescriptorVector)
        }
    }

    impl<'de> Deserialize<'de> for NestedTreeDescriptorVector {
        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_newtype_struct(
                ORDERED_PANE_TREE_DESCRIPTORS_V1_NEWTYPE,
                NestedTreeDescriptorVectorVisitor,
            )
        }
    }

    #[derive(Debug)]
    struct OuterWindowTitleVector(NestedTreeDescriptorVector);

    struct OuterWindowTitleVectorVisitor;

    impl<'de> de::Visitor<'de> for OuterWindowTitleVectorVisitor {
        type Value = OuterWindowTitleVector;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("outer ordered-window-title marker")
        }

        fn visit_newtype_struct<D>(
            self,
            deserializer: D,
        ) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            NestedTreeDescriptorVector::deserialize(deserializer).map(OuterWindowTitleVector)
        }
    }

    impl<'de> Deserialize<'de> for OuterWindowTitleVector {
        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_newtype_struct(
                ORDERED_PANE_WINDOW_TITLES_V1_NEWTYPE,
                OuterWindowTitleVectorVisitor,
            )
        }
    }

    fn write_len(wire: &mut Vec<u8>, len: usize) {
        let len = u64::try_from(len).expect("test length fits u64");
        leb128::write::unsigned(wire, len).expect("write in-memory length prefix");
    }

    fn assert_marked_vector_rejects_prefix<T>(len: usize, expected_label: &str)
    where
        T: serde::de::DeserializeOwned + std::fmt::Debug,
    {
        let mut wire = Vec::new();
        write_len(&mut wire, len);
        let mut reader = wire.as_slice();
        let error = deserialize::<T, _>(&mut reader)
            .expect_err("schema admission must reject before reading vector elements");
        assert!(error.to_string().contains(expected_label), "{}", error);
    }

    #[test]
    fn serialized_line_resource_markers_reject_oversized_prefixes_before_allocation() {
        assert_marked_vector_rejects_prefix::<SerializedLineEntriesVector>(
            SERIALIZED_LINE_ENTRIES_V1_MAX_ITEMS + 1,
            "serialized line entries length",
        );
        assert_marked_vector_rejects_prefix::<SerializedHyperlinksVector>(
            SERIALIZED_HYPERLINKS_V1_MAX_ITEMS + 1,
            "serialized hyperlinks length",
        );
        assert_marked_vector_rejects_prefix::<SerializedHyperlinkCoordinatesVector>(
            SERIALIZED_HYPERLINK_COORDINATES_V1_MAX_ITEMS + 1,
            "serialized hyperlink coordinates length",
        );
        assert_marked_vector_rejects_prefix::<SerializedImageReferencesVector>(
            SERIALIZED_IMAGE_REFERENCES_V1_MAX_ITEMS + 1,
            "serialized image references length",
        );
    }

    #[test]
    fn image_wire_marker_rejects_64_mib_plus_one_before_allocating_payload() {
        let mut wire = Vec::new();
        write_len(&mut wire, IMAGE_WIRE_BYTES_V1_MAX_BYTES + 1);
        let mut reader = wire.as_slice();
        let error = deserialize::<ImageWireBytes, _>(&mut reader)
            .expect_err("oversized image bytes must fail from the length prefix alone");
        assert!(
            error.to_string().contains("image wire bytes length"),
            "{}",
            error
        );
    }

    #[test]
    fn container_marker_is_consumed_by_only_the_first_nested_container() {
        let second_len = ORDERED_TAB_IDS_V1_MAX_ITEMS + 1;
        let mut wire = Vec::with_capacity(second_len + 8);
        write_len(&mut wire, 0);
        write_len(&mut wire, second_len);
        wire.resize(wire.len() + second_len, 0x5a);

        let mut reader = wire.as_slice();
        let decoded: OrderedTabSequence =
            deserialize(&mut reader).expect("second sequence uses global admission");
        assert!(decoded.0.first.is_empty());
        assert_eq!(decoded.0.second, vec![0x5a; second_len]);
    }

    #[test]
    fn consumed_container_marker_does_not_leak_after_prefix_error() {
        let rejected_len = ORDERED_TAB_IDS_V1_MAX_ITEMS + 1;
        let mut wire = Vec::with_capacity(rejected_len + 16);
        write_len(&mut wire, rejected_len);
        write_len(&mut wire, rejected_len);
        wire.resize(wire.len() + rejected_len, 0x6b);
        let mut reader = wire.as_slice();
        let mut deserializer = Deserializer::new(&mut reader);

        let rejected = OrderedTabVector::deserialize(&mut deserializer)
            .expect_err("the marked sequence must reject its oversized prefix");
        assert!(
            rejected.to_string().contains("ordered tab ids length"),
            "unexpected marker rejection: {}",
            rejected,
        );
        let recovered = Vec::<u8>::deserialize(&mut deserializer)
            .expect("the following unmarked sequence must use global admission");
        assert_eq!(recovered, vec![0x6b; rejected_len]);
    }

    #[test]
    fn nested_container_markers_can_only_tighten_the_pending_admission() {
        let rejected_len = ORDERED_PANE_WINDOW_TITLES_V1_MAX_ITEMS + 1;
        let mut wire = Vec::new();
        write_len(&mut wire, rejected_len);

        let mut reader = wire.as_slice();
        let rejected = deserialize::<NestedOrderedTabVector, _>(&mut reader)
            .expect_err("the tighter nested marker must reject before elements");
        assert!(
            rejected
                .to_string()
                .contains("ordered pane window titles length"),
            "unexpected nested marker rejection: {}",
            rejected,
        );
    }

    #[test]
    fn nested_container_marker_cannot_relax_a_tighter_outer_admission() {
        let rejected_len = ORDERED_PANE_WINDOW_TITLES_V1_MAX_ITEMS + 1;
        let mut wire = Vec::new();
        write_len(&mut wire, rejected_len);

        let mut reader = wire.as_slice();
        let rejected = deserialize::<OuterWindowTitleVector, _>(&mut reader)
            .expect_err("the looser inner marker must preserve the outer ceiling");
        assert!(
            rejected
                .to_string()
                .contains("ordered pane window titles length"),
            "unexpected non-relaxation rejection: {}",
            rejected,
        );
    }
}
