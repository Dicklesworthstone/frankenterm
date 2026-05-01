//! Atlas-stability contract and structured-logging schema
//! ([BR-TERM-EMULATOR-UPLIFT.1.1] / `ft-mpc9b.1.1`).
//!
//! Stable-versioned atlas (ghostty pattern). The atlas API foundation
//! shipped in `ece09688b` (parent `ft-mpc9b.1.1`); the renderer
//! integration shipped in `ft-c9arc` (drop the
//! `recreate_texture_atlas` from `apply_scale_change`, wire the
//! `last_synced_version` cursor on `GlyphCache`, expose `Atlas::grow`
//! + `atlas_grow_count`). This module pins the **structured-logging
//! contract** the renderer emits per atlas op + per resize, and the
//! **stability invariants** the regression fixture asserts.
//!
//! The headline correctness rule:
//!
//! > A pure window-resize that does NOT allocate any new sprites
//! > MUST leave `atlas.version()` and `atlas_rebuilds_total`
//! > unchanged.
//!
//! That single property is what unblocks the 5 downstream beads
//! (`ft-mpc9b.6.1`, `ft-mpc9b.4.1`, `ft-mpc9b.3.1`, `ft-mpc9b.1.4`,
//! `ft-2okh0.{6,10,11,15}`). It is enforced at three layers:
//!
//! 1. **Atlas API** (`window::bitmaps::atlas`): only `allocate*` and
//!    `clear` / `grow` bump the version. Resize is not an atlas op.
//! 2. **Renderer integration** (`crates/frankenterm-gui/src/termwindow/resize.rs`):
//!    `apply_scale_change` no longer calls `recreate_texture_atlas`.
//! 3. **Per-frame cursor**
//!    (`crates/frankenterm-gui/src/glyphcache.rs::last_synced_version`):
//!    snapshots `atlas.version()` at the start of each paint pass;
//!    a frame that does no allocates produces a no-op snapshot.
//!
//! This module is the **structured-logging + invariant** layer
//! sitting above all three.

use serde::{Deserialize, Serialize};

// ============================================================================
// Per-op event
// ============================================================================

/// One atlas mutation event — written to
/// `tests/atlas_stability/logs/<scenario>.jsonl` by the regression
/// fixture and (eventually) by the GUI's atlas op recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtlasStabilityEvent {
    /// Monotonic timestamp (ms since fixture start).
    pub ts_ms: u64,
    /// What kind of op fired.
    pub op: AtlasOp,
    /// Version cursor at the START of the op (snapshot before).
    pub version_before: u64,
    /// Version cursor at the END of the op (snapshot after).
    pub version_after: u64,
    /// Bytes of texture memory the op touched. For `Upload`, the
    /// sprite size; for `Grow`, the size of the new texture; for
    /// `Resize`, always 0 (resize is not an atlas op — present in
    /// the stream for context).
    pub bytes: u64,
}

/// The closed list of atlas op kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasOp {
    /// Successful sprite upload (`Atlas::allocate*` returned `Ok`).
    Upload,
    /// `Atlas::clear` (full reset).
    Clear,
    /// `Atlas::grow` (resize to a larger texture).
    Grow,
    /// Per-frame paint-pass snapshot
    /// (`GlyphCache::snapshot_atlas_version`). Bytes always 0.
    Sync,
    /// Window resize event — present so the stream records the
    /// timing context, but **MUST NOT** bump version_after relative
    /// to version_before.
    Resize,
}

impl AtlasOp {
    /// Whether this op is allowed to bump the atlas version.
    /// `Sync` and `Resize` MUST be version-stable.
    #[must_use]
    pub const fn may_bump_version(self) -> bool {
        matches!(self, Self::Upload | Self::Clear | Self::Grow)
    }
}

// ============================================================================
// Per-resize summary
// ============================================================================

/// Per-resize aggregate — what the bead's "JSON-line at
/// tests/atlas_stability/logs/" schema captures for the resize-storm
/// regression test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtlasStabilityResize {
    /// Monotonic timestamp (ms since fixture start).
    pub ts_ms: u64,
    /// Number of sprites that needed re-upload during this resize.
    /// On a pure window-size resize this is `0` (the headline
    /// correctness rule); on a scale change it equals the number of
    /// *new* glyphs the renderer rasterized at the new metrics
    /// (lazy-rerasterize).
    pub glyphs_re_uploaded: u64,
    /// Atlas size in bytes before the resize.
    pub atlas_size_bytes_before: u64,
    /// Atlas size in bytes after the resize.
    pub atlas_size_bytes_after: u64,
    /// Wall-clock duration of the resize sync in milliseconds.
    pub sync_duration_ms: u64,
}

// ============================================================================
// Invariants
// ============================================================================

/// Named invariant violations the regression fixture asserts on a
/// captured event stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AtlasStabilityViolation {
    /// `Sync` or `Resize` op bumped the version. The bead's
    /// headline correctness rule: these ops MUST be no-ops on the
    /// version cursor.
    NonAllocBumpedVersion {
        op: AtlasOp,
        version_before: u64,
        version_after: u64,
        index: usize,
    },
    /// Version went backwards across two adjacent events.
    NonMonotonicVersion {
        prior: u64,
        current: u64,
        index: usize,
    },
    /// Timestamps went backwards.
    NonMonotonicTimestamp {
        prior_ts_ms: u64,
        current_ts_ms: u64,
        index: usize,
    },
    /// `Upload` event reported zero bytes — every sprite has
    /// non-zero extent.
    UploadWithZeroBytes { index: usize },
    /// Pure resize (`AtlasStabilityResize`) reported `> 0`
    /// re-uploaded glyphs. The bead's headline correctness rule.
    PureResizeReUploaded { glyphs_re_uploaded: u64, ts_ms: u64 },
}

/// Run all invariant checks against a captured event stream.
/// Returns the accumulated list (empty Vec = clean).
#[must_use]
pub fn check_invariants(events: &[AtlasStabilityEvent]) -> Vec<AtlasStabilityViolation> {
    let mut violations = Vec::new();
    let mut prior_version: Option<u64> = None;
    let mut prior_ts: Option<u64> = None;
    for (i, ev) in events.iter().enumerate() {
        if let Some(prev_v) = prior_version {
            if ev.version_before < prev_v {
                violations.push(AtlasStabilityViolation::NonMonotonicVersion {
                    prior: prev_v,
                    current: ev.version_before,
                    index: i,
                });
            }
        }
        if ev.version_after < ev.version_before {
            violations.push(AtlasStabilityViolation::NonMonotonicVersion {
                prior: ev.version_before,
                current: ev.version_after,
                index: i,
            });
        }
        if let Some(prev_ts) = prior_ts {
            if ev.ts_ms < prev_ts {
                violations.push(AtlasStabilityViolation::NonMonotonicTimestamp {
                    prior_ts_ms: prev_ts,
                    current_ts_ms: ev.ts_ms,
                    index: i,
                });
            }
        }
        if !ev.op.may_bump_version() && ev.version_after != ev.version_before {
            violations.push(AtlasStabilityViolation::NonAllocBumpedVersion {
                op: ev.op,
                version_before: ev.version_before,
                version_after: ev.version_after,
                index: i,
            });
        }
        if matches!(ev.op, AtlasOp::Upload) && ev.bytes == 0 {
            violations.push(AtlasStabilityViolation::UploadWithZeroBytes { index: i });
        }
        prior_version = Some(ev.version_after);
        prior_ts = Some(ev.ts_ms);
    }
    violations
}

/// Check the per-resize summary against the bead's headline rule:
/// pure resize MUST NOT re-upload glyphs. "Pure" means no scale
/// change — the caller decides; the rule applies whenever the
/// caller declares a resize "pure".
#[must_use]
pub fn check_pure_resize(resize: &AtlasStabilityResize) -> Vec<AtlasStabilityViolation> {
    let mut v = Vec::new();
    if resize.glyphs_re_uploaded > 0 {
        v.push(AtlasStabilityViolation::PureResizeReUploaded {
            glyphs_re_uploaded: resize.glyphs_re_uploaded,
            ts_ms: resize.ts_ms,
        });
    }
    v
}

// ============================================================================
// Health diagnostics surface
// ============================================================================

/// Compact snapshot of atlas-stability counters for the `ft doctor`
/// telemetry surface (bead acceptance: "Telemetry exposed via ft
/// doctor").
///
/// The GUI populates this from the live `metrics` recorder at
/// snapshot time; the regression fixture exercises the field shape
/// + serde stability so the future GUI integration can plug in
/// without re-deriving the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtlasStabilityHealth {
    /// `window.atlas.uploads.total` since process start.
    pub uploads_total: u64,
    /// `window.atlas.rebuilds.total` since process start. The bead's
    /// headline acceptance rule is that this stays at 0 across
    /// normal use. A non-zero value here is the alert signal.
    pub rebuilds_total: u64,
    /// `window.atlas.grow.count` since process start.
    pub grow_count: u64,
    /// `window.atlas.size_bytes` (current gauge value).
    pub size_bytes: u64,
}

impl AtlasStabilityHealth {
    /// Construct an "everything zero" baseline — the state at
    /// process start before any frames have been painted.
    #[must_use]
    pub fn baseline() -> Self {
        Self {
            uploads_total: 0,
            rebuilds_total: 0,
            grow_count: 0,
            size_bytes: 0,
        }
    }

    /// Whether the current snapshot satisfies the bead's headline
    /// acceptance rule: zero rebuilds.
    #[must_use]
    pub fn is_resize_stable(&self) -> bool {
        self.rebuilds_total == 0
    }
}

// ============================================================================
// JSONL writer
// ============================================================================

/// Serialize a slice of events as JSONL.
#[must_use]
pub fn render_events_jsonl(events: &[AtlasStabilityEvent]) -> String {
    let mut out = String::new();
    for ev in events {
        let line = serde_json::to_string(ev).expect("AtlasStabilityEvent always serializes");
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Parse a JSONL string back into events.
pub fn parse_events_jsonl(jsonl: &str) -> Result<Vec<AtlasStabilityEvent>, serde_json::Error> {
    let mut out = Vec::new();
    for line in jsonl.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        out.push(serde_json::from_str(trimmed)?);
    }
    Ok(out)
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_classification_is_stable() {
        assert!(AtlasOp::Upload.may_bump_version());
        assert!(AtlasOp::Clear.may_bump_version());
        assert!(AtlasOp::Grow.may_bump_version());
        assert!(!AtlasOp::Sync.may_bump_version());
        assert!(!AtlasOp::Resize.may_bump_version());
    }

    #[test]
    fn pure_resize_with_zero_reuploads_is_clean() {
        let r = AtlasStabilityResize {
            ts_ms: 0,
            glyphs_re_uploaded: 0,
            atlas_size_bytes_before: 4 * 1024 * 1024,
            atlas_size_bytes_after: 4 * 1024 * 1024,
            sync_duration_ms: 0,
        };
        assert!(check_pure_resize(&r).is_empty());
    }

    #[test]
    fn pure_resize_with_reuploads_violates() {
        let r = AtlasStabilityResize {
            ts_ms: 100,
            glyphs_re_uploaded: 42,
            atlas_size_bytes_before: 4 * 1024 * 1024,
            atlas_size_bytes_after: 4 * 1024 * 1024,
            sync_duration_ms: 5,
        };
        let v = check_pure_resize(&r);
        assert!(v.iter().any(|x| matches!(
            x,
            AtlasStabilityViolation::PureResizeReUploaded {
                glyphs_re_uploaded: 42,
                ..
            }
        )));
    }

    #[test]
    fn sync_event_with_version_bump_violates() {
        let events = vec![AtlasStabilityEvent {
            ts_ms: 0,
            op: AtlasOp::Sync,
            version_before: 5,
            version_after: 6,
            bytes: 0,
        }];
        let v = check_invariants(&events);
        assert!(
            v.iter().any(|x| matches!(
                x,
                AtlasStabilityViolation::NonAllocBumpedVersion {
                    op: AtlasOp::Sync,
                    ..
                }
            )),
            "expected NonAllocBumpedVersion for Sync, got {v:?}"
        );
    }

    #[test]
    fn resize_event_with_version_bump_violates() {
        let events = vec![AtlasStabilityEvent {
            ts_ms: 0,
            op: AtlasOp::Resize,
            version_before: 5,
            version_after: 7,
            bytes: 0,
        }];
        let v = check_invariants(&events);
        assert!(v.iter().any(|x| matches!(
            x,
            AtlasStabilityViolation::NonAllocBumpedVersion {
                op: AtlasOp::Resize,
                ..
            }
        )));
    }

    #[test]
    fn upload_event_with_proper_bump_is_clean() {
        let events = vec![
            AtlasStabilityEvent {
                ts_ms: 0,
                op: AtlasOp::Sync,
                version_before: 0,
                version_after: 0,
                bytes: 0,
            },
            AtlasStabilityEvent {
                ts_ms: 5,
                op: AtlasOp::Upload,
                version_before: 0,
                version_after: 1,
                bytes: 64,
            },
            AtlasStabilityEvent {
                ts_ms: 10,
                op: AtlasOp::Resize,
                version_before: 1,
                version_after: 1,
                bytes: 0,
            },
        ];
        assert!(check_invariants(&events).is_empty());
    }

    #[test]
    fn non_monotonic_version_detected() {
        let events = vec![
            AtlasStabilityEvent {
                ts_ms: 0,
                op: AtlasOp::Upload,
                version_before: 0,
                version_after: 5,
                bytes: 8,
            },
            AtlasStabilityEvent {
                ts_ms: 5,
                op: AtlasOp::Upload,
                version_before: 3,
                version_after: 4,
                bytes: 8,
            },
        ];
        let v = check_invariants(&events);
        assert!(
            v.iter()
                .any(|x| matches!(x, AtlasStabilityViolation::NonMonotonicVersion { .. }))
        );
    }

    #[test]
    fn non_monotonic_timestamp_detected() {
        let events = vec![
            AtlasStabilityEvent {
                ts_ms: 100,
                op: AtlasOp::Upload,
                version_before: 0,
                version_after: 1,
                bytes: 8,
            },
            AtlasStabilityEvent {
                ts_ms: 50,
                op: AtlasOp::Upload,
                version_before: 1,
                version_after: 2,
                bytes: 8,
            },
        ];
        let v = check_invariants(&events);
        assert!(
            v.iter()
                .any(|x| matches!(x, AtlasStabilityViolation::NonMonotonicTimestamp { .. }))
        );
    }

    #[test]
    fn upload_with_zero_bytes_detected() {
        let events = vec![AtlasStabilityEvent {
            ts_ms: 0,
            op: AtlasOp::Upload,
            version_before: 0,
            version_after: 1,
            bytes: 0,
        }];
        let v = check_invariants(&events);
        assert!(
            v.iter()
                .any(|x| matches!(x, AtlasStabilityViolation::UploadWithZeroBytes { .. }))
        );
    }

    #[test]
    fn jsonl_roundtrip() {
        let events = vec![
            AtlasStabilityEvent {
                ts_ms: 0,
                op: AtlasOp::Sync,
                version_before: 0,
                version_after: 0,
                bytes: 0,
            },
            AtlasStabilityEvent {
                ts_ms: 5,
                op: AtlasOp::Upload,
                version_before: 0,
                version_after: 1,
                bytes: 128,
            },
        ];
        let rendered = render_events_jsonl(&events);
        let parsed = parse_events_jsonl(&rendered).expect("parse");
        assert_eq!(parsed, events);
    }

    #[test]
    fn baseline_health_is_resize_stable() {
        let h = AtlasStabilityHealth::baseline();
        assert!(h.is_resize_stable());
        assert_eq!(h.rebuilds_total, 0);
    }

    #[test]
    fn health_with_rebuilds_is_not_resize_stable() {
        let mut h = AtlasStabilityHealth::baseline();
        h.rebuilds_total = 1;
        assert!(!h.is_resize_stable());
    }

    #[test]
    fn health_serde_roundtrips() {
        let h = AtlasStabilityHealth {
            uploads_total: 5_000,
            rebuilds_total: 0,
            grow_count: 2,
            size_bytes: 16 * 1024 * 1024,
        };
        let json = serde_json::to_string(&h).unwrap();
        let parsed: AtlasStabilityHealth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, h);
    }
}
