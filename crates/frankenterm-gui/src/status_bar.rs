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
        let width = label
            .len()
            .clamp(self.min_width() as usize, self.max_width() as usize) as u16;
        RenderedTile::new(width).with_tooltip(self.accessibility_label(ctx))
    }

    fn on_click(&mut self, _x_in_tile: u16) -> Option<TileAction> {
        self.action()
    }

    fn on_hover(&self, _x_in_tile: u16) -> Option<String> {
        Some(self.source().to_string())
    }
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
}
