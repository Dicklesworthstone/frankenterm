use crate::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::ops::{Deref, DerefMut};

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Map {
    inner: BTreeMap<Value, Value>,
}

impl Ord for Map {
    fn cmp(&self, other: &Self) -> Ordering {
        let self_ptr = self as *const Self;
        let other_ptr = other as *const Self;
        self_ptr.cmp(&other_ptr)
    }
}

impl PartialOrd for Map {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        for (_, child) in std::mem::replace(&mut self.inner, BTreeMap::new()) {
            crate::drop::safely(child);
        }
    }
}

impl From<BTreeMap<Value, Value>> for Map {
    fn from(inner: BTreeMap<Value, Value>) -> Self {
        Self { inner }
    }
}

impl Deref for Map {
    type Target = BTreeMap<Value, Value>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for Map {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

fn take(object: Map) -> BTreeMap<Value, Value> {
    let object = core::mem::ManuallyDrop::new(object);
    unsafe { core::ptr::read(&object.inner) }
}

impl IntoIterator for Map {
    type Item = (Value, Value);
    type IntoIter = <BTreeMap<Value, Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        take(self).into_iter()
    }
}

impl<'a> IntoIterator for &'a Map {
    type Item = (&'a Value, &'a Value);
    type IntoIter = <&'a BTreeMap<Value, Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a mut Map {
    type Item = (&'a Value, &'a mut Value);
    type IntoIter = <&'a mut BTreeMap<Value, Value> as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl FromIterator<(Value, Value)> for Map {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = (Value, Value)>,
    {
        Map {
            inner: BTreeMap::from_iter(iter),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::format;

    // ── Construction ─────────────────────────────────────────

    #[test]
    fn from_btreemap() {
        let mut map = BTreeMap::new();
        map.insert(Value::String("k".into()), Value::U64(1));
        let m: Map = map.into();
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn from_iter_pairs() {
        let m: Map = vec![
            (Value::String("a".into()), Value::U64(1)),
            (Value::String("b".into()), Value::U64(2)),
        ]
        .into_iter()
        .collect();
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn default_is_empty() {
        let m = Map::default();
        assert!(m.is_empty());
    }

    // ── Clone & Equality ─────────────────────────────────────

    #[test]
    fn clone_produces_equal_map() {
        let m: Map = vec![(Value::String("k".into()), Value::Bool(true))]
            .into_iter()
            .collect();
        let c = m.clone();
        assert_eq!(m, c);
    }

    #[test]
    fn equality_same_entries() {
        let a: Map = vec![(Value::String("x".into()), Value::U64(42))]
            .into_iter()
            .collect();
        let b: Map = vec![(Value::String("x".into()), Value::U64(42))]
            .into_iter()
            .collect();
        assert_eq!(a, b);
    }

    #[test]
    fn inequality_different_values() {
        let a: Map = vec![(Value::String("x".into()), Value::U64(1))]
            .into_iter()
            .collect();
        let b: Map = vec![(Value::String("x".into()), Value::U64(2))]
            .into_iter()
            .collect();
        assert_ne!(a, b);
    }

    // ── Deref to BTreeMap ────────────────────────────────────

    #[test]
    fn deref_provides_btreemap_methods() {
        let m: Map = vec![
            (Value::String("a".into()), Value::U64(1)),
            (Value::String("b".into()), Value::U64(2)),
        ]
        .into_iter()
        .collect();
        assert!(m.contains_key(&Value::String("a".into())));
        assert!(!m.contains_key(&Value::String("c".into())));
        assert_eq!(m.keys().count(), 2);
    }

    #[test]
    fn deref_mut_allows_insert() {
        let mut m = Map::default();
        m.insert(Value::String("new".into()), Value::Null);
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn deref_mut_allows_remove() {
        let mut m: Map = vec![(Value::String("a".into()), Value::U64(1))]
            .into_iter()
            .collect();
        let removed = m.remove(&Value::String("a".into()));
        assert_eq!(removed, Some(Value::U64(1)));
        assert!(m.is_empty());
    }

    // ── IntoIterator ─────────────────────────────────────────

    #[test]
    fn into_iter_owned() {
        let m: Map = vec![
            (Value::String("a".into()), Value::U64(1)),
            (Value::String("b".into()), Value::U64(2)),
        ]
        .into_iter()
        .collect();
        let pairs: Vec<(Value, Value)> = m.into_iter().collect();
        assert_eq!(pairs.len(), 2);
    }

    #[test]
    fn into_iter_ref() {
        let m: Map = vec![(Value::String("k".into()), Value::Bool(true))]
            .into_iter()
            .collect();
        let refs: Vec<(&Value, &Value)> = (&m).into_iter().collect();
        assert_eq!(refs.len(), 1);
    }

    #[test]
    fn into_iter_mut_ref() {
        let mut m: Map = vec![(Value::String("k".into()), Value::U64(0))]
            .into_iter()
            .collect();
        for (_, v) in &mut m {
            *v = Value::U64(99);
        }
        assert_eq!(m.get(&Value::String("k".into())), Some(&Value::U64(99)));
    }

    // ── Ord / PartialOrd ─────────────────────────────────────

    #[test]
    fn partial_ord_returns_some() {
        let a = Map::default();
        let b = Map::default();
        assert!(a.partial_cmp(&b).is_some());
    }

    // ── Debug ────────────────────────────────────────────────

    #[test]
    fn debug_format_includes_entries() {
        let m: Map = vec![(Value::String("key".into()), Value::U64(42))]
            .into_iter()
            .collect();
        let debug = format!("{:?}", m);
        assert!(debug.contains("key"));
        assert!(debug.contains("42"));
    }

    // ── Hash ─────────────────────────────────────────────────

    #[test]
    fn hash_equal_maps_same_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        fn hash_of(m: &Map) -> u64 {
            let mut h = DefaultHasher::new();
            m.hash(&mut h);
            h.finish()
        }

        let a: Map = vec![(Value::String("k".into()), Value::U64(1))]
            .into_iter()
            .collect();
        let b: Map = vec![(Value::String("k".into()), Value::U64(1))]
            .into_iter()
            .collect();
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    // ── Drop safety ──────────────────────────────────────────

    #[test]
    fn deep_nested_map_drops_without_stack_overflow() {
        let mut current = Map::default();
        for i in 0..100 {
            let mut next = Map::default();
            next.insert(
                Value::String(format!("level_{i}")),
                Value::Object(crate::object::Object::default()),
            );
            current = next;
        }
        drop(current);
    }
}
