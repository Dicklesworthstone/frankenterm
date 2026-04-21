use byteorder::{LittleEndian, ReadBytesExt};
use serde::de::{self, Error as _, IntoDeserializer};
use std::io::Read;
use varbincode::error::{Error, Result};

const MAX_CONTAINER_ITEMS: usize = 1_000_000;

pub fn deserialize<T: serde::de::DeserializeOwned, R: Read>(reader: &mut R) -> Result<T> {
    let mut deserializer = Deserializer::new(reader);
    serde::Deserialize::deserialize(&mut deserializer)
}

pub struct Deserializer<'a, R: Read> {
    reader: &'a mut R,
}

impl<'a, R: Read> Deserializer<'a, R> {
    pub fn new(reader: &'a mut R) -> Self {
        Self { reader }
    }

    fn read_signed(&mut self) -> Result<i64> {
        leb128::read::signed(&mut self.reader).map_err(Into::into)
    }

    fn read_unsigned(&mut self) -> Result<u64> {
        leb128::read::unsigned(&mut self.reader).map_err(Into::into)
    }

    fn read_container_len(&mut self, kind: &str) -> Result<usize> {
        let len: usize = serde::Deserialize::deserialize(&mut *self)?;
        if len > MAX_CONTAINER_ITEMS {
            return Err(Error::custom(format!(
                "{kind} length {len} exceeds safe maximum {MAX_CONTAINER_ITEMS}"
            )));
        }
        Ok(len)
    }

    fn read_vec(&mut self) -> Result<Vec<u8>> {
        let len: usize = serde::Deserialize::deserialize(&mut *self)?;
        if len > super::MAX_PDU_SIZE {
            return Err(Error::custom(format!(
                "byte buffer length {len} exceeds maximum {}",
                super::MAX_PDU_SIZE
            )));
        }
        let mut result = vec![0u8; len];
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
            if value > <$ty>::max_value() as u64 {
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
            if value < <$ty>::min_value() as i64 || value > <$ty>::max_value() as i64 {
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
            value => Err(Error::InvalidBoolEncoding(value).into()),
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
        let len = self.read_container_len("sequence")?;
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V>(self, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        let len = self.read_container_len("map")?;
        visitor.visit_map(Access {
            deserializer: self,
            len,
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

    fn deserialize_newtype_struct<V>(self, _name: &str, visitor: V) -> Result<V::Value>
    where
        V: de::Visitor<'de>,
    {
        visitor.visit_newtype_struct(self)
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
        Some(self.len.min(MAX_CONTAINER_ITEMS))
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
        Some(self.len.min(MAX_CONTAINER_ITEMS))
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
