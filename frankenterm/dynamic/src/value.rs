use crate::array::Array;
use crate::object::Object;
#[allow(unused)]
#[cfg(not(feature = "std"))]
use ordered_float::FloatCore;
use ordered_float::OrderedFloat;

#[cfg(not(feature = "std"))]
extern crate alloc;

use alloc::string::String;

/// Represents values of various possible other types.
/// Value is intended to be convertible to the same set
/// of types as Lua and is a superset of the types possible
/// in TOML and JSON.
#[derive(Clone, PartialEq, Hash, Eq, Ord, PartialOrd)]
pub enum Value {
    Null,
    Bool(bool),
    String(String),
    Array(Array),
    Object(Object),
    U64(u64),
    I64(i64),
    F64(OrderedFloat<f64>),
}

impl Default for Value {
    fn default() -> Self {
        Self::Null
    }
}

impl core::fmt::Debug for Value {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::String(s) => fmt.write_fmt(format_args!("{:?}", s)),
            Self::Null => fmt.write_str("nil"),
            Self::Bool(i) => i.fmt(fmt),
            Self::I64(i) => i.fmt(fmt),
            Self::U64(i) => i.fmt(fmt),
            Self::F64(i) => i.fmt(fmt),
            Self::Array(a) => a.fmt(fmt),
            Self::Object(o) => o.fmt(fmt),
        }
    }
}

impl Value {
    pub fn variant_name(&self) -> &str {
        match self {
            Self::Null => "Null",
            Self::Bool(_) => "Bool",
            Self::String(_) => "String",
            Self::Array(_) => "Array",
            Self::Object(_) => "Object",
            Self::U64(_) => "U64",
            Self::I64(_) => "I64",
            Self::F64(_) => "F64",
        }
    }

    pub fn coerce_unsigned(&self) -> Option<u64> {
        match self {
            Self::U64(u) => Some(*u),
            Self::I64(i) => (*i).try_into().ok(),
            Self::F64(OrderedFloat(f))
                if f.fract() == 0.0 && *f >= u64::MIN as f64 && *f <= u64::MAX as f64 =>
            {
                Some(*f as u64)
            }
            _ => None,
        }
    }

    pub fn coerce_signed(&self) -> Option<i64> {
        match self {
            Self::I64(u) => Some(*u),
            Self::U64(i) => (*i).try_into().ok(),
            Self::F64(OrderedFloat(f))
                if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 =>
            {
                Some(*f as i64)
            }
            _ => None,
        }
    }

    pub fn coerce_float(&self) -> Option<f64> {
        match self {
            Self::I64(u) => Some(*u as f64),
            Self::U64(i) => Some(*i as f64),
            Self::F64(OrderedFloat(f)) => Some(*f),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    extern crate std;

    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec;

    // ── Default & variant_name ───────────────────────────────

    #[test]
    fn default_is_null() {
        assert_eq!(Value::default(), Value::Null);
    }

    #[test]
    fn variant_name_all_variants() {
        assert_eq!(Value::Null.variant_name(), "Null");
        assert_eq!(Value::Bool(true).variant_name(), "Bool");
        assert_eq!(Value::String("x".to_string()).variant_name(), "String");
        assert_eq!(Value::U64(0).variant_name(), "U64");
        assert_eq!(Value::I64(0).variant_name(), "I64");
        assert_eq!(Value::F64(OrderedFloat(0.0)).variant_name(), "F64");
        assert_eq!(Value::Array(Array::new()).variant_name(), "Array");
        let obj: Object = BTreeMap::new().into();
        assert_eq!(Value::Object(obj).variant_name(), "Object");
    }

    // ── Equality & Clone ─────────────────────────────────────

    #[test]
    fn value_eq_same_type() {
        assert_eq!(Value::Bool(true), Value::Bool(true));
        assert_ne!(Value::Bool(true), Value::Bool(false));
        assert_eq!(Value::U64(42), Value::U64(42));
        assert_ne!(Value::U64(1), Value::U64(2));
        assert_eq!(
            Value::String("abc".to_string()),
            Value::String("abc".to_string())
        );
    }

    #[test]
    fn value_ne_different_types() {
        assert_ne!(Value::U64(0), Value::I64(0));
        assert_ne!(Value::Null, Value::Bool(false));
        assert_ne!(Value::String("0".to_string()), Value::U64(0));
    }

    #[test]
    fn value_clone_is_equal() {
        let values = vec![
            Value::Null,
            Value::Bool(true),
            Value::String("test".to_string()),
            Value::U64(999),
            Value::I64(-42),
            Value::F64(OrderedFloat(3.14)),
        ];
        for v in values {
            assert_eq!(v.clone(), v);
        }
    }

    // ── coerce_unsigned ──────────────────────────────────────

    #[test]
    fn coerce_unsigned_from_u64() {
        assert_eq!(Value::U64(42).coerce_unsigned(), Some(42));
        assert_eq!(Value::U64(0).coerce_unsigned(), Some(0));
        assert_eq!(Value::U64(u64::MAX).coerce_unsigned(), Some(u64::MAX));
    }

    #[test]
    fn coerce_unsigned_from_positive_i64() {
        assert_eq!(Value::I64(100).coerce_unsigned(), Some(100));
        assert_eq!(Value::I64(0).coerce_unsigned(), Some(0));
    }

    #[test]
    fn coerce_unsigned_from_negative_i64_is_none() {
        assert_eq!(Value::I64(-1).coerce_unsigned(), None);
        assert_eq!(Value::I64(i64::MIN).coerce_unsigned(), None);
    }

    #[test]
    fn coerce_unsigned_from_whole_f64() {
        assert_eq!(Value::F64(OrderedFloat(42.0)).coerce_unsigned(), Some(42));
        assert_eq!(Value::F64(OrderedFloat(0.0)).coerce_unsigned(), Some(0));
    }

    #[test]
    fn coerce_unsigned_from_fractional_f64_is_none() {
        assert_eq!(Value::F64(OrderedFloat(3.14)).coerce_unsigned(), None);
        assert_eq!(Value::F64(OrderedFloat(0.5)).coerce_unsigned(), None);
    }

    #[test]
    fn coerce_unsigned_from_negative_f64_is_none() {
        assert_eq!(Value::F64(OrderedFloat(-1.0)).coerce_unsigned(), None);
    }

    #[test]
    fn coerce_unsigned_from_non_numeric_is_none() {
        assert_eq!(Value::Null.coerce_unsigned(), None);
        assert_eq!(Value::Bool(true).coerce_unsigned(), None);
        assert_eq!(Value::String("42".to_string()).coerce_unsigned(), None);
    }

    // ── coerce_signed ────────────────────────────────────────

    #[test]
    fn coerce_signed_from_i64() {
        assert_eq!(Value::I64(-42).coerce_signed(), Some(-42));
        assert_eq!(Value::I64(0).coerce_signed(), Some(0));
        assert_eq!(Value::I64(i64::MAX).coerce_signed(), Some(i64::MAX));
        assert_eq!(Value::I64(i64::MIN).coerce_signed(), Some(i64::MIN));
    }

    #[test]
    fn coerce_signed_from_small_u64() {
        assert_eq!(Value::U64(42).coerce_signed(), Some(42));
        assert_eq!(Value::U64(0).coerce_signed(), Some(0));
    }

    #[test]
    fn coerce_signed_from_large_u64_is_none() {
        assert_eq!(Value::U64(u64::MAX).coerce_signed(), None);
        assert_eq!(Value::U64(i64::MAX as u64 + 1).coerce_signed(), None);
    }

    #[test]
    fn coerce_signed_from_whole_f64() {
        assert_eq!(Value::F64(OrderedFloat(-42.0)).coerce_signed(), Some(-42));
    }

    #[test]
    fn coerce_signed_from_fractional_f64_is_none() {
        assert_eq!(Value::F64(OrderedFloat(1.5)).coerce_signed(), None);
    }

    #[test]
    fn coerce_signed_from_non_numeric_is_none() {
        assert_eq!(Value::Null.coerce_signed(), None);
        assert_eq!(Value::Bool(false).coerce_signed(), None);
    }

    // ── coerce_float ─────────────────────────────────────────

    #[test]
    fn coerce_float_from_f64() {
        assert_eq!(Value::F64(OrderedFloat(3.14)).coerce_float(), Some(3.14));
    }

    #[test]
    fn coerce_float_from_i64() {
        assert_eq!(Value::I64(-10).coerce_float(), Some(-10.0));
    }

    #[test]
    fn coerce_float_from_u64() {
        assert_eq!(Value::U64(100).coerce_float(), Some(100.0));
    }

    #[test]
    fn coerce_float_from_non_numeric_is_none() {
        assert_eq!(Value::Null.coerce_float(), None);
        assert_eq!(Value::Bool(true).coerce_float(), None);
        assert_eq!(Value::String("3.14".to_string()).coerce_float(), None);
    }

    // ── Debug formatting ─────────────────────────────────────

    #[test]
    fn debug_null_is_nil() {
        assert_eq!(format!("{:?}", Value::Null), "nil");
    }

    #[test]
    fn debug_string_is_quoted() {
        let debug = format!("{:?}", Value::String("hello".to_string()));
        assert!(debug.contains("hello"));
        assert!(debug.starts_with('"'));
    }

    #[test]
    fn debug_bool_shows_value() {
        assert_eq!(format!("{:?}", Value::Bool(true)), "true");
        assert_eq!(format!("{:?}", Value::Bool(false)), "false");
    }

    #[test]
    fn debug_numbers_show_values() {
        assert_eq!(format!("{:?}", Value::U64(42)), "42");
        assert_eq!(format!("{:?}", Value::I64(-7)), "-7");
    }

    // ── Hash consistency ─────────────────────────────────────

    #[test]
    fn hash_consistent_for_equal_values() {
        use core::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;

        fn hash_of(v: &Value) -> u64 {
            let mut h = DefaultHasher::new();
            v.hash(&mut h);
            h.finish()
        }

        let a = Value::String("test".to_string());
        let b = Value::String("test".to_string());
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    // ── Array via Value ──────────────────────────────────────

    #[test]
    fn array_from_vec_and_iterate() {
        let arr: Array = vec![Value::U64(1), Value::U64(2), Value::U64(3)].into();
        let val = Value::Array(arr);
        assert_eq!(val.variant_name(), "Array");
        if let Value::Array(a) = val {
            assert_eq!(a.len(), 3);
            assert_eq!(a[0], Value::U64(1));
        }
    }

    // ── Object via Value ─────────────────────────────────────

    #[test]
    fn object_get_by_str() {
        let mut map = BTreeMap::new();
        map.insert(
            Value::String("key".to_string()),
            Value::String("value".to_string()),
        );
        let obj: Object = map.into();
        assert_eq!(
            obj.get_by_str("key"),
            Some(&Value::String("value".to_string()))
        );
        assert_eq!(obj.get_by_str("missing"), None);
    }

    #[test]
    fn object_from_iter() {
        let obj: Object = vec![
            (Value::String("a".to_string()), Value::U64(1)),
            (Value::String("b".to_string()), Value::U64(2)),
        ]
        .into_iter()
        .collect();
        assert_eq!(obj.len(), 2);
        assert_eq!(obj.get_by_str("a"), Some(&Value::U64(1)));
    }

    // ── F64 special values ───────────────────────────────────

    #[test]
    fn f64_nan_coerce_unsigned_is_none() {
        assert_eq!(Value::F64(OrderedFloat(f64::NAN)).coerce_unsigned(), None);
    }

    #[test]
    fn f64_infinity_coerce_signed_is_none() {
        assert_eq!(
            Value::F64(OrderedFloat(f64::INFINITY)).coerce_signed(),
            None
        );
        assert_eq!(
            Value::F64(OrderedFloat(f64::NEG_INFINITY)).coerce_signed(),
            None
        );
    }

    #[test]
    fn f64_nan_coerce_float_returns_nan() {
        let result = Value::F64(OrderedFloat(f64::NAN)).coerce_float();
        assert!(result.unwrap().is_nan());
    }
}
