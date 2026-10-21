//! `ft mobile export`: the machine inventory as the phone's public workspace
//! document (`ftworkspace` version 1). The phone imports it as is, or the Mac
//! companion seals it together with the private keys the user selects.
//!
//! Exported: machines, groups, fully resolved themes, SSH routes, the public
//! identities named by each route (`<identity>.pub`, never the private file)
//! and each endpoint's host keys from `known_hosts`, hashed entries included.
//! Not exported: desktop-only decoration, and mux routes until the phone can
//! attach to mux panes. Everything left out is reported, never dropped
//! silently.

use crate::machines::{
    InventoryContext, Machine, MachineInventory, MachineRoute, MachineTheme, RouteTarget,
};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{json, Map, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;
use std::path::{Path, PathBuf};

/// Stable across exports, so a new export replaces the phone's earlier one.
pub const EXPORTER_ID: &str = "frankenterm.machines";

/// Something the export left out or could not make usable, with the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportNote {
    pub machine: String,
    pub route: Option<String>,
    pub message: String,
}

impl std::fmt::Display for ExportNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.route {
            Some(route) => write!(f, "{} ({}): {}", self.machine, route, self.message),
            None => write!(f, "{}: {}", self.machine, self.message),
        }
    }
}

pub struct MobileExport {
    /// The complete `ftworkspace` document.
    pub document: Value,
    pub machines: usize,
    pub routes: usize,
    pub notes: Vec<ExportNote>,
}

/// Where SSH settings come from. Tests supply text; the CLI reads the files.
pub struct SshSources {
    /// `~/.ssh/config`, used only for `ssh = "alias"` routes.
    pub config: Option<String>,
    /// `known_hosts` contents by path, for every file a route may name.
    pub known_hosts: BTreeMap<PathBuf, String>,
    /// The known_hosts files of a direct route (OpenSSH's defaults).
    pub default_known_hosts: Vec<PathBuf>,
    /// `.pub` contents by path; a route's identity `x` reads `x.pub`.
    pub public_keys: BTreeMap<PathBuf, String>,
}

impl SshSources {
    /// Reads the user's real files: SSH config, the default known_hosts files
    /// and every `.pub` an inventory route can name. Missing files are simply
    /// absent; the export reports what that leaves unusable.
    pub fn from_disk(inventory: &MachineInventory, context: &InventoryContext) -> Self {
        let ssh = context.home.join(".ssh");
        let read = |path: &Path| std::fs::read_to_string(path).ok();
        let config = read(&ssh.join("config"));
        let resolver = alias_resolver(config.as_deref(), &context.home);
        let default_known_hosts = vec![ssh.join("known_hosts"), ssh.join("known_hosts2")];
        let mut known_files: BTreeSet<PathBuf> = default_known_hosts.iter().cloned().collect();
        let mut identities = BTreeSet::new();
        for machine in &inventory.machines {
            for route in &machine.routes {
                match route {
                    MachineRoute::Ssh {
                        target: RouteTarget::Direct { identity_files, .. },
                        ..
                    } => identities.extend(identity_files.iter().map(|f| context.expand(f))),
                    MachineRoute::Ssh {
                        target: RouteTarget::Alias(alias),
                        ..
                    } => {
                        if let Some(resolver) = resolver.as_ref() {
                            let options = resolver.for_host(alias);
                            identities.extend(split_paths(options.get("identityfile"), context));
                            known_files
                                .extend(split_paths(options.get("userknownhostsfile"), context));
                        }
                    }
                    MachineRoute::FrankenTermMux { .. } => {}
                }
            }
        }
        Self {
            config,
            known_hosts: known_files
                .into_iter()
                .filter_map(|path| read(&path).map(|text| (path, text)))
                .collect(),
            default_known_hosts,
            public_keys: identities
                .into_iter()
                .filter_map(|path| {
                    let public = public_key_path(&path);
                    read(&public).map(|text| (public, text))
                })
                .collect(),
        }
    }
}

fn alias_resolver(config: Option<&str>, home: &Path) -> Option<frankenterm_ssh::Config> {
    let text = config?;
    let mut resolver = frankenterm_ssh::Config::new();
    // A passive export never runs a local command to evaluate `Match exec`,
    // and expands `~`/`%d` against this home and no other environment value.
    resolver.set_match_exec_policy(frankenterm_ssh::MatchExecPolicy::Deny);
    let mut environment = frankenterm_ssh::ConfigMap::new();
    environment.insert("HOME".to_string(), home.to_string_lossy().into_owned());
    resolver.assign_environment(environment);
    resolver.add_config_string(text);
    Some(resolver)
}

/// The concrete `Host` aliases in the user's SSH config, without patterns.
pub fn ssh_config_hosts(home: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(home.join(".ssh").join("config")).unwrap_or_default();
    alias_resolver(Some(&text), home)
        .map(|resolver| resolver.enumerate_hosts())
        .unwrap_or_default()
}

fn split_paths(value: Option<&String>, context: &InventoryContext) -> Vec<PathBuf> {
    value
        .map(|text| text.split_whitespace().map(|p| context.expand(p)).collect())
        .unwrap_or_default()
}

fn public_key_path(identity: &Path) -> PathBuf {
    if identity.extension().is_some_and(|ext| ext == "pub") {
        identity.to_path_buf()
    } else {
        let mut text = identity.as_os_str().to_owned();
        text.push(".pub");
        PathBuf::from(text)
    }
}

/// An OpenSSH public key line: algorithm, base64 blob, optional comment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    pub algorithm: String,
    pub blob: Vec<u8>,
}

impl PublicKey {
    pub fn parse_line(line: &str) -> Option<Self> {
        let mut words = line.split_whitespace();
        let algorithm = words.next()?.to_string();
        let blob = STANDARD.decode(words.next()?).ok()?;
        // The blob starts with its own algorithm name; a mismatch is not a key.
        let name_length = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
        if blob.get(4..4 + name_length)? != algorithm.as_bytes() {
            return None;
        }
        Some(Self { algorithm, blob })
    }

    /// OpenSSH's `SHA256:` fingerprint, the form the phone pins.
    pub fn fingerprint(&self) -> String {
        format!(
            "SHA256:{}",
            STANDARD_NO_PAD.encode(Sha256::digest(&self.blob))
        )
    }
}

/// Host keys for one endpoint from known_hosts text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostKeys {
    pub pins: Vec<PublicKey>,
    pub revoked: Vec<PublicKey>,
    /// `@cert-authority` entries matched: the phone cannot use them.
    pub certificate_authorities: usize,
}

/// OpenSSH matching: port 22 uses the bare name, other ports `[name]:port`;
/// patterns may be hashed (`|1|salt|hmac`), use `*`/`?`, or negate with `!`.
pub fn host_keys(known_hosts: &str, host: &str, port: u16) -> HostKeys {
    let name = if port == 22 {
        host.to_ascii_lowercase()
    } else {
        format!("[{}]:{}", host.to_ascii_lowercase(), port)
    };
    let mut keys = HostKeys::default();
    for line in known_hosts.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        let mut first = match words.next() {
            Some(word) => word,
            None => continue,
        };
        let marker = if first.starts_with('@') {
            let marker = first;
            first = match words.next() {
                Some(word) => word,
                None => continue,
            };
            Some(marker)
        } else {
            None
        };
        if !patterns_match(first, &name) {
            continue;
        }
        let rest: Vec<&str> = words.collect();
        let key = match PublicKey::parse_line(&rest.join(" ")) {
            Some(key) => key,
            None => continue,
        };
        match marker {
            None => {
                if !keys.pins.contains(&key) {
                    keys.pins.push(key)
                }
            }
            Some("@revoked") => {
                if !keys.revoked.contains(&key) {
                    keys.revoked.push(key)
                }
            }
            Some(_) => keys.certificate_authorities += 1,
        }
    }
    // A revoked key is never also a pin.
    let revoked = keys.revoked.clone();
    keys.pins.retain(|key| !revoked.contains(key));
    keys
}

fn patterns_match(field: &str, name: &str) -> bool {
    if let Some(hashed) = field.strip_prefix("|1|") {
        let mut parts = hashed.splitn(2, '|');
        let (salt, expected) = match (parts.next(), parts.next()) {
            (Some(salt), Some(hash)) => (STANDARD.decode(salt), STANDARD.decode(hash)),
            _ => return false,
        };
        let (salt, expected) = match (salt, expected) {
            (Ok(salt), Ok(expected)) => (salt, expected),
            _ => return false,
        };
        let mut mac = match <Hmac<Sha1> as KeyInit>::new_from_slice(&salt) {
            Ok(mac) => mac,
            Err(_) => return false,
        };
        mac.update(name.as_bytes());
        return mac.verify_slice(&expected).is_ok();
    }
    let mut matched = false;
    for pattern in field.split(',') {
        if let Some(negated) = pattern.strip_prefix('!') {
            if glob(&negated.to_ascii_lowercase(), name) {
                return false;
            }
        } else if glob(&pattern.to_ascii_lowercase(), name) {
            matched = true;
        }
    }
    matched
}

/// `*` and `?` wildcards over bytes, as OpenSSH's match_pattern.
fn glob(pattern: &str, text: &str) -> bool {
    let (p, t) = (pattern.as_bytes(), text.as_bytes());
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

fn short_digest(text: &str) -> String {
    Sha256::digest(text.as_bytes())[..12]
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn theme_json(id: &str, theme: &MachineTheme) -> Value {
    let p = &theme.palette;
    let mut fields = Map::new();
    fields.insert("id".into(), json!(id));
    fields.insert("name".into(), json!(theme.name));
    fields.insert("foreground".into(), json!(p.foreground.hex()));
    fields.insert("background".into(), json!(p.background.hex()));
    fields.insert("accent".into(), json!(theme.accent.hex()));
    fields.insert("cursor_foreground".into(), json!(p.cursor_fg.hex()));
    fields.insert("cursor_background".into(), json!(p.cursor_bg.hex()));
    fields.insert("cursor_border".into(), json!(p.cursor_border.hex()));
    fields.insert("selection_foreground".into(), json!(p.selection_fg.hex()));
    fields.insert("selection_background".into(), json!(p.selection_bg.hex()));
    fields.insert(
        "ansi".into(),
        Value::Array(p.ansi.iter().map(|c| json!(c.hex())).collect()),
    );
    if !p.indexed.is_empty() {
        let indexed: Map<String, Value> = p
            .indexed
            .iter()
            .map(|(index, color)| (index.to_string(), json!(color.hex())))
            .collect();
        fields.insert("indexed".into(), Value::Object(indexed));
    }
    if let Some(font) = &theme.font {
        let mut value = Map::new();
        value.insert("family".into(), json!(font.family));
        if let Some(size) = font.size {
            value.insert("point_size".into(), json!(size));
        }
        fields.insert("font".into(), Value::Object(value));
    }
    Value::Object(fields)
}

/// The catalog of every theme as phone theme records, for the app bundle.
pub fn theme_catalog_json() -> Value {
    let themes: Vec<Value> = crate::machines::theme_catalog()
        .iter()
        .enumerate()
        .map(|(index, theme)| theme_json(&format!("catalog.{}", index), theme))
        .collect();
    json!({
        "format": "ftthemes",
        "version": 1,
        "distinct": crate::machines::distinct_theme_count(),
        "themes": themes,
    })
}

struct Endpoint {
    host: String,
    port: u16,
    user: String,
    identities: Vec<PathBuf>,
    host_key_alias: Option<String>,
    known_hosts: Vec<PathBuf>,
}

fn resolve_endpoint(
    machine: &Machine,
    route: &str,
    target: &RouteTarget,
    sources: &SshSources,
    resolver: Option<&frankenterm_ssh::Config>,
    context: &InventoryContext,
    notes: &mut Vec<ExportNote>,
) -> Option<Endpoint> {
    let note = |notes: &mut Vec<ExportNote>, message: String| {
        notes.push(ExportNote {
            machine: machine.id.clone(),
            route: Some(route.to_string()),
            message,
        })
    };
    match target {
        RouteTarget::Direct {
            host,
            user,
            port,
            identity_files,
        } => Some(Endpoint {
            host: host.clone(),
            port: port.unwrap_or(22),
            user: user.clone(),
            identities: identity_files.iter().map(|f| context.expand(f)).collect(),
            host_key_alias: None,
            known_hosts: sources.default_known_hosts.clone(),
        }),
        RouteTarget::Alias(alias) => {
            let resolver = match resolver {
                Some(resolver) => resolver,
                None => {
                    note(
                        notes,
                        "no ~/.ssh/config to resolve this alias; not exported".into(),
                    );
                    return None;
                }
            };
            let options = resolver.for_host(alias);
            for unsupported in ["proxyjump", "proxycommand"] {
                if options.get(unsupported).is_some_and(|v| v != "none") {
                    note(
                        notes,
                        format!(
                            "uses {} in ~/.ssh/config, which the phone cannot run; not exported",
                            unsupported
                        ),
                    );
                    return None;
                }
            }
            let port = match options.get("port").map(|p| p.parse::<u16>()) {
                None => 22,
                Some(Ok(port)) if port > 0 => port,
                Some(_) => {
                    note(
                        notes,
                        "its port in ~/.ssh/config is not a number; not exported".into(),
                    );
                    return None;
                }
            };
            let user = match options.get("user") {
                Some(user) => user.clone(),
                None => {
                    note(
                        notes,
                        "no User for this alias in ~/.ssh/config; not exported".into(),
                    );
                    return None;
                }
            };
            Some(Endpoint {
                host: options
                    .get("hostname")
                    .cloned()
                    .unwrap_or_else(|| alias.clone()),
                port,
                user,
                identities: split_paths(options.get("identityfile"), context),
                host_key_alias: options.get("hostkeyalias").cloned(),
                known_hosts: {
                    let files = split_paths(options.get("userknownhostsfile"), context);
                    if files.is_empty() {
                        sources.default_known_hosts.clone()
                    } else {
                        files
                    }
                },
            })
        }
    }
}

/// The phone's workspace for the inventory, or for one group of it.
pub fn export(
    inventory: &MachineInventory,
    source: &[u8],
    group: Option<&str>,
    sources: &SshSources,
    context: &InventoryContext,
) -> anyhow::Result<MobileExport> {
    let groups: Vec<_> = match group {
        Some(id) => vec![inventory
            .groups
            .iter()
            .find(|g| g.id == id)
            .ok_or_else(|| anyhow::anyhow!("no group named {:?} in the inventory", id))?],
        None => inventory.groups.iter().collect(),
    };
    let wanted: BTreeSet<&str> = groups
        .iter()
        .flat_map(|g| g.machines.iter().map(String::as_str))
        .collect();
    let resolver = alias_resolver(sources.config.as_deref(), &context.home);
    let mut notes = Vec::new();
    let (mut machines, mut routes, mut credentials, mut trusts, mut themes) = (
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        Vec::new(),
    );
    let mut exported = BTreeSet::new();

    for machine in inventory
        .machines
        .iter()
        .filter(|m| wanted.contains(m.id.as_str()))
    {
        let mut route_ids = Vec::new();
        let mut mux = 0;
        for route in &machine.routes {
            let (route_name, target) = match route {
                MachineRoute::Ssh { id, target } => (id, target),
                MachineRoute::FrankenTermMux { .. } => {
                    mux += 1;
                    continue;
                }
            };
            let endpoint = match resolve_endpoint(
                machine,
                route_name,
                target,
                sources,
                resolver.as_ref(),
                context,
                &mut notes,
            ) {
                Some(endpoint) => endpoint,
                None => continue,
            };
            let mut credential_ids = Vec::new();
            for identity in &endpoint.identities {
                let public = public_key_path(identity);
                let key = match sources.public_keys.get(&public).and_then(|text| {
                    text.lines()
                        .find(|l| !l.trim().is_empty())
                        .and_then(PublicKey::parse_line)
                }) {
                    Some(key) => key,
                    None => {
                        notes.push(ExportNote {
                            machine: machine.id.clone(),
                            route: Some(route_name.clone()),
                            message: format!(
                                "no readable public key at {}; that identity is not offered",
                                public.display()
                            ),
                        });
                        continue;
                    }
                };
                let fingerprint = key.fingerprint();
                let id = format!("c.{}", short_digest(&fingerprint));
                credentials.entry(id.clone()).or_insert_with(|| {
                    json!({
                        "id": id,
                        "algorithm": key.algorithm,
                        "public_fingerprint": fingerprint,
                        "requirement": "matching_private_key",
                        "private_material": "not_included",
                    })
                });
                if credential_ids.len() < 8 && !credential_ids.contains(&id) {
                    credential_ids.push(id);
                }
            }
            if credential_ids.is_empty() {
                notes.push(ExportNote {
                    machine: machine.id.clone(),
                    route: Some(route_name.clone()),
                    message:
                        "no public key is available, so the phone cannot sign in on this route"
                            .into(),
                });
            }
            let lookup = endpoint
                .host_key_alias
                .clone()
                .unwrap_or_else(|| endpoint.host.clone());
            let mut keys = HostKeys::default();
            for file in &endpoint.known_hosts {
                if let Some(text) = sources.known_hosts.get(file) {
                    let found = host_keys(text, &lookup, endpoint.port);
                    for key in found.pins {
                        if !keys.pins.contains(&key) {
                            keys.pins.push(key);
                        }
                    }
                    for key in found.revoked {
                        if !keys.revoked.contains(&key) {
                            keys.revoked.push(key);
                        }
                    }
                    keys.certificate_authorities += found.certificate_authorities;
                }
            }
            let revoked = keys.revoked.clone();
            keys.pins.retain(|key| !revoked.contains(key));
            if keys.pins.is_empty() {
                notes.push(ExportNote {
                    machine: machine.id.clone(),
                    route: Some(route_name.clone()),
                    message: format!(
                        "no host key for {} in known_hosts{}; connect once with ssh from this Mac, then export again",
                        lookup,
                        if keys.certificate_authorities > 0 { " (only a certificate authority, which the phone cannot use)" } else { "" }
                    ),
                });
            }
            let trust_key = format!(
                "{}:{}:{}",
                endpoint.host,
                endpoint.port,
                endpoint.host_key_alias.as_deref().unwrap_or("")
            );
            let trust_id = format!("h.{}", short_digest(&trust_key));
            trusts.entry(trust_id.clone()).or_insert_with(|| {
                let pin = |key: &PublicKey| json!({"algorithm": key.algorithm, "fingerprint": key.fingerprint()});
                let mut trust = json!({
                    "id": trust_id,
                    "host": endpoint.host,
                    "port": endpoint.port,
                    "claimed_source": "known_hosts",
                    "pins": keys.pins.iter().map(pin).collect::<Vec<_>>(),
                    "revoked_keys": keys.revoked.iter().map(pin).collect::<Vec<_>>(),
                });
                if let Some(alias) = &endpoint.host_key_alias {
                    trust["host_key_alias"] = json!(alias);
                }
                trust
            });
            let id = format!("r.{}.{}", machine.id, route_name);
            let mut record = json!({
                "id": id,
                "mode": "direct_ssh",
                "host": endpoint.host,
                "port": endpoint.port,
                "user": endpoint.user,
                "credential_refs": credential_ids,
                "authentication_methods": ["publickey"],
                "host_trust_ref": trust_id,
                "jump_routes": [],
                "session_creation": "on_select",
            });
            if let Some(alias) = &endpoint.host_key_alias {
                record["host_key_alias"] = json!(alias);
            }
            routes.push(record);
            route_ids.push(id);
        }
        if mux > 0 {
            notes.push(ExportNote {
                machine: machine.id.clone(),
                route: None,
                message: "mux routes stay on the desktop until the phone can attach to mux panes"
                    .into(),
            });
        }
        if route_ids.is_empty() {
            notes.push(ExportNote {
                machine: machine.id.clone(),
                route: None,
                message: "no exportable SSH route; the machine is left out".into(),
            });
            continue;
        }
        let theme_id = format!("t.{}", machine.id);
        themes.push(theme_json(&theme_id, &machine.theme));
        let mut record = json!({
            "id": format!("m.{}", machine.id),
            "name": machine.name,
            "theme_ref": theme_id,
            "route_refs": route_ids,
            "preferred_route_ref": route_ids[0],
            "auto_connect": false,
            "tags": machine.tags,
        });
        if let Some(label) = &machine.short_label {
            record["short_label"] = json!(label);
        }
        machines.push(record);
        exported.insert(machine.id.clone());
    }

    let group_records: Vec<Value> = groups
        .iter()
        .filter_map(|group| {
            let order: Vec<String> = group
                .machines
                .iter()
                .filter(|id| exported.contains(id.as_str()))
                .map(|id| format!("m.{}", id))
                .collect();
            if order.is_empty() {
                None
            } else {
                Some(json!({"id": format!("g.{}", group.id), "name": group.name, "machine_order": order, "auto_connect": false}))
            }
        })
        .collect();
    let initial_group = group_records
        .iter()
        .find(|g| g["id"] == json!(format!("g.{}", inventory.default_group)))
        .or_else(|| group_records.first())
        .ok_or_else(|| {
            anyhow::anyhow!("no machine in the selection has an exportable SSH route")
        })?;
    let initial_machine = initial_group["machine_order"][0].clone();
    let initial_group_id = initial_group["id"].clone();
    let workspace_id = match group {
        Some(group) => format!("w.{}.{}", inventory.workspace_id, group),
        None => format!("w.{}", inventory.workspace_id),
    };
    let revision = Sha256::digest(source)
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let bundle_id = format!(
        "b.{}",
        short_digest(&format!("{}:{}:{}", revision, nanos, std::process::id()))
    );
    let machine_count = machines.len();
    let route_count = routes.len();
    let document = json!({
        "format": "ftworkspace",
        "version": 1,
        "bundle_id": bundle_id,
        "required_capabilities": [],
        "source": {
            "exporter_id": EXPORTER_ID,
            "kind": "machines_toml",
            "configuration_revision": format!("sha256:{}", revision),
        },
        "workspace": {
            "id": workspace_id,
            "name": inventory.workspace_name,
            "groups": group_records,
            "machines": machines,
            "routes": routes,
            "credentials": credentials.into_values().collect::<Vec<_>>(),
            "host_trust": trusts.into_values().collect::<Vec<_>>(),
            "themes": themes,
            "preferences": {
                "initial_group_ref": initial_group_id,
                "initial_machine_ref": initial_machine,
                "connect_on_open": false,
            },
        },
    });
    Ok(MobileExport {
        document,
        machines: machine_count,
        routes: route_count,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway test key; `ssh-keygen -lf` prints FINGERPRINT for it.
    const ED25519: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIJReX44NbzXpRwWjwAJSjGbVZu0TR3fTviPvQF9JkPEr";
    const FINGERPRINT: &str = "SHA256:0QMmKpubmKjdQYS8jbWmzKi7cvVkxacRPLyJX118Too";

    fn context() -> InventoryContext {
        InventoryContext {
            directory: PathBuf::from("/home/me/.config/frankenterm"),
            home: PathBuf::from("/home/me"),
        }
    }

    #[test]
    fn public_keys_parse_only_when_the_blob_names_the_same_algorithm() {
        let key = PublicKey::parse_line(&format!("ssh-ed25519 {} me@mac", ED25519)).unwrap();
        assert_eq!(key.algorithm, "ssh-ed25519");
        assert_eq!(key.fingerprint(), FINGERPRINT, "matches ssh-keygen -lf");
        assert_eq!(PublicKey::parse_line(&format!("ssh-rsa {}", ED25519)), None);
        assert_eq!(PublicKey::parse_line("ssh-ed25519 !!!"), None);
    }

    #[test]
    fn known_hosts_match_plain_ported_wildcard_negated_and_hashed_names() {
        let line = |hosts: &str| format!("{} ssh-ed25519 {}\n", hosts, ED25519);
        assert_eq!(
            host_keys(&line("trj,100.91.120.17"), "100.91.120.17", 22)
                .pins
                .len(),
            1
        );
        assert_eq!(
            host_keys(&line("Trj"), "trj", 22).pins.len(),
            1,
            "names compare case-insensitively"
        );
        assert_eq!(host_keys(&line("[box]:2222"), "box", 2222).pins.len(), 1);
        assert_eq!(
            host_keys(&line("box"), "box", 2222).pins.len(),
            0,
            "a bare name is port 22 only"
        );
        assert_eq!(host_keys(&line("*.lan"), "nas.lan", 22).pins.len(), 1);
        assert_eq!(
            host_keys(&line("*.lan,!nas.lan"), "nas.lan", 22).pins.len(),
            0
        );
        assert_eq!(host_keys(&line("# comment"), "comment", 22).pins.len(), 0);

        // `ssh-keygen -H` form: |1|base64(salt)|base64(HMAC-SHA1(salt, name)).
        let salt = [7u8; 20];
        let mut mac = <Hmac<Sha1> as KeyInit>::new_from_slice(&salt).unwrap();
        mac.update(b"10.10.10.1");
        let hashed = format!(
            "|1|{}|{}",
            STANDARD.encode(salt),
            STANDARD.encode(mac.finalize().into_bytes())
        );
        assert_eq!(host_keys(&line(&hashed), "10.10.10.1", 22).pins.len(), 1);
        assert_eq!(host_keys(&line(&hashed), "10.10.10.2", 22).pins.len(), 0);

        let revoked = format!("{}@revoked * ssh-ed25519 {}\n", line("trj"), ED25519);
        let keys = host_keys(&revoked, "trj", 22);
        assert!(
            keys.pins.is_empty() && keys.revoked.len() == 1,
            "a revoked key is never pinned"
        );
        let ca = format!("@cert-authority *.corp ssh-ed25519 {}\n", ED25519);
        assert_eq!(host_keys(&ca, "a.corp", 22).certificate_authorities, 1);
    }

    fn sources(config: Option<&str>) -> SshSources {
        let mut public_keys = BTreeMap::new();
        public_keys.insert(
            PathBuf::from("/home/me/.ssh/trj_ed25519.pub"),
            format!("ssh-ed25519 {} trj", ED25519),
        );
        let mut known_hosts = BTreeMap::new();
        known_hosts.insert(
            PathBuf::from("/home/me/.ssh/known_hosts"),
            format!(
                "100.91.120.17 ssh-ed25519 {}\ncss.example ssh-ed25519 {}\n",
                ED25519, ED25519
            ),
        );
        SshSources {
            config: config.map(str::to_string),
            known_hosts,
            default_known_hosts: vec![PathBuf::from("/home/me/.ssh/known_hosts")],
            public_keys,
        }
    }

    const INVENTORY: &str = r##"
version = 1
default_group = "fleet"
[workspace]
name = "Fleet"
[groups.fleet]
name = "Fleet"
machines = ["trj", "css", "dark"]
[machines.trj]
name = "Threadripper"
[machines.trj.theme.colors]
background = "#0a0e14"
[[machines.trj.routes]]
id = "tailscale"
host = "100.91.120.17"
user = "ubuntu"
identity_file = "~/.ssh/trj_ed25519"
[[machines.trj.routes]]
id = "mux"
kind = "frankenterm_mux"
via = "tailscale"
socket = "/run/user/1000/frankenterm/sock"
[machines.css]
[[machines.css.routes]]
id = "ssh"
ssh = "css"
[machines.dark]
[[machines.dark.routes]]
id = "mux"
kind = "frankenterm_mux"
via = "ssh"
socket = "/s"
[[machines.dark.routes]]
id = "ssh"
ssh = "jumped"
"##;

    #[test]
    fn an_inventory_exports_routes_trust_identities_and_resolved_themes() {
        let inventory = MachineInventory::parse(INVENTORY, &context()).unwrap();
        let config = "Host css\n  HostName css.example\n  User ubuntu\n  IdentityFile ~/.ssh/trj_ed25519\nHost jumped\n  HostName inner\n  User u\n  ProxyJump bastion\n";
        let export = export(
            &inventory,
            INVENTORY.as_bytes(),
            None,
            &sources(Some(config)),
            &context(),
        )
        .unwrap();
        let doc = &export.document;
        assert_eq!(doc["format"], "ftworkspace");
        assert_eq!(doc["source"]["exporter_id"], EXPORTER_ID);
        let workspace = &doc["workspace"];
        assert_eq!(workspace["id"], "w.machines");
        let machines: Vec<&str> = workspace["machines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            machines,
            ["m.trj", "m.css"],
            "dark has only a ProxyJump alias and a mux route"
        );
        assert_eq!(export.machines, 2);
        assert_eq!(
            workspace["groups"][0]["machine_order"],
            json!(["m.trj", "m.css"])
        );
        assert_eq!(workspace["preferences"]["initial_machine_ref"], "m.trj");

        let routes = workspace["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0]["id"], "r.trj.tailscale");
        assert_eq!(routes[0]["host"], "100.91.120.17");
        assert_eq!(
            routes[1]["host"], "css.example",
            "the alias resolves through ~/.ssh/config"
        );
        assert_eq!(routes[1]["user"], "ubuntu");
        // Both routes use the same key: one credential record.
        assert_eq!(workspace["credentials"].as_array().unwrap().len(), 1);
        assert_eq!(routes[0]["credential_refs"], routes[1]["credential_refs"]);
        let trust = workspace["host_trust"].as_array().unwrap();
        assert_eq!(trust.len(), 2);
        assert!(trust
            .iter()
            .all(|t| t["pins"].as_array().unwrap().len() == 1));

        let theme = &workspace["themes"][0];
        assert_eq!(theme["background"], "#0A0E14");
        assert_eq!(theme["ansi"].as_array().unwrap().len(), 16);

        let messages: Vec<String> = export.notes.iter().map(ToString::to_string).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("trj: mux routes stay on the desktop")),
            "{:?}",
            messages
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("dark (ssh): uses proxyjump")),
            "{:?}",
            messages
        );
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("dark: no exportable SSH route")),
            "{:?}",
            messages
        );
    }

    #[test]
    fn missing_keys_and_host_keys_are_reported_and_a_group_scopes_the_workspace() {
        let inventory = MachineInventory::parse(INVENTORY, &context()).unwrap();
        let mut bare = sources(None);
        bare.public_keys.clear();
        bare.known_hosts.clear();
        let export = export(&inventory, b"x", Some("fleet"), &bare, &context()).unwrap();
        assert_eq!(export.document["workspace"]["id"], "w.machines.fleet");
        let messages: Vec<String> = export.notes.iter().map(ToString::to_string).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("no readable public key at /home/me/.ssh/trj_ed25519.pub")),
            "{:?}",
            messages
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("no host key for 100.91.120.17")),
            "{:?}",
            messages
        );
        assert!(
            messages
                .iter()
                .any(|m| m.contains("css (ssh): no ~/.ssh/config")),
            "{:?}",
            messages
        );
        assert!(super::export(&inventory, b"x", Some("nope"), &bare, &context()).is_err());
    }

    #[test]
    fn the_theme_catalog_lists_distinct_themes_then_every_builtin() {
        let catalog = theme_catalog_json();
        let themes = catalog["themes"].as_array().unwrap();
        assert_eq!(
            catalog["distinct"],
            json!(crate::machines::distinct_theme_count())
        );
        assert!(themes.len() > 1000);
        assert_eq!(themes[0]["name"], "FrankenTerm Azure");
        assert!(themes
            .iter()
            .all(|t| t["ansi"].as_array().unwrap().len() == 16));
    }

    /// The app and Kit read these files; they must be what this code writes.
    /// FT_WRITE_MOBILE_FIXTURES=1 rewrites them for review.
    fn matches_file(relative: &str, expected: &[u8]) {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(relative);
        if std::env::var_os("FT_WRITE_MOBILE_FIXTURES").is_some() {
            std::fs::write(&path, expected).unwrap();
        }
        let actual =
            std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {}", path.display(), err));
        assert!(
            actual == expected,
            "{} is stale; run this test with FT_WRITE_MOBILE_FIXTURES=1 and review the diff",
            relative
        );
    }

    #[test]
    fn the_bundled_phone_theme_catalog_is_current() {
        let mut bytes = serde_json::to_vec(&theme_catalog_json()).unwrap();
        bytes.push(b'\n');
        matches_file("apps/ios/FrankenTerm/ThemeCatalog.json", &bytes);
    }

    #[test]
    fn the_kit_fixture_is_this_exporters_output() {
        let inventory = MachineInventory::parse(INVENTORY, &context()).unwrap();
        let config =
            "Host css\n  HostName css.example\n  User ubuntu\n  IdentityFile ~/.ssh/trj_ed25519\n";
        let mut document = export(
            &inventory,
            INVENTORY.as_bytes(),
            None,
            &sources(Some(config)),
            &context(),
        )
        .unwrap()
        .document;
        // The only per-run field.
        document["bundle_id"] = json!("b.fixture");
        let mut bytes = serde_json::to_vec_pretty(&document).unwrap();
        bytes.push(b'\n');
        matches_file("fixtures/ios-airgap/machines-export.ftworkspace", &bytes);
    }
}
