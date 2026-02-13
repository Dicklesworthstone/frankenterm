//! Property-based tests for the `wezterm` module.
//!
//! Covers `PaneSize` serde, `CursorVisibility` serde,
//! `CwdInfo` serde + `parse()` correctness,
//! and `PaneInfo` serde + `effective_*` / `inferred_domain` methods.

use frankenterm_core::wezterm::{CursorVisibility, CwdInfo, PaneInfo, PaneSize};
use proptest::prelude::*;

// =========================================================================
// Strategies
// =========================================================================

fn arb_pane_size() -> impl Strategy<Value = PaneSize> {
    (
        0_u32..500,
        0_u32..500,
        proptest::option::of(0_u32..4000),
        proptest::option::of(0_u32..4000),
        proptest::option::of(72_u32..300),
    )
        .prop_map(|(rows, cols, pixel_width, pixel_height, dpi)| PaneSize {
            rows,
            cols,
            pixel_width,
            pixel_height,
            dpi,
        })
}

fn arb_cursor_visibility() -> impl Strategy<Value = CursorVisibility> {
    prop_oneof![
        Just(CursorVisibility::Visible),
        Just(CursorVisibility::Hidden),
    ]
}

fn arb_pane_info() -> impl Strategy<Value = PaneInfo> {
    (
        0_u64..100_000,
        0_u64..100,
        0_u64..100,
        proptest::option::of(0_u64..1000),
        proptest::option::of("[a-z]{3,10}"),
        proptest::option::of("[a-z_]{3,15}"),
        proptest::option::of(arb_pane_size()),
        proptest::option::of(0_u32..500),
        proptest::option::of(0_u32..500),
    )
        .prop_map(
            |(pane_id, tab_id, window_id, domain_id, domain_name, workspace, size, rows, cols)| {
                PaneInfo {
                    pane_id,
                    tab_id,
                    window_id,
                    domain_id,
                    domain_name,
                    workspace,
                    size,
                    rows,
                    cols,
                    title: None,
                    cwd: None,
                    tty_name: None,
                    cursor_x: None,
                    cursor_y: None,
                    cursor_visibility: None,
                    left_col: None,
                    top_row: None,
                    is_active: false,
                    is_zoomed: false,
                    extra: std::collections::HashMap::new(),
                }
            },
        )
}

// =========================================================================
// PaneSize — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_pane_size_serde(size in arb_pane_size()) {
        let json = serde_json::to_string(&size).unwrap();
        let back: PaneSize = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.rows, size.rows);
        prop_assert_eq!(back.cols, size.cols);
        prop_assert_eq!(back.pixel_width, size.pixel_width);
        prop_assert_eq!(back.pixel_height, size.pixel_height);
        prop_assert_eq!(back.dpi, size.dpi);
    }

    #[test]
    fn prop_pane_size_deterministic(size in arb_pane_size()) {
        let j1 = serde_json::to_string(&size).unwrap();
        let j2 = serde_json::to_string(&size).unwrap();
        prop_assert_eq!(&j1, &j2);
    }

    #[test]
    fn prop_pane_size_default(_dummy in 0..1_u8) {
        let size = PaneSize::default();
        prop_assert_eq!(size.rows, 0);
        prop_assert_eq!(size.cols, 0);
        prop_assert!(size.pixel_width.is_none());
        prop_assert!(size.pixel_height.is_none());
        prop_assert!(size.dpi.is_none());
    }
}

// =========================================================================
// CursorVisibility — serde
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_cursor_visibility_serde(vis in arb_cursor_visibility()) {
        let json = serde_json::to_string(&vis).unwrap();
        let back: CursorVisibility = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back, vis);
    }

    #[test]
    fn prop_cursor_visibility_pascal_case(vis in arb_cursor_visibility()) {
        let json = serde_json::to_string(&vis).unwrap();
        let expected = match vis {
            CursorVisibility::Visible => "\"Visible\"",
            CursorVisibility::Hidden => "\"Hidden\"",
        };
        prop_assert_eq!(&json, expected);
    }

    #[test]
    fn prop_cursor_visibility_default(_dummy in 0..1_u8) {
        let vis = CursorVisibility::default();
        prop_assert_eq!(vis, CursorVisibility::Visible);
    }
}

// =========================================================================
// CwdInfo — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_cwd_info_serde(
        path in "/[a-z]{2,10}/[a-z]{2,10}",
        host in "[a-z]{3,10}",
        is_remote in any::<bool>(),
    ) {
        let info = CwdInfo {
            raw_uri: if is_remote {
                format!("file://{host}{path}")
            } else {
                format!("file://{path}")
            },
            path: path.clone(),
            host: if is_remote { host.clone() } else { String::new() },
            is_remote,
        };
        let json = serde_json::to_string(&info).unwrap();
        let back: CwdInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&back.path, &info.path);
        prop_assert_eq!(&back.host, &info.host);
        prop_assert_eq!(back.is_remote, info.is_remote);
    }

    #[test]
    fn prop_cwd_info_default(_dummy in 0..1_u8) {
        let info = CwdInfo::default();
        prop_assert!(info.raw_uri.is_empty());
        prop_assert!(info.path.is_empty());
        prop_assert!(info.host.is_empty());
        prop_assert!(!info.is_remote);
    }
}

// =========================================================================
// CwdInfo::parse — correctness
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// Parsing a local file:// URI yields correct path and is_remote=false.
    #[test]
    fn prop_parse_local_uri(path in "/[a-z]{2,10}/[a-z]{2,10}") {
        let uri = format!("file://{path}");
        let info = CwdInfo::parse(&uri);
        prop_assert_eq!(&info.path, &path);
        prop_assert!(!info.is_remote);
        prop_assert!(info.host.is_empty());
    }

    /// Parsing a remote file:// URI yields host and is_remote=true.
    #[test]
    fn prop_parse_remote_uri(
        host in "[a-z]{3,10}",
        path in "/[a-z]{2,10}/[a-z]{2,10}",
    ) {
        let uri = format!("file://{host}{path}");
        let info = CwdInfo::parse(&uri);
        prop_assert_eq!(&info.host, &host);
        prop_assert_eq!(&info.path, &path);
        prop_assert!(info.is_remote);
    }

    /// Parsing an empty string yields default.
    #[test]
    fn prop_parse_empty(_dummy in 0..1_u8) {
        let info = CwdInfo::parse("");
        prop_assert!(info.raw_uri.is_empty());
        prop_assert!(info.path.is_empty());
        prop_assert!(!info.is_remote);
    }

    /// Parsing a bare path (no scheme) yields the path as-is.
    #[test]
    fn prop_parse_bare_path(path in "/[a-z]{2,10}/[a-z]{2,10}") {
        let info = CwdInfo::parse(&path);
        prop_assert_eq!(&info.path, &path);
        prop_assert!(!info.is_remote);
        prop_assert!(info.host.is_empty());
    }

    /// parse().raw_uri preserves the original input.
    #[test]
    fn prop_parse_preserves_raw_uri(
        host in "[a-z]{3,10}",
        path in "/[a-z]{2,10}",
    ) {
        let uri = format!("file://{host}{path}");
        let info = CwdInfo::parse(&uri);
        prop_assert_eq!(&info.raw_uri, &uri);
    }
}

// =========================================================================
// PaneInfo — serde roundtrip
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn prop_pane_info_serde(pane in arb_pane_info()) {
        let json = serde_json::to_string(&pane).unwrap();
        let back: PaneInfo = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.pane_id, pane.pane_id);
        prop_assert_eq!(back.tab_id, pane.tab_id);
        prop_assert_eq!(back.window_id, pane.window_id);
        prop_assert_eq!(back.is_active, pane.is_active);
        prop_assert_eq!(back.is_zoomed, pane.is_zoomed);
    }

    #[test]
    fn prop_pane_info_deterministic(pane in arb_pane_info()) {
        let j1 = serde_json::to_string(&pane).unwrap();
        let j2 = serde_json::to_string(&pane).unwrap();
        prop_assert_eq!(&j1, &j2);
    }
}

// =========================================================================
// PaneInfo::effective_* methods
// =========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    /// effective_domain falls back to "local" when domain_name is None.
    #[test]
    fn prop_effective_domain_fallback(pane_id in 0_u64..100) {
        let pane = PaneInfo {
            pane_id,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        prop_assert_eq!(pane.effective_domain(), "local");
    }

    /// effective_domain returns domain_name when set.
    #[test]
    fn prop_effective_domain_set(
        pane_id in 0_u64..100,
        domain in "[a-z]{3,10}",
    ) {
        let pane = PaneInfo {
            pane_id,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: Some(domain.clone()),
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        prop_assert_eq!(pane.effective_domain(), domain.as_str());
    }

    /// effective_rows falls back to 24 when no size info.
    #[test]
    fn prop_effective_rows_fallback(pane_id in 0_u64..100) {
        let pane = PaneInfo {
            pane_id,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        prop_assert_eq!(pane.effective_rows(), 24);
        prop_assert_eq!(pane.effective_cols(), 80);
    }

    /// effective_rows prefers size.rows over flat rows.
    #[test]
    fn prop_effective_rows_from_size(
        size_rows in 1_u32..500,
        size_cols in 1_u32..500,
        flat_rows in 1_u32..500,
        flat_cols in 1_u32..500,
    ) {
        let pane = PaneInfo {
            pane_id: 1,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: Some(PaneSize {
                rows: size_rows,
                cols: size_cols,
                pixel_width: None,
                pixel_height: None,
                dpi: None,
            }),
            rows: Some(flat_rows),
            cols: Some(flat_cols),
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        // size.rows takes priority over flat rows
        prop_assert_eq!(pane.effective_rows(), size_rows);
        prop_assert_eq!(pane.effective_cols(), size_cols);
    }

    /// effective_rows uses flat rows when size is None.
    #[test]
    fn prop_effective_rows_flat_fallback(
        flat_rows in 1_u32..500,
        flat_cols in 1_u32..500,
    ) {
        let pane = PaneInfo {
            pane_id: 1,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: Some(flat_rows),
            cols: Some(flat_cols),
            title: None,
            cwd: None,
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        prop_assert_eq!(pane.effective_rows(), flat_rows);
        prop_assert_eq!(pane.effective_cols(), flat_cols);
    }

    /// inferred_domain falls back to "local" when no domain_name and local cwd.
    #[test]
    fn prop_inferred_domain_local(pane_id in 0_u64..100) {
        let pane = PaneInfo {
            pane_id,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: Some("file:///home/user".to_string()),
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        prop_assert_eq!(pane.inferred_domain(), "local");
    }

    /// inferred_domain infers ssh: prefix from remote cwd.
    #[test]
    fn prop_inferred_domain_remote(
        host in "[a-z]{3,10}",
    ) {
        let pane = PaneInfo {
            pane_id: 1,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: None,
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: Some(format!("file://{host}/home/user")),
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        let expected = format!("ssh:{host}");
        prop_assert_eq!(pane.inferred_domain(), expected);
    }

    /// inferred_domain prefers explicit domain_name over cwd inference.
    #[test]
    fn prop_inferred_domain_explicit_wins(
        domain in "[a-z]{3,10}",
        host in "[a-z]{3,10}",
    ) {
        let pane = PaneInfo {
            pane_id: 1,
            tab_id: 0,
            window_id: 0,
            domain_id: None,
            domain_name: Some(domain.clone()),
            workspace: None,
            size: None,
            rows: None,
            cols: None,
            title: None,
            cwd: Some(format!("file://{host}/home/user")),
            tty_name: None,
            cursor_x: None,
            cursor_y: None,
            cursor_visibility: None,
            left_col: None,
            top_row: None,
            is_active: false,
            is_zoomed: false,
            extra: std::collections::HashMap::new(),
        };
        prop_assert_eq!(pane.inferred_domain(), domain);
    }
}

// =========================================================================
// Unit tests
// =========================================================================

#[test]
fn cursor_visibility_variants_distinct_json() {
    let a = serde_json::to_string(&CursorVisibility::Visible).unwrap();
    let b = serde_json::to_string(&CursorVisibility::Hidden).unwrap();
    assert_ne!(a, b);
}

#[test]
fn pane_info_deserializes_with_extra_fields() {
    let json = r#"{
        "pane_id": 1,
        "tab_id": 2,
        "window_id": 3,
        "is_active": true,
        "is_zoomed": false,
        "unknown_field": "should be captured"
    }"#;
    let pane: PaneInfo = serde_json::from_str(json).unwrap();
    assert_eq!(pane.pane_id, 1);
    assert!(pane.extra.contains_key("unknown_field"));
}

#[test]
fn cwd_parse_host_only() {
    // file://hostname (no path after host)
    let info = CwdInfo::parse("file://myhost");
    assert!(info.is_remote);
    assert_eq!(info.host, "myhost");
    assert!(info.path.is_empty());
}
