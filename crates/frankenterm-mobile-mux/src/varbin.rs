//! Varbincode decoding of the server's bytes: exactly the wire format
//! `varbincode::deserialize` reads, except that every length is checked
//! against the bytes that remain before anything is allocated or iterated.
//! A corrupt or hostile length then fails the decode instead of asking for
//! gigabytes (an allocation failure aborts the app) or looping for ages.
//!
//! Every element of the schemas read here takes at least one byte, so a
//! sequence or map can never hold more elements than bytes remain.

use serde::de::{self, DeserializeSeed, IntoDeserializer, Visitor};
use std::fmt;

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl de::Error for Error {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

type Result<T> = std::result::Result<T, Error>;

fn error(message: &str) -> Error {
    Error(message.to_owned())
}

/// Decodes `T` from the front of `bytes`; trailing bytes (reply fields this
/// client does not read) are ignored.
pub fn deserialize<T: de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    T::deserialize(&mut Deserializer { input: bytes })
}

/// Effect receipts are complete schemas: trailing bytes cannot silently turn
/// an incompatible response into authority to send the next input operation.
pub fn deserialize_exact<T: de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let mut decoder = Deserializer { input: bytes };
    let value = T::deserialize(&mut decoder)?;
    if !decoder.input.is_empty() {
        return Err(error("unexpected trailing reply bytes"));
    }
    Ok(value)
}

struct Deserializer<'de> {
    input: &'de [u8],
}

impl<'de> Deserializer<'de> {
    fn take(&mut self, count: usize) -> Result<&'de [u8]> {
        if count > self.input.len() {
            return Err(error("a length runs past the end of the reply"));
        }
        let (head, tail) = self.input.split_at(count);
        self.input = tail;
        Ok(head)
    }

    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn unsigned(&mut self) -> Result<u64> {
        leb128::read::unsigned(&mut self.input).map_err(|_| error("bad unsigned number"))
    }

    fn signed(&mut self) -> Result<i64> {
        leb128::read::signed(&mut self.input).map_err(|_| error("bad signed number"))
    }

    /// A length prefix, which cannot exceed the bytes that follow it.
    fn length(&mut self) -> Result<usize> {
        usize::try_from(self.unsigned()?)
            .ok()
            .filter(|length| *length <= self.input.len())
            .ok_or_else(|| error("a length runs past the end of the reply"))
    }

    fn bytes(&mut self) -> Result<&'de [u8]> {
        let length = self.length()?;
        self.take(length)
    }

    fn str(&mut self) -> Result<&'de str> {
        std::str::from_utf8(self.bytes()?).map_err(|_| error("text is not UTF-8"))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }
}

macro_rules! unsigned {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            let value =
                <$ty>::try_from(self.unsigned()?).map_err(|_| error("number out of range"))?;
            visitor.$visit(value)
        }
    };
}

macro_rules! signed {
    ($method:ident, $visit:ident, $ty:ty) => {
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
            let value =
                <$ty>::try_from(self.signed()?).map_err(|_| error("number out of range"))?;
            visitor.$visit(value)
        }
    };
}

impl<'de> de::Deserializer<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(error("self-describing values are not part of the protocol"))
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.byte()? {
            0 => visitor.visit_bool(false),
            1 => visitor.visit_bool(true),
            _ => Err(error("bad bool")),
        }
    }

    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_u8(self.byte()?)
    }

    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_i8(i8::from_le_bytes([self.byte()?]))
    }

    unsigned!(deserialize_u16, visit_u16, u16);
    unsigned!(deserialize_u32, visit_u32, u32);
    unsigned!(deserialize_u64, visit_u64, u64);
    signed!(deserialize_i16, visit_i16, i16);
    signed!(deserialize_i32, visit_i32, i32);
    signed!(deserialize_i64, visit_i64, i64);

    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_f32(f32::from_le_bytes(self.array()?))
    }

    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_f64(f64::from_le_bytes(self.array()?))
    }

    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let value = u32::try_from(self.unsigned()?).map_err(|_| error("bad char"))?;
        visitor.visit_char(char::from_u32(value).ok_or_else(|| error("bad char"))?)
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_str(self.str()?)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_str(self.str()?)
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_bytes(self.bytes()?)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_borrowed_bytes(self.bytes()?)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        match self.byte()? {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(self),
            _ => Err(error("bad option tag")),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let len = self.length()?;
        visitor.visit_seq(Access {
            deserializer: self,
            len,
        })
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value> {
        visitor.visit_seq(Access {
            deserializer: self,
            len,
        })
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_tuple(len, visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value> {
        let len = self.length()?;
        visitor.visit_map(Access {
            deserializer: self,
            len,
        })
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        self.deserialize_tuple(fields.len(), visitor)
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        visitor.visit_enum(self)
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(error("identifiers are not part of the protocol"))
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value> {
        Err(error("ignored values are not part of the protocol"))
    }

    fn is_human_readable(&self) -> bool {
        false
    }
}

struct Access<'a, 'de> {
    deserializer: &'a mut Deserializer<'de>,
    len: usize,
}

impl<'de> de::SeqAccess<'de> for Access<'_, 'de> {
    type Error = Error;

    fn next_element_seed<T: DeserializeSeed<'de>>(&mut self, seed: T) -> Result<Option<T::Value>> {
        if self.len == 0 {
            return Ok(None);
        }
        self.len -= 1;
        seed.deserialize(&mut *self.deserializer).map(Some)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.len)
    }
}

impl<'de> de::MapAccess<'de> for Access<'_, 'de> {
    type Error = Error;

    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>> {
        if self.len == 0 {
            return Ok(None);
        }
        self.len -= 1;
        seed.deserialize(&mut *self.deserializer).map(Some)
    }

    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value> {
        seed.deserialize(&mut *self.deserializer)
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.len)
    }
}

impl<'de> de::EnumAccess<'de> for &mut Deserializer<'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self)> {
        let index = u32::try_from(self.unsigned()?).map_err(|_| error("bad variant"))?;
        let value = seed.deserialize(index.into_deserializer())?;
        Ok((value, self))
    }
}

impl<'de> de::VariantAccess<'de> for &mut Deserializer<'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value> {
        seed.deserialize(self)
    }

    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value> {
        de::Deserializer::deserialize_tuple(self, len, visitor)
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value> {
        de::Deserializer::deserialize_tuple(self, fields.len(), visitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    enum Shape {
        Empty,
        Pair(i32, String),
        Named { x: Option<u16>, y: Vec<u8> },
    }

    #[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq)]
    struct Sample {
        flag: bool,
        small: u8,
        neg: i64,
        big: u64,
        ratio: f32,
        text: String,
        letter: char,
        shapes: Vec<Shape>,
        names: HashMap<usize, String>,
        range: std::ops::Range<isize>,
    }

    #[test]
    fn reads_what_varbincode_writes() {
        let sample = Sample {
            flag: true,
            small: 200,
            neg: -1_000_000,
            big: u64::MAX,
            ratio: 0.25,
            text: "héllo".into(),
            letter: '界',
            shapes: vec![
                Shape::Empty,
                Shape::Pair(-3, "x".into()),
                Shape::Named {
                    x: Some(7),
                    y: vec![1, 2, 3],
                },
            ],
            names: HashMap::from([(3, "three".to_owned())]),
            range: -5..9,
        };
        let bytes = varbincode::serialize(&sample).unwrap();
        assert_eq!(deserialize::<Sample>(&bytes).unwrap(), sample);
    }

    #[test]
    fn a_length_past_the_end_fails_instead_of_allocating() {
        // A string claiming 2^62 bytes, then a sequence claiming 2^40 items.
        let mut huge = Vec::new();
        leb128::write::unsigned(&mut huge, 1 << 62).unwrap();
        assert!(deserialize::<String>(&huge).is_err());
        let mut many = Vec::new();
        leb128::write::unsigned(&mut many, 1 << 40).unwrap();
        many.extend_from_slice(&[0; 16]);
        assert!(deserialize::<Vec<u8>>(&many).is_err());
        assert!(deserialize::<Vec<String>>(&[3, 0, 0]).is_err());
    }
}
