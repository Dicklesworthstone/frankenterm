//! Smoke test for the headless ingest throughput bench (ft-yccm0.1.2).
//!
//! Pins every corpus by SHA-256, checks that `color_emoji_random` replicates
//! the operator's frame format, runs every lane over 1 MiB of every corpus
//! asserting the final grid invariants (so a lane cannot silently do nothing),
//! and covers the corpus cache, the CLI parser and the JSON record shape.

#[path = "../benches/ingest/mod.rs"]
#[allow(dead_code)]
mod ingest;

use std::collections::HashSet;
use std::path::PathBuf;

use ingest::cache::{self, CacheOutcome};
use ingest::cli::{self, Entry, Source};
use ingest::corpus::{self, Corpus, DEFAULT_SEED, DEFAULT_SIZE, GENERATOR_VERSION};
use ingest::lanes::{self, Expectation, Geometry, GridSummary, Lane, LaneRun};

const PIN_SIZE: usize = 64 * 1024;
const SMOKE_SIZE: usize = 1024 * 1024;

/// SHA-256 of `Corpus::generate(PIN_SIZE, DEFAULT_SEED)`, in `Corpus::ALL`
/// order. The `seq_lines` pin equals
/// `seq 1 100000 | head -c 65536 | shasum -a 256`.
const PINS: [(&str, &str); 6] = [
    (
        "color_emoji_random",
        "62920c4c0802b7a3b1a98d0bd75e2f4b292976cab92f25bd94b6f044dc1fd868",
    ),
    (
        "color_random",
        "c7936cd3fa4f79a5e86b24308e3248619931b30d1b138206a99cb061f92e5121",
    ),
    (
        "seq_lines",
        "0136344a2c720245d024fd969cb1051e9a577c5b64d91b881c4d9c658cf489b7",
    ),
    (
        "long_lines",
        "c60f35b2e0baf7c6cee14937844596065c0da01f07368d584f42c1bbafeb1eb5",
    ),
    (
        "unicode_mix",
        "09ba7cd0af6fc4b13b6043adbe197b88ea7427a882c32b56b5c66cf258202923",
    ),
    (
        "tui_repaint",
        "455f63c19f8758328f6bb8c249b120e9838eb6b41953b7dc30a995ac20f8b09d",
    ),
];

#[test]
fn corpus_sha256_pins_hold() {
    assert_eq!(
        Corpus::ALL[0],
        Corpus::ColorEmojiRandom,
        "the operator's primary test must stay the first corpus"
    );
    assert_eq!(PINS.len(), Corpus::ALL.len());
    for (corpus, (name, pin)) in Corpus::ALL.iter().zip(PINS.iter()) {
        assert_eq!(corpus.name(), *name);
        let bytes = corpus.generate(PIN_SIZE, DEFAULT_SEED);
        assert_eq!(bytes.len(), PIN_SIZE);
        assert_eq!(
            cache::sha256_hex(&bytes),
            *pin,
            "{name} bytes changed; if that is intended, bump GENERATOR_VERSION \
             (now {GENERATOR_VERSION}) so stale cached corpora regenerate"
        );
    }
}

#[test]
fn generation_is_deterministic_exactly_sized_and_seed_sensitive() {
    for &corpus in Corpus::ALL.iter() {
        for &size in [0, 1, 4093, 65_537].iter() {
            let first = corpus.generate(size, 7);
            assert_eq!(first.len(), size, "{} at {size} bytes", corpus.name());
            assert_eq!(
                first,
                corpus.generate(size, 7),
                "{} is not deterministic",
                corpus.name()
            );
        }
        let one = corpus.generate(16 * 1024, 1);
        let two = corpus.generate(16 * 1024, 2);
        if corpus == Corpus::SeqLines {
            assert_eq!(one, two, "seq_lines ignores the seed");
        } else {
            assert_ne!(one, two, "{} ignores its seed", corpus.name());
        }
    }
    assert_eq!(DEFAULT_SIZE, 64 * 1024 * 1024);
}

#[test]
fn seq_lines_is_coreutils_seq_cut_by_head_c() {
    for &size in [65_536, 1_000_003].iter() {
        let mut expected = Vec::with_capacity(size + 16);
        let mut n: u64 = 1;
        while expected.len() < size {
            expected.extend_from_slice(format!("{n}\n").as_bytes());
            n += 1;
        }
        expected.truncate(size);
        assert_eq!(Corpus::SeqLines.generate(size, DEFAULT_SEED), expected);
    }
}

/// Parses `ESC [ {selector} ; 5 ; {n} m` at `at`; returns `(n, end)`.
fn parse_sgr_256(bytes: &[u8], at: usize, selector: &[u8]) -> Option<(usize, usize)> {
    let rest = bytes.get(at..)?.strip_prefix(b"\x1b[")?;
    let rest = rest.strip_prefix(selector)?.strip_prefix(b";5;")?;
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits == 0 || digits > 3 || (digits > 1 && rest[0] == b'0') {
        return None;
    }
    if rest.get(digits) != Some(&b'm') {
        return None;
    }
    let n = std::str::from_utf8(&rest[..digits]).ok()?.parse().ok()?;
    Some((n, at + 2 + selector.len() + 3 + digits + 1))
}

/// The first complete UTF-8 character of `bytes`, if any.
fn first_char(bytes: &[u8]) -> Option<char> {
    let window = &bytes[..bytes.len().min(4)];
    let text = match std::str::from_utf8(window) {
        Ok(text) => text,
        Err(error) => std::str::from_utf8(&window[..error.valid_up_to()]).ok()?,
    };
    text.chars().next()
}

#[test]
fn color_emoji_random_replicates_the_operator_format() {
    let pool = corpus::emoji_pool();
    assert_eq!(pool.len(), 1447, "1,376 emoji + the script's 71 ASCII characters");
    assert_eq!(pool.iter().filter(|ch| ch.is_ascii()).count(), 71);
    assert!(!pool.contains(&'$'), "the script's ASCII pool has no dollar sign");
    assert_eq!(pool[0], '\u{1F600}');
    assert_eq!(pool[80], '\u{1F300}');
    assert_eq!(pool[1375], '\u{1FAFF}');
    assert_eq!(pool[1376], 'A');
    assert_eq!(pool[1446], ')');
    let pool: HashSet<char> = pool.into_iter().collect();
    assert_eq!(pool.len(), 1447, "pool entries are distinct");

    let bytes = Corpus::ColorEmojiRandom.generate(SMOKE_SIZE, DEFAULT_SEED);
    let mut fg_seen = [false; 256];
    let mut bg_seen = [false; 256];
    let (mut frames, mut ascii, mut at) = (0_usize, 0_usize, 0_usize);
    while let Some((fg, after_fg)) = parse_sgr_256(&bytes, at, b"38") {
        let (bg, after_bg) = match parse_sgr_256(&bytes, after_fg, b"48") {
            Some(parsed) => parsed,
            None => break,
        };
        let ch = match first_char(&bytes[after_bg..]) {
            Some(ch) => ch,
            None => break,
        };
        assert!(fg < 256 && bg < 256, "frame {}: fg {} bg {}", frames, fg, bg);
        assert!(
            pool.contains(&ch),
            "frame {} prints {:?}, outside the pool",
            frames,
            ch
        );
        fg_seen[fg] = true;
        bg_seen[bg] = true;
        if ch.is_ascii() {
            ascii += 1;
        }
        frames += 1;
        at = after_bg + ch.len_utf8();
    }
    // Only a torn final frame (at most 2 x 11 + 4 bytes) may remain.
    assert!(bytes.len() - at < 26, "{} unparsed bytes at {at}", bytes.len() - at);
    assert!(
        fg_seen.iter().all(|&seen| seen) && bg_seen.iter().all(|&seen| seen),
        "fg and bg must both range over 0..=255"
    );
    // Expected 71 / 1447 = 4.9% ASCII and 24.993 bytes per frame; the bounds
    // sit more than 10 standard deviations out at ~42k frames.
    let ascii_share = ascii as f64 / frames as f64;
    assert!(
        (0.035..0.065).contains(&ascii_share),
        "ASCII share {}",
        ascii_share
    );
    let bytes_per_frame = at as f64 / frames as f64;
    assert!(
        (24.9..25.1).contains(&bytes_per_frame),
        "{} bytes per frame",
        bytes_per_frame
    );
    let full_size = bytes_per_frame * 30_000_000.0;
    assert!(
        (full_size - 749_801_000.0).abs() < 3_000_000.0,
        "30M frames come to {} bytes, the operator's file to 749,801,000",
        full_size
    );
}

fn log_bench_line(corpus: Corpus, run: &LaneRun) {
    eprintln!(
        "[BENCH] {}",
        serde_json::json!({
            "schema": "ft.bench.ingest-throughput.smoke.v1",
            "lane": run.lane.name(),
            "corpus": corpus.name(),
            "bytes": run.bytes,
            "secs": run.secs,
            "mib_per_s": run.mib_per_s(),
            "actions": run.actions,
            "debug_assertions": cfg!(debug_assertions),
        })
    );
}

#[test]
fn every_lane_keeps_the_grid_invariants_on_every_corpus() {
    let geometry = Geometry::default();
    for &corpus in Corpus::ALL.iter() {
        let data = corpus.generate(SMOKE_SIZE, DEFAULT_SEED);
        let expectation = Expectation::for_generated(&data, &geometry, corpus.is_line_oriented());
        if matches!(
            corpus,
            Corpus::SeqLines | Corpus::LongLines | Corpus::UnicodeMix
        ) {
            // Thousands of lines: the bound demands a full scrollback.
            assert_eq!(
                expectation.min_retained_rows,
                geometry.rows + geometry.scrollback,
                "{}",
                corpus.name()
            );
        }

        let mut action_counts = Vec::new();
        let mut summaries: Vec<(Lane, GridSummary)> = Vec::new();
        for &lane in Lane::ALL.iter() {
            let run = lanes::run_lane(lane, &[], &data, &geometry);
            assert_eq!(run.lane, lane);
            assert_eq!(run.bytes, SMOKE_SIZE);
            log_bench_line(corpus, &run);
            let (summary, verdict) = lanes::evaluate(&run, &geometry, &expectation);
            assert_eq!(
                verdict,
                Ok(()),
                "{} lane on {}",
                lane.name(),
                corpus.name()
            );
            if let Some(actions) = run.actions {
                assert!(actions > 0, "{} lane on {}", lane.name(), corpus.name());
                action_counts.push((lane, actions));
            }
            assert_eq!(summary.is_some(), lane != Lane::Parse);
            if let Some(summary) = summary {
                summaries.push((lane, summary));
            }
        }

        // parse, mux_two_stage and prod_config run one parser over the same chunks.
        assert_eq!(action_counts.len(), 3, "{}", corpus.name());
        assert!(
            action_counts.windows(2).all(|pair| pair[0].1 == pair[1].1),
            "{}: action counts differ: {action_counts:?}",
            corpus.name()
        );
        // The fused, two-stage and production-config paths leave one grid.
        assert_eq!(summaries.len(), 3, "{}", corpus.name());
        let (first_lane, first) = &summaries[0];
        for (lane, summary) in &summaries[1..] {
            assert_eq!(
                summary,
                first,
                "{} on {} diverges from {}",
                lane.name(),
                corpus.name(),
                first_lane.name()
            );
        }
        match corpus {
            Corpus::ColorEmojiRandom | Corpus::ColorRandom => assert!(
                first.nonblank_visible_cells >= geometry.rows * geometry.cols / 4,
                "{}: only {} visible cells hold text",
                corpus.name(),
                first.nonblank_visible_cells
            ),
            // Cursor addressing within the frame never scrolls.
            Corpus::TuiRepaint => assert_eq!(first.retained_rows, geometry.rows),
            Corpus::SeqLines | Corpus::LongLines | Corpus::UnicodeMix => {}
        }
    }
}

#[test]
fn the_sticky_zwj_prelude_runs_untimed_and_keeps_the_invariants() {
    let geometry = Geometry::default();
    let data = Corpus::ColorEmojiRandom.generate(64 * 1024, DEFAULT_SEED);
    let expectation = Expectation::for_generated(&data, &geometry, true);
    let mut summaries = Vec::new();
    for &lane in Lane::ALL.iter() {
        let run = lanes::run_lane(lane, lanes::STICKY_ZWJ_PRELUDE, &data, &geometry);
        assert_eq!(run.bytes, data.len(), "the prelude is not timed");
        assert_eq!(run.prelude_bytes, lanes::STICKY_ZWJ_PRELUDE.len());
        let (summary, verdict) = lanes::evaluate(&run, &geometry, &expectation);
        assert_eq!(verdict, Ok(()), "{} lane", lane.name());
        summaries.extend(summary);
    }
    assert_eq!(summaries.len(), 3);
    assert!(
        summaries.windows(2).all(|pair| pair[0] == pair[1]),
        "the terminal lanes diverge after the prelude"
    );
    // The prelude's ZWJ-tail cell stays on screen, so the grid differs from
    // a run without it.
    let plain = lanes::run_lane(Lane::Term, &[], &data, &geometry);
    let plain = lanes::summarize(plain.terminal.as_ref().expect("the term lane has a terminal"));
    assert_ne!(plain.fingerprint, summaries[0].fingerprint);
}

#[test]
fn the_sanity_checks_reject_a_lane_that_never_reached_the_grid() {
    let geometry = Geometry::default();
    let data = Corpus::SeqLines.generate(64 * 1024, DEFAULT_SEED);
    let expectation = Expectation::for_generated(&data, &geometry, true);
    let untouched = lanes::summarize(&lanes::new_terminal(Lane::Term, &geometry));
    let error = lanes::check_invariants(&untouched, &geometry, &expectation)
        .expect_err("an untouched terminal must fail");
    assert!(error.contains("rows retained"), "{}", error);

    let file_expectation = Expectation::for_file(&data, &geometry);
    let error = lanes::check_invariants(&untouched, &geometry, &file_expectation)
        .expect_err("an untouched terminal must fail");
    assert!(error.contains("no visible text"), "{}", error);

    let mut stray = untouched.clone();
    stray.nonblank_visible_cells = 1;
    stray.cursor_x = geometry.cols;
    let error = lanes::check_invariants(&stray, &geometry, &file_expectation)
        .expect_err("a cursor past the last column must fail");
    assert!(error.contains("cursor x"), "{}", error);

    let silent = LaneRun {
        lane: Lane::Parse,
        bytes: data.len(),
        prelude_bytes: 0,
        secs: 0.0,
        actions: Some(0),
        terminal: None,
    };
    let (summary, verdict) = lanes::evaluate(&silent, &geometry, &expectation);
    assert!(summary.is_none());
    assert_eq!(verdict, Err("the parser produced no actions".to_string()));
}

#[test]
fn the_cache_reuses_verified_corpora_and_regenerates_bad_ones() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("ingest-corpus-cache-{}", std::process::id()));
    let (corpus, size, seed) = (Corpus::ColorEmojiRandom, 8 * 1024, 99);
    let expected = corpus.generate(size, seed);
    let path = cache::corpus_path(&dir, corpus, size, seed);
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some("color_emoji_random-8192-0000000000000063.bin")
    );

    let first = cache::load_or_generate(&dir, corpus, size, seed).expect("generate");
    assert_eq!(first.outcome, CacheOutcome::Generated);
    assert_eq!(first.bytes, expected);
    assert_eq!(first.sha256, cache::sha256_hex(&expected));
    assert_eq!(first.path, path);

    let second = cache::load_or_generate(&dir, corpus, size, seed).expect("reuse");
    assert_eq!(second.outcome, CacheOutcome::Reused);
    assert_eq!(second.bytes, expected);
    assert_eq!(second.sha256, first.sha256);

    // Same length, different content: the SHA check catches it.
    let mut tampered = expected.clone();
    tampered[0] ^= 0x20;
    std::fs::write(&path, &tampered).expect("tamper");
    let third = cache::load_or_generate(&dir, corpus, size, seed).expect("regenerate");
    assert_eq!(third.outcome, CacheOutcome::Generated);
    assert_eq!(third.bytes, expected);

    // A truncated file is regenerated too.
    std::fs::write(&path, &expected[..size / 2]).expect("truncate");
    let fourth = cache::load_or_generate(&dir, corpus, size, seed).expect("regenerate");
    assert_eq!(fourth.outcome, CacheOutcome::Generated);

    // So is a corpus written by another generator version.
    let mut sidecar = path.as_os_str().to_owned();
    sidecar.push(".sha256");
    std::fs::write(&sidecar, format!("{} generator-v0\n", first.sha256)).expect("sidecar");
    let fifth = cache::load_or_generate(&dir, corpus, size, seed).expect("regenerate");
    assert_eq!(fifth.outcome, CacheOutcome::Generated);
    let sixth = cache::load_or_generate(&dir, corpus, size, seed).expect("reuse");
    assert_eq!(sixth.outcome, CacheOutcome::Reused);

    let external = cache::load_file(&path).expect("read file");
    assert_eq!(external.outcome, CacheOutcome::External);
    assert_eq!(external.sha256, first.sha256);
}

fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|arg| arg.to_string()).collect()
}

fn no_env(_: &str) -> Option<String> {
    None
}

#[test]
fn the_cli_parses_knobs_and_rejects_bad_input() {
    let defaults = cli::parse_args(&[], &no_env, Entry::Example).expect("defaults");
    assert_eq!(defaults.lanes, Lane::ALL.to_vec());
    assert_eq!(
        defaults.source,
        Source::Generated(vec![Corpus::ColorEmojiRandom])
    );
    assert_eq!(defaults.size, DEFAULT_SIZE);
    assert_eq!(defaults.seed, DEFAULT_SEED);
    assert_eq!(defaults.geometry, Geometry::default());
    assert!(!defaults.allow_debug);
    assert!(!defaults.sticky_zwj);
    let sticky = cli::parse_args(&args(&["--sticky-zwj"]), &no_env, Entry::Example)
        .expect("sticky");
    assert!(sticky.sticky_zwj);

    let env = |name: &str| match name {
        "ROWS" => Some("24".to_string()),
        "CHUNK" => Some("4KiB".to_string()),
        _ => None,
    };
    let options = cli::parse_args(
        &args(&[
            "--lane",
            "term,mux-two-stage",
            "--corpus=all",
            "--size",
            "10MB",
            "--seed",
            "0x10",
            "--cols",
            "200",
            "--scrollback=0",
            "--bench",
        ]),
        &env,
        Entry::Example,
    )
    .expect("flags");
    assert_eq!(options.lanes, vec![Lane::Term, Lane::MuxTwoStage]);
    assert_eq!(options.source, Source::Generated(Corpus::ALL.to_vec()));
    assert_eq!(options.size, 10_000_000);
    assert_eq!(options.seed, 16);
    assert_eq!(
        options.geometry,
        Geometry {
            rows: 24,
            cols: 200,
            scrollback: 0,
            chunk: 4096,
        }
    );
    let flag_beats_env =
        cli::parse_args(&args(&["--rows", "50"]), &env, Entry::Example).expect("rows");
    assert_eq!(flag_beats_env.geometry.rows, 50);

    let file = cli::parse_args(
        &args(&["--from-file", "/tmp/color-emoji-random.bin"]),
        &no_env,
        Entry::Example,
    )
    .expect("file");
    assert_eq!(
        file.source,
        Source::File(PathBuf::from("/tmp/color-emoji-random.bin"))
    );

    // cargo test --benches runs the bench target without --bench: quick mode.
    let quick = cli::parse_args(&[], &no_env, Entry::BenchTarget).expect("quick");
    assert_eq!(quick.size, cli::TEST_MODE_SIZE);
    assert!(quick.allow_debug);
    let bench = cli::parse_args(&args(&["--bench"]), &no_env, Entry::BenchTarget).expect("bench");
    assert_eq!(bench.size, DEFAULT_SIZE);
    assert!(!bench.allow_debug);

    assert_eq!(cli::parse_size("64MiB"), Ok(64 << 20));
    assert_eq!(cli::parse_size("1 k"), Ok(1024));
    assert_eq!(cli::parse_size("7"), Ok(7));
    for bad in [
        &["--frobnicate"][..],
        &["--lane", "vte"],
        &["--corpus", "nope"],
        &["--size"],
        &["--size", "12XB"],
        &["--rows", "0"],
        &["--chunk", "0"],
        &["--seed", "0xzz"],
        &["--corpus", "seq_lines", "--from-file", "x.bin"],
        &["--gen-only", "--from-file", "x.bin"],
    ]
    .iter()
    {
        assert!(
            cli::parse_args(&args(bad), &no_env, Entry::Example).is_err(),
            "{:?} must be rejected",
            bad
        );
    }
    let bad_env = |name: &str| (name == "COLS").then(|| "wide".to_string());
    assert!(cli::parse_args(&[], &bad_env, Entry::Example).is_err());
}

#[test]
fn each_lane_run_becomes_one_json_line_with_the_required_fields() {
    let geometry = Geometry::default();
    let corpus = Corpus::ColorEmojiRandom;
    let bytes = corpus.generate(32 * 1024, DEFAULT_SEED);
    let input = cli::Input {
        label: corpus.name().to_string(),
        seed: Some(DEFAULT_SEED),
        expectation: Expectation::for_generated(&bytes, &geometry, corpus.is_line_oriented()),
        corpus: cache::LoadedCorpus {
            sha256: cache::sha256_hex(&bytes),
            bytes,
            path: PathBuf::from("color_emoji_random.bin"),
            outcome: CacheOutcome::Generated,
        },
    };
    let context = cli::RecordContext {
        git_sha: "0123abc".to_string(),
        profile_dir: "release-perf".to_string(),
        debug_assertions: false,
    };
    for &lane in Lane::ALL.iter() {
        let run = lanes::run_lane(lane, &[], &input.corpus.bytes, &geometry);
        let (summary, verdict) = lanes::evaluate(&run, &geometry, &input.expectation);
        let outcome = cli::Outcome {
            summary,
            verdict,
            load_avg_1m: Some(1.5),
        };
        let record = cli::lane_record(&run, &input, &outcome, &geometry, &context);
        let line = record.to_string();
        assert!(!line.contains('\n'), "one line per run: {}", line);
        assert_eq!(record["lane"], lane.name());
        assert_eq!(record["corpus"], "color_emoji_random");
        assert_eq!(record["bytes"], 32 * 1024);
        assert_eq!(record["prelude_bytes"], 0);
        assert!(
            record["secs"].is_f64() && record["mib_per_s"].is_f64(),
            "{}",
            line
        );
        assert!(record["allocations"].is_null());
        assert_eq!(record["git_sha"], "0123abc");
        assert_eq!(record["profile"], "release-perf");
        assert_eq!(record["sanity"], "ok");
        assert_eq!(record["load_avg_1m"], 1.5);
        assert_eq!(record["corpus_sha256"], input.corpus.sha256.as_str());
        assert_eq!(record["cursor_x"].is_null(), lane == Lane::Parse, "{line}");
        assert_eq!(record["actions"].is_null(), lane == Lane::Term, "{line}");
    }
}
