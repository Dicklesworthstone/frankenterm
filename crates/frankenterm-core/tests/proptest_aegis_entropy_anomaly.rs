//! Property-based tests for Aegis Entropy Anomaly Detection (ft-l5em3.4).
//!
//! Verifies algebraic invariants of the e-process, error density tracker,
//! and the combined anomaly detector.

use frankenterm_core::aegis_entropy_anomaly::{
    AnomalyDecision, EProcess, EntropyAnomalyConfig, EntropyAnomalyDetector, ErrorDensityTracker,
    PaneAnomalySnapshot,
};
use proptest::prelude::*;

// ── Strategies ─────────────────────────────────────────────────────────

fn arb_alpha() -> impl Strategy<Value = f64> {
    prop_oneof![Just(0.001), Just(0.01), Just(0.05), Just(0.1),]
}

fn arb_decay() -> impl Strategy<Value = f64> {
    0.5..=0.999_f64
}

fn arb_config() -> impl Strategy<Value = EntropyAnomalyConfig> {
    (
        arb_alpha(),
        64..=8192_usize, // window_bytes
        1.0..=4.0_f64,   // collapse_threshold
        1..=10_usize,    // min_collapse_streak
        0.01..=0.5_f64,  // error_density_threshold
        arb_decay(),     // e_value_decay
        1..=20_usize,    // warmup_observations
        10..=100_usize,  // density_window
    )
        .prop_map(
            |(alpha, window, threshold, streak, err_thresh, decay, warmup, density_win)| {
                EntropyAnomalyConfig {
                    alpha,
                    window_bytes: window,
                    baseline_entropy_low: threshold + 1.0,
                    baseline_entropy_high: 7.5,
                    collapse_threshold: threshold,
                    min_collapse_streak: streak,
                    signature_bloom_capacity: 256,
                    signature_bloom_fp_rate: 0.01,
                    error_density_threshold: err_thresh,
                    e_value_decay: decay,
                    max_e_value: 1e12,
                    warmup_observations: warmup,
                    density_window: density_win,
                }
            },
        )
}

fn arb_byte_chunk(max_len: usize) -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 1..=max_len)
}

fn arb_pane_id() -> impl Strategy<Value = u64> {
    1..=100_u64
}

// ── EProcess Properties ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // EP-1: E-value is always positive
    #[test]
    fn e_value_always_positive(
        alpha in arb_alpha(),
        decay in arb_decay(),
        observations in prop::collection::vec(any::<bool>(), 1..100),
    ) {
        let mut ep = EProcess::new(alpha, decay, 1e12);
        for is_collapse in observations {
            let h = if is_collapse { 0.5 } else { 5.0 };
            ep.update(h, is_collapse, 5.0, 1.0, 0.5, 0.3, 1);
            prop_assert!(ep.e_value() > 0.0, "E-value must be positive, got {}", ep.e_value());
        }
    }

    // EP-2: E-value starts at 1.0
    #[test]
    fn e_value_initial_is_one(alpha in arb_alpha(), decay in arb_decay()) {
        let ep = EProcess::new(alpha, decay, 1e12);
        prop_assert!((ep.e_value() - 1.0).abs() < 1e-10);
    }

    // EP-3: Observation count matches updates
    #[test]
    fn observation_count_matches(
        alpha in arb_alpha(),
        n in 1..200_usize,
    ) {
        let mut ep = EProcess::new(alpha, 0.95, 1e12);
        for _ in 0..n {
            ep.update(5.0, false, 5.0, 1.0, 0.5, 0.3, 3);
        }
        prop_assert_eq!(ep.n_observations(), n);
    }

    // EP-4: Reset clears all state
    #[test]
    fn reset_clears_state(
        alpha in arb_alpha(),
        observations in prop::collection::vec(any::<bool>(), 1..50),
    ) {
        let mut ep = EProcess::new(alpha, 0.95, 1e12);
        for is_collapse in observations {
            let h = if is_collapse { 0.5 } else { 5.0 };
            ep.update(h, is_collapse, 5.0, 1.0, 0.5, 0.3, 2);
        }
        ep.reset();
        prop_assert!((ep.e_value() - 1.0).abs() < 1e-10);
        prop_assert_eq!(ep.n_observations(), 0);
        prop_assert_eq!(ep.collapse_streak(), 0);
    }

    // EP-5: Clamp prevents overflow
    #[test]
    fn e_value_respects_clamp(
        max_e in 10.0..=1000.0_f64,
    ) {
        let mut ep = EProcess::new(0.001, 0.999, max_e);
        for _ in 0..500 {
            ep.update(0.1, true, 5.0, 1.0, 0.1, 0.1, 1);
        }
        prop_assert!(ep.e_value() <= max_e, "E-value {} exceeded clamp {}", ep.e_value(), max_e);
    }

    // EP-6: Normal entropy keeps e-value at 1.0 (null hypothesis)
    // The e-process decays *toward* 1.0 on non-collapse observations,
    // so starting from 1.0, consecutive non-collapses keep it at 1.0.
    #[test]
    fn normal_entropy_decays(
        decay in 0.5..=0.98_f64,
        n in 5..50_usize,
    ) {
        let mut ep = EProcess::new(0.01, decay, 1e12);
        for _ in 0..n {
            ep.update(5.0, false, 5.0, 1.0, 0.5, 0.3, 3);
        }
        // Starting from e_value=1.0 with non-collapse updates, value stays at 1.0
        // because decay only pulls away-from-1.0 values back toward 1.0
        prop_assert!((ep.e_value() - 1.0).abs() < 1e-10,
            "E-value should remain 1.0 after non-collapse from baseline, got {}", ep.e_value());
    }

    // EP-7: Streak gate prevents premature accumulation
    #[test]
    fn streak_gate_prevents_early_accumulation(
        min_streak in 2..=10_usize,
    ) {
        let mut ep = EProcess::new(0.01, 1.0, 1e12); // decay=1.0 to isolate streak effect
        // Feed fewer collapses than min_streak
        for _ in 0..(min_streak - 1) {
            ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, min_streak);
        }
        // E-value should still be 1.0 (decay=1.0, no accumulation)
        prop_assert!(
            (ep.e_value() - 1.0).abs() < 1e-10,
            "E-value should be 1.0 before streak met, got {}",
            ep.e_value()
        );
    }

    // EP-8: Collapse streak resets on normal
    #[test]
    fn collapse_streak_resets(
        streak_len in 1..20_usize,
    ) {
        let mut ep = EProcess::new(0.01, 0.95, 1e12);
        for _ in 0..streak_len {
            ep.update(0.5, true, 5.0, 1.0, 0.5, 0.3, 100);
        }
        prop_assert_eq!(ep.collapse_streak(), streak_len);
        ep.update(5.0, false, 5.0, 1.0, 0.5, 0.3, 100);
        prop_assert_eq!(ep.collapse_streak(), 0);
    }
}

// ── ErrorDensityTracker Properties ────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    // DT-1: Density is always in [0, 1]
    #[test]
    fn density_bounded(
        window_size in 5..100_usize,
        observations in prop::collection::vec(any::<bool>(), 1..200),
    ) {
        let mut tracker = ErrorDensityTracker::new(window_size);
        for obs in observations {
            tracker.record(obs);
            let d = tracker.density();
            prop_assert!((0.0..=1.0).contains(&d), "Density {} out of bounds", d);
        }
    }

    // DT-2: All-hit window gives density 1.0
    #[test]
    fn all_hits_density_one(window_size in 5..50_usize) {
        let mut tracker = ErrorDensityTracker::new(window_size);
        for _ in 0..window_size {
            tracker.record(true);
        }
        prop_assert!((tracker.density() - 1.0).abs() < 1e-10);
    }

    // DT-3: No-hit window gives density 0.0
    #[test]
    fn no_hits_density_zero(window_size in 5..50_usize) {
        let mut tracker = ErrorDensityTracker::new(window_size);
        for _ in 0..window_size {
            tracker.record(false);
        }
        prop_assert!(tracker.density().abs() < 1e-10);
    }

    // DT-4: Hit count ≤ window size
    #[test]
    fn hit_count_bounded(
        window_size in 5..50_usize,
        observations in prop::collection::vec(any::<bool>(), 1..200),
    ) {
        let mut tracker = ErrorDensityTracker::new(window_size);
        for obs in observations {
            tracker.record(obs);
            prop_assert!(
                tracker.hit_count() <= window_size,
                "Hit count {} > window size {}",
                tracker.hit_count(),
                window_size
            );
        }
    }

    // DT-5: Total observations monotonically increases
    #[test]
    fn total_observations_monotone(
        observations in prop::collection::vec(any::<bool>(), 1..100),
    ) {
        let mut tracker = ErrorDensityTracker::new(20);
        let mut prev = 0;
        for obs in observations {
            tracker.record(obs);
            prop_assert!(tracker.total_observations() > prev);
            prev = tracker.total_observations();
        }
    }

    // DT-6: Reset clears everything
    #[test]
    fn density_reset_clears(
        observations in prop::collection::vec(any::<bool>(), 1..50),
    ) {
        let mut tracker = ErrorDensityTracker::new(20);
        for obs in observations {
            tracker.record(obs);
        }
        tracker.reset();
        prop_assert!(tracker.density().abs() < 1e-10);
        prop_assert_eq!(tracker.hit_count(), 0);
        prop_assert_eq!(tracker.total_observations(), 0);
    }
}

// ── Detector Properties ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    // DET-1: Never blocks during warmup
    #[test]
    fn never_blocks_during_warmup(
        config in arb_config(),
        data in arb_byte_chunk(256),
    ) {
        let warmup = config.warmup_observations;
        let mut det = EntropyAnomalyDetector::new(config);
        for _ in 0..warmup.saturating_sub(1) {
            let decision = det.observe(1, &data, &[b"error"]);
            prop_assert!(!decision.should_block, "Must not block during warmup");
        }
    }

    // DET-2: Diverse text never triggers block
    #[test]
    fn diverse_text_never_blocks(
        config in arb_config(),
        n in 10..50_usize,
    ) {
        let mut det = EntropyAnomalyDetector::new(config);
        // Max-entropy data (all 256 byte values)
        let diverse: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        for _ in 0..n {
            let decision = det.observe(1, &diverse, &[]);
            prop_assert!(
                !decision.should_block,
                "Diverse text should never block (entropy={})",
                decision.current_entropy
            );
        }
    }

    // DET-3: Pane isolation — different panes have independent state
    #[test]
    fn pane_isolation(
        pane1 in arb_pane_id(),
        pane2 in arb_pane_id(),
        data in arb_byte_chunk(128),
    ) {
        prop_assume!(pane1 != pane2);
        let mut det = EntropyAnomalyDetector::with_defaults();
        det.observe(pane1, &data, &[]);
        det.observe(pane2, &[0u8; 128], &[]);

        let snap1 = det.pane_snapshot(pane1);
        let snap2 = det.pane_snapshot(pane2);
        prop_assert!(snap1.is_some());
        prop_assert!(snap2.is_some());
        let s1 = snap1.unwrap();
        let s2 = snap2.unwrap();
        prop_assert_eq!(s1.pane_id, pane1);
        prop_assert_eq!(s2.pane_id, pane2);
    }

    // DET-4: Pane reset removes state
    #[test]
    fn pane_reset_removes(pane_id in arb_pane_id()) {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let data: Vec<u8> = (0..128).map(|i| i as u8).collect();
        det.observe(pane_id, &data, &[]);
        prop_assert!(det.pane_snapshot(pane_id).is_some());
        det.reset_pane(pane_id);
        prop_assert!(det.pane_snapshot(pane_id).is_none());
    }

    // DET-5: Global reset clears all panes
    #[test]
    fn global_reset_clears(
        pane_ids in prop::collection::vec(arb_pane_id(), 1..10),
    ) {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let data: Vec<u8> = (0..128).map(|i| i as u8).collect();
        for &pid in &pane_ids {
            det.observe(pid, &data, &[]);
        }
        det.reset();
        prop_assert_eq!(det.pane_count(), 0);
    }

    // DET-6: Snapshot count matches pane count
    #[test]
    fn snapshot_count_matches(
        pane_ids in prop::collection::vec(1..50_u64, 1..15),
    ) {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let data: Vec<u8> = (0..128).map(|i| i as u8).collect();
        let mut unique_ids = std::collections::HashSet::new();
        for &pid in &pane_ids {
            det.observe(pid, &data, &[]);
            unique_ids.insert(pid);
        }
        prop_assert_eq!(det.all_snapshots().len(), unique_ids.len());
    }

    // DET-7: Config serde roundtrip
    #[test]
    fn config_serde_roundtrip(config in arb_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: EntropyAnomalyConfig = serde_json::from_str(&json).unwrap();
        prop_assert!((config.alpha - back.alpha).abs() < 1e-10);
        prop_assert_eq!(config.window_bytes, back.window_bytes);
        prop_assert_eq!(config.warmup_observations, back.warmup_observations);
        prop_assert_eq!(config.min_collapse_streak, back.min_collapse_streak);
    }

    // DET-8: Snapshot serde roundtrip
    #[test]
    fn snapshot_serde_roundtrip(
        pane_id in arb_pane_id(),
        e_value in 0.001..1000.0_f64,
        n_obs in 0..1000_usize,
        streak in 0..100_usize,
        entropy in 0.0..8.0_f64,
        density in 0.0..1.0_f64,
        hits in 0..100_usize,
    ) {
        let snap = PaneAnomalySnapshot {
            pane_id,
            e_value,
            n_observations: n_obs,
            collapse_streak: streak,
            last_entropy: entropy,
            error_density: density,
            error_hits: hits,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let back: PaneAnomalySnapshot = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(snap.pane_id, back.pane_id);
        prop_assert_eq!(snap.n_observations, back.n_observations);
        prop_assert_eq!(snap.error_hits, back.error_hits);
        // f64 roundtrip tolerance
        prop_assert!((snap.e_value - back.e_value).abs() < 1e-10);
    }

    // DET-9: Decision serde roundtrip
    #[test]
    fn decision_serde_roundtrip(
        should_block in any::<bool>(),
        e_value in 0.001..1000.0_f64,
        entropy in 0.0..8.0_f64,
        density in 0.0..1.0_f64,
        n_obs in 0..1000_usize,
    ) {
        let decision = AnomalyDecision {
            should_block,
            e_value,
            rejection_threshold: 100.0,
            entropy_collapsed: true,
            error_density_high: density > 0.3,
            current_entropy: entropy,
            error_density: density,
            n_observations: n_obs,
            collapse_streak: 5,
            warming_up: false,
        };
        let json = serde_json::to_string(&decision).unwrap();
        let back: AnomalyDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decision.should_block, back.should_block);
        prop_assert!((decision.e_value - back.e_value).abs() < 1e-10);
    }

    // DET-10: E-value is always positive in detector
    #[test]
    fn detector_e_value_positive(
        config in arb_config(),
        chunks in prop::collection::vec(arb_byte_chunk(64), 1..30),
    ) {
        let mut det = EntropyAnomalyDetector::new(config);
        for chunk in chunks {
            let decision = det.observe(1, &chunk, &[]);
            prop_assert!(
                decision.e_value > 0.0,
                "E-value must be positive, got {}",
                decision.e_value
            );
        }
    }

    // DET-11: Entropy is always in [0, 8]
    #[test]
    fn entropy_bounded(
        chunks in prop::collection::vec(arb_byte_chunk(128), 1..20),
    ) {
        let mut det = EntropyAnomalyDetector::with_defaults();
        for chunk in chunks {
            let decision = det.observe(1, &chunk, &[]);
            prop_assert!(
                decision.current_entropy >= 0.0 && decision.current_entropy <= 8.0,
                "Entropy {} out of bounds",
                decision.current_entropy
            );
        }
    }

    // DET-12: Error density is always in [0, 1]
    #[test]
    fn error_density_bounded(
        chunks in prop::collection::vec(arb_byte_chunk(64), 1..30),
        has_errors in prop::collection::vec(any::<bool>(), 1..30),
    ) {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let n = chunks.len().min(has_errors.len());
        for i in 0..n {
            let sigs: Vec<&[u8]> = if has_errors[i] { vec![b"error"] } else { vec![] };
            let decision = det.observe(1, &chunks[i], &sigs);
            prop_assert!(
                decision.error_density >= 0.0 && decision.error_density <= 1.0,
                "Error density {} out of bounds",
                decision.error_density
            );
        }
    }

    // DET-13: Progress bar (low entropy, no errors) never blocks
    #[test]
    fn progress_bar_never_blocks(
        byte in any::<u8>(),
        n in 10..100_usize,
    ) {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let progress = vec![byte; 4096];
        for _ in 0..n {
            let decision = det.observe(1, &progress, &[]);
            prop_assert!(
                !decision.should_block,
                "Single-byte stream should never block (no error signatures)"
            );
        }
    }

    // DET-14: Observation count increments correctly
    #[test]
    fn observation_count_correct(
        n in 1..50_usize,
    ) {
        let mut det = EntropyAnomalyDetector::with_defaults();
        let data: Vec<u8> = (0..64).map(|i| i as u8).collect();
        for i in 0..n {
            let decision = det.observe(1, &data, &[]);
            prop_assert_eq!(
                decision.n_observations,
                i + 1,
                "Observation count mismatch at step {}",
                i
            );
        }
    }
}

// ── Determinism Properties ────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    // DTRM-1: Same inputs produce same decisions
    #[test]
    fn deterministic_decisions(
        config in arb_config(),
        chunks in prop::collection::vec(arb_byte_chunk(64), 1..20),
    ) {
        let mut det1 = EntropyAnomalyDetector::new(config.clone());
        let mut det2 = EntropyAnomalyDetector::new(config);

        for chunk in &chunks {
            let d1 = det1.observe(1, chunk, &[]);
            let d2 = det2.observe(1, chunk, &[]);
            prop_assert_eq!(d1.should_block, d2.should_block);
            prop_assert!((d1.e_value - d2.e_value).abs() < 1e-10);
            prop_assert!((d1.current_entropy - d2.current_entropy).abs() < 1e-10);
            prop_assert!((d1.error_density - d2.error_density).abs() < 1e-10);
        }
    }
}
