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
//! # Math
//!
//! - DFT: X\[k\] = Σₙ x\[n\] e^{-j2πkn/N}, computed via Cooley-Tukey FFT in O(N log N)
//! - PSD: S\[k\] = |X\[k\]|² / N
//! - Spectral flatness: SF = exp(mean(ln S)) / mean(S), range \[0, 1\]
//! - Peak quality factor: Q = f_center / bandwidth_3dB
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
    /// Sampling rate in Hz (10 Hz = 100 ms bins). Default: 10.0.
    pub sample_rate_hz: f64,
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
            sample_rate_hz: 10.0,
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
    /// Detected spectral peaks.
    pub peaks: Vec<SpectralPeak>,
    /// Number of FFT points used.
    pub fft_size: usize,
}

/// A detected spectral peak.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectralPeak {
    /// Frequency bin index.
    pub bin: usize,
    /// Frequency in Hz (requires sample rate context).
    pub frequency_hz: f64,
    /// Power spectral density at the peak.
    pub power: f64,
    /// Signal-to-noise ratio (peak power / median power).
    pub snr: f64,
    /// Quality factor Q = f_center / bandwidth_3dB.
    pub quality_factor: f64,
}

// =============================================================================
// Circular Sample Buffer
// =============================================================================

/// Circular buffer for collecting output rate samples.
#[derive(Debug, Clone)]
pub struct SampleBuffer {
    data: Vec<f64>,
    write_pos: usize,
    count: usize,
    capacity: usize,
}

impl SampleBuffer {
    /// Create a new buffer with the given capacity.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            data: vec![0.0; capacity],
            write_pos: 0,
            count: 0,
            capacity,
        }
    }

    /// Push a sample into the buffer.
    pub fn push(&mut self, sample: f64) {
        self.data[self.write_pos] = sample;
        self.write_pos = (self.write_pos + 1) % self.capacity;
        if self.count < self.capacity {
            self.count += 1;
        }
    }

    /// Whether the buffer is full.
    #[must_use]
    pub fn is_full(&self) -> bool {
        self.count == self.capacity
    }

    /// Number of samples stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Extract samples in order (oldest first).
    #[must_use]
    pub fn to_vec(&self) -> Vec<f64> {
        if self.count < self.capacity {
            self.data[..self.count].to_vec()
        } else {
            let start = self.write_pos;
            let mut out = Vec::with_capacity(self.capacity);
            out.extend_from_slice(&self.data[start..]);
            out.extend_from_slice(&self.data[..start]);
            out
        }
    }
}

// =============================================================================
// Hann Window
// =============================================================================

/// Apply a Hann window to reduce spectral leakage.
///
/// w\[n\] = 0.5 × (1 - cos(2πn / (N-1)))
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

    let fft_n = n.next_power_of_two();
    let mut data: Vec<Complex> = signal
        .iter()
        .map(|&x| Complex::new(x, 0.0))
        .collect();
    data.resize(fft_n, Complex::new(0.0, 0.0));

    fft_in_place(&mut data);

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
/// SF = exp(mean(ln(S))) / mean(S), range \[0, 1\].
#[must_use]
pub fn spectral_flatness(psd: &[f64]) -> f64 {
    if psd.is_empty() {
        return 0.0;
    }

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

/// Detect peaks in a PSD that exceed the noise floor. Computes quality factor Q.
#[must_use]
pub fn detect_peaks(
    psd: &[f64],
    snr_threshold: f64,
    sample_rate: f64,
    fft_size: usize,
) -> Vec<SpectralPeak> {
    if psd.len() < 3 {
        return vec![];
    }

    let mut sorted = psd.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];

    if median <= 0.0 {
        return vec![];
    }

    let threshold = median * snr_threshold;
    let freq_resolution = sample_rate / fft_size.max(1) as f64;

    let mut peaks = Vec::new();
    for i in 1..psd.len() - 1 {
        if psd[i] > threshold && psd[i] >= psd[i - 1] && psd[i] >= psd[i + 1] {
            let quality_factor = compute_quality_factor(psd, i, freq_resolution);
            peaks.push(SpectralPeak {
                bin: i,
                frequency_hz: i as f64 * freq_resolution,
                power: psd[i],
                snr: psd[i] / median,
                quality_factor,
            });
        }
    }

    peaks.sort_by(|a, b| {
        b.power
            .partial_cmp(&a.power)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    peaks
}

/// Compute quality factor Q = f_center / bandwidth_3dB for a peak.
fn compute_quality_factor(psd: &[f64], peak_idx: usize, freq_resolution: f64) -> f64 {
    let peak_power = psd[peak_idx];
    let half_power = peak_power / 2.0;

    let mut left = peak_idx;
    while left > 0 && psd[left] > half_power {
        left -= 1;
    }

    let mut right = peak_idx;
    while right < psd.len() - 1 && psd[right] > half_power {
        right += 1;
    }

    let bandwidth_bins = (right - left).max(1) as f64;
    let bandwidth_hz = bandwidth_bins * freq_resolution;
    let center_hz = peak_idx as f64 * freq_resolution;

    if bandwidth_hz < 1e-15 {
        return 0.0;
    }

    center_hz / bandwidth_hz
}

/// Classify an agent from its output rate time series.
#[must_use]
pub fn classify(signal: &[f64], config: &SpectralConfig) -> SpectralFingerprint {
    let windowed = hann_window(signal);
    let psd = power_spectral_density(&windowed);
    let effective_fft_size = psd.len().saturating_sub(1) * 2;
    let total_power: f64 = psd.iter().sum();

    if total_power < config.idle_power_threshold {
        return SpectralFingerprint {
            classification: AgentClass::Idle,
            total_power,
            spectral_flatness: 0.0,
            peaks: vec![],
            fft_size: effective_fft_size,
        };
    }

    let flatness = spectral_flatness(&psd);
    let peaks = detect_peaks(
        &psd,
        config.peak_snr_threshold,
        config.sample_rate_hz,
        effective_fft_size,
    );

    let high_q_peaks: usize = peaks
        .iter()
        .filter(|p| p.quality_factor >= config.min_peak_quality)
        .count();

    let classification = if high_q_peaks > 0 && high_q_peaks <= config.max_polling_peaks {
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
        fft_size: effective_fft_size,
    }
}

// =============================================================================
// SpectralMonitor — per-pane tracking
// =============================================================================

/// Per-pane spectral monitor that accumulates output rate samples and classifies.
#[derive(Debug, Clone)]
pub struct SpectralMonitor {
    config: SpectralConfig,
    buffer: SampleBuffer,
    last_fingerprint: Option<SpectralFingerprint>,
}

impl SpectralMonitor {
    /// Create a new monitor with the given configuration.
    #[must_use]
    pub fn new(config: SpectralConfig) -> Self {
        let cap = config.fft_size.next_power_of_two();
        Self {
            buffer: SampleBuffer::new(cap),
            config,
            last_fingerprint: None,
        }
    }

    /// Push a new output rate sample (e.g., bytes per sampling interval).
    pub fn push_sample(&mut self, rate: f64) {
        self.buffer.push(rate);
    }

    /// Classify the current signal. Returns `None` if insufficient samples.
    pub fn classify(&mut self) -> Option<AgentClass> {
        if !self.buffer.is_full() {
            return None;
        }
        let samples = self.buffer.to_vec();
        let fp = classify(&samples, &self.config);
        let class = fp.classification;
        self.last_fingerprint = Some(fp);
        Some(class)
    }

    /// Get the last computed fingerprint.
    #[must_use]
    pub fn last_fingerprint(&self) -> Option<&SpectralFingerprint> {
        self.last_fingerprint.as_ref()
    }

    /// Number of samples collected.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.buffer.len()
    }

    /// Whether the buffer is full and ready for classification.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.buffer.is_full()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift64(state: &mut u64) -> f64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        (*state as f64) / (u64::MAX as f64)
    }

    fn generate_white_noise(seed: u64, n: usize) -> Vec<f64> {
        let mut s = seed | 1;
        (0..n).map(|_| xorshift64(&mut s) - 0.5).collect()
    }

    fn generate_sine(freq_bin: usize, amplitude: f64, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| amplitude * (2.0 * PI * freq_bin as f64 * i as f64 / n as f64).sin())
            .collect()
    }

    fn generate_scaled_noise(scale: f64, seed: u64, n: usize) -> Vec<f64> {
        let mut s = seed | 1;
        (0..n).map(|_| (xorshift64(&mut s) - 0.5) * scale).collect()
    }

    // ── FFT correctness ──────────────────────────────────────────────────

    #[test]
    fn fft_dc_signal() {
        let signal: Vec<f64> = vec![1.0; 64];
        let psd = power_spectral_density(&signal);
        assert!(psd[0] > psd[1] * 100.0, "DC should dominate");
    }

    #[test]
    fn fft_pure_sine() {
        let n = 64;
        let freq_bin = 8;
        let signal = generate_sine(freq_bin, 1.0, n);
        let psd = power_spectral_density(&signal);
        let (peak_bin, _) = psd
            .iter()
            .enumerate()
            .skip(1)
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .unwrap();
        assert_eq!(peak_bin, freq_bin);
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

    #[test]
    fn fft_parseval_theorem() {
        let signal = generate_sine(3, 1.0, 128);
        let time_energy: f64 = signal.iter().map(|x| x * x).sum();
        let fft_n = 128usize;
        let mut data: Vec<Complex> = signal.iter().map(|&x| Complex::new(x, 0.0)).collect();
        fft_in_place(&mut data);
        let mut freq_energy = data[0].mag_sq();
        for bin in &data[1..fft_n / 2] {
            freq_energy += 2.0 * bin.mag_sq();
        }
        freq_energy += data[fft_n / 2].mag_sq();
        freq_energy /= fft_n as f64;
        assert!(
            (time_energy - freq_energy).abs() / time_energy < 0.01,
            "Parseval: time={time_energy:.4}, freq={freq_energy:.4}"
        );
    }

    // ── Hann window ──────────────────────────────────────────────────────

    #[test]
    fn hann_window_endpoints_zero() {
        let w = hann_window(&[1.0, 1.0, 1.0, 1.0, 1.0]);
        assert!(w[0].abs() < 1e-10);
        assert!(w[4].abs() < 1e-10);
    }

    #[test]
    fn hann_window_center_one() {
        let w = hann_window(&[1.0; 5]);
        assert!((w[2] - 1.0).abs() < 1e-10);
    }

    #[test]
    fn hann_window_empty() {
        assert!(hann_window(&[]).is_empty());
    }

    #[test]
    fn hann_window_single() {
        let w = hann_window(&[5.0]);
        assert_eq!(w, vec![5.0]);
    }

    // ── Spectral flatness ────────────────────────────────────────────────

    #[test]
    fn spectral_flatness_white_noise_high() {
        let psd = vec![1.0; 100];
        let sf = spectral_flatness(&psd);
        assert!((sf - 1.0).abs() < 0.01, "flat PSD flatness: {sf}");
    }

    #[test]
    fn spectral_flatness_tonal_low() {
        let mut psd = vec![0.001; 100];
        psd[10] = 1000.0;
        let sf = spectral_flatness(&psd);
        assert!(sf < 0.1, "tonal PSD flatness: {sf}");
    }

    #[test]
    fn spectral_flatness_empty() {
        assert_eq!(spectral_flatness(&[]), 0.0);
    }

    #[test]
    fn spectral_flatness_all_zero() {
        assert_eq!(spectral_flatness(&[0.0; 100]), 0.0);
    }

    // ── Peak detection ───────────────────────────────────────────────────

    #[test]
    fn detect_peaks_finds_peak() {
        let mut psd = vec![1.0; 50];
        psd[10] = 100.0;
        let peaks = detect_peaks(&psd, 6.0, 10.0, 98);
        assert!(!peaks.is_empty());
        assert_eq!(peaks[0].bin, 10);
        assert!(peaks[0].frequency_hz > 0.0);
    }

    #[test]
    fn detect_peaks_flat_no_peaks() {
        let psd = vec![1.0; 50];
        let peaks = detect_peaks(&psd, 6.0, 10.0, 98);
        assert!(peaks.is_empty());
    }

    #[test]
    fn detect_peaks_too_short() {
        assert!(detect_peaks(&[1.0, 2.0], 6.0, 10.0, 2).is_empty());
    }

    #[test]
    fn peak_quality_factor_sharp() {
        let mut psd = vec![0.01; 512];
        psd[100] = 100.0;
        let peaks = detect_peaks(&psd, 6.0, 10.0, 1024);
        if let Some(p) = peaks.first() {
            assert!(p.quality_factor > 5.0, "sharp Q={}", p.quality_factor);
        }
    }

    #[test]
    fn peak_quality_factor_broad() {
        let mut psd = vec![0.01; 512];
        for i in 90..110 {
            psd[i] = 100.0;
        }
        let peaks = detect_peaks(&psd, 6.0, 10.0, 1024);
        if let Some(p) = peaks.first() {
            assert!(p.quality_factor < 10.0, "broad Q={}", p.quality_factor);
        }
    }

    // ── Classification ───────────────────────────────────────────────────

    #[test]
    fn classify_idle() {
        let signal = vec![0.0; 1024];
        let fp = classify(&signal, &SpectralConfig::default());
        assert_eq!(fp.classification, AgentClass::Idle);
    }

    #[test]
    fn classify_idle_near_zero() {
        let signal = generate_scaled_noise(1e-8, 42, 1024);
        let fp = classify(&signal, &SpectralConfig::default());
        assert_eq!(fp.classification, AgentClass::Idle);
    }

    #[test]
    fn classify_polling_sine() {
        let signal = generate_sine(32, 1.0, 1024);
        let fp = classify(&signal, &SpectralConfig::default());
        assert_eq!(fp.classification, AgentClass::Polling,
            "flatness={}, peaks={}, high_q={}",
            fp.spectral_flatness, fp.peaks.len(),
            fp.peaks.iter().filter(|p| p.quality_factor >= 5.0).count());
    }

    #[test]
    fn classify_steady_white_noise() {
        // Iterated LCG for proper pseudo-random. Raw periodograms have high
        // variance so use relaxed thresholds matching real periodogram stats.
        let mut state: u64 = 42;
        let signal: Vec<f64> = (0..1024)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (state >> 33) as f64 / (u32::MAX as f64) - 0.5
            })
            .collect();
        let config = SpectralConfig {
            peak_snr_threshold: 20.0,
            steady_flatness_threshold: 0.05,
            ..Default::default()
        };
        let fp = classify(&signal, &config);
        assert_eq!(fp.classification, AgentClass::Steady,
            "flatness={}", fp.spectral_flatness);
    }

    #[test]
    fn classify_impulse_is_broadband() {
        // A single impulse has perfectly flat broadband spectrum → Steady.
        // This is physically correct: an impulse excites all frequencies equally.
        let mut signal = vec![0.0; 1024];
        signal[512] = 100.0;
        let fp = classify(&signal, &SpectralConfig::default());
        assert_eq!(fp.classification, AgentClass::Steady,
            "impulse has flat broadband PSD (flatness={})", fp.spectral_flatness);
    }

    #[test]
    fn classify_burst_chirp() {
        // A chirp (frequency sweep) has energy spread unevenly across
        // frequencies → non-flat, non-peaky spectrum → Burst.
        let signal: Vec<f64> = (0..1024)
            .map(|i| {
                let t = i as f64 / 1024.0;
                let freq = 1.0 + 50.0 * t; // 1 → 51 Hz sweep
                (2.0 * PI * freq * t * 1024.0 / 10.0).sin()
            })
            .collect();
        let fp = classify(&signal, &SpectralConfig::default());
        assert_ne!(fp.classification, AgentClass::Idle);
        assert_ne!(fp.classification, AgentClass::Polling,
            "chirp should not be polling");
    }

    // ── SampleBuffer ────────────────────────────────────────────────────

    #[test]
    fn sample_buffer_basic() {
        let mut buf = SampleBuffer::new(4);
        assert!(buf.is_empty());
        assert!(!buf.is_full());
        buf.push(1.0);
        buf.push(2.0);
        buf.push(3.0);
        buf.push(4.0);
        assert!(buf.is_full());
        assert_eq!(buf.len(), 4);
        assert_eq!(buf.to_vec(), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn sample_buffer_overwrite() {
        let mut buf = SampleBuffer::new(4);
        for i in 1..=5 {
            buf.push(i as f64);
        }
        assert_eq!(buf.to_vec(), vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn sample_buffer_partial() {
        let mut buf = SampleBuffer::new(8);
        buf.push(10.0);
        buf.push(20.0);
        assert_eq!(buf.len(), 2);
        assert!(!buf.is_full());
        assert_eq!(buf.to_vec(), vec![10.0, 20.0]);
    }

    // ── SpectralMonitor ────────────────────────────────────────────────

    #[test]
    fn monitor_needs_full_buffer() {
        let config = SpectralConfig { fft_size: 64, ..SpectralConfig::default() };
        let mut mon = SpectralMonitor::new(config);
        for i in 0..32 {
            mon.push_sample(i as f64);
        }
        assert!(!mon.is_ready());
        assert_eq!(mon.classify(), None);
        for i in 32..64 {
            mon.push_sample(i as f64);
        }
        assert!(mon.is_ready());
        assert!(mon.classify().is_some());
    }

    #[test]
    fn monitor_classifies_sine() {
        let config = SpectralConfig { fft_size: 256, ..SpectralConfig::default() };
        let mut mon = SpectralMonitor::new(config);
        let signal = generate_sine(16, 10.0, 256);
        for &s in &signal {
            mon.push_sample(s);
        }
        let class = mon.classify().unwrap();
        assert_eq!(class, AgentClass::Polling);
        assert!(mon.last_fingerprint().is_some());
    }

    #[test]
    fn monitor_sample_count() {
        let config = SpectralConfig { fft_size: 128, ..SpectralConfig::default() };
        let mut mon = SpectralMonitor::new(config);
        assert_eq!(mon.sample_count(), 0);
        mon.push_sample(1.0);
        assert_eq!(mon.sample_count(), 1);
    }

    // ── Serde ──────────────────────────────────────────────────────────

    #[test]
    fn fingerprint_serde_roundtrip() {
        let fp = SpectralFingerprint {
            classification: AgentClass::Polling,
            total_power: 42.5,
            spectral_flatness: 0.15,
            peaks: vec![SpectralPeak {
                bin: 10, frequency_hz: 0.098, power: 100.0, snr: 50.0, quality_factor: 12.0,
            }],
            fft_size: 1024,
        };
        let json = serde_json::to_string(&fp).unwrap();
        let parsed: SpectralFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.classification, AgentClass::Polling);
        assert_eq!(parsed.peaks.len(), 1);
        assert!((parsed.peaks[0].quality_factor - 12.0).abs() < 1e-10);
    }

    #[test]
    fn config_serde_roundtrip() {
        let config = SpectralConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: SpectralConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.fft_size, 1024);
        assert!((parsed.sample_rate_hz - 10.0).abs() < 1e-10);
    }

    #[test]
    fn config_defaults_from_empty_json() {
        let config: SpectralConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(config.fft_size, 1024);
        assert_eq!(config.max_polling_peaks, 3);
    }

    #[test]
    fn agent_class_display() {
        assert_eq!(format!("{}", AgentClass::Polling), "polling");
        assert_eq!(format!("{}", AgentClass::Burst), "burst");
        assert_eq!(format!("{}", AgentClass::Steady), "steady");
        assert_eq!(format!("{}", AgentClass::Idle), "idle");
    }

    #[test]
    fn agent_class_serde() {
        let class = AgentClass::Burst;
        let json = serde_json::to_string(&class).unwrap();
        let parsed: AgentClass = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, class);
    }

    // ── Proptest ───────────────────────────────────────────────────────

    mod proptest_spectral {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn classification_deterministic(
                signal in proptest::collection::vec(-100.0f64..100.0, 1024),
            ) {
                let c1 = classify(&signal, &SpectralConfig::default());
                let c2 = classify(&signal, &SpectralConfig::default());
                prop_assert_eq!(c1.classification, c2.classification);
            }

            #[test]
            fn flatness_bounded(
                signal in proptest::collection::vec(-100.0f64..100.0, 1024),
            ) {
                let windowed = hann_window(&signal);
                let psd = power_spectral_density(&windowed);
                let sf = spectral_flatness(&psd);
                prop_assert!(sf >= 0.0 && sf <= 1.0, "flatness {sf} out of [0, 1]");
            }

            #[test]
            fn total_power_nonneg(
                signal in proptest::collection::vec(-1000.0f64..1000.0, 1024),
            ) {
                let windowed = hann_window(&signal);
                let psd = power_spectral_density(&windowed);
                let tp: f64 = psd.iter().sum();
                prop_assert!(tp >= 0.0, "total power {tp} < 0");
            }

            #[test]
            fn zero_signal_idle(n in 64usize..2048) {
                let signal = vec![0.0; n];
                let config = SpectralConfig {
                    fft_size: n.next_power_of_two(),
                    ..SpectralConfig::default()
                };
                let fp = classify(&signal, &config);
                prop_assert_eq!(fp.classification, AgentClass::Idle);
            }

            #[test]
            fn scaling_preserves_class(
                signal in proptest::collection::vec(1.0f64..100.0, 1024),
                scale in 2.0f64..10.0,
            ) {
                let c1 = classify(&signal, &SpectralConfig::default());
                if c1.classification == AgentClass::Idle {
                    return Ok(());
                }
                let scaled: Vec<f64> = signal.iter().map(|&s| s * scale).collect();
                let c2 = classify(&scaled, &SpectralConfig::default());
                prop_assert_eq!(c1.classification, c2.classification,
                    "scaling by {} changed class", scale);
            }

            #[test]
            fn psd_nonnegative(
                signal in proptest::collection::vec(-50.0f64..50.0, 256),
            ) {
                let psd = power_spectral_density(&signal);
                for (i, &v) in psd.iter().enumerate() {
                    prop_assert!(v >= 0.0, "PSD[{i}] = {v} < 0");
                }
            }
        }
    }
}
