use super::*;
use luahelper::mlua::LuaSerdeExt;
use luahelper::{dynamic_to_lua_value, from_lua, to_lua};
use mlua::Value;
use mux::pane::CachePolicy;
use std::cmp::Ordering;
use std::convert::TryFrom;
use std::ops::Range;
use std::sync::Arc;
use termwiz::cell::SemanticType;
use termwiz_funcs::lines_to_escapes;
use url_funcs::Url;
use wezterm_term::{SemanticZone, StableRowIndex};

#[derive(Clone, Copy, Debug)]
pub struct MuxPane(pub PaneId);

impl MuxPane {
    pub fn resolve(&self, mux: &Arc<Mux>) -> mlua::Result<Arc<dyn Pane>> {
        mux.get_pane(self.0)
            .ok_or_else(|| mlua::Error::external(format!("pane id {} not found in mux", self.0)))
    }

    fn get_text_from_semantic_zone(&self, zone: SemanticZone) -> mlua::Result<String> {
        let mux = get_mux()?;
        let pane = self.resolve(&mux)?;
        pane.get_text_from_semantic_zone(zone)
            .map_err(mlua::Error::external)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_row_range_for_tail_checks_bottom_row_arithmetic() {
        assert_eq!(visible_row_range_for_tail(10, 5, 3).unwrap(), 12..15);
        assert!(visible_row_range_for_tail(StableRowIndex::MAX, 1, 1).is_err());
        assert!(visible_row_range_for_tail(0, usize::MAX, 1).is_err());
    }
}

fn visible_row_range_for_tail(
    physical_top: StableRowIndex,
    viewport_rows: usize,
    nlines: usize,
) -> mlua::Result<Range<StableRowIndex>> {
    let viewport_rows = StableRowIndex::try_from(viewport_rows)
        .map_err(|_| mlua::Error::external("viewport row count exceeds stable row range"))?;
    let bottom_row = physical_top
        .checked_add(viewport_rows)
        .ok_or_else(|| mlua::Error::external("stable row range overflow"))?;
    let nlines = StableRowIndex::try_from(nlines).unwrap_or(StableRowIndex::MAX);
    let top_row = bottom_row.saturating_sub(nlines);

    Ok(top_row..bottom_row)
}

impl UserData for MuxPane {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, _: ()| {
            Ok(format!("MuxPane(pane_id:{}, pid:{})", this.0, unsafe {
                libc::getpid()
            }))
        });
        methods.add_method("pane_id", |_, this, _: ()| Ok(this.0));

        methods.add_method("split", |_, this, args: Option<SplitPane>| {
            promise::spawn::block_on(args.unwrap_or_default().run(this))
        });

        methods.add_method("send_paste", |_, this, text: String| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            pane.send_paste(&text)
                .map_err(|e| mlua::Error::external(format!("{:#}", e)))?;
            Ok(())
        });

        // An alias of send-paste for backwards compatibility with prior releases when there was a
        // separate Gui-level PaneObject
        methods.add_method("paste", |_, this, text: String| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            pane.send_paste(&text)
                .map_err(|e| mlua::Error::external(format!("{:#}", e)))?;
            Ok(())
        });

        methods.add_method("send_text", |_, this, text: String| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            pane.writer()
                .write_all(text.as_bytes())
                .map_err(|e| mlua::Error::external(format!("{:#}", e)))?;
            Ok(())
        });
        methods.add_method("window", |_, this, _: ()| {
            let mux = get_mux()?;
            Ok(mux
                .resolve_pane_id(this.0)
                .map(|(_domain_id, window_id, _tab_id)| MuxWindow(window_id)))
        });
        methods.add_method("tab", |_, this, _: ()| {
            let mux = get_mux()?;
            Ok(mux
                .resolve_pane_id(this.0)
                .map(|(_domain_id, _window_id, tab_id)| MuxTab(tab_id)))
        });

        // For backwards compatibility with prior releases when there
        // was a separate Gui-level PaneObject
        methods.add_method("mux_pane", |_, this, _: ()| Ok(*this));

        methods.add_method("get_title", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.get_title())
        });

        methods.add_method("get_progress", |lua, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            let progress = pane.get_progress();
            lua.to_value(&progress)
        });

        methods.add_method("get_current_working_dir", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane
                .get_current_working_dir(CachePolicy::FetchImmediate)
                .map(|url| Url { url }))
        });

        methods.add_method("get_metadata", |lua, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            let value = pane.get_metadata();
            dynamic_to_lua_value(lua, value)
        });

        methods.add_method("get_foreground_process_name", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.get_foreground_process_name(CachePolicy::FetchImmediate))
        });

        methods.add_method("get_foreground_process_info", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.get_foreground_process_info(CachePolicy::AllowStale))
        });

        methods.add_method("get_cursor_position", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.get_cursor_position())
        });

        methods.add_method("get_dimensions", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.get_dimensions())
        });

        methods.add_method("get_user_vars", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.copy_user_vars())
        });

        methods.add_method("has_unseen_output", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.has_unseen_output())
        });

        methods.add_method("is_alt_screen_active", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.is_alt_screen_active())
        });

        // When called with no arguments, returns the lines from the
        // viewport as plain text (no escape sequences).
        // When called with an optional integer argument, returns the
        // last nlines lines of the terminal output.
        // The returned string will have trailing whitespace trimmed.
        methods.add_method("get_lines_as_text", |_, this, nlines: Option<usize>| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            let dims = pane.get_dimensions();
            let nlines = nlines.unwrap_or(dims.viewport_rows);
            let range = visible_row_range_for_tail(dims.physical_top, dims.viewport_rows, nlines)?;
            let (_first_row, lines) = pane.get_lines(range);
            let mut text = String::new();
            for line in lines {
                for cell in line.visible_cells() {
                    text.push_str(cell.str());
                }
                let trimmed = text.trim_end().len();
                text.truncate(trimmed);
                text.push('\n');
            }
            let trimmed = text.trim_end().len();
            text.truncate(trimmed);
            Ok(text)
        });

        methods.add_method("get_lines_as_escapes", |_, this, nlines: Option<usize>| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            let dims = pane.get_dimensions();
            let nlines = nlines.unwrap_or(dims.viewport_rows);
            let range = visible_row_range_for_tail(dims.physical_top, dims.viewport_rows, nlines)?;
            let (_first_row, lines) = pane.get_lines(range);
            let text = lines_to_escapes(lines).map_err(mlua::Error::external)?;
            Ok(text)
        });

        methods.add_method(
            "get_logical_lines_as_text",
            |_, this, nlines: Option<usize>| {
                let mux = get_mux()?;
                let pane = this.resolve(&mux)?;
                let dims = pane.get_dimensions();
                let nlines = nlines.unwrap_or(dims.viewport_rows);
                let range =
                    visible_row_range_for_tail(dims.physical_top, dims.viewport_rows, nlines)?;
                let lines = pane.get_logical_lines(range);
                let mut text = String::new();
                for line in lines {
                    for cell in line.logical.visible_cells() {
                        text.push_str(cell.str());
                    }
                    let trimmed = text.trim_end().len();
                    text.truncate(trimmed);
                    text.push('\n');
                }
                let trimmed = text.trim_end().len();
                text.truncate(trimmed);
                Ok(text)
            },
        );

        methods.add_method("get_domain_name", |_, this, _: ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            let mut name = None;
            if let Some(mux) = Mux::try_get() {
                let domain_id = pane.domain_id();
                name = mux
                    .get_domain(domain_id)
                    .map(|dom| dom.domain_name().to_string());
            }
            match name {
                Some(name) => Ok(name),
                None => Ok("".to_string()),
            }
        });

        methods.add_method("inject_output", |_, this, text: String| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;

            let mut parser = termwiz::escape::parser::Parser::new();
            let mut actions = vec![];
            parser.parse(text.as_bytes(), |action| actions.push(action));

            pane.perform_actions(actions);
            Ok(())
        });

        methods.add_method("get_semantic_zones", |lua, this, of_type: Value| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;

            let of_type: Option<SemanticType> = from_lua(of_type)?;

            let mut zones = pane
                .get_semantic_zones()
                .map_err(|e| mlua::Error::external(format!("{:#}", e)))?;

            if let Some(of_type) = of_type {
                zones.retain(|zone| zone.semantic_type == of_type);
            }

            let zones = to_lua(lua, zones)?;
            Ok(zones)
        });

        methods.add_method(
            "get_semantic_zone_at",
            |lua, this, (x, y): (usize, StableRowIndex)| {
                let mux = get_mux()?;
                let pane = this.resolve(&mux)?;

                let zones = pane.get_semantic_zones().unwrap_or_else(|_| vec![]);

                fn find_zone(x: usize, y: StableRowIndex, zone: &SemanticZone) -> Ordering {
                    match zone.start_y.cmp(&y) {
                        Ordering::Greater => return Ordering::Greater,
                        // If the zone starts on the same line then check that the
                        // x position is within bounds
                        Ordering::Equal => match zone.start_x.cmp(&x) {
                            Ordering::Greater => return Ordering::Greater,
                            Ordering::Equal | Ordering::Less => {}
                        },
                        Ordering::Less => {}
                    }
                    match zone.end_y.cmp(&y) {
                        Ordering::Less => Ordering::Less,
                        // If the zone ends on the same line then check that the
                        // x position is within bounds
                        Ordering::Equal => match zone.end_x.cmp(&x) {
                            Ordering::Less => Ordering::Less,
                            Ordering::Equal | Ordering::Greater => Ordering::Equal,
                        },
                        Ordering::Greater => Ordering::Equal,
                    }
                }

                match zones.binary_search_by(|zone| find_zone(x, y, zone)) {
                    Ok(idx) => {
                        let zone = to_lua(lua, zones[idx])?;
                        Ok(Some(zone))
                    }
                    Err(_) => Ok(None),
                }
            },
        );

        methods.add_method("get_text_from_semantic_zone", |_lua, this, zone: Value| {
            let zone: SemanticZone = from_lua(zone)?;
            this.get_text_from_semantic_zone(zone)
        });

        methods.add_method("get_text_from_region", |_lua, this, (start_x, start_y, end_x, end_y): (usize, StableRowIndex, usize, StableRowIndex)| {
            let zone = SemanticZone {
                start_x,
                start_y,
                end_x,
                end_y,
                // semantic_type is not used by get_text_from_semantic_zone
                semantic_type: SemanticType::Output,
            };
            this.get_text_from_semantic_zone(zone)
        });

        methods.add_method("move_to_new_tab", |_lua, this, ()| {
            let mux = get_mux()?;
            let (_domain, window_id, _tab) = mux
                .resolve_pane_id(this.0)
                .ok_or_else(|| mlua::Error::external(format!("pane {} not found", this.0)))?;
            let (tab, window) =
                promise::spawn::block_on(mux.move_pane_to_new_tab(this.0, Some(window_id), None))
                    .map_err(|e| mlua::Error::external(format!("{:#?}", e)))?;

            Ok((MuxTab(tab.tab_id()), MuxWindow(window)))
        });

        methods.add_method(
            "move_to_new_window",
            |_lua, this, workspace: Option<String>| {
                let mux = get_mux()?;
                let (tab, window) =
                    promise::spawn::block_on(mux.move_pane_to_new_tab(this.0, None, workspace))
                        .map_err(|e| mlua::Error::external(format!("{:#?}", e)))?;

                Ok((MuxTab(tab.tab_id()), MuxWindow(window)))
            },
        );

        methods.add_method("activate", move |_lua, this, ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            let (_domain_id, window_id, tab_id) = mux
                .resolve_pane_id(this.0)
                .ok_or_else(|| mlua::Error::external(format!("pane {} not found", this.0)))?;
            {
                let mut window = mux.get_window_mut(window_id).ok_or_else(|| {
                    mlua::Error::external(format!("window {window_id} not found"))
                })?;
                let tab_idx = window.idx_by_id(tab_id).ok_or_else(|| {
                    mlua::Error::external(format!(
                        "tab {tab_id} isn't really in window {window_id}!?"
                    ))
                })?;
                window.save_and_then_set_active(tab_idx);
            }
            let tab = mux
                .get_tab(tab_id)
                .ok_or_else(|| mlua::Error::external(format!("tab {tab_id} not found")))?;
            tab.set_active_pane(&pane);
            Ok(())
        });

        methods.add_method("get_tty_name", move |_lua, this, ()| {
            let mux = get_mux()?;
            let pane = this.resolve(&mux)?;
            Ok(pane.tty_name())
        });
    }
}

#[derive(Debug, Default, FromDynamic, ToDynamic)]
struct SplitPane {
    #[dynamic(flatten)]
    cmd_builder: CommandBuilderFrag,
    #[dynamic(default = "spawn_tab_default_domain")]
    domain: SpawnTabDomain,
    #[dynamic(default)]
    direction: HandySplitDirection,
    #[dynamic(default)]
    top_level: bool,
    #[dynamic(default = "default_split_size")]
    size: f32,
}
impl_lua_conversion_dynamic!(SplitPane);

fn default_split_size() -> f32 {
    0.5
}

impl SplitPane {
    async fn run(&self, pane: &MuxPane) -> mlua::Result<MuxPane> {
        let (command, command_dir) = self.cmd_builder.to_command_builder();
        let source = SplitSource::Spawn {
            command,
            command_dir,
        };

        let size = if self.size == 0.0 {
            SplitSize::Percent(50)
        } else if self.size < 1.0 {
            SplitSize::Percent((self.size * 100.).floor() as u8)
        } else {
            SplitSize::Cells(self.size as usize)
        };

        let direction = match self.direction {
            HandySplitDirection::Right | HandySplitDirection::Left => SplitDirection::Horizontal,
            HandySplitDirection::Top | HandySplitDirection::Bottom => SplitDirection::Vertical,
        };

        let request = SplitRequest {
            direction,
            target_is_second: match self.direction {
                HandySplitDirection::Top | HandySplitDirection::Left => false,
                HandySplitDirection::Bottom | HandySplitDirection::Right => true,
            },
            top_level: self.top_level,
            size,
        };

        let mux = get_mux()?;
        let (pane, _size) = mux
            .split_pane(pane.0, request, source, self.domain.clone())
            .await
            .map_err(|e| mlua::Error::external(format!("{:#?}", e)))?;

        Ok(MuxPane(pane.pane_id()))
    }
}
