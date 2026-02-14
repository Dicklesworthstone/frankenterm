//! Property-based tests for the spectral fingerprinting module.
//!
//! Verifies core invariants:
//! - PSD values are non-negative and finite
//! - Hann window preserves length and zeroes endpoints
//! - Spectral flatness is in [0, 1]
//! - FFT output length is correct (N/2 + 1)
//! - Classification is deterministic
//! - Peak SNR matches definition
//! - SampleBuffer capacity/push/FIFO invariants
//! - SpectralClassifier lifecycle (push/ready/classify/reset)
//! - Serde roundtrips for all serializable types
//! - psd_similarity self-similarity = 1.0
//! - Signal generator invariants
//!
//! Bead: wa-283h4.9, wa-1u90p.7.1

use proptest::prelude::*;

use frankenterm_core::spectral::{
    AgentClass, SampleBuffer, SpectralClassifier, SpectralConfig, SpectralFingerprint,
    SpectralPeak, classify_signal, detect_peaks, generate_impulse_train, generate_sine,
    generate_white_noise, hann_window, psd_similarity, power_spectral_density,
    spectral_centroid, spectral_flatness,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_agent_class() -> impl Strategy<Value = AgentClass> {
    prop_oneof![
        Just(AgentClass::Polling),
        Just(AgentClass::Burst),
        Just(AgentClass::Steady),
        Just(AgentClass::Idle),
    ]
}

fn arb_spectral_config() -> impl Strategy<Value = SpectralConfig> {
    (
        prop_oneof![Just(64usize), Just(128), Just(256), Just(512), Just(1024)],
        1e-8f64..1e-3,      // idle_power_threshold
        2.0f64..20.0,        // peak_snr_threshold
        1usize..10,          // max_polling_peaks
        1.0f64..20.0,        // min_peak_quality
        0.1f64..0.9,         // steady_flatness_threshold
        1.0f64..100.0,       // sample_rate_hz
    )
        .prop_map(|(fft_size, ipt, pst, mpp, mpq, sft, srh)| {
            SpectralConfig {
                fft_size,
                idle_power_threshold: ipt,
                peak_snr_threshold: pst,
                max_polling_peaks: mpp,
                min_peak_quality: mpq,
                steady_flatness_threshold: sft,
                sample_rate_hz: srh,
            }
        })
}

fn arb_spectral_peak() -> impl Strategy<Value = SpectralPeak> {
    (
        0usize..512,
        0.0f64..500.0,
        0.0f64..10000.0,
        0.0f64..100.0,
        0.0f64..50.0,
    )
        .prop_map(|(bin, frequency_hz, power, snr, quality_factor)| {
            SpectralPeak {
                bin,
                frequency_hz,
                power,
                snr,
                quality_factor,
            }
        })
}

// =============================================================================
// PSD properties
// =============================================================================

proptest! {
    #[test]
    fn psd_non_negative(
        signal in prop::collection::vec(-1000.0f64..1000.0, 1..512),
    ) {
        let psd = power_spectral_density(&signal);
        for (i, &val) in psd.iter().enumerate() {
            prop_assert!(
                val >= 0.0,
                "PSD[{}] = {} must be non-negative", i, val
            );
            prop_assert!(
                val.is_finite(),
                "PSD[{}] = {} must be finite", i, val
            );
        }
    }

    #[test]
    fn psd_output_length(
        signal in prop::collection::vec(-100.0f64..100.0, 1..1024),
    ) {
        let n = signal.len();
        let fft_n = n.next_power_of_two();
        let psd = power_spectral_density(&signal);
        let expected_len = fft_n / 2 + 1;
        prop_assert_eq!(psd.len(), expected_len);
    }
}

// =============================================================================
// Hann window properties
// =============================================================================

proptest! {
    #[test]
    fn hann_preserves_length(
        signal in prop::collection::vec(-100.0f64..100.0, 0..512),
    ) {
        let windowed = hann_window(&signal);
        prop_assert_eq!(
            windowed.len(),
            signal.len(),
            "Hann window must preserve signal length"
        );
    }

    #[test]
    fn hann_endpoints_near_zero(
        signal in prop::collection::vec(1.0f64..100.0, 3..512),
    ) {
        let windowed = hann_window(&signal);
        prop_assert!(
            windowed[0].abs() < 1e-10,
            "Hann window first sample should be ~0: {}",
            windowed[0]
        );
        let last = windowed.len() - 1;
        prop_assert!(
            windowed[last].abs() < 1e-10,
            "Hann window last sample should be ~0: {}",
            windowed[last]
        );
    }
}

// =============================================================================
// Spectral flatness
// =============================================================================

proptest! {
    #[test]
    fn flatness_bounded(
        psd in prop::collection::vec(0.001f64..1000.0, 2..256),
    ) {
        let sf = spectral_flatness(&psd);
        prop_assert!(
            (0.0..=1.0).contains(&sf),
            "Spectral flatness must be in [0,1]: {}", sf
        );
    }
}

// =============================================================================
// Classification
// =============================================================================

proptest! {
    #[test]
    fn classification_deterministic(
        signal in prop::collection::vec(-100.0f64..100.0, 32..256),
    ) {
        let config = SpectralConfig::default();
        let fp1 = classify_signal(&signal, &config);
        let fp2 = classify_signal(&signal, &config);
        prop_assert_eq!(
            fp1.classification,
            fp2.classification,
            "Classification must be deterministic"
        );
        prop_assert!(
            (fp1.total_power - fp2.total_power).abs() < 1e-10,
            "Total power must be deterministic"
        );
        prop_assert!(
            (fp1.spectral_flatness - fp2.spectral_flatness).abs() < 1e-10,
            "Spectral flatness must be deterministic"
        );
    }

    #[test]
    fn classification_is_valid_variant(
        signal in prop::collection::vec(-100.0f64..100.0, 16..512),
    ) {
        let config = SpectralConfig::default();
        let fp = classify_signal(&signal, &config);
        match fp.classification {
            AgentClass::Polling | AgentClass::Burst |
            AgentClass::Steady | AgentClass::Idle => {}
        }
        prop_assert!(fp.total_power >= 0.0 || fp.total_power < 0.0 || !fp.total_power.is_nan());
        prop_assert!(fp.fft_size > 0 || fp.classification == AgentClass::Idle);
    }
}

// =============================================================================
// Peak detection
// =============================================================================

proptest! {
    #[test]
    fn peak_snr_consistent(
        psd in prop::collection::vec(0.01f64..100.0, 10..256),
        threshold in 2.0f64..20.0,
    ) {
        let peaks = detect_peaks(&psd, threshold, 10.0);

        let mut sorted = psd.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        #[allow(clippy::manual_midpoint)]
        let mid = sorted.len() / 2;
        let median = if sorted.len() % 2 == 0 {
            f64::midpoint(sorted[mid - 1], sorted[mid])
        } else {
            sorted[mid]
        };

        for peak in &peaks {
            if median > 0.0 {
                let expected_snr = peak.power / median;
                prop_assert!(
                    (peak.snr - expected_snr).abs() < 1e-10,
                    "Peak SNR {} should equal power/median {}",
                    peak.snr,
                    expected_snr
                );
            }
            prop_assert!(
                peak.power >= median * threshold,
                "Peak power {} must exceed {} (threshold={} × median={})",
                peak.power,
                median * threshold,
                threshold,
                median
            );
        }
    }
}

// =============================================================================
// Serde roundtrips
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    #[test]
    fn fingerprint_roundtrip(
        signal in prop::collection::vec(-100.0f64..100.0, 32..256),
    ) {
        let config = SpectralConfig::default();
        let fp = classify_signal(&signal, &config);

        let json = serde_json::to_string(&fp).unwrap();
        let parsed: SpectralFingerprint = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(parsed.classification, fp.classification);
        prop_assert_eq!(parsed.peaks.len(), fp.peaks.len());
        prop_assert!(
            (parsed.total_power - fp.total_power).abs() < 1e-10,
            "total_power roundtrip mismatch"
        );
    }

    /// SpectralConfig serde roundtrip preserves all fields.
    #[test]
    fn prop_config_serde_roundtrip(config in arb_spectral_config()) {
        let json = serde_json::to_string(&config).unwrap();
        let back: SpectralConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.fft_size, config.fft_size);
        prop_assert!((back.idle_power_threshold - config.idle_power_threshold).abs() < 1e-10);
        prop_assert!((back.peak_snr_threshold - config.peak_snr_threshold).abs() < 1e-10);
        prop_assert_eq!(back.max_polling_peaks, config.max_polling_peaks);
        prop_assert!((back.min_peak_quality - config.min_peak_quality).abs() < 1e-10);
        prop_assert!((back.steady_flatness_threshold - config.steady_flatness_threshold).abs() < 1e-10);
        prop_assert!((back.sample_rate_hz - config.sample_rate_hz).abs() < 1e-10);
    }

    /// AgentClass serde roundtrip preserves the variant.
    #[test]
    fn prop_agent_class_serde_roundtrip(class in arb_agent_class()) {
        let json = serde_json::to_string(&class).unwrap();
        let back: AgentClass = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, class);
    }

    /// SpectralPeak serde roundtrip preserves all fields.
    #[test]
    fn prop_peak_serde_roundtrip(peak in arb_spectral_peak()) {
        let json = serde_json::to_string(&peak).unwrap();
        let back: SpectralPeak = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.bin, peak.bin);
        prop_assert!((back.frequency_hz - peak.frequency_hz).abs() < 1e-10);
        prop_assert!((back.power - peak.power).abs() < 1e-10);
        prop_assert!((back.snr - peak.snr).abs() < 1e-10);
        prop_assert!((back.quality_factor - peak.quality_factor).abs() < 1e-10);
    }
}

// =============================================================================
// SampleBuffer properties
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(80))]

    /// SampleBuffer starts empty.
    #[test]
    fn prop_sample_buffer_starts_empty(capacity in 1usize..256) {
        let buf = SampleBuffer::new(capacity);
        prop_assert!(buf.is_empty());
        prop_assert!(!buf.is_full());
        prop_assert_eq!(buf.len(), 0);
    }

    /// Push increments len until full, then stays at capacity.
    #[test]
    fn prop_sample_buffer_push_len(
        capacity in 1usize..64,
        values in prop::collection::vec(-100.0f64..100.0, 1..200),
    ) {
        let mut buf = SampleBuffer::new(capacity);
        for (i, &v) in values.iter().enumerate() {
            buf.push(v);
            let expected_len = (i + 1).min(capacity);
            prop_assert_eq!(buf.len(), expected_len, "len mismatch at push {}", i);
        }
    }

    /// is_full becomes true exactly at capacity pushes.
    #[test]
    fn prop_sample_buffer_is_full(capacity in 1usize..64) {
        let mut buf = SampleBuffer::new(capacity);
        for i in 0..capacity {
            prop_assert!(!buf.is_full(), "should not be full at push {}", i);
            buf.push(i as f64);
        }
        prop_assert!(buf.is_full());
    }

    /// to_vec returns exactly capacity elements when full.
    #[test]
    fn prop_sample_buffer_to_vec_len(
        capacity in 1usize..64,
        extra in 0usize..100,
    ) {
        let mut buf = SampleBuffer::new(capacity);
        for i in 0..(capacity + extra) {
            buf.push(i as f64);
        }
        let vec = buf.to_vec();
        prop_assert_eq!(vec.len(), capacity,
            "to_vec should return capacity elements");
    }
}

// =============================================================================
// SpectralClassifier lifecycle
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    /// Classifier is not ready until buffer is full.
    #[test]
    fn prop_classifier_not_ready_before_full(
        fft_size in prop_oneof![Just(64usize), Just(128)],
        partial in 1usize..64,
    ) {
        let partial = partial.min(fft_size - 1);
        let config = SpectralConfig { fft_size, ..Default::default() };
        let mut clf = SpectralClassifier::new(config);
        for i in 0..partial {
            clf.push_sample(i as f64);
        }
        prop_assert!(!clf.is_ready());
        prop_assert!(clf.classify().is_none());
    }

    /// After pushing fft_size samples, classify returns Some.
    #[test]
    fn prop_classifier_ready_after_full(
        fft_size in prop_oneof![Just(64usize), Just(128)],
    ) {
        let config = SpectralConfig { fft_size, ..Default::default() };
        let mut clf = SpectralClassifier::new(config);
        for i in 0..fft_size {
            clf.push_sample(i as f64);
        }
        prop_assert!(clf.is_ready());
        let fp = clf.classify();
        prop_assert!(fp.is_some());
    }

    /// Reset clears the buffer and fingerprint.
    #[test]
    fn prop_classifier_reset(
        fft_size in prop_oneof![Just(64usize), Just(128)],
    ) {
        let config = SpectralConfig { fft_size, ..Default::default() };
        let mut clf = SpectralClassifier::new(config);
        for i in 0..fft_size {
            clf.push_sample(i as f64);
        }
        clf.classify();
        prop_assert!(clf.last_fingerprint().is_some());

        clf.reset();
        prop_assert!(!clf.is_ready());
        prop_assert_eq!(clf.sample_count(), 0);
        prop_assert!(clf.last_fingerprint().is_none());
    }
}

// =============================================================================
// psd_similarity
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(60))]

    /// Self-similarity is 1.0 for non-zero vectors.
    #[test]
    fn prop_psd_similarity_self(
        psd in prop::collection::vec(0.01f64..100.0, 2..128),
    ) {
        let sim = psd_similarity(&psd, &psd);
        prop_assert!(
            (sim - 1.0).abs() < 1e-10,
            "self-similarity should be 1.0, got {}", sim
        );
    }

    /// Similarity is in [0, 1].
    #[test]
    fn prop_psd_similarity_bounded(
        a in prop::collection::vec(0.0f64..100.0, 2..128),
    ) {
        // Generate b of same length
        let b: Vec<f64> = a.iter().map(|x| x + 1.0).collect();
        let sim = psd_similarity(&a, &b);
        prop_assert!(
            (0.0..=1.0).contains(&sim),
            "similarity should be in [0,1], got {}", sim
        );
    }

    /// Different length vectors return 0.0.
    #[test]
    fn prop_psd_similarity_different_len(
        a in prop::collection::vec(0.01f64..100.0, 2..64),
        extra in 1usize..10,
    ) {
        let b: Vec<f64> = (0..(a.len() + extra)).map(|i| i as f64 + 1.0).collect();
        let sim = psd_similarity(&a, &b);
        prop_assert!(
            sim.abs() < f64::EPSILON,
            "mismatched lengths should give 0.0, got {}", sim
        );
    }
}

// =============================================================================
// Signal generators
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(40))]

    /// generate_sine produces the requested number of samples.
    #[test]
    fn prop_sine_length(
        freq in 0.1f64..4.0,
        amp in 0.1f64..10.0,
        n in 16usize..512,
    ) {
        let signal = generate_sine(freq, amp, 0.0, n);
        prop_assert_eq!(signal.len(), n);
    }

    /// generate_white_noise produces the requested number of samples.
    #[test]
    fn prop_white_noise_length(
        seed in any::<u64>(),
        n in 1usize..512,
    ) {
        let signal = generate_white_noise(seed, n);
        prop_assert_eq!(signal.len(), n);
        // All values in [-1, 1]
        for &v in &signal {
            prop_assert!((-1.0..=1.0).contains(&v), "noise value {} out of range", v);
        }
    }

    /// generate_impulse_train produces the requested number of samples.
    #[test]
    fn prop_impulse_train_length(
        period in 1usize..50,
        amp in 0.1f64..10.0,
        n in 1usize..512,
    ) {
        let signal = generate_impulse_train(period, amp, n);
        prop_assert_eq!(signal.len(), n);
        // Non-impulse samples should be zero
        for (i, &v) in signal.iter().enumerate() {
            if i % period != 0 {
                prop_assert!(
                    v.abs() < f64::EPSILON,
                    "non-impulse sample[{}] = {} should be 0", i, v
                );
            }
        }
    }
}

// =============================================================================
// spectral_centroid
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Spectral centroid is non-negative and finite.
    #[test]
    fn prop_centroid_non_negative(
        psd in prop::collection::vec(0.001f64..100.0, 2..128),
        sample_rate in 1.0f64..100.0,
    ) {
        let c = spectral_centroid(&psd, sample_rate);
        prop_assert!(c >= 0.0, "centroid {} should be >= 0", c);
        prop_assert!(c.is_finite(), "centroid should be finite");
    }
}
