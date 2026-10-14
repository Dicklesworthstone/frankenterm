//! `wezterm.enumerate_ssh_hosts` and `wezterm.default_ssh_domains`
//! (WezTerm's ssh-funcs, ported to mlua 0.12).
use config::lua::get_or_create_module;
use config::lua::mlua::{self, Lua, Variadic};
use std::collections::HashMap;

pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let wezterm_mod = get_or_create_module(lua, "wezterm")?;
    wezterm_mod.set(
        "enumerate_ssh_hosts",
        lua.create_function(enumerate_ssh_hosts)?,
    )?;
    wezterm_mod.set(
        "default_ssh_domains",
        lua.create_function(|_, ()| Ok(config::SshDomain::default_domains()))?,
    )?;
    Ok(())
}

fn enumerate_ssh_hosts(
    lua: &Lua,
    config_files: Variadic<String>,
) -> mlua::Result<HashMap<String, wezterm_ssh::ConfigMap>> {
    let mut config = wezterm_ssh::Config::new();
    for file in config_files {
        config.add_config_file(file);
    }
    config.add_default_config_files();

    // Reload the configuration when any ssh config file it read changes.
    let files: Variadic<String> = config
        .loaded_config_files()
        .into_iter()
        .filter_map(|p| p.to_str().map(|s| s.to_string()))
        .collect();
    config::lua::add_to_config_reload_watch_list(lua, files)?;

    Ok(config
        .enumerate_hosts()
        .into_iter()
        .map(|host| {
            let host_config = config.for_host(&host);
            (host, host_config)
        })
        .collect())
}
