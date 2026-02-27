//! Property-based regression tests for bounded buffer caps introduced by
//! fork hardening (ft-3kxe.1 / ft-3kxe.6).
//!
//! Verifies that every bounded data structure in frankenterm-core:
//! - Enforces its stated capacity under random inputs
//! - Evicts the correct entries (LRU / FIFO)
//! - Preserves ordering and determinism after eviction
//! - Does not lose track of recently-inserted items
//! - Produces identical output for identical input (isomorphism)

use proptest::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use frankenterm_core::events::{CooldownVerdict, DedupeVerdict, EventDeduplicator, NotificationCooldown};
use frankenterm_core::memory_pressure::MemoryPressureTier;
use frankenterm_core::patterns::{AgentType, Detection, DetectionContext, Severity};
use frankenterm_core::rate_limit_tracker::RateLimitTracker;
use frankenterm_core::pane_tiers::PaneTier;
use frankenterm_core::ring_buffer::RingBuffer;
use frankenterm_core::scrollback_eviction::{
    EvictionConfig, PaneTierSource, ScrollbackEvictor, SegmentStore,
};
use frankenterm_core::spsc_ring_buffer::channel;

// ---------------------------------------------------------------------------
// Strategy helpers
// ---------------------------------------------------------------------------

/// Generate a key string with a bounded alphabet to exercise both
/// collision and capacity-overflow paths.
fn arb_event_key() -> impl Strategy<Value = String> {
    prop::string::string_regex("[a-z0-9]{1,16}").unwrap()
}

/// Generate a sequence of unique-enough event keys that will exceed a
/// given capacity.
fn arb_keys(max_len: usize) -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(arb_event_key(), 1..=max_len)
}

// ---------------------------------------------------------------------------
// EventDeduplicator cap tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// The deduplicator never holds more entries than max_capacity.
    #[test]
    fn dedup_never_exceeds_capacity(keys in arb_keys(200)) {
        let cap = 32;
        let mut dedup = EventDeduplicator::with_config(Duration::from_secs(3600), cap);

        for key in &keys {
            let _ = dedup.check(key);
        }

        // The tracked key count must never exceed the cap.
        // We check via `.get()` — count how many keys are still tracked.
        let tracked: usize = keys.iter()
            .collect::<HashSet<_>>()
            .iter()
            .filter(|k| dedup.get(k).is_some())
            .count();
        prop_assert!(tracked <= cap, "tracked {} > cap {}", tracked, cap);
    }

    /// Duplicate detection within the window still works after eviction.
    #[test]
    fn dedup_detects_duplicates_after_eviction(
        base_keys in prop::collection::vec(arb_event_key(), 10..=50),
        extra_keys in prop::collection::vec(arb_event_key(), 10..=50),
    ) {
        let cap = 8;
        let mut dedup = EventDeduplicator::with_config(Duration::from_secs(3600), cap);

        // Fill to capacity
        for key in &base_keys {
            let _ = dedup.check(key);
        }

        // Insert new keys (causes eviction)
        for key in &extra_keys {
            let _ = dedup.check(key);
        }

        // A key that was just inserted should be detected as duplicate
        if let Some(last) = extra_keys.last() {
            let verdict = dedup.check(last);
            let is_dup = matches!(verdict, DedupeVerdict::Duplicate { .. });
            prop_assert!(is_dup, "last key should be a duplicate on re-check");
        }
    }

    /// Dedup produces deterministic results for identical input sequences.
    #[test]
    fn dedup_is_deterministic(keys in arb_keys(100)) {
        let cap = 20;

        let mut dedup1 = EventDeduplicator::with_config(Duration::from_secs(3600), cap);
        let mut dedup2 = EventDeduplicator::with_config(Duration::from_secs(3600), cap);

        let mut verdicts1 = Vec::new();
        let mut verdicts2 = Vec::new();

        for key in &keys {
            verdicts1.push(format!("{:?}", dedup1.check(key)));
        }
        for key in &keys {
            verdicts2.push(format!("{:?}", dedup2.check(key)));
        }

        // Verdict sequences must match (determinism).
        // Note: Instant::now() introduces slight timing differences in the
        // internal timestamps, but the verdict enum variant (New/Duplicate)
        // should match for identical key sequences.
        for (i, (v1, v2)) in verdicts1.iter().zip(&verdicts2).enumerate() {
            let v1_is_new = v1.starts_with("New");
            let v2_is_new = v2.starts_with("New");
            prop_assert_eq!(v1_is_new, v2_is_new, "verdict mismatch at step {}", i);
        }
    }
}

// ---------------------------------------------------------------------------
// NotificationCooldown cap tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The cooldown tracker never holds more entries than max_capacity.
    /// We verify by re-checking all unique keys after insertion: keys still
    /// in the tracker return `Suppress`, evicted keys return `Send`.
    #[test]
    fn cooldown_never_exceeds_capacity(keys in arb_keys(200)) {
        let cap = 24;
        let mut cooldown = NotificationCooldown::with_config(
            Duration::from_secs(3600), // long cooldown so nothing expires
            cap,
        );

        // Insert all keys (unique ones create new entries, duplicates suppress)
        for key in &keys {
            let _ = cooldown.check(key);
        }

        // Re-check every unique key. Keys still tracked return Suppress;
        // evicted keys return Send (treated as new).
        let unique_keys: Vec<String> = keys.iter()
            .collect::<HashSet<_>>()
            .into_iter()
            .cloned()
            .collect();
        let mut tracked_count = 0usize;
        for key in &unique_keys {
            let verdict = cooldown.check(key);
            if matches!(verdict, CooldownVerdict::Suppress { .. }) {
                tracked_count += 1;
            }
        }
        prop_assert!(
            tracked_count <= cap,
            "tracked {} > cap {}",
            tracked_count, cap
        );
    }
}

// ---------------------------------------------------------------------------
// RateLimitTracker cap tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The tracker never holds more than MAX_TRACKED_PANES (256) entries.
    #[test]
    fn rate_tracker_never_exceeds_pane_cap(
        pane_ids in prop::collection::vec(0u64..1000, 10..=300),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for (i, &pane_id) in pane_ids.iter().enumerate() {
            tracker.record_at(
                pane_id,
                AgentType::ClaudeCode,
                format!("rule_{}", i % 10),
                None,
                now + Duration::from_millis(i as u64),
            );
        }

        prop_assert!(
            tracker.tracked_pane_count() <= 256,
            "tracked {} panes > 256 cap",
            tracker.tracked_pane_count()
        );
    }

    /// Events per pane are capped at MAX_EVENTS_PER_PANE (64).
    #[test]
    fn rate_tracker_events_per_pane_capped(
        event_count in 1usize..200,
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();
        let pane_id = 42;

        for i in 0..event_count {
            tracker.record_at(
                pane_id,
                AgentType::ClaudeCode,
                format!("rule_{}", i),
                None,
                now + Duration::from_millis(i as u64),
            );
        }

        prop_assert!(
            tracker.total_event_count() <= 64,
            "total events {} > 64 cap for single pane",
            tracker.total_event_count()
        );
    }

    /// After hitting pane cap, the most recently recorded pane is always
    /// present (LRU eviction evicts oldest, not newest).
    #[test]
    fn rate_tracker_lru_preserves_recent(
        pane_ids in prop::collection::vec(0u64..2000, 260..=400),
    ) {
        let mut tracker = RateLimitTracker::new();
        let now = Instant::now();

        for (i, &pane_id) in pane_ids.iter().enumerate() {
            tracker.record_at(
                pane_id,
                AgentType::ClaudeCode,
                "r".to_string(),
                None,
                now + Duration::from_millis(i as u64),
            );
        }

        // The last pane inserted should always be present
        if let Some(&last_pane) = pane_ids.last() {
            let is_limited = tracker.is_pane_rate_limited_at(
                last_pane,
                now + Duration::from_millis(pane_ids.len() as u64),
            );
            // It should be tracked (rate-limited with a very recent event)
            prop_assert!(is_limited, "last pane {} should still be tracked", last_pane);
        }
    }
}

// ---------------------------------------------------------------------------
// RingBuffer cap tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Ring buffer never exceeds its capacity.
    #[test]
    fn ring_buffer_bounded(
        capacity in 1usize..=128,
        items in prop::collection::vec(0u32..10000, 0..=500),
    ) {
        let mut ring = RingBuffer::new(capacity);

        for &item in &items {
            let _ = ring.push(item);
        }

        prop_assert!(ring.len() <= capacity, "len {} > capacity {}", ring.len(), capacity);
    }

    /// Ring buffer eviction tracking is accurate.
    #[test]
    fn ring_buffer_eviction_tracking(
        capacity in 1usize..=64,
        items in prop::collection::vec(0u32..10000, 0..=300),
    ) {
        let mut ring = RingBuffer::new(capacity);
        let mut expected_evictions = 0u64;

        for &item in &items {
            if ring.is_full() {
                expected_evictions += 1;
            }
            let _ = ring.push(item);
        }

        prop_assert_eq!(
            ring.total_evicted(), expected_evictions,
            "eviction count mismatch"
        );
        prop_assert_eq!(
            ring.total_pushed(),
            items.len() as u64,
            "push count mismatch"
        );
    }

    /// Ring buffer preserves insertion order (most recent at back).
    #[test]
    fn ring_buffer_order_preserved(
        capacity in 2usize..=32,
        items in prop::collection::vec(0u32..10000, 1..=200),
    ) {
        let mut ring = RingBuffer::new(capacity);

        for &item in &items {
            let _ = ring.push(item);
        }

        // The last item pushed should be at `back()`
        if let Some(&last) = items.last() {
            let is_back = ring.back() == Some(&last);
            prop_assert!(is_back, "back() should be the last pushed item");
        }

        // Items in the ring should be the tail of the input, in order
        let expected_start = items.len().saturating_sub(capacity);
        let expected_tail: Vec<u32> = items[expected_start..].to_vec();
        let ring_items: Vec<u32> = ring.iter().copied().collect();
        prop_assert_eq!(ring_items, expected_tail, "ring contents should be tail of inputs");
    }
}

// ---------------------------------------------------------------------------
// SPSC ring buffer bounded channel tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// SPSC channel never delivers more items than were sent.
    #[test]
    fn spsc_no_duplication(
        capacity in 4usize..=64,
        items in prop::collection::vec(0u64..10000, 1..=500),
    ) {
        let (tx, rx) = channel::<u64>(capacity);
        let items_clone = items.clone();
        let expected_checksum: u64 = items.iter().copied().sum();

        let producer = std::thread::spawn(move || {
            for &item in &items_clone {
                let mut value = item;
                loop {
                    match tx.try_send(value) {
                        Ok(()) => break,
                        Err(v) => {
                            value = v;
                            std::hint::spin_loop();
                        }
                    }
                }
            }
            tx.close();
        });

        let mut received = 0u64;
        let mut checksum = 0u64;
        loop {
            if let Some(value) = rx.try_recv() {
                received += 1;
                checksum += value;
                continue;
            }
            if rx.is_closed() {
                // Drain any remaining items after close
                while let Some(value) = rx.try_recv() {
                    received += 1;
                    checksum += value;
                }
                break;
            }
            std::hint::spin_loop();
        }

        producer.join().expect("producer panicked");

        prop_assert_eq!(received, items.len() as u64, "item count mismatch");
        prop_assert_eq!(checksum, expected_checksum, "checksum mismatch — data corrupted");
    }
}

// ---------------------------------------------------------------------------
// DetectionContext seen-key cap tests
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// DetectionContext.seen_keys is bounded at MAX_SEEN_KEYS (1000).
    #[test]
    fn detection_context_seen_cap(
        key_count in 900usize..=1200,
    ) {
        let mut ctx = DetectionContext::new();

        for i in 0..key_count {
            let detection = Detection {
                rule_id: format!("rule_{}", i),
                agent_type: AgentType::ClaudeCode,
                event_type: "test".to_string(),
                severity: Severity::Info,
                confidence: 1.0,
                extracted: serde_json::Value::Null,
                matched_text: format!("match_{}", i),
                span: (0, 0),
            };
            let _ = ctx.mark_seen(&detection);
        }

        prop_assert!(
            ctx.seen_count() <= 1000,
            "seen count {} > 1000 cap",
            ctx.seen_count()
        );
    }
}

// ---------------------------------------------------------------------------
// ScrollbackEvictor deterministic planning
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct PropSegmentStore {
    segments: BTreeMap<u64, usize>,
}

impl SegmentStore for PropSegmentStore {
    fn count_segments(&self, pane_id: u64) -> Result<usize, String> {
        Ok(*self.segments.get(&pane_id).unwrap_or(&0))
    }

    fn delete_oldest_segments(&self, _pane_id: u64, count: usize) -> Result<usize, String> {
        Ok(count)
    }

    fn list_pane_ids(&self) -> Result<Vec<u64>, String> {
        let mut ids: Vec<u64> = self.segments.keys().copied().collect();
        ids.sort_unstable();
        Ok(ids)
    }
}

#[derive(Debug, Clone)]
struct PropTierSource {
    tiers: BTreeMap<u64, PaneTier>,
}

impl PaneTierSource for PropTierSource {
    fn tier_for(&self, pane_id: u64) -> Option<PaneTier> {
        self.tiers.get(&pane_id).copied()
    }
}

fn arb_tier() -> impl Strategy<Value = PaneTier> {
    prop_oneof![
        Just(PaneTier::Active),
        Just(PaneTier::Thinking),
        Just(PaneTier::Idle),
        Just(PaneTier::Background),
        Just(PaneTier::Dormant),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Eviction planning produces identical results for identical inputs.
    #[test]
    fn eviction_planning_deterministic(
        pane_count in 1usize..=20,
        segment_counts in prop::collection::vec(100usize..=20000, 1..=20),
        tiers in prop::collection::vec(arb_tier(), 1..=20),
    ) {
        let count = pane_count.min(segment_counts.len()).min(tiers.len());
        let segments: BTreeMap<u64, usize> = (0..count)
            .map(|i| (i as u64, segment_counts[i]))
            .collect();
        let tier_map: BTreeMap<u64, PaneTier> = (0..count)
            .map(|i| (i as u64, tiers[i]))
            .collect();

        let store = PropSegmentStore { segments };
        let tier_source = PropTierSource { tiers: tier_map };
        let config = EvictionConfig::default();

        let evictor = ScrollbackEvictor::new(config.clone(), store.clone(), tier_source.clone());

        let plan1 = evictor.plan(MemoryPressureTier::Orange)
            .expect("first plan should succeed");
        let plan2 = evictor.plan(MemoryPressureTier::Orange)
            .expect("second plan should succeed");

        let json1 = serde_json::to_string(&plan1).expect("serialize plan1");
        let json2 = serde_json::to_string(&plan2).expect("serialize plan2");
        prop_assert_eq!(json1, json2, "eviction planning must be deterministic");
    }

    /// Eviction plans respect tier priority: Dormant/Background evicted
    /// before Active/Thinking.
    #[test]
    fn eviction_respects_tier_priority(
        pane_count in 2usize..=10,
        segment_count in 500usize..=5000,
    ) {
        let mut segments = BTreeMap::new();
        let mut tiers = BTreeMap::new();

        // First pane is Active with many segments
        segments.insert(0, segment_count);
        tiers.insert(0, PaneTier::Active);

        // Rest are Dormant with many segments
        for i in 1..pane_count {
            segments.insert(i as u64, segment_count);
            tiers.insert(i as u64, PaneTier::Dormant);
        }

        let store = PropSegmentStore { segments };
        let tier_source = PropTierSource { tiers };
        let config = EvictionConfig::default();
        let evictor = ScrollbackEvictor::new(config, store, tier_source);

        let plan = evictor.plan(MemoryPressureTier::Orange)
            .expect("plan should succeed");

        if !plan.is_empty() {
            // Active pane (id=0) should have fewer segments evicted than
            // dormant panes, if any segments are evicted from active at all.
            let active_evicted = plan.targets.iter()
                .find(|t| t.pane_id == 0)
                .map(|t| t.segments_to_remove)
                .unwrap_or(0);

            for target in &plan.targets {
                if target.pane_id > 0 {
                    // Dormant panes should be evicted at least as aggressively
                    prop_assert!(
                        target.segments_to_remove >= active_evicted,
                        "dormant pane {} evicted {} < active evicted {}",
                        target.pane_id, target.segments_to_remove, active_evicted
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cross-cutting: cap enforcement under adversarial churn
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Simulate rapid register/evict churn across all capped data structures.
    /// Verify no structure exceeds its stated bound at any point.
    #[test]
    fn all_caps_hold_under_churn(
        ops in prop::collection::vec(0u8..6, 100..=500),
    ) {
        let mut dedup = EventDeduplicator::with_config(Duration::from_secs(3600), 16);
        let mut tracker = RateLimitTracker::new();
        let mut ring = RingBuffer::<u32>::new(32);
        let now = Instant::now();

        for (step, &op) in ops.iter().enumerate() {
            let key = format!("k_{}", step);
            let pane_id = step as u64;
            let ts = now + Duration::from_millis(step as u64);

            match op {
                0 | 1 => {
                    // Dedup insert
                    let _ = dedup.check(&key);
                }
                2 | 3 => {
                    // Rate tracker insert
                    tracker.record_at(
                        pane_id % 300,
                        AgentType::ClaudeCode,
                        key,
                        None,
                        ts,
                    );
                }
                4 | 5 => {
                    // Ring buffer insert
                    let _ = ring.push(step as u32);
                }
                _ => {}
            }

            // Invariant checks at every step
            let dedup_count: usize = (0..=step)
                .filter(|i| dedup.get(&format!("k_{}", i)).is_some())
                .count();
            prop_assert!(dedup_count <= 16, "dedup exceeded cap at step {}", step);
            prop_assert!(
                tracker.tracked_pane_count() <= 256,
                "tracker exceeded cap at step {}", step
            );
            prop_assert!(ring.len() <= 32, "ring exceeded cap at step {}", step);
        }
    }
}
