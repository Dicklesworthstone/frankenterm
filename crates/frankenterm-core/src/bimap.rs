//! Bidirectional map: two-way `K ↔ V` lookup with O(1) access in both directions.
//!
//! A `BiMap<K, V>` maintains two `HashMap`s internally so that lookups by key
//! and by value are both constant-time.  Insertions that collide on either side
//! silently evict the old pairing, keeping both directions consistent.
//!
//! Useful for pane ID remapping (old_id ↔ new_id), name↔index mappings, and
//! any scenario where reverse lookups are needed.

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;

use serde::{Deserialize, Serialize};

/// A bidirectional map with O(1) lookup in both directions.
///
/// Both `K` and `V` must be `Clone + Eq + Hash`.  On insertion, if the key
/// already exists the old value is removed; if the value already exists its
/// old key is removed.  This ensures the mapping is always a bijection.
#[derive(Clone, Serialize, Deserialize)]
pub struct BiMap<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone + Eq + Hash,
{
    forward: HashMap<K, V>,
    reverse: HashMap<V, K>,
}

impl<K, V> BiMap<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone + Eq + Hash,
{
    /// Create an empty BiMap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
        }
    }

    /// Create a BiMap with pre-allocated capacity.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            forward: HashMap::with_capacity(capacity),
            reverse: HashMap::with_capacity(capacity),
        }
    }

    /// Number of key-value pairs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.forward.len()
    }

    /// True if the map is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.forward.is_empty()
    }

    /// Insert a key-value pair.
    ///
    /// If `key` was already present, its old value is removed (and the old
    /// value's reverse entry is cleaned up).  If `value` was already present
    /// under a different key, that old key is removed too.
    ///
    /// Returns the previous value associated with `key`, if any.
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        // Remove any existing mapping for the value (different key)
        if let Some(old_key) = self.reverse.remove(&value) {
            if old_key != key {
                self.forward.remove(&old_key);
            }
        }

        // Remove old value for this key
        let old_value = self.forward.insert(key.clone(), value.clone());
        if let Some(ref old_v) = old_value {
            if *old_v != value {
                self.reverse.remove(old_v);
            }
        }

        self.reverse.insert(value, key);
        old_value
    }

    /// Look up value by key.
    #[must_use]
    pub fn get_by_key(&self, key: &K) -> Option<&V> {
        self.forward.get(key)
    }

    /// Look up key by value (reverse lookup).
    #[must_use]
    pub fn get_by_value(&self, value: &V) -> Option<&K> {
        self.reverse.get(value)
    }

    /// Check if a key is present.
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.forward.contains_key(key)
    }

    /// Check if a value is present.
    #[must_use]
    pub fn contains_value(&self, value: &V) -> bool {
        self.reverse.contains_key(value)
    }

    /// Remove by key. Returns the value if the key was present.
    pub fn remove_by_key(&mut self, key: &K) -> Option<V> {
        if let Some(value) = self.forward.remove(key) {
            self.reverse.remove(&value);
            Some(value)
        } else {
            None
        }
    }

    /// Remove by value. Returns the key if the value was present.
    pub fn remove_by_value(&mut self, value: &V) -> Option<K> {
        if let Some(key) = self.reverse.remove(value) {
            self.forward.remove(&key);
            Some(key)
        } else {
            None
        }
    }

    /// Remove all entries.
    pub fn clear(&mut self) {
        self.forward.clear();
        self.reverse.clear();
    }

    /// Iterate over `(key, value)` pairs.
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.forward.iter()
    }

    /// Iterate over keys.
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.forward.keys()
    }

    /// Iterate over values.
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.forward.values()
    }

    /// Collect all pairs into a Vec.
    pub fn to_pairs(&self) -> Vec<(K, V)>
    where
        K: Clone,
        V: Clone,
    {
        self.forward
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// Build a BiMap from an iterator of (key, value) pairs.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (K, V)>) -> Self {
        let mut bimap = Self::new();
        for (k, v) in pairs {
            bimap.insert(k, v);
        }
        bimap
    }

    /// Swap key and value sides, producing a `BiMap<V, K>`.
    #[must_use]
    pub fn inverse(&self) -> BiMap<V, K> {
        BiMap {
            forward: self.reverse.clone(),
            reverse: self.forward.clone(),
        }
    }

    /// Retain only pairs where the predicate returns true.
    pub fn retain<F>(&mut self, mut f: F)
    where
        F: FnMut(&K, &V) -> bool,
    {
        let to_remove: Vec<K> = self
            .forward
            .iter()
            .filter(|(k, v)| !f(k, v))
            .map(|(k, _)| k.clone())
            .collect();
        for key in to_remove {
            self.remove_by_key(&key);
        }
    }
}

impl<K, V> Default for BiMap<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone + Eq + Hash,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> PartialEq for BiMap<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone + Eq + Hash,
{
    fn eq(&self, other: &Self) -> bool {
        self.forward == other.forward
    }
}

impl<K, V> Eq for BiMap<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone + Eq + Hash,
{
}

impl<K, V> fmt::Debug for BiMap<K, V>
where
    K: Clone + Eq + Hash + fmt::Debug,
    V: Clone + Eq + Hash + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BiMap")
            .field("len", &self.len())
            .field("pairs", &self.forward)
            .finish()
    }
}

impl<K, V> fmt::Display for BiMap<K, V>
where
    K: Clone + Eq + Hash + fmt::Display,
    V: Clone + Eq + Hash + fmt::Display,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{{")?;
        let mut first = true;
        for (k, v) in &self.forward {
            if !first {
                write!(f, ", ")?;
            }
            write!(f, "{k} <-> {v}")?;
            first = false;
        }
        write!(f, "}}")
    }
}

impl<K, V> FromIterator<(K, V)> for BiMap<K, V>
where
    K: Clone + Eq + Hash,
    V: Clone + Eq + Hash,
{
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Self::from_pairs(iter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let bm: BiMap<u32, String> = BiMap::new();
        assert!(bm.is_empty());
        assert_eq!(bm.len(), 0);
    }

    #[test]
    fn insert_and_lookup() {
        let mut bm = BiMap::new();
        bm.insert(1, "one".to_string());
        assert_eq!(bm.get_by_key(&1), Some(&"one".to_string()));
        assert_eq!(bm.get_by_value(&"one".to_string()), Some(&1));
    }

    #[test]
    fn insert_overwrites_old_key_for_value() {
        let mut bm = BiMap::new();
        bm.insert(1, "x".to_string());
        bm.insert(2, "x".to_string());
        // key 1 should be evicted because "x" now maps to key 2
        assert!(!bm.contains_key(&1));
        assert_eq!(bm.get_by_key(&2), Some(&"x".to_string()));
        assert_eq!(bm.get_by_value(&"x".to_string()), Some(&2));
        assert_eq!(bm.len(), 1);
    }

    #[test]
    fn insert_overwrites_old_value_for_key() {
        let mut bm = BiMap::new();
        bm.insert(1, "a".to_string());
        bm.insert(1, "b".to_string());
        assert!(!bm.contains_value(&"a".to_string()));
        assert_eq!(bm.get_by_key(&1), Some(&"b".to_string()));
        assert_eq!(bm.len(), 1);
    }

    #[test]
    fn contains_key_and_value() {
        let mut bm = BiMap::new();
        bm.insert(42u32, 99u32);
        assert!(bm.contains_key(&42));
        assert!(bm.contains_value(&99));
        assert!(!bm.contains_key(&99));
        assert!(!bm.contains_value(&42));
    }

    #[test]
    fn remove_by_key() {
        let mut bm = BiMap::new();
        bm.insert(1, 10);
        assert_eq!(bm.remove_by_key(&1), Some(10));
        assert!(bm.is_empty());
        assert!(!bm.contains_value(&10));
    }

    #[test]
    fn remove_by_value() {
        let mut bm = BiMap::new();
        bm.insert(1, 10);
        assert_eq!(bm.remove_by_value(&10), Some(1));
        assert!(bm.is_empty());
        assert!(!bm.contains_key(&1));
    }

    #[test]
    fn remove_nonexistent_returns_none() {
        let mut bm: BiMap<u32, u32> = BiMap::new();
        assert_eq!(bm.remove_by_key(&999), None);
        assert_eq!(bm.remove_by_value(&999), None);
    }

    #[test]
    fn clear() {
        let mut bm = BiMap::new();
        bm.insert(1, "a".to_string());
        bm.insert(2, "b".to_string());
        bm.clear();
        assert!(bm.is_empty());
    }

    #[test]
    fn len_tracks_insertions() {
        let mut bm = BiMap::new();
        bm.insert(1u32, 10u32);
        bm.insert(2, 20);
        bm.insert(3, 30);
        assert_eq!(bm.len(), 3);
        bm.remove_by_key(&2);
        assert_eq!(bm.len(), 2);
    }

    #[test]
    fn with_capacity() {
        let bm: BiMap<u32, u32> = BiMap::with_capacity(100);
        assert!(bm.is_empty());
    }

    #[test]
    fn inverse() {
        let mut bm = BiMap::new();
        bm.insert(1u32, "one".to_string());
        bm.insert(2, "two".to_string());
        let inv = bm.inverse();
        assert_eq!(inv.get_by_key(&"one".to_string()), Some(&1));
        assert_eq!(inv.get_by_key(&"two".to_string()), Some(&2));
        assert_eq!(inv.get_by_value(&1), Some(&"one".to_string()));
    }

    #[test]
    fn from_pairs() {
        let bm = BiMap::from_pairs(vec![(1, 10), (2, 20), (3, 30)]);
        assert_eq!(bm.len(), 3);
        assert_eq!(bm.get_by_key(&2), Some(&20));
        assert_eq!(bm.get_by_value(&30), Some(&3));
    }

    #[test]
    fn to_pairs() {
        let mut bm = BiMap::new();
        bm.insert(1u32, 10u32);
        let pairs = bm.to_pairs();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0], (1, 10));
    }

    #[test]
    fn from_iterator() {
        let bm: BiMap<u32, u32> = vec![(1, 10), (2, 20)].into_iter().collect();
        assert_eq!(bm.len(), 2);
    }

    #[test]
    fn iter() {
        let bm = BiMap::from_pairs(vec![(1, 10), (2, 20)]);
        let mut pairs: Vec<(u32, u32)> = bm.iter().map(|(&k, &v)| (k, v)).collect();
        pairs.sort();
        assert_eq!(pairs, vec![(1, 10), (2, 20)]);
    }

    #[test]
    fn keys_and_values() {
        let bm = BiMap::from_pairs(vec![(1u32, 10u32), (2, 20)]);
        let mut keys: Vec<u32> = bm.keys().copied().collect();
        keys.sort();
        assert_eq!(keys, vec![1, 2]);
        let mut vals: Vec<u32> = bm.values().copied().collect();
        vals.sort();
        assert_eq!(vals, vec![10, 20]);
    }

    #[test]
    fn retain() {
        let mut bm = BiMap::from_pairs(vec![(1, 10), (2, 20), (3, 30), (4, 40)]);
        bm.retain(|k, _| *k % 2 == 0);
        assert_eq!(bm.len(), 2);
        assert!(bm.contains_key(&2));
        assert!(bm.contains_key(&4));
        assert!(!bm.contains_key(&1));
        // Reverse lookups also cleaned
        assert!(!bm.contains_value(&10));
        assert!(bm.contains_value(&20));
    }

    #[test]
    fn default_is_empty() {
        let bm: BiMap<u32, u32> = BiMap::default();
        assert!(bm.is_empty());
    }

    #[test]
    fn equality() {
        let a = BiMap::from_pairs(vec![(1, 10), (2, 20)]);
        let b = BiMap::from_pairs(vec![(2, 20), (1, 10)]);
        assert_eq!(a, b);
    }

    #[test]
    fn inequality() {
        let a = BiMap::from_pairs(vec![(1, 10)]);
        let b = BiMap::from_pairs(vec![(1, 20)]);
        assert_ne!(a, b);
    }

    #[test]
    fn debug_format() {
        let bm = BiMap::from_pairs(vec![(1u32, 10u32)]);
        let dbg = format!("{bm:?}");
        assert!(dbg.contains("BiMap"));
        assert!(dbg.contains("len"));
    }

    #[test]
    fn display_format() {
        let bm = BiMap::from_pairs(vec![(1u32, 10u32)]);
        let disp = format!("{bm}");
        assert!(disp.contains("1 <-> 10"));
    }

    #[test]
    fn serde_roundtrip() {
        let bm = BiMap::from_pairs(vec![(1u32, 10u32), (2, 20)]);
        let json = serde_json::to_string(&bm).unwrap();
        let back: BiMap<u32, u32> = serde_json::from_str(&json).unwrap();
        assert_eq!(bm.len(), back.len());
        assert_eq!(bm.get_by_key(&1), back.get_by_key(&1));
        assert_eq!(bm.get_by_key(&2), back.get_by_key(&2));
    }

    #[test]
    fn insert_returns_old_value() {
        let mut bm = BiMap::new();
        assert_eq!(bm.insert(1, "a".to_string()), None);
        assert_eq!(bm.insert(1, "b".to_string()), Some("a".to_string()));
    }

    #[test]
    fn string_keys_and_values() {
        let mut bm = BiMap::new();
        bm.insert("hello".to_string(), "world".to_string());
        assert_eq!(
            bm.get_by_key(&"hello".to_string()),
            Some(&"world".to_string())
        );
        assert_eq!(
            bm.get_by_value(&"world".to_string()),
            Some(&"hello".to_string())
        );
    }

    #[test]
    fn inverse_of_inverse_equals_original() {
        let bm = BiMap::from_pairs(vec![(1u32, 10u32), (2, 20), (3, 30)]);
        let double_inv = bm.inverse().inverse();
        assert_eq!(bm, double_inv);
    }

    #[test]
    fn bijection_maintained_complex() {
        let mut bm = BiMap::new();
        // Insert a -> 1
        bm.insert("a".to_string(), 1u32);
        // Insert b -> 1 (should evict a)
        bm.insert("b".to_string(), 1);
        assert_eq!(bm.len(), 1);
        assert!(!bm.contains_key(&"a".to_string()));
        assert!(bm.contains_key(&"b".to_string()));
        // Insert b -> 2 (should update b's value)
        bm.insert("b".to_string(), 2);
        assert_eq!(bm.len(), 1);
        assert!(!bm.contains_value(&1));
        assert_eq!(bm.get_by_key(&"b".to_string()), Some(&2));
    }

    #[test]
    fn clone() {
        let bm = BiMap::from_pairs(vec![(1, 10), (2, 20)]);
        let cloned = bm.clone();
        assert_eq!(bm, cloned);
    }

    #[test]
    fn empty_iter() {
        let bm: BiMap<u32, u32> = BiMap::new();
        assert_eq!(bm.iter().count(), 0);
        assert_eq!(bm.keys().count(), 0);
        assert_eq!(bm.values().count(), 0);
    }
}
