//! Compact time-series storage with window queries and aggregation.
//!
//! Stores `(timestamp_ms, f64)` pairs in a bounded ring buffer.  Provides
//! efficient range queries, statistical aggregation (min/max/mean/percentile),
//! and downsampling for multi-resolution retention.
//!
//! Useful for tracking per-pane output rates, latency percentiles, resource
//! usage over time, and correlation analysis across panes.

use std::collections::VecDeque;

use serde::{Deserialize, Serialize};

/// A single data point in the time series.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DataPoint {
    /// Timestamp in milliseconds.
    pub timestamp_ms: u64,
    /// The measured value.
    pub value: f64,
}

impl DataPoint {
    /// Create a new data point.
    #[must_use]
    pub fn new(timestamp_ms: u64, value: f64) -> Self {
        Self {
            timestamp_ms,
            value,
        }
    }
}

/// Statistical summary of a range of data points.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub count: usize,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    pub mean: f64,
    /// Start timestamp of the range.
    pub start_ms: u64,
    /// End timestamp of the range.
    pub end_ms: u64,
}

impl Stats {
    /// Compute stats from an iterator of data points.
    fn from_points(points: &[DataPoint]) -> Option<Self> {
        if points.is_empty() {
            return None;
        }
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut sum = 0.0;
        let mut start_ms = u64::MAX;
        let mut end_ms = 0u64;

        for dp in points {
            min = min.min(dp.value);
            max = max.max(dp.value);
            sum += dp.value;
            start_ms = start_ms.min(dp.timestamp_ms);
            end_ms = end_ms.max(dp.timestamp_ms);
        }

        Some(Self {
            count: points.len(),
            min,
            max,
            sum,
            mean: sum / points.len() as f64,
            start_ms,
            end_ms,
        })
    }
}

/// Configuration for a TimeSeries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeSeriesConfig {
    /// Maximum number of data points to retain.
    pub max_points: usize,
}

impl Default for TimeSeriesConfig {
    fn default() -> Self {
        Self { max_points: 10_000 }
    }
}

/// A bounded time-series store.
///
/// Data points are stored in chronological order.  When the capacity is
/// reached, the oldest points are evicted.  Timestamps should be
/// monotonically non-decreasing; out-of-order inserts are accepted but
/// placed at the end regardless.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeSeries {
    data: VecDeque<DataPoint>,
    config: TimeSeriesConfig,
    /// Running count of all points ever inserted (including evicted).
    total_inserted: u64,
    /// Running count of evicted points.
    total_evicted: u64,
}

impl TimeSeries {
    /// Create an empty time series with default config.
    #[must_use]
    pub fn new() -> Self {
        Self::with_config(TimeSeriesConfig::default())
    }

    /// Create with explicit config.
    ///
    /// # Panics
    /// Panics if `max_points == 0`.
    #[must_use]
    pub fn with_config(config: TimeSeriesConfig) -> Self {
        assert!(config.max_points > 0, "max_points must be > 0");
        Self {
            data: VecDeque::with_capacity(config.max_points.min(1024)),
            config,
            total_inserted: 0,
            total_evicted: 0,
        }
    }

    /// Push a data point.  Evicts the oldest point if at capacity.
    pub fn push(&mut self, timestamp_ms: u64, value: f64) {
        if self.data.len() >= self.config.max_points {
            self.data.pop_front();
            self.total_evicted += 1;
        }
        self.data.push_back(DataPoint::new(timestamp_ms, value));
        self.total_inserted += 1;
    }

    /// Push a pre-built data point.
    pub fn push_point(&mut self, point: DataPoint) {
        self.push(point.timestamp_ms, point.value);
    }

    /// Number of stored data points.
    #[must_use]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// True if no data points are stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Maximum capacity.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.config.max_points
    }

    /// Access config.
    #[must_use]
    pub fn config(&self) -> &TimeSeriesConfig {
        &self.config
    }

    /// Total points ever inserted.
    #[must_use]
    pub fn total_inserted(&self) -> u64 {
        self.total_inserted
    }

    /// Total points evicted.
    #[must_use]
    pub fn total_evicted(&self) -> u64 {
        self.total_evicted
    }

    /// Clear all data points.
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Get the newest data point.
    #[must_use]
    pub fn latest(&self) -> Option<&DataPoint> {
        self.data.back()
    }

    /// Get the oldest data point.
    #[must_use]
    pub fn oldest(&self) -> Option<&DataPoint> {
        self.data.front()
    }

    /// Time span of stored data in milliseconds.
    #[must_use]
    pub fn time_span_ms(&self) -> u64 {
        match (self.oldest(), self.latest()) {
            (Some(first), Some(last)) => last.timestamp_ms.saturating_sub(first.timestamp_ms),
            _ => 0,
        }
    }

    /// Query data points in the time range `[start_ms, end_ms]` (inclusive).
    pub fn range(&self, start_ms: u64, end_ms: u64) -> Vec<DataPoint> {
        self.data
            .iter()
            .filter(|dp| dp.timestamp_ms >= start_ms && dp.timestamp_ms <= end_ms)
            .copied()
            .collect()
    }

    /// Compute statistics for points in `[start_ms, end_ms]`.
    #[must_use]
    pub fn stats(&self, start_ms: u64, end_ms: u64) -> Option<Stats> {
        let points = self.range(start_ms, end_ms);
        Stats::from_points(&points)
    }

    /// Compute statistics over all stored points.
    #[must_use]
    pub fn stats_all(&self) -> Option<Stats> {
        let points: Vec<DataPoint> = self.data.iter().copied().collect();
        Stats::from_points(&points)
    }

    /// Compute the value at a given percentile (0.0 to 1.0) over all points.
    #[must_use]
    pub fn percentile(&self, p: f64) -> Option<f64> {
        self.percentile_range(p, 0, u64::MAX)
    }

    /// Compute percentile over points in `[start_ms, end_ms]`.
    #[must_use]
    pub fn percentile_range(&self, p: f64, start_ms: u64, end_ms: u64) -> Option<f64> {
        let mut values: Vec<f64> = self
            .range(start_ms, end_ms)
            .iter()
            .map(|dp| dp.value)
            .collect();
        if values.is_empty() {
            return None;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p = p.clamp(0.0, 1.0);
        let idx = ((values.len() - 1) as f64 * p) as usize;
        Some(values[idx])
    }

    /// Downsample to at most `target_points` by averaging adjacent groups.
    #[must_use]
    pub fn downsample(&self, target_points: usize) -> TimeSeries {
        if target_points == 0 || self.data.is_empty() {
            return TimeSeries::with_config(TimeSeriesConfig {
                max_points: target_points.max(1),
            });
        }

        let n = self.data.len();
        if n <= target_points {
            return self.clone();
        }

        let mut result = TimeSeries::with_config(TimeSeriesConfig {
            max_points: target_points,
        });

        let points: Vec<DataPoint> = self.data.iter().copied().collect();
        let step = n as f64 / target_points as f64;

        for i in 0..target_points {
            let start = (i as f64 * step) as usize;
            let end = ((i + 1) as f64 * step).round() as usize;
            let end = end.min(n);
            let chunk = &points[start..end];
            if chunk.is_empty() {
                continue;
            }
            let avg_ts = chunk.iter().map(|dp| dp.timestamp_ms).sum::<u64>() / chunk.len() as u64;
            let avg_val = chunk.iter().map(|dp| dp.value).sum::<f64>() / chunk.len() as f64;
            result.push(avg_ts, avg_val);
        }
        result
    }

    /// Merge another time series into this one, maintaining chronological order.
    /// Points are interleaved by timestamp. If capacity is exceeded, oldest are evicted.
    pub fn merge(&mut self, other: &TimeSeries) {
        let mut all: Vec<DataPoint> = self.data.iter().copied().collect();
        all.extend(other.data.iter().copied());
        all.sort_by_key(|dp| dp.timestamp_ms);

        self.data.clear();
        let start = if all.len() > self.config.max_points {
            all.len() - self.config.max_points
        } else {
            0
        };
        for dp in &all[start..] {
            self.data.push_back(*dp);
        }
        self.total_inserted += other.len() as u64;
        self.total_evicted += start as u64;
    }

    /// Iterate over all stored data points in chronological order.
    pub fn iter(&self) -> impl Iterator<Item = &DataPoint> {
        self.data.iter()
    }

    /// Collect all points into a Vec.
    #[must_use]
    pub fn to_vec(&self) -> Vec<DataPoint> {
        self.data.iter().copied().collect()
    }

    /// Rate of change (values per second) over the last `window_ms` milliseconds.
    #[must_use]
    pub fn rate(&self, now_ms: u64, window_ms: u64) -> f64 {
        if window_ms == 0 {
            return 0.0;
        }
        let start = now_ms.saturating_sub(window_ms);
        let count = self
            .data
            .iter()
            .filter(|dp| dp.timestamp_ms >= start && dp.timestamp_ms <= now_ms)
            .count();
        count as f64 / (window_ms as f64 / 1000.0)
    }
}

impl Default for TimeSeries {
    fn default() -> Self {
        Self::new()
    }
}

impl PartialEq for TimeSeries {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data && self.config == other.config
    }
}

impl Eq for TimeSeries {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_empty() {
        let ts = TimeSeries::new();
        assert!(ts.is_empty());
        assert_eq!(ts.len(), 0);
    }

    #[test]
    fn push_and_len() {
        let mut ts = TimeSeries::new();
        ts.push(100, 1.0);
        ts.push(200, 2.0);
        assert_eq!(ts.len(), 2);
        assert!(!ts.is_empty());
    }

    #[test]
    fn push_point() {
        let mut ts = TimeSeries::new();
        ts.push_point(DataPoint::new(100, 3.14));
        assert_eq!(ts.len(), 1);
        assert!((ts.latest().unwrap().value - 3.14).abs() < f64::EPSILON);
    }

    #[test]
    fn capacity_eviction() {
        let mut ts = TimeSeries::with_config(TimeSeriesConfig { max_points: 3 });
        ts.push(1, 1.0);
        ts.push(2, 2.0);
        ts.push(3, 3.0);
        ts.push(4, 4.0);
        assert_eq!(ts.len(), 3);
        assert_eq!(ts.oldest().unwrap().timestamp_ms, 2);
        assert_eq!(ts.latest().unwrap().timestamp_ms, 4);
        assert_eq!(ts.total_evicted(), 1);
        assert_eq!(ts.total_inserted(), 4);
    }

    #[test]
    fn latest_and_oldest() {
        let mut ts = TimeSeries::new();
        assert!(ts.latest().is_none());
        assert!(ts.oldest().is_none());
        ts.push(10, 1.0);
        ts.push(20, 2.0);
        assert_eq!(ts.oldest().unwrap().timestamp_ms, 10);
        assert_eq!(ts.latest().unwrap().timestamp_ms, 20);
    }

    #[test]
    fn time_span() {
        let mut ts = TimeSeries::new();
        assert_eq!(ts.time_span_ms(), 0);
        ts.push(100, 1.0);
        ts.push(500, 2.0);
        assert_eq!(ts.time_span_ms(), 400);
    }

    #[test]
    fn range_query() {
        let mut ts = TimeSeries::new();
        for i in 0..10 {
            ts.push(i * 100, i as f64);
        }
        let range = ts.range(200, 500);
        assert_eq!(range.len(), 4); // 200, 300, 400, 500
        assert_eq!(range[0].timestamp_ms, 200);
        assert_eq!(range[3].timestamp_ms, 500);
    }

    #[test]
    fn range_empty() {
        let mut ts = TimeSeries::new();
        ts.push(100, 1.0);
        let range = ts.range(200, 300);
        assert!(range.is_empty());
    }

    #[test]
    fn stats() {
        let mut ts = TimeSeries::new();
        ts.push(100, 10.0);
        ts.push(200, 20.0);
        ts.push(300, 30.0);
        let s = ts.stats(100, 300).unwrap();
        assert_eq!(s.count, 3);
        assert!((s.min - 10.0).abs() < f64::EPSILON);
        assert!((s.max - 30.0).abs() < f64::EPSILON);
        assert!((s.sum - 60.0).abs() < f64::EPSILON);
        assert!((s.mean - 20.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_all() {
        let mut ts = TimeSeries::new();
        ts.push(0, 5.0);
        ts.push(1, 15.0);
        let s = ts.stats_all().unwrap();
        assert_eq!(s.count, 2);
        assert!((s.mean - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stats_empty() {
        let ts = TimeSeries::new();
        assert!(ts.stats(0, 100).is_none());
        assert!(ts.stats_all().is_none());
    }

    #[test]
    fn percentile() {
        let mut ts = TimeSeries::new();
        for i in 1..=100 {
            ts.push(i, i as f64);
        }
        let p50 = ts.percentile(0.5).unwrap();
        assert!((p50 - 50.0).abs() < 2.0);
        let p0 = ts.percentile(0.0).unwrap();
        assert!((p0 - 1.0).abs() < f64::EPSILON);
        let p100 = ts.percentile(1.0).unwrap();
        assert!((p100 - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn percentile_empty() {
        let ts = TimeSeries::new();
        assert!(ts.percentile(0.5).is_none());
    }

    #[test]
    fn percentile_range() {
        let mut ts = TimeSeries::new();
        for i in 0..100 {
            ts.push(i * 10, i as f64);
        }
        let p50 = ts.percentile_range(0.5, 0, 500).unwrap();
        assert!(p50 >= 0.0 && p50 <= 50.0);
    }

    #[test]
    fn downsample() {
        let mut ts = TimeSeries::new();
        for i in 0..100 {
            ts.push(i * 10, i as f64);
        }
        let ds = ts.downsample(10);
        assert!(ds.len() <= 10);
        // Values should be averages of groups
        assert!(ds.latest().is_some());
    }

    #[test]
    fn downsample_no_reduction() {
        let mut ts = TimeSeries::new();
        ts.push(0, 1.0);
        ts.push(1, 2.0);
        let ds = ts.downsample(10);
        assert_eq!(ds.len(), 2); // already fewer than target
    }

    #[test]
    fn downsample_empty() {
        let ts = TimeSeries::new();
        let ds = ts.downsample(10);
        assert!(ds.is_empty());
    }

    #[test]
    fn merge() {
        let mut ts1 = TimeSeries::new();
        ts1.push(100, 1.0);
        ts1.push(300, 3.0);

        let mut ts2 = TimeSeries::new();
        ts2.push(200, 2.0);
        ts2.push(400, 4.0);

        ts1.merge(&ts2);
        assert_eq!(ts1.len(), 4);
        let points = ts1.to_vec();
        assert_eq!(points[0].timestamp_ms, 100);
        assert_eq!(points[1].timestamp_ms, 200);
        assert_eq!(points[2].timestamp_ms, 300);
        assert_eq!(points[3].timestamp_ms, 400);
    }

    #[test]
    fn merge_with_eviction() {
        let mut ts1 = TimeSeries::with_config(TimeSeriesConfig { max_points: 3 });
        ts1.push(100, 1.0);
        ts1.push(200, 2.0);

        let mut ts2 = TimeSeries::new();
        ts2.push(300, 3.0);
        ts2.push(400, 4.0);

        ts1.merge(&ts2);
        assert_eq!(ts1.len(), 3); // capped at 3
        // Should keep the newest 3
        assert_eq!(ts1.oldest().unwrap().timestamp_ms, 200);
    }

    #[test]
    fn clear() {
        let mut ts = TimeSeries::new();
        ts.push(0, 1.0);
        ts.push(1, 2.0);
        ts.clear();
        assert!(ts.is_empty());
    }

    #[test]
    fn iter() {
        let mut ts = TimeSeries::new();
        ts.push(10, 1.0);
        ts.push(20, 2.0);
        let collected: Vec<DataPoint> = ts.iter().copied().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].timestamp_ms, 10);
    }

    #[test]
    fn to_vec() {
        let mut ts = TimeSeries::new();
        ts.push(0, 0.0);
        ts.push(1, 1.0);
        let v = ts.to_vec();
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn rate() {
        let mut ts = TimeSeries::new();
        for i in 0..10 {
            ts.push(i * 100, 1.0); // 10 events over 1 second
        }
        let r = ts.rate(900, 1000);
        assert!((r - 10.0).abs() < 1.0);
    }

    #[test]
    fn rate_empty() {
        let ts = TimeSeries::new();
        assert_eq!(ts.rate(1000, 1000), 0.0);
    }

    #[test]
    fn rate_zero_window() {
        let mut ts = TimeSeries::new();
        ts.push(100, 1.0);
        assert_eq!(ts.rate(100, 0), 0.0);
    }

    #[test]
    fn default_config() {
        let config = TimeSeriesConfig::default();
        assert_eq!(config.max_points, 10_000);
    }

    #[test]
    fn with_config() {
        let config = TimeSeriesConfig { max_points: 500 };
        let ts = TimeSeries::with_config(config);
        assert_eq!(ts.capacity(), 500);
    }

    #[test]
    fn serde_data_point() {
        let dp = DataPoint::new(100, 42.5);
        let json = serde_json::to_string(&dp).unwrap();
        let back: DataPoint = serde_json::from_str(&json).unwrap();
        assert_eq!(dp, back);
    }

    #[test]
    fn serde_stats() {
        let s = Stats {
            count: 10,
            min: 1.0,
            max: 100.0,
            sum: 550.0,
            mean: 55.0,
            start_ms: 0,
            end_ms: 1000,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Stats = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
    }

    #[test]
    fn serde_time_series() {
        let mut ts = TimeSeries::new();
        ts.push(100, 1.0);
        ts.push(200, 2.0);
        let json = serde_json::to_string(&ts).unwrap();
        let back: TimeSeries = serde_json::from_str(&json).unwrap();
        assert_eq!(ts, back);
    }

    #[test]
    fn serde_config() {
        let config = TimeSeriesConfig { max_points: 42 };
        let json = serde_json::to_string(&config).unwrap();
        let back: TimeSeriesConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }

    #[test]
    fn default_trait() {
        let ts = TimeSeries::default();
        assert!(ts.is_empty());
        assert_eq!(ts.capacity(), 10_000);
    }

    #[test]
    fn clone() {
        let mut ts = TimeSeries::new();
        ts.push(100, 1.0);
        let cloned = ts.clone();
        assert_eq!(ts, cloned);
    }

    #[test]
    fn equality() {
        let mut a = TimeSeries::new();
        let mut b = TimeSeries::new();
        a.push(0, 1.0);
        b.push(0, 1.0);
        assert_eq!(a, b);
    }

    #[test]
    fn debug_format() {
        let ts = TimeSeries::new();
        let dbg = format!("{ts:?}");
        assert!(dbg.contains("TimeSeries"));
    }

    #[test]
    #[should_panic(expected = "max_points must be > 0")]
    fn zero_capacity_panics() {
        let _ = TimeSeries::with_config(TimeSeriesConfig { max_points: 0 });
    }
}
