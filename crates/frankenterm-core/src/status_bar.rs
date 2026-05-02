//! Status bar layout + tile-truncation policy substrate
//! (ft-mpc9b.4.4).
//!
//! Steals the zellij pattern of a status bar with pluggable tiles
//! (Mode / SessionName / PaneCount / AgentStateBreakdown / FleetMemoryTier
//! / CostMeter / NetworkActivity / Clock). Adds the swarm-native tiles
//! the bead calls out (agent state, fleet health, rate-limit, workflow
//! count). Built-in tiles + WIT-bound plugin tiles use the same surface.
//!
//! ## What this module ships
//!
//! - `TileAlignment` — Left / Center / Right.
//! - `TileSpec` — pure-data descriptor: id, alignment, width bounds,
//!   priority (lower = drop first under truncation), a11y label
//!   (REQUIRED per the bead — empty label is a config error).
//! - `RenderedTile` — the tile's visible content + tooltip + hit map.
//!   The integration layer's `StatusTile::render` returns this.
//! - `StatusBarLayout` — given a list of `TileSpec` + per-tile
//!   `RenderedTile` + the bar's available width, produces a deterministic
//!   `LaidOutBar` describing the final placement (left/center/right
//!   pixel-x ranges, the dropped tiles, and the truncation reason).
//! - Hover / click hit-test: `hit_test(x_in_bar) -> Option<HitResult>`
//!   maps a cursor x-coordinate back to a tile + the offset within
//!   the tile.
//! - A11y enforcement: `validate_tile_specs` returns
//!   `Err(MissingA11yLabel)` if any tile's label is empty.
//!
//! ## What is deferred to the integration bead (ft-mpc9b.4.4.cont)
//!
//! - The `StatusTile` trait + WIT bindings for plugin tiles
//!   (cross-link 4.3 / ft-mpc9b.4.3 — WASM plugin system).
//! - The 8 built-in tile implementations (Mode / SessionName /
//!   PaneCount / AgentStateBreakdown / FleetMemoryTier / CostMeter /
//!   NetworkActivity / Clock).
//! - Per-tile update cadence (Mode = on-event, Clock = 1 Hz, agent
//!   state = 250 ms, etc.). The substrate's `TileSpec` carries the
//!   refresh hint so the cadence wiring lives in the gui crate.
//! - Cross-link `docs/status-bar/truncation-rules.md` for the
//!   operator-facing truncation behaviour spec.
//! - Screen-reader announcement playback (NSAccessibility / AT-SPI)
//!   when a tile's label or rendered content changes.

#![allow(dead_code)]

// ============================================================================
// TileAlignment
// ============================================================================

/// Where the tile lives in the status bar. The layout engine packs
/// `Left`-aligned tiles from the left edge rightward, `Right`-aligned
/// tiles from the right edge leftward, and `Center`-aligned tiles
/// around the middle. Conflicts (overflow into the centre group) are
/// resolved by the priority-based truncation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum TileAlignment {
    #[default]
    Left,
    Center,
    Right,
}

// ============================================================================
// TileSpec
// ============================================================================

/// Operator-tunable refresh hint per tile. The integration's tick
/// scheduler reads this and re-renders accordingly. Pure-data only;
/// the substrate doesn't drive the timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TileRefreshHint {
    /// On state change only (e.g. mode indicator).
    OnEvent,
    /// Every N milliseconds.
    EveryMs(u32),
}

/// Pure-data descriptor for a status-bar tile. The integration layer's
/// `StatusTile` trait holds the actual render closure; this substrate
/// works with the descriptor + the rendered output side-by-side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileSpec {
    /// Stable identifier (e.g. "ft.mode", "plugin.cost"). Used for
    /// hit-test routing and ordering ties.
    pub id: String,
    pub alignment: TileAlignment,
    pub min_width: u16,
    pub max_width: u16,
    /// Higher priority survives truncation longer. The bead's
    /// truncation rules: drop lowest-priority first, ties broken by
    /// id ordering for determinism.
    pub priority: u8,
    /// Screen-reader label. The bead requires this — empty string
    /// fails `validate_tile_specs`.
    pub a11y_label: String,
    pub refresh_hint: TileRefreshHint,
}

impl TileSpec {
    /// Convenience constructor for tests + plugin host code.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        alignment: TileAlignment,
        min_width: u16,
        max_width: u16,
        priority: u8,
        a11y_label: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            alignment,
            min_width,
            max_width,
            priority,
            a11y_label: a11y_label.into(),
            refresh_hint: TileRefreshHint::OnEvent,
        }
    }

    #[must_use]
    pub fn with_refresh(mut self, hint: TileRefreshHint) -> Self {
        self.refresh_hint = hint;
        self
    }
}

// ============================================================================
// RenderedTile
// ============================================================================

/// What the tile renders this tick. Plain data — the integration's
/// StatusTile::render builds it from the live state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedTile {
    /// Visible width in cells. Layout engine truncates (drops the
    /// tile entirely) if this exceeds the tile's `max_width`.
    pub width: u16,
    /// Optional hover tooltip. The integration's hover handler
    /// reads this when `hit_test` returns the tile.
    pub tooltip: Option<String>,
}

impl RenderedTile {
    #[must_use]
    pub fn new(width: u16) -> Self {
        Self {
            width,
            tooltip: None,
        }
    }

    #[must_use]
    pub fn with_tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }
}

// ============================================================================
// Validation
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TileValidationError {
    /// A11y label is empty — REQUIRED per the bead.
    MissingA11yLabel { tile_id: String },
    /// `min_width > max_width` — degenerate config.
    InvalidWidthRange { tile_id: String, min: u16, max: u16 },
    /// Two tiles share an id — hit-test routing breaks.
    DuplicateId { tile_id: String },
}

/// Validate a tile-spec list. The integration layer should call this
/// at config load and refuse to start if any error is returned.
pub fn validate_tile_specs(specs: &[TileSpec]) -> Result<(), TileValidationError> {
    let mut seen_ids: Vec<&str> = Vec::with_capacity(specs.len());
    for spec in specs {
        if spec.a11y_label.is_empty() {
            return Err(TileValidationError::MissingA11yLabel {
                tile_id: spec.id.clone(),
            });
        }
        if spec.min_width > spec.max_width {
            return Err(TileValidationError::InvalidWidthRange {
                tile_id: spec.id.clone(),
                min: spec.min_width,
                max: spec.max_width,
            });
        }
        if seen_ids.contains(&spec.id.as_str()) {
            return Err(TileValidationError::DuplicateId {
                tile_id: spec.id.clone(),
            });
        }
        seen_ids.push(&spec.id);
    }
    Ok(())
}

// ============================================================================
// Layout engine
// ============================================================================

/// One tile placed in the laid-out bar. The integration layer's
/// painter walks `LaidOutBar::placements` left-to-right (already
/// sorted) and renders each tile at its `x_start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TilePlacement {
    pub tile_id: String,
    pub alignment: TileAlignment,
    pub x_start: u16,
    pub width: u16,
}

impl TilePlacement {
    #[must_use]
    pub fn x_end(&self) -> u32 {
        u32::from(self.x_start) + u32::from(self.width)
    }
}

/// Why a tile was dropped from the layout. The integration's status
/// log reports these so operators can tune priority / max_width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DropReason {
    /// Total tile width exceeded the bar width; this tile lost the
    /// priority sort.
    TruncatedByWidth,
    /// Rendered tile width exceeded `max_width`. The tile's
    /// rendering is buggy; layout drops to avoid corrupting the bar.
    OverflowsMaxWidth,
    /// Rendered tile width was below `min_width`. The tile's
    /// rendering is buggy.
    UnderflowsMinWidth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedTile {
    pub tile_id: String,
    pub reason: DropReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaidOutBar {
    pub placements: Vec<TilePlacement>,
    pub dropped: Vec<DroppedTile>,
    pub bar_width: u16,
}

impl LaidOutBar {
    #[must_use]
    pub fn empty(bar_width: u16) -> Self {
        Self {
            placements: Vec::new(),
            dropped: Vec::new(),
            bar_width,
        }
    }

    #[must_use]
    pub fn placement_for(&self, tile_id: &str) -> Option<&TilePlacement> {
        self.placements.iter().find(|p| p.tile_id == tile_id)
    }

    #[must_use]
    pub fn was_dropped(&self, tile_id: &str) -> bool {
        self.dropped.iter().any(|d| d.tile_id == tile_id)
    }
}

/// Compute the layout. Pure-logic; the integration layer feeds in the
/// per-tile rendered widths each frame. The algorithm:
///
/// 1. Sanity-check rendered widths against `min_width` / `max_width`;
///    over-/underflow drops the tile with the corresponding reason.
/// 2. Group surviving tiles by alignment.
/// 3. Allocate space:
///    - Left tiles pack from `x=0` rightward.
///    - Right tiles pack from `x=bar_width` leftward.
///    - Center tiles pack around the midpoint, expanding both sides.
/// 4. If total width > `bar_width`, drop the lowest-priority tile
///    (ties by id ascending for determinism) and retry. Repeats until
///    fits or all tiles dropped.
#[must_use]
pub fn layout_status_bar(
    specs: &[TileSpec],
    rendered: &[(String, RenderedTile)],
    bar_width: u16,
) -> LaidOutBar {
    // Index renders by id.
    let mut renders: Vec<(&TileSpec, &RenderedTile)> = Vec::new();
    let mut dropped = Vec::new();
    for spec in specs {
        let Some((_, render)) = rendered.iter().find(|(id, _)| *id == spec.id) else {
            // Tile not rendered this tick — silently skip (the
            // refresh-hint scheduler may not have run yet).
            continue;
        };
        if render.width > spec.max_width {
            dropped.push(DroppedTile {
                tile_id: spec.id.clone(),
                reason: DropReason::OverflowsMaxWidth,
            });
            continue;
        }
        if render.width < spec.min_width {
            dropped.push(DroppedTile {
                tile_id: spec.id.clone(),
                reason: DropReason::UnderflowsMinWidth,
            });
            continue;
        }
        renders.push((spec, render));
    }

    // Width-truncation loop: while the survivors don't fit, drop
    // lowest priority.
    loop {
        let total: u32 = renders.iter().map(|(_, r)| u32::from(r.width)).sum();
        if total <= u32::from(bar_width) || renders.is_empty() {
            break;
        }
        // Lowest priority loses; tie-break by id ascending.
        let drop_idx = renders
            .iter()
            .enumerate()
            .min_by(|(_, (a_spec, _)), (_, (b_spec, _))| {
                a_spec
                    .priority
                    .cmp(&b_spec.priority)
                    .then_with(|| a_spec.id.cmp(&b_spec.id))
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        let (dropped_spec, _) = renders.remove(drop_idx);
        dropped.push(DroppedTile {
            tile_id: dropped_spec.id.clone(),
            reason: DropReason::TruncatedByWidth,
        });
    }

    // Place: left-to-right for Left, right-to-left for Right,
    // around-centre for Center.
    let mut placements = Vec::new();

    // Left tiles
    let mut left_x: u32 = 0;
    for (spec, render) in renders
        .iter()
        .filter(|(s, _)| s.alignment == TileAlignment::Left)
    {
        placements.push(TilePlacement {
            tile_id: spec.id.clone(),
            alignment: TileAlignment::Left,
            x_start: left_x.min(u32::from(u16::MAX)) as u16,
            width: render.width,
        });
        left_x += u32::from(render.width);
    }

    // Right tiles (pack from the right edge; placements still expressed
    // as x_start = absolute position).
    let mut right_x: u32 = u32::from(bar_width);
    let right_renders: Vec<&(&TileSpec, &RenderedTile)> = renders
        .iter()
        .filter(|(s, _)| s.alignment == TileAlignment::Right)
        .collect();
    // Reverse so the rightmost tile-spec ends up at the right edge.
    let mut right_placements: Vec<TilePlacement> = Vec::new();
    for (spec, render) in right_renders.iter().rev() {
        right_x = right_x.saturating_sub(u32::from(render.width));
        right_placements.push(TilePlacement {
            tile_id: spec.id.clone(),
            alignment: TileAlignment::Right,
            x_start: right_x.min(u32::from(u16::MAX)) as u16,
            width: render.width,
        });
    }
    // Reverse back to left-to-right order so the painter walks
    // placements left-to-right consistently.
    right_placements.reverse();

    // Centre tiles: pack around the geometric midpoint. Self-review fix
    // (br-ft-3zpjl): clamp the centre band so it can't intersect the
    // Left tiles' right edge or the Right tiles' left edge. When the
    // available band can't fit all centre tiles, drop the lowest-priority
    // centre tile (TruncatedByWidth) and retry.
    let left_x_end = left_x;
    let right_x_start = right_x;
    let centre_renders_owned: Vec<(&TileSpec, &RenderedTile)> = renders
        .iter()
        .filter(|(s, _)| s.alignment == TileAlignment::Center)
        .map(|(s, r)| (*s, *r))
        .collect();

    let mut centre_renders: Vec<(&TileSpec, &RenderedTile)> = centre_renders_owned;
    let centre_band_width = right_x_start.saturating_sub(left_x_end);
    loop {
        let centre_total_width: u32 = centre_renders.iter().map(|(_, r)| u32::from(r.width)).sum();
        if centre_total_width <= centre_band_width || centre_renders.is_empty() {
            break;
        }
        // Drop the lowest-priority (id-tiebreak ascending) centre tile.
        let drop_idx = centre_renders
            .iter()
            .enumerate()
            .min_by(|(_, (a_spec, _)), (_, (b_spec, _))| {
                a_spec
                    .priority
                    .cmp(&b_spec.priority)
                    .then_with(|| a_spec.id.cmp(&b_spec.id))
            })
            .map(|(i, _)| i)
            .unwrap_or(0);
        let (dropped_spec, _) = centre_renders.remove(drop_idx);
        dropped.push(DroppedTile {
            tile_id: dropped_spec.id.clone(),
            reason: DropReason::TruncatedByWidth,
        });
    }
    let centre_total_width: u32 = centre_renders.iter().map(|(_, r)| u32::from(r.width)).sum();
    // Geometric midpoint, then clamp to the [left_x_end, right_x_start) band.
    let geometric_start = u32::from(bar_width).saturating_sub(centre_total_width) / 2;
    let max_start = right_x_start.saturating_sub(centre_total_width);
    let centre_start = geometric_start
        .max(left_x_end)
        .min(max_start.max(left_x_end));
    let mut centre_x = centre_start;
    let mut centre_placements: Vec<TilePlacement> = Vec::new();
    for (spec, render) in &centre_renders {
        centre_placements.push(TilePlacement {
            tile_id: spec.id.clone(),
            alignment: TileAlignment::Center,
            x_start: centre_x.min(u32::from(u16::MAX)) as u16,
            width: render.width,
        });
        centre_x += u32::from(render.width);
    }

    placements.extend(centre_placements);
    placements.extend(right_placements);
    // Sort by x_start so the painter walks left-to-right.
    placements.sort_by_key(|p| p.x_start);

    LaidOutBar {
        placements,
        dropped,
        bar_width,
    }
}

// ============================================================================
// Hit-test
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitResult {
    pub tile_id: String,
    /// Offset within the tile (0..tile.width).
    pub x_in_tile: u16,
}

impl LaidOutBar {
    /// Map a cursor x-coordinate (0..bar_width) to the tile under it,
    /// returning the tile id + offset within the tile. Returns `None`
    /// when the cursor lands in the gap between tiles.
    #[must_use]
    pub fn hit_test(&self, x: u16) -> Option<HitResult> {
        for placement in &self.placements {
            let start = placement.x_start;
            let end = placement.x_end();
            if u32::from(x) >= u32::from(start) && u32::from(x) < end {
                return Some(HitResult {
                    tile_id: placement.tile_id.clone(),
                    x_in_tile: x - start,
                });
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(id: &str, align: TileAlignment, min: u16, max: u16, priority: u8) -> TileSpec {
        TileSpec::new(id, align, min, max, priority, format!("a11y:{id}"))
    }

    fn rendered(id: &str, width: u16) -> (String, RenderedTile) {
        (id.to_string(), RenderedTile::new(width))
    }

    // ----------------------------------------------------------------
    // Validation
    // ----------------------------------------------------------------

    #[test]
    fn validate_accepts_well_formed_specs() {
        let specs = vec![
            spec("a", TileAlignment::Left, 1, 10, 5),
            spec("b", TileAlignment::Right, 1, 10, 5),
        ];
        assert!(validate_tile_specs(&specs).is_ok());
    }

    #[test]
    fn validate_rejects_empty_a11y_label() {
        let mut specs = vec![spec("a", TileAlignment::Left, 1, 10, 5)];
        specs[0].a11y_label.clear();
        let err = validate_tile_specs(&specs).unwrap_err();
        assert_eq!(
            err,
            TileValidationError::MissingA11yLabel {
                tile_id: "a".to_string()
            }
        );
    }

    #[test]
    fn validate_rejects_inverted_width_range() {
        let specs = vec![spec("a", TileAlignment::Left, 10, 5, 5)];
        let err = validate_tile_specs(&specs).unwrap_err();
        assert_eq!(
            err,
            TileValidationError::InvalidWidthRange {
                tile_id: "a".to_string(),
                min: 10,
                max: 5,
            }
        );
    }

    #[test]
    fn validate_rejects_duplicate_id() {
        let specs = vec![
            spec("dup", TileAlignment::Left, 1, 5, 5),
            spec("dup", TileAlignment::Right, 1, 5, 5),
        ];
        let err = validate_tile_specs(&specs).unwrap_err();
        assert_eq!(
            err,
            TileValidationError::DuplicateId {
                tile_id: "dup".to_string()
            }
        );
    }

    // ----------------------------------------------------------------
    // Layout — happy paths
    // ----------------------------------------------------------------

    #[test]
    fn layout_empty_returns_empty() {
        let bar = layout_status_bar(&[], &[], 80);
        assert_eq!(bar.placements, vec![]);
        assert_eq!(bar.dropped, vec![]);
        assert_eq!(bar.bar_width, 80);
    }

    #[test]
    fn layout_left_tile_starts_at_zero() {
        let specs = vec![spec("a", TileAlignment::Left, 1, 10, 5)];
        let renders = vec![rendered("a", 5)];
        let bar = layout_status_bar(&specs, &renders, 80);
        assert_eq!(bar.placements.len(), 1);
        assert_eq!(bar.placements[0].x_start, 0);
        assert_eq!(bar.placements[0].width, 5);
    }

    #[test]
    fn layout_right_tile_ends_at_bar_width() {
        let specs = vec![spec("a", TileAlignment::Right, 1, 10, 5)];
        let renders = vec![rendered("a", 7)];
        let bar = layout_status_bar(&specs, &renders, 80);
        assert_eq!(bar.placements.len(), 1);
        assert_eq!(bar.placements[0].x_start, 73);
        assert_eq!(bar.placements[0].width, 7);
        assert_eq!(bar.placements[0].x_end(), 80);
    }

    #[test]
    fn layout_centre_tile_centres_on_bar() {
        let specs = vec![spec("a", TileAlignment::Center, 1, 20, 5)];
        let renders = vec![rendered("a", 10)];
        let bar = layout_status_bar(&specs, &renders, 80);
        // Centre of 80-cell bar around 10-cell tile: x=35
        assert_eq!(bar.placements[0].x_start, 35);
        assert_eq!(bar.placements[0].width, 10);
    }

    #[test]
    fn layout_centre_clamped_when_would_overlap_left() {
        // Self-review fix (br-ft-3zpjl): bar=22, Left=10,
        // Right=8, Center=4 (total=22). Geometric centre =
        // (22-4)/2 = 9, which would overlap Left's x=0..10.
        // Substrate clamps centre_start to left_x_end=10.
        let specs = vec![
            spec("L", TileAlignment::Left, 1, 20, 5),
            spec("C", TileAlignment::Center, 1, 20, 5),
            spec("R", TileAlignment::Right, 1, 20, 5),
        ];
        let renders = vec![rendered("L", 10), rendered("C", 4), rendered("R", 8)];
        let bar = layout_status_bar(&specs, &renders, 22);
        let l = bar.placement_for("L").unwrap();
        let c = bar.placement_for("C").unwrap();
        let r = bar.placement_for("R").unwrap();
        assert_eq!(l.x_start, 0);
        assert_eq!(l.x_end(), 10);
        // Centre clamped — must NOT overlap Left.
        assert!(
            c.x_start >= 10,
            "centre x_start={} must be >= 10",
            c.x_start
        );
        assert!(
            c.x_end() <= u32::from(r.x_start),
            "centre x_end={} must be <= right x_start={}",
            c.x_end(),
            r.x_start
        );
        assert_eq!(r.x_start, 14);
        assert_eq!(r.x_end(), 22);
    }

    #[test]
    fn layout_centre_dropped_when_band_too_narrow() {
        // Bar=20, Left=10, Right=10, Center=4 (total=24).
        // Width truncation drops one tile by priority. Setup
        // priorities so Center drops first.
        let mut left_spec = spec("L", TileAlignment::Left, 1, 20, 9);
        let mut right_spec = spec("R", TileAlignment::Right, 1, 20, 9);
        let centre_spec = spec("C", TileAlignment::Center, 1, 20, 1); // low priority
        left_spec.a11y_label = "left".to_string();
        right_spec.a11y_label = "right".to_string();
        let specs = vec![left_spec, right_spec, centre_spec];
        let renders = vec![rendered("L", 10), rendered("R", 10), rendered("C", 4)];
        let bar = layout_status_bar(&specs, &renders, 20);
        // Centre dropped first (lowest priority).
        assert!(bar.was_dropped("C"));
        assert!(bar.placement_for("L").is_some());
        assert!(bar.placement_for("R").is_some());
    }

    #[test]
    fn layout_three_alignments_pack_correctly() {
        let specs = vec![
            spec("L", TileAlignment::Left, 1, 10, 5),
            spec("C", TileAlignment::Center, 1, 10, 5),
            spec("R", TileAlignment::Right, 1, 10, 5),
        ];
        let renders = vec![rendered("L", 5), rendered("C", 4), rendered("R", 7)];
        let bar = layout_status_bar(&specs, &renders, 80);
        let l = bar.placement_for("L").unwrap();
        let c = bar.placement_for("C").unwrap();
        let r = bar.placement_for("R").unwrap();
        assert_eq!(l.x_start, 0);
        assert_eq!(l.width, 5);
        // Centre on 80-cell bar around 4-cell tile: x=38
        assert_eq!(c.x_start, 38);
        assert_eq!(c.width, 4);
        // Right at 80-7 = 73
        assert_eq!(r.x_start, 73);
        assert_eq!(r.width, 7);
    }

    #[test]
    fn layout_left_tiles_pack_left_to_right() {
        let specs = vec![
            spec("a", TileAlignment::Left, 1, 10, 5),
            spec("b", TileAlignment::Left, 1, 10, 5),
            spec("c", TileAlignment::Left, 1, 10, 5),
        ];
        let renders = vec![rendered("a", 3), rendered("b", 4), rendered("c", 5)];
        let bar = layout_status_bar(&specs, &renders, 80);
        assert_eq!(bar.placement_for("a").unwrap().x_start, 0);
        assert_eq!(bar.placement_for("b").unwrap().x_start, 3);
        assert_eq!(bar.placement_for("c").unwrap().x_start, 7);
    }

    #[test]
    fn layout_right_tiles_pack_right_to_left_in_spec_order() {
        let specs = vec![
            spec("a", TileAlignment::Right, 1, 10, 5),
            spec("b", TileAlignment::Right, 1, 10, 5),
        ];
        let renders = vec![rendered("a", 3), rendered("b", 4)];
        let bar = layout_status_bar(&specs, &renders, 80);
        // Spec order [a, b]; b ends at right edge, then a abuts left of b.
        // bar_width=80, b width=4 → b at x=76; a width=3 → a at x=73.
        assert_eq!(bar.placement_for("b").unwrap().x_start, 76);
        assert_eq!(bar.placement_for("a").unwrap().x_start, 73);
    }

    // ----------------------------------------------------------------
    // Truncation
    // ----------------------------------------------------------------

    #[test]
    fn layout_truncates_lowest_priority_first() {
        let specs = vec![
            spec("hi", TileAlignment::Left, 1, 50, 9),
            spec("mid", TileAlignment::Left, 1, 50, 5),
            spec("lo", TileAlignment::Left, 1, 50, 1),
        ];
        let renders = vec![rendered("hi", 30), rendered("mid", 30), rendered("lo", 30)];
        // Total = 90, bar_width = 80 → drop lowest priority "lo".
        let bar = layout_status_bar(&specs, &renders, 80);
        assert_eq!(bar.placements.len(), 2);
        assert!(bar.was_dropped("lo"));
        assert_eq!(bar.dropped[0].reason, DropReason::TruncatedByWidth);
    }

    #[test]
    fn layout_truncation_breaks_ties_by_id_ascending() {
        let specs = vec![
            spec("z", TileAlignment::Left, 1, 50, 5),
            spec("a", TileAlignment::Left, 1, 50, 5),
            spec("m", TileAlignment::Left, 1, 50, 5),
        ];
        let renders = vec![rendered("z", 30), rendered("a", 30), rendered("m", 30)];
        // Total = 90 > 80; all tied at priority 5; drop "a" first
        // (lowest id).
        let bar = layout_status_bar(&specs, &renders, 80);
        assert!(bar.was_dropped("a"));
        assert!(!bar.was_dropped("m"));
        assert!(!bar.was_dropped("z"));
    }

    #[test]
    fn layout_truncates_until_fits() {
        let specs = vec![
            spec("a", TileAlignment::Left, 1, 50, 1),
            spec("b", TileAlignment::Left, 1, 50, 2),
            spec("c", TileAlignment::Left, 1, 50, 3),
        ];
        let renders = vec![rendered("a", 50), rendered("b", 50), rendered("c", 50)];
        // bar_width=80; total=150; need to drop until fits.
        // Drop a (priority 1) → total=100; drop b (priority 2) → total=50; fits.
        let bar = layout_status_bar(&specs, &renders, 80);
        assert!(bar.was_dropped("a"));
        assert!(bar.was_dropped("b"));
        assert_eq!(bar.placements.len(), 1);
        assert_eq!(bar.placements[0].tile_id, "c");
    }

    #[test]
    fn layout_drops_overflowing_tile_with_reason() {
        let specs = vec![spec("a", TileAlignment::Left, 1, 5, 5)];
        let renders = vec![rendered("a", 10)]; // > max_width
        let bar = layout_status_bar(&specs, &renders, 80);
        assert!(bar.placements.is_empty());
        assert_eq!(bar.dropped.len(), 1);
        assert_eq!(bar.dropped[0].tile_id, "a");
        assert_eq!(bar.dropped[0].reason, DropReason::OverflowsMaxWidth);
    }

    #[test]
    fn layout_drops_underflowing_tile_with_reason() {
        let specs = vec![spec("a", TileAlignment::Left, 5, 10, 5)];
        let renders = vec![rendered("a", 1)]; // < min_width
        let bar = layout_status_bar(&specs, &renders, 80);
        assert!(bar.placements.is_empty());
        assert_eq!(bar.dropped[0].reason, DropReason::UnderflowsMinWidth);
    }

    #[test]
    fn layout_skips_tiles_without_render() {
        let specs = vec![spec("a", TileAlignment::Left, 1, 10, 5)];
        // No rendered tile (refresh hint hasn't fired yet).
        let bar = layout_status_bar(&specs, &[], 80);
        assert!(bar.placements.is_empty());
        // Not dropped — just absent.
        assert!(bar.dropped.is_empty());
    }

    // ----------------------------------------------------------------
    // Hit-test
    // ----------------------------------------------------------------

    #[test]
    fn hit_test_left_tile() {
        let specs = vec![spec("a", TileAlignment::Left, 1, 10, 5)];
        let renders = vec![rendered("a", 5)];
        let bar = layout_status_bar(&specs, &renders, 80);
        let hit = bar.hit_test(2).unwrap();
        assert_eq!(hit.tile_id, "a");
        assert_eq!(hit.x_in_tile, 2);
    }

    #[test]
    fn hit_test_at_tile_boundary() {
        let specs = vec![spec("a", TileAlignment::Left, 1, 10, 5)];
        let renders = vec![rendered("a", 5)];
        let bar = layout_status_bar(&specs, &renders, 80);
        // x=5 is one past the tile (tile occupies 0..5 exclusive).
        assert!(bar.hit_test(5).is_none());
        // x=4 is the last cell of the tile.
        let hit = bar.hit_test(4).unwrap();
        assert_eq!(hit.x_in_tile, 4);
    }

    #[test]
    fn hit_test_in_gap_returns_none() {
        let specs = vec![
            spec("a", TileAlignment::Left, 1, 10, 5),
            spec("b", TileAlignment::Right, 1, 10, 5),
        ];
        let renders = vec![rendered("a", 5), rendered("b", 5)];
        let bar = layout_status_bar(&specs, &renders, 80);
        // Gap is x=5..75
        assert!(bar.hit_test(40).is_none());
    }

    #[test]
    fn hit_test_centre_tile() {
        let specs = vec![spec("c", TileAlignment::Center, 1, 10, 5)];
        let renders = vec![rendered("c", 6)];
        let bar = layout_status_bar(&specs, &renders, 80);
        // Centre tile starts at 37 (80-6=74 / 2 = 37)
        let placement = bar.placement_for("c").unwrap();
        let hit = bar.hit_test(placement.x_start + 2).unwrap();
        assert_eq!(hit.tile_id, "c");
        assert_eq!(hit.x_in_tile, 2);
    }

    // ----------------------------------------------------------------
    // RenderedTile + tooltip
    // ----------------------------------------------------------------

    #[test]
    fn rendered_tile_with_tooltip() {
        let r = RenderedTile::new(5).with_tooltip("hover me");
        assert_eq!(r.width, 5);
        assert_eq!(r.tooltip.as_deref(), Some("hover me"));
    }

    #[test]
    fn rendered_tile_default_no_tooltip() {
        let r = RenderedTile::new(5);
        assert_eq!(r.tooltip, None);
    }

    // ----------------------------------------------------------------
    // TileSpec helpers
    // ----------------------------------------------------------------

    #[test]
    fn tile_spec_with_refresh_hint() {
        let s =
            spec("a", TileAlignment::Left, 1, 10, 5).with_refresh(TileRefreshHint::EveryMs(250));
        assert_eq!(s.refresh_hint, TileRefreshHint::EveryMs(250));
    }

    // ----------------------------------------------------------------
    // Cross-cut: realistic scenarios
    // ----------------------------------------------------------------

    #[test]
    fn scenario_built_in_8_tile_default_layout() {
        // The bead lists 8 default built-in tiles. Mock them out as
        // well-formed specs and confirm the layout fits in an 80-cell
        // bar without any drops.
        let specs = vec![
            spec("ft.mode", TileAlignment::Left, 4, 8, 9),
            spec("ft.session", TileAlignment::Left, 4, 16, 7),
            spec("ft.panes", TileAlignment::Left, 4, 8, 6),
            spec("ft.agents", TileAlignment::Center, 8, 16, 8),
            spec("ft.fleet_memory", TileAlignment::Center, 4, 8, 5),
            spec("ft.cost", TileAlignment::Right, 4, 8, 4),
            spec("ft.network", TileAlignment::Right, 4, 8, 3),
            spec("ft.clock", TileAlignment::Right, 5, 5, 9),
        ];
        let renders = vec![
            rendered("ft.mode", 6),
            rendered("ft.session", 12),
            rendered("ft.panes", 5),
            rendered("ft.agents", 10),
            rendered("ft.fleet_memory", 6),
            rendered("ft.cost", 6),
            rendered("ft.network", 6),
            rendered("ft.clock", 5),
        ];
        let bar = layout_status_bar(&specs, &renders, 80);
        assert_eq!(bar.placements.len(), 8);
        assert!(bar.dropped.is_empty());
        // Smoke test: clock is the rightmost tile.
        let placements_sorted = &bar.placements;
        let rightmost = placements_sorted.last().unwrap();
        assert_eq!(rightmost.tile_id, "ft.clock");
    }

    #[test]
    fn scenario_narrow_bar_drops_low_priority_tiles() {
        // Bar shrinks (operator resized window); low-priority tiles
        // drop in priority order until the layout fits.
        let specs = vec![
            spec("ft.mode", TileAlignment::Left, 4, 8, 9),
            spec("ft.clock", TileAlignment::Right, 5, 5, 9),
            spec("ft.session", TileAlignment::Left, 4, 16, 7),
            spec("ft.panes", TileAlignment::Left, 4, 8, 6),
            spec("ft.cost", TileAlignment::Right, 4, 8, 4),
            spec("ft.network", TileAlignment::Right, 4, 8, 3),
        ];
        let renders = vec![
            rendered("ft.mode", 6),
            rendered("ft.clock", 5),
            rendered("ft.session", 12),
            rendered("ft.panes", 5),
            rendered("ft.cost", 6),
            rendered("ft.network", 6),
        ];
        // Total = 40; bar = 30 → drop network (priority 3), then
        // cost (priority 4). After drops, total = 28 ≤ 30.
        let bar = layout_status_bar(&specs, &renders, 30);
        assert!(bar.was_dropped("ft.network"));
        assert!(bar.was_dropped("ft.cost"));
        // High-priority tiles survive.
        assert!(!bar.was_dropped("ft.mode"));
        assert!(!bar.was_dropped("ft.clock"));
    }

    #[test]
    fn scenario_validation_then_layout_full_pipeline() {
        let specs = vec![
            spec("ft.mode", TileAlignment::Left, 4, 8, 9),
            spec("ft.clock", TileAlignment::Right, 5, 5, 9),
        ];
        // Validation passes.
        validate_tile_specs(&specs).unwrap();
        // Layout produces correct placements.
        let renders = vec![rendered("ft.mode", 6), rendered("ft.clock", 5)];
        let bar = layout_status_bar(&specs, &renders, 40);
        assert_eq!(bar.placements.len(), 2);
        assert_eq!(bar.placement_for("ft.mode").unwrap().x_start, 0);
        assert_eq!(bar.placement_for("ft.clock").unwrap().x_start, 35);
        // Hit-test works.
        assert_eq!(bar.hit_test(2).unwrap().tile_id, "ft.mode");
        assert_eq!(bar.hit_test(36).unwrap().tile_id, "ft.clock");
    }
}
