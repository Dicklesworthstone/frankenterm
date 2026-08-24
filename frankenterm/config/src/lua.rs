use crate::exec_domain::{ExecDomain, ValueOrFunc};
use crate::keyassignment::KeyAssignment;
use crate::{
    Config, FontAttributes, FontStretch, FontStyle, FontWeight, FreeTypeLoadTarget, RgbaColor,
    TextStyle,
};
use anyhow::{anyhow, Context};
use frankenterm_dynamic::{
    FromDynamic, FromDynamicOptions, ToDynamic, UnknownFieldAction, Value as DynValue,
};
use luahelper::{from_lua_value_dynamic, lua_value_to_dynamic, to_lua};
use mlua::{FromLua, IntoLuaMulti, Lua, Table, Value, Variadic};
use ordered_float::NotNan;
use portable_pty::CommandBuilder;
use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub use mlua;

static LUA_REGISTRY_USER_CALLBACK_COUNT: &str = "wezterm-user-callback-count";

pub type SetupFunc = fn(&Lua) -> anyhow::Result<()>;

lazy_static::lazy_static! {
    static ref SETUP_FUNCS: Mutex<Vec<SetupFunc>> = Mutex::new(vec![]);
}

fn setup_funcs_lock() -> MutexGuard<'static, Vec<SetupFunc>> {
    match SETUP_FUNCS.lock() {
        Ok(funcs) => funcs,
        Err(poisoned) => {
            log::warn!("recovering poisoned Lua SETUP_FUNCS mutex");
            SETUP_FUNCS.clear_poison();
            poisoned.into_inner()
        }
    }
}

fn setup_funcs_snapshot() -> Vec<SetupFunc> {
    setup_funcs_lock().clone()
}

pub fn add_context_setup_func(func: SetupFunc) {
    setup_funcs_lock().push(func);
}

pub fn get_or_create_module(lua: &Lua, name: &str) -> anyhow::Result<mlua::Table> {
    let globals = lua.globals();
    let package: Table = globals.get("package")?;
    let loaded: Table = package.get("loaded")?;

    let module = loaded.get(name)?;
    match module {
        Value::Nil => {
            let module = lua.create_table()?;
            loaded.set(name, module.clone())?;
            Ok(module)
        }
        Value::Table(table) => Ok(table),
        wat => anyhow::bail!(
            "cannot register module {} as package.loaded.{} is already set to a value of type {}",
            name,
            name,
            wat.type_name()
        ),
    }
}

pub fn get_or_create_sub_module(lua: &Lua, name: &str) -> anyhow::Result<mlua::Table> {
    let wezterm_mod = get_or_create_module(lua, "wezterm")?;
    let sub = wezterm_mod.get(name)?;
    match sub {
        Value::Nil => {
            let sub = lua.create_table()?;
            wezterm_mod.set(name, sub.clone())?;
            Ok(sub)
        }
        Value::Table(sub) => Ok(sub),
        wat => anyhow::bail!(
            "cannot register module wezterm.{name} as it is already set to a value of type {}",
            wat.type_name()
        ),
    }
}

fn config_builder_set_strict_mode(_lua: &Lua, (myself, strict): (Table, bool)) -> mlua::Result<()> {
    let mt = myself
        .metatable()
        .ok_or_else(|| mlua::Error::external("impossible that we have no metatable"))?;
    mt.set("__strict_mode", strict)
}

fn config_builder_index(
    _lua: &Lua,
    (myself, key): (Table, mlua::Value),
) -> mlua::Result<mlua::Value> {
    let mt = myself
        .metatable()
        .ok_or_else(|| mlua::Error::external("impossible that we have no metatable"))?;
    match mt.get(key.clone()) {
        Ok(value) => Ok(value),
        _ => myself.raw_get(key),
    }
}

fn config_builder_new_index(
    lua: &Lua,
    (myself, key, value): (Table, String, Value),
) -> mlua::Result<()> {
    let stub_config = lua.create_table()?;
    stub_config.set(key.clone(), value.clone())?;

    let dvalue = lua_value_to_dynamic(Value::Table(stub_config)).map_err(|e| {
        mlua::Error::FromLuaConversionError {
            from: "table",
            to: "Config".to_string(),
            message: Some(format!("lua_value_to_dynamic: {e}")),
        }
    })?;

    let mt = myself
        .metatable()
        .ok_or_else(|| mlua::Error::external("impossible that we have no metatable"))?;
    let strict = match mt.get("__strict_mode") {
        Ok(Value::Boolean(b)) => b,
        _ => true,
    };

    let options = FromDynamicOptions {
        unknown_fields: if strict {
            UnknownFieldAction::Deny
        } else {
            UnknownFieldAction::Warn
        },
        deprecated_fields: UnknownFieldAction::Warn,
    };

    let config_object = Config::from_dynamic(&dvalue, options).map_err(|e| {
        mlua::Error::FromLuaConversionError {
            from: "table",
            to: "Config".to_string(),
            message: Some(format!("Config::from_dynamic: {e}")),
        }
    })?;

    match config_object.to_dynamic() {
        DynValue::Object(obj) => {
            match obj.get_by_str(&key) {
                None => {
                    // Show a stack trace to help them figure out where they made
                    // a mistake. This path is taken when they are not in strict
                    // mode, and we want to print some more context after the from_dynamic
                    // impl has logged a warning and suggested alternative field names.
                    let mut message =
                        format!("Attempted to set invalid config option `{key}` at:\n");
                    // Start at frame 1, our caller, as the frame for invoking this
                    // metamethod is not interesting
                    for i in 1.. {
                        if let Some((source, line, func_name)) = lua.inspect_stack(i, |debug| {
                            let names = debug.names();
                            let name = names.name;
                            let name_what = names.name_what;

                            let dbg_source = debug.source();
                            let source = dbg_source.source.unwrap_or_default().to_string();
                            let func_name = match (name, name_what) {
                                (Some(name), Some(name_what)) => {
                                    format!("{name_what} {name}")
                                }
                                (Some(name), None) => format!("{name}"),
                                _ => "".to_string(),
                            };

                            let line = debug.current_line().map_or(-1_i64, |line| line as i64);
                            (source, line, func_name)
                        }) {
                            message.push_str(&format!("    [{i}] {source}:{line} {func_name}\n"));
                        } else {
                            break;
                        }
                    }
                    frankenterm_dynamic::Error::warn(message);
                }
                Some(_dvalue) => {
                    myself.raw_set(key, value)?;
                }
            };
            Ok(())
        }
        _ => Err(mlua::Error::external(
            "computed config object is, impossibly, not an object",
        )),
    }
}

/// Set up a lua context for executing some code.
/// The path to the directory containing the configuration is
/// passed in and is used to pre-set some global values in
/// the environment.
///
/// The `package.path` is configured to search the user's
/// wezterm specific config paths for lua modules, should
/// they choose to `require` additional code from their config.
///
/// A `wezterm` module is registered so that the script can
/// `require "wezterm"` and call into functions provided by
/// wezterm.  The wezterm module contains:
/// * `executable_dir` - the directory containing the wezterm
///   executable.  This is potentially useful for portable
///   installs on Windows.
/// * `config_dir` - the directory containing the wezterm
///   configuration.
/// * `log_error` - a function that logs to stderr (or the server
///   log file for daemonized wezterm).
/// * `target_triple` - the rust compilation target triple.
/// * `version` - the version of the running wezterm instance.
/// * `home_dir` - the path to the user's home directory
///
/// In addition to this, the lua standard library, except for
/// the `debug` module, is also available to the script.
/// Build the list of `package.path` entries that front the Lua module
/// search path, in priority order (earliest entry wins for `require`).
///
/// `config_dirs` must already be ordered highest-priority first; the legacy
/// `~/.wezterm` directory is appended after all of them so that a
/// side-by-side WezTerm install can never shadow a FrankenTerm-namespaced
/// module of the same name (GH#76).
fn lua_package_path_prefix(config_dirs: &[PathBuf], home_dir: &Path) -> Vec<String> {
    let mut prefix = Vec::with_capacity((config_dirs.len() + 1) * 2);
    let mut push_dir = |dir: &Path| {
        prefix.push(format!("{}/?.lua", dir.display()));
        prefix.push(format!("{}/?/init.lua", dir.display()));
    };
    for dir in config_dirs {
        push_dir(dir);
    }
    push_dir(&home_dir.join(".wezterm"));
    prefix
}

pub fn make_lua_context(config_file: &Path) -> anyhow::Result<Lua> {
    let lua = Lua::new();

    let config_dir = config_file.parent().unwrap_or_else(|| Path::new("/"));

    {
        let globals = lua.globals();
        // This table will be the `wezterm` module in the script
        let wezterm_mod = get_or_create_module(&lua, "wezterm")?;

        let package: Table = globals.get("package").context("get _G.package")?;
        let package_path: String = package.get("path").context("get package.path as String")?;
        let mut path_array: Vec<String> = package_path.split(";").map(|s| s.to_owned()).collect();

        fn prefix_path(array: &mut Vec<String>, path: &Path) {
            array.insert(0, format!("{}/?.lua", path.display()));
            array.insert(1, format!("{}/?/init.lua", path.display()));
        }

        // Splice the config-dir search entries onto the front of
        // `package.path` in priority order. `CONFIG_DIRS` is ordered
        // highest-priority first (FrankenTerm-namespaced dirs, then the
        // wezterm-namespaced fallbacks — see `config_dirs()` in lib.rs), and
        // `require` resolves left-to-right, so the resulting `package.path`
        // must preserve that order. The legacy `~/.wezterm` directory is a
        // wezterm-namespaced fallback and ranks below every `CONFIG_DIRS`
        // entry.
        //
        // GH#76 regression note: this used to call `prefix_path` (which
        // inserts at index 0) once per dir while iterating `CONFIG_DIRS`
        // front-to-back, which *reversed* the precedence so that
        // `~/.config/wezterm` shadowed `~/.config/frankenterm` for a bare
        // `require("mod")` issued from frankenterm.lua.
        path_array.splice(
            0..0,
            lua_package_path_prefix(&crate::CONFIG_DIRS, &crate::HOME_DIR),
        );
        path_array.insert(
            2,
            format!("{}/plugins/?/plugin/init.lua", crate::DATA_DIR.display()),
        );

        if let Ok(exe) = std::env::current_exe() {
            if let Some(path) = exe.parent() {
                wezterm_mod
                    .set(
                        "executable_dir",
                        path.to_str()
                            .ok_or_else(|| anyhow!("current_exe path is not UTF-8"))?,
                    )
                    .context("set wezterm.executable_dir")?;
                if cfg!(windows) {
                    // For a portable windows install, force in this path ahead
                    // of the rest
                    prefix_path(&mut path_array, &path.join("wezterm_modules"));
                }
            }
        }
        let config_file_str = config_file
            .to_str()
            .ok_or_else(|| anyhow!("config file path is not UTF-8"))?;

        // Hook into loader and arrange to watch all require'd files.
        // <https://www.lua.org/manual/5.3/manual.html#pdf-package.searchers>
        // says that the second searcher function is the one that is responsible
        // for loading lua files, so we shim around that and speculatively
        // add the name of the file that it would find (as returned from
        // package.searchpath) to the watch list, then we just call the
        // original implementation.
        lua.load(
            r#"
local orig = package.searchers[2]
package.searchers[2] = function(module)
  local name, err = package.searchpath(module, package.path)
  if name then
    package.loaded.wezterm.add_to_config_reload_watch_list(name)
  end
  return orig(module)
end
        "#,
        )
        .set_name("=searcher")
        .eval::<()>()
        .context("replace package.searchers")?;

        wezterm_mod.set(
            "config_builder",
            lua.create_function(|lua, _: ()| {
                let config = lua.create_table()?;
                let mt = lua.create_table()?;

                mt.set("__index", lua.create_function(config_builder_index)?)?;
                mt.set("__newindex", lua.create_function(config_builder_new_index)?)?;
                mt.set(
                    "set_strict_mode",
                    lua.create_function(config_builder_set_strict_mode)?,
                )?;

                config.set_metatable(Some(mt))?;

                Ok(config)
            })?,
        )?;

        wezterm_mod.set(
            "reload_configuration",
            lua.create_function(|_, _: ()| {
                crate::reload();
                Ok(())
            })?,
        )?;
        wezterm_mod
            .set("config_file", config_file_str)
            .context("set wezterm.config_file")?;
        wezterm_mod
            .set(
                "config_dir",
                config_dir
                    .to_str()
                    .ok_or_else(|| anyhow!("config dir path is not UTF-8"))?,
            )
            .context("set wezterm.config_dir")?;

        lua.set_named_registry_value("wezterm-watch-paths", Vec::<String>::new())?;
        wezterm_mod.set(
            "add_to_config_reload_watch_list",
            lua.create_function(add_to_config_reload_watch_list)?,
        )?;

        wezterm_mod.set("target_triple", crate::wezterm_target_triple())?;
        wezterm_mod.set("version", crate::wezterm_version())?;
        wezterm_mod.set("home_dir", crate::HOME_DIR.to_str())?;
        wezterm_mod.set(
            "running_under_wsl",
            lua.create_function(|_, ()| Ok(crate::running_under_wsl()))?,
        )?;

        wezterm_mod.set(
            "default_wsl_domains",
            lua.create_function(|_, ()| Ok(crate::WslDomain::default_domains()))?,
        )?;

        wezterm_mod.set("font", lua.create_function(font)?)?;
        wezterm_mod.set(
            "font_with_fallback",
            lua.create_function(font_with_fallback)?,
        )?;
        wezterm_mod.set("hostname", lua.create_function(hostname)?)?;
        wezterm_mod.set("action", luahelper::enumctor::Enum::<KeyAssignment>::new())?;
        wezterm_mod.set(
            "has_action",
            lua.create_function(|_lua, name: String| {
                Ok(KeyAssignment::variants().contains(&name.as_str()))
            })?,
        )?;

        lua.set_named_registry_value(LUA_REGISTRY_USER_CALLBACK_COUNT, 0)?;
        wezterm_mod.set("action_callback", lua.create_function(action_callback)?)?;
        wezterm_mod.set("exec_domain", lua.create_function(exec_domain)?)?;

        wezterm_mod.set("utf16_to_utf8", lua.create_function(utf16_to_utf8)?)?;
        wezterm_mod.set("split_by_newlines", lua.create_function(split_by_newlines)?)?;
        wezterm_mod.set("on", lua.create_function(register_event)?)?;
        wezterm_mod.set("emit", lua.create_async_function(emit_event)?)?;
        wezterm_mod.set("shell_join_args", lua.create_function(shell_join_args)?)?;
        wezterm_mod.set("shell_quote_arg", lua.create_function(shell_quote_arg)?)?;
        wezterm_mod.set("shell_split", lua.create_function(shell_split)?)?;

        // FrankenTerm-branded log helpers. Route to the Rust `log` crate so
        // diagnostics from Lua land in the same RUST_LOG
        // stream as the rest of the binary. Five levels mirror the upstream
        // wezterm surface; the wezterm.* names below are kept as aliases so
        // reference WezTerm configs paste in unmodified.
        wezterm_mod.set("log_error", lua.create_function(lua_log_error)?)?;
        wezterm_mod.set("log_warn", lua.create_function(lua_log_warn)?)?;
        wezterm_mod.set("log_info", lua.create_function(lua_log_info)?)?;
        wezterm_mod.set("log_debug", lua.create_function(lua_log_debug)?)?;
        wezterm_mod.set("log_trace", lua.create_function(lua_log_trace)?)?;

        // FrankenTerm-branded time helpers. `time.call_after(secs, fn)`
        // schedules a one-shot Lua callback via the asupersync timer
        // primitive (NOT tokio — the project bans direct tokio use; see
        // AGENTS.md "Async Runtime: asupersync"). Implemented in
        // `lua_time_call_after` below, which pins the callback to the Lua
        // generation that created it, owns the delay on the bounded background
        // executor, and retries transient main-thread admission failures
        // without dropping the callback. The wezterm.time alias below is the
        // back-compat surface for upstream configs.
        let time_mod = get_or_create_sub_module(&lua, "time")?;
        time_mod.set("call_after", lua.create_function(lua_time_call_after)?)?;
        time_mod.set("now", lua.create_function(lua_time_now)?)?;

        wezterm_mod.set(
            "default_hyperlink_rules",
            lua.create_function(move |lua, ()| {
                let rules = crate::config::default_hyperlink_rules();
                Ok(to_lua(lua, rules))
            })?,
        )?;

        // Define our own os.getenv function that knows how to resolve current
        // environment values from eg: the registry on Windows, or for
        // the current SHELL value on unix, even if the user has changed
        // those values since wezterm was started
        get_or_create_module(&lua, "os")?.set("getenv", lua.create_function(getenv)?)?;

        package
            .set("path", path_array.join(";"))
            .context("assign package.path")?;

        // Register `frankenterm` as a sibling module that shares the same
        // backing table as `wezterm`. This is the official rebrand: new code
        // should write `local frankenterm = require 'frankenterm'`. The
        // `wezterm` alias remains so reference WezTerm configs paste in
        // without edits. Both `package.loaded.wezterm` and
        // `package.loaded.frankenterm` resolve to the same Lua table, so a
        // setting reached via either name reaches the same place.
        let loaded_table: Table = package.get("loaded").context("get package.loaded")?;
        loaded_table
            .set("frankenterm", wezterm_mod.clone())
            .context("set package.loaded.frankenterm = wezterm_mod")?;
        // Also expose as a top-level global so `frankenterm.action.…`
        // works in config files that don't explicitly require() it,
        // mirroring the `wezterm` global that the package loader installs.
        globals
            .set("frankenterm", wezterm_mod)
            .context("set _G.frankenterm = wezterm_mod")?;
    }

    for func in setup_funcs_snapshot() {
        func(&lua).context("calling SETUP_FUNCS")?;
    }

    Ok(lua)
}

// ─── FrankenTerm Lua API: log_* helpers ───────────────────────────────────────
//
// Lua-facing wrappers around the Rust `log` crate. Each one accepts a single
// message argument (coerced via `tostring()` semantics for consistency with
// the upstream `wezterm.log_*` shape) and emits at the corresponding level.
// The `target` is set to `lua_config` so log lines coming out of user config
// scripts are easy to filter via `RUST_LOG=lua_config=info` (or the per-line wezterm style
// lua_config=trace`.

/// Format a variadic list of Lua values into a single space-separated string
/// using `tostring()` semantics. Mirrors upstream `wezterm.log_*` which
/// accepts any number of args and stringifies each; without this, configs
/// that do `wezterm.log_info("connected", host, "→", port)` would fail
/// to type-check at the FFI boundary.
fn format_lua_log_args(lua: &Lua, args: Variadic<mlua::Value>) -> mlua::Result<String> {
    let mut parts: Vec<String> = Vec::with_capacity(args.len());
    for arg in args {
        // Honor the value's `__tostring` metamethod when present (matches
        // Lua's `tostring()` builtin). Falls back to `nil` literal for
        // values that coerce to None.
        let coerced = lua.coerce_string(arg)?;
        let part = match coerced {
            Some(s) => s.to_str()?.to_string(),
            None => "nil".to_string(),
        };
        parts.push(part);
    }
    Ok(parts.join(" "))
}

fn lua_log_error(lua: &Lua, args: Variadic<mlua::Value>) -> mlua::Result<()> {
    let msg = format_lua_log_args(lua, args)?;
    log::error!(target: "lua_config", "{msg}");
    Ok(())
}

fn lua_log_warn(lua: &Lua, args: Variadic<mlua::Value>) -> mlua::Result<()> {
    let msg = format_lua_log_args(lua, args)?;
    log::warn!(target: "lua_config", "{msg}");
    Ok(())
}

fn lua_log_info(lua: &Lua, args: Variadic<mlua::Value>) -> mlua::Result<()> {
    let msg = format_lua_log_args(lua, args)?;
    log::info!(target: "lua_config", "{msg}");
    Ok(())
}

fn lua_log_debug(lua: &Lua, args: Variadic<mlua::Value>) -> mlua::Result<()> {
    let msg = format_lua_log_args(lua, args)?;
    log::debug!(target: "lua_config", "{msg}");
    Ok(())
}

fn lua_log_trace(lua: &Lua, args: Variadic<mlua::Value>) -> mlua::Result<()> {
    let msg = format_lua_log_args(lua, args)?;
    log::trace!(target: "lua_config", "{msg}");
    Ok(())
}

// ─── FrankenTerm Lua API: time helpers ────────────────────────────────────────

const TIME_CALL_AFTER_TASK_ESTIMATED_BYTES: usize = 8 * 1024;
const TIME_CALL_AFTER_INITIAL_ADMISSION_RETRY: Duration = Duration::from_millis(10);
const TIME_CALL_AFTER_MAX_ADMISSION_RETRY: Duration = Duration::from_secs(1);

// asupersync's canonical timer represents deadlines as u64 nanoseconds. Do
// not accept a larger Duration and silently let that layer saturate it to a
// different delay. This also keeps the non-asupersync promise backend within
// the same public input contract.
const TIME_CALL_AFTER_MAX_DURATION: Duration = Duration::from_nanos(u64::MAX);

/// One accepted timer owns the exact Lua generation and function that created
/// it. A config reload publishes a distinct `Lua`; looking a registry key up in
/// that replacement state is both type-correct and semantically wrong. The
/// clone here deliberately retains the origin generation until the callback
/// finishes. `time.call_after` is explicit fire-and-forget work, so an accepted
/// callback is not implicitly revoked by an unrelated config reload.
///
/// This is a deliberate FrankenTerm divergence from upstream's
/// generation-cancel policy: this profile's sole reconnect watchdog starts
/// from `gui-startup`, which does not re-fire on reload. Reloading therefore
/// neither clones nor reschedules an old timer; one self-rescheduling chain
/// remains one chain in its origin VM. That VM-retention tradeoff is bounded by
/// `try_reserve_background_task`'s process-wide task and byte authority.
struct LuaTimerCallback {
    origin_lua: Lua,
    callback: mlua::Function,
}

impl LuaTimerCallback {
    async fn invoke(self) {
        let Self {
            origin_lua,
            callback,
        } = self;
        if let Err(err) = callback.call_async::<()>(()).await {
            log::warn!(
                target: "lua_config",
                "time.call_after callback raised: {err}"
            );
        }
        // Keep the origin state alive across the complete async invocation,
        // including any yields from Lua code.
        drop(origin_lua);
    }
}

fn lua_timer_callback_lock(
    callback: &Mutex<Option<LuaTimerCallback>>,
) -> MutexGuard<'_, Option<LuaTimerCallback>> {
    match callback.lock() {
        Ok(callback) => callback,
        Err(poisoned) => {
            log::warn!(target: "lua_config", "recovering poisoned Lua timer callback mutex");
            callback.clear_poison();
            poisoned.into_inner()
        }
    }
}

fn lua_time_call_after_duration(seconds: f64) -> mlua::Result<Duration> {
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(mlua::Error::external(format!(
            "time.call_after: seconds must be a finite, non-negative number; got {seconds}"
        )));
    }

    let duration = Duration::try_from_secs_f64(seconds).map_err(|err| {
        mlua::Error::external(format!(
            "time.call_after: seconds is outside the representable duration range; got {seconds}: {err}"
        ))
    })?;
    if duration > TIME_CALL_AFTER_MAX_DURATION {
        return Err(mlua::Error::external(format!(
            "time.call_after: seconds exceeds the maximum supported timer duration of {} seconds; got {seconds}",
            TIME_CALL_AFTER_MAX_DURATION.as_secs_f64()
        )));
    }
    Ok(duration)
}

fn lua_timer_admission_retry_delay(rejection_count: u32) -> Duration {
    let exponent = rejection_count.saturating_sub(1).min(7);
    let multiplier = 1_u32 << exponent;
    TIME_CALL_AFTER_INITIAL_ADMISSION_RETRY
        .saturating_mul(multiplier)
        .min(TIME_CALL_AFTER_MAX_ADMISSION_RETRY)
}

/// Retain an already-accepted callback until some live main-thread scheduler
/// generation admits it. In particular, a timer callback can schedule its
/// successor while its own main-thread permit is still live: the successor is
/// handed to the independent bounded background executor, rather than being
/// rejected just because the current callback temporarily consumes the last
/// general main-thread slot.
async fn dispatch_lua_timer_callback(callback: LuaTimerCallback) {
    // Keep fallback ownership outside the scheduled future until that future
    // actually starts. A scheduler generation can retire after admitting the
    // task but before polling its first runnable; in that race the fallible
    // task resolves to None and the callback is still present here for a new
    // generation. Once the task takes it, execution is at-most-once.
    let callback = Arc::new(Mutex::new(Some(callback)));
    let mut rejection_count = 0_u32;

    loop {
        match promise::spawn::try_reserve_main_thread_with_low_priority(
            promise::spawn::MainThreadServiceClass::Background,
            TIME_CALL_AFTER_TASK_ESTIMATED_BYTES,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => {
                let callback_for_task = Arc::clone(&callback);
                let completed = reservation
                    .spawn(async move {
                        let callback = lua_timer_callback_lock(&callback_for_task).take();
                        let Some(callback) = callback else {
                            log::error!(
                                target: "lua_config",
                                "time.call_after: admitted task found callback ownership already consumed"
                            );
                            return;
                        };
                        callback.invoke().await;
                    })
                    .into_task()
                    .fallible()
                    .await;
                if completed.is_some() {
                    return;
                }

                if lua_timer_callback_lock(&callback).is_none() {
                    // The runnable began and consumed the callback before its
                    // scheduler disappeared. Replaying could invoke user code
                    // twice, so preserve the at-most-once contract and report
                    // the indeterminate interrupted execution loudly.
                    log::error!(
                        target: "lua_config",
                        "time.call_after: callback execution was interrupted after main-thread start; refusing an unsafe duplicate invocation"
                    );
                    return;
                }

                rejection_count = rejection_count.saturating_add(1);
                if rejection_count.is_power_of_two() {
                    log::warn!(
                        target: "lua_config",
                        "time.call_after: retaining callback after admitted scheduler generation retired before first poll (attempt {rejection_count})"
                    );
                }
                promise::spawn::sleep(lua_timer_admission_retry_delay(rejection_count)).await;
            }
            promise::spawn::MainThreadReservationOutcome::InvalidSize(rejection) => {
                // The estimate above is a positive constant, so retrying this
                // result can never make progress. Preserve a loud diagnostic
                // instead of spinning forever on an internal contract breach.
                log::error!(
                    target: "lua_config",
                    "time.call_after: main-thread scheduler rejected the fixed callback size: {rejection:?}"
                );
                return;
            }
            rejected => {
                rejection_count = rejection_count.saturating_add(1);
                if rejection_count.is_power_of_two() {
                    log::warn!(
                        target: "lua_config",
                        "time.call_after: retaining callback after transient main-thread admission rejection (attempt {rejection_count}): {rejected:?}"
                    );
                }
                promise::spawn::sleep(lua_timer_admission_retry_delay(rejection_count)).await;
            }
        }
    }
}

/// `wezterm.time.now()` — returns seconds since UNIX epoch as a float.
fn lua_time_now(_: &Lua, _: ()) -> mlua::Result<f64> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    Ok(secs)
}

/// `wezterm.time.call_after(seconds, function)` — schedules `function` to be
/// called once after `seconds` have elapsed. Returns immediately (fire-and-
/// forget); the callback runs on the main thread against the exact Lua
/// generation that accepted it.
///
/// Implementation uses the project's `promise` crate, which delegates to
/// `asupersync::time::sleep` when the `async-asupersync` feature is enabled
/// (see `frankenterm/promise/src/spawn.rs::sleep`). This is the
/// FrankenTerm-policy-compliant timer path — direct `tokio` usage is banned
/// by AGENTS.md.
///
/// Internals:
///   1. Validate and convert the delay without a panic path.
///   2. Reserve bounded background capacity before retaining callback state.
///   3. Clone the originating `Lua` and move its function handle into that
///      reserved timer task.
///   4. On wake, retry bounded main-thread admission until a live scheduler
///      generation accepts the still-owned callback, then invoke it there.
fn lua_time_call_after(lua: &Lua, (seconds, func): (f64, mlua::Function)) -> mlua::Result<()> {
    let duration = lua_time_call_after_duration(seconds)?;
    let reservation = promise::spawn::try_reserve_background_task(
        TIME_CALL_AFTER_TASK_ESTIMATED_BYTES,
    )
    .map_err(|err| {
        mlua::Error::external(format!(
            "time.call_after: bounded background timer capacity unavailable before callback handoff: {err}"
        ))
    })?;

    let callback = LuaTimerCallback {
        origin_lua: lua.clone(),
        callback: func,
    };
    reservation.spawn(async move {
        promise::spawn::sleep(duration).await;
        dispatch_lua_timer_callback(callback).await;
    });

    Ok(())
}

/// Resolve an environment variable.
/// Lean on CommandBuilder's ability to update to current values of certain
/// environment variables that may be adjusted via the registry or implicitly
/// via eg: chsh (SHELL).
fn getenv(_: &Lua, env: String) -> mlua::Result<Option<String>> {
    let cmd = CommandBuilder::new_default_prog();
    match cmd.get_env(&env) {
        Some(s) => match s.to_str() {
            Some(s) => Ok(Some(s.to_string())),
            None => Err(mlua::Error::external(format!(
                "env var {env} is not representable as UTF-8"
            ))),
        },
        None => Ok(None),
    }
}

fn shell_split(_: &Lua, line: String) -> mlua::Result<Vec<String>> {
    shlex::split(&line).ok_or_else(|| {
        mlua::Error::external(format!("cannot tokenize `{line}` using posix shell rules"))
    })
}

fn shell_join_args(_: &Lua, args: Vec<String>) -> mlua::Result<String> {
    Ok(shlex::try_join(args.iter().map(|arg| arg.as_ref())).map_err(mlua::Error::external)?)
}

fn shell_quote_arg(_: &Lua, arg: String) -> mlua::Result<String> {
    Ok(shlex::try_quote(&arg)
        .map_err(mlua::Error::external)?
        .into_owned())
}

/// Returns the system hostname.
/// Errors may occur while retrieving the hostname from the system,
/// or if the hostname isn't a UTF-8 string.
fn hostname(_: &Lua, _: ()) -> mlua::Result<String> {
    let hostname = hostname::get().map_err(mlua::Error::external)?;
    match hostname.to_str() {
        Some(hostname) => Ok(hostname.to_owned()),
        None => Err(mlua::Error::external(anyhow!("hostname isn't UTF-8"))),
    }
}

#[derive(Debug, Default, FromDynamic, ToDynamic, Clone, PartialEq, Eq, Hash)]
struct TextStyleAttributes {
    /// Whether the font should be a bold variant
    #[dynamic(default)]
    pub bold: Option<bool>,
    #[dynamic(default)]
    pub weight: Option<FontWeight>,
    #[dynamic(default)]
    pub stretch: FontStretch,
    /// Whether the font should be an italic variant
    #[dynamic(default)]
    pub style: FontStyle,
    // Ideally we'd simply use serde's aliasing functionality on the `style`
    // field to support backwards compatibility, but aliases are invisible
    // to serde_lua, so we do a little fixup here ourselves in our from_lua impl.
    italic: Option<bool>,
    /// If set, when rendering text that is set to the default
    /// foreground color, use this color instead.  This is most
    /// useful in a `[[font_rules]]` section to implement changing
    /// the text color for eg: bold text.
    pub foreground: Option<RgbaColor>,
}
impl FromLua for TextStyleAttributes {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self, mlua::Error> {
        let mut attr: TextStyleAttributes = from_lua_value_dynamic(value)?;
        if let Some(italic) = attr.italic.take() {
            attr.style = if italic {
                FontStyle::Italic
            } else {
                FontStyle::Normal
            };
        }
        Ok(attr)
    }
}

#[derive(Debug, Default, FromDynamic, ToDynamic, Clone, PartialEq, Eq, Hash)]
struct LuaFontAttributes {
    /// The font family name
    pub family: String,
    /// Whether the font should be a bold variant
    #[dynamic(default)]
    pub weight: FontWeight,
    #[dynamic(default)]
    pub stretch: FontStretch,
    /// Whether the font should be an italic variant
    #[dynamic(default)]
    pub style: FontStyle,
    // Ideally we'd simply use serde's aliasing functionality on the `style`
    // field to support backwards compatibility, but aliases are invisible
    // to serde_lua, so we do a little fixup here ourselves in our from_lua impl.
    #[dynamic(default)]
    italic: Option<bool>,

    #[dynamic(default)]
    pub harfbuzz_features: Option<Vec<String>>,
    #[dynamic(default)]
    pub freetype_load_target: Option<FreeTypeLoadTarget>,
    #[dynamic(default)]
    pub freetype_render_target: Option<FreeTypeLoadTarget>,
    #[dynamic(default)]
    pub freetype_load_flags: Option<String>,
    #[dynamic(default)]
    pub scale: Option<NotNan<f64>>,
    #[dynamic(default)]
    pub assume_emoji_presentation: Option<bool>,
}
impl FromLua for LuaFontAttributes {
    fn from_lua(value: Value, _lua: &Lua) -> Result<Self, mlua::Error> {
        match value {
            Value::String(s) => {
                let mut attr = LuaFontAttributes::default();
                attr.family = s.to_str()?.to_string();
                Ok(attr)
            }
            v => {
                let mut attr: LuaFontAttributes = from_lua_value_dynamic(v)?;
                if let Some(italic) = attr.italic.take() {
                    attr.style = if italic {
                        FontStyle::Italic
                    } else {
                        FontStyle::Normal
                    };
                }
                Ok(attr)
            }
        }
    }
}

/// On macOS, both Menlo and Monaco fonts have ligatures for `fi` that
/// take effect for words like `find` and which are a source of
/// confusion/annoyance and issues filed on Github.
/// Let's default to disabling ligatures for these fonts unless
/// the user has explicitly specified harfbuzz_features.
/// <https://github.com/wezterm/wezterm/issues/1736>
/// <https://github.com/wezterm/wezterm/issues/1786>
fn disable_ligatures_for_menlo_or_monaco(mut attrs: FontAttributes) -> FontAttributes {
    if attrs.harfbuzz_features.is_none() && (attrs.family == "Menlo" || attrs.family == "Monaco") {
        attrs.harfbuzz_features = Some(vec![
            "kern".to_string(),
            "clig".to_string(),
            "liga=0".to_string(),
        ]);
    }
    attrs
}

/// Given a simple font family name, returns a text style instance.
/// The second optional argument is a list of the other TextStyle
/// fields, which at the time of writing includes only the
/// `foreground` color that can be used to force a particular
/// color to be used for this text style.
///
/// `wezterm.font("foo", {foreground="tomato"})`
/// yields:
/// `{ font = {{ family = "foo" }}, foreground="tomato"}`
fn font(
    _lua: &Lua,
    (mut attrs, map_defaults): (LuaFontAttributes, Option<TextStyleAttributes>),
) -> mlua::Result<TextStyle> {
    let mut text_style = TextStyle::default();
    text_style.font.clear();

    if let Some(map_defaults) = map_defaults {
        attrs.weight = match map_defaults.bold {
            Some(true) => FontWeight::BOLD,
            Some(false) => FontWeight::REGULAR,
            None => map_defaults.weight.unwrap_or(FontWeight::REGULAR),
        };
        attrs.stretch = map_defaults.stretch;
        attrs.style = map_defaults.style;
        text_style.foreground = map_defaults.foreground;
    }

    text_style
        .font
        .push(disable_ligatures_for_menlo_or_monaco(FontAttributes {
            family: attrs.family,
            stretch: attrs.stretch,
            weight: attrs.weight,
            style: attrs.style,
            is_fallback: false,
            is_synthetic: false,
            harfbuzz_features: attrs.harfbuzz_features,
            freetype_load_target: attrs.freetype_load_target,
            freetype_render_target: attrs.freetype_render_target,
            freetype_load_flags: match attrs.freetype_load_flags {
                Some(flags) => Some(TryFrom::try_from(flags).map_err(mlua::Error::external)?),
                None => None,
            },
            scale: attrs.scale,
            assume_emoji_presentation: attrs.assume_emoji_presentation,
        }));

    Ok(text_style)
}

/// Given a list of font family names in order of preference, return a
/// text style instance for that font configuration.
///
/// `wezterm.font_with_fallback({"Operator Mono", "DengXian"})`
///
/// The second optional argument is a list of other TextStyle fields,
/// as described by the `wezterm.font` documentation.
fn font_with_fallback(
    _lua: &Lua,
    (fallback, map_defaults): (Vec<LuaFontAttributes>, Option<TextStyleAttributes>),
) -> mlua::Result<TextStyle> {
    let mut text_style = TextStyle::default();
    text_style.font.clear();

    for (idx, mut attrs) in fallback.into_iter().enumerate() {
        if let Some(map_defaults) = &map_defaults {
            attrs.weight = match map_defaults.bold {
                Some(true) => FontWeight::BOLD,
                Some(false) => FontWeight::REGULAR,
                None => map_defaults.weight.unwrap_or(FontWeight::REGULAR),
            };
            attrs.stretch = map_defaults.stretch;
            attrs.style = map_defaults.style;
            text_style.foreground = map_defaults.foreground;
        }

        text_style
            .font
            .push(disable_ligatures_for_menlo_or_monaco(FontAttributes {
                family: attrs.family,
                stretch: attrs.stretch,
                weight: attrs.weight,
                style: attrs.style,
                is_fallback: idx != 0,
                is_synthetic: false,
                harfbuzz_features: attrs.harfbuzz_features,
                freetype_load_target: attrs.freetype_load_target,
                freetype_render_target: attrs.freetype_render_target,
                freetype_load_flags: match attrs.freetype_load_flags {
                    Some(flags) => Some(TryFrom::try_from(flags).map_err(mlua::Error::external)?),
                    None => None,
                },
                scale: attrs.scale,
                assume_emoji_presentation: attrs.assume_emoji_presentation,
            }));
    }

    Ok(text_style)
}

pub fn wrap_callback(lua: &Lua, callback: mlua::Function) -> mlua::Result<String> {
    let callback_count: i32 = lua.named_registry_value(LUA_REGISTRY_USER_CALLBACK_COUNT)?;
    let user_event_id = format!("user-defined-{}", callback_count);
    lua.set_named_registry_value(LUA_REGISTRY_USER_CALLBACK_COUNT, callback_count + 1)?;
    register_event(lua, (user_event_id.clone(), callback))?;
    Ok(user_event_id)
}

fn action_callback(lua: &Lua, callback: mlua::Function) -> mlua::Result<KeyAssignment> {
    let user_event_id = wrap_callback(lua, callback)?;
    Ok(KeyAssignment::EmitEvent(user_event_id))
}

fn exec_domain(
    lua: &Lua,
    (name, fixup_command, label): (String, mlua::Function, Option<mlua::Value>),
) -> mlua::Result<ExecDomain> {
    let fixup_command = {
        let event_name = format!("exec-domain-{name}");
        register_event(lua, (event_name.clone(), fixup_command))?;
        event_name
    };

    let label = match label {
        Some(Value::Function(callback)) => {
            let event_name = format!("exec-domain-{name}-label");
            register_event(lua, (event_name.clone(), callback))?;
            Some(ValueOrFunc::Func(event_name))
        }
        Some(Value::String(value)) => Some(ValueOrFunc::Value(lua_value_to_dynamic(
            Value::String(value),
        )?)),
        Some(_) => {
            return Err(mlua::Error::external(
                "label function parameter must be either a string or a lua function",
            ));
        }
        None => None,
    };
    Ok(ExecDomain {
        name,
        fixup_command,
        label,
    })
}

fn split_by_newlines(_: &Lua, text: String) -> mlua::Result<Vec<String>> {
    Ok(text
        .lines()
        .map(|s| {
            // Ungh, `str.lines()` is supposed to split by `\n` or `\r\n`, but I've
            // found that it is necessary to have an additional trim here in order
            // to actually remove the `\r`.
            s.trim_end_matches('\r').to_string()
        })
        .collect())
}

/// This implements `wezterm.on`, whose goal is to register an event handler
/// callback.
/// The callback function may return `false` to prevent other handlers from
/// triggering.  The `false` return means "prevent the default action",
/// and thus, depending on the semantics of the emitted event, can be used
/// to override rather augment built-in behavior.
///
/// To allow the default action you can omit a return statement, or
/// explicitly return `true`.
///
/// The arguments to the handler are passed through from the corresponding
/// `wezterm.emit` call.
///
/// ```lua
/// wezterm.on("event-name", function(arg1, arg2)
///   -- do something
///   return false -- if you want to prevent other handlers running
/// end);
///
/// wezterm.emit("event-name", "foo", "bar");
/// ```
pub fn register_event(lua: &Lua, (name, func): (String, mlua::Function)) -> mlua::Result<()> {
    let decorated_name = format!("wezterm-event-{}", name);
    let tbl: mlua::Value = lua.named_registry_value(&decorated_name)?;
    match tbl {
        mlua::Value::Nil => {
            let tbl = lua.create_table()?;
            tbl.set(1, func)?;
            lua.set_named_registry_value(&decorated_name, tbl)?;
            Ok(())
        }
        mlua::Value::Table(tbl) => {
            let len = tbl.raw_len();
            tbl.set(len + 1, func)?;
            Ok(())
        }
        _ => Err(mlua::Error::external(anyhow!(
            "registry key for {} has invalid type",
            decorated_name
        ))),
    }
}

const IS_EVENT: &str = "wezterm-is-event-emission";

/// Returns true if the current lua context is being called as part
/// of an emit_event call.
pub fn is_event_emission(lua: &Lua) -> mlua::Result<bool> {
    match lua.named_registry_value(IS_EVENT)? {
        Value::Nil => Ok(false),
        Value::Boolean(value) => Ok(value),
        value => Err(mlua::Error::external(anyhow!(
            "registry key for {} has invalid type {}",
            IS_EVENT,
            value.type_name()
        ))),
    }
}

/// This implements `wezterm.emit`.
/// The first parameter to emit is the name of a signal that may or may not
/// have previously been registered via `wezterm.on`.
/// `wezterm.emit` will call each of the registered handlers in the order
/// that they were registered and pass the remainder of the `emit` arguments
/// to those handler functions.
/// If a handler returns `false` then `wezterm.emit` will stop calling
/// any additional handlers and then return `false`.
/// Otherwise, once all handlers have been called and none of them returned
/// `false`, `wezterm.emit` will return `true`.
/// The return value indicates to the caller whether the default action
/// should take place.
pub async fn emit_event(lua: Lua, (name, args): (String, mlua::MultiValue)) -> mlua::Result<bool> {
    let was_emitting = is_event_emission(&lua)?;
    lua.set_named_registry_value(IS_EVENT, true)?;

    let decorated_name = format!("wezterm-event-{}", name);
    let tbl: mlua::Value = lua.named_registry_value(&decorated_name)?;
    let result = match tbl {
        mlua::Value::Table(tbl) => {
            let mut emit_result = Ok(true);
            let handlers = tbl
                .sequence_values::<mlua::Function>()
                .collect::<mlua::Result<Vec<_>>>();
            let handlers = match handlers {
                Ok(handlers) => handlers,
                Err(err) => return Err(err),
            };
            for func in handlers {
                match func.call_async(args.clone()).await {
                    Ok(mlua::Value::Boolean(b)) if !b => {
                        // Default action prevented
                        emit_result = Ok(false);
                        break;
                    }
                    Err(e) => {
                        emit_result = Err(e);
                        break;
                    }
                    _ => {
                        // Continue with other handlers
                    }
                }
            }
            emit_result
        }
        _ => Ok(true),
    };
    let restore_result = lua.set_named_registry_value(IS_EVENT, was_emitting);
    match (result, restore_result) {
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
        (Ok(value), Ok(())) => Ok(value),
    }
}

pub fn emit_sync_callback<A>(lua: &Lua, (name, args): (String, A)) -> mlua::Result<mlua::Value>
where
    A: IntoLuaMulti,
{
    let decorated_name = format!("wezterm-event-{}", name);
    let tbl: mlua::Value = lua.named_registry_value(&decorated_name)?;
    match tbl {
        mlua::Value::Table(tbl) => {
            if let Some(func) = tbl.sequence_values::<mlua::Function>().next() {
                let func = func?;
                return func.call(args);
            }
            Ok(mlua::Value::Nil)
        }
        _ => Ok(mlua::Value::Nil),
    }
}

pub async fn emit_async_callback<A>(
    lua: &Lua,
    (name, args): (String, A),
) -> mlua::Result<mlua::Value>
where
    A: IntoLuaMulti,
{
    let decorated_name = format!("wezterm-event-{}", name);
    let tbl: mlua::Value = lua.named_registry_value(&decorated_name)?;
    match tbl {
        mlua::Value::Table(tbl) => {
            if let Some(func) = tbl.sequence_values::<mlua::Function>().next() {
                let func = func?;
                return func.call_async(args).await;
            }
            Ok(mlua::Value::Nil)
        }
        _ => Ok(mlua::Value::Nil),
    }
}

/// Ungh: https://github.com/microsoft/WSL/issues/4456
fn utf16_to_utf8(_: &Lua, text: mlua::String) -> mlua::Result<String> {
    let bytes = text.as_bytes();

    if bytes.len() % 2 != 0 {
        return Err(mlua::Error::external(anyhow!(
            "input data has odd length, cannot be utf16"
        )));
    }

    // This is "safe" because we checked that the length seems reasonable,
    // and our new slice is within those same bounds.
    let wide: &[u16] =
        unsafe { std::slice::from_raw_parts(bytes.as_ptr() as *const u16, bytes.len() / 2) };

    String::from_utf16(wide).map_err(mlua::Error::external)
}

pub fn add_to_config_reload_watch_list(lua: &Lua, args: Variadic<String>) -> mlua::Result<()> {
    let mut watch_paths: Vec<String> = lua.named_registry_value("wezterm-watch-paths")?;
    watch_paths.extend_from_slice(&args);
    lua.set_named_registry_value("wezterm-watch-paths", watch_paths)?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Instant;

    fn timer_test_executor() -> promise::spawn::SimpleExecutor {
        let limits = promise::spawn::MainThreadAdmissionLimits::new(
            1,
            TIME_CALL_AFTER_TASK_ESTIMATED_BYTES * 2,
            0,
            0,
        )
        .expect("timer test scheduler limits are valid");
        promise::spawn::SimpleExecutor::try_with_limits(limits)
            .expect("timer test scheduler identity is available")
    }

    fn drive_timer_executor_until<T>(
        executor: &promise::spawn::SimpleExecutor,
        receiver: &mpsc::Receiver<T>,
    ) -> anyhow::Result<Option<T>> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            match receiver.try_recv() {
                Ok(value) => return Ok(Some(value)),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => {
                    anyhow::bail!("timer callback result channel disconnected before delivery")
                }
            }
            while executor.try_tick()? {}
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn wait_for_timer_queue_depth(
        executor: &promise::spawn::SimpleExecutor,
        expected_depth: usize,
    ) -> bool {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if executor.queue_snapshot().depth == expected_depth {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn time_call_after_duration_validation_has_no_finite_overflow_panic() {
        assert_eq!(
            lua_time_call_after_duration(0.0).expect("zero is a valid delay"),
            Duration::ZERO
        );
        assert!(lua_time_call_after_duration(-1.0).is_err());
        assert!(lua_time_call_after_duration(f64::NAN).is_err());
        assert!(lua_time_call_after_duration(f64::INFINITY).is_err());

        let oversized_but_finite = f64::MAX;
        let result =
            std::panic::catch_unwind(|| lua_time_call_after_duration(oversized_but_finite));
        assert!(
            result.is_ok(),
            "finite Lua input must never reach Duration::from_secs_f64's panic path"
        );
        assert!(
            result.expect("duration conversion did not panic").is_err(),
            "an unrepresentable finite delay must be rejected"
        );

        let exceeds_canonical_timer = TIME_CALL_AFTER_MAX_DURATION.as_secs_f64() * 2.0;
        assert!(
            lua_time_call_after_duration(exceeds_canonical_timer).is_err(),
            "a delay that the canonical timer would saturate must be rejected"
        );
    }

    #[test]
    fn time_call_after_admission_retry_is_bounded_exponential_backoff() {
        assert_eq!(
            lua_timer_admission_retry_delay(1),
            TIME_CALL_AFTER_INITIAL_ADMISSION_RETRY
        );
        assert_eq!(
            lua_timer_admission_retry_delay(2),
            TIME_CALL_AFTER_INITIAL_ADMISSION_RETRY * 2
        );
        assert_eq!(
            lua_timer_admission_retry_delay(u32::MAX),
            TIME_CALL_AFTER_MAX_ADMISSION_RETRY
        );
    }

    #[test]
    fn time_call_after_retains_callback_across_transient_main_admission_rejection(
    ) -> anyhow::Result<()> {
        let _env = crate::test_env_lock();
        let executor = timer_test_executor();
        let blocker = match promise::spawn::try_reserve_main_thread_with_low_priority(
            promise::spawn::MainThreadServiceClass::Background,
            TIME_CALL_AFTER_TASK_ESTIMATED_BYTES,
        ) {
            promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
            outcome => anyhow::bail!("failed to plant full main-thread admission: {outcome:?}"),
        };

        let lua = make_lua_context(Path::new("testing"))?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let callback = lua.create_function(move |_, ()| {
            let _ = sender.try_send(());
            Ok(())
        })?;
        lua_time_call_after(&lua, (0.0, callback))?;

        // The background owner may wake, but the planted reservation is the
        // only main-thread slot, so the callback cannot have run yet.
        std::thread::sleep(Duration::from_millis(50));
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        drop(blocker);
        assert!(
            drive_timer_executor_until(&executor, &receiver)?.is_some(),
            "the still-owned callback must run after transient pressure clears"
        );
        Ok(())
    }

    #[test]
    fn time_call_after_retries_when_admitted_generation_retires_before_poll() -> anyhow::Result<()>
    {
        let _env = crate::test_env_lock();
        let old_executor = timer_test_executor();
        let lua = make_lua_context(Path::new("testing"))?;
        let (sender, receiver) = mpsc::sync_channel(1);
        let callback = lua.create_function(move |_, ()| {
            let _ = sender.try_send(());
            Ok(())
        })?;
        lua_time_call_after(&lua, (0.0, callback))?;

        let queued_on_old_generation = wait_for_timer_queue_depth(&old_executor, 1);
        let new_executor = timer_test_executor();
        drop(old_executor);

        assert!(
            queued_on_old_generation,
            "test must plant admission and enqueue on the retiring generation"
        );
        assert!(
            drive_timer_executor_until(&new_executor, &receiver)?.is_some(),
            "callback authority must return to the background owner and retry on the replacement generation"
        );
        Ok(())
    }

    #[test]
    fn time_call_after_successor_survives_parent_callback_admission() -> anyhow::Result<()> {
        let _env = crate::test_env_lock();
        let executor = timer_test_executor();
        crate::designate_this_as_the_main_thread();
        crate::run_immediate_with_lua_config(|_| Ok(()))?;

        let lua = make_lua_context(Path::new("testing"))?;
        crate::LUA_PIPE
            .sender
            .try_send(lua.clone())
            .map_err(|err| anyhow!("failed to publish timer-chain Lua test context: {err}"))?;
        crate::run_immediate_with_lua_config(|_| Ok(()))?;

        let (sender, receiver) = mpsc::sync_channel(1);
        lua.globals().set(
            "record_timer_chain_completion",
            lua.create_function(move |_, ()| {
                let _ = sender.try_send(());
                Ok(())
            })?,
        )?;
        let first_callback: mlua::Function = lua
            .load(
                r#"
local frankenterm = require 'frankenterm'
return function()
  frankenterm.time.call_after(0, function()
    record_timer_chain_completion()
  end)
end
"#,
            )
            .eval()?;

        lua_time_call_after(&lua, (0.0, first_callback))?;
        assert!(
            drive_timer_executor_until(&executor, &receiver)?.is_some(),
            "a callback holding the only main-thread slot must durably hand off its successor"
        );
        Ok(())
    }

    #[test]
    fn time_call_after_dispatches_against_its_origin_lua_after_reload() -> anyhow::Result<()> {
        let _env = crate::test_env_lock();
        let executor = timer_test_executor();
        crate::designate_this_as_the_main_thread();
        crate::run_immediate_with_lua_config(|_| Ok(()))?;

        let origin = make_lua_context(Path::new("origin.lua"))?;
        origin.set_named_registry_value("timer-generation-marker", "origin")?;
        let replacement = make_lua_context(Path::new("replacement.lua"))?;
        replacement.set_named_registry_value("timer-generation-marker", "replacement")?;

        let (sender, receiver) = mpsc::sync_channel(1);
        let callback = origin.create_function(move |lua, ()| {
            let marker: String = lua.named_registry_value("timer-generation-marker")?;
            let _ = sender.try_send(marker);
            Ok(())
        })?;
        lua_time_call_after(&origin, (0.0, callback))?;

        // Publish a replacement before the timer is allowed to fire. The old
        // implementation fetched the origin registry key from this newest Lua
        // and silently lost the callback.
        crate::LUA_PIPE
            .sender
            .try_send(replacement)
            .map_err(|err| anyhow!("failed to publish replacement Lua test context: {err}"))?;
        drop(origin);

        let marker = drive_timer_executor_until(&executor, &receiver)?
            .context("origin-generation timer callback did not run")?;
        assert_eq!(marker, "origin");

        let latest_marker = crate::run_immediate_with_lua_config(|lua| {
            let lua = lua.context("replacement Lua was not published")?;
            Ok(lua.named_registry_value::<String>("timer-generation-marker")?)
        })?;
        assert_eq!(latest_marker, "replacement");
        Ok(())
    }

    #[test]
    fn time_call_after_origin_chain_survives_reloads_once_without_multiplication(
    ) -> anyhow::Result<()> {
        let _env = crate::test_env_lock();
        let executor = timer_test_executor();
        crate::designate_this_as_the_main_thread();
        crate::run_immediate_with_lua_config(|_| Ok(()))?;

        let origin = make_lua_context(Path::new("origin-chain.lua"))?;
        origin.globals().set("timer_generation_marker", "origin")?;
        let (sender, receiver) = mpsc::sync_channel(4);
        origin.globals().set(
            "record_timer_chain_generation",
            origin.create_function(move |_, marker: String| {
                let _ = sender.try_send(marker);
                Ok(())
            })?,
        )?;
        let first_callback: mlua::Function = origin
            .load(
                r#"
local frankenterm = require 'frankenterm'
return function()
  record_timer_chain_generation(timer_generation_marker)
  frankenterm.time.call_after(0, function()
    record_timer_chain_generation(timer_generation_marker)
  end)
end
"#,
            )
            .eval()?;
        lua_time_call_after(&origin, (0.0, first_callback))?;

        for (path, marker) in [
            ("replacement-chain-1.lua", "replacement-1"),
            ("replacement-chain-2.lua", "replacement-2"),
        ] {
            let replacement = make_lua_context(Path::new(path))?;
            replacement
                .globals()
                .set("timer_generation_marker", marker)?;
            crate::LUA_PIPE
                .sender
                .try_send(replacement)
                .map_err(|err| anyhow!("failed to publish {marker} Lua test context: {err}"))?;
        }
        drop(origin);

        let first = drive_timer_executor_until(&executor, &receiver)?
            .context("origin callback was silently lost across reload publications")?;
        let successor = drive_timer_executor_until(&executor, &receiver)?
            .context("origin callback failed to hand off its sole successor")?;
        assert_eq!((first.as_str(), successor.as_str()), ("origin", "origin"));

        // Continue pumping after both expected deliveries. Reload publication
        // must not clone or restart the old chain behind our back.
        let quiet_deadline = Instant::now() + Duration::from_millis(100);
        let mut duplicate = None;
        while Instant::now() < quiet_deadline {
            while executor.try_tick()? {}
            match receiver.try_recv() {
                Ok(marker) => {
                    duplicate = Some(marker);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => break,
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(duplicate, None, "reload multiplied the origin timer chain");

        let latest_marker = crate::run_immediate_with_lua_config(|lua| {
            let lua = lua.context("latest replacement Lua was not published")?;
            Ok(lua.globals().get::<String>("timer_generation_marker")?)
        })?;
        assert_eq!(latest_marker, "replacement-2");
        Ok(())
    }

    /// GH#76: `require` from frankenterm.lua must resolve modules in the
    /// FrankenTerm-namespaced config dirs before any wezterm-namespaced
    /// fallback, and the legacy `~/.wezterm` dir must rank last. The old
    /// implementation inserted each dir at index 0 while iterating
    /// front-to-back, which reversed the precedence so a side-by-side
    /// `~/.config/wezterm/mod.lua` silently shadowed
    /// `~/.config/frankenterm/mod.lua`.
    #[test]
    fn package_path_prefix_preserves_config_dir_precedence() {
        let config_dirs = vec![
            PathBuf::from("/home/u/.config/frankenterm"),
            PathBuf::from("/etc/xdg/frankenterm"),
            PathBuf::from("/home/u/.config/wezterm"),
        ];
        let prefix = lua_package_path_prefix(&config_dirs, Path::new("/home/u"));
        assert_eq!(
            prefix,
            vec![
                "/home/u/.config/frankenterm/?.lua".to_string(),
                "/home/u/.config/frankenterm/?/init.lua".to_string(),
                "/etc/xdg/frankenterm/?.lua".to_string(),
                "/etc/xdg/frankenterm/?/init.lua".to_string(),
                "/home/u/.config/wezterm/?.lua".to_string(),
                "/home/u/.config/wezterm/?/init.lua".to_string(),
                "/home/u/.wezterm/?.lua".to_string(),
                "/home/u/.wezterm/?/init.lua".to_string(),
            ]
        );
    }

    /// GH#76: the fully-assembled `package.path` inside a live Lua context
    /// must list every FrankenTerm-namespaced dir ahead of every
    /// wezterm-namespaced dir.
    #[test]
    fn lua_context_package_path_orders_frankenterm_dirs_first() -> anyhow::Result<()> {
        let _env = crate::test_env_lock();
        let lua = make_lua_context(Path::new("testing"))?;
        let package: Table = lua.globals().get("package")?;
        let package_path: String = package.get("path")?;
        let entries: Vec<&str> = package_path.split(';').collect();

        let pos_of = |needle: &str| entries.iter().position(|e| e.contains(needle));
        let first_frankenterm = pos_of("frankenterm");
        let first_wezterm = entries
            .iter()
            .position(|e| e.contains("wezterm") && !e.contains("frankenterm"));
        if let (Some(ft), Some(wz)) = (first_frankenterm, first_wezterm) {
            assert!(
                ft < wz,
                "frankenterm-namespaced dirs must precede wezterm-namespaced dirs in package.path: {}",
                package_path
            );
        } else {
            panic!(
                "expected both frankenterm and wezterm entries in package.path: {}",
                package_path
            );
        }
        Ok(())
    }

    #[test]
    fn setup_funcs_recover_after_poisoned_lock() {
        let _env = crate::test_env_lock();

        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = SETUP_FUNCS.lock().unwrap();
            panic!("poison Lua setup funcs");
        }));
        assert!(poison.is_err());

        add_context_setup_func(|_| Ok(()));
        make_lua_context(Path::new("testing")).expect("poisoned setup lock should recover");
    }

    #[test]
    fn can_register_and_emit_multiple_events() -> anyhow::Result<()> {
        let _ = env_logger::Builder::new()
            .is_test(true)
            .filter_level(log::LevelFilter::Trace)
            .try_init();

        let lua = make_lua_context(Path::new("testing"))?;

        let total = Arc::new(Mutex::new(0));

        let first = lua.create_function({
            let total = total.clone();
            move |_lua: &mlua::Lua, n: i32| {
                let mut l = total.lock().unwrap();
                *l += n;
                Ok(())
            }
        })?;

        let second = lua.create_function({
            let total = total.clone();
            move |_lua: &mlua::Lua, n: i32| {
                let mut l = total.lock().unwrap();
                *l += n * 2;
                // Prevent any later functions from being called
                Ok(false)
            }
        })?;

        let third = lua.create_function({
            let total = total.clone();
            move |_lua: &mlua::Lua, n: i32| {
                let mut l = total.lock().unwrap();
                *l += n * 3;
                Ok(())
            }
        })?;

        register_event(&lua, ("foo".to_string(), first))?;
        register_event(&lua, ("foo".to_string(), second))?;
        register_event(&lua, ("foo".to_string(), third))?;
        register_event(
            &lua,
            (
                "bar".to_string(),
                lua.create_function(|_: &mlua::Lua, (a, b): (i32, String)| {
                    eprintln!("a: {}, b: {}", a, b);
                    Ok(())
                })?,
            ),
        )?;

        promise::spawn::block_on(
            lua.load(
                r#"
local wezterm = require 'wezterm';

wezterm.on('foo', function (n)
    print("lua hook recording " .. n);
end);

-- one of the foo handlers returns false, so the emit
-- returns false overall, indicating that the default
-- action should not be taken
assert(wezterm.emit('foo', 2) == false)

wezterm.on('bar', function (n, str)
    print("bar says " .. n .. " " .. str)
end);

-- None of the bar handlers return anything, so the
-- emit returns true to indicate that the default
-- action should be performed
assert(wezterm.emit('bar', 42, 'woot') == true)
"#,
            )
            .exec_async(),
        )?;

        assert_eq!(*total.lock().unwrap(), 6);

        Ok(())
    }

    #[test]
    fn event_emission_flag_defaults_false_and_is_restored_after_success() -> anyhow::Result<()> {
        let lua = make_lua_context(Path::new("testing"))?;
        assert!(!is_event_emission(&lua)?);

        let seen_inside_emit = Arc::new(Mutex::new(false));
        let handler = lua.create_function({
            let seen_inside_emit = seen_inside_emit.clone();
            move |lua: &mlua::Lua, ()| {
                *seen_inside_emit.lock().unwrap() = is_event_emission(lua)?;
                Ok(())
            }
        })?;
        register_event(&lua, ("flag-success".to_string(), handler))?;

        promise::spawn::block_on(
            lua.load(
                r#"
local wezterm = require 'wezterm'
assert(wezterm.emit('flag-success') == true)
"#,
            )
            .exec_async(),
        )?;

        assert!(
            *seen_inside_emit.lock().unwrap(),
            "handler should observe event-emission state"
        );
        assert!(
            !is_event_emission(&lua)?,
            "event-emission state should be cleared after emit completes"
        );

        Ok(())
    }

    #[test]
    fn event_emission_flag_is_restored_after_handler_error() -> anyhow::Result<()> {
        let lua = make_lua_context(Path::new("testing"))?;
        assert!(!is_event_emission(&lua)?);

        let seen_inside_emit = Arc::new(Mutex::new(false));
        let handler = lua.create_function({
            let seen_inside_emit = seen_inside_emit.clone();
            move |lua: &mlua::Lua, ()| -> mlua::Result<()> {
                *seen_inside_emit.lock().unwrap() = is_event_emission(lua)?;
                Err(mlua::Error::external("boom"))
            }
        })?;
        register_event(&lua, ("flag-error".to_string(), handler))?;

        let result = promise::spawn::block_on(
            lua.load(
                r#"
local wezterm = require 'wezterm'
wezterm.emit('flag-error')
"#,
            )
            .exec_async(),
        );
        assert!(result.is_err(), "handler error should propagate");
        assert!(
            *seen_inside_emit.lock().unwrap(),
            "handler should observe event-emission state"
        );
        assert!(
            !is_event_emission(&lua)?,
            "event-emission state should be cleared after handler error"
        );

        Ok(())
    }
}
