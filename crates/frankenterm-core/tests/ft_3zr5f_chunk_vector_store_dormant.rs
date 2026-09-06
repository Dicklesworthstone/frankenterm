//! ft-3zr5f regression guard: `ChunkVectorStore` must remain dormant
//! (no production callers) until the orphan-vector architectural
//! decision is made per
//! [`docs/proposals/ft-3zr5f-semantic-chunk-orphan-decision.md`].
//!
//! # Why this guard exists
//!
//! `semantic_chunk_embeddings` lives in a standalone SQLite DB
//! (`crates/frankenterm-core/src/search/chunk_vector_store.rs`)
//! with no foreign key to `panes(pane_id)` or `output_segments(id)`.
//! SQLite can't express cross-DB FKs. So if the vector store ever
//! gets a production caller, every retention sweep / pane deletion
//! orphans rows in the vector DB and `semantic_search` can return
//! hits pointing at deleted segments.
//!
//! cod_4's audit on 2026-04-28 confirmed there is *currently* no
//! production caller — the vector store is shipped as a future-use
//! library. The companion proposal documents three architectural
//! options (rehome / mirror / explicit cleanup) and recommends one
//! when the trigger fires.
//!
//! This guard is the trip-wire: it scans `crates/` and
//! `frankenterm/` for any non-test reference to
//! `ChunkVectorStore::{open,new}` or `prune_chunks_through_ordinal`
//! and fails the test if one appears. The failure message points at
//! the proposal so the next agent doesn't accidentally introduce
//! the orphan-accumulation bug.
//!
//! # When to update the allowlist
//!
//! When a production caller is genuinely intended, do **all** of:
//!
//! 1. Read `docs/proposals/ft-3zr5f-semantic-chunk-orphan-decision.md`
//!    and pick Option A / B / C.
//! 2. Implement the chosen option (typically Option A: rehome the
//!    tables into the main storage DB).
//! 3. Update the proposal's "Decision" section to reflect the choice.
//! 4. Add the new file to `ALLOWLIST` below with a rationale comment.
//! 5. Land all of the above in the same commit so reviewers see the
//!    full picture.

use std::fs;
use std::path::{Path, PathBuf};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

/// Files allowed to reference `ChunkVectorStore` outside `tests/`.
/// Test-adjacent directories listed in `is_test_path` and inline
/// `#[cfg(test)]` modules are auto-allowed.
///
/// The library's own definition file is allowed because it both
/// declares and (in its `#[cfg(test)] mod tests`) exercises the
/// API. The line-level scan below skips lines inside `mod tests`
/// blocks so the inline tests don't trip the guard.
const ALLOWLIST: &[&str] = &["crates/frankenterm-core/src/search/chunk_vector_store.rs"];

/// Symbol patterns that, if found in production code, escalate the
/// dormant bug. Each entry is a substring matcher; the scan is
/// line-based and skips comment-only lines. Other occurrences are
/// conservatively reported, including references inside string literals.
const TRIPWIRE_PATTERNS: &[&str] = &[
    "ChunkVectorStore::open",
    "ChunkVectorStore::new",
    "prune_chunks_through_ordinal",
];

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .expect("expected frankenterm-core to live at <ws>/crates/frankenterm-core")
}

fn supported_path_roots(root: &Path) -> Vec<PathBuf> {
    vec![root.join("crates"), root.join("frankenterm")]
}

/// True when the file is a test or test-adjacent path. The guard
/// only fires on production code; test files exercising the API
/// are expected and don't contribute to the orphan-accumulation
/// bug because they tear down the DB on completion.
fn is_test_path(path: &Path) -> bool {
    let s = path.to_string_lossy();
    s.contains("/tests/")
        || s.contains("/benches/")
        || s.contains("/examples/")
        || s.contains("/fuzz/")
}

/// Walk `root` recursively, return every `.rs` file path that is
/// not under `target/` / `legacy_*` / `.beads/`. Reject symlinks rather
/// than silently losing coverage or following a directory cycle.
fn collect_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(root)
        .unwrap_or_else(|error| panic!("cannot scan {}: {error}", root.display()));
    for entry in entries {
        let entry = entry.expect("cannot read production source directory entry");
        let p = entry.path();
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("cannot inspect {}: {error}", p.display()));
        assert!(
            !file_type.is_symlink(),
            "source scan requires an explicit scope decision for symlink {}",
            p.display()
        );
        if file_type.is_dir() {
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == "target" || name == ".beads" || name == ".git" || name.starts_with("legacy_")
            {
                continue;
            }
            collect_rs_files(&p, out);
        } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Path>()
                .is_ok_and(|path| path.is_ident("test"))
    })
}

#[derive(Default)]
struct TestModuleSpans(Vec<std::ops::Range<usize>>);

impl<'ast> Visit<'ast> for TestModuleSpans {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        if test_only(&module.attrs) {
            self.0.push(module.span().byte_range());
        } else {
            visit::visit_item_mod(self, module);
        }
    }
}

/// Blank only syntax-proven test modules. Preserve every other source byte
/// and every newline so diagnostics refer to the original file. Braces in
/// comments, strings and raw strings cannot change the parsed module boundary.
fn strip_cfg_test_modules(text: &str) -> syn::Result<String> {
    let file = syn::parse_file(text)?;
    let mut spans = TestModuleSpans::default();
    if test_only(&file.attrs) {
        spans.0.push(0..text.len());
    } else {
        spans.visit_file(&file);
        // syn removes the BOM and shebang before tokenization. Its spans
        // therefore start after those bytes (but before the shebang newline).
        let stripped_prefix_len = usize::from(text.starts_with('\u{feff}')) * '\u{feff}'.len_utf8()
            + file.shebang.as_ref().map_or(0, String::len);
        for range in &mut spans.0 {
            range.start += stripped_prefix_len;
            range.end += stripped_prefix_len;
        }
    }
    spans.0.sort_unstable_by_key(|range| range.start);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for range in spans.0 {
        assert!(range.start >= cursor, "test module spans must not overlap");
        let prefix = text
            .get(cursor..range.start)
            .expect("valid UTF-8 module prefix");
        let excluded = text.get(range.clone()).expect("valid UTF-8 module span");
        out.push_str(prefix);
        for byte in excluded.bytes() {
            out.push(if byte == b'\n' { '\n' } else { ' ' });
        }
        cursor = range.end;
    }
    out.push_str(text.get(cursor..).expect("valid UTF-8 source suffix"));
    Ok(out)
}

/// Find tripwire-pattern hits in `text`, returning `(line_no, line_text)`
/// pairs for each match. Skips lines that look like comments
/// (`//` at the start) or doc-comments (`///`, `//!`).
fn scan_lines(text: &str) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("//") {
            continue;
        }
        if TRIPWIRE_PATTERNS.iter().any(|pat| line.contains(pat)) {
            hits.push((idx + 1, line.to_string()));
        }
    }
    hits
}

#[test]
fn test_module_literals_cannot_hide_following_production_calls() {
    let source = r###"#[cfg(test)]
mod tests {
    // {{{ comments do not open modules.
    const NORMAL: &str = "{{{";
    const RAW: &str = r#"{{{ ChunkVectorStore::new"#;
    fn fixture() { ChunkVectorStore::open("test.db"); }
}
fn production() {
    ChunkVectorStore::open("production.db");
    prune_chunks_through_ordinal(10);
}
"###;
    let cleaned = strip_cfg_test_modules(source).expect("valid Rust fixture");
    assert_eq!(
        scan_lines(&cleaned),
        vec![
            (9, "    ChunkVectorStore::open(\"production.db\");".into()),
            (10, "    prune_chunks_through_ordinal(10);".into()),
        ]
    );
}

#[test]
fn blanking_preserves_unicode_bytes_and_original_line_numbers() {
    let prefix = "// café 🦀\nconst LABEL: &str = \"日本語\";\n";
    let module =
        "#[cfg(test)]\nmod tests {\n    const LABEL: &str = \"é🦀 ChunkVectorStore::new\";\n}\n";
    let suffix = "fn café() { ChunkVectorStore::new(\"日本語\"); }\n";
    let source = format!("{prefix}{module}{suffix}");
    let cleaned = strip_cfg_test_modules(&source).expect("valid Unicode Rust");
    assert!(cleaned.starts_with(prefix));
    assert!(cleaned.ends_with(suffix));
    assert_eq!(cleaned.len(), source.len());
    assert_eq!(
        cleaned.match_indices('\n').collect::<Vec<_>>(),
        source.match_indices('\n').collect::<Vec<_>>()
    );
    assert_eq!(scan_lines(&cleaned), vec![(7, suffix.trim_end().into())]);
}

#[test]
fn nested_attributed_test_modules_are_excluded_without_overlapping_spans() {
    let source = r#"mod production {
    #[allow(dead_code)]
    #[cfg ( test )]
    pub(crate) mod fixtures {
        #[cfg(test)]
        mod nested { fn test() { ChunkVectorStore::new(); } }
        fn fixture() { ChunkVectorStore::open(); }
    }
    fn real() { ChunkVectorStore::new(); }
}
"#;
    let cleaned = strip_cfg_test_modules(source).expect("valid nested modules");
    assert_eq!(
        scan_lines(&cleaned),
        vec![(9, "    fn real() { ChunkVectorStore::new(); }".into())]
    );
}

#[test]
fn external_test_module_cannot_swallow_following_production_module() {
    let source =
        "#[cfg(test)] mod fixtures;\nmod production { fn real() { ChunkVectorStore::open(); } }\n";
    let cleaned = strip_cfg_test_modules(source).expect("valid external module");
    assert_eq!(
        scan_lines(&cleaned),
        vec![(
            2,
            "mod production { fn real() { ChunkVectorStore::open(); } }".into()
        )]
    );
    assert!(!cleaned.contains("fixtures"));
}

#[test]
fn ambiguous_cfg_and_cfg_attr_are_not_suppressed() {
    let source = r#"#[cfg(any(test, feature = "production"))]
mod shared { fn real() { ChunkVectorStore::new(); } }
#[cfg_attr(feature = "testing", cfg(test))]
mod conditional { fn real() { ChunkVectorStore::open(); } }
"#;
    assert_eq!(strip_cfg_test_modules(source).unwrap(), source);
    assert_eq!(scan_lines(source).len(), 2);
}

#[test]
fn test_only_file_is_blanked_but_keeps_line_and_byte_positions() {
    let source = "#![cfg(test)]\n// 🦀\nfn fixture() { ChunkVectorStore::new(); }\n";
    let cleaned = strip_cfg_test_modules(source).expect("valid test-only file");
    assert_eq!(cleaned.len(), source.len());
    assert!(cleaned.bytes().all(|byte| byte == b' ' || byte == b'\n'));
    assert_eq!(
        cleaned.match_indices('\n').collect::<Vec<_>>(),
        source.match_indices('\n').collect::<Vec<_>>()
    );
    assert!(scan_lines(&cleaned).is_empty());
}

#[test]
fn production_source_is_preserved_byte_for_byte() {
    let source = "// 🦀\nfn café() { ChunkVectorStore::open(\"日本語\"); }\n";
    assert_eq!(strip_cfg_test_modules(source).unwrap(), source);
    assert_eq!(scan_lines(source).len(), 1);
}

#[test]
fn bom_and_shebang_offsets_cannot_erase_preceding_production_calls() {
    for prefix in [
        "\u{feff}",
        "#!/usr/bin/env rust-script --long-argument-日本語\n",
        "\u{feff}#!/usr/bin/env rust-script --long-argument-日本語\n",
    ] {
        let before = "fn before() { ChunkVectorStore::open(\"é🦀\"); }\n";
        let module = "#[cfg(test)] mod fixtures { fn test() { ChunkVectorStore::new(); } }\n";
        let after = "fn after() { prune_chunks_through_ordinal(10); }\n";
        let source = format!("{prefix}{before}{module}{after}");
        let cleaned = strip_cfg_test_modules(&source).expect("valid prefixed source");
        assert!(cleaned.starts_with(&format!("{prefix}{before}")));
        assert!(cleaned.ends_with(after));
        assert_eq!(cleaned.len(), source.len());
        assert!(!cleaned.contains("ChunkVectorStore::new"));
        let hits = scan_lines(&cleaned);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 1 + usize::from(prefix.ends_with('\n')));
        assert_eq!(hits[1].0, 3 + usize::from(prefix.ends_with('\n')));
    }
}

#[test]
fn malformed_candidate_source_fails_closed() {
    assert!(strip_cfg_test_modules("#[cfg(test)] mod tests {\nChunkVectorStore::open();").is_err());
}

#[test]
fn unreadable_traversal_root_fails_closed() {
    // An existing regular file deterministically rejects read_dir, including
    // when the test runs as root and permission bits would not deny access.
    let non_directory = workspace_root().join("crates/frankenterm-core/Cargo.toml");
    assert!(non_directory.is_file());
    let outcome = std::panic::catch_unwind(|| collect_rs_files(&non_directory, &mut Vec::new()));
    assert!(
        outcome.is_err(),
        "failed traversal must not become an empty scan"
    );
}

#[test]
fn ft_3zr5f_chunk_vector_store_remains_dormant() {
    let root = workspace_root();
    let mut files: Vec<PathBuf> = Vec::new();
    for r in supported_path_roots(&root) {
        collect_rs_files(&r, &mut files);
    }

    let mut violations: Vec<String> = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(&root)
            .map(Path::to_path_buf)
            .unwrap_or_else(|_| file.clone());
        let rel_str = rel.to_string_lossy().to_string();

        if is_test_path(&rel) {
            continue;
        }
        if ALLOWLIST.contains(&rel_str.as_str()) {
            continue;
        }

        let text = fs::read_to_string(file)
            .unwrap_or_else(|error| panic!("cannot read {rel_str}: {error}"));
        // Files without any literal tripwire cannot contribute a hit. Parse
        // every candidate; malformed candidates must never pass silently.
        if !TRIPWIRE_PATTERNS
            .iter()
            .any(|pattern| text.contains(pattern))
        {
            continue;
        }
        let cleaned = strip_cfg_test_modules(&text)
            .unwrap_or_else(|error| panic!("cannot parse {rel_str}: {error}"));
        for (line_no, line) in scan_lines(&cleaned) {
            violations.push(format!("  {rel_str}:{line_no}  {}", line.trim_end()));
        }
    }

    if !violations.is_empty() {
        let mut msg = String::from(
            "\nft-3zr5f guard: ChunkVectorStore introduced into a production path \
             without resolving the orphan-vector question.\n\n",
        );
        msg.push_str(
            "Read docs/proposals/ft-3zr5f-semantic-chunk-orphan-decision.md \
                      and either implement Option A/B/C or update the ALLOWLIST in \
                      this test with a rationale comment in the same commit.\n\n",
        );
        msg.push_str("Tripwire patterns: ");
        msg.push_str(&TRIPWIRE_PATTERNS.join(", "));
        msg.push_str("\n\nViolations:\n");
        for v in &violations {
            msg.push_str(v);
            msg.push('\n');
        }
        panic!("{msg}");
    }
}
