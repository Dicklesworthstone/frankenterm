use std::fmt;
use std::marker::PhantomData;

use serde::de::{IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::WireError;

/// UTF-8 text with a byte limit, including after JSON escape decoding.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Text<const MAX: usize>(String);

impl<const MAX: usize> Text<MAX> {
    pub fn new(value: &str) -> Result<Self, WireError> {
        if value.len() > MAX {
            return Err(WireError::InvalidBounds);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<'de, const MAX: usize> Deserialize<'de> for Text<MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TextVisitor<const MAX: usize>;
        impl<'de, const MAX: usize> Visitor<'de> for TextVisitor<MAX> {
            type Value = Text<MAX>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("bounded UTF-8 text")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Text::new(value).map_err(E::custom)
            }

            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                if value.len() > MAX {
                    Err(E::custom(WireError::InvalidBounds))
                } else {
                    Ok(Text(value))
                }
            }
        }
        deserializer.deserialize_str(TextVisitor::<MAX>)
    }
}

/// An exact opaque identity. UUIDs remain UUID strings, never transient pane
/// numbers. The backend establishes the meaning and authority of these IDs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Id(Text<128>);

impl Id {
    pub fn new(value: &str) -> Result<Self, WireError> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
        {
            return Err(WireError::InvalidIdentifier);
        }
        Ok(Self(Text::new(value)?))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Text::<128>::deserialize(deserializer)?;
        Self::new(value.as_str()).map_err(serde::de::Error::custom)
    }
}

/// Asset/content identity, canonical lower-case SHA-256 hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Sha256(Text<64>);

impl Sha256 {
    pub fn new(value: &str) -> Result<Self, WireError> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(WireError::InvalidIdentifier);
        }
        Ok(Self(Text::new(value)?))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<'de> Deserialize<'de> for Sha256 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Text::<64>::deserialize(deserializer)?;
        Self::new(value.as_str()).map_err(serde::de::Error::custom)
    }
}

/// A u64 represented ONLY by its canonical decimal string on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct WireU64(pub u64);

impl WireU64 {
    pub fn next(self) -> Result<Self, WireError> {
        self.0.checked_add(1).map(Self).ok_or(WireError::StaleSequence)
    }

    pub fn parse(value: &str) -> Result<Self, WireError> {
        if value.is_empty()
            || value.len() > 20
            || (value.len() > 1 && value.starts_with('0'))
            || !value.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(WireError::InvalidNumber);
        }
        value.parse().map(Self).map_err(|_| WireError::InvalidNumber)
    }
}

impl Serialize for WireU64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for WireU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DecimalVisitor;
        impl Visitor<'_> for DecimalVisitor {
            type Value = WireU64;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a canonical decimal u64 string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                WireU64::parse(value).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(DecimalVisitor)
    }
}

/// Signed stable-row coordinates are also exact strings across JavaScript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WireI64(pub i64);

impl WireI64 {
    pub fn parse(value: &str) -> Result<Self, WireError> {
        let magnitude = value.strip_prefix('-').unwrap_or(value);
        WireU64::parse(magnitude)?;
        if value == "-0" {
            return Err(WireError::InvalidNumber);
        }
        value.parse().map(Self).map_err(|_| WireError::InvalidNumber)
    }
}

impl Serialize for WireI64 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for WireI64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SignedVisitor;
        impl Visitor<'_> for SignedVisitor {
            type Value = WireI64;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a canonical decimal i64 string")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                WireI64::parse(value).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(SignedVisitor)
    }
}

/// Collection length is checked before every append. No size hint reserves
/// memory; an excess element is skipped without constructing another T.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct BoundedVec<T, const MAX: usize>(Vec<T>);

impl<T, const MAX: usize> BoundedVec<T, MAX> {
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    pub fn try_push(&mut self, value: T) -> Result<(), WireError> {
        if self.0.len() >= MAX {
            return Err(WireError::LimitExceeded);
        }
        self.0.push(value);
        Ok(())
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    /// Move the admitted elements without cloning their owned payloads.
    pub fn into_vec(self) -> Vec<T> {
        self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T, const MAX: usize> Default for BoundedVec<T, MAX> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const MAX: usize> TryFrom<Vec<T>> for BoundedVec<T, MAX> {
    type Error = WireError;

    fn try_from(value: Vec<T>) -> Result<Self, Self::Error> {
        if value.len() > MAX {
            Err(WireError::LimitExceeded)
        } else {
            Ok(Self(value))
        }
    }
}

impl<'de, T: Deserialize<'de>, const MAX: usize> Deserialize<'de> for BoundedVec<T, MAX> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ListVisitor<T, const MAX: usize>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>, const MAX: usize> Visitor<'de> for ListVisitor<T, MAX> {
            type Value = BoundedVec<T, MAX>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a bounded array")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
                let mut result = BoundedVec::new();
                while result.len() < MAX {
                    let Some(value) = seq.next_element()? else {
                        return Ok(result);
                    };
                    result.try_push(value).map_err(serde::de::Error::custom)?;
                }
                if seq.next_element::<IgnoredAny>()?.is_some() {
                    return Err(serde::de::Error::custom(WireError::LimitExceeded));
                }
                Ok(result)
            }
        }
        deserializer.deserialize_seq(ListVisitor::<T, MAX>(PhantomData))
    }
}
