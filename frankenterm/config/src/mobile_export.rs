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
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::convert::TryInto;
use std::io::{ErrorKind, Read};
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};

const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOURCE_FILES: usize = 64;
const MAX_SOURCE_LINES: usize = 16_384;
/// The phone's SSH driver runs remote commands up to this size.
const MAX_REMOTE_COMMAND_BYTES: usize = 1024;
const UNRESOLVED_POLICY: &str = "ssh_config.unresolved_policy";

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
    /// For each credential, the private key file its public key came from:
    /// what the desktop's own ssh would offer. Read by nothing here.
    pub identity_files: BTreeMap<String, PathBuf>,
}

/// OpenSSH's identities when a route names none, in its order.
fn default_identities(context: &InventoryContext) -> Vec<PathBuf> {
    ["id_dsa", "id_ecdsa", "id_ed25519", "id_rsa"]
        .iter()
        .map(|name| context.home.join(".ssh").join(name))
        .collect()
}

/// Where SSH settings come from. Tests supply text; the CLI reads the files.
pub struct SshSources {
    /// `~/.ssh/config`, used only for `ssh = "alias"` routes.
    pub config: Option<String>,
    /// System configuration follows user configuration. Unknown system policy
    /// must not disappear merely because an alias was defined by the user.
    pub system_config: KnownHostsSource,
    /// Every effective trust source, including absence and failed reads. An
    /// omitted entry is unresolved, never evidence that a file did not exist.
    pub known_hosts: BTreeMap<PathBuf, KnownHostsSource>,
    /// The known_hosts files of a direct route (OpenSSH's defaults).
    pub default_known_hosts: Vec<PathBuf>,
    /// `.pub` contents by path; a route's identity `x` reads `x.pub`.
    pub public_keys: BTreeMap<PathBuf, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KnownHostsSource {
    Present(String),
    Missing,
    Unavailable,
}

/// Read one immutable, regular local source within its allocation budget.
/// In particular, a permission error or an oversized revocation file must not
/// be converted into the same state as ENOENT.
fn read_source(path: &Path, limit: usize) -> KnownHostsSource {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => return KnownHostsSource::Missing,
        Err(_) => return KnownHostsSource::Unavailable,
    };
    let before = match file.metadata() {
        Ok(value) if value.is_file() && value.len() <= limit as u64 => value,
        _ => return KnownHostsSource::Unavailable,
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (before.uid() != 0 && before.uid() != nix::unistd::Uid::current().as_raw())
            || before.mode() & 0o022 != 0
        {
            return KnownHostsSource::Unavailable;
        }
    }
    let mut bytes = Vec::new();
    if (&file)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() > limit
        || bytes.len() as u64 != before.len()
    {
        return KnownHostsSource::Unavailable;
    }
    let after = match file.metadata() {
        Ok(value) => value,
        Err(_) => return KnownHostsSource::Unavailable,
    };
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return KnownHostsSource::Unavailable;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.ctime() != after.ctime() || before.ctime_nsec() != after.ctime_nsec() {
            return KnownHostsSource::Unavailable;
        }
    }
    match String::from_utf8(bytes) {
        Ok(text) if text.lines().count() <= MAX_SOURCE_LINES => KnownHostsSource::Present(text),
        _ => KnownHostsSource::Unavailable,
    }
}

fn default_known_hosts(home: &Path) -> Vec<PathBuf> {
    vec![
        home.join(".ssh/known_hosts"),
        home.join(".ssh/known_hosts2"),
        PathBuf::from("/etc/ssh/ssh_known_hosts"),
        PathBuf::from("/etc/ssh/ssh_known_hosts2"),
    ]
}

impl SshSources {
    /// Reads the user's real files: SSH config, the default known_hosts files
    /// and every `.pub` an inventory route can name. Missing files are simply
    /// absent; the export reports what that leaves unusable.
    pub fn from_disk(inventory: &MachineInventory, context: &InventoryContext) -> Self {
        let ssh = context.home.join(".ssh");
        let config = match read_source(&ssh.join("config"), MAX_SOURCE_BYTES) {
            KnownHostsSource::Present(text) => Some(text),
            _ => None,
        };
        let system_config = read_source(
            Path::new("/etc/ssh/ssh_config"),
            MAX_SOURCE_BYTES.saturating_sub(config.as_ref().map_or(0, String::len)),
        );
        let resolver = alias_resolver_with_system(config.as_deref(), &system_config, &context.home);
        let default_known_hosts = default_known_hosts(&context.home);
        let mut known_files: BTreeSet<PathBuf> = default_known_hosts.iter().cloned().collect();
        let mut identities = BTreeSet::new();
        for machine in &inventory.machines {
            for route in &machine.routes {
                match route {
                    MachineRoute::Ssh {
                        target: RouteTarget::Direct { identity_files, .. },
                        ..
                    } => {
                        if identity_files.is_empty() {
                            identities.extend(default_identities(context));
                        }
                        identities.extend(identity_files.iter().map(|f| context.expand(f)));
                    }
                    MachineRoute::Ssh {
                        target: RouteTarget::Alias(alias),
                        ..
                    } => {
                        if let Some(resolver) = resolver.as_ref() {
                            let options = resolver.config.for_host(alias);
                            identities.extend(split_paths(options.get("identityfile"), context));
                            known_files.extend(resolver.trust_files(&options).0);
                        }
                    }
                    MachineRoute::FrankenTermMux { .. } => {}
                }
            }
        }
        let mut remaining = MAX_SOURCE_BYTES;
        let mut remaining_lines = MAX_SOURCE_LINES;
        let known_hosts = known_files
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                let mut source = if index < MAX_SOURCE_FILES {
                    read_source(&path, remaining)
                } else {
                    KnownHostsSource::Unavailable
                };
                if let KnownHostsSource::Present(text) = &source {
                    remaining -= text.len();
                    let lines = text.lines().count();
                    if lines > remaining_lines {
                        source = KnownHostsSource::Unavailable;
                    } else {
                        remaining_lines -= lines;
                    }
                }
                (path, source)
            })
            .collect();
        Self {
            config,
            system_config,
            known_hosts,
            default_known_hosts,
            public_keys: identities
                .into_iter()
                .filter_map(|path| {
                    let public = public_key_path(&path);
                    match read_source(&public, 64 * 1024) {
                        KnownHostsSource::Present(text) => Some((public, text)),
                        _ => None,
                    }
                })
                .collect(),
        }
    }
}

struct AliasResolver {
    config: frankenterm_ssh::Config,
    trust_lists: BTreeMap<String, Option<Vec<PathBuf>>>,
    incomplete: bool,
}

impl AliasResolver {
    fn trust_files(&self, options: &frankenterm_ssh::ConfigMap) -> (Vec<PathBuf>, bool) {
        let mut files = Vec::new();
        let mut incomplete = self.incomplete;
        for keyword in ["userknownhostsfile", "globalknownhostsfile"] {
            match options
                .get(keyword)
                .and_then(|token| self.trust_lists.get(token))
            {
                Some(Some(paths)) => {
                    for path in paths {
                        if !files.contains(path) {
                            files.push(path.clone());
                        }
                    }
                }
                _ => incomplete = true,
            }
        }
        (files, incomplete)
    }
}

fn directive(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let split = line
        .find(|c: char| c == '=' || c.is_whitespace())
        .unwrap_or(line.len());
    let (keyword, value) = line.split_at(split);
    Some((
        keyword,
        value.trim_start().strip_prefix('=').unwrap_or(value).trim(),
    ))
}

/// Bounded OpenSSH word syntax for passive export. Unlike the general resolver,
/// this discards inline comments before Host matching and keeps quoted spaces.
/// Escapes and single quotes stay unsupported instead of changing their meaning.
fn configuration_words(value: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for character in value.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            '#' if !quoted && !started => break,
            '\\' | '\'' | '\0' => return None,
            value if value.is_ascii_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            value => {
                word.push(value);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        words.push(word);
    }
    Some(words)
}

fn known_host_paths(value: &str, home: &Path) -> Option<Vec<PathBuf>> {
    // This bounded profile supports literal absolute paths and ~/ paths,
    // including double-quoted spaces. It never expands environment, percent
    // tokens, glob patterns, named-user homes, or relative inventory paths.
    let words = configuration_words(value)?;
    if words.len() == 1 && words[0].eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    if words.is_empty() || words.len() > 16 {
        return None;
    }
    words
        .into_iter()
        .map(|word| {
            if word.len() > 4096
                || word.contains(['%', '$', '*', '?', '[', ']'])
                || word.eq_ignore_ascii_case("none")
            {
                return None;
            }
            let path = match word.strip_prefix("~/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(word),
            };
            (path.is_absolute() && !path.components().any(|c| c == Component::ParentDir))
                .then_some(path)
        })
        .collect()
}

fn alias_resolver_with_system(
    config: Option<&str>,
    system: &KnownHostsSource,
    home: &Path,
) -> Option<AliasResolver> {
    let mut combined = config?.to_string();
    // A separate file starts in unconditional context; it does not inherit the
    // final user Host stanza. Values already assigned by the user still win.
    combined.push_str("\nHost *\n");
    if let KnownHostsSource::Present(text) = system {
        combined.push_str(text);
    }
    let mut resolver = alias_resolver(Some(&combined), home)?;
    if matches!(system, KnownHostsSource::Unavailable) {
        resolver.incomplete = true;
    }
    Some(resolver)
}

fn alias_resolver(config: Option<&str>, home: &Path) -> Option<AliasResolver> {
    let text = config?;
    if text.len() > MAX_SOURCE_BYTES || text.lines().count() > MAX_SOURCE_LINES {
        return None;
    }
    let mut trust_lists = BTreeMap::new();
    let mut passive = String::new();
    let mut incomplete = false;
    for line in text.lines() {
        if let Some((keyword, value)) = directive(line) {
            match keyword.to_ascii_lowercase().as_str() {
                "include" => {
                    // The general resolver's Include reader is unbounded and
                    // cannot prove the selected file context. Preserve the gap
                    // as a mandatory policy, without opening another file.
                    incomplete = true;
                    continue;
                }
                "match" => {
                    incomplete = true;
                    passive.push_str("Host __frankenterm_unresolved_match__\n");
                    continue;
                }
                "userknownhostsfile" | "globalknownhostsfile" => {
                    let token = format!("__frankenterm_known_hosts_{}__", trust_lists.len());
                    trust_lists.insert(token.clone(), known_host_paths(value, home));
                    passive.push_str(&format!("{} {}\n", keyword, token));
                    continue;
                }
                _ => {}
            }
            // The general resolver does not remove inline comments or accept
            // whitespace before an optional '=' consistently. Normalize those
            // before it can turn a comment's '*' into a matching Host pattern.
            match configuration_words(value) {
                Some(mut words)
                    if !words.is_empty()
                        && words.iter().all(|word| {
                            !word.is_empty() && !word.chars().any(char::is_whitespace)
                        }) =>
                {
                    if keyword.eq_ignore_ascii_case("host") {
                        // The general matcher returns at its first positive
                        // match. OpenSSH requires ANY matching negation to win.
                        words.sort_by_key(|word| !word.starts_with('!'));
                    }
                    if keyword.eq_ignore_ascii_case("certificatefile")
                        && words.iter().any(|word| !word.eq_ignore_ascii_case("none"))
                    {
                        // CertificateFile is additive, unlike the resolver's
                        // scalar map. A preceding `none` must not hide a later
                        // certificate candidate or imply ordinary key parity.
                        passive.push_str("frankenterm_required_certificate yes\n");
                    }
                    passive.push_str(&format!("{} {}\n", keyword, words.join(" ")));
                }
                _ => {
                    incomplete = true;
                    if keyword.eq_ignore_ascii_case("host") {
                        passive.push_str("Host __frankenterm_unresolved_match__\n");
                    }
                }
            }
        }
    }
    // Opaque lists keep the general resolver's first-assignment semantics
    // without allowing its quote stripping or token expansion to change paths.
    passive.push_str("Host *\n");
    let defaults = default_known_hosts(home);
    for (keyword, paths) in [
        ("userknownhostsfile", defaults[..2].to_vec()),
        ("globalknownhostsfile", defaults[2..].to_vec()),
    ] {
        let token = format!("__frankenterm_known_hosts_{}__", trust_lists.len());
        trust_lists.insert(token.clone(), Some(paths));
        passive.push_str(&format!("{} {}\n", keyword, token));
    }
    let mut resolver = frankenterm_ssh::Config::new();
    // A passive export never runs a local command to evaluate `Match exec`,
    // and expands `~`/`%d` against this home and no other environment value.
    resolver.set_match_exec_policy(frankenterm_ssh::MatchExecPolicy::Deny);
    let mut environment = frankenterm_ssh::ConfigMap::new();
    environment.insert("HOME".to_string(), home.to_string_lossy().into_owned());
    resolver.assign_environment(environment);
    resolver.add_config_string(&passive);
    Some(AliasResolver {
        config: resolver,
        trust_lists,
        incomplete,
    })
}

/// The concrete `Host` aliases in the user's SSH config, without patterns.
pub fn ssh_config_hosts(home: &Path) -> Vec<String> {
    let text = match read_source(&home.join(".ssh/config"), MAX_SOURCE_BYTES) {
        KnownHostsSource::Present(text) => text,
        _ => return Vec::new(),
    };
    alias_resolver(Some(&text), home)
        .map(|resolver| {
            resolver
                .config
                .enumerate_hosts()
                .into_iter()
                .filter(|host| host != "__frankenterm_unresolved_match__")
                .collect()
        })
        .unwrap_or_default()
}

fn split_paths(value: Option<&String>, context: &InventoryContext) -> Vec<PathBuf> {
    value
        .map(|text| {
            text.split_whitespace()
                .filter(|path| !path.eq_ignore_ascii_case("none"))
                .map(|path| context.expand(path))
                .collect()
        })
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
        if line.len() > 64 * 1024 {
            return None;
        }
        let mut words = line.split_whitespace();
        let algorithm = words.next()?.to_string();
        let blob = STANDARD.decode(words.next()?).ok()?;
        // The blob starts with its own algorithm name; a mismatch is not a key.
        let name_length = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
        if name_length > 128
            || blob.get(4..4usize.checked_add(name_length)?)? != algorithm.as_bytes()
        {
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
    /// A malformed or unsupported record may contain a required denial.
    pub unresolved_entries: usize,
}

/// OpenSSH matching: port 22 uses the bare name, other ports `[name]:port`;
/// patterns may be hashed (`|1|salt|hmac`), use `*`/`?`, or negate with `!`.
pub fn host_keys(known_hosts: &str, host: &str, port: u16) -> HostKeys {
    let name = if port == 22 {
        host.to_ascii_lowercase()
    } else {
        format!("[{}]:{}", host.to_ascii_lowercase(), port)
    };
    host_keys_for_lookup(known_hosts, &name, &name)
}

fn host_keys_for_lookup(known_hosts: &str, name: &str, canonical_name: &str) -> HostKeys {
    let mut keys = HostKeys::default();
    if known_hosts.len() > MAX_SOURCE_BYTES || known_hosts.lines().count() > MAX_SOURCE_LINES {
        keys.unresolved_entries = 1;
        return keys;
    }
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
                None => {
                    keys.unresolved_entries += 1;
                    continue;
                }
            };
            Some(marker)
        } else {
            None
        };
        // A malformed hash cannot establish that a revocation applies to some
        // other endpoint. Keep it as an unresolved mandatory source policy.
        if first.contains('|') && !valid_hashed_host(first) {
            keys.unresolved_entries += 1;
            continue;
        }
        if !patterns_match(first, name)
            && !(marker.is_some() && patterns_match(first, canonical_name))
        {
            continue;
        }
        let rest: Vec<&str> = words.collect();
        let key = match PublicKey::parse_line(&rest.join(" ")) {
            Some(key) => key,
            None => {
                keys.unresolved_entries += 1;
                continue;
            }
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
            Some("@cert-authority") => keys.certificate_authorities += 1,
            Some(_) => keys.unresolved_entries += 1,
        }
    }
    // A revoked key is never also a pin.
    let revoked = keys.revoked.clone();
    keys.pins.retain(|key| !revoked.contains(key));
    keys
}

fn valid_hashed_host(field: &str) -> bool {
    let parts: Vec<&str> = field.split('|').collect();
    parts.len() == 4
        && parts[0].is_empty()
        && parts[1] == "1"
        && parts[2..].iter().all(|part| {
            STANDARD
                .decode(part)
                .ok()
                .is_some_and(|bytes| bytes.len() == 20 && STANDARD.encode(bytes) == *part)
        })
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

fn canonical_host(host: &str) -> String {
    let value = host
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(host);
    match value.parse::<IpAddr>() {
        Ok(IpAddr::V6(address)) => address
            .to_ipv4_mapped()
            .map(|address| address.to_string())
            .unwrap_or_else(|| address.to_string()),
        Ok(address) => address.to_string(),
        Err(_) => value
            .strip_suffix('.')
            .unwrap_or(value)
            .to_ascii_lowercase(),
    }
}

fn endpoint_trust_scope(endpoint: &Endpoint) -> (String, u16) {
    (
        canonical_host(endpoint.host_key_alias.as_deref().unwrap_or(&endpoint.host)),
        endpoint.port,
    )
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
    unresolved_policy: bool,
}

fn resolve_endpoint(
    machine: &Machine,
    route: &str,
    target: &RouteTarget,
    sources: &SshSources,
    resolver: Option<&AliasResolver>,
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
            identities: if identity_files.is_empty() {
                default_identities(context)
            } else {
                identity_files.iter().map(|f| context.expand(f)).collect()
            },
            host_key_alias: None,
            known_hosts: sources.default_known_hosts.clone(),
            unresolved_policy: false,
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
            let options = resolver.config.for_host(alias);
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
                Some(user) if !user.chars().any(char::is_whitespace) => user.clone(),
                Some(_) => {
                    note(
                        notes,
                        "its User in ~/.ssh/config contains spaces; not exported".into(),
                    );
                    return None;
                }
                None => {
                    note(
                        notes,
                        "no User for this alias in ~/.ssh/config; not exported".into(),
                    );
                    return None;
                }
            };
            let host = options
                .get("hostname")
                .cloned()
                .unwrap_or_else(|| alias.clone());
            let alias_ok = options
                .get("hostkeyalias")
                .is_none_or(|alias| crate::machines::valid_host(alias));
            if !crate::machines::valid_host(&host) || !alias_ok {
                note(
                    notes,
                    format!(
                        "its HostName or HostKeyAlias in ~/.ssh/config is not usable on the phone ({}); not exported",
                        crate::machines::INVALID_HOST
                    ),
                );
                return None;
            }
            let (known_hosts, mut unresolved_policy) = resolver.trust_files(&options);
            if unresolved_policy {
                note(notes, "the SSH source has unavailable system configuration, unresolved Include/Match conditions, or unsupported passive syntax or trust-file lists; this route remains blocked on the phone".into());
            }
            // Do not silently substitute the phone's defaults for mandatory
            // source trust, revocation, or algorithm constraints. These are
            // preserved by a required-capability marker in the document itself.
            for keyword in [
                "revokedhostkeys",
                "knownhostscommand",
                "hostkeyalgorithms",
                "casignaturealgorithms",
                "required_rsasize",
                "requiredrsasize",
                "pubkeyacceptedalgorithms",
                "pubkeyacceptedkeytypes",
                "kexalgorithms",
                "ciphers",
                "macs",
                "verifyhostkeydns",
                "checkhostip",
                "canonicalizehostname",
                "pubkeyauthentication",
                "preferredauthentications",
                "certificatefile",
                "frankenterm_required_certificate",
                "refuseconnection",
            ] {
                if let Some(value) = options.get(keyword) {
                    let disabled = match keyword {
                        "revokedhostkeys" | "knownhostscommand" | "certificatefile" => {
                            value.eq_ignore_ascii_case("none")
                        }
                        "verifyhostkeydns"
                        | "checkhostip"
                        | "canonicalizehostname"
                        | "refuseconnection" => value.eq_ignore_ascii_case("no"),
                        "pubkeyauthentication" => value.eq_ignore_ascii_case("yes"),
                        "preferredauthentications" => value == "publickey",
                        _ => false,
                    };
                    if !disabled {
                        unresolved_policy = true;
                        note(
                            notes,
                            format!(
                                "{} requires policy the phone cannot enforce; this route remains blocked",
                                keyword
                            ),
                        );
                    }
                }
            }
            Some(Endpoint {
                host,
                port,
                user,
                identities: split_paths(options.get("identityfile"), context),
                host_key_alias: options.get("hostkeyalias").cloned(),
                known_hosts,
                unresolved_policy,
            })
        }
    }
}

/// Resolve one route's positive trust separately from the inventory-wide
/// denial union. A group filter may discard a route, never its same-scope
/// revocations or unresolved mandatory trust policy.
fn endpoint_keys(
    machine: &Machine,
    route: &str,
    endpoint: &Endpoint,
    sources: &SshSources,
    notes: &mut Vec<ExportNote>,
) -> (HostKeys, bool) {
    let note = |notes: &mut Vec<ExportNote>, message: String| {
        notes.push(ExportNote {
            machine: machine.id.clone(),
            route: Some(route.to_string()),
            message,
        });
    };
    // OpenSSH uses HostKeyAlias verbatim, even for a non-default port.
    // Only ordinary host lookups receive the [host]:port wrapper.
    let lookup_name = |host: &str| {
        if endpoint.host_key_alias.is_some() || endpoint.port == 22 {
            host.to_ascii_lowercase()
        } else {
            format!("[{}]:{}", host.to_ascii_lowercase(), endpoint.port)
        }
    };
    let lookup = lookup_name(endpoint.host_key_alias.as_deref().unwrap_or(&endpoint.host));
    let canonical_lookup = lookup_name(&endpoint_trust_scope(endpoint).0);
    let mut keys = HostKeys::default();
    let mut unresolved_policy = endpoint.unresolved_policy;
    for file in &endpoint.known_hosts {
        match sources.known_hosts.get(file) {
            Some(KnownHostsSource::Present(text)) => {
                let found = host_keys_for_lookup(text, &lookup, &canonical_lookup);
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
                keys.unresolved_entries += found.unresolved_entries;
            }
            Some(KnownHostsSource::Missing) => {}
            Some(KnownHostsSource::Unavailable) | None => {
                unresolved_policy = true;
                note(
                    notes,
                    format!(
                        "known-hosts source {} could not be read safely; its trust and revocations are unresolved and this route remains blocked",
                        file.display()
                    ),
                );
            }
        }
    }
    if keys.certificate_authorities > 0 || keys.unresolved_entries > 0 {
        unresolved_policy = true;
        note(notes, "known_hosts contains a matching certificate authority, unsupported marker, or unresolved record; this route remains blocked".into());
    }
    let revoked = keys.revoked.clone();
    keys.pins.retain(|key| !revoked.contains(key));
    if keys.pins.is_empty() {
        note(
            notes,
            format!(
                "no host key for {} in known_hosts{}; review the source trust policy before connecting",
                lookup,
                if keys.certificate_authorities > 0 {
                    " (only a certificate authority, which the phone cannot use)"
                } else {
                    ""
                }
            ),
        );
    }
    (keys, unresolved_policy)
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
        Some(id) => vec![
            inventory
                .groups
                .iter()
                .find(|g| g.id == id)
                .ok_or_else(|| anyhow::anyhow!("no group named {:?} in the inventory", id))?,
        ],
        None => inventory.groups.iter().collect(),
    };
    let wanted: BTreeSet<&str> = groups
        .iter()
        .flat_map(|g| g.machines.iter().map(String::as_str))
        .collect();
    let resolver = alias_resolver_with_system(
        sources.config.as_deref(),
        &sources.system_config,
        &context.home,
    );
    let mut notes = Vec::new();
    let (mut machines, mut routes, mut credentials, mut trusts, mut themes) = (
        Vec::new(),
        Vec::new(),
        BTreeMap::new(),
        BTreeMap::new(),
        Vec::new(),
    );
    let mut exported = BTreeSet::new();
    let mut identity_files = BTreeMap::new();
    let mut trust_scopes = BTreeMap::new();
    let mut revoked_by_scope: BTreeMap<(String, u16), Vec<PublicKey>> = BTreeMap::new();
    let mut unresolved_scopes = BTreeSet::new();
    let mut resolved_routes = BTreeMap::new();

    // Collect denials before applying the selected group. Positive pins remain
    // route-specific, but omitting another machine is not permission to forget
    // a revocation or required policy for the same host-key identity.
    for machine in &inventory.machines {
        for route in &machine.routes {
            let MachineRoute::Ssh { id, target } = route else {
                continue;
            };
            let mut route_notes = Vec::new();
            if let Some(endpoint) = resolve_endpoint(
                machine,
                id,
                target,
                sources,
                resolver.as_ref(),
                context,
                &mut route_notes,
            ) {
                let (keys, unresolved) =
                    endpoint_keys(machine, id, &endpoint, sources, &mut route_notes);
                let scope = endpoint_trust_scope(&endpoint);
                let revoked = revoked_by_scope.entry(scope.clone()).or_default();
                for key in &keys.revoked {
                    if !revoked.contains(key) {
                        revoked.push(key.clone());
                    }
                }
                if unresolved {
                    unresolved_scopes.insert(scope);
                }
                if wanted.contains(machine.id.as_str()) {
                    resolved_routes.insert((machine.id.clone(), id.clone()), (endpoint, keys));
                }
            }
            if wanted.contains(machine.id.as_str()) {
                notes.extend(route_notes);
            }
        }
    }
    for revoked in revoked_by_scope.values_mut() {
        revoked.sort_by_key(PublicKey::fingerprint);
    }

    for machine in inventory
        .machines
        .iter()
        .filter(|m| wanted.contains(m.id.as_str()))
    {
        let mut route_ids = Vec::new();
        let mut exported_ssh = BTreeMap::new();
        for route in &machine.routes {
            let MachineRoute::Ssh { id: route_name, .. } = route else {
                continue;
            };
            let (endpoint, keys) =
                match resolved_routes.remove(&(machine.id.clone(), route_name.clone())) {
                    Some(resolved) => resolved,
                    None => continue,
                };
            let mut credential_ids = Vec::new();
            let defaults = default_identities(context);
            for identity in &endpoint.identities {
                let public = public_key_path(identity);
                let key = match sources.public_keys.get(&public).and_then(|text| {
                    text.lines()
                        .find(|l| !l.trim().is_empty())
                        .and_then(PublicKey::parse_line)
                }) {
                    Some(key) => key,
                    // OpenSSH skips absent default identities silently too.
                    None if defaults.contains(identity) => continue,
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
                if credential_ids.contains(&id) {
                    continue;
                }
                if credential_ids.len() == 8 {
                    notes.push(ExportNote {
                        machine: machine.id.clone(),
                        route: Some(route_name.clone()),
                        message: format!(
                            "the phone offers at most 8 identities per route; {} is not offered",
                            identity.display()
                        ),
                    });
                    continue;
                }
                credentials.entry(id.clone()).or_insert_with(|| {
                    json!({
                        "id": id,
                        "algorithm": key.algorithm,
                        "public_fingerprint": fingerprint,
                        "requirement": "matching_private_key",
                        "private_material": "not_included",
                    })
                });
                identity_files.entry(id.clone()).or_insert_with(|| {
                    if identity.extension().is_some_and(|ext| ext == "pub") {
                        identity.with_extension("")
                    } else {
                        identity.clone()
                    }
                });
                credential_ids.push(id);
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
            let id = format!("r.{}.{}", machine.id, route_name);
            // Trust belongs to the route's selected file policy. Sharing an
            // endpoint must never borrow another route's positive allowlist.
            let trust_id = format!("h.{}", short_digest(&id));
            trust_scopes.insert(trust_id.clone(), endpoint_trust_scope(&endpoint));
            let pin = |key: &PublicKey| json!({"algorithm": key.algorithm, "fingerprint": key.fingerprint()});
            let mut trust = json!({
                "id": trust_id,
                "host": endpoint.host,
                "port": endpoint.port,
                "claimed_source": "known_hosts",
                "pins": keys.pins.iter().map(pin).collect::<Vec<_>>(),
                "revoked_keys": [],
            });
            if let Some(alias) = &endpoint.host_key_alias {
                trust["host_key_alias"] = json!(alias);
            }
            trusts.insert(trust_id.clone(), trust);
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
            exported_ssh.insert(route_name.clone(), record.clone());
            routes.push(record);
            route_ids.push(id);
        }
        // A mux route becomes one phone route per exported SSH route in its
        // `via` order: the same sign-in, then the command the desktop runs
        // on the machine to reach its mux, so the phone attaches to the same
        // panes instead of starting a shell. They come first: the phone
        // prefers a machine's existing panes.
        let mut mux_ids = Vec::new();
        for route in &machine.routes {
            let MachineRoute::FrankenTermMux {
                id: mux_name, via, ..
            } = route
            else {
                continue;
            };
            let Some(command) = machine.mux_remote_command(mux_name) else {
                continue;
            };
            if command.is_empty()
                || command.len() > MAX_REMOTE_COMMAND_BYTES
                || !command.bytes().all(|byte| (b' '..=b'~').contains(&byte))
            {
                notes.push(ExportNote {
                    machine: machine.id.clone(),
                    route: Some(mux_name.clone()),
                    message: format!(
                        "the phone runs only printable ASCII commands of at most {} bytes; this mux route stays on the desktop",
                        MAX_REMOTE_COMMAND_BYTES
                    ),
                });
                continue;
            }
            let before = mux_ids.len();
            for via_name in via {
                let Some(ssh) = exported_ssh.get(via_name) else {
                    continue;
                };
                let id = if via.len() == 1 {
                    format!("r.{}.{}", machine.id, mux_name)
                } else {
                    format!("r.{}.{}.{}", machine.id, mux_name, via_name)
                };
                let mut record = ssh.clone();
                record["id"] = json!(id);
                record["mode"] = json!("frankenterm_mux");
                record["attach_policy"] = json!("existing_only");
                record["remote_command"] = json!(command);
                if let Some(fields) = record.as_object_mut() {
                    fields.remove("session_creation");
                }
                routes.push(record);
                mux_ids.push(id);
            }
            if mux_ids.len() == before {
                notes.push(ExportNote {
                    machine: machine.id.clone(),
                    route: Some(mux_name.clone()),
                    message: "none of this mux route's SSH routes reached the phone, so it stays on the desktop".into(),
                });
            }
        }
        mux_ids.append(&mut route_ids);
        let route_ids = mux_ids;
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
        // The phone signs every machine in once the workspace opens, so
        // switching to one is instant; its terminal opens only when chosen.
        let mut record = json!({
            "id": format!("m.{}", machine.id),
            "name": machine.name,
            "theme_ref": theme_id,
            "route_refs": route_ids,
            "preferred_route_ref": route_ids[0],
            "auto_connect": true,
            "tags": machine.tags,
        });
        if let Some(label) = &machine.short_label {
            record["short_label"] = json!(label);
        }
        machines.push(record);
        exported.insert(machine.id.clone());
    }

    // Denials and unresolved required trust are monotonic for one canonical
    // host-key scope, across every selected route and source spelling. Apply
    // them after collecting all routes, independently of inventory order.
    for (trust_id, trust) in &mut trusts {
        let scope = &trust_scopes[trust_id];
        if let Some(revoked) = revoked_by_scope.get(scope) {
            let fingerprints: BTreeSet<String> =
                revoked.iter().map(PublicKey::fingerprint).collect();
            trust["pins"].as_array_mut().unwrap().retain(|pin| {
                !pin["fingerprint"]
                    .as_str()
                    .is_some_and(|value| fingerprints.contains(value))
            });
            trust["revoked_keys"] = json!(
                revoked
                    .iter()
                    .map(|key| {
                        json!({"algorithm": key.algorithm, "fingerprint": key.fingerprint()})
                    })
                    .collect::<Vec<_>>()
            );
        }
        if unresolved_scopes.contains(scope) {
            trust["required_capabilities"] = json!([UNRESOLVED_POLICY]);
        }
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
                Some(json!({"id": format!("g.{}", group.id), "name": group.name, "machine_order": order, "auto_connect": true}))
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
        identity_files,
    })
}

/// The companion source for the inventory at `path`, every group, read with
/// this user's SSH files: what `ft mobile source` and the companion's helper print.
pub fn load_companion_source(path: &Path) -> anyhow::Result<Value> {
    let bytes = std::fs::read(path)
        .map_err(|err| anyhow::anyhow!("reading {}: {}", path.display(), err))?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", path.display()))?;
    let context = InventoryContext {
        directory: path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(".")),
        home: crate::HOME_DIR.clone(),
    };
    let inventory = MachineInventory::parse(text, &context)?;
    let sources = SshSources::from_disk(&inventory, &context);
    let export = export(&inventory, &bytes, None, &sources, &context)?;
    Ok(companion_source(&export, path))
}

/// What the Mac companion loads at launch (`ft mobile source`): the workspace
/// for every group, and where this Mac keeps each credential's private key,
/// so the companion can seal the keys the desktop already uses without the
/// user picking files. Paths only: no key material passes through here.
pub fn companion_source(export: &MobileExport, inventory: &Path) -> Value {
    json!({
        "format": "ftmachines-source",
        "version": 1,
        "inventory": inventory.to_string_lossy(),
        "document": export.document,
        "identity_files": export
            .identity_files
            .iter()
            .map(|(id, path)| (id.clone(), json!(path.to_string_lossy())))
            .collect::<Map<String, Value>>(),
        "notes": export.notes.iter().map(ToString::to_string).collect::<Vec<_>>(),
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
        let defaults = default_known_hosts(&context().home);
        let mut known_hosts: BTreeMap<PathBuf, KnownHostsSource> = defaults
            .iter()
            .cloned()
            .map(|path| (path, KnownHostsSource::Missing))
            .collect();
        known_hosts.insert(
            PathBuf::from("/home/me/.ssh/known_hosts"),
            KnownHostsSource::Present(format!(
                "100.91.120.17 ssh-ed25519 {}\ncss.example ssh-ed25519 {}\n",
                ED25519, ED25519
            )),
        );
        SshSources {
            config: config.map(str::to_string),
            system_config: KnownHostsSource::Missing,
            known_hosts,
            default_known_hosts: defaults,
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
        // Opening the workspace on the phone signs every machine in.
        assert_eq!(workspace["groups"][0]["auto_connect"], true);
        for machine in workspace["machines"].as_array().unwrap() {
            assert_eq!(machine["auto_connect"], true, "{}", machine["id"]);
        }
        assert_eq!(workspace["preferences"]["initial_machine_ref"], "m.trj");

        let routes = workspace["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 3);
        assert_eq!(routes[0]["id"], "r.trj.tailscale");
        assert_eq!(routes[0]["host"], "100.91.120.17");
        assert_eq!(routes[0]["mode"], "direct_ssh");
        // The mux route signs in like its `via` route, then runs the same
        // relay command the desktop runs, attaching to the existing panes.
        let mux = &routes[1];
        assert_eq!(mux["id"], "r.trj.mux");
        assert_eq!(mux["mode"], "frankenterm_mux");
        assert_eq!(mux["attach_policy"], "existing_only");
        assert_eq!(
            mux["remote_command"],
            "nc -N -U /run/user/1000/frankenterm/sock"
        );
        assert!(mux.get("session_creation").is_none());
        for field in ["host", "port", "user", "credential_refs", "host_trust_ref"] {
            assert_eq!(mux[field], routes[0][field], "{}", field);
        }
        let trj = &workspace["machines"][0];
        assert_eq!(trj["route_refs"], json!(["r.trj.mux", "r.trj.tailscale"]));
        assert_eq!(trj["preferred_route_ref"], "r.trj.mux");
        assert_eq!(
            routes[2]["host"], "css.example",
            "the alias resolves through ~/.ssh/config"
        );
        assert_eq!(routes[2]["user"], "ubuntu");
        // Both routes use the same key: one credential record.
        assert_eq!(workspace["credentials"].as_array().unwrap().len(), 1);
        assert_eq!(routes[0]["credential_refs"], routes[2]["credential_refs"]);
        let trust = workspace["host_trust"].as_array().unwrap();
        assert_eq!(trust.len(), 2);
        assert!(
            trust
                .iter()
                .all(|t| t["pins"].as_array().unwrap().len() == 1)
        );

        let theme = &workspace["themes"][0];
        assert_eq!(theme["background"], "#0A0E14");
        assert_eq!(theme["ansi"].as_array().unwrap().len(), 16);

        let messages: Vec<String> = export.notes.iter().map(ToString::to_string).collect();
        assert!(
            !messages
                .iter()
                .any(|m| m.starts_with("trj") && m.contains("stays on the desktop")),
            "{:?}",
            messages
        );
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("dark (mux): none of this mux route's SSH routes")),
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

    /// A distinct well-formed Ed25519 public key line for `seed`.
    fn fake_key(seed: u8) -> String {
        let mut blob = vec![0, 0, 0, 11];
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&[0, 0, 0, 32]);
        blob.extend_from_slice(&[seed; 32]);
        format!("ssh-ed25519 {} k{}", STANDARD.encode(blob), seed)
    }

    #[test]
    fn the_companion_source_names_the_private_key_behind_each_credential() {
        let text = r#"
version = 1
[machines.a]
[[machines.a.routes]]
id = "direct"
host = "100.91.120.17"
user = "ubuntu"
identity_file = "~/.ssh/trj_ed25519.pub"
[machines.b]
[[machines.b.routes]]
id = "plain"
host = "css.example"
user = "ubuntu"
[machines.c]
[[machines.c.routes]]
id = "bad"
ssh = "bad"
"#;
        let inventory = MachineInventory::parse(text, &context()).unwrap();
        let mut sources = sources(Some("Host bad\n  HostName me@evil\n  User u\n"));
        // b names no identity, so OpenSSH's defaults apply; only id_ecdsa exists.
        sources
            .public_keys
            .insert(PathBuf::from("/home/me/.ssh/id_ecdsa.pub"), fake_key(7));
        let export = export(&inventory, text.as_bytes(), None, &sources, &context()).unwrap();
        let routes = export.document["workspace"]["routes"].as_array().unwrap();
        assert_eq!(
            routes.len(),
            2,
            "c's alias resolves to a host the phone refuses"
        );
        assert_eq!(routes[1]["credential_refs"].as_array().unwrap().len(), 1);
        let messages: Vec<String> = export.notes.iter().map(ToString::to_string).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("c (bad): its HostName")),
            "{:?}",
            messages
        );
        assert!(
            !messages
                .iter()
                .any(|m| m.contains("id_dsa") || m.contains("id_rsa")),
            "absent default identities are not reported: {:?}",
            messages
        );

        let source = companion_source(
            &export,
            Path::new("/home/me/.config/frankenterm/machines.toml"),
        );
        assert_eq!(source["format"], "ftmachines-source");
        assert_eq!(
            source["inventory"],
            "/home/me/.config/frankenterm/machines.toml"
        );
        assert_eq!(source["document"], export.document);
        let files: BTreeSet<&str> = source["identity_files"]
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_str().unwrap())
            .collect();
        let expected: BTreeSet<&str> = ["/home/me/.ssh/trj_ed25519", "/home/me/.ssh/id_ecdsa"]
            .iter()
            .copied()
            .collect();
        assert_eq!(files, expected, "private paths, never the .pub");
    }

    #[test]
    fn a_route_offers_at_most_eight_identities_and_says_which_it_left_out() {
        let mut text = "version = 1\n[[machines.a.routes]]\nid = \"many\"\nhost = \"100.91.120.17\"\nuser = \"u\"\nidentity_files = [".to_string();
        let mut sources = sources(None);
        for seed in 0..9u8 {
            text.push_str(&format!("\"~/.ssh/k{}\", ", seed));
            sources.public_keys.insert(
                PathBuf::from(format!("/home/me/.ssh/k{}.pub", seed)),
                fake_key(seed),
            );
        }
        text.push_str("]\n");
        let inventory = MachineInventory::parse(&text, &context()).unwrap();
        let export = export(&inventory, text.as_bytes(), None, &sources, &context()).unwrap();
        let route = &export.document["workspace"]["routes"][0];
        assert_eq!(route["credential_refs"].as_array().unwrap().len(), 8);
        let messages: Vec<String> = export.notes.iter().map(ToString::to_string).collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("at most 8 identities") && m.contains("/home/me/.ssh/k8")),
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

    const TRUST_INVENTORY: &str = r#"
version = 1
default_group = "fleet"
[groups.fleet]
machines = ["left", "right"]
[machines.left]
[[machines.left.routes]]
id = "ssh"
ssh = "left"
[machines.right]
[[machines.right.routes]]
id = "ssh"
ssh = "right"
"#;

    fn trust_export(sources: &SshSources) -> MobileExport {
        let inventory = MachineInventory::parse(TRUST_INVENTORY, &context()).unwrap();
        export(
            &inventory,
            TRUST_INVENTORY.as_bytes(),
            None,
            sources,
            &context(),
        )
        .unwrap()
    }

    fn route_trust<'a>(export: &'a MobileExport, route: &str) -> &'a Value {
        let workspace = &export.document["workspace"];
        let route = workspace["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["id"] == route)
            .unwrap();
        workspace["host_trust"]
            .as_array()
            .unwrap()
            .iter()
            .find(|value| value["id"] == route["host_trust_ref"])
            .unwrap()
    }

    fn supply(sources: &mut SshSources, path: &str, text: String) {
        sources
            .known_hosts
            .insert(PathBuf::from(path), KnownHostsSource::Present(text));
    }

    fn write_source(path: &Path, contents: impl AsRef<[u8]>) {
        std::fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    #[test]
    fn route_allowlists_stay_separate_and_later_revocations_reach_every_same_scope_route() {
        let mut sources = sources(Some(
            "Host left\n HostName box\n User u\n UserKnownHostsFile /trust/left\n GlobalKnownHostsFile none\nHost right\n HostName box\n User u\n UserKnownHostsFile /trust/right\n GlobalKnownHostsFile none\n",
        ));
        let mut other_blob = STANDARD.decode(ED25519).unwrap();
        *other_blob.last_mut().unwrap() ^= 1;
        let other = STANDARD.encode(other_blob);
        let other_fingerprint = PublicKey::parse_line(&format!("ssh-ed25519 {}", other))
            .unwrap()
            .fingerprint();
        supply(
            &mut sources,
            "/trust/left",
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        supply(
            &mut sources,
            "/trust/right",
            format!("box ssh-ed25519 {}\n", other),
        );
        let before = trust_export(&sources);
        let left = route_trust(&before, "r.left.ssh");
        let right = route_trust(&before, "r.right.ssh");
        assert_ne!(left["id"], right["id"]);
        assert_eq!(left["pins"][0]["fingerprint"], FINGERPRINT);
        assert_eq!(right["pins"][0]["fingerprint"], other_fingerprint);
        assert_eq!(left["pins"].as_array().unwrap().len(), 1);
        assert_eq!(right["pins"].as_array().unwrap().len(), 1);

        supply(
            &mut sources,
            "/trust/right",
            format!(
                "box ssh-ed25519 {}\n@revoked box ssh-ed25519 {}\n",
                other, ED25519
            ),
        );
        let after = trust_export(&sources);
        for route in ["r.left.ssh", "r.right.ssh"] {
            let trust = route_trust(&after, route);
            assert_eq!(trust["revoked_keys"][0]["fingerprint"], FINGERPRINT);
        }
        assert!(
            route_trust(&after, "r.left.ssh")["pins"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            route_trust(&after, "r.right.ssh")["pins"][0]["fingerprint"],
            other_fingerprint
        );

        let mut inventory = MachineInventory::parse(TRUST_INVENTORY, &context()).unwrap();
        inventory.machines.reverse();
        let reversed = export(
            &inventory,
            TRUST_INVENTORY.as_bytes(),
            None,
            &sources,
            &context(),
        )
        .unwrap();
        assert_eq!(
            after.document["workspace"]["host_trust"],
            reversed.document["workspace"]["host_trust"]
        );
    }

    #[test]
    fn group_selection_keeps_denials_from_unselected_routes_without_borrowing_pins() {
        let inventory_text = format!(
            "{}\n[groups.only_left]\nmachines = [\"left\"]\n",
            TRUST_INVENTORY
        );
        let inventory = MachineInventory::parse(&inventory_text, &context()).unwrap();
        let mut sources = sources(Some(
            "Host left\n HostName box\n User u\n UserKnownHostsFile /trust/left\n GlobalKnownHostsFile none\nHost right\n HostName box\n User u\n UserKnownHostsFile /trust/right\n GlobalKnownHostsFile none\n",
        ));
        supply(&mut sources, "/trust/left", String::new());
        supply(
            &mut sources,
            "/trust/right",
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        let only_left = |sources: &SshSources| {
            export(
                &inventory,
                inventory_text.as_bytes(),
                Some("only_left"),
                sources,
                &context(),
            )
            .unwrap()
        };
        let unconfigured = only_left(&sources);
        assert_eq!(unconfigured.machines, 1);
        assert_eq!(unconfigured.routes, 1);
        assert!(
            route_trust(&unconfigured, "r.left.ssh")["pins"]
                .as_array()
                .unwrap()
                .is_empty(),
            "an unselected route cannot contribute positive pins"
        );

        supply(
            &mut sources,
            "/trust/left",
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        supply(
            &mut sources,
            "/trust/right",
            format!("@revoked box ssh-ed25519 {}\n", ED25519),
        );
        let denied = only_left(&sources);
        let trust = route_trust(&denied, "r.left.ssh");
        assert!(trust["pins"].as_array().unwrap().is_empty());
        assert_eq!(trust["revoked_keys"][0]["fingerprint"], FINGERPRINT);

        sources
            .known_hosts
            .insert(PathBuf::from("/trust/right"), KnownHostsSource::Unavailable);
        let incomplete = only_left(&sources);
        assert_eq!(
            route_trust(&incomplete, "r.left.ssh")["required_capabilities"],
            json!([UNRESOLVED_POLICY])
        );
    }

    #[test]
    fn embedded_hashes_are_literal_paths_and_only_word_boundary_hashes_start_comments() {
        let config = "Host * # unrelated * patterns\n HostName box\n User u\n UserKnownHostsFile /trust/keys#revocations \"/trust/#quoted\" # /trust/ignored\n GlobalKnownHostsFile none\n";
        let resolver = alias_resolver(Some(config), &context().home).unwrap();
        assert_eq!(
            resolver.trust_files(&resolver.config.for_host("left")),
            (
                vec![
                    PathBuf::from("/trust/keys#revocations"),
                    PathBuf::from("/trust/#quoted")
                ],
                false
            )
        );
        let mut sources = sources(Some(config));
        supply(
            &mut sources,
            "/trust/keys",
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        supply(
            &mut sources,
            "/trust/keys#revocations",
            format!("@revoked box ssh-ed25519 {}\n", ED25519),
        );
        supply(
            &mut sources,
            "/trust/#quoted",
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        let export = trust_export(&sources);
        let trust = route_trust(&export, "r.left.ssh");
        assert!(trust["pins"].as_array().unwrap().is_empty());
        assert_eq!(trust["revoked_keys"][0]["fingerprint"], FINGERPRINT);
        assert!(trust["required_capabilities"].is_null());
        assert_eq!(
            split_paths(Some(&"none ~/.ssh/allowed NONE".to_string()), &context()),
            vec![PathBuf::from("/home/me/.ssh/allowed")],
            "IdentityFile none is never a key path"
        );
    }

    #[test]
    fn effective_user_and_global_lists_keep_quotes_first_assignment_and_none() {
        let resolver = alias_resolver(Some("Host left\n UserKnownHostsFile \"~/trust keys\" /trust/second\n GlobalKnownHostsFile = /trust/global\n UserKnownHostsFile /trust/ignored\nHost right\n UserKnownHostsFile NONE\n GlobalKnownHostsFile none\nHost *\n UserKnownHostsFile /trust/fallback\n GlobalKnownHostsFile /trust/ignored-global\n"), &context().home).unwrap();
        let (left, incomplete) = resolver.trust_files(&resolver.config.for_host("left"));
        assert!(!incomplete);
        assert_eq!(
            left,
            [
                PathBuf::from("/home/me/trust keys"),
                PathBuf::from("/trust/second"),
                PathBuf::from("/trust/global")
            ]
        );
        assert_eq!(
            resolver.trust_files(&resolver.config.for_host("right")),
            (Vec::new(), false)
        );
        for list in [
            "none /trust/a",
            "relative",
            "${HOME}/trust",
            "%d/trust",
            "~other/trust",
            "/trust/../other",
            "\"/unterminated",
        ] {
            assert!(
                known_host_paths(list, &context().home).is_none(),
                "{}",
                list
            );
        }

        let mut sources = sources(Some(
            "Host *\n HostName box\n User u\n UserKnownHostsFile /trust/user\n GlobalKnownHostsFile /trust/global\n",
        ));
        supply(
            &mut sources,
            "/trust/user",
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        supply(
            &mut sources,
            "/trust/global",
            format!("@revoked box ssh-ed25519 {}\n", ED25519),
        );
        let export = trust_export(&sources);
        assert!(
            route_trust(&export, "r.left.ssh")["pins"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            route_trust(&export, "r.left.ssh")["revoked_keys"][0]["fingerprint"],
            FINGERPRINT
        );

        sources.config = Some("Host *\n HostName 100.91.120.17\n User u\n UserKnownHostsFile none\n GlobalKnownHostsFile none\n".into());
        let disabled = trust_export(&sources);
        assert!(
            route_trust(&disabled, "r.left.ssh")["pins"]
                .as_array()
                .unwrap()
                .is_empty(),
            "none must not restore default files"
        );
        assert!(route_trust(&disabled, "r.left.ssh")["required_capabilities"].is_null());
    }

    #[test]
    fn unavailable_or_unsupplied_trust_is_blocked_but_observed_absence_is_not() {
        let mut sources = sources(Some(
            "Host left\n HostName box\n User u\n UserKnownHostsFile /trust/unavailable\n GlobalKnownHostsFile /trust/pins\nHost right\n HostName BOX.\n User u\n UserKnownHostsFile none\n GlobalKnownHostsFile /trust/pins\n",
        ));
        supply(
            &mut sources,
            "/trust/pins",
            format!("* ssh-ed25519 {}\n", ED25519),
        );
        let path = PathBuf::from("/trust/unavailable");
        for state in [
            None,
            Some(KnownHostsSource::Unavailable),
            Some(KnownHostsSource::Missing),
        ] {
            let blocked = state != Some(KnownHostsSource::Missing);
            sources.known_hosts.remove(&path);
            if let Some(state) = state {
                sources.known_hosts.insert(path.clone(), state);
            }
            let export = trust_export(&sources);
            for route in ["r.left.ssh", "r.right.ssh"] {
                let trust = route_trust(&export, route);
                assert_eq!(trust["pins"][0]["fingerprint"], FINGERPRINT);
                assert_eq!(
                    trust["required_capabilities"] == json!([UNRESOLVED_POLICY]),
                    blocked
                );
            }
        }
    }

    #[test]
    fn explicit_host_key_alias_is_verbatim_at_nondefault_ports_and_denials_canonicalize() {
        let mut sources = sources(Some(
            "Host left\n HostName one.example\n User u\n Port 2222\n HostKeyAlias shared\n UserKnownHostsFile /trust/left\n GlobalKnownHostsFile none\nHost right\n HostName two.example\n User u\n Port 2222\n HostKeyAlias SHARED.\n UserKnownHostsFile /trust/right\n GlobalKnownHostsFile none\n",
        ));
        supply(
            &mut sources,
            "/trust/left",
            format!("shared ssh-ed25519 {}\n", ED25519),
        );
        supply(&mut sources, "/trust/right", String::new());
        let before = trust_export(&sources);
        assert_eq!(
            route_trust(&before, "r.left.ssh")["pins"][0]["fingerprint"],
            FINGERPRINT
        );
        supply(
            &mut sources,
            "/trust/right",
            format!("@revoked shared ssh-ed25519 {}\n", ED25519),
        );
        let after = trust_export(&sources);
        for route in ["r.left.ssh", "r.right.ssh"] {
            assert_eq!(
                route_trust(&after, route)["revoked_keys"][0]["fingerprint"],
                FINGERPRINT
            );
            assert!(
                route_trust(&after, route)["pins"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
        assert_eq!(
            canonical_host("[2001:0db8:0:0:0:0:0:1]"),
            canonical_host("2001:db8::1")
        );
        assert_eq!(
            canonical_host("::ffff:192.0.2.1"),
            canonical_host("192.0.2.1")
        );
    }

    #[test]
    fn malformed_trust_and_required_source_policies_survive_as_document_blockers() {
        let base = "Host *\n HostName box\n User u\n UserKnownHostsFile /trust/keys\n GlobalKnownHostsFile none\n";
        for policy in [
            "RevokedHostKeys /trust/revoked",
            "KnownHostsCommand /bin/false",
            "HostKeyAlgorithms ssh-ed25519",
            "CheckHostIP yes",
            "VerifyHostKeyDNS yes",
            "PubkeyAuthentication no",
            "PreferredAuthentications password",
            "CertificateFile /trust/user-cert.pub",
            "CertificateFile none\n CertificateFile /trust/user-cert.pub",
            "RefuseConnection yes",
            "Match exec \"touch /must-not-execute\"",
            "Include /must-not-read",
        ] {
            let mut sources = sources(Some(&format!("{}{}\n", base, policy)));
            supply(
                &mut sources,
                "/trust/keys",
                format!("box ssh-ed25519 {}\n", ED25519),
            );
            let export = trust_export(&sources);
            assert_eq!(
                route_trust(&export, "r.left.ssh")["required_capabilities"],
                json!([UNRESOLVED_POLICY]),
                "{}",
                policy
            );
        }
        for invalid in [
            format!("@cert-authority box ssh-ed25519 {}", ED25519),
            format!("@unknown box ssh-ed25519 {}", ED25519),
            "@revoked box ssh-ed25519 !!!".into(),
            format!("@revoked |2|bad|hash ssh-ed25519 {}", ED25519),
        ] {
            let mut sources = sources(Some(base));
            supply(
                &mut sources,
                "/trust/keys",
                format!("box ssh-ed25519 {}\n{}\n", ED25519, invalid),
            );
            assert_eq!(
                route_trust(&trust_export(&sources), "r.left.ssh")["required_capabilities"],
                json!([UNRESOLVED_POLICY]),
                "{}",
                invalid
            );
        }
    }

    #[test]
    fn bounded_disk_reads_distinguish_missing_from_unsafe_or_oversized_sources() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("known_hosts");
        assert_eq!(read_source(&file, 8), KnownHostsSource::Missing);
        assert_eq!(
            read_source(directory.path(), 8),
            KnownHostsSource::Unavailable
        );
        write_source(&file, b"contents");
        assert_eq!(
            read_source(&file, 8),
            KnownHostsSource::Present("contents".into())
        );
        assert_eq!(read_source(&file, 7), KnownHostsSource::Unavailable);
        write_source(&file, [0xff]);
        assert_eq!(read_source(&file, 8), KnownHostsSource::Unavailable);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let link = directory.path().join("linked_hosts");
            symlink(&file, &link).unwrap();
            assert_eq!(read_source(&link, 8), KnownHostsSource::Unavailable);
            write_source(&file, b"contents");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
            assert_eq!(read_source(&file, 8), KnownHostsSource::Unavailable);
        }
    }

    #[test]
    fn disk_snapshot_loads_effective_quoted_user_and_global_revocation_sources() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        let global = directory.path().join("global trust");
        write_source(
            &home.join("trust pins"),
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        write_source(&global, format!("@revoked box ssh-ed25519 {}\n", ED25519));
        write_source(
            &home.join(".ssh/config"),
            format!(
                "Host *\n HostName box\n User u\n UserKnownHostsFile \"~/trust pins\"\n GlobalKnownHostsFile \"{}\"\n",
                global.display()
            ),
        );
        let context = InventoryContext {
            directory: directory.path().to_path_buf(),
            home,
        };
        let inventory = MachineInventory::parse(TRUST_INVENTORY, &context).unwrap();
        let snapshot = SshSources::from_disk(&inventory, &context);
        assert!(matches!(
            snapshot.known_hosts.get(&global),
            Some(KnownHostsSource::Present(_))
        ));
        let export = export(
            &inventory,
            TRUST_INVENTORY.as_bytes(),
            None,
            &snapshot,
            &context,
        )
        .unwrap();
        for route in ["r.left.ssh", "r.right.ssh"] {
            let trust = route_trust(&export, route);
            assert!(trust["pins"].as_array().unwrap().is_empty());
            assert_eq!(trust["revoked_keys"][0]["fingerprint"], FINGERPRINT);
        }
    }

    #[test]
    fn inline_comments_and_equals_do_not_expand_a_host_match_or_change_its_endpoint() {
        let text = "Host left # *\n HostName = left.example # not right\n UserKnownHostsFile /trust/left\nHost right\n HostName=right.example\n UserKnownHostsFile = /trust/right # /trust/left\nHost *\n User u\n GlobalKnownHostsFile none\n";
        let resolver = alias_resolver(Some(text), &context().home).unwrap();
        let right = resolver.config.for_host("right");
        assert_eq!(
            right.get("hostname").map(String::as_str),
            Some("right.example")
        );
        assert_eq!(
            resolver.trust_files(&right),
            (vec![PathBuf::from("/trust/right")], false)
        );
        let mut sources = sources(Some(text));
        supply(
            &mut sources,
            "/trust/left",
            format!("* ssh-ed25519 {}\n", ED25519),
        );
        supply(&mut sources, "/trust/right", String::new());
        let export = trust_export(&sources);
        let right = route_trust(&export, "r.right.ssh");
        assert_eq!(right["host"], "right.example");
        assert!(right["pins"].as_array().unwrap().is_empty());
        assert!(right["required_capabilities"].is_null());

        let negated = alias_resolver(Some("Host * !right\n HostName left.example\n UserKnownHostsFile /trust/left\nHost right\n HostName right.example\n UserKnownHostsFile /trust/right\nHost *\n GlobalKnownHostsFile none\n"), &context().home).unwrap();
        let right = negated.config.for_host("right");
        assert_eq!(
            right.get("hostname").map(String::as_str),
            Some("right.example")
        );
        assert_eq!(
            negated.trust_files(&right),
            (vec![PathBuf::from("/trust/right")], false)
        );
    }

    #[test]
    fn system_policy_follows_user_assignment_and_cannot_disappear_when_unreadable() {
        let mut sources = sources(Some(
            "Host left\n HostName box\n UserKnownHostsFile /trust/pins\n GlobalKnownHostsFile none\nHost right\n HostName box\n UserKnownHostsFile /trust/pins\n",
        ));
        supply(
            &mut sources,
            "/trust/pins",
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        supply(
            &mut sources,
            "/trust/system",
            format!("@revoked box ssh-ed25519 {}\n", ED25519),
        );
        sources.system_config =
            KnownHostsSource::Present("User u\n GlobalKnownHostsFile /trust/system\n".into());
        let resolver = alias_resolver_with_system(
            sources.config.as_deref(),
            &sources.system_config,
            &context().home,
        )
        .unwrap();
        assert_eq!(
            resolver.trust_files(&resolver.config.for_host("left")),
            (vec![PathBuf::from("/trust/pins")], false)
        );
        assert_eq!(
            resolver.trust_files(&resolver.config.for_host("right")),
            (
                vec![PathBuf::from("/trust/pins"), PathBuf::from("/trust/system")],
                false
            )
        );
        let export = trust_export(&sources);
        for route in ["r.left.ssh", "r.right.ssh"] {
            assert_eq!(
                route_trust(&export, route)["revoked_keys"][0]["fingerprint"],
                FINGERPRINT
            );
        }
        for system in [
            KnownHostsSource::Unavailable,
            KnownHostsSource::Present("Host *\n RevokedHostKeys /trust/revoked\n".into()),
            KnownHostsSource::Present("Include /etc/ssh/ssh_config.d/*\n".into()),
        ] {
            sources.system_config = system;
            let export = trust_export(&sources);
            assert_eq!(
                route_trust(&export, "r.left.ssh")["required_capabilities"],
                json!([UNRESOLVED_POLICY])
            );
        }
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
        assert!(
            themes
                .iter()
                .all(|t| t["ansi"].as_array().unwrap().len() == 16)
        );
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
