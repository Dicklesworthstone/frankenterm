//! Spectral fingerprinting for automatic agent classification via FFT.
//!
//! Applies frequency-domain analysis to pane output rate time series to
//! classify agent behavior patterns: polling loops (periodic peaks), burst
//! workers (broadband impulse), steady streamers (flat spectrum), and idle.
//!
//! # Pipeline
//!
//! ```text
//! output_rate[n] → Hann window → FFT → PSD → peak detection → classify
//! ```
//!
//! # Performance
//!
//! 1024-point FFT: target < 50μs.

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

// =============================================================================
// Agent Classification
// =============================================================================

/// Spectral classification of agent behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentClass {
    /// Sharp periodic peaks — polling loop, heartbeat monitor.
    Polling,
    /// Broadband impulse response — compile job, test run.
    Burst,
    /// Flat spectrum — log tailing, data pipeline.
    Steady,
    /// Near-zero spectral power — inactive pane.
    Idle,
}

impl std::fmt::Display for AgentClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Polling => write!(f, "polling"),
            Self::Burst => write!(f, "burst"),
            Self::Steady => write!(f, "steady"),
            Self::Idle => write!(f, "idle"),
        }
    }
}

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for spectral fingerprinting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpectralConfig {
    /// FFT size (must be power of 2). Default: 1024.
    pub fft_size: usize,
    /// Total PSD power below this → Idle. Default: 1e-6.
    pub idle_power_threshold: f64,
    /// Peak must be this many times above median PSD. Default: 6.0.
    pub peak_snr_threshold: f64,
    /// Maximum peaks for Polling classification. Default: 3.
    pub max_polling_peaks: usize,
    /// Minimum quality factor (Q = f/bandwidth) for a peak. Default: 5.0.
    pub min_peak_quality: f64,
    /// Spectral flatness above this → Steady (range 0-1). Default: 0.7.
    pub steady_flatness_threshold: f64,
}

impl Default for SpectralConfig {
    fn default() -> Self {
        Self {
            fft_size: 1024,
            idle_power_threshold: 1e-6,
            peak_snr_threshold: 6.0,
            max_polling_peaks: 3,
            min_peak_quality: 5.0,
            steady_flatness_threshold: 0.7,
        }
    }
}

// =============================================================================
// Spectral Fingerprint
// =============================================================================

/// Result of spectral analysis on a time series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectralFingerprint {
    /// Classified agent behavior.
    pub classification: AgentClass,
    /// Total spectral power.
    pub total_power: f64,
    /// Spectral flatness (Wiener entropy), 0-1. 1 = perfectly flat (white noise).
    pub spectral_flatness: f64,
    /// Detected spectral peaks (frequency bin, power).
    pub peaks: Vec<SpectralPeak>,
    /// Number of FFT points used.
    pub fft_size: usize,
}

/// A detected spectral peak.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectralPeak {
    /// Frequency bin index.
    pub bin: usize,
    /// Power spectral density at the peak.
    pub power: f64,
    /// Signal-to-noise ratio (peak power / median power).
    pub snr: f64,
}

// =============================================================================
// Hann Window
// =============================================================================

/// Apply a Hann window to reduce spectral leakage.
///
/// w[n] = 0.5 × (1 - cos(2πn / (N-1)))
#[must_use]
pub fn hann_window(signal: &[f64]) -> Vec<f64> {
    let n = signal.len();
    if n <= 1 {
        return signal.to_vec();
    }
    let denom = (n - 1) as f64;
    signal
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            let w = 0.5 * (1.0 - (2.0 * PI * i as f64 / denom).cos());
            x * w
        })
        .collect()
}

// =============================================================================
// Radix-2 FFT (Cooley-Tukey)
// =============================================================================

/// Complex number for FFT computation.
#[derive(Debug, Clone, Copy)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    fn new(re: f64, im: f64) -> Self {
        Self { re, im }
    }

    fn mag_sq(self) -> f64 {
        self.re.mul_add(self.re, self.im * self.im)
    }
}

impl std::ops::Add for Complex {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self::new(self.re + rhs.re, self.im + rhs.im)
    }
}

impl std::ops::Sub for Complex {
    type Output = Self;
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.re - rhs.re, self.im - rhs.im)
    }
}

impl std::ops::Mul for Complex {
    type Output = Self;
    fn mul(self, rhs: Self) -> Self {
        Self::new(
            self.re.mul_add(rhs.re, -(self.im * rhs.im)),
            self.re.mul_add(rhs.im, self.im * rhs.re),
        )
    }
}

/// In-place radix-2 decimation-in-time FFT.
///
/// Input must have power-of-2 length. Modifies `data` in-place.
fn fft_in_place(data: &mut [Complex]) {
    let n = data.len();
    if n <= 1 {
        return;
    }
    assert!(n.is_power_of_two(), "FFT size must be power of 2");

    // Bit-reversal permutation
    let mut j = 0usize;
    for i in 0..n {
        if i < j {
            data.swap(i, j);
        }
        let mut m = n >> 1;
        while m >= 1 && j >= m {
            j -= m;
            m >>= 1;
        }
        j += m;
    }

    // Butterfly computation
    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let angle = -2.0 * PI / len as f64;

        for start in (0..n).step_by(len) {
            for k in 0..half {
                let twiddle = Complex::new(
                    (angle * k as f64).cos(),
                    (angle * k as f64).sin(),
                );
                let u = data[start + k];
                let v = data[start + k + half] * twiddle;
                data[start + k] = u + v;
                data[start + k + half] = u - v;
            }
        }

        len <<= 1;
    }
}

/// Compute the power spectral density from a real-valued signal.
///
/// Returns N/2 + 1 PSD values (DC through Nyquist).
#[must_use]
pub fn power_spectral_density(signal: &[f64]) -> Vec<f64> {
    let n = signal.len();
    if n == 0 {
        return vec![];
    }

    // Pad to power of 2 if needed
    let fft_n = n.next_power_of_two();
    let mut data: Vec<Complex> = signal
        .iter()
        .map(|&x| Complex::new(x, 0.0))
        .collect();
    data.resize(fft_n, Complex::new(0.0, 0.0));

    fft_in_place(&mut data);

    // Compute PSD: |X[k]|² / N for k = 0..N/2
    let n_f64 = fft_n as f64;
    (0..=fft_n / 2)
        .map(|k| data[k].mag_sq() / n_f64)
        .collect()
}

// =============================================================================
// Spectral Analysis
// =============================================================================

/// Compute the spectral flatness (Wiener entropy) of a PSD.
///
/// SF = exp(mean(ln(S))) / mean(S)
///
/// Range [0, 1]. 1.0 = perfectly flat (white noise). 0.0 = tonal (single frequency).
#[must_use]
pub fn spectral_flatness(psd: &[f64]) -> f64 {
    if psd.is_empty() {
        return 0.0;
    }

    // Filter out zero/negative values
    let positive: Vec<f64> = psd.iter().copied().filter(|&x| x > 0.0).collect();
    if positive.is_empty() {
        return 0.0;
    }

    let n = positive.len() as f64;
    let log_mean = positive.iter().map(|x| x.ln()).sum::<f64>() / n;
    let arith_mean = positive.iter().sum::<f64>() / n;

    if arith_mean <= 0.0 {
        return 0.0;
    }

    let geometric_mean = log_mean.exp();
    (geometric_mean / arith_mean).clamp(0.0, 1.0)
}

/// Detect peaks in a PSD that exceed the noise floor.
#[must_use]
pub fn detect_peaks(psd: &[f64], snr_threshold: f64) -> Vec<SpectralPeak> {
    if psd.len() < 3 {
        return vec![];
    }

    // Compute median PSD as noise floor estimate
    let mut sorted = psd.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];

    if median <= 0.0 {
        return vec![];
    }

    let threshold = median * snr_threshold;

    // Find local maxima above threshold (skip DC bin 0)
    let mut peaks = Vec::new();
    for i in 1..psd.len() - 1 {
        if psd[i] > threshold && psd[i] >= psd[i - 1] && psd[i] >= psd[i + 1] {
            peaks.push(SpectralPeak {
                bin: i,
                power: psd[i],
                snr: psd[i] / median,
            });
        }
    }

    // Sort by power descending
    peaks.sort_by(|a, b| {
        b.power
            .partial_cmp(&a.power)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    peaks
}

/// Classify an agent from its output rate time series.
#[must_use]
pub fn classify(signal: &[f64], config: &SpectralConfig) -> SpectralFingerprint {
    // Apply Hann window
    let windowed = hann_window(signal);

    // Compute PSD
    let psd = power_spectral_density(&windowed);

    // Total power
    let total_power: f64 = psd.iter().sum();

    // Check for idle
    if total_power < config.idle_power_threshold {
        return SpectralFingerprint {
            classification: AgentClass::Idle,
            total_power,
            spectral_flatness: 0.0,
            peaks: vec![],
            fft_size: psd.len().saturating_sub(1) * 2,
        };
    }

    // Spectral flatness
    let flatness = spectral_flatness(&psd);

    // Detect peaks
    let peaks = detect_peaks(&psd, config.peak_snr_threshold);

    // Classify
    let classification = if !peaks.is_empty() && peaks.len() <= config.max_polling_peaks {
        AgentClass::Polling
    } else if flatness >= config.steady_flatness_threshold {
        AgentClass::Steady
    } else {
        AgentClass::Burst
    };

    SpectralFingerprint {
        classification,
        total_power,
        spectral_flatness: flatness,
        peaks,
        fft_size: psd.len().saturating_sub(1) * 2,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // ── FFT correctness ──────────────────────────────────────────────────

    #[test]
    fn fft_dc_signal() {
        // Constant signal → all power at DC (bin 0)
        let signal: Vec<f64> = vec![1.0; 64];
        let psd = power_spectral_density(&signal);
        assert!(psd[0] > psd[1] * 100.0, "DC should dominate");
    }

    #[test]
    fn fft_pure_sine() {
        // Sine wave at bin 8 of a 64-point FFT
        let n = 64;
        let freq_bin = 8;
        let signal: Vec<f64> = (0..n)
            .map(|i| (2.0 * PI * freq_bin as f64 * i as f64 / n as f64).sin())
            .collect();
        let psd = power_spectral_density(&signal);

        // Peak should be at or near bin 8
        let (peak_bin, _) = psd
            .iter()
            .enumerate()
            .skip(1) // skip DC
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .unwrap();
        assert_eq!(peak_bin, freq_bin, "peak should be at frequency bin {freq_bin}");
    }

    #[test]
    fn fft_empty() {
        assert!(power_spectral_density(&[]).is_empty());
    }

    #[test]
    fn fft_single_sample() {
        let psd = power_spectral_density(&[42.0]);
        assert_eq!(psd.len(), 1);
    }

    // ── Hann window ──────────────────────────────────────────────────────

    #[test]
    fn hann_window_endpoints_zero() {
        let w = hann_window(&[1.0, 1.0, 1.0, 1.0, 1.0]);
        assert!(w[0].abs() < 1e-10, "first sample should be ~0");
        assert!(w[4].abs() < 1e-10, "last sample should be ~0");
    }

    #[test]
    fn hann_window_center_one() {
        let w = hann_window(&[1.0; 5]);
        assert!((w[2] - 1.0).abs() < 1e-10, "center should be ~1");
    }

    #[test]
    fn hann_window_empty() {
        assert!(hann_window(&[]).is_empty());
    }

    // ── Spectral flatness ────────────────────────────────────────────────

    #[test]
    fn spectral_flatness_white_noise_high() {
        // Flat PSD → flatness near 1.0
        let psd = vec![1.0; 100];
        let sf = spectral_flatness(&psd);
        assert!(
            (sf - 1.0).abs() < 0.01,
            "flat PSD should have flatness ~1.0: {sf}"
        );
    }

    #[test]
    fn spectral_flatness_tonal_low() {
        // Single peak → flatness near 0
        let mut psd = vec![0.001; 100];
        psd[10] = 1000.0;
        let sf = spectral_flatness(&psd);
        assert!(sf < 0.1, "tonal PSD should have low flatness: {sf}");
    }

    #[test]
    fn spectral_flatness_empty() {
        assert_eq!(spectral_flatness(&[]), 0.0);
    }

    // ── Peak detection ───────────────────────────────────────────────────

    #[test]
    fn detect_peaks_finds_peak() {
        let mut psd = vec![1.0; 50];
        psd[10] = 100.0; // Big peak
        let peaks = detect_peaks(&psd, 6.0);
        assert!(!peaks.is_empty(), "should find at least one peak");
        assert_eq!(peaks[0].bin, 10);
    }

    #[test]
    fn detect_peaks_flat_no_peaks() {
        let psd = vec![1.0; 50];
        let peaks = detect_peaks(&psd, 6.0);
        assert!(peaks.is_empty(), "flat PSD should have no peaks");
    }

    #[test]
    fn detect_peaks_too_short() {
        assert!(detect_peaks(&[1.0, 2.0], 6.0).is_empty());
    }

    // ── Classification ───────────────────────────────────────────────────

    #[test]
    fn classify_idle() {
        let signal = vec![0.0; 1024];
        let fp = classify(&signal, &SpectralConfig::default());
        assert_eq!(fp.classification, AgentClass::Idle);
    }

    #[test]
    fn classify_polling_sine() {
        // Pure sine at known frequency → should detect as Polling
        let n = 1024;
        let freq = 32; // 32 cycles in 1024 samples
        let signal: Vec<f64> = (0..n)
            .map(|i| (2.0 * PI * freq as f64 * i as f64 / n as f64).sin())
            .collect();
        let fp = classify(&signal, &SpectralConfig::default());
        assert_eq!(
            fp.classification,
            AgentClass::Polling,
            "pure sine should be classified as Polling"
        );
        assert!(!fp.peaks.is_empty(), "should detect spectral peak");
    }

    #[test]
    fn classify_steady_white_noise() {
        // Pseudo-random → flat spectrum → Steady
        let signal: Vec<f64> = (0..1024)
            .map(|i| {
                let s = (i as u64)
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                (s >> 33) as f64 / (u32::MAX as f64) - 0.5
            })
            .collect();
        let config = SpectralConfig {
            steady_flatness_threshold: 0.5,
            ..Default::default()
        };
        let fp = classify(&signal, &config);
        assert_eq!(
            fp.classification,
            AgentClass::Steady,
            "white noise should be Steady (flatness={})",
            fp.spectral_flatness
        );
    }

    #[test]
    fn fingerprint_serde_roundtrip() {
        let fp = SpectralFingerprint {
            classification: AgentClass::Polling,
            total_power: 42.5,
            spectral_flatness: 0.15,
            peaks: vec![SpectralPeak {
                bin: 10,
                power: 100.0,
                snr: 50.0,
            }],
            fft_size: 1024,
        };
        let json = serde_json::to_string(&fp).unwrap();
        let parsed: SpectralFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.classification, AgentClass::Polling);
        assert_eq!(parsed.peaks.len(), 1);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = SpectralConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: SpectralConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.fft_size, 1024);
    }

    #[test]
    fn agent_class_display() {
        assert_eq!(format!("{}", AgentClass::Polling), "polling");
        assert_eq!(format!("{}", AgentClass::Burst), "burst");
        assert_eq!(format!("{}", AgentClass::Steady), "steady");
        assert_eq!(format!("{}", AgentClass::Idle), "idle");
    }
}
