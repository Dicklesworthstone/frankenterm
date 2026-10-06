//! Command line shared by the bench target and the example target.

use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};

use super::cache::{self, LoadedCorpus};
use super::corpus::{Corpus, DEFAULT_SEED, DEFAULT_SIZE};
use super::lanes::{self, Expectation, Geometry, GridSummary, Lane, LaneRun};

pub const USAGE: &str = "\
ingest_throughput: headless FrankenTerm ingest throughput (ft-yccm0.1.2)

Build optimized, never with --release (opt-level=z):
  cargo build -p frankenterm-term --profile release-perf --example ingest_throughput
  hyperfine -w 1 -r 10 \"$CARGO_TARGET_DIR/release-perf/examples/ingest_throughput --lane term\"
or run every lane through cargo bench, whose [profile.bench] mirrors release-perf:
  cargo bench -p frankenterm-term --bench ingest_throughput [-- OPTIONS]

Options:
  --lane L          parse | term | mux_two_stage | prod_config | all (default all);
                    comma-separated lists are accepted
  --corpus C        color_emoji_random (default) | color_random | seq_lines |
                    long_lines | unicode_mix | tui_repaint | all; comma-separated
  --from-file PATH  drive the lanes with PATH instead, e.g. the operator's
                    color-emoji-random.bin
  --size N          generated corpus size in bytes, or with a K/KiB/M/MiB/G/GiB or
                    KB/MB/GB suffix (default 64MiB)
  --seed N          generator seed, decimal or 0x-hex (default 20261005)
  --rows N  --cols N  --scrollback N  --chunk N
                    terminal geometry and bytes per feed call; the defaults come
                    from $ROWS $COLS $SCROLLBACK $CHUNK, else 80 120 3500 128KiB
  --corpus-dir DIR  corpus cache (default $FT_CORPUS_DIR, else
                    $CARGO_TARGET_DIR/ft-corpus)
  --gen-only        write the corpora to the cache and print their paths and
                    SHA-256s, e.g. to feed ghostty-bench identical bytes
  --sticky-zwj      before timing, print a ZWJ sequence cut off by CR LF, so the
                    terminal's ZWJ-tail flag stays set for the run (ft-yccm0.2.14)
  --allow-debug     run an unoptimized build anyway (its numbers mean nothing)

Prints one JSON line per lane run on stdout and a [BENCH] summary on stderr.
$FT_BENCH_GIT_SHA overrides the recorded git SHA (default: git rev-parse HEAD).
Exits 1 when a lane's final state fails the sanity checks, 2 on usage errors.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// `examples/ingest_throughput.rs`, the binary hyperfine runs.
    Example,
    /// `benches/ingest_throughput.rs`. `cargo bench` passes `--bench`; without
    /// it (`cargo test --benches`) the defaults shrink to a quick 64 KiB run.
    BenchTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Generated(Vec<Corpus>),
    File(PathBuf),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub lanes: Vec<Lane>,
    pub source: Source,
    pub size: usize,
    pub seed: u64,
    pub geometry: Geometry,
    pub corpus_dir: Option<PathBuf>,
    pub gen_only: bool,
    /// Feed `lanes::STICKY_ZWJ_PRELUDE` untimed before each lane run.
    pub sticky_zwj: bool,
    pub allow_debug: bool,
    pub help: bool,
}

/// Size of a quick `cargo test --benches` run of the bench target.
pub const TEST_MODE_SIZE: usize = 64 * 1024;

pub fn parse_size(text: &str) -> Result<usize, String> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (digits, suffix) = text.split_at(split);
    let value: usize = digits.parse().map_err(|_| format!("bad size {text:?}"))?;
    let multiplier: usize = match suffix.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kib" => 1 << 10,
        "m" | "mib" => 1 << 20,
        "g" | "gib" => 1 << 30,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        _ => {
            return Err(format!(
                "bad size suffix in {text:?}; use B, K, KiB, M, MiB, G, GiB, KB, MB or GB"
            ))
        }
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| format!("size {text:?} overflows"))
}

fn parse_seed(text: &str) -> Result<u64, String> {
    let parsed = match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|_| format!("bad seed {text:?}"))
}

fn parse_count(name: &str, text: &str) -> Result<usize, String> {
    match text.parse::<usize>() {
        Ok(value) if value > 0 => Ok(value),
        _ => Err(format!("{name} must be a positive integer, got {text:?}")),
    }
}

fn parse_list<T>(
    kind: &str,
    text: &str,
    all: &[T],
    from_name: fn(&str) -> Option<T>,
) -> Result<Vec<T>, String>
where
    T: Copy,
{
    if text == "all" {
        return Ok(all.to_vec());
    }
    text.split(',')
        .map(|name| from_name(name.trim()).ok_or_else(|| format!("unknown {kind} {name:?}")))
        .collect()
}

fn take_value(
    flag: &str,
    inline: Option<&str>,
    rest: &mut std::slice::Iter<'_, String>,
) -> Result<String, String> {
    match inline {
        Some(value) => Ok(value.to_string()),
        None => rest
            .next()
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value")),
    }
}

/// Parses the arguments after the program name. `env` supplies the
/// `ROWS`/`COLS`/`SCROLLBACK`/`CHUNK` defaults.
pub fn parse_args(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
    entry: Entry,
) -> Result<Options, String> {
    let test_mode = entry == Entry::BenchTarget && !args.iter().any(|arg| arg == "--bench");
    let mut geometry = Geometry::default();
    if let Some(rows) = env("ROWS") {
        geometry.rows = parse_count("ROWS", &rows)?;
    }
    if let Some(cols) = env("COLS") {
        geometry.cols = parse_count("COLS", &cols)?;
    }
    if let Some(scrollback) = env("SCROLLBACK") {
        geometry.scrollback = scrollback
            .parse()
            .map_err(|_| format!("SCROLLBACK must be an integer, got {scrollback:?}"))?;
    }
    if let Some(chunk) = env("CHUNK") {
        geometry.chunk = parse_size(&chunk)?;
    }

    let mut options = Options {
        lanes: Lane::ALL.to_vec(),
        source: Source::Generated(vec![Corpus::ColorEmojiRandom]),
        size: if test_mode {
            TEST_MODE_SIZE
        } else {
            DEFAULT_SIZE
        },
        seed: DEFAULT_SEED,
        geometry,
        corpus_dir: None,
        gen_only: false,
        sticky_zwj: false,
        allow_debug: test_mode,
        help: false,
    };
    let mut corpus_flag = false;
    let mut file_flag = false;

    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if flag.starts_with("--") => (flag, Some(value)),
            _ => (arg.as_str(), None),
        };
        match flag {
            "--bench" => {}
            "-h" | "--help" => options.help = true,
            "--gen-only" => options.gen_only = true,
            "--sticky-zwj" => options.sticky_zwj = true,
            "--allow-debug" => options.allow_debug = true,
            "--lane" => {
                let value = take_value(flag, inline, &mut rest)?;
                options.lanes = parse_list("lane", &value, &Lane::ALL, Lane::from_name)?;
            }
            "--corpus" => {
                let value = take_value(flag, inline, &mut rest)?;
                let corpora = parse_list("corpus", &value, &Corpus::ALL, Corpus::from_name)?;
                options.source = Source::Generated(corpora);
                corpus_flag = true;
            }
            "--from-file" => {
                let value = take_value(flag, inline, &mut rest)?;
                options.source = Source::File(PathBuf::from(value));
                file_flag = true;
            }
            "--size" => options.size = parse_size(&take_value(flag, inline, &mut rest)?)?,
            "--seed" => options.seed = parse_seed(&take_value(flag, inline, &mut rest)?)?,
            "--rows" => {
                options.geometry.rows = parse_count(flag, &take_value(flag, inline, &mut rest)?)?
            }
            "--cols" => {
                options.geometry.cols = parse_count(flag, &take_value(flag, inline, &mut rest)?)?
            }
            "--scrollback" => {
                let value = take_value(flag, inline, &mut rest)?;
                options.geometry.scrollback = value
                    .parse()
                    .map_err(|_| format!("--scrollback must be an integer, got {value:?}"))?;
            }
            "--chunk" => {
                options.geometry.chunk = parse_size(&take_value(flag, inline, &mut rest)?)?
            }
            "--corpus-dir" => {
                options.corpus_dir = Some(PathBuf::from(take_value(flag, inline, &mut rest)?))
            }
            _ => return Err(format!("unknown argument {arg:?}")),
        }
    }

    if corpus_flag && file_flag {
        return Err("--corpus and --from-file are mutually exclusive".to_string());
    }
    if file_flag && options.gen_only {
        return Err("--gen-only generates corpora; it cannot take --from-file".to_string());
    }
    if options.geometry.chunk == 0 {
        return Err("the chunk size must be positive".to_string());
    }
    if options.size == 0 && !file_flag {
        return Err("the corpus size must be positive".to_string());
    }
    Ok(options)
}

/// Build facts recorded with every run.
#[derive(Clone, Debug)]
pub struct RecordContext {
    pub git_sha: String,
    /// Directory name of the cargo profile the binary was built into.
    /// `cargo bench` builds into `release/` with the `bench` profile.
    pub profile_dir: String,
    pub debug_assertions: bool,
}

impl RecordContext {
    pub fn capture() -> Self {
        Self {
            git_sha: git_sha(),
            profile_dir: profile_dir().unwrap_or_else(|| "unknown".to_string()),
            debug_assertions: cfg!(debug_assertions),
        }
    }
}

fn git_sha() -> String {
    if let Ok(sha) = std::env::var("FT_BENCH_GIT_SHA") {
        return sha;
    }
    Command::new("git")
        .args(["-C", env!("CARGO_MANIFEST_DIR"), "rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|sha| sha.trim().to_string())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `<target>/<profile>/{examples,deps}/<binary>` -> `<profile>`.
fn profile_dir() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let mut dir = exe.parent()?;
    if matches!(
        dir.file_name().and_then(|name| name.to_str()),
        Some("examples") | Some("deps")
    ) {
        dir = dir.parent()?;
    }
    Some(dir.file_name()?.to_string_lossy().into_owned())
}

/// The host's 1-minute load average, recorded next to every number because
/// the measurement host is shared.
pub fn load_avg_1m() -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string("/proc/loadavg") {
        return text.split_whitespace().next()?.parse().ok();
    }
    // macOS: `sysctl -n vm.loadavg` prints "{ 1.23 2.34 3.45 }".
    let output = Command::new("sysctl")
        .args(["-n", "vm.loadavg"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).ok()?;
    let first = text.split_whitespace().find(|field| *field != "{")?;
    first.parse().ok()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

/// One corpus to drive the lanes with.
pub struct Input {
    pub label: String,
    pub seed: Option<u64>,
    pub corpus: LoadedCorpus,
    pub expectation: Expectation,
}

/// What a lane run left behind.
pub struct Outcome {
    pub summary: Option<GridSummary>,
    pub verdict: Result<(), String>,
    pub load_avg_1m: Option<f64>,
}

/// One JSON line per lane run.
#[derive(Serialize)]
struct LaneRecord<'a> {
    schema: &'static str,
    lane: &'static str,
    corpus: &'a str,
    corpus_source: &'static str,
    corpus_path: String,
    corpus_sha256: &'a str,
    seed: Option<u64>,
    bytes: usize,
    prelude_bytes: usize,
    secs: f64,
    mib_per_s: f64,
    actions: Option<u64>,
    /// Always null: a counting global allocator needs an unsafe `GlobalAlloc`
    /// impl, which this workspace does not admit without an audit.
    allocations: Option<u64>,
    rows: usize,
    cols: usize,
    scrollback: usize,
    chunk: usize,
    cursor_x: Option<usize>,
    cursor_y: Option<i64>,
    retained_rows: Option<usize>,
    nonblank_visible_cells: Option<usize>,
    /// The same input and toolchain must give the same fingerprint, so an A/B
    /// arm that changes the final grid shows up here.
    state_fingerprint: Option<String>,
    sanity: &'a str,
    git_sha: &'a str,
    profile: &'a str,
    debug_assertions: bool,
    load_avg_1m: Option<f64>,
    ts_ms: u64,
}

pub fn lane_record(
    run: &LaneRun,
    input: &Input,
    outcome: &Outcome,
    geometry: &Geometry,
    context: &RecordContext,
) -> Value {
    let summary = outcome.summary.as_ref();
    let record = LaneRecord {
        schema: "ft.bench.ingest-throughput.v1",
        lane: run.lane.name(),
        corpus: &input.label,
        corpus_source: input.corpus.outcome.name(),
        corpus_path: input.corpus.path.display().to_string(),
        corpus_sha256: &input.corpus.sha256,
        seed: input.seed,
        bytes: run.bytes,
        prelude_bytes: run.prelude_bytes,
        secs: run.secs,
        mib_per_s: run.mib_per_s(),
        actions: run.actions,
        allocations: None,
        rows: geometry.rows,
        cols: geometry.cols,
        scrollback: geometry.scrollback,
        chunk: geometry.chunk,
        cursor_x: summary.map(|s| s.cursor_x),
        cursor_y: summary.map(|s| s.cursor_y),
        retained_rows: summary.map(|s| s.retained_rows),
        nonblank_visible_cells: summary.map(|s| s.nonblank_visible_cells),
        state_fingerprint: summary.map(|s| format!("{:016x}", s.fingerprint)),
        sanity: match &outcome.verdict {
            Ok(()) => "ok",
            Err(error) => error.as_str(),
        },
        git_sha: &context.git_sha,
        profile: &context.profile_dir,
        debug_assertions: context.debug_assertions,
        load_avg_1m: outcome.load_avg_1m,
        ts_ms: now_ms(),
    };
    serde_json::to_value(&record).expect("a lane record always serializes")
}

/// Runs every requested lane over `input`; returns the number of lanes whose
/// final state failed the sanity checks.
fn run_lanes(options: &Options, input: &Input, context: &RecordContext) -> usize {
    let mut failures = 0;
    for &lane in &options.lanes {
        let load_avg_1m = load_avg_1m();
        let prelude: &[u8] = if options.sticky_zwj {
            lanes::STICKY_ZWJ_PRELUDE
        } else {
            &[]
        };
        let run = lanes::run_lane(lane, prelude, &input.corpus.bytes, &options.geometry);
        let (summary, verdict) = lanes::evaluate(&run, &options.geometry, &input.expectation);
        let outcome = Outcome {
            summary,
            verdict,
            load_avg_1m,
        };
        let record = lane_record(&run, input, &outcome, &options.geometry, context);
        println!("{record}");
        eprintln!(
            "[BENCH] ingest_throughput lane={} corpus={} bytes={} secs={:.3} MiB/s={:.1} load_avg_1m={} sanity={}",
            lane.name(),
            input.label,
            run.bytes,
            run.secs,
            run.mib_per_s(),
            load_avg_1m.map_or_else(|| "unknown".to_string(), |load| format!("{load:.2}")),
            record["sanity"].as_str().unwrap_or("unknown"),
        );
        if outcome.verdict.is_err() {
            failures += 1;
        }
    }
    failures
}

fn file_label(path: &Path) -> String {
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    format!("file:{name}")
}

fn run(options: &Options) -> Result<usize, String> {
    let context = RecordContext::capture();
    if context.debug_assertions {
        eprintln!("ingest_throughput: unoptimized build; these numbers are not measurements");
    }
    let corpora = match &options.source {
        Source::File(path) => {
            let corpus = cache::load_file(path)
                .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
            let expectation = Expectation::for_file(&corpus.bytes, &options.geometry);
            let input = Input {
                label: file_label(path),
                seed: None,
                corpus,
                expectation,
            };
            return Ok(run_lanes(options, &input, &context));
        }
        Source::Generated(corpora) => corpora,
    };
    let dir = options
        .corpus_dir
        .clone()
        .unwrap_or_else(cache::default_cache_dir);
    let mut failures = 0;
    for &corpus in corpora {
        let loaded =
            cache::load_or_generate(&dir, corpus, options.size, options.seed).map_err(|error| {
                format!(
                    "cannot cache {} in {}: {error}",
                    corpus.name(),
                    dir.display()
                )
            })?;
        if options.gen_only {
            println!(
                "{}",
                json!({
                    "schema": "ft.bench.ingest-corpus.v1",
                    "corpus": corpus.name(),
                    "corpus_source": loaded.outcome.name(),
                    "corpus_path": loaded.path.display().to_string(),
                    "corpus_sha256": loaded.sha256,
                    "seed": options.seed,
                    "bytes": loaded.bytes.len(),
                })
            );
            continue;
        }
        let expectation =
            Expectation::for_generated(&loaded.bytes, &options.geometry, corpus.is_line_oriented());
        let input = Input {
            label: corpus.name().to_string(),
            seed: Some(options.seed),
            corpus: loaded,
            expectation,
        };
        failures += run_lanes(options, &input, &context);
    }
    Ok(failures)
}

/// Entry point for both targets; returns the process exit code.
pub fn main_with_args(args: Vec<String>, entry: Entry) -> i32 {
    let options = match parse_args(&args, &|name| std::env::var(name).ok(), entry) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("ingest_throughput: {error}\n\n{USAGE}");
            return 2;
        }
    };
    if options.help {
        println!("{USAGE}");
        return 0;
    }
    if cfg!(debug_assertions) && !options.allow_debug {
        eprintln!(
            "ingest_throughput: this build is unoptimized; build with --profile release-perf \
             or run cargo bench, or pass --allow-debug"
        );
        return 2;
    }
    match run(&options) {
        Ok(0) => 0,
        Ok(failures) => {
            eprintln!("ingest_throughput: {failures} lane run(s) failed the sanity checks");
            1
        }
        Err(error) => {
            eprintln!("ingest_throughput: {error}");
            1
        }
    }
}
