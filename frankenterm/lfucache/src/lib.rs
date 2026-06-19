use ahash::AHasher;
use config::ConfigHandle;
use intrusive_collections::{
    Bound, KeyAdapter, LinkedList, LinkedListLink, RBTree, RBTreeLink, intrusive_adapter,
};
use std::borrow::Borrow;
use std::cell::RefCell;
use std::cmp::Eq;
use std::collections::VecDeque;
use std::fmt::Debug;
use std::hash::{BuildHasher, BuildHasherDefault, Hash};
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheEvictionPolicy {
    Lfu,
    S3Fifo,
}

impl CacheEvictionPolicy {
    #[must_use]
    pub fn from_cache_eviction_value(value: &str) -> Option<Self> {
        match value {
            "lfu" | "fifo" | "current" | "default" => Some(Self::Lfu),
            "s3fifo" | "s3-fifo" | "wtinylfu" | "w-tinylfu" => Some(Self::S3Fifo),
            _ => None,
        }
    }
}

impl Default for CacheEvictionPolicy {
    fn default() -> Self {
        Self::Lfu
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum S3Segment {
    Small,
    Main,
}

struct Entry<K, V> {
    hash_link: LinkedListLink,
    recency_link: LinkedListLink,
    frequency_link: RBTreeLink,
    freq: RefCell<u16>,
    last_tick: RefCell<u32>,
    s3_segment: RefCell<S3Segment>,
    key: K,
    value: V,
}

intrusive_adapter!(RecencyAdapter<K,V> = Rc<Entry<K,V>>: Entry<K,V> { recency_link => LinkedListLink });
intrusive_adapter!(FrequenceAdapter<K,V> = Rc<Entry<K,V>>: Entry<K,V> { frequency_link => RBTreeLink });
intrusive_adapter!(HashAdapter<K,V> = Rc<Entry<K,V>>: Entry<K,V> { hash_link => LinkedListLink });

/// Key by Entry::freq
impl<'a, K, V> KeyAdapter<'a> for FrequenceAdapter<K, V> {
    type Key = u16;
    fn get_key(&self, entry: &'a Entry<K, V>) -> u16 {
        *entry.freq.borrow()
    }
}

pub type CapFunc = fn(&ConfigHandle) -> usize;

/// A cache using a Least-Frequently-Used eviction policy.
/// If K is u64 you should use LfuCacheU64 instead as it has
/// a more optimal hasher for integer keys.
pub struct LfuCache<K, V, S = BuildHasherDefault<AHasher>> {
    hit: &'static str,
    miss: &'static str,
    cap: usize,
    cap_func: CapFunc,
    hasher: S,

    /// hash buckets for key-based lookup
    buckets: Vec<LinkedList<HashAdapter<K, V>>>,
    /// frequency-keyed rb-tree
    frequency_index: RBTree<FrequenceAdapter<K, V>>,
    /// the back is the least-recently-used whereas the front is the
    /// most-recently-used
    recency_index: LinkedList<RecencyAdapter<K, V>>,
    /// Number of items in the cache
    len: usize,
    /// tracks number of operations that affect the frequency/age of entries
    tick: u32,
    eviction_policy: CacheEvictionPolicy,
    s3_small_len: usize,
    s3_ghost: VecDeque<K>,
}

impl<K: Hash + Eq + Clone + Debug, V, S: Default + BuildHasher> LfuCache<K, V, S> {
    #[cfg(test)]
    fn with_capacity(cap: usize) -> Self {
        Self::with_capacity_and_policy(cap, CacheEvictionPolicy::Lfu)
    }

    #[cfg(test)]
    fn with_capacity_and_policy(cap: usize, eviction_policy: CacheEvictionPolicy) -> Self {
        let mut buckets = vec![];
        let num_buckets = (cap / 10).next_power_of_two();
        for _ in 0..num_buckets {
            buckets.push(LinkedList::new(HashAdapter::new()));
        }

        let hasher = S::default();

        fn dummy_cap_func(_: &ConfigHandle) -> usize {
            8
        }

        Self {
            hit: "hit",
            miss: "miss",
            cap,
            cap_func: dummy_cap_func,
            buckets,
            frequency_index: RBTree::new(FrequenceAdapter::new()),
            recency_index: LinkedList::new(RecencyAdapter::new()),
            len: 0,
            tick: 0,
            eviction_policy,
            s3_small_len: 0,
            s3_ghost: VecDeque::new(),
            hasher,
        }
    }

    pub fn new(
        hit: &'static str,
        miss: &'static str,
        cap_func: CapFunc,
        config: &ConfigHandle,
    ) -> Self {
        let cap = cap_func(config);
        let mut buckets = vec![];
        let num_buckets = (cap / 10).next_power_of_two();
        for _ in 0..num_buckets {
            buckets.push(LinkedList::new(HashAdapter::new()));
        }

        let hasher = S::default();

        Self {
            hit,
            miss,
            cap,
            cap_func,
            buckets,
            frequency_index: RBTree::new(FrequenceAdapter::new()),
            recency_index: LinkedList::new(RecencyAdapter::new()),
            len: 0,
            tick: 0,
            eviction_policy: CacheEvictionPolicy::Lfu,
            s3_small_len: 0,
            s3_ghost: VecDeque::new(),
            hasher,
        }
    }

    pub fn new_with_eviction_policy(
        hit: &'static str,
        miss: &'static str,
        cap_func: CapFunc,
        config: &ConfigHandle,
        eviction_policy: CacheEvictionPolicy,
    ) -> Self {
        let mut cache = Self::new(hit, miss, cap_func, config);
        cache.eviction_policy = eviction_policy;
        cache
    }

    pub fn set_eviction_policy(&mut self, eviction_policy: CacheEvictionPolicy) {
        self.eviction_policy = eviction_policy;
        self.s3_small_len = if eviction_policy == CacheEvictionPolicy::S3Fifo {
            self.recency_index
                .iter()
                .filter(|entry| *entry.s3_segment.borrow() == S3Segment::Small)
                .count()
        } else {
            0
        };
        if eviction_policy == CacheEvictionPolicy::Lfu {
            self.s3_ghost.clear();
        }
    }

    fn bucket_for_key<Q: Hash + ?Sized>(&self, k: &Q) -> usize {
        (self.hasher.hash_one(k) as usize) % self.buckets.len()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Grow the hash buckets in the pursuit of reducing potential
    /// key collisions in any given bucket
    fn grow_hash(&mut self) {
        let num_buckets = self.buckets.len() * 2;
        let mut buckets = vec![];
        for _ in 0..num_buckets {
            buckets.push(LinkedList::new(HashAdapter::new()));
        }
        std::mem::swap(&mut buckets, &mut self.buckets);

        for mut old_bucket in buckets {
            while let Some(entry) = old_bucket.pop_front() {
                let bucket = self.bucket_for_key(&entry.key);
                self.buckets[bucket].push_front(entry);
            }
        }
    }

    pub fn update_config(&mut self, config: &ConfigHandle) {
        let _ = self.update_config_capturing_evictions(config);
    }

    pub fn update_config_capturing_evictions(&mut self, config: &ConfigHandle) -> Vec<(K, V)> {
        let mut evicted = Vec::new();
        let new_cap = (self.cap_func)(config);
        if new_cap != self.cap {
            self.cap = new_cap;
            self.trim_s3_ghost();
            while self.len > self.cap {
                if let Some(entry) = self.evict_one_for_policy() {
                    evicted.push(entry);
                } else {
                    break;
                }
            }
        }
        evicted
    }

    /// In order to mitigate previously-very-hot entries that are
    /// not currently being accessed from occupying the bulk of the
    /// table, this function finds the least-recently-accessed item
    /// and decreases its frequency value based on the number of ticks
    /// that have occurred since its last use.
    fn decay_least_recent(&mut self) {
        let mut cursor = self.recency_index.back_mut();
        if let Some(entry) = cursor.get() {
            if *entry.freq.borrow() == 0 {
                // No point removing/reinserting in the rbtree if the freq
                // is already 0
                return;
            }

            let delta = (self.tick.wrapping_sub(*entry.last_tick.borrow()) / 10) as u16;
            if delta <= 1 {
                // No point removing/reinserting in the rbtree if there is no change
                return;
            }

            // Adjust lfu
            unsafe {
                let lfu_entry = self
                    .frequency_index
                    .cursor_mut_from_ptr(entry)
                    .remove()
                    .unwrap();
                {
                    let mut freq = entry.freq.borrow_mut();
                    *freq /= delta;
                }
                self.frequency_index.insert(lfu_entry);
            }

            // Adjust lru so that we don't immediately revisit this one
            // on the next decay_least_recent() call
            let lru_entry = cursor.remove().unwrap();
            self.recency_index.push_front(lru_entry);
        }
    }

    /// Remove the entry with the smallest frequency value
    fn entry_into_pair(entry: Rc<Entry<K, V>>) -> Option<(K, V)> {
        Rc::try_unwrap(entry)
            .ok()
            .map(|entry| (entry.key, entry.value))
    }

    fn evict_one(&mut self) -> Option<(K, V)> {
        self.decay_least_recent();

        let mut cursor = self.frequency_index.lower_bound_mut(Bound::Included(&0));
        if let Some(entry) = cursor.remove() {
            let bucket = self.bucket_for_key(&entry.key);
            let was_s3_small = *entry.s3_segment.borrow() == S3Segment::Small;
            unsafe {
                self.buckets
                    .get_mut(bucket)
                    .unwrap()
                    .cursor_mut_from_ptr(&*entry)
                    .remove();
                self.recency_index.cursor_mut_from_ptr(&*entry).remove();
            }
            self.len -= 1;
            if was_s3_small {
                self.s3_small_len = self.s3_small_len.saturating_sub(1);
            }
            return Self::entry_into_pair(entry);
        }
        None
    }

    fn evict_one_for_policy(&mut self) -> Option<(K, V)> {
        match self.eviction_policy {
            CacheEvictionPolicy::Lfu => self.evict_one(),
            CacheEvictionPolicy::S3Fifo => self.evict_s3fifo(),
        }
    }

    fn s3_small_capacity(&self) -> usize {
        if self.cap == 0 {
            0
        } else {
            (self.cap / 10).max(1)
        }
    }

    fn s3_ghost_capacity(&self) -> usize {
        self.cap.saturating_mul(2).max(1)
    }

    fn trim_s3_ghost(&mut self) {
        let ghost_capacity = self.s3_ghost_capacity();
        while self.s3_ghost.len() > ghost_capacity {
            self.s3_ghost.pop_back();
        }
    }

    fn s3_take_ghost_hit(&mut self, key: &K) -> bool {
        if let Some(index) = self.s3_ghost.iter().position(|ghost| ghost == key) {
            self.s3_ghost.remove(index);
            true
        } else {
            false
        }
    }

    fn s3_record_ghost(&mut self, key: K) {
        if self.eviction_policy != CacheEvictionPolicy::S3Fifo {
            return;
        }
        if let Some(index) = self.s3_ghost.iter().position(|ghost| ghost == &key) {
            self.s3_ghost.remove(index);
        }
        self.s3_ghost.push_front(key);
        self.trim_s3_ghost();
    }

    fn s3_oldest_small_key(&self) -> Option<K> {
        let mut candidate = None;
        for entry in self.recency_index.iter() {
            if *entry.s3_segment.borrow() == S3Segment::Small {
                candidate = Some(entry.key.clone());
            }
        }
        candidate
    }

    fn s3_oldest_main_zero_key(&self) -> Option<K> {
        let mut candidate = None;
        for entry in self.recency_index.iter() {
            if *entry.s3_segment.borrow() == S3Segment::Main && *entry.freq.borrow() == 0 {
                candidate = Some(entry.key.clone());
            }
        }
        candidate
    }

    fn s3_has_main_credit(&self) -> bool {
        self.recency_index.iter().any(|entry| {
            *entry.s3_segment.borrow() == S3Segment::Main && *entry.freq.borrow() > 0
        })
    }

    fn s3_should_reject_scan_candidate(&self, ghost_hit: bool) -> bool {
        !ghost_hit && self.s3_small_len == 0 && self.s3_has_main_credit()
    }

    fn s3_remove_victim(&mut self, key: K) -> Option<(K, V)> {
        let removed = self.remove(&key);
        if removed.is_some() {
            self.s3_record_ghost(key);
        }
        removed
    }

    fn evict_s3fifo(&mut self) -> Option<(K, V)> {
        if self.s3_small_len > self.s3_small_capacity()
            && let Some(key) = self.s3_oldest_small_key()
        {
            return self.s3_remove_victim(key);
        }

        if let Some(key) = self.s3_oldest_main_zero_key() {
            return self.s3_remove_victim(key);
        }

        if let Some(key) = self.s3_oldest_small_key() {
            return self.s3_remove_victim(key);
        }

        self.evict_one()
    }

    pub fn evict_lfu(&mut self) -> Option<(K, V)> {
        self.evict_one()
    }

    pub fn clear(&mut self) {
        self.frequency_index.clear();
        self.recency_index.clear();
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        self.len = 0;
        self.s3_small_len = 0;
        self.s3_ghost.clear();
    }

    pub fn remove<Q>(&mut self, k: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: ?Sized + Debug + Hash + Eq,
    {
        let bucket = self.bucket_for_key(k);
        let mut cursor = self.buckets.get_mut(bucket)?.front_mut();
        while let Some(entry) = cursor.get() {
            if entry.key.borrow() == k {
                let was_s3_small = *entry.s3_segment.borrow() == S3Segment::Small;
                unsafe {
                    self.frequency_index.cursor_mut_from_ptr(entry).remove();
                    self.recency_index.cursor_mut_from_ptr(entry).remove();
                }
                let removed = cursor.remove().and_then(Self::entry_into_pair);
                self.len -= 1;
                if was_s3_small {
                    self.s3_small_len = self.s3_small_len.saturating_sub(1);
                }
                return removed;
            }
            cursor.move_next();
        }
        None
    }

    pub fn get<'a, Q>(&'a mut self, k: &Q) -> Option<&'a V>
    where
        K: Borrow<Q>,
        Q: ?Sized + Debug + Hash + Eq,
    {
        let bucket = self.bucket_for_key(&k);
        let mut cursor = self.buckets.get_mut(bucket)?.front_mut();
        while let Some(entry) = cursor.get() {
            if entry.key.borrow() == k {
                // Adjust lru
                unsafe {
                    let lru_entry = self
                        .recency_index
                        .cursor_mut_from_ptr(entry)
                        .remove()
                        .unwrap();
                    self.recency_index.push_front(lru_entry);
                }

                let entry = cursor.into_ref()?;
                metrics::histogram!(self.hit).record(1.);

                self.tick = self.tick.wrapping_add(1);

                *entry.last_tick.borrow_mut() = self.tick;
                if self.eviction_policy == CacheEvictionPolicy::S3Fifo
                    && *entry.s3_segment.borrow() == S3Segment::Small
                {
                    *entry.s3_segment.borrow_mut() = S3Segment::Main;
                    self.s3_small_len = self.s3_small_len.saturating_sub(1);
                }

                // Adjust lfu
                unsafe {
                    let lfu_entry = self
                        .frequency_index
                        .cursor_mut_from_ptr(entry)
                        .remove()
                        .unwrap();
                    {
                        let mut freq = lfu_entry.freq.borrow_mut();
                        *freq = if self.eviction_policy == CacheEvictionPolicy::S3Fifo {
                            freq.saturating_add(1).min(3)
                        } else {
                            freq.saturating_add(1)
                        };
                    }
                    self.frequency_index.insert(lfu_entry);
                }

                return Some(&entry.value);
            }

            cursor.move_next();
        }
        metrics::histogram!(self.miss).record(1.);
        None
    }

    pub fn put(&mut self, k: K, v: V) {
        let _ = self.put_capturing_evictions(k, v);
    }

    pub fn put_capturing_evictions(&mut self, k: K, v: V) -> Vec<(K, V)> {
        let bucket = self.bucket_for_key(&k);
        let mut evicted = Vec::new();

        self.tick = self.tick.wrapping_add(1);
        let ghost_hit = if self.eviction_policy == CacheEvictionPolicy::S3Fifo {
            self.s3_take_ghost_hit(&k)
        } else {
            false
        };

        // Remove any prior value
        {
            let mut cursor = self
                .buckets
                .get_mut(bucket)
                .expect("valid bucket index")
                .front_mut();
            while let Some(entry) = cursor.get() {
                if entry.key == k {
                    let was_s3_small = *entry.s3_segment.borrow() == S3Segment::Small;
                    unsafe {
                        self.frequency_index.cursor_mut_from_ptr(entry).remove();
                        self.recency_index.cursor_mut_from_ptr(entry).remove();
                    }
                    if let Some(entry) = cursor.remove().and_then(Self::entry_into_pair) {
                        evicted.push(entry);
                    }
                    self.len -= 1;
                    if was_s3_small {
                        self.s3_small_len = self.s3_small_len.saturating_sub(1);
                    }
                    break;
                }
                cursor.move_next();
            }
        }

        if self.cap == 0 {
            return evicted;
        }

        if self.eviction_policy == CacheEvictionPolicy::S3Fifo
            && self.len >= self.cap
            && self.s3_should_reject_scan_candidate(ghost_hit)
        {
            return evicted;
        }

        while self.len >= self.cap {
            if let Some(entry) = self.evict_one_for_policy() {
                evicted.push(entry);
            } else {
                break;
            }
        }

        let s3_segment = if self.eviction_policy == CacheEvictionPolicy::S3Fifo && !ghost_hit {
            S3Segment::Small
        } else {
            S3Segment::Main
        };
        let initial_freq =
            if self.eviction_policy == CacheEvictionPolicy::S3Fifo && ghost_hit {
                1
            } else {
                0
            };
        let entry = Rc::new(Entry {
            key: k,
            value: v,
            freq: RefCell::new(initial_freq),
            recency_link: LinkedListLink::new(),
            frequency_link: RBTreeLink::new(),
            hash_link: LinkedListLink::new(),
            last_tick: RefCell::new(self.tick),
            s3_segment: RefCell::new(s3_segment),
        });
        if self.eviction_policy == CacheEvictionPolicy::S3Fifo && s3_segment == S3Segment::Small {
            self.s3_small_len += 1;
        }
        self.buckets[bucket].push_front(Rc::clone(&entry));
        self.frequency_index.insert(Rc::clone(&entry));
        self.recency_index.push_front(entry);
        self.len += 1;
        if self.buckets.len() < self.cap && self.len > self.buckets.len() / 2 {
            self.grow_hash();
        }
        evicted
    }
}

/// A cache using a Least-Frequently-Used eviction policy,
/// where the cache keys are u64
pub type LfuCacheU64<V> = LfuCache<u64, V, fnv::FnvBuildHasher>;

#[cfg(test)]
mod test {
    use super::*;

    #[derive(Debug)]
    #[allow(dead_code)]
    struct EntryData<'a, K, V> {
        freq: u16,
        last_tick: u32,
        key: &'a K,
        value: &'a V,
    }

    impl<'a, K, V> EntryData<'a, K, V> {
        fn new(item: &'a Entry<K, V>) -> Self {
            Self {
                freq: *item.freq.borrow(),
                last_tick: *item.last_tick.borrow(),
                key: &item.key,
                value: &item.value,
            }
        }
    }

    fn frequency_order<K, V, S>(cache: &LfuCache<K, V, S>) -> Vec<EntryData<'_, K, V>> {
        let mut entries = vec![];
        for item in cache.frequency_index.iter() {
            entries.push(EntryData::new(item));
        }
        entries
    }

    fn recency_order<K, V, S>(cache: &LfuCache<K, V, S>) -> Vec<EntryData<'_, K, V>> {
        let mut entries = vec![];
        for item in cache.recency_index.iter() {
            entries.push(EntryData::new(item));
        }
        entries
    }

    fn lfu_eviction_trace(policy: CacheEvictionPolicy) -> Vec<u64> {
        let mut cache = LfuCacheU64::with_capacity_and_policy(3, policy);
        for key in 0..3 {
            cache.put(key, key);
        }
        cache.get(&0);
        cache.get(&0);
        cache.get(&1);

        let mut evicted = Vec::new();
        for key in 3..7 {
            evicted.extend(
                cache
                    .put_capturing_evictions(key, key)
                    .into_iter()
                    .map(|(key, _)| key),
            );
        }
        evicted
    }

    fn scan_trace_hit_rate(policy: CacheEvictionPolicy, scan_len: u64) -> f64 {
        let mut cache = LfuCacheU64::with_capacity_and_policy(4, policy);
        let mut hits = 0u64;
        let mut requests = 0u64;

        for _ in 0..2 {
            for key in 0..4 {
                requests += 1;
                if cache.get(&key).is_some() {
                    hits += 1;
                } else {
                    cache.put(key, key);
                }
            }
        }

        for round in 0..8 {
            let scan_base = 10_000 + round * 1_000 + scan_len * 100;
            for offset in 0..scan_len {
                let key = scan_base + offset;
                requests += 1;
                if cache.get(&key).is_some() {
                    hits += 1;
                } else {
                    cache.put(key, key);
                }
            }
            for key in 0..4 {
                requests += 1;
                if cache.get(&key).is_some() {
                    hits += 1;
                } else {
                    cache.put(key, key);
                }
            }
        }

        hits as f64 / requests as f64
    }

    fn mann_whitney_dominance(candidate: &[f64], baseline: &[f64]) -> f64 {
        let mut wins = 0.0;
        let mut total = 0.0;
        for candidate_sample in candidate {
            for baseline_sample in baseline {
                total += 1.0;
                if candidate_sample > baseline_sample {
                    wins += 1.0;
                } else if candidate_sample == baseline_sample {
                    wins += 0.5;
                }
            }
        }
        wins / total
    }

    #[test]
    fn decay() {
        let mut cache = LfuCacheU64::with_capacity(4);
        for i in 0..4 {
            cache.put(i, i);
            for _ in 0..i * 2 {
                cache.get(&i);
            }
        }
        k9::snapshot!(
            frequency_order(&cache),
            "
[
    EntryData {
        freq: 0,
        last_tick: 1,
        key: 0,
        value: 0,
    },
    EntryData {
        freq: 2,
        last_tick: 4,
        key: 1,
        value: 1,
    },
    EntryData {
        freq: 4,
        last_tick: 9,
        key: 2,
        value: 2,
    },
    EntryData {
        freq: 6,
        last_tick: 16,
        key: 3,
        value: 3,
    },
]
"
        );

        cache.get(&1);
        cache.get(&2);
        cache.put(10, 10);

        k9::snapshot!(
            frequency_order(&cache),
            "
[
    EntryData {
        freq: 0,
        last_tick: 19,
        key: 10,
        value: 10,
    },
    EntryData {
        freq: 3,
        last_tick: 17,
        key: 1,
        value: 1,
    },
    EntryData {
        freq: 5,
        last_tick: 18,
        key: 2,
        value: 2,
    },
    EntryData {
        freq: 6,
        last_tick: 16,
        key: 3,
        value: 3,
    },
]
"
        );

        cache.get(&10);
        cache.put(11, 11);
        // bump up freq of 11 so that we can displace 1 on the next put
        cache.get(&11);
        cache.get(&11);
        cache.get(&11);
        cache.get(&11);
        k9::snapshot!(
            frequency_order(&cache),
            "
[
    EntryData {
        freq: 3,
        last_tick: 17,
        key: 1,
        value: 1,
    },
    EntryData {
        freq: 4,
        last_tick: 25,
        key: 11,
        value: 11,
    },
    EntryData {
        freq: 5,
        last_tick: 18,
        key: 2,
        value: 2,
    },
    EntryData {
        freq: 6,
        last_tick: 16,
        key: 3,
        value: 3,
    },
]
"
        );

        cache.put(12, 12);
        k9::snapshot!(
            frequency_order(&cache),
            "
[
    EntryData {
        freq: 0,
        last_tick: 26,
        key: 12,
        value: 12,
    },
    EntryData {
        freq: 4,
        last_tick: 25,
        key: 11,
        value: 11,
    },
    EntryData {
        freq: 5,
        last_tick: 18,
        key: 2,
        value: 2,
    },
    EntryData {
        freq: 6,
        last_tick: 16,
        key: 3,
        value: 3,
    },
]
"
        );

        // Ensure that we're all non-zero
        for _ in 0..5 {
            cache.get(&2);
            cache.get(&11);
            cache.get(&12);
        }

        // and bump up the ticks so that we trigger decay for 3
        for _ in 0..10 {
            cache.get(&11);
        }

        // Note that key: 3 has freq 6 in this snapshot
        k9::snapshot!(
            frequency_order(&cache),
            "
[
    EntryData {
        freq: 5,
        last_tick: 41,
        key: 12,
        value: 12,
    },
    EntryData {
        freq: 6,
        last_tick: 16,
        key: 3,
        value: 3,
    },
    EntryData {
        freq: 10,
        last_tick: 39,
        key: 2,
        value: 2,
    },
    EntryData {
        freq: 19,
        last_tick: 51,
        key: 11,
        value: 11,
    },
]
"
        );

        // trigger an eviction. This will decay key 3's freq
        // and it will be evicted, even though key 12 in
        // the snapshot above had freq 5 when key 3 had freq 6.
        cache.put(42, 42);
        k9::snapshot!(
            frequency_order(&cache),
            "
[
    EntryData {
        freq: 0,
        last_tick: 52,
        key: 42,
        value: 42,
    },
    EntryData {
        freq: 5,
        last_tick: 41,
        key: 12,
        value: 12,
    },
    EntryData {
        freq: 10,
        last_tick: 39,
        key: 2,
        value: 2,
    },
    EntryData {
        freq: 19,
        last_tick: 51,
        key: 11,
        value: 11,
    },
]
"
        );
    }

    #[test]
    fn eviction() {
        let mut cache = LfuCacheU64::with_capacity(8);
        for i in 0..8 {
            cache.put(i, i);
            for _ in 0..i {
                cache.get(&i);
            }
        }

        k9::assert_equal!(cache.len(), 8);
        cache.put(8, 8);
        k9::assert_equal!(cache.len(), 8);

        let freq = frequency_order(&cache);
        k9::assert_equal!(*freq[0].key, 8, "0 got evicted, so 8 is first");
        k9::snapshot!(
            freq,
            "
[
    EntryData {
        freq: 0,
        last_tick: 37,
        key: 8,
        value: 8,
    },
    EntryData {
        freq: 1,
        last_tick: 3,
        key: 1,
        value: 1,
    },
    EntryData {
        freq: 2,
        last_tick: 6,
        key: 2,
        value: 2,
    },
    EntryData {
        freq: 3,
        last_tick: 10,
        key: 3,
        value: 3,
    },
    EntryData {
        freq: 4,
        last_tick: 15,
        key: 4,
        value: 4,
    },
    EntryData {
        freq: 5,
        last_tick: 21,
        key: 5,
        value: 5,
    },
    EntryData {
        freq: 6,
        last_tick: 28,
        key: 6,
        value: 6,
    },
    EntryData {
        freq: 7,
        last_tick: 36,
        key: 7,
        value: 7,
    },
]
"
        );

        for i in 9..12 {
            cache.put(i, i);
            cache.get(&i);
        }
        k9::snapshot!(
            frequency_order(&cache),
            "
[
    EntryData {
        freq: 1,
        last_tick: 39,
        key: 9,
        value: 9,
    },
    EntryData {
        freq: 1,
        last_tick: 41,
        key: 10,
        value: 10,
    },
    EntryData {
        freq: 1,
        last_tick: 10,
        key: 3,
        value: 3,
    },
    EntryData {
        freq: 1,
        last_tick: 43,
        key: 11,
        value: 11,
    },
    EntryData {
        freq: 4,
        last_tick: 15,
        key: 4,
        value: 4,
    },
    EntryData {
        freq: 5,
        last_tick: 21,
        key: 5,
        value: 5,
    },
    EntryData {
        freq: 6,
        last_tick: 28,
        key: 6,
        value: 6,
    },
    EntryData {
        freq: 7,
        last_tick: 36,
        key: 7,
        value: 7,
    },
]
"
        );
    }

    #[test]
    fn basic() {
        let mut cache = LfuCacheU64::<&'static str>::with_capacity(8);
        cache.put(1, "hello");
        cache.put(2, "there");

        k9::snapshot!(
            frequency_order(&cache),
            r#"
[
    EntryData {
        freq: 0,
        last_tick: 1,
        key: 1,
        value: "hello",
    },
    EntryData {
        freq: 0,
        last_tick: 2,
        key: 2,
        value: "there",
    },
]
"#
        );

        cache.get(&1);
        cache.get(&1);
        cache.get(&1);
        cache.get(&2);

        k9::snapshot!(
            frequency_order(&cache),
            r#"
[
    EntryData {
        freq: 1,
        last_tick: 6,
        key: 2,
        value: "there",
    },
    EntryData {
        freq: 3,
        last_tick: 5,
        key: 1,
        value: "hello",
    },
]
"#
        );

        k9::snapshot!(
            recency_order(&cache),
            r#"
[
    EntryData {
        freq: 1,
        last_tick: 6,
        key: 2,
        value: "there",
    },
    EntryData {
        freq: 3,
        last_tick: 5,
        key: 1,
        value: "hello",
    },
]
"#
        );

        cache.get(&1);
        k9::snapshot!(
            recency_order(&cache),
            r#"
[
    EntryData {
        freq: 4,
        last_tick: 7,
        key: 1,
        value: "hello",
    },
    EntryData {
        freq: 1,
        last_tick: 6,
        key: 2,
        value: "there",
    },
]
"#
        );
    }

    #[test]
    fn put_capturing_evictions_returns_lfu_entry() {
        let mut cache = LfuCacheU64::<&'static str>::with_capacity(2);
        assert!(cache.put_capturing_evictions(1, "one").is_empty());
        assert!(cache.put_capturing_evictions(2, "two").is_empty());
        cache.get(&1);

        let evicted = cache.put_capturing_evictions(3, "three");

        assert_eq!(evicted, vec![(2, "two")]);
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&1).is_some());
        assert!(cache.get(&2).is_none());
        assert!(cache.get(&3).is_some());
    }

    #[test]
    fn zero_capacity_put_does_not_insert() {
        let mut cache = LfuCacheU64::<&'static str>::with_capacity(0);

        assert!(cache.put_capturing_evictions(1, "one").is_empty());

        assert_eq!(cache.len(), 0);
        assert!(cache.is_empty());
        assert!(cache.get(&1).is_none());
    }

    #[test]
    fn update_config_to_zero_evicts_all_and_keeps_cache_disabled() {
        fn zero_cap(_: &ConfigHandle) -> usize {
            0
        }

        let mut cache = LfuCacheU64::<&'static str>::with_capacity(2);
        cache.put(1, "one");
        cache.put(2, "two");
        cache.cap_func = zero_cap;

        let mut evicted = cache.update_config_capturing_evictions(&ConfigHandle::default_config());
        evicted.sort_by_key(|(key, _)| *key);

        assert_eq!(evicted, vec![(1, "one"), (2, "two")]);
        assert_eq!(cache.len(), 0);
        assert!(cache.put_capturing_evictions(3, "three").is_empty());
        assert!(cache.get(&3).is_none());
    }

    #[test]
    fn remove_returns_entry_and_updates_len() {
        let mut cache = LfuCacheU64::<&'static str>::with_capacity(2);
        cache.put(1, "one");
        cache.put(2, "two");

        assert_eq!(cache.remove(&1), Some((1, "one")));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&1).is_none());
        assert!(cache.get(&2).is_some());
    }

    #[test]
    fn default_eviction_policy_reproduces_legacy_lfu_order() {
        let default_trace = lfu_eviction_trace(CacheEvictionPolicy::default());
        let explicit_lfu_trace = lfu_eviction_trace(CacheEvictionPolicy::Lfu);

        assert_eq!(
            CacheEvictionPolicy::from_cache_eviction_value("s3fifo"),
            Some(CacheEvictionPolicy::S3Fifo)
        );
        assert_eq!(
            CacheEvictionPolicy::from_cache_eviction_value("lfu"),
            Some(CacheEvictionPolicy::Lfu)
        );
        assert_eq!(default_trace, explicit_lfu_trace);
        assert_eq!(default_trace, vec![2, 3, 4, 5]);
    }

    #[test]
    fn s3fifo_scan_trace_improves_hit_rate_at_equal_capacity() {
        let baseline: Vec<f64> = (0..12)
            .map(|_| scan_trace_hit_rate(CacheEvictionPolicy::Lfu, 12))
            .collect();
        let candidate: Vec<f64> = (0..12)
            .map(|_| scan_trace_hit_rate(CacheEvictionPolicy::S3Fifo, 12))
            .collect();

        let baseline_mean = baseline.iter().sum::<f64>() / baseline.len() as f64;
        let candidate_mean = candidate.iter().sum::<f64>() / candidate.len() as f64;
        let dominance = mann_whitney_dominance(&candidate, &baseline);

        assert!(
            candidate_mean > baseline_mean,
            "S3-FIFO should improve scan-resistant hit rate: candidate={candidate_mean}, baseline={baseline_mean}"
        );
        assert!(
            dominance > 0.95,
            "Mann-Whitney dominance should show candidate wins most pairwise trace comparisons: {dominance}"
        );
    }
}
