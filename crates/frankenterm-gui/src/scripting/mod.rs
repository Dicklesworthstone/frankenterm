use crate::frontend::try_front_end;
use crate::inputmap::InputMap;
use config::keyassignment::KeyTable;
use config::lua::get_or_create_sub_module;
use config::lua::mlua::{self, Lua};
use config::{DeferredKeyCode, GpuInfo, Key, KeyNoAction};
use luahelper::dynamic_to_lua_value;
use mux::window::WindowId as MuxWindowId;
use std::collections::HashMap;
use wezterm_dynamic::ToDynamic;

pub mod guiwin;

fn luaerr(err: anyhow::Error) -> mlua::Error {
    mlua::Error::external(err)
}

#[allow(dead_code)]
pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let window_mod = get_or_create_sub_module(lua, "gui")?;

    window_mod.set(
        "gui_window_for_mux_window",
        // Must be an async Lua function: `reconcile_workspace()` spawns
        // loop-driven `TermWindow::new_window().await` work, so the returned
        // future only resolves once the GUI event loop runs. Driving it with
        // `promise::spawn::block_on` deadlocks (and trips the main-thread
        // dispatch guard) when this is called from a `gui-startup` handler
        // running on the main-thread spawn queue. Awaiting yields to the loop
        // instead. (Regressed to block_on during the mlua 0.11 migration;
        // restored to the pre-migration async form.)
        lua.create_async_function(|_, mux_window_id: MuxWindowId| async move {
            // The front-end handle is `Rc<GuiFrontEnd>` (!Send), and mlua 0.11's
            // `create_async_function` requires the future to be `Send`. So we must
            // not hold the handle across the `.await`: acquire it, obtain the owned
            // reconcile `Future<()>` (which spawns the loop-driven window creation),
            // drop the handle, then await. Re-acquire it afterward for the
            // synchronous lookup. Only the (Send) reconcile future + window id are
            // live across the await point.
            let reconcile = {
                let fe = try_front_end()
                    .ok_or_else(|| mlua::Error::external("not called on gui thread"))?;
                fe.reconcile_workspace()
            };
            let _ = reconcile.await;
            let fe =
                try_front_end().ok_or_else(|| mlua::Error::external("not called on gui thread"))?;
            let win = fe.gui_window_for_mux_window(mux_window_id).ok_or_else(|| {
                mlua::Error::external(format!(
                    "mux window id {mux_window_id} is not currently associated with a gui window"
                ))
            })?;
            Ok(win)
        })?,
    )?;

    fn key_table_to_lua(table: &KeyTable) -> Vec<Key> {
        let mut keys = vec![];
        for ((key, mods), entry) in table {
            keys.push(Key {
                key: KeyNoAction {
                    key: DeferredKeyCode::KeyCode(key.clone()),
                    mods: *mods,
                },
                action: entry.action.clone(),
            });
        }
        keys
    }

    window_mod.set(
        "gui_windows",
        lua.create_function(|_, _: ()| {
            let fe =
                try_front_end().ok_or_else(|| mlua::Error::external("not called on gui thread"))?;
            Ok(fe.gui_windows())
        })?,
    )?;

    window_mod.set(
        "default_keys",
        lua.create_function(|lua, _: ()| {
            let map = InputMap::default_input_map();
            let keys = key_table_to_lua(&map.keys.default);
            dynamic_to_lua_value(lua, keys.to_dynamic())
        })?,
    )?;

    window_mod.set(
        "default_key_tables",
        lua.create_function(|lua, _: ()| {
            let inputmap = InputMap::default_input_map();
            let mut tables: HashMap<String, Vec<Key>> = HashMap::new();
            for (k, table) in &inputmap.keys.by_name {
                let keys = key_table_to_lua(table);
                tables.insert(k.to_string(), keys);
            }
            dynamic_to_lua_value(lua, tables.to_dynamic())
        })?,
    )?;

    window_mod.set(
        "enumerate_gpus",
        lua.create_function(|_, _: ()| {
            let backends = wgpu::Backends::all();
            let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
            descriptor.backends = backends;
            let instance = wgpu::Instance::new(descriptor);
            // Self-contained wgpu adapter enumeration (no GUI-loop dependency),
            // so drive it with a private executor rather than
            // `promise::spawn::block_on`, which would trip the main-thread
            // dispatch guard if a config evaluates `enumerate_gpus()` from a
            // gui-startup/event handler running on the main-thread spawn queue.
            let gpus: Vec<GpuInfo> =
                futures::executor::block_on(instance.enumerate_adapters(backends))
                    .into_iter()
                    .map(|adapter| {
                        let info = adapter.get_info();
                        crate::termwindow::webgpu::adapter_info_to_gpu_info(info)
                    })
                    .collect();
            Ok(gpus)
        })?,
    )?;

    Ok(())
}
