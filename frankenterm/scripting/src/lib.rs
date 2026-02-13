//! Scripting abstraction layer for FrankenTerm runtime engines.
//!
//! This crate defines a common `ScriptingEngine` trait and a
//! `ScriptingDispatcher` that can coordinate multiple engines
//! (for example Lua + WASM) behind one interface.

use anyhow::Result;
use std::path::Path;

mod dispatcher;
#[cfg(feature = "lua")]
mod lua_engine;
mod types;
#[cfg(feature = "wasm")]
mod wasm_engine;

pub use dispatcher::ScriptingDispatcher;
#[cfg(feature = "lua")]
pub use lua_engine::LuaEngine;
pub use types::{
    Action, ConfigValue, EngineCapabilities, ExtensionId, ExtensionManifest, HookHandler, HookId,
    LogLevel,
};
#[cfg(feature = "wasm")]
pub use wasm_engine::{WasmEngine, WasmEngineConfig};

/// Unified interface for all scripting engines.
///
/// Implementors can be language runtimes such as Lua or WASM.
pub trait ScriptingEngine: Send + Sync + 'static {
    /// Evaluate a configuration file and return structured config data.
    fn eval_config(&self, path: &Path) -> Result<ConfigValue>;

    /// Register an event hook and return a stable handle.
    fn register_hook(&self, event: &str, handler: HookHandler) -> Result<HookId>;

    /// Remove a previously registered hook.
    fn unregister_hook(&self, id: HookId) -> Result<()>;

    /// Fire an event and collect resulting actions.
    fn fire_event(&self, event: &str, payload: &frankenterm_dynamic::Value) -> Result<Vec<Action>>;

    /// Load an extension manifest and return an extension handle.
    fn load_extension(&self, manifest: &ExtensionManifest) -> Result<ExtensionId>;

    /// Unload an extension by handle.
    fn unload_extension(&self, id: ExtensionId) -> Result<()>;

    /// Report runtime capabilities to callers.
    fn capabilities(&self) -> EngineCapabilities;

    /// Human-readable runtime name (`lua-5.4`, `wasmtime-28`, ...).
    fn engine_name(&self) -> &str;
}
