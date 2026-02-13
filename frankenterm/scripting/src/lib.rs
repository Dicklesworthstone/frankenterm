//! Scripting abstraction layer for FrankenTerm runtime engines.
//!
//! This crate defines a common `ScriptingEngine` trait and a
//! `ScriptingDispatcher` that can coordinate multiple engines
//! (for example Lua + WASM) behind one interface.

use anyhow::Result;
use std::path::Path;

pub mod audit;
mod dispatcher;
pub mod events;
pub mod extension;
pub mod keybindings;
pub mod lifecycle;
#[cfg(feature = "lua")]
mod lua_engine;
pub mod manifest;
pub mod package;
pub mod sandbox;
pub mod storage;
mod types;
#[cfg(feature = "wasm")]
mod wasm_cache;
#[cfg(feature = "wasm")]
mod wasm_engine;
#[cfg(feature = "wasm")]
mod wasm_host;

pub use audit::{AuditOutcome, AuditTrail};
pub use dispatcher::ScriptingDispatcher;
pub use events::{DispatchTier, EventBus, EventHookId};
pub use extension::{ExtensionManager, InstalledExtension};
pub use keybindings::{KeyCombo, KeybindingId, KeybindingRegistry, Modifiers};
pub use lifecycle::{ExtensionLifecycle, ExtensionState, ManagedExtension};
#[cfg(feature = "lua")]
pub use lua_engine::LuaEngine;
pub use manifest::{EngineConfig, EngineType, ExtensionPermissions, ParsedManifest};
pub use package::{FtxBuilder, FtxPackage};
pub use sandbox::{ResourceLimits, SandboxConfig, SandboxEnforcer};
pub use storage::ExtensionStorage;
pub use types::{
    Action, ConfigValue, EngineCapabilities, ExtensionId, ExtensionManifest, HookHandler, HookId,
    LogLevel,
};
#[cfg(feature = "wasm")]
pub use wasm_cache::ModuleCache;
#[cfg(feature = "wasm")]
pub use wasm_engine::{WasmEngine, WasmEngineConfig};
#[cfg(feature = "wasm")]
pub use wasm_host::HostState;

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
