//! Real-renderer scenes for the renderer image-parity corpus (ft-yccm0.1.10).
//!
//! A scene is a byte stream plus configuration. [`render_scene_snapshot`]
//! launches the real `frankenterm-gui` binary with a pinned configuration
//! (bundled fonts at a fixed size and DPI, no blinking, no animation) in a
//! throwaway `HOME`, plays the scene into a fresh pane, and collects the PNG
//! that the GUI's snapshot hook writes when the scene's final `OSC 2` title
//! sentinel arrives (see `termwindow::render_snapshot`). The frame therefore
//! comes from the production font, shaping, atlas and shader path, not from
//! the synthetic rasterizer in `headless_render`.
//!
//! The child gets a cleared environment, so it can never attach to, or
//! publish itself as, the operator's mux, and `--always-new-process` keeps it
//! out of any running GUI. It runs non-activating
//! (`FRANKENTERM_NATIVE_E2E_NONACTIVATING=1`), so it never takes keyboard
//! focus; its window renders unfocused unless the scene asks the snapshot
//! hook to render it focused ([`SceneActions::focus`], which changes only the
//! window's own focus state).
//!
//! [`SceneActions`] also covers the state bytes cannot create: a selection,
//! and a split whose second pane plays its own bytes. String config values
//! and `extra_toml` may name committed fixtures as `${FIXTURES}/<file>`
//! ([`FIXTURES_DIR`]).

use anyhow::Context;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The title a scene sets last; the GUI snapshots the next presented frame.
pub const SNAPSHOT_TITLE: &str = "ft-render-snapshot";
/// The title that makes the GUI perform a scene's split (the GUI's
/// `render_snapshot::SPLIT_TITLE`).
pub const SPLIT_TITLE: &str = "ft-render-split";
/// Committed fixture files scenes refer to as `${FIXTURES}`, relative to the
/// repository root.
pub const FIXTURES_DIR: &str = "tests/golden/gpu/real/fixtures";
/// The placeholder for [`FIXTURES_DIR`] in scene configuration.
pub const FIXTURES_PLACEHOLDER: &str = "${FIXTURES}";
/// Bundled fonts, relative to the repository root.
pub const BUNDLED_FONTS_DIR: &str = "frankenterm/assets/fonts";
/// The pinned font files the base configuration resolves glyphs from.
pub const PINNED_FONT_FILES: &[&str] = &[
    "JetBrainsMono-Regular.ttf",
    "JetBrainsMono-Bold.ttf",
    "JetBrainsMono-Italic.ttf",
    "JetBrainsMono-BoldItalic.ttf",
    "FiraCode-Regular.ttf",
    "NotoColorEmoji.ttf",
    "SymbolsNerdFontMono-Regular.ttf",
];
/// macOS system fonts the base configuration names for CJK. Their SHA-256
/// is recorded too, so an OS font update shows up as an explained delta.
pub const SYSTEM_FALLBACK_FONT_FILES: &[&str] = &[
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/AppleSDGothicNeo.ttc",
];
/// The rasterizer a golden is produced by unless its scene selects another:
/// the base configuration pins FreeType. Goldens record their scene's
/// identity ([`rasterizer_identity`]) so a rasterizer switch, such as Track
/// C's CoreText, is an explained delta rather than a mystery.
pub const RASTERIZER_IDENTITY: &str =
    "frankenterm-gui WebGpu front end; FreeType rasterizer; HarfBuzz shaper";

/// The rasterizer identity a scene's golden records: [`RASTERIZER_IDENTITY`]
/// for the base FreeType rasterizer, otherwise the same line naming the
/// rasterizer the scene's `font_rasterizer` selects (ft-yccm0.4.3.1).
pub fn rasterizer_identity(scene: &SceneSpec) -> String {
    let rasterizer = scene
        .config
        .get("font_rasterizer")
        .cloned()
        .or_else(|| base_config().remove("font_rasterizer"))
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "FreeType".to_string());
    format!("frankenterm-gui WebGpu front end; {rasterizer} rasterizer; HarfBuzz shaper")
}

/// One font in the golden's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FontFileIdentity {
    pub path: String,
    pub sha256: String,
}

/// A configured font family for a scene's font stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneFont {
    pub family: String,
    #[serde(default)]
    pub harfbuzz_features: Vec<String>,
}

/// A scene: the bytes written to the pane, plus configuration changes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SceneSpec {
    /// Bytes the scene writes to the terminal (escapes as `\u001b`).
    pub text: String,
    /// Top-level configuration keys overriding the base (scalars and arrays
    /// of scalars).
    #[serde(default)]
    pub config: BTreeMap<String, serde_json::Value>,
    /// Replaces the base font stack (for example to turn ligatures on).
    #[serde(default)]
    pub fonts: Option<Vec<SceneFont>>,
    /// Extra TOML appended after the generated configuration, for tables
    /// such as `[window_background_gradient]`.
    #[serde(default)]
    pub extra_toml: Option<String>,
    /// Window state the snapshot hook sets up first.
    #[serde(default, skip_serializing_if = "SceneActions::is_empty")]
    pub snapshot: SceneActions,
}

/// Window state a scene's bytes cannot create, set up by the GUI's snapshot
/// hook (its `FRANKENTERM_RENDER_SNAPSHOT_*` variables).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneActions {
    /// Render the window as focused (focused cursor shapes).
    #[serde(default)]
    pub focus: bool,
    /// `[start_row, start_col, end_row, end_col]` in visible cells, inclusive.
    #[serde(default)]
    pub selection: Option<[u32; 4]>,
    /// Split the pane once the scene's bytes are shown.
    #[serde(default)]
    pub split: Option<SceneSplit>,
    /// Open this modal overlay before the snapshot: `char_select`,
    /// `pane_select` or `command_palette` (ft-yccm0.4.7.1).
    #[serde(default)]
    pub modal: Option<String>,
}

impl SceneActions {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A split: the new pane goes `right` or `bottom` and plays `text`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SceneSplit {
    pub direction: String,
    pub text: String,
}

/// The snapshot-hook variables for `actions`. `split_scene` is the file the
/// split pane plays.
pub fn snapshot_action_env(
    actions: &SceneActions,
    split_scene: &Path,
) -> anyhow::Result<Vec<(String, String)>> {
    let mut env = Vec::new();
    if actions.focus {
        env.push((
            "FRANKENTERM_RENDER_SNAPSHOT_FOCUS".to_string(),
            "1".to_string(),
        ));
    }
    if let Some([start_row, start_col, end_row, end_col]) = actions.selection {
        anyhow::ensure!(
            (end_row, end_col) >= (start_row, start_col),
            "the scene selection ends before it starts"
        );
        env.push((
            "FRANKENTERM_RENDER_SNAPSHOT_SELECTION".to_string(),
            format!("{start_row},{start_col},{end_row},{end_col}"),
        ));
    }
    if let Some(split) = &actions.split {
        anyhow::ensure!(
            matches!(split.direction.as_str(), "right" | "bottom"),
            "split direction {:?} is not right or bottom",
            split.direction
        );
        let path = split_scene.display().to_string();
        anyhow::ensure!(
            !path.contains('\''),
            "the split scene path {path:?} contains a single quote"
        );
        env.push((
            "FRANKENTERM_RENDER_SNAPSHOT_SPLIT".to_string(),
            split.direction.clone(),
        ));
        env.push((
            "FRANKENTERM_RENDER_SNAPSHOT_SPLIT_COMMAND".to_string(),
            format!("cat '{path}'; printf '\\033]2;{SNAPSHOT_TITLE}\\007'; exec sleep 600"),
        ));
    }
    if let Some(modal) = &actions.modal {
        anyhow::ensure!(
            matches!(
                modal.as_str(),
                "char_select" | "pane_select" | "command_palette"
            ),
            "modal {modal:?} is not char_select, pane_select or command_palette"
        );
        env.push((
            "FRANKENTERM_RENDER_SNAPSHOT_MODAL".to_string(),
            modal.clone(),
        ));
    }
    Ok(env)
}

/// Replaces [`FIXTURES_PLACEHOLDER`] in generated TOML with `fixtures_dir`.
pub fn substitute_fixtures(toml: &str, fixtures_dir: &Path) -> anyhow::Result<String> {
    let dir = fixtures_dir.display().to_string();
    anyhow::ensure!(
        !dir.contains(['"', '\\']),
        "the fixtures directory {dir:?} cannot appear in a TOML string"
    );
    Ok(toml.replace(FIXTURES_PLACEHOLDER, &dir))
}

/// The base font stack: JetBrains Mono without ligatures, then emoji, Nerd
/// symbols, and the macOS CJK families.
pub fn base_fonts() -> Vec<SceneFont> {
    let plain = |family: &str| SceneFont {
        family: family.to_string(),
        harfbuzz_features: Vec::new(),
    };
    vec![
        SceneFont {
            family: "JetBrains Mono".to_string(),
            harfbuzz_features: ["calt=0", "clig=0", "liga=0"].map(String::from).to_vec(),
        },
        plain("Noto Color Emoji"),
        plain("Symbols Nerd Font Mono"),
        plain("Hiragino Sans GB"),
        plain("Apple SD Gothic Neo"),
    ]
}

/// The base configuration every scene starts from: fixed font size and DPI,
/// no tab bar or scroll bar, no blinking, no animation, WebGpu, and the
/// FreeType rasterizer named by [`RASTERIZER_IDENTITY`], so a change of the
/// platform's default rasterizer cannot silently re-render the goldens.
pub fn base_config() -> BTreeMap<String, serde_json::Value> {
    use serde_json::json;
    BTreeMap::from([
        ("font_size".to_string(), json!(13.0)),
        ("dpi".to_string(), json!(144.0)),
        ("line_height".to_string(), json!(1.0)),
        ("initial_cols".to_string(), json!(64)),
        ("initial_rows".to_string(), json!(12)),
        ("enable_tab_bar".to_string(), json!(false)),
        ("enable_scroll_bar".to_string(), json!(false)),
        ("window_decorations".to_string(), json!("RESIZE")),
        (
            "window_close_confirmation".to_string(),
            json!("NeverPrompt"),
        ),
        ("cursor_blink_rate".to_string(), json!(0)),
        ("text_blink_rate".to_string(), json!(0)),
        ("text_blink_rate_rapid".to_string(), json!(0)),
        ("animation_fps".to_string(), json!(1)),
        ("front_end".to_string(), json!("WebGpu")),
        ("font_rasterizer".to_string(), json!("FreeType")),
        ("check_for_updates".to_string(), json!(false)),
        ("automatically_reload_config".to_string(), json!(false)),
        ("color_scheme".to_string(), json!("Builtin Dark")),
        ("window_background_opacity".to_string(), json!(1.0)),
    ])
}

/// A TOML literal for a scalar or an array of scalars.
fn toml_literal(value: &serde_json::Value) -> anyhow::Result<String> {
    use serde_json::Value;
    Ok(match value {
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        // A JSON string literal is a valid TOML basic string for every
        // string serde_json emits.
        Value::String(_) => serde_json::to_string(value)?,
        Value::Array(items) => {
            let items = items
                .iter()
                .map(toml_literal)
                .collect::<anyhow::Result<Vec<_>>>()?;
            format!("[{}]", items.join(", "))
        }
        Value::Null | Value::Object(_) => {
            anyhow::bail!("scene config values must be scalars or arrays, got {value}")
        }
    })
}

/// Renders the scene's full TOML configuration.
pub fn scene_config_toml(fonts_dir: &Path, scene: &SceneSpec) -> anyhow::Result<String> {
    let mut config = base_config();
    for (key, value) in &scene.config {
        config.insert(key.clone(), value.clone());
    }
    let mut toml = String::new();
    toml.push_str(&format!(
        "font_dirs = [{}]\n",
        toml_literal(&serde_json::Value::String(fonts_dir.display().to_string()))?
    ));
    for (key, value) in &config {
        anyhow::ensure!(
            key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
            "scene config key {key:?} is not a bare TOML key"
        );
        toml.push_str(&format!("{key} = {}\n", toml_literal(value)?));
    }
    toml.push_str("\n[window_padding]\nleft = 8\nright = 8\ntop = 8\nbottom = 8\n");
    let fonts = scene.fonts.clone().unwrap_or_else(base_fonts);
    for font in &fonts {
        toml.push_str("\n[[font.font]]\n");
        toml.push_str(&format!(
            "family = {}\n",
            toml_literal(&serde_json::Value::String(font.family.clone()))?
        ));
        if !font.harfbuzz_features.is_empty() {
            let features = serde_json::Value::from(font.harfbuzz_features.clone());
            toml.push_str(&format!(
                "harfbuzz_features = {}\n",
                toml_literal(&features)?
            ));
        }
    }
    if let Some(extra) = &scene.extra_toml {
        toml.push('\n');
        toml.push_str(extra);
        toml.push('\n');
    }
    Ok(toml)
}

/// SHA-256 of a file, as lowercase hex.
pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The golden identity of the fonts a scene can resolve: every pinned bundled
/// file, plus the system CJK fallbacks that exist on this host.
pub fn font_identity(repo_root: &Path) -> anyhow::Result<Vec<FontFileIdentity>> {
    let fonts_dir = repo_root.join(BUNDLED_FONTS_DIR);
    let mut identity = Vec::new();
    for file in PINNED_FONT_FILES {
        identity.push(FontFileIdentity {
            path: format!("{BUNDLED_FONTS_DIR}/{file}"),
            sha256: sha256_file(&fonts_dir.join(file))?,
        });
    }
    for path in SYSTEM_FALLBACK_FONT_FILES {
        let path = Path::new(path);
        if path.exists() {
            identity.push(FontFileIdentity {
                path: path.display().to_string(),
                sha256: sha256_file(path)?,
            });
        }
    }
    Ok(identity)
}

/// One SHA-256 over the whole font identity, for `meta.json`'s
/// `font_set_sha`.
pub fn font_set_sha(identity: &[FontFileIdentity]) -> String {
    let mut hasher = Sha256::new();
    for font in identity {
        hasher.update(font.path.as_bytes());
        hasher.update([0]);
        hasher.update(font.sha256.as_bytes());
        hasher.update([0]);
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("sha256:{hex}")
}

/// Which font files differ between a golden's identity and the current one.
pub fn font_identity_changes(
    golden: &[FontFileIdentity],
    current: &[FontFileIdentity],
) -> Vec<String> {
    let golden: BTreeMap<_, _> = golden.iter().map(|f| (&f.path, &f.sha256)).collect();
    let current: BTreeMap<_, _> = current.iter().map(|f| (&f.path, &f.sha256)).collect();
    let mut changes = Vec::new();
    for (path, sha) in &current {
        match golden.get(path) {
            None => changes.push(format!("{path}: added")),
            Some(old) if old != sha => changes.push(format!("{path}: content changed")),
            Some(_) => {}
        }
    }
    for path in golden.keys() {
        if !current.contains_key(path) {
            changes.push(format!("{path}: removed"));
        }
    }
    changes
}

/// A rendered scene: the frame and what the GUI said about its renderer.
#[derive(Debug)]
pub struct SceneSnapshot {
    pub image: image::RgbaImage,
    /// The GUI's `Renderer initialized: ...` log line, if it was logged.
    pub renderer_info: Option<String>,
    pub elapsed: Duration,
}

/// Renders one scene through the real GUI. `work_dir` receives the config,
/// the scene bytes, the throwaway `HOME`, the GUI log and the snapshot.
pub fn render_scene_snapshot(
    gui_bin: &Path,
    repo_root: &Path,
    scene: &SceneSpec,
    work_dir: &Path,
    timeout: Duration,
) -> anyhow::Result<SceneSnapshot> {
    let started = Instant::now();
    std::fs::create_dir_all(work_dir)
        .with_context(|| format!("creating {}", work_dir.display()))?;
    let home = work_dir.join("home");
    let tmp = work_dir.join("tmp");
    for dir in [&home, &tmp] {
        std::fs::create_dir_all(dir)?;
    }
    let config_path = work_dir.join("frankenterm.toml");
    std::fs::write(
        &config_path,
        substitute_fixtures(
            &scene_config_toml(&repo_root.join(BUNDLED_FONTS_DIR), scene)?,
            &repo_root.join(FIXTURES_DIR),
        )?,
    )?;
    let scene_path = work_dir.join("scene.bin");
    std::fs::write(&scene_path, scene.text.as_bytes())?;
    let split_path = work_dir.join("split-scene.bin");
    if let Some(split) = &scene.snapshot.split {
        std::fs::write(&split_path, split.text.as_bytes())?;
    }
    let action_env = snapshot_action_env(&scene.snapshot, &split_path)?;
    let snapshot_path = work_dir.join("snapshot.png");
    let _ = std::fs::remove_file(&snapshot_path);
    let log_path = work_dir.join("gui.log");
    let log = std::fs::File::create(&log_path)?;

    // The scene, then the sentinel title (or, for a split scene, the split
    // title; the split pane sets the sentinel); then stay alive until killed.
    let first_title = if scene.snapshot.split.is_some() {
        SPLIT_TITLE
    } else {
        SNAPSHOT_TITLE
    };
    let script = format!("cat \"$1\"; printf '\\033]2;{first_title}\\007'; exec sleep 600");
    let mut command = Command::new(gui_bin);
    command
        .arg("--config-file")
        .arg(&config_path)
        .args([
            "start",
            "--always-new-process",
            "--",
            "/bin/sh",
            "-c",
            &script,
            "sh",
        ])
        .arg(&scene_path)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("XDG_RUNTIME_DIR", &tmp)
        .env("TMPDIR", &tmp)
        .env("LANG", "en_US.UTF-8")
        .env("TERM", "xterm-256color")
        .env("RUST_LOG", "frankenterm_gui=info")
        .env("FRANKENTERM_RENDER_SNAPSHOT", &snapshot_path)
        .env("FRANKENTERM_RENDER_SNAPSHOT_TITLE", SNAPSHOT_TITLE)
        // Accessory app, window ordered back, never activated: the run
        // cannot take keyboard focus from the operator, and every snapshot
        // sees the same (unfocused) window state.
        .env("FRANKENTERM_NATIVE_E2E_NONACTIVATING", "1")
        .envs(action_env)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    if let Ok(user) = std::env::var("USER") {
        command.env("USER", user);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("launching {}", gui_bin.display()))?;

    let outcome = loop {
        if snapshot_path.exists() {
            break Ok(());
        }
        if let Some(status) = child.try_wait()? {
            break Err(anyhow::anyhow!(
                "the GUI exited ({status}) before writing a snapshot; log tail:\n{}",
                log_tail(&log_path)
            ));
        }
        if started.elapsed() > timeout {
            break Err(anyhow::anyhow!(
                "no snapshot within {timeout:?}; log tail:\n{}",
                log_tail(&log_path)
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let _ = child.kill();
    let _ = child.wait();
    outcome?;

    let image = image::open(&snapshot_path)
        .with_context(|| format!("decoding {}", snapshot_path.display()))?
        .into_rgba8();
    let renderer_info = std::fs::read_to_string(&log_path).ok().and_then(|log| {
        log.lines().rev().find_map(|line| {
            line.split_once("Renderer initialized: ")
                .map(|(_, info)| info.trim().to_string())
        })
    });
    Ok(SceneSnapshot {
        image,
        renderer_info,
        elapsed: started.elapsed(),
    })
}

fn log_tail(path: &Path) -> String {
    let log = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = log.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// The repository root, from this crate's manifest directory.
pub fn repo_root_from_manifest(manifest_dir: &str) -> anyhow::Result<PathBuf> {
    Path::new(manifest_dir)
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("frankenterm-gui must live under crates/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        repo_root_from_manifest(env!("CARGO_MANIFEST_DIR")).unwrap()
    }

    /// Parses the generated file with FrankenTerm's own config loader.
    fn parse(toml: &str) -> config::Config {
        config::parse_toml_config_from_str(toml, &frankenterm_dynamic::Value::default())
            .unwrap_or_else(|err| panic!("generated config must load: {err:#}\n{toml}"))
    }

    #[test]
    fn config_pins_fonts_size_dpi_and_disables_blink_and_chrome() {
        let toml = scene_config_toml(Path::new("/repo/fonts"), &SceneSpec::default()).unwrap();
        for line in [
            "font_dirs = [\"/repo/fonts\"]",
            "font_size = 13.0",
            "dpi = 144.0",
            "cursor_blink_rate = 0",
            "text_blink_rate = 0",
            "enable_tab_bar = false",
            "front_end = \"WebGpu\"",
            "font_rasterizer = \"FreeType\"",
            "family = \"JetBrains Mono\"",
            "harfbuzz_features = [\"calt=0\", \"clig=0\", \"liga=0\"]",
            "family = \"Noto Color Emoji\"",
            "family = \"Hiragino Sans GB\"",
        ] {
            assert!(
                toml.lines().any(|l| l == line),
                "missing {line:?} in\n{toml}"
            );
        }
        let parsed = parse(&toml);
        assert_eq!(parsed.font_size, 13.0);
        assert_eq!(parsed.dpi, Some(144.0));
        assert!(!parsed.enable_tab_bar);
        assert_eq!(parsed.cursor_blink_rate, 0);
        assert_eq!(parsed.front_end, config::FrontEndSelection::WebGpu);
        assert!(matches!(
            parsed.font_rasterizer,
            config::FontRasterizerSelection::FreeType
        ));
        let families: Vec<&str> = parsed.font.font.iter().map(|f| f.family.as_str()).collect();
        assert_eq!(
            families,
            base_fonts()
                .iter()
                .map(|f| f.family.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn rasterizer_identity_follows_the_scene_rasterizer() {
        assert_eq!(
            rasterizer_identity(&SceneSpec::default()),
            RASTERIZER_IDENTITY
        );
        let coretext = SceneSpec {
            config: BTreeMap::from([(
                "font_rasterizer".to_string(),
                serde_json::json!("CoreText"),
            )]),
            ..SceneSpec::default()
        };
        assert_eq!(
            rasterizer_identity(&coretext),
            "frankenterm-gui WebGpu front end; CoreText rasterizer; HarfBuzz shaper"
        );
    }

    #[test]
    fn scene_overrides_replace_base_keys_and_font_stack() {
        let scene = SceneSpec {
            config: BTreeMap::from([
                ("font_size".to_string(), serde_json::json!(18.0)),
                ("enable_tab_bar".to_string(), serde_json::json!(true)),
            ]),
            fonts: Some(vec![SceneFont {
                family: "Fira Code".into(),
                harfbuzz_features: vec!["liga=1".into()],
            }]),
            extra_toml: Some("[window_background_gradient]\ncolors = [\"#000\", \"#228\"]".into()),
            ..SceneSpec::default()
        };
        let toml = scene_config_toml(Path::new("/f"), &scene).unwrap();
        let parsed = parse(&toml);
        assert_eq!(parsed.font_size, 18.0);
        assert!(parsed.enable_tab_bar);
        assert_eq!(parsed.font.font.len(), 1);
        assert_eq!(parsed.font.font[0].family, "Fira Code");
        assert_eq!(
            parsed.font.font[0].harfbuzz_features.as_deref(),
            Some(&["liga=1".to_string()][..])
        );
        assert!(parsed.window_background_gradient.is_some());
    }

    #[test]
    fn config_refuses_tables_and_non_bare_keys() {
        let mut scene = SceneSpec::default();
        scene.config.insert("bad key".into(), serde_json::json!(1));
        assert!(scene_config_toml(Path::new("/f"), &scene).is_err());
        let mut scene = SceneSpec::default();
        scene
            .config
            .insert("window_padding".into(), serde_json::json!({"left": 1}));
        assert!(scene_config_toml(Path::new("/f"), &scene).is_err());
    }

    #[test]
    fn strings_with_quotes_and_backslashes_stay_valid_toml() {
        let mut scene = SceneSpec::default();
        scene.config.insert(
            "default_cwd".into(),
            serde_json::json!("a \"quoted\" \\ path"),
        );
        let toml = scene_config_toml(Path::new("/f"), &scene).unwrap();
        let parsed = parse(&toml);
        assert_eq!(
            parsed.default_cwd.as_deref(),
            Some(Path::new("a \"quoted\" \\ path"))
        );
    }

    #[test]
    fn scene_actions_become_snapshot_hook_variables() {
        let split_scene = Path::new("/work/split-scene.bin");
        assert!(
            snapshot_action_env(&SceneActions::default(), split_scene)
                .unwrap()
                .is_empty()
        );
        let actions = SceneActions {
            focus: true,
            selection: Some([1, 2, 3, 4]),
            split: Some(SceneSplit {
                direction: "right".to_string(),
                text: "second".to_string(),
            }),
            modal: Some("char_select".to_string()),
        };
        let env: BTreeMap<String, String> = snapshot_action_env(&actions, split_scene)
            .unwrap()
            .into_iter()
            .collect();
        assert_eq!(env["FRANKENTERM_RENDER_SNAPSHOT_MODAL"], "char_select");
        assert_eq!(env["FRANKENTERM_RENDER_SNAPSHOT_FOCUS"], "1");
        assert_eq!(env["FRANKENTERM_RENDER_SNAPSHOT_SELECTION"], "1,2,3,4");
        assert_eq!(env["FRANKENTERM_RENDER_SNAPSHOT_SPLIT"], "right");
        assert_eq!(
            env["FRANKENTERM_RENDER_SNAPSHOT_SPLIT_COMMAND"],
            "cat '/work/split-scene.bin'; printf '\\033]2;ft-render-snapshot\\007'; exec sleep 600"
        );
    }

    #[test]
    fn malformed_scene_actions_are_refused() {
        let split_scene = Path::new("/work/split-scene.bin");
        let backwards = SceneActions {
            selection: Some([3, 0, 1, 0]),
            ..SceneActions::default()
        };
        assert!(snapshot_action_env(&backwards, split_scene).is_err());
        let sideways = SceneActions {
            split: Some(SceneSplit {
                direction: "left".to_string(),
                text: String::new(),
            }),
            ..SceneActions::default()
        };
        assert!(snapshot_action_env(&sideways, split_scene).is_err());
        let split = SceneActions {
            split: Some(SceneSplit {
                direction: "bottom".to_string(),
                text: String::new(),
            }),
            ..SceneActions::default()
        };
        assert!(snapshot_action_env(&split, Path::new("/it's/here")).is_err());
        let launcher = SceneActions {
            modal: Some("launcher".to_string()),
            ..SceneActions::default()
        };
        assert!(snapshot_action_env(&launcher, split_scene).is_err());
    }

    #[test]
    fn fixture_paths_are_substituted_into_valid_config() {
        let scene = SceneSpec {
            config: BTreeMap::from([(
                "window_background_image".to_string(),
                serde_json::json!("${FIXTURES}/background-checker.png"),
            )]),
            ..SceneSpec::default()
        };
        let toml = substitute_fixtures(
            &scene_config_toml(Path::new("/f"), &scene).unwrap(),
            Path::new("/repo/tests/golden/gpu/real/fixtures"),
        )
        .unwrap();
        assert!(!toml.contains(FIXTURES_PLACEHOLDER));
        let config = parse(&toml);
        assert_eq!(
            config.window_background_image,
            Some(PathBuf::from(
                "/repo/tests/golden/gpu/real/fixtures/background-checker.png"
            ))
        );
        assert!(substitute_fixtures("x", Path::new("/a\"b")).is_err());
    }

    #[test]
    fn scene_actions_round_trip_and_stay_out_of_plain_scenes() {
        let plain = serde_json::to_value(SceneSpec::default()).unwrap();
        assert!(plain.get("snapshot").is_none(), "{plain}");
        let scene: SceneSpec = serde_json::from_value(serde_json::json!({
            "text": "x",
            "snapshot": {"focus": true, "split": {"direction": "bottom", "text": "y"}}
        }))
        .unwrap();
        assert!(scene.snapshot.focus);
        assert_eq!(scene.snapshot.split.unwrap().text, "y");
    }

    #[test]
    fn font_identity_hashes_every_pinned_bundled_file() {
        let identity = font_identity(&repo_root()).unwrap();
        for file in PINNED_FONT_FILES {
            let entry = identity
                .iter()
                .find(|f| f.path.ends_with(file))
                .unwrap_or_else(|| panic!("{file} missing from {identity:?}"));
            assert_eq!(entry.sha256.len(), 64);
        }
        let again = font_identity(&repo_root()).unwrap();
        assert_eq!(font_set_sha(&identity), font_set_sha(&again));
        assert!(font_set_sha(&identity).starts_with("sha256:"));
    }

    #[test]
    fn font_identity_changes_explain_added_changed_and_removed_files() {
        let font = |path: &str, sha: &str| FontFileIdentity {
            path: path.into(),
            sha256: sha.into(),
        };
        let golden = vec![font("a.ttf", "1"), font("b.ttf", "2"), font("c.ttf", "3")];
        let current = vec![font("a.ttf", "1"), font("b.ttf", "9"), font("d.ttf", "4")];
        assert_eq!(
            font_identity_changes(&golden, &current),
            vec!["b.ttf: content changed", "d.ttf: added", "c.ttf: removed"]
        );
        assert!(font_identity_changes(&golden, &golden).is_empty());
        assert_ne!(font_set_sha(&golden), font_set_sha(&current));
    }
}
