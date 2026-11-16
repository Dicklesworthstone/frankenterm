//! `wezterm.procinfo` (WezTerm's procinfo-funcs, ported to mlua 0.12).
use config::lua::get_or_create_sub_module;
use config::lua::mlua::Lua;
use procinfo::LocalProcessInfo;

pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let proc_mod = get_or_create_sub_module(lua, "procinfo")?;
    proc_mod.set("pid", lua.create_function(|_, ()| Ok(std::process::id()))?)?;
    proc_mod.set(
        "get_info_for_pid",
        lua.create_function(|_, pid: u32| Ok(LocalProcessInfo::with_root_pid(pid)))?,
    )?;
    proc_mod.set(
        "current_working_dir_for_pid",
        lua.create_function(|_, pid: u32| {
            Ok(LocalProcessInfo::current_working_dir(pid)
                .and_then(|p| p.to_str().map(|s| s.to_string())))
        })?,
    )?;
    proc_mod.set(
        "executable_path_for_pid",
        lua.create_function(|_, pid: u32| {
            Ok(LocalProcessInfo::executable_path(pid)
                .and_then(|p| p.to_str().map(|s| s.to_string())))
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_this_process() {
        let lua = Lua::new();
        register(&lua).expect("register procinfo");
        let pid: u32 = lua
            .load("return require('wezterm').procinfo.pid()")
            .eval()
            .expect("pid");
        assert_eq!(pid, std::process::id());
        let exe: Option<String> = lua
            .load(
                "local p = require('wezterm').procinfo; return p.executable_path_for_pid(p.pid())",
            )
            .eval()
            .expect("executable_path_for_pid");
        #[cfg(any(target_os = "macos", target_os = "linux", windows))]
        assert!(exe.is_some_and(|exe| !exe.is_empty()));
        #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
        let _ = exe;
    }
}
