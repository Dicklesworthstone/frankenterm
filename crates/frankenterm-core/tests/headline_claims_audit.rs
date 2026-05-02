//! Audit harness for `docs/perf/headline-claims.json`.
//!
//! **Bead:** [BR-RC-FOUNDATION.G3.5] / `ft-f5zyr`.
//! **Manifest:** `docs/perf/headline-claims.json` (ft-syqcz.3).
//! **Bench schema:** `docs/perf/bench-stats-schema.json` (ft-syqcz.2).
//! **Doc:** `docs/perf/headline-claims-audit.md`.
//!
//! # What this harness verifies
//!
//! Substrate-level regression guards that don't require running
//! the actual benches. Each test asserts on a stable invariant
//! of the manifest + bench-output contract:
//!
//! 1. The manifest parses + carries the documented top-level
//!    keys + has exactly 5 claims (the headline set).
//! 2. Each claim names a `bench.file` that exists at the
//!    expected path. CI fails fast if a bench is renamed or
//!    deleted without updating the manifest.
//! 3. Each claim names a `criterion_group` matching what the
//!    bench source actually declares (regex check on the bench
//!    source — accepts `criterion_group!(<group> ... )` or
//!    `BenchmarkGroup::new(... "<group>" ...)` shapes).
//! 4. Each claim has a `metric.kind` from the documented
//!    enumeration and a `comparison_direction` that's
//!    semantically consistent with `kind`.
//! 5. The `bench-stats-schema.json` parses + has the
//!    documented `$id` + the documented `$defs.distribution`.
//!
//! # What this harness intentionally does NOT verify
//!
//! - Actual bench *runs* — those are wired-pass: producing
//!   Distribution JSON artifacts is `cargo bench` infrastructure
//!   that requires hardware-baseline runners. Filed as
//!   `ft-f5zyr.cont.runs`.
//! - Corpus content — pinning concrete agent-output samples is
//!   filed as `ft-f5zyr.cont.corpus`. The `MANIFEST` schema
//!   ships under this bead.
//! - Per-PR regression gating — that's `ft-9zzkg` (already
//!   referenced from the manifest's `regression_gate` block).
//!
//! The harness reads the manifest + bench source files. It does
//! NOT shell out to `cargo bench`.

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn manifest_path() -> PathBuf {
    repo_root().join("docs/perf/headline-claims.json")
}

fn schema_path() -> PathBuf {
    repo_root().join("docs/perf/bench-stats-schema.json")
}

/// Cheap manifest parser — full structural parse, but reads
/// only the fields we need to assert against. Keeps the test
/// crate lean (no serde_json dependency for this one consumer).
struct Manifest {
    claims: Vec<Claim>,
}

struct Claim {
    id: String,
    bench_file: String,
    criterion_group: String,
    metric_kind: String,
    comparison_direction: String,
}

impl Manifest {
    fn load() -> Self {
        let src = fs::read_to_string(manifest_path()).expect("manifest file readable");
        Self::parse(&src)
    }

    fn parse(src: &str) -> Self {
        let claims_block = extract_array(src, "\"claims\"").expect("claims array present");
        let claim_objects = split_top_level_objects(&claims_block);
        let claims = claim_objects
            .into_iter()
            .map(|obj| Claim::parse(&obj))
            .collect();
        Self { claims }
    }
}

impl Claim {
    fn parse(obj: &str) -> Self {
        let id = extract_string(obj, "\"id\"").expect("id present");
        let bench_block = extract_object(obj, "\"bench\"").expect("bench block present");
        let bench_file = extract_string(&bench_block, "\"file\"").expect("bench.file present");
        let criterion_group =
            extract_string(&bench_block, "\"criterion_group\"").expect("criterion_group present");
        let metric_block = extract_object(obj, "\"metric\"").expect("metric block present");
        let metric_kind = extract_string(&metric_block, "\"kind\"").expect("metric.kind present");
        let comparison_direction = extract_string(&metric_block, "\"comparison_direction\"")
            .expect("comparison_direction present");
        Self {
            id,
            bench_file,
            criterion_group,
            metric_kind,
            comparison_direction,
        }
    }
}

fn extract_string(haystack: &str, key: &str) -> Option<String> {
    let key_pos = haystack.find(key)?;
    let after_key = &haystack[key_pos + key.len()..];
    let colon = after_key.find(':')?;
    let after_colon = &after_key[colon + 1..];
    let mut chars = after_colon.char_indices();
    while let Some((_, c)) = chars.next() {
        if c == '"' {
            break;
        }
    }
    let mut out = String::new();
    let mut prev_was_backslash = false;
    for (_, c) in chars {
        if prev_was_backslash {
            out.push(c);
            prev_was_backslash = false;
        } else if c == '\\' {
            prev_was_backslash = true;
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None
}

fn extract_object(haystack: &str, key: &str) -> Option<String> {
    let key_pos = haystack.find(key)?;
    let after_key = &haystack[key_pos + key.len()..];
    let brace = after_key.find('{')?;
    let body_start = brace;
    let mut depth = 0;
    let mut in_string = false;
    let mut prev_was_backslash = false;
    for (i, c) in after_key.char_indices().skip(body_start) {
        if in_string {
            if prev_was_backslash {
                prev_was_backslash = false;
            } else if c == '\\' {
                prev_was_backslash = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(after_key[body_start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn extract_array(haystack: &str, key: &str) -> Option<String> {
    let key_pos = haystack.find(key)?;
    let after_key = &haystack[key_pos + key.len()..];
    let bracket = after_key.find('[')?;
    let body_start = bracket;
    let mut depth = 0;
    let mut in_string = false;
    let mut prev_was_backslash = false;
    for (i, c) in after_key.char_indices().skip(body_start) {
        if in_string {
            if prev_was_backslash {
                prev_was_backslash = false;
            } else if c == '\\' {
                prev_was_backslash = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(after_key[body_start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_objects(arr: &str) -> Vec<String> {
    let inner = arr.trim().strip_prefix('[').unwrap_or(arr).trim();
    let inner = inner.strip_suffix(']').unwrap_or(inner);
    let mut out = Vec::new();
    let mut depth = 0;
    let mut start: Option<usize> = None;
    let mut in_string = false;
    let mut prev_was_backslash = false;
    for (i, c) in inner.char_indices() {
        if in_string {
            if prev_was_backslash {
                prev_was_backslash = false;
            } else if c == '\\' {
                prev_was_backslash = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => {
                if depth == 0 {
                    start = Some(i);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        out.push(inner[s..=i].to_string());
                    }
                    start = None;
                }
            }
            _ => {}
        }
    }
    out
}

#[test]
fn manifest_parses_with_exactly_five_headline_claims() {
    let m = Manifest::load();
    assert_eq!(
        m.claims.len(),
        5,
        "headline-claims manifest must list exactly the 5 documented claims, got {}",
        m.claims.len()
    );
}

#[test]
fn manifest_carries_documented_claim_ids() {
    let m = Manifest::load();
    let ids: Vec<&str> = m.claims.iter().map(|c| c.id.as_str()).collect();
    for expected in &[
        "capture_latency_p99",
        "concurrent_panes_capacity",
        "memory_per_pane_budget",
        "zstd_compression_ratio",
        "bloom_prefilter_speedup",
    ] {
        assert!(
            ids.contains(expected),
            "manifest missing claim `{expected}`. Got: {ids:?}"
        );
    }
}

#[test]
fn every_claim_names_a_bench_file_that_exists() {
    let m = Manifest::load();
    for claim in &m.claims {
        let path = repo_root().join(&claim.bench_file);
        assert!(
            path.is_file(),
            "claim `{}` names bench file `{}` which does not exist at {}",
            claim.id,
            claim.bench_file,
            path.display(),
        );
    }
}

#[test]
fn every_claim_criterion_group_appears_in_the_named_bench_source() {
    let m = Manifest::load();
    for claim in &m.claims {
        let path = repo_root().join(&claim.bench_file);
        let src = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "claim `{}` bench file `{}` unreadable: {e}",
                claim.id, claim.bench_file
            )
        });
        // Some criterion_group values are paths like
        // "mux_pool/throughput_scaling" — we accept either:
        // (a) the head segment appears in a `criterion_group!`
        //     macro invocation, OR
        // (b) the whole string appears literally in the source
        //     (covers `BenchmarkGroup::new(..., "<group>", ...)`).
        let head_segment = claim
            .criterion_group
            .split('/')
            .next()
            .unwrap_or(&claim.criterion_group);
        let literal_present = src.contains(&claim.criterion_group);
        let head_in_macro =
            src.contains(&format!("criterion_group!")) && src.contains(head_segment);
        assert!(
            literal_present || head_in_macro,
            "claim `{}` names criterion_group `{}` but neither the literal string nor `criterion_group!(...{head_segment}...)` appears in {}",
            claim.id,
            claim.criterion_group,
            claim.bench_file
        );
    }
}

#[test]
fn every_claim_metric_kind_is_from_documented_enumeration() {
    let m = Manifest::load();
    for claim in &m.claims {
        match claim.metric_kind.as_str() {
            "latency" | "throughput" | "memory" | "compression_ratio" | "speedup_ratio" => {}
            other => panic!(
                "claim `{}` has metric.kind `{other}` not in documented enumeration: latency / throughput / memory / compression_ratio / speedup_ratio",
                claim.id
            ),
        }
    }
}

#[test]
fn every_claim_comparison_direction_is_semantically_consistent_with_kind() {
    let m = Manifest::load();
    for claim in &m.claims {
        let dir = claim.comparison_direction.as_str();
        match (claim.metric_kind.as_str(), dir) {
            ("latency", "lower_is_better") => {}
            ("memory", "lower_is_better") => {}
            ("throughput", "higher_is_better") => {}
            ("compression_ratio", "higher_is_better") => {}
            ("speedup_ratio", "higher_is_better") => {}
            (kind, dir) => panic!(
                "claim `{}` has metric.kind=`{kind}` paired with comparison_direction=`{dir}` — inconsistent. \
                 latency/memory must be lower_is_better; throughput/compression_ratio/speedup_ratio must be higher_is_better.",
                claim.id
            ),
        }
    }
}

#[test]
fn bench_stats_schema_parses_and_has_documented_fields() {
    let src = fs::read_to_string(schema_path()).expect("schema readable");
    assert!(
        src.contains("\"$id\": \"https://frankenterm.dev/schemas/bench-stats/v1.json\""),
        "bench-stats-schema.json missing the documented $id"
    );
    assert!(src.contains("\"$defs\""), "schema must declare $defs");
    assert!(
        src.contains("\"distribution\""),
        "schema must define $defs.distribution"
    );
    assert!(
        src.contains("\"percentile\""),
        "schema must define $defs.percentile"
    );
    // The schema's distribution requires p50/p95/p99/p99.9 — guard
    // the documented minimum reporting set via the `contains` rule
    // on q==0.999.
    assert!(
        src.contains("\"q\": { \"const\": 0.999 }"),
        "schema must require p99.9 via the contains rule"
    );
}

#[test]
fn corpus_manifest_exists_for_zstd_compression_ratio() {
    // The zstd_compression_ratio claim needs a pinned corpus
    // under fixtures/perf/agent-output-corpus/. ft-f5zyr shipped
    // the MANIFEST schema (the substrate); ft-ym2y1 pinned the
    // sha256 column and verified ratio bands (see
    // `corpus_pinned_hashes_match_files_on_disk` and
    // `corpus_compression_ratios_satisfy_documented_bands`
    // below for the post-pin regression guards).
    let manifest = repo_root().join("fixtures/perf/agent-output-corpus/MANIFEST");
    assert!(
        manifest.is_file(),
        "fixtures/perf/agent-output-corpus/MANIFEST must exist (corpus pinning substrate)"
    );
    let src = fs::read_to_string(&manifest).unwrap();
    // The MANIFEST is keyed by fixture name → sha256.
    assert!(
        src.contains("repetitive"),
        "MANIFEST must declare a `repetitive` fixture"
    );
    assert!(
        src.contains("heterogeneous_agent_log"),
        "MANIFEST must declare a `heterogeneous_agent_log` fixture"
    );
    assert!(
        src.contains("compressed_already"),
        "MANIFEST must declare a `compressed_already` fixture"
    );
}

/// Parses MANIFEST data lines. Each non-comment, non-blank line
/// must split into exactly 3 whitespace-separated fields:
/// `<fixture_name> <sha256_hex> <relative_filename>`.
fn parse_corpus_manifest() -> Vec<(String, String, String)> {
    let manifest_path = repo_root().join("fixtures/perf/agent-output-corpus/MANIFEST");
    let src = fs::read_to_string(&manifest_path).expect("MANIFEST readable");
    let mut entries = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = trimmed.split_whitespace().collect();
        assert_eq!(
            parts.len(),
            3,
            "MANIFEST line `{trimmed}` must have exactly 3 \
             whitespace-separated fields: name, sha256, filename"
        );
        entries.push((parts[0].to_string(), parts[1].to_string(), parts[2].to_string()));
    }
    entries
}

#[test]
fn corpus_pinned_hashes_match_files_on_disk() {
    // br-ft-ym2y1 / BR-RC-FOUNDATION.G3.5.cont.corpus.
    // The MANIFEST is the canonical source of truth for what
    // bytes the headline-claim bench MUST consume. If any sample
    // file drifts (re-encoded, accidentally rewritten by a
    // sibling agent, or replaced by a non-canonical version),
    // its sha256 will diverge from the MANIFEST entry and this
    // test will name the divergent fixture.
    use sha2::{Digest, Sha256};

    let entries = parse_corpus_manifest();
    assert_eq!(
        entries.len(),
        3,
        "MANIFEST must declare exactly the 3 documented fixtures"
    );

    let corpus_dir = repo_root().join("fixtures/perf/agent-output-corpus");
    for (name, expected_sha, filename) in &entries {
        // Reject any leftover TBD placeholders. ft-f5zyr shipped
        // the substrate with `TBD-cont-corpus` placeholders;
        // ft-ym2y1 replaced them. A future regression that
        // restores the placeholder must fail loudly here.
        assert!(
            !expected_sha.starts_with("TBD"),
            "fixture `{name}` still has placeholder sha256 `{expected_sha}` \
             — corpus pinning (br-ft-ym2y1) was reverted"
        );
        assert_eq!(
            expected_sha.len(),
            64,
            "fixture `{name}` sha256 `{expected_sha}` must be 64 lowercase hex chars"
        );

        let path = corpus_dir.join(filename);
        let bytes = fs::read(&path).unwrap_or_else(|e| {
            panic!("fixture `{filename}` missing on disk: {e}")
        });
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let actual_sha = format!("{:x}", hasher.finalize());

        assert_eq!(
            &actual_sha, expected_sha,
            "fixture `{name}` ({filename}): on-disk sha256 `{actual_sha}` \
             diverges from MANIFEST-pinned `{expected_sha}`. Either the file \
             was modified or the MANIFEST is stale; both are bench-blocking \
             for the zstd_compression_ratio headline claim."
        );
    }
}

#[test]
fn corpus_compression_ratios_satisfy_documented_bands() {
    // br-ft-ym2y1 / BR-RC-FOUNDATION.G3.5.cont.corpus
    // acceptance criterion 4: each fixture must compress with
    // zstd into the ratio band documented in MANIFEST + in
    // docs/perf/headline-claims.json:
    //   repetitive             > 8:1   (high-redundancy)
    //   heterogeneous_agent_log  5:1 — 8:1 (typical case)
    //   compressed_already       ≤ 1.1:1 (lower-bound)
    //
    // We use zstd level 3 (the workspace's default) so this test
    // tracks what a real compressor would produce on the bench
    // path. A regression that swaps a sample for content outside
    // its band would silently violate the headline claim's
    // workload contract — this guard catches it pre-merge.
    use std::io::Cursor;

    let corpus_dir = repo_root().join("fixtures/perf/agent-output-corpus");
    // (fixture name, lower bound exclusive, upper bound inclusive).
    // Use f64::INFINITY for the upper end of the high-redundancy
    // band — the documented contract is `> 8:1` with no ceiling.
    let bands: &[(&str, &str, f64, f64)] = &[
        ("repetitive", "repetitive_sample.txt", 8.0, f64::INFINITY),
        (
            "heterogeneous_agent_log",
            "heterogeneous_agent_log_sample.txt",
            5.0,
            8.0,
        ),
        ("compressed_already", "compressed_already_sample.bin", 0.0, 1.1),
    ];

    for (name, filename, lower_excl, upper_incl) in bands {
        let path = corpus_dir.join(filename);
        let bytes = fs::read(&path).unwrap_or_else(|e| {
            panic!("fixture `{filename}` missing on disk: {e}")
        });
        let orig = bytes.len();
        let compressed = zstd::stream::encode_all(Cursor::new(&bytes), 3)
            .expect("zstd encode_all");
        let comp = compressed.len();
        let ratio = orig as f64 / comp as f64;
        // For the upper-bounded bands, fail if ratio > upper.
        // For the unbounded band (repetitive), upper_incl is
        // f64::INFINITY so the comparison is always false.
        let above_upper = ratio > *upper_incl;
        let at_or_below_lower = ratio <= *lower_excl;
        assert!(
            !above_upper && !at_or_below_lower,
            "fixture `{name}` ({filename}): \
             zstd-3 ratio {ratio:.2}:1 (orig={orig} comp={comp}) is outside \
             documented band ({lower_excl}:1, {upper_incl}:1]. The headline \
             claim's workload contract requires this fixture to land in band."
        );
    }
}

#[test]
fn manifest_has_publishing_block_with_regression_gate() {
    let src = fs::read_to_string(manifest_path()).expect("manifest readable");
    let publishing = extract_object(&src, "\"publishing\"").expect("publishing block present");
    assert!(
        publishing.contains("\"regression_gate\""),
        "publishing block must declare a regression_gate"
    );
    let gate = extract_object(&publishing, "\"regression_gate\"").expect("regression_gate present");
    let method = extract_string(&gate, "\"method\"").expect("method present");
    assert!(
        ["ebci-upper-bound", "mann-whitney-u", "ks"].contains(&method.as_str()),
        "regression_gate.method `{method}` must be one of the documented tests"
    );
}
