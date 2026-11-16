use config::configuration;
use config::lua::get_or_create_sub_module;
use config::lua::mlua::Lua;
use hdrhistogram::Histogram;
use metrics::{Counter, Gauge, Key, KeyName, Metadata, Recorder, SharedString, Unit};
use parking_lot::Mutex;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tabout::{Alignment, Column, tabulate_output};

static ENABLE_STAT_PRINT: AtomicBool = AtomicBool::new(true);
lazy_static::lazy_static! {
    static ref INNER: Arc<Mutex<Inner>> = make_inner();
}

struct ThroughputInner {
    hist: Histogram<u64>,
    last: Option<Instant>,
    count: u64,
}

struct Throughput {
    inner: Mutex<ThroughputInner>,
}

impl Throughput {
    fn new() -> Self {
        Self {
            inner: Mutex::new(ThroughputInner {
                hist: Histogram::new(2).expect("failed to create histogram"),
                last: None,
                count: 0,
            }),
        }
    }
    fn current(&self) -> u64 {
        self.inner.lock().current()
    }

    fn percentiles(&self) -> (u64, u64, u64) {
        let inner = self.inner.lock();
        let p50 = inner.hist.value_at_percentile(50.);
        let p75 = inner.hist.value_at_percentile(75.);
        let p95 = inner.hist.value_at_percentile(95.);
        (p50, p75, p95)
    }
}

impl ThroughputInner {
    fn add(&mut self, value: u64) {
        if let Some(ref last) = self.last {
            let elapsed = last.elapsed();
            if elapsed > Duration::from_secs(1) {
                self.hist.record(self.count).ok();
                self.count = 0;
                self.last = Some(Instant::now());
            }
        } else {
            // Start a new window
            self.last = Some(Instant::now());
        };
        self.count += value;
    }

    fn current(&mut self) -> u64 {
        if let Some(ref last) = self.last {
            let elapsed = last.elapsed();
            if elapsed > Duration::from_secs(1) {
                self.hist.record(self.count).ok();
                self.count = 0;
                self.last = Some(Instant::now());
            }
        }
        self.count
    }
}

impl metrics::HistogramFn for Throughput {
    fn record(&self, value: f64) {
        self.inner.lock().add(value as u64);
    }
}

struct ScaledHistogram {
    hist: Mutex<Histogram<u64>>,
    scale: f64,
}

impl ScaledHistogram {
    fn new(scale: f64) -> Arc<Self> {
        Arc::new(Self {
            hist: Mutex::new(Histogram::new(2).expect("failed to create new Histogram")),
            scale,
        })
    }
    fn percentiles(&self) -> (u64, u64, u64) {
        let hist = self.hist.lock();
        let p50 = hist.value_at_percentile(50.);
        let p75 = hist.value_at_percentile(75.);
        let p95 = hist.value_at_percentile(95.);
        (p50, p75, p95)
    }

    fn latency_percentiles(&self) -> (Duration, Duration, Duration) {
        let hist = self.hist.lock();
        let p50 = pctile_latency(&*hist, 50.);
        let p75 = pctile_latency(&*hist, 75.);
        let p95 = pctile_latency(&*hist, 95.);
        (p50, p75, p95)
    }
}

impl metrics::HistogramFn for ScaledHistogram {
    fn record(&self, value: f64) {
        self.hist.lock().record((value * self.scale) as u64).ok();
    }
}

fn pctile_latency(histogram: &Histogram<u64>, p: f64) -> Duration {
    Duration::from_nanos(histogram.value_at_percentile(p))
}

struct MyCounter {
    value: AtomicUsize,
}

impl metrics::CounterFn for MyCounter {
    fn increment(&self, value: u64) {
        self.value.fetch_add(value as usize, Ordering::Relaxed);
    }

    fn absolute(&self, value: u64) {
        self.value.store(value as usize, Ordering::Relaxed);
    }
}

struct Inner {
    histograms: HashMap<Key, Arc<ScaledHistogram>>,
    throughput: HashMap<Key, Arc<Throughput>>,
    counters: HashMap<Key, Arc<MyCounter>>,
}

impl Inner {
    fn run(inner: Arc<Mutex<Inner>>) {
        let mut last_print = Instant::now();

        let rate_cols = vec![
            Column {
                name: "STAT".to_string(),
                alignment: Alignment::Left,
            },
            Column {
                name: "current".to_string(),
                alignment: Alignment::Left,
            },
            Column {
                name: "p50".to_string(),
                alignment: Alignment::Left,
            },
            Column {
                name: "p75".to_string(),
                alignment: Alignment::Left,
            },
            Column {
                name: "p95".to_string(),
                alignment: Alignment::Left,
            },
        ];
        let cols = vec![
            Column {
                name: "STAT".to_string(),
                alignment: Alignment::Left,
            },
            Column {
                name: "p50".to_string(),
                alignment: Alignment::Left,
            },
            Column {
                name: "p75".to_string(),
                alignment: Alignment::Left,
            },
            Column {
                name: "p95".to_string(),
                alignment: Alignment::Left,
            },
        ];
        let count_cols = vec![
            Column {
                name: "STAT".to_string(),
                alignment: Alignment::Left,
            },
            Column {
                name: "COUNT".to_string(),
                alignment: Alignment::Left,
            },
        ];

        loop {
            std::thread::sleep(Duration::from_secs(1));

            if !ENABLE_STAT_PRINT.load(Ordering::Acquire) {
                break;
            }

            let seconds = configuration().periodic_stat_logging;
            if seconds == 0 {
                continue;
            }
            if last_print.elapsed() >= Duration::from_secs(seconds) {
                let mut data = vec![];

                let mut inner = inner.lock();
                for (key, tput) in &mut inner.throughput {
                    let current = tput.current();
                    let (p50, p75, p95) = tput.percentiles();
                    data.push(vec![
                        key.to_string(),
                        format!("{:.2?}", current),
                        format!("{:.2?}", p50),
                        format!("{:.2?}", p75),
                        format!("{:.2?}", p95),
                    ]);
                }
                data.sort_by(|a, b| a[0].cmp(&b[0]));
                eprintln!();
                tabulate_output(&rate_cols, &data, &mut std::io::stderr().lock()).ok();

                data.clear();
                for (key, histogram) in &inner.histograms {
                    if key.name().ends_with(".size") {
                        let (p50, p75, p95) = histogram.percentiles();
                        data.push(vec![
                            key.to_string(),
                            format!("{:.2?}", p50),
                            format!("{:.2?}", p75),
                            format!("{:.2?}", p95),
                        ]);
                    } else {
                        let (p50, p75, p95) = histogram.latency_percentiles();
                        data.push(vec![
                            key.to_string(),
                            format!("{:.2?}", p50),
                            format!("{:.2?}", p75),
                            format!("{:.2?}", p95),
                        ]);
                    }
                }
                data.sort_by(|a, b| a[0].cmp(&b[0]));
                eprintln!();
                tabulate_output(&cols, &data, &mut std::io::stderr().lock()).ok();

                data.clear();
                for (key, count) in &inner.counters {
                    data.push(vec![
                        key.to_string(),
                        count.value.load(Ordering::Relaxed).to_string(),
                    ]);
                }
                data.sort_by(|a, b| a[0].cmp(&b[0]));
                eprintln!();
                tabulate_output(&count_cols, &data, &mut std::io::stderr().lock()).ok();

                last_print = Instant::now();
            }
        }
    }
}

#[allow(dead_code)]
fn make_inner() -> Arc<Mutex<Inner>> {
    Arc::new(Mutex::new(Inner {
        histograms: HashMap::new(),
        throughput: HashMap::new(),
        counters: HashMap::new(),
    }))
}

pub struct Stats {
    inner: Arc<Mutex<Inner>>,
}

impl Stats {
    pub fn new() -> Self {
        Self {
            inner: Arc::clone(&INNER),
        }
    }

    pub fn init() -> anyhow::Result<()> {
        let stats = Self::new();
        let inner = Arc::clone(&stats.inner);
        std::thread::Builder::new()
            .name("gui-stats-recorder".to_string())
            .spawn(move || Inner::run(inner))
            .map_err(|e| anyhow::anyhow!("Failed to spawn stats recorder thread:{e}"))?;
        metrics::set_global_recorder(stats)
            .map_err(|e| anyhow::anyhow!("Failed to set metrics recorder:{}", e))
    }
}

/// Safety valve for the per-thread handle caches below. The recorder is fed only the small, fixed
/// set of static metric names declared across the GUI (a couple dozen), so in practice each cache
/// holds a handful of entries and this cap is never reached. It exists purely to bound memory if a
/// future caller ever introduces dynamically-keyed metrics (labels / formatted names): once a
/// thread has cached this many distinct handles, further keys fall back to the global-lock path
/// instead of growing the cache without bound (graceful degradation to the pre-cache behavior).
const MAX_PER_THREAD_HANDLE_CACHE: usize = 1024;

thread_local! {
    /// Per-thread metric-handle cache.
    ///
    /// The `metrics` macros re-resolve a metric's handle on *every* emission — the crate caches
    /// only the static `Key`, never the resolved `Counter`/`Histogram` handle (see metrics 0.24
    /// `__register_metric!`). So without this cache, every `histogram!`/`counter!` on a hot path
    /// (per-glyph, per-line, per-frame, per-PDU, per-RPC) would lock the single global `INNER`
    /// mutex, serializing all emitting threads and surfacing as `parking_lot RawMutex::lock_slow`
    /// contention under load.
    ///
    /// Resolving each metric once per (thread, key) and caching the handle thread-locally makes
    /// every subsequent emission lock-free; the global `INNER` mutex is taken only on the cold
    /// miss (a handful of times per thread over the whole process). The cached handles are `Arc`
    /// clones of the exact same metric objects stored in `INNER`, so records made through a cached
    /// handle land in the same histogram/counter, and the stats-printer thread and the Lua
    /// `get_counters` view are unaffected. Entries are never invalidated because `INNER` never
    /// replaces a key's handle (insert-once, reuse-forever), so a cached handle is always valid.
    static COUNTER_CACHE: RefCell<HashMap<Key, Counter>> = RefCell::new(HashMap::new());
    static HISTOGRAM_CACHE: RefCell<HashMap<Key, metrics::Histogram>> = RefCell::new(HashMap::new());
}

impl Recorder for Stats {
    fn describe_counter(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn describe_gauge(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn describe_histogram(&self, _key: KeyName, _unit: Option<Unit>, _description: SharedString) {}

    fn register_counter(&self, key: &Key, _metadata: &Metadata) -> Counter {
        if let Some(existing) = COUNTER_CACHE.with(|c| c.borrow().get(key).cloned()) {
            return existing;
        }
        let counter = {
            let mut inner = self.inner.lock();
            match inner.counters.get(key) {
                Some(existing) => Counter::from_arc(existing.clone()),
                None => {
                    let counter = Arc::new(MyCounter {
                        value: AtomicUsize::new(0),
                    });
                    inner.counters.insert(key.clone(), counter.clone());
                    metrics::Counter::from_arc(counter)
                }
            }
        };
        COUNTER_CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            if cache.len() < MAX_PER_THREAD_HANDLE_CACHE {
                cache.insert(key.clone(), counter.clone());
            }
        });
        counter
    }

    fn register_gauge(&self, _key: &Key, _metadata: &Metadata) -> Gauge {
        Gauge::noop()
    }

    fn register_histogram(&self, key: &Key, _metadata: &Metadata) -> metrics::Histogram {
        if let Some(existing) = HISTOGRAM_CACHE.with(|c| c.borrow().get(key).cloned()) {
            return existing;
        }
        let histogram = {
            let mut inner = self.inner.lock();
            if key.name().ends_with(".rate") {
                match inner.throughput.get(key) {
                    Some(existing) => metrics::Histogram::from_arc(existing.clone()),
                    None => {
                        let tput = Arc::new(Throughput::new());
                        inner.throughput.insert(key.clone(), tput.clone());

                        metrics::Histogram::from_arc(tput)
                    }
                }
            } else {
                match inner.histograms.get(key) {
                    Some(existing) => metrics::Histogram::from_arc(existing.clone()),
                    None => {
                        let scale = if key.name().ends_with(".size") {
                            1.0
                        } else {
                            // Assume seconds; convert to nanoseconds
                            1_000_000_000.0
                        };

                        let histogram = ScaledHistogram::new(scale);
                        inner.histograms.insert(key.clone(), histogram.clone());

                        metrics::Histogram::from_arc(histogram)
                    }
                }
            }
        };
        HISTOGRAM_CACHE.with(|c| {
            let mut cache = c.borrow_mut();
            if cache.len() < MAX_PER_THREAD_HANDLE_CACHE {
                cache.insert(key.clone(), histogram.clone());
            }
        });
        histogram
    }
}

#[allow(dead_code)]
pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let metrics_mod = get_or_create_sub_module(lua, "metrics")?;
    metrics_mod.set(
        "get_counters",
        lua.create_function(|_, _: ()| {
            let inner = INNER.lock();
            let counters: HashMap<String, usize> = inner
                .counters
                .iter()
                .map(|(k, v)| (k.name().to_string(), v.value.load(Ordering::Relaxed)))
                .collect();
            Ok(counters)
        })?,
    )?;
    metrics_mod.set(
        "get_throughput",
        lua.create_function(|_, _: ()| {
            let mut inner = INNER.lock();
            let counters: HashMap<String, HashMap<String, u64>> = inner
                .throughput
                .iter_mut()
                .map(|(k, tput)| {
                    let mut res = HashMap::new();
                    res.insert("current".to_string(), tput.current());
                    let (p50, p75, p95) = tput.percentiles();
                    res.insert("p50".to_string(), p50);
                    res.insert("p75".to_string(), p75);
                    res.insert("p95".to_string(), p95);
                    (k.name().to_string(), res)
                })
                .collect();
            Ok(counters)
        })?,
    )?;
    metrics_mod.set(
        "get_sizes",
        lua.create_function(|_, _: ()| {
            let mut inner = INNER.lock();
            let counters: HashMap<String, HashMap<String, u64>> = inner
                .histograms
                .iter_mut()
                .filter_map(|(key, hist)| {
                    if key.name().ends_with(".size") {
                        let mut res = HashMap::new();
                        let (p50, p75, p95) = hist.percentiles();
                        res.insert("p50".to_string(), p50);
                        res.insert("p75".to_string(), p75);
                        res.insert("p95".to_string(), p95);
                        Some((key.name().to_string(), res))
                    } else {
                        None
                    }
                })
                .collect();
            Ok(counters)
        })?,
    )?;
    metrics_mod.set(
        "get_latency",
        lua.create_function(|_, _: ()| {
            let mut inner = INNER.lock();
            let counters: HashMap<String, HashMap<String, String>> = inner
                .histograms
                .iter_mut()
                .filter_map(|(key, hist)| {
                    if !key.name().ends_with(".size") {
                        let mut res = HashMap::new();
                        let (p50, p75, p95) = hist.latency_percentiles();
                        res.insert("p50".to_string(), format!("{p50:?}"));
                        res.insert("p75".to_string(), format!("{p75:?}"));
                        res.insert("p95".to_string(), format!("{p95:?}"));
                        Some((key.name().to_string(), res))
                    } else {
                        None
                    }
                })
                .collect();
            Ok(counters)
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics::Level;

    // The per-thread handle cache must be transparent: a handle returned from the cache
    // (thread-local hit) must point at the SAME metric object stored in the global `INNER` map
    // that the stats-printer thread and the Lua `get_counters`/`get_latency` views read — so
    // records made through cached handles are observable exactly as before the cache existed.

    #[test]
    fn cached_counter_handles_share_the_inner_counter() {
        let stats = Stats::new();
        let md = Metadata::new("test", Level::INFO, None);
        let key = Key::from_static_name("test.recorder_cache.counter.alpha");
        // Cold miss: registers into INNER + populates the thread-local cache.
        let c1 = stats.register_counter(&key, &md);
        c1.increment(3);
        // Thread-local hit: a handle for the same underlying counter.
        let c2 = stats.register_counter(&key, &md);
        c2.increment(4);
        // Both increments land in the single counter held by INNER.
        let inner = stats.inner.lock();
        let stored = inner
            .counters
            .get(&key)
            .expect("counter registered exactly once in INNER");
        assert_eq!(stored.value.load(Ordering::Relaxed), 7);
    }

    #[test]
    fn cached_histogram_handles_share_the_inner_histogram() {
        let stats = Stats::new();
        let md = Metadata::new("test", Level::INFO, None);
        let key = Key::from_static_name("test.recorder_cache.histogram.alpha");
        let h1 = stats.register_histogram(&key, &md);
        let h2 = stats.register_histogram(&key, &md);
        h1.record(0.000_001);
        h2.record(0.000_002);
        // Exactly one ScaledHistogram in INNER for this key; both cached handles recorded into it.
        let inner = stats.inner.lock();
        let stored = inner
            .histograms
            .get(&key)
            .expect("histogram registered exactly once in INNER");
        assert_eq!(stored.hist.lock().len(), 2);
    }
}
