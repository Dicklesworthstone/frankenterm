//! TOML configuration loader for FrankenTerm.
//!
//! Loads a `frankenterm.toml` file and converts it to a [`Config`] struct via
//! the existing `toml_to_dynamic()` → `Config::from_dynamic()` pipeline.
//! This path does not require Lua and is always available.
//!
//! # Search order
//!
//! 1. `$FRANKENTERM_CONFIG_FILE` (environment variable)
//! 2. `~/.config/frankenterm/frankenterm.toml`
//! 3. `~/.frankenterm.toml`
//!
//! If no TOML config is found, returns `None` and the caller falls through
//! to Lua config or defaults.

use crate::{toml_to_dynamic, LoadedConfig, CONFIG_DIRS, HOME_DIR};
use anyhow::{anyhow, Context};
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, UnknownFieldAction, Value};
use std::path::{Path, PathBuf};

use super::Config;

/// Search for a `frankenterm.toml` file and load it if found.
///
/// Returns `Some(LoadedConfig)` if a TOML config was found and loaded
/// (successfully or with an error), or `None` if no TOML config exists.
pub(crate) fn try_load_toml_config(overrides: &Value) -> Option<LoadedConfig> {
    if let Some(explicit_path) = explicit_toml_config_path_from_env() {
        return match try_load_toml_file(&explicit_path, overrides) {
            Ok(Some(loaded)) => Some(loaded),
            Ok(None) => Some(LoadedConfig {
                config: Err(anyhow!(
                    "$FRANKENTERM_CONFIG_FILE points to {}, but the file does not exist",
                    explicit_path.display()
                )),
                file_name: Some(explicit_path),
                lua: None,
                warnings: vec![],
            }),
            Err(err) => Some(LoadedConfig {
                config: Err(err),
                file_name: Some(explicit_path),
                lua: None,
                warnings: vec![],
            }),
        };
    }

    let paths = toml_config_search_paths();

    for path in &paths {
        log::trace!("consider toml config: {}", path.display());
        match try_load_toml_file(path, overrides) {
            Ok(Some(loaded)) => return Some(loaded),
            Ok(None) => continue,
            Err(err) => {
                return Some(LoadedConfig {
                    config: Err(err),
                    file_name: Some(path.clone()),
                    lua: None,
                    warnings: vec![],
                });
            }
        }
    }

    None
}

fn is_toml_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.to_string_lossy().eq_ignore_ascii_case("toml"))
}

fn explicit_toml_config_path_from_env() -> Option<PathBuf> {
    std::env::var_os("FRANKENTERM_CONFIG_FILE")
        .map(PathBuf::from)
        .filter(|path| is_toml_path(path))
}

/// Build the list of paths to search for `frankenterm.toml`.
fn toml_config_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    // XDG config dirs (reuse existing CONFIG_DIRS but for frankenterm subdir)
    for dir in CONFIG_DIRS.iter() {
        // CONFIG_DIRS points to wezterm subdirs; go up one level and add
        // frankenterm subdir.
        if let Some(parent) = dir.parent() {
            paths.push(parent.join("frankenterm").join("frankenterm.toml"));
        }
    }

    // Home directory dotfile
    paths.push(HOME_DIR.join(".frankenterm.toml"));

    paths
}

/// Try to load a single TOML config file.
///
/// Returns `Ok(None)` if the file does not exist, `Ok(Some(loaded))` on
/// success, or `Err` on parse/conversion failure.
fn try_load_toml_file(path: &Path, overrides: &Value) -> anyhow::Result<Option<LoadedConfig>> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => anyhow::bail!("Error reading {}: {}", path.display(), err),
    };

    let (config, warnings) =
        frankenterm_dynamic::Error::capture_warnings(|| -> anyhow::Result<Config> {
            let toml_value: toml::Value = content
                .parse()
                .with_context(|| format!("Error parsing TOML from {}", path.display()))?;

            let mut dynamic = toml_to_dynamic(&toml_value);

            // Merge overrides into the dynamic value
            if let (Value::Object(ref mut base), Value::Object(ref ovr)) = (&mut dynamic, overrides)
            {
                for (k, v) in ovr.iter() {
                    base.insert(k.clone(), v.clone());
                }
            }

            let cfg = Config::from_dynamic(
                &dynamic,
                FromDynamicOptions {
                    unknown_fields: UnknownFieldAction::Warn,
                    deprecated_fields: UnknownFieldAction::Warn,
                },
            )
            .with_context(|| {
                format!(
                    "Error converting TOML config from {} to Config struct",
                    path.display()
                )
            })?;

            cfg.check_consistency()
                .context("check_consistency on TOML config")?;

            // Compute but discard the key bindings here so that we raise any
            // problems earlier than we use them.
            let _ = cfg.key_bindings();

            std::env::set_var("FRANKENTERM_CONFIG_FILE", path);
            if let Some(dir) = path.parent() {
                std::env::set_var("FRANKENTERM_CONFIG_DIR", dir);
            }

            Ok(cfg)
        });

    let cfg = config?;

    Ok(Some(LoadedConfig {
        config: Ok(cfg.compute_extra_defaults(Some(path))),
        file_name: Some(path.to_path_buf()),
        lua: None,
        warnings,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.previous {
                std::env::set_var(self.key, prev);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn empty_toml_produces_default_config() {
        let toml_str = "";
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        // An empty TOML should produce a valid config with defaults
        assert!(cfg.scrollback_lines > 0);
    }

    #[test]
    fn basic_toml_fields_parse() {
        let toml_str = r#"
scrollback_lines = 10000
font_size = 14.0
color_scheme = "Catppuccin Mocha"
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert_eq!(cfg.scrollback_lines, 10000);
        assert!((cfg.font_size - 14.0).abs() < 0.01);
        assert_eq!(cfg.color_scheme.as_deref(), Some("Catppuccin Mocha"));
    }

    #[test]
    fn toml_search_paths_returns_nonempty() {
        let paths = toml_config_search_paths();
        // Should always have at least the home dir dotfile
        assert!(!paths.is_empty());
        assert!(paths.last().unwrap().ends_with(".frankenterm.toml"));
    }

    #[test]
    fn nonexistent_toml_returns_none() {
        let result = try_load_toml_file(
            Path::new("/tmp/nonexistent_frankenterm_test_12345.toml"),
            &Value::default(),
        )
        .unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn malformed_toml_returns_error() {
        let path = std::env::temp_dir().join("frankenterm_test_malformed.toml");
        std::fs::write(&path, "this is [not valid {toml").unwrap();
        let result = try_load_toml_file(&path, &Value::default());
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn explicit_env_missing_toml_returns_error() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing-config.toml");
        let _guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", &missing_path);

        let loaded = try_load_toml_config(&Value::default()).expect("expected explicit env result");
        assert_eq!(loaded.file_name, Some(missing_path));
        assert!(loaded.config.is_err());
    }

    #[test]
    fn explicit_env_toml_extension_is_case_insensitive() {
        let _env_lock = ENV_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frankenterm.TOML");
        std::fs::write(&path, "scrollback_lines = 4242\n").unwrap();
        let _guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", &path);

        let loaded = try_load_toml_config(&Value::default()).expect("expected explicit env result");
        assert_eq!(loaded.file_name, Some(path));
        let cfg = loaded.config.expect("expected parsed config");
        assert_eq!(cfg.scrollback_lines, 4242);
    }

    #[test]
    fn overrides_take_precedence() {
        let toml_str = r#"
scrollback_lines = 5000
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let mut dynamic = toml_to_dynamic(&toml_value);

        // Override scrollback_lines to 9999
        let mut ovr_map = std::collections::BTreeMap::new();
        ovr_map.insert(Value::String("scrollback_lines".into()), Value::U64(9999));
        let override_val = Value::Object(ovr_map.into());

        if let (Value::Object(ref mut base), Value::Object(ref ovr)) = (&mut dynamic, &override_val)
        {
            for (k, v) in ovr.iter() {
                base.insert(k.clone(), v.clone());
            }
        }

        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        )
        .unwrap();
        assert_eq!(cfg.scrollback_lines, 9999);
    }
}
