//! Extension lifecycle state management.
//!
//! Tracks whether each installed extension is enabled, disabled, loaded,
//! or in an error state. Persists state to `.state.toml` per extension.

use crate::events::{EventBus, EventHookId};
use crate::extension::ExtensionManager;
use crate::keybindings::KeybindingRegistry;
use crate::manifest::ParsedManifest;
use crate::storage::ExtensionStorage;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Lifecycle state for an extension.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtensionState {
    /// Installed on disk, not yet loaded.
    Installed,
    /// Actively loaded and running.
    Loaded,
    /// Installed but explicitly disabled by the user.
    Disabled,
    /// Failed to load — stores the error message.
    Error(String),
}

impl ExtensionState {
    /// Whether this extension should be loaded on startup.
    pub fn should_load(&self) -> bool {
        matches!(self, Self::Installed | Self::Loaded)
    }

    /// Human-readable label.
    pub fn label(&self) -> &str {
        match self {
            Self::Installed => "installed",
            Self::Loaded => "loaded",
            Self::Disabled => "disabled",
            Self::Error(_) => "error",
        }
    }
}

/// Runtime information about a managed extension.
#[derive(Clone, Debug)]
pub struct ManagedExtension {
    pub name: String,
    pub version: String,
    pub state: ExtensionState,
    pub manifest: ParsedManifest,
    pub path: PathBuf,
    pub hook_ids: Vec<EventHookId>,
}

/// Full extension lifecycle manager.
///
/// Wraps [`ExtensionManager`] with state tracking, event bus integration,
/// keybinding registration, and per-extension storage.
pub struct ExtensionLifecycle {
    manager: ExtensionManager,
    pub event_bus: EventBus,
    pub keybindings: KeybindingRegistry,
    pub storage: ExtensionStorage,
    extensions: Mutex<HashMap<String, ManagedExtension>>,
}

impl ExtensionLifecycle {
    /// Create a new lifecycle manager.
    pub fn new(extensions_dir: PathBuf, storage_dir: PathBuf) -> Result<Self> {
        let manager = ExtensionManager::new(extensions_dir)?;
        let storage = ExtensionStorage::new(storage_dir)?;

        Ok(Self {
            manager,
            event_bus: EventBus::new(),
            keybindings: KeybindingRegistry::new(),
            storage,
            extensions: Mutex::new(HashMap::new()),
        })
    }

    /// Install a .ftx package and return the managed extension.
    pub fn install(&self, ftx_path: &Path) -> Result<ManagedExtension> {
        let installed = self.manager.install(ftx_path)?;

        // Write initial state file
        write_state_file(&installed.path, true)?;

        let managed = ManagedExtension {
            name: installed.name.clone(),
            version: installed.version.clone(),
            state: ExtensionState::Installed,
            manifest: installed.manifest,
            path: installed.path,
            hook_ids: Vec::new(),
        };

        if let Ok(mut exts) = self.extensions.lock() {
            exts.insert(managed.name.clone(), managed.clone());
        }

        Ok(managed)
    }

    /// Enable a disabled extension.
    pub fn enable(&self, name: &str) -> Result<()> {
        let mut exts = self
            .extensions
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;

        let ext = exts
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("extension '{name}' not found"))?;

        match &ext.state {
            ExtensionState::Disabled | ExtensionState::Error(_) => {
                ext.state = ExtensionState::Installed;
                write_state_file(&ext.path, true)?;
                Ok(())
            }
            ExtensionState::Loaded => Ok(()), // already enabled
            ExtensionState::Installed => Ok(()),
        }
    }

    /// Disable a loaded or installed extension.
    pub fn disable(&self, name: &str) -> Result<()> {
        let mut exts = self
            .extensions
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;

        let ext = exts
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("extension '{name}' not found"))?;

        // Unregister hooks if loaded
        for hook_id in &ext.hook_ids {
            self.event_bus.unregister(*hook_id);
        }
        self.keybindings.unregister_extension(name);
        ext.hook_ids.clear();

        ext.state = ExtensionState::Disabled;
        write_state_file(&ext.path, false)?;
        Ok(())
    }

    /// Remove an extension entirely (unload + delete from disk).
    pub fn remove(&self, name: &str) -> Result<()> {
        // Unregister all hooks and bindings
        if let Ok(exts) = self.extensions.lock()
            && let Some(ext) = exts.get(name)
        {
            for hook_id in &ext.hook_ids {
                self.event_bus.unregister(*hook_id);
            }
        }
        self.keybindings.unregister_extension(name);
        self.event_bus.unregister_extension(name);

        // Clean up storage
        self.storage.clear_extension(name)?;

        // Remove from disk
        self.manager.remove(name)?;

        // Remove from tracked extensions
        if let Ok(mut exts) = self.extensions.lock() {
            exts.remove(name);
        }

        Ok(())
    }

    /// Mark an extension as loaded (call after successfully compiling and registering hooks).
    pub fn mark_loaded(&self, name: &str, hook_ids: Vec<EventHookId>) -> Result<()> {
        let mut exts = self
            .extensions
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;

        let ext = exts
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("extension '{name}' not found"))?;

        ext.state = ExtensionState::Loaded;
        ext.hook_ids = hook_ids;
        Ok(())
    }

    /// Mark an extension as having a load error.
    pub fn mark_error(&self, name: &str, error: &str) -> Result<()> {
        let mut exts = self
            .extensions
            .lock()
            .map_err(|_| anyhow::anyhow!("lock poisoned"))?;

        let ext = exts
            .get_mut(name)
            .ok_or_else(|| anyhow::anyhow!("extension '{name}' not found"))?;

        ext.state = ExtensionState::Error(error.to_string());
        Ok(())
    }

    /// Scan the extensions directory and populate the managed extensions map.
    pub fn scan(&self) -> Result<Vec<ManagedExtension>> {
        let installed = self.manager.list()?;
        let mut managed = Vec::new();

        for ext in installed {
            let enabled = read_state_file(&ext.path);
            let state = if enabled {
                ExtensionState::Installed
            } else {
                ExtensionState::Disabled
            };

            let m = ManagedExtension {
                name: ext.name.clone(),
                version: ext.version.clone(),
                state,
                manifest: ext.manifest,
                path: ext.path,
                hook_ids: Vec::new(),
            };

            if let Ok(mut exts) = self.extensions.lock() {
                exts.insert(m.name.clone(), m.clone());
            }

            managed.push(m);
        }

        Ok(managed)
    }

    /// Get the state of a specific extension.
    pub fn get_state(&self, name: &str) -> Option<ExtensionState> {
        self.extensions
            .lock()
            .ok()
            .and_then(|exts| exts.get(name).map(|e| e.state.clone()))
    }

    /// List all managed extensions.
    pub fn list(&self) -> Vec<ManagedExtension> {
        self.extensions
            .lock()
            .map(|exts| {
                let mut list: Vec<_> = exts.values().cloned().collect();
                list.sort_by(|a, b| a.name.cmp(&b.name));
                list
            })
            .unwrap_or_default()
    }

    /// List names of extensions that should be loaded.
    pub fn loadable_extensions(&self) -> Vec<String> {
        self.extensions
            .lock()
            .map(|exts| {
                exts.values()
                    .filter(|e| e.state.should_load())
                    .map(|e| e.name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Access the underlying extension manager.
    pub fn manager(&self) -> &ExtensionManager {
        &self.manager
    }
}

/// Per-extension state file (.state.toml).
const STATE_FILE: &str = ".state.toml";

fn write_state_file(ext_dir: &Path, enabled: bool) -> Result<()> {
    let content = format!("enabled = {enabled}\n");
    let path = ext_dir.join(STATE_FILE);
    std::fs::write(&path, content)
        .with_context(|| format!("failed to write state file {}", path.display()))?;
    Ok(())
}

fn read_state_file(ext_dir: &Path) -> bool {
    let path = ext_dir.join(STATE_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => content
            .parse::<toml::Value>()
            .ok()
            .and_then(|v| v.get("enabled")?.as_bool())
            .unwrap_or(true),
        Err(_) => true, // Default to enabled if no state file
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::DispatchTier;
    use crate::package::FtxBuilder;

    const WASM_MAGIC: &[u8] = &[0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];

    fn test_manifest(name: &str) -> String {
        format!(
            r#"
[extension]
name = "{name}"
version = "1.0.0"

[engine]
type = "wasm"
entry = "main.wasm"
"#
        )
    }

    fn create_test_ftx(dir: &Path, name: &str) -> PathBuf {
        let ftx_path = dir.join(format!("{name}.ftx"));
        FtxBuilder::new()
            .add_manifest(&test_manifest(name))
            .add_file("main.wasm", WASM_MAGIC)
            .write_to(&ftx_path)
            .unwrap();
        ftx_path
    }

    fn test_lifecycle(dir: &Path) -> ExtensionLifecycle {
        let ext_dir = dir.join("extensions");
        let storage_dir = dir.join("storage");
        ExtensionLifecycle::new(ext_dir, storage_dir).unwrap()
    }

    #[test]
    fn install_sets_installed_state() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        let ftx = create_test_ftx(dir.path(), "test-ext");
        let managed = lifecycle.install(&ftx).unwrap();

        assert_eq!(managed.state, ExtensionState::Installed);
        assert_eq!(
            lifecycle.get_state("test-ext"),
            Some(ExtensionState::Installed)
        );
    }

    #[test]
    fn disable_and_enable() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        let ftx = create_test_ftx(dir.path(), "toggler");
        lifecycle.install(&ftx).unwrap();

        lifecycle.disable("toggler").unwrap();
        assert_eq!(
            lifecycle.get_state("toggler"),
            Some(ExtensionState::Disabled)
        );

        lifecycle.enable("toggler").unwrap();
        assert_eq!(
            lifecycle.get_state("toggler"),
            Some(ExtensionState::Installed)
        );
    }

    #[test]
    fn remove_cleans_everything() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        let ftx = create_test_ftx(dir.path(), "removable");
        lifecycle.install(&ftx).unwrap();

        // Store some data
        lifecycle.storage.set("removable", "key", b"value").unwrap();

        lifecycle.remove("removable").unwrap();

        assert!(lifecycle.get_state("removable").is_none());
        assert!(lifecycle.storage.keys("removable").unwrap().is_empty());
    }

    #[test]
    fn mark_loaded_and_error() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        let ftx = create_test_ftx(dir.path(), "loadable");
        lifecycle.install(&ftx).unwrap();

        lifecycle.mark_loaded("loadable", vec![1, 2, 3]).unwrap();
        assert_eq!(
            lifecycle.get_state("loadable"),
            Some(ExtensionState::Loaded)
        );

        lifecycle
            .mark_error("loadable", "missing export: configure")
            .unwrap();
        assert_eq!(
            lifecycle.get_state("loadable"),
            Some(ExtensionState::Error(
                "missing export: configure".to_string()
            ))
        );
    }

    #[test]
    fn scan_discovers_installed_extensions() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        // Install some extensions
        let ftx_a = create_test_ftx(dir.path(), "alpha");
        let ftx_b = create_test_ftx(dir.path(), "bravo");
        lifecycle.install(&ftx_a).unwrap();
        lifecycle.install(&ftx_b).unwrap();

        // Clear in-memory state to simulate restart
        lifecycle.extensions.lock().unwrap().clear();

        // Scan should rediscover them
        let scanned = lifecycle.scan().unwrap();
        assert_eq!(scanned.len(), 2);

        let names: Vec<&str> = scanned.iter().map(|e| e.name.as_str()).collect();
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"bravo"));
    }

    #[test]
    fn scan_respects_disabled_state() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        let ftx = create_test_ftx(dir.path(), "disabled-ext");
        lifecycle.install(&ftx).unwrap();
        lifecycle.disable("disabled-ext").unwrap();

        // Clear and rescan
        lifecycle.extensions.lock().unwrap().clear();
        lifecycle.scan().unwrap();

        assert_eq!(
            lifecycle.get_state("disabled-ext"),
            Some(ExtensionState::Disabled)
        );
    }

    #[test]
    fn loadable_extensions_filters_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        let ftx_a = create_test_ftx(dir.path(), "enabled-ext");
        let ftx_b = create_test_ftx(dir.path(), "disabled-ext");
        lifecycle.install(&ftx_a).unwrap();
        lifecycle.install(&ftx_b).unwrap();
        lifecycle.disable("disabled-ext").unwrap();

        let loadable = lifecycle.loadable_extensions();
        assert_eq!(loadable, vec!["enabled-ext"]);
    }

    #[test]
    fn list_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        let ftx_c = create_test_ftx(dir.path(), "charlie");
        let ftx_a = create_test_ftx(dir.path(), "alpha");
        lifecycle.install(&ftx_c).unwrap();
        lifecycle.install(&ftx_a).unwrap();

        let list = lifecycle.list();
        let names: Vec<&str> = list.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "charlie"]);
    }

    #[test]
    fn enable_nonexistent_fails() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        assert!(lifecycle.enable("ghost").is_err());
    }

    #[test]
    fn disable_unregisters_hooks() {
        let dir = tempfile::tempdir().unwrap();
        let lifecycle = test_lifecycle(dir.path());

        let ftx = create_test_ftx(dir.path(), "hooked");
        lifecycle.install(&ftx).unwrap();

        // Register a hook via the event bus
        let hook_id = lifecycle.event_bus.register(
            "pane.focus",
            DispatchTier::Wasm,
            0,
            Some("hooked"),
            |_, _| Ok(vec![]),
        );

        lifecycle.mark_loaded("hooked", vec![hook_id]).unwrap();
        assert_eq!(lifecycle.event_bus.hook_count(), 1);

        lifecycle.disable("hooked").unwrap();
        assert_eq!(lifecycle.event_bus.hook_count(), 0);
    }

    #[test]
    fn state_labels() {
        assert_eq!(ExtensionState::Installed.label(), "installed");
        assert_eq!(ExtensionState::Loaded.label(), "loaded");
        assert_eq!(ExtensionState::Disabled.label(), "disabled");
        assert_eq!(ExtensionState::Error("x".into()).label(), "error");
    }

    #[test]
    fn state_should_load() {
        assert!(ExtensionState::Installed.should_load());
        assert!(ExtensionState::Loaded.should_load());
        assert!(!ExtensionState::Disabled.should_load());
        assert!(!ExtensionState::Error("x".into()).should_load());
    }
}
