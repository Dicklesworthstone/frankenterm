use crate::Value;
use core::cmp::Ordering;
use core::ops::{Deref, DerefMut};

#[cfg(not(feature = "std"))]
extern crate alloc;

use alloc::borrow::Borrow;
use alloc::collections::BTreeMap;

/// We'd like to avoid allocating when resolving struct fields,
/// so this is the borrowed version of Value.
/// It's a bit involved to make this work; more details can be
/// found in the excellent guide here:
/// <https://github.com/sunshowers/borrow-complex-key-example/blob/master/src/lib.rs>
#[derive(Copy, Clone, Debug, PartialEq, Hash, Eq, Ord, PartialOrd)]
pub enum BorrowedKey<'a> {
    Value(&'a Value),
    Str(&'a str),
}

pub trait ObjectKeyTrait {
    fn key<'k>(&'k self) -> BorrowedKey<'k>;
}

impl ObjectKeyTrait for Value {
    fn key<'k>(&'k self) -> BorrowedKey<'k> {
        match self {
            Value::String(s) => BorrowedKey::Str(s.as_str()),
            v => BorrowedKey::Value(v),
        }
    }
}

impl<'a> ObjectKeyTrait for BorrowedKey<'a> {
    fn key<'k>(&'k self) -> BorrowedKey<'k> {
        *self
    }
}

impl<'a> Borrow<dyn ObjectKeyTrait + 'a> for Value {
    fn borrow(&self) -> &(dyn ObjectKeyTrait + 'a) {
        self
    }
}

impl<'a> PartialEq for dyn ObjectKeyTrait + 'a {
    fn eq(&self, other: &Self) -> bool {
        self.key().eq(&other.key())
    }
}

impl<'a> Eq for dyn ObjectKeyTrait + 'a {}

impl<'a> PartialOrd for dyn ObjectKeyTrait + 'a {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for dyn ObjectKeyTrait + 'a {
    fn cmp(&self, other: &Self) -> Ordering {
        self.key().cmp(&other.key())
    }
}

impl<'a> core::hash::Hash for dyn ObjectKeyTrait + 'a {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.key().hash(state)
    }
}

#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct Object {
    inner: BTreeMap<Value, Value>,
}

impl Object {
    pub fn get_by_str(&self, field_name: &str) -> Option<&Value> {
        self.inner
            .get(&BorrowedKey::Str(field_name) as &dyn ObjectKeyTrait)
    }
}

impl core::fmt::Debug for Object {
    fn fmt(&self, fmt: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.inner.fmt(fmt)
    }
}

impl Ord for Object {
    fn cmp(&self, other: &Self) -> Ordering {
        let self_ptr = self as *const Self;
        let other_ptr = other as *const Self;
        self_ptr.cmp(&other_ptr)
    }
}

impl PartialOrd for Object {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        for (_, child) in core::mem::take(&mut self.inner) {
            crate::drop::safely(child);
        }
    }
}

impl From<BTreeMap<Value, Value>> for Object {
    fn from(inner: BTreeMap<Value, Value>) -> Self {
        Self { inner }
    }
}

impl Deref for Object {
    type Target = BTreeMap<Value, Value>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Object {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

fn take(object: Object) -> BTreeMap<Value, Value> {
    let object = core::mem::ManuallyDrop::new(object);
    unsafe { core::ptr::read(&object.inner) }
}

impl IntoIterator for Object {
    type Item = (Value, Value);
    type IntoIter = <BTreeMap<Value, Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        take(self).into_iter()
    }
}

impl<'a> IntoIterator for &'a Object {
    type Item = (&'a Value, &'a Value);
    type IntoIter = <&'a BTreeMap<Value, Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut Object {
    type Item = (&'a Value, &'a mut Value);
    type IntoIter = <&'a mut BTreeMap<Value, Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl FromIterator<(Value, Value)> for Object {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (Value, Value)>,
    {
        Object {
            inner: BTreeMap::from_iter(iter),
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
    use alloc::vec::Vec;

    // ── Construction ─────────────────────────────────────────

    #[test]
    fn from_btreemap() {
        let mut map = BTreeMap::new();
        map.insert(Value::String("a".to_string()), Value::U64(1));
        let obj: Object = map.into();
        assert_eq!(obj.len(), 1);
    }

    #[test]
    fn from_iter_pairs() {
        let obj: Object = vec![
            (Value::String("x".to_string()), Value::Bool(true)),
            (Value::String("y".to_string()), Value::Bool(false)),
        ]
        .into_iter()
        .collect();
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn default_is_empty() {
        let obj = Object::default();
        assert!(obj.is_empty());
        assert_eq!(obj.len(), 0);
    }

    // ── get_by_str ───────────────────────────────────────────

    #[test]
    fn get_by_str_finds_string_key() {
        let obj: Object = vec![(
            Value::String("name".to_string()),
            Value::String("Alice".to_string()),
        )]
        .into_iter()
        .collect();
        assert_eq!(
            obj.get_by_str("name"),
            Some(&Value::String("Alice".to_string()))
        );
    }

    #[test]
    fn get_by_str_missing_key_returns_none() {
        let obj: Object = vec![(Value::String("a".to_string()), Value::U64(1))]
            .into_iter()
            .collect();
        assert_eq!(obj.get_by_str("b"), None);
    }

    #[test]
    fn get_by_str_empty_object() {
        let obj = Object::default();
        assert_eq!(obj.get_by_str("anything"), None);
    }

    #[test]
    fn get_by_str_non_string_key_not_found() {
        let obj: Object = vec![(Value::U64(42), Value::Bool(true))]
            .into_iter()
            .collect();
        assert_eq!(obj.get_by_str("42"), None);
    }

    // ── Clone & Equality ─────────────────────────────────────

    #[test]
    fn clone_produces_equal_object() {
        let obj: Object = vec![(Value::String("k".to_string()), Value::I64(-1))]
            .into_iter()
            .collect();
        let cloned = obj.clone();
        assert_eq!(obj, cloned);
    }

    #[test]
    fn equality_same_entries() {
        let a: Object = vec![(Value::String("k".to_string()), Value::U64(1))]
            .into_iter()
            .collect();
        let b: Object = vec![(Value::String("k".to_string()), Value::U64(1))]
            .into_iter()
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn inequality_different_values() {
        let a: Object = vec![(Value::String("k".to_string()), Value::U64(1))]
            .into_iter()
            .collect();
        let b: Object = vec![(Value::String("k".to_string()), Value::U64(2))]
            .into_iter()
            .collect();
        assert_ne!(a, b);
    }

    // ── Deref to BTreeMap ────────────────────────────────────

    #[test]
    fn deref_provides_btreemap_methods() {
        let obj: Object = vec![
            (Value::String("a".to_string()), Value::U64(1)),
            (Value::String("b".to_string()), Value::U64(2)),
        ]
        .into_iter()
        .collect();
        assert!(obj.contains_key(&Value::String("a".to_string())));
        assert!(!obj.contains_key(&Value::String("c".to_string())));
        assert_eq!(obj.keys().count(), 2);
    }

    #[test]
    fn deref_mut_allows_insert() {
        let mut obj = Object::default();
        obj.insert(Value::String("new".to_string()), Value::Null);
        assert_eq!(obj.len(), 1);
        assert_eq!(obj.get_by_str("new"), Some(&Value::Null));
    }

    #[test]
    fn deref_mut_allows_remove() {
        let mut obj: Object = vec![(Value::String("a".to_string()), Value::U64(1))]
            .into_iter()
            .collect();
        let removed = obj.remove(&Value::String("a".to_string()));
        assert_eq!(removed, Some(Value::U64(1)));
        assert!(obj.is_empty());
    }

    // ── IntoIterator ─────────────────────────────────────────

    #[test]
    fn into_iter_owned() {
        let obj: Object = vec![
            (Value::String("a".to_string()), Value::U64(1)),
            (Value::String("b".to_string()), Value::U64(2)),
        ]
        .into_iter()
        .collect();
        let pairs: Vec<(Value, Value)> = obj.into_iter().collect();
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn into_iter_ref() {
        let obj: Object = vec![(Value::String("k".to_string()), Value::Bool(true))]
            .into_iter()
            .collect();
        let refs: Vec<(&Value, &Value)> = (&obj).into_iter().collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].1, &Value::Bool(true));
    }

    #[test]
    fn into_iter_mut_ref() {
        let mut obj: Object = vec![(Value::String("k".to_string()), Value::U64(0))]
            .into_iter()
            .collect();
        for (_, v) in &mut obj {
            *v = Value::U64(99);
        }
        assert_eq!(obj.get_by_str("k"), Some(&Value::U64(99)));
    }

    // ── Ord / PartialOrd ─────────────────────────────────────

    #[test]
    fn partial_ord_returns_some() {
        let a = Object::default();
        let b = Object::default();
        assert!(a.partial_cmp(&b).is_some());
    }

    // ── Debug ────────────────────────────────────────────────

    #[test]
    fn debug_format_includes_entries() {
        let obj: Object = vec![(
            Value::String("name".to_string()),
            Value::String("test".to_string()),
        )]
        .into_iter()
        .collect();
        let debug = format!("{:?}", obj);
        assert!(debug.contains("name"));
        assert!(debug.contains("test"));
    }

    // ── Hash ─────────────────────────────────────────────────

    #[test]
    fn hash_equal_objects_same_hash() {
        use core::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;

        fn hash_of(o: &Object) -> u64 {
            let mut h = DefaultHasher::new();
            o.hash(&mut h);
            h.finish()
        }

        let a: Object = vec![(Value::String("k".to_string()), Value::U64(1))]
            .into_iter()
            .collect();
        let b: Object = vec![(Value::String("k".to_string()), Value::U64(1))]
            .into_iter()
            .collect();
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    // ── BorrowedKey ──────────────────────────────────────────

    #[test]
    fn borrowed_key_str_equals_value_string() {
        let bk_str = BorrowedKey::Str("hello");
        let val = Value::String("hello".to_string());
        let bk_val = BorrowedKey::Value(&val);
        // Both should produce equivalent keys via ObjectKeyTrait
        assert_eq!(val.key(), bk_str);
        assert_eq!(bk_str.key(), bk_str);
        // BorrowedKey::Value wraps the full Value
        assert_eq!(bk_val.key(), bk_val);
    }

    #[test]
    fn borrowed_key_copy() {
        let bk = BorrowedKey::Str("test");
        let copied = bk;
        let copied2 = bk;
        assert_eq!(copied, copied2);
    }

    #[test]
    fn object_key_trait_string_produces_str_key() {
        let val = Value::String("abc".to_string());
        match val.key() {
            BorrowedKey::Str(s) => assert_eq!(s, "abc"),
            _ => panic!("expected BorrowedKey::Str"),
        }
    }

    #[test]
    fn object_key_trait_non_string_produces_value_key() {
        let val = Value::U64(42);
        match val.key() {
            BorrowedKey::Value(v) => assert_eq!(*v, Value::U64(42)),
            _ => panic!("expected BorrowedKey::Value"),
        }
    }

    // ── Nested objects ───────────────────────────────────────

    #[test]
    fn nested_object_creation_and_access() {
        let inner: Object = vec![(Value::String("x".to_string()), Value::U64(42))]
            .into_iter()
            .collect();
        let outer: Object = vec![(Value::String("nested".to_string()), Value::Object(inner))]
            .into_iter()
            .collect();
        if let Some(Value::Object(ref inner)) = outer.get_by_str("nested") {
            assert_eq!(inner.get_by_str("x"), Some(&Value::U64(42)));
        } else {
            panic!("expected nested object");
        }
    }

    // ── Drop safety ──────────────────────────────────────────

    #[test]
    fn deep_nested_object_drops_without_stack_overflow() {
        let mut current = Object::default();
        for i in 0..100 {
            let mut next = Object::default();
            next.insert(Value::String(format!("level_{i}")), Value::Object(current));
            current = next;
        }
        drop(current);
    }
}
