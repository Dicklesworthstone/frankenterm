use crate::object::Object;
use crate::value::Value;
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

/// The ToDynamic trait allows a type to emit a representation of itself
/// as the Value type.
/// This trait can be derived.
pub trait ToDynamic {
    fn to_dynamic(&self) -> Value;
}

/// The PlaceDynamic trait is used by derived implementations of FromDynamic
/// to implement flattened conversions.
/// Deriving FromDynamic for a struct will usually also derive
/// PlaceDynamic for the same struct.
/// You do not typically consume PlaceDynamic directly.
pub trait PlaceDynamic {
    /// Convert from Self to Value, by storing directly into the
    /// target Object.
    fn place_dynamic(&self, place: &mut Object);
}

impl ToDynamic for Value {
    fn to_dynamic(&self) -> Value {
        self.clone()
    }
}

impl ToDynamic for ordered_float::NotNan<f64> {
    fn to_dynamic(&self) -> Value {
        Value::F64(OrderedFloat::from(**self))
    }
}

#[cfg(feature = "std")]
impl ToDynamic for std::time::Duration {
    fn to_dynamic(&self) -> Value {
        Value::F64(OrderedFloat(self.as_secs_f64()))
    }
}

impl<K: ToDynamic + ToString + 'static, T: ToDynamic> ToDynamic for BTreeMap<K, T> {
    fn to_dynamic(&self) -> Value {
        Value::Object(
            self.iter()
                .map(|(k, v)| (k.to_dynamic(), v.to_dynamic()))
                .collect::<BTreeMap<_, _>>()
                .into(),
        )
    }
}

#[cfg(feature = "std")]
impl<K: ToDynamic + ToString + 'static, T: ToDynamic> ToDynamic for HashMap<K, T> {
    fn to_dynamic(&self) -> Value {
        Value::Object(
            self.iter()
                .map(|(k, v)| (k.to_dynamic(), v.to_dynamic()))
                .collect::<BTreeMap<_, _>>()
                .into(),
        )
    }
}

impl<T: ToDynamic> ToDynamic for Arc<T> {
    fn to_dynamic(&self) -> Value {
        self.as_ref().to_dynamic()
    }
}

impl<T: ToDynamic> ToDynamic for Box<T> {
    fn to_dynamic(&self) -> Value {
        self.as_ref().to_dynamic()
    }
}

impl<T: ToDynamic> ToDynamic for Option<T> {
    fn to_dynamic(&self) -> Value {
        match self {
            None => Value::Null,
            Some(t) => t.to_dynamic(),
        }
    }
}

impl<T: ToDynamic, const N: usize> ToDynamic for [T; N] {
    fn to_dynamic(&self) -> Value {
        Value::Array(
            self.iter()
                .map(T::to_dynamic)
                .collect::<Vec<Value>>()
                .into(),
        )
    }
}

impl<T: ToDynamic> ToDynamic for Vec<T> {
    fn to_dynamic(&self) -> Value {
        Value::Array(
            self.iter()
                .map(T::to_dynamic)
                .collect::<Vec<Value>>()
                .into(),
        )
    }
}

impl ToDynamic for () {
    fn to_dynamic(&self) -> Value {
        Value::Null
    }
}

impl ToDynamic for bool {
    fn to_dynamic(&self) -> Value {
        Value::Bool(*self)
    }
}

impl ToDynamic for str {
    fn to_dynamic(&self) -> Value {
        Value::String(self.to_string())
    }
}

#[cfg(feature = "std")]
impl ToDynamic for std::path::PathBuf {
    fn to_dynamic(&self) -> Value {
        Value::String(self.to_string_lossy().to_string())
    }
}

impl ToDynamic for String {
    fn to_dynamic(&self) -> Value {
        Value::String(self.to_string())
    }
}

impl ToDynamic for char {
    fn to_dynamic(&self) -> Value {
        Value::String(self.to_string())
    }
}

impl ToDynamic for isize {
    fn to_dynamic(&self) -> Value {
        Value::I64((*self).try_into().unwrap())
    }
}

impl ToDynamic for i8 {
    fn to_dynamic(&self) -> Value {
        Value::I64((*self).into())
    }
}

impl ToDynamic for i16 {
    fn to_dynamic(&self) -> Value {
        Value::I64((*self).into())
    }
}

impl ToDynamic for i32 {
    fn to_dynamic(&self) -> Value {
        Value::I64((*self).into())
    }
}

impl ToDynamic for i64 {
    fn to_dynamic(&self) -> Value {
        Value::I64(*self)
    }
}

impl ToDynamic for usize {
    fn to_dynamic(&self) -> Value {
        Value::U64((*self).try_into().unwrap())
    }
}

impl ToDynamic for u8 {
    fn to_dynamic(&self) -> Value {
        Value::U64((*self).into())
    }
}

impl ToDynamic for u16 {
    fn to_dynamic(&self) -> Value {
        Value::U64((*self).into())
    }
}

impl ToDynamic for u32 {
    fn to_dynamic(&self) -> Value {
        Value::U64((*self).into())
    }
}

impl ToDynamic for u64 {
    fn to_dynamic(&self) -> Value {
        Value::U64(*self)
    }
}

impl ToDynamic for f64 {
    fn to_dynamic(&self) -> Value {
        Value::F64(OrderedFloat(*self))
    }
}

impl ToDynamic for f32 {
    fn to_dynamic(&self) -> Value {
        Value::F64(OrderedFloat((*self).into()))
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::string::ToString;
    use alloc::sync::Arc;
    use alloc::vec;
    use ordered_float::OrderedFloat;

    // ── Primitive types ──────────────────────────────────────

    #[test]
    fn bool_to_dynamic() {
        assert_eq!(true.to_dynamic(), Value::Bool(true));
        assert_eq!(false.to_dynamic(), Value::Bool(false));
    }

    #[test]
    fn unit_to_dynamic() {
        assert_eq!(().to_dynamic(), Value::Null);
    }

    #[test]
    fn string_to_dynamic() {
        assert_eq!(
            "hello".to_string().to_dynamic(),
            Value::String("hello".to_string())
        );
    }

    #[test]
    fn str_to_dynamic() {
        assert_eq!("world".to_dynamic(), Value::String("world".to_string()));
    }

    #[test]
    fn char_to_dynamic() {
        assert_eq!('A'.to_dynamic(), Value::String("A".to_string()));
    }

    // ── Signed integers ──────────────────────────────────────

    #[test]
    fn i8_to_dynamic() {
        assert_eq!((-42i8).to_dynamic(), Value::I64(-42));
    }

    #[test]
    fn i16_to_dynamic() {
        assert_eq!(1000i16.to_dynamic(), Value::I64(1000));
    }

    #[test]
    fn i32_to_dynamic() {
        assert_eq!((-999_999i32).to_dynamic(), Value::I64(-999_999));
    }

    #[test]
    fn i64_to_dynamic() {
        assert_eq!(i64::MIN.to_dynamic(), Value::I64(i64::MIN));
        assert_eq!(i64::MAX.to_dynamic(), Value::I64(i64::MAX));
    }

    #[test]
    fn isize_to_dynamic() {
        assert_eq!(0isize.to_dynamic(), Value::I64(0));
    }

    // ── Unsigned integers ────────────────────────────────────

    #[test]
    fn u8_to_dynamic() {
        assert_eq!(255u8.to_dynamic(), Value::U64(255));
    }

    #[test]
    fn u16_to_dynamic() {
        assert_eq!(65535u16.to_dynamic(), Value::U64(65535));
    }

    #[test]
    fn u32_to_dynamic() {
        assert_eq!(42u32.to_dynamic(), Value::U64(42));
    }

    #[test]
    fn u64_to_dynamic() {
        assert_eq!(u64::MAX.to_dynamic(), Value::U64(u64::MAX));
    }

    #[test]
    fn usize_to_dynamic() {
        assert_eq!(0usize.to_dynamic(), Value::U64(0));
    }

    // ── Floating point ───────────────────────────────────────

    #[test]
    fn f64_to_dynamic() {
        assert_eq!(2.72f64.to_dynamic(), Value::F64(OrderedFloat(2.72)));
    }

    #[test]
    fn f32_to_dynamic() {
        let v = 1.5f32.to_dynamic();
        if let Value::F64(OrderedFloat(f)) = v {
            assert!((f - 1.5).abs() < 0.001);
        } else {
            panic!("expected F64");
        }
    }

    // ── Option<T> ────────────────────────────────────────────

    #[test]
    fn none_to_dynamic() {
        let v: Option<u64> = None;
        assert_eq!(v.to_dynamic(), Value::Null);
    }

    #[test]
    fn some_to_dynamic() {
        let v: Option<u64> = Some(42);
        assert_eq!(v.to_dynamic(), Value::U64(42));
    }

    // ── Vec<T> ───────────────────────────────────────────────

    #[test]
    fn vec_to_dynamic() {
        let v = vec![1u64, 2, 3];
        let result = v.to_dynamic();
        if let Value::Array(arr) = result {
            assert_eq!(arr.len(), 3);
            assert_eq!(arr[0], Value::U64(1));
            assert_eq!(arr[2], Value::U64(3));
        } else {
            panic!("expected Array");
        }
    }

    #[test]
    fn empty_vec_to_dynamic() {
        let v: Vec<u64> = vec![];
        if let Value::Array(arr) = v.to_dynamic() {
            assert!(arr.is_empty());
        } else {
            panic!("expected Array");
        }
    }

    // ── Fixed-size array ─────────────────────────────────────

    #[test]
    fn fixed_array_to_dynamic() {
        let arr = [10u64, 20, 30];
        let result = arr.to_dynamic();
        if let Value::Array(a) = result {
            assert_eq!(a.len(), 3);
            assert_eq!(a[1], Value::U64(20));
        } else {
            panic!("expected Array");
        }
    }

    // ── BTreeMap ─────────────────────────────────────────────

    #[test]
    fn btreemap_to_dynamic() {
        let mut map = BTreeMap::new();
        map.insert("a".to_string(), 1u64);
        map.insert("b".to_string(), 2u64);
        let result = map.to_dynamic();
        if let Value::Object(obj) = result {
            assert_eq!(obj.len(), 2);
            assert_eq!(obj.get_by_str("a"), Some(&Value::U64(1)));
            assert_eq!(obj.get_by_str("b"), Some(&Value::U64(2)));
        } else {
            panic!("expected Object");
        }
    }

    #[test]
    fn empty_btreemap_to_dynamic() {
        let map: BTreeMap<String, u64> = BTreeMap::new();
        if let Value::Object(obj) = map.to_dynamic() {
            assert!(obj.is_empty());
        } else {
            panic!("expected Object");
        }
    }

    // ── Box<T> / Arc<T> ──────────────────────────────────────

    #[test]
    fn box_to_dynamic() {
        let b: Box<u64> = Box::new(99);
        assert_eq!(b.to_dynamic(), Value::U64(99));
    }

    #[test]
    fn arc_to_dynamic() {
        let a: Arc<String> = Arc::new("test".to_string());
        assert_eq!(a.to_dynamic(), Value::String("test".to_string()));
    }

    // ── Value identity ───────────────────────────────────────

    #[test]
    fn value_to_dynamic_is_clone() {
        let v = Value::Bool(true);
        assert_eq!(v.to_dynamic(), v);
    }

    // ── NotNan<f64> ──────────────────────────────────────────

    #[test]
    fn notnan_to_dynamic() {
        let nn = ordered_float::NotNan::new(2.5).unwrap();
        assert_eq!(nn.to_dynamic(), Value::F64(OrderedFloat(2.5)));
    }

    // ── Roundtrip: ToDynamic then FromDynamic ────────────────

    #[test]
    fn roundtrip_u64() {
        use crate::FromDynamic;
        let original = 42u64;
        let dynamic = original.to_dynamic();
        let recovered = u64::from_dynamic(&dynamic, Default::default()).unwrap();
        assert_eq!(original, recovered);
    }

    #[test]
    fn roundtrip_string() {
        use crate::FromDynamic;
        let original = "hello world".to_string();
        let dynamic = original.to_dynamic();
        let recovered = String::from_dynamic(&dynamic, Default::default()).unwrap();
        assert_eq!(original, recovered);
    }

    #[test]
    fn roundtrip_bool() {
        use crate::FromDynamic;
        for val in [true, false] {
            let dynamic = val.to_dynamic();
            let recovered = bool::from_dynamic(&dynamic, Default::default()).unwrap();
            assert_eq!(val, recovered);
        }
    }

    #[test]
    fn roundtrip_vec() {
        use crate::FromDynamic;
        let original = vec![1u64, 2, 3, 4, 5];
        let dynamic = original.to_dynamic();
        let recovered = Vec::<u64>::from_dynamic(&dynamic, Default::default()).unwrap();
        assert_eq!(original, recovered);
    }

    #[test]
    fn roundtrip_option_some() {
        use crate::FromDynamic;
        let original: Option<i64> = Some(-42);
        let dynamic = original.to_dynamic();
        let recovered = Option::<i64>::from_dynamic(&dynamic, Default::default()).unwrap();
        assert_eq!(original, recovered);
    }

    #[test]
    fn roundtrip_option_none() {
        use crate::FromDynamic;
        let original: Option<i64> = None;
        let dynamic = original.to_dynamic();
        let recovered = Option::<i64>::from_dynamic(&dynamic, Default::default()).unwrap();
        assert_eq!(original, recovered);
    }
}
