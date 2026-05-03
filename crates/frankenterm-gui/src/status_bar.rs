//! GUI status-bar integration surface.
//!
//! The core crate owns deterministic layout/truncation. This module owns
//! the GUI-facing tile trait, built-in tile descriptors, and refresh cadence
//! bookkeeping that the render loop can call without knowing each tile's
//! implementation details.

use frankenterm_core::status_bar::{
    RenderedTile, TileAlignment, TileRefreshHint, TileSpec, TileValidationError,
    validate_tile_specs,
};
use std::collections::{BTreeSet, HashMap};
use termwiz::cell::unicode_column_width;

/// Snapshot of GUI state available to status tiles for one render tick.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusTileContext {
    pub mode_label: String,
    pub session_name: String,
    pub active_pane_index: u16,
    pub pane_count: u16,
    pub codex_agents: u16,
    pub claude_agents: u16,
    pub gemini_agents: u16,
    pub fleet_memory_tier: String,
    pub session_cost_usd: f64,
    pub network_bytes_per_sec: u64,
    pub local_time_label: String,
    pub utc_time_label: String,
}

impl Default for StatusTileContext {
    fn default() -> Self {
        Self {
            mode_label: "Normal".to_string(),
            session_name: "default".to_string(),
            active_pane_index: 1,
            pane_count: 1,
            codex_agents: 0,
            claude_agents: 0,
            gemini_agents: 0,
            fleet_memory_tier: "Normal".to_string(),
            session_cost_usd: 0.0,
            network_bytes_per_sec: 0,
            local_time_label: "--:--".to_string(),
            utc_time_label: "--:-- UTC".to_string(),
        }
    }
}

/// Action emitted when a tile handles a click.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TileAction {
    ToggleMode,
    OpenSessionSwitcher,
    OpenPaneList,
    OpenAgentList,
    OpenFleetMemoryPanel,
    ShowCostBreakdown,
    ShowNetworkPanel,
    ShowClockSettings,
}

/// GUI status-tile trait. Plugin tiles and built-ins share this surface.
pub trait StatusTile: Send + Sync {
    fn id(&self) -> &'static str;
    fn alignment(&self) -> TileAlignment;
    fn min_width(&self) -> u16;
    fn max_width(&self) -> u16;
    fn priority(&self) -> u8;
    fn refresh_hint(&self) -> TileRefreshHint;
    fn accessibility_label(&self, ctx: &StatusTileContext) -> String;
    fn render(&self, ctx: &StatusTileContext) -> RenderedTile;

    fn on_click(&mut self, _x_in_tile: u16) -> Option<TileAction> {
        None
    }

    fn on_hover(&self, _x_in_tile: u16) -> Option<String> {
        None
    }

    fn spec(&self, ctx: &StatusTileContext) -> TileSpec {
        TileSpec::new(
            self.id(),
            self.alignment(),
            self.min_width(),
            self.max_width(),
            self.priority(),
            self.accessibility_label(ctx),
        )
        .with_refresh(self.refresh_hint())
    }
}

/// Built-in status tiles shipped by the GUI integration layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltInStatusTile {
    Mode,
    Session,
    Panes,
    Agents,
    FleetMemory,
    Cost,
    Network,
    Clock,
}

impl BuiltInStatusTile {
    pub const ALL: [Self; 8] = [
        Self::Mode,
        Self::Session,
        Self::Panes,
        Self::Agents,
        Self::FleetMemory,
        Self::Cost,
        Self::Network,
        Self::Clock,
    ];

    #[must_use]
    pub const fn source(self) -> &'static str {
        match self {
            Self::Mode => "builtin:ft.mode",
            Self::Session => "builtin:ft.session",
            Self::Panes => "builtin:ft.panes",
            Self::Agents => "builtin:ft.agents",
            Self::FleetMemory => "builtin:ft.fleet_memory",
            Self::Cost => "builtin:ft.cost",
            Self::Network => "builtin:ft.network",
            Self::Clock => "builtin:ft.clock",
        }
    }

    #[must_use]
    pub const fn action(self) -> Option<TileAction> {
        match self {
            Self::Mode => Some(TileAction::ToggleMode),
            Self::Session => Some(TileAction::OpenSessionSwitcher),
            Self::Panes => Some(TileAction::OpenPaneList),
            Self::Agents => Some(TileAction::OpenAgentList),
            Self::FleetMemory => Some(TileAction::OpenFleetMemoryPanel),
            Self::Cost => Some(TileAction::ShowCostBreakdown),
            Self::Network => Some(TileAction::ShowNetworkPanel),
            Self::Clock => Some(TileAction::ShowClockSettings),
        }
    }

    fn rendered_label(self, ctx: &StatusTileContext) -> String {
        match self {
            Self::Mode => ctx.mode_label.clone(),
            Self::Session => ctx.session_name.clone(),
            Self::Panes => format!("{}/{}", ctx.active_pane_index, ctx.pane_count),
            Self::Agents => format!(
                "cod:{} cc:{} gmi:{}",
                ctx.codex_agents, ctx.claude_agents, ctx.gemini_agents
            ),
            Self::FleetMemory => ctx.fleet_memory_tier.clone(),
            Self::Cost => format!("${:.2}", ctx.session_cost_usd),
            Self::Network => format!("{} B/s", ctx.network_bytes_per_sec),
            Self::Clock => format!("{} {}", ctx.local_time_label, ctx.utc_time_label),
        }
    }
}

impl StatusTile for BuiltInStatusTile {
    fn id(&self) -> &'static str {
        match self {
            Self::Mode => "ft.mode",
            Self::Session => "ft.session",
            Self::Panes => "ft.panes",
            Self::Agents => "ft.agents",
            Self::FleetMemory => "ft.fleet_memory",
            Self::Cost => "ft.cost",
            Self::Network => "ft.network",
            Self::Clock => "ft.clock",
        }
    }

    fn alignment(&self) -> TileAlignment {
        match self {
            Self::Mode | Self::Session | Self::Panes => TileAlignment::Left,
            Self::Agents | Self::FleetMemory => TileAlignment::Center,
            Self::Cost | Self::Network | Self::Clock => TileAlignment::Right,
        }
    }

    fn min_width(&self) -> u16 {
        match self {
            Self::Mode => 4,
            Self::Session => 4,
            Self::Panes => 3,
            Self::Agents => 8,
            Self::FleetMemory => 6,
            Self::Cost => 4,
            Self::Network => 5,
            Self::Clock => 8,
        }
    }

    fn max_width(&self) -> u16 {
        match self {
            Self::Mode => 12,
            Self::Session => 24,
            Self::Panes => 8,
            Self::Agents => 24,
            Self::FleetMemory => 16,
            Self::Cost => 12,
            Self::Network => 16,
            Self::Clock => 28,
        }
    }

    fn priority(&self) -> u8 {
        match self {
            Self::Mode => 240,
            Self::Session => 220,
            Self::Panes => 230,
            Self::Agents => 210,
            Self::FleetMemory => 200,
            Self::Cost => 120,
            Self::Network => 130,
            Self::Clock => 100,
        }
    }

    fn refresh_hint(&self) -> TileRefreshHint {
        match self {
            Self::Mode | Self::Session | Self::Panes => TileRefreshHint::OnEvent,
            Self::Agents => TileRefreshHint::EveryMs(250),
            Self::FleetMemory | Self::Network | Self::Clock => TileRefreshHint::EveryMs(1_000),
            Self::Cost => TileRefreshHint::EveryMs(5_000),
        }
    }

    fn accessibility_label(&self, ctx: &StatusTileContext) -> String {
        match self {
            Self::Mode => format!("mode {}", ctx.mode_label),
            Self::Session => format!("session {}", ctx.session_name),
            Self::Panes => format!("pane {} of {}", ctx.active_pane_index, ctx.pane_count),
            Self::Agents => format!(
                "agents codex {}, claude {}, gemini {}",
                ctx.codex_agents, ctx.claude_agents, ctx.gemini_agents
            ),
            Self::FleetMemory => format!("fleet memory {}", ctx.fleet_memory_tier),
            Self::Cost => format!("session cost {:.2} dollars", ctx.session_cost_usd),
            Self::Network => format!("network {} bytes per second", ctx.network_bytes_per_sec),
            Self::Clock => format!("local {}, utc {}", ctx.local_time_label, ctx.utc_time_label),
        }
    }

    fn render(&self, ctx: &StatusTileContext) -> RenderedTile {
        let label = self.rendered_label(ctx);
        let width = tile_label_cell_width(&label, self.min_width(), self.max_width());
        RenderedTile::new(width).with_tooltip(self.accessibility_label(ctx))
    }

    fn on_click(&mut self, _x_in_tile: u16) -> Option<TileAction> {
        self.action()
    }

    fn on_hover(&self, _x_in_tile: u16) -> Option<String> {
        Some(self.source().to_string())
    }
}

fn tile_label_cell_width(label: &str, min_width: u16, max_width: u16) -> u16 {
    unicode_column_width(label, None).clamp(min_width as usize, max_width as usize) as u16
}

/// Construct the default built-in tile list in stable render order.
#[must_use]
pub fn default_builtin_tiles() -> Vec<BuiltInStatusTile> {
    BuiltInStatusTile::ALL.to_vec()
}

/// Build TileSpec records for a configured tile set.
pub fn tile_specs_for(
    tiles: &[BuiltInStatusTile],
    ctx: &StatusTileContext,
) -> Result<Vec<TileSpec>, TileValidationError> {
    let specs: Vec<TileSpec> = tiles.iter().map(|tile| tile.spec(ctx)).collect();
    validate_tile_specs(&specs)?;
    Ok(specs)
}

/// Event/timer scheduler state for status-tile rerenders.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StatusTileRefreshScheduler {
    last_rendered_at_ms: HashMap<String, u64>,
    event_dirty_tiles: BTreeSet<String>,
}

impl StatusTileRefreshScheduler {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_event_dirty(&mut self, tile_id: impl Into<String>) {
        self.event_dirty_tiles.insert(tile_id.into());
    }

    pub fn mark_rendered(&mut self, tile_id: &str, now_ms: u64) {
        self.last_rendered_at_ms.insert(tile_id.to_string(), now_ms);
        self.event_dirty_tiles.remove(tile_id);
    }

    #[must_use]
    pub fn should_render(&self, tile: &impl StatusTile, now_ms: u64) -> bool {
        match tile.refresh_hint() {
            TileRefreshHint::OnEvent => {
                !self.last_rendered_at_ms.contains_key(tile.id())
                    || self.event_dirty_tiles.contains(tile.id())
            }
            TileRefreshHint::EveryMs(interval_ms) => self
                .last_rendered_at_ms
                .get(tile.id())
                .is_none_or(|last| now_ms.saturating_sub(*last) >= u64::from(interval_ms)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use frankenterm_core::status_bar::{LaidOutBar, layout_status_bar};
    use proptest::prelude::*;

    #[test]
    fn default_tiles_ship_eight_unique_valid_specs() {
        let ctx = StatusTileContext::default();
        let tiles = default_builtin_tiles();
        let specs = tile_specs_for(&tiles, &ctx).unwrap();

        assert_eq!(specs.len(), 8);
        assert!(specs.iter().all(|spec| !spec.a11y_label.is_empty()));
        let ids: BTreeSet<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
        assert_eq!(ids.len(), 8);
        assert!(ids.contains("ft.mode"));
        assert!(ids.contains("ft.clock"));
    }

    #[test]
    fn status_tile_builtin_conformance() {
        let ctx = StatusTileContext {
            mode_label: "Command".to_string(),
            session_name: "prod-repair-session".to_string(),
            active_pane_index: 7,
            pane_count: 12,
            codex_agents: 4,
            claude_agents: 3,
            gemini_agents: 2,
            fleet_memory_tier: "Elevated".to_string(),
            session_cost_usd: 42.37,
            network_bytes_per_sec: 65_536,
            local_time_label: "10:45".to_string(),
            utc_time_label: "14:45 UTC".to_string(),
        };
        let mut ids = BTreeSet::new();
        let mut sources = BTreeSet::new();
        let mut specs = Vec::new();

        for tile in BuiltInStatusTile::ALL {
            let spec = tile.spec(&ctx);
            let rendered = tile.render(&ctx);
            let mut clickable_tile = tile;

            assert!(
                ids.insert(tile.id()),
                "duplicate status tile id {}",
                tile.id()
            );
            assert!(
                sources.insert(tile.source()),
                "duplicate status tile source {}",
                tile.source()
            );
            assert_eq!(spec.id, tile.id());
            assert_eq!(spec.alignment, tile.alignment());
            assert_eq!(spec.min_width, tile.min_width());
            assert_eq!(spec.max_width, tile.max_width());
            assert_eq!(spec.priority, tile.priority());
            assert_eq!(spec.refresh_hint, tile.refresh_hint());
            assert_eq!(spec.a11y_label, tile.accessibility_label(&ctx));
            assert!(
                !spec.a11y_label.trim().is_empty(),
                "{} has an empty accessibility label",
                tile.id()
            );
            assert!(
                rendered.width >= spec.min_width && rendered.width <= spec.max_width,
                "{} rendered width {} outside [{}, {}]",
                tile.id(),
                rendered.width,
                spec.min_width,
                spec.max_width
            );
            assert_eq!(rendered.tooltip.as_deref(), Some(spec.a11y_label.as_str()));
            assert_eq!(clickable_tile.on_click(0), tile.action());
            assert_eq!(tile.on_hover(0).as_deref(), Some(tile.source()));
            match tile.refresh_hint() {
                TileRefreshHint::OnEvent => {}
                TileRefreshHint::EveryMs(interval_ms) => assert!(
                    (1..=5_000).contains(&interval_ms),
                    "{} refresh interval {}ms is outside GUI status-bar bounds",
                    tile.id(),
                    interval_ms
                ),
            }

            specs.push(spec);
        }

        assert_eq!(ids.len(), BuiltInStatusTile::ALL.len());
        assert_eq!(sources.len(), BuiltInStatusTile::ALL.len());
        validate_tile_specs(&specs).unwrap();
    }

    #[test]
    fn builtin_tiles_render_with_expected_refresh_cadence() {
        assert_eq!(
            BuiltInStatusTile::Mode.refresh_hint(),
            TileRefreshHint::OnEvent
        );
        assert_eq!(
            BuiltInStatusTile::Agents.refresh_hint(),
            TileRefreshHint::EveryMs(250)
        );
        assert_eq!(
            BuiltInStatusTile::Cost.refresh_hint(),
            TileRefreshHint::EveryMs(5_000)
        );
    }

    #[test]
    fn scheduler_respects_event_and_interval_tiles() {
        let mut scheduler = StatusTileRefreshScheduler::new();
        let mode = BuiltInStatusTile::Mode;
        let agents = BuiltInStatusTile::Agents;

        assert!(scheduler.should_render(&mode, 0));
        scheduler.mark_rendered(mode.id(), 0);
        assert!(!scheduler.should_render(&mode, 1));
        scheduler.mark_event_dirty(mode.id());
        assert!(scheduler.should_render(&mode, 2));

        assert!(scheduler.should_render(&agents, 0));
        scheduler.mark_rendered(agents.id(), 1_000);
        assert!(!scheduler.should_render(&agents, 1_249));
        assert!(scheduler.should_render(&agents, 1_250));
    }

    #[test]
    fn click_and_hover_are_routed_through_tile_trait() {
        let mut tile = BuiltInStatusTile::Cost;
        assert_eq!(tile.on_click(0), Some(TileAction::ShowCostBreakdown));
        assert_eq!(tile.on_hover(0), Some("builtin:ft.cost".to_string()));
    }

    #[test]
    fn rendered_tile_width_is_bounded_by_tile_spec() {
        let ctx = StatusTileContext {
            session_name: "a-very-long-session-name-that-will-not-fit".to_string(),
            ..StatusTileContext::default()
        };
        let rendered = BuiltInStatusTile::Session.render(&ctx);
        assert_eq!(rendered.width, BuiltInStatusTile::Session.max_width());
        assert!(rendered.tooltip.unwrap().contains("session"));
    }

    #[test]
    fn rendered_tile_width_uses_terminal_cells_not_utf8_bytes() {
        let ctx = StatusTileContext {
            session_name: "\u{00e9}".repeat(13),
            ..StatusTileContext::default()
        };

        let rendered = BuiltInStatusTile::Session.render(&ctx);

        assert_eq!(ctx.session_name.len(), 26);
        assert_eq!(rendered.width, 13);
    }

    fn builtin_layout_for(ctx: &StatusTileContext, bar_width: u16) -> LaidOutBar {
        let tiles = default_builtin_tiles();
        let specs = tile_specs_for(&tiles, ctx).unwrap();
        let rendered: Vec<(String, RenderedTile)> = tiles
            .iter()
            .map(|tile| (tile.id().to_string(), tile.render(ctx)))
            .collect();

        layout_status_bar(&specs, &rendered, bar_width)
    }

    fn arb_status_label(max_len: usize) -> impl Strategy<Value = String> {
        let alphabet = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 _-:.";
        prop::collection::vec(
            prop::sample::select(alphabet.chars().collect::<Vec<_>>()),
            0..max_len,
        )
        .prop_map(|chars| chars.into_iter().collect())
    }

    fn arb_pane_position() -> impl Strategy<Value = (u16, u16)> {
        (1_u16..=512).prop_flat_map(|pane_count| (1_u16..=pane_count, Just(pane_count)))
    }

    fn arb_status_tile_context() -> impl Strategy<Value = StatusTileContext> {
        (
            arb_status_label(24),
            arb_status_label(64),
            arb_pane_position(),
            0_u16..=512,
            0_u16..=512,
            0_u16..=512,
            arb_status_label(24),
            0_u64..=1_000_000,
            0_u64..=100_000_000_000,
            arb_status_label(16),
            arb_status_label(24),
        )
            .prop_map(
                |(
                    mode_label,
                    session_name,
                    (active_pane_index, pane_count),
                    codex_agents,
                    claude_agents,
                    gemini_agents,
                    fleet_memory_tier,
                    session_cost_cents,
                    network_bytes_per_sec,
                    local_time_label,
                    utc_time_label,
                )| StatusTileContext {
                    mode_label,
                    session_name,
                    active_pane_index,
                    pane_count,
                    codex_agents,
                    claude_agents,
                    gemini_agents,
                    fleet_memory_tier,
                    session_cost_usd: session_cost_cents as f64 / 100.0,
                    network_bytes_per_sec,
                    local_time_label,
                    utc_time_label,
                },
            )
    }

    fn assert_layout_invariants(bar: &LaidOutBar) {
        let mut last_end = 0_u32;
        let mut placed_ids = BTreeSet::new();
        let mut dropped_ids = BTreeSet::new();

        for placement in &bar.placements {
            assert!(
                placed_ids.insert(placement.tile_id.as_str()),
                "duplicate placement for {}",
                placement.tile_id
            );
            assert!(
                u32::from(placement.x_start) >= last_end,
                "{} starts at {} before previous end {}",
                placement.tile_id,
                placement.x_start,
                last_end
            );
            assert!(
                placement.x_end() <= u32::from(bar.bar_width),
                "{} ends at {} beyond bar width {}",
                placement.tile_id,
                placement.x_end(),
                bar.bar_width
            );
            assert_eq!(
                bar.hit_test(placement.x_start)
                    .map(|hit| (hit.tile_id, hit.x_in_tile)),
                Some((placement.tile_id.clone(), 0)),
                "{} first cell should hit-test back to the tile",
                placement.tile_id
            );
            assert_eq!(
                bar.hit_test(placement.x_start + placement.width - 1)
                    .map(|hit| (hit.tile_id, hit.x_in_tile)),
                Some((placement.tile_id.clone(), placement.width - 1)),
                "{} last cell should hit-test back to the tile",
                placement.tile_id
            );
            last_end = placement.x_end();
        }

        for dropped in &bar.dropped {
            assert!(
                dropped_ids.insert(dropped.tile_id.as_str()),
                "duplicate dropped tile {}",
                dropped.tile_id
            );
            assert!(
                !placed_ids.contains(dropped.tile_id.as_str()),
                "{} cannot be both placed and dropped",
                dropped.tile_id
            );
        }
    }

    #[test]
    fn builtin_tile_arrangement_is_deterministic_across_pane_configs_and_resize() {
        let contexts = [
            StatusTileContext::default(),
            StatusTileContext {
                mode_label: "Command".to_string(),
                session_name: "ops-grid".to_string(),
                active_pane_index: 3,
                pane_count: 9,
                codex_agents: 5,
                claude_agents: 2,
                gemini_agents: 1,
                fleet_memory_tier: "High".to_string(),
                session_cost_usd: 19.25,
                network_bytes_per_sec: 12_048,
                local_time_label: "09:42".to_string(),
                utc_time_label: "13:42 UTC".to_string(),
            },
            StatusTileContext {
                mode_label: "Resize".to_string(),
                session_name: "wide-unicode-\u{00e9}\u{00e9}".to_string(),
                active_pane_index: 12,
                pane_count: 24,
                codex_agents: 13,
                claude_agents: 8,
                gemini_agents: 5,
                fleet_memory_tier: "Elevated".to_string(),
                session_cost_usd: 123.45,
                network_bytes_per_sec: 987_654,
                local_time_label: "23:59".to_string(),
                utc_time_label: "03:59 UTC".to_string(),
            },
        ];

        for ctx in &contexts {
            for bar_width in [18_u16, 24, 32, 48, 80, 120] {
                let first = builtin_layout_for(ctx, bar_width);
                let second = builtin_layout_for(ctx, bar_width);

                assert_eq!(
                    first, second,
                    "status bar layout must be deterministic for {ctx:?} at width {bar_width}"
                );
                assert_layout_invariants(&first);
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(128))]

        #[test]
        fn proptest_status_bar_layout_preserves_generated_context_invariants(
            ctx in arb_status_tile_context(),
            bar_width in 0_u16..=192
        ) {
            let tiles = default_builtin_tiles();
            let specs = tile_specs_for(&tiles, &ctx).unwrap();
            let rendered: Vec<(String, RenderedTile)> = tiles
                .iter()
                .map(|tile| {
                    let rendered = tile.render(&ctx);
                    let spec = tile.spec(&ctx);

                    prop_assert!(
                        rendered.width >= spec.min_width && rendered.width <= spec.max_width,
                        "{} rendered width {} outside [{}, {}] for {:?}",
                        tile.id(),
                        rendered.width,
                        spec.min_width,
                        spec.max_width,
                        ctx
                    );
                    prop_assert_eq!(
                        rendered.tooltip.as_deref(),
                        Some(spec.a11y_label.as_str()),
                        "{} tooltip drifted from accessibility label",
                        tile.id()
                    );

                    Ok((tile.id().to_string(), rendered))
                })
                .collect::<Result<_, TestCaseError>>()?;

            let first = layout_status_bar(&specs, &rendered, bar_width);
            let second = layout_status_bar(&specs, &rendered, bar_width);

            prop_assert_eq!(
                &first,
                &second,
                "status bar layout changed for same generated context at width {}: {:?}",
                bar_width,
                ctx
            );
            assert_layout_invariants(&first);

            let placed_ids = first
                .placements
                .iter()
                .map(|placement| placement.tile_id.as_str())
                .collect::<BTreeSet<_>>();
            let dropped_ids = first
                .dropped
                .iter()
                .map(|dropped| dropped.tile_id.as_str())
                .collect::<BTreeSet<_>>();
            let all_layout_ids = placed_ids.union(&dropped_ids).copied().collect::<BTreeSet<_>>();
            let all_tile_ids = tiles.iter().map(|tile| tile.id()).collect::<BTreeSet<_>>();

            prop_assert_eq!(
                all_layout_ids,
                all_tile_ids,
                "layout must account for every built-in tile exactly once"
            );
        }
    }

    #[test]
    fn builtin_tile_resize_preserves_survivors_when_width_grows() {
        let ctx = StatusTileContext {
            mode_label: "Command".to_string(),
            session_name: "swarm-status".to_string(),
            active_pane_index: 7,
            pane_count: 16,
            codex_agents: 6,
            claude_agents: 4,
            gemini_agents: 2,
            fleet_memory_tier: "Elevated".to_string(),
            session_cost_usd: 84.20,
            network_bytes_per_sec: 250_000,
            local_time_label: "10:15".to_string(),
            utc_time_label: "14:15 UTC".to_string(),
        };
        let mut previous_survivors: BTreeSet<String> = BTreeSet::new();

        for bar_width in [20_u16, 28, 36, 48, 64, 96, 128] {
            let bar = builtin_layout_for(&ctx, bar_width);
            assert_layout_invariants(&bar);

            let survivors: BTreeSet<String> = bar
                .placements
                .iter()
                .map(|placement| placement.tile_id.clone())
                .collect();
            assert!(
                previous_survivors.is_subset(&survivors),
                "growing to width {bar_width} dropped survivors: previous={previous_survivors:?}, current={survivors:?}"
            );
            previous_survivors = survivors;
        }
    }
}
