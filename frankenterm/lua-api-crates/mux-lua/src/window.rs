use super::*;
use parking_lot::MappedRwLockReadGuard;

#[derive(Clone, Copy, Debug)]
pub struct MuxWindow(pub WindowId);

impl MuxWindow {
    pub fn resolve<'a>(
        &self,
        mux: &'a Arc<Mux>,
    ) -> mlua::Result<MappedRwLockReadGuard<'a, Window>> {
        mux.get_window(self.0)
            .ok_or_else(|| mlua::Error::external(format!("window id {} not found in mux", self.0)))
    }

}

impl UserData for MuxWindow {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, _: ()| {
            Ok(format!(
                "MuxWindow(mux_window_id:{}, pid:{})",
                this.0,
                unsafe { libc::getpid() }
            ))
        });
        methods.add_method("window_id", |_, this, _: ()| Ok(this.0));
        methods.add_async_method("gui_window", |lua, this, _: ()| async move {
            // Weakly bound to the gui module; mux cannot hard-depend
            // on wezterm-gui, but we can runtime resolve the appropriate module
            let wezterm_mod = get_or_create_module(&lua, "wezterm")
                .map_err(|err| mlua::Error::external(format!("{err:#}")))?;
            let gui: mlua::Table = wezterm_mod.get("gui")?;
            let func: mlua::Function = gui.get("gui_window_for_mux_window")?;
            func.call_async::<mlua::Value>(this.0).await
        });
        methods.add_method("get_workspace", |_, this, _: ()| {
            let mux = get_mux()?;
            let window = this.resolve(&mux)?;
            Ok(window.get_workspace().to_string())
        });
        methods.add_method("set_workspace", |_, this, new_name: String| {
            let mux = get_mux()?;
            mux.set_window_workspace(this.0, &new_name)
                .map_err(mlua::Error::external)?;
            Ok(())
        });
        // Must be an async Lua method: `spawn.spawn()` awaits
        // `Mux::spawn_tab_or_window`, whose future is only driven once the GUI
        // event loop runs. Driving it with `promise::spawn::block_on` deadlocks
        // (and trips the main-thread dispatch guard) when `spawn_tab` is invoked
        // from a `gui-startup`/event handler running on the main-thread spawn
        // queue. (Regressed to block_on during the mlua 0.11 migration.)
        //
        // mlua 0.11's `add_async_method` (send feature on) requires a `Send`
        // future, but `Mux::spawn_tab_or_window`'s future is `!Send` (the
        // `Domain` trait yields `Pin<Box<dyn Future>>` local futures), so we
        // cannot `.await` it directly. Instead spawn the `!Send` work on the
        // main-thread-local executor — `promise::spawn::spawn` lifts the `Send`
        // bound and schedules onto the same main-thread queue the loop pumps —
        // and `.await` its `Task` join handle, which IS `Send` (the result is
        // Copy IDs). This future therefore holds only the `Send` join handle
        // across the await; the actual spawn work still yields to the GUI loop,
        // so there is no `block_on` and no main-thread dispatch deadlock.
        methods.add_async_method("spawn_tab", |_, this, spawn: SpawnTab| async move {
            // MuxWindow is Copy: take the id by value so the deferred task does
            // not retain the Lua `UserDataRef` registry handle.
            let window = *this;
            promise::spawn::spawn(async move { spawn.spawn(&window).await }).await
        });
        methods.add_method("get_title", |_, this, _: ()| {
            let mux = get_mux()?;
            let window = this.resolve(&mux)?;
            Ok(window.get_title().to_string())
        });
        methods.add_method("set_title", |_, this, title: String| {
            let mux = get_mux()?;
            match mux.set_window_title(this.0, &title) {
                Ok(_) => {}
                Err(error) if error.is_not_found() => {
                    return Err(mlua::Error::external(format!(
                        "window id {} not found in mux",
                        this.0
                    )));
                }
                Err(error) => return Err(mlua::Error::external(error)),
            }
            Ok(())
        });
        methods.add_method("tabs", |_, this, _: ()| {
            let mux = get_mux()?;
            let window = this.resolve(&mux)?;
            Ok(window
                .iter()
                .map(|tab| MuxTab(tab.tab_id()))
                .collect::<Vec<MuxTab>>())
        });
        methods.add_method("tabs_with_info", |lua, this, _: ()| {
            let mux = get_mux()?;
            let window = this.resolve(&mux)?;
            let result = lua.create_table()?;
            let active_idx = window.get_active_idx();
            for (index, tab) in window.iter().enumerate() {
                let info = MuxTabInfo {
                    index,
                    is_active: index == active_idx,
                };
                let info = luahelper::dynamic_to_lua_value(lua, info.to_dynamic())?;
                if let LuaValue::Table(t) = &info {
                    t.set("tab", MuxTab(tab.tab_id()))?;
                }
                result.set(index + 1, info)?;
            }
            Ok(result)
        });
        methods.add_method("active_tab", |_, this, _: ()| {
            let mux = get_mux()?;
            let window = this.resolve(&mux)?;
            Ok(window.get_active().map(|tab| MuxTab(tab.tab_id())))
        });
        methods.add_method("active_pane", |_, this, _: ()| {
            let mux = get_mux()?;
            let window = this.resolve(&mux)?;
            Ok(window
                .get_active()
                .and_then(|tab| tab.get_active_pane().map(|pane| MuxPane(pane.pane_id()))))
        });
    }
}
