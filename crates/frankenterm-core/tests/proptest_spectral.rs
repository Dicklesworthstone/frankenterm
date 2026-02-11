//! Property-based tests for the spectral fingerprinting module.
//!
//! Verifies core invariants:
//! - PSD values are non-negative
//! - Hann window preserves length and zeroes endpoints
//! - Spectral flatness is in [0, 1]
//! - FFT output length is correct (N/2 + 1)
//! - Classification is deterministic
//! - Peak SNR matches definition
//!
//! Bead: wa-283h4.9

use proptest::prelude::*;

use frankenterm_core::spectral::{
    classify, detect_peaks, hann_window, power_spectral_density, spectral_flatness, AgentClass,
    SpectralConfig, SpectralFingerprint,
};

// =============================================================================
// Proptest: PSD non-negativity
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
                "PSD[{i}] = {val} must be non-negative"
            );
            prop_assert!(
                val.is_finite(),
                "PSD[{i}] = {val} must be finite"
            );
        }
    }

    // =========================================================================
    // Proptest: PSD output length
    // =========================================================================

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

    // =========================================================================
    // Proptest: Hann window preserves length
    // =========================================================================

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

    // =========================================================================
    // Proptest: Hann window endpoints near zero
    // =========================================================================

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

    // =========================================================================
    // Proptest: Spectral flatness in [0, 1]
    // =========================================================================

    #[test]
    fn flatness_bounded(
        psd in prop::collection::vec(0.001f64..1000.0, 2..256),
    ) {
        let sf = spectral_flatness(&psd);
        prop_assert!(
            (0.0..=1.0).contains(&sf),
            "Spectral flatness must be in [0,1]: {sf}"
        );
    }

    // =========================================================================
    // Proptest: Classification is deterministic
    // =========================================================================

    #[test]
    fn classification_deterministic(
        signal in prop::collection::vec(-100.0f64..100.0, 32..256),
    ) {
        let config = SpectralConfig::default();
        let fp1 = classify(&signal, &config);
        let fp2 = classify(&signal, &config);
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

    // =========================================================================
    // Proptest: Peak SNR matches definition
    // =========================================================================

    #[test]
    fn peak_snr_consistent(
        psd in prop::collection::vec(0.01f64..100.0, 10..256),
        threshold in 2.0f64..20.0,
    ) {
        let peaks = detect_peaks(&psd, threshold);

        // Compute median
        let mut sorted = psd.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];

        for peak in &peaks {
            // SNR should equal power / median
            if median > 0.0 {
                let expected_snr = peak.power / median;
                prop_assert!(
                    (peak.snr - expected_snr).abs() < 1e-10,
                    "Peak SNR {} should equal power/median {}",
                    peak.snr,
                    expected_snr
                );
            }

            // Peak power must exceed threshold × median
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

    // =========================================================================
    // Proptest: Fingerprint serde roundtrip
    // =========================================================================

    #[test]
    fn fingerprint_roundtrip(
        signal in prop::collection::vec(-100.0f64..100.0, 32..256),
    ) {
        let config = SpectralConfig::default();
        let fp = classify(&signal, &config);

        let json = serde_json::to_string(&fp).unwrap();
        let parsed: SpectralFingerprint = serde_json::from_str(&json).unwrap();

        prop_assert_eq!(parsed.classification, fp.classification);
        prop_assert_eq!(parsed.peaks.len(), fp.peaks.len());
        prop_assert!(
            (parsed.total_power - fp.total_power).abs() < 1e-10,
            "total_power roundtrip mismatch"
        );
    }

    // =========================================================================
    // Proptest: Classification covers all variants
    // =========================================================================

    #[test]
    fn classification_is_valid_variant(
        signal in prop::collection::vec(-100.0f64..100.0, 16..512),
    ) {
        let config = SpectralConfig::default();
        let fp = classify(&signal, &config);
        // Just verify it's one of the known variants (exhaustive match)
        match fp.classification {
            AgentClass::Polling | AgentClass::Burst |
            AgentClass::Steady | AgentClass::Idle => {}
        }
        prop_assert!(fp.total_power >= 0.0 || fp.total_power < 0.0 || fp.total_power.is_nan() == false);
        prop_assert!(fp.fft_size > 0 || fp.classification == AgentClass::Idle);
    }
}
