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
use finl_unicode::categories::CharacterCategories;
use hmac::{Hmac, KeyInit, Mac};
use openssl::bn::{BigNum, BigNumContext};
use openssl::ec::{EcGroup, EcKey, EcPoint};
use openssl::ecdsa::EcdsaSig;
use openssl::hash::MessageDigest;
use openssl::nid::Nid;
use openssl::pkey::{Id, PKey, Public};
use openssl::rsa::{Padding, Rsa};
use openssl::sign::Verifier;
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
const MAX_ROUTE_IDENTITIES: usize = 8;
const MAX_CERTIFICATE_BLOB_BYTES: usize = 32 * 1024;
const MAX_EXPORTED_CERTIFICATE_BYTES: usize = 1024 * 1024;
const USER_CERTIFICATE_CAPABILITY: &str = "ssh_user_certificate_v1";
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
    /// Sources whose contents could not be established, including files beyond
    /// the snapshot's read budgets. These are not observed missing identities:
    /// an affected route must not silently export its other candidates instead.
    pub unavailable_public_keys: BTreeSet<PathBuf>,
    /// Explicit CertificateFile sources only. Missing or unavailable files
    /// remain witnessed failures; no implicit certificate or private sibling
    /// is discovered by filename.
    pub public_certificates: BTreeMap<PathBuf, KnownHostsSource>,
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

/// The deepest `Include` nesting OpenSSH reads (READCONF_MAX_DEPTH).
const MAX_INCLUDE_DEPTH: usize = 16;
/// Directory operations and entries the complete configuration snapshot may
/// inspect, shared by user/system sources and every Include pattern.
const MAX_INCLUDE_ENTRIES: usize = 16_384;

/// Where `expand_includes` reads: the disk, or a test's fixture.
trait IncludeFiles {
    /// One included file, under `read_source`'s rules.
    fn read(&self, path: &Path, limit: usize) -> KnownHostsSource;
    /// The names in `directory`: empty when it does not exist, `None` when
    /// it cannot be listed completely within the remaining traversal budget.
    fn names(&self, directory: &Path, limit: usize) -> Option<Vec<String>>;
    /// Glob components before the last must name directories. Missing paths
    /// and ordinary files do not match; an unreadable path remains unresolved.
    fn is_directory(&self, path: &Path) -> Option<bool>;
}

struct DiskIncludes;

impl IncludeFiles for DiskIncludes {
    fn read(&self, path: &Path, limit: usize) -> KnownHostsSource {
        // macOS ships /etc/ssh/crypto.conf as a root-owned link to
        // crypto/apple.conf. Follow one link owned by root or this user; the
        // target is read under the same rules and may not be a link itself.
        #[cfg(unix)]
        if let Ok(link) = std::fs::symlink_metadata(path) {
            use std::os::unix::fs::MetadataExt;
            if link.file_type().is_symlink() {
                let owner = link.uid();
                if owner != 0 && owner != nix::unistd::Uid::current().as_raw() {
                    return KnownHostsSource::Unavailable;
                }
                return match std::fs::read_link(path) {
                    Ok(target) => read_source(
                        &path
                            .parent()
                            .map_or(target.clone(), |dir| dir.join(&target)),
                        limit,
                    ),
                    Err(_) => KnownHostsSource::Unavailable,
                };
            }
        }
        read_source(path, limit)
    }

    fn names(&self, directory: &Path, limit: usize) -> Option<Vec<String>> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                return Some(Vec::new());
            }
            Err(_) => return None,
        };
        let mut names = Vec::new();
        for entry in entries {
            if names.len() == limit {
                return None;
            }
            names.push(entry.ok()?.file_name().into_string().ok()?);
        }
        Some(names)
    }

    fn is_directory(&self, path: &Path) -> Option<bool> {
        match std::fs::metadata(path) {
            Ok(metadata) => Some(metadata.is_dir()),
            Err(err) if matches!(err.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
                Some(false)
            }
            Err(_) => None,
        }
    }
}

/// What one SSH source, with everything it includes, may still add.
struct IncludeBudget {
    bytes: usize,
    lines: usize,
    files: usize,
    entries: usize,
}

impl IncludeBudget {
    fn new() -> Self {
        // The margin covers the `Host *` lines splicing adds, so the result
        // still fits the resolver's own limit.
        Self {
            bytes: MAX_SOURCE_BYTES - 4096,
            lines: MAX_SOURCE_LINES - 512,
            files: MAX_SOURCE_FILES,
            entries: MAX_INCLUDE_ENTRIES,
        }
    }

    fn take(&mut self, text: &str) -> bool {
        let lines = text.lines().count();
        if text.len() > self.bytes || lines > self.lines {
            return false;
        }
        self.bytes -= text.len();
        self.lines -= lines;
        true
    }

    fn names(&mut self, files: &dyn IncludeFiles, directory: &Path) -> Option<Vec<String>> {
        // Charge the operation even for empty/nonexistent directories. On an
        // incomplete listing, retire the remaining budget: repeated patterns
        // must not retry the same expensive failed scan for free.
        self.entries = self.entries.checked_sub(1)?;
        let names = files.names(directory, self.entries);
        match names {
            Some(names) if names.len() <= self.entries => {
                self.entries -= names.len();
                Some(names)
            }
            _ => {
                self.entries = 0;
                None
            }
        }
    }

    fn is_directory(&mut self, files: &dyn IncludeFiles, path: &Path) -> Option<bool> {
        self.entries = self.entries.checked_sub(1)?;
        files.is_directory(path)
    }
}

/// Reads one top-level SSH source and splices in what it includes.
fn read_config_source(
    path: &Path,
    base: &Path,
    home: &Path,
    budget: &mut IncludeBudget,
) -> KnownHostsSource {
    let Some(remaining) = budget.files.checked_sub(1) else {
        return KnownHostsSource::Unavailable;
    };
    budget.files = remaining;
    match read_source(path, budget.bytes) {
        KnownHostsSource::Present(text) if budget.take(&text) => KnownHostsSource::Present(
            expand_includes(&text, base, home, &DiskIncludes, budget, 0, true),
        ),
        KnownHostsSource::Present(_) => KnownHostsSource::Unavailable,
        other => other,
    }
}

/// Splices `Include`d files into `text` where OpenSSH's meaning survives as
/// flat text (ft-5qgrk): at the top level or under `Host *`, or under any
/// other Host when the included text starts no Host or Match block. A file
/// starts in its includer's context, and that context returns after it, as
/// OpenSSH restores it. An Include it cannot splice in full stays as written
/// (the resolver then treats the source as unresolved): a pattern needing
/// token, variable or bracket expansion, a file it cannot read safely,
/// or the depth or budget exceeded. A missing file adds nothing, as in
/// OpenSSH. Relative patterns resolve against `base`: ~/.ssh for the user's
/// file and /etc/ssh for the system's.
fn expand_includes(
    text: &str,
    base: &Path,
    home: &Path,
    files: &dyn IncludeFiles,
    budget: &mut IncludeBudget,
    depth: usize,
    mut unconditional: bool,
) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if let Some((keyword, value)) = directive(line) {
            if keyword.eq_ignore_ascii_case("host") || keyword.eq_ignore_ascii_case("match") {
                unconditional = keyword.eq_ignore_ascii_case("host")
                    && configuration_words(value).is_some_and(|words| words == ["*"]);
            } else if keyword.eq_ignore_ascii_case("include") {
                if let Some(spliced) =
                    splice_include(value, base, home, files, budget, depth, unconditional)
                {
                    out.push_str(&spliced);
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

fn splice_include(
    value: &str,
    base: &Path,
    home: &Path,
    files: &dyn IncludeFiles,
    budget: &mut IncludeBudget,
    depth: usize,
    unconditional: bool,
) -> Option<String> {
    if depth >= MAX_INCLUDE_DEPTH {
        return None;
    }
    let mut spliced = String::new();
    for pattern in configuration_words(value)? {
        for path in include_paths(&pattern, base, home, files, budget)? {
            budget.files = budget.files.checked_sub(1)?;
            let text = match files.read(&path, budget.bytes) {
                KnownHostsSource::Present(text) => text,
                KnownHostsSource::Missing => continue,
                KnownHostsSource::Unavailable => return None,
            };
            if !budget.take(&text) {
                return None;
            }
            let expanded =
                expand_includes(&text, base, home, files, budget, depth + 1, unconditional);
            let opens_blocks = expanded.lines().filter_map(directive).any(|(keyword, _)| {
                keyword.eq_ignore_ascii_case("host") || keyword.eq_ignore_ascii_case("match")
            });
            if opens_blocks && !unconditional {
                return None;
            }
            spliced.push_str(&expanded);
            if opens_blocks {
                // Back to the includer's context: everything, as before.
                spliced.push_str("Host *\n");
            }
        }
    }
    Some(spliced)
}

/// The files one Include pattern names in lexical order. `*` and `?` may
/// appear in any component; they never cross a path separator. A shared scan
/// budget and a 64-path frontier bound traversal before any included source is
/// read. An exhausted or unreadable branch refuses the complete Include.
fn include_paths(
    pattern: &str,
    base: &Path,
    home: &Path,
    files: &dyn IncludeFiles,
    budget: &mut IncludeBudget,
) -> Option<Vec<PathBuf>> {
    if pattern.is_empty()
        || pattern.len() > 4096
        || pattern.ends_with('/')
        || pattern.contains(['%', '$', '[', ']', '{', '}'])
    {
        return None;
    }
    // The supplied home/base are literal paths. A wildcard in a directory's
    // own name must not become syntax just because a relative Include uses it.
    let (root, path) = if let Some(rest) = pattern.strip_prefix("~/") {
        (home.to_path_buf(), PathBuf::from(rest))
    } else if pattern.starts_with('~') {
        return None;
    } else if Path::new(pattern).is_absolute() {
        (PathBuf::new(), PathBuf::from(pattern))
    } else {
        (base.to_path_buf(), PathBuf::from(pattern))
    };
    if path
        .components()
        .chain(root.components())
        .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    let components: Vec<_> = path.components().collect();
    let mut paths = vec![root];
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            for path in &mut paths {
                path.push(component.as_os_str());
            }
            continue;
        };
        let name = name.to_str()?;
        let last = index + 1 == components.len();
        let mut next = Vec::new();
        for parent in paths {
            let wildcard = name.contains(['*', '?']);
            let names = if wildcard {
                budget.names(files, &parent)?
            } else {
                vec![name.to_string()]
            };
            for candidate_name in names {
                if wildcard && !glob_component_matches(name, &candidate_name) {
                    continue;
                }
                let candidate = parent.join(candidate_name);
                if last || budget.is_directory(files, &candidate)? {
                    if next.len() >= MAX_SOURCE_FILES {
                        return None;
                    }
                    next.push(candidate);
                }
            }
        }
        paths = next;
        if paths.is_empty() {
            break;
        }
    }
    paths.sort();
    Some(paths)
}

/// glob(3)'s `*` and `?` on one UTF-8 component: a leading `.` is matched only
/// by a pattern's leading `.`. A question mark consumes one Unicode scalar,
/// not one byte of a multibyte filename. There is one backtrack point.
fn glob_component_matches(pattern: &str, name: &str) -> bool {
    if name.starts_with('.') && !pattern.starts_with('.') {
        return false;
    }
    let (pattern, name): (Vec<_>, Vec<_>) = (pattern.chars().collect(), name.chars().collect());
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && pattern[p] == '*' {
            star = Some((p, n));
            p += 1;
        } else if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if let Some((star_p, star_n)) = star {
            p = star_p + 1;
            n = star_n + 1;
            star = Some((star_p, star_n + 1));
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|&character| character == '*')
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
    /// absent; unreadable and over-budget sources remain explicit blockers.
    pub fn from_disk(inventory: &MachineInventory, context: &InventoryContext) -> Self {
        let ssh = context.home.join(".ssh");
        // One budget for both sources and everything they include.
        let mut budget = IncludeBudget::new();
        let config = match read_config_source(&ssh.join("config"), &ssh, &context.home, &mut budget)
        {
            KnownHostsSource::Present(text) => Some(text),
            _ => None,
        };
        let system_config = read_config_source(
            Path::new("/etc/ssh/ssh_config"),
            Path::new("/etc/ssh"),
            &context.home,
            &mut budget,
        );
        let resolver = alias_resolver_with_system(config.as_deref(), &system_config, &context.home);
        let default_known_hosts = default_known_hosts(&context.home);
        let mut known_files: BTreeSet<PathBuf> = default_known_hosts.iter().cloned().collect();
        let mut identities = BTreeSet::new();
        let mut certificate_files = BTreeSet::new();
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
                                    certificate_files
                                        .extend(resolver.certificate_files(&options).0);
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
        let mut public_keys = BTreeMap::new();
        let mut unavailable_public_keys = BTreeSet::new();
        for (index, public) in identities
            .into_iter()
            .map(|identity| public_key_path(&identity))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .enumerate()
        {
            let source = if index < MAX_PUBLIC_KEY_FILES {
                read_source(&public, remaining_public_bytes.min(64 * 1024))
            } else {
                KnownHostsSource::Unavailable
            };
            match source {
                KnownHostsSource::Present(text) => {
                    remaining_public_bytes -= text.len();
                    public_keys.insert(public, text);
                }
                KnownHostsSource::Missing => {
                    // A private IdentityFile need not have a .pub sidecar.
                    // Probe existence only: passive discovery must not read or
                    // unlock private material. Only ENOENT establishes absence;
                    // an existing file/link or failed probe leaves identity
                    // resolution unavailable rather than silently omitting it.
                    let private = public.with_extension("");
                    if !matches!(std::fs::symlink_metadata(private), Err(error) if error.kind() == ErrorKind::NotFound)
                    {
                        unavailable_public_keys.insert(public);
                    }
                }
                KnownHostsSource::Unavailable => {
                    unavailable_public_keys.insert(public);
                }
            }
        }
        let mut remaining_certificate_bytes = MAX_SOURCE_BYTES;
        let public_certificates = certificate_files
            .into_iter()
            .enumerate()
            .map(|(index, path)| {
                let source = if index < MAX_PUBLIC_KEY_FILES {
                    read_source(&path, remaining_certificate_bytes.min(64 * 1024))
                } else {
                    KnownHostsSource::Unavailable
                };
                if let KnownHostsSource::Present(text) = &source {
                    remaining_certificate_bytes -= text.len();
                }
                (path, source)
            })
            .collect();
        Self {
            config,
            system_config,
            known_hosts,
            default_known_hosts,
            public_keys,
            unavailable_public_keys,
            public_certificates,
        }
    }
}

struct AliasResolver {
    config: frankenterm_ssh::Config,
    trust_lists: BTreeMap<String, Option<Vec<PathBuf>>>,
    proxies: BTreeMap<String, ProxySetting>,
    authentication_values: BTreeMap<String, Option<String>>,
    certificate_paths: Vec<(String, Option<PathBuf>)>,
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
    /// Every matching CertificateFile contributes in source order. Opaque,
    /// unique resolver slots preserve additive semantics and quoted paths
    /// without allowing the scalar resolver to discard later declarations.
    fn certificate_files(&self, options: &frankenterm_ssh::ConfigMap) -> (Vec<PathBuf>, bool) {
        let mut paths = Vec::new();
        let mut incomplete = false;
        for (slot, path) in &self.certificate_paths {
            if options.contains_key(slot) {
                match path {
                    Some(path) if !paths.contains(path) => paths.push(path.clone()),
                    Some(_) => {}
                    None => incomplete = true,
                }
            }
        }
        (paths, incomplete)
    }

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
    let mut certificate_paths = Vec::new();
    let mut passive = String::new();
    let mut incomplete = false;
    for line in text.lines() {
        if let Some((keyword, value)) = directive(line) {
            if keyword
                .to_ascii_lowercase()
                .starts_with("frankenterm_certificate_")
                || keyword.eq_ignore_ascii_case("frankenterm_required_certificate")
            {
                // Source text cannot select a synthetic parser slot belonging
                // to an unrelated Host block.
                incomplete = true;
                continue;
            }
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
                "certificatefile" => {
                    let words = configuration_words(value);
                    if words.as_ref().is_some_and(|words| {
                        words.len() == 1 && words[0].eq_ignore_ascii_case("none")
                    }) {
                        continue;
                    }
                    let path = words
                        .filter(|words| words.len() == 1)
                        .and_then(|_| known_host_paths(value, home))
                        .and_then(|mut paths| paths.pop());
                    let slot = format!("frankenterm_certificate_{}", certificate_paths.len());
                    passive.push_str(&format!("{} yes\n", slot));
                    certificate_paths.push((slot, path));
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
        certificate_paths,
        aliases,
        incomplete,
    })
}

/// The concrete `Host` aliases in the user's SSH config, without patterns.
pub fn ssh_config_hosts(home: &Path) -> Vec<String> {
    let ssh = home.join(".ssh");
    let text = match read_config_source(&ssh.join("config"), &ssh, home, &mut IncludeBudget::new())
    {
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

/// One completely verified OpenSSH v01 user certificate. Verification proves
/// the signed contents, not that a server trusts this CA. The phone preserves
/// the exact certificate and checks time, principals, critical options and
/// PTY/forwarding permissions again before each new authentication.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UserCertificate {
    blob: Vec<u8>,
    subject: PublicKey,
}

struct SshBlob<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> SshBlob<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let end = self.offset.checked_add(count)?;
        let value = self.bytes.get(self.offset..end)?;
        self.offset = end;
        Some(value)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn string(&mut self, limit: usize) -> Option<&'a [u8]> {
        let count = self.u32()? as usize;
        if count > limit {
            return None;
        }
        self.take(count)
    }

    fn name(&mut self, limit: usize) -> Option<&'a str> {
        let value = self.string(limit)?;
        if value.is_empty() || !value.iter().all(|byte| (33..=126).contains(byte)) {
            return None;
        }
        std::str::from_utf8(value).ok()
    }

    fn positive_mpint(&mut self, limit: usize) -> Option<&'a [u8]> {
        let encoded = self.string(limit + 1)?;
        let first = *encoded.first()?;
        if first & 0x80 != 0 {
            return None;
        }
        let magnitude = if first == 0 {
            if encoded.get(1)? & 0x80 == 0 {
                return None;
            }
            &encoded[1..]
        } else {
            encoded
        };
        (magnitude.len() <= limit).then_some(magnitude)
    }

    fn finished(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

struct CertificateCurve {
    name: &'static str,
    nid: Nid,
    point_bytes: usize,
    scalar_bytes: usize,
    digest: MessageDigest,
}

impl CertificateCurve {
    fn for_algorithm(algorithm: &str) -> Option<Self> {
        let (name, nid, point_bytes, scalar_bytes, digest) = match algorithm {
            "ecdsa-sha2-nistp256" => (
                "nistp256",
                Nid::X9_62_PRIME256V1,
                65,
                32,
                MessageDigest::sha256(),
            ),
            "ecdsa-sha2-nistp384" => ("nistp384", Nid::SECP384R1, 97, 48, MessageDigest::sha384()),
            "ecdsa-sha2-nistp521" => ("nistp521", Nid::SECP521R1, 133, 66, MessageDigest::sha512()),
            _ => return None,
        };
        Some(Self {
            name,
            nid,
            point_bytes,
            scalar_bytes,
            digest,
        })
    }
}

/// The same canonical raw-key profile as SSHPublicIdentity on the phone:
/// Ed25519, NIST P-256/384/521, or odd 2048..8192-bit RSA with an odd exponent
/// of at most 64 bits. OpenSSL validates EC points and performs all crypto.
fn certificate_public_key(blob: &[u8]) -> Option<(PublicKey, PKey<Public>)> {
    if blob.is_empty() || blob.len() > 1053 {
        return None;
    }
    let mut reader = SshBlob::new(blob);
    let algorithm = reader.name(64)?;
    let key = if algorithm == "ssh-ed25519" {
        let bytes = reader.string(32)?;
        if bytes.len() != 32 {
            return None;
        }
        PKey::public_key_from_raw_bytes(bytes, Id::ED25519).ok()?
    } else if algorithm == "ssh-rsa" {
        let exponent = reader.positive_mpint(8)?;
        let modulus = reader.positive_mpint(1024)?;
        let bits = (modulus.len() - 1) * 8 + (8 - modulus[0].leading_zeros() as usize);
        if !(2048..=8192).contains(&bits)
            || modulus.last()? & 1 == 0
            || exponent.last()? & 1 == 0
            || (exponent.len() == 1 && exponent[0] < 3)
        {
            return None;
        }
        let key = Rsa::from_public_components(
            BigNum::from_slice(modulus).ok()?,
            BigNum::from_slice(exponent).ok()?,
        )
        .ok()?;
        PKey::from_rsa(key).ok()?
    } else {
        let curve = CertificateCurve::for_algorithm(algorithm)?;
        if reader.string(16)? != curve.name.as_bytes() {
            return None;
        }
        let point = reader.string(curve.point_bytes)?;
        if point.len() != curve.point_bytes || point.first() != Some(&4) {
            return None;
        }
        let group = EcGroup::from_curve_name(curve.nid).ok()?;
        let mut context = BigNumContext::new().ok()?;
        let point = EcPoint::from_bytes(&group, point, &mut context).ok()?;
        let key = EcKey::from_public_key(&group, &point).ok()?;
        key.check_key().ok()?;
        PKey::from_ec_key(key).ok()?
    };
    reader.finished().then(|| {
        (
            PublicKey {
                algorithm: algorithm.into(),
                blob: blob.into(),
            },
            key,
        )
    })
}

fn certificate_options(bytes: &[u8], extensions: bool) -> Option<()> {
    let mut reader = SshBlob::new(bytes);
    let mut previous: Option<&str> = None;
    let mut count = 0;
    while !reader.finished() {
        count += 1;
        if count > 128 {
            return None;
        }
        let name = reader.name(128)?;
        if previous.is_some_and(|previous| previous.as_bytes() >= name.as_bytes()) {
            return None;
        }
        let value = reader.string(MAX_CERTIFICATE_BLOB_BYTES)?;
        if extensions
            && matches!(
                name,
                "permit-X11-forwarding"
                    | "permit-agent-forwarding"
                    | "permit-port-forwarding"
                    | "permit-pty"
                    | "permit-user-rc"
                    | "no-touch-required"
            )
            && !value.is_empty()
        {
            return None;
        }
        previous = Some(name);
    }
    Some(())
}

fn verify_certificate_signature(
    signature: &[u8],
    message: &[u8],
    authority: &PublicKey,
    key: &PKey<Public>,
) -> Option<()> {
    let mut reader = SshBlob::new(signature);
    let algorithm = reader.name(64)?;
    let payload = reader.string(1024)?;
    if !reader.finished() {
        return None;
    }
    let valid = if authority.algorithm == "ssh-ed25519" {
        if algorithm != authority.algorithm || payload.len() != 64 {
            return None;
        }
        Verifier::new_without_digest(key)
            .ok()?
            .verify_oneshot(payload, message)
            .ok()?
    } else if authority.algorithm == "ssh-rsa" {
        let digest = match algorithm {
            "rsa-sha2-256" => MessageDigest::sha256(),
            "rsa-sha2-512" => MessageDigest::sha512(),
            _ => return None,
        };
        let size = key.rsa().ok()?.size() as usize;
        if payload.is_empty() || payload.len() > size {
            return None;
        }
        let mut padded = vec![0; size - payload.len()];
        padded.extend_from_slice(payload);
        let mut verifier = Verifier::new(digest, key).ok()?;
        verifier.set_rsa_padding(Padding::PKCS1).ok()?;
        verifier.update(message).ok()?;
        verifier.verify(&padded).ok()?
    } else {
        if algorithm != authority.algorithm {
            return None;
        }
        let curve = CertificateCurve::for_algorithm(algorithm)?;
        let mut components = SshBlob::new(payload);
        let r = BigNum::from_slice(components.positive_mpint(curve.scalar_bytes)?).ok()?;
        let s = BigNum::from_slice(components.positive_mpint(curve.scalar_bytes)?).ok()?;
        if !components.finished() {
            return None;
        }
        let signature = EcdsaSig::from_private_components(r, s)
            .ok()?
            .to_der()
            .ok()?;
        let mut verifier = Verifier::new(curve.digest, key).ok()?;
        verifier.update(message).ok()?;
        verifier.verify(&signature).ok()?
    };
    valid.then_some(())
}

impl UserCertificate {
    fn parse_file(text: &str) -> Option<Self> {
        if text.len() > 64 * 1024 {
            return None;
        }
        let mut lines = text.lines().filter(|line| !line.trim().is_empty());
        let line = lines.next()?;
        if lines.next().is_some() {
            return None;
        }
        let key = PublicKey::parse_line(line)?;
        // Require one canonical public certificate encoding, not alternate
        // base64 spellings or an authorized_keys option prefix.
        if line.split_whitespace().nth(1)? != STANDARD.encode(&key.blob) {
            return None;
        }
        Self::from_blob(key.blob)
    }

    fn from_blob(blob: Vec<u8>) -> Option<Self> {
        if blob.is_empty() || blob.len() > MAX_CERTIFICATE_BLOB_BYTES {
            return None;
        }
        let mut reader = SshBlob::new(&blob);
        let algorithm = reader.name(64)?;
        let raw = algorithm.strip_suffix("-cert-v01@openssh.com")?;
        if raw != "ssh-ed25519"
            && raw != "ssh-rsa"
            && CertificateCurve::for_algorithm(raw).is_none()
        {
            return None;
        }
        reader.string(MAX_CERTIFICATE_BLOB_BYTES)?; // nonce, authenticated below
        let mut subject = Vec::new();
        let mut append = |value: &[u8]| {
            subject.extend_from_slice(&(value.len() as u32).to_be_bytes());
            subject.extend_from_slice(value);
        };
        append(raw.as_bytes());
        match raw {
            "ssh-ed25519" => append(reader.string(32)?),
            "ssh-rsa" => {
                append(reader.string(9)?);
                append(reader.string(1025)?);
            }
            _ => {
                append(reader.string(16)?);
                append(reader.string(133)?);
            }
        }
        let (subject, _) = certificate_public_key(&subject)?;
        reader.u64()?; // serial is signed opaque metadata
        if reader.u32()? != 1 {
            return None; // host certificates never become user identities
        }
        reader.string(1024)?; // key ID is not a trusted display label
        let mut principal_bytes = SshBlob::new(reader.string(MAX_CERTIFICATE_BLOB_BYTES)?);
        let mut principals = BTreeSet::new();
        while !principal_bytes.finished() {
            if principals.len() == 128 {
                return None;
            }
            let principal = std::str::from_utf8(principal_bytes.string(256)?).ok()?;
            // Foundation's CharacterSet.controlCharacters includes both Cc
            // and Cf. Reject the same controls/format characters so a signed
            // certificate cannot produce a document the phone cannot parse.
            if principal.is_empty()
                || principal
                    .chars()
                    .any(|character| character.is_control() || character.is_format())
                || !principals.insert(principal)
            {
                return None;
            }
        }
        let valid_after = reader.u64()?;
        if valid_after >= reader.u64()? {
            return None;
        }
        certificate_options(reader.string(MAX_CERTIFICATE_BLOB_BYTES)?, false)?;
        certificate_options(reader.string(MAX_CERTIFICATE_BLOB_BYTES)?, true)?;
        if !reader.string(MAX_CERTIFICATE_BLOB_BYTES)?.is_empty() {
            return None;
        }
        let (authority, key) = certificate_public_key(reader.string(1053)?)?;
        let signed = &blob[..reader.offset];
        let signature = reader.string(1044)?;
        if !reader.finished() {
            return None;
        }
        verify_certificate_signature(signature, signed, &authority, &key)?;
        Some(Self { blob, subject })
    }

    fn fingerprint(&self) -> String {
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
    certificates: Vec<UserCertificate>,
    identity_policy_complete: bool,
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
            certificates: Vec::new(),
            identity_policy_complete: true,
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
            let mut certificates = Vec::new();
            let mut identity_policy_complete = true;
            if authentication.methods.contains(&"publickey") {
                let (paths, incomplete) = resolver.certificate_files(&options);
                if incomplete || paths.len() > MAX_PUBLIC_KEY_FILES {
                    identity_policy_complete = false;
                    note(notes, "CertificateFile requires one supported absolute or ~/ reference per declaration within the 256-file source budget; no partial identity list was exported and this route remains blocked".into());
                } else {
                    for path in paths {
                        let certificate = match sources.public_certificates.get(&path) {
                            Some(KnownHostsSource::Present(text)) => {
                                UserCertificate::parse_file(text)
                            }
                            _ => None,
                        };
                        match certificate {
                            Some(certificate) => {
                                if !certificates.iter().any(|candidate: &UserCertificate| {
                                    candidate.blob == certificate.blob
                                }) {
                                    certificates.push(certificate);
                                    if certificates.len() > MAX_ROUTE_IDENTITIES {
                                        identity_policy_complete = false;
                                        note(notes, "the phone offers at most eight distinct certificate identities per route; no partial identity list was exported and this route remains blocked".into());
                                        break;
                                    }
                                }
                            }
                            None => {
                                identity_policy_complete = false;
                                note(notes, format!(
                                    "CertificateFile {} is missing, unreadable, over budget, unsupported or invalid (including its CA signature); no partial identity list was exported and this route remains blocked",
                                    path.display()
                                ));
                                break;
                            }
                        }
                    }
                }
                if !identity_policy_complete {
                    certificates.clear();
                    unresolved_policy = true;
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
                "refuseconnection",
            ] {
                if let Some(value) = options.get(keyword) {
                    let disabled = match keyword {
                        "revokedhostkeys" | "knownhostscommand" => {
                            value.eq_ignore_ascii_case("none")
                        }
                        "verifyhostkeydns"
                        | "checkhostip"
                        | "canonicalizehostname"
                        | "refuseconnection" => value.eq_ignore_ascii_case("no"),
                        // `^` puts the listed algorithms first and removes
                        // none of OpenSSH's defaults: no constraint for the
                        // phone to enforce. macOS sets Ciphers, KexAlgorithms
                        // and MACs this way in /etc/ssh/crypto.conf
                        // (ft-5qgrk). `+`, `-` and a full list still block.
                        "hostkeyalgorithms"
                        | "casignaturealgorithms"
                        | "pubkeyacceptedalgorithms"
                        | "pubkeyacceptedkeytypes"
                        | "kexalgorithms"
                        | "ciphers"
                        | "macs" => value.starts_with('^'),
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
                certificates,
                identity_policy_complete,
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
    certificate_bytes: usize,
    trusts: BTreeMap<String, Value>,
    trust_scopes: BTreeMap<String, (String, u16)>,
    identity_files: BTreeMap<String, PathBuf>,
}

struct CredentialCandidate<'a> {
    id: String,
    key: PublicKey,
    certificate: Option<&'a UserCertificate>,
    identity: Option<&'a PathBuf>,
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
        // Admit the complete ordered candidate set before publishing any
        // credential or private-file hint. A phone limit is not permission to
        // replace the source authentication policy with its usable prefix.
        let mut raw_candidates: Vec<CredentialCandidate<'_>> = Vec::new();
        let mut identities_complete = endpoint.identity_policy_complete;
        let defaults = default_identities(context);
        for identity in &endpoint.identities {
            let public = public_key_path(identity);
            if sources.unavailable_public_keys.contains(&public) {
                identities_complete = false;
                notes.push(ExportNote {
                    machine: machine.id.clone(),
                    route: Some(route_name.to_string()),
                    message: format!(
                        "public identity source {} could not be read safely or exceeded the snapshot budget; no partial identity list was exported and this route remains blocked",
                        public.display()
                    ),
                });
                break;
            }
            let public_text = sources.public_keys.get(&public);
            let key = match public_text.and_then(|text| {
                let mut lines = text.lines().filter(|line| !line.trim().is_empty());
                let key = PublicKey::parse_line(lines.next()?);
                if lines.next().is_some() {
                    None
                } else {
                    key
                }
            }) {
                Some(key) => key,
                // Disk snapshots establish absent default identities only
                // when neither the private file nor its sidecar exists.
                None if public_text.is_none() && defaults.contains(identity) => continue,
                None => {
                    identities_complete = false;
                    notes.push(ExportNote {
                        machine: machine.id.clone(),
                        route: Some(route_name.to_string()),
                        message: format!(
                            "no readable public key at {}; no partial identity list was exported and this route remains blocked",
                            public.display()
                        ),
                    });
                    break;
                }
            };
            let fingerprint = key.fingerprint();
            let id = format!("c.{}", short_digest(&fingerprint));
            if raw_candidates.iter().any(|candidate| candidate.id == id) {
                continue;
            }
            if raw_candidates.len() == MAX_ROUTE_IDENTITIES {
                identities_complete = false;
                notes.push(ExportNote {
                    machine: machine.id.clone(),
                    route: Some(route_name.to_string()),
                    message: format!(
                        "the phone offers at most {} distinct identities per route; no partial identity list was exported and this route remains blocked; select a smaller identity policy",
                        MAX_ROUTE_IDENTITIES
                    ),
                });
                break;
            }
            raw_candidates.push(CredentialCandidate {
                id,
                key,
                certificate: None,
                identity: Some(identity),
            });
        }
        // OpenSSH offers explicitly configured certificates before file keys.
        // A certificate does not invent a raw fallback or a private filename:
        // only a separately configured, exactly matching public identity can
        // supply the companion's private-file hint for this certificate.
        let mut candidates = Vec::new();
        for certificate in &endpoint.certificates {
            let identity = raw_candidates
                .iter()
                .find(|candidate| candidate.key == certificate.subject)
                .and_then(|candidate| candidate.identity);
            candidates.push(CredentialCandidate {
                id: format!("c.cert.{}", short_digest(&certificate.fingerprint())),
                key: certificate.subject.clone(),
                certificate: Some(certificate),
                identity,
            });
        }
        candidates.extend(raw_candidates);
        if candidates.len() > MAX_ROUTE_IDENTITIES {
            identities_complete = false;
            notes.push(ExportNote {
                machine: machine.id.clone(),
                route: Some(route_name.to_string()),
                message: format!(
                    "the combined certificate and raw-key policy exceeds the phone's {}-candidate limit; no partial identity list was exported and this route remains blocked",
                    MAX_ROUTE_IDENTITIES
                ),
            });
        }
        if !identities_complete {
            candidates.clear();
        }
        let mut credential_ids = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if !self.credentials.contains_key(&candidate.id) {
                let mut credential = json!({
                    "id": candidate.id,
                    "algorithm": candidate.key.algorithm,
                    "public_fingerprint": candidate.key.fingerprint(),
                    "requirement": "matching_private_key",
                    "private_material": "not_included",
                });
                if let Some(certificate) = candidate.certificate {
                    credential["user_certificate"] = json!(STANDARD.encode(&certificate.blob));
                    credential["required_capabilities"] = json!([USER_CERTIFICATE_CAPABILITY]);
                    self.certificate_bytes += certificate.blob.len();
                }
                self.credentials.insert(candidate.id.clone(), credential);
            }
            if let Some(identity) = candidate.identity {
                self.identity_files
                    .entry(candidate.id.clone())
                    .or_insert_with(|| {
                        if identity.extension().is_some_and(|ext| ext == "pub") {
                            identity.with_extension("")
                        } else {
                            identity.clone()
                        }
                    });
            }
            credential_ids.push(candidate.id);
        }
        if identities_complete
            && credential_ids.is_empty()
            && endpoint.authentication_methods.contains(&"publickey")
        {
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
        if !identities_complete {
            record["required_capabilities"] = json!([UNRESOLVED_POLICY]);
        }
        record
    }
}

/// The phone's workspace for the inventory, or for one group of it. Its
/// revision identifies the resolved public configuration, not just the raw
/// inventory file: SSH policy, trust, identities and external theme changes
/// must change the witness even when `machines.toml` itself is unchanged.
pub fn export(
    inventory: &MachineInventory,
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
    let machine_count = machines.len();
    let route_count = routes.len();
    anyhow::ensure!(
        records.credentials.len() <= 256,
        "the complete workspace exceeds the phone's 256-credential limit; select a smaller group"
    );
    anyhow::ensure!(
        records.certificate_bytes <= MAX_EXPORTED_CERTIFICATE_BYTES,
        "the complete workspace exceeds the phone's 1 MiB certificate limit; select a smaller group"
    );
    let mut document = json!({
        "format": "ftworkspace",
        "version": 1,
        "required_capabilities": [],
        "source": {
            "exporter_id": EXPORTER_ID,
            "kind": "machines_toml",
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
    // Hash exactly the public projection, before adding this self-reference
    // and the fresh per-export bundle identity. Object keys are recursively
    // sorted even if another workspace crate enables serde_json's
    // preserve_order feature; array order carries route/group semantics.
    // Private paths and portability diagnostics stay in the local companion
    // envelope. Neither they nor raw source comments enter this public witness.
    document.sort_all_objects();
    let mut digest = Sha256::new();
    digest.update(b"frankenterm.mobile-export.public-configuration.v1\0");
    digest.update(serde_json::to_vec(&document)?);
    let revision = digest
        .finalize()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    document["source"]["configuration_revision"] = json!(format!("sha256:{}", revision));
    document["bundle_id"] = json!(format!(
        "b.{}",
        short_digest(&format!("{}:{}:{}", revision, nanos, std::process::id()))
    ));
    document.sort_all_objects();
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
    let export = export(&inventory, None, &sources, &context)?;
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

    fn certificate_fixtures() -> Value {
        // Independent OpenSSH certificates shared with the Swift parser and
        // native authentication tests. All private fixture keys are synthetic.
        serde_json::from_str(include_str!(
            "../../../fixtures/ios-airgap/ssh-user-certificates.json"
        ))
        .unwrap()
    }

    fn signed_test_certificate(principals: &[&str], serial: u64, nonce_bytes: usize) -> String {
        use openssl::sign::Signer;

        fn string(output: &mut Vec<u8>, value: &[u8]) {
            output.extend_from_slice(&(value.len() as u32).to_be_bytes());
            output.extend_from_slice(value);
        }
        // Deliberately public test seed, never an authorized identity.
        let key = PKey::private_key_from_raw_bytes(&[0x42; 32], Id::ED25519).unwrap();
        let point = key.raw_public_key().unwrap();
        let mut public = Vec::new();
        string(&mut public, b"ssh-ed25519");
        string(&mut public, &point);
        let mut blob = Vec::new();
        string(&mut blob, b"ssh-ed25519-cert-v01@openssh.com");
        string(&mut blob, &vec![0x27; nonce_bytes]);
        string(&mut blob, &point);
        blob.extend_from_slice(&serial.to_be_bytes());
        blob.extend_from_slice(&1u32.to_be_bytes());
        string(&mut blob, b"public synthetic fixture");
        let mut names = Vec::new();
        for principal in principals {
            string(&mut names, principal.as_bytes());
        }
        string(&mut blob, &names);
        blob.extend_from_slice(&1_577_836_800u64.to_be_bytes());
        blob.extend_from_slice(&2_524_608_000u64.to_be_bytes());
        string(&mut blob, b""); // critical options
        let mut extensions = Vec::new();
        for name in [
            b"permit-port-forwarding".as_slice(),
            b"permit-pty".as_slice(),
        ] {
            string(&mut extensions, name);
            string(&mut extensions, b"");
        }
        string(&mut blob, &extensions);
        string(&mut blob, b""); // reserved
        string(&mut blob, &public);
        let signature = Signer::new_without_digest(&key)
            .unwrap()
            .sign_oneshot_to_vec(&blob)
            .unwrap();
        let mut envelope = Vec::new();
        string(&mut envelope, b"ssh-ed25519");
        string(&mut envelope, &signature);
        string(&mut blob, &envelope);
        format!("ssh-ed25519-cert-v01@openssh.com {}", STANDARD.encode(blob))
    }

    #[test]
    fn certificates_verify_independent_openssh_families_and_exact_subjects() {
        let fixtures = certificate_fixtures();
        for fixture in fixtures["certificates"]
            .as_array()
            .unwrap()
            .iter()
            .chain(std::iter::once(&fixtures["largeCertificate"]))
        {
            let certificate = UserCertificate::parse_file(fixture["certificate"].as_str().unwrap())
                .unwrap_or_else(|| panic!("{}", fixture["id"]));
            let public = PublicKey::parse_line(fixture["publicKey"].as_str().unwrap()).unwrap();
            assert_eq!(certificate.subject, public, "{}", fixture["id"]);
            let mut nonce_tampered = certificate.blob.clone();
            let mut reader = SshBlob::new(&certificate.blob);
            reader.string(64).unwrap();
            nonce_tampered[reader.offset + 4] ^= 1;
            let mut signature_tampered = certificate.blob.clone();
            *signature_tampered.last_mut().unwrap() ^= 1;
            let mut trailing = certificate.blob.clone();
            trailing.push(0);
            for invalid in [
                nonce_tampered,
                signature_tampered,
                trailing,
                certificate.blob[..certificate.blob.len() - 1].to_vec(),
                public.blob,
            ] {
                assert!(
                    UserCertificate::from_blob(invalid).is_none(),
                    "{}",
                    fixture["id"]
                );
            }
        }
        // Restriction/time policy remains in the signed bytes for the phone;
        // passive export does not erase a valid but currently unusable cert.
        for name in [
            "unicodeCertificate",
            "wildcardCertificate",
            "restrictedCertificate",
            "withoutPTYCertificate",
            "withoutForwardingCertificate",
        ] {
            assert!(
                UserCertificate::parse_file(fixtures[name].as_str().unwrap()).is_some(),
                "{}",
                name
            );
        }
        assert!(
            UserCertificate::parse_file(fixtures["hostCertificate"].as_str().unwrap()).is_none()
        );
        assert!(UserCertificate::from_blob(vec![0; MAX_CERTIFICATE_BLOB_BYTES + 1]).is_none());
        let line = fixtures["certificates"][0]["certificate"].as_str().unwrap();
        assert!(UserCertificate::parse_file(&format!("{}\n{}", line, line)).is_none());
        assert!(UserCertificate::parse_file(&line.replacen("ssh-ed25519", "ssh-rsa", 1)).is_none());
    }

    #[test]
    fn signed_certificate_principals_share_the_phones_unicode_and_count_bounds() {
        for principals in [vec!["operator"], vec!["e\u{301}"], Vec::new()] {
            assert!(
                UserCertificate::parse_file(&signed_test_certificate(&principals, 1, 32)).is_some()
            );
        }
        for principals in [
            vec![""],
            vec!["same", "same"],
            vec!["account\n"],
            vec!["account\u{200d}"],
            vec!["account\u{ad}"],
            vec!["account\u{e0001}"],
        ] {
            assert!(
                UserCertificate::parse_file(&signed_test_certificate(&principals, 1, 32)).is_none()
            );
        }
        let names: Vec<String> = (0..129).map(|index| format!("user{}", index)).collect();
        let principals: Vec<&str> = names.iter().map(String::as_str).collect();
        assert!(
            UserCertificate::parse_file(&signed_test_certificate(&principals[..128], 1, 32))
                .is_some()
        );
        assert!(
            UserCertificate::parse_file(&signed_test_certificate(&principals, 1, 32)).is_none()
        );
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
            unavailable_public_keys: BTreeSet::new(),
            public_certificates: BTreeMap::new(),
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
        let export = export(&inventory, None, &sources(Some(config)), &context()).unwrap();
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
        export(&inventory, None, sources, &context())
    }

    #[test]
    fn public_revision_is_stable_across_exports_and_local_source_relocation() {
        let baseline = jump_export(&jump_sources()).unwrap();
        let repeated = jump_export(&jump_sources()).unwrap();
        assert_eq!(
            baseline.document["source"]["configuration_revision"],
            repeated.document["source"]["configuration_revision"]
        );

        let mut relocated = jump_sources();
        relocated.config = Some(format!(
            "# PRIVATE_SOURCE_SENTINEL\n{}",
            JUMP_CONFIG.replace(
                "IdentityFile ~/.ssh/app",
                "IdentityFile ~/.ssh/renamed-private-key"
            )
        ));
        relocated.public_keys.insert(
            context().home.join(".ssh/renamed-private-key.pub"),
            fake_key(4),
        );
        relocated.public_keys.insert(
            context().home.join(".ssh/unused.pub"),
            "UNSELECTED_SOURCE_SENTINEL".into(),
        );
        let annotated = format!("# PRIVATE_INVENTORY_SENTINEL\n{}", JUMP_INVENTORY);
        let inventory = MachineInventory::parse(&annotated, &context()).unwrap();
        let moved = export(&inventory, None, &relocated, &context()).unwrap();
        assert_ne!(baseline.identity_files, moved.identity_files);
        assert_eq!(baseline.document["workspace"], moved.document["workspace"]);
        assert_eq!(
            baseline.document["source"]["configuration_revision"],
            moved.document["source"]["configuration_revision"],
            "local private-file mappings and unexported text are not public configuration"
        );
        let public = serde_json::to_string(&moved.document).unwrap();
        for forbidden in [
            "/home/me/",
            "renamed-private-key",
            "PRIVATE_SOURCE_SENTINEL",
            "PRIVATE_INVENTORY_SENTINEL",
            "UNSELECTED_SOURCE_SENTINEL",
            "identity_files",
        ] {
            assert!(!public.contains(forbidden), "{}", forbidden);
        }
    }

    #[test]
    fn public_revision_changes_with_resolved_endpoints_hops_and_authentication() {
        let baseline = jump_export(&jump_sources()).unwrap();
        for (before, after) in [
            ("HostName service.internal", "HostName changed.internal"),
            ("Port 2224", "Port 2225"),
            ("User deploy", "User replacement"),
            (
                "ProxyJump first,second,third",
                "ProxyJump third,second,first",
            ),
            (
                "Host app\n",
                "Host app\n PreferredAuthentications password,publickey\n",
            ),
            (
                "Host first\n",
                "Host first\n PreferredAuthentications publickey,password\n",
            ),
            ("Host app\n", "Host app\n Ciphers aes256-ctr\n"),
        ] {
            let mut changed = jump_sources();
            changed.config = Some(JUMP_CONFIG.replace(before, after));
            let result = jump_export(&changed).unwrap();
            assert_ne!(
                baseline.document["workspace"], result.document["workspace"],
                "{}",
                after
            );
            assert_ne!(
                baseline.document["source"]["configuration_revision"],
                result.document["source"]["configuration_revision"],
                "unchanged inventory bytes cannot hide effective SSH changes: {}",
                after
            );
        }
    }

    #[test]
    fn public_revision_changes_with_pins_revocations_identities_and_missing_policy() {
        let baseline = jump_export(&jump_sources()).unwrap();
        for change in ["pin", "revocation", "identity", "unavailable trust"] {
            let mut changed = jump_sources();
            match change {
                "pin" => supply(
                    &mut changed,
                    "/trust/app",
                    format!("[service.internal]:2224 {}\n", fake_key(71)),
                ),
                "revocation" => supply(
                    &mut changed,
                    "/trust/app",
                    format!("@revoked [service.internal]:2224 {}\n", fake_key(44)),
                ),
                "identity" => {
                    changed
                        .public_keys
                        .insert(context().home.join(".ssh/app.pub"), fake_key(72));
                }
                "unavailable trust" => {
                    changed
                        .known_hosts
                        .insert(PathBuf::from("/trust/app"), KnownHostsSource::Unavailable);
                }
                _ => unreachable!(),
            }
            let result = jump_export(&changed).unwrap();
            assert_ne!(
                baseline.document["source"]["configuration_revision"],
                result.document["source"]["configuration_revision"],
                "{}",
                change
            );
        }
    }

    #[test]
    fn public_revision_covers_resolved_themes_and_group_order_and_selection() {
        let inventory = MachineInventory::parse(INVENTORY, &context()).unwrap();
        let sources = sources(Some("Host css\n User ubuntu\n HostName css.example\n"));
        let baseline = export(&inventory, None, &sources, &context()).unwrap();
        for change in ["palette", "indexed palette", "font", "group order"] {
            let mut changed = inventory.clone();
            match change {
                "palette" => {
                    changed.machines[0].theme.palette.background =
                        crate::machines::Rgba::parse("#123456").unwrap();
                }
                "indexed palette" => {
                    changed.machines[0]
                        .theme
                        .palette
                        .indexed
                        .insert(201, crate::machines::Rgba::parse("#654321").unwrap());
                }
                "font" => {
                    changed.machines[0].theme.font = Some(crate::machines::ThemeFont {
                        family: "Portable Mono".into(),
                        size: Some(14.5),
                    });
                }
                "group order" => changed.groups[0].machines.reverse(),
                _ => unreachable!(),
            }
            let result = export(&changed, None, &sources, &context()).unwrap();
            assert_ne!(
                baseline.document["source"]["configuration_revision"],
                result.document["source"]["configuration_revision"],
                "{}",
                change
            );
        }
        let mut selection = inventory.clone();
        let mut group = inventory.groups[0].clone();
        group.id = "only_css".into();
        group.machines = vec!["css".into()];
        selection.groups.push(group);
        let selected = export(&selection, Some("only_css"), &sources, &context()).unwrap();
        assert_eq!(
            selected.document["workspace"]["groups"][0]["machine_order"],
            json!(["m.css"])
        );
        assert_ne!(
            baseline.document["source"]["configuration_revision"],
            selected.document["source"]["configuration_revision"]
        );
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

    fn exported_credential<'a>(export: &'a MobileExport, id: &Value) -> &'a Value {
        export.document["workspace"]["credentials"]
            .as_array()
            .unwrap()
            .iter()
            .find(|credential| &credential["id"] == id)
            .unwrap()
    }

    #[test]
    fn certificate_exports_preserve_exact_offers_subject_binding_and_raw_fallback() {
        let fixtures = certificate_fixtures();
        for fixture in fixtures["certificates"]
            .as_array()
            .unwrap()
            .iter()
            .chain(std::iter::once(&fixtures["largeCertificate"]))
        {
            let mut sources = authentication_sources("CertificateFile ~/.ssh/user-cert.pub");
            sources.public_keys.insert(
                context().home.join(".ssh/trj_ed25519.pub"),
                fixture["publicKey"].as_str().unwrap().into(),
            );
            sources.public_certificates.insert(
                context().home.join(".ssh/user-cert.pub"),
                KnownHostsSource::Present(fixture["certificate"].as_str().unwrap().into()),
            );
            let exported = jump_export(&sources).unwrap();
            let route = exported_route(&exported, "r.app.ssh");
            let refs = route["credential_refs"].as_array().unwrap();
            assert_eq!(refs.len(), 2, "{}", fixture["id"]);
            assert!(route["required_capabilities"].is_null());
            assert!(route_trust(&exported, "r.app.ssh")["required_capabilities"].is_null());
            let certificate = exported_credential(&exported, &refs[0]);
            let raw = exported_credential(&exported, &refs[1]);
            let public = PublicKey::parse_line(fixture["publicKey"].as_str().unwrap()).unwrap();
            assert_eq!(certificate["algorithm"], public.algorithm);
            assert_eq!(certificate["public_fingerprint"], public.fingerprint());
            assert_eq!(
                certificate["required_capabilities"],
                json!([USER_CERTIFICATE_CAPABILITY])
            );
            assert_eq!(
                certificate["user_certificate"],
                fixture["certificate"]
                    .as_str()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
            );
            assert_eq!(raw["public_fingerprint"], public.fingerprint());
            assert!(raw["user_certificate"].is_null());
            for id in refs {
                assert_eq!(
                    exported.identity_files.get(id.as_str().unwrap()),
                    Some(&context().home.join(".ssh/trj_ed25519"))
                );
            }
            assert_eq!(
                exported_route(&exported, "r.app.mux")["credential_refs"],
                route["credential_refs"]
            );
            assert!(!exported.document.to_string().contains("PRIVATE KEY"));
        }
    }

    #[test]
    fn certificate_only_and_unrelated_keys_never_invent_private_bindings() {
        let certificate = signed_test_certificate(&["operator"], 1, 32);
        for identity in ["none", "~/.ssh/trj_ed25519"] {
            let mut sources = sources(Some(&format!(
                "Host app\n HostName css.example\n User operator\n IdentityFile {}\n CertificateFile ~/.ssh/user-cert.pub\n GlobalKnownHostsFile none\n", identity
            )));
            sources.public_certificates.insert(
                context().home.join(".ssh/user-cert.pub"),
                KnownHostsSource::Present(certificate.clone()),
            );
            let first = jump_export(&sources).unwrap();
            let refs = exported_route(&first, "r.app.ssh")["credential_refs"]
                .as_array()
                .unwrap();
            assert_eq!(refs.len(), if identity == "none" { 1 } else { 2 });
            assert!(!first.identity_files.contains_key(refs[0].as_str().unwrap()));
            assert_eq!(
                first.identity_files.len(),
                if identity == "none" { 0 } else { 1 }
            );
            sources.public_certificates.insert(
                context().home.join(".ssh/user-cert.pub"),
                KnownHostsSource::Present(signed_test_certificate(&["operator"], 2, 32)),
            );
            let renewed = jump_export(&sources).unwrap();
            let renewed_refs = exported_route(&renewed, "r.app.ssh")["credential_refs"]
                .as_array()
                .unwrap();
            assert_ne!(
                refs[0], renewed_refs[0],
                "a renewal changes the exact certificate binding"
            );
            assert_eq!(
                refs[1..],
                renewed_refs[1..],
                "independent raw identities stay stable"
            );
            assert_ne!(
                first.document["source"]["configuration_revision"],
                renewed.document["source"]["configuration_revision"]
            );
        }
    }

    #[test]
    fn certificate_sources_are_additive_in_host_order_and_internal_slots_are_not_source_policy() {
        let mut sources = sources(Some(
            "CertificateFile ~/.ssh/first.pub\nHost other\n CertificateFile /must-not-read\nHost app\n HostName css.example\n User operator\n IdentityFile none\n CertificateFile none\n CertificateFile \"~/.ssh/second cert.pub\"\n CertificateFile ~/.ssh/first.pub\nHost *\n CertificateFile ~/.ssh/third.pub\n GlobalKnownHostsFile none\n"
        ));
        sources.system_config =
            KnownHostsSource::Present("CertificateFile ~/.ssh/fourth.pub\n".into());
        let mut expected = Vec::new();
        for (index, file) in ["first.pub", "second cert.pub", "third.pub", "fourth.pub"]
            .iter()
            .enumerate()
        {
            let certificate = signed_test_certificate(&["operator"], index as u64, 32);
            expected.push(certificate.split_whitespace().nth(1).unwrap().to_owned());
            sources.public_certificates.insert(
                context().home.join(".ssh").join(file),
                KnownHostsSource::Present(certificate),
            );
        }
        let exported = jump_export(&sources).unwrap();
        let route = exported_route(&exported, "r.app.ssh");
        let actual: Vec<_> = route["credential_refs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| {
                exported_credential(&exported, id)["user_certificate"]
                    .as_str()
                    .unwrap()
            })
            .collect();
        assert_eq!(actual, expected);
        assert!(route_trust(&exported, "r.app.ssh")["required_capabilities"].is_null());
        let original = sources.config.clone().unwrap();
        for injection in [
            "frankenterm_certificate_1 yes",
            "FrankenTerm_Required_Certificate no",
        ] {
            let mut injected = original.clone();
            injected.push_str(injection);
            injected.push('\n');
            sources.config = Some(injected);
            assert_eq!(
                route_trust(&jump_export(&sources).unwrap(), "r.app.ssh")["required_capabilities"],
                json!([UNRESOLVED_POLICY])
            );
        }
    }

    #[test]
    fn any_unresolved_certificate_blocks_the_complete_candidate_policy() {
        let certificate = signed_test_certificate(&["operator"], 1, 32);
        let mut corrupted = PublicKey::parse_line(&certificate).unwrap();
        *corrupted.blob.last_mut().unwrap() ^= 1;
        let corrupted = format!(
            "{} {}",
            corrupted.algorithm,
            STANDARD.encode(corrupted.blob)
        );
        for invalid in [
            None,
            Some(KnownHostsSource::Missing),
            Some(KnownHostsSource::Unavailable),
            Some(KnownHostsSource::Present("invalid".into())),
            Some(KnownHostsSource::Present(corrupted)),
        ] {
            let mut sources = authentication_sources(
                "CertificateFile ~/.ssh/first-cert.pub\n CertificateFile ~/.ssh/second-cert.pub",
            );
            sources.public_certificates.insert(
                context().home.join(".ssh/first-cert.pub"),
                KnownHostsSource::Present(certificate.clone()),
            );
            if let Some(invalid) = invalid {
                sources
                    .public_certificates
                    .insert(context().home.join(".ssh/second-cert.pub"), invalid);
            }
            let exported = jump_export(&sources).unwrap();
            assert!(exported.document["workspace"]["credentials"]
                .as_array()
                .unwrap()
                .is_empty());
            assert!(exported.identity_files.is_empty());
            for id in ["r.app.ssh", "r.app.mux"] {
                let route = exported_route(&exported, id);
                assert!(route["credential_refs"].as_array().unwrap().is_empty());
                assert_eq!(route["required_capabilities"], json!([UNRESOLVED_POLICY]));
            }
        }
        for reference in [
            "relative-cert.pub",
            "%d/.ssh/cert.pub",
            "${HOME}/.ssh/cert.pub",
            "~someone/.ssh/cert.pub",
            "~/.ssh/../cert.pub",
            "~/.ssh/a ~/.ssh/b",
        ] {
            let exported = jump_export(&authentication_sources(&format!(
                "CertificateFile {}",
                reference
            )))
            .unwrap();
            assert!(exported.document["workspace"]["credentials"]
                .as_array()
                .unwrap()
                .is_empty());
            assert_eq!(
                exported_route(&exported, "r.app.ssh")["required_capabilities"],
                json!([UNRESOLVED_POLICY])
            );
        }
        // Disabled public-key authentication never reads/offers a certificate
        // and cannot acquire a blocker from an unused certificate path.
        let exported = jump_export(&authentication_sources(
            "PubkeyAuthentication no\n CertificateFile /missing-cert",
        ))
        .unwrap();
        assert!(exported_route(&exported, "r.app.ssh")["required_capabilities"].is_null());
        assert!(route_trust(&exported, "r.app.ssh")["required_capabilities"].is_null());
    }

    #[test]
    fn certificate_candidate_limits_apply_after_blob_dedup_and_before_publication() {
        let certificates: Vec<_> = (0..9)
            .map(|serial| signed_test_certificate(&["operator"], serial, 32))
            .collect();
        for (count, distinct, raw, expected) in [
            (9, false, false, 1),
            (9, true, false, 0),
            (8, true, false, 8),
            (8, true, true, 0),
        ] {
            let mut config = format!("Host app\n HostName css.example\n User operator\n GlobalKnownHostsFile none\n IdentityFile {}\n",
                if raw { "~/.ssh/trj_ed25519" } else { "none" });
            for index in 0..count {
                config.push_str(&format!(" CertificateFile ~/.ssh/cert{}.pub\n", index));
            }
            let mut sources = sources(Some(&config));
            for index in 0..count {
                sources.public_certificates.insert(
                    context().home.join(format!(".ssh/cert{}.pub", index)),
                    KnownHostsSource::Present(
                        certificates[if distinct { index } else { 0 }].clone(),
                    ),
                );
            }
            let exported = jump_export(&sources).unwrap();
            assert_eq!(
                exported.document["workspace"]["credentials"]
                    .as_array()
                    .unwrap()
                    .len(),
                expected
            );
            for id in ["r.app.ssh", "r.app.mux"] {
                let route = exported_route(&exported, id);
                assert_eq!(route["credential_refs"].as_array().unwrap().len(), expected);
                assert_eq!(!route["required_capabilities"].is_null(), expected == 0);
            }
            assert!(
                exported.identity_files.is_empty(),
                "no raw prefix or guessed filename survives"
            );
        }
    }

    #[test]
    fn certificate_disk_budgets_keep_explicit_failures_without_private_probes() {
        let certificate = signed_test_certificate(&["operator"], 1, 32);
        for (count, padded_bytes, admitted) in [
            (256, 0, true),
            (257, 0, false),
            (32, 64 * 1024, true),
            (33, 64 * 1024, false),
            (1, 64 * 1024 + 1, false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let context = InventoryContext {
                directory: directory.path().to_path_buf(),
                home: directory.path().to_path_buf(),
            };
            std::fs::create_dir(context.home.join(".ssh")).unwrap();
            let mut config = "Host app\n HostName css.example\n User operator\n IdentityFile none\n UserKnownHostsFile none\n GlobalKnownHostsFile none\n".to_string();
            let mut text = certificate.clone();
            if padded_bytes > text.len() {
                text.push_str(&" ".repeat(padded_bytes - text.len()));
            }
            for index in 0..count {
                let name = format!("cert{:03}.pub", index);
                config.push_str(&format!(" CertificateFile ~/.ssh/{}\n", name));
                write_source(&context.home.join(".ssh").join(name), text.as_bytes());
            }
            write_source(&context.home.join(".ssh/config"), config);
            write_source(
                &context.home.join(".ssh/cert000"),
                b"PRIVATE-SIBLING-MUST-NOT-BE-READ",
            );
            write_source(
                &context.home.join(".ssh/unselected-cert.pub"),
                certificate.as_bytes(),
            );
            let inventory = MachineInventory::parse(JUMP_INVENTORY, &context).unwrap();
            let mut sources = SshSources::from_disk(&inventory, &context);
            // Only the test's selected user configuration is part of this
            // assertion; the host's system configuration is not its oracle.
            sources.system_config = KnownHostsSource::Missing;
            assert_eq!(sources.public_certificates.len(), count);
            let unavailable = sources
                .public_certificates
                .values()
                .filter(|source| matches!(source, KnownHostsSource::Unavailable))
                .count();
            assert_eq!(unavailable, usize::from(!admitted));
            let exported = export(&inventory, None, &sources, &context).unwrap();
            let route = exported_route(&exported, "r.app.ssh");
            assert_eq!(
                route["credential_refs"].as_array().unwrap().len(),
                usize::from(admitted)
            );
            assert_eq!(route["required_capabilities"].is_null(), admitted);
            assert!(exported.identity_files.is_empty());
            assert!(!companion_source(&exported, Path::new("/inventory"))
                .to_string()
                .contains("PRIVATE-SIBLING-MUST-NOT-BE-READ"));
        }
    }

    #[test]
    fn complete_workspace_certificate_bytes_obey_the_phone_manifest_limit() {
        let overhead = UserCertificate::parse_file(&signed_test_certificate(&["operator"], 0, 0))
            .unwrap()
            .blob
            .len();
        let nonce_bytes = MAX_CERTIFICATE_BLOB_BYTES - overhead;
        let mut config = String::new();
        let mut inventory = "version = 1\n".to_string();
        let mut sources = sources(None);
        for index in 0..33 {
            inventory.push_str(&format!(
                "[machines.m{0}]\n[[machines.m{0}.routes]]\nid = \"ssh\"\nssh = \"m{0}\"\n",
                index
            ));
            config.push_str(&format!(
                "Host m{0}\n HostName css.example\n User operator\n IdentityFile none\n GlobalKnownHostsFile none\n CertificateFile ~/.ssh/cert{0}.pub\n", index
            ));
            let certificate = signed_test_certificate(&["operator"], index, nonce_bytes);
            assert_eq!(
                UserCertificate::parse_file(&certificate)
                    .unwrap()
                    .blob
                    .len(),
                MAX_CERTIFICATE_BLOB_BYTES
            );
            sources.public_certificates.insert(
                context().home.join(format!(".ssh/cert{}.pub", index)),
                KnownHostsSource::Present(certificate),
            );
        }
        sources.config = Some(config);
        let inventory = MachineInventory::parse(&inventory, &context()).unwrap();
        let error = export(&inventory, None, &sources, &context())
            .err()
            .unwrap();
        assert!(error.to_string().contains("1 MiB certificate limit"));
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
        // This case models genuinely absent default identities. An explicitly
        // configured identity without its public witness remains unresolved.
        sources.config = Some(
            sources
                .config
                .unwrap()
                .replace(" IdentityFile ~/.ssh/trj_ed25519\n", ""),
        );
        sources.public_keys.clear();
        let export = jump_export(&sources).unwrap();
        assert!(export.identity_files.is_empty());
        assert!(route_trust(&export, "r.app.ssh")["required_capabilities"].is_null());
        for id in ["r.app.ssh", "r.app.mux"] {
            assert!(exported_route(&export, id)["required_capabilities"].is_null());
        }
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
        assert!(exported_route(&export, "r.app.ssh")["required_capabilities"].is_null());
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
        let export = export(&inventory, None, &sources, &context()).unwrap();
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
        let before = export(&inventory, None, &sources, &context()).unwrap();
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
        let selected = export(&inventory, Some("selected"), &sources, &context()).unwrap();
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
        let refused = export(&inventory, None, &sources, &context()).unwrap();
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
        let selected = export(&inventory, Some("selected"), &sources, &context()).unwrap();
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
        let complete = export(&inventory, None, &sources, &context()).unwrap();
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
        let result = export(&inventory, None, &sources, &context());
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
        let export = export(&inventory, None, &snapshot, &context).unwrap();
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
        let password_export = super::export(&inventory, None, &snapshot, &context).unwrap();
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
    fn disk_identity_budgets_leave_explicit_refusals_instead_of_partial_exports() {
        for (file_count, padded_bytes, blocked) in [
            (256, 0, false),
            (257, 0, true),
            (32, 65_536, false),
            (33, 65_536, true),
            (1, 65_537, true),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let home = directory.path().join("home");
            std::fs::create_dir_all(home.join(".ssh")).unwrap();
            let mut text = "version = 1\n[[machines.a.routes]]\nid = \"ssh\"\nhost = \"box\"\nuser = \"u\"\nidentity_files = [".to_string();
            for index in 0..file_count {
                text.push_str(&format!("\"~/.ssh/k{:03}\", ", index));
                // Every file names the same key, so this exercises snapshot
                // limits independently of the eight-distinct-candidate limit.
                let mut public = fake_key(1);
                if padded_bytes > public.len() {
                    public.push_str(&" ".repeat(padded_bytes - public.len()));
                }
                write_source(&home.join(format!(".ssh/k{:03}.pub", index)), public);
            }
            text.push_str("]\n");
            let context = InventoryContext {
                directory: directory.path().to_path_buf(),
                home,
            };
            let inventory = MachineInventory::parse(&text, &context).unwrap();
            let sources = SshSources::from_disk(&inventory, &context);
            assert_eq!(!sources.unavailable_public_keys.is_empty(), blocked);
            let export = export(&inventory, None, &sources, &context).unwrap();
            let route = exported_route(&export, "r.a.ssh");
            assert_eq!(
                route["required_capabilities"] == json!([UNRESOLVED_POLICY]),
                blocked
            );
            assert_eq!(
                route["credential_refs"].as_array().unwrap().len(),
                usize::from(!blocked)
            );
            assert_eq!(export.identity_files.len(), usize::from(!blocked));
            assert_eq!(
                export.document["workspace"]["credentials"]
                    .as_array()
                    .unwrap()
                    .len(),
                usize::from(!blocked)
            );
            assert_eq!(
                export
                    .notes
                    .iter()
                    .any(|note| note.message.contains("no partial identity list")),
                blocked,
                "{} source files, {} bytes each",
                file_count,
                padded_bytes
            );
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
        let export = export(&inventory, None, &sources, &context()).unwrap();
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
    fn a_route_admits_its_complete_identity_policy_or_blocks_without_a_prefix() {
        for (files, distinct, expected) in [(8, true, 8), (9, true, 0), (9, false, 1)] {
            let mut text = "version = 1\n[[machines.a.routes]]\nid = \"many\"\nhost = \"100.91.120.17\"\nuser = \"u\"\nidentity_files = [".to_string();
            let mut sources = sources(None);
            for index in 0..files {
                text.push_str(&format!("\"~/.ssh/k{}\", ", index));
                sources.public_keys.insert(
                    PathBuf::from(format!("/home/me/.ssh/k{}.pub", index)),
                    fake_key(if distinct { index } else { 1 }),
                );
            }
            text.push_str("]\n[[machines.a.routes]]\nid = \"mux\"\nkind = \"frankenterm_mux\"\nvia = \"many\"\nsocket = \"/run/frankenterm.sock\"\n");
            let inventory = MachineInventory::parse(&text, &context()).unwrap();
            let export = export(&inventory, None, &sources, &context()).unwrap();
            for id in ["r.a.many", "r.a.mux"] {
                let route = exported_route(&export, id);
                assert_eq!(route["credential_refs"].as_array().unwrap().len(), expected);
                assert_eq!(
                    route["required_capabilities"] == json!([UNRESOLVED_POLICY]),
                    expected == 0,
                    "{} files (distinct={}) on {}",
                    files,
                    distinct,
                    id
                );
            }
            assert_eq!(export.identity_files.len(), expected);
            assert_eq!(
                export.document["workspace"]["credentials"]
                    .as_array()
                    .unwrap()
                    .len(),
                expected,
                "refused candidates must not survive as unreferenced private-key requests"
            );
            assert_eq!(
                export
                    .notes
                    .iter()
                    .any(|note| note.message.contains("no partial identity list")),
                expected == 0
            );
        }
    }

    #[test]
    fn an_unavailable_identity_blocks_only_its_route_and_every_mux_copy() {
        for alias in ["app", "first"] {
            let mut sources = jump_sources();
            let unavailable = context().home.join(format!(".ssh/{}.pub", alias));
            // Unavailability wins even if an earlier snapshot entry survived.
            sources.unavailable_public_keys.insert(unavailable);
            let export = jump_export(&sources).unwrap();
            let blocked_host = if alias == "app" {
                "service.internal"
            } else {
                "outer.example"
            };
            let mut blocked_ids = Vec::new();
            for route in export.document["workspace"]["routes"].as_array().unwrap() {
                let blocked = route["host"] == blocked_host;
                assert_eq!(
                    route["credential_refs"].as_array().unwrap().len(),
                    usize::from(!blocked)
                );
                assert_eq!(
                    route["required_capabilities"] == json!([UNRESOLVED_POLICY]),
                    blocked
                );
                if blocked {
                    blocked_ids.push(route["id"].clone());
                }
            }
            if alias == "first" {
                for id in ["r.app.ssh", "r.app.mux"] {
                    assert!(exported_route(&export, id)["jump_routes"]
                        .as_array()
                        .unwrap()
                        .contains(&blocked_ids[0]));
                }
            } else {
                assert_eq!(blocked_ids.len(), 2, "SSH and mux share the refusal");
            }
            assert_eq!(export.identity_files.len(), 3);
            assert!(!export
                .identity_files
                .values()
                .any(|path| path == &context().home.join(format!(".ssh/{}", alias))));
        }
    }

    #[test]
    fn malformed_or_unresolved_sidecars_cannot_publish_other_identity_candidates() {
        let inventory = MachineInventory::parse(
            "version = 1\n[[machines.a.routes]]\nid = \"ssh\"\nhost = \"box\"\nuser = \"u\"\nidentity_files = [\"~/.ssh/trj_ed25519\", \"~/.ssh/second\"]\n",
            &context(),
        )
        .unwrap();
        for sidecar in [
            None,
            Some("not a public key".to_string()),
            Some(format!("{}\n{}\n", fake_key(1), fake_key(2))),
        ] {
            let mut sources = sources(None);
            if let Some(text) = sidecar {
                sources
                    .public_keys
                    .insert(context().home.join(".ssh/second.pub"), text);
            }
            let export = export(&inventory, None, &sources, &context()).unwrap();
            let route = exported_route(&export, "r.a.ssh");
            assert_eq!(route["credential_refs"], json!([]));
            assert_eq!(route["required_capabilities"], json!([UNRESOLVED_POLICY]));
            assert!(export.identity_files.is_empty());
            assert_eq!(export.document["workspace"]["credentials"], json!([]));
        }
    }

    #[test]
    fn a_missing_default_sidecar_is_not_evidence_that_its_private_identity_is_absent() {
        for state in ["absent", "private_only", "malformed_public"] {
            let directory = tempfile::tempdir().unwrap();
            let home = directory.path().join("home");
            std::fs::create_dir_all(home.join(".ssh")).unwrap();
            // The first default candidate is available. A later unresolved
            // default must prevent it from becoming a partial ready policy.
            write_source(&home.join(".ssh/id_ecdsa.pub"), fake_key(1));
            if state == "private_only" {
                write_source(&home.join(".ssh/id_ed25519"), "PRIVATE-MATERIAL-NOT-READ");
            } else if state == "malformed_public" {
                write_source(&home.join(".ssh/id_ed25519.pub"), "not a public key");
            }
            let context = InventoryContext {
                directory: directory.path().to_path_buf(),
                home,
            };
            let inventory = MachineInventory::parse(
                "version = 1\n[[machines.a.routes]]\nid = \"ssh\"\nhost = \"box\"\nuser = \"u\"\n",
                &context,
            )
            .unwrap();
            let sources = SshSources::from_disk(&inventory, &context);
            assert_eq!(
                sources
                    .unavailable_public_keys
                    .contains(&context.home.join(".ssh/id_ed25519.pub")),
                state == "private_only"
            );
            let export = export(&inventory, None, &sources, &context).unwrap();
            let route = exported_route(&export, "r.a.ssh");
            assert_eq!(
                route["required_capabilities"] == json!([UNRESOLVED_POLICY]),
                state != "absent"
            );
            assert_eq!(export.identity_files.len(), usize::from(state == "absent"));
            assert!(!serde_json::to_string(&companion_source(
                &export,
                Path::new("/machines.toml")
            ))
            .unwrap()
            .contains("PRIVATE-MATERIAL-NOT-READ"));
        }
    }

    #[test]
    fn missing_keys_and_host_keys_are_reported_and_a_group_scopes_the_workspace() {
        let inventory = MachineInventory::parse(INVENTORY, &context()).unwrap();
        let mut bare = sources(None);
        bare.public_keys.clear();
        bare.known_hosts.clear();
        let export = export(&inventory, Some("fleet"), &bare, &context()).unwrap();
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
        assert!(super::export(&inventory, Some("nope"), &bare, &context()).is_err());
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
        export(&inventory, None, sources, &context()).unwrap()
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
        let reversed = export(&inventory, None, &sources, &context()).unwrap();
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
            export(&inventory, Some("only_left"), sources, &context()).unwrap()
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
        let export = export(&inventory, None, &snapshot, &context).unwrap();
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
        let mut document = export(&inventory, None, &sources(Some(config)), &context())
            .unwrap()
            .document;
        // Independently computed with Python hashlib over the fixture's
        // sorted compact public JSON, excluding bundle_id and this revision,
        // prefixed by b"frankenterm.mobile-export.public-configuration.v1\0".
        // Only this witness changed; the cross-language workspace is intact.
        assert_eq!(
            document["source"]["configuration_revision"],
            "sha256:4b1ca6b6c22da221f0956ee064cd84c756ac08247c1f26597779fbfb37f8e8e6"
        );
        // The only per-run field.
        document["bundle_id"] = json!("b.fixture");
        let mut bytes = serde_json::to_vec_pretty(&document).unwrap();
        bytes.push(b'\n');
        matches_file("fixtures/ios-airgap/machines-export.ftworkspace", &bytes);
    }

    /// Include fixtures: file texts by path, and directory listings.
    #[derive(Default)]
    struct FixtureIncludes {
        files: BTreeMap<PathBuf, KnownHostsSource>,
        directories: BTreeMap<PathBuf, Vec<String>>,
    }

    impl IncludeFiles for FixtureIncludes {
        fn read(&self, path: &Path, limit: usize) -> KnownHostsSource {
            match self.files.get(path) {
                Some(KnownHostsSource::Present(text)) if text.len() > limit => {
                    KnownHostsSource::Unavailable
                }
                Some(source) => source.clone(),
                None => KnownHostsSource::Missing,
            }
        }

        fn names(&self, directory: &Path, limit: usize) -> Option<Vec<String>> {
            let names = self.directories.get(directory).cloned().unwrap_or_default();
            (names.len() <= limit).then_some(names)
        }

        fn is_directory(&self, path: &Path) -> Option<bool> {
            Some(
                self.directories.contains_key(path)
                    || self
                        .files
                        .keys()
                        .any(|file| file != path && file.starts_with(path))
                    || self.directories.keys().any(|dir| dir.starts_with(path)),
            )
        }
    }

    fn expand(text: &str, base: &str, files: &FixtureIncludes) -> String {
        let home = context().home;
        let mut budget = IncludeBudget::new();
        expand_includes(text, Path::new(base), &home, files, &mut budget, 0, true)
    }

    fn present(text: &str) -> KnownHostsSource {
        KnownHostsSource::Present(text.into())
    }

    #[test]
    fn stock_macos_system_configuration_leaves_alias_routes_ready() {
        // /etc/ssh as macOS ships it: one Include, a Host * block, and
        // crypto.conf's ^ preference lists.
        let mut files = FixtureIncludes::default();
        files.directories.insert(
            "/etc/ssh/ssh_config.d".into(),
            vec!["100-macos.conf".into()],
        );
        files.files.insert(
            "/etc/ssh/ssh_config.d/100-macos.conf".into(),
            present("Host *\n  SendEnv LANG LC_*\n  Include /etc/ssh/crypto.conf\n"),
        );
        files.files.insert(
            "/etc/ssh/crypto.conf".into(),
            present("Ciphers ^aes128-gcm@openssh.com,aes256-gcm@openssh.com\nKexAlgorithms ^ecdh-sha2-nistp256\nMACs ^hmac-sha2-256-etm@openssh.com,hmac-sha2-256\n"),
        );
        let system = expand(
            "# Comments only, then:\nInclude /etc/ssh/ssh_config.d/*\n",
            "/etc/ssh",
            &files,
        );
        assert_eq!(
            system,
            "# Comments only, then:\nHost *\n  SendEnv LANG LC_*\nCiphers ^aes128-gcm@openssh.com,aes256-gcm@openssh.com\nKexAlgorithms ^ecdh-sha2-nistp256\nMACs ^hmac-sha2-256-etm@openssh.com,hmac-sha2-256\nHost *\n"
        );

        let mut sources = sources(Some(
            "Host left right\n HostName box\n User u\n UserKnownHostsFile /trust/pins\n GlobalKnownHostsFile none\n",
        ));
        supply(
            &mut sources,
            "/trust/pins",
            format!("box ssh-ed25519 {}\n", ED25519),
        );
        sources.system_config = present(&system);
        let export = trust_export(&sources);
        let left = route_trust(&export, "r.left.ssh");
        assert!(left["required_capabilities"].is_null(), "{}", left);
        assert_eq!(left["pins"].as_array().unwrap().len(), 1);
        // Unexpanded, the same file still blocks the route.
        sources.system_config = present("Include /etc/ssh/ssh_config.d/*\n");
        assert_eq!(
            route_trust(&trust_export(&sources), "r.left.ssh")["required_capabilities"],
            json!([UNRESOLVED_POLICY])
        );
    }

    #[test]
    fn includes_splice_only_where_openssh_meaning_survives() {
        let mut files = FixtureIncludes::default();
        files.files.insert(
            "/home/me/.ssh/options".into(),
            present("IdentitiesOnly yes\n"),
        );
        files.files.insert(
            "/home/me/.ssh/hosts".into(),
            present("Host extra\n HostName extra.example\n"),
        );
        files
            .files
            .insert("/home/me/.ssh/self".into(), present("Include self\n"));
        files
            .files
            .insert("/home/me/.ssh/locked".into(), KnownHostsSource::Unavailable);
        files.directories.insert(
            "/home/me/.ssh/config.d".into(),
            ["b.conf", "a.conf", ".hidden.conf", "notes.txt"]
                .map(String::from)
                .to_vec(),
        );
        for name in ["a", "b", ".hidden"] {
            files.files.insert(
                format!("/home/me/.ssh/config.d/{name}.conf").into(),
                present(&format!("Host {name}\n")),
            );
        }
        let base = "/home/me/.ssh";
        // Options under a specific Host splice in place; relative to ~/.ssh.
        assert_eq!(
            expand("Host box\n Include options\n User u\n", base, &files),
            "Host box\nIdentitiesOnly yes\n User u\n"
        );
        // Blocks splice at the top level, which returns after them.
        assert_eq!(
            expand("Include ~/.ssh/hosts\nUser u\n", base, &files),
            "Host extra\n HostName extra.example\nHost *\nUser u\n"
        );
        // Not under a specific Host: OpenSSH would require both to match.
        assert_eq!(
            expand("Host box\n Include hosts\n", base, &files),
            "Host box\n Include hosts\n"
        );
        // glob(3) order, no dot files, each file back in the includer's
        // context; a missing file adds nothing.
        assert_eq!(
            expand("Include config.d/*.conf absent\n", base, &files),
            "Host a\nHost *\nHost b\nHost *\n"
        );
        // Unreadable, cyclic and unsupported patterns stay as written.
        for unresolved in [
            "Include locked\n",
            "Include self\n",
            "Include %d/x\n",
            "Include $HOME/x\n",
            "Include ~other/x\n",
            "Include ../x\n",
            "Include config.d/[ab].conf\n",
            "Include config.d/*/\n",
        ] {
            assert!(
                expand(unresolved, base, &files).contains("Include "),
                "{}",
                unresolved
            );
        }
    }

    #[test]
    fn nested_include_globs_preserve_source_order_and_host_policy() {
        let base = "/home/me/.ssh";
        let mut files = FixtureIncludes::default();
        files.directories.insert(
            format!("{base}/config.d").into(),
            ["region2", "notes", "region1", ".region3"]
                .map(String::from)
                .to_vec(),
        );
        files.files.insert(
            format!("{base}/config.d/notes").into(),
            present("not a directory"),
        );
        for region in ["region1", "region2", ".region3"] {
            files.directories.insert(
                format!("{base}/config.d/{region}").into(),
                ["20-auth.conf", "10-host.conf", ".private.conf"]
                    .map(String::from)
                    .to_vec(),
            );
            files.files.insert(
                format!("{base}/config.d/{region}/10-host.conf").into(),
                present(&format!(
                    "Host left right\n HostName box\n User {region}\n UserKnownHostsFile /trust/pins\n GlobalKnownHostsFile none\n"
                )),
            );
            files.files.insert(
                format!("{base}/config.d/{region}/20-auth.conf").into(),
                present("Host left right\n User later\n PasswordAuthentication no\n"),
            );
            files.files.insert(
                format!("{base}/config.d/{region}/.private.conf").into(),
                present("Match exec must-never-run\n ProxyCommand must-never-run\n"),
            );
        }
        let expanded = expand("Include config.d/region?/*.conf\n", base, &files);
        assert!(!expanded.contains("Include "));
        assert!(!expanded.contains("must-never-run"));
        let resolver = alias_resolver(Some(&expanded), &context().home).unwrap();
        let options = resolver.config.for_host("left");
        assert_eq!(options.get("user").map(String::as_str), Some("region1"));
        assert_eq!(
            resolver.authentication_policy(&options).methods,
            vec!["publickey", "keyboard-interactive"]
        );
        let mut sources = sources(Some(&expanded));
        supply(
            &mut sources,
            "/trust/pins",
            format!("box ssh-ed25519 {ED25519}\n"),
        );
        let export = trust_export(&sources);
        let trust = route_trust(&export, "r.left.ssh");
        assert!(trust["required_capabilities"].is_null(), "{trust}");
        assert_eq!(trust["pins"].as_array().unwrap().len(), 1);
        // A wildcard may match a file as well as directories; a file cannot
        // become an intermediate component or poison otherwise valid matches.
        assert_eq!(
            expand("Include config.d/*/*.conf\n", base, &files),
            expanded
        );
    }

    #[test]
    fn include_question_marks_match_characters_and_keep_dotfiles_explicit() {
        for (pattern, name, matches) in [
            ("region?", "region1", true),
            ("region?", "region12", false),
            ("région?", "régioné", true),
            ("région??", "régioné", false),
            ("*?*.conf", "a.conf", true),
            ("*?*.conf", ".a.conf", false),
            (".?*.conf", ".é.conf", true),
            ("*.conf", "a.txt", false),
        ] {
            assert_eq!(
                glob_component_matches(pattern, name),
                matches,
                "{pattern} / {name}"
            );
        }
    }

    #[test]
    fn include_globs_do_not_reinterpret_a_literal_home_directory() {
        let home = Path::new("/home/a?b");
        let base = home.join(".ssh");
        let directory = base.join("config.d");
        let mut files = FixtureIncludes::default();
        files
            .directories
            .insert(directory.clone(), vec!["a.conf".into()]);
        files
            .files
            .insert(directory.join("a.conf"), present("User selected\n"));
        for pattern in ["config.d/*.conf", "~/.ssh/config.d/?.conf"] {
            let paths =
                include_paths(pattern, &base, home, &files, &mut IncludeBudget::new()).unwrap();
            assert_eq!(paths, vec![directory.join("a.conf")]);
        }
    }

    #[test]
    fn include_scan_budget_is_shared_even_when_patterns_match_no_files() {
        let base = "/home/me/.ssh";
        let mut files = FixtureIncludes::default();
        files.directories.insert(
            format!("{base}/empty").into(),
            (0..9000)
                .map(|index| format!("unselected-{index}.txt"))
                .collect(),
        );
        let mut budget = IncludeBudget::new();
        let expanded = expand_includes(
            "Include empty/*.conf\nInclude empty/*.conf\nInclude empty/*.conf\n",
            Path::new(base),
            &context().home,
            &files,
            &mut budget,
            0,
            true,
        );
        // The first complete no-match scan is harmless. The next consumes the
        // remaining allowance and both uncompleted Includes stay mandatory.
        assert_eq!(expanded, "Include empty/*.conf\nInclude empty/*.conf\n");
        assert_eq!(budget.entries, 0);
        assert!(
            alias_resolver(Some(&expanded), &context().home)
                .unwrap()
                .incomplete
        );
    }

    #[test]
    fn include_traversal_rejects_overfull_frontiers_and_partial_sources() {
        let base = "/home/me/.ssh";
        let mut files = FixtureIncludes::default();
        let names: Vec<_> = (0..65).map(|index| format!("region-{index:02}")).collect();
        files
            .directories
            .insert(format!("{base}/config.d").into(), names.clone());
        for name in names {
            files
                .directories
                .insert(format!("{base}/config.d/{name}").into(), vec![]);
        }
        let include = "Include config.d/*/options\n";
        assert_eq!(expand(include, base, &files), include);

        files.directories.insert(
            format!("{base}/config.d").into(),
            vec!["a.conf".into(), "b.conf".into()],
        );
        files.files.insert(
            format!("{base}/config.d/a.conf").into(),
            present("User partial\n"),
        );
        files.files.insert(
            format!("{base}/config.d/b.conf").into(),
            KnownHostsSource::Unavailable,
        );
        let include = "Include config.d/?.conf\n";
        assert_eq!(expand(include, base, &files), include);
    }

    #[test]
    fn algorithm_preferences_do_not_block_but_constraints_do() {
        let base = "Host *\n HostName box\n User u\n UserKnownHostsFile /trust/keys\n GlobalKnownHostsFile none\n";
        let required = |policy: &str| {
            let mut sources = sources(Some(&format!("{base}{policy}\n")));
            supply(
                &mut sources,
                "/trust/keys",
                format!("box ssh-ed25519 {}\n", ED25519),
            );
            route_trust(&trust_export(&sources), "r.left.ssh")["required_capabilities"].clone()
        };
        for preference in [
            "Ciphers ^aes128-gcm@openssh.com,aes256-gcm@openssh.com",
            "KexAlgorithms ^ecdh-sha2-nistp256",
            "MACs ^hmac-sha2-256-etm@openssh.com",
            "HostKeyAlgorithms ^ssh-ed25519",
            "PubkeyAcceptedAlgorithms ^ssh-ed25519",
            "CASignatureAlgorithms ^ssh-ed25519",
        ] {
            assert!(required(preference).is_null(), "{}", preference);
        }
        for constraint in [
            "Ciphers aes128-ctr",
            "Ciphers +aes128-cbc",
            "KexAlgorithms -curve25519-sha256",
            "MACs hmac-sha2-256",
            "HostKeyAlgorithms +ssh-rsa",
        ] {
            assert_eq!(
                required(constraint),
                json!([UNRESOLVED_POLICY]),
                "{constraint}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn disk_includes_follow_one_owned_link_as_macos_crypto_conf_needs() {
        let dir = tempfile::tempdir().unwrap();
        write_source(
            &dir.path().join("apple.conf"),
            "Ciphers ^aes128-gcm@openssh.com\n",
        );
        let link = dir.path().join("crypto.conf");
        std::os::unix::fs::symlink("apple.conf", &link).unwrap();
        assert_eq!(
            DiskIncludes.read(&link, 1024),
            present("Ciphers ^aes128-gcm@openssh.com\n")
        );
        // A link to a link is not followed.
        let twice = dir.path().join("twice.conf");
        std::os::unix::fs::symlink("crypto.conf", &twice).unwrap();
        assert_eq!(
            DiskIncludes.read(&twice, 1024),
            KnownHostsSource::Unavailable
        );
        // A directory that does not exist has no files; one that does lists them.
        assert_eq!(
            DiskIncludes.names(&dir.path().join("absent"), MAX_INCLUDE_ENTRIES),
            Some(Vec::new())
        );
        let mut names = DiskIncludes.names(dir.path(), MAX_INCLUDE_ENTRIES).unwrap();
        names.sort();
        assert_eq!(names, ["apple.conf", "crypto.conf", "twice.conf"]);
        assert!(DiskIncludes.names(dir.path(), 2).is_none());
        assert_eq!(DiskIncludes.is_directory(dir.path()), Some(true));
        assert_eq!(
            DiskIncludes.is_directory(&dir.path().join("apple.conf")),
            Some(false)
        );
    }
}
