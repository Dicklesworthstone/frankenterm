//! Configuration for the gui portion of the terminal
#![allow(clippy::comparison_to_empty)]
#![allow(clippy::derivable_impls)]
#![allow(clippy::explicit_auto_deref)]
#![allow(clippy::field_reassign_with_default)]
#![allow(clippy::from_over_into)]
#![allow(clippy::large_const_arrays)]
#![allow(clippy::manual_contains)]
#![allow(clippy::manual_flatten)]
#![allow(clippy::manual_is_multiple_of)]
#![allow(clippy::manual_map)]
#![allow(clippy::missing_const_for_thread_local)]
#![allow(clippy::needless_borrow)]
#![allow(clippy::needless_borrows_for_generic_args)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::needless_question_mark)]
#![allow(clippy::new_without_default)]
#![allow(clippy::op_ref)]
#![allow(clippy::redundant_static_lifetimes)]
#![allow(clippy::single_match)]
#![allow(clippy::to_string_trait_impl)]
#![allow(clippy::useless_conversion)]
#![allow(clippy::wrong_self_convention)]

use std::convert::TryFrom as _;
use std::future::Future;

use anyhow::{anyhow, bail, Context, Error};
use flume::{unbounded, Receiver, Sender};
#[cfg(feature = "lua")]
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, UnknownFieldAction};
use frankenterm_dynamic::{ToDynamic, Value};
use frankenterm_term::UnicodeVersion;
use lazy_static::lazy_static;
use ordered_float::NotNan;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::fs::DirBuilder;
#[cfg(unix)]
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

#[cfg(all(feature = "lua", feature = "no-lua"))]
compile_error!(
    "Features `lua` and `no-lua` cannot both be enabled. Use `--no-default-features --features no-lua` for a no-Lua build."
);

#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(feature = "lua")]
use mlua::Lua;
#[cfg(not(feature = "lua"))]
#[derive(Debug)]
pub struct Lua;

mod background;
mod bell;
mod cell;
mod color;
mod config;
mod daemon;
pub mod detection;
mod exec_domain;
mod font;
mod frontend;
pub mod gui_socket;
pub mod keyassignment;
mod keys;
#[cfg(feature = "lua")]
pub mod lua;
pub mod meta;
pub mod migrate;
mod scheme_data;
mod serial;
mod ssh;
mod terminal;
mod tls;
pub(crate) mod toml_config;
mod units;
mod unix;
mod version;
pub(crate) mod wasm_config;
pub mod window;
mod wsl;

pub use crate::config::*;
pub use background::*;
pub use bell::*;
pub use cell::*;
pub use color::*;
pub use daemon::*;
pub use exec_domain::*;
pub use font::*;
pub use frontend::*;
pub use keys::*;
pub use serial::*;
pub use ssh::*;
pub use terminal::*;
pub use tls::*;
pub use units::*;
pub use unix::*;
pub use version::*;
pub use wsl::*;

type ErrorCallback = fn(&str);

lazy_static! {
    pub static ref HOME_DIR: PathBuf = dirs_next::home_dir().expect("can't find HOME dir");
    pub static ref CONFIG_DIRS: Vec<PathBuf> = config_dirs();
    pub static ref RUNTIME_DIR: PathBuf = compute_runtime_dir().unwrap();
    pub static ref DATA_DIR: PathBuf = compute_data_dir().unwrap();
    pub static ref CACHE_DIR: PathBuf = compute_cache_dir().unwrap();
    static ref CONFIG: Configuration = Configuration::new();
    static ref CONFIG_FILE_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
    static ref CONFIG_SKIP: AtomicBool = AtomicBool::new(false);
    static ref CONFIG_OVERRIDES: Mutex<Vec<(String, String)>> = Mutex::new(vec![]);
    static ref SHOW_ERROR: Mutex<Option<ErrorCallback>> =
        Mutex::new(Some(|e| log::error!("{}", e)));
    static ref LUA_PIPE: LuaPipe = LuaPipe::new();
    pub static ref COLOR_SCHEMES: HashMap<String, Palette> = build_default_schemes();
}

fn recover_mutex<'a, T>(mutex: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("recovering poisoned config mutex: {name}");
            mutex.clear_poison();
            poisoned.into_inner()
        }
    }
}

fn config_file_override_lock() -> MutexGuard<'static, Option<PathBuf>> {
    recover_mutex(&CONFIG_FILE_OVERRIDE, "CONFIG_FILE_OVERRIDE")
}

pub(crate) fn config_file_override_snapshot() -> Option<PathBuf> {
    config_file_override_lock().clone()
}

fn config_overrides_lock() -> MutexGuard<'static, Vec<(String, String)>> {
    recover_mutex(&CONFIG_OVERRIDES, "CONFIG_OVERRIDES")
}

pub(crate) fn config_overrides_snapshot() -> Vec<(String, String)> {
    config_overrides_lock().clone()
}

fn show_error_lock() -> MutexGuard<'static, Option<ErrorCallback>> {
    recover_mutex(&SHOW_ERROR, "SHOW_ERROR")
}

thread_local! {
    static LUA_CONFIG: RefCell<Option<LuaConfigState>> = RefCell::new(None);
}

fn toml_table_has_numeric_keys(t: &toml::value::Table) -> bool {
    t.keys().all(|k| k.parse::<isize>().is_ok())
}

fn json_object_has_numeric_keys(t: &serde_json::Map<String, serde_json::Value>) -> bool {
    t.keys().all(|k| k.parse::<isize>().is_ok())
}

fn toml_to_dynamic(value: &toml::Value) -> Value {
    match value {
        toml::Value::String(s) => s.to_dynamic(),
        toml::Value::Integer(n) => n.to_dynamic(),
        toml::Value::Float(n) => n.to_dynamic(),
        toml::Value::Boolean(b) => b.to_dynamic(),
        toml::Value::Datetime(d) => d.to_string().to_dynamic(),
        toml::Value::Array(a) => a
            .iter()
            .map(toml_to_dynamic)
            .collect::<Vec<_>>()
            .to_dynamic(),
        // Allow `colors.indexed` to be passed through with actual integer keys
        toml::Value::Table(t) if toml_table_has_numeric_keys(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.parse::<isize>().unwrap().to_dynamic(), toml_to_dynamic(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
        toml::Value::Table(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (Value::String(k.to_string()), toml_to_dynamic(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
    }
}

#[doc(hidden)]
pub fn merge_dynamic_overrides(base: &mut Value, overrides: &Value) {
    if let (Value::Object(base_map), Value::Object(override_map)) = (base, overrides) {
        for (key, value) in override_map.iter() {
            base_map.insert(key.clone(), value.clone());
        }
    }
}

#[doc(hidden)]
pub fn parse_toml_config_from_str(content: &str, overrides: &Value) -> anyhow::Result<Config> {
    toml_config::parse_toml_config_with_overrides(content, overrides)
        .map(|cfg| cfg.compute_extra_defaults(None))
}

fn json_to_dynamic(value: &serde_json::Value) -> Value {
    match value {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => b.to_dynamic(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.to_dynamic()
            } else if let Some(i) = n.as_u64() {
                i.to_dynamic()
            } else if let Some(f) = n.as_f64() {
                f.to_dynamic()
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => s.to_dynamic(),
        serde_json::Value::Array(a) => a
            .iter()
            .map(json_to_dynamic)
            .collect::<Vec<_>>()
            .to_dynamic(),
        // Allow `colors.indexed` to be passed through with actual integer keys
        serde_json::Value::Object(t) if json_object_has_numeric_keys(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (k.parse::<isize>().unwrap().to_dynamic(), json_to_dynamic(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
        serde_json::Value::Object(t) => Value::Object(
            t.iter()
                .map(|(k, v)| (Value::String(k.to_string()), json_to_dynamic(v)))
                .collect::<BTreeMap<_, _>>()
                .into(),
        ),
    }
}

pub fn build_default_schemes() -> HashMap<String, Palette> {
    let mut color_schemes = HashMap::new();
    for (scheme_name, data) in scheme_data::SCHEMES.iter() {
        let scheme_name = scheme_name.to_string();
        let scheme = ColorSchemeFile::from_toml_str(data).unwrap();
        color_schemes.insert(scheme_name, scheme.colors.clone());
        for alias in scheme.metadata.aliases {
            color_schemes.insert(alias, scheme.colors.clone());
        }
    }
    color_schemes
}

struct LuaPipe {
    sender: Sender<Lua>,
    receiver: Receiver<Lua>,
}
impl LuaPipe {
    pub fn new() -> Self {
        let (sender, receiver) = unbounded();
        Self { sender, receiver }
    }
}

/// The implementation is only slightly crazy...
/// `Lua` is Send but !Sync.
/// We take care to reference this only from the main thread of
/// the application.
/// We also need to take care to keep this `lua` alive if a long running
/// future is outstanding while a config reload happens.
/// We have to use `Rc` to manage its lifetime, but due to some issues
/// with rust's async lifetime tracking we need to indirectly schedule
/// some of the futures to avoid it thinking that the generated future
/// in the async block needs to be Send.
///
/// A further complication is that config reloading tends to happen in
/// a background filesystem watching thread.
///
/// The result of all these constraints is that the LuaPipe struct above
/// is used as a channel to transport newly loaded lua configs to the
/// main thread.
///
/// The main thread pops the loaded configs to obtain the latest one
/// and updates LuaConfigState
struct LuaConfigState {
    lua: Option<Rc<Lua>>,
}

impl LuaConfigState {
    /// Consume any lua contexts sent to us via the
    /// config loader until we end up with the most
    /// recent one being referenced by LUA_CONFIG.
    fn update_to_latest(&mut self) {
        while let Ok(lua) = LUA_PIPE.receiver.try_recv() {
            self.lua.replace(Rc::new(lua));
        }
    }

    /// Take a reference on the latest generation of the lua context
    fn get_lua(&self) -> Option<Rc<Lua>> {
        self.lua.as_ref().map(Rc::clone)
    }
}

pub fn designate_this_as_the_main_thread() {
    LUA_CONFIG.with(|lc| {
        let mut lc = lc.borrow_mut();
        if lc.is_none() {
            lc.replace(LuaConfigState { lua: None });
        }
    });
}

#[must_use = "Cancels the subscription when dropped"]
pub struct ConfigSubscription(usize);

impl Drop for ConfigSubscription {
    fn drop(&mut self) {
        CONFIG.unsub(self.0);
    }
}

pub fn subscribe_to_config_reload<F>(subscriber: F) -> ConfigSubscription
where
    F: Fn() -> bool + 'static + Send,
{
    ConfigSubscription(CONFIG.subscribe(subscriber))
}

/// Spawn a future that will run with an optional Lua state from the most
/// recently loaded lua configuration.
/// The `func` argument is passed the lua state and must return a Future.
///
/// This function MUST only be called from the main thread.
/// In exchange for the caller checking for this, the parameters to
/// this method are not required to be Send.
///
/// Calling this function from a secondary thread will panic.
/// You should use `with_lua_config` if you are triggering a
/// call from a secondary thread.
pub async fn with_lua_config_on_main_thread<F, RETF, RET>(func: F) -> anyhow::Result<RET>
where
    F: FnOnce(Option<Rc<Lua>>) -> RETF,
    RETF: Future<Output = anyhow::Result<RET>>,
{
    let lua = LUA_CONFIG.with(|lc| {
        let mut lc = lc.borrow_mut();
        let lc = lc.as_mut().expect(
            "with_lua_config_on_main_thread not called
             from main thread, use with_lua_config instead!",
        );
        lc.update_to_latest();
        lc.get_lua()
    });

    func(lua).await
}

pub fn run_immediate_with_lua_config<F, RET>(func: F) -> anyhow::Result<RET>
where
    F: FnOnce(Option<Rc<Lua>>) -> anyhow::Result<RET>,
{
    let lua = LUA_CONFIG.with(|lc| {
        let mut lc = lc.borrow_mut();
        let lc = lc.as_mut().expect(
            "with_lua_config_on_main_thread not called
             from main thread, use with_lua_config instead!",
        );
        lc.update_to_latest();
        lc.get_lua()
    });

    func(lua)
}

/// Spawn a future that will run with an optional Lua state from the most
/// recently loaded lua configuration.
/// The `func` argument is passed the lua state and must return a Future.
pub async fn with_lua_config<F, RETF, RET>(func: F) -> anyhow::Result<RET>
where
    F: Fn(Option<Rc<Lua>>) -> RETF,
    RETF: Future<Output = anyhow::Result<RET>> + Send + 'static,
    F: Send + 'static,
    RET: Send + 'static,
{
    let reservation = match promise::spawn::try_reserve_main_thread(
        promise::spawn::MainThreadServiceClass::Topology,
        4 * 1024,
    ) {
        promise::spawn::MainThreadReservationOutcome::Reserved(reservation) => reservation,
        rejected => {
            anyhow::bail!(
                "main-thread scheduler rejected Lua configuration operation before task construction: {rejected:?}"
            )
        }
    };
    reservation
        .spawn(async move { with_lua_config_on_main_thread(func).await })
        .into_task()
        .await
}

#[cfg(feature = "lua")]
fn default_config_with_overrides_applied() -> anyhow::Result<Config> {
    // Cause the default config to be re-evaluated with the overrides applied
    let lua = lua::make_lua_context(Path::new("override")).context("make_lua_context")?;
    let table = mlua::Value::Table(lua.create_table()?);
    let config = Config::apply_overrides_to(&lua, table).context("apply_overrides_to")?;

    let dyn_config = luahelper::lua_value_to_dynamic(config)?;

    let cfg: Config = Config::from_dynamic(
        &dyn_config,
        FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Deny,
            deprecated_fields: UnknownFieldAction::Warn,
        },
    )
    .context("Error converting lua value from overrides to Config struct")?;
    // Compute but discard the key bindings here so that we raise any
    // problems earlier than we use them.
    let _ = cfg.key_bindings();

    cfg.check_consistency().context("check_consistency")?;

    Ok(cfg)
}

#[cfg(not(feature = "lua"))]
fn default_config_with_overrides_applied() -> anyhow::Result<Config> {
    Ok(Config::default_config())
}

pub fn common_init(
    config_file: Option<&OsString>,
    overrides: &[(String, String)],
    skip_config: bool,
) -> anyhow::Result<()> {
    if let Some(config_file) = config_file {
        set_config_file_override(Path::new(config_file));
    } else if skip_config {
        CONFIG_SKIP.store(true, Ordering::Relaxed);
    }

    set_config_overrides(overrides).context("common_init: set_config_overrides")?;
    reload();
    Ok(())
}

pub fn assign_error_callback(cb: ErrorCallback) {
    let mut factory = show_error_lock();
    factory.replace(cb);
}

pub fn show_error(err: &str) {
    let factory = show_error_lock();
    if let Some(cb) = factory.as_ref() {
        cb(err)
    }
}

pub fn create_user_owned_dirs(p: &Path) -> anyhow::Result<()> {
    let mut builder = DirBuilder::new();
    builder.recursive(true);

    #[cfg(unix)]
    {
        builder.mode(0o700);
    }

    builder
        .create(p)
        .with_context(|| format!("creating private user directory {}", p.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};

        // Pin the final path without following a planted symbolic link.  All
        // callers use this helper for user-private state, so an older 0755
        // directory is tightened through the opened directory descriptor
        // before any state file is created inside it.
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
        let directory = options
            .open(p)
            .with_context(|| format!("opening private user directory {}", p.display()))?;
        let before = directory
            .metadata()
            .with_context(|| format!("inspecting private user directory {}", p.display()))?;
        if !before.is_dir() {
            bail!("private user path is not a directory: {}", p.display());
        }
        let expected_uid = nix::unistd::geteuid().as_raw();
        if before.uid() != expected_uid {
            bail!(
                "private user directory is owned by uid {}, expected uid {expected_uid}: {}",
                before.uid(),
                p.display()
            );
        }
        if before.permissions().mode() & 0o7777 != 0o700 {
            directory
                .set_permissions(std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("tightening private user directory {}", p.display()))?;
        }

        let after = directory
            .metadata()
            .with_context(|| format!("re-inspecting private user directory {}", p.display()))?;
        let path_after = std::fs::symlink_metadata(p)
            .with_context(|| format!("revalidating private user directory name {}", p.display()))?;
        if path_after.file_type().is_symlink()
            || !path_after.is_dir()
            || path_after.dev() != after.dev()
            || path_after.ino() != after.ino()
        {
            bail!(
                "private user directory changed identity during validation: {}",
                p.display()
            );
        }
        if after.permissions().mode() & 0o7777 != 0o700 {
            bail!(
                "private user directory permissions are not 0700: {}",
                p.display()
            );
        }
        if after.uid() != expected_uid || path_after.uid() != expected_uid {
            bail!(
                "private user directory ownership changed during validation: {}",
                p.display()
            );
        }
    }

    #[cfg(not(unix))]
    {
        let metadata = std::fs::symlink_metadata(p)
            .with_context(|| format!("inspecting private user directory {}", p.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "private user path is not a direct directory: {}",
                p.display()
            );
        }
    }

    Ok(())
}

const MAX_USER_OWNED_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[cfg(unix)]
fn validate_user_owned_file(
    path: &Path,
    file: &std::fs::File,
    normalize_mode: bool,
) -> anyhow::Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("private user file has no parent: {}", path.display()))?;
    let directory = std::fs::symlink_metadata(parent).with_context(|| {
        format!(
            "inspecting private user file directory {}",
            parent.display()
        )
    })?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspecting open private user file {}", path.display()))?;
    let named = std::fs::symlink_metadata(path)
        .with_context(|| format!("revalidating private user file name {}", path.display()))?;
    let expected_uid = nix::unistd::geteuid().as_raw();
    if !directory.is_dir()
        || directory.file_type().is_symlink()
        || directory.uid() != expected_uid
        || directory.permissions().mode() & 0o7777 != 0o700
        || !opened.is_file()
        || !named.is_file()
        || named.file_type().is_symlink()
        || opened.uid() != expected_uid
        || named.uid() != expected_uid
        || opened.dev() != directory.dev()
        || opened.dev() != named.dev()
        || opened.ino() != named.ino()
        || opened.nlink() != 1
        || named.nlink() != 1
    {
        bail!(
            "private user file is not a direct, single-link authority owned by uid {expected_uid}: {}",
            path.display()
        );
    }

    if normalize_mode && opened.permissions().mode() & 0o7777 != 0o600 {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("tightening private user file {}", path.display()))?;
    }

    let opened_after = file
        .metadata()
        .with_context(|| format!("re-inspecting private user file {}", path.display()))?;
    let named_after = std::fs::symlink_metadata(path)
        .with_context(|| format!("revalidating private user file name {}", path.display()))?;
    if !opened_after.is_file()
        || !named_after.is_file()
        || named_after.file_type().is_symlink()
        || opened_after.uid() != expected_uid
        || named_after.uid() != expected_uid
        || opened_after.dev() != directory.dev()
        || opened_after.dev() != named_after.dev()
        || opened_after.ino() != named_after.ino()
        || opened_after.nlink() != 1
        || named_after.nlink() != 1
        || opened_after.permissions().mode() & 0o7777 != 0o600
        || named_after.permissions().mode() & 0o7777 != 0o600
    {
        bail!(
            "private user file identity or permissions changed during validation: {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_user_owned_file(
    path: &Path,
    file: &std::fs::File,
    _normalize_mode: bool,
) -> anyhow::Result<()> {
    let opened = file
        .metadata()
        .with_context(|| format!("inspecting open private user file {}", path.display()))?;
    let named = std::fs::symlink_metadata(path)
        .with_context(|| format!("revalidating private user file name {}", path.display()))?;
    if !opened.is_file() || !named.is_file() || named.file_type().is_symlink() {
        bail!(
            "private user file is not a direct regular file: {}",
            path.display()
        );
    }
    Ok(())
}

fn open_user_owned_file_for_write(path: &Path, append: bool) -> anyhow::Result<std::fs::File> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("private user file has no parent: {}", path.display()))?;
    create_user_owned_dirs(parent)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true).append(append);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .with_context(|| format!("opening private user file {}", path.display()))?;
    fs2::FileExt::lock_exclusive(&file)
        .with_context(|| format!("locking private user file {}", path.display()))?;
    validate_user_owned_file(path, &file, true)?;
    Ok(file)
}

/// Read one bounded file from a private, direct, single-link user
/// authority. The pathname and opened descriptor must continue to name the
/// same file for the complete read.
pub fn read_user_owned_file(path: &Path) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("opening private user file {}", path.display()))?;
    fs2::FileExt::lock_shared(&file)
        .with_context(|| format!("locking private user file {}", path.display()))?;
    #[cfg(unix)]
    let requires_mode_normalization = {
        use std::os::unix::fs::PermissionsExt as _;

        file.metadata()
            .with_context(|| format!("inspecting private user file {}", path.display()))?
            .permissions()
            .mode()
            & 0o7777
            != 0o600
    };
    #[cfg(not(unix))]
    let requires_mode_normalization = false;
    if requires_mode_normalization {
        fs2::FileExt::unlock(&file)
            .with_context(|| format!("upgrading private user file lock {}", path.display()))?;
        fs2::FileExt::lock_exclusive(&file)
            .with_context(|| format!("locking private user file {} exclusively", path.display()))?;
    }
    validate_user_owned_file(path, &file, requires_mode_normalization)?;
    let maximum = MAX_USER_OWNED_FILE_BYTES;
    let mut bytes = Vec::new();
    (&mut file)
        .take(maximum.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading private user file {}", path.display()))?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > maximum {
        bail!(
            "private user file exceeds the {maximum}-byte bound: {}",
            path.display()
        );
    }
    validate_user_owned_file(path, &file, false)?;
    Ok(bytes)
}

/// Replace one private user file after validating its direct, single-link
/// descriptor authority. Existing owned files with older permissive modes are
/// tightened through the opened descriptor before truncation.
pub fn write_user_owned_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::{Seek as _, Write as _};

    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_USER_OWNED_FILE_BYTES {
        bail!(
            "private user file replacement exceeds the {}-byte bound: {}",
            MAX_USER_OWNED_FILE_BYTES,
            path.display()
        );
    }
    let mut file = open_user_owned_file_for_write(path, false)?;
    file.set_len(0)
        .with_context(|| format!("truncating private user file {}", path.display()))?;
    file.rewind()
        .with_context(|| format!("seeking private user file {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("writing private user file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing private user file {}", path.display()))?;
    validate_user_owned_file(path, &file, false)
}

/// Append to one private user file without following aliases or accepting a
/// multi-link authority.
pub fn append_user_owned_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut file = open_user_owned_file_for_write(path, true)?;
    let current = file
        .metadata()
        .with_context(|| format!("inspecting private user file {}", path.display()))?
        .len();
    let addition = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if current
        .checked_add(addition)
        .is_none_or(|total| total > MAX_USER_OWNED_FILE_BYTES)
    {
        bail!(
            "private user file append exceeds the {}-byte bound: {}",
            MAX_USER_OWNED_FILE_BYTES,
            path.display()
        );
    }
    file.write_all(bytes)
        .with_context(|| format!("appending private user file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("syncing private user file {}", path.display()))?;
    validate_user_owned_file(path, &file, false)
}

fn xdg_config_home() -> PathBuf {
    match std::env::var_os("XDG_CONFIG_HOME").map(|s| PathBuf::from(s).join("wezterm")) {
        Some(p) => p,
        None => HOME_DIR.join(".config").join("wezterm"),
    }
}

pub(crate) fn frankenterm_config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    match std::env::var_os("XDG_CONFIG_HOME").map(|s| PathBuf::from(s).join("frankenterm")) {
        Some(p) => dirs.push(p),
        None => dirs.push(HOME_DIR.join(".config").join("frankenterm")),
    }

    #[cfg(unix)]
    if let Some(d) = std::env::var_os("XDG_CONFIG_DIRS") {
        dirs.extend(std::env::split_paths(&d).map(|s| PathBuf::from(s).join("frankenterm")));
    }

    dirs
}

fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    // FrankenTerm-namespaced config locations take precedence so a side-by-side
    // wezterm install on the same machine doesn't have its config silently
    // co-opted by FrankenTerm. A user who wants the two clients to share config
    // can still place files under ~/.config/wezterm/ — that path is searched
    // below as a fallback. `frankenterm_config_dirs()` already honors
    // `XDG_CONFIG_DIRS` for FrankenTerm-namespaced system dirs, so we only need
    // to add the wezterm-namespaced `XDG_CONFIG_DIRS` entries explicitly here.
    dirs.extend(frankenterm_config_dirs());
    dirs.push(xdg_config_home());

    #[cfg(unix)]
    if let Some(d) = std::env::var_os("XDG_CONFIG_DIRS") {
        dirs.extend(std::env::split_paths(&d).map(|s| PathBuf::from(s).join("wezterm")));
    }

    dirs
}

pub fn set_config_file_override(path: &Path) {
    config_file_override_lock().replace(path.to_path_buf());
}

pub fn set_config_overrides(items: &[(String, String)]) -> anyhow::Result<()> {
    *config_overrides_lock() = items.to_vec();

    let _ = default_config_with_overrides_applied()?;
    Ok(())
}

pub fn is_config_overridden() -> bool {
    CONFIG_SKIP.load(Ordering::Relaxed)
        || !config_overrides_lock().is_empty()
        || config_file_override_lock().is_some()
}

/// Discard the current configuration and replace it with
/// the default configuration
pub fn use_default_configuration() {
    CONFIG.use_defaults();
}

/// Use a config that doesn't depend on the user's
/// environment and is suitable for unit testing
pub fn use_test_configuration() {
    CONFIG.use_test();
}

pub fn use_this_configuration(config: Config) {
    CONFIG.use_this_config(config);
}

/// Returns a handle to the current configuration
pub fn configuration() -> ConfigHandle {
    CONFIG.get()
}

/// Returns a version of the config (loaded from the config file)
/// with some field overridden based on the supplied overrides object.
pub fn overridden_config(overrides: &frankenterm_dynamic::Value) -> Result<ConfigHandle, Error> {
    CONFIG.overridden(overrides)
}

pub fn reload() {
    CONFIG.reload();
}

/// If there was an error loading the preferred configuration,
/// return it, otherwise return the current configuration
pub fn configuration_result() -> Result<ConfigHandle, Error> {
    if let Some(error) = CONFIG.get_error() {
        bail!("{}", error);
    }
    Ok(CONFIG.get())
}

/// Returns the combined set of errors + warnings encountered
/// while loading the preferred configuration
pub fn configuration_warnings_and_errors() -> Vec<String> {
    CONFIG.get_warnings_and_errors()
}

struct ConfigInner {
    config: Arc<Config>,
    error: Option<String>,
    warnings: Vec<String>,
    generation: usize,
    watcher: Option<notify::RecommendedWatcher>,
    subscribers: HashMap<usize, Box<dyn Fn() -> bool + Send>>,
}

#[track_caller]
fn next_unique_config_subscription_id(counter: &AtomicUsize) -> usize {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        let Some(next) = current.checked_add(1) else {
            panic!(
                "config subscription id space exhausted; refusing to replace an existing subscriber"
            );
        };
        match counter.compare_exchange_weak(current, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return current,
            Err(observed) => current = observed,
        }
    }
}

#[track_caller]
fn next_config_generation(current: usize) -> usize {
    current.checked_add(1).unwrap_or_else(|| {
        panic!(
            "config generation space exhausted; refusing to publish a configuration under a reused generation"
        )
    })
}

impl ConfigInner {
    fn new() -> Self {
        Self {
            config: Arc::new(Config::default_config()),
            error: None,
            warnings: vec![],
            generation: 0,
            watcher: None,
            subscribers: HashMap::new(),
        }
    }

    fn subscribe<F>(&mut self, subscriber: F) -> usize
    where
        F: Fn() -> bool + 'static + Send,
    {
        static SUB_ID: AtomicUsize = AtomicUsize::new(0);
        let sub_id = next_unique_config_subscription_id(&SUB_ID);
        self.subscribers.insert(sub_id, Box::new(subscriber));
        sub_id
    }

    fn unsub(&mut self, sub_id: usize) {
        self.subscribers.remove(&sub_id);
    }

    fn notify(&mut self) {
        self.subscribers.retain(|_, notify| notify());
    }

    fn install_config(&mut self, config: Config, warnings: Option<Vec<String>>) {
        let generation = next_config_generation(self.generation);
        if let Some(warnings) = warnings {
            self.warnings = warnings;
        }
        self.config = Arc::new(config);
        self.error.take();
        self.generation = generation;
    }

    fn watch_path(&mut self, path: PathBuf) {
        if self.watcher.is_none() {
            let (tx, rx) = std::sync::mpsc::channel();
            const DELAY: Duration = Duration::from_millis(200);
            // `notify::recommended_watcher` can fail on platforms where
            // filesystem change notifications are unavailable — CI
            // sandboxes without inotify, minimal Linux containers, WSL1
            // edge cases, and the stripped-down mount namespaces some
            // Docker / Kubernetes images ship with. The earlier
            // revision called `.unwrap()` on this Result, which made a
            // missing kernel feature fatal to the whole config
            // subsystem: every caller of `reload()` / `subscribe_to_
            // config_reload()` / the `common_init` path would panic the
            // process. Fall back to no-watcher mode instead — the
            // process can still reload via SIGHUP or an explicit
            // `config::reload()` call, which is exactly how mux-server
            // and headless deployments already trigger reloads.
            let watcher = match notify::recommended_watcher(tx) {
                Ok(w) => w,
                Err(err) => {
                    log::warn!(
                        "unable to install filesystem watcher for config reload \
                         (path {:?}): {err:#}; running without fs-watch, \
                         explicit reload()/SIGHUP still works",
                        path
                    );
                    return;
                }
            };
            let watched_path = path.clone();

            let event_thread = std::thread::Builder::new()
                .name("config-file-watcher".to_string())
                .spawn(move || {
                    // block until we get an event
                    use notify::EventKind;

                    fn extract_path(event: notify::Event) -> Vec<PathBuf> {
                        match event.kind {
                            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_) => {
                                event.paths
                            }
                            _ => vec![],
                        }
                    }

                    while let Ok(event) = rx.recv() {
                        log::debug!("event:{:?}", event);
                        match event {
                            Ok(event) => {
                                let mut paths = extract_path(event);
                                if !paths.is_empty() {
                                    // Grace period to allow events to settle
                                    std::thread::sleep(DELAY);
                                    // Drain any other immediately ready events
                                    while let Ok(Ok(event)) = rx.try_recv() {
                                        paths.append(&mut extract_path(event));
                                    }
                                    paths.sort();
                                    paths.dedup();
                                    log::debug!("paths {:?} changed, reload config", watched_path);
                                    reload();
                                }
                            }
                            Err(_) => {
                                reload();
                            }
                        }
                    }
                });
            if let Err(err) = event_thread {
                log::warn!(
                    "unable to spawn config filesystem watcher thread \
                     (path {:?}): {err:#}; running without fs-watch, \
                     explicit reload()/SIGHUP still works",
                    path
                );
                return;
            }
            self.watcher.replace(watcher);
        }
        if let Some(watcher) = self.watcher.as_mut() {
            use notify::Watcher;
            watcher
                .watch(&path, notify::RecursiveMode::NonRecursive)
                .ok();
        }
    }

    #[cfg(feature = "lua")]
    fn accumulate_watch_paths(lua: &Lua, watch_paths: &mut Vec<PathBuf>) {
        if let Ok(mlua::Value::Table(tbl)) = lua.named_registry_value("wezterm-watch-paths") {
            for path in tbl.sequence_values::<String>() {
                if let Ok(path) = path {
                    watch_paths.push(PathBuf::from(path));
                }
            }
        }
    }

    #[cfg(not(feature = "lua"))]
    fn accumulate_watch_paths(_lua: &Lua, _watch_paths: &mut Vec<PathBuf>) {}

    /// Attempt to load the user's configuration.
    /// On success, clear any error and replace the current
    /// configuration.
    /// On failure, retain the existing configuration but
    /// replace any captured error message.
    fn reload(&mut self) {
        let LoadedConfig {
            config,
            file_name,
            lua,
            warnings,
        } = Config::load();

        // Before we process the success/failure, extract and update
        // any paths that we should be watching
        let mut watch_paths = vec![];
        if let Some(path) = file_name {
            // Let's also watch the parent directory for folks that do
            // things with symlinks:
            if let Some(parent) = path.parent() {
                // But avoid watching the home dir itself, so that we
                // don't keep reloading every time something in the
                // home dir changes!
                // <https://github.com/wezterm/wezterm/issues/1895>
                if parent != &*HOME_DIR {
                    watch_paths.push(parent.to_path_buf());
                }
            }
            watch_paths.push(path);
        }
        if let Some(lua) = &lua {
            ConfigInner::accumulate_watch_paths(lua, &mut watch_paths);
        }

        match config {
            Ok(config) => {
                self.install_config(config, Some(warnings));

                // If we loaded a user config, publish this latest version of
                // the lua state to the LUA_PIPE.  This allows a subsequent
                // call to `with_lua_config` to reference this lua context
                // even though we are (probably) resolving this from a background
                // reloading thread.
                if let Some(lua) = lua {
                    LUA_PIPE.sender.try_send(lua).ok();
                }
                log::debug!("Reloaded configuration! generation={}", self.generation);
            }
            Err(err) => {
                self.warnings = warnings;
                let err = format!("{:#}", err);
                if self.generation > 0 {
                    // Only generate the message for an actual reload
                    show_error(&err);
                }
                self.error.replace(err);
            }
        }

        self.notify();
        if self.config.automatically_reload_config {
            for path in watch_paths {
                self.watch_path(path);
            }
        }
    }

    /// Discard the current configuration and any recorded
    /// error message; replace them with the default
    /// configuration
    fn use_defaults(&mut self) {
        self.install_config(Config::default_config(), None);
    }

    fn use_this_config(&mut self, cfg: Config) {
        self.install_config(cfg, None);
    }

    fn overridden(
        &mut self,
        overrides: &frankenterm_dynamic::Value,
    ) -> Result<ConfigHandle, Error> {
        let config = Config::load_with_overrides(overrides);
        Ok(ConfigHandle {
            config: Arc::new(config.config?),
            generation: self.generation,
        })
    }

    fn use_test(&mut self) {
        let mut config = Config::default_config();
        config.font_locator = FontLocatorSelection::ConfigDirsOnly;
        let exe_name = std::env::current_exe().unwrap();
        let exe_dir = exe_name.parent().unwrap();
        config.font_dirs.push(exe_dir.join("../../../assets/fonts"));
        // If we're building for a specific target, the dir
        // level is one deeper.
        #[cfg(target_os = "macos")]
        config
            .font_dirs
            .push(exe_dir.join("../../../../assets/fonts"));
        // Specify the same DPI used on non-mac systems so
        // that we have consistent values regardless of the
        // operating system that we're running tests on
        config.dpi.replace(96.0);
        self.install_config(config, None);
    }
}

pub struct Configuration {
    inner: Mutex<ConfigInner>,
}

impl Configuration {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ConfigInner::new()),
        }
    }

    fn inner_lock(&self) -> MutexGuard<'_, ConfigInner> {
        recover_mutex(&self.inner, "Configuration::inner")
    }

    /// Returns the effective configuration.
    pub fn get(&self) -> ConfigHandle {
        let inner = self.inner_lock();
        ConfigHandle {
            config: Arc::clone(&inner.config),
            generation: inner.generation,
        }
    }

    /// Subscribe to config reload events
    fn subscribe<F>(&self, subscriber: F) -> usize
    where
        F: Fn() -> bool + 'static + Send,
    {
        let mut inner = self.inner_lock();
        inner.subscribe(subscriber)
    }

    fn unsub(&self, sub_id: usize) {
        let mut inner = self.inner_lock();
        inner.unsub(sub_id);
    }

    /// Reset the configuration to defaults
    pub fn use_defaults(&self) {
        let mut inner = self.inner_lock();
        inner.use_defaults();
    }

    fn use_this_config(&self, cfg: Config) {
        let mut inner = self.inner_lock();
        inner.use_this_config(cfg);
    }

    fn overridden(&self, overrides: &frankenterm_dynamic::Value) -> Result<ConfigHandle, Error> {
        let mut inner = self.inner_lock();
        inner.overridden(overrides)
    }

    /// Use a config that doesn't depend on the user's
    /// environment and is suitable for unit testing
    pub fn use_test(&self) {
        let mut inner = self.inner_lock();
        inner.use_test();
    }

    /// Reload the configuration
    pub fn reload(&self) {
        let mut inner = self.inner_lock();
        inner.reload();
    }

    /// Returns a copy of any captured error message.
    /// The error message is not cleared.
    pub fn get_error(&self) -> Option<String> {
        let inner = self.inner_lock();
        inner.error.as_ref().cloned()
    }

    pub fn get_warnings_and_errors(&self) -> Vec<String> {
        let mut result = vec![];
        let inner = self.inner_lock();
        if let Some(error) = &inner.error {
            result.push(error.clone());
        }
        for warning in &inner.warnings {
            result.push(warning.clone());
        }
        result
    }

    /// Returns any captured error message, and clears
    /// it from the config state.
    #[allow(dead_code)]
    pub fn clear_error(&self) -> Option<String> {
        let mut inner = self.inner_lock();
        inner.error.take()
    }
}

#[derive(Clone, Debug)]
pub struct ConfigHandle {
    config: Arc<Config>,
    generation: usize,
}

impl ConfigHandle {
    /// Returns the generation number for the configuration,
    /// allowing consuming code to know whether the config
    /// has been reloading since they last derived some
    /// information from the configuration
    pub fn generation(&self) -> usize {
        self.generation
    }

    pub fn default_config() -> Self {
        Self {
            config: Arc::new(Config::default_config()),
            generation: 0,
        }
    }

    pub fn with_resolved_palette(&self, resolved_palette: crate::Palette) -> Self {
        let mut config = (*self.config).clone();
        config.resolved_palette = resolved_palette;
        Self {
            config: Arc::new(config),
            generation: self.generation,
        }
    }

    pub fn unicode_version(&self) -> UnicodeVersion {
        UnicodeVersion {
            version: self.config.unicode_version,
            ambiguous_are_wide: self.config.treat_east_asian_ambiguous_width_as_wide,
            cell_widths: CellWidth::compile_to_map(self.config.cell_widths.clone()),
        }
    }
}

impl std::ops::Deref for ConfigHandle {
    type Target = Config;
    fn deref(&self) -> &Config {
        &*self.config
    }
}

pub struct LoadedConfig {
    pub config: anyhow::Result<Config>,
    pub file_name: Option<PathBuf>,
    pub lua: Option<Lua>,
    pub warnings: Vec<String>,
}

fn default_one_point_oh_f64() -> f64 {
    1.0
}

fn default_one_point_oh() -> f32 {
    1.0
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::AssertUnwindSafe;

    #[test]
    fn config_subscription_allocator_uses_the_last_unreserved_identity_once() {
        let counter = AtomicUsize::new(usize::MAX - 1);

        assert_eq!(next_unique_config_subscription_id(&counter), usize::MAX - 1);
        assert_eq!(counter.load(Ordering::Relaxed), usize::MAX);
    }

    #[test]
    #[should_panic(
        expected = "config subscription id space exhausted; refusing to replace an existing subscriber"
    )]
    fn config_subscription_allocator_fails_closed_at_exhaustion() {
        let counter = AtomicUsize::new(usize::MAX);
        let _ = next_unique_config_subscription_id(&counter);
    }

    #[test]
    fn config_install_publishes_the_terminal_generation_once() {
        let mut inner = ConfigInner::new();
        inner.generation = usize::MAX - 1;
        inner.error = Some("stale error".to_string());

        inner.install_config(
            Config::default_config(),
            Some(vec!["current warning".to_string()]),
        );

        assert_eq!(inner.generation, usize::MAX);
        assert!(inner.error.is_none());
        assert_eq!(inner.warnings, ["current warning"]);
    }

    #[test]
    fn config_generation_exhaustion_rejects_install_transactionally() {
        let mut inner = ConfigInner::new();
        inner.generation = usize::MAX;
        inner.error = Some("retained error".to_string());
        inner.warnings = vec!["retained warning".to_string()];
        let retained_config = Arc::clone(&inner.config);

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            inner.install_config(
                Config::default_config(),
                Some(vec!["must not publish".to_string()]),
            );
        }));

        assert!(result.is_err());
        assert_eq!(inner.generation, usize::MAX);
        assert!(Arc::ptr_eq(&inner.config, &retained_config));
        assert_eq!(inner.error.as_deref(), Some("retained error"));
        assert_eq!(inner.warnings, ["retained warning"]);
    }

    #[test]
    fn toml_table_numeric_keys_with_all_numeric() {
        let mut table = toml::value::Table::new();
        table.insert("0".to_string(), toml::Value::Integer(10));
        table.insert("1".to_string(), toml::Value::Integer(20));
        table.insert("42".to_string(), toml::Value::Integer(30));
        assert!(toml_table_has_numeric_keys(&table));
    }

    #[test]
    fn toml_table_numeric_keys_with_mixed_keys() {
        let mut table = toml::value::Table::new();
        table.insert("0".to_string(), toml::Value::Integer(10));
        table.insert("name".to_string(), toml::Value::String("hello".into()));
        assert!(!toml_table_has_numeric_keys(&table));
    }

    #[test]
    fn toml_table_numeric_keys_empty_is_true() {
        let table = toml::value::Table::new();
        assert!(
            toml_table_has_numeric_keys(&table),
            "empty table should vacuously satisfy all-numeric"
        );
    }

    #[test]
    fn toml_table_numeric_keys_negative_keys_are_numeric() {
        let mut table = toml::value::Table::new();
        table.insert("-1".to_string(), toml::Value::Integer(10));
        table.insert("-99".to_string(), toml::Value::Integer(20));
        assert!(toml_table_has_numeric_keys(&table));
    }

    #[test]
    fn global_config_locks_recover_after_poison() {
        let _env = test_env_lock();

        let file_poison = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = CONFIG_FILE_OVERRIDE.lock().unwrap();
            panic!("poison config file override");
        }));
        assert!(file_poison.is_err());
        set_config_file_override(Path::new("poisoned.toml"));
        assert_eq!(
            config_file_override_snapshot().as_deref(),
            Some(Path::new("poisoned.toml"))
        );
        *config_file_override_lock() = None;

        let overrides_poison = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = CONFIG_OVERRIDES.lock().unwrap();
            panic!("poison config overrides");
        }));
        assert!(overrides_poison.is_err());
        set_config_overrides(&[]).expect("poisoned overrides lock should recover");
        assert!(!is_config_overridden());

        static ERROR_CALLBACK_CALLS: AtomicUsize = AtomicUsize::new(0);
        fn count_error_callback(_: &str) {
            ERROR_CALLBACK_CALLS.fetch_add(1, Ordering::SeqCst);
        }
        fn default_error_callback(err: &str) {
            log::error!("{}", err);
        }

        ERROR_CALLBACK_CALLS.store(0, Ordering::SeqCst);
        let show_error_poison = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = SHOW_ERROR.lock().unwrap();
            panic!("poison error callback");
        }));
        assert!(show_error_poison.is_err());
        assign_error_callback(count_error_callback);
        show_error("after poison");
        assert_eq!(ERROR_CALLBACK_CALLS.load(Ordering::SeqCst), 1);
        *show_error_lock() = Some(default_error_callback);

        let config_poison = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = CONFIG.inner.lock().unwrap();
            panic!("poison configuration state");
        }));
        assert!(config_poison.is_err());
        let _ = CONFIG.get();
    }

    #[test]
    fn json_object_numeric_keys_with_all_numeric() {
        let mut map = serde_json::Map::new();
        map.insert("0".to_string(), serde_json::Value::Number(10.into()));
        map.insert("255".to_string(), serde_json::Value::Number(20.into()));
        assert!(json_object_has_numeric_keys(&map));
    }

    #[test]
    fn json_object_numeric_keys_with_non_numeric() {
        let mut map = serde_json::Map::new();
        map.insert("color".to_string(), serde_json::Value::String("red".into()));
        assert!(!json_object_has_numeric_keys(&map));
    }

    #[test]
    fn toml_to_dynamic_string() {
        let val = toml::Value::String("hello".into());
        let dyn_val = toml_to_dynamic(&val);
        assert_eq!(dyn_val, Value::String("hello".to_string()));
    }

    #[test]
    fn toml_to_dynamic_integer() {
        let val = toml::Value::Integer(42);
        let dyn_val = toml_to_dynamic(&val);
        assert_eq!(dyn_val, 42i64.to_dynamic());
    }

    #[test]
    fn toml_to_dynamic_float() {
        let val = toml::Value::Float(3.5);
        let dyn_val = toml_to_dynamic(&val);
        assert_eq!(dyn_val, 3.5f64.to_dynamic());
    }

    #[test]
    fn toml_to_dynamic_boolean() {
        let t = toml::Value::Boolean(true);
        let f = toml::Value::Boolean(false);
        assert_eq!(toml_to_dynamic(&t), true.to_dynamic());
        assert_eq!(toml_to_dynamic(&f), false.to_dynamic());
    }

    #[test]
    fn toml_to_dynamic_array() {
        let arr = toml::Value::Array(vec![
            toml::Value::Integer(1),
            toml::Value::Integer(2),
            toml::Value::Integer(3),
        ]);
        let dyn_val = toml_to_dynamic(&arr);
        let expected = vec![1i64.to_dynamic(), 2i64.to_dynamic(), 3i64.to_dynamic()].to_dynamic();
        assert_eq!(dyn_val, expected);
    }

    #[test]
    fn toml_to_dynamic_table_string_keys() {
        let mut table = toml::value::Table::new();
        table.insert("name".to_string(), toml::Value::String("test".into()));
        let val = toml::Value::Table(table);
        let dyn_val = toml_to_dynamic(&val);

        match dyn_val {
            Value::Object(obj) => {
                let key = Value::String("name".to_string());
                assert_eq!(obj.get(&key), Some(&Value::String("test".to_string())));
            }
            other => panic!("expected Object, got {:?}", other),
        }
    }

    #[test]
    fn toml_to_dynamic_table_numeric_keys() {
        let mut table = toml::value::Table::new();
        table.insert("0".to_string(), toml::Value::String("red".into()));
        table.insert("1".to_string(), toml::Value::String("green".into()));
        let val = toml::Value::Table(table);
        let dyn_val = toml_to_dynamic(&val);

        match dyn_val {
            Value::Object(obj) => {
                let key_0 = 0isize.to_dynamic();
                assert_eq!(obj.get(&key_0), Some(&Value::String("red".to_string())));
            }
            other => panic!("expected Object, got {:?}", other),
        }
    }

    #[test]
    fn json_to_dynamic_null() {
        let val = serde_json::Value::Null;
        assert_eq!(json_to_dynamic(&val), Value::Null);
    }

    #[test]
    fn json_to_dynamic_bool() {
        assert_eq!(
            json_to_dynamic(&serde_json::Value::Bool(true)),
            true.to_dynamic()
        );
    }

    #[test]
    fn json_to_dynamic_integer() {
        let val = serde_json::Value::Number(serde_json::Number::from(99));
        assert_eq!(json_to_dynamic(&val), 99i64.to_dynamic());
    }

    #[test]
    fn json_to_dynamic_float() {
        let val: serde_json::Value = serde_json::from_str("2.5").unwrap();
        assert_eq!(json_to_dynamic(&val), 2.5f64.to_dynamic());
    }

    #[test]
    fn json_to_dynamic_string() {
        let val = serde_json::Value::String("world".into());
        assert_eq!(json_to_dynamic(&val), Value::String("world".to_string()));
    }

    #[test]
    fn json_to_dynamic_array() {
        let val: serde_json::Value = serde_json::json!([1, "two", true]);
        let dyn_val = json_to_dynamic(&val);
        match dyn_val {
            Value::Array(arr) => assert_eq!(arr.len(), 3),
            other => panic!("expected Array, got {:?}", other),
        }
    }

    #[test]
    fn json_to_dynamic_object_numeric_keys() {
        let val: serde_json::Value = serde_json::json!({"0": "red", "1": "blue"});
        let dyn_val = json_to_dynamic(&val);
        match dyn_val {
            Value::Object(obj) => {
                let key_0 = 0isize.to_dynamic();
                assert_eq!(obj.get(&key_0), Some(&Value::String("red".to_string())));
            }
            other => panic!("expected Object with numeric keys, got {:?}", other),
        }
    }

    #[test]
    fn json_to_dynamic_object_string_keys() {
        let val: serde_json::Value = serde_json::json!({"color": "red"});
        let dyn_val = json_to_dynamic(&val);
        match dyn_val {
            Value::Object(obj) => {
                let key = Value::String("color".to_string());
                assert_eq!(obj.get(&key), Some(&Value::String("red".to_string())));
            }
            other => panic!("expected Object with string keys, got {:?}", other),
        }
    }

    #[test]
    fn build_default_schemes_not_empty() {
        let schemes = build_default_schemes();
        assert!(
            schemes.len() > 100,
            "should have many built-in color schemes, got {}",
            schemes.len()
        );
    }

    #[test]
    fn build_default_schemes_contains_known_scheme() {
        let schemes = build_default_schemes();
        assert!(
            schemes.contains_key("Solarized (dark) (terminal.sexy)"),
            "should contain a Solarized scheme"
        );
    }

    #[test]
    fn config_handle_default_config_has_generation_zero() {
        let handle = ConfigHandle::default_config();
        assert_eq!(handle.generation(), 0);
    }

    #[test]
    fn config_handle_default_config_has_positive_font_size() {
        let handle = ConfigHandle::default_config();
        assert!(
            handle.font_size > 0.0,
            "default font size should be positive"
        );
    }

    #[test]
    fn test_configuration_uses_defaults() {
        use_test_configuration();
        let handle = configuration();
        assert!(
            handle.font_size > 0.0,
            "test configuration should have a valid font size"
        );
    }

    #[test]
    fn default_helpers_return_expected_values() {
        assert_eq!(default_one_point_oh_f64(), 1.0);
        assert_eq!(default_one_point_oh(), 1.0f32);
        assert!(default_true());
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set<V>(key: &'static str, value: V) -> Self
        where
            V: AsRef<std::ffi::OsStr>,
        {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }

        fn unset(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn frankenterm_config_dirs_use_native_xdg_home() {
        let _lock = test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _xdg_home = EnvVarGuard::set("XDG_CONFIG_HOME", dir.path());
        let _xdg_dirs = EnvVarGuard::unset("XDG_CONFIG_DIRS");

        assert_eq!(
            frankenterm_config_dirs(),
            vec![dir.path().join("frankenterm")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn frankenterm_config_dirs_include_native_xdg_dirs() {
        let _lock = test_env_lock();
        let home = tempfile::tempdir().unwrap();
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let joined = std::env::join_paths([first.path(), second.path()]).unwrap();
        let _xdg_home = EnvVarGuard::set("XDG_CONFIG_HOME", home.path());
        let _xdg_dirs = EnvVarGuard::set("XDG_CONFIG_DIRS", &joined);

        assert_eq!(
            frankenterm_config_dirs(),
            vec![
                home.path().join("frankenterm"),
                first.path().join("frankenterm"),
                second.path().join("frankenterm"),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_owned_directory_creation_is_private_and_symlink_safe() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        let fixture = tempfile::tempdir().expect("private-directory fixture");
        let private = fixture.path().join("state").join("nested");
        create_user_owned_dirs(&private).expect("create private nested directory");
        assert_eq!(
            std::fs::symlink_metadata(&private)
                .expect("inspect private directory")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );

        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o755))
            .expect("plant legacy permissive mode");
        create_user_owned_dirs(&private).expect("tighten legacy private directory");
        assert_eq!(
            std::fs::symlink_metadata(&private)
                .expect("reinspect tightened directory")
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );

        let target = fixture.path().join("symlink-target");
        std::fs::create_dir(&target).expect("create symlink target");
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .expect("plant public target permissions");
        let alias = fixture.path().join("symlink-alias");
        symlink(&target, &alias).expect("plant final-component symlink");
        assert!(
            create_user_owned_dirs(&alias).is_err(),
            "a symbolic-link directory authority must fail closed"
        );
        assert_eq!(
            std::fs::symlink_metadata(&target)
                .expect("inspect untouched symlink target")
                .permissions()
                .mode()
                & 0o7777,
            0o755,
            "rejected symbolic-link authority must not tighten its target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_owned_file_io_normalizes_legacy_mode_and_preserves_descriptor_identity() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = tempfile::tempdir().expect("private-file fixture");
        let private = fixture.path().join("state");
        create_user_owned_dirs(&private).expect("create private file directory");
        let path = private.join("state.json");
        std::fs::write(&path, b"legacy").expect("plant legacy private file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("plant legacy permissive file mode");

        assert_eq!(
            read_user_owned_file(&path).expect("read and normalize legacy file"),
            b"legacy"
        );
        assert_eq!(
            path.symlink_metadata()
                .expect("inspect normalized private file")
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
        write_user_owned_file(&path, b"new").expect("replace private file");
        append_user_owned_file(&path, b"-tail").expect("append private file");
        assert_eq!(
            read_user_owned_file(&path).expect("read replaced private file"),
            b"new-tail"
        );
    }

    #[cfg(unix)]
    #[test]
    fn user_owned_file_io_rejects_symlink_and_hardlink_aliases_without_mutating_targets() {
        use std::os::unix::fs::{symlink, PermissionsExt as _};

        for hardlink in [false, true] {
            let fixture = tempfile::tempdir().expect("private-file alias fixture");
            let private = fixture.path().join("state");
            create_user_owned_dirs(&private).expect("create private file directory");
            let target = fixture.path().join("foreign-target");
            std::fs::write(&target, b"foreign").expect("write foreign target");
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600))
                .expect("make foreign target private");
            let alias = private.join("state.json");
            if hardlink {
                std::fs::hard_link(&target, &alias).expect("plant hardlink alias");
            } else {
                symlink(&target, &alias).expect("plant symlink alias");
            }

            assert!(read_user_owned_file(&alias).is_err());
            assert!(write_user_owned_file(&alias, b"replacement").is_err());
            assert!(append_user_owned_file(&alias, b"tail").is_err());
            assert_eq!(
                std::fs::read(&target).expect("foreign target remains readable"),
                b"foreign"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn user_owned_file_io_fails_closed_at_the_size_bound() {
        use std::os::unix::fs::PermissionsExt as _;

        let fixture = tempfile::tempdir().expect("private-file bound fixture");
        let private = fixture.path().join("state");
        create_user_owned_dirs(&private).expect("create private file directory");
        let path = private.join("bounded-state");
        let file = std::fs::File::create(&path).expect("create bounded state fixture");
        file.set_len(MAX_USER_OWNED_FILE_BYTES.saturating_add(1))
            .expect("create sparse oversized fixture");
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .expect("make bounded state fixture private");

        assert!(read_user_owned_file(&path).is_err());
        assert!(append_user_owned_file(&path, b"x").is_err());
        assert_eq!(
            path.metadata()
                .expect("inspect unchanged oversized file")
                .len(),
            MAX_USER_OWNED_FILE_BYTES.saturating_add(1)
        );
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_user_owned_appends_serialize_the_size_bound() {
        let fixture = tempfile::tempdir().expect("concurrent private-file fixture");
        let private = fixture.path().join("state");
        create_user_owned_dirs(&private).expect("create private file directory");
        let path = private.join("bounded-append-state");
        write_user_owned_file(&path, b"").expect("initialize bounded append authority");

        let append_bytes =
            usize::try_from(MAX_USER_OWNED_FILE_BYTES / 4).expect("append test payload fits usize");
        let payload = std::sync::Arc::new(vec![b'x'; append_bytes]);
        let successes = std::thread::scope(|scope| {
            let workers = (0..8)
                .map(|_| {
                    let path = path.clone();
                    let payload = std::sync::Arc::clone(&payload);
                    scope.spawn(move || append_user_owned_file(&path, payload.as_slice()).is_ok())
                })
                .collect::<Vec<_>>();
            workers
                .into_iter()
                .map(|worker| worker.join().expect("append worker did not panic"))
                .filter(|success| *success)
                .count()
        });

        assert_eq!(successes, 4);
        assert_eq!(
            path.metadata()
                .expect("inspect serialized append authority")
                .len(),
            MAX_USER_OWNED_FILE_BYTES
        );
    }
}
