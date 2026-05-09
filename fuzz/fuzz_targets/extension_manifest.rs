#![no_main]

use arbitrary::Arbitrary;
use frankenterm_scripting::ParsedManifest;
use libfuzzer_sys::fuzz_target;
use toml::Value;
use toml::map::Map;

const MAX_RAW_TOML_BYTES: usize = 64 * 1024;
const MAX_RENDERED_TOML_BYTES: usize = 64 * 1024;
const MAX_TEXT_CHARS: usize = 96;
const MAX_LIST_ITEMS: usize = 8;
const MAX_HOOKS: usize = 8;

#[derive(Arbitrary, Debug)]
enum FuzzInput<'a> {
    Raw(&'a [u8]),
    Structured(Box<ManifestCase>),
}

#[derive(Arbitrary, Debug)]
struct ManifestCase {
    decode_mode: DecodeMode,
    wire_mode: WireMode,
    engine_type: RawEngineType,
    name: String,
    version: String,
    description: String,
    authors: Vec<String>,
    license: Option<String>,
    homepage: Option<String>,
    min_version: Option<String>,
    entry: String,
    filesystem: Vec<RawFilesystemPermission>,
    environment: Vec<String>,
    network: bool,
    pane_access: bool,
    hooks: Vec<(String, String)>,
    asset_themes: Vec<String>,
}

#[derive(Arbitrary, Debug)]
enum DecodeMode {
    Valid,
    MissingExtensionTable,
    MissingName,
    MistypedExtensionTable,
    MistypedName,
    MistypedEngineTable,
    MistypedPermissionsTable,
    DeepNesting,
}

#[derive(Arbitrary, Debug)]
enum WireMode {
    Exact,
    Truncate(u8),
    AppendGarbage,
}

#[derive(Arbitrary, Debug)]
enum RawEngineType {
    Wasm,
    Lua,
    Both,
    Unknown(String),
}

#[derive(Arbitrary, Debug)]
enum RawFilesystemPermission {
    Read(String),
    Write(String),
    Bare(String),
}

impl ManifestCase {
    fn into_toml_string(self) -> Option<String> {
        let mut root = self.build_root_table();
        apply_decode_mode(&mut root, self.decode_mode);

        let mut bytes = toml::to_string(&Value::Table(root)).ok()?.into_bytes();
        apply_wire_mode(&mut bytes, self.wire_mode);
        if bytes.len() > MAX_RENDERED_TOML_BYTES {
            return None;
        }

        String::from_utf8(bytes).ok()
    }

    fn build_root_table(&self) -> Map<String, Value> {
        let mut root = Map::new();

        let mut extension = Map::new();
        extension.insert(
            "name".to_string(),
            Value::String(limited_text(&self.name, "generated-extension")),
        );
        extension.insert(
            "version".to_string(),
            Value::String(limited_text(&self.version, "0.1.0")),
        );
        extension.insert(
            "description".to_string(),
            Value::String(limited_text(&self.description, "generated manifest")),
        );
        extension.insert(
            "authors".to_string(),
            Value::Array(
                self.authors
                    .iter()
                    .take(MAX_LIST_ITEMS)
                    .map(|author| Value::String(limited_text(author, "fuzzer")))
                    .collect(),
            ),
        );
        if let Some(license) = &self.license {
            extension.insert(
                "license".to_string(),
                Value::String(limited_text(license, "Apache-2.0")),
            );
        }
        if let Some(homepage) = &self.homepage {
            extension.insert(
                "homepage".to_string(),
                Value::String(limited_text(homepage, "https://example.invalid/ext")),
            );
        }
        if let Some(min_version) = &self.min_version {
            extension.insert(
                "min_frankenterm_version".to_string(),
                Value::String(limited_text(min_version, "0.1.0")),
            );
        }
        root.insert("extension".to_string(), Value::Table(extension));

        let mut engine = Map::new();
        engine.insert(
            "type".to_string(),
            Value::String(self.engine_type.manifest_value()),
        );
        engine.insert(
            "entry".to_string(),
            Value::String(limited_text(
                &self.entry,
                self.engine_type.default_entry_name(),
            )),
        );
        root.insert("engine".to_string(), Value::Table(engine));

        let mut permissions = Map::new();
        permissions.insert(
            "filesystem".to_string(),
            Value::Array(
                self.filesystem
                    .iter()
                    .take(MAX_LIST_ITEMS)
                    .map(|permission| Value::String(permission.manifest_value()))
                    .collect(),
            ),
        );
        permissions.insert(
            "environment".to_string(),
            Value::Array(
                self.environment
                    .iter()
                    .take(MAX_LIST_ITEMS)
                    .map(|env| Value::String(limited_text(env, "FRANKENTERM_*")))
                    .collect(),
            ),
        );
        permissions.insert("network".to_string(), Value::Boolean(self.network));
        permissions.insert("pane_access".to_string(), Value::Boolean(self.pane_access));
        root.insert("permissions".to_string(), Value::Table(permissions));

        let mut hooks = Map::new();
        for (idx, (event, handler)) in self.hooks.iter().take(MAX_HOOKS).enumerate() {
            hooks.insert(
                safe_key(event, idx, "event"),
                Value::String(limited_text(handler, "handle_event")),
            );
        }
        if !hooks.is_empty() {
            root.insert("hooks".to_string(), Value::Table(hooks));
        }

        let mut assets = Map::new();
        assets.insert(
            "themes".to_string(),
            Value::Array(
                self.asset_themes
                    .iter()
                    .take(MAX_LIST_ITEMS)
                    .map(|theme| Value::String(limited_text(theme, "assets/theme.toml")))
                    .collect(),
            ),
        );
        root.insert("assets".to_string(), Value::Table(assets));

        root
    }
}

impl RawEngineType {
    fn manifest_value(&self) -> String {
        match self {
            Self::Wasm => "wasm".to_string(),
            Self::Lua => "lua".to_string(),
            Self::Both => "both".to_string(),
            Self::Unknown(value) => limited_text(value, "unknown"),
        }
    }

    fn default_entry_name(&self) -> &'static str {
        match self {
            Self::Lua => "main.lua",
            Self::Wasm | Self::Both | Self::Unknown(_) => "main.wasm",
        }
    }
}

impl RawFilesystemPermission {
    fn manifest_value(&self) -> String {
        match self {
            Self::Read(path) => format!("read:{}", limited_text(path, "/tmp/frankenterm-read")),
            Self::Write(path) => format!("write:{}", limited_text(path, "/tmp/frankenterm-write")),
            Self::Bare(path) => limited_text(path, "/tmp/frankenterm"),
        }
    }
}

fn apply_decode_mode(root: &mut Map<String, Value>, mode: DecodeMode) {
    match mode {
        DecodeMode::Valid => {}
        DecodeMode::MissingExtensionTable => {
            root.remove("extension");
        }
        DecodeMode::MissingName => {
            if let Some(Value::Table(extension)) = root.get_mut("extension") {
                extension.remove("name");
            }
        }
        DecodeMode::MistypedExtensionTable => {
            root.insert("extension".to_string(), Value::String("wrong".to_string()));
        }
        DecodeMode::MistypedName => {
            if let Some(Value::Table(extension)) = root.get_mut("extension") {
                extension.insert("name".to_string(), Value::Integer(7));
            }
        }
        DecodeMode::MistypedEngineTable => {
            root.insert(
                "engine".to_string(),
                Value::Array(vec![Value::String("wasm".to_string())]),
            );
        }
        DecodeMode::MistypedPermissionsTable => {
            root.insert(
                "permissions".to_string(),
                Value::String("network=true".to_string()),
            );
        }
        DecodeMode::DeepNesting => {
            root.insert("deep".to_string(), deep_value(18));
        }
    }
}

fn apply_wire_mode(bytes: &mut Vec<u8>, mode: WireMode) {
    match mode {
        WireMode::Exact => {}
        WireMode::Truncate(seed) => {
            if bytes.is_empty() {
                return;
            }
            let keep = bytes
                .len()
                .saturating_sub(usize::from(seed) % (bytes.len() + 1));
            bytes.truncate(keep);
        }
        WireMode::AppendGarbage => {
            bytes.extend_from_slice(b"\n[extension\nname = ");
        }
    }
}

fn deep_value(depth: usize) -> Value {
    let mut value = Value::String("leaf".to_string());
    for idx in 0..depth {
        let mut table = Map::new();
        table.insert(format!("level_{idx}"), value);
        value = Value::Table(table);
    }
    value
}

fn parse_manifest(text: &str) {
    let _ = ParsedManifest::from_toml_str(text);
}

fn limited_text(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return fallback.to_string();
    }
    trimmed.chars().take(MAX_TEXT_CHARS).collect()
}

fn safe_key(value: &str, idx: usize, fallback_prefix: &str) -> String {
    let key = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .take(32)
        .collect::<String>();
    if key.is_empty() {
        format!("{fallback_prefix}_{idx}")
    } else {
        key
    }
}

fuzz_target!(|input: FuzzInput| {
    match input {
        FuzzInput::Raw(bytes) => {
            if bytes.len() > MAX_RAW_TOML_BYTES {
                return;
            }
            let Ok(text) = std::str::from_utf8(bytes) else {
                return;
            };
            parse_manifest(text);
        }
        FuzzInput::Structured(case) => {
            let Some(text) = case.into_toml_string() else {
                return;
            };
            parse_manifest(&text);
        }
    }
});
