//! Regression-guard tests for the cx-propagation burn-down dashboard.
//!
//! **Bead:** [BR-RC-RUNTIME-SEMANTICS.G14.2] / `ft-t9a6q.2`.
//! **Doc:** `docs/runtime/cx-propagation-burndown.md`.
//! **Generator:** `scripts/cx_propagation_burndown.py`.
//!
//! These tests assert on the dashboard's schema + invariant
//! shape so a future commit cannot silently break the release-
//! attestation surface or the trend-file consumers under
//! ft-t9a6q.2.cont.attestation / ft-t9a6q.2.cont.cron.
//!
//! What's verified:
//! - The snapshot JSON parses + carries the documented top-level keys.
//! - `schema_version` matches the doc.
//! - The 8 documented buckets all exist (7 priority + `other`).
//! - `totals.uncovered_sites` is 0 (regression guard — if a future
//!   commit reintroduces an uncovered `pub async fn`, this test
//!   fails before the file even reaches CI).
//! - `totals.total_sites` accounts for every site exactly once
//!   (covered + wrapper_exempt + exempt_file + uncovered).
//! - Per-bucket `total` sums across all buckets equal the
//!   top-level `total_sites` (no site dropped on the floor).
//!
//! What's intentionally NOT verified:
//! - The exact site counts per bucket — those drift naturally
//!   as code lands and the dashboard records that drift in the
//!   trend file. Pinning the count here would create churn.
//!
//! The test parses the on-disk snapshot rather than re-running
//! the audit script — the audit script's contract is a separate
//! test (under ft-3kv6e). This test is the *consumer* contract
//! for the dashboard.

use std::path::PathBuf;

fn snapshot_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/runtime/cx-propagation.json")
}

/// Cheap recursive JSON value type — enough to parse the dashboard
/// shape without pulling in serde_json. The dashboard JSON is
/// straightforward (numbers, strings, objects, arrays); a
/// hand-rolled parser keeps this test crate lean.
#[derive(Debug, Clone)]
enum Json {
    Null,
    Bool,
    Num(f64),
    Str(String),
    Arr,
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn as_obj(&self) -> &[(String, Json)] {
        if let Json::Obj(o) = self {
            o
        } else {
            panic!("expected object, got {self:?}")
        }
    }
    fn get(&self, key: &str) -> &Json {
        for (k, v) in self.as_obj() {
            if k == key {
                return v;
            }
        }
        panic!("key `{key}` missing in {self:?}");
    }
    fn as_int(&self) -> i64 {
        if let Json::Num(n) = self {
            *n as i64
        } else {
            panic!("expected number, got {self:?}")
        }
    }
    fn as_str_owned(&self) -> String {
        if let Json::Str(s) = self {
            s.clone()
        } else {
            panic!("expected string, got {self:?}")
        }
    }
}

fn parse(src: &str) -> Json {
    let mut p = Parser {
        src: src.as_bytes(),
        pos: 0,
    };
    p.skip_ws();
    let v = p.value();
    p.skip_ws();
    v
}

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn peek(&self) -> u8 {
        self.src[self.pos]
    }
    fn bump(&mut self) -> u8 {
        let b = self.src[self.pos];
        self.pos += 1;
        b
    }
    fn skip_ws(&mut self) {
        while self.pos < self.src.len() && self.src[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }
    fn expect(&mut self, b: u8) {
        let got = self.bump();
        assert_eq!(
            got, b,
            "expected `{}`, got `{}` at pos {}",
            b as char, got as char, self.pos
        );
    }
    fn value(&mut self) -> Json {
        self.skip_ws();
        match self.peek() {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => Json::Str(self.string()),
            b't' | b'f' => self.bool(),
            b'n' => self.null(),
            _ => self.number(),
        }
    }
    fn object(&mut self) -> Json {
        self.expect(b'{');
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == b'}' {
            self.bump();
            return Json::Obj(entries);
        }
        loop {
            self.skip_ws();
            let k = self.string();
            self.skip_ws();
            self.expect(b':');
            let v = self.value();
            entries.push((k, v));
            self.skip_ws();
            match self.bump() {
                b',' => {}
                b'}' => break,
                other => panic!("expected `,` or `}}`, got `{}`", other as char),
            }
        }
        Json::Obj(entries)
    }
    fn array(&mut self) -> Json {
        self.expect(b'[');
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == b']' {
            self.bump();
            return Json::Arr;
        }
        loop {
            let v = self.value();
            items.push(v);
            self.skip_ws();
            match self.bump() {
                b',' => {}
                b']' => break,
                other => panic!("expected `,` or `]`, got `{}`", other as char),
            }
        }
        Json::Arr
    }
    fn string(&mut self) -> String {
        self.expect(b'"');
        let mut out = String::new();
        loop {
            let b = self.bump();
            match b {
                b'"' => break,
                b'\\' => {
                    let next = self.bump();
                    match next {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'n' => out.push('\n'),
                        b't' => out.push('\t'),
                        b'/' => out.push('/'),
                        other => out.push(other as char),
                    }
                }
                _ => out.push(b as char),
            }
        }
        out
    }
    fn bool(&mut self) -> Json {
        if self.src[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Json::Bool
        } else if self.src[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Json::Bool
        } else {
            panic!("expected bool at pos {}", self.pos)
        }
    }
    fn null(&mut self) -> Json {
        assert!(self.src[self.pos..].starts_with(b"null"));
        self.pos += 4;
        Json::Null
    }
    fn number(&mut self) -> Json {
        let start = self.pos;
        if self.peek() == b'-' {
            self.bump();
        }
        while self.pos < self.src.len() {
            let c = self.src[self.pos];
            if c.is_ascii_digit() || c == b'.' || c == b'e' || c == b'E' || c == b'+' || c == b'-' {
                self.pos += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.src[start..self.pos]).unwrap();
        Json::Num(
            s.parse::<f64>()
                .unwrap_or_else(|e| panic!("bad number `{s}` at pos {start}: {e}")),
        )
    }
}

fn load_snapshot() -> Json {
    let path = snapshot_path();
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "snapshot {} missing — run `scripts/cx_propagation_burndown.py` to generate: {e}",
            path.display()
        )
    });
    parse(&src)
}

#[test]
fn snapshot_has_documented_top_level_keys() {
    let snap = load_snapshot();
    let _schema = snap.get("schema_version");
    let _generated_at = snap.get("generated_at");
    let _totals = snap.get("totals");
    let _buckets = snap.get("buckets");
}

#[test]
fn schema_version_matches_documented_value() {
    let snap = load_snapshot();
    assert_eq!(
        snap.get("schema_version").as_int(),
        2,
        "schema version drifted — bump SCHEMA_VERSION in the generator + the doc"
    );
}

#[test]
fn labruntime_coverage_subkey_exists_with_documented_shape() {
    // ft-y9wxt: labruntime_coverage subkey added in schema v2.
    // Asserts the documented top-level fields exist.
    let snap = load_snapshot();
    let lab = snap.get("labruntime_coverage");
    let _ = lab.get("total_call_sites");
    let _ = lab.get("files_with_calls");
    let _ = lab.get("tested_files_intersect_covered");
    let by_bucket = lab.get("by_bucket");
    for name in &[
        "capture",
        "workflow",
        "web_sse",
        "mcp",
        "distributed",
        "connectors",
        "tx",
        "other",
    ] {
        let bucket = by_bucket.get(name);
        let _ = bucket.get("call_sites");
        let _ = bucket.get("files_with_calls");
        let _ = bucket.get("files");
    }
}

#[test]
fn labruntime_per_bucket_call_sites_sum_to_total() {
    // ft-y9wxt: arithmetic consistency for the labruntime coverage
    // metric — every call site lands in exactly one bucket.
    let snap = load_snapshot();
    let lab = snap.get("labruntime_coverage");
    let total = lab.get("total_call_sites").as_int();
    let by_bucket = lab.get("by_bucket");
    let mut bucket_sum = 0i64;
    for name in &[
        "capture",
        "workflow",
        "web_sse",
        "mcp",
        "distributed",
        "connectors",
        "tx",
        "other",
    ] {
        bucket_sum += by_bucket.get(name).get("call_sites").as_int();
    }
    assert_eq!(
        bucket_sum, total,
        "labruntime per-bucket call_sites ({bucket_sum}) must sum to total_call_sites ({total})"
    );
}

#[test]
fn generated_at_is_iso8601_utc() {
    let snap = load_snapshot();
    let s = snap.get("generated_at").as_str_owned();
    assert!(
        s.ends_with('Z'),
        "generated_at must be UTC (trailing Z), got {s}"
    );
    assert!(
        s.contains('T'),
        "generated_at must be ISO-8601 (contains T), got {s}"
    );
}

#[test]
fn all_eight_documented_buckets_exist() {
    let snap = load_snapshot();
    let buckets = snap.get("buckets");
    for name in &[
        "capture",
        "workflow",
        "web_sse",
        "mcp",
        "distributed",
        "connectors",
        "tx",
        "other",
    ] {
        let _ = buckets.get(name);
    }
}

#[test]
fn totals_arithmetic_is_consistent() {
    let snap = load_snapshot();
    let totals = snap.get("totals");
    let total = totals.get("total_sites").as_int();
    let covered = totals.get("covered_sites").as_int();
    let wrapper_exempt = totals.get("wrapper_exempt_sites").as_int();
    let exempt_file = totals.get("exempt_file_sites").as_int();
    let uncovered = totals.get("uncovered_sites").as_int();
    assert_eq!(
        total,
        covered + wrapper_exempt + exempt_file + uncovered,
        "totals arithmetic broke: total({total}) != covered({covered}) + wrapper_exempt({wrapper_exempt}) + exempt_file({exempt_file}) + uncovered({uncovered})"
    );
}

#[test]
fn uncovered_sites_is_zero_regression_guard() {
    // Regression guard: any commit that reintroduces an uncovered
    // pub async fn will fail this test before CI runs the audit
    // script. Since the snapshot is checked in, the offending
    // commit must regenerate the snapshot — and the regenerated
    // snapshot would carry uncovered > 0, which fails here.
    let snap = load_snapshot();
    let uncovered = snap.get("totals").get("uncovered_sites").as_int();
    assert_eq!(
        uncovered, 0,
        "cx-propagation regression: {uncovered} uncovered pub async fn site(s). \
         Run `scripts/check_runtime_proof_coverage.py` for the list."
    );
}

#[test]
fn per_bucket_totals_sum_to_top_level_total() {
    let snap = load_snapshot();
    let total = snap.get("totals").get("total_sites").as_int();
    let buckets = snap.get("buckets");
    let mut bucket_sum = 0i64;
    for name in &[
        "capture",
        "workflow",
        "web_sse",
        "mcp",
        "distributed",
        "connectors",
        "tx",
        "other",
    ] {
        bucket_sum += buckets.get(name).get("total").as_int();
    }
    assert_eq!(
        bucket_sum, total,
        "per-bucket totals ({bucket_sum}) must sum to top-level total ({total}) — \
         a site fell through the categorization"
    );
}

#[test]
fn each_bucket_has_documented_subkeys() {
    let snap = load_snapshot();
    let buckets = snap.get("buckets");
    for name in &[
        "capture",
        "workflow",
        "web_sse",
        "mcp",
        "distributed",
        "connectors",
        "tx",
        "other",
    ] {
        let bucket = buckets.get(name);
        let _ = bucket.get("total");
        let _ = bucket.get("covered");
        let _ = bucket.get("wrapper_exempt");
        let _ = bucket.get("exempt_file");
        let _ = bucket.get("uncovered");
        let _ = bucket.get("files");
    }
}
