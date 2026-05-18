//! TOML configuration loader for FrankenTerm.
//!
//! Loads a `frankenterm.toml` file and converts it to a [`Config`] struct via
//! the existing `toml_to_dynamic()` → `Config::from_dynamic()` pipeline.
//! This path does not require Lua and is always available.
//!
//! # Search order
//!
//! 1. Explicit `--config-file` override (if it points to TOML)
//! 2. `$FRANKENTERM_CONFIG_FILE` (environment variable)
//! 3. `~/.config/frankenterm/frankenterm.toml`
//! 4. `~/.frankenterm.toml`
//!
//! If no TOML config is found, returns `None` and the caller falls through
//! to Lua config or defaults.

use crate::{
    config_file_override_snapshot, frankenterm_config_dirs, merge_dynamic_overrides,
    toml_to_dynamic, LoadedConfig, HOME_DIR,
};
use anyhow::{anyhow, Context};
use frankenterm_dynamic::{FromDynamic, FromDynamicOptions, UnknownFieldAction, Value};
use std::path::{Path, PathBuf};

use super::Config;

/// Search for a `frankenterm.toml` file and load it if found.
///
/// Returns `Some(LoadedConfig)` if a TOML config was found and loaded
/// (successfully or with an error), or `None` if no TOML config exists.
pub(crate) fn try_load_toml_config(overrides: &Value) -> Option<LoadedConfig> {
    if let Some(override_path) = explicit_config_file_override_path() {
        if is_toml_path(&override_path) {
            return Some(load_required_toml_path(override_path, overrides));
        }
        // A non-TOML explicit override should be handled by the Lua/other path.
        return None;
    }

    if let Some(explicit_path) = explicit_toml_config_path_from_env() {
        return Some(load_required_toml_path(explicit_path, overrides));
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

fn explicit_config_file_override_path() -> Option<PathBuf> {
    config_file_override_snapshot()
}

fn explicit_toml_config_path_from_env() -> Option<PathBuf> {
    std::env::var_os("FRANKENTERM_CONFIG_FILE")
        .map(PathBuf::from)
        .filter(|path| is_toml_path(path))
}

fn load_required_toml_path(explicit_path: PathBuf, overrides: &Value) -> LoadedConfig {
    match try_load_toml_file(&explicit_path, overrides) {
        Ok(Some(loaded)) => loaded,
        Ok(None) => LoadedConfig {
            config: Err(anyhow!(
                "explicit TOML config path {} does not exist",
                explicit_path.display()
            )),
            file_name: Some(explicit_path),
            lua: None,
            warnings: vec![],
        },
        Err(err) => LoadedConfig {
            config: Err(err),
            file_name: Some(explicit_path),
            lua: None,
            warnings: vec![],
        },
    }
}

/// Build the list of paths to search for `frankenterm.toml`.
fn toml_config_search_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    paths.extend(
        frankenterm_config_dirs()
            .into_iter()
            .map(|dir| dir.join("frankenterm.toml")),
    );

    // Home directory dotfile
    paths.push(HOME_DIR.join(".frankenterm.toml"));

    paths
}

/// Try to load a single TOML config file.
///
/// Returns `Ok(None)` if the file does not exist, `Ok(Some(loaded))` on
/// success, or `Err` on parse/conversion failure.
pub(crate) fn parse_toml_config_with_overrides(
    content: &str,
    overrides: &Value,
) -> anyhow::Result<Config> {
    let toml_value: toml::Value = content
        .parse()
        .context("Error parsing TOML config content")?;

    let mut dynamic = toml_to_dynamic(&toml_value);
    merge_dynamic_overrides(&mut dynamic, overrides);

    let cfg = Config::from_dynamic(
        &dynamic,
        FromDynamicOptions {
            unknown_fields: UnknownFieldAction::Warn,
            deprecated_fields: UnknownFieldAction::Warn,
        },
    )
    .context("Error converting TOML config to Config struct")?;

    cfg.check_consistency()
        .context("check_consistency on TOML config")?;

    // Compute but discard the key bindings here so that we raise any
    // problems earlier than we use them.
    let _ = cfg.key_bindings();

    Ok(cfg)
}

fn try_load_toml_file(path: &Path, overrides: &Value) -> anyhow::Result<Option<LoadedConfig>> {
    let content = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => anyhow::bail!("Error reading {}: {}", path.display(), err),
    };

    let (config, warnings) =
        frankenterm_dynamic::Error::capture_warnings(|| -> anyhow::Result<Config> {
            let cfg = parse_toml_config_with_overrides(&content, overrides)
                .with_context(|| format!("Error loading TOML config from {}", path.display()))?;

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
    use crate::merge_dynamic_overrides;
    use frankenterm_dynamic::ToDynamic;
    use proptest::prelude::*;
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::fmt::Write as _;
    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set<V>(key: &'static str, value: V) -> Self
        where
            V: AsRef<OsStr>,
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
            if let Some(prev) = &self.previous {
                std::env::set_var(self.key, prev);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    struct ConfigFileOverrideGuard {
        previous: Option<PathBuf>,
    }

    impl ConfigFileOverrideGuard {
        fn set(path: Option<&Path>) -> Self {
            let mut override_lock = crate::CONFIG_FILE_OVERRIDE.lock().unwrap();
            let previous = override_lock.clone();
            *override_lock = path.map(Path::to_path_buf);
            Self { previous }
        }
    }

    impl Drop for ConfigFileOverrideGuard {
        fn drop(&mut self) {
            if let Ok(mut guard) = crate::CONFIG_FILE_OVERRIDE.lock() {
                *guard = self.previous.clone();
            }
        }
    }

    #[derive(Debug, Clone)]
    struct RoundTripCase {
        scrollback_lines: u32,
        scrollback_tiered_enabled: bool,
        font_size_tenths: u16,
        enable_scroll_bar: bool,
        initial_rows: u16,
        initial_cols: u16,
        color_scheme: String,
    }

    fn round_trip_case_strategy() -> impl Strategy<Value = RoundTripCase> {
        (
            1u32..200_000,
            any::<bool>(),
            80u16..360u16,
            any::<bool>(),
            10u16..120u16,
            10u16..240u16,
            "[A-Za-z0-9 _-]{1,24}",
        )
            .prop_map(
                |(
                    scrollback_lines,
                    scrollback_tiered_enabled,
                    font_size_tenths,
                    enable_scroll_bar,
                    initial_rows,
                    initial_cols,
                    color_scheme,
                )| RoundTripCase {
                    scrollback_lines,
                    scrollback_tiered_enabled,
                    font_size_tenths,
                    enable_scroll_bar,
                    initial_rows,
                    initial_cols,
                    color_scheme,
                },
            )
    }

    fn render_round_trip_toml(case: &RoundTripCase, presence_mask: u8) -> String {
        let mut toml = String::new();

        if presence_mask & 0b000001 != 0 {
            let _ = writeln!(toml, "scrollback_lines = {}", case.scrollback_lines);
        }
        if presence_mask & 0b000010 != 0 {
            let _ = writeln!(
                toml,
                "scrollback_tiered_enabled = {}",
                case.scrollback_tiered_enabled
            );
        }
        if presence_mask & 0b000100 != 0 {
            let _ = writeln!(
                toml,
                "font_size = {:.1}",
                f64::from(case.font_size_tenths) / 10.0
            );
        }
        if presence_mask & 0b001000 != 0 {
            let _ = writeln!(toml, "enable_scroll_bar = {}", case.enable_scroll_bar);
        }
        if presence_mask & 0b010000 != 0 {
            let _ = writeln!(toml, "initial_rows = {}", case.initial_rows);
        }
        if presence_mask & 0b100000 != 0 {
            let _ = writeln!(toml, "initial_cols = {}", case.initial_cols);
        }
        if presence_mask & 0b1000000 != 0 {
            let _ = writeln!(toml, "color_scheme = {:?}", case.color_scheme);
        }

        toml
    }

    fn render_config_scalar_subset_toml(config: &Config) -> String {
        let mut toml = String::new();

        let _ = writeln!(toml, "scrollback_lines = {}", config.scrollback_lines);
        let _ = writeln!(
            toml,
            "scrollback_tiered_enabled = {}",
            config.scrollback_tiered_enabled
        );
        let _ = writeln!(toml, "font_size = {:.1}", config.font_size);
        let _ = writeln!(toml, "enable_scroll_bar = {}", config.enable_scroll_bar);
        let _ = writeln!(toml, "initial_rows = {}", config.initial_rows);
        let _ = writeln!(toml, "initial_cols = {}", config.initial_cols);
        if let Some(color_scheme) = &config.color_scheme {
            let _ = writeln!(toml, "color_scheme = {:?}", color_scheme);
        }

        toml
    }

    fn override_layer_strategy() -> impl Strategy<Value = Value> {
        prop::collection::btree_map(
            prop::sample::select(vec![
                "scrollback_lines".to_string(),
                "scrollback_tiered_enabled".to_string(),
                "font_size".to_string(),
                "enable_scroll_bar".to_string(),
                "initial_rows".to_string(),
                "initial_cols".to_string(),
            ]),
            prop_oneof![
                (1u64..200_000u64).prop_map(|value| value.to_dynamic()),
                any::<bool>().prop_map(|value| value.to_dynamic()),
                (80u16..360u16).prop_map(|value| (f64::from(value) / 10.0).to_dynamic()),
                "[A-Za-z0-9 _-]{1,24}".prop_map(|value| value.to_dynamic()),
            ],
            0..7,
        )
        .prop_map(|entries| {
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (Value::String(key), value))
                    .collect::<BTreeMap<_, _>>()
                    .into(),
            )
        })
    }

    fn escaped_toml_string_strategy() -> impl Strategy<Value = String> {
        let alphabet: Vec<char> =
            " \t\"\\abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_-./:"
                .chars()
                .collect();
        prop::collection::vec(prop::sample::select(alphabet), 1..64)
            .prop_map(|chars| chars.into_iter().collect())
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
scrollback_tiered_enabled = true
scrollback_hot_lines = 1200
scrollback_warm_max_mb = 48
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
        assert!(cfg.scrollback_tiered_enabled);
        assert_eq!(cfg.scrollback_hot_lines, 1200);
        assert_eq!(cfg.scrollback_warm_max_mb, 48);
        assert!((cfg.font_size - 14.0).abs() < 0.01);
        assert_eq!(cfg.color_scheme.as_deref(), Some("Catppuccin Mocha"));
    }

    #[test]
    fn agent_detection_config_from_toml() {
        let toml_str = r#"
agent_detection_enabled = true
agent_active_threshold_ms = 3000
agent_thinking_threshold_ms = 8000
agent_stuck_threshold_ms = 45000
agent_idle_threshold_ms = 90000
agent_show_name_overlay = false
agent_show_backpressure = true
agent_border_width = 3
agent_auto_layout = "by_activity"
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
        assert!(cfg.agent_detection_enabled);
        assert_eq!(cfg.agent_active_threshold_ms, 3000);
        assert_eq!(cfg.agent_thinking_threshold_ms, 8000);
        assert_eq!(cfg.agent_stuck_threshold_ms, 45000);
        assert_eq!(cfg.agent_idle_threshold_ms, 90000);
        assert!(!cfg.agent_show_name_overlay);
        assert!(cfg.agent_show_backpressure);
        assert_eq!(cfg.agent_border_width, 3);
        assert_eq!(cfg.agent_auto_layout, "by_activity");
    }

    #[test]
    fn toml_search_paths_returns_nonempty() {
        let paths = toml_config_search_paths();
        // Should always have at least the home dir dotfile
        assert!(!paths.is_empty());
        assert!(paths.last().unwrap().ends_with(".frankenterm.toml"));
    }

    #[test]
    fn toml_search_paths_use_native_xdg_home() {
        let _env_lock = crate::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _xdg_home = EnvVarGuard::set("XDG_CONFIG_HOME", dir.path());
        let _xdg_dirs = EnvVarGuard::unset("XDG_CONFIG_DIRS");

        let paths = toml_config_search_paths();
        assert_eq!(paths[0], dir.path().join("frankenterm/frankenterm.toml"));
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
        let _env_lock = crate::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing-config.toml");
        let _guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", &missing_path);

        let loaded = try_load_toml_config(&Value::default()).expect("expected explicit env result");
        assert_eq!(loaded.file_name, Some(missing_path));
        assert!(loaded.config.is_err());
    }

    #[test]
    fn explicit_env_toml_extension_is_case_insensitive() {
        let _env_lock = crate::test_env_lock();
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
    fn explicit_override_missing_toml_returns_error() {
        let _env_lock = crate::test_env_lock();
        let _env_guard = EnvVarGuard::set(
            "FRANKENTERM_CONFIG_FILE",
            Path::new("/tmp/this_should_not_be_considered.toml"),
        );

        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().join("missing-override.toml");
        let _override_guard = ConfigFileOverrideGuard::set(Some(&missing_path));

        let loaded = try_load_toml_config(&Value::default()).expect("expected override result");
        assert_eq!(loaded.file_name, Some(missing_path));
        assert!(loaded.config.is_err());
    }

    #[test]
    fn non_toml_override_skips_toml_loader_even_with_env_toml() {
        let _env_lock = crate::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let env_toml = dir.path().join("from-env.toml");
        std::fs::write(&env_toml, "scrollback_lines = 1111\n").unwrap();
        let _env_guard = EnvVarGuard::set("FRANKENTERM_CONFIG_FILE", &env_toml);

        let override_lua = dir.path().join("override.lua");
        let _override_guard = ConfigFileOverrideGuard::set(Some(&override_lua));

        let loaded = try_load_toml_config(&Value::default());
        assert!(
            loaded.is_none(),
            "non-TOML override should bypass TOML loader"
        );
    }

    #[test]
    fn full_config_loads_all_fields() {
        let toml_str = r#"
scrollback_lines = 50000
font_size = 16.0
color_scheme = "Builtin Dark"
font_dirs = ["fonts"]
enable_scroll_bar = true
enable_tab_bar = true
hide_tab_bar_if_only_one_tab = true
tab_bar_at_bottom = false
initial_rows = 40
initial_cols = 120
window_close_confirmation = "AlwaysPrompt"
skip_close_confirmation_for_processes_named = []
check_for_updates = false
automatically_reload_config = true
max_fps = 60

[window_padding]
left = 4
right = 4
top = 4
bottom = 4

[[font.font]]
family = "JetBrains Mono"
harfbuzz_features = ["calt=0", "clig=0", "liga=0"]

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
        assert_eq!(cfg.scrollback_lines, 50000);
        assert!((cfg.font_size - 16.0).abs() < 0.01);
        assert_eq!(cfg.color_scheme.as_deref(), Some("Builtin Dark"));
        assert_eq!(cfg.font_dirs, vec![std::path::PathBuf::from("fonts")]);
        assert!(cfg.enable_scroll_bar);
        assert_eq!(cfg.initial_rows, 40);
        assert_eq!(cfg.initial_cols, 120);
        assert!(matches!(
            cfg.window_close_confirmation,
            crate::WindowCloseConfirmation::AlwaysPrompt
        ));
        assert!(cfg.skip_close_confirmation_for_processes_named.is_empty());
        assert_eq!(cfg.font.font[0].family, "JetBrains Mono");
        assert_eq!(
            cfg.font.font[0].harfbuzz_features.as_deref(),
            Some(
                &[
                    "calt=0".to_string(),
                    "clig=0".to_string(),
                    "liga=0".to_string()
                ][..]
            )
        );
    }

    #[test]
    fn type_mismatch_produces_error() {
        // scrollback_lines should be integer, not string
        let toml_str = r#"scrollback_lines = "not a number""#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let result = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn default_values_for_unpopulated_keys() {
        let toml_str = "color_scheme = \"Test\"\n";
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
        // scrollback_lines should have a default > 0
        assert!(cfg.scrollback_lines > 0);
        // font_size should have a reasonable default
        assert!(cfg.font_size > 0.0);
        // initial_rows/cols should have defaults
        assert!(cfg.initial_rows > 0);
        assert!(cfg.initial_cols > 0);
    }

    #[test]
    fn window_padding_config_loads() {
        let toml_str = r#"
[window_padding]
left = 8
right = 8
top = 12
bottom = 12
"#;
        let toml_value: toml::Value = toml_str.parse().unwrap();
        let dynamic = toml_to_dynamic(&toml_value);
        let cfg = Config::from_dynamic(
            &dynamic,
            FromDynamicOptions {
                unknown_fields: UnknownFieldAction::Warn,
                deprecated_fields: UnknownFieldAction::Warn,
            },
        );
        assert!(
            cfg.is_ok(),
            "window_padding config should parse without error"
        );
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

    #[test]
    fn resize_wrap_scorecard_config_loads() {
        // Use non-default values to prove TOML parsing overrides defaults
        // (defaults are: 500, 2000, 20)
        let toml_str = r#"
resize_wrap_scorecard_enabled = true
resize_wrap_readability_gate_enabled = true
resize_wrap_readability_max_line_badness_delta = 1000
resize_wrap_readability_max_total_badness_delta = 5000
resize_wrap_readability_max_fallback_ratio_percent = 40
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
        assert!(cfg.resize_wrap_scorecard_enabled);
        assert!(cfg.resize_wrap_readability_gate_enabled);
        assert_eq!(cfg.resize_wrap_readability_max_line_badness_delta, 1000);
        assert_eq!(cfg.resize_wrap_readability_max_total_badness_delta, 5000);
        assert_eq!(cfg.resize_wrap_readability_max_fallback_ratio_percent, 40);
    }

    #[test]
    fn resize_wrap_kp_cost_model_config_loads() {
        let toml_str = r#"
resize_wrap_kp_badness_scale = 20000
resize_wrap_kp_forced_break_penalty = 10000
resize_wrap_kp_lookahead_limit = 128
resize_wrap_kp_max_dp_states = 16384
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
        assert_eq!(cfg.resize_wrap_kp_badness_scale, 20000);
        assert_eq!(cfg.resize_wrap_kp_forced_break_penalty, 10000);
        assert_eq!(cfg.resize_wrap_kp_lookahead_limit, 128);
        assert_eq!(cfg.resize_wrap_kp_max_dp_states, 16384);
    }

    #[test]
    fn resize_wrap_defaults_when_not_specified() {
        let toml_str = r#"
scrollback_lines = 5000
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
        // Defaults: scorecard and gate are enabled for resize quality telemetry
        assert!(cfg.resize_wrap_scorecard_enabled);
        assert!(cfg.resize_wrap_readability_gate_enabled);
        // Default KP cost model values
        assert_eq!(cfg.resize_wrap_kp_badness_scale, 10_000);
        assert_eq!(cfg.resize_wrap_kp_forced_break_penalty, 5_000);
        assert_eq!(cfg.resize_wrap_kp_lookahead_limit, 64);
        assert_eq!(cfg.resize_wrap_kp_max_dp_states, 8_192);
        // Default gate thresholds (sensible production values)
        assert_eq!(cfg.resize_wrap_readability_max_line_badness_delta, 500);
        assert_eq!(cfg.resize_wrap_readability_max_total_badness_delta, 2000);
        assert_eq!(cfg.resize_wrap_readability_max_fallback_ratio_percent, 20);
    }

    #[test]
    fn resize_wrap_fallback_ratio_percent_clamped() {
        // Values above 100 should still parse (validation happens separately)
        let toml_str = r#"
resize_wrap_readability_max_fallback_ratio_percent = 50
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
        assert_eq!(cfg.resize_wrap_readability_max_fallback_ratio_percent, 50);
    }

    #[test]
    fn resize_wrap_scorecard_can_be_explicitly_disabled() {
        let toml_str = r#"
resize_wrap_scorecard_enabled = false
resize_wrap_readability_gate_enabled = false
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
        assert!(!cfg.resize_wrap_scorecard_enabled);
        assert!(!cfg.resize_wrap_readability_gate_enabled);
    }

    #[test]
    fn ssh_domain_config_parses() {
        let toml_str = r#"
[[ssh_domains]]
name = "production"
remote_address = "10.0.0.5:22"
username = "deploy"
connect_automatically = true

[[ssh_domains]]
name = "staging"
remote_address = "staging.example.com"
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
        let domains = cfg.ssh_domains.expect("ssh_domains should be Some");
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0].name, "production");
        assert_eq!(domains[0].remote_address, "10.0.0.5:22");
        assert_eq!(domains[0].username.as_deref(), Some("deploy"));
        // connect_automatically defaults to false; verify TOML override to true
        assert!(domains[0].connect_automatically);
        assert_eq!(domains[1].name, "staging");
        assert_eq!(domains[1].remote_address, "staging.example.com");
        assert!(domains[1].username.is_none());
        // staging doesn't set connect_automatically, so it should default to false
        assert!(!domains[1].connect_automatically);
    }

    #[test]
    fn ssh_domain_defaults_from_ssh_config() {
        // When no ssh_domains specified, ssh_domains() auto-discovers from ~/.ssh/config
        let toml_str = r#"
scrollback_lines = 5000
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
        // When not explicitly set, ssh_domains is None (auto-discovered at runtime)
        assert!(cfg.ssh_domains.is_none());
        // The ssh_domains() method will auto-discover; just verify the field is unset
    }

    #[test]
    fn ssh_domain_with_options() {
        let toml_str = r#"
[[ssh_domains]]
name = "custom"
remote_address = "myhost.local"
multiplexing = "None"
no_agent_auth = true
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
        let domains = cfg.ssh_domains.expect("ssh_domains should be Some");
        assert_eq!(domains.len(), 1);
        assert_eq!(domains[0].name, "custom");
        assert!(domains[0].no_agent_auth);
        assert_eq!(domains[0].multiplexing, crate::SshMultiplexing::None);
    }

    #[test]
    fn ssh_domain_config_file_override_parses() {
        let toml_str = r#"
[[ssh_domains]]
name = "custom"
remote_address = "myhost.local"
ssh_config_file = "/tmp/ft-ssh-config"
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
        let domains = cfg.ssh_domains.expect("ssh_domains should be Some");
        assert_eq!(
            domains[0].ssh_config_file.as_deref(),
            Some("/tmp/ft-ssh-config")
        );
    }

    proptest! {
        #[test]
        fn generated_round_trip_fields_are_preserved(case in round_trip_case_strategy()) {
            let cfg = parse_toml_config_with_overrides(
                &render_round_trip_toml(&case, 0b111_1111),
                &Value::default(),
            )
            .unwrap()
            .compute_extra_defaults(None);

            prop_assert_eq!(cfg.scrollback_lines, case.scrollback_lines as usize);
            prop_assert_eq!(cfg.scrollback_tiered_enabled, case.scrollback_tiered_enabled);
            prop_assert_eq!(
                (cfg.font_size * 10.0).round() as u16,
                case.font_size_tenths
            );
            prop_assert_eq!(cfg.enable_scroll_bar, case.enable_scroll_bar);
            prop_assert_eq!(cfg.initial_rows, case.initial_rows);
            prop_assert_eq!(cfg.initial_cols, case.initial_cols);
            prop_assert_eq!(cfg.color_scheme, Some(case.color_scheme));
        }

        #[test]
        fn omitted_fields_fall_back_to_defaults(case in round_trip_case_strategy(), presence_mask in 0u8..128u8) {
            let cfg = parse_toml_config_with_overrides(
                &render_round_trip_toml(&case, presence_mask),
                &Value::default(),
            )
            .unwrap()
            .compute_extra_defaults(None);
            let defaults = Config::default_config();

            prop_assert_eq!(
                cfg.scrollback_lines,
                if presence_mask & 0b000001 != 0 {
                    case.scrollback_lines as usize
                } else {
                    defaults.scrollback_lines
                }
            );
            prop_assert_eq!(
                cfg.scrollback_tiered_enabled,
                if presence_mask & 0b000010 != 0 {
                    case.scrollback_tiered_enabled
                } else {
                    defaults.scrollback_tiered_enabled
                }
            );
            prop_assert_eq!(
                (cfg.font_size * 10.0).round() as u16,
                if presence_mask & 0b000100 != 0 {
                    case.font_size_tenths
                } else {
                    (defaults.font_size * 10.0).round() as u16
                }
            );
            prop_assert_eq!(
                cfg.enable_scroll_bar,
                if presence_mask & 0b001000 != 0 {
                    case.enable_scroll_bar
                } else {
                    defaults.enable_scroll_bar
                }
            );
            prop_assert_eq!(
                cfg.initial_rows,
                if presence_mask & 0b010000 != 0 {
                    case.initial_rows
                } else {
                    defaults.initial_rows
                }
            );
            prop_assert_eq!(
                cfg.initial_cols,
                if presence_mask & 0b100000 != 0 {
                    case.initial_cols
                } else {
                    defaults.initial_cols
                }
            );
            prop_assert_eq!(
                cfg.color_scheme,
                if presence_mask & 0b1000000 != 0 {
                    Some(case.color_scheme.clone())
                } else {
                    defaults.color_scheme.clone()
                }
            );
        }

        #[test]
        fn escaped_toml_string_scalars_survive_parser(color_scheme in escaped_toml_string_strategy()) {
            let mut doc = BTreeMap::new();
            doc.insert("color_scheme".to_string(), color_scheme.clone());
            let toml = toml::to_string(&doc).unwrap();

            let cfg = parse_toml_config_with_overrides(&toml, &Value::default())
                .unwrap()
                .compute_extra_defaults(None);

            prop_assert_eq!(cfg.color_scheme, Some(color_scheme));
        }

        #[test]
        fn generated_config_scalar_subset_parse_write_parse_roundtrip(
            case in round_trip_case_strategy(),
            presence_mask in 0u8..128u8,
        ) {
            let cfg = parse_toml_config_with_overrides(
                &render_round_trip_toml(&case, presence_mask),
                &Value::default(),
            )
            .unwrap()
            .compute_extra_defaults(None);

            let emitted_toml = render_config_scalar_subset_toml(&cfg);
            let reparsed = parse_toml_config_with_overrides(&emitted_toml, &Value::default())
                .unwrap()
                .compute_extra_defaults(None);

            prop_assert_eq!(reparsed.scrollback_lines, cfg.scrollback_lines);
            prop_assert_eq!(
                reparsed.scrollback_tiered_enabled,
                cfg.scrollback_tiered_enabled
            );
            prop_assert_eq!(
                (reparsed.font_size * 10.0).round() as u16,
                (cfg.font_size * 10.0).round() as u16,
            );
            prop_assert_eq!(reparsed.enable_scroll_bar, cfg.enable_scroll_bar);
            prop_assert_eq!(reparsed.initial_rows, cfg.initial_rows);
            prop_assert_eq!(reparsed.initial_cols, cfg.initial_cols);
            prop_assert_eq!(reparsed.color_scheme, cfg.color_scheme);
        }

        #[test]
        fn merge_overrides_is_associative_and_idempotent(
            base in override_layer_strategy(),
            file in override_layer_strategy(),
            env in override_layer_strategy(),
            cli in override_layer_strategy(),
        ) {
            let mut sequential = base.clone();
            merge_dynamic_overrides(&mut sequential, &file);
            merge_dynamic_overrides(&mut sequential, &env);
            merge_dynamic_overrides(&mut sequential, &cli);

            let mut combined = file.clone();
            merge_dynamic_overrides(&mut combined, &env);
            merge_dynamic_overrides(&mut combined, &cli);

            let mut grouped = base.clone();
            merge_dynamic_overrides(&mut grouped, &combined);

            prop_assert_eq!(sequential, grouped);

            let mut once = base.clone();
            merge_dynamic_overrides(&mut once, &env);

            let mut twice = once.clone();
            merge_dynamic_overrides(&mut twice, &env);

            prop_assert_eq!(once, twice);
        }
    }
}
