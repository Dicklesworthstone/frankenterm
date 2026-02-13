use crate::error::Error;
use crate::value::Value;
use core::convert::TryInto;
#[cfg(feature = "std")]
use core::hash::Hash;
use ordered_float::OrderedFloat;
#[cfg(feature = "std")]
use std::collections::HashMap;

#[cfg(not(feature = "std"))]
extern crate alloc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

/// Specify how FromDynamic will treat unknown fields
/// when converting from Value to a given target type
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum UnknownFieldAction {
    /// Don't check, don't warn, don't raise an error
    Ignore,
    /// Emit a log::warn log
    #[default]
    Warn,
    /// Return an Error
    Deny,
}

/// Specify various options for FromDynamic::from_dynamic
#[derive(Copy, Clone, Debug, Default)]
pub struct FromDynamicOptions {
    pub unknown_fields: UnknownFieldAction,
    pub deprecated_fields: UnknownFieldAction,
}

impl FromDynamicOptions {
    pub fn flatten(self) -> Self {
        Self {
            unknown_fields: UnknownFieldAction::Ignore,
            ..self
        }
    }
}

/// The FromDynamic trait allows a type to construct itself from a Value.
/// This trait can be derived.
pub trait FromDynamic {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error>
    where
        Self: Sized;
}

impl FromDynamic for Value {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        Ok(value.clone())
    }
}

impl FromDynamic for ordered_float::NotNan<f64> {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        let f = f64::from_dynamic(value, options)?;
        ordered_float::NotNan::new(f).map_err(|e| Error::Message(e.to_string()))
    }
}

#[cfg(feature = "std")]
impl FromDynamic for std::time::Duration {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        let f = f64::from_dynamic(value, options)?;
        Ok(std::time::Duration::from_secs_f64(f))
    }
}

impl<T: FromDynamic> FromDynamic for Box<T> {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        let value = T::from_dynamic(value, options)?;
        Ok(Box::new(value))
    }
}

impl<T: FromDynamic> FromDynamic for Arc<T> {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        let value = T::from_dynamic(value, options)?;
        Ok(Arc::new(value))
    }
}

impl<T: FromDynamic> FromDynamic for Option<T> {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::Null => Ok(None),
            value => Ok(Some(T::from_dynamic(value, options)?)),
        }
    }
}

impl<T: FromDynamic, const N: usize> FromDynamic for [T; N] {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::Array(arr) => {
                let v = arr
                    .iter()
                    .map(|v| T::from_dynamic(v, options))
                    .collect::<Result<Vec<T>, Error>>()?;
                v.try_into().map_err(|v: Vec<T>| Error::ArraySizeMismatch {
                    vec_size: v.len(),
                    array_size: N,
                })
            }
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "array",
            }),
        }
    }
}

impl<K: FromDynamic + Eq + Ord, T: FromDynamic> FromDynamic for BTreeMap<K, T> {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::Object(obj) => {
                let mut map = BTreeMap::new();
                for (k, v) in obj.iter() {
                    map.insert(K::from_dynamic(k, options)?, T::from_dynamic(v, options)?);
                }
                Ok(map)
            }
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "BTreeMap",
            }),
        }
    }
}

#[cfg(feature = "std")]
impl<K: FromDynamic + Eq + Hash, T: FromDynamic> FromDynamic for HashMap<K, T> {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::Object(obj) => {
                let mut map = HashMap::with_capacity(obj.len());
                for (k, v) in obj.iter() {
                    map.insert(K::from_dynamic(k, options)?, T::from_dynamic(v, options)?);
                }
                Ok(map)
            }
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "HashMap",
            }),
        }
    }
}

impl<T: FromDynamic> FromDynamic for Vec<T> {
    fn from_dynamic(value: &Value, options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::Array(arr) => Ok(arr
                .iter()
                .map(|v| T::from_dynamic(v, options))
                .collect::<Result<Vec<T>, Error>>()?),
            // lua uses tables for everything; we can end up here if we got an empty
            // table and treated it as an object. Allow that to stand-in for an empty
            // array instead.
            Value::Object(obj) if obj.is_empty() => Ok(Vec::new()),
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "Vec",
            }),
        }
    }
}

impl FromDynamic for () {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::Null => Ok(()),
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "()",
            }),
        }
    }
}

impl FromDynamic for bool {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::Bool(b) => Ok(*b),
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "bool",
            }),
        }
    }
}

#[cfg(feature = "std")]
impl FromDynamic for std::path::PathBuf {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::String(s) => Ok(s.into()),
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "PathBuf",
            }),
        }
    }
}

impl FromDynamic for char {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::String(s) => {
                let mut iter = s.chars();
                let c = iter.next().ok_or(Error::CharFromWrongSizedString)?;
                if iter.next().is_some() {
                    Err(Error::CharFromWrongSizedString)
                } else {
                    Ok(c)
                }
            }
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "char",
            }),
        }
    }
}

impl FromDynamic for String {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::String(s) => Ok(s.to_string()),
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "String",
            }),
        }
    }
}

macro_rules! int {
    ($($ty:ty),* $(,)?) => {
        $(
impl FromDynamic for $ty {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::I64(n) => match (*n).try_into() {
                Ok(n) => Ok(n),
                Err(err) => Err(Error::Message(err.to_string())),
            },
            Value::U64(n) => match (*n).try_into() {
                Ok(n) => Ok(n),
                Err(err) => Err(Error::Message(err.to_string())),
            },
            other => Err(Error::NoConversion{
                source_type:other.variant_name().to_string(),
                dest_type: stringify!($ty),
            })
        }
    }
}
        )*
    }
}

int!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

impl FromDynamic for f32 {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::F64(OrderedFloat(n)) => Ok((*n) as f32),
            Value::I64(n) => Ok((*n) as f32),
            Value::U64(n) => Ok((*n) as f32),
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "f32",
            }),
        }
    }
}

impl FromDynamic for f64 {
    fn from_dynamic(value: &Value, _options: FromDynamicOptions) -> Result<Self, Error> {
        match value {
            Value::F64(OrderedFloat(n)) => Ok(*n),
            Value::I64(n) => Ok((*n) as f64),
            Value::U64(n) => Ok((*n) as f64),
            other => Err(Error::NoConversion {
                source_type: other.variant_name().to_string(),
                dest_type: "f64",
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use crate::array::Array;
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::vec;
    use ordered_float::OrderedFloat;

    fn opts() -> FromDynamicOptions {
        FromDynamicOptions::default()
    }

    // ── UnknownFieldAction ───────────────────────────────────

    #[test]
    fn unknown_field_action_default_is_warn() {
        assert_eq!(UnknownFieldAction::default(), UnknownFieldAction::Warn);
    }

    #[test]
    fn unknown_field_action_clone_and_eq() {
        let a = UnknownFieldAction::Deny;
        let b = a;
        assert_eq!(a, b);
    }

    // ── FromDynamicOptions ───────────────────────────────────

    #[test]
    fn options_default() {
        let o = FromDynamicOptions::default();
        assert_eq!(o.unknown_fields, UnknownFieldAction::Warn);
        assert_eq!(o.deprecated_fields, UnknownFieldAction::Warn);
    }

    #[test]
    fn options_flatten_sets_unknown_to_ignore() {
        let o = FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Deny,
            deprecated_fields: UnknownFieldAction::Deny,
        };
        let flat = o.flatten();
        assert_eq!(flat.unknown_fields, UnknownFieldAction::Ignore);
        assert_eq!(flat.deprecated_fields, UnknownFieldAction::Deny);
    }

    // ── Value -> Value (identity) ────────────────────────────

    #[test]
    fn value_from_dynamic_is_clone() {
        let v = Value::String("hello".to_string());
        let result = Value::from_dynamic(&v, opts()).unwrap();
        assert_eq!(result, v);
    }

    // ── bool ─────────────────────────────────────────────────

    #[test]
    fn bool_from_bool_value() {
        assert!(bool::from_dynamic(&Value::Bool(true), opts()).unwrap());
        assert!(!bool::from_dynamic(&Value::Bool(false), opts()).unwrap());
    }

    #[test]
    fn bool_from_non_bool_fails() {
        assert!(bool::from_dynamic(&Value::U64(1), opts()).is_err());
        assert!(bool::from_dynamic(&Value::Null, opts()).is_err());
        assert!(bool::from_dynamic(&Value::String("true".to_string()), opts()).is_err());
    }

    // ── String ───────────────────────────────────────────────

    #[test]
    fn string_from_string_value() {
        let result = String::from_dynamic(&Value::String("abc".to_string()), opts()).unwrap();
        assert_eq!(result, "abc");
    }

    #[test]
    fn string_from_non_string_fails() {
        assert!(String::from_dynamic(&Value::U64(42), opts()).is_err());
        assert!(String::from_dynamic(&Value::Bool(true), opts()).is_err());
    }

    // ── char ─────────────────────────────────────────────────

    #[test]
    fn char_from_single_char_string() {
        assert_eq!(
            char::from_dynamic(&Value::String("a".to_string()), opts()).unwrap(),
            'a'
        );
    }

    #[test]
    fn char_from_empty_string_fails() {
        let result = char::from_dynamic(&Value::String(String::new()), opts());
        assert!(result.is_err());
    }

    #[test]
    fn char_from_multi_char_string_fails() {
        let result = char::from_dynamic(&Value::String("ab".to_string()), opts());
        assert!(result.is_err());
    }

    #[test]
    fn char_from_non_string_fails() {
        assert!(char::from_dynamic(&Value::U64(65), opts()).is_err());
    }

    // ── unit ─────────────────────────────────────────────────

    #[test]
    fn unit_from_null() {
        assert_eq!(<()>::from_dynamic(&Value::Null, opts()).unwrap(), ());
    }

    #[test]
    fn unit_from_non_null_fails() {
        assert!(<()>::from_dynamic(&Value::Bool(false), opts()).is_err());
    }

    // ── Integer types ────────────────────────────────────────

    #[test]
    fn u8_from_u64_value() {
        assert_eq!(u8::from_dynamic(&Value::U64(255), opts()).unwrap(), 255u8);
    }

    #[test]
    fn u8_from_i64_value() {
        assert_eq!(u8::from_dynamic(&Value::I64(100), opts()).unwrap(), 100u8);
    }

    #[test]
    fn u8_overflow_fails() {
        assert!(u8::from_dynamic(&Value::U64(256), opts()).is_err());
    }

    #[test]
    fn i8_from_i64_value() {
        assert_eq!(i8::from_dynamic(&Value::I64(-128), opts()).unwrap(), -128i8);
    }

    #[test]
    fn i8_underflow_fails() {
        assert!(i8::from_dynamic(&Value::I64(-129), opts()).is_err());
    }

    #[test]
    fn u64_from_u64() {
        assert_eq!(
            u64::from_dynamic(&Value::U64(u64::MAX), opts()).unwrap(),
            u64::MAX
        );
    }

    #[test]
    fn i64_from_i64() {
        assert_eq!(
            i64::from_dynamic(&Value::I64(i64::MIN), opts()).unwrap(),
            i64::MIN
        );
    }

    #[test]
    fn integer_from_non_numeric_fails() {
        assert!(u32::from_dynamic(&Value::String("42".to_string()), opts()).is_err());
        assert!(i32::from_dynamic(&Value::Bool(true), opts()).is_err());
        assert!(usize::from_dynamic(&Value::Null, opts()).is_err());
    }

    // ── f32 / f64 ────────────────────────────────────────────

    #[test]
    fn f64_from_f64_value() {
        let v = Value::F64(OrderedFloat(2.72));
        let r = f64::from_dynamic(&v, opts()).unwrap();
        assert!((r - 2.72).abs() < f64::EPSILON);
    }

    #[test]
    fn f64_from_i64_value() {
        let r = f64::from_dynamic(&Value::I64(-10), opts()).unwrap();
        assert!((r - (-10.0)).abs() < f64::EPSILON);
    }

    #[test]
    fn f64_from_u64_value() {
        let r = f64::from_dynamic(&Value::U64(100), opts()).unwrap();
        assert!((r - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn f32_from_f64_value() {
        let v = Value::F64(OrderedFloat(1.5));
        let r = f32::from_dynamic(&v, opts()).unwrap();
        assert!((r - 1.5f32).abs() < f32::EPSILON);
    }

    #[test]
    fn f64_from_non_numeric_fails() {
        assert!(f64::from_dynamic(&Value::String("3.14".to_string()), opts()).is_err());
        assert!(f64::from_dynamic(&Value::Null, opts()).is_err());
    }

    // ── Option<T> ────────────────────────────────────────────

    #[test]
    fn option_from_null_is_none() {
        let r = Option::<u64>::from_dynamic(&Value::Null, opts()).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn option_from_value_is_some() {
        let r = Option::<u64>::from_dynamic(&Value::U64(42), opts()).unwrap();
        assert_eq!(r, Some(42));
    }

    #[test]
    fn option_from_wrong_type_propagates_error() {
        let r = Option::<u64>::from_dynamic(&Value::String("nope".to_string()), opts());
        assert!(r.is_err());
    }

    // ── Vec<T> ───────────────────────────────────────────────

    #[test]
    fn vec_from_array() {
        let arr: Array = vec![Value::U64(1), Value::U64(2), Value::U64(3)].into();
        let r = Vec::<u64>::from_dynamic(&Value::Array(arr), opts()).unwrap();
        assert_eq!(r, vec![1u64, 2, 3]);
    }

    #[test]
    fn vec_from_empty_object_is_empty_vec() {
        let obj: crate::Object = BTreeMap::new().into();
        let r = Vec::<u64>::from_dynamic(&Value::Object(obj), opts()).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn vec_from_non_empty_object_fails() {
        let mut map = BTreeMap::new();
        map.insert(Value::String("a".to_string()), Value::U64(1));
        let obj: crate::Object = map.into();
        assert!(Vec::<u64>::from_dynamic(&Value::Object(obj), opts()).is_err());
    }

    #[test]
    fn vec_from_non_array_fails() {
        assert!(Vec::<u64>::from_dynamic(&Value::U64(1), opts()).is_err());
    }

    // ── Fixed-size array ─────────────────────────────────────

    #[test]
    fn fixed_array_from_matching_size() {
        let arr: Array = vec![Value::U64(10), Value::U64(20)].into();
        let r = <[u64; 2]>::from_dynamic(&Value::Array(arr), opts()).unwrap();
        assert_eq!(r, [10u64, 20]);
    }

    #[test]
    fn fixed_array_size_mismatch_fails() {
        let arr: Array = vec![Value::U64(1)].into();
        let r = <[u64; 3]>::from_dynamic(&Value::Array(arr), opts());
        assert!(r.is_err());
    }

    #[test]
    fn fixed_array_from_non_array_fails() {
        let r = <[u64; 1]>::from_dynamic(&Value::U64(1), opts());
        assert!(r.is_err());
    }

    // ── BTreeMap ─────────────────────────────────────────────

    #[test]
    fn btreemap_from_object() {
        let mut map = BTreeMap::new();
        map.insert(Value::String("a".to_string()), Value::U64(1));
        map.insert(Value::String("b".to_string()), Value::U64(2));
        let obj: crate::Object = map.into();
        let r = BTreeMap::<String, u64>::from_dynamic(&Value::Object(obj), opts()).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r["a"], 1);
        assert_eq!(r["b"], 2);
    }

    #[test]
    fn btreemap_from_non_object_fails() {
        assert!(BTreeMap::<String, u64>::from_dynamic(&Value::Null, opts()).is_err());
    }

    // ── Box<T> / Arc<T> ──────────────────────────────────────

    #[test]
    fn box_from_dynamic() {
        let r = Box::<u64>::from_dynamic(&Value::U64(42), opts()).unwrap();
        assert_eq!(*r, 42);
    }

    #[test]
    fn arc_from_dynamic() {
        let r =
            alloc::sync::Arc::<String>::from_dynamic(&Value::String("hello".to_string()), opts())
                .unwrap();
        assert_eq!(*r, "hello");
    }

    // ── NotNan<f64> ──────────────────────────────────────────

    #[test]
    fn notnan_from_valid_f64() {
        let v = Value::F64(OrderedFloat(2.5));
        let r = ordered_float::NotNan::<f64>::from_dynamic(&v, opts()).unwrap();
        assert!((r.into_inner() - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn notnan_from_nan_fails() {
        let v = Value::F64(OrderedFloat(f64::NAN));
        let r = ordered_float::NotNan::<f64>::from_dynamic(&v, opts());
        assert!(r.is_err());
    }
}
