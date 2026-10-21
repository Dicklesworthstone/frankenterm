//! The machine inventory, `machines.toml`: the user's machines, how to reach
//! them and how each one looks. It is the single source the desktop config
//! (`frankenterm.machines()`), `ft mobile export` and the phone all read, so a
//! machine has one name and one theme everywhere.
//!
//! The file is pure data. Loading it never evaluates the Lua config, runs a
//! command or reads a private key, and every colour resolves here, on the Mac,
//! to a complete palette: the phone needs no scheme catalog to match.
//!
//! Colour resolution has one fixed order: the terminal's default palette, then
//! the optional `scheme` (a built-in name or a `.toml` scheme file), then the
//! `colors` overrides, then derived defaults for whatever is still unset (the
//! accent follows the cursor, the cursor follows the accent, the cursor text
//! uses the background, the selection blends the accent into the background).
//! A machine without a `theme` table gets one of the distinct FrankenTerm hue
//! themes, never one already used by another machine.

use crate::color::{ColorSchemeFile, Palette};
use frankenterm_term::color::{ColorPalette, SrgbaTuple};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "machines.toml";
pub const FORMAT_VERSION: i64 = 1;
const MAX_MACHINES: usize = 128;
const MAX_ROUTES: usize = 16;
const MAX_GROUPS: usize = 64;

/// An sRGB colour with straight alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Rgba(pub [u8; 4]);

impl Rgba {
    /// Only `#RRGGBB` and `#RRGGBBAA`: the forms every consumer accepts.
    pub fn parse(text: &str) -> Option<Self> {
        let digits = text.strip_prefix('#')?;
        if !(digits.len() == 6 || digits.len() == 8)
            || !digits.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return None;
        }
        let byte = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();
        let alpha = if digits.len() == 8 { byte(6)? } else { 255 };
        Some(Self([byte(0)?, byte(2)?, byte(4)?, alpha]))
    }

    /// `#RRGGBB` when opaque, else `#RRGGBBAA`; upper case, as the phone stores it.
    pub fn hex(self) -> String {
        let [r, g, b, a] = self.0;
        if a == 255 {
            format!("#{:02X}{:02X}{:02X}", r, g, b)
        } else {
            format!("#{:02X}{:02X}{:02X}{:02X}", r, g, b, a)
        }
    }

    /// Rounds, unlike `SrgbaTuple::to_srgb_u8`, so `#0A0E14` survives a round trip.
    fn from_srgba(color: SrgbaTuple) -> Self {
        let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        Self([
            channel(color.0),
            channel(color.1),
            channel(color.2),
            channel(color.3),
        ])
    }

    /// This colour at `amount` opacity over an opaque `base`.
    fn over(self, base: Self, amount: f64) -> Self {
        let mix = |top: u8, bottom: u8| {
            (f64::from(top) * amount + f64::from(bottom) * (1.0 - amount)).round() as u8
        };
        Self([
            mix(self.0[0], base.0[0]),
            mix(self.0[1], base.0[1]),
            mix(self.0[2], base.0[2]),
            255,
        ])
    }

    fn hsl(hue: f64, saturation: f64, lightness: f64) -> Self {
        let chroma = (1.0 - (2.0 * lightness - 1.0).abs()) * saturation;
        let sector = (hue.rem_euclid(360.0)) / 60.0;
        let x = chroma * (1.0 - (sector % 2.0 - 1.0).abs());
        let (r, g, b) = match sector as u32 {
            0 => (chroma, x, 0.0),
            1 => (x, chroma, 0.0),
            2 => (0.0, chroma, x),
            3 => (0.0, x, chroma),
            4 => (x, 0.0, chroma),
            _ => (chroma, 0.0, x),
        };
        let m = lightness - chroma / 2.0;
        let channel = |value: f64| ((value + m).clamp(0.0, 1.0) * 255.0).round() as u8;
        Self([channel(r), channel(g), channel(b), 255])
    }

    /// Hue in degrees, or None for a nearly grey colour, which has no useful hue.
    fn hue(self) -> Option<f64> {
        let [r, g, b] = [self.0[0], self.0[1], self.0[2]].map(|c| f64::from(c) / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        if delta < 0.15 {
            return None;
        }
        let hue = if max == r {
            60.0 * ((g - b) / delta).rem_euclid(6.0)
        } else if max == g {
            60.0 * ((b - r) / delta + 2.0)
        } else {
            60.0 * ((r - g) / delta + 4.0)
        };
        Some(hue)
    }
}

impl fmt::Display for Rgba {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

/// A complete terminal palette in the phone's theme vocabulary.
#[derive(Clone, Debug, PartialEq)]
pub struct ThemePalette {
    pub foreground: Rgba,
    pub background: Rgba,
    pub cursor_fg: Rgba,
    pub cursor_bg: Rgba,
    pub cursor_border: Rgba,
    pub selection_fg: Rgba,
    pub selection_bg: Rgba,
    /// The 8 ANSI colours, then the 8 bright ones.
    pub ansi: [Rgba; 16],
    /// Explicit entries 16...255; the rest are the standard cube and grey ramp.
    pub indexed: BTreeMap<u8, Rgba>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ThemeFont {
    pub family: String,
    pub size: Option<f64>,
}

/// Desktop-only decoration. Never exported: the phone draws `background`.
#[derive(Clone, Debug, PartialEq)]
pub struct BackgroundGradient {
    pub colors: Vec<Rgba>,
    pub angle: f64,
    pub opacity: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MachineTheme {
    pub name: String,
    /// Chosen by the inventory because the machine declared no theme.
    pub automatic: bool,
    /// The machine's identifying colour: rail, tab and status highlights.
    pub accent: Rgba,
    pub palette: ThemePalette,
    pub font: Option<ThemeFont>,
    pub background_gradient: Option<BackgroundGradient>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RouteTarget {
    /// A `Host` alias from the user's SSH config, resolved only at export.
    Alias(String),
    Direct {
        host: String,
        user: String,
        port: Option<u16>,
        identity_files: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MachineRoute {
    Ssh {
        id: String,
        target: RouteTarget,
    },
    /// The machine's FrankenTerm mux, reached over its SSH routes in `via`
    /// order. `relay` replaces `nc -N -U <socket>` on hosts that need one.
    FrankenTermMux {
        id: String,
        via: Vec<String>,
        socket: Option<String>,
        relay: Option<String>,
    },
}

impl MachineRoute {
    pub fn id(&self) -> &str {
        match self {
            Self::Ssh { id, .. } | Self::FrankenTermMux { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub id: String,
    pub name: String,
    pub short_label: Option<String>,
    /// Desktop status-bar glyph.
    pub icon: Option<String>,
    /// Desktop working directory for new panes.
    pub cwd: Option<String>,
    pub tags: Vec<String>,
    pub theme: MachineTheme,
    pub routes: Vec<MachineRoute>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineGroup {
    pub id: String,
    pub name: String,
    pub machines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MachineInventory {
    pub workspace_id: String,
    pub workspace_name: String,
    pub default_group: String,
    /// Sorted by id.
    pub groups: Vec<MachineGroup>,
    /// In the order of first appearance in the default group, then the other
    /// groups, then any machine in no group, by id.
    pub machines: Vec<Machine>,
}

/// A refusal naming the exact key, e.g. `machines.trj.theme.colors.ansi`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InventoryError {
    pub path: String,
    pub message: String,
}

impl fmt::Display for InventoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: {}", FILE_NAME, self.message)
        } else {
            write!(f, "{}: {}: {}", FILE_NAME, self.path, self.message)
        }
    }
}

impl std::error::Error for InventoryError {}

fn fail<T>(path: &str, message: impl Into<String>) -> Result<T, InventoryError> {
    Err(InventoryError {
        path: path.to_string(),
        message: message.into(),
    })
}

/// The distinct hue themes, in assignment order: each next pick is far from
/// the ones before it, so a small fleet is already easy to tell apart.
const DISTINCT_HUES: [(&str, f64); 12] = [
    ("Azure", 210.0),
    ("Amber", 38.0),
    ("Emerald", 150.0),
    ("Magenta", 305.0),
    ("Violet", 265.0),
    ("Teal", 178.0),
    ("Crimson", 352.0),
    ("Lime", 95.0),
    ("Indigo", 235.0),
    ("Citrine", 55.0),
    ("Cyan", 192.0),
    ("Rose", 330.0),
];

/// The name of distinct theme `index`, e.g. "FrankenTerm Azure".
pub fn distinct_theme_name(index: usize) -> String {
    format!(
        "FrankenTerm {}",
        DISTINCT_HUES[index % DISTINCT_HUES.len()].0
    )
}

/// A dark theme tinted toward one hue, with a vivid accent of that hue and
/// readable ANSI colours: the same shape as hand-made per-host themes.
pub fn distinct_theme(index: usize) -> MachineTheme {
    let (_, hue) = DISTINCT_HUES[index % DISTINCT_HUES.len()];
    let background = Rgba::hsl(hue, 0.45, 0.06);
    let foreground = Rgba::hsl(hue, 0.40, 0.88);
    let accent = Rgba::hsl(hue, 0.85, 0.62);
    let fixed = |hex: &str| Rgba::parse(hex).expect("static colour");
    let ansi = [
        Rgba::hsl(hue, 0.30, 0.11),
        fixed("#FF5F6D"),
        fixed("#4ADE80"),
        fixed("#FACC15"),
        fixed("#60A5FA"),
        fixed("#E879F9"),
        fixed("#22D3EE"),
        Rgba::hsl(hue, 0.15, 0.80),
        Rgba::hsl(hue, 0.20, 0.34),
        fixed("#FF8A94"),
        fixed("#86EFAC"),
        fixed("#FDE047"),
        fixed("#93C5FD"),
        fixed("#F0ABFC"),
        fixed("#67E8F9"),
        Rgba::hsl(hue, 0.30, 0.96),
    ];
    MachineTheme {
        name: distinct_theme_name(index),
        automatic: false,
        accent,
        palette: ThemePalette {
            foreground,
            background,
            cursor_fg: background,
            cursor_bg: accent,
            cursor_border: accent,
            selection_fg: foreground,
            selection_bg: accent.over(background, 0.35),
            ansi,
            indexed: BTreeMap::new(),
        },
        font: None,
        background_gradient: Some(BackgroundGradient {
            colors: vec![
                background,
                Rgba::hsl(hue, 0.45, 0.09),
                Rgba::hsl(hue, 0.45, 0.12),
            ],
            angle: -45.0,
            opacity: 0.92,
        }),
    }
}

pub fn distinct_theme_count() -> usize {
    DISTINCT_HUES.len()
}

/// Every theme a machine can name or the phone can offer: the distinct hue
/// themes first, then the built-in schemes by name. Aliases are skipped.
pub fn theme_catalog() -> Vec<MachineTheme> {
    let mut themes: Vec<MachineTheme> = (0..DISTINCT_HUES.len()).map(distinct_theme).collect();
    for (name, palette) in crate::builtin_scheme_palettes() {
        let overrides = Overrides::from_palette(&palette);
        let (accent, resolved) = resolve(&overrides, None);
        themes.push(MachineTheme {
            name,
            automatic: false,
            accent,
            palette: resolved,
            font: None,
            background_gradient: None,
        });
    }
    themes
}

/// Colours set by a scheme or by the user; `None` is unset.
#[derive(Clone, Debug, Default, PartialEq)]
struct Overrides {
    foreground: Option<Rgba>,
    background: Option<Rgba>,
    cursor_fg: Option<Rgba>,
    cursor_bg: Option<Rgba>,
    cursor_border: Option<Rgba>,
    selection_fg: Option<Rgba>,
    selection_bg: Option<Rgba>,
    ansi: Option<[Rgba; 8]>,
    brights: Option<[Rgba; 8]>,
    indexed: BTreeMap<u8, Rgba>,
}

impl Overrides {
    fn from_palette(palette: &Palette) -> Self {
        let color = |value: &Option<crate::color::RgbaColor>| value.map(|c| Rgba::from_srgba(*c));
        let eight = |value: &Option<[crate::color::RgbaColor; 8]>| {
            value.map(|colors| colors.map(|c| Rgba::from_srgba(*c)))
        };
        Self {
            foreground: color(&palette.foreground),
            background: color(&palette.background),
            cursor_fg: color(&palette.cursor_fg),
            cursor_bg: color(&palette.cursor_bg),
            cursor_border: color(&palette.cursor_border),
            selection_fg: color(&palette.selection_fg),
            selection_bg: color(&palette.selection_bg),
            ansi: eight(&palette.ansi),
            brights: eight(&palette.brights),
            indexed: palette
                .indexed
                .iter()
                .filter(|(index, _)| **index >= 16)
                .map(|(index, c)| (*index, Rgba::from_srgba(**c)))
                .collect(),
        }
    }

    fn overlay(&mut self, top: &Self) {
        macro_rules! take {
            ($($field:ident),*) => {
                $(if top.$field.is_some() { self.$field = top.$field; })*
            };
        }
        take!(
            foreground,
            background,
            cursor_fg,
            cursor_bg,
            cursor_border,
            selection_fg,
            selection_bg,
            ansi,
            brights
        );
        for (index, color) in &top.indexed {
            self.indexed.insert(*index, *color);
        }
    }
}

/// Defaults, then the derived colours. Returns the accent and the palette.
fn resolve(set: &Overrides, accent: Option<Rgba>) -> (Rgba, ThemePalette) {
    let base = ColorPalette::default();
    let base_color = |c: SrgbaTuple| Rgba::from_srgba(c);
    let background = set
        .background
        .unwrap_or_else(|| base_color(base.background));
    let foreground = set
        .foreground
        .unwrap_or_else(|| base_color(base.foreground));
    let accent = accent
        .or(set.cursor_bg)
        .unwrap_or_else(|| base_color(base.cursor_bg));
    let cursor_bg = set.cursor_bg.unwrap_or(accent);
    let mut ansi = [Rgba([0, 0, 0, 255]); 16];
    for (index, slot) in ansi.iter_mut().enumerate() {
        *slot = base_color(base.colors.0[index]);
    }
    if let Some(colors) = set.ansi {
        ansi[..8].copy_from_slice(&colors);
    }
    if let Some(colors) = set.brights {
        ansi[8..].copy_from_slice(&colors);
    }
    let palette = ThemePalette {
        foreground,
        background,
        cursor_fg: set.cursor_fg.unwrap_or(background),
        cursor_bg,
        cursor_border: set.cursor_border.unwrap_or(cursor_bg),
        selection_fg: set.selection_fg.unwrap_or(foreground),
        selection_bg: set
            .selection_bg
            .unwrap_or_else(|| accent.over(background, 0.35)),
        ansi,
        indexed: set.indexed.clone(),
    };
    (accent, palette)
}

/// Strict reading of one TOML table: every key must be known or `x-*`.
struct Fields<'a> {
    path: String,
    table: &'a toml::Table,
}

impl<'a> Fields<'a> {
    fn new(path: &str, value: &'a toml::Value) -> Result<Self, InventoryError> {
        match value.as_table() {
            Some(table) => Ok(Self {
                path: path.to_string(),
                table,
            }),
            None => fail(path, "expected a table"),
        }
    }

    fn at(&self, key: &str) -> String {
        if self.path.is_empty() {
            key.to_string()
        } else {
            format!("{}.{}", self.path, key)
        }
    }

    fn allow(&self, keys: &[&str]) -> Result<(), InventoryError> {
        for key in self.table.keys() {
            if !keys.contains(&key.as_str()) && !key.starts_with("x-") {
                return fail(&self.at(key), "unknown key (extensions must start with x-)");
            }
        }
        Ok(())
    }

    fn get(&self, key: &str) -> Option<&'a toml::Value> {
        self.table.get(key)
    }

    fn string(&self, key: &str) -> Result<Option<String>, InventoryError> {
        match self.get(key) {
            None => Ok(None),
            Some(toml::Value::String(text)) => {
                if text.is_empty() || text.chars().any(char::is_control) || text.len() > 256 {
                    return fail(
                        &self.at(key),
                        "expected non-empty text without control characters, at most 256 bytes",
                    );
                }
                Ok(Some(text.clone()))
            }
            Some(_) => fail(&self.at(key), "expected a string"),
        }
    }

    fn required_string(&self, key: &str) -> Result<String, InventoryError> {
        match self.string(key)? {
            Some(text) => Ok(text),
            None => fail(&self.at(key), "is required"),
        }
    }

    fn strings(&self, key: &str) -> Result<Option<Vec<String>>, InventoryError> {
        match self.get(key) {
            None => Ok(None),
            Some(toml::Value::Array(values)) => {
                let mut result = Vec::with_capacity(values.len());
                for (index, value) in values.iter().enumerate() {
                    match value.as_str() {
                        Some(text) if !text.is_empty() && !text.chars().any(char::is_control) => {
                            result.push(text.to_string())
                        }
                        _ => {
                            return fail(
                                &format!("{}[{}]", self.at(key), index),
                                "expected non-empty text",
                            )
                        }
                    }
                }
                Ok(Some(result))
            }
            Some(_) => fail(&self.at(key), "expected an array of strings"),
        }
    }

    fn color(&self, key: &str) -> Result<Option<Rgba>, InventoryError> {
        match self.get(key) {
            None => Ok(None),
            Some(value) => parse_color(&self.at(key), value).map(Some),
        }
    }

    fn colors8(&self, key: &str) -> Result<Option<[Rgba; 8]>, InventoryError> {
        match self.get(key) {
            None => Ok(None),
            Some(toml::Value::Array(values)) if values.len() == 8 => {
                let mut result = [Rgba([0, 0, 0, 255]); 8];
                for (index, value) in values.iter().enumerate() {
                    result[index] = parse_color(&format!("{}[{}]", self.at(key), index), value)?;
                }
                Ok(Some(result))
            }
            Some(_) => fail(&self.at(key), "expected exactly 8 colours"),
        }
    }

    fn number(&self, key: &str) -> Result<Option<f64>, InventoryError> {
        match self.get(key) {
            None => Ok(None),
            Some(toml::Value::Float(value)) if value.is_finite() => Ok(Some(*value)),
            Some(toml::Value::Integer(value)) => Ok(Some(*value as f64)),
            Some(_) => fail(&self.at(key), "expected a number"),
        }
    }
}

fn parse_color(path: &str, value: &toml::Value) -> Result<Rgba, InventoryError> {
    match value.as_str().and_then(Rgba::parse) {
        Some(color) => Ok(color),
        None => fail(path, "expected a colour written #RRGGBB or #RRGGBBAA"),
    }
}

/// Lower-case ASCII letters, digits, `.`, `_` and `-`, starting with a letter
/// or digit: stable keys that every consumer can use as an identifier. At most
/// 48 bytes, so composed export ids stay within the phone's 128.
fn identifier(path: &str, text: &str) -> Result<String, InventoryError> {
    let valid = !text.is_empty()
        && text.len() <= 48
        && text
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && text.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'_' || b == b'-'
        });
    if valid {
        Ok(text.to_string())
    } else {
        fail(
            path,
            "expected an id of lower-case letters, digits, '.', '_' or '-' (at most 48)",
        )
    }
}

/// Where scheme files and `~` resolve: the inventory's own directory and HOME.
pub struct InventoryContext {
    pub directory: PathBuf,
    pub home: PathBuf,
}

impl InventoryContext {
    pub fn expand(&self, path: &str) -> PathBuf {
        if path == "~" {
            self.home.clone()
        } else if let Some(rest) = path.strip_prefix("~/") {
            self.home.join(rest)
        } else if Path::new(path).is_absolute() {
            PathBuf::from(path)
        } else {
            self.directory.join(path)
        }
    }
}

/// The inventory file this configuration reads: the first `machines.toml`
/// in the FrankenTerm config directories, or where a new one belongs.
pub fn default_path() -> PathBuf {
    for dir in crate::CONFIG_DIRS.iter() {
        let candidate = dir.join(FILE_NAME);
        if candidate.exists() {
            return candidate;
        }
    }
    crate::CONFIG_DIRS
        .first()
        .cloned()
        .unwrap_or_else(|| crate::HOME_DIR.join(".config").join("frankenterm"))
        .join(FILE_NAME)
}

impl MachineInventory {
    /// Reads and resolves an inventory file. Scheme files resolve beside it.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|err| anyhow::anyhow!("reading {}: {}", path.display(), err))?;
        let context = InventoryContext {
            directory: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            home: crate::HOME_DIR.clone(),
        };
        Ok(Self::parse(&text, &context)?)
    }

    pub fn parse(text: &str, context: &InventoryContext) -> Result<Self, InventoryError> {
        let root: toml::Value = match toml::from_str::<toml::Table>(text) {
            Ok(table) => toml::Value::Table(table),
            Err(err) => return fail("", format!("not valid TOML: {}", err.message())),
        };
        let fields = Fields::new("", &root)?;
        fields.allow(&[
            "version",
            "workspace",
            "default_group",
            "groups",
            "machines",
        ])?;
        match fields.get("version") {
            Some(toml::Value::Integer(FORMAT_VERSION)) => {}
            Some(_) => return fail("version", "only version 1 is understood"),
            None => return fail("version", "is required (version = 1)"),
        }

        let (workspace_id, workspace_name) = match fields.get("workspace") {
            Some(value) => {
                let workspace = Fields::new("workspace", value)?;
                workspace.allow(&["id", "name"])?;
                let id = match workspace.string("id")? {
                    Some(id) => identifier("workspace.id", &id)?,
                    None => "machines".to_string(),
                };
                let name = workspace
                    .string("name")?
                    .unwrap_or_else(|| "Machines".to_string());
                (id, name)
            }
            None => ("machines".to_string(), "Machines".to_string()),
        };

        let machine_values = match fields.get("machines") {
            Some(value) => Fields::new("machines", value)?,
            None => return fail("machines", "is required: at least one machine"),
        };
        if machine_values.table.is_empty() {
            return fail("machines", "is required: at least one machine");
        }
        if machine_values.table.len() > MAX_MACHINES {
            return fail("machines", format!("at most {} machines", MAX_MACHINES));
        }
        let mut parsed: BTreeMap<String, (ParsedMachine, Option<ParsedTheme>)> = BTreeMap::new();
        for (key, value) in machine_values.table {
            let path = format!("machines.{}", key);
            let id = identifier(&path, key)?;
            parsed.insert(id.clone(), parse_machine(&id, &path, value, context)?);
        }

        let mut groups = Vec::new();
        if let Some(value) = fields.get("groups") {
            let group_values = Fields::new("groups", value)?;
            if group_values.table.len() > MAX_GROUPS {
                return fail("groups", format!("at most {} groups", MAX_GROUPS));
            }
            for (key, value) in group_values.table {
                let path = format!("groups.{}", key);
                let id = identifier(&path, key)?;
                let group = Fields::new(&path, value)?;
                group.allow(&["name", "machines"])?;
                let members = match group.strings("machines")? {
                    Some(members) if !members.is_empty() => members,
                    _ => {
                        return fail(
                            &group.at("machines"),
                            "is required: at least one machine id",
                        )
                    }
                };
                let mut seen = BTreeSet::new();
                for (index, member) in members.iter().enumerate() {
                    let at = format!("{}.machines[{}]", path, index);
                    if !parsed.contains_key(member.as_str()) {
                        return fail(&at, format!("no machine named {:?}", member));
                    }
                    if !seen.insert(member.clone()) {
                        return fail(&at, format!("{:?} appears twice", member));
                    }
                }
                groups.push(MachineGroup {
                    name: group.string("name")?.unwrap_or_else(|| id.clone()),
                    id,
                    machines: members,
                });
            }
        }
        if groups.is_empty() {
            // Every machine is still reachable as one implicit group.
            groups.push(MachineGroup {
                id: "all".to_string(),
                name: "All machines".to_string(),
                machines: parsed.keys().cloned().collect(),
            });
        }
        let default_group = match fields.string("default_group")? {
            Some(id) => {
                if !groups.iter().any(|g| g.id == id) {
                    return fail("default_group", format!("no group named {:?}", id));
                }
                id
            }
            None => groups[0].id.clone(),
        };

        // The order a person reads: the default group, then the others, then
        // machines in no group.
        let mut order: Vec<String> = Vec::new();
        let mut placed = BTreeSet::new();
        let default_first = groups
            .iter()
            .filter(|g| g.id == default_group)
            .chain(groups.iter().filter(|g| g.id != default_group));
        for group in default_first {
            for member in &group.machines {
                if placed.insert(member.clone()) {
                    order.push(member.clone());
                }
            }
        }
        for id in parsed.keys() {
            if placed.insert(id.clone()) {
                order.push(id.clone());
            }
        }

        // Explicit themes first: automatic ones must avoid their hues.
        let mut used_names = BTreeSet::new();
        let mut used_hues: Vec<f64> = Vec::new();
        let mut themes: BTreeMap<String, MachineTheme> = BTreeMap::new();
        for id in &order {
            if let (_, Some(theme)) = &parsed[id] {
                used_names.insert(theme.theme.name.clone());
                if let Some(hue) = theme.theme.accent.hue() {
                    used_hues.push(hue);
                }
                themes.insert(id.clone(), theme.theme.clone());
            }
        }
        let mut next = 0usize;
        for id in &order {
            if themes.contains_key(id) {
                continue;
            }
            let theme = next_distinct(&mut next, &used_names, &used_hues);
            used_names.insert(theme.name.clone());
            if let Some(hue) = theme.accent.hue() {
                used_hues.push(hue);
            }
            themes.insert(id.clone(), theme);
        }

        let machines = order
            .iter()
            .map(|id| {
                let (machine, _) = parsed.remove(id).expect("ordered machine exists");
                Machine {
                    id: id.clone(),
                    name: machine.name,
                    short_label: machine.short_label,
                    icon: machine.icon,
                    cwd: machine.cwd,
                    tags: machine.tags,
                    theme: themes.remove(id).expect("every machine has a theme"),
                    routes: machine.routes,
                }
            })
            .collect();
        Ok(Self {
            workspace_id,
            workspace_name,
            default_group,
            groups,
            machines,
        })
    }

    pub fn machine(&self, id: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.id == id)
    }
}

fn hue_distance(a: f64, b: f64) -> f64 {
    let delta = (a - b).abs() % 360.0;
    delta.min(360.0 - delta)
}

/// The next distinct hue theme whose name is unused and whose hue is at least
/// 22 degrees from every hue in use, in assignment order. Once none is left
/// (more machines than hues), the hue farthest from those in use, with a
/// numeric suffix when its name is already taken.
fn next_distinct(
    next: &mut usize,
    used_names: &BTreeSet<String>,
    used_hues: &[f64],
) -> MachineTheme {
    let nearest = |hue: f64| {
        used_hues
            .iter()
            .map(|used| hue_distance(*used, hue))
            .fold(f64::INFINITY, f64::min)
    };
    for _ in 0..DISTINCT_HUES.len() {
        let index = *next % DISTINCT_HUES.len();
        *next += 1;
        let candidate = distinct_theme(index);
        if !used_names.contains(&candidate.name) && nearest(DISTINCT_HUES[index].1) >= 22.0 {
            return MachineTheme {
                automatic: true,
                ..candidate
            };
        }
    }
    let index = (0..DISTINCT_HUES.len())
        .max_by(|a, b| {
            nearest(DISTINCT_HUES[*a].1)
                .partial_cmp(&nearest(DISTINCT_HUES[*b].1))
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.cmp(a))
        })
        .unwrap_or(0);
    let mut candidate = distinct_theme(index);
    let base = candidate.name.clone();
    let mut round = 2;
    while used_names.contains(&candidate.name) {
        candidate.name = format!("{} {}", base, round);
        round += 1;
    }
    MachineTheme {
        automatic: true,
        ..candidate
    }
}

struct ParsedMachine {
    name: String,
    short_label: Option<String>,
    icon: Option<String>,
    cwd: Option<String>,
    tags: Vec<String>,
    routes: Vec<MachineRoute>,
}

struct ParsedTheme {
    theme: MachineTheme,
}

fn parse_machine(
    id: &str,
    path: &str,
    value: &toml::Value,
    context: &InventoryContext,
) -> Result<(ParsedMachine, Option<ParsedTheme>), InventoryError> {
    let machine = Fields::new(path, value)?;
    machine.allow(&[
        "name",
        "short_label",
        "icon",
        "cwd",
        "tags",
        "theme",
        "routes",
    ])?;
    let short_label = machine.string("short_label")?;
    if let Some(label) = &short_label {
        if label.len() > 32 {
            return fail(&machine.at("short_label"), "at most 32 bytes");
        }
    }
    let tags = machine.strings("tags")?.unwrap_or_default();
    if tags.len() > 64 {
        return fail(&machine.at("tags"), "at most 64 tags");
    }
    if let Some(index) = tags.iter().position(|tag| tag.len() > 128) {
        return fail(
            &format!("{}[{}]", machine.at("tags"), index),
            "at most 128 bytes",
        );
    }
    let theme = match machine.get("theme") {
        Some(value) => Some(ParsedTheme {
            theme: parse_theme(id, &machine.at("theme"), value, context)?,
        }),
        None => None,
    };
    let routes = parse_routes(&machine.at("routes"), machine.get("routes"))?;
    Ok((
        ParsedMachine {
            name: machine.string("name")?.unwrap_or_else(|| id.to_string()),
            short_label,
            icon: machine.string("icon")?,
            cwd: machine.string("cwd")?,
            tags,
            routes,
        },
        theme,
    ))
}

fn parse_theme(
    machine: &str,
    path: &str,
    value: &toml::Value,
    context: &InventoryContext,
) -> Result<MachineTheme, InventoryError> {
    let theme = Fields::new(path, value)?;
    theme.allow(&["name", "scheme", "accent", "colors", "font", "desktop"])?;
    let mut set = Overrides::default();
    let mut name = None;
    let mut distinct_gradient = None;
    if let Some(scheme) = theme.string("scheme")? {
        let at = theme.at("scheme");
        if let Some(index) =
            (0..DISTINCT_HUES.len()).find(|index| distinct_theme_name(*index) == scheme)
        {
            let distinct = distinct_theme(index);
            set.overlay(&overrides_from_theme(&distinct));
            distinct_gradient = distinct.background_gradient;
        } else if scheme.ends_with(".toml") || scheme.contains('/') {
            let file = context.expand(&scheme);
            let data = match std::fs::read_to_string(&file) {
                Ok(data) => data,
                Err(err) => return fail(&at, format!("reading {}: {}", file.display(), err)),
            };
            match ColorSchemeFile::from_toml_str(&data) {
                Ok(parsed) => set.overlay(&Overrides::from_palette(&parsed.colors)),
                Err(err) => {
                    return fail(
                        &at,
                        format!("{} is not a colour scheme: {:#}", file.display(), err),
                    )
                }
            }
        } else if let Some(palette) = crate::COLOR_SCHEMES.get(&scheme) {
            set.overlay(&Overrides::from_palette(palette));
        } else {
            return fail(
                &at,
                format!("no built-in scheme or FrankenTerm theme named {:?}", scheme),
            );
        }
        name = Some(scheme);
    }
    if let Some(value) = theme.get("colors") {
        let colors = Fields::new(&theme.at("colors"), value)?;
        colors.allow(&[
            "foreground",
            "background",
            "cursor_fg",
            "cursor_bg",
            "cursor_border",
            "selection_fg",
            "selection_bg",
            "ansi",
            "brights",
            "indexed",
        ])?;
        let mut top = Overrides {
            foreground: colors.color("foreground")?,
            background: colors.color("background")?,
            cursor_fg: colors.color("cursor_fg")?,
            cursor_bg: colors.color("cursor_bg")?,
            cursor_border: colors.color("cursor_border")?,
            selection_fg: colors.color("selection_fg")?,
            selection_bg: colors.color("selection_bg")?,
            ansi: colors.colors8("ansi")?,
            brights: colors.colors8("brights")?,
            indexed: BTreeMap::new(),
        };
        if let Some(value) = colors.get("indexed") {
            let indexed = Fields::new(&colors.at("indexed"), value)?;
            for (key, value) in indexed.table {
                let at = indexed.at(key);
                let index = match key.parse::<u8>() {
                    Ok(index) if index >= 16 && index.to_string() == *key => index,
                    _ => {
                        return fail(
                            &at,
                            "indexed keys are 16 through 255 (use ansi or brights below 16)",
                        )
                    }
                };
                top.indexed.insert(index, parse_color(&at, value)?);
            }
        }
        set.overlay(&top);
    }
    let accent = theme.color("accent")?;
    let (accent, palette) = resolve(&set, accent);
    let font = match theme.get("font") {
        Some(value) => {
            let font = Fields::new(&theme.at("font"), value)?;
            font.allow(&["family", "size"])?;
            let size = font.number("size")?;
            if let Some(size) = size {
                if !(6.0..=96.0).contains(&size) {
                    return fail(&font.at("size"), "expected 6 through 96 points");
                }
            }
            Some(ThemeFont {
                family: font.required_string("family")?,
                size,
            })
        }
        None => None,
    };
    let background_gradient = match theme.get("desktop") {
        Some(value) => {
            let desktop = Fields::new(&theme.at("desktop"), value)?;
            desktop.allow(&["background_gradient"])?;
            match desktop.get("background_gradient") {
                Some(value) => Some(parse_gradient(&desktop.at("background_gradient"), value)?),
                None => None,
            }
        }
        None => distinct_gradient,
    };
    Ok(MachineTheme {
        name: theme
            .string("name")?
            .or(name)
            .unwrap_or_else(|| machine.to_string()),
        automatic: false,
        accent,
        palette,
        font,
        background_gradient,
    })
}

fn overrides_from_theme(theme: &MachineTheme) -> Overrides {
    let p = &theme.palette;
    let mut ansi = [Rgba([0, 0, 0, 255]); 8];
    let mut brights = ansi;
    ansi.copy_from_slice(&p.ansi[..8]);
    brights.copy_from_slice(&p.ansi[8..]);
    Overrides {
        foreground: Some(p.foreground),
        background: Some(p.background),
        cursor_fg: Some(p.cursor_fg),
        cursor_bg: Some(p.cursor_bg),
        cursor_border: Some(p.cursor_border),
        selection_fg: Some(p.selection_fg),
        selection_bg: Some(p.selection_bg),
        ansi: Some(ansi),
        brights: Some(brights),
        indexed: p.indexed.clone(),
    }
}

fn parse_gradient(path: &str, value: &toml::Value) -> Result<BackgroundGradient, InventoryError> {
    let gradient = Fields::new(path, value)?;
    gradient.allow(&["colors", "angle", "opacity"])?;
    let colors = match gradient.get("colors") {
        Some(toml::Value::Array(values)) if (1..=8).contains(&values.len()) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                parse_color(&format!("{}[{}]", gradient.at("colors"), index), value)
            })
            .collect::<Result<Vec<_>, _>>()?,
        _ => return fail(&gradient.at("colors"), "expected 1 through 8 colours"),
    };
    let opacity = gradient.number("opacity")?.unwrap_or(1.0);
    if !(0.0..=1.0).contains(&opacity) {
        return fail(&gradient.at("opacity"), "expected 0 through 1");
    }
    Ok(BackgroundGradient {
        colors,
        angle: gradient.number("angle")?.unwrap_or(0.0),
        opacity,
    })
}

fn parse_routes(
    path: &str,
    value: Option<&toml::Value>,
) -> Result<Vec<MachineRoute>, InventoryError> {
    let values = match value {
        Some(toml::Value::Array(values)) if !values.is_empty() => values,
        Some(toml::Value::Array(_)) | None => return fail(path, "is required: at least one route"),
        Some(_) => return fail(path, "expected [[routes]] tables"),
    };
    if values.len() > MAX_ROUTES {
        return fail(path, format!("at most {} routes", MAX_ROUTES));
    }
    let mut routes = Vec::with_capacity(values.len());
    let mut ids = BTreeSet::new();
    for (index, value) in values.iter().enumerate() {
        let at = format!("{}[{}]", path, index);
        let route = Fields::new(&at, value)?;
        let id = identifier(&route.at("id"), &route.required_string("id")?)?;
        if !ids.insert(id.clone()) {
            return fail(&route.at("id"), format!("route {:?} appears twice", id));
        }
        let kind = route.string("kind")?;
        let parsed = match kind.as_deref() {
            Some("frankenterm_mux") => {
                route.allow(&["id", "kind", "via", "socket", "relay"])?;
                let via = match route.get("via") {
                    Some(toml::Value::String(one)) => vec![one.clone()],
                    Some(_) => route.strings("via")?.unwrap_or_default(),
                    None => Vec::new(),
                };
                if via.is_empty() {
                    return fail(
                        &route.at("via"),
                        "is required: the SSH route ids to reach the mux, in order",
                    );
                }
                let socket = route.string("socket")?;
                let relay = route.string("relay")?;
                for (key, value) in [("socket", &socket), ("relay", &relay)] {
                    if let Some(text) = value {
                        if !text.starts_with('/') || text.contains('\'') {
                            return fail(
                                &route.at(key),
                                "expected an absolute remote path without quotes",
                            );
                        }
                    }
                }
                if socket.is_none() && relay.is_none() {
                    return fail(&at, "a mux route needs socket (or relay)");
                }
                MachineRoute::FrankenTermMux {
                    id,
                    via,
                    socket,
                    relay,
                }
            }
            None | Some("ssh") => {
                route.allow(&[
                    "id",
                    "kind",
                    "ssh",
                    "host",
                    "user",
                    "port",
                    "identity_file",
                    "identity_files",
                ])?;
                let target = match (route.string("ssh")?, route.string("host")?) {
                    (Some(alias), None) => {
                        for key in ["user", "port", "identity_file", "identity_files"] {
                            if route.get(key).is_some() {
                                return fail(
                                    &route.at(key),
                                    "an ssh alias route takes these from ~/.ssh/config",
                                );
                            }
                        }
                        RouteTarget::Alias(alias)
                    }
                    (None, Some(host)) => {
                        let port = match route.get("port") {
                            None => None,
                            Some(toml::Value::Integer(port)) if (1..=65535).contains(port) => {
                                Some(*port as u16)
                            }
                            Some(_) => return fail(&route.at("port"), "expected 1 through 65535"),
                        };
                        let mut identity_files = Vec::new();
                        if let Some(one) = route.string("identity_file")? {
                            identity_files.push(one);
                        }
                        identity_files.extend(route.strings("identity_files")?.unwrap_or_default());
                        let user = route.required_string("user")?;
                        if user.chars().any(char::is_whitespace) {
                            return fail(&route.at("user"), "cannot contain spaces");
                        }
                        if !valid_host(&host) {
                            return fail(&route.at("host"), INVALID_HOST);
                        }
                        RouteTarget::Direct {
                            host,
                            user,
                            port,
                            identity_files,
                        }
                    }
                    (Some(_), Some(_)) => {
                        return fail(&at, "use either ssh (an alias) or host, not both")
                    }
                    (None, None) => {
                        return fail(
                            &at,
                            "needs ssh (an alias from ~/.ssh/config) or host and user",
                        )
                    }
                };
                MachineRoute::Ssh { id, target }
            }
            Some(other) => {
                return fail(
                    &route.at("kind"),
                    format!("unknown route kind {:?} (ssh or frankenterm_mux)", other),
                )
            }
        };
        routes.push(parsed);
    }
    // Mux routes reach the mux over this machine's own SSH routes.
    let ssh_ids: BTreeSet<&str> = routes
        .iter()
        .filter_map(|route| match route {
            MachineRoute::Ssh { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    for (index, route) in routes.iter().enumerate() {
        if let MachineRoute::FrankenTermMux { via, .. } = route {
            for name in via {
                if !ssh_ids.contains(name.as_str()) {
                    return fail(
                        &format!("{}[{}].via", path, index),
                        format!("no ssh route named {:?} on this machine", name),
                    );
                }
            }
        }
    }
    Ok(routes)
}

pub(crate) const INVALID_HOST: &str =
    "expected a host name or address (an IPv6 address in [brackets]), without spaces or / \\ @ ? # %";

/// A host the phone accepts as a network target: no spaces, controls or URL
/// punctuation, and anything with a colon an IPv6 address (brackets optional).
pub(crate) fn valid_host(host: &str) -> bool {
    if host.is_empty()
        || host.len() > 1024
        || host
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "/\\@?#%".contains(c))
    {
        return false;
    }
    let inner = match host.strip_prefix('[') {
        Some(rest) => match rest.strip_suffix(']') {
            Some(inner) if inner.contains(':') => inner,
            _ => return false,
        },
        None => host,
    };
    if inner.contains(['[', ']']) {
        return false;
    }
    !inner.contains(':') || inner.parse::<std::net::Ipv6Addr>().is_ok()
}

/// A first inventory for SSH config host aliases: one machine per alias, in
/// one group, each pinned to its own distinct theme. Pinned explicitly rather
/// than left automatic, so adding a machine later never recolours another.
pub fn inventory_template(aliases: &[String]) -> String {
    let mut ids: Vec<String> = Vec::new();
    for alias in aliases {
        let mut id: String = alias
            .to_ascii_lowercase()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        id = id
            .trim_start_matches(|c: char| !c.is_ascii_alphanumeric())
            .to_string();
        id.truncate(40);
        if id.is_empty() {
            id = "machine".to_string();
        }
        let base = id.clone();
        let mut n = 2;
        while ids.contains(&id) {
            id = format!("{}-{}", base, n);
            n += 1;
        }
        ids.push(id);
    }
    let quoted = |text: &str| format!("{:?}", text);
    let mut text = String::from(
        "# FrankenTerm machine inventory: read by the desktop config\n\
         # (frankenterm.machines()), `ft mobile export` and the phone.\n\
         # Each machine names a theme: a FrankenTerm hue theme, any built-in\n\
         # scheme, or a .toml scheme file; [machines.<id>.theme.colors] overrides it.\n\
         # A machine with no theme table gets an unused hue theme automatically.\n\n\
         version = 1\ndefault_group = \"machines\"\n\n[groups.machines]\nname = \"Machines\"\nmachines = [",
    );
    text.push_str(
        &ids.iter()
            .map(|id| quoted(id))
            .collect::<Vec<_>>()
            .join(", "),
    );
    text.push_str("]\n");
    for (index, (id, alias)) in ids.iter().zip(aliases).enumerate() {
        text.push_str(&format!(
            "\n[machines.{}]\nname = {}\n\n[machines.{}.theme]\nscheme = {}\n\n[[machines.{}.routes]]\nid = \"ssh\"\nssh = {}\n",
            id,
            quoted(alias),
            id,
            quoted(&distinct_theme_name(index)),
            id,
            quoted(alias)
        ));
    }
    text
}

/// The SSH options the desktop has always used for its mux proxies.
const MUX_SSH_OPTIONS: [&str; 5] = [
    "ConnectTimeout=10",
    "BatchMode=yes",
    "ServerAliveInterval=30",
    "ServerAliveCountMax=4",
    "StrictHostKeyChecking=accept-new",
];

fn shell_word(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-@:=+,%".contains(&b));
    if safe {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

impl Machine {
    pub fn route(&self, id: &str) -> Option<&MachineRoute> {
        self.routes.iter().find(|route| route.id() == id)
    }

    /// The local command that splices stdio to this machine's mux: one `ssh`
    /// per `via` route, tried in order. A single route is a plain argv; more
    /// than one runs under `sh -c` joined with `||`, so each next address is
    /// tried only when the previous connection failed.
    pub fn mux_proxy_command(
        &self,
        route_id: &str,
        context: &InventoryContext,
    ) -> Option<Vec<String>> {
        let (via, socket, relay) = match self.route(route_id)? {
            MachineRoute::FrankenTermMux {
                via, socket, relay, ..
            } => (via, socket, relay),
            _ => return None,
        };
        let remote = match (relay, socket) {
            (Some(relay), _) => relay.clone(),
            (None, Some(socket)) => format!("nc -N -U {}", socket),
            (None, None) => return None,
        };
        let invocations: Vec<Vec<String>> = via
            .iter()
            .filter_map(|id| match self.route(id) {
                Some(MachineRoute::Ssh { target, .. }) => Some(target),
                _ => None,
            })
            .map(|target| {
                let mut argv = vec!["ssh".to_string()];
                match target {
                    RouteTarget::Alias(alias) => {
                        for option in &MUX_SSH_OPTIONS {
                            argv.push("-o".to_string());
                            argv.push(option.to_string());
                        }
                        argv.push(alias.clone());
                    }
                    RouteTarget::Direct {
                        host,
                        user,
                        port,
                        identity_files,
                    } => {
                        for file in identity_files {
                            argv.push("-i".to_string());
                            argv.push(context.expand(file).to_string_lossy().into_owned());
                        }
                        for option in &MUX_SSH_OPTIONS {
                            argv.push("-o".to_string());
                            argv.push(option.to_string());
                        }
                        if let Some(port) = port.filter(|port| *port != 22) {
                            argv.push("-p".to_string());
                            argv.push(port.to_string());
                        }
                        argv.push(format!("{}@{}", user, host));
                    }
                }
                argv
            })
            .collect();
        match invocations.len() {
            0 => None,
            1 => {
                let mut argv = invocations.into_iter().next().expect("one invocation");
                argv.push(remote);
                Some(argv)
            }
            _ => {
                let script = invocations
                    .iter()
                    .map(|argv| {
                        let mut words: Vec<String> = argv.iter().map(|w| shell_word(w)).collect();
                        words.push(format!("'{}'", remote));
                        words.join(" ")
                    })
                    .collect::<Vec<_>>()
                    .join(" || ");
                Some(vec!["sh".to_string(), "-c".to_string(), script])
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> InventoryContext {
        InventoryContext {
            directory: PathBuf::from("/inventory"),
            home: PathBuf::from("/home/me"),
        }
    }

    fn parse(text: &str) -> Result<MachineInventory, InventoryError> {
        MachineInventory::parse(text, &context())
    }

    const FLEET: &str = r##"
version = 1
default_group = "fleet"

[workspace]
name = "Jeff's fleet"

[groups.fleet]
name = "Fleet"
machines = ["trj", "css"]

[groups.macs]
machines = ["mmax"]

[machines.trj]
name = "Threadripper"
icon = "T"
cwd = "/data/projects"
tags = ["linux"]

[machines.trj.theme]
accent = "#ff00ff"

[machines.trj.theme.colors]
background = "#0a0e14"
foreground = "#b3f4ff"
cursor_bg = "#ff00ff"
ansi = ["#1a1f29","#ff3366","#39ffb4","#ffe566","#00aaff","#ff00ff","#00ffff","#c7d5e0"]
brights = ["#3d4f5f","#ff6b9d","#6bffcd","#ffef99","#66ccff","#ff66ff","#66ffff","#ffffff"]

[machines.trj.theme.desktop]
background_gradient = { colors = ["#0a0e14","#0e1420","#12182a"], angle = -45, opacity = 0.92 }

[[machines.trj.routes]]
id = "tailscale"
host = "100.91.120.17"
user = "ubuntu"
identity_file = "~/.ssh/trj_ed25519"

[[machines.trj.routes]]
id = "lan"
host = "10.10.10.1"
user = "ubuntu"
identity_file = "~/.ssh/trj_ed25519"

[[machines.trj.routes]]
id = "mux"
kind = "frankenterm_mux"
via = ["tailscale", "lan"]
socket = "/run/user/1000/frankenterm/sock"

[machines.css]
[[machines.css.routes]]
id = "ssh"
ssh = "css"

[machines.mmax]
name = "Mac mini Max"
[[machines.mmax.routes]]
id = "lan"
host = "192.168.1.240"
user = "jemanuel"
identity_file = "/home/me/.ssh/id_ed25519"

[[machines.mmax.routes]]
id = "mux"
kind = "frankenterm_mux"
via = "lan"
relay = "/Users/jemanuel/.local/bin/ft-mux-proxy"
"##;

    #[test]
    fn a_fleet_resolves_in_reading_order_with_explicit_and_automatic_themes() {
        let inventory = parse(FLEET).unwrap();
        assert_eq!(inventory.workspace_name, "Jeff's fleet");
        assert_eq!(inventory.default_group, "fleet");
        let order: Vec<&str> = inventory.machines.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(order, ["trj", "css", "mmax"]);

        let trj = inventory.machine("trj").unwrap();
        assert!(!trj.theme.automatic);
        assert_eq!(trj.theme.name, "trj");
        let p = &trj.theme.palette;
        assert_eq!(p.background.hex(), "#0A0E14");
        assert_eq!(p.foreground.hex(), "#B3F4FF");
        assert_eq!(trj.theme.accent.hex(), "#FF00FF");
        // Derived: cursor text is the background, border follows the cursor,
        // selection blends the accent into the background.
        assert_eq!(p.cursor_fg, p.background);
        assert_eq!(p.cursor_border.hex(), "#FF00FF");
        assert_eq!(
            p.selection_bg,
            Rgba::parse("#ff00ff").unwrap().over(p.background, 0.35)
        );
        assert_eq!(p.ansi[1].hex(), "#FF3366");
        assert_eq!(p.ansi[15].hex(), "#FFFFFF");
        let gradient = trj.theme.background_gradient.as_ref().unwrap();
        assert_eq!(
            (gradient.angle, gradient.opacity, gradient.colors.len()),
            (-45.0, 0.92, 3)
        );

        // css and mmax declared no theme: distinct automatic hue themes,
        // neither one near trj's magenta.
        let css = inventory.machine("css").unwrap();
        let mmax = inventory.machine("mmax").unwrap();
        assert!(css.theme.automatic && mmax.theme.automatic);
        assert_ne!(css.theme.name, mmax.theme.name);
        assert_eq!(css.theme.name, "FrankenTerm Azure");
        assert_eq!(mmax.theme.name, "FrankenTerm Amber");
        assert_eq!(css.name, "css", "the name defaults to the id");
    }

    fn fleet_of(count: usize, extra: &str) -> MachineInventory {
        let mut text = String::from("version = 1\n");
        for index in 0..count {
            text.push_str(&format!(
                "[machines.m{:02}]\n[[machines.m{:02}.routes]]\nid = \"ssh\"\nssh = \"m{:02}\"\n",
                index, index, index
            ));
        }
        text.push_str(extra);
        parse(&text).unwrap()
    }

    #[test]
    fn a_small_fleet_gets_widely_separated_hues_avoiding_explicit_ones() {
        let inventory = fleet_of(8, "[machines.m00.theme]\nscheme = \"FrankenTerm Azure\"\n");
        assert!(!inventory.machines[0].theme.automatic);
        assert!(inventory.machines[1..].iter().all(|m| m.theme.automatic));
        assert_ne!(
            inventory.machines[1].theme.name, "FrankenTerm Azure",
            "an explicitly used theme is not reassigned"
        );
        let hues: Vec<f64> = inventory
            .machines
            .iter()
            .filter_map(|m| m.theme.accent.hue())
            .collect();
        assert_eq!(hues.len(), 8);
        for (i, a) in hues.iter().enumerate() {
            for b in &hues[i + 1..] {
                assert!(
                    hue_distance(*a, *b) >= 20.0,
                    "{} and {} are too close",
                    a,
                    b
                );
            }
        }
    }

    #[test]
    fn a_large_fleet_never_repeats_a_theme_name() {
        let inventory = fleet_of(30, "");
        let names: BTreeSet<&str> = inventory
            .machines
            .iter()
            .map(|m| m.theme.name.as_str())
            .collect();
        assert_eq!(names.len(), 30, "every machine gets its own theme name");
        // Past the distinct hues, a repeat hue is always marked by a suffix.
        for machine in &inventory.machines {
            let base: String = machine
                .theme
                .name
                .trim_end_matches(|c: char| c.is_ascii_digit() || c == ' ')
                .to_string();
            assert!(base.starts_with("FrankenTerm "), "{}", machine.theme.name);
        }
    }

    #[test]
    fn a_builtin_scheme_or_scheme_file_is_the_base_under_overrides() {
        let scheme = crate::COLOR_SCHEMES.keys().min().unwrap().clone();
        let text = format!(
            "version = 1\n[machines.a.theme]\nscheme = {:?}\n[machines.a.theme.colors]\nbackground = \"#123456\"\n[[machines.a.routes]]\nid = \"ssh\"\nssh = \"a\"\n",
            scheme
        );
        let inventory = parse(&text).unwrap();
        let theme = &inventory.machines[0].theme;
        assert_eq!(theme.name, scheme);
        assert_eq!(
            theme.palette.background.hex(),
            "#123456",
            "overrides win over the scheme"
        );
        let base = Overrides::from_palette(&crate::COLOR_SCHEMES[&scheme]);
        let foreground = base.foreground.expect("built-in schemes set a foreground");
        assert_eq!(theme.palette.foreground, foreground);
        assert_eq!(&theme.palette.ansi[..8], &base.ansi.unwrap()[..]);

        let missing = "version = 1\n[machines.a.theme]\nscheme = \"No Such Scheme\"\n[[machines.a.routes]]\nid = \"ssh\"\nssh = \"a\"\n";
        assert_eq!(parse(missing).unwrap_err().path, "machines.a.theme.scheme");
        let file = "version = 1\n[machines.a.theme]\nscheme = \"themes/missing.toml\"\n[[machines.a.routes]]\nid = \"ssh\"\nssh = \"a\"\n";
        let err = parse(file).unwrap_err();
        assert!(
            err.message.contains("/inventory/themes/missing.toml"),
            "{}",
            err
        );
    }

    #[test]
    fn every_builtin_scheme_and_distinct_theme_resolves_completely() {
        let catalog = theme_catalog();
        assert!(catalog.len() > 1000, "{}", catalog.len());
        let names: BTreeSet<&str> = catalog.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), catalog.len(), "catalog names are unique");
        for (index, theme) in catalog.iter().take(distinct_theme_count()).enumerate() {
            assert_eq!(theme.name, distinct_theme_name(index));
            assert_eq!(theme.palette.background.0[3], 255);
        }
        for theme in &catalog {
            assert!(
                theme.palette.indexed.keys().all(|index| *index >= 16),
                "{}",
                theme.name
            );
        }
    }

    #[test]
    fn mux_proxy_commands_match_the_desktops_long_standing_forms() {
        let inventory = parse(FLEET).unwrap();
        let trj = inventory.machine("trj").unwrap();
        let command = trj.mux_proxy_command("mux", &context()).unwrap();
        let options = "-o ConnectTimeout=10 -o BatchMode=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=4 -o StrictHostKeyChecking=accept-new";
        assert_eq!(command[..2], ["sh", "-c"]);
        assert_eq!(
            command[2],
            format!(
                "ssh -i /home/me/.ssh/trj_ed25519 {} ubuntu@100.91.120.17 'nc -N -U /run/user/1000/frankenterm/sock' || ssh -i /home/me/.ssh/trj_ed25519 {} ubuntu@10.10.10.1 'nc -N -U /run/user/1000/frankenterm/sock'",
                options, options
            )
        );
        let mmax = inventory.machine("mmax").unwrap();
        let mut expected = vec!["ssh", "-i", "/home/me/.ssh/id_ed25519"];
        for option in &MUX_SSH_OPTIONS {
            expected.push("-o");
            expected.push(option);
        }
        expected.push("jemanuel@192.168.1.240");
        expected.push("/Users/jemanuel/.local/bin/ft-mux-proxy");
        assert_eq!(mmax.mux_proxy_command("mux", &context()).unwrap(), expected);
        assert_eq!(trj.mux_proxy_command("lan", &context()), None);
    }

    #[test]
    fn malformed_inventories_are_refused_at_the_exact_key() {
        let ok_route = "[[machines.a.routes]]\nid = \"ssh\"\nssh = \"a\"\n";
        let cases: Vec<(String, &str)> = vec![
            ("[machines.a]\n".to_string() + ok_route, "version"),
            (format!("version = 2\n{}", ok_route), "version"),
            (format!("version = 1\nflavour = 1\n{}", ok_route), "flavour"),
            (format!("version = 1\nx-note = 1\n[machines.a]\ncolour = 1\n{}", ok_route), "machines.a.colour"),
            ("version = 1\n[machines.a]\n".to_string(), "machines.a.routes"),
            ("version = 1\n[machines.A]\n[[machines.A.routes]]\nid = \"s\"\nssh = \"a\"\n".to_string(), "machines.A"),
            (format!("version = 1\n[machines.a.theme.colors]\nbackground = \"red\"\n{}", ok_route), "machines.a.theme.colors.background"),
            (format!("version = 1\n[machines.a.theme.colors]\nansi = [\"#000000\"]\n{}", ok_route), "machines.a.theme.colors.ansi"),
            (format!("version = 1\n[machines.a.theme.colors.indexed]\n\"7\" = \"#000000\"\n{}", ok_route), "machines.a.theme.colors.indexed.7"),
            (format!("version = 1\n[groups.g]\nmachines = [\"b\"]\n{}", ok_route), "groups.g.machines[0]"),
            (format!("version = 1\ndefault_group = \"nope\"\n{}", ok_route), "default_group"),
            ("version = 1\n[[machines.a.routes]]\nid = \"s\"\nssh = \"a\"\nhost = \"h\"\n".to_string(), "machines.a.routes[0]"),
            ("version = 1\n[[machines.a.routes]]\nid = \"s\"\nssh = \"a\"\nuser = \"u\"\n".to_string(), "machines.a.routes[0].user"),
            ("version = 1\n[[machines.a.routes]]\nid = \"m\"\nkind = \"frankenterm_mux\"\nvia = \"nope\"\nsocket = \"/s\"\n".to_string(), "machines.a.routes[0].via"),
            ("version = 1\n[[machines.a.routes]]\nid = \"m\"\nkind = \"frankenterm_mux\"\nvia = \"s\"\nsocket = \"relative\"\n[[machines.a.routes]]\nid = \"s\"\nssh = \"a\"\n".to_string(), "machines.a.routes[0].socket"),
            ("version = 1\n[[machines.a.routes]]\nid = \"s\"\nssh = \"a\"\n[[machines.a.routes]]\nid = \"s\"\nssh = \"b\"\n".to_string(), "machines.a.routes[1].id"),
            ("version = 1\n[[machines.a.routes]]\nid = \"s\"\nhost = \"h\"\nuser = \"u\"\nport = 70000\n".to_string(), "machines.a.routes[0].port"),
            // The phone refuses these hosts, so the inventory does too.
            ("version = 1\n[[machines.a.routes]]\nid = \"s\"\nhost = \"me@h\"\nuser = \"u\"\n".to_string(), "machines.a.routes[0].host"),
            ("version = 1\n[[machines.a.routes]]\nid = \"s\"\nhost = \"h:22\"\nuser = \"u\"\n".to_string(), "machines.a.routes[0].host"),
            ("version = 1\n[[machines.a.routes]]\nid = \"s\"\nhost = \"[h]\"\nuser = \"u\"\n".to_string(), "machines.a.routes[0].host"),
            ("version = 1\n[[machines.a.routes]]\nid = \"s\"\nhost = \"h\"\nuser = \"a u\"\n".to_string(), "machines.a.routes[0].user"),
            (format!("version = 1\n[machines.a]\ntags = [\"{}\"]\n{}", "t".repeat(129), ok_route), "machines.a.tags[0]"),
        ];
        for (text, path) in cases {
            match parse(&text) {
                Err(err) => assert_eq!(err.path, path, "{}\n---\n{}", err, text),
                Ok(_) => panic!("accepted:\n{}", text),
            }
        }
    }

    #[test]
    fn hosts_are_names_or_ip_addresses_the_phone_accepts() {
        for host in [
            "trj",
            "100.91.120.17",
            "css.example",
            "fe80::1",
            "[2001:db8::1]",
        ] {
            assert!(valid_host(host), "{}", host);
        }
        for host in [
            "", "a b", "me@h", "h/x", "h:22", "[h]", "[::1", "::1]", "a%b", "h#",
        ] {
            assert!(!valid_host(host), "{:?}", host);
        }
    }

    #[test]
    fn a_first_inventory_pins_a_distinct_theme_to_every_host() {
        let aliases: Vec<String> = ["css", "Trj", "my host", "css"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let text = inventory_template(&aliases);
        let inventory = parse(&text).unwrap();
        let ids: Vec<&str> = inventory.machines.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["css", "trj", "my-host", "css-2"]);
        let names: BTreeSet<&str> = inventory
            .machines
            .iter()
            .map(|m| m.theme.name.as_str())
            .collect();
        assert_eq!(names.len(), 4);
        assert!(
            inventory.machines.iter().all(|m| !m.theme.automatic),
            "pinned, not automatic"
        );
        assert_eq!(inventory.machines[2].name, "my host");
        assert_eq!(
            inventory.machines[1].routes,
            vec![MachineRoute::Ssh {
                id: "ssh".into(),
                target: RouteTarget::Alias("Trj".into())
            }]
        );
    }

    #[test]
    fn colours_parse_strictly_and_round_trip_exactly() {
        assert_eq!(Rgba::parse("#0a0e14").unwrap().hex(), "#0A0E14");
        assert_eq!(Rgba::parse("#FFFFFF33").unwrap().hex(), "#FFFFFF33");
        for bad in [
            "0a0e14",
            "#0a0e1",
            "#0a0e14f",
            "#gggggg",
            "rgba(1,2,3,4)",
            "",
        ] {
            assert_eq!(Rgba::parse(bad), None, "{}", bad);
        }
        for value in 0..=255u8 {
            let tuple = SrgbaTuple(f32::from(value) / 255.0, 0.0, 1.0, 1.0);
            assert_eq!(Rgba::from_srgba(tuple).0[0], value);
        }
    }
}
