//! `wezterm.run_child_process`, `wezterm.background_child_process` and
//! `wezterm.open_with` (WezTerm's spawn-funcs, ported to mlua 0.12).
//!
//! Upstream ran children on smol's process API; this project forbids smol, so
//! the child runs on a worker thread from `promise::spawn::spawn_into_new_thread`,
//! whose future any executor can await.
use bstr::BString;
use config::lua::get_or_create_module;
use config::lua::mlua::{self, Lua};
use std::process::{Command, Stdio};

pub fn register(lua: &Lua) -> anyhow::Result<()> {
    let wezterm_mod = get_or_create_module(lua, "wezterm")?;
    wezterm_mod.set("open_with", lua.create_function(open_with)?)?;
    wezterm_mod.set(
        "run_child_process",
        lua.create_async_function(run_child_process)?,
    )?;
    wezterm_mod.set(
        "background_child_process",
        lua.create_async_function(background_child_process)?,
    )?;
    Ok(())
}

fn open_with(_: &Lua, (url, app): (String, Option<String>)) -> mlua::Result<()> {
    match app {
        Some(app) => frankenterm_open_url::open_with(&url, &app),
        None => frankenterm_open_url::open_url(&url),
    }
    Ok(())
}

/// The program and its arguments; an empty list is an error rather than the
/// out-of-bounds panic it was upstream.
fn command(args: &[String]) -> mlua::Result<Command> {
    let (program, rest) = args.split_first().ok_or_else(|| {
        mlua::Error::external("expected the program to run and its arguments, got an empty list")
    })?;
    let mut cmd = Command::new(program);
    cmd.args(rest);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console window flashes up for the child.
        cmd.creation_flags(0x0800_0000);
    }
    Ok(cmd)
}

async fn run_child_process(_: Lua, args: Vec<String>) -> mlua::Result<(bool, BString, BString)> {
    let mut cmd = command(&args)?;
    let output = promise::spawn::spawn_into_new_thread(move || Ok(cmd.output()?))
        .await
        .map_err(mlua::Error::external)?;
    Ok((
        output.status.success(),
        output.stdout.into(),
        output.stderr.into(),
    ))
}

async fn background_child_process(_: Lua, args: Vec<String>) -> mlua::Result<()> {
    let mut child = command(&args)?
        .stdin(Stdio::null())
        .spawn()
        .map_err(mlua::Error::external)?;
    // Reap it when it exits, so it does not linger as a zombie.
    std::thread::Builder::new()
        .name("lua-background-child".to_string())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(mlua::Error::external)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lua() -> Lua {
        let lua = Lua::new();
        register(&lua).expect("register spawn-funcs");
        lua
    }

    #[test]
    fn run_child_process_returns_status_and_output() {
        let lua = lua();
        let (ok, stdout, stderr): (bool, String, String) = promise::spawn::block_on(
            lua.load(
                "return require('wezterm').run_child_process { '/bin/sh', '-c', 'printf out; printf err >&2' }",
            )
            .eval_async(),
        )
        .expect("run_child_process");
        assert!(ok);
        assert_eq!(stdout, "out");
        assert_eq!(stderr, "err");

        let (ok, _, _): (bool, String, String) = promise::spawn::block_on(
            lua.load("return require('wezterm').run_child_process { '/bin/sh', '-c', 'exit 3' }")
                .eval_async(),
        )
        .expect("run_child_process");
        assert!(!ok);
    }

    #[test]
    fn an_empty_command_is_an_error_not_a_panic() {
        let lua = lua();
        let err = promise::spawn::block_on(
            lua.load("return require('wezterm').run_child_process {}")
                .exec_async(),
        )
        .expect_err("empty command");
        assert!(err.to_string().contains("empty list"), "{}", err);
    }

    #[test]
    fn background_child_process_starts_the_child() {
        let dir = std::env::temp_dir().join(format!("ft-lua-bg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("test dir");
        let marker = dir.join("ran");
        let lua = lua();
        lua.globals()
            .set("MARKER", marker.to_str().expect("utf8"))
            .expect("MARKER");
        promise::spawn::block_on(
            lua.load("require('wezterm').background_child_process { '/usr/bin/touch', MARKER }")
                .exec_async(),
        )
        .expect("background_child_process");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(marker.exists(), "the background child ran");
    }
}
