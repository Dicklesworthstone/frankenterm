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
        .filter(|f| f.rel_path.file_name().map(|n| n == name).unwrap_or(false))
        .collect()
}

#[test]
fn bad_fixture_is_flagged() {
    let findings = audit_fixture("bad_global_cache.rs");
    assert!(
        !findings.is_empty(),
        "bad_global_cache.rs must produce >=1 finding (the pre-fix leak shape), got 0"
    );
    let f = findings
        .iter()
        .find(|f| f.reason == FindingReason::GlobalReachesGpuHandle)
        .expect("the process-global interner must be flagged");
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
    // Across the whole fixtures dir, only the bad fixture is dirty: its
    // global, and (ft-yccm0.2.5 cache-field rule) the interner's map field
    // that holds the glyphs.
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let report = audit_dir(&fixtures).unwrap();
    let mut flagged: Vec<_> = report
        .findings
        .iter()
        .map(|f| {
            (
                f.rel_path.to_string_lossy().into_owned(),
                f.global.clone(),
                f.reason.clone(),
            )
        })
        .collect();
    flagged.sort();
    assert_eq!(
        flagged,
        [
            (
                "bad_global_cache.rs".to_string(),
                "BAD_SHAPED_RUN_INTERNER".to_string(),
                FindingReason::GlobalReachesGpuHandle,
            ),
            (
                "bad_global_cache.rs".to_string(),
                "BadInterner.runs".to_string(),
                FindingReason::CacheFieldReachesGpuHandle,
            ),
        ],
        "only the bad fixture may be flagged, got {:#?}",
        report.findings
    );
    assert!(
        report.total_globals >= 3,
        "fixtures declare multiple globals"
    );
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

/// ft-yccm0.2.5: the per-window cache-field rule is live on the real tree.
/// It must actually see every fenced TermWindow cache (so the clean result
/// above is not vacuous), every fence entry must still match, and the atlas
/// owner's maps must be recognised as such. Since ft-yccm0.4.3.4 only the
/// line-shape cache is fenced; the shape cache must scan clean.
#[test]
fn real_gui_per_window_gpu_caches_are_exactly_the_fenced_allow_list() {
    let gui_src = repo_root().join("crates/frankenterm-gui/src");
    let window_src = repo_root().join("frankenterm/window/src");
    if !gui_src.is_dir() || !window_src.is_dir() {
        return;
    }
    let report = audit_dirs(&[gui_src, window_src]).unwrap();
    let cache_field_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.reason == FindingReason::CacheFieldReachesGpuHandle)
        .map(|f| f.render())
        .collect();
    assert!(
        cache_field_findings.is_empty(),
        "unfenced per-window caches can pin an old atlas: {cache_field_findings:#?}"
    );
    assert!(
        report.stale_cache_field_allow_entries.is_empty(),
        "stale ALLOWED_CACHE_FIELDS entries: {:?}",
        report.stale_cache_field_allow_entries
    );
    assert_eq!(
        report.allow_listed_cache_fields,
        cache_gpu_handle_lint::allow_list::ALLOWED_CACHE_FIELDS.len(),
        "every fenced cache field must be found and must reach a GPU leaf"
    );
    assert!(
        report.atlas_owner_cache_fields > 0,
        "GlyphCache's sprite/glyph maps must be recognised as the atlas owner's"
    );
    assert!(report.total_cache_fields > report.allow_listed_cache_fields);
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
        .filter(|f| f.rel_path.to_string_lossy().contains("shapecache.rs"))
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
