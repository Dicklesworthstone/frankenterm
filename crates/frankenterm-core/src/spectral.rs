//! Spectral fingerprinting for automatic agent classification via FFT.
//!
//! Applies frequency-domain analysis to pane output rate time series to
//! classify agent behavior patterns: polling loops (periodic peaks), burst
//! workers (broadband impulse), steady streamers (flat spectrum), and idle.
//!
//! Different from `session_dna` which captures WHAT agents do (command
//! patterns, output types); spectral fingerprinting captures WHEN they
//! do it (temporal frequency structure via FFT).

use serde::{Deserialize, Serialize};
use std::f64::consts::PI;

/// Default FFT window size (1024 samples = ~102s at 10Hz).
pub const DEFAULT_FFT_SIZE: usize = 1024;

/// Default sampling rate in Hz.
pub const DEFAULT_SAMPLE_RATE_HZ: f64 = 10.0;

/// Spectral classification of agent behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentClass {
    /// Sharp periodic peaks -- polling loop, heartbeat monitor.
    Polling,
    /// Broadband impulse response -- compile job, test run.
    Burst,
    /// Flat spectrum -- log tailing, data pipeline.
    Steady,
    /// Near-zero spectral power -- inactive pane.
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

/// Configuration for spectral fingerprinting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SpectralConfig {
    /// FFT size (must be power of 2). Default: 1024.
    pub fft_size: usize,
    /// Total PSD power below this -> Idle. Default: 1e-6.
    pub idle_power_threshold: f64,
    /// Peak must exceed median PSD by this factor. Default: 6.0.
    pub peak_snr_threshold: f64,
    /// Maximum sharp peaks for Polling classification. Default: 3.
    pub max_polling_peaks: usize,
    /// Minimum quality factor for a sharp peak. Default: 5.0.
    pub min_peak_quality: f64,
    /// Spectral flatness above this -> Steady (0-1). Default: 0.3.
    pub steady_flatness_threshold: f64,
    /// Sampling rate in Hz. Default: 10.0.
    pub sample_rate_hz: f64,
}

impl Default for SpectralConfig {
    fn default() -> Self {
        Self {
            fft_size: DEFAULT_FFT_SIZE,
            idle_power_threshold: 1e-6,
            peak_snr_threshold: 6.0,
            max_polling_peaks: 3,
            min_peak_quality: 5.0,
            steady_flatness_threshold: 0.3,
            sample_rate_hz: DEFAULT_SAMPLE_RATE_HZ,
        }
    }
}

/// Result of spectral analysis on a time series.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectralFingerprint {
    /// Classified agent behavior.
    pub classification: AgentClass,
    /// Total spectral power.
    pub total_power: f64,
    /// Spectral flatness (Wiener entropy), 0-1. 1 = white noise.
    pub spectral_flatness: f64,
    /// Spectral centroid in Hz.
    pub centroid_hz: f64,
    /// Detected spectral peaks.
    pub peaks: Vec<SpectralPeak>,
    /// FFT size used.
    pub fft_size: usize,
}

/// A detected spectral peak.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpectralPeak {
    /// Frequency bin index.
    pub bin: usize,
    /// Frequency in Hz.
    pub frequency_hz: f64,
    /// Power spectral density at peak.
    pub power: f64,
    /// Signal-to-noise ratio (peak / median).
    pub snr: f64,
    /// Quality factor Q = f_center / bandwidth_3dB.
    pub quality_factor: f64,
}

/// Circular buffer for output rate samples.
#[derive(Debug, Clone)]
pub struct SampleBuffer {
    data: Vec<f64>,
    write_pos: usize,
    count: usize,
}

impl SampleBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: vec![0.0; capacity],
            write_pos: 0,
            count: 0,
        }
    }

    pub fn push(&mut self, value: f64) {
        let cap = self.data.len();
        self.data[self.write_pos] = value;
        self.write_pos = (self.write_pos + 1) % cap;
        if self.count < cap {
            self.count += 1;
        }
    }

    pub fn is_full(&self) -> bool {
        self.count == self.data.len()
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn to_vec(&self) -> Vec<f64> {
        let cap = self.data.len();
        let mut result = vec![0.0; cap];
        if self.count == cap {
            for i in 0..cap {
                result[i] = self.data[(self.write_pos + i) % cap];
            }
        } else {
            result[..self.count].copy_from_slice(&self.data[..self.count]);
        }
        result
    }
}

/// Stateful spectral classifier with internal sample buffer.
#[derive(Debug, Clone)]
pub struct SpectralClassifier {
    buffer: SampleBuffer,
    config: SpectralConfig,
    last_fingerprint: Option<SpectralFingerprint>,
}

impl SpectralClassifier {
    pub fn new(config: SpectralConfig) -> Self {
        let size = config.fft_size;
        Self {
            buffer: SampleBuffer::new(size),
            config,
            last_fingerprint: None,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(SpectralConfig::default())
    }
    pub fn push_sample(&mut self, value: f64) {
        self.buffer.push(value);
    }
    pub fn is_ready(&self) -> bool {
        self.buffer.is_full()
    }
    pub fn sample_count(&self) -> usize {
        self.buffer.len()
    }

    pub fn classify(&mut self) -> Option<&SpectralFingerprint> {
        if !self.is_ready() {
            return None;
        }
        let signal = self.buffer.to_vec();
        let fp = classify_signal(&signal, &self.config);
        self.last_fingerprint = Some(fp);
        self.last_fingerprint.as_ref()
    }

    pub fn last_fingerprint(&self) -> Option<&SpectralFingerprint> {
        self.last_fingerprint.as_ref()
    }

    pub fn reset(&mut self) {
        self.buffer = SampleBuffer::new(self.config.fft_size);
        self.last_fingerprint = None;
    }
}

/// Apply a Hann window to reduce spectral leakage.
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

fn fft_in_place(data: &mut [Complex]) {
    let n = data.len();
    if n <= 1 {
        return;
    }
    assert!(n.is_power_of_two(), "FFT size must be power of 2");

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

    let mut len = 2;
    while len <= n {
        let half = len / 2;
        let angle = -2.0 * PI / len as f64;
        for start in (0..n).step_by(len) {
            for k in 0..half {
                let tw = Complex::new((angle * k as f64).cos(), (angle * k as f64).sin());
                let u = data[start + k];
                let v = data[start + k + half] * tw;
                data[start + k] = u + v;
                data[start + k + half] = u - v;
            }
        }
        len <<= 1;
    }
}

/// Compute the power spectral density from a real-valued signal.
#[must_use]
pub fn power_spectral_density(signal: &[f64]) -> Vec<f64> {
    let n = signal.len();
    if n == 0 {
        return vec![];
    }
    let fft_n = n.next_power_of_two();
    let mut data: Vec<Complex> = signal.iter().map(|&x| Complex::new(x, 0.0)).collect();
    data.resize(fft_n, Complex::new(0.0, 0.0));
    fft_in_place(&mut data);
    let n_f64 = fft_n as f64;
    (0..=fft_n / 2).map(|k| data[k].mag_sq() / n_f64).collect()
}

/// Spectral flatness (Wiener entropy). Range [0, 1].
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

/// Spectral centroid in Hz.
#[must_use]
pub fn spectral_centroid(psd: &[f64], sample_rate_hz: f64) -> f64 {
    let total_power: f64 = psd.iter().sum();
    if total_power <= 0.0 {
        return 0.0;
    }
    let fft_n = (psd.len().saturating_sub(1)) * 2;
    if fft_n == 0 {
        return 0.0;
    }
    let freq_res = sample_rate_hz / fft_n as f64;
    let weighted: f64 = psd
        .iter()
        .enumerate()
        .map(|(k, &s)| k as f64 * freq_res * s)
        .sum();
    weighted / total_power
}

fn median_of(data: &[f64]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f64> = data.iter().copied().filter(|x| x.is_finite()).collect();
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mid = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

fn peak_quality_factor(psd: &[f64], peak_bin: usize, freq_res: f64) -> f64 {
    let half_power = psd[peak_bin] / 2.0;
    let mut left = peak_bin;
    while left > 0 && psd[left] > half_power {
        left -= 1;
    }
    let mut right = peak_bin;
    while right < psd.len() - 1 && psd[right] > half_power {
        right += 1;
    }
    let bw_bins = (right - left).max(1) as f64;
    let bw_hz = bw_bins * freq_res;
    let center_hz = peak_bin as f64 * freq_res;
    if bw_hz > 0.0 { center_hz / bw_hz } else { 0.0 }
}

/// Detect peaks in a PSD exceeding the noise floor.
#[must_use]
pub fn detect_peaks(psd: &[f64], snr_threshold: f64, sample_rate_hz: f64) -> Vec<SpectralPeak> {
    if psd.len() < 3 {
        return vec![];
    }
    let noise_floor = median_of(psd);
    if noise_floor <= 0.0 {
        return vec![];
    }
    let threshold = noise_floor * snr_threshold;
    let fft_n = (psd.len().saturating_sub(1)) * 2;
    let freq_res = if fft_n > 0 {
        sample_rate_hz / fft_n as f64
    } else {
        1.0
    };

    let mut peaks = Vec::new();
    for i in 1..psd.len() - 1 {
        if psd[i] > threshold && psd[i] >= psd[i - 1] && psd[i] >= psd[i + 1] {
            let q = peak_quality_factor(psd, i, freq_res);
            peaks.push(SpectralPeak {
                bin: i,
                frequency_hz: i as f64 * freq_res,
                power: psd[i],
                snr: psd[i] / noise_floor,
                quality_factor: q,
            });
        }
    }
    peaks.sort_by(|a, b| {
        b.power
            .partial_cmp(&a.power)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Cluster nearby peaks: Hann window main lobe spans ~4 bins, so peaks
    // within 8 bins of a stronger peak are side lobes, not independent peaks.
    let mut clustered = Vec::new();
    let mut used = vec![false; peaks.len()];
    for i in 0..peaks.len() {
        if used[i] {
            continue;
        }
        clustered.push(peaks[i].clone());
        // Mark nearby weaker peaks as used
        for j in (i + 1)..peaks.len() {
            if !used[j] {
                let dist = if peaks[i].bin > peaks[j].bin {
                    peaks[i].bin - peaks[j].bin
                } else {
                    peaks[j].bin - peaks[i].bin
                };
                if dist <= 16 {
                    used[j] = true;
                }
            }
        }
    }
    clustered
}

/// Classify an agent from its output rate time series.
#[must_use]
pub fn classify_signal(signal: &[f64], config: &SpectralConfig) -> SpectralFingerprint {
    let windowed = hann_window(signal);
    let psd = power_spectral_density(&windowed);
    let total_power: f64 = psd.iter().sum();

    if total_power < config.idle_power_threshold {
        return SpectralFingerprint {
            classification: AgentClass::Idle,
            total_power,
            spectral_flatness: 0.0,
            centroid_hz: 0.0,
            peaks: vec![],
            fft_size: psd.len().saturating_sub(1) * 2,
        };
    }

    let flatness = spectral_flatness(&psd);
    let centroid = spectral_centroid(&psd, config.sample_rate_hz);
    let peaks = detect_peaks(&psd, config.peak_snr_threshold, config.sample_rate_hz);

    let sharp_peaks: usize = peaks
        .iter()
        .filter(|p| p.quality_factor >= config.min_peak_quality)
        .count();

    // Flatness check first: a flat spectrum is noise/steady, even if it has
    // accidental peaks. Only non-flat signals can be Polling.
    let classification = if flatness >= config.steady_flatness_threshold {
        AgentClass::Steady
    } else if sharp_peaks > 0 && sharp_peaks <= config.max_polling_peaks {
        AgentClass::Polling
    } else {
        AgentClass::Burst
    };

    SpectralFingerprint {
        classification,
        total_power,
        spectral_flatness: flatness,
        centroid_hz: centroid,
        peaks,
        fft_size: psd.len().saturating_sub(1) * 2,
    }
}

/// Cosine similarity between two PSD vectors. Returns 0-1.
#[must_use]
pub fn psd_similarity(a: &[f64], b: &[f64]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let mag_a = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let mag_b = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if mag_a == 0.0 || mag_b == 0.0 {
        return 0.0;
    }
    (dot / (mag_a * mag_b)).clamp(0.0, 1.0)
}

fn xorshift64(state: &mut u64) -> f64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    (*state as f64) / (u64::MAX as f64)
}

/// Generate a pure sine wave at the given frequency.
pub fn generate_sine(freq_hz: f64, amplitude: f64, noise_level: f64, n: usize) -> Vec<f64> {
    let sr = DEFAULT_SAMPLE_RATE_HZ;
    let mut rng = ((freq_hz * 1e6) as u64).wrapping_add(42);
    (0..n)
        .map(|i| {
            let t = i as f64 / sr;
            amplitude * (2.0 * PI * freq_hz * t).sin()
                + (xorshift64(&mut rng) * 2.0 - 1.0) * noise_level
        })
        .collect()
}

/// Generate white noise with a given seed.
pub fn generate_white_noise(seed: u64, n: usize) -> Vec<f64> {
    let mut state = seed.wrapping_add(1);
    (0..n).map(|_| xorshift64(&mut state) * 2.0 - 1.0).collect()
}

/// Generate white noise scaled by amplitude.
pub fn generate_white_noise_scaled(amplitude: f64, n: usize) -> Vec<f64> {
    generate_white_noise(42, n)
        .iter()
        .map(|x| x * amplitude)
        .collect()
}

/// Generate an impulse train with a given period in samples.
pub fn generate_impulse_train(period: usize, amplitude: f64, n: usize) -> Vec<f64> {
    (0..n)
        .map(|i| {
            if period > 0 && i % period == 0 {
                amplitude
            } else {
                0.0
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fft_dc_signal() {
        let psd = power_spectral_density(&vec![1.0; 64]);
        assert!(psd[0] > psd[1] * 100.0);
    }

    #[test]
    fn fft_pure_sine() {
        let n = 64;
        let fb = 8;
        let signal: Vec<f64> = (0..n)
            .map(|i| (2.0 * PI * fb as f64 * i as f64 / n as f64).sin())
            .collect();
        let psd = power_spectral_density(&signal);
        let (peak, _) = psd
            .iter()
            .enumerate()
            .skip(1)
            .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap())
            .unwrap();
        assert_eq!(peak, fb);
    }

    #[test]
    fn fft_empty() {
        assert!(power_spectral_density(&[]).is_empty());
    }

    #[test]
    fn fft_single() {
        assert_eq!(power_spectral_density(&[42.0]).len(), 1);
    }

    #[test]
    fn fft_psd_non_negative() {
        for &v in power_spectral_density(&generate_sine(2.0, 5.0, 1.0, 1024)).iter() {
            assert!(v >= 0.0);
        }
    }

    #[test]
    fn hann_endpoints_zero() {
        let w = hann_window(&[1.0; 5]);
        assert!(w[0].abs() < 1e-10);
        assert!(w[4].abs() < 1e-10);
    }

    #[test]
    fn hann_center_one() {
        assert!((hann_window(&[1.0; 5])[2] - 1.0).abs() < 1e-10);
    }

    #[test]
    fn hann_empty() {
        assert!(hann_window(&[]).is_empty());
    }

    #[test]
    fn hann_symmetry() {
        let w = hann_window(&[1.0; 128]);
        for i in 0..64 {
            assert!((w[i] - w[127 - i]).abs() < 1e-10);
        }
    }

    #[test]
    fn flatness_constant_psd() {
        let sf = spectral_flatness(&vec![1.0; 100]);
        assert!((sf - 1.0).abs() < 0.01, "got {}", sf);
    }

    #[test]
    fn flatness_tonal_low() {
        let mut psd = vec![0.001; 100];
        psd[10] = 1000.0;
        assert!(spectral_flatness(&psd) < 0.1);
    }

    #[test]
    fn flatness_empty() {
        assert_eq!(spectral_flatness(&[]), 0.0);
    }
    #[test]
    fn flatness_zero() {
        assert_eq!(spectral_flatness(&[0.0; 50]), 0.0);
    }

    #[test]
    fn centroid_low() {
        let psd = power_spectral_density(&hann_window(&generate_sine(0.5, 10.0, 0.0, 1024)));
        assert!(spectral_centroid(&psd, DEFAULT_SAMPLE_RATE_HZ) < 2.0);
    }

    #[test]
    fn centroid_high() {
        let psd = power_spectral_density(&hann_window(&generate_sine(4.0, 10.0, 0.0, 1024)));
        assert!(spectral_centroid(&psd, DEFAULT_SAMPLE_RATE_HZ) > 2.0);
    }

    #[test]
    fn centroid_zero() {
        assert_eq!(spectral_centroid(&[0.0; 100], 10.0), 0.0);
    }

    #[test]
    fn peaks_finds_peak() {
        let mut psd = vec![1.0; 50];
        psd[10] = 100.0;
        let peaks = detect_peaks(&psd, 6.0, 10.0);
        assert!(!peaks.is_empty());
        assert_eq!(peaks[0].bin, 10);
        assert!(peaks[0].quality_factor > 0.0);
    }

    #[test]
    fn peaks_flat_none() {
        assert!(detect_peaks(&vec![1.0; 50], 6.0, 10.0).is_empty());
    }
    #[test]
    fn peaks_short() {
        assert!(detect_peaks(&[1.0, 2.0], 6.0, 10.0).is_empty());
    }

    #[test]
    fn quality_sharp() {
        let mut psd = vec![0.01; 100];
        psd[50] = 100.0;
        assert!(peak_quality_factor(&psd, 50, 0.1) > 1.0);
    }

    #[test]
    fn classify_idle() {
        assert_eq!(
            classify_signal(&vec![0.0; 1024], &SpectralConfig::default()).classification,
            AgentClass::Idle
        );
    }

    #[test]
    fn classify_polling() {
        let n = 1024;
        let signal: Vec<f64> = (0..n)
            .map(|i| (2.0 * PI * 32.0 * i as f64 / n as f64).sin())
            .collect();
        let fp = classify_signal(&signal, &SpectralConfig::default());
        assert_eq!(fp.classification, AgentClass::Polling);
        assert!(!fp.peaks.is_empty());
    }

    #[test]
    fn classify_steady() {
        let fp = classify_signal(
            &generate_white_noise(12345, 1024),
            &SpectralConfig::default(),
        );
        assert_eq!(
            fp.classification,
            AgentClass::Steady,
            "flatness={}",
            fp.spectral_flatness
        );
    }

    #[test]
    fn classify_burst() {
        let mut signal = vec![0.0; 1024];
        signal[100] = 100.0;
        signal[300] = 80.0;
        signal[600] = 90.0;
        let config = SpectralConfig {
            steady_flatness_threshold: 0.8,
            ..Default::default()
        };
        assert_eq!(
            classify_signal(&signal, &config).classification,
            AgentClass::Burst
        );
    }

    #[test]
    fn classify_has_centroid() {
        assert!(
            classify_signal(
                &generate_sine(2.0, 10.0, 0.1, 1024),
                &SpectralConfig::default()
            )
            .centroid_hz
                > 0.0
        );
    }

    #[test]
    fn buffer_basic() {
        let mut buf = SampleBuffer::new(4);
        assert!(buf.is_empty());
        for i in 1..=4 {
            buf.push(i as f64);
        }
        assert!(buf.is_full());
        assert_eq!(buf.to_vec(), vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn buffer_circular() {
        let mut buf = SampleBuffer::new(4);
        for i in 1..=5 {
            buf.push(i as f64);
        }
        assert_eq!(buf.to_vec(), vec![2.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn buffer_partial() {
        let mut buf = SampleBuffer::new(8);
        buf.push(1.0);
        buf.push(2.0);
        let v = buf.to_vec();
        assert_eq!((v[0], v[1], v[2]), (1.0, 2.0, 0.0));
    }

    #[test]
    fn classifier_not_ready() {
        let mut c = SpectralClassifier::with_defaults();
        for i in 0..100 {
            c.push_sample(i as f64);
        }
        assert!(!c.is_ready());
        assert!(c.classify().is_none());
    }

    #[test]
    fn classifier_ready() {
        let mut c = SpectralClassifier::with_defaults();
        for &s in &generate_sine(1.0, 50.0, 0.1, DEFAULT_FFT_SIZE) {
            c.push_sample(s);
        }
        assert!(c.is_ready());
        assert!(c.classify().is_some());
    }

    #[test]
    fn classifier_reset() {
        let mut c = SpectralClassifier::with_defaults();
        for i in 0..DEFAULT_FFT_SIZE {
            c.push_sample(i as f64);
        }
        c.reset();
        assert!(!c.is_ready());
        assert_eq!(c.sample_count(), 0);
    }

    #[test]
    fn sim_identical() {
        let psd = vec![1.0, 2.0, 3.0, 4.0];
        assert!((psd_similarity(&psd, &psd) - 1.0).abs() < 1e-10);
    }

    #[test]
    fn sim_orthogonal() {
        assert!(psd_similarity(&[1.0, 0.0, 0.0], &[0.0, 1.0, 0.0]).abs() < 1e-10);
    }

    #[test]
    fn sim_zero() {
        assert_eq!(psd_similarity(&[0.0; 10], &[0.0; 10]), 0.0);
    }
    #[test]
    fn sim_mismatch() {
        assert_eq!(psd_similarity(&[1.0, 2.0], &[1.0]), 0.0);
    }

    #[test]
    fn fingerprint_serde() {
        let fp = SpectralFingerprint {
            classification: AgentClass::Polling,
            total_power: 42.5,
            spectral_flatness: 0.15,
            centroid_hz: 2.5,
            peaks: vec![SpectralPeak {
                bin: 10,
                frequency_hz: 1.0,
                power: 100.0,
                snr: 50.0,
                quality_factor: 15.0,
            }],
            fft_size: 1024,
        };
        let json = serde_json::to_string(&fp).unwrap();
        let parsed: SpectralFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.classification, AgentClass::Polling);
        assert!((parsed.peaks[0].quality_factor - 15.0).abs() < 1e-10);
    }

    #[test]
    fn config_serde() {
        let json = serde_json::to_string(&SpectralConfig::default()).unwrap();
        assert_eq!(
            serde_json::from_str::<SpectralConfig>(&json)
                .unwrap()
                .fft_size,
            1024
        );
    }

    #[test]
    fn agent_class_display() {
        assert_eq!(format!("{}", AgentClass::Polling), "polling");
        assert_eq!(format!("{}", AgentClass::Burst), "burst");
        assert_eq!(format!("{}", AgentClass::Steady), "steady");
        assert_eq!(format!("{}", AgentClass::Idle), "idle");
    }

    #[test]
    fn impulse_harmonics() {
        let psd = power_spectral_density(&hann_window(&generate_impulse_train(100, 50.0, 1024)));
        assert!(detect_peaks(&psd, 4.0, DEFAULT_SAMPLE_RATE_HZ).len() >= 2);
    }

    #[test]
    fn gen_sine_len() {
        assert_eq!(generate_sine(1.0, 10.0, 0.0, 512).len(), 512);
    }
    #[test]
    fn gen_noise_len() {
        assert_eq!(generate_white_noise(99, 256).len(), 256);
    }

    #[test]
    fn gen_impulse_spikes() {
        let s = generate_impulse_train(10, 5.0, 30);
        assert_eq!((s[0], s[1], s[10], s[20]), (5.0, 0.0, 5.0, 5.0));
    }
}

#[cfg(test)]
mod proptest_tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn polling_classified(freq_hz in 0.5f64..4.5, amplitude in 50.0f64..200.0) {
            let signal = generate_sine(freq_hz, amplitude, 0.01, DEFAULT_FFT_SIZE);
            // Higher SNR threshold to avoid Hann window side lobes being detected as peaks
            let config = SpectralConfig {
                min_peak_quality: 3.0,
                peak_snr_threshold: 20.0,
                ..SpectralConfig::default()
            };
            let fp = classify_signal(&signal, &config);
            prop_assert_eq!(fp.classification, AgentClass::Polling,
                "freq={}Hz amp={} => {:?} flatness={} peaks={}",
                freq_hz, amplitude, fp.classification, fp.spectral_flatness, fp.peaks.len());
        }

        #[test]
        fn noise_not_polling(seed in 1u64..100000) {
            let fp = classify_signal(&generate_white_noise(seed, DEFAULT_FFT_SIZE), &SpectralConfig::default());
            prop_assert_ne!(fp.classification, AgentClass::Polling);
        }

        #[test]
        fn idle_detection(noise_level in 0.0f64..1e-4) {
            let fp = classify_signal(&generate_white_noise_scaled(noise_level, DEFAULT_FFT_SIZE), &SpectralConfig::default());
            prop_assert_eq!(fp.classification, AgentClass::Idle,
                "noise={} power={}", noise_level, fp.total_power);
        }

        #[test]
        fn flatness_bounded(values in proptest::collection::vec(-100.0f64..100.0, DEFAULT_FFT_SIZE)) {
            let psd = power_spectral_density(&hann_window(&values));
            let f = spectral_flatness(&psd);
            prop_assert!(f >= 0.0 && f <= 1.0, "flatness={}", f);
        }

        #[test]
        fn psd_non_negative(values in proptest::collection::vec(-50.0f64..50.0, DEFAULT_FFT_SIZE)) {
            let psd = power_spectral_density(&values);
            for (i, &v) in psd.iter().enumerate() {
                prop_assert!(v >= 0.0, "PSD[{}] = {}", i, v);
            }
        }
    }
}
