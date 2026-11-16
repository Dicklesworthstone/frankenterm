//! Parse an ssh_config(5) formatted config file
use regex::{Captures, Regex};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

pub type ConfigMap = BTreeMap<String, String>;

const MATCH_EXEC_TOKENS: &[&str] = &[
    "%C", "%d", "%h", "%i", "%j", "%k", "%L", "%l", "%n", "%p", "%r", "%u",
];

/// A Pattern in a `Host` list
#[derive(Debug, PartialEq, Eq, Clone)]
struct Pattern {
    negated: bool,
    pattern: String,
    original: String,
    is_literal: bool,
}

/// Compile a glob style pattern string into a regex pattern string
fn wildcard_to_pattern(s: &str) -> (String, bool) {
    let mut pattern = String::new();
    let mut is_literal = true;
    pattern.push('^');
    for c in s.chars() {
        if c == '*' {
            pattern.push_str(".*");
            is_literal = false;
        } else if c == '?' {
            pattern.push('.');
            is_literal = false;
        } else {
            let s = regex::escape(&c.to_string());
            pattern.push_str(&s);
        }
    }
    pattern.push('$');
    (pattern, is_literal)
}

impl Pattern {
    /// Returns true if this pattern matches the provided hostname
    fn match_text(&self, hostname: &str) -> bool {
        if let Ok(re) = Regex::new(&self.pattern) {
            re.is_match(hostname)
        } else {
            false
        }
    }

    fn new(text: &str, negated: bool) -> Self {
        let (pattern, is_literal) = wildcard_to_pattern(text);
        Self {
            pattern,
            is_literal,
            negated,
            original: text.to_string(),
        }
    }

    /// Returns true if hostname matches the
    /// condition specified by a list of patterns
    fn match_group(hostname: &str, patterns: &[Self]) -> bool {
        for pat in patterns {
            if pat.match_text(hostname) {
                // We got a definitive name match.
                // If it was an exlusion then we've been told
                // that this doesn't really match, otherwise
                // we got one that we were looking for
                return !pat.negated;
            }
        }
        false
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
enum Criteria {
    Host(Vec<Pattern>),
    Exec(String),
    OriginalHost(Vec<Pattern>),
    User(Vec<Pattern>),
    LocalUser(Vec<Pattern>),
    All,
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
enum Context {
    FirstPass,
    Canonical,
    Final,
}

/// Policy for evaluating `Match exec` criteria.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default)]
pub enum MatchExecPolicy {
    /// Execute `Match exec` commands using the local shell.
    #[default]
    Permit,
    /// Refuse to execute `Match exec` commands and treat the criterion as false.
    Deny,
}

/// Outcome of a `Match exec` evaluation.
#[derive(Clone, Eq, PartialEq, Debug)]
pub enum MatchExecOutcome {
    /// The command exited successfully and the criterion matched.
    Matched { exit_status: i32 },
    /// The command ran but did not satisfy the criterion.
    False { exit_status: Option<i32> },
    /// Execution was denied by local policy.
    DeniedByPolicy,
    /// Command execution could not be completed.
    ExecutionFailed { error: String },
}

impl MatchExecOutcome {
    fn is_match(&self) -> bool {
        matches!(self, Self::Matched { .. })
    }
}

/// Diagnostic record for a single `Match exec` criterion evaluation.
#[derive(Clone, Eq, PartialEq, Debug)]
pub struct MatchExecDiagnostic {
    pub command: String,
    pub expanded_command: String,
    pub outcome: MatchExecOutcome,
}

#[derive(Copy, Clone)]
struct MatchCriteriaContext<'a> {
    hostname: &'a str,
    original_host: &'a str,
    user: &'a str,
    local_user: &'a str,
    pass: Context,
}

/// Represents `Host pattern,list` stanza in the config,
/// and the options that it logically contains
#[derive(Debug, PartialEq, Eq, Clone)]
struct MatchGroup {
    criteria: Vec<Criteria>,
    context: Context,
    options: ConfigMap,
}

impl MatchGroup {
    fn is_match<'a, F>(&self, ctx: MatchCriteriaContext<'a>, exec_matcher: &mut F) -> bool
    where
        F: FnMut(&str, MatchCriteriaContext<'a>) -> bool,
    {
        if self.context != ctx.pass {
            return false;
        }
        for c in &self.criteria {
            match c {
                Criteria::Host(patterns) => {
                    if !Pattern::match_group(ctx.hostname, patterns) {
                        return false;
                    }
                }
                Criteria::Exec(command) => {
                    if !exec_matcher(command, ctx) {
                        return false;
                    }
                }
                Criteria::OriginalHost(patterns) => {
                    if !Pattern::match_group(ctx.original_host, patterns) {
                        return false;
                    }
                }
                Criteria::User(patterns) => {
                    if !Pattern::match_group(ctx.user, patterns) {
                        return false;
                    }
                }
                Criteria::LocalUser(patterns) => {
                    if !Pattern::match_group(ctx.local_user, patterns) {
                        return false;
                    }
                }
                Criteria::All => {
                    // Always matches
                }
            }
        }
        true
    }
}

/// Holds the ordered set of parsed options.
/// The config file semantics are that the first matching value
/// for a given option takes precedence
#[derive(Debug, PartialEq, Eq, Clone)]
struct ParsedConfigFile {
    /// options that appeared before any `Host` stanza
    options: ConfigMap,
    /// options inside a `Host` stanza
    groups: Vec<MatchGroup>,
    /// list of loaded file names
    loaded_files: Vec<PathBuf>,
}

impl ParsedConfigFile {
    fn parse(s: &str, cwd: Option<&Path>, source_file: Option<&Path>) -> Self {
        let mut options = ConfigMap::new();
        let mut groups = vec![];
        let mut loaded_files = vec![];

        if let Some(source) = source_file {
            loaded_files.push(source.to_path_buf());
        }

        Self::parse_impl(s, cwd, &mut options, &mut groups, &mut loaded_files);

        Self {
            options,
            groups,
            loaded_files,
        }
    }

    fn do_include(
        pattern: &str,
        cwd: Option<&Path>,
        options: &mut ConfigMap,
        groups: &mut Vec<MatchGroup>,
        loaded_files: &mut Vec<PathBuf>,
    ) {
        match filenamegen::Glob::new(&pattern) {
            Ok(g) => {
                match cwd
                    .as_ref()
                    .map(|p| p.to_path_buf())
                    .or_else(|| std::env::current_dir().ok())
                {
                    Some(cwd) => {
                        for path in g.walk(&cwd) {
                            let path = if path.is_absolute() {
                                path
                            } else {
                                cwd.join(path)
                            };
                            match std::fs::read_to_string(&path) {
                                Ok(data) => {
                                    loaded_files.push(path.clone());
                                    Self::parse_impl(
                                        &data,
                                        Some(&cwd),
                                        options,
                                        groups,
                                        loaded_files,
                                    );
                                }
                                Err(err) => {
                                    log::error!(
                                        "error expanding `Include {}`: unable to open {}: {:#}",
                                        pattern,
                                        path.display(),
                                        err
                                    );
                                }
                            }
                        }
                    }
                    None => {
                        log::error!(
                            "error expanding `Include {}`: unable to determine cwd",
                            pattern
                        );
                    }
                }
            }
            Err(err) => {
                log::error!("error expanding `Include {}`: {:#}", pattern, err);
            }
        }
    }

    fn parse_impl(
        s: &str,
        cwd: Option<&Path>,
        options: &mut ConfigMap,
        groups: &mut Vec<MatchGroup>,
        loaded_files: &mut Vec<PathBuf>,
    ) {
        fn parse_match_tokens(v: &str) -> Vec<String> {
            match shell_words::split(v) {
                Ok(tokens) => tokens,
                Err(err) => {
                    log::warn!(
                        "failed to shell-parse Match stanza {:?}: {}; falling back to whitespace tokenization",
                        v,
                        err
                    );
                    v.split_ascii_whitespace().map(str::to_string).collect()
                }
            }
        }

        for line in s.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            if let Some(sep) = line.find(|c: char| c == '=' || c.is_whitespace()) {
                let (k, v) = line.split_at(sep);
                let k = k.trim().to_lowercase();
                let v = v[1..].trim();

                let v = if v.starts_with('"') && v.ends_with('"') {
                    &v[1..v.len() - 1]
                } else {
                    v
                };

                fn parse_pattern_list(v: &str) -> Vec<Pattern> {
                    let mut patterns = vec![];
                    for p in v.split(',') {
                        let p = p.trim();
                        if p.starts_with('!') {
                            patterns.push(Pattern::new(&p[1..], true));
                        } else {
                            patterns.push(Pattern::new(p, false));
                        }
                    }
                    patterns
                }
                fn parse_whitespace_pattern_list(v: &str) -> Vec<Pattern> {
                    let mut patterns = vec![];
                    for p in v.split_ascii_whitespace() {
                        let p = p.trim();
                        if p.starts_with('!') {
                            patterns.push(Pattern::new(&p[1..], true));
                        } else {
                            patterns.push(Pattern::new(p, false));
                        }
                    }
                    patterns
                }

                if k == "include" {
                    Self::do_include(v, cwd, options, groups, loaded_files);
                    continue;
                }

                if k == "host" {
                    let patterns = parse_whitespace_pattern_list(v);
                    groups.push(MatchGroup {
                        criteria: vec![Criteria::Host(patterns)],
                        options: ConfigMap::new(),
                        context: Context::FirstPass,
                    });
                    continue;
                }

                if k == "match" {
                    let mut criteria = vec![];
                    let mut context = Context::FirstPass;

                    let mut tokens = parse_match_tokens(v).into_iter();

                    while let Some(cname) = tokens.next() {
                        match cname.to_lowercase().as_str() {
                            "all" => {
                                criteria.push(Criteria::All);
                            }
                            "canonical" => {
                                context = Context::Canonical;
                            }
                            "final" => {
                                context = Context::Final;
                            }
                            "exec" => {
                                let command = tokens.next().unwrap_or_else(|| "false".to_string());
                                criteria.push(Criteria::Exec(command));
                            }
                            "host" => {
                                criteria.push(Criteria::Host(parse_pattern_list(
                                    tokens.next().as_deref().unwrap_or(""),
                                )));
                            }
                            "originalhost" => {
                                criteria.push(Criteria::OriginalHost(parse_pattern_list(
                                    tokens.next().as_deref().unwrap_or(""),
                                )));
                            }
                            "user" => {
                                criteria.push(Criteria::User(parse_pattern_list(
                                    tokens.next().as_deref().unwrap_or(""),
                                )));
                            }
                            "localuser" => {
                                criteria.push(Criteria::LocalUser(parse_pattern_list(
                                    tokens.next().as_deref().unwrap_or(""),
                                )));
                            }
                            _ => break,
                        }
                    }

                    groups.push(MatchGroup {
                        criteria,
                        options: ConfigMap::new(),
                        context,
                    });
                    continue;
                }

                fn add_option(options: &mut ConfigMap, k: String, v: &str) {
                    // first option wins in ssh_config, except for identityfile
                    // which explicitly allows multiple entries to combine together
                    let is_identity_file = k == "identityfile";
                    options
                        .entry(k)
                        .and_modify(|e| {
                            if is_identity_file {
                                e.push(' ');
                                e.push_str(v);
                            }
                        })
                        .or_insert_with(|| v.to_string());
                }

                if let Some(group) = groups.last_mut() {
                    add_option(&mut group.options, k, v);
                } else {
                    add_option(options, k, v);
                }
            }
        }
    }

    /// Apply configuration values that match the specified hostname to target,
    /// but only if a given key is not already present in target, because the
    /// semantics are that the first match wins
    fn apply_matches<'a, F>(
        &self,
        ctx: MatchCriteriaContext<'a>,
        target: &mut ConfigMap,
        exec_matcher: &mut F,
    ) -> bool
    where
        F: FnMut(&str, &ConfigMap, MatchCriteriaContext<'a>) -> bool,
    {
        let mut needs_reparse = false;

        for (k, v) in &self.options {
            target.entry(k.to_string()).or_insert_with(|| v.to_string());
        }
        for group in &self.groups {
            if group.context != Context::FirstPass {
                needs_reparse = true;
            }
            if group.is_match(ctx, &mut |command, match_ctx| {
                exec_matcher(command, target, match_ctx)
            }) {
                for (k, v) in &group.options {
                    target.entry(k.to_string()).or_insert_with(|| v.to_string());
                }
            }
        }

        needs_reparse
    }
}

/// A context for resolving configuration values.
/// Holds a combination of environment and token expansion state,
/// as well as the set of configs that should be consulted.
#[derive(Clone)]
pub struct Config {
    config_files: Vec<ParsedConfigFile>,
    options: ConfigMap,
    tokens: ConfigMap,
    environment: Option<ConfigMap>,
    match_exec_policy: MatchExecPolicy,
}

impl Config {
    /// Create a new context without any config files loaded
    pub fn new() -> Self {
        Self {
            config_files: vec![],
            options: ConfigMap::new(),
            tokens: ConfigMap::new(),
            environment: None,
            match_exec_policy: MatchExecPolicy::Permit,
        }
    }

    /// Control whether `Match exec` criteria are allowed to spawn local commands.
    pub fn set_match_exec_policy(&mut self, policy: MatchExecPolicy) {
        self.match_exec_policy = policy;
    }

    /// Assign a fake environment map, useful for testing.
    /// The environment is used to expand certain values
    /// from the config.
    pub fn assign_environment(&mut self, env: ConfigMap) {
        self.environment.replace(env);
    }

    /// Assigns token names and expansions for use with a number of
    /// options.  The names and expansions are specified
    /// by `man 5 ssh_config`
    pub fn assign_tokens(&mut self, tokens: ConfigMap) {
        self.tokens = tokens;
    }

    /// Assign the value for an option.
    /// This is logically equivalent to the user specifying command
    /// line options to override config values.
    /// These values take precedence over any values found in config files.
    pub fn set_option<K: AsRef<str>, V: AsRef<str>>(&mut self, key: K, value: V) {
        self.options
            .insert(key.as_ref().to_lowercase(), value.as_ref().to_string());
    }

    /// Parse `config_string` as if it were the contents of an `ssh_config` file,
    /// and add that to the list of configs.
    pub fn add_config_string(&mut self, config_string: &str) {
        self.config_files
            .push(ParsedConfigFile::parse(config_string, None, None));
    }

    /// Open `path`, read its contents and parse it as an `ssh_config` file,
    /// adding that to the list of configs
    pub fn add_config_file<P: AsRef<Path>>(&mut self, path: P) {
        if let Ok(data) = std::fs::read_to_string(path.as_ref()) {
            self.config_files.push(ParsedConfigFile::parse(
                &data,
                path.as_ref().parent(),
                Some(path.as_ref()),
            ));
        }
    }

    /// Convenience method for adding the ~/.ssh/config and system-wide
    /// `/etc/ssh/config` files to the list of configs
    pub fn add_default_config_files(&mut self) {
        if let Some(home) = dirs_next::home_dir() {
            self.add_config_file(home.join(".ssh").join("config"));
        }
        self.add_config_file("/etc/ssh/ssh_config");
        if let Ok(sysdrive) = std::env::var("SystemDrive") {
            self.add_config_file(format!("{}/ProgramData/ssh/ssh_config", sysdrive));
        }
    }

    fn resolve_local_host(&self, include_domain_name: bool) -> String {
        let hostname = if cfg!(test) {
            // Use a fixed and plausible name for the local hostname
            // when running tests.  This isn't an ideal solution, but
            // it is convenient and sufficient at the time of writing
            "localhost".to_string()
        } else {
            gethostname::gethostname().to_string_lossy().to_string()
        };

        if include_domain_name {
            hostname
        } else {
            match hostname.split_once('.') {
                Some((hostname, _domain)) => hostname.to_string(),
                None => hostname,
            }
        }
    }

    fn resolve_local_user(&self) -> String {
        for user in &["USER", "USERNAME"] {
            if let Some(user) = self.resolve_env(user) {
                return user;
            }
        }
        "unknown-user".to_string()
    }

    /// Resolve the configuration for a given host.
    /// The returned map will expand environment and tokens for options
    /// where that is specified.
    /// Note that in some configurations, the config should be parsed once
    /// to resolve the main configuration, and then based on some options
    /// (such as CanonicalHostname), the tokens should be updated and
    /// the config parsed a second time in order for value expansion
    /// to have the same results as `ssh`.
    pub fn for_host<H: AsRef<str>>(&self, host: H) -> ConfigMap {
        self.for_host_with_match_diagnostics(host).0
    }

    /// Resolve the configuration for a given host and return `Match exec`
    /// diagnostics describing any command evaluations that occurred.
    pub fn for_host_with_match_diagnostics<H: AsRef<str>>(
        &self,
        host: H,
    ) -> (ConfigMap, Vec<MatchExecDiagnostic>) {
        let host = host.as_ref();
        let local_user = self.resolve_local_user();
        let mut result = self.options.clone();
        let mut needs_reparse = false;
        let mut diagnostics = Vec::new();

        for config in &self.config_files {
            let target_user = result
                .get("user")
                .cloned()
                .unwrap_or_else(|| local_user.clone());
            let match_ctx = MatchCriteriaContext {
                hostname: host,
                original_host: host,
                user: &target_user,
                local_user: &local_user,
                pass: Context::FirstPass,
            };
            if config.apply_matches(
                match_ctx,
                &mut result,
                &mut |command, current_target, ctx| {
                    self.evaluate_match_exec(command, current_target, ctx, &mut diagnostics)
                },
            ) {
                needs_reparse = true;
            }
        }

        if needs_reparse {
            log::debug!(
                "ssh configuration uses options that require two-phase \
                parsing, which isn't supported"
            );
        }

        let mut token_map = self.tokens.clone();
        token_map.insert("%h".to_string(), host.to_string());
        result
            .entry("hostname".to_string())
            .and_modify(|curr| {
                if let Some(tokens) = self.should_expand_tokens("hostname") {
                    self.expand_tokens(curr, tokens, &token_map);
                }
            })
            .or_insert_with(|| host.to_string());
        token_map.insert("%h".to_string(), result["hostname"].to_string());
        token_map.insert("%n".to_string(), host.to_string());
        token_map.insert(
            "%r".to_string(),
            result
                .get("user")
                .cloned()
                .unwrap_or_else(|| local_user.clone()),
        );
        token_map.insert("%k".to_string(), host.to_string());
        token_map.insert(
            "%p".to_string(),
            result
                .get("port")
                .map(|p| p.to_string())
                .unwrap_or_else(|| "22".to_string()),
        );

        for (k, v) in &mut result {
            if let Some(tokens) = self.should_expand_tokens(k) {
                self.expand_tokens(v, tokens, &token_map);
            }

            if self.should_expand_environment(k) {
                self.expand_environment(v);
            }
        }

        result
            .entry("port".to_string())
            .or_insert_with(|| "22".to_string());

        result
            .entry("user".to_string())
            .or_insert_with(|| local_user.clone());

        if !result.contains_key("userknownhostsfile") {
            if let Some(home) = self.resolve_home() {
                result.insert(
                    "userknownhostsfile".to_string(),
                    format!("{}/.ssh/known_hosts {}/.ssh/known_hosts2", home, home,),
                );
            }
        }

        if !result.contains_key("identityfile") {
            if let Some(home) = self.resolve_home() {
                result.insert(
                    "identityfile".to_string(),
                    format!(
                        "{}/.ssh/id_dsa {}/.ssh/id_ecdsa {}/.ssh/id_ed25519 {}/.ssh/id_rsa",
                        home, home, home, home
                    ),
                );
            }
        }

        if !result.contains_key("identityagent") {
            if let Some(sock_path) = self.resolve_env("SSH_AUTH_SOCK") {
                result.insert("identityagent".to_string(), sock_path);
            }
        }

        (result, diagnostics)
    }

    /// Return true if a given option name is subject to environment variable
    /// expansion.
    fn should_expand_environment(&self, key: &str) -> bool {
        match key {
            "certificatefile" | "controlpath" | "identityagent" | "identityfile"
            | "userknownhostsfile" | "localforward" | "remoteforward" => true,
            _ => false,
        }
    }

    /// Returns a set of tokens that should be expanded for a given option name
    fn should_expand_tokens(&self, key: &str) -> Option<&[&str]> {
        match key {
            "certificatefile" | "controlpath" | "identityagent" | "identityfile"
            | "localforward" | "remotecommand" | "remoteforward" | "userknownhostsfile" => {
                Some(&["%C", "%d", "%h", "%i", "%L", "%l", "%n", "%p", "%r", "%u"])
            }
            "hostname" => Some(&["%h"]),
            "localcommand" => Some(&[
                "%C", "%d", "%h", "%i", "%k", "%L", "%l", "%n", "%p", "%r", "%T", "%u",
            ]),
            "proxycommand" => Some(&["%h", "%n", "%p", "%r"]),
            _ => None,
        }
    }

    /// Resolve the home directory.
    /// For the sake of unit testing, this will look for HOME in the provided
    /// environment override before asking the system for the home directory.
    fn resolve_home(&self) -> Option<String> {
        if let Some(env) = self.environment.as_ref() {
            if let Some(home) = env.get("HOME") {
                return Some(home.to_string());
            }
        }
        if let Some(home) = dirs_next::home_dir() {
            if let Some(home) = home.to_str() {
                return Some(home.to_string());
            }
        }
        None
    }

    fn resolve_uid(&self) -> String {
        #[cfg(test)]
        if let Some(env) = self.environment.as_ref() {
            // For testing purposes only, allow pretending that we
            // have a specific fixed UID so that test expectations
            // are easier to handle with snapshots
            if let Some(uid) = env.get("WEZTERM_SSH_UID") {
                return uid.to_string();
            }
        }

        #[cfg(unix)]
        {
            let uid = unsafe { libc::getuid() };
            return uid.to_string();
        }

        #[cfg(not(unix))]
        {
            String::new()
        }
    }

    /// Perform token substitution
    fn expand_tokens(&self, value: &mut String, tokens: &[&str], token_map: &ConfigMap) {
        let orig_value = value.to_string();
        for &t in tokens {
            if let Some(v) = token_map.get(t) {
                *value = value.replace(t, v);
            } else if t == "%i" {
                *value = value.replace(t, &self.resolve_uid());
            } else if t == "%u" {
                *value = value.replace(t, &self.resolve_local_user());
            } else if t == "%l" {
                *value = value.replace(t, &self.resolve_local_host(false));
            } else if t == "%L" {
                *value = value.replace(t, &self.resolve_local_host(true));
            } else if t == "%d" {
                if let Some(home) = self.resolve_home() {
                    let mut items = value
                        .split_whitespace()
                        .map(|s| s.to_string())
                        .collect::<Vec<String>>();
                    for item in &mut items {
                        if item.starts_with("~/") {
                            item.replace_range(0..1, &home);
                        } else {
                            *item = item.replace(t, &home);
                        }
                    }
                    *value = items.join(" ");
                }
            } else if t == "%j" {
                // %j: The contents of the ProxyJump option, or the empty string if this option is unset
                // We don't directly support ProxyJump, and this %j token referencing
                // may technically put this into two-phase evaluation territory which
                // we don't support.
                // Let's silently gloss over this and treat this token as the empty
                // string.
                // Someone in the future will probably curse this.
                *value = value.replace(t, "");
            } else if t == "%T" {
                // %T: The local tun(4) or tap(4) network interface assigned if tunnel
                // forwarding was requested, or "NONE" otherwise.
                // We don't support this function, so it is always NONE
                *value = value.replace(t, "NONE");
            } else if t == "%C" && value.contains("%C") {
                // %C: Hash of %l%h%p%r%j
                use sha2::Digest;
                let mut c_value = "%l%h%p%r%j".to_string();
                self.expand_tokens(&mut c_value, tokens, token_map);
                let hashed = hex::encode(sha2::Sha256::digest(&c_value.as_bytes()));
                *value = value.replace("%C", &hashed);
            } else if value.contains(t) {
                log::warn!("Unsupported token {t} when evaluating `{orig_value}`");
            }
        }

        *value = value.replace("%%", "%");
    }

    /// Resolve an environment variable; if an override is set use that,
    /// otherwise read from the real environment.
    fn resolve_env(&self, name: &str) -> Option<String> {
        if let Some(env) = self.environment.as_ref() {
            env.get(name).cloned()
        } else {
            std::env::var(name).ok()
        }
    }

    fn build_match_exec_token_map(
        &self,
        current_target: &ConfigMap,
        ctx: MatchCriteriaContext<'_>,
    ) -> ConfigMap {
        let mut hostname_value = current_target
            .get("hostname")
            .cloned()
            .unwrap_or_else(|| ctx.hostname.to_string());
        let mut bootstrap_tokens = self.tokens.clone();
        bootstrap_tokens.insert("%h".to_string(), ctx.hostname.to_string());
        if let Some(tokens) = self.should_expand_tokens("hostname") {
            self.expand_tokens(&mut hostname_value, tokens, &bootstrap_tokens);
        }

        let mut token_map = self.tokens.clone();
        token_map.insert("%h".to_string(), hostname_value);
        token_map.insert("%n".to_string(), ctx.original_host.to_string());
        token_map.insert("%k".to_string(), ctx.original_host.to_string());
        token_map.insert("%r".to_string(), ctx.user.to_string());
        token_map.insert("%u".to_string(), ctx.local_user.to_string());
        token_map.insert(
            "%p".to_string(),
            current_target
                .get("port")
                .cloned()
                .unwrap_or_else(|| "22".to_string()),
        );
        token_map
    }

    fn evaluate_match_exec(
        &self,
        command: &str,
        current_target: &ConfigMap,
        ctx: MatchCriteriaContext<'_>,
        diagnostics: &mut Vec<MatchExecDiagnostic>,
    ) -> bool {
        let token_map = self.build_match_exec_token_map(current_target, ctx);
        let mut expanded_command = command.to_string();
        self.expand_tokens(&mut expanded_command, MATCH_EXEC_TOKENS, &token_map);

        let outcome = match self.match_exec_policy {
            MatchExecPolicy::Deny => MatchExecOutcome::DeniedByPolicy,
            MatchExecPolicy::Permit => {
                let mut shell_cmd;
                if cfg!(windows) {
                    let comspec = self
                        .resolve_env("COMSPEC")
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "cmd".to_string());
                    shell_cmd = Command::new(comspec);
                    shell_cmd.args(["/c", expanded_command.as_str()]);
                } else {
                    let shell = self
                        .resolve_env("SHELL")
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or_else(|| "sh".to_string());
                    shell_cmd = Command::new(shell);
                    shell_cmd.args(["-c", expanded_command.as_str()]);
                }

                match shell_cmd.status() {
                    Ok(status) if status.success() => MatchExecOutcome::Matched {
                        exit_status: status.code().unwrap_or(0),
                    },
                    Ok(status) => MatchExecOutcome::False {
                        exit_status: status.code(),
                    },
                    Err(err) => MatchExecOutcome::ExecutionFailed {
                        error: err.to_string(),
                    },
                }
            }
        };

        diagnostics.push(MatchExecDiagnostic {
            command: command.to_string(),
            expanded_command,
            outcome: outcome.clone(),
        });

        outcome.is_match()
    }

    /// Look for `${NAME}` and substitute the value of the `NAME` env var
    /// into the provided string.
    fn expand_environment(&self, value: &mut String) {
        let re = Regex::new(r#"\$\{([a-zA-Z_][a-zA-Z_0-9]+)\}"#).unwrap();
        *value = re
            .replace_all(value, |caps: &Captures| -> String {
                if let Some(rep) = self.resolve_env(&caps[1]) {
                    rep
                } else {
                    caps[0].to_string()
                }
            })
            .to_string();
    }

    /// Returns the list of file names that were loaded as part of parsing
    /// the ssh config
    pub fn loaded_config_files(&self) -> Vec<PathBuf> {
        let mut files = vec![];

        for config in &self.config_files {
            for file in &config.loaded_files {
                if !files.contains(file) {
                    files.push(file.to_path_buf());
                }
            }
        }

        files
    }

    /// Returns the list of host names that have defined ssh config entries.
    /// The host names are literal (non-pattern), non-negated hosts extracted
    /// from `Host` and `Match` stanzas in the ssh config.
    pub fn enumerate_hosts(&self) -> Vec<String> {
        let mut hosts = vec![];

        for config in &self.config_files {
            for group in &config.groups {
                for c in &group.criteria {
                    if let Criteria::Host(patterns) = c {
                        for pattern in patterns {
                            if pattern.is_literal && !pattern.negated {
                                if !hosts.contains(&pattern.original) {
                                    hosts.push(pattern.original.clone());
                                }
                            }
                        }
                    }
                }
            }
        }

        hosts
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("Config");
        debug.field("config_files", &self.config_files);
        debug.field("options", &self.options);
        debug.field("tokens", &self.tokens);
        debug.field("environment", &self.environment);
        if self.match_exec_policy != MatchExecPolicy::Permit {
            debug.field("match_exec_policy", &self.match_exec_policy);
        }
        debug.finish()
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use k9::snapshot;

    #[test]
    fn parse_keepalive() {
        let mut config = Config::new();
        config.add_config_string(
            r#"
        Host foo
            ServerAliveInterval 60
            "#,
        );
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        let opts = config.for_host("foo");
        snapshot!(
            opts,
            r#"
{
    "hostname": "foo",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "serveraliveinterval": "60",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn parse_proxy_command_tokens() {
        let mut config = Config::new();
        config.add_config_string(
            r#"
        Host foo
            ProxyCommand /usr/bin/corp-ssh-helper -dst_username=%r %h %p
            Port 2222
            "#,
        );
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        let opts = config.for_host("foo");
        snapshot!(
            opts,
            r#"
{
    "hostname": "foo",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "2222",
    "proxycommand": "/usr/bin/corp-ssh-helper -dst_username=me foo 2222",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn parse_proxy_command() {
        let mut config = Config::new();
        config.add_config_string(
            r#"
        Host foo
            ProxyCommand /usr/bin/ssh-proxy-helper -oX=Y host 22
            "#,
        );

        snapshot!(
            &config,
            r#"
Config {
    config_files: [
        ParsedConfigFile {
            options: {},
            groups: [
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^foo$",
                                    original: "foo",
                                    is_literal: true,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "proxycommand": "/usr/bin/ssh-proxy-helper -oX=Y host 22",
                    },
                },
            ],
            loaded_files: [],
        },
    ],
    options: {},
    tokens: {},
    environment: None,
}
"#
        );
    }

    #[test]
    fn misc_tokens() {
        let mut config = Config::new();

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        fake_env.insert("WEZTERM_SSH_UID".to_string(), "1000".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        Host target-host
            LocalCommand C=%C d=%d h=%h i=%i L=%L l=%L n=%n p=%p r=%r T=%T u=%u
            "#,
        );

        let opts = config.for_host("target-host");
        snapshot!(
            opts,
            r#"
{
    "hostname": "target-host",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "localcommand": "C=8de28522efb92214d9c442ea0402863e34d095a4006467ad9136a48e930870ea d=/home/me h=target-host i=1000 L=localhost l=localhost n=target-host p=22 r=me T=NONE u=me",
    "port": "22",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn parse_user() {
        let mut config = Config::new();

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        Host foo
            HostName 10.0.0.1
            User foo
            IdentityFile "%d/.ssh/id_pub.dsa"
            "#,
        );

        snapshot!(
            &config,
            r#"
Config {
    config_files: [
        ParsedConfigFile {
            options: {},
            groups: [
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^foo$",
                                    original: "foo",
                                    is_literal: true,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "hostname": "10.0.0.1",
                        "identityfile": "%d/.ssh/id_pub.dsa",
                        "user": "foo",
                    },
                },
            ],
            loaded_files: [],
        },
    ],
    options: {},
    tokens: {},
    environment: Some(
        {
            "HOME": "/home/me",
            "USER": "me",
        },
    ),
}
"#
        );

        let opts = config.for_host("foo");
        snapshot!(
            opts,
            r#"
{
    "hostname": "10.0.0.1",
    "identityfile": "/home/me/.ssh/id_pub.dsa",
    "port": "22",
    "user": "foo",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn hostname_expansion() {
        let mut config = Config::new();

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        Host foo0 foo1 foo2
            HostName server-%h
            "#,
        );

        let opts = config.for_host("foo0");
        snapshot!(
            opts,
            r#"
{
    "hostname": "server-foo0",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let opts = config.for_host("foo1");
        snapshot!(
            opts,
            r#"
{
    "hostname": "server-foo1",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let opts = config.for_host("foo2");
        snapshot!(
            opts,
            r#"
{
    "hostname": "server-foo2",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn parse_proxy_command_hostname_expansion() {
        let mut config = Config::new();

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        Host foo
            HostName server-%h
            ProxyCommand nc -x localhost:1080 %h %p
            "#,
        );

        let opts = config.for_host("foo");
        snapshot!(
            opts,
            r#"
{
    "hostname": "server-foo",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "proxycommand": "nc -x localhost:1080 server-foo 22",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn multiple_identityfile() {
        let mut config = Config::new();

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        Host foo
            HostName 10.0.0.1
            User foo
            IdentityFile "~/.ssh/id_pub.dsa"
            IdentityFile "~/.ssh/id_pub.rsa"
            "#,
        );

        let opts = config.for_host("foo");
        snapshot!(
            opts,
            r#"
{
    "hostname": "10.0.0.1",
    "identityfile": "/home/me/.ssh/id_pub.dsa /home/me/.ssh/id_pub.rsa",
    "port": "22",
    "user": "foo",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn sub_tilde() {
        let mut config = Config::new();

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        Host foo
            HostName 10.0.0.1
            User foo
            IdentityFile "~/.ssh/id_pub.dsa"
            "#,
        );

        let opts = config.for_host("foo");
        snapshot!(
            opts,
            r#"
{
    "hostname": "10.0.0.1",
    "identityfile": "/home/me/.ssh/id_pub.dsa",
    "port": "22",
    "user": "foo",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn parse_match() {
        let mut config = Config::new();

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        # I am a comment
        Something first
        # the prior Something takes precedence
        Something ignored
        Match Host 192.168.1.8,wopr
            FowardAgent yes
            IdentityFile "%d/.ssh/id_pub.dsa"

        Match Host !a.b,*.b User fred
            ForwardAgent no
            IdentityAgent "${HOME}/.ssh/agent"

        Match Host !a.b,*.b User me
            ForwardAgent no
            IdentityAgent "${HOME}/.ssh/agent-me"

        Host *
            Something  else
            "#,
        );

        snapshot!(
            &config,
            r#"
Config {
    config_files: [
        ParsedConfigFile {
            options: {
                "something": "first",
            },
            groups: [
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^192\\.168\\.1\\.8$",
                                    original: "192.168.1.8",
                                    is_literal: true,
                                },
                                Pattern {
                                    negated: false,
                                    pattern: "^wopr$",
                                    original: "wopr",
                                    is_literal: true,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "fowardagent": "yes",
                        "identityfile": "%d/.ssh/id_pub.dsa",
                    },
                },
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: true,
                                    pattern: "^a\\.b$",
                                    original: "a.b",
                                    is_literal: true,
                                },
                                Pattern {
                                    negated: false,
                                    pattern: "^.*\\.b$",
                                    original: "*.b",
                                    is_literal: false,
                                },
                            ],
                        ),
                        User(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^fred$",
                                    original: "fred",
                                    is_literal: true,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "forwardagent": "no",
                        "identityagent": "${HOME}/.ssh/agent",
                    },
                },
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: true,
                                    pattern: "^a\\.b$",
                                    original: "a.b",
                                    is_literal: true,
                                },
                                Pattern {
                                    negated: false,
                                    pattern: "^.*\\.b$",
                                    original: "*.b",
                                    is_literal: false,
                                },
                            ],
                        ),
                        User(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^me$",
                                    original: "me",
                                    is_literal: true,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "forwardagent": "no",
                        "identityagent": "${HOME}/.ssh/agent-me",
                    },
                },
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^.*$",
                                    original: "*",
                                    is_literal: false,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "something": "else",
                    },
                },
            ],
            loaded_files: [],
        },
    ],
    options: {},
    tokens: {},
    environment: Some(
        {
            "HOME": "/home/me",
            "USER": "me",
        },
    ),
}
"#
        );

        snapshot!(
            config.enumerate_hosts(),
            r#"
[
    "192.168.1.8",
    "wopr",
]
"#
        );

        let opts = config.for_host("random");
        snapshot!(
            opts,
            r#"
{
    "hostname": "random",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "something": "first",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let opts = config.for_host("192.168.1.8");
        snapshot!(
            opts,
            r#"
{
    "fowardagent": "yes",
    "hostname": "192.168.1.8",
    "identityfile": "/home/me/.ssh/id_pub.dsa",
    "port": "22",
    "something": "first",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let opts = config.for_host("a.b");
        snapshot!(
            opts,
            r#"
{
    "hostname": "a.b",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "something": "first",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let opts = config.for_host("b.b");
        snapshot!(
            opts,
            r#"
{
    "forwardagent": "no",
    "hostname": "b.b",
    "identityagent": "/home/me/.ssh/agent-me",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "something": "first",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/fred".to_string());
        fake_env.insert("USER".to_string(), "fred".to_string());
        config.assign_environment(fake_env);

        let opts = config.for_host("b.b");
        snapshot!(
            opts,
            r#"
{
    "forwardagent": "no",
    "hostname": "b.b",
    "identityagent": "/home/fred/.ssh/agent",
    "identityfile": "/home/fred/.ssh/id_dsa /home/fred/.ssh/id_ecdsa /home/fred/.ssh/id_ed25519 /home/fred/.ssh/id_rsa",
    "port": "22",
    "something": "first",
    "user": "fred",
    "userknownhostsfile": "/home/fred/.ssh/known_hosts /home/fred/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn parse_match_exec_preserves_quoted_command() {
        let mut config = Config::new();
        config.add_config_string(
            r#"
        Match exec "exit 0" host foo
            Port 2200
            "#,
        );

        snapshot!(
            &config,
            r#"
Config {
    config_files: [
        ParsedConfigFile {
            options: {},
            groups: [
                MatchGroup {
                    criteria: [
                        Exec(
                            "exit 0",
                        ),
                        Host(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^foo$",
                                    original: "foo",
                                    is_literal: true,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "port": "2200",
                    },
                },
            ],
            loaded_files: [],
        },
    ],
    options: {},
    tokens: {},
    environment: None,
}
"#
        );
    }

    #[test]
    fn match_exec_success_applies_options_and_reports_diagnostics() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        fake_env.insert(
            if cfg!(windows) {
                "COMSPEC".to_string()
            } else {
                "SHELL".to_string()
            },
            if cfg!(windows) {
                "cmd".to_string()
            } else {
                "sh".to_string()
            },
        );
        config.assign_environment(fake_env);
        config.add_config_string(
            r#"
        Match exec "exit 0"
            SendEnv LANG
            "#,
        );

        let (opts, diagnostics) = config.for_host_with_match_diagnostics("anyhost");
        assert_eq!(opts.get("sendenv"), Some(&"LANG".to_string()));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].outcome,
            MatchExecOutcome::Matched { exit_status: 0 }
        );
        assert_eq!(diagnostics[0].expanded_command, "exit 0");
    }

    #[test]
    fn match_exec_false_skips_options_and_reports_diagnostics() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        fake_env.insert(
            if cfg!(windows) {
                "COMSPEC".to_string()
            } else {
                "SHELL".to_string()
            },
            if cfg!(windows) {
                "cmd".to_string()
            } else {
                "sh".to_string()
            },
        );
        config.assign_environment(fake_env);
        config.add_config_string(
            r#"
        Match exec "exit 7"
            SendEnv LANG
            "#,
        );

        let (opts, diagnostics) = config.for_host_with_match_diagnostics("anyhost");
        assert!(!opts.contains_key("sendenv"));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(
            diagnostics[0].outcome,
            MatchExecOutcome::False {
                exit_status: Some(7),
            }
        );
    }

    #[test]
    fn match_exec_denied_by_policy_is_explicit() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);
        config.set_match_exec_policy(MatchExecPolicy::Deny);
        config.add_config_string(
            r#"
        Match exec "exit 0"
            SendEnv LANG
            "#,
        );

        let (opts, diagnostics) = config.for_host_with_match_diagnostics("anyhost");
        assert!(!opts.contains_key("sendenv"));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].outcome, MatchExecOutcome::DeniedByPolicy);
    }

    #[cfg(unix)]
    #[test]
    fn match_exec_spawn_failure_is_reported() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        fake_env.insert("SHELL".to_string(), "/definitely/missing-shell".to_string());
        config.assign_environment(fake_env);
        config.add_config_string(
            r#"
        Match exec "exit 0"
            SendEnv LANG
            "#,
        );

        let (opts, diagnostics) = config.for_host_with_match_diagnostics("anyhost");
        assert!(!opts.contains_key("sendenv"));
        assert_eq!(diagnostics.len(), 1);
        assert!(matches!(
            diagnostics[0].outcome,
            MatchExecOutcome::ExecutionFailed { .. }
        ));
    }

    #[test]
    fn parse_simple() {
        let mut config = Config::new();

        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        # I am a comment
        Something first
        # the prior Something takes precedence
        Something ignored
        Host 192.168.1.8 wopr
            FowardAgent yes
            IdentityFile "%d/.ssh/id_pub.dsa"

        Host !a.b *.b
            ForwardAgent no
            IdentityAgent "${HOME}/.ssh/agent"

        Host *
            Something  else
            "#,
        );

        snapshot!(
            &config,
            r#"
Config {
    config_files: [
        ParsedConfigFile {
            options: {
                "something": "first",
            },
            groups: [
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^192\\.168\\.1\\.8$",
                                    original: "192.168.1.8",
                                    is_literal: true,
                                },
                                Pattern {
                                    negated: false,
                                    pattern: "^wopr$",
                                    original: "wopr",
                                    is_literal: true,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "fowardagent": "yes",
                        "identityfile": "%d/.ssh/id_pub.dsa",
                    },
                },
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: true,
                                    pattern: "^a\\.b$",
                                    original: "a.b",
                                    is_literal: true,
                                },
                                Pattern {
                                    negated: false,
                                    pattern: "^.*\\.b$",
                                    original: "*.b",
                                    is_literal: false,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "forwardagent": "no",
                        "identityagent": "${HOME}/.ssh/agent",
                    },
                },
                MatchGroup {
                    criteria: [
                        Host(
                            [
                                Pattern {
                                    negated: false,
                                    pattern: "^.*$",
                                    original: "*",
                                    is_literal: false,
                                },
                            ],
                        ),
                    ],
                    context: FirstPass,
                    options: {
                        "something": "else",
                    },
                },
            ],
            loaded_files: [],
        },
    ],
    options: {},
    tokens: {},
    environment: Some(
        {
            "HOME": "/home/me",
            "USER": "me",
        },
    ),
}
"#
        );

        let opts = config.for_host("random");
        snapshot!(
            opts,
            r#"
{
    "hostname": "random",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "something": "first",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let opts = config.for_host("192.168.1.8");
        snapshot!(
            opts,
            r#"
{
    "fowardagent": "yes",
    "hostname": "192.168.1.8",
    "identityfile": "/home/me/.ssh/id_pub.dsa",
    "port": "22",
    "something": "first",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let opts = config.for_host("a.b");
        snapshot!(
            opts,
            r#"
{
    "hostname": "a.b",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "something": "first",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );

        let opts = config.for_host("b.b");
        snapshot!(
            opts,
            r#"
{
    "forwardagent": "no",
    "hostname": "b.b",
    "identityagent": "/home/me/.ssh/agent",
    "identityfile": "/home/me/.ssh/id_dsa /home/me/.ssh/id_ecdsa /home/me/.ssh/id_ed25519 /home/me/.ssh/id_rsa",
    "port": "22",
    "something": "first",
    "user": "me",
    "userknownhostsfile": "/home/me/.ssh/known_hosts /home/me/.ssh/known_hosts2",
}
"#
        );
    }

    #[test]
    fn wildcard_literal() {
        let (pat, is_literal) = wildcard_to_pattern("foo");
        assert!(is_literal);
        assert_eq!(pat, "^foo$");
    }

    #[test]
    fn wildcard_star() {
        let (pat, is_literal) = wildcard_to_pattern("*.example.com");
        assert!(!is_literal);
        assert!(pat.contains(".*"));
    }

    #[test]
    fn wildcard_question_mark() {
        let (pat, is_literal) = wildcard_to_pattern("host?");
        assert!(!is_literal);
        assert!(pat.contains("."));
    }

    #[test]
    fn wildcard_escapes_special_chars() {
        let (pat, is_literal) = wildcard_to_pattern("192.168.1.1");
        assert!(is_literal);
        assert!(pat.contains(r"\."));
    }

    #[test]
    fn pattern_match_literal() {
        let pat = Pattern::new("myhost", false);
        assert!(pat.match_text("myhost"));
        assert!(!pat.match_text("otherhost"));
    }

    #[test]
    fn pattern_match_wildcard() {
        let pat = Pattern::new("*.example.com", false);
        assert!(pat.match_text("foo.example.com"));
        assert!(pat.match_text("bar.example.com"));
        assert!(!pat.match_text("example.com"));
    }

    #[test]
    fn pattern_match_question_mark() {
        let pat = Pattern::new("host?", false);
        assert!(pat.match_text("host1"));
        assert!(pat.match_text("hostA"));
        assert!(!pat.match_text("host12"));
    }

    #[test]
    fn pattern_negated() {
        let pat = Pattern::new("badhost", true);
        assert!(pat.negated);
        assert!(pat.match_text("badhost"));
    }

    #[test]
    fn pattern_match_group_positive() {
        let patterns = vec![Pattern::new("foo", false), Pattern::new("bar", false)];
        assert!(Pattern::match_group("foo", &patterns));
        assert!(Pattern::match_group("bar", &patterns));
        assert!(!Pattern::match_group("baz", &patterns));
    }

    #[test]
    fn pattern_match_group_negated() {
        let patterns = vec![Pattern::new("excluded", true), Pattern::new("*", false)];
        assert!(!Pattern::match_group("excluded", &patterns));
        assert!(Pattern::match_group("anything_else", &patterns));
    }

    #[test]
    fn pattern_match_group_empty() {
        let patterns: Vec<Pattern> = vec![];
        assert!(!Pattern::match_group("anything", &patterns));
    }

    #[test]
    fn config_new_is_empty() {
        let config = Config::new();
        assert!(config.enumerate_hosts().is_empty());
    }

    #[test]
    fn config_enumerate_hosts_excludes_patterns() {
        let mut config = Config::new();
        config.add_config_string(
            r#"
        Host literal_host
            Port 22
        Host *.wildcard.com
            Port 22
        "#,
        );
        let hosts = config.enumerate_hosts();
        assert!(hosts.contains(&"literal_host".to_string()));
        assert!(!hosts.iter().any(|h| h.contains("*")));
    }

    #[test]
    fn config_for_host_defaults() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/test".to_string());
        fake_env.insert("USER".to_string(), "test".to_string());
        config.assign_environment(fake_env);

        let opts = config.for_host("anyhost");
        assert_eq!(opts.get("hostname").unwrap(), "anyhost");
        assert_eq!(opts.get("port").unwrap(), "22");
        assert_eq!(opts.get("user").unwrap(), "test");
    }

    #[test]
    fn config_first_match_wins() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        Host myhost
            Port 2222
        Host myhost
            Port 3333
        "#,
        );
        let opts = config.for_host("myhost");
        assert_eq!(opts.get("port").unwrap(), "2222");
    }

    #[test]
    fn config_wildcard_host_applies() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string(
            r#"
        Host *
            ServerAliveInterval 30
        "#,
        );
        let opts = config.for_host("anyhost");
        assert_eq!(opts.get("serveraliveinterval").unwrap(), "30");
    }

    #[test]
    fn context_variants() {
        assert_ne!(Context::FirstPass, Context::Canonical);
        assert_ne!(Context::Canonical, Context::Final);
        assert_ne!(Context::FirstPass, Context::Final);
    }

    #[test]
    fn criteria_equality() {
        let a = Criteria::All;
        let b = Criteria::All;
        assert_eq!(a, b);
    }

    #[test]
    fn config_loaded_files_empty_for_string_config() {
        let mut config = Config::new();
        config.add_config_string("Host foo\n    Port 22\n");
        assert!(config.loaded_config_files().is_empty());
    }

    #[test]
    fn config_set_option_overrides_config_file() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string("Host myhost\n    Port 2222\n");
        config.set_option("port", "9999");

        let opts = config.for_host("myhost");
        assert_eq!(opts.get("port").unwrap(), "9999");
    }

    #[test]
    fn config_empty_string() {
        let mut config = Config::new();
        config.add_config_string("");
        assert!(config.enumerate_hosts().is_empty());
    }

    #[test]
    fn config_comment_only() {
        let mut config = Config::new();
        config.add_config_string("# This is a comment\n# Another comment\n");
        assert!(config.enumerate_hosts().is_empty());
    }

    #[test]
    fn config_should_expand_environment_known_keys() {
        let config = Config::new();
        assert!(config.should_expand_environment("certificatefile"));
        assert!(config.should_expand_environment("controlpath"));
        assert!(config.should_expand_environment("identityagent"));
        assert!(config.should_expand_environment("identityfile"));
        assert!(config.should_expand_environment("userknownhostsfile"));
        assert!(config.should_expand_environment("localforward"));
        assert!(config.should_expand_environment("remoteforward"));
    }

    #[test]
    fn config_should_not_expand_environment_unknown_keys() {
        let config = Config::new();
        assert!(!config.should_expand_environment("hostname"));
        assert!(!config.should_expand_environment("port"));
        assert!(!config.should_expand_environment("user"));
    }

    #[test]
    fn config_match_all_stanza() {
        let mut config = Config::new();
        let mut fake_env = ConfigMap::new();
        fake_env.insert("HOME".to_string(), "/home/me".to_string());
        fake_env.insert("USER".to_string(), "me".to_string());
        config.assign_environment(fake_env);

        config.add_config_string("Match all\n    SendEnv LANG\n");

        let opts = config.for_host("anyhost");
        assert_eq!(opts.get("sendenv").unwrap(), "LANG");
    }

    #[test]
    fn pattern_star_matches_everything() {
        let pat = Pattern::new("*", false);
        assert!(pat.match_text("anything"));
        assert!(pat.match_text(""));
        assert!(pat.match_text("foo.bar.baz"));
    }

    #[test]
    fn config_percent_escape() {
        let config = Config::new();
        let mut value = "literal%%percent".to_string();
        let token_map = ConfigMap::new();
        config.expand_tokens(&mut value, &[], &token_map);
        assert_eq!(value, "literal%percent");
    }
}
