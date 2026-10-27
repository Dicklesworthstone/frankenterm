//! `ft mobile export`: the machine inventory as the phone's public workspace
//! document (`ftworkspace` version 1). The phone imports it as is, or the Mac
//! companion seals it together with the private keys the user selects.
//!
//! Exported: machines, groups, fully resolved themes, SSH routes with up to
//! three configured jump hosts, public identities (`<identity>.pub`, never
//! the private file), and each endpoint's host keys from `known_hosts`.
//! Mux routes use the same SSH graph to attach to existing panes. Unsupported
//! routes remain on the desktop with an explanation; unresolved trust policy
//! remains an explicit blocker in the exported document.

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
use std::io::{ErrorKind, Read};
use std::net::IpAddr;
use std::path::{Component, Path, PathBuf};

const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOURCE_FILES: usize = 64;
const MAX_SOURCE_LINES: usize = 16_384;
const MAX_PUBLIC_KEY_FILES: usize = 256;
const MAX_JUMP_HOPS: usize = 3;
const MAX_EXPORTED_ROUTES: usize = 256;
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
                            // The first listed hop can itself contribute a
                            // prefix. Freeze every dependency's public identity
                            // and exact trust sources before constructing any
                            // route; an invalid graph contributes no prefix.
                            let mut aliases = resolver.jump_chain(alias).unwrap_or_default();
                            aliases.push(alias.clone());
                            for name in aliases {
                                let options = resolver.config.for_host(&name);
                                if resolver
                                    .authentication_policy(&options)
                                    .methods
                                    .contains(&"publickey")
                                {
                                    identities
                                        .extend(split_paths(options.get("identityfile"), context));
                                }
                                known_files.extend(resolver.trust_files(&options).0);
                            }
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
        let mut remaining_public_bytes = MAX_SOURCE_BYTES;
        let public_keys = identities
            .into_iter()
            .map(|identity| public_key_path(&identity))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .take(MAX_PUBLIC_KEY_FILES)
            .filter_map(|public| {
                match read_source(&public, remaining_public_bytes.min(64 * 1024)) {
                    KnownHostsSource::Present(text) => {
                        remaining_public_bytes -= text.len();
                        Some((public, text))
                    }
                    _ => None,
                }
            })
            .collect();
        Self {
            config,
            system_config,
            known_hosts,
            default_known_hosts,
            public_keys,
        }
    }
}

struct AliasResolver {
    config: frankenterm_ssh::Config,
    trust_lists: BTreeMap<String, Option<Vec<PathBuf>>>,
    proxies: BTreeMap<String, ProxySetting>,
    authentication_values: BTreeMap<String, Option<String>>,
    aliases: BTreeSet<String>,
    incomplete: bool,
}

enum ProxySetting {
    Jump(String),
    Command { disabled: bool },
}

struct AuthenticationPolicy {
    /// Empty means no usable policy, not permission to substitute a default.
    methods: Vec<&'static str>,
    problems: Vec<String>,
}

impl AliasResolver {
    fn authentication_value<'a>(
        &'a self,
        options: &frankenterm_ssh::ConfigMap,
        keyword: &str,
    ) -> Result<Option<&'a str>, ()> {
        match options.get(keyword) {
            None => Ok(None),
            Some(token) => self
                .authentication_values
                .get(token)
                .and_then(Option::as_deref)
                .map(Some)
                .ok_or(()),
        }
    }

    /// Preserve OpenSSH's enabled method order, independently of which local
    /// keys happen to exist. Unsupported required semantics block the entire
    /// route; they cannot silently turn into a password or key fallback.
    fn authentication_policy(&self, options: &frankenterm_ssh::ConfigMap) -> AuthenticationPolicy {
        let mut enabled = BTreeSet::new();
        let mut problems = Vec::new();
        for (method, keyword) in [
            ("publickey", "pubkeyauthentication"),
            ("password", "passwordauthentication"),
            ("keyboard-interactive", "kbdinteractiveauthentication"),
        ] {
            match self.authentication_value(options, keyword) {
                Ok(None) => {
                    enabled.insert(method);
                }
                Ok(Some(value)) if value.eq_ignore_ascii_case("yes") => {
                    enabled.insert(method);
                }
                Ok(Some(value)) if value.eq_ignore_ascii_case("no") => {}
                _ => problems.push(format!(
                    "{} is not a supported yes/no authentication policy; this route remains blocked",
                    keyword
                )),
            }
        }
        // None of these desktop requirements is implemented by a phone-side
        // password prompt. Explicitly disabling one is faithfully portable.
        for (keyword, disabled) in [
            ("batchmode", "no"),
            ("gssapiauthentication", "no"),
            ("hostbasedauthentication", "no"),
            ("pkcs11provider", "none"),
            ("securitykeyprovider", "none"),
        ] {
            match self.authentication_value(options, keyword) {
                Ok(None) => {}
                Ok(Some(value)) if value.eq_ignore_ascii_case(disabled) => {}
                _ => problems.push(format!(
                    "{} requires authentication behavior the phone cannot enforce; this route remains blocked",
                    keyword
                )),
            }
        }
        // Unlike provider sentinels and yes/no values, OpenSSH compares the
        // IdentityAgent sentinel exactly; "NONE" is still an agent path.
        match self.authentication_value(options, "identityagent") {
            Ok(None) | Ok(Some("none")) => {}
            _ => problems.push("identityagent requires authentication behavior the phone cannot enforce; this route remains blocked".into()),
        }
        if !self
            .authentication_value(options, "identitiesonly")
            .is_ok_and(|value| {
                value.is_none_or(|value| {
                    value.eq_ignore_ascii_case("yes") || value.eq_ignore_ascii_case("no")
                })
            })
        {
            problems.push("identitiesonly is not a supported yes/no identity policy; this route remains blocked".into());
        }
        for keyword in [
            "numberofpasswordprompts",
            "kbdinteractivedevices",
            "authenticationmethods",
        ] {
            if options.contains_key(keyword) {
                problems.push(format!(
                    "{} requires authentication policy the phone cannot enforce; this route remains blocked",
                    keyword
                ));
            }
        }
        let preferred = match self.authentication_value(options, "preferredauthentications") {
            Ok(None) => "publickey,keyboard-interactive,password",
            Ok(Some(value)) if value.len() <= 1024 && value.split(',').count() <= 16 => value,
            _ => {
                problems.push("preferredauthentications is not one bounded ordered method list; this route remains blocked".into());
                ""
            }
        };
        let mut methods = Vec::new();
        let mut unsupported = false;
        for method in preferred.split(',') {
            let known = match method {
                "publickey" => Some("publickey"),
                "password" => Some("password"),
                "keyboard-interactive" => Some("keyboard-interactive"),
                "gssapi-with-mic"
                    if self
                        .authentication_value(options, "gssapiauthentication")
                        .is_ok_and(|value| {
                            value.is_none_or(|value| value.eq_ignore_ascii_case("no"))
                        }) =>
                {
                    None
                }
                "hostbased"
                    if self
                        .authentication_value(options, "hostbasedauthentication")
                        .is_ok_and(|value| {
                            value.is_none_or(|value| value.eq_ignore_ascii_case("no"))
                        }) =>
                {
                    None
                }
                _ => {
                    unsupported = true;
                    None
                }
            };
            if let Some(method) = known {
                if enabled.contains(method) && !methods.contains(&method) {
                    methods.push(method);
                }
            }
        }
        if unsupported {
            problems.push("preferredauthentications contains an unsupported or empty method; this route remains blocked".into());
        }
        if methods.is_empty() {
            problems.push("no permitted authentication method remains; this route stays inactive until its policy is deliberately replaced".into());
        }
        AuthenticationPolicy { methods, problems }
    }

    /// Match OpenSSH's explicit ordered prefix: only the first named jump
    /// inherits its configured proxy chain. The preceding -J list overrides
    /// any proxy configuration on later jumps. All hops must be concrete
    /// configured aliases, with no inline account/port or token expansion.
    fn jump_chain(&self, alias: &str) -> Result<Vec<String>, &'static str> {
        fn expand(
            resolver: &AliasResolver,
            alias: &str,
            path: &BTreeSet<String>,
            remaining: usize,
        ) -> Result<Vec<String>, &'static str> {
            if path.contains(alias) {
                return Err("the chain returns to an earlier alias");
            }
            let options = resolver.config.for_host(alias);
            let Some(token) = options.get("frankenterm_proxy") else {
                return Ok(Vec::new());
            };
            let value = match resolver.proxies.get(token) {
                Some(ProxySetting::Jump(value)) => value,
                Some(ProxySetting::Command { .. }) => return Ok(Vec::new()),
                None => return Err("its exact proxy setting could not be preserved"),
            };
            if value.eq_ignore_ascii_case("none") {
                return Ok(Vec::new());
            }
            let hops: Vec<String> = value.split(',').map(str::to_string).collect();
            if hops.iter().any(|name| {
                name.is_empty()
                    || name.len() > 255
                    || name.starts_with('-')
                    || name.eq_ignore_ascii_case("none")
                    || !name.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_')
                    })
            }) {
                return Err("only comma-separated configured aliases are supported");
            }
            if hops.iter().any(|name| !resolver.aliases.contains(name)) {
                return Err("a hop has no concrete Host declaration");
            }
            let unique: BTreeSet<_> = hops.iter().collect();
            if unique.len() != hops.len()
                || hops.iter().any(|name| name == alias || path.contains(name))
            {
                return Err("the chain repeats or returns to an earlier alias");
            }
            if hops.len() > remaining {
                return Err("the complete chain exceeds three jump hosts");
            }
            let mut path = path.clone();
            path.insert(alias.to_string());
            let mut chain = expand(resolver, &hops[0], &path, remaining - hops.len())?;
            chain.extend(hops);
            if chain.iter().collect::<BTreeSet<_>>().len() != chain.len() {
                return Err("the chain repeats a host alias");
            }
            Ok(chain)
        }
        expand(self, alias, &BTreeSet::new(), MAX_JUMP_HOPS)
    }

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
    let mut proxies = BTreeMap::new();
    let mut authentication_values = BTreeMap::new();
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
                "frankenterm_proxy" => {
                    // This private parser slot cannot be selected by source
                    // text to bypass a real ProxyCommand/ProxyJump directive.
                    incomplete = true;
                    continue;
                }
                "userknownhostsfile" | "globalknownhostsfile" => {
                    let token = format!("__frankenterm_known_hosts_{}__", trust_lists.len());
                    trust_lists.insert(token.clone(), known_host_paths(value, home));
                    passive.push_str(&format!("{} {}\n", keyword, token));
                    continue;
                }
                "proxyjump" | "proxycommand" => {
                    // Preserve the reviewed proxy spelling behind an opaque
                    // token rather than giving executable values to the
                    // general resolver or accepting a partial argument list.
                    // Both directives share one first-assignment slot: the
                    // first matching ProxyCommand/ProxyJump wins in OpenSSH.
                    let token = format!("__frankenterm_proxy_{}__", proxies.len());
                    let words = configuration_words(value);
                    let setting = if keyword.eq_ignore_ascii_case("proxyjump") {
                        ProxySetting::Jump(
                            words
                                .filter(|words| words.len() == 1)
                                .map(|words| words[0].clone())
                                .unwrap_or_default(),
                        )
                    } else {
                        ProxySetting::Command {
                            disabled: words.is_some_and(|words| {
                                words.len() == 1 && words[0].eq_ignore_ascii_case("none")
                            }),
                        }
                    };
                    proxies.insert(token.clone(), setting);
                    passive.push_str(&format!("frankenterm_proxy {}\n", token));
                    continue;
                }
                "preferredauthentications"
                | "pubkeyauthentication"
                | "passwordauthentication"
                | "kbdinteractiveauthentication"
                | "challengeresponseauthentication"
                | "batchmode"
                | "numberofpasswordprompts"
                | "kbdinteractivedevices"
                | "gssapiauthentication"
                | "hostbasedauthentication"
                | "identityagent"
                | "pkcs11provider"
                | "securitykeyprovider"
                | "identitiesonly"
                | "authenticationmethods" => {
                    // Keep malformed first assignments too: dropping one must
                    // not let a later wildcard silently enable another method.
                    let token = format!(
                        "__frankenterm_authentication_{}__",
                        authentication_values.len()
                    );
                    let value = configuration_words(value)
                        .filter(|words| words.len() == 1 && !words[0].is_empty())
                        .map(|words| words[0].clone());
                    authentication_values.insert(token.clone(), value);
                    // The deprecated spelling controls the same OpenSSH slot;
                    // whichever matching spelling appears first takes effect.
                    let keyword = if keyword.eq_ignore_ascii_case("challengeresponseauthentication")
                    {
                        "kbdinteractiveauthentication"
                    } else {
                        keyword
                    };
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
    let aliases = resolver
        .enumerate_hosts()
        .into_iter()
        .filter(|host| host != "__frankenterm_unresolved_match__")
        .collect();
    Some(AliasResolver {
        config: resolver,
        trust_lists,
        proxies,
        authentication_values,
        aliases,
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
    authentication_methods: Vec<&'static str>,
    identities: Vec<PathBuf>,
    host_key_alias: Option<String>,
    known_hosts: Vec<PathBuf>,
    unresolved_policy: bool,
}

struct EndpointSources<'a> {
    ssh: &'a SshSources,
    resolver: Option<&'a AliasResolver>,
    inventory: &'a InventoryContext,
}

struct ResolvedRoute {
    id: String,
    endpoint: Endpoint,
    keys: HostKeys,
    jump_routes: Vec<String>,
    destination: bool,
}

/// Resolve the entire graph before publishing any transit record. Available
/// endpoints of a refused graph still contribute their known denials, but a
/// malformed or unavailable hop cannot produce a direct or partial fallback.
fn resolve_route(
    machine: &Machine,
    route: &str,
    target: &RouteTarget,
    sources: &EndpointSources<'_>,
    notes: &mut Vec<ExportNote>,
) -> (Vec<ResolvedRoute>, bool) {
    let mut exportable = true;
    let chain = match (target, sources.resolver) {
        (RouteTarget::Alias(alias), Some(resolver)) => match resolver.jump_chain(alias) {
            Ok(chain) => chain,
            Err(reason) => {
                notes.push(ExportNote {
                    machine: machine.id.clone(),
                    route: Some(route.to_string()),
                    message: format!(
                        "uses proxyjump in ~/.ssh/config but {}; the complete route was not exported",
                        reason
                    ),
                });
                exportable = false;
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    let mut records = Vec::new();
    let mut jumps = Vec::new();
    for (index, alias) in chain.iter().enumerate() {
        let hop_name = format!("{} via {}", route, alias);
        let Some(endpoint) = resolve_endpoint(
            machine,
            &hop_name,
            &RouteTarget::Alias(alias.clone()),
            sources,
            index > 0,
            notes,
        ) else {
            exportable = false;
            continue;
        };
        // Keep the trust/file policy local to this originating route. The same
        // host in another chain must not borrow its pins or proxy context.
        let id = format!(
            "j.{}",
            short_digest(&format!("{}\0{}\0{}", machine.id, route, alias))
        );
        jumps.push(id.clone());
        records.push(ResolvedRoute {
            id,
            endpoint,
            keys: HostKeys::default(),
            jump_routes: Vec::new(),
            destination: false,
        });
    }
    if let Some(endpoint) = resolve_endpoint(machine, route, target, sources, false, notes) {
        records.push(ResolvedRoute {
            id: format!("r.{}.{}", machine.id, route),
            endpoint,
            keys: HostKeys::default(),
            jump_routes: jumps,
            destination: true,
        });
    } else {
        exportable = false;
    }
    (records, exportable)
}

fn resolve_endpoint(
    machine: &Machine,
    route: &str,
    target: &RouteTarget,
    resolution: &EndpointSources<'_>,
    proxy_overridden: bool,
    notes: &mut Vec<ExportNote>,
) -> Option<Endpoint> {
    let sources = resolution.ssh;
    let resolver = resolution.resolver;
    let context = resolution.inventory;
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
            // Literal inventory routes have an explicit file-key contract,
            // independent of SSH alias defaults and interactive methods.
            authentication_methods: vec!["publickey"],
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
            if !proxy_overridden
                && options.get("frankenterm_proxy").is_some_and(|token| {
                    matches!(
                        resolver.proxies.get(token),
                        None | Some(ProxySetting::Command { disabled: false })
                    )
                })
            {
                note(
                    notes,
                    "uses proxycommand in ~/.ssh/config, which the phone cannot run; not exported"
                        .into(),
                );
                return None;
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
            let authentication = resolver.authentication_policy(&options);
            if !authentication.problems.is_empty() {
                unresolved_policy = true;
                for problem in &authentication.problems {
                    note(notes, problem.clone());
                }
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
                identities: if authentication.methods.contains(&"publickey") {
                    split_paths(options.get("identityfile"), context)
                } else {
                    Vec::new()
                },
                authentication_methods: authentication.methods,
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

#[derive(Default)]
struct EndpointRecords {
    credentials: BTreeMap<String, Value>,
    trusts: BTreeMap<String, Value>,
    trust_scopes: BTreeMap<String, (String, u16)>,
    identity_files: BTreeMap<String, PathBuf>,
}

impl EndpointRecords {
    fn record(
        &mut self,
        machine: &Machine,
        route_name: &str,
        resolved: &ResolvedRoute,
        sources: &SshSources,
        context: &InventoryContext,
        notes: &mut Vec<ExportNote>,
    ) -> Value {
        let endpoint = &resolved.endpoint;
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
                        route: Some(route_name.to_string()),
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
                    route: Some(route_name.to_string()),
                    message: format!(
                        "the phone offers at most 8 identities per route; {} is not offered",
                        identity.display()
                    ),
                });
                continue;
            }
            self.credentials.entry(id.clone()).or_insert_with(|| {
                json!({
                    "id": id,
                    "algorithm": key.algorithm,
                    "public_fingerprint": fingerprint,
                    "requirement": "matching_private_key",
                    "private_material": "not_included",
                })
            });
            self.identity_files.entry(id.clone()).or_insert_with(|| {
                if identity.extension().is_some_and(|ext| ext == "pub") {
                    identity.with_extension("")
                } else {
                    identity.clone()
                }
            });
            credential_ids.push(id);
        }
        if credential_ids.is_empty() && endpoint.authentication_methods.contains(&"publickey") {
            notes.push(ExportNote {
                machine: machine.id.clone(),
                route: Some(route_name.to_string()),
                message: if endpoint.authentication_methods.len() == 1 {
                    "no public key is available, so the phone cannot sign in on this route"
                } else {
                    "no public key is available; the other configured authentication methods remain in the export"
                }
                .into(),
            });
        }
        // Trust belongs to the route's selected file policy. Sharing an
        // endpoint must never borrow another route's positive allowlist.
        let trust_id = format!("h.{}", short_digest(&resolved.id));
        self.trust_scopes
            .insert(trust_id.clone(), endpoint_trust_scope(endpoint));
        let pin =
            |key: &PublicKey| json!({"algorithm": key.algorithm, "fingerprint": key.fingerprint()});
        let mut trust = json!({
            "id": trust_id,
            "host": endpoint.host,
            "port": endpoint.port,
            "claimed_source": "known_hosts",
            "pins": resolved.keys.pins.iter().map(pin).collect::<Vec<_>>(),
            "revoked_keys": [],
        });
        if let Some(alias) = &endpoint.host_key_alias {
            trust["host_key_alias"] = json!(alias);
        }
        self.trusts.insert(trust_id.clone(), trust);
        let mut record = json!({
            "id": resolved.id,
            "mode": "direct_ssh",
            "host": endpoint.host,
            "port": endpoint.port,
            "user": endpoint.user,
            "credential_refs": credential_ids,
            // The schema requires a nonempty list. An empty effective policy
            // gets an inert placeholder with no credentials; its mandatory
            // unresolved-policy marker prevents activation on the phone.
            "authentication_methods": if endpoint.authentication_methods.is_empty() {
                vec!["publickey"]
            } else {
                endpoint.authentication_methods.clone()
            },
            "host_trust_ref": trust_id,
            "jump_routes": resolved.jump_routes,
        });
        if resolved.destination {
            record["session_creation"] = json!("on_select");
        }
        if let Some(alias) = &endpoint.host_key_alias {
            record["host_key_alias"] = json!(alias);
        }
        record
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
    let resolver = alias_resolver_with_system(
        sources.config.as_deref(),
        &sources.system_config,
        &context.home,
    );
    let endpoint_sources = EndpointSources {
        ssh: sources,
        resolver: resolver.as_ref(),
        inventory: context,
    };
    let mut notes = Vec::new();
    let (mut machines, mut routes, mut themes) = (Vec::new(), Vec::new(), Vec::new());
    let mut records = EndpointRecords::default();
    let mut exported = BTreeSet::new();
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
            let (mut family, exportable) =
                resolve_route(machine, id, target, &endpoint_sources, &mut route_notes);
            for resolved in &mut family {
                let name = if resolved.destination {
                    id.clone()
                } else {
                    format!("{} via {}", id, resolved.endpoint.host)
                };
                let (keys, unresolved) = endpoint_keys(
                    machine,
                    &name,
                    &resolved.endpoint,
                    sources,
                    &mut route_notes,
                );
                let scope = endpoint_trust_scope(&resolved.endpoint);
                let revoked = revoked_by_scope.entry(scope.clone()).or_default();
                for key in &keys.revoked {
                    if !revoked.contains(key) {
                        revoked.push(key.clone());
                    }
                }
                if unresolved {
                    unresolved_scopes.insert(scope);
                }
                resolved.keys = keys;
            }
            if exportable && wanted.contains(machine.id.as_str()) {
                resolved_routes.insert((machine.id.clone(), id.clone()), family);
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
            let family = match resolved_routes.remove(&(machine.id.clone(), route_name.clone())) {
                Some(resolved) => resolved,
                None => continue,
            };
            anyhow::ensure!(
                routes.len() + family.len() <= MAX_EXPORTED_ROUTES,
                "the complete workspace exceeds the phone's {}-route limit; select a smaller group",
                MAX_EXPORTED_ROUTES
            );
            for resolved in family {
                let name = if resolved.destination {
                    route_name.clone()
                } else {
                    format!("{} via {}", route_name, resolved.endpoint.host)
                };
                let record =
                    records.record(machine, &name, &resolved, sources, context, &mut notes);
                if resolved.destination {
                    exported_ssh.insert(route_name.clone(), record.clone());
                    route_ids.push(resolved.id);
                }
                routes.push(record);
            }
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
                anyhow::ensure!(
                    routes.len() < MAX_EXPORTED_ROUTES,
                    "the complete workspace exceeds the phone's {}-route limit; select a smaller group",
                    MAX_EXPORTED_ROUTES
                );
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
    for (trust_id, trust) in &mut records.trusts {
        let scope = &records.trust_scopes[trust_id];
        if let Some(revoked) = revoked_by_scope.get(scope) {
            let fingerprints: BTreeSet<String> =
                revoked.iter().map(PublicKey::fingerprint).collect();
            trust["pins"].as_array_mut().unwrap().retain(|pin| {
                !pin["fingerprint"]
                    .as_str()
                    .is_some_and(|value| fingerprints.contains(value))
            });
            trust["revoked_keys"] = json!(revoked
                .iter()
                .map(|key| {
                    json!({"algorithm": key.algorithm, "fingerprint": key.fingerprint()})
                })
                .collect::<Vec<_>>());
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
    anyhow::ensure!(
        records.credentials.len() <= 256,
        "the complete workspace exceeds the phone's 256-credential limit; select a smaller group"
    );
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
            "credentials": records.credentials.into_values().collect::<Vec<_>>(),
            "host_trust": records.trusts.into_values().collect::<Vec<_>>(),
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
        identity_files: records.identity_files,
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
        assert!(trust
            .iter()
            .all(|t| t["pins"].as_array().unwrap().len() == 1));

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

    const JUMP_INVENTORY: &str = r#"
version = 1
[machines.app]
[[machines.app.routes]]
id = "ssh"
ssh = "app"
[[machines.app.routes]]
id = "mux"
kind = "frankenterm_mux"
via = "ssh"
socket = "/run/frankenterm.sock"
"#;

    const JUMP_CONFIG: &str = "Host app\n HostName service.internal\n Port 2224\n User deploy\n IdentityFile ~/.ssh/app\n UserKnownHostsFile /trust/app\n ProxyJump first,second,third\nHost first\n HostName outer.example\n Port 2221\n User gateway\n IdentityFile ~/.ssh/first\n HostKeyAlias outer-key\n UserKnownHostsFile /trust/first\nHost second\n HostName middle.internal\n User transit\n IdentityFile ~/.ssh/second\n UserKnownHostsFile /trust/second\nHost third\n HostName inner.internal\n Port 2223\n User jump\n IdentityFile ~/.ssh/third\n UserKnownHostsFile /trust/third\nHost *\n GlobalKnownHostsFile none\n";

    fn jump_sources() -> SshSources {
        let mut sources = sources(Some(JUMP_CONFIG));
        for (seed, alias, lookup) in [
            (1, "first", "outer-key"),
            (2, "second", "middle.internal"),
            (3, "third", "[inner.internal]:2223"),
            (4, "app", "[service.internal]:2224"),
        ] {
            sources.public_keys.insert(
                context().home.join(format!(".ssh/{}.pub", alias)),
                fake_key(seed),
            );
            supply(
                &mut sources,
                &format!("/trust/{}", alias),
                format!("{} {}\n", lookup, fake_key(seed + 40)),
            );
        }
        sources
    }

    fn jump_export(sources: &SshSources) -> anyhow::Result<MobileExport> {
        let inventory = MachineInventory::parse(JUMP_INVENTORY, &context()).unwrap();
        export(
            &inventory,
            JUMP_INVENTORY.as_bytes(),
            None,
            sources,
            &context(),
        )
    }

    fn exported_route<'a>(export: &'a MobileExport, id: &str) -> &'a Value {
        export.document["workspace"]["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|route| route["id"] == id)
            .unwrap()
    }

    fn authentication_sources(policy: &str) -> SshSources {
        sources(Some(&format!(
            "Host app\n{}\n HostName css.example\n User u\n IdentityFile ~/.ssh/trj_ed25519\n GlobalKnownHostsFile none\n",
            policy
        )))
    }

    #[test]
    fn alias_authentication_preserves_order_switches_and_legacy_alias_precedence() {
        for (policy, expected) in [
            ("", vec!["publickey", "keyboard-interactive", "password"]),
            (
                "PreferredAuthentications password,publickey,keyboard-interactive\n PasswordAuthentication no\n ChallengeResponseAuthentication yes\n KbdInteractiveAuthentication no",
                vec!["publickey", "keyboard-interactive"],
            ),
            (
                "PreferredAuthentications password,publickey\n PasswordAuthentication \"yEs\"\n PubkeyAuthentication \"nO\"\n KbdInteractiveAuthentication nO\n IdentityAgent \"none\"\n PKCS11Provider NONE\n SecurityKeyProvider \"NoNe\"\n IdentitiesOnly YES",
                vec!["password"],
            ),
            (
                "PubkeyAuthentication no",
                vec!["keyboard-interactive", "password"],
            ),
            (
                "PreferredAuthentications keyboard-interactive,password,publickey\n KbdInteractiveAuthentication no\n ChallengeResponseAuthentication yes",
                vec!["password", "publickey"],
            ),
            (
                "PreferredAuthentications gssapi-with-mic,hostbased,password,publickey,password\n GSSAPIAuthentication NO\n HostbasedAuthentication no\n BatchMode NO",
                vec!["password", "publickey"],
            ),
            (
                "PreferredAuthentications = \"password,publickey\" # keep the order\n PubkeyAuthentication no\n PasswordAuthentication yes",
                vec!["password"],
            ),
            (
                "PasswordAuthentication no\n KbdInteractiveAuthentication no",
                vec!["publickey"],
            ),
        ] {
            let export = jump_export(&authentication_sources(policy)).unwrap();
            for id in ["r.app.ssh", "r.app.mux"] {
                let route = exported_route(&export, id);
                assert_eq!(
                    route["authentication_methods"],
                    json!(expected),
                    "{}",
                    policy
                );
                assert!(
                    route_trust(&export, id)["required_capabilities"].is_null(),
                    "{}",
                    policy
                );
                assert_eq!(
                    route["credential_refs"].as_array().unwrap().len(),
                    usize::from(expected.contains(&"publickey"))
                );
            }
            assert_eq!(
                export.identity_files.len(),
                usize::from(expected.contains(&"publickey"))
            );
        }
    }

    #[test]
    fn malformed_or_required_authentication_policy_cannot_become_a_usable_fallback() {
        for (policy, credential_count) in [
            (
                "PubkeyAuthentication no\n PasswordAuthentication no\n KbdInteractiveAuthentication no",
                0,
            ),
            ("PreferredAuthentications none", 0),
            ("PreferredAuthentications password,unsupported", 0),
            ("PreferredAuthentications password,,publickey", 1),
            ("PreferredAuthentications Password", 0),
            (
                "PreferredAuthentications password publickey\n PreferredAuthentications password",
                0,
            ),
            ("PreferredAuthentications \"password publickey\"", 0),
            (
                "PubkeyAuthentication no\n PasswordAuthentication \"\"\n PasswordAuthentication yes\n KbdInteractiveAuthentication no",
                0,
            ),
            (
                "PreferredAuthentications password\n PubkeyAuthentication host-bound",
                0,
            ),
            (
                "PreferredAuthentications password\n PubkeyAuthentication unbound",
                0,
            ),
            (
                "PreferredAuthentications password\n PasswordAuthentication maybe",
                0,
            ),
            (
                "PreferredAuthentications password\n PasswordAuthentication \" YES \"",
                0,
            ),
            (
                "PreferredAuthentications password\n PasswordAuthentication yes#disabled",
                0,
            ),
            ("PreferredAuthentications password\n BatchMode yes", 0),
            (
                "PreferredAuthentications password\n NumberOfPasswordPrompts 0",
                0,
            ),
            (
                "PreferredAuthentications password\n KbdInteractiveDevices pam",
                0,
            ),
            (
                "PreferredAuthentications password\n AuthenticationMethods publickey,password",
                0,
            ),
            (
                "PreferredAuthentications password\n GSSAPIAuthentication yes",
                0,
            ),
            (
                "PreferredAuthentications hostbased,password\n HostbasedAuthentication yes",
                0,
            ),
            (
                "PreferredAuthentications password\n IdentityAgent /must-not-open/agent",
                0,
            ),
            ("PreferredAuthentications password\n IdentityAgent NONE", 0),
            (
                "PreferredAuthentications password\n PKCS11Provider /must-not-load/provider",
                0,
            ),
            (
                "PreferredAuthentications password\n SecurityKeyProvider internal",
                0,
            ),
            (
                "PreferredAuthentications password\n IdentitiesOnly maybe",
                0,
            ),
        ] {
            let export = jump_export(&authentication_sources(policy)).unwrap();
            for id in ["r.app.ssh", "r.app.mux"] {
                assert_eq!(
                    route_trust(&export, id)["required_capabilities"],
                    json!([UNRESOLVED_POLICY]),
                    "{}",
                    policy
                );
            }
            assert!(
                export
                    .notes
                    .iter()
                    .any(|note| note.message.contains("blocked")
                        || note.message.contains("inactive")),
                "{}",
                policy
            );
            assert_eq!(
                export.identity_files.len(),
                credential_count,
                "inactive or non-key policy must not request private material: {}",
                policy
            );
        }
        for list in [vec!["password"; 17].join(","), "x".repeat(1025)] {
            let export = jump_export(&authentication_sources(&format!(
                "PreferredAuthentications {}",
                list
            )))
            .unwrap();
            assert_eq!(
                route_trust(&export, "r.app.ssh")["required_capabilities"],
                json!([UNRESOLVED_POLICY])
            );
            assert_eq!(
                exported_route(&export, "r.app.ssh")["authentication_methods"],
                json!(["publickey"])
            );
            assert!(
                export.identity_files.is_empty(),
                "the inert schema placeholder grants no key authority"
            );
        }
    }

    #[test]
    fn password_only_graphs_need_no_key_records_or_companion_identity_files() {
        let mut sources = jump_sources();
        sources.public_keys.clear();
        sources.config = Some(format!(
            "{} PreferredAuthentications password\n PubkeyAuthentication no\n PasswordAuthentication yes\n KbdInteractiveAuthentication no\n",
            JUMP_CONFIG
        ));
        let export = jump_export(&sources).unwrap();
        assert_eq!(export.routes, 5);
        assert_eq!(export.document["workspace"]["credentials"], json!([]));
        assert!(export.identity_files.is_empty());
        assert!(export.notes.is_empty(), "{:?}", export.notes);
        for route in export.document["workspace"]["routes"].as_array().unwrap() {
            assert_eq!(route["authentication_methods"], json!(["password"]));
            assert_eq!(route["credential_refs"], json!([]));
            assert!(
                route_trust(&export, route["id"].as_str().unwrap())["required_capabilities"]
                    .is_null()
            );
        }
        let source = companion_source(&export, Path::new("/config/machines.toml"));
        assert_eq!(source["identity_files"], json!({}));
        assert_eq!(source["document"], export.document);
    }

    #[test]
    fn every_hop_retains_its_own_authentication_policy_and_key_scope() {
        let mut sources = jump_sources();
        sources.config = Some(
            JUMP_CONFIG
                .replace(
                    "Host app\n",
                    "Host app\n PreferredAuthentications password,publickey\n",
                )
                .replace(
                    "Host first\n",
                    "Host first\n PreferredAuthentications password\n PubkeyAuthentication no\n",
                )
                .replace(
                    "Host second\n",
                    "Host second\n PreferredAuthentications keyboard-interactive\n",
                )
                .replace(
                    "Host third\n",
                    "Host third\n PreferredAuthentications publickey\n",
                ),
        );
        let export = jump_export(&sources).unwrap();
        let destination = exported_route(&export, "r.app.ssh");
        assert_eq!(
            destination["authentication_methods"],
            json!(["password", "publickey"])
        );
        for (index, method) in ["password", "keyboard-interactive", "publickey"]
            .iter()
            .enumerate()
        {
            let hop = exported_route(&export, destination["jump_routes"][index].as_str().unwrap());
            assert_eq!(hop["authentication_methods"], json!([method]));
            assert_eq!(
                hop["credential_refs"].as_array().unwrap().len(),
                usize::from(*method == "publickey")
            );
            assert!(
                route_trust(&export, hop["id"].as_str().unwrap())["required_capabilities"]
                    .is_null()
            );
        }
        assert_eq!(export.identity_files.len(), 2);
        let files: BTreeSet<_> = export.identity_files.values().cloned().collect();
        assert_eq!(
            files,
            [
                context().home.join(".ssh/third"),
                context().home.join(".ssh/app")
            ]
            .iter()
            .cloned()
            .collect()
        );
    }

    #[test]
    fn user_and_system_authentication_first_assignments_remain_authoritative() {
        let mut sources = authentication_sources(
            "PreferredAuthentications password,keyboard-interactive,publickey\n PasswordAuthentication no\n PubkeyAuthentication yes",
        );
        sources.system_config = KnownHostsSource::Present(
            "PreferredAuthentications password\n PasswordAuthentication yes\n PubkeyAuthentication no\n KbdInteractiveAuthentication no\n".into(),
        );
        let export = jump_export(&sources).unwrap();
        assert_eq!(
            exported_route(&export, "r.app.ssh")["authentication_methods"],
            json!(["publickey"])
        );
        assert!(route_trust(&export, "r.app.ssh")["required_capabilities"].is_null());
        let mut sources =
            authentication_sources("PasswordAuthentication maybe\n PasswordAuthentication yes");
        sources.system_config = KnownHostsSource::Present("PasswordAuthentication yes\n".into());
        let export = jump_export(&sources).unwrap();
        assert_eq!(
            route_trust(&export, "r.app.ssh")["required_capabilities"],
            json!([UNRESOLVED_POLICY])
        );
        assert!(
            !exported_route(&export, "r.app.ssh")["authentication_methods"]
                .as_array()
                .unwrap()
                .contains(&json!("password"))
        );
    }

    #[test]
    fn missing_optional_keys_do_not_claim_a_password_route_cannot_sign_in() {
        let mut sources = authentication_sources("PreferredAuthentications password,publickey");
        sources.public_keys.clear();
        let export = jump_export(&sources).unwrap();
        assert!(export.identity_files.is_empty());
        assert!(route_trust(&export, "r.app.ssh")["required_capabilities"].is_null());
        assert!(export.notes.iter().any(|note| {
            note.message
                .contains("other configured authentication methods")
        }));
        assert!(!export
            .notes
            .iter()
            .any(|note| note.message.contains("cannot sign in")));
        sources.config = Some(
            sources
                .config
                .unwrap()
                .replace("password,publickey", "publickey"),
        );
        let export = jump_export(&sources).unwrap();
        assert!(export
            .notes
            .iter()
            .any(|note| note.message.contains("cannot sign in")));
    }

    #[test]
    fn three_hop_inventory_export_preserves_ordered_endpoints_identity_trust_and_mux() {
        let export = jump_export(&jump_sources()).unwrap();
        let workspace = &export.document["workspace"];
        assert_eq!(
            export.machines, 1,
            "transit hosts are not selectable machines"
        );
        assert_eq!(
            export.routes, 5,
            "three transit hosts, destination, and mux"
        );
        assert!(export.notes.is_empty(), "{:?}", export.notes);
        let destination = exported_route(&export, "r.app.ssh");
        let jumps = destination["jump_routes"].as_array().unwrap();
        assert_eq!(jumps.len(), 3);
        assert_eq!(destination["session_creation"], "on_select");
        let mux = exported_route(&export, "r.app.mux");
        assert_eq!(mux["jump_routes"], destination["jump_routes"]);
        assert_eq!(mux["mode"], "frankenterm_mux");
        assert_eq!(mux["attach_policy"], "existing_only");
        assert!(mux.get("session_creation").is_none());
        assert_eq!(
            workspace["machines"][0]["route_refs"],
            json!(["r.app.mux", "r.app.ssh"])
        );

        let expected = [
            ("first", "outer.example", 2221, "gateway", 1),
            ("second", "middle.internal", 22, "transit", 2),
            ("third", "inner.internal", 2223, "jump", 3),
            ("app", "service.internal", 2224, "deploy", 4),
        ];
        for (index, (alias, host, port, user, seed)) in expected.iter().copied().enumerate() {
            let route = if index < 3 {
                let route = exported_route(&export, jumps[index].as_str().unwrap());
                assert!(route.get("session_creation").is_none());
                assert_eq!(route["jump_routes"], json!([]));
                route
            } else {
                destination
            };
            assert_eq!(route["host"], host);
            assert_eq!(route["port"], port);
            assert_eq!(route["user"], user);
            assert_eq!(
                route["authentication_methods"],
                json!(["publickey", "keyboard-interactive", "password"])
            );
            let credential_ids = route["credential_refs"].as_array().unwrap();
            assert_eq!(credential_ids.len(), 1);
            let credential = workspace["credentials"]
                .as_array()
                .unwrap()
                .iter()
                .find(|credential| credential["id"] == credential_ids[0])
                .unwrap();
            assert_eq!(
                credential["public_fingerprint"],
                PublicKey::parse_line(&fake_key(seed))
                    .unwrap()
                    .fingerprint()
            );
            assert_eq!(
                export.identity_files[credential_ids[0].as_str().unwrap()],
                context().home.join(format!(".ssh/{}", alias))
            );
            let trust = route_trust(&export, route["id"].as_str().unwrap());
            assert_eq!(trust["host"], host);
            assert_eq!(trust["port"], port);
            assert_eq!(trust["pins"].as_array().unwrap().len(), 1);
            assert_eq!(
                trust["pins"][0]["fingerprint"],
                PublicKey::parse_line(&fake_key(seed + 40))
                    .unwrap()
                    .fingerprint()
            );
            assert!(trust["required_capabilities"].is_null());
        }
        assert_eq!(workspace["credentials"].as_array().unwrap().len(), 4);
        assert_eq!(workspace["host_trust"].as_array().unwrap().len(), 4);
        let outer = exported_route(&export, jumps[0].as_str().unwrap());
        assert_eq!(outer["host_key_alias"], "outer-key");
        assert_eq!(
            route_trust(&export, outer["id"].as_str().unwrap())["host_key_alias"],
            "outer-key"
        );
        let public = serde_json::to_string(&export.document).unwrap();
        assert!(!public.contains("/trust/") && !public.contains(".ssh/"));
        let source = companion_source(&export, Path::new("/config/machines.toml"));
        assert_eq!(source["identity_files"].as_object().unwrap().len(), 4);
    }

    #[test]
    fn proxyjump_expands_only_the_first_hops_configured_prefix_and_honors_proxy_precedence() {
        for (text, expected) in [
            (
                "Host app\n ProxyJump third\nHost third\n ProxyJump second\nHost second\n ProxyJump first\nHost first\n",
                vec!["first", "second", "third"],
            ),
            (
                "Host app\n ProxyJump second,third\nHost second\n ProxyJump first\nHost first\nHost third\n ProxyJump nonexistent\n",
                vec!["first", "second", "third"],
            ),
            (
                "Host app\n ProxyJump first\n ProxyCommand touch /must-not-execute\nHost first\n",
                vec!["first"],
            ),
            (
                "Host app\n ProxyCommand none\n ProxyJump nonexistent\n",
                vec![],
            ),
            (
                "Host app\n ProxyJump NONE\n ProxyCommand touch /must-not-execute\n",
                vec![],
            ),
        ] {
            let resolver = alias_resolver(Some(text), &context().home).unwrap();
            assert_eq!(resolver.jump_chain("app").unwrap(), expected, "{}", text);
        }
        let mut sources = jump_sources();
        sources.config = Some(JUMP_CONFIG.replace(
            "Host second\n",
            "Host second\n ProxyCommand touch /must-not-execute\n ProxyJump nonexistent\n",
        ));
        let export = jump_export(&sources).unwrap();
        assert_eq!(
            export.routes, 5,
            "the explicit preceding hop overrides second's proxy"
        );
        assert!(!serde_json::to_string(&export.document)
            .unwrap()
            .contains("must-not-execute"));
        sources.config = Some(JUMP_CONFIG.replace(
            "Host first\n",
            "Host first\n ProxyCommand touch /must-not-execute\n",
        ));
        assert!(
            jump_export(&sources).is_err(),
            "the first hop cannot run a proxy command"
        );
        sources.config = Some(JUMP_CONFIG.replace(
            "Host app\n",
            "Host app\n ProxyCommand touch /must-not-execute\n",
        ));
        assert!(
            jump_export(&sources).is_err(),
            "a destination's earlier proxy command wins"
        );
        sources.config = Some(JUMP_CONFIG.replace(
            "Host app\n",
            "Host app\n frankenterm_proxy __frankenterm_proxy_1__\n ProxyCommand touch /must-not-execute\n",
        ));
        assert!(
            jump_export(&sources).is_err(),
            "source text cannot inject the resolver's private proxy slot"
        );
    }

    #[test]
    fn invalid_jump_graphs_never_export_a_direct_fallback_or_partial_transit_chain() {
        for value in [
            "missing",
            "first,missing",
            "first,second,third,fourth",
            "first,first",
            "app",
            "first,app",
            "first,,second",
            "first,",
            ",first",
            "user@first",
            "first:2222",
            "ssh://first",
            "first,none",
            "-first",
            "%j",
            "${JUMP}",
            "first, second",
        ] {
            let config = format!(
                "Host app\n ProxyJump {}\nHost first second third fourth\n User u\n",
                value
            );
            let resolver = alias_resolver(Some(&config), &context().home).unwrap();
            assert!(resolver.jump_chain("app").is_err(), "{}", value);
            let mut sources = jump_sources();
            sources.config = Some(config);
            assert!(jump_export(&sources).is_err(), "{}", value);
        }
        for config in [
            "Host app\n ProxyJump first\nHost first\n ProxyJump app\n",
            "Host app\n ProxyJump first,second\nHost first\n ProxyJump second\nHost second\n",
            "Host app\n ProxyJump second,third\nHost second\n ProxyJump first\nHost first\n ProxyJump fourth\nHost third fourth\n",
            "Host app\n ProxyJump first\nHost fir*\n User u\n",
        ] {
            let resolver = alias_resolver(Some(config), &context().home).unwrap();
            assert!(resolver.jump_chain("app").is_err(), "{}", config);
        }
        let mut sources = jump_sources();
        sources.config = Some(JUMP_CONFIG.replace("Port 2223", "Port invalid"));
        assert!(
            jump_export(&sources).is_err(),
            "an invalid endpoint rejects the whole graph"
        );
        let text = format!(
            "{}\n[[machines.safe.routes]]\nid = \"ssh\"\nhost = \"100.91.120.17\"\nuser = \"ubuntu\"\nidentity_file = \"~/.ssh/trj_ed25519\"\n",
            JUMP_INVENTORY
        );
        let inventory = MachineInventory::parse(&text, &context()).unwrap();
        let export = export(&inventory, text.as_bytes(), None, &sources, &context()).unwrap();
        assert_eq!(export.machines, 1);
        assert_eq!(export.routes, 1);
        assert_eq!(
            export.document["workspace"]["routes"][0]["id"],
            "r.safe.ssh"
        );
        assert_eq!(
            export.document["workspace"]["host_trust"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            export.identity_files.len(),
            1,
            "refused transit keys cannot enter the companion's sealing list"
        );
    }

    #[test]
    fn a_transit_hosts_required_policy_and_revocations_survive_in_the_workspace() {
        for policy in [
            "HostKeyAlgorithms ssh-ed25519",
            "RevokedHostKeys /must-not-read",
            "KnownHostsCommand touch /must-not-execute",
            "CertificateFile ~/.ssh/second-cert.pub",
            "PubkeyAuthentication host-bound",
        ] {
            let mut sources = jump_sources();
            sources.config =
                Some(JUMP_CONFIG.replace("Host second\n", &format!("Host second\n {}\n", policy)));
            let export = jump_export(&sources).unwrap();
            let route = exported_route(&export, "r.app.ssh");
            let hop = route["jump_routes"][1].as_str().unwrap();
            assert_eq!(
                route_trust(&export, hop)["required_capabilities"],
                json!([UNRESOLVED_POLICY]),
                "{}",
                policy
            );
            assert!(route_trust(&export, "r.app.ssh")["required_capabilities"].is_null());
        }
        let mut sources = jump_sources();
        supply(
            &mut sources,
            "/trust/second",
            format!(
                "middle.internal {}\n@revoked middle.internal {}\n",
                fake_key(42),
                fake_key(42)
            ),
        );
        let export = jump_export(&sources).unwrap();
        let hop = exported_route(&export, "r.app.ssh")["jump_routes"][1]
            .as_str()
            .unwrap();
        let trust = route_trust(&export, hop);
        assert!(trust["pins"].as_array().unwrap().is_empty());
        assert_eq!(trust["revoked_keys"].as_array().unwrap().len(), 1);
        sources.known_hosts.insert(
            PathBuf::from("/trust/second"),
            KnownHostsSource::Unavailable,
        );
        let export = jump_export(&sources).unwrap();
        let hop = exported_route(&export, "r.app.ssh")["jump_routes"][1]
            .as_str()
            .unwrap();
        assert_eq!(
            route_trust(&export, hop)["required_capabilities"],
            json!([UNRESOLVED_POLICY])
        );
    }

    #[test]
    fn selecting_a_group_cannot_discard_an_unselected_jump_hosts_same_scope_denial() {
        let text = format!(
            "{}\n[groups.selected]\nmachines = [\"left\"]\n",
            TRUST_INVENTORY
        );
        let inventory = MachineInventory::parse(&text, &context()).unwrap();
        let mut sources = sources(Some(
            "Host left\n HostName left.internal\n ProxyJump first\n UserKnownHostsFile /trust/left\nHost right\n HostName right.internal\n ProxyJump second\n UserKnownHostsFile /trust/right\nHost first\n HostName shared\n UserKnownHostsFile /trust/first\nHost second\n HostName SHARED.\n UserKnownHostsFile /trust/second\nHost *\n User u\n IdentityFile ~/.ssh/trj_ed25519\n GlobalKnownHostsFile none\n",
        ));
        supply(
            &mut sources,
            "/trust/left",
            format!("left.internal {}\n", fake_key(11)),
        );
        supply(
            &mut sources,
            "/trust/right",
            format!("right.internal {}\n", fake_key(12)),
        );
        supply(
            &mut sources,
            "/trust/first",
            format!("shared {}\n", fake_key(41)),
        );
        supply(
            &mut sources,
            "/trust/second",
            format!("SHARED. {}\n", fake_key(42)),
        );
        let before = export(&inventory, text.as_bytes(), None, &sources, &context()).unwrap();
        let first = exported_route(&before, "r.left.ssh")["jump_routes"][0]
            .as_str()
            .unwrap();
        let second = exported_route(&before, "r.right.ssh")["jump_routes"][0]
            .as_str()
            .unwrap();
        let left = route_trust(&before, first);
        let right = route_trust(&before, second);
        assert_ne!(left["id"], right["id"]);
        assert_eq!(left["pins"].as_array().unwrap().len(), 1);
        assert_eq!(right["pins"].as_array().unwrap().len(), 1);
        assert_ne!(
            left["pins"], right["pins"],
            "positive pins remain route-specific"
        );

        supply(
            &mut sources,
            "/trust/second",
            format!("@revoked shared {}\n", fake_key(41)),
        );
        let selected = export(
            &inventory,
            text.as_bytes(),
            Some("selected"),
            &sources,
            &context(),
        )
        .unwrap();
        assert_eq!(selected.machines, 1);
        assert_eq!(selected.routes, 2);
        let first = exported_route(&selected, "r.left.ssh")["jump_routes"][0]
            .as_str()
            .unwrap();
        let trust = route_trust(&selected, first);
        assert!(trust["pins"].as_array().unwrap().is_empty());
        assert_eq!(
            trust["revoked_keys"][0]["fingerprint"],
            PublicKey::parse_line(&fake_key(41)).unwrap().fingerprint()
        );

        sources.config = sources
            .config
            .as_ref()
            .map(|text| text.replace("Host right\n", "Host right\n Port invalid\n"));
        let refused = export(&inventory, text.as_bytes(), None, &sources, &context()).unwrap();
        assert_eq!(
            refused.machines, 1,
            "the invalid destination is omitted atomically"
        );
        let first = exported_route(&refused, "r.left.ssh")["jump_routes"][0]
            .as_str()
            .unwrap();
        assert!(
            route_trust(&refused, first)["pins"]
                .as_array()
                .unwrap()
                .is_empty(),
            "its resolved transit host still contributes a denial"
        );

        sources.known_hosts.insert(
            PathBuf::from("/trust/second"),
            KnownHostsSource::Unavailable,
        );
        let selected = export(
            &inventory,
            text.as_bytes(),
            Some("selected"),
            &sources,
            &context(),
        )
        .unwrap();
        let first = exported_route(&selected, "r.left.ssh")["jump_routes"][0]
            .as_str()
            .unwrap();
        assert_eq!(
            route_trust(&selected, first)["required_capabilities"],
            json!([UNRESOLVED_POLICY])
        );
    }

    #[test]
    fn route_limit_counts_complete_transit_graphs_and_never_merges_their_trust() {
        let mut text = "version = 1\n".to_string();
        for index in 0..64 {
            text.push_str(&format!(
                "[[machines.m{}.routes]]\nid = \"ssh\"\nssh = \"app\"\n",
                index
            ));
        }
        let inventory = MachineInventory::parse(&text, &context()).unwrap();
        let sources = jump_sources();
        let complete = export(&inventory, text.as_bytes(), None, &sources, &context()).unwrap();
        assert_eq!(complete.routes, 256);
        assert_eq!(
            complete.document["workspace"]["host_trust"]
                .as_array()
                .unwrap()
                .len(),
            256
        );
        let ids: BTreeSet<_> = complete.document["workspace"]["routes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|route| route["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids.len(), 256);
        text.push_str("[[machines.overflow.routes]]\nid = \"ssh\"\nssh = \"app\"\n");
        let inventory = MachineInventory::parse(&text, &context()).unwrap();
        let result = export(&inventory, text.as_bytes(), None, &sources, &context());
        assert!(
            result.is_err(),
            "an extra graph cannot be partially exported"
        );
        assert!(result
            .err()
            .unwrap()
            .to_string()
            .contains("256-route limit"));
    }

    #[test]
    fn disk_snapshot_collects_every_hops_public_key_and_effective_trust_file() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join("trust")).unwrap();
        // Disable any machine-specific system proxy default after the
        // fixture's explicit destination chain has already won precedence.
        let config = format!(
            "{} ProxyCommand none\n",
            JUMP_CONFIG.replace("/trust/", "~/trust/")
        );
        write_source(&home.join(".ssh/config"), config);
        for (seed, alias, lookup) in [
            (1, "first", "outer-key"),
            (2, "second", "middle.internal"),
            (3, "third", "[inner.internal]:2223"),
            (4, "app", "[service.internal]:2224"),
        ] {
            // There is deliberately no private file: passive discovery needs
            // only the .pub, and supplies a path for later companion review.
            write_source(&home.join(format!(".ssh/{}.pub", alias)), fake_key(seed));
            write_source(
                &home.join(format!("trust/{}", alias)),
                format!("{} {}\n", lookup, fake_key(seed + 40)),
            );
        }
        let context = InventoryContext {
            directory: directory.path().to_path_buf(),
            home,
        };
        let inventory = MachineInventory::parse(JUMP_INVENTORY, &context).unwrap();
        let snapshot = SshSources::from_disk(&inventory, &context);
        for alias in ["first", "second", "third", "app"] {
            assert!(snapshot
                .public_keys
                .contains_key(&context.home.join(format!(".ssh/{}.pub", alias))));
            assert!(matches!(
                snapshot
                    .known_hosts
                    .get(&context.home.join(format!("trust/{}", alias))),
                Some(KnownHostsSource::Present(_))
            ));
        }
        let export = export(
            &inventory,
            JUMP_INVENTORY.as_bytes(),
            None,
            &snapshot,
            &context,
        )
        .unwrap();
        assert_eq!(export.routes, 5);
        assert_eq!(export.identity_files.len(), 4);
        assert!(export.identity_files.values().all(|path| !path.exists()));
        for trust in export.document["workspace"]["host_trust"]
            .as_array()
            .unwrap()
        {
            assert_eq!(trust["pins"].as_array().unwrap().len(), 1);
        }
        // Existing public files are deliberately excluded when the effective
        // method list never offers publickey, even without disabling the flag.
        write_source(
            &context.home.join(".ssh/config"),
            format!(
                "{} ProxyCommand none\n PreferredAuthentications password\n PasswordAuthentication YES\n",
                JUMP_CONFIG.replace("/trust/", "~/trust/")
            ),
        );
        let snapshot = SshSources::from_disk(&inventory, &context);
        assert!(snapshot.public_keys.is_empty());
        let password_export = super::export(
            &inventory,
            JUMP_INVENTORY.as_bytes(),
            None,
            &snapshot,
            &context,
        )
        .unwrap();
        assert!(password_export.identity_files.is_empty());
        assert_eq!(
            password_export.document["workspace"]["credentials"],
            json!([])
        );
        for route in password_export.document["workspace"]["routes"]
            .as_array()
            .unwrap()
        {
            assert_eq!(route["authentication_methods"], json!(["password"]));
            assert_eq!(route["credential_refs"], json!([]));
        }
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
        assert!(route_trust(&after, "r.left.ssh")["pins"]
            .as_array()
            .unwrap()
            .is_empty());
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
        assert!(route_trust(&export, "r.left.ssh")["pins"]
            .as_array()
            .unwrap()
            .is_empty());
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
            assert!(route_trust(&after, route)["pins"]
                .as_array()
                .unwrap()
                .is_empty());
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
            "PubkeyAuthentication host-bound",
            "PreferredAuthentications unsupported-method",
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
            use std::os::unix::fs::{symlink, PermissionsExt};
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
        // This cross-language fixture deliberately models key-only routes;
        // do not make an implicit SSH default redefine their method policy.
        // That includes dark's alias, which otherwise has no Host block.
        let config = "Host css\n  HostName css.example\n  User ubuntu\n  IdentityFile ~/.ssh/trj_ed25519\n  PreferredAuthentications publickey\nHost jumped\n  PreferredAuthentications publickey\n";
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
