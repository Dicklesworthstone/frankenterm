//! `wezterm.read_dir` and `wezterm.glob` (WezTerm's filesystem crate,
//! ported to mlua 0.12). Upstream used smol, which this project forbids; the
//! directory walk runs on a worker thread from
//! `promise::spawn::spawn_into_new_thread` instead.
use anyhow::anyhow;
use config::lua::get_or_create_module;
use config::lua::mlua::{self, Lua};
use std::path::Path;

pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let wezterm_mod = get_or_create_module(lua, "wezterm")?;
    wezterm_mod.set("read_dir", lua.create_async_function(read_dir)?)?;
    wezterm_mod.set("glob", lua.create_async_function(glob)?)?;
    Ok(())
}

fn utf8(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("path entry {} is not representable as utf8", path.display()))
}

async fn read_dir(_: Lua, path: String) -> mlua::Result<Vec<String>> {
    promise::spawn::spawn_into_new_thread(move || {
        std::fs::read_dir(path)?
            .map(|entry| utf8(&entry?.path()))
            .collect()
    })
    .await
    .map_err(mlua::Error::external)
}

async fn glob(_: Lua, (pattern, path): (String, Option<String>)) -> mlua::Result<Vec<String>> {
    promise::spawn::spawn_into_new_thread(move || {
        let glob = filenamegen::Glob::new(&pattern)?;
        glob.walk(path.as_deref().unwrap_or("."))
            .iter()
            .map(|path| utf8(path))
            .collect()
    })
    .await
    .map_err(mlua::Error::external)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_dir_and_glob_list_entries() {
        let dir = std::env::temp_dir().join(format!("ft-lua-fs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");
        std::fs::write(dir.join("a.txt"), "a").expect("a.txt");
        std::fs::write(dir.join("b.log"), "b").expect("b.log");
        let lua = Lua::new();
        register(&lua).expect("register filesystem");
        lua.globals()
            .set("DIR", dir.to_str().expect("utf8 dir"))
            .expect("DIR");

        let mut listed: Vec<String> = promise::spawn::block_on(
            lua.load("return require('wezterm').read_dir(DIR)")
                .eval_async(),
        )
        .expect("read_dir");
        listed.sort();
        assert_eq!(
            listed,
            [dir.join("a.txt"), dir.join("b.log")]
                .iter()
                .map(|p| p.to_str().expect("utf8").to_string())
                .collect::<Vec<_>>()
        );

        let globbed: Vec<String> = promise::spawn::block_on(
            lua.load("return require('wezterm').glob('*.txt', DIR)")
                .eval_async(),
        )
        .expect("glob");
        assert_eq!(globbed, ["a.txt"]);
    }
}
