//! Keybinding registration for extensions.
//!
//! Extensions can register keybindings that trigger callback functions.
//! Each binding associates a key + modifier combination with a handler.

use crate::types::Action;
use anyhow::{Result, bail};
use frankenterm_dynamic::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Opaque handle for a registered keybinding.
pub type KeybindingId = u64;

/// Modifier keys that can be combined with a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

impl Modifiers {
    pub const NONE: Self = Self {
        ctrl: false,
        shift: false,
        alt: false,
        super_key: false,
    };

    /// Parse a modifier string like "ctrl+shift" or "alt".
    pub fn parse(s: &str) -> Self {
        let mut mods = Self::NONE;
        for part in s.split('+') {
            match part.trim().to_lowercase().as_str() {
                "ctrl" | "control" => mods.ctrl = true,
                "shift" => mods.shift = true,
                "alt" | "opt" | "option" => mods.alt = true,
                "super" | "cmd" | "command" | "meta" => mods.super_key = true,
                _ => {}
            }
        }
        mods
    }

    /// Convert to a canonical string representation.
    pub fn to_string_repr(&self) -> String {
        let mut parts = Vec::new();
        if self.ctrl {
            parts.push("ctrl");
        }
        if self.shift {
            parts.push("shift");
        }
        if self.alt {
            parts.push("alt");
        }
        if self.super_key {
            parts.push("super");
        }
        parts.join("+")
    }
}

/// A key + modifier combination.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct KeyCombo {
    pub key: String,
    pub modifiers: Modifiers,
}

impl KeyCombo {
    /// Parse a key combo string like "ctrl+shift+t" or "alt+enter".
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('+').collect();
        if parts.is_empty() {
            bail!("empty keybinding");
        }

        // Last part is the key, everything before is modifiers
        let key = parts.last().unwrap().trim().to_lowercase();
        if key.is_empty() {
            bail!("keybinding has no key");
        }

        let mod_str = if parts.len() > 1 {
            parts[..parts.len() - 1].join("+")
        } else {
            String::new()
        };

        Ok(Self {
            key,
            modifiers: Modifiers::parse(&mod_str),
        })
    }

    /// Canonical string representation.
    pub fn to_string_repr(&self) -> String {
        let mods = self.modifiers.to_string_repr();
        if mods.is_empty() {
            self.key.clone()
        } else {
            format!("{}+{}", mods, self.key)
        }
    }
}

type KeyHandlerFn = dyn Fn(&Value) -> Result<Vec<Action>> + Send + Sync + 'static;

struct RegisteredBinding {
    id: KeybindingId,
    #[allow(dead_code)] // stored for introspection/debugging
    combo: KeyCombo,
    extension_id: String,
    handler: Arc<KeyHandlerFn>,
}

/// Keybinding registry for extensions.
pub struct KeybindingRegistry {
    bindings: Mutex<Vec<RegisteredBinding>>,
    next_id: AtomicU64,
    /// Lookup table for fast dispatch: canonical combo string → binding ids
    lookup: Mutex<HashMap<String, Vec<KeybindingId>>>,
}

impl Default for KeybindingRegistry {
    fn default() -> Self {
        Self {
            bindings: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            lookup: Mutex::new(HashMap::new()),
        }
    }
}

impl KeybindingRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a keybinding for an extension.
    pub fn register<F>(&self, combo: KeyCombo, extension_id: &str, handler: F) -> KeybindingId
    where
        F: Fn(&Value) -> Result<Vec<Action>> + Send + Sync + 'static,
    {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let canonical = combo.to_string_repr();

        let binding = RegisteredBinding {
            id,
            combo,
            extension_id: extension_id.to_string(),
            handler: Arc::new(handler),
        };

        if let Ok(mut bindings) = self.bindings.lock() {
            bindings.push(binding);
        }

        if let Ok(mut lookup) = self.lookup.lock() {
            lookup.entry(canonical).or_default().push(id);
        }

        id
    }

    /// Unregister a keybinding by handle.
    pub fn unregister(&self, id: KeybindingId) -> bool {
        let removed = if let Ok(mut bindings) = self.bindings.lock() {
            let len_before = bindings.len();
            bindings.retain(|b| b.id != id);
            bindings.len() < len_before
        } else {
            return false;
        };

        if removed && let Ok(mut lookup) = self.lookup.lock() {
            for ids in lookup.values_mut() {
                ids.retain(|&i| i != id);
            }
            lookup.retain(|_, ids| !ids.is_empty());
        }

        removed
    }

    /// Unregister all bindings for an extension.
    pub fn unregister_extension(&self, extension_id: &str) -> usize {
        let mut removed_ids = Vec::new();

        if let Ok(mut bindings) = self.bindings.lock() {
            bindings.retain(|b| {
                if b.extension_id == extension_id {
                    removed_ids.push(b.id);
                    false
                } else {
                    true
                }
            });
        }

        if !removed_ids.is_empty()
            && let Ok(mut lookup) = self.lookup.lock()
        {
            for ids in lookup.values_mut() {
                ids.retain(|id| !removed_ids.contains(id));
            }
            lookup.retain(|_, ids| !ids.is_empty());
        }

        removed_ids.len()
    }

    /// Dispatch a key press to all matching handlers.
    pub fn dispatch(&self, combo: &KeyCombo, payload: &Value) -> Result<Vec<Action>> {
        let canonical = combo.to_string_repr();

        let handlers: Vec<Arc<KeyHandlerFn>> = {
            let lookup = self
                .lookup
                .lock()
                .map_err(|_| anyhow::anyhow!("lock poisoned"))?;
            let ids = match lookup.get(&canonical) {
                Some(ids) => ids.clone(),
                None => return Ok(vec![]),
            };

            let bindings = self
                .bindings
                .lock()
                .map_err(|_| anyhow::anyhow!("lock poisoned"))?;
            ids.iter()
                .filter_map(|id| {
                    bindings
                        .iter()
                        .find(|b| b.id == *id)
                        .map(|b| Arc::clone(&b.handler))
                })
                .collect()
        };

        let mut actions = Vec::new();
        for handler in handlers {
            let mut result = handler(payload)?;
            actions.append(&mut result);
        }
        Ok(actions)
    }

    /// Number of registered bindings.
    pub fn count(&self) -> usize {
        self.bindings.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// List all key combos currently bound.
    pub fn bound_combos(&self) -> Vec<String> {
        self.lookup
            .lock()
            .map(|l| l.keys().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_key_combo() {
        let combo = KeyCombo::parse("ctrl+shift+t").unwrap();
        assert_eq!(combo.key, "t");
        assert!(combo.modifiers.ctrl);
        assert!(combo.modifiers.shift);
        assert!(!combo.modifiers.alt);
    }

    #[test]
    fn parse_single_key() {
        let combo = KeyCombo::parse("f5").unwrap();
        assert_eq!(combo.key, "f5");
        assert_eq!(combo.modifiers, Modifiers::NONE);
    }

    #[test]
    fn parse_empty_fails() {
        assert!(KeyCombo::parse("").is_err());
    }

    #[test]
    fn key_combo_canonical_repr() {
        let combo = KeyCombo::parse("shift+ctrl+t").unwrap();
        // Canonical is always: ctrl+shift+alt+super+key
        assert_eq!(combo.to_string_repr(), "ctrl+shift+t");
    }

    #[test]
    fn modifiers_parse_aliases() {
        let mods = Modifiers::parse("cmd+opt");
        assert!(mods.super_key);
        assert!(mods.alt);
    }

    #[test]
    fn register_and_dispatch() {
        let registry = KeybindingRegistry::new();
        let combo = KeyCombo::parse("ctrl+t").unwrap();

        registry.register(combo.clone(), "my-ext", |_payload| {
            Ok(vec![Action::Log {
                level: crate::types::LogLevel::Info,
                message: "keybinding fired".to_string(),
            }])
        });

        let actions = registry.dispatch(&combo, &Value::Null).unwrap();
        assert_eq!(actions.len(), 1);
    }

    #[test]
    fn dispatch_no_match() {
        let registry = KeybindingRegistry::new();
        let combo = KeyCombo::parse("ctrl+t").unwrap();
        let other = KeyCombo::parse("ctrl+n").unwrap();

        registry.register(combo, "my-ext", |_| Ok(vec![]));

        let actions = registry.dispatch(&other, &Value::Null).unwrap();
        assert!(actions.is_empty());
    }

    #[test]
    fn unregister() {
        let registry = KeybindingRegistry::new();
        let combo = KeyCombo::parse("ctrl+t").unwrap();

        let id = registry.register(combo, "my-ext", |_| Ok(vec![]));
        assert_eq!(registry.count(), 1);

        assert!(registry.unregister(id));
        assert_eq!(registry.count(), 0);
    }

    #[test]
    fn unregister_extension() {
        let registry = KeybindingRegistry::new();

        registry.register(KeyCombo::parse("ctrl+a").unwrap(), "ext-1", |_| Ok(vec![]));
        registry.register(KeyCombo::parse("ctrl+b").unwrap(), "ext-1", |_| Ok(vec![]));
        registry.register(KeyCombo::parse("ctrl+c").unwrap(), "ext-2", |_| Ok(vec![]));

        let removed = registry.unregister_extension("ext-1");
        assert_eq!(removed, 2);
        assert_eq!(registry.count(), 1);
    }

    #[test]
    fn multiple_handlers_same_combo() {
        let registry = KeybindingRegistry::new();
        let combo = KeyCombo::parse("ctrl+t").unwrap();

        registry.register(combo.clone(), "ext-1", |_| {
            Ok(vec![Action::Log {
                level: crate::types::LogLevel::Info,
                message: "handler-1".to_string(),
            }])
        });

        registry.register(combo.clone(), "ext-2", |_| {
            Ok(vec![Action::Log {
                level: crate::types::LogLevel::Info,
                message: "handler-2".to_string(),
            }])
        });

        let actions = registry.dispatch(&combo, &Value::Null).unwrap();
        assert_eq!(actions.len(), 2);
    }
}
