//! Integration tests for the cache-gpu-handle analyzer.
//!
//! Three layers of proof:
//!
//! 1. `bad_global_cache.rs` (pre-fix shape) MUST produce >=1 finding.
//! 2. `good_global_cache.rs` (post-fix shape) MUST be clean.
//! 3. The REAL `crates/frankenterm-gui/src` tree (where the leak lived
//!    and was fixed) MUST be clean — this both validates the fix and
//!    guards it against regression.
//!
//! **Class:** glyph-run-interner atlas-pinning GPU-memory leak.

use cache_gpu_handle_lint::{audit_dir, audit_dirs, FindingReason};
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    // lints/cache_gpu_handle -> repo root is two levels up.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

/// Audit a single fixture FILE by pointing the analyzer at a dir that
/// contains only that file (via the fixtures dir + filename filter).
fn audit_fixture(name: &str) -> Vec<cache_gpu_handle_lint::Finding> {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let report = audit_dir(&fixtures).expect("audit_dir on fixtures dir");
    report
        .findings
        .into_iter()
        .filter(|f| {
            f.rel_path
                .file_name()
                .map(|n| n == name)
                .unwrap_or(false)
        })
        .collect()
}

#[test]
fn bad_fixture_is_flagged() {
    let findings = audit_fixture("bad_global_cache.rs");
    assert!(
        !findings.is_empty(),
        "bad_global_cache.rs must produce >=1 finding (the pre-fix leak shape), got 0"
    );
    let f = &findings[0];
    assert_eq!(f.global, "BAD_SHAPED_RUN_INTERNER");
    assert_eq!(f.reason, FindingReason::GlobalReachesGpuHandle);
    // Reaches a forbidden GPU-handle leaf (CachedGlyph encountered before
    // Sprite on the BFS).
    assert!(
        f.forbidden_leaf == "CachedGlyph" || f.forbidden_leaf == "Sprite",
        "expected GPU-handle leaf, got {}",
        f.forbidden_leaf
    );
    // Witness path is root-first and ends at the leaf.
    assert!(
        f.path.first().unwrap().contains("BadInterner"),
        "witness must start at the root type, got {:?}",
        f.path
    );
    assert_eq!(f.path.last().unwrap(), &f.forbidden_leaf);
}

#[test]
fn good_fixture_is_clean() {
    let findings = audit_fixture("good_global_cache.rs");
    assert!(
        findings.is_empty(),
        "good_global_cache.rs (post-fix POD-only shape) must be clean, got {findings:#?}"
    );
}

#[test]
fn fixtures_dir_produces_exactly_one_finding() {
    // Across the whole fixtures dir, only the bad fixture is dirty.
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let report = audit_dir(&fixtures).unwrap();
    assert_eq!(
        report.findings.len(),
        1,
        "exactly one finding (bad fixture) expected, got {:#?}",
        report.findings
    );
    assert!(report.total_globals >= 3, "fixtures declare multiple globals");
}

#[test]
fn real_gui_src_is_clean() {
    // The leak lived in crates/frankenterm-gui/src/shapecache.rs and was
    // fixed by caching ShapedInfoTemplate (no Rc<CachedGlyph>). The whole
    // GUI src tree must now be clean. The window src is included in the
    // type graph so cross-crate leaves (Sprite, Texture2d) resolve.
    let gui_src = repo_root().join("crates/frankenterm-gui/src");
    let window_src = repo_root().join("frankenterm/window/src");
    if !gui_src.is_dir() {
        return;
    }
    let roots = if window_src.is_dir() {
        vec![gui_src.clone(), window_src]
    } else {
        vec![gui_src.clone()]
    };
    let report = audit_dirs(&roots).expect("audit runs against real GUI src");
    assert!(
        report.total_globals > 0,
        "the GUI src tree must contain process-global containers to scan"
    );
    assert!(
        report.findings.is_empty(),
        "real GUI src tree must be CLEAN post-fix — findings indicate a re-introduced \
         (or additional) GPU-handle leak: {:#?}",
        report
            .findings
            .iter()
            .map(|f| f.render())
            .collect::<Vec<_>>()
    );
}

#[test]
fn real_shapecache_dir_is_clean() {
    // Tighter guard: the exact dir holding the (fixed) interner.
    let gui_src = repo_root().join("crates/frankenterm-gui/src");
    let window_src = repo_root().join("frankenterm/window/src");
    if !gui_src.is_dir() {
        return;
    }
    let roots = if window_src.is_dir() {
        vec![gui_src, window_src]
    } else {
        vec![gui_src]
    };
    let report = audit_dirs(&roots).unwrap();
    let shapecache_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| {
            f.rel_path
                .to_string_lossy()
                .contains("shapecache.rs")
        })
        .collect();
    assert!(
        shapecache_findings.is_empty(),
        "shapecache.rs interner must not pin any GPU handle: {:#?}",
        shapecache_findings
            .iter()
            .map(|f| f.render())
            .collect::<Vec<_>>()
    );
}
