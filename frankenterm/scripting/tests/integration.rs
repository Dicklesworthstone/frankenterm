//! Integration tests for the scripting crate.
//!
//! Tests cross-module interactions: package → install → lifecycle → events.
//! These run without any feature flags (no Lua, no WASM) to verify the core
//! infrastructure works independently.

use frankenterm_scripting::events::{DispatchTier, EventBus};
use frankenterm_scripting::extension::ExtensionManager;
use frankenterm_scripting::keybindings::{KeyCombo, KeybindingRegistry};
use frankenterm_scripting::lifecycle::{ExtensionLifecycle, ExtensionState};
use frankenterm_scripting::package::FtxBuilder;
use frankenterm_scripting::storage::ExtensionStorage;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

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

fn create_test_ftx(dir: &Path, name: &str) -> std::path::PathBuf {
    let ftx_path = dir.join(format!("{name}.ftx"));
    FtxBuilder::new()
        .add_manifest(&test_manifest(name))
        .add_file("main.wasm", WASM_MAGIC)
        .write_to(&ftx_path)
        .unwrap();
    ftx_path
}

// --- Full lifecycle integration ---

#[test]
fn full_extension_lifecycle_install_to_remove() {
    let dir = tempfile::tempdir().unwrap();
    let lifecycle =
        ExtensionLifecycle::new(dir.path().join("ext"), dir.path().join("storage")).unwrap();

    // Install
    let ftx = create_test_ftx(dir.path(), "full-test");
    let managed = lifecycle.install(&ftx).unwrap();
    assert_eq!(managed.state, ExtensionState::Installed);
    assert_eq!(managed.name, "full-test");

    // Store data
    lifecycle
        .storage
        .set("full-test", "prefs", b"dark_theme")
        .unwrap();
    assert_eq!(
        lifecycle.storage.get("full-test", "prefs").unwrap(),
        Some(b"dark_theme".to_vec())
    );

    // Register hook
    let hook_id = lifecycle.event_bus.register(
        "pane.focus",
        DispatchTier::Native,
        0,
        Some("full-test"),
        |_, _| Ok(vec![]),
    );

    // Mark loaded
    lifecycle.mark_loaded("full-test", vec![hook_id]).unwrap();
    assert_eq!(
        lifecycle.get_state("full-test"),
        Some(ExtensionState::Loaded)
    );

    // Disable
    lifecycle.disable("full-test").unwrap();
    assert_eq!(
        lifecycle.get_state("full-test"),
        Some(ExtensionState::Disabled)
    );
    assert_eq!(lifecycle.event_bus.hook_count(), 0);

    // Re-enable
    lifecycle.enable("full-test").unwrap();
    assert_eq!(
        lifecycle.get_state("full-test"),
        Some(ExtensionState::Installed)
    );

    // Remove
    lifecycle.remove("full-test").unwrap();
    assert!(lifecycle.get_state("full-test").is_none());
    assert!(lifecycle.storage.keys("full-test").unwrap().is_empty());
}

#[test]
fn scan_preserves_disabled_state_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let lifecycle =
        ExtensionLifecycle::new(dir.path().join("ext"), dir.path().join("storage")).unwrap();

    // Install two extensions
    let ftx_a = create_test_ftx(dir.path(), "ext-a");
    let ftx_b = create_test_ftx(dir.path(), "ext-b");
    lifecycle.install(&ftx_a).unwrap();
    lifecycle.install(&ftx_b).unwrap();

    // Disable one
    lifecycle.disable("ext-b").unwrap();

    // Simulate restart: new lifecycle pointing to same directories
    let lifecycle2 =
        ExtensionLifecycle::new(dir.path().join("ext"), dir.path().join("storage")).unwrap();
    lifecycle2.scan().unwrap();

    assert_eq!(
        lifecycle2.get_state("ext-a"),
        Some(ExtensionState::Installed)
    );
    assert_eq!(
        lifecycle2.get_state("ext-b"),
        Some(ExtensionState::Disabled)
    );

    let loadable = lifecycle2.loadable_extensions();
    assert!(loadable.contains(&"ext-a".to_string()));
    assert!(!loadable.contains(&"ext-b".to_string()));
}

// --- Event bus stress tests ---

#[test]
fn event_bus_many_handlers() {
    let bus = EventBus::new();
    let mut hook_ids = Vec::new();

    // Register 100 handlers
    for i in 0..100 {
        let id = bus.register("stress.event", DispatchTier::Native, i, None, |_, _| {
            Ok(vec![])
        });
        hook_ids.push(id);
    }

    assert_eq!(bus.hook_count(), 100);

    // Fire event — all 100 should execute
    let actions = bus
        .fire("stress.event", &frankenterm_dynamic::Value::Null)
        .unwrap();
    assert!(actions.is_empty()); // all return empty vecs

    // Fire counts
    let counts = bus.fire_counts();
    assert_eq!(*counts.get("stress.event").unwrap(), 1);

    // Unregister half
    for id in hook_ids.iter().take(50) {
        bus.unregister(*id);
    }
    assert_eq!(bus.hook_count(), 50);
}

#[test]
fn event_bus_wildcard_and_prefix_stress() {
    let bus = EventBus::new();

    // Register wildcard handler
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    bus.register("*", DispatchTier::Native, 0, None, move |_, _| {
        counter_clone.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    });

    // Register prefix handler
    let prefix_counter = Arc::new(AtomicUsize::new(0));
    let prefix_clone = prefix_counter.clone();
    bus.register("pane.*", DispatchTier::Native, 0, None, move |_, _| {
        prefix_clone.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    });

    // Fire various events
    for event in &[
        "pane.focus",
        "pane.close",
        "pane.resize",
        "config.reload",
        "window.close",
    ] {
        bus.fire(event, &frankenterm_dynamic::Value::Null).unwrap();
    }

    // Wildcard should match all 5
    assert_eq!(counter.load(Ordering::SeqCst), 5);
    // Prefix should match 3 (pane.*)
    assert_eq!(prefix_counter.load(Ordering::SeqCst), 3);
}

// --- Keybinding integration ---

#[test]
fn keybinding_multi_extension_registration() {
    use frankenterm_scripting::{Action, LogLevel};

    let registry = KeybindingRegistry::new();

    // Extension A registers some bindings
    let combo_a = KeyCombo::parse("ctrl+t").unwrap();
    registry.register(combo_a, "ext-a", |_payload| {
        Ok(vec![Action::Log {
            level: LogLevel::Info,
            message: "open_tab".into(),
        }])
    });

    // Extension B registers different bindings
    let combo_b = KeyCombo::parse("ctrl+w").unwrap();
    registry.register(combo_b, "ext-b", |_payload| {
        Ok(vec![Action::Log {
            level: LogLevel::Info,
            message: "close_tab".into(),
        }])
    });

    assert_eq!(registry.count(), 2);

    // Dispatch
    let combo_t = KeyCombo::parse("ctrl+t").unwrap();
    let results = registry
        .dispatch(&combo_t, &frankenterm_dynamic::Value::Null)
        .unwrap();
    assert_eq!(results.len(), 1);

    // Unregister extension A
    registry.unregister_extension("ext-a");
    assert_eq!(registry.count(), 1);

    // ctrl+t should no longer dispatch
    let results = registry
        .dispatch(&combo_t, &frankenterm_dynamic::Value::Null)
        .unwrap();
    assert!(results.is_empty());

    // ctrl+w should still work
    let combo_w = KeyCombo::parse("ctrl+w").unwrap();
    let results = registry
        .dispatch(&combo_w, &frankenterm_dynamic::Value::Null)
        .unwrap();
    assert_eq!(results.len(), 1);
}

// --- Storage isolation ---

#[test]
fn storage_extension_isolation() {
    let dir = tempfile::tempdir().unwrap();
    let storage = ExtensionStorage::new(dir.path().to_path_buf()).unwrap();

    // Extension A writes data
    storage.set("ext-a", "theme", b"dark").unwrap();
    storage.set("ext-a", "font", b"monospace").unwrap();

    // Extension B writes data
    storage.set("ext-b", "theme", b"light").unwrap();

    // Verify isolation
    assert_eq!(
        storage.get("ext-a", "theme").unwrap(),
        Some(b"dark".to_vec())
    );
    assert_eq!(
        storage.get("ext-b", "theme").unwrap(),
        Some(b"light".to_vec())
    );

    // Clear extension A — B should be unaffected
    storage.clear_extension("ext-a").unwrap();
    assert!(storage.get("ext-a", "theme").unwrap().is_none());
    assert_eq!(
        storage.get("ext-b", "theme").unwrap(),
        Some(b"light".to_vec())
    );
}

// --- Package validation ---

#[test]
fn ftx_package_roundtrip_through_install() {
    let dir = tempfile::tempdir().unwrap();
    let manager = ExtensionManager::new(dir.path().join("ext")).unwrap();

    // Build and install
    let ftx = create_test_ftx(dir.path(), "roundtrip-ext");
    let installed = manager.install(&ftx).unwrap();

    assert_eq!(installed.name, "roundtrip-ext");
    assert_eq!(installed.version, "1.0.0");
    assert!(installed.path.exists());

    // Verify we can re-list it
    let list = manager.list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].name, "roundtrip-ext");

    // Get by name
    let got = manager.get("roundtrip-ext").unwrap();
    assert!(got.is_some());
    assert_eq!(got.unwrap().version, "1.0.0");
}

#[test]
fn multiple_extensions_coexist() {
    let dir = tempfile::tempdir().unwrap();
    let lifecycle =
        ExtensionLifecycle::new(dir.path().join("ext"), dir.path().join("storage")).unwrap();

    // Install 10 extensions
    for i in 0..10 {
        let name = format!("ext-{i}");
        let ftx = create_test_ftx(dir.path(), &name);
        lifecycle.install(&ftx).unwrap();
    }

    // All should be listed
    let list = lifecycle.list();
    assert_eq!(list.len(), 10);

    // Disable half
    for i in 0..5 {
        lifecycle.disable(&format!("ext-{i}")).unwrap();
    }

    // Only 5 should be loadable
    let loadable = lifecycle.loadable_extensions();
    assert_eq!(loadable.len(), 5);

    // Verify correct ones are loadable
    for name in &loadable {
        let n: usize = name.strip_prefix("ext-").unwrap().parse().unwrap();
        assert!(n >= 5);
    }
}

// --- Tier ordering ---

#[test]
fn event_dispatch_respects_tier_ordering() {
    let bus = EventBus::new();
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));

    let o1 = order.clone();
    bus.register("test.event", DispatchTier::Lua, 0, None, move |_, _| {
        o1.lock().unwrap().push("lua");
        Ok(vec![])
    });

    let o2 = order.clone();
    bus.register("test.event", DispatchTier::Native, 0, None, move |_, _| {
        o2.lock().unwrap().push("native");
        Ok(vec![])
    });

    let o3 = order.clone();
    bus.register("test.event", DispatchTier::Wasm, 0, None, move |_, _| {
        o3.lock().unwrap().push("wasm");
        Ok(vec![])
    });

    bus.fire("test.event", &frankenterm_dynamic::Value::Null)
        .unwrap();

    let fired_order = order.lock().unwrap();
    assert_eq!(&*fired_order, &["native", "wasm", "lua"]);
}
