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

/// Files allowed to reference `ChunkVectorStore` outside `tests/`.
/// Test files (anything under `tests/` or matching `test_*.rs` /
/// `*_test.rs` / inline `#[cfg(test)]` modules) are auto-allowed.
///
/// The library's own definition file is allowed because it both
/// declares and (in its `#[cfg(test)] mod tests`) exercises the
/// API. The line-level scan below skips lines inside `mod tests`
/// blocks so the inline tests don't trip the guard.
const ALLOWLIST: &[&str] = &["crates/frankenterm-core/src/search/chunk_vector_store.rs"];

/// Symbol patterns that, if found in production code, escalate the
/// dormant bug. Each entry is a substring matcher; the scan is
/// line-based and ignores comments + string literals via simple
/// heuristics (good enough for this guard's purpose).
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
/// not under `target/` / `legacy_*` / `.beads/`.
fn collect_rs_files(root: &Path, out: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
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

/// Strip `#[cfg(test)] mod tests { … }` blocks from a source file
/// so the inline test code in the library doesn't trip the guard.
/// Heuristic: find `#[cfg(test)]` followed by `mod` and balance
/// braces from there; emit the rest unchanged.
fn strip_cfg_test_modules(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `#[cfg(test)]` followed (within ~80 bytes of
        // whitespace + attributes) by `mod`. Conservative: only
        // strip the literal pattern.
        let needle = b"#[cfg(test)]";
        if i + needle.len() < bytes.len() && &bytes[i..i + needle.len()] == needle {
            // Skip whitespace + attrs; look for `mod`
            let mut j = i + needle.len();
            while j < bytes.len() && (bytes[j].is_ascii_whitespace() || bytes[j] == b'#') {
                if bytes[j] == b'#' {
                    while j < bytes.len() && bytes[j] != b']' {
                        j += 1;
                    }
                    if j < bytes.len() {
                        j += 1;
                    }
                } else {
                    j += 1;
                }
            }
            if j + 3 < bytes.len() && &bytes[j..j + 3] == b"mod" {
                // Balance braces from the next `{`
                while j < bytes.len() && bytes[j] != b'{' {
                    j += 1;
                }
                let mut depth = 0_i32;
                while j < bytes.len() {
                    match bytes[j] {
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                j += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    j += 1;
                }
                i = j;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
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

        let text = match fs::read_to_string(file) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let cleaned = strip_cfg_test_modules(&text);
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
