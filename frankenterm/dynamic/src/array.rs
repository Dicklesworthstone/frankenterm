use crate::Value;
use core::cmp::Ordering;
use core::iter::FromIterator;
use core::ops::{Deref, DerefMut};

#[cfg(not(feature = "std"))]
extern crate alloc;

use alloc::vec::Vec;

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Array {
    inner: Vec<Value>,
}

impl Ord for Array {
    fn cmp(&self, other: &Self) -> Ordering {
        let self_ptr = self as *const Self;
        let other_ptr = other as *const Self;
        self_ptr.cmp(&other_ptr)
    }
}

impl PartialOrd for Array {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<Vec<Value>> for Array {
    fn from(inner: Vec<Value>) -> Self {
        Self { inner }
    }
}

impl Drop for Array {
    fn drop(&mut self) {
        self.inner.drain(..).for_each(crate::drop::safely);
    }
}

fn take(array: Array) -> Vec<Value> {
    let array = core::mem::ManuallyDrop::new(array);
    unsafe { core::ptr::read(&array.inner) }
}

impl Array {
    pub fn new() -> Self {
        Array { inner: Vec::new() }
    }
}

impl Deref for Array {
    type Target = Vec<Value>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Array {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl IntoIterator for Array {
    type Item = Value;
    type IntoIter = <Vec<Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        take(self).into_iter()
    }
}

impl<'a> IntoIterator for &'a Array {
    type Item = &'a Value;
    type IntoIter = <&'a Vec<Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut Array {
    type Item = &'a mut Value;
    type IntoIter = <&'a mut Vec<Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl FromIterator<Value> for Array {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = Value>,
    {
        Array {
            inner: Vec::from_iter(iter),
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;
    extern crate std;

    use super::*;
    use alloc::vec;

    // ── Construction ─────────────────────────────────────────

    #[test]
    fn new_creates_empty_array() {
        let a = Array::new();
        assert!(a.is_empty());
        assert_eq!(a.len(), 0);
    }

    #[test]
    fn from_vec() {
        let v = vec![Value::U64(1), Value::Bool(true)];
        let a: Array = v.into();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0], Value::U64(1));
        assert_eq!(a[1], Value::Bool(true));
    }

    #[test]
    fn from_iter_collects() {
        let a: Array = (0..5).map(Value::U64).collect();
        assert_eq!(a.len(), 5);
        assert_eq!(a[4], Value::U64(4));
    }

    #[test]
    fn default_is_empty() {
        let a = Array::default();
        assert!(a.is_empty());
    }

    // ── Clone & Equality ─────────────────────────────────────

    #[test]
    fn clone_produces_equal_array() {
        let a: Array = vec![Value::U64(1), Value::String("hello".into())].into();
        let b = a.clone();
        assert_eq!(a, b);
    }

    #[test]
    fn equality_same_elements() {
        let a: Array = vec![Value::U64(1)].into();
        let b: Array = vec![Value::U64(1)].into();
        assert_eq!(a, b);
    }

    #[test]
    fn inequality_different_elements() {
        let a: Array = vec![Value::U64(1)].into();
        let b: Array = vec![Value::U64(2)].into();
        assert_ne!(a, b);
    }

    #[test]
    fn inequality_different_lengths() {
        let a: Array = vec![Value::U64(1)].into();
        let b: Array = vec![Value::U64(1), Value::U64(2)].into();
        assert_ne!(a, b);
    }

    // ── Deref to Vec ─────────────────────────────────────────

    #[test]
    fn deref_provides_vec_methods() {
        let a: Array = vec![Value::U64(10), Value::U64(20)].into();
        assert_eq!(a.len(), 2);
        assert!(!a.is_empty());
        assert!(a.contains(&Value::U64(10)));
        assert!(!a.contains(&Value::U64(99)));
    }

    #[test]
    fn deref_mut_allows_push() {
        let mut a = Array::new();
        a.push(Value::Bool(true));
        a.push(Value::Null);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0], Value::Bool(true));
        assert_eq!(a[1], Value::Null);
    }

    #[test]
    fn deref_mut_allows_pop() {
        let mut a: Array = vec![Value::U64(1), Value::U64(2)].into();
        assert_eq!(a.pop(), Some(Value::U64(2)));
        assert_eq!(a.len(), 1);
    }

    // ── IntoIterator ─────────────────────────────────────────

    #[test]
    fn into_iter_owned() {
        let a: Array = vec![Value::U64(1), Value::U64(2), Value::U64(3)].into();
        let collected: Vec<Value> = a.into_iter().collect();
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0], Value::U64(1));
    }

    #[test]
    fn into_iter_ref() {
        let a: Array = vec![Value::Bool(true)].into();
        let refs: Vec<&Value> = (&a).into_iter().collect();
        assert_eq!(refs, vec![&Value::Bool(true)]);
    }

    #[test]
    fn into_iter_mut_ref() {
        let mut a: Array = vec![Value::U64(0)].into();
        for v in &mut a {
            *v = Value::U64(42);
        }
        assert_eq!(a[0], Value::U64(42));
    }

    // ── Ord / PartialOrd ─────────────────────────────────────

    #[test]
    fn partial_ord_returns_some() {
        let a = Array::new();
        let b = Array::new();
        assert!(a.partial_cmp(&b).is_some());
    }

    // ── Debug ────────────────────────────────────────────────

    #[test]
    fn debug_format_shows_elements() {
        let a: Array = vec![Value::U64(1), Value::Bool(false)].into();
        let debug = alloc::format!("{:?}", a);
        assert!(debug.contains('1'));
        assert!(debug.contains("false"));
    }

    // ── Hash ─────────────────────────────────────────────────

    #[test]
    fn hash_equal_arrays_same_hash() {
        use core::hash::{Hash, Hasher};
        use std::collections::hash_map::DefaultHasher;

        fn hash_of(a: &Array) -> u64 {
            let mut h = DefaultHasher::new();
            a.hash(&mut h);
            h.finish()
        }

        let a: Array = vec![Value::U64(1), Value::U64(2)].into();
        let b: Array = vec![Value::U64(1), Value::U64(2)].into();
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    // ── Nested arrays ────────────────────────────────────────

    #[test]
    fn nested_array_creation_and_access() {
        let inner: Array = vec![Value::U64(42)].into();
        let outer: Array = vec![Value::Array(inner)].into();
        if let Value::Array(ref inner) = outer[0] {
            assert_eq!(inner[0], Value::U64(42));
        } else {
            panic!("expected inner array");
        }
    }

    // ── Drop safety (deeply nested) ──────────────────────────

    #[test]
    fn deep_nested_array_drops_without_stack_overflow() {
        let mut current = Array::new();
        for i in 0..100 {
            let next: Array = vec![Value::U64(i), Value::Array(current)].into();
            current = next;
        }
        // Dropping current should use iterative drop, not recursive
        drop(current);
    }
}
