//! Mux session client + concrete data types.
//!
//! ft-zoxxq.2: this module is the strangler-fig anchor for the relocation
//! of `WeztermClient` and the concrete data types currently defined in
//! `wezterm.rs`. Per `docs/proposals/ft-zoxxq-mux-boundary-truth.md`
//! stance (b), `wezterm.rs` should become a thin shim that hosts the
//! `MuxInterface` trait + re-exports the concrete types from this file.
//!
//! This first slice physically relocates the two simplest leaf types
//! (`PaneSize`, `CursorVisibility`) to prove the move pattern works
//! end-to-end without disturbing the 5 `impl MuxInterface for X` blocks
//! or the ~192 concrete-type call sites. Subsequent ft-zoxxq.2.x child
//! commits will move `CwdInfo`, `PaneInfo`, `SpawnTarget`,
//! `WeztermClient`, and the trait impls one cluster at a time.
//!
//! Re-export rule: every type that lives here is also reachable as
//! `crate::wezterm::<TypeName>` so existing imports continue to compile.
//! New code should reach for `crate::mux_client::<TypeName>` directly.

use serde::{Deserialize, Serialize};

/// Pane size information.
///
/// Relocated from `wezterm.rs` per ft-zoxxq.2. The serde shape is
/// preserved exactly — adding `#[serde(default)]` to every field keeps
/// the on-the-wire JSON identical to the pre-rename payload.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PaneSize {
    /// Number of rows (character cells)
    #[serde(default)]
    pub rows: u32,
    /// Number of columns (character cells)
    #[serde(default)]
    pub cols: u32,
    /// Pixel width (if available)
    #[serde(default)]
    pub pixel_width: Option<u32>,
    /// Pixel height (if available)
    #[serde(default)]
    pub pixel_height: Option<u32>,
    /// DPI (if available)
    #[serde(default)]
    pub dpi: Option<u32>,
}

/// Cursor visibility state.
///
/// Relocated from `wezterm.rs` per ft-zoxxq.2. PascalCase serde rename is
/// preserved so the wire format stays compatible with existing payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum CursorVisibility {
    /// Cursor is visible
    #[default]
    Visible,
    /// Cursor is hidden
    Hidden,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ft-zoxxq.2: pin the relocated `PaneSize` serde shape so the
    /// physical move from `wezterm.rs` to `mux_client.rs` cannot drift
    /// the wire format. Adding/removing fields here will fail this test
    /// and force the change to be considered explicitly.
    #[test]
    fn pane_size_serde_shape_is_preserved_after_relocation() {
        let size = PaneSize {
            rows: 24,
            cols: 80,
            pixel_width: Some(640),
            pixel_height: Some(480),
            dpi: Some(96),
        };
        let json = serde_json::to_string(&size).unwrap();
        // Field order matches the source-of-truth field order; serde_json
        // emits in declaration order for serialized structs.
        assert_eq!(
            json,
            r#"{"rows":24,"cols":80,"pixel_width":640,"pixel_height":480,"dpi":96}"#
        );
        let back: PaneSize = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rows, 24);
        assert_eq!(back.cols, 80);
        assert_eq!(back.pixel_width, Some(640));
        assert_eq!(back.pixel_height, Some(480));
        assert_eq!(back.dpi, Some(96));
    }

    /// ft-zoxxq.2: pin the relocated `CursorVisibility` PascalCase
    /// serialization. The wire payload says "Visible" / "Hidden" — drift
    /// would break any persisted snapshot containing the old shape.
    #[test]
    fn cursor_visibility_pascal_case_is_preserved_after_relocation() {
        assert_eq!(
            serde_json::to_string(&CursorVisibility::Visible).unwrap(),
            "\"Visible\""
        );
        assert_eq!(
            serde_json::to_string(&CursorVisibility::Hidden).unwrap(),
            "\"Hidden\""
        );
        let v: CursorVisibility = serde_json::from_str("\"Visible\"").unwrap();
        assert_eq!(v, CursorVisibility::Visible);
        let h: CursorVisibility = serde_json::from_str("\"Hidden\"").unwrap();
        assert_eq!(h, CursorVisibility::Hidden);
        assert_eq!(CursorVisibility::default(), CursorVisibility::Visible);
    }

    /// ft-zoxxq.2 + ft-zoxxq.1: the relocated types must remain reachable
    /// through the legacy `crate::wezterm::*` path because callers across
    /// the workspace import them that way. This guards against an
    /// accidental break of the re-export shim.
    #[test]
    fn relocated_types_still_reachable_via_legacy_wezterm_path() {
        let _: crate::wezterm::PaneSize = PaneSize::default();
        let _: crate::wezterm::CursorVisibility = CursorVisibility::default();
    }
}
